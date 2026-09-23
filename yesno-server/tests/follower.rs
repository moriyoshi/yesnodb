//! The standby daemon: keeping up, restarting, and knowing when to stop.
//!
//! **The tests that matter most are the two refusals.** Catching up is the
//! easy direction and would pass with a loop that retried everything forever;
//! what makes a standby trustworthy is that it *stops* on the two conditions a
//! retry cannot fix, and that it tells them apart from the one it can repair.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use futures::StreamExt;
use yesno_core::{Db, DbOptions};
use yesno_server::config::{Config, Role};

fn leader_config(dir: &Path, shards: usize) -> Config {
    let mut c = Config::default();
    c.server.data_dir = Some(dir.to_path_buf());
    c.server.flight.listen = "127.0.0.1:0".into();
    c.server.metrics.listen = "127.0.0.1:0".into();
    c.server.control.listen = "127.0.0.1:0".into();
    c.server.control.journal_dir = dir.join("control-journal");
    c.auth.rules.push(yesno_server::config::AuthzRule {
        channel: yesno_server::config::AuthzChannel::Host,
        principal: "all".into(),
        address: "all".into(),
        capability: yesno_server::config::EndpointCapability::Replication,
        action: yesno_server::config::RuleAction::Allow,
    });
    c.server.shutdown_grace_secs = 10;
    c.db.shards = shards;
    c
}

fn follower_config(dir: &Path, leader: std::net::SocketAddr) -> Config {
    let mut c = Config::default();
    c.server.role = Role::Follower;
    c.server.data_dir = Some(dir.to_path_buf());
    c.server.metrics.listen = "127.0.0.1:0".into();
    c.follower.leader = format!("http://{leader}");
    c.follower.poll_interval_secs = 1;
    c.follower.max_backoff_secs = 1;
    c
}

/// Plaintext replication: `--insecure-replication` is the operator saying so,
/// and these tests are about the *loop*, not the transport. The transport has
/// its own file.
fn validated(c: &Config) -> &Config {
    c.validate_with(false, true).expect("config must validate");
    c
}

async fn wait_for(deadline: Duration, mut f: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + deadline;
    while Instant::now() < end {
        if f() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    f()
}

fn set_of(dir: &Path, shards: usize, key: u64) -> BTreeSet<u64> {
    let db = Db::open_with(
        dir,
        DbOptions {
            shards,
            ..Default::default()
        },
    )
    .expect("the standby's directory must be openable");
    let s = db.snapshot().unwrap().load(key).unwrap().iter().collect();
    s
}

/// A standby catches up, keeps up, and survives its own restart.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_standby_catches_up_keeps_up_and_resumes_after_a_restart() {
    const SHARDS: usize = 2;
    let leader_dir = tempfile::tempdir().unwrap();
    let standby_dir = tempfile::tempdir().unwrap();

    let lcfg = leader_config(leader_dir.path(), SHARDS);
    let leader = yesno_server::start(validated(&lcfg)).await.unwrap();
    let repl = leader.control_addr.unwrap();
    let db = leader.db();

    db.insert_range(1, 0, 5_000).unwrap();
    db.checkpoint().unwrap();
    // **After** the checkpoint, and this is the point. The checkpoint
    // reclaims the redundant WAL, so everything below lives in the image and arrives by
    // `FetchBaseSnapshot`; key 4 exists only as log records and can arrive only
    // by catch-up. Asserting on `records` with a fully checkpointed leader would
    // be asserting that zero is greater than zero — which is how the first
    // draft of this test failed.
    db.insert_range(4, 0, 100).unwrap();

    // ---- follow
    let fcfg = follower_config(standby_dir.path(), repl);
    let node = yesno_server::follower::start(validated(&fcfg)).unwrap();

    assert!(
        wait_for(Duration::from_secs(20), || node
            .status
            .records
            .load(Ordering::Relaxed)
            > 0)
        .await,
        "the standby applied no log records in 20 s"
    );
    assert!(node.status.connected.load(Ordering::Relaxed));
    assert!(!node.is_halted());

    // ---- keep up: write more, and wait for it to arrive
    db.insert_range(2, 10_000, 12_000).unwrap();
    let before = node.status.applied_bytes.load(Ordering::Relaxed);
    assert!(
        wait_for(Duration::from_secs(20), || node
            .status
            .applied_bytes
            .load(Ordering::Relaxed)
            > before)
        .await,
        "the standby did not pick up a later write"
    );

    node.stop().await;

    // ---- restart, and it must resume rather than rebuild
    //
    // This is what `resume_from_disk` exists for. Without it the cursors are
    // lost, the standby asks from zero, the leader answers `FailedPrecondition`
    // and the whole database is copied again — correct, and an image per
    // restart. `rebootstraps` is the instrument: it must stay at zero.
    let node = yesno_server::follower::start(validated(&fcfg)).unwrap();
    assert!(
        wait_for(Duration::from_secs(20), || node
            .status
            .passes
            .load(Ordering::Relaxed)
            > 0)
        .await,
        "the restarted standby completed no pass"
    );
    assert_eq!(
        node.status.rebootstraps.load(Ordering::Relaxed),
        0,
        "the standby re-bootstrapped after a restart instead of resuming from its own logs"
    );
    assert!(!node.is_halted());

    node.stop().await;
    drop(db);
    leader.shutdown().await;

    // ---- and what it holds is what the leader held
    let got = set_of(standby_dir.path(), SHARDS, 1);
    assert_eq!(got.len(), 5_001, "key 1 did not arrive whole");
    assert!(got.contains(&0) && got.contains(&5_000));
    let got2 = set_of(standby_dir.path(), SHARDS, 2);
    assert_eq!(got2.len(), 2_001, "the later write did not arrive");
    // The two routes, both exercised: key 1 through the image, key 4 through the
    // log.
    assert_eq!(
        set_of(standby_dir.path(), SHARDS, 4).len(),
        101,
        "the post-checkpoint write did not arrive through catch-up"
    );
}

/// Pointed at the wrong database, it must write nothing and stop.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_standby_pointed_at_a_different_database_halts() {
    let a_dir = tempfile::tempdir().unwrap();
    let b_dir = tempfile::tempdir().unwrap();
    let standby_dir = tempfile::tempdir().unwrap();

    // Two unrelated databases, each with its own identity.
    let acfg = leader_config(a_dir.path(), 1);
    let a = yesno_server::start(validated(&acfg)).await.unwrap();
    a.db().insert_range(1, 0, 100).unwrap();

    let bcfg = leader_config(b_dir.path(), 1);
    let b = yesno_server::start(validated(&bcfg)).await.unwrap();
    b.db().insert_range(1, 0, 100).unwrap();

    // Seed the standby from A, then point it at B.
    let fa = follower_config(standby_dir.path(), a.control_addr.unwrap());
    let node = yesno_server::follower::start(validated(&fa)).unwrap();
    assert!(
        wait_for(Duration::from_secs(20), || node
            .status
            .records
            .load(Ordering::Relaxed)
            > 0)
        .await,
        "the standby never caught up to A"
    );
    node.stop().await;

    let fb = follower_config(standby_dir.path(), b.control_addr.unwrap());
    let node = yesno_server::follower::start(validated(&fb)).unwrap();

    assert!(
        wait_for(Duration::from_secs(20), || node.is_halted()).await,
        "the standby did not stop when pointed at a different database"
    );
    let why = node.status.halt_reason.lock().unwrap().clone().unwrap();
    assert!(
        why.contains("belongs to database"),
        "the halt does not name its cause: {why}"
    );
    // And it must not have re-bootstrapped from the wrong leader on the way.
    assert_eq!(node.status.rebootstraps.load(Ordering::Relaxed), 0);

    node.stop().await;
    a.shutdown().await;
    b.shutdown().await;
}

/// The repairable one: a leader that reclaimed a generation this standby wanted.
///
/// This is the case that looks like the one above and is its opposite.
/// `FailedPrecondition` means the standby has fallen too far *behind*, which is
/// recoverable by taking a fresh image; `OutOfRange` means it is *ahead*, which
/// is not recoverable and must never be repaired automatically. A loop that
/// treated them alike would either give up on a healthy standby or silently
/// destroy the evidence of a lost write.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_standby_left_behind_by_a_checkpoint_rebuilds_itself() {
    let leader_dir = tempfile::tempdir().unwrap();
    let standby_dir = tempfile::tempdir().unwrap();

    let lcfg = leader_config(leader_dir.path(), 1);
    let leader = yesno_server::start(validated(&lcfg)).await.unwrap();
    let db = leader.db();
    db.insert_range(1, 0, 1_000).unwrap();

    let fcfg = follower_config(standby_dir.path(), leader.control_addr.unwrap());
    let node = yesno_server::follower::start(validated(&fcfg)).unwrap();
    assert!(
        wait_for(Duration::from_secs(20), || node
            .status
            .records
            .load(Ordering::Relaxed)
            > 0)
        .await,
        "the standby never caught up"
    );
    node.stop().await;

    // While it is away: write and checkpoint, which reclaims the generation
    // containing the cursor the standby is holding.
    for i in 0..40u64 {
        db.insert_range(2, i * 1_000, i * 1_000 + 200).unwrap();
    }
    db.checkpoint().unwrap();
    db.insert_range(3, 0, 500).unwrap();

    // Sabotage the standby's cursor into the gap the checkpoint left, which is
    // exactly what a long absence produces.
    let wal = standby_dir.path().join("shard-0000.wal");
    std::fs::write(&wal, b"").unwrap();
    std::fs::remove_file(standby_dir.path().join("shard-0000.yno")).ok();

    let node = yesno_server::follower::start(validated(&fcfg)).unwrap();
    assert!(
        wait_for(Duration::from_secs(30), || node
            .status
            .passes
            .load(Ordering::Relaxed)
            > 0
            && node.status.records.load(Ordering::Relaxed) > 0)
        .await,
        "the standby did not recover from a cut log"
    );
    assert!(!node.is_halted(), "a recoverable gap halted the standby");

    node.stop().await;
    drop(db);
    leader.shutdown().await;

    let got = set_of(standby_dir.path(), 1, 3);
    assert_eq!(
        got.len(),
        501,
        "the standby did not end up with the leader's data"
    );
}

/// `role = "follower"` needs somewhere to follow.
#[test]
fn a_follower_without_a_leader_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = Config::default();
    c.server.role = Role::Follower;
    c.server.data_dir = Some(dir.path().to_path_buf());

    let e = c.validate(false).unwrap_err();
    assert!(format!("{e}").contains("follower.leader"), "{e}");

    c.follower.leader = "a.internal:50052".into();
    let e = c.validate(false).unwrap_err();
    assert!(
        format!("{e}").contains("http"),
        "a bare host:port was accepted: {e}"
    );

    // An https:// leader with no trust material fails at the handshake with
    // something opaque, so it is refused here where the message can be useful.
    c.follower.leader = "https://a.internal:50052".into();
    let e = c.validate(false).unwrap_err();
    assert!(format!("{e}").contains("follower.tls"), "{e}");

    c.follower.tls.ca = Some("/x/ca.pem".into());
    c.validate(false)
        .expect("a CA is enough to verify a leader");

    // Half an identity is worse than none: it looks like mutual TLS.
    c.follower.tls.cert = Some("/x/c.pem".into());
    assert!(c.validate(false).is_err());
}

/// A standby that **serves reads while it follows**.
///
/// The distinguishing assertion is not that the data arrives — a cold standby
/// gets that too — but that it is readable **from the standby, over Flight,
/// without the standby ever closing**. That is the whole difference between this
/// and the cold path, and a test that opened the directory afterwards would pass
/// on either.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_read_serving_standby_answers_queries_while_it_follows() {
    use arrow_flight::flight_service_client::FlightServiceClient;
    use arrow_flight::FlightDescriptor;

    let leader_dir = tempfile::tempdir().unwrap();
    let standby_dir = tempfile::tempdir().unwrap();

    let lcfg = leader_config(leader_dir.path(), 2);
    let leader = yesno_server::start(validated(&lcfg)).await.unwrap();
    let db = leader.db();
    db.insert_range(1, 0, 3_000).unwrap();
    db.checkpoint().unwrap();
    db.insert_range(2, 0, 500).unwrap();

    let mut fcfg = follower_config(standby_dir.path(), leader.control_addr.unwrap());
    fcfg.follower.serve_reads = true;
    fcfg.server.flight.listen = "127.0.0.1:0".into();
    fcfg.db.shards = 2;

    let node = yesno_server::follower::start(validated(&fcfg)).unwrap();
    let slot = node.db.clone().expect("a read-serving standby has a slot");
    let reads = yesno_server::lifecycle::serve_reads(validated(&fcfg), slot)
        .await
        .unwrap();

    // Wait for a **completed pass**, not merely for a record. A sweep opens
    // the database partway through, so `records > 0` can be true while the node
    // is still mid-bootstrap — which is how the first draft of this test raced.
    assert!(
        wait_for(Duration::from_secs(30), || node
            .status
            .passes
            .load(Ordering::Relaxed)
            > 0
            && node.status.records.load(Ordering::Relaxed) > 0)
        .await,
        "the standby completed no pass"
    );
    assert!(
        node.status.serving.load(Ordering::Relaxed),
        "the standby is not holding its database open"
    );

    // ---- read from the standby, over the wire, while it is still following
    let ch = tonic::transport::Channel::from_shared(format!("http://{}", reads.addr))
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut c = FlightServiceClient::new(ch);

    let info = c
        .get_flight_info(FlightDescriptor::new_cmd(1u64.to_le_bytes().to_vec()))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        info.total_records, 3_001,
        "the standby did not serve the image half"
    );
    let info2 = c
        .get_flight_info(FlightDescriptor::new_cmd(2u64.to_le_bytes().to_vec()))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        info2.total_records, 501,
        "the standby did not serve what arrived through catch-up"
    );

    // ---- a write made *now* reaches it without a restart
    db.insert_range(3, 0, 250).unwrap();
    assert!(
        wait_for(Duration::from_secs(30), || {
            let slot = node.db.as_ref().unwrap();
            let g = slot.read().unwrap();
            g.as_ref()
                .map(|d| d.snapshot().unwrap().cardinality(3).unwrap() == 251)
                .unwrap_or(false)
        })
        .await,
        "a live write did not reach the serving standby"
    );

    // And it is a replica: a write is refused with a code the client can act
    // on. `FailedPrecondition`, deliberately not `Internal` — a client told
    // "internal error" retries and pages somebody, when what it needs to do is
    // send the write to the leader.
    {
        let g = node.db.as_ref().unwrap().read().unwrap();
        let d = g.as_ref().unwrap();
        assert!(matches!(
            d.insert(9, 1),
            Err(yesno_core::CodecError::ReadOnlyReplica)
        ));
    }
    let schema = yesno_flight::pairs_schema();
    let batch = arrow_array::RecordBatch::try_new(
        schema.clone(),
        vec![
            std::sync::Arc::new(arrow_array::UInt64Array::new(vec![9u64].into(), None)),
            std::sync::Arc::new(arrow_array::UInt64Array::new(vec![1u64].into(), None)),
        ],
    )
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
    let code = match c.do_put(input).await {
        Err(e) => e.code(),
        Ok(resp) => {
            let mut st = resp.into_inner();
            let mut code = tonic::Code::Ok;
            while let Some(r) = st.next().await {
                if let Err(e) = r {
                    code = e.code();
                }
            }
            code
        }
    };
    assert_eq!(
        code,
        tonic::Code::FailedPrecondition,
        "a write to a replica must name its remedy, not read as a server fault"
    );

    drop(c);
    reads.stop().await;
    node.stop().await;
    drop(db);
    leader.shutdown().await;
}

/// Reads answer `unavailable` while the standby is rebuilding, rather than the
/// port closing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rebuilding_standby_says_so_instead_of_dropping_its_port() {
    use arrow_flight::flight_service_client::FlightServiceClient;
    use arrow_flight::FlightDescriptor;

    let dir = tempfile::tempdir().unwrap();
    // An empty slot is exactly the state a rebuild leaves behind, and it is the
    // state worth pinning: the listener is bound and the database is not there.
    let slot: yesno_server::guard::DbSlot = std::sync::Arc::new(std::sync::RwLock::new(None));

    let mut cfg = Config::default();
    cfg.server.data_dir = Some(dir.path().to_path_buf());
    cfg.server.flight.listen = "127.0.0.1:0".into();
    cfg.server.metrics.listen = "127.0.0.1:0".into();

    let reads = yesno_server::lifecycle::serve_reads(&cfg, slot.clone())
        .await
        .unwrap();
    let ch = tonic::transport::Channel::from_shared(format!("http://{}", reads.addr))
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut c = FlightServiceClient::new(ch);

    let e = c
        .get_flight_info(FlightDescriptor::new_cmd(1u64.to_le_bytes().to_vec()))
        .await
        .expect_err("a node with no database served a read");
    assert_eq!(e.code(), tonic::Code::Unavailable, "{e}");
    assert!(e.message().contains("rebuilding"), "{}", e.message());

    // ---- fill the slot, and the *same connection* starts working
    let db = std::sync::Arc::new(
        yesno_core::Db::open_with(dir.path(), yesno_core::DbOptions::default()).unwrap(),
    );
    db.insert_range(1, 0, 10).unwrap();
    *slot.write().unwrap() = Some(db.clone());

    let info = c
        .get_flight_info(FlightDescriptor::new_cmd(1u64.to_le_bytes().to_vec()))
        .await
        .expect("the listener did not recover when the database came back")
        .into_inner();
    assert_eq!(info.total_records, 11);

    drop(c);
    reads.stop().await;
}

/// A standby whose base image was left half-written repairs itself.
///
/// # The failure this pins
///
/// `bootstrap_shard` used to write the image in place, and `File::create`
/// truncates — so for the whole duration of a copy the file existed and could
/// not be parsed. `resume_from_disk` reads `exists()` as "this shard has a
/// base", so a standby interrupted mid-bootstrap came back, failed to read its
/// own image, and returned before reaching the code that would have replaced
/// it. It then retried the identical bytes about three times a second, for
/// ever, reporting `failed to fill whole buffer` and never serving.
///
/// **Found live, not by review.** A yesnodb operator restarts every Pod once
/// when it learns which EBS volume backs each instance, and on 2026-09-05 that
/// restart landed inside a follower's first bootstrap. The standby never
/// recovered and the arm failed on a read that could not have succeeded.
///
/// The truncation below is the *whole* test. Against the fixed code the
/// standby treats an unreadable image as a shard to bootstrap and comes back;
/// against the old code it halts on it for ever, so this times out.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_standby_repairs_a_half_written_base_image() {
    const SHARDS: usize = 2;
    let leader_dir = tempfile::tempdir().unwrap();
    let standby_dir = tempfile::tempdir().unwrap();

    let lcfg = leader_config(leader_dir.path(), SHARDS);
    let leader = yesno_server::start(validated(&lcfg)).await.unwrap();
    let repl = leader.control_addr.unwrap();
    let db = leader.db();
    db.insert_range(1, 0, 5_000).unwrap();
    db.checkpoint().unwrap();

    let fcfg = follower_config(standby_dir.path(), repl);
    let node = yesno_server::follower::start(validated(&fcfg)).unwrap();
    assert!(
        wait_for(Duration::from_secs(20), || node
            .status
            .passes
            .load(Ordering::Relaxed)
            > 0)
        .await,
        "the standby completed no pass before it was interrupted"
    );
    node.stop().await;

    // ---- what an interrupted copy leaves behind
    //
    // A short file rather than a deleted one: deleting it would be the case
    // that already worked, since `exists()` is false and the shard is simply
    // bootstrapped. The bug is a file that exists and cannot be read.
    let image = standby_dir.path().join("shard-0000.yno");
    let before = std::fs::metadata(&image).unwrap().len();
    assert!(
        before > 128,
        "the standby never wrote a base image, so this test would prove nothing"
    );
    std::fs::OpenOptions::new()
        .write(true)
        .open(&image)
        .unwrap()
        .set_len(128)
        .unwrap();

    // ---- and the standby has to come back from it
    let node = yesno_server::follower::start(validated(&fcfg)).unwrap();
    assert!(
        wait_for(Duration::from_secs(30), || node
            .status
            .passes
            .load(Ordering::Relaxed)
            > 0)
        .await,
        "the standby never completed a pass after its base image was truncated"
    );
    assert!(
        !node.is_halted(),
        "the standby halted on a repairable condition"
    );
    assert!(
        wait_for(Duration::from_secs(30), || {
            std::fs::metadata(&image).map(|m| m.len()).unwrap_or(0) >= before
        })
        .await,
        "the standby did not replace the truncated image"
    );

    node.stop().await;
    drop(db);
    leader.shutdown().await;

    // Repaired, not merely running. A standby that re-created the file and
    // served an empty shard would satisfy every check above.
    let got = set_of(standby_dir.path(), SHARDS, 1);
    assert_eq!(got.len(), 5_001, "the repaired standby lost data");
    assert!(got.contains(&0) && got.contains(&5_000));
}

/// A completed bootstrap leaves no scratch file beside the image.
///
/// Cheap, and it guards the thing the fix introduced rather than the thing
/// it fixed: an image written under a second name and renamed is only correct
/// if the rename actually happens. A `.partial` left in the directory is inert
/// -- nothing reads it -- so nothing else would notice one per bootstrap.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_completed_bootstrap_leaves_no_partial_image() {
    const SHARDS: usize = 2;
    let leader_dir = tempfile::tempdir().unwrap();
    let standby_dir = tempfile::tempdir().unwrap();

    let lcfg = leader_config(leader_dir.path(), SHARDS);
    let leader = yesno_server::start(validated(&lcfg)).await.unwrap();
    let repl = leader.control_addr.unwrap();
    let db = leader.db();
    db.insert_range(1, 0, 1_000).unwrap();
    db.checkpoint().unwrap();

    let fcfg = follower_config(standby_dir.path(), repl);
    let node = yesno_server::follower::start(validated(&fcfg)).unwrap();
    assert!(
        wait_for(Duration::from_secs(20), || node
            .status
            .passes
            .load(Ordering::Relaxed)
            > 0)
        .await,
        "the standby completed no pass"
    );
    node.stop().await;
    drop(db);
    leader.shutdown().await;

    let leftovers: Vec<String> = std::fs::read_dir(standby_dir.path())
        .unwrap()
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".partial"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "bootstrap left scratch files behind: {leftovers:?}"
    );
    for shard in 0..SHARDS {
        assert!(
            standby_dir
                .path()
                .join(format!("shard-{shard:04}.yno"))
                .exists(),
            "shard {shard} has no base image"
        );
    }
}

/// A bootstrapped standby's image is as sparse as its leader's.
///
/// # What this pins
///
/// A shard image is grown to a whole 1 GiB mmap segment with `set_len`, so a
/// database holding kilobytes reports gigabytes and occupies almost nothing.
/// Bootstrap used to copy it byte for byte, which cost the *apparent* size
/// three times over: `tokio::fs::read` of the whole image into the leader's
/// memory, every zero on the wire, and every hole materialized on the
/// follower's disk.
///
/// The leader's memory was the dangerous one — a replica asking to join
/// could OOM-kill the leader it was joining. Found live on 2026-09-05, where
/// a standby's 1 GiB volume filled during its first bootstrap and the
/// unreadable remains were then reported as a corrupt image for three minutes.
///
/// Allocation is the only observable that separates the two implementations:
/// the file's *contents* were always right, which is why every existing
/// replication test passed throughout.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_bootstrapped_image_keeps_its_holes() {
    use std::os::unix::fs::MetadataExt;

    const SHARDS: usize = 2;
    let leader_dir = tempfile::tempdir().unwrap();
    let standby_dir = tempfile::tempdir().unwrap();

    let lcfg = leader_config(leader_dir.path(), SHARDS);
    let leader = yesno_server::start(validated(&lcfg)).await.unwrap();
    let repl = leader.control_addr.unwrap();
    let db = leader.db();
    db.insert_range(1, 0, 5_000).unwrap();
    db.checkpoint().unwrap();

    let fcfg = follower_config(standby_dir.path(), repl);
    let node = yesno_server::follower::start(validated(&fcfg)).unwrap();
    assert!(
        wait_for(Duration::from_secs(30), || node
            .status
            .passes
            .load(Ordering::Relaxed)
            > 0)
        .await,
        "the standby completed no pass"
    );
    node.stop().await;
    drop(db);
    leader.shutdown().await;

    for shard in 0..SHARDS {
        let name = format!("shard-{shard:04}.yno");
        let leader_image = std::fs::metadata(leader_dir.path().join(&name)).unwrap();
        let standby_image = std::fs::metadata(standby_dir.path().join(&name)).unwrap();

        // The length has to match exactly: it is the mmap segment boundary the
        // store rounds up to, and a shorter file is a truncated database.
        assert_eq!(
            standby_image.len(),
            leader_image.len(),
            "{name}: the standby's image is a different length from its leader's"
        );
        // The premise. If the leader's own image were dense this test would
        // pass against either implementation and prove nothing.
        assert!(
            leader_image.blocks() * 512 < leader_image.len() / 8,
            "{name}: the leader's own image is not sparse, so this proves nothing"
        );
        // The assertion. Against a byte-for-byte copy the standby's file is
        // fully allocated and this is the length itself.
        assert!(
            standby_image.blocks() * 512 < leader_image.len() / 8,
            "{name}: the standby materialized its holes — {} bytes allocated of {}",
            standby_image.blocks() * 512,
            standby_image.len()
        );
    }

    // Sparse *and* right. A standby that wrote nothing at all would satisfy
    // every allocation check above.
    let got = set_of(standby_dir.path(), SHARDS, 1);
    assert_eq!(got.len(), 5_001, "the sparse copy lost data");
    assert!(got.contains(&0) && got.contains(&5_000));
}
