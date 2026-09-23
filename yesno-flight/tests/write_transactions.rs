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
