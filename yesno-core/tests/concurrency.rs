//! Multiple writers, which nothing else in the suite exercises.
//!
//! Every other test file drives a single thread. That leaves the whole reason
//! the database is sharded untested: concurrent commits, the ascending shard
//! lock order that keeps multi-shard batches from deadlocking, late version
//! assignment under contention, and the watermark's consecutive-prefix rule
//! when versions resolve out of order.
//!
//! It is also the prerequisite for measuring two things at all. Group commit
//! cannot be observed with one writer — there is never a second waiter to batch
//! — and the WAL rollover guard ( "the physical end still matches the checkpoint
//! snapshot" ) is *always* true single-threaded, so a single-writer test can
//! neither see it fail nor prove it holds.
//!
//! # What these assert, and what they cannot
//!
//! These are stress tests, not a model checker. A pass means the interleavings
//! that happened to occur were correct; it is not proof that all of them are.
//! Where a property is checkable deterministically — every write present, a
//! snapshot never showing half a batch — it is asserted exactly. Timing-derived
//! numbers are deliberately absent: a flaky assertion about how fast something
//! happened is worse than no assertion.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

use yesno_core::{Db, DbOptions};

struct CleanDir(PathBuf);
impl Drop for CleanDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tmpdir(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("yesno-conc-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    p
}

/// Start every thread at once, so they contend rather than queue.
fn race<T: Send + 'static>(n: usize, f: impl Fn(usize) -> T + Send + Sync + 'static) -> Vec<T> {
    let f = Arc::new(f);
    let gate = Arc::new(Barrier::new(n));
    let handles: Vec<_> = (0..n)
        .map(|i| {
            let f = f.clone();
            let gate = gate.clone();
            thread::spawn(move || {
                gate.wait();
                f(i)
            })
        })
        .collect();
    handles.into_iter().map(|h| h.join().unwrap()).collect()
}

#[test]
fn concurrent_writers_to_distinct_keys_all_land() {
    let dir = tmpdir("distinct");
    let _c = CleanDir(dir.clone());
    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 4,
            ..Default::default()
        },
    )
    .unwrap();

    let threads = 8usize;
    let per_thread = 50u64;
    {
        let db = db.clone();
        race(threads, move |t| {
            for k in 0..per_thread {
                let key = t as u64 * 1_000 + k;
                db.insert_many(
                    key,
                    &(0..40u64).map(|i| key * 10_000 + i * 3).collect::<Vec<_>>(),
                )
                .unwrap();
            }
        });
    }

    let snap = db.snapshot().unwrap();
    for t in 0..threads as u64 {
        for k in 0..per_thread {
            let key = t * 1_000 + k;
            let want: Vec<u64> = (0..40u64).map(|i| key * 10_000 + i * 3).collect();
            assert_eq!(
                snap.load(key).unwrap().iter().collect::<Vec<_>>(),
                want,
                "key {key} lost or corrupted under concurrent writers"
            );
        }
    }
}

/// The sharp case: many threads read-modify-writing the **same** chunk.
///
/// Every insert reads the current container, adds an ordinal, and writes it
/// back. Without serialization inside the shard, two threads read the same base
/// and one write is lost — silently, because both return success.
#[test]
fn concurrent_writers_to_one_key_do_not_lose_updates() {
    let dir = tmpdir("same-key");
    let _c = CleanDir(dir.clone());
    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 4,
            ..Default::default()
        },
    )
    .unwrap();

    let threads = 8usize;
    let per_thread = 200u64;
    {
        let db = db.clone();
        race(threads, move |t| {
            // Disjoint ordinals in one chunk, so every insert touches the same
            // container and they can only interleave, never conflict logically.
            for i in 0..per_thread {
                db.insert(7, t as u64 * per_thread + i).unwrap();
            }
        });
    }

    let want: BTreeSet<u64> = (0..threads as u64 * per_thread).collect();
    let got: BTreeSet<u64> = db.snapshot().unwrap().load(7).unwrap().iter().collect();
    assert_eq!(
        got.len(),
        want.len(),
        "{} of {} inserts survived; a read-modify-write was lost",
        got.len(),
        want.len()
    );
    assert_eq!(got, want);
}

/// Multi-shard batches from several threads must not deadlock.
///
/// A batch takes every participating shard's lock. Taking them in *any* order
/// other than a single global one lets two batches hold each other's next lock
/// — and the failure is a hang, not an error, so a deadlock here would look
/// like a test that never finishes rather than one that fails.
#[test]
fn multi_shard_batches_do_not_deadlock() {
    let dir = tmpdir("multishard");
    let _c = CleanDir(dir.clone());
    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 8,
            ..Default::default()
        },
    )
    .unwrap();

    let done = Arc::new(AtomicBool::new(false));
    let threads = 8usize;
    let rounds = 60u64;

    {
        let db = db.clone();
        // Each thread writes keys chosen to span many shards, in an order that
        // differs per thread — which is exactly the shape that deadlocks if the
        // lock order is per-batch rather than global.
        race(threads, move |t| {
            for r in 0..rounds {
                let mut b = db.batch();
                for j in 0..6u64 {
                    let key = ((t as u64 + j * 3) * 7 + r) % 64;
                    b.insert(key, t as u64 * 10_000 + r * 10 + j);
                }
                b.commit().unwrap();
            }
        });
    }
    done.store(true, Ordering::Release);

    // Every ordinal written must be present exactly once.
    let snap = db.snapshot().unwrap();
    let mut total = 0u64;
    for key in 0..64u64 {
        total += snap.cardinality(key).unwrap();
    }
    assert_eq!(
        total,
        threads as u64 * rounds * 6,
        "ordinals were lost across multi-shard batches"
    );
}

/// A snapshot must never show part of a batch.
///
/// A multi-shard commit becomes visible atomically or not at all: the watermark
/// only advances over resolved versions, so a reader either sees every shard's
/// half or none of it. This is the property that would break if a commit
/// published to some shards before the version was durable everywhere.
#[test]
fn a_snapshot_never_observes_half_a_multi_shard_batch() {
    let dir = tmpdir("atomicity");
    let _c = CleanDir(dir.clone());
    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 8,
            ..Default::default()
        },
    )
    .unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let torn = Arc::new(AtomicU64::new(0));
    // A reader that samples rarely would pass this test without ever looking at
    // an interleaving. Counting the samples is what stops it being vacuous.
    let samples = Arc::new(AtomicU64::new(0));

    // Writer: batches that always touch four keys together, tagged by round.
    let w = {
        let db = db.clone();
        let stop = stop.clone();
        thread::spawn(move || {
            for r in 0..3_000u64 {
                let mut b = db.batch();
                for j in 0..4u64 {
                    b.insert(j, r);
                }
                b.commit().unwrap();
            }
            stop.store(true, Ordering::Release);
        })
    };

    // Reader: every snapshot must show the same count for all four keys.
    let r = {
        let db = db.clone();
        let stop = stop.clone();
        let torn = torn.clone();
        let samples = samples.clone();
        thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                let snap = db.snapshot().unwrap();
                let counts: Vec<u64> = (0..4u64).map(|k| snap.cardinality(k).unwrap()).collect();
                samples.fetch_add(1, Ordering::Relaxed);
                if counts.iter().any(|c| *c != counts[0]) {
                    torn.fetch_add(1, Ordering::Relaxed);
                }
            }
        })
    };

    w.join().unwrap();
    r.join().unwrap();

    let n = samples.load(Ordering::Relaxed);
    assert!(
        n > 100,
        "the reader only sampled {n} times; this test cannot see a torn batch it \
         never looked for"
    );
    assert_eq!(
        torn.load(Ordering::Relaxed),
        0,
        "a snapshot observed a partially applied multi-shard batch ({n} samples)"
    );
}

/// Checkpoints must be safe to run while writers are working.
///
/// The checkpointer takes each shard's write lock to snapshot its memtable, and
/// then does all its I/O outside them. A writer racing that must neither lose a
/// write nor see one appear twice.
#[test]
fn checkpoints_concurrent_with_writers_lose_nothing() {
    let dir = tmpdir("ckpt-race");
    let _c = CleanDir(dir.clone());
    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 4,
            ..Default::default()
        },
    )
    .unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let ck = {
        let db = db.clone();
        let stop = stop.clone();
        thread::spawn(move || {
            let mut n = 0u64;
            while !stop.load(Ordering::Acquire) {
                db.checkpoint().unwrap();
                n += 1;
            }
            n
        })
    };

    let threads = 4usize;
    let per_thread = 300u64;
    {
        let db = db.clone();
        race(threads, move |t| {
            for i in 0..per_thread {
                db.insert(t as u64, i).unwrap();
            }
        });
    }
    stop.store(true, Ordering::Release);
    let checkpoints = ck.join().unwrap();
    assert!(checkpoints > 0, "the checkpointer never ran");

    db.checkpoint().unwrap();
    drop(db);

    // Everything must be durable, which is the point of racing them.
    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 4,
            ..Default::default()
        },
    )
    .unwrap();
    let snap = db.snapshot().unwrap();
    for t in 0..threads as u64 {
        assert_eq!(
            snap.cardinality(t).unwrap(),
            per_thread,
            "key {t} lost writes to a concurrent checkpoint"
        );
    }
}

/// A reader holding a snapshot across concurrent writes and checkpoints must
/// keep seeing its own version.
#[test]
fn a_snapshot_is_stable_while_writers_and_checkpoints_run() {
    let dir = tmpdir("stable-snap");
    let _c = CleanDir(dir.clone());
    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 4,
            ..Default::default()
        },
    )
    .unwrap();

    for k in 0..8u64 {
        db.insert_many(
            k,
            &(0..500u64).map(|i| k * 100_000 + i * 3).collect::<Vec<_>>(),
        )
        .unwrap();
    }
    db.checkpoint().unwrap();

    let snap = db.snapshot().unwrap();
    let baseline: Vec<Vec<u64>> = (0..8u64)
        .map(|k| snap.load(k).unwrap().iter().collect())
        .collect();

    let stop = Arc::new(AtomicBool::new(false));
    let ck = {
        let db = db.clone();
        let stop = stop.clone();
        thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                db.checkpoint().unwrap();
            }
        })
    };
    {
        let db = db.clone();
        race(4, move |t| {
            for i in 0..400u64 {
                db.insert(t as u64, 9_000_000 + i).unwrap();
            }
        });
    }
    stop.store(true, Ordering::Release);
    ck.join().unwrap();

    for k in 0..8u64 {
        assert_eq!(
            snap.load(k).unwrap().iter().collect::<Vec<_>>(),
            baseline[k as usize],
            "key {k} changed underneath a held snapshot"
        );
    }
}

/// What concurrency actually costs, measured rather than assumed.
///
/// Two open questions needed a multi-writer harness before they could be
/// answered at all, and this is that measurement rather than an assertion about
/// a target. It pins the *current* behaviour so a change to either is visible.
///
/// **fsyncs per commit.** Was 1.00 — every commit paid for its own durability
/// even with a dozen threads waiting on the same device. With the group-commit
/// leader in `wal::group` it measures about 0.55 here. The assertion guards the
/// direction rather than the number, because how much batching happens depends
/// on how often threads actually overlap.
///
/// **Log truncation.** The log is cut at a checkpoint only when nothing arrived
/// while it ran. Single-threaded that is always true, which is why it looked
/// fine; under contention it may rarely hold, and then the log grows across
/// checkpoints instead of being reclaimed.
#[test]
fn concurrent_commit_cost_is_measured_not_assumed() {
    let dir = tmpdir("commit-cost");
    let _c = CleanDir(dir.clone());
    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 4,
            ..Default::default()
        },
    )
    .unwrap();

    let threads = 8usize;
    let per_thread = 250u64;
    let commits = threads as u64 * per_thread;
    {
        let db = db.clone();
        race(threads, move |t| {
            for i in 0..per_thread {
                db.insert(t as u64 % 4, t as u64 * 10_000 + i).unwrap();
            }
        });
    }

    let syncs = db.wal_syncs();
    eprintln!(
        "  {commits} single-shard commits -> {syncs} fsyncs ({:.2} per commit)",
        syncs as f64 / commits as f64
    );
    // One sync per commit is the *un-batched* cost, and was the measurement
    // before group commit landed: 2 000 commits, 2 000 fsyncs. With a leader
    // batching concurrent waiters it measures around 0.55.
    //
    // The bound is deliberately loose. How much batching happens depends on how
    // often threads overlap, which depends on the machine; a threshold tuned to
    // this one would be a flaky test on another. What it has to catch is a
    // return to one-sync-per-commit.
    assert!(
        syncs * 10 < commits * 9,
        "{syncs} fsyncs for {commits} concurrent commits ({:.2} each): commits are \
         not sharing fsyncs",
        syncs as f64 / commits as f64
    );

    // Now the log's size under the contention that used to defeat reclamation.
    // The question is not whether a checkpoint leaves the log *empty* — it keeps
    // whatever arrived while it ran — but whether the log stays bounded instead
    // of accumulating every write the database has taken.
    let stop = Arc::new(AtomicBool::new(false));
    let ck = {
        let db = db.clone();
        let stop = stop.clone();
        thread::spawn(move || {
            let mut ran = 0u64;
            let mut peak = 0u64;
            while !stop.load(Ordering::Acquire) {
                db.checkpoint().unwrap();
                ran += 1;
                peak = peak.max(db.wal_bytes());
            }
            (ran, peak)
        })
    };
    {
        let db = db.clone();
        race(4, move |t| {
            for i in 0..400u64 {
                db.insert(t as u64, 5_000_000 + i).unwrap();
            }
        });
    }
    stop.store(true, Ordering::Release);
    let (ran, peak) = ck.join().unwrap();
    let written = db.wal_syncs(); // one record group per commit, near enough
    eprintln!("  {ran} concurrent checkpoints, peak log {peak} bytes over {written} commits");
    // 1 600 concurrent inserts. If the prefix were not being cut, the log would
    // hold all of them; bounded means it holds roughly what arrives during one
    // checkpoint instead.
    assert!(
        peak < 256 << 10,
        "log peaked at {peak} bytes under concurrent writers; the prefix is not \
         being reclaimed across checkpoints"
    );

    // A final quiet checkpoint must always cut it — that is the path a
    // single-writer test exercises, and it must not have regressed.
    db.checkpoint().unwrap();
    assert_eq!(
        db.wal_bytes(),
        0,
        "a checkpoint with no concurrent writer must always truncate the log"
    );
}

/// The log must stay bounded when checkpoints are rare and writes never stop.
///
/// The log is cut at a checkpoint only when nothing arrived while it ran. That
/// looked like a hole worth closing — under contention the condition ought to
/// fail most of the time — so cutting the *prefix* instead ( keeping the tail,
/// so the condition never has to hold ) was built and then reverted, because
/// across four workload shapes it never measured better: frequent checkpoints,
/// rare checkpoints, slow checkpoints over a large resident dataset, and a
/// single shard with a single continuous writer.
///
/// This test is what remains of that: it pins the property that matters — the
/// log does not accumulate the whole write history — without asserting which
/// mechanism delivers it. See JOURNAL, 2026-08-25.
#[test]
fn the_log_stays_bounded_when_checkpoints_are_rare() {
    let dir = tmpdir("rare-ckpt");
    let _c = CleanDir(dir.clone());
    // A policy that never fires, so the only checkpoints are the ones below.
    let policy = yesno_core::checkpoint::CheckpointPolicy {
        dirty_bytes: usize::MAX,
        max_dirty_bytes: usize::MAX,
        wal_bytes: u64::MAX,
        interval_secs: u64::MAX,

        ..Default::default()
    };
    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 2,
            policy,
            ..Default::default()
        },
    )
    .unwrap();

    // A large resident dataset, so each checkpoint does real work and takes long
    // enough that a continuously-writing thread is essentially certain to append
    // while it runs. That is what makes the "was the log quiet?" test fail every
    // time, which is the case a fast checkpoint hides.
    for k in 100..900u64 {
        db.insert_many(
            k,
            &(0..900u64).map(|i| k * 100_000 + i * 3).collect::<Vec<_>>(),
        )
        .unwrap();
    }
    db.checkpoint().unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let writers: Vec<_> = (0..3u64)
        .map(|t| {
            let db = db.clone();
            let stop = stop.clone();
            thread::spawn(move || {
                let mut i = 0u64;
                while !stop.load(Ordering::Acquire) {
                    db.insert(t, t * 1_000_000 + i).unwrap();
                    i += 1;
                }
                i
            })
        })
        .collect();

    // Only eight checkpoints, spaced out, while the writers never pause.
    let mut peak = 0u64;
    for _ in 0..8 {
        thread::sleep(std::time::Duration::from_millis(40));
        db.checkpoint().unwrap();
        peak = peak.max(db.wal_bytes());
    }
    stop.store(true, Ordering::Release);
    let written: u64 = writers.into_iter().map(|h| h.join().unwrap()).sum();

    eprintln!("  {written} commits, 8 rare checkpoints, peak log {peak} bytes");
    // Each record is ~72 bytes, so holding every write would be ~72 * written.
    // Bounded means holding roughly one checkpoint interval's worth.
    let all_of_it = written * 40;
    assert!(
        peak < all_of_it,
        "log peaked at {peak} bytes for {written} commits, which is most of the \
         whole history; the prefix is not being reclaimed"
    );
}

/// Snapshots taken and held across concurrent checkpoints must keep their data.
///
/// The general guard over `Db::snapshot` racing `Db::checkpoint`: a snapshot
/// captures per-shard index roots and records the watermark those roots
/// correspond to, and `evict_floor` reads that watermark back to decide what
/// `evict_durable` may drop from the memtable. A watermark that does not match
/// the root it was taken with puts the floor above what that reader needs, and
/// the memtable drops versions only that reader's older root cannot answer for
/// — the key vanishes for one snapshot and for nobody else.
///
/// The writer appends to **one key across four chunks**, round-robin, so every
/// valid snapshot sees a contiguous prefix of the insertion order; a mix of
/// stale and fresh chunks shows up as a hole no commit point could produce.
/// Snapshots are **held across several checkpoints**, because `evict_durable`
/// only drops a chain `prune` has reduced to a single version, which a
/// continuously-written chunk never is.
///
/// **What this does not do is catch the ordering defect it was written for.**
/// `Db::snapshot` used to read the roots and the watermark in two separate
/// passes over the shards, and a checkpoint landing between them recorded a
/// watermark newer than the root pinned. Instrumenting the two-pass version
/// showed that skew occurring on **95 of 1200** snapshots — so the precondition
/// fires readily — but no arrangement of this test turned it into observable
/// loss. That needs a further coincidence: the affected chunk must go quiet
/// long enough for its chain to reduce to one version, while the skewed reader
/// is both still alive and the *oldest*, so that it is the one setting the
/// floor. Reachable, but not on demand.
///
/// So the one-lock-hold capture in `Db::snapshot` is justified by construction
/// rather than by a failing test here, and this test remains what its name
/// says: a general stress guard, not a regression test for that defect.
#[test]
fn snapshots_taken_during_a_checkpoint_keep_their_memtable() {
    /// The i-th ordinal the writer appends: chunk `i % 4`, position `i / 4`.
    fn nth(i: u64) -> u64 {
        (i % 4) * 65_536 + i / 4
    }

    let dir = tmpdir("snap-vs-ckpt");
    let _c = CleanDir(dir.clone());
    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 2,
            ..Default::default()
        },
    )
    .unwrap();

    // Seed all four chunks and get them on disk, so a snapshot's root has
    // something to answer with and the chunks already exist in the index.
    for i in 0..64u64 {
        db.insert(1, nth(i)).unwrap();
    }
    db.checkpoint().unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let progress = Arc::new(AtomicU64::new(64));

    let writer = {
        let db = db.clone();
        let stop = stop.clone();
        let progress = progress.clone();
        thread::spawn(move || {
            let mut i = 64u64;
            while !stop.load(Ordering::Acquire) {
                db.insert(1, nth(i)).unwrap();
                i += 1;
                progress.store(i, Ordering::Release);
                // Let the chains go quiet between writes, so `prune` can reduce
                // them to a single version and `evict_durable` can act.
                thread::sleep(std::time::Duration::from_millis(2));
            }
        })
    };
    let ckpt = {
        let db = db.clone();
        let stop = stop.clone();
        thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                db.checkpoint().unwrap();
            }
        })
    };

    let readers: Vec<_> = (0..3)
        .map(|_| {
            let db = db.clone();
            thread::spawn(move || {
                for _ in 0..60 {
                    let snap = db.snapshot().unwrap();
                    // Hold it across several checkpoints. Eviction only drops a
                    // chain that `prune` has reduced to one version, which a
                    // continuously-written chunk never is — the loss needs the
                    // chunks to go quiet while a stale-pinned snapshot is still
                    // alive.
                    thread::sleep(std::time::Duration::from_millis(8));
                    let got: BTreeSet<u64> = snap.load(1).unwrap().iter().collect();
                    // Whatever commit point this snapshot landed on, the set has
                    // to be exactly the first `n` the writer appended.
                    let want: BTreeSet<u64> = (0..got.len() as u64).map(nth).collect();
                    assert_eq!(
                        got.len(),
                        want.len(),
                        "snapshot saw a set that is not a prefix of the insertion order"
                    );
                    assert_eq!(
                        got, want,
                        "snapshot saw a hole: some chunks answered from an older root \
                         than others, so the memtable was evicted below its pinned root"
                    );
                }
            })
        })
        .collect();

    for r in readers {
        r.join().unwrap();
    }
    stop.store(true, Ordering::Release);
    writer.join().unwrap();
    ckpt.join().unwrap();
    assert!(
        progress.load(Ordering::Acquire) > 64,
        "the writer never got going, so no snapshot raced a checkpoint"
    );
}

#[test]
fn backup_lease_blocks_checkpoints_but_not_commits() {
    let dir = tmpdir("backup-barrier");
    let _c = CleanDir(dir.clone());
    let db = Db::open(&dir).unwrap();
    db.insert(1, 10).unwrap();
    db.checkpoint().unwrap();

    let lease = db.begin_backup();
    // The lease protects the cross-shard image flip, not the WAL. A write made
    // while it is held must complete normally and become part of snapshot
    // recovery rather than waiting behind filesystem snapshot creation.
    db.insert(1, 20).unwrap();
    assert_eq!(
        db.snapshot()
            .unwrap()
            .load(1)
            .unwrap()
            .iter()
            .collect::<Vec<_>>(),
        [10, 20]
    );

    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    db.set_checkpoint_hook(Some(Arc::new(move || {
        entered_tx.send(()).unwrap();
    })));
    let (attempted_tx, attempted_rx) = std::sync::mpsc::sync_channel(1);
    let checkpoint = {
        let db = db.clone();
        thread::spawn(move || {
            attempted_tx.send(()).unwrap();
            db.checkpoint().unwrap()
        })
    };
    attempted_rx.recv().unwrap();
    assert!(
        entered_rx
            .recv_timeout(std::time::Duration::from_millis(50))
            .is_err(),
        "checkpoint entered its shard loop while a backup lease was live"
    );

    drop(lease);
    entered_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("checkpoint did not resume after the backup lease dropped");
    assert_eq!(checkpoint.join().unwrap(), 2);
    db.set_checkpoint_hook(None);
}

/// Commit times never invert under contention.
///
/// I9's monotonicity is the property a wall-clock recovery target rests on, and
/// contention is the only way to break it that a single-threaded test cannot
/// see: two committers reading the clock and then entering the oracle in the
/// other order would produce a version whose stamp precedes its predecessor's.
/// The stamp is taken *inside* the oracle's lock precisely so this cannot
/// happen — so a change that hoists the clock read out of that lock to "avoid
/// a syscall under a mutex" must fail here.
///
/// This is a stress test, not a proof: a pass means the interleavings that
/// occurred were correct. It runs enough contending writers that an inversion
/// window of a single instruction is very likely to be hit.
#[test]
fn commit_times_never_invert_under_contention() {
    use std::collections::BTreeMap;
    use yesno_core::wal::record::{RecType, Scanner};

    const WRITERS: usize = 8;
    const EACH: u64 = 40;

    let dir = tmpdir("commit-time-order");
    let _c = CleanDir(dir.clone());
    let db = Arc::new(
        Db::open_with(
            &dir,
            DbOptions {
                shards: 4,
                ..Default::default()
            },
        )
        .unwrap(),
    );

    {
        let db = db.clone();
        race(WRITERS, move |w| {
            for i in 0..EACH {
                let mut b = db.batch();
                // Alternate single-shard and cross-shard batches: the multi-shard
                // arm is the one that holds several locks, which is where an
                // out-of-lock clock read would be most visible.
                if i % 2 == 0 {
                    b.insert(w as u64, i);
                } else {
                    for k in 0..8u64 {
                        b.insert(k * 13 + w as u64, i);
                    }
                }
                b.commit().unwrap();
            }
        });
    }
    let db = Arc::try_unwrap(db).ok().expect("writers still hold the db");
    drop(db);

    // version -> the one time every participant agreed on
    let mut stamps: BTreeMap<u64, u64> = BTreeMap::new();
    for entry in std::fs::read_dir(&dir).unwrap().flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|x| x != "wal") {
            continue;
        }
        let bytes = std::fs::read(&path).unwrap();
        for rec in Scanner::new(&bytes, 0).flatten() {
            if !matches!(rec.rtype, RecType::ShardCommit | RecType::Abort) {
                continue;
            }
            let time = rec.commit_time().unwrap().expect("every commit is stamped");
            if let Some(seen) = stamps.insert(rec.commit_version, time) {
                assert_eq!(
                    seen, time,
                    "version {} was stamped twice, {seen} and {time}",
                    rec.commit_version
                );
            }
        }
    }

    assert!(
        stamps.len() as u64 >= WRITERS as u64 * EACH / 2,
        "only {} commits were observed; this test is not contending",
        stamps.len()
    );

    // Strictly increasing, not merely non-decreasing: the clamp is `last + 1`,
    // so two commits can never share a stamp however fast they arrive. That is
    // stronger than I9 requires and worth pinning — an equal pair would make a
    // time target ambiguous about which of them it includes.
    let mut prev: Option<(u64, u64)> = None;
    for (version, time) in &stamps {
        if let Some((pv, pt)) = prev {
            assert!(
                *time > pt,
                "commit time did not increase: version {pv} at {pt}, then version {version} at {time}"
            );
        }
        prev = Some((*version, *time));
    }
}

/// **A retry after an ambiguous failure silently undoes another client's
/// delete, and the return value tells the retrying client the opposite.**
///
/// Every mutation here is idempotent *in isolation* — `insert` is a set union,
/// so replaying it lands the same state. That is what makes retry look safe.
/// It is not safe under interleaving, and there is no idempotency key, request
/// id, or dedup anywhere on the write path to make it so: a retry is
/// indistinguishable from a fresh intent, because that is literally all it is.
///
/// **Deterministic, not a race.** Nothing here depends on timing. The
/// ambiguity is in the *client's knowledge*, not in the engine: a commit that
/// succeeded but whose acknowledgement was lost — a timeout, a reset connection,
/// a crash between commit and response — leaves the client with exactly two
/// choices, retry or give up, and no way to tell which is correct.
///
/// The two arms are the point. Read alone, either one looks benign.
#[test]
fn a_retried_insert_resurrects_an_ordinal_another_client_removed() {
    let dir = tmpdir("retry-resurrect");
    let _c = CleanDir(dir.clone());
    let db = Db::open_with(&dir, DbOptions::default()).unwrap();

    // ---- Arm 1: the benign duplicate, for contrast.
    //
    // X inserts, loses the acknowledgement, and retries with nobody else
    // touching the key. The retry reports `false` — "already present" — which
    // is exactly how a client learns its first attempt had landed.
    assert!(db.insert(7, 1).unwrap(), "first attempt adds the ordinal");
    let benign_retry = db.insert(7, 1).unwrap();
    assert!(
        !benign_retry,
        "an uncontested retry must report `false`, i.e. already present"
    );

    // ---- Arm 2: the same retry, with a deliberate delete in between.
    assert!(db.insert(7, 42).unwrap(), "X's first attempt lands");
    // X never sees this acknowledgement.

    // Y removes the ordinal, deliberately and successfully.
    assert!(db.remove(7, 42).unwrap(), "Y's delete is applied");
    assert!(
        !db.snapshot().unwrap().contains(7, 42).unwrap(),
        "and is visible before the retry"
    );

    // X retries, because it cannot distinguish a lost write from a lost reply.
    let hazardous_retry = db.insert(7, 42).unwrap();

    // The delete is undone and no error is raised anywhere.
    assert!(
        db.snapshot().unwrap().contains(7, 42).unwrap(),
        "the retry resurrected an ordinal that was deliberately removed"
    );

    // **And this is the sharp edge.** The retry reports `true` — "newly
    // added" — which is the *same* answer X would get if its first attempt had
    // never landed. So the one signal available to X says "good thing you
    // retried" at precisely the moment the retry did damage, while the benign
    // case above says `false`. The return value is not merely uninformative,
    // it points the wrong way.
    assert!(
        hazardous_retry,
        "the destructive retry reports `true`, the same as a genuinely needed one"
    );
    assert_ne!(
        benign_retry, hazardous_retry,
        "a client cannot distinguish these, yet they differ in exactly the case \
         where the difference matters"
    );

    // ---- The symmetric hazard: a retried delete removes a re-insert.
    assert!(db.remove(7, 1).unwrap(), "Z's delete lands");
    assert!(
        db.insert(7, 1).unwrap(),
        "W re-adds the ordinal deliberately"
    );
    let retried_delete = db.remove(7, 1).unwrap();
    assert!(
        !db.snapshot().unwrap().contains(7, 1).unwrap(),
        "a retried delete removed an ordinal added after it"
    );
    assert!(
        retried_delete,
        "and again reports the same `true` a first, needed delete would"
    );
}

/// **The engine does not lose updates; `store_set` does.**
///
/// Read this beside `concurrent_writers_to_one_key_do_not_lose_updates` above,
/// which drives eight threads at one container and asserts that *nothing* is
/// lost. That is true and it is about `insert` / `remove`, which are commutative
/// set operations applied under the shard lock. It says nothing about a client
/// that reads a set, computes a new one, and writes the whole thing back.
///
/// `WriteBatch::store_set` is a blind whole-key replace — `DeleteKey` followed
/// by the new chunks — with no precondition, no expected version, and no
/// compare-and-set anywhere in the API to build one from. Two clients that read
/// the same base and each store their own edit produce last-writer-wins, and
/// both are told they succeeded.
///
/// **Deterministic, not a race.** The interleaving is written out: both
/// clients read before either writes, which is the whole of the classic
/// lost-update anomaly. Threads would make it intermittent without making it
/// any more true.
#[test]
fn a_read_modify_write_through_store_set_loses_a_concurrent_update() {
    let dir = tmpdir("lost-update");
    let _c = CleanDir(dir.clone());
    let db = Db::open_with(&dir, DbOptions::default()).unwrap();

    let load =
        |key: u64| -> BTreeSet<u64> { db.snapshot().unwrap().load(key).unwrap().iter().collect() };
    let store = |key: u64, want: &BTreeSet<u64>| {
        let set = yesno_core::OrdSet::from_iter_unsorted(want.iter().copied());
        let mut b = db.batch();
        b.store_set(key, &set);
        b.commit().unwrap()
    };

    // ---- Arm A: read-modify-write through `store_set`.
    db.insert_many(7, &[1, 2, 3]).unwrap();

    // Both clients read the same base. Neither has written yet.
    let x_read = load(7);
    let y_read = load(7);
    assert_eq!(x_read, y_read, "both clients start from the same state");

    // X adds 10 and writes its whole set back.
    let mut x_next = x_read;
    x_next.insert(10);
    store(7, &x_next);
    assert_eq!(load(7), BTreeSet::from([1, 2, 3, 10]), "X's edit landed");

    // Y adds 20 to the set *it* read — before X wrote — and writes it back.
    // This commit succeeds. Nothing compares Y's base against what is there.
    let mut y_next = y_read;
    y_next.insert(20);
    let y_commit = store(7, &y_next);

    assert_eq!(
        load(7),
        BTreeSet::from([1, 2, 3, 20]),
        "Y's blind replace overwrote X's edit"
    );
    assert!(
        !db.snapshot().unwrap().contains(7, 10).unwrap(),
        "X's update is gone, and X was told it succeeded"
    );
    // And the acknowledgement Y receives is an ordinary success. `changed`
    // counts ordinals it altered, which is nonzero for any real edit, so it
    // cannot distinguish "I added 20" from "I added 20 and destroyed 10".
    assert!(
        y_commit.changed > 0,
        "the clobbering commit reports a normal, successful edit"
    );

    // ---- Arm B: the identical intent, expressed as set operations.
    //
    // This is why the hazard is the API and not the engine. The same two
    // clients, the same base, the same additions — and nothing is lost, because
    // `insert` states an intent that commutes instead of a whole-set assertion
    // that does not.
    db.insert_many(8, &[1, 2, 3]).unwrap();
    let x_read = load(8);
    let y_read = load(8);
    assert_eq!(x_read, y_read);

    db.insert(8, 10).unwrap();
    db.insert(8, 20).unwrap();

    assert_eq!(
        load(8),
        BTreeSet::from([1, 2, 3, 10, 20]),
        "expressed as set operations, both edits survive"
    );

    // The contrast, asserted rather than left to the reader.
    assert_ne!(
        load(7),
        load(8),
        "the same logical intent produces different results depending only on \
         which write primitive the client chose"
    );
}

/// Readers stay correct while a checkpoint is running its `fsync`s.
///
/// # The window this exists for
///
/// `Db::checkpoint` used to hold `Mutex<ShardStore>` across its whole body, so
/// no reader could be inside the store while the superblock was being committed.
/// It now **releases** that lock across the three `fsync`s -- worth ~10x on
/// worst-case reader latency ( max 5.9-8.3 ms held, 0.5-0.7 ms released ) --
/// which creates a window that did not previously exist: a reader walking the
/// old root while a checkpoint flushes the new one and writes the inactive
/// superblock slot.
///
/// The argument that this is safe is that the live superblock does not change
/// until `adopt_superblock`, the new one goes to the slot `pick` is not
/// returning, and new extents are unreachable from the old root. **That is an
/// argument, and this is the test.** A reader must never see a short set, a torn
/// set, or an error.
///
/// Non-vacuity is asserted two ways: the checkpoint count must be non-zero, and
/// the reader count must be large enough that some reads landed inside a
/// checkpoint rather than between them.
#[test]
fn readers_are_correct_while_a_checkpoint_syncs() {
    let dir = tmpdir("read-during-ckpt");
    let db = Arc::new(
        Db::open_with(
            &dir,
            DbOptions {
                shards: 2,
                ..Default::default()
            },
        )
        .unwrap(),
    );

    const KEYS: u64 = 24;
    const PER: u64 = 400;
    for k in 0..KEYS {
        let v: Vec<u64> = (0..PER).map(|i| (k << 20) | (i * 2)).collect();
        db.insert_many(k, &v).unwrap();
    }
    db.checkpoint().unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let ckpts = Arc::new(AtomicU64::new(0));
    let reads = Arc::new(AtomicU64::new(0));

    let writer = {
        let (db, stop, ckpts) = (db.clone(), stop.clone(), ckpts.clone());
        thread::spawn(move || {
            // A distinct key range, so the resident set the readers check is
            // never itself rewritten -- the assertion is about what a checkpoint
            // does to *unrelated* reads.
            let mut n = 0u64;
            while !stop.load(Ordering::Relaxed) {
                db.insert(9_000 + (n % 16), 500_000 + n).unwrap();
                db.checkpoint().unwrap();
                ckpts.fetch_add(1, Ordering::Relaxed);
                n += 1;
            }
        })
    };

    let readers: Vec<_> = (0..4)
        .map(|_| {
            let (db, stop, reads) = (db.clone(), stop.clone(), reads.clone());
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let snap = db.snapshot().unwrap();
                    for k in 0..KEYS {
                        let got: Vec<u64> = snap.load(k).unwrap().iter().collect();
                        let want: Vec<u64> = (0..PER).map(|i| (k << 20) | (i * 2)).collect();
                        assert_eq!(got, want, "key {k} read short or torn during a checkpoint");
                    }
                    reads.fetch_add(1, Ordering::Relaxed);
                }
            })
        })
        .collect();

    thread::sleep(std::time::Duration::from_millis(1500));
    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();
    for r in readers {
        r.join().unwrap();
    }

    let c = ckpts.load(Ordering::Relaxed);
    let r = reads.load(Ordering::Relaxed);
    assert!(
        c > 5,
        "the writer must have checkpointed repeatedly, got {c}"
    );
    assert!(r > 50, "the readers must have swept repeatedly, got {r}");
    let _ = std::fs::remove_dir_all(&dir);
}
