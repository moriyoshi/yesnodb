//! The named invariants, asserted rather than commented.
//!
//! `ARCHITECTURE.md` and the design both say of I1-I7: *"Name these in the code
//! and assert them in `fsck`."* They were named — every one appears in a `//!`
//! block explaining why it matters — and as of 2026-08-25 **not one of I1-I6
//! was mentioned by any test**, nor asserted by `fsck`.
//!
//! That is a specific kind of gap. These are not properties some other test
//! covers incidentally: each one is the precondition for a *different*
//! subsystem's correctness, and each fails silently.
//!
//! - **I2** ( extent immutability ) is the one the whole zero-copy design rests
//!   on. Containers hand out slices aliasing the mapping, so writing into a
//!   published extent is Rust UB, not merely a torn read.
//! - **I3** ( allocation only at checkpoint ) is what makes the deferred free
//!   list need no persistence at all: extents allocated since the last
//!   checkpoint are unreachable from any durable root, so a crash loses them
//!   and recovery re-derives everything.
//! - **I5** ( per-shard cv monotonicity ) is what lets recovery truncate at the
//!   first record above the global watermark. Without it the discarded records
//!   are not a clean suffix and truncation drops committed data.
//! - **I6** ( append-only mappings ) is why the file is never `ftruncate`d.
//!   Truncating under a live mapping is a SIGBUS factory, and SIGBUS is not
//!   catchable as a `Result`.
//!
//! - **I9** ( commit time non-decreasing in commit version, and identical across
//!   the participants of one commit ) is what lets a wall-clock recovery target
//!   name a prefix at all. An inversion does not fail any read: it makes one
//!   restore silently include a commit an earlier target excluded.
//!
//! I1 ( little-endian only ) is deliberately absent here: it is enforced at
//! open, and on a little-endian host the branch is unreachable, so a test would
//! assert nothing. See the JOURNAL entry for 2026-08-25.

use std::collections::BTreeSet;
use std::path::PathBuf;

use yesno_core::wal::record::{RecType, Scanner};
use yesno_core::{Db, DbOptions};

struct CleanDir(PathBuf);
impl Drop for CleanDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tmpdir(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("yesno-inv-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    p
}

/// The first `n` bytes of a file, or all of it if shorter.
fn read_prefix(path: &std::path::Path, n: usize) -> Vec<u8> {
    use std::io::Read;
    let f = std::fs::File::open(path).unwrap();
    let mut buf = Vec::with_capacity(n);
    f.take(n as u64).read_to_end(&mut buf).unwrap();
    buf
}

fn opts(shards: usize) -> DbOptions {
    DbOptions {
        shards,
        ..Default::default()
    }
}

/// A spread of keys and ordinals that lands in every container kind.
fn churn(db: &Db, round: u64) {
    for key in 0..12u64 {
        db.insert_many(
            key,
            &(0..300u64)
                .map(|i| i * 7 + round + key * 1_000)
                .collect::<Vec<_>>(),
        )
        .unwrap();
    }
    db.insert_range(99, round * 100_000, round * 100_000 + 50_000)
        .unwrap();
}

// ---------------------------------------------------------------------------
// I2 - a published extent is never mutated in place
// ---------------------------------------------------------------------------

/// Bytes of the shard file below the previous checkpoint's high-water mark must
/// never change, however much is written afterwards.
///
/// This is the invariant the zero-copy design rests on: a live `Container`
/// holds a slice into this file, so an in-place write to a published extent is
/// undefined behaviour rather than a stale read. Shadow paging is what makes it
/// hold — new data goes to unreferenced space and only becomes reachable when
/// the superblock flips.
#[test]
fn i2_published_extents_are_never_written_in_place() {
    let dir = tmpdir("i2");
    let _c = CleanDir(dir.clone());
    let db = Db::open_with(&dir, opts(1)).unwrap();

    churn(&db, 0);
    db.checkpoint().unwrap();

    // Pin a reader for the whole test.
    //
    // Without this the test is wrong rather than strict: once an extent is
    // freed and its three reclamation conditions are met, its bytes are
    // **legitimately** reusable, and a rewrite there violates nothing. I2
    // protects *published* extents. A live snapshot is entitled to everything
    // written so far, so nothing below can be recycled, and any rewrite in
    // extent space is then a real violation.
    let reader = db.snapshot().unwrap();

    // Read only as far as the allocator has actually opened slabs.
    //
    // The shard file is a full `SEGMENT_SIZE` — **1 GiB from the moment it is
    // created**, sparse and almost entirely zero. Reading all of it twice and
    // comparing it byte-wise in a debug build is what made this test take 37
    // seconds. Extents can only exist inside allocated slabs, so this bound is
    // exact, not a sample.
    let path = dir.join("shard-0000.yno");
    let span = db.allocated_bytes() as usize;
    assert!(span > 0, "no slabs were allocated; this test is blind");
    let before = read_prefix(&path, span);
    assert!(
        before.len() > 8192,
        "nothing was written; this test is blind"
    );

    // Churn hard enough to supersede everything written above.
    for round in 1..5 {
        churn(&db, round);
        db.checkpoint().unwrap();
    }

    // Re-read the same prefix. The allocator only grows, so every byte compared
    // below was inside an open slab both times.
    let after = read_prefix(&path, span);
    assert_eq!(
        after.len(),
        before.len(),
        "the prefix under test changed size"
    );

    // Two documented in-place exceptions, and getting the boundary wrong is how
    // this test first "failed": the superblocks at the head of the file, and
    // **each slab's own `SLAB_META` header**, which `write_slab_metadata`
    // rewrites every checkpoint. Both are CRC-protected, both are rederivable
    // by an index scan, and neither is ever aliased by a `Container` — which is
    // the actual criterion. Slab 0 is reserved in full.
    const SLAB_SIZE: usize = 2 * 1024 * 1024;
    const SLAB_META: usize = 8192;

    // Walk slab by slab rather than testing every byte for membership.
    //
    // The index-per-byte form cost a modulo and a closure call per byte, which
    // in a debug build over a file this size took **36 seconds** and made the
    // Valgrind gate impractical. Slicing each slab's extent region and zipping
    // is the same comparison without either.
    //
    // Only bytes that already held something can have been *mutated*. Free
    // space inside an open slab is zero-filled and becomes non-zero the first
    // time an extent is allocated there — a change, but the ordinary
    // allocation path, not a write into a published extent.
    let mut changed_count = 0usize;
    let mut first_changed = None;
    let mut zero_fills = 0usize;
    let mut extent_bytes = 0usize;

    // Slab 0 is reserved in full, so start at 1.
    for slab in 1..before.len().div_ceil(SLAB_SIZE) {
        let lo = slab * SLAB_SIZE + SLAB_META;
        let hi = ((slab + 1) * SLAB_SIZE).min(before.len());
        if lo >= hi {
            continue;
        }
        extent_bytes += hi - lo;
        for (i, (b, a)) in before[lo..hi].iter().zip(&after[lo..hi]).enumerate() {
            if *b == 0 {
                zero_fills += (*a != 0) as usize;
            } else if b != a {
                changed_count += 1;
                first_changed.get_or_insert(lo + i);
            }
        }
    }
    assert!(
        extent_bytes > 0,
        "no extent space in the file at all; this test is blind"
    );

    assert!(
        changed_count == 0,
        "{changed_count} extent-space bytes were rewritten in place, first at \
         offset {} - a live Container aliasing that range would be reading \
         mutated memory",
        first_changed.unwrap_or(0)
    );

    assert!(
        zero_fills > 0,
        "no previously-free byte was written either, so the churn did nothing"
    );

    // The reader must still be able to read what it was entitled to, which is
    // what those untouched bytes are for.
    assert_eq!(reader.cardinality(0).unwrap(), 300);
}

// ---------------------------------------------------------------------------
// I3 - extent allocation happens only in the checkpointer
// ---------------------------------------------------------------------------

/// Committing must not allocate file space; only `checkpoint()` may.
///
/// This is what makes the deferred free list need no durable form. If the write
/// path allocated, a crash would leave extents that are reachable from no root
/// and recorded in no free list, and recovery would have to scan for orphans.
#[test]
fn i3_the_write_path_allocates_no_file_space() {
    let dir = tmpdir("i3");
    let _c = CleanDir(dir.clone());
    let db = Db::open_with(&dir, opts(1)).unwrap();

    churn(&db, 0);
    db.checkpoint().unwrap();
    // Extents, not `allocated_bytes`: that moves in 2 MiB slab steps and so
    // cannot see the handful of extents a write path would take.
    let quiesced = db.used_extents();
    assert!(
        quiesced > 0,
        "nothing was allocated at all; this test is blind"
    );

    // A large batch of writes, deliberately with no checkpoint between them.
    for round in 1..4 {
        churn(&db, round);
    }

    assert_eq!(
        db.used_extents(),
        quiesced,
        "the write path allocated {} extents; I3 says only the checkpointer may",
        db.used_extents().saturating_sub(quiesced)
    );

    // And a checkpoint *is* what allocates — shown on keys that are new, since
    // rewriting existing ones can reuse space freed in the same pass and leave
    // the total flat.
    for key in 500..520u64 {
        db.insert_many(key, &(0..400u64).map(|i| i * 11 + key).collect::<Vec<_>>())
            .unwrap();
    }
    let before_ckpt = db.used_extents();
    db.checkpoint().unwrap();
    assert!(
        db.used_extents() > before_ckpt,
        "the checkpoint allocated nothing for 20 new keys, so the assertion \
         above proved nothing"
    );
}

// ---------------------------------------------------------------------------
// I5 - each shard's WAL strictly increases in commit version
// ---------------------------------------------------------------------------

/// Recovery truncates at the first record above the global watermark, which is
/// only correct if the discarded records form a clean suffix.
///
/// Assigning `cv` while holding every participating shard's lock is what
/// guarantees it. A regression would not show up as a failed read — it would
/// show up as recovery silently dropping committed data on some later crash.
#[test]
fn i5_commit_versions_never_go_backwards_within_a_shard() {
    let dir = tmpdir("i5");
    let _c = CleanDir(dir.clone());
    let db = Db::open_with(&dir, opts(4)).unwrap();

    // Interleave single-key and multi-shard batches, which is when a cv could
    // be assigned out of order.
    for round in 0..6u64 {
        churn(&db, round);
        let mut b = db.batch();
        for key in 0..12u64 {
            b.insert(key, round * 31 + key);
        }
        b.commit().unwrap();
    }
    drop(db);

    let mut checked = 0;
    for entry in std::fs::read_dir(&dir).unwrap().flatten() {
        let p = entry.path();
        if p.extension().is_none_or(|x| x != "wal") {
            continue;
        }
        let bytes = std::fs::read(&p).unwrap();
        let mut last = 0u64;
        let mut seen = 0;
        for rec in Scanner::new(&bytes, 0).flatten() {
            // Fences and padding carry no commit version.
            if matches!(rec.rtype, RecType::Pad | RecType::EpochFence) {
                continue;
            }
            assert!(
                rec.commit_version >= last,
                "{:?}: commit version went backwards, {last} then {} at lsn {}",
                p.file_name().unwrap(),
                rec.commit_version,
                rec.lsn
            );
            last = rec.commit_version;
            seen += 1;
        }
        if seen > 0 {
            checked += 1;
        }
    }
    assert!(
        checked > 0,
        "no WAL records were examined; this test is blind"
    );
}

// ---------------------------------------------------------------------------
// I6 - mappings are append-only; the file never shrinks
// ---------------------------------------------------------------------------

/// Space is returned by punching holes, never by truncating.
///
/// `ftruncate` under a live mapping is a SIGBUS factory, and SIGBUS cannot be
/// caught as a `Result` — so a regression here is an uncatchable crash in a
/// reader that did nothing wrong.
#[test]
fn i6_the_shard_file_never_shrinks() {
    let dir = tmpdir("i6");
    let _c = CleanDir(dir.clone());
    let db = Db::open_with(&dir, opts(1)).unwrap();
    let path = dir.join("shard-0000.yno");

    let mut high = 0u64;
    let mut grew = false;
    for round in 0..6 {
        churn(&db, round);
        // Delete most of it back, so reclamation has real work to do.
        if round % 2 == 1 {
            let mut b = db.batch();
            for key in 0..12u64 {
                b.delete_key(key);
            }
            b.commit().unwrap();
        }
        db.checkpoint().unwrap();

        let len = std::fs::metadata(&path).unwrap().len();
        assert!(
            len >= high,
            "the shard file shrank from {high} to {len} - a live mapping over \
             the lost range would SIGBUS, which is not catchable"
        );
        grew |= len > high;
        high = len;
    }
    assert!(grew, "the file never grew, so nothing was really exercised");
}

// ---------------------------------------------------------------------------
// I7 - every chunk of one key lives in one shard
// ---------------------------------------------------------------------------

/// Sharding is by key, never by prefix, so a key's chunks are one cursor walk.
#[test]
fn i7_a_key_never_spans_shards() {
    let dir = tmpdir("i7");
    let _c = CleanDir(dir.clone());
    let db = Db::open_with(&dir, opts(8)).unwrap();

    // Ordinals spread over many prefixes: if sharding were by prefix, these
    // would scatter.
    let wide: Vec<u64> = (0..400u64).map(|i| i * 500_000).collect();
    for key in 0..40u64 {
        db.insert_many(key, &wide).unwrap();
    }
    db.checkpoint().unwrap();
    drop(db);

    let db = Db::open_with(&dir, opts(8)).unwrap();
    for key in 0..40u64 {
        let got: BTreeSet<u64> = db.snapshot().unwrap().load(key).unwrap().iter().collect();
        let want: BTreeSet<u64> = wide.iter().copied().collect();
        assert_eq!(
            got, want,
            "key {key} lost chunks across a reopen - its chunks are not all in one shard"
        );
    }
}

// ---------------------------------------------------------------------------
// I4 - a checkpoint persists only state at or below the visible watermark
// ---------------------------------------------------------------------------

/// The data file alone, with no WAL to replay, must be a consistent snapshot.
///
/// This is what makes recovery redo-only: *"nothing from an uncommitted `cv` is
/// ever written to the data file, so undo is just don't redo."* It is also what
/// makes physical bootstrap of a follower legal — the copied image is
/// consistent by construction rather than by a hot-backup protocol.
///
/// `tests/crash_matrix.rs` already asserts the recovery half of I4 ( a replay
/// plan never includes a record above the watermark ). This asserts the
/// *checkpointer's* half, which is a different code path: what it chose to
/// write, not what recovery chose to replay.
#[test]
fn i4_the_data_file_alone_is_a_consistent_snapshot() {
    let dir = tmpdir("i4");
    let _c = CleanDir(dir.clone());

    let at_checkpoint: BTreeSet<u64>;
    {
        let db = Db::open_with(&dir, opts(1)).unwrap();
        db.insert_many(7, &(0..5_000u64).map(|i| i * 3).collect::<Vec<_>>())
            .unwrap();
        db.checkpoint().unwrap();
        at_checkpoint = db.snapshot().unwrap().load(7).unwrap().iter().collect();

        // Committed *after* the checkpoint, so it lives only in the WAL and the
        // memtable. None of it may have reached the data file.
        db.insert_many(7, &(0..5_000u64).map(|i| i * 3 + 1).collect::<Vec<_>>())
            .unwrap();
        db.insert_many(8, &(0..900u64).collect::<Vec<_>>()).unwrap();
    }

    // Rebuild a database from the data file *only* — the image a follower would
    // be handed by a physical bootstrap.
    let bare = tmpdir("i4-bare");
    let _c2 = CleanDir(bare.clone());
    std::fs::create_dir_all(&bare).unwrap();
    // The MANIFEST, not a bare UUID file. Identity moved into it on
    // 2026-08-28 together with the shard count and the `vshard -> shard` map,
    // and that makes this copy say more than it used to: a physical bootstrap
    // must carry **routing**, not just identity. A follower handed only the data
    // file would have no map, and re-deriving one would send keys to shards that
    // do not hold them.
    std::fs::copy(dir.join("MANIFEST"), bare.join("MANIFEST")).unwrap();
    std::fs::copy(dir.join("shard-0000.yno"), bare.join("shard-0000.yno")).unwrap();

    let db = Db::open_with(&bare, opts(1)).unwrap();
    let snap = db.snapshot().unwrap();
    let got: BTreeSet<u64> = snap.load(7).unwrap().iter().collect();

    assert_eq!(
        got, at_checkpoint,
        "the data file does not match the state at its own checkpoint watermark"
    );
    assert!(
        !at_checkpoint.is_empty(),
        "nothing was checkpointed; this test is blind"
    );
    assert_eq!(
        snap.cardinality(8).unwrap(),
        0,
        "a key committed only after the checkpoint reached the data file - \
         the checkpoint barrier leaked state above its watermark"
    );
}

// ---------------------------------------------------------------------------
// I9 - commit time is non-decreasing in commit version, and agrees across the
//      participants of one commit
// ---------------------------------------------------------------------------

/// The property a wall-clock recovery target rests on.
///
/// A restore that resolves a time to a version needs both halves. Without
/// monotonicity there is no version whose prefix is exactly "everything at or
/// before T". Without agreement across participants, a multi-shard commit has no
/// single time to compare against, and recovery would have to pick one — the
/// precise-looking wrong answer.
///
/// Neither half fails a read. The stamp is written by `WriteBatch::commit`
/// and consulted only by a restore that may run months later, so this test is
/// the only thing standing between a regression and a wrong recovery.
#[test]
fn i9_commit_times_are_monotone_and_agree_across_participants() {
    use std::collections::BTreeMap;

    let dir = tmpdir("i9");
    let _c = CleanDir(dir.clone());
    let db = Db::open_with(&dir, opts(4)).unwrap();

    // Mix single-shard and multi-shard batches. Only the multi-shard ones can
    // expose a per-participant disagreement, and only interleaving them with
    // single-shard commits can expose an ordering inversion.
    for round in 0..8u64 {
        let mut b = db.batch();
        b.insert(round, round * 7);
        b.commit().unwrap();

        let mut b = db.batch();
        for key in 0..16u64 {
            b.insert(key, round * 101 + key);
        }
        b.commit().unwrap();
    }
    drop(db);

    // version -> (time, shards that recorded it)
    let mut stamps: BTreeMap<u64, (u64, Vec<String>)> = BTreeMap::new();
    for entry in std::fs::read_dir(&dir).unwrap().flatten() {
        let p = entry.path();
        if p.extension().is_none_or(|x| x != "wal") {
            continue;
        }
        let who = p.file_name().unwrap().to_string_lossy().into_owned();
        let bytes = std::fs::read(&p).unwrap();
        for rec in Scanner::new(&bytes, 0).flatten() {
            if !matches!(rec.rtype, RecType::ShardCommit | RecType::Abort) {
                continue;
            }
            let time = rec.commit_time().unwrap().unwrap_or_else(|| {
                panic!(
                    "{who}: commit marker for version {} carries no time; \
                                           every commit this build writes is stamped",
                    rec.commit_version
                )
            });
            let slot = stamps
                .entry(rec.commit_version)
                .or_insert((time, Vec::new()));
            assert_eq!(
                slot.0, time,
                "version {} was stamped {} by {:?} and {} by {who}; participants of one \
                 commit must share one time",
                rec.commit_version, slot.0, slot.1, time
            );
            slot.1.push(who.clone());
        }
    }

    assert!(
        stamps.len() >= 16,
        "only {} stamped commits were examined; this test is blind",
        stamps.len()
    );
    assert!(
        stamps.values().any(|(_, who)| who.len() > 1),
        "no multi-shard commit was observed, so the agreement half asserts nothing"
    );

    let mut last = 0u64;
    for (version, (time, _)) in &stamps {
        assert!(
            *time >= last,
            "commit time went backwards at version {version}: {last} then {time}"
        );
        last = *time;
    }
}

/// A restart must not let the clock fall below what is already on disk.
///
/// The WAL below a checkpoint stops being replayed, so the checkpoint carries
/// the clock forward in the superblock. Without that, a reopen right after a
/// checkpoint resumes from the system clock alone — and a clock that stepped
/// backwards in between stamps new commits under old ones.
#[test]
fn i9_survives_a_checkpoint_that_reclaims_every_stamp() {
    let dir = tmpdir("i9-reopen");
    let _c = CleanDir(dir.clone());

    let db = Db::open_with(&dir, opts(2)).unwrap();
    for key in 0..8u64 {
        let mut b = db.batch();
        b.insert(key, key * 3);
        b.commit().unwrap();
    }
    db.checkpoint().unwrap();
    let clock_before = shard_commit_clocks(&dir);
    drop(db);

    assert!(
        clock_before.iter().any(|c| *c > 0),
        "the checkpoint persisted no commit clock, so a reopen has no floor"
    );

    // Reopen and commit again. The new stamps must sit above the persisted
    // floor even though the replayed log contributes none.
    let db = Db::open_with(&dir, opts(2)).unwrap();
    let mut b = db.batch();
    b.insert(99, 999);
    b.commit().unwrap();
    db.checkpoint().unwrap();
    let clock_after = shard_commit_clocks(&dir);
    drop(db);

    let before = clock_before.iter().copied().max().unwrap();
    let after = clock_after.iter().copied().max().unwrap();
    assert!(
        after > before,
        "the commit clock did not advance across a reopen: {before} then {after}"
    );
}

/// Every shard image's persisted commit clock.
fn shard_commit_clocks(dir: &std::path::Path) -> Vec<u64> {
    use yesno_core::store::superblock;
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let p = entry.path();
        if p.extension().is_none_or(|x| x != "yno") {
            continue;
        }
        let bytes = read_prefix(&p, 2 * yesno_core::store::PAGE);
        let page = yesno_core::store::PAGE;
        if let Some(sb) = superblock::pick(&bytes[..page], &bytes[page..]).unwrap() {
            out.push(sb.commit_clock);
        }
    }
    out
}
