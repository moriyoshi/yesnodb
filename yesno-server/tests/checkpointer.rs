//! The interval trigger, which does not fire without a daemon.
//!
//! **`CheckpointPolicy::interval_secs` is dead config in `yesno-core` alone**,
//! and not only on a replica. `Db::enforce_policy` is reached from
//! `WriteBatch::commit` and nowhere else, and the engine starts no background
//! threads in production — so on a database whose writes stop, the interval
//! never elapses into anything. Dirty chunks stay in the memtable and the log
//! keeps what it was holding, for as long as the process lives.
//!
//! That is the precise hole `maintenance::Ticker` exists to fill, so the test
//! for it has to be shaped to see exactly that: **write once, then stop
//! writing.** A test that keeps writing would be checkpointed by the write path
//! and would pass with the ticker deleted.

use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use yesno_core::Db;
use yesno_server::config::Config;

fn config_for(dir: &Path, interval_secs: u64) -> Config {
    let mut c = Config::default();
    c.server.data_dir = Some(dir.to_path_buf());
    c.server.flight.listen = "127.0.0.1:0".into();
    c.server.metrics.listen = "127.0.0.1:0".into();
    c.server.shutdown_grace_secs = 10;
    c.db.shards = 1;
    c.db.checkpoint.interval_secs = interval_secs;
    c
}

fn wal_len(dir: &Path, shard: u32) -> u64 {
    let (base, end) =
        yesno_core::wal::log_bounds(dir.join(format!("shard-{shard:04}.wal")), 0).unwrap();
    end - base
}

/// Write once, stop, and wait. A checkpoint must happen anyway.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_idle_database_still_checkpoints() {
    let dir = tempfile::tempdir().unwrap();
    let running = yesno_server::start(&config_for(dir.path(), 1))
        .await
        .unwrap();
    let counters = running.counters();
    let db = running.db();

    // One write, through the engine directly — this is about the *timer*, not
    // about the network.
    db.insert_many(1, &(0..5_000u64).map(|i| i * 3).collect::<Vec<_>>())
        .unwrap();
    assert!(
        db.dirty_bytes() > 0,
        "nothing is dirty, so there would be nothing for a checkpoint to do"
    );
    assert!(
        wal_len(dir.path(), 0) > 0,
        "the log is empty, so reclaiming it at checkpoint would prove nothing"
    );
    // And nothing else writes from here on. That is the whole design of the
    // test: the write path must not be what triggers the checkpoint.

    let deadline = Instant::now() + Duration::from_secs(20);
    while counters.checkpoints.load(Ordering::Relaxed) == 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    assert!(
        counters.checkpoints.load(Ordering::Relaxed) > 0,
        "no background checkpoint ran in 20 s with interval_secs = 1; \
         CheckpointPolicy::interval_secs only fires if something outside the engine calls in"
    );
    assert_eq!(
        counters.checkpoint_failures.load(Ordering::Relaxed),
        0,
        "the background checkpoint failed"
    );
    assert_eq!(
        db.dirty_bytes(),
        0,
        "the checkpoint ran but left the memtable dirty"
    );
    assert_eq!(
        wal_len(dir.path(), 0),
        0,
        "the checkpoint ran but did not reclaim the redundant WAL"
    );

    drop(db);
    drop(counters);
    let t = running.shutdown().await;
    assert!(t.is_clean());
}

/// `interval_secs = 0` reads as "off" and means "always". The config refuses
/// it, and this pins **both halves** of why — the refusal, and the engine
/// behaviour that makes the refusal necessary.
///
/// Without the second half this is just a test of an arbitrary rule, and the
/// day someone decides zero ought to mean "disabled" they would delete the
/// check and ship a database that takes a full checkpoint on every single
/// write.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_zero_checkpoint_interval_is_refused_because_the_engine_reads_it_as_always() {
    // Half one: the daemon will not start with it.
    let dir = tempfile::tempdir().unwrap();
    let cfg = config_for(dir.path(), 0);
    let e = cfg
        .validate(false)
        .expect_err("interval_secs = 0 was accepted");
    let msg = format!("{e}");
    assert!(msg.contains("every commit"), "{msg}");

    // Half two: what it would have done. `should_checkpoint` is
    // `.. || elapsed_secs >= interval_secs`, so zero makes that arm
    // unconditionally true.
    let policy = yesno_core::checkpoint::CheckpointPolicy {
        interval_secs: 0,
        ..Default::default()
    };
    assert!(
        policy.should_checkpoint(0, 0, 0),
        "with interval_secs = 0 the elapsed-time trigger must fire even at zero elapsed \
         and zero dirty bytes; if this ever stops being true, the config check above is \
         the thing to revisit"
    );

    // And end to end: every commit checkpoints, so nothing stays dirty.
    let db = Db::open_with(
        dir.path(),
        yesno_core::DbOptions {
            shards: 1,
            policy,
            ..Default::default()
        },
    )
    .unwrap();
    db.insert_many(1, &(0..1_000u64).collect::<Vec<_>>())
        .unwrap();
    assert_eq!(
        db.dirty_bytes(),
        0,
        "a commit under interval_secs = 0 left the memtable dirty, so the trigger did not \
         fire on every commit after all"
    );
}

/// `/metrics` must answer, and its numbers must be the database's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_metrics_endpoint_reports_the_databases_own_counters() {
    let dir = tempfile::tempdir().unwrap();
    let running = yesno_server::start(&config_for(dir.path(), 0))
        .await
        .unwrap();
    let db = running.db();
    db.insert_many(7, &(0..2_000u64).collect::<Vec<_>>())
        .unwrap();

    let addr = running
        .metrics_addr
        .expect("the metrics listener is enabled");
    let body = get(addr, "/metrics").await;

    assert!(body.contains("yesnod_build_info"), "{body}");
    assert!(
        body.contains(&format!("yesnod_shards {}", db.shard_count())),
        "the shard count is not the database's own: {body}"
    );
    assert!(
        body.contains(&format!("yesnod_visible_version {}", db.visible())),
        "the visible version is not the database's own: {body}"
    );
    // Prometheus refuses a payload whose HELP/TYPE lines do not pair with a
    // sample, so a malformed renderer is a silent scrape failure. Cheap to pin.
    let helps = body.matches("# HELP ").count();
    let types = body.matches("# TYPE ").count();
    assert_eq!(helps, types, "unbalanced HELP and TYPE lines:\n{body}");

    assert_eq!(get(addr, "/healthz").await.trim(), "ok");
    assert_eq!(get(addr, "/readyz").await.trim(), "ready");

    drop(db);
    running.shutdown().await;
}

/// A hand-rolled HTTP/1.0 GET. Deliberately not a client dependency: this
/// crate has no HTTP client and adding one to read three lines of text would be
/// a dependency earning nothing.
async fn get(addr: std::net::SocketAddr, path: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(format!("GET {path} HTTP/1.0\r\nHost: localhost\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut buf = String::new();
    s.read_to_string(&mut buf).await.unwrap();
    let (head, body) = buf.split_once("\r\n\r\n").expect("no header terminator");
    // Match on the status code, not the version: the response echoes the
    // HTTP/1.0 of the request above, so pinning "HTTP/1.1 200" would fail on a
    // perfectly good answer.
    let status = head.lines().next().unwrap_or_default();
    assert!(status.contains(" 200 "), "{path} answered: {status}");
    body.to_owned()
}
