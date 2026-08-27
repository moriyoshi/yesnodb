//! Whether a client can read its own write over Flight.
//!
//! # The gap this closes
//!
//! `commit` returns a version as soon as *its own* version resolves, but the
//! visible watermark advances over a **consecutive prefix**, so a commit that
//! resolves while an earlier one is still inside its fsync holds a version the
//! database will not yet show. On the library surface that was demonstrated by
//! `a_returned_commit_can_be_invisible_while_an_earlier_one_is_pending`.
//!
//! Over Flight it was worse than a window, because `do_put` returned a row count
//! and **discarded the version entirely**. The engine knew it and the handler
//! logged it. So a remote client could not name the version its write landed at,
//! could not wait for it, and could not bind a read to it — read-your-writes was
//! not slow on this surface, it was *inexpressible*.
//!
//! None of this makes reads linearizable, and these tests do not claim it. A
//! client that needs recency can now ask for it; one that does not, still reads
//! whatever the watermark had reached.

use std::sync::Arc;

use arrow_flight::flight_service_server::FlightServiceServer;
use tonic::transport::Server;
use yesno_core::Db;
use yesno_flight::{SetExpr, YesnoClient, YesnoFlightService};

async fn serve_with(
    service: YesnoFlightService,
) -> (
    String,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        Server::builder()
            .add_service(FlightServiceServer::new(service))
            .serve_with_incoming_shutdown(
                tokio_stream::wrappers::TcpListenerStream::new(listener),
                async {
                    let _ = stopped.await;
                },
            )
            .await
            .unwrap();
    });
    (format!("http://{address}"), stop, task)
}

/// The version is reported, and reading *at* it sees the write.
///
/// The `version > 0` assertion is not the point and would pass against a
/// server that returned any constant. What pins it is the pair below: the same
/// version is accepted by `prepare_query_at` — which is strict, so a wrong
/// version is refused rather than silently replaced — and the count it reports
/// is the one just written.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ingest_reports_the_version_its_rows_landed_at() {
    let directory = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::open(directory.path()).unwrap());
    let (url, stop, task) = serve_with(YesnoFlightService::new(db.clone())).await;
    let mut client = YesnoClient::connect(url).await.unwrap();

    let pairs: Vec<_> = (0..5_000u64).map(|ordinal| (7u64, ordinal)).collect();
    let ack = client.insert_batch_acked(pairs.clone()).await.unwrap();

    assert_eq!(ack.rows, 5_000, "every pair should be acknowledged");
    let version = ack
        .version
        .expect("a committing server must report the version it committed at");

    // Strict: `prepare_query_at` refuses a version this database never
    // assigned, so a fabricated or off-by-one version cannot reach this
    // assertion. That is what makes the version *meaningful* and not decorative.
    let info = client
        .prepare_query_at(&SetExpr::Key(7), version)
        .await
        .expect("the version the server just reported must be readable");
    assert_eq!(info.total_records(), 5_000);
    assert_eq!(
        info.version(),
        version,
        "the ticket must name the same instant"
    );

    // A second write must advance it — a constant would not.
    let second = client
        .insert_batch_acked(vec![(7u64, 99_999u64)])
        .await
        .unwrap();
    assert!(
        second.version.unwrap() > version,
        "a later commit must report a later version, got {:?} after {version}",
        second.version
    );

    let _ = stop.send(());
    let _ = task.await;
}

/// Ask for a version that is **not visible yet but will be**, and report what
/// the server did.
///
/// # How this construction was arrived at, because two earlier ones failed
///
/// The condition under test is server-side and simple: `requested > visible`.
/// The *cause* of it in production is the pending-prefix window — a commit that
/// resolves while an earlier one is still in its fsync — and that cause is
/// pinned at the library layer by
/// `a_returned_commit_can_be_invisible_while_an_earlier_one_is_pending`, which
/// reproduces it 8 runs out of 8 because it samples in the same thread,
/// microseconds after the commit returns.
///
/// Reproducing that same *cause* across two network round trips did not work,
/// and the numbers are worth keeping so nobody re-attempts it:
///
/// * A big ( 60 000-ordinal ) concurrent commit gated to start just before the
///   client's write: **0 windows in 48 attempts**.
/// * A background thread committing 200 000-ordinal batches back-to-back to hold
///   the prefix blocked: **0 in 256**. The printed versions came out
///   `1, 2, 3, 4, 5, 6, 8, 9 …` — in a debug build the churn thread spends its
///   time *constructing* the batch, not in the fsync, so there is almost never a
///   pending lower version at all.
///
/// So this constructs the condition directly instead. Versions are dense and
/// never reused, so with no other committer the next commit takes exactly
/// `V + 1`: the probe asks for `V + 1` while a writer is sleeping, and that
/// writer then takes it. Deterministic in both arms, and the assertion that the
/// ticket comes back naming `V + 1` is what proves the server honoured the
/// version rather than quietly answering from a newer snapshot.
async fn probe_pending_version(
    visibility_wait: std::time::Duration,
) -> (u64, Result<u64, tonic::Status>) {
    let directory = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::open(directory.path()).unwrap());
    let service = YesnoFlightService::new(db.clone()).with_visibility_wait(visibility_wait);
    let (url, stop, task) = serve_with(service).await;
    let mut client = YesnoClient::connect(url).await.unwrap();

    let settled = client
        .insert_batch_acked(vec![(7u64, 1u64)])
        .await
        .unwrap()
        .version
        .expect("the server reports its commit version");
    assert_eq!(
        db.visible(),
        settled,
        "with one committer the watermark is exactly the last commit"
    );
    let pending = settled + 1;

    // The write that will take `pending`, deliberately later than the request
    // below so the request arrives while `pending` is still unassigned.
    let writer = {
        let db = db.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(150));
            let mut b = db.batch();
            b.insert(7, 2);
            b.commit().unwrap().version
        })
    };

    let result = client
        .prepare_query_at(&SetExpr::Key(7), pending)
        .await
        .map(|info| info.version())
        .map_err(|e| match e {
            arrow_flight::error::FlightError::Tonic(status) => *status,
            other => tonic::Status::internal(other.to_string()),
        });
    let took = writer.join().unwrap();
    assert_eq!(
        took, pending,
        "versions are dense, so the delayed commit must take v{pending}"
    );
    println!("asked for v{pending} while visible was {settled} -> {result:?}");

    let _ = stop.send(());
    let _ = task.await;
    (pending, result)
}

/// With the wait, a client reading back its own commit inside the window
/// succeeds.
///
/// Paired with the test below on purpose. This one alone proves nothing: it
/// would pass just as well if the window were never reached, or if
/// `prepare_query_at` had quietly answered from a newer snapshot. The `ZERO`
/// arm is what establishes that the window is real and that the wait is the
/// thing closing it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_read_at_a_not_yet_visible_version_waits_instead_of_refusing() {
    let (pending, result) =
        probe_pending_version(YesnoFlightService::DEFAULT_VISIBILITY_WAIT).await;
    let served = result.expect("a version that is merely not-yet-visible must be waited for");
    assert_eq!(
        served, pending,
        "the wait must serve the version asked for, not a newer one"
    );
}

/// The positive control: with the wait disabled the same request is refused.
///
/// This is the behaviour every remote client had. If this ever starts passing
/// `Ok`, the test above has stopped proving anything and the window has moved —
/// do not delete this one to make a suite green.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn without_the_wait_the_same_read_is_refused_as_not_visible() {
    let (_, result) = probe_pending_version(std::time::Duration::ZERO).await;
    let status = result.expect_err(
        "with no wait, a version above the watermark must be refused — if this \
         succeeded, the request did not actually arrive before the commit",
    );
    let message = status.message().to_string();
    assert!(
        message.contains("not visible"),
        "expected a not-visible refusal, got {:?}: {message}",
        status.code()
    );
}
