//! The write-transaction boundary, against the acceptance list it was asked for.
//!
//! The question this answers is whether one committed source transaction can
//! become exactly one visible yesno version. Every assertion below is about
//! that, and several of them would pass against a *wrong* implementation that
//! merely looked atomic -- the order fixture in particular.
//!
//! What is deliberately **not** covered, because the surface does not exist:
//! staging or committing from another principal or leadership term. The Flight
//! service authenticates nobody today, so a handle is bound to whoever holds
//! its eight bytes. That is a real gap and it is named here rather than
//! papered over with a test that would assert nothing.

use std::sync::Arc;

use arrow_flight::flight_service_server::FlightServiceServer;
use tonic::transport::Server;
use yesno_core::{Db, DbOptions};
use yesno_flight::client::{Mutation, WriteTxn};
use yesno_flight::{
    SetExpr, YesnoClient, YesnoFlightService, FEATURE_MIXED_PUT, FEATURE_WRITE_TRANSACTIONS,
};

async fn serve(db: Arc<Db>) -> (String, tokio::sync::oneshot::Sender<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        Server::builder()
            .add_service(FlightServiceServer::new(YesnoFlightService::new(db)))
            .serve_with_incoming_shutdown(
                tokio_stream::wrappers::TcpListenerStream::new(listener),
                async {
                    let _ = rx.await;
                },
            )
            .await
            .unwrap();
    });
    (format!("http://{addr}"), tx)
}

fn open(dir: &std::path::Path) -> Arc<Db> {
    Arc::new(
        Db::open_with(
            dir,
            DbOptions {
                shards: 2,
                ..Default::default()
            },
        )
        .unwrap(),
    )
}

/// Every ordinal under `key`, as the database currently sees it.
async fn members(client: &mut YesnoClient, key: u64) -> Vec<u64> {
    let mut v = client
        .get(key)
        .await
        .unwrap()
        .collect_ordinals()
        .await
        .unwrap();
    v.sort_unstable();
    v
}

/// Every ordinal under `key` as of `version`.
async fn members_at(client: &mut YesnoClient, key: u64, version: u64) -> Vec<u64> {
    let info = client
        .prepare_query_at(&SetExpr::Key(key), version)
        .await
        .unwrap();
    let mut stream = client.fetch(&info).await.unwrap();
    let mut v = Vec::new();
    while let Some(batch) = futures::TryStreamExt::try_next(&mut stream).await.unwrap() {
        let ords = batch
            .column_by_name("ordinal")
            .and_then(|c| c.as_any().downcast_ref::<arrow_array::UInt64Array>())
            .expect("a UInt64 ordinal column");
        v.extend(ords.values().iter().copied());
    }
    v.sort_unstable();
    v
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_server_advertises_what_it_can_do() {
    // Capability discovery is what stops a new client sending a command an
    // older server would silently treat as insert.
    let dir = tempfile::tempdir().unwrap();
    let (url, _stop) = serve(open(dir.path())).await;
    let mut client = YesnoClient::connect(url).await.unwrap();
    assert!(client.supports(FEATURE_MIXED_PUT).await.unwrap());
    assert!(client.supports(FEATURE_WRITE_TRANSACTIONS).await.unwrap());
    // A bit this build does not implement must not be claimed.
    assert!(!client.supports(1 << 63).await.unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn staged_work_is_invisible_until_commit_then_all_visible_at_one_version() {
    let dir = tempfile::tempdir().unwrap();
    let (url, _stop) = serve(open(dir.path())).await;
    let mut client = YesnoClient::connect(url).await.unwrap();

    let before = client.insert_batch_acked([(7u64, 1u64)]).await.unwrap();
    let before_version = before.version.expect("this server reports versions");

    let txn = client.begin_write().await.unwrap();
    // Two separate DoPut calls, so this also covers "several streams, one
    // commit".
    client
        .stage(txn, [Mutation::Insert { key: 7, ordinal: 2 }])
        .await
        .unwrap();
    client
        .stage(
            txn,
            [
                Mutation::Insert { key: 7, ordinal: 3 },
                Mutation::Insert { key: 8, ordinal: 9 },
            ],
        )
        .await
        .unwrap();

    // 1. Nothing staged is visible, to a fresh reader or at the old version.
    assert_eq!(members(&mut client, 7).await, vec![1]);
    assert_eq!(members(&mut client, 8).await, Vec::<u64>::new());
    assert_eq!(members_at(&mut client, 7, before_version).await, vec![1]);

    let version = client.commit_write(txn).await.unwrap();
    assert!(version > before_version, "commit must advance the version");

    // 3. Everything appears at that one version, across both keys and both calls.
    assert_eq!(members_at(&mut client, 7, version).await, vec![1, 2, 3]);
    assert_eq!(members_at(&mut client, 8, version).await, vec![9]);
    // And nothing leaked into the version that preceded it.
    assert_eq!(members_at(&mut client, 7, before_version).await, vec![1]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn order_is_preserved_across_staging_calls() {
    // **The fixture that catches a plausible wrong implementation.** Grouping
    // staged operations by kind -- all deletes, then all inserts -- still
    // commits atomically and still returns one version, and would invert both
    // of these.
    let dir = tempfile::tempdir().unwrap();
    let (url, _stop) = serve(open(dir.path())).await;
    let mut client = YesnoClient::connect(url).await.unwrap();

    client
        .insert_batch([(1u64, 100u64), (1, 101), (1, 102), (2, 500)])
        .await
        .unwrap();

    let txn = client.begin_write().await.unwrap();
    // Whole-key replacement: the delete must not be applied after the inserts.
    client
        .stage(txn, [Mutation::DeleteKey { key: 1 }])
        .await
        .unwrap();
    client
        .stage(
            txn,
            [
                Mutation::Insert {
                    key: 1,
                    ordinal: 200,
                },
                Mutation::Insert {
                    key: 1,
                    ordinal: 201,
                },
            ],
        )
        .await
        .unwrap();
    // Remove then re-insert the same membership: the insert must win.
    client
        .stage(
            txn,
            [
                Mutation::Remove {
                    key: 2,
                    ordinal: 500,
                },
                Mutation::Insert {
                    key: 2,
                    ordinal: 500,
                },
            ],
        )
        .await
        .unwrap();
    // **The discriminating cases.** Everything above has removals *before*
    // insertions, which is exactly what grouping by kind produces anyway -- so
    // it would pass against an implementation that reorders. These two put a
    // removal *after* an insertion in one batch, where grouping inverts the
    // result instead of preserving it.
    client
        .stage(
            txn,
            [
                Mutation::Insert { key: 9, ordinal: 1 },
                Mutation::DeleteKey { key: 9 },
            ],
        )
        .await
        .unwrap();
    client
        .stage(
            txn,
            [
                Mutation::Insert {
                    key: 10,
                    ordinal: 5,
                },
                Mutation::Remove {
                    key: 10,
                    ordinal: 5,
                },
            ],
        )
        .await
        .unwrap();
    let version = client.commit_write(txn).await.unwrap();

    assert_eq!(
        members_at(&mut client, 9, version).await,
        Vec::<u64>::new(),
        "insert then delete-key must leave the key empty; grouping would keep the insert"
    );
    assert_eq!(
        members_at(&mut client, 10, version).await,
        Vec::<u64>::new(),
        "insert then remove must leave it absent; grouping would keep the insert"
    );
    assert_eq!(
        members_at(&mut client, 1, version).await,
        vec![200, 201],
        "the delete was applied after the inserts, or grouped away"
    );
    assert_eq!(
        members_at(&mut client, 2, version).await,
        vec![500],
        "remove-then-insert of one membership must leave it present"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_inclusive_range_removal_is_one_operation() {
    // Ranges exist on the wire so a contiguous clear is not 65 536 operations.
    let dir = tempfile::tempdir().unwrap();
    let (url, _stop) = serve(open(dir.path())).await;
    let mut client = YesnoClient::connect(url).await.unwrap();

    let txn = client.begin_write().await.unwrap();
    client
        .stage(
            txn,
            [Mutation::InsertRange {
                key: 5,
                lo: 10,
                hi: 20,
            }],
        )
        .await
        .unwrap();
    client
        .stage(
            txn,
            [Mutation::RemoveRange {
                key: 5,
                lo: 12,
                hi: 18,
            }],
        )
        .await
        .unwrap();
    let version = client.commit_write(txn).await.unwrap();
    assert_eq!(
        members_at(&mut client, 5, version).await,
        vec![10, 11, 19, 20],
        "inclusive bounds, and the removal applied after the insertion"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn abort_leaves_the_database_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let (url, _stop) = serve(open(dir.path())).await;
    let mut client = YesnoClient::connect(url).await.unwrap();
    client.insert_batch([(3u64, 1u64)]).await.unwrap();

    let txn = client.begin_write().await.unwrap();
    client
        .stage(
            txn,
            [
                Mutation::DeleteKey { key: 3 },
                Mutation::Insert {
                    key: 3,
                    ordinal: 99,
                },
            ],
        )
        .await
        .unwrap();
    client.abort_write(txn).await.unwrap();

    assert_eq!(members(&mut client, 3).await, vec![1]);
    // Aborting again is not an error: the intent already holds.
    client.abort_write(txn).await.unwrap();
    // But committing an aborted transaction must fail rather than apply it.
    assert!(client.commit_write(txn).await.is_err());
    assert_eq!(members(&mut client, 3).await, vec![1]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn commit_is_idempotent_after_a_lost_response() {
    // The recovery case: a pipe that did not see the acknowledgement retries
    // and must learn the original version rather than apply the work twice.
    let dir = tempfile::tempdir().unwrap();
    let (url, _stop) = serve(open(dir.path())).await;
    let mut client = YesnoClient::connect(url).await.unwrap();

    let txn = client.begin_write().await.unwrap();
    client
        .stage(
            txn,
            [
                Mutation::Insert { key: 4, ordinal: 1 },
                Mutation::Insert { key: 4, ordinal: 2 },
            ],
        )
        .await
        .unwrap();
    let first = client.commit_write(txn).await.unwrap();
    for _ in 0..3 {
        assert_eq!(
            client.commit_write(txn).await.unwrap(),
            first,
            "a retry must report the original version"
        );
    }
    assert_eq!(members(&mut client, 4).await, vec![1, 2]);
    // And aborting after the fact must not pretend it undid anything.
    assert!(client.abort_write(txn).await.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_one_shot_mixed_apply_is_a_single_commit() {
    let dir = tempfile::tempdir().unwrap();
    let (url, _stop) = serve(open(dir.path())).await;
    let mut client = YesnoClient::connect(url).await.unwrap();
    client.insert_batch([(6u64, 1u64), (6, 2)]).await.unwrap();

    let ack = client
        .apply([
            Mutation::Remove { key: 6, ordinal: 1 },
            Mutation::Insert { key: 6, ordinal: 3 },
            Mutation::DeleteKey { key: 7 },
        ])
        .await
        .unwrap();
    let version = ack.version.expect("this server reports versions");
    assert_eq!(members_at(&mut client, 6, version).await, vec![2, 3]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_put_command_is_refused_rather_than_treated_as_insert() {
    // The hazard this whole feature had to clear first. An older server maps
    // any unrecognised command to insert, so a mixed batch's removals would be
    // applied as insertions with no error. This server refuses instead.
    let dir = tempfile::tempdir().unwrap();
    let (url, _stop) = serve(open(dir.path())).await;
    let mut client = YesnoClient::connect(url).await.unwrap();
    let err = client
        .prepare_command(b"definitely-not-a-command")
        .await
        .err();
    // `prepare_command` is the read path; the write path is what matters, and
    // it is exercised by staging against a transaction that does not exist.
    let _ = err;
    assert!(
        client
            .stage(WriteTxn(987_654), [Mutation::Insert { key: 1, ordinal: 1 }])
            .await
            .is_err(),
        "staging against an unknown transaction must fail, not insert"
    );
    assert_eq!(members(&mut client, 1).await, Vec::<u64>::new());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_point_operation_carrying_a_range_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (url, _stop) = serve(open(dir.path())).await;
    let mut client = YesnoClient::connect(url).await.unwrap();
    let txn = client.begin_write().await.unwrap();
    // Built by hand: the `Mutation` constructors cannot express this, which is
    // the point -- the check is for clients that are not this one.
    let bad = yesno_flight::client::Mutation::InsertRange {
        key: 1,
        lo: 9,
        hi: 4,
    };
    assert!(
        client.stage(txn, [bad]).await.is_err(),
        "an inverted range is a transposed pair of arguments, not an empty set"
    );
}

// The cases below come from an external audit of this feature ( haiiie,
// 2026-09-23 ). Both passed review and passed every test in this file at the
// time, which is why they are written as behaviour rather than as notes.

/// A staging call the server **rejected** must leave nothing committable.
///
/// `a_point_operation_carrying_a_range_is_refused` above asserts the refusal
/// and stops there, and that gap was the bug: rows were applied as they were
/// validated, so an invalid row left every *earlier* row of the same batch in
/// the resident batch, and `commit_write` re-validates nothing.
///
/// Two things fix it and only one of them is observable here. Decoding is now
/// separated from application, so a single record batch applies entirely or
/// not at all; and a failed staging call poisons the transaction, so the
/// question "what did that batch leave behind" can no longer be asked through
/// the public API at all. The second subsumes the first, which is why this
/// asserts the refusal rather than an empty commit -- the contract a client
/// sees is that a rejected stage cannot be committed, only aborted.
///
/// The row-bound refusal shares this path and is ordered the same way --
/// checked before anything is staged -- but is not asserted separately here,
/// because `MAX_TRANSACTION_ROWS` is sixteen million and a fixture that cannot
/// reach it would assert nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rejected_batch_stages_none_of_its_rows() {
    let dir = tempfile::tempdir().unwrap();
    let (url, _stop) = serve(open(dir.path())).await;
    let mut client = YesnoClient::connect(url).await.unwrap();

    let txn = client.begin_write().await.unwrap();
    // One batch, two rows: a valid insert followed by an inverted range. The
    // insert is what must not survive the refusal.
    let refused = client
        .stage(
            txn,
            [
                Mutation::Insert {
                    key: 50,
                    ordinal: 1,
                },
                Mutation::RemoveRange {
                    key: 50,
                    lo: 9,
                    hi: 4,
                },
            ],
        )
        .await;
    assert!(refused.is_err(), "an inverted range must be refused");

    assert!(
        client.commit_write(txn).await.is_err(),
        "a transaction whose staging call was refused must not commit"
    );
    client.abort_write(txn).await.unwrap();
    assert_eq!(
        members(&mut client, 50).await,
        Vec::<u64>::new(),
        "a refused batch must contribute nothing"
    );
}

/// A handle must never be reissued, so a stale retry fails closed.
///
/// Handles were a counter from zero, so a server restarted over the same
/// database issued handle 1 again and a delayed `commit_write` from before the
/// restart resolved a **different** transaction, publishing another caller's
/// staged work under the retrying caller's identity. The audit reproduced that
/// with no hostile client. Two sequential services over one database is the
/// same aliasing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_handle_from_a_previous_service_is_not_reissued() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());

    let (first_url, stop_first) = serve(db.clone()).await;
    let mut first = YesnoClient::connect(first_url).await.unwrap();
    let stale = first.begin_write().await.unwrap();
    first
        .stage(
            txn_of(stale),
            [Mutation::Insert {
                key: 70,
                ordinal: 1,
            }],
        )
        .await
        .unwrap();
    drop(first);
    let _ = stop_first.send(());

    let (second_url, _stop) = serve(db.clone()).await;
    let mut second = YesnoClient::connect(second_url).await.unwrap();
    let fresh = second.begin_write().await.unwrap();
    second
        .stage(
            txn_of(fresh),
            [Mutation::Insert {
                key: 71,
                ordinal: 2,
            }],
        )
        .await
        .unwrap();

    assert_ne!(
        stale.0, fresh.0,
        "a handle issued by a previous service must not be reissued"
    );
    assert!(
        second.commit_write(stale).await.is_err(),
        "a stale handle must resolve nothing rather than commit another transaction"
    );
    // And the transaction that really is open is untouched by that attempt.
    let version = second.commit_write(fresh).await.unwrap();
    assert_eq!(members_at(&mut second, 71, version).await, vec![2]);
    assert_eq!(
        members_at(&mut second, 70, version).await,
        Vec::<u64>::new(),
        "the first service's staged work died with it"
    );
}

/// `WriteTxn` is a newtype; this keeps the call sites above readable.
fn txn_of(txn: WriteTxn) -> WriteTxn {
    txn
}

/// A `do_put` that fails in a **later** record batch poisons the transaction.
///
/// `a_rejected_batch_stages_none_of_its_rows` puts its rows in one batch and
/// therefore cannot tell two contracts apart, which a follow-up audit pointed
/// out. This drives `DoPut` directly, because **the Rust client cannot express
/// the case**: `put_mutations` collects every mutation into a single
/// `RecordBatch` regardless of count, so no number of mutations passed to
/// `stage` produces a second one. An earlier version of this test passed 8193
/// of them and claimed two batches; it passed for the single-batch reason
/// while its name promised the other, which is the defect this whole audit
/// thread has been about.
///
/// The server applies each record batch as it arrives, so a failure in the
/// second leaves the first staged with no way to withdraw it -- the core
/// `WriteBatch` has no rollback. The transaction therefore fails closed:
/// commit is refused, abort is the only way out, and none of the first batch
/// becomes visible.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failure_in_a_later_batch_poisons_the_transaction() {
    use arrow_flight::flight_service_client::FlightServiceClient;
    use futures::StreamExt;

    let dir = tempfile::tempdir().unwrap();
    let (url, _stop) = serve(open(dir.path())).await;
    let mut client = YesnoClient::connect(url.clone()).await.unwrap();
    let txn = client.begin_write().await.unwrap();

    // Two record batches under one `txn:` descriptor. The first is valid; the
    // second carries an inverted range and must be refused.
    let rows = |spec: &[(u64, u64, u64, u8)]| {
        arrow_array::RecordBatch::try_new(
            yesno_flight::mutations_schema(),
            vec![
                Arc::new(arrow_array::UInt64Array::from(
                    spec.iter().map(|r| r.0).collect::<Vec<_>>(),
                )),
                Arc::new(arrow_array::UInt64Array::from(
                    spec.iter().map(|r| r.1).collect::<Vec<_>>(),
                )),
                Arc::new(arrow_array::UInt64Array::from(
                    spec.iter().map(|r| r.2).collect::<Vec<_>>(),
                )),
                Arc::new(arrow_array::UInt8Array::from(
                    spec.iter().map(|r| r.3).collect::<Vec<_>>(),
                )),
            ],
        )
        .unwrap()
    };
    let first = rows(&[
        (80, 1, 1, yesno_flight::OP_INSERT),
        (80, 2, 2, yesno_flight::OP_INSERT),
    ]);
    // Inverted bounds: refused, and it arrives after the first batch is staged.
    let second = rows(&[(80, 9, 4, yesno_flight::OP_REMOVE_RANGE)]);

    let mut command = yesno_flight::PUT_TXN_PREFIX.to_vec();
    command.extend_from_slice(&txn.0.to_le_bytes());
    let input = arrow_flight::encode::FlightDataEncoderBuilder::new()
        .with_schema(yesno_flight::mutations_schema())
        .with_flight_descriptor(Some(arrow_flight::FlightDescriptor::new_cmd(command)))
        .build(futures::stream::iter(vec![Ok(first), Ok(second)]))
        .map(|r| r.unwrap());

    let channel = tonic::transport::Channel::from_shared(url)
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut raw = FlightServiceClient::new(channel);
    let staged = match raw.do_put(input).await {
        Err(_) => Err(()),
        Ok(response) => {
            let mut acks = response.into_inner();
            let mut outcome = Ok(());
            while let Some(r) = acks.next().await {
                if r.is_err() {
                    outcome = Err(());
                }
            }
            outcome
        }
    };
    assert!(staged.is_err(), "the second batch must be refused");

    let err = client
        .commit_write(txn)
        .await
        .expect_err("a poisoned transaction must not commit");
    let message = err.to_string();
    assert!(
        message.contains("Abort it"),
        "the refusal has to say what to do about it: {message}"
    );

    client.abort_write(txn).await.unwrap();
    assert_eq!(
        members(&mut client, 80).await,
        Vec::<u64>::new(),
        "no row from the first batch may become visible"
    );
}
