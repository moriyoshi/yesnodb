//! The ticket's version is a promise, and this is where it is kept.
//!
//! # What this layer catches that `roundtrip.rs` cannot
//!
//! `roundtrip.rs` asserts that the ticket *carries* a version — `t.version > 0`
//! — and that was the strongest statement available while `do_get` opened a
//! fresh snapshot and never read the field. A field that is written and never
//! read passes every assertion about its contents; the only thing that can
//! falsify it is a **write landing between the two calls**, which is what every
//! test here arranges.
//!
//! That gap is the whole point of the field. `ticket.rs` says so:
//!
//! > a coordinator can hand N endpoints — potentially N read-only followers —
//! > disjoint prefix ranges of the *same* snapshot … Without the version each
//! > endpoint would answer from whatever it could see, and the union would be a
//! > set that never existed at any instant.
//!
//! Two endpoints answering from two instants is not a *slow* fan-out, it is a
//! wrong one, and nothing downstream can detect it: every ordinal returned is
//! real, and the union is simply not a set the database ever held.

use std::collections::BTreeSet;
use std::sync::Arc;

use arrow_array::{Array, UInt64Array};
use arrow_flight::flight_service_client::FlightServiceClient;
use arrow_flight::flight_service_server::FlightServiceServer;
use arrow_flight::FlightDescriptor;
use futures::StreamExt;
use tonic::transport::{Channel, Server};
use yesno_core::{Db, DbOptions};
use yesno_flight::{Ticket, YesnoFlightService};

async fn client_for(url: String) -> FlightServiceClient<Channel> {
    let ch = Channel::from_shared(url).unwrap().connect().await.unwrap();
    FlightServiceClient::new(ch)
}

async fn serve(db: Arc<Db>) -> (String, tokio::sync::oneshot::Sender<()>) {
    serve_with_lease(db, YesnoFlightService::DEFAULT_TICKET_LEASE).await
}

async fn serve_with_lease(
    db: Arc<Db>,
    lease: std::time::Duration,
) -> (String, tokio::sync::oneshot::Sender<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        Server::builder()
            .add_service(FlightServiceServer::new(
                YesnoFlightService::with_ticket_lease(db, lease),
            ))
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

/// Drain a `do_get` into the set it carries.
///
/// **The stream is consumed to completion *before* it is decoded**, and that
/// is not a stylistic choice. `FlightRecordBatchStream` wraps whatever its inner
/// stream yields in `FlightError::ExternalError`, so a `tonic::Status` that
/// reaches it comes back out as `Internal("External error: code: 'Client
/// specified an invalid argument' …")` — the real code survives only in the
/// message text. A test asserting on `code()` through the decoder therefore
/// reads `Internal` for **every** refusal, which would have made every
/// status-discipline assertion in this file vacuously wrong in the same
/// direction. Reading `Streaming::message` directly keeps the status intact.
async fn drain(
    client: &mut FlightServiceClient<Channel>,
    raw: Vec<u8>,
) -> Result<BTreeSet<u64>, tonic::Status> {
    let mut stream = client
        .do_get(arrow_flight::Ticket::new(raw))
        .await?
        .into_inner();
    let mut raw_data = Vec::new();
    // A per-message error, not a call-level one: `do_get` returns its stream
    // immediately and reports a failed read on the channel, so awaiting the call
    // alone would miss every server-side refusal here.
    while let Some(d) = stream.message().await? {
        raw_data.push(Ok(d));
    }

    let mut decoded = arrow_flight::decode::FlightRecordBatchStream::new_from_flight_data(
        futures::stream::iter(raw_data),
    );
    let mut got = BTreeSet::new();
    while let Some(b) = decoded.next().await {
        let b = b.expect("the server's own batches must decode");
        let col = b.column(0).as_any().downcast_ref::<UInt64Array>().unwrap();
        got.extend((0..col.len()).map(|i| col.value(i)));
    }
    Ok(got)
}

/// **The plan's F2 test.** A write lands between `get_flight_info` and
/// `do_get`; the rows delivered must be the rows promised.
///
/// This failed before `Db::snapshot_at` existed — `do_get` opened a current
/// snapshot, so it returned `want + extra` against a `total_records` of
/// `want.len()`. A client sizing a buffer from the count would have overflowed
/// it, and one that trusted the count would have silently truncated.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn do_get_delivers_exactly_the_rows_get_flight_info_promised() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());

    let want: BTreeSet<u64> = (0..20_000u64).map(|i| i * 3).collect();
    db.insert_many(7, &want.iter().copied().collect::<Vec<_>>())
        .unwrap();
    db.checkpoint().unwrap();

    let (url, _stop) = serve(db.clone()).await;
    let mut client = client_for(url).await;

    let d = FlightDescriptor::new_cmd(7u64.to_le_bytes().to_vec());
    let info = client.get_flight_info(d).await.unwrap().into_inner();
    let promised = info.total_records;
    assert_eq!(promised, want.len() as i64);
    let raw = info.endpoint[0].ticket.as_ref().unwrap().ticket.to_vec();
    let t = Ticket::decode(&raw).unwrap();

    // ---- the concurrent write. Ordinals disjoint from `want`, so an answer
    // from the wrong instant is detectable ordinal-for-ordinal rather than only
    // by a count.
    let extra: Vec<u64> = (0..5_000u64).map(|i| i * 3 + 1).collect();
    db.insert_many(7, &extra).unwrap();
    assert!(
        db.visible() > t.version,
        "the write must have advanced the clock"
    );

    let got = drain(&mut client, raw)
        .await
        .expect("the promised version is still readable");
    assert_eq!(
        got.len() as i64,
        promised,
        "do_get delivered {} rows against a promise of {promised}",
        got.len()
    );
    assert_eq!(
        got, want,
        "the rows are not the ones the count was computed over"
    );
    // And the write is genuinely there — this test must not pass by the write
    // having silently failed.
    assert_eq!(
        db.snapshot().unwrap().load(7).unwrap().len(),
        (want.len() + extra.len()) as u64
    );
}

/// A view ticket pins the physical key version as well as the descriptor.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_view_ticket_keeps_the_pre_write_logical_set() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    let want = BTreeSet::from([2, 8, 89]);
    let packed: Vec<u64> = want.iter().map(|x| x * 3 + 1).collect();
    db.insert_many(9, &packed).unwrap();
    db.checkpoint().unwrap();

    let (url, _stop) = serve(db.clone()).await;
    let mut client = client_for(url).await;
    let expr = yesno_flight::SetExpr::At(
        Box::new(yesno_flight::VecSetExpr::View(
            Box::new(yesno_flight::SetExpr::Key(9)),
            yesno_flight::ViewSpec::interleaved(3),
        )),
        1,
    );
    let info = client
        .get_flight_info(FlightDescriptor::new_cmd(expr.encode()))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(info.total_records, want.len() as i64);
    let raw = info.endpoint[0].ticket.as_ref().unwrap().ticket.to_vec();
    let ticket = Ticket::decode(&raw).unwrap();
    assert_eq!(ticket.expr, Some(expr));

    let extra_logical = 144;
    db.insert(9, extra_logical * 3 + 1).unwrap();
    assert!(db.visible() > ticket.version);

    assert_eq!(drain(&mut client, raw).await.unwrap(), want);
    assert!(db
        .snapshot()
        .unwrap()
        .load(9)
        .unwrap()
        .contains(extra_logical * 3 + 1));
}

/// Two endpoints splitting one query see one instant, which is the case the
/// version field was added for.
///
/// The prefix ranges are **disjoint and exhaustive**, so the union is the
/// whole key. That is what makes a torn read visible: with two instants the
/// union contains rows from both, and its size is neither endpoint's promise.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_split_fetch_assembles_a_set_that_existed_at_one_instant() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());

    // Two ordinals per 2^16 prefix across many prefixes, so a prefix split
    // genuinely divides the data rather than sending it all one way.
    let want: BTreeSet<u64> = (0..400u64).flat_map(|p| [p << 16, (p << 16) | 7]).collect();
    db.insert_many(7, &want.iter().copied().collect::<Vec<_>>())
        .unwrap();
    db.checkpoint().unwrap();

    let (url, _stop) = serve(db.clone()).await;
    let mut client = client_for(url).await;

    let d = FlightDescriptor::new_cmd(7u64.to_le_bytes().to_vec());
    let info = client.get_flight_info(d).await.unwrap().into_inner();
    let t = Ticket::decode(&info.endpoint[0].ticket.as_ref().unwrap().ticket).unwrap();

    // The coordinator's job: same version, disjoint prefix ranges.
    let lo = Ticket {
        prefix_hi: 200,
        ..t.clone()
    };
    let hi = Ticket {
        prefix_lo: 200,
        ..t.clone()
    };

    // A write between the two fetches — the interleaving a real fan-out cannot
    // prevent, because the endpoints are independent.
    let a = drain(&mut client, lo.encode()).await.unwrap();
    db.insert_many(7, &(0..400u64).map(|p| (p << 16) | 9).collect::<Vec<_>>())
        .unwrap();
    let b = drain(&mut client, hi.encode()).await.unwrap();

    assert!(
        !a.is_empty() && !b.is_empty(),
        "the split must divide the data"
    );
    assert!(
        a.is_disjoint(&b),
        "the halves overlap; the split is not a partition"
    );
    let union: BTreeSet<u64> = a.union(&b).copied().collect();
    assert_eq!(
        union, want,
        "the assembled set is not one this database ever held at any instant"
    );
}

/// A ticket whose version a checkpoint has collapsed **and whose lease has
/// gone** is refused, and the refusal names what to do about it.
///
/// **Leasing is disabled here deliberately, and that is not a weakened
/// assertion.** Since 2026-09-12 `GetFlightInfo` parks a clone of the snapshot
/// it answered from, so within the lease the version cannot be collapsed and
/// this ticket would succeed — which is the whole point of the lease. The
/// refusal path it replaces is still reachable and still has to be right: a
/// ticket older than the lease, one minted by another process, or a deployment
/// that sets the TTL to zero. That is what `Duration::ZERO` selects.
///
/// `failed_precondition`, not `internal` and not `unavailable`. Both of those
/// read as "retry", and retrying a version the floor has passed can only fail
/// again — the floor never falls. What the client must do is ask for a new
/// ticket, and the status code is how it learns that without parsing prose.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_ticket_is_refused_with_a_code_that_means_get_a_new_one() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    db.insert_many(7, &[1, 2, 3]).unwrap();
    db.checkpoint().unwrap();

    let (url, _stop) = serve_with_lease(db.clone(), std::time::Duration::ZERO).await;
    let mut client = client_for(url).await;

    let d = FlightDescriptor::new_cmd(7u64.to_le_bytes().to_vec());
    let info = client.get_flight_info(d).await.unwrap().into_inner();
    let raw = info.endpoint[0].ticket.as_ref().unwrap().ticket.to_vec();

    // Age the ticket out: write, then checkpoint with no reader holding the
    // floor down.
    db.insert_many(7, &[4, 5]).unwrap();
    db.checkpoint().unwrap();

    let err = drain(&mut client, raw)
        .await
        .expect_err("a collapsed version must be refused");
    assert_eq!(
        err.code(),
        tonic::Code::FailedPrecondition,
        "got {:?}: {}",
        err.code(),
        err.message()
    );
    assert!(
        err.message().contains("GetFlightInfo"),
        "the refusal must say what to do; got {:?}",
        err.message()
    );

    // And the server is fine — a stale ticket is the client's problem, not an
    // outage. A fresh one works immediately.
    let d = FlightDescriptor::new_cmd(7u64.to_le_bytes().to_vec());
    let info = client.get_flight_info(d).await.unwrap().into_inner();
    let raw = info.endpoint[0].ticket.as_ref().unwrap().ticket.to_vec();
    assert_eq!(
        drain(&mut client, raw).await.unwrap(),
        BTreeSet::from([1, 2, 3, 4, 5])
    );
}

/// A ticket minted against a different database is refused as a bad argument,
/// not reported as a server fault.
///
/// This is the shape a fan-out coordinator produces by construction — one
/// server's ticket presented to another — so it needs a code that says "your
/// ticket", not one that sends an operator looking at the server's logs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_ticket_from_a_future_this_server_has_not_reached_is_a_bad_argument() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    db.insert_many(7, &[1, 2, 3]).unwrap();

    let (url, _stop) = serve(db.clone()).await;
    let mut client = client_for(url).await;

    let forged = Ticket::whole_key(db.visible() + 1_000, 7);
    let err = drain(&mut client, forged.encode())
        .await
        .expect_err("a version this server never assigned must be refused");
    assert_eq!(
        err.code(),
        tonic::Code::InvalidArgument,
        "{}",
        err.message()
    );
}

/// The lease is the difference between a ticket that survives a checkpoint
/// and one that does not.
///
/// This is the *same* sequence as
/// `a_stale_ticket_is_refused_with_a_code_that_means_get_a_new_one` — mint a
/// ticket, write, checkpoint with no reader holding the floor — and the only
/// change is that leasing is on. Running the pair is the point: either test
/// alone is satisfied by a server that always refuses or always succeeds, and
/// only together do they show the lease deciding it.
///
/// `Snapshot` clones by refcounting its registry slot, so the parked clone
/// *is* the floor holding down. No new retention mechanism was added; a lease
/// is a registered reader with a deadline.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_leased_ticket_survives_a_checkpoint_that_would_collapse_it() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    db.insert_many(7, &[1, 2, 3]).unwrap();
    db.checkpoint().unwrap();

    let (url, _stop) = serve(db.clone()).await;
    let mut client = client_for(url).await;

    let d = FlightDescriptor::new_cmd(7u64.to_le_bytes().to_vec());
    let info = client.get_flight_info(d).await.unwrap().into_inner();
    let raw = info.endpoint[0].ticket.as_ref().unwrap().ticket.to_vec();

    // Exactly what ages a ticket out when nothing holds its version.
    db.insert_many(7, &[4, 5]).unwrap();
    db.checkpoint().unwrap();

    let got = drain(&mut client, raw)
        .await
        .expect("a leased ticket must still be readable after a checkpoint");

    // And it answers from the ticket's instant, not the current one: the
    // post-ticket writes must not appear, or the lease would have fixed
    // availability by breaking the promise the version exists to make.
    assert_eq!(
        got,
        BTreeSet::from([1, 2, 3]),
        "the lease must preserve the snapshot, not merely produce rows"
    );
}

/// `Duration::MAX` is how a caller spells "never expire", and `Instant +
/// Duration` panics on overflow — so the lease that exists to keep a version
/// readable would have killed the request that took it, on its first
/// `GetFlightInfo`.
///
/// Verified by sabotage: restoring `now + self.lease_ttl` reddens this test
/// with `overflow when adding duration to instant` and reddens nothing else.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lease_that_never_expires_does_not_overflow_the_clock() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::open(dir.path()).unwrap());
    db.insert(7, 1).unwrap();
    let (url, stop) = serve_with_lease(db, std::time::Duration::MAX).await;
    let mut client = client_for(url).await;

    let info = client
        .get_flight_info(tonic::Request::new(FlightDescriptor::new_cmd(
            7u64.to_le_bytes().to_vec(),
        )))
        .await
        .expect("an unbounded lease must be taken, not panicked on")
        .into_inner();
    assert_eq!(info.total_records, 1);

    let _ = stop.send(());
}
