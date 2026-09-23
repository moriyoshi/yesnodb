//! The write path must not park a reactor thread.
//!
//! `yesno-flight`'s module header states the rule for reads — a yesno read can
//! take a major page fault on the mmap, so every read goes through
//! `spawn_blocking` onto a bounded channel. The **write** path was doing exactly
//! what that rule forbids, and worse: `WriteBatch::commit` appends to the WAL
//! and fsyncs, and may synchronously trigger a whole checkpoint — serializing
//! dirty chunks, rebuilding the index and syncing again. Under a sustained
//! ingest that is seconds of a parked worker, not microseconds.
//!
//! # Why this test is shaped the way it is
//!
//! **The server gets its own runtime with exactly one worker, and the client
//! stays on the test's runtime.** Both halves of that matter:
//!
//! * one worker is what makes an inline commit *observable* — with two, a
//!   parked worker leaves another to answer and the regression hides;
//! * a separate runtime for the client is what makes the regression **fail
//!   rather than hang**. `tokio::time::timeout` needs a live reactor to fire,
//!   so a timeout armed on the same starved runtime would never resolve, and
//!   the repo's rule is that a broken thing fails loudly rather than stalling.
//!
//! The block itself is injected through `Db::set_checkpoint_hook`, which is a
//! documented testing seam ( "so that hazard has a deterministic regression
//! rather than a threaded one" ). That is what makes this deterministic: there
//! is no sleep anywhere, and the only timeout is the one that reports failure.

use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use arrow_array::UInt64Array;
use arrow_flight::flight_service_client::FlightServiceClient;
use arrow_flight::flight_service_server::FlightServiceServer;
use futures::StreamExt;
use tonic::transport::{Channel, Server};
use yesno_core::checkpoint::CheckpointPolicy;
use yesno_core::{Db, DbOptions};
use yesno_flight::YesnoFlightService;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_committing_do_put_does_not_park_the_reactor() {
    let dir = tempfile::tempdir().unwrap();

    // `dirty_bytes: 1` so the very first commit checkpoints, which is where the
    // hook below lives. Everything else stays at its default.
    let db = Arc::new(
        Db::open_with(
            dir.path(),
            DbOptions {
                shards: 1,
                policy: CheckpointPolicy {
                    dirty_bytes: 1,
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap(),
    );

    // `started` fires when the commit has reached the checkpoint; `release` is
    // what lets it finish. `notify_one` stores a permit when nobody is waiting
    // yet, so there is no race between arming the wait and the hook running.
    let started = Arc::new(tokio::sync::Notify::new());
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let release_rx = Mutex::new(release_rx);
    let started_in_hook = started.clone();
    db.set_checkpoint_hook(Some(Arc::new(move || {
        started_in_hook.notify_one();
        // Blocks the thread the commit is running on — the whole point.
        let _ = release_rx.lock().unwrap().recv();
    })));

    // ---- the server, alone on one worker thread
    let (url_tx, url_rx) = mpsc::channel::<String>();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let db_for_server = db.clone();
    let server = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            url_tx
                .send(format!("http://{}", listener.local_addr().unwrap()))
                .unwrap();
            Server::builder()
                .add_service(FlightServiceServer::new(YesnoFlightService::new(
                    db_for_server,
                )))
                .serve_with_incoming_shutdown(
                    tokio_stream::wrappers::TcpListenerStream::new(listener),
                    async {
                        let _ = stop_rx.await;
                    },
                )
                .await
                .unwrap();
        });
    });
    let url = url_rx.recv().unwrap();

    let ch = Channel::from_shared(url).unwrap().connect().await.unwrap();
    let mut writer = FlightServiceClient::new(ch.clone());
    let mut reader = FlightServiceClient::new(ch);

    // ---- a do_put that will block inside its commit
    let schema = yesno_flight::pairs_schema();
    let keys = UInt64Array::new((0..2_000u64).map(|_| 1u64).collect::<Vec<_>>().into(), None);
    let ords = UInt64Array::new((0..2_000u64).collect::<Vec<_>>().into(), None);
    let batch =
        arrow_array::RecordBatch::try_new(schema.clone(), vec![Arc::new(keys), Arc::new(ords)])
            .unwrap();
    let input = arrow_flight::encode::FlightDataEncoderBuilder::new()
        .with_schema(schema)
        // `do_put` requires an explicit command; it no longer defaults
        // to insert, so that an unrecognised one cannot be applied as one.
        .with_flight_descriptor(Some(arrow_flight::FlightDescriptor::new_cmd(
            yesno_flight::PUT_INSERT.to_vec(),
        )))
        .build(futures::stream::iter(vec![Ok(batch)]))
        .map(|r| r.unwrap());
    let put = tokio::spawn(async move {
        let acked = writer.do_put(input).await?.into_inner();
        let _: Vec<_> = acked.collect().await;
        Ok::<_, tonic::Status>(())
    });

    // The commit is now inside the checkpoint hook, holding whatever thread it
    // is running on.
    started.notified().await;

    // ---- the assertion: the server can still answer
    let answered = tokio::time::timeout(
        Duration::from_secs(10),
        reader.list_actions(arrow_flight::Empty {}),
    )
    .await;
    // Release first, so a failure below tears down cleanly instead of wedging
    // the blocked commit and the server thread with it.
    let _ = release_tx.send(());

    answered
        .expect(
            "the server did not answer while a do_put was committing: the commit is running on a \
             reactor thread, so nothing else on that runtime can make progress",
        )
        .expect("list_actions failed");

    put.await.unwrap().expect("do_put failed");

    let _ = stop_tx.send(());
    server.join().unwrap();

    // The hook holds an `Arc<Db>`-free closure, but drop it anyway so the
    // database is not left with a dangling testing seam if this file grows.
    db.set_checkpoint_hook(None);
}
