//! Serving a follower must cost a *batch*, not a *log*.
//!
//! # Why this test has a file to itself
//!
//! **Its instrument needs a private process, and a test file is the only way
//! to get one.** The measurement is `rchar` from `/proc/self/io` — bytes handed
//! to read syscalls, whether or not they hit the page cache, which is exactly
//! the quantity that regressed and is exact rather than statistical. But that
//! counter is **process-wide**, and `cargo test` runs one file's tests
//! concurrently in one process. Cargo gives each test *target* its own process,
//! so a file of one test is a counter of one test.
//!
//! This is not hypothetical tidiness. Living among thirteen neighbours in
//! `replication_steady_state.rs`, the assertion went red on 2026-09-06 while
//! the poll loop was innocent: the file's other tests build multi-megabyte logs
//! and images, and their reads land inside the window. It passed alone and
//! under `--test-threads=1`, which is the signature.
//!
//! Taking the minimum of several attempts does **not** fix it and was tried:
//! with thirteen tests on several cores something is always reading, so the
//! floor is contaminated too. The fix is isolation, not statistics.
//!
//! Do not move this test back in with others, and do not add a second test
//! to this file unless it does no I/O of its own.

mod replication_common;

use replication_common::{opts, serve};
use yesno_core::Db;
use yesno_server::replication::pb;

fn wal_len(dir: &std::path::Path, shard: u32) -> u64 {
    let (base, end) =
        yesno_core::wal::log_bounds(dir.join(format!("shard-{shard:04}.wal")), 0).unwrap();
    end - base
}

/// Until this landed, `subscribe`'s poll loop did `tokio::fs::read` of the
/// whole log — every 50 ms, per shard, per follower. `CheckpointPolicy` lets a
/// retained WAL reach a gigabyte before a checkpoint reclaims it, so an *idle*
/// follower cost tens of gigabytes per second of page-cache traffic. Nothing
/// failed; it just did not scale, which is why no existing test saw it: they
/// all run against logs of a few kilobytes.
///
/// Linux-only for the obvious reason. The behaviour is not platform-specific;
/// the instrument is.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serving_a_follower_reads_a_batch_not_the_whole_log() {
    fn rchar() -> u64 {
        let s = std::fs::read_to_string("/proc/self/io").unwrap();
        for line in s.lines() {
            if let Some(v) = line.strip_prefix("rchar: ") {
                return v.trim().parse().unwrap();
            }
        }
        panic!("no rchar in /proc/self/io");
    }

    const BATCH: u32 = 32 << 10;

    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_with(dir.path(), opts(1)).unwrap();
    // A log far larger than one batch, and deliberately never checkpointed so
    // it is all still there to be re-read.
    //
    // **One** `WriteBatch`, and a stride. Written as 40 000 separate
    // `insert` calls this took 143 seconds — a commit each, so an fsync each —
    // and still produced under a megabyte. The stride is what makes the log
    // large without making it slow: it spreads the ordinals over thousands of
    // chunks, and a chunk is a record. Keeping each record far below `BATCH`
    // also keeps this a test of the *poll loop* rather than of `batch_from`'s
    // oversized-record arm, which has its own test elsewhere.
    let mut wb = db.batch();
    for i in 0..800_000u64 {
        wb.insert(1, i * 1024);
    }
    wb.commit().unwrap();

    let log = wal_len(dir.path(), 0);
    assert!(
        log > 32 * BATCH as u64,
        "the log is only {log} bytes, so a whole-file read would not stand out"
    );

    let mut leader = serve(dir.path(), 1).await;

    // One batch, from the start, measured. A single measurement, which is
    // only sound because this file holds one test — see the module header.
    let before = rchar();
    let mut stream = leader
        .client
        .subscribe(pb::SubscribeRequest {
            shard: 0,
            after_lsn: 0,
            max_batch_bytes: BATCH,
        })
        .await
        .unwrap()
        .into_inner();
    let first = stream.message().await.unwrap().expect("a batch");
    let read = rchar() - before;

    assert!(!first.is_heartbeat, "the leader shipped nothing to measure");
    assert!(
        first.records.len() <= BATCH as usize,
        "the batch itself broke its budget: {} > {BATCH}",
        first.records.len()
    );
    // The whole point: bounded by the batch, not by the log.
    assert!(
        read < log / 4,
        "serving one {BATCH}-byte batch read {read} bytes of a {log}-byte log; \
         the poll loop is reading the whole file again"
    );

    drop(stream);
}
