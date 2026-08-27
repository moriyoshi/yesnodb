//! Starting is easy. These are about **stopping**.
//!
//! `yesno-core` has no `Db::close()`: teardown is `Drop` on the last
//! `Arc<DbInner>`, and the database's exclusive `flock` lives inside it. So the
//! daemon cannot *call* a shutdown, it has to *prove* one — and the only proof
//! that is not a restatement of the code is behavioural:
//!
//! **after shutdown, the same process can reopen the directory.**
//!
//! `Db::open_with` takes a non-blocking `try_lock` and answers `AlreadyOpen`
//! otherwise, so a successful reopen is a mechanical demonstration that the
//! descriptor closed and every `Arc<DbInner>` — including any hiding inside a
//! `Snapshot`, which holds one directly and is invisible to `Arc<Db>`'s
//! refcount — is gone.
//!
//! The second test is the first one's sabotage check, made permanent. A
//! shutdown test that keeps passing when shutdown is broken is worse than no
//! test, and this repo has a written record of that exact failure mode.

use std::path::Path;
use std::sync::Arc;

use arrow_flight::flight_service_client::FlightServiceClient;
use arrow_flight::FlightDescriptor;
use futures::StreamExt;
use yesno_core::{Db, DbOptions};
use yesno_server::config::{AuthzRule, Config, EndpointCapability, RuleAction};

fn config_for(dir: &Path) -> Config {
    let mut c = Config::default();
    c.server.data_dir = Some(dir.to_path_buf());
    // Port 0: the OS picks, and `Running::flight_addr` reports what it picked.
    // Both listeners, not just Flight. The metrics default is a fixed port,
    // so two of these tests running concurrently — which is the default — would
    // collide on it and fail with "address in use" for a reason that has
    // nothing to do with what they are testing.
    c.server.flight.listen = "127.0.0.1:0".into();
    c.server.control.listen = "127.0.0.1:0".into();
    c.server.control.journal_dir = dir.join("control");
    c.server.metrics.listen = "127.0.0.1:0".into();
    c.server.shutdown_grace_secs = 10;
    c.db.shards = 2;
    c.auth.rules = vec![AuthzRule {
        channel: yesno_server::config::AuthzChannel::Host,
        principal: "all".into(),
        address: "127.0.0.0/8".into(),
        capability: EndpointCapability::ControlAdmin,
        action: RuleAction::Allow,
    }];
    c
}

fn opts() -> DbOptions {
    DbOptions {
        shards: 2,
        ..Default::default()
    }
}

async fn client(addr: std::net::SocketAddr) -> FlightServiceClient<tonic::transport::Channel> {
    let ch = tonic::transport::Channel::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect()
        .await
        .unwrap();
    FlightServiceClient::new(ch)
}

/// The whole lifecycle, ending in the assertion that matters.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_clean_shutdown_releases_the_database_lock() {
    let dir = tempfile::tempdir().unwrap();
    let running = yesno_server::start(&config_for(dir.path())).await.unwrap();
    let addr = running.flight_addr;

    // Do real work, so this is not a test of an idle server.
    let mut c = client(addr).await;
    let schema = yesno_flight::pairs_schema();
    let keys = arrow_array::UInt64Array::new(vec![7u64; 500].into(), None);
    let ords = arrow_array::UInt64Array::new((0..500u64).collect::<Vec<_>>().into(), None);
    let batch =
        arrow_array::RecordBatch::try_new(schema.clone(), vec![Arc::new(keys), Arc::new(ords)])
            .unwrap();
    let input = arrow_flight::encode::FlightDataEncoderBuilder::new()
        .with_schema(schema)
        .build(futures::stream::iter(vec![Ok(batch)]))
        .map(|r| r.unwrap());
    let _: Vec<_> = c.do_put(input).await.unwrap().into_inner().collect().await;

    // And a read, because `do_get` is what registers a reader slot — the thing
    // the drain in step 2 of teardown exists for.
    let info = c
        .get_flight_info(FlightDescriptor::new_cmd(7u64.to_le_bytes().to_vec()))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        info.total_records, 500,
        "the exact row count did not come back from the index"
    );
    let ticket = info.endpoint[0].ticket.clone().unwrap();
    let got: Vec<_> = c.do_get(ticket).await.unwrap().into_inner().collect().await;
    assert!(!got.is_empty(), "do_get returned nothing to drain");
    drop(c);

    let t = running.shutdown().await;
    assert_eq!(t.readers_left, 0, "readers did not drain");
    assert!(t.sole_owner, "something still held a database handle");
    assert!(t.is_clean());
    assert!(
        t.checkpoint.is_some(),
        "the final checkpoint did not run, so a restart would replay more log than it \
         needs to"
    );

    // The final checkpoint reclaimed the redundant WAL. Measured **on disk, before
    // reopening**, and that ordering is the whole trick: `Db::open` writes an
    // `EpochFence` into every shard's log, so a `wal_bytes()` taken after the
    // reopen is never zero and says nothing about what shutdown achieved. What
    // is on the files right now is what the final checkpoint left.
    for s in 0..2 {
        let wal = dir.path().join(format!("shard-{s:04}.wal"));
        let len = std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0);
        assert_eq!(
            len, 0,
            "shard {s}'s log still holds {len} bytes after a clean shutdown, so the final \
             checkpoint did not run and a restart would replay work it need not"
        );
    }

    // ---- the proof
    let db = Db::open_with(dir.path(), opts()).expect(
        "the directory could not be reopened after shutdown: the exclusive lock is still \
         held, so some Arc<DbInner> outlived teardown",
    );
    let snap = db.snapshot().unwrap();
    assert_eq!(
        snap.cardinality(7).unwrap(),
        500,
        "the ingest did not survive shutdown"
    );
}

/// The detector must detect. This is the sabotage check for the test above,
/// kept rather than performed once by hand.
///
/// A leaked `Arc<Db>` is the realistic failure — a task that outlives the
/// listener, a handle parked in a struct — and its consequence is precise and
/// nasty: the *next* start fails with `AlreadyOpen` and nothing in the previous
/// run's output says why. So teardown reports it, and this pins both halves:
/// that `sole_owner` goes false, and that the reopen really does fail.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_leaked_database_handle_is_reported_rather_than_silent() {
    let dir = tempfile::tempdir().unwrap();
    let running = yesno_server::start(&config_for(dir.path())).await.unwrap();

    // Exactly what a stray task would hold.
    let leaked = running.db();

    let t = running.shutdown().await;
    assert!(
        !t.sole_owner,
        "teardown reported a clean shutdown while a handle was still outstanding; the \
         leak detector is not detecting"
    );
    assert!(!t.is_clean());

    assert!(
        Db::open_with(dir.path(), opts()).is_err(),
        "the directory reopened while a handle was still held, which would mean the lock \
         is not doing its job"
    );

    // Releasing it must be sufficient: this is what proves the leaked handle
    // was the *only* thing holding the lock, so the report named the real cause.
    drop(leaked);
    Db::open_with(dir.path(), opts()).expect("dropping the leaked handle did not release the lock");
}

/// One process per database directory, and the message has to say so.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_server_on_the_same_directory_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let running = yesno_server::start(&config_for(dir.path())).await.unwrap();

    let msg = match yesno_server::start(&config_for(dir.path())).await {
        Ok(_) => panic!("two servers opened the same directory, which is unbounded corruption"),
        Err(e) => format!("{e}"),
    };
    assert!(
        msg.contains("already open"),
        "the refusal does not name its cause: {msg}"
    );

    running.shutdown().await;
}

/// `checkpoint` is reachable and returns a watermark — which is also the reason
/// authentication is not optional for long: this forces real I/O on demand and
/// nothing authenticates it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_checkpoint_rpc_answers_with_a_watermark() {
    let dir = tempfile::tempdir().unwrap();
    let running = yesno_server::start(&config_for(dir.path())).await.unwrap();
    let channel =
        tonic::transport::Channel::from_shared(format!("http://{}", running.control_addr.unwrap()))
            .unwrap()
            .connect()
            .await
            .unwrap();
    let mut control =
        yesno_server::control::pb::control_plane_client::ControlPlaneClient::new(channel);
    let response = control
        .checkpoint(yesno_server::control::pb::CheckpointRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        response.watermark, 0,
        "an empty database checkpoints at its initial watermark"
    );

    drop(control);
    running.shutdown().await;
}
