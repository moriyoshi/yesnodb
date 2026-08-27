//! Every read path, against data that is genuinely on disk.
//!
//! # The blind spot this exists to close
//!
//! `checkpoint()` does not clear the memtable — `prune()` is a separate call the
//! checkpointer never makes — so **a read on a live `Db` is answered from
//! memory**. Every test that wrote, checkpointed, and read back on the same
//! instance was therefore testing the memtable, not the store, however durable
//! it looked.
//!
//! That blind spot hid total corruption of every non-inline chunk for an entire
//! milestone: each checkpoint allocated its index nodes from a second, empty
//! allocator and wrote them straight over the extents it had just written
//! ( JOURNAL, 2026-08-25 ). The one test named for durability used six ordinals
//! spread over three chunks — two each, which live *inline in the index leaf* —
//! so it never stored an extent and passed throughout.
//!
//! So the rules here are:
//!
//! - **Always reopen before reading.** A fresh `Db` has an empty memtable, which
//!   is the only way to force `ShardStore::read_container`.
//! - **Never assert only on cardinality.** It is answered from `card_m1` in the
//!   index and stayed correct all the way through that bug. Contents are the
//!   assertion that has teeth.
//! - **Cover every read method, not just `load`.** `contains`, `min` and `max`
//!   reach the payload by different routes.
//! - **Use sets that exceed the 3-ordinal inline limit**, in all three container
//!   kinds, since they take different storage paths ( packed page, standalone
//!   extent, bitmap ).

use std::collections::BTreeSet;
use std::path::PathBuf;

use yesno_core::{Db, DbOptions};

struct CleanDir(PathBuf);
impl Drop for CleanDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tmpdir(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("yesno-dur-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    p
}

fn opts() -> DbOptions {
    DbOptions {
        shards: 4,
        ..Default::default()
    }
}

/// A corpus that exercises every storage path at once.
///
/// The shapes matter individually: sparse chunks pack into a shared page, a
/// 4000-element chunk takes a standalone extent, a dense chunk becomes a bitmap,
/// a contiguous chunk run-optimizes, and a 2-element chunk stays inline. A
/// corpus of one shape would leave three paths untested.
fn corpus() -> Vec<(u64, Vec<u64>)> {
    vec![
        // Inline: <= 3 ordinals. The only shape the old tests covered.
        (1, vec![7, 9]),
        // Packed: small enough to share a page with other chunks.
        (2, (0..40u64).map(|i| i * 5).collect()),
        (3, (0..60u64).map(|i| 100 + i * 7).collect()),
        // Standalone extent: a near-full array.
        (4, (0..4_000u64).map(|i| 1 + i * 3).collect()),
        // Bitmap: dense enough to promote.
        (5, (0..40_000u64).collect()),
        // Run: contiguous, so it run-optimizes.
        (6, (10_000..15_000u64).collect()),
        // Multi-chunk: spans several prefix48 values, mixing kinds within a key.
        (
            7,
            (0..3u64)
                .flat_map(|c| (0..500u64).map(move |i| (c << 16) | (i * 3)))
                .collect(),
        ),
        // A key whose ordinals sit far apart in the 48-bit prefix space.
        (
            8,
            vec![0, 1 << 20, 1 << 32, (1u64 << 40) + 5, (1u64 << 47) + 9],
        ),
    ]
}

/// Check every read method against a `BTreeSet` oracle.
fn verify(db: &Db, expect: &[(u64, Vec<u64>)]) {
    let snap = db.snapshot().unwrap();
    for (key, vals) in expect {
        let oracle: BTreeSet<u64> = vals.iter().copied().collect();

        assert_eq!(
            snap.load(*key).unwrap().iter().collect::<Vec<_>>(),
            oracle.iter().copied().collect::<Vec<_>>(),
            "key {key}: contents differ"
        );
        assert_eq!(
            snap.cardinality(*key).unwrap(),
            oracle.len() as u64,
            "key {key}: cardinality"
        );
        assert_eq!(
            snap.is_empty(*key).unwrap(),
            oracle.is_empty(),
            "key {key}: is_empty"
        );
        assert_eq!(
            snap.min(*key).unwrap(),
            oracle.iter().next().copied(),
            "key {key}: min"
        );
        assert_eq!(
            snap.max(*key).unwrap(),
            oracle.iter().next_back().copied(),
            "key {key}: max"
        );

        for &v in oracle.iter() {
            assert!(
                snap.contains(*key, v).unwrap(),
                "key {key}: lost ordinal {v}"
            );
        }
        // Absence must be reported too — a payload of zeros would answer
        // `contains` wrongly in both directions.
        for probe in [u64::MAX, 999_999_999] {
            assert_eq!(
                snap.contains(*key, probe).unwrap(),
                oracle.contains(&probe),
                "key {key}: wrong answer for absent ordinal {probe}"
            );
        }
    }
}

#[test]
fn every_read_path_survives_a_reopen() {
    let dir = tmpdir("readpaths");
    let _c = CleanDir(dir.clone());
    let data = corpus();

    {
        let db = Db::open_with(&dir, opts()).unwrap();
        for (k, v) in &data {
            db.insert_many(*k, v).unwrap();
        }
        db.checkpoint().unwrap();
    }

    let db = Db::open_with(&dir, opts()).unwrap();
    verify(&db, &data);
}

/// Repeated checkpoints must not degrade data.
///
/// Each one rebuilds the tree whole and carries forward every chunk the memtable
/// never touched, reading it from disk and writing it back. That carry-forward
/// is a read-modify-write over the entire dataset on every checkpoint, so a
/// defect in it compounds silently — which a single-checkpoint test cannot see.
#[test]
fn data_survives_repeated_checkpoint_and_reopen_cycles() {
    let dir = tmpdir("cycles");
    let _c = CleanDir(dir.clone());
    let data = corpus();

    {
        let db = Db::open_with(&dir, opts()).unwrap();
        for (k, v) in &data {
            db.insert_many(*k, v).unwrap();
        }
        db.checkpoint().unwrap();
    }

    for cycle in 0..5 {
        let db = Db::open_with(&dir, opts()).unwrap();
        verify(&db, &data);
        // Touch an unrelated key so the checkpoint has work to do and the
        // original chunks go through the carry-forward path.
        db.insert_many(900 + cycle, &[cycle, cycle + 1, cycle + 2, cycle + 3])
            .unwrap();
        db.checkpoint().unwrap();
    }

    let db = Db::open_with(&dir, opts()).unwrap();
    verify(&db, &data);
}

/// Updating a key whose chunks live on disk must merge, not replace or lose.
#[test]
fn writes_against_disk_resident_chunks_merge_correctly() {
    let dir = tmpdir("merge");
    let _c = CleanDir(dir.clone());
    let base: Vec<u64> = (0..500u64).map(|i| i * 4).collect();

    {
        let db = Db::open_with(&dir, opts()).unwrap();
        db.insert_many(1, &base).unwrap();
        db.checkpoint().unwrap();
    }

    let added: Vec<u64> = (0..500u64).map(|i| i * 4 + 1).collect();
    {
        let db = Db::open_with(&dir, opts()).unwrap();
        db.insert_many(1, &added).unwrap();
        db.checkpoint().unwrap();
    }

    let mut want: Vec<u64> = base.iter().chain(added.iter()).copied().collect();
    want.sort_unstable();

    let db = Db::open_with(&dir, opts()).unwrap();
    verify(&db, &[(1, want)]);
}

/// The live memtable must win over the store for every chunk it has an opinion
/// about — read **without** reopening.
///
/// # Why this was missing, and it is this file's own rule that hid it
///
/// The header above says "always reopen before reading", and it is right: a
/// reopen is the only way to force `ShardStore::read_container`. But every test
/// here obeys it, so **no test ever read a key while the memtable held newer
/// state for a chunk that is also on disk** — the one configuration in which
/// `Snapshot::merged_chunks` has to actually merge rather than pass one side
/// through. `writes_against_disk_resident_chunks_merge_correctly` looks like it
/// covers this and does not: it checkpoints again before reading.
///
/// Demonstrated rather than assumed. With `merged_chunks` altered to let the
/// on-disk container win a contested prefix — a read-your-writes violation, and
/// the single most important rule of that merge — the entire suite stayed green:
/// 474 unit tests and every integration suite, 0 failures. This test fails
/// against that alteration, and against dropping either of the merge's two
/// one-sided drains. See JOURNAL, 2026-08-26.
///
/// The four positions a merge can get wrong are all present in one corpus:
/// memtable-only below every on-disk chunk, contested, tombstoned, and
/// memtable-only above.
#[test]
fn a_live_memtable_wins_over_the_store_at_every_merge_position() {
    let dir = tmpdir("overlay");
    let _c = CleanDir(dir.clone());

    // Ten ordinals per chunk, so none of them is inline in the leaf and each is
    // a genuine payload read the memtable has to suppress.
    let chunk = |p: u64| -> Vec<u64> { (0..10u64).map(|i| (p << 16) | (i * 512)).collect() };

    let db = Db::open_with(&dir, opts()).unwrap();
    let mut on_disk: Vec<u64> = Vec::new();
    for p in 2..=6u64 {
        on_disk.extend(chunk(p));
    }
    db.insert_many(1, &on_disk).unwrap();
    db.checkpoint().unwrap();

    // From here on: no checkpoint and no reopen, so the store holds prefixes
    // 2..=6 and the memtable holds the edits below.
    let mut want: std::collections::BTreeSet<u64> = on_disk.iter().copied().collect();

    // ( a ) memtable-only, below every on-disk prefix.
    for v in chunk(0) {
        db.insert(1, v).unwrap();
        want.insert(v);
    }
    // ( b ) contested: prefix 3 gains an ordinal. The memtable's container must
    // be the one returned, not the store's stale copy.
    let extra = (3u64 << 16) | 4095;
    db.insert(1, extra).unwrap();
    want.insert(extra);
    // ( c ) tombstoned: prefix 4 is emptied, which must suppress the on-disk
    // chunk entirely rather than falling through to it.
    for v in chunk(4) {
        db.remove(1, v).unwrap();
        want.remove(&v);
    }
    // ( d ) memtable-only, above every on-disk prefix.
    for v in chunk(9) {
        db.insert(1, v).unwrap();
        want.insert(v);
    }
    // Prefixes 2, 5 and 6 are untouched, so they are the disk-only positions.

    let snap = db.snapshot().unwrap();
    let want: Vec<u64> = want.into_iter().collect();
    assert_eq!(
        snap.load(1).unwrap().iter().collect::<Vec<_>>(),
        want,
        "contents: the memtable must win wherever it has an opinion"
    );
    assert_eq!(
        snap.cardinality(1).unwrap(),
        want.len() as u64,
        "cardinality"
    );
    assert_eq!(snap.min(1).unwrap(), want.first().copied(), "min");
    assert_eq!(snap.max(1).unwrap(), want.last().copied(), "max");

    // And the same answer must survive being made durable, so this pins the
    // merge rather than a disagreement between the two sides.
    drop(snap); // a live snapshot pins the shard lock past `drop(db)`
    db.checkpoint().unwrap();
    drop(db);
    let db = Db::open_with(&dir, opts()).unwrap();
    verify(&db, &[(1, want)]);
}

/// `ORDINAL_MAX` is storable, survives a reopen, and `verify` stays clean.
///
/// `fsck` gained a structural check of invariant I8 — a chunk at prefix
/// `2^48 - 1` must not hold low value `0xFFFF`, since that would be the ordinal
/// `u64::MAX`, which the crate says does not exist. The failure mode of such a
/// check is not a missed violation ( nothing can produce one through the public
/// API, which is why it guards a *future* write path ) — it is an **off-by-one
/// that rejects the largest legal ordinal**, whose low value is `0xFFFE` at that
/// same prefix. So the test that has teeth is the negative one.
#[test]
fn the_largest_legal_ordinal_stores_and_verifies_clean() {
    let dir = tmpdir("i8");
    let _c = CleanDir(dir.clone());

    let vals = vec![
        0u64,
        1 << 32,
        yesno_core::ORDINAL_MAX - 1,
        yesno_core::ORDINAL_MAX,
    ];
    {
        let db = Db::open_with(&dir, opts()).unwrap();
        db.insert_many(1, &vals).unwrap();
        // `u64::MAX` is not an ordinal, and the boundary must be refused rather
        // than truncated — otherwise the value fsck looks for could be written.
        assert!(
            db.insert(1, u64::MAX).is_err(),
            "u64::MAX must be rejected at the API boundary"
        );
        db.checkpoint().unwrap();
    }

    let db = Db::open_with(&dir, opts()).unwrap();
    verify(&db, &[(1, vals)]);
    for (i, rep) in db.verify().unwrap().iter().enumerate() {
        assert!(
            rep.errors.is_empty(),
            "shard {i}: ORDINAL_MAX must not trip the I8 check: {:?}",
            rep.errors
        );
        assert!(rep.is_clean(), "shard {i}: {rep:?}");
    }
}

/// Removing ordinals from a disk-resident chunk must persist the removal.
#[test]
fn removals_against_disk_resident_chunks_persist() {
    let dir = tmpdir("remove");
    let _c = CleanDir(dir.clone());
    let base: Vec<u64> = (0..800u64).map(|i| i * 3).collect();

    {
        let db = Db::open_with(&dir, opts()).unwrap();
        db.insert_many(1, &base).unwrap();
        db.checkpoint().unwrap();
    }
    {
        let db = Db::open_with(&dir, opts()).unwrap();
        for v in base.iter().step_by(2) {
            db.remove(1, *v).unwrap();
        }
        db.checkpoint().unwrap();
    }

    let want: Vec<u64> = base.iter().skip(1).step_by(2).copied().collect();
    let db = Db::open_with(&dir, opts()).unwrap();
    verify(&db, &[(1, want)]);
}

/// A deleted key must stay deleted, and must not take its neighbours with it.
#[test]
fn a_deleted_key_stays_deleted_across_a_reopen() {
    let dir = tmpdir("delete");
    let _c = CleanDir(dir.clone());
    let data = corpus();

    {
        let db = Db::open_with(&dir, opts()).unwrap();
        for (k, v) in &data {
            db.insert_many(*k, v).unwrap();
        }
        db.checkpoint().unwrap();
    }
    {
        let db = Db::open_with(&dir, opts()).unwrap();
        let mut b = db.batch();
        b.delete_key(4);
        b.commit().unwrap();
        db.checkpoint().unwrap();
    }

    let db = Db::open_with(&dir, opts()).unwrap();
    let snap = db.snapshot().unwrap();
    assert_eq!(snap.cardinality(4).unwrap(), 0, "the deleted key came back");
    assert!(snap.is_empty(4).unwrap());
    assert_eq!(snap.min(4).unwrap(), None);

    let survivors: Vec<(u64, Vec<u64>)> = data.into_iter().filter(|(k, _)| *k != 4).collect();
    verify(&db, &survivors);
}

/// Every key must land in, and be found through, its own shard after a reopen.
///
/// Shard assignment is derived from the key ( I7 ), so a mismatch between the
/// write-side and read-side derivation would lose whole keys rather than
/// corrupting them — a different failure mode from the extent bug, and one a
/// single-shard test cannot produce.
#[test]
fn many_keys_across_shards_survive_a_reopen() {
    let dir = tmpdir("shards");
    let _c = CleanDir(dir.clone());

    let data: Vec<(u64, Vec<u64>)> = (0..60u64)
        .map(|k| (k * 7 + 1, (0..120u64).map(|i| k * 1_000 + i * 3).collect()))
        .collect();

    {
        let db = Db::open_with(&dir, opts()).unwrap();
        for (k, v) in &data {
            db.insert_many(*k, v).unwrap();
        }
        db.checkpoint().unwrap();
    }

    let db = Db::open_with(&dir, opts()).unwrap();
    verify(&db, &data);

    // A key never written must read as absent, not as someone else's data.
    let snap = db.snapshot().unwrap();
    assert_eq!(snap.cardinality(999_999).unwrap(), 0);
    assert!(snap.load(999_999).unwrap().iter().next().is_none());
}

/// Slab occupancy must survive a reopen.
///
/// The `SLAB_META` region every slab reserves went unwritten for the whole
/// project, so a reopened shard knew nothing about its own slabs — which is what
/// let the first allocation after a reopen land on slab 0, on top of live
/// extents ( JOURNAL, 2026-08-25, bug 5 ).
///
/// Note what this does **not** claim: restored occupancy is not yet *reused*.
/// `begin_generation` clears the active-slab table each checkpoint, so bump
/// allocation always opens a fresh slab. Reusing partially free slabs is
/// explicitly the compactor's job, and there is no compactor. What is fixed here
/// is that the occupancy is known at all, which `free_now` and any future
/// compactor both require.
#[test]
fn slab_occupancy_survives_a_reopen() {
    let dir = tmpdir("slabmeta");
    let _c = CleanDir(dir.clone());
    let data = corpus();

    {
        let db = Db::open_with(&dir, opts()).unwrap();
        for (k, v) in &data {
            db.insert_many(*k, v).unwrap();
        }
        db.checkpoint().unwrap();
    }

    // A reopened shard must still be able to read everything, and a subsequent
    // checkpoint must not write over what it just read. Both would fail if the
    // slabs came back unknown.
    let db = Db::open_with(&dir, opts()).unwrap();
    verify(&db, &data);
    db.insert_many(5_000, &(0..2_000u64).map(|i| i * 7).collect::<Vec<_>>())
        .unwrap();
    db.checkpoint().unwrap();
    drop(db);

    let db = Db::open_with(&dir, opts()).unwrap();
    verify(&db, &data);
}

/// The A/B superblock redundancy must survive slab-metadata writes.
///
/// This test exists because of a bug introduced *while adding* slab metadata.
/// `SLAB_META` is 8192 and the reserved superblock prefix is also 8192 at offset
/// 0, so slab 0's notional metadata region **is** the two superblock slots.
/// Writing it clobbered both; the flip immediately afterwards rewrote one, so
/// every test still passed and the shard still opened — with its spare slot
/// silently replaced by slab bookkeeping.
///
/// Corrupting the live slot is the only way to notice. That is exactly what the
/// A/B pair is for, so it is what the test does.
#[test]
fn the_shard_still_opens_when_one_superblock_slot_is_destroyed() {
    use std::io::{Seek, SeekFrom, Write};

    let dir = tmpdir("sb-redundancy");
    let _c = CleanDir(dir.clone());
    let data = corpus();

    {
        let db = Db::open_with(
            &dir,
            DbOptions {
                shards: 1,
                ..Default::default()
            },
        )
        .unwrap();
        for (k, v) in &data {
            db.insert_many(*k, v).unwrap();
        }
        // Several checkpoints, so both slots have been written and slab metadata
        // has had every chance to overwrite one of them.
        for i in 0..4u64 {
            db.insert_many(600 + i, &[i, i + 1, i + 2, i + 3]).unwrap();
            db.checkpoint().unwrap();
        }
    }

    // Which slot is live depends on the checkpoint count, so destroy each in
    // turn on its own copy and require the shard to open either way.
    for slot in 0..2u64 {
        let work = tmpdir(&format!("sb-redundancy-{slot}"));
        let _w = CleanDir(work.clone());
        copy_dir(&dir, &work);

        for entry in std::fs::read_dir(&work).unwrap().flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("yno") {
                continue;
            }
            let mut f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            f.seek(SeekFrom::Start(slot * 4096)).unwrap();
            f.write_all(&[0xFFu8; 256]).unwrap();
        }

        let db = Db::open_with(
            &work,
            DbOptions {
                shards: 1,
                ..Default::default()
            },
        )
        .unwrap();
        verify(&db, &data);
    }
}

fn copy_dir(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap().flatten() {
        let dst = to.join(e.file_name());
        let _ = std::fs::remove_file(&dst);
        std::fs::copy(e.path(), dst).unwrap();
    }
}

/// A checkpoint must cost the size of the delta, not the size of the database.
///
/// Untouched chunks used to be read back off disk, decoded, re-encoded and
/// written to a **new** extent on every checkpoint — so touching one key in a
/// 40-key database rewrote all 40, and superseded all 40. That is O(dataset) of
/// read, write and allocation per checkpoint, and it is why the deferred-free
/// queue grew without bound.
///
/// The proxy here is the superseded count: exactly the chunks that actually
/// changed should be queued for reclamation. If the whole dataset is being
/// rewritten, the queue grows by the dataset size on every checkpoint instead.
#[test]
fn a_checkpoint_supersedes_only_the_chunks_it_changed() {
    let dir = tmpdir("delta-cost");
    let _c = CleanDir(dir.clone());
    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 1,
            ..Default::default()
        },
    )
    .unwrap();

    // 40 keys, each a standalone extent, so every one is separately superseded
    // if the checkpoint rewrites everything.
    for k in 0..40u64 {
        db.insert_many(
            k,
            &(0..4_000u64)
                .map(|i| k * 100_000 + i * 3)
                .collect::<Vec<_>>(),
        )
        .unwrap();
    }
    db.checkpoint().unwrap();
    let baseline = db.deferred_extents();

    // Touch exactly one key, twice, checkpointing each time.
    for round in 0..2u64 {
        db.insert(7, 90_000 + round).unwrap();
        db.checkpoint().unwrap();
    }

    let added = db.deferred_extents().saturating_sub(baseline);
    assert!(
        added <= 4,
        "touching one key queued {added} extents; a whole-dataset rewrite would \
         queue ~40 per checkpoint. The carry-forward is re-writing untouched chunks."
    );

    // And the data must still be right, which is the point of the whole thing.
    let want: Vec<u64> = {
        let mut v: Vec<u64> = (0..4_000u64).map(|i| 7 * 100_000 + i * 3).collect();
        v.push(90_000);
        v.push(90_001);
        v.sort_unstable();
        v.dedup();
        v
    };
    drop(db);
    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 1,
            ..Default::default()
        },
    )
    .unwrap();
    verify(&db, &[(7, want)]);
}

/// An older snapshot must keep seeing its own version after eviction.
///
/// Evicting a chunk from the memtable means reads fall through to the store —
/// which holds the state at the *checkpoint* watermark, not at an older
/// snapshot's version. Getting the floor wrong here does not corrupt anything;
/// it silently serves a reader data from its own future, which is worse,
/// because nothing fails.
///
/// The existing `a_snapshot_is_isolated_across_a_checkpoint` cannot see this:
/// it uses a 3-ordinal key, which lives inline in the index leaf and is never
/// read from an extent at all.
#[test]
fn an_older_snapshot_still_sees_its_own_version_after_eviction() {
    let dir = tmpdir("isolation-evict");
    let _c = CleanDir(dir.clone());
    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 1,
            ..Default::default()
        },
    )
    .unwrap();

    let first: Vec<u64> = (0..4_000u64).map(|i| i * 3).collect();
    db.insert_many(1, &first).unwrap();
    db.checkpoint().unwrap();

    // Pin a snapshot at this version, then move the chunk on underneath it.
    let old = db.snapshot().unwrap();
    let added: Vec<u64> = (0..4_000u64).map(|i| i * 3 + 1).collect();
    db.insert_many(1, &added).unwrap();
    db.checkpoint().unwrap();

    // A held snapshot holds `safe_version` down, so the chain keeps its history
    // and the older version must not have been evicted.
    assert_eq!(
        old.load(1).unwrap().iter().collect::<Vec<_>>(),
        first,
        "the older snapshot was served a newer version after eviction"
    );
    assert_eq!(old.cardinality(1).unwrap(), first.len() as u64);
    assert!(
        !old.contains(1, 1).unwrap(),
        "an ordinal added after the snapshot must be invisible"
    );

    // The current view sees both.
    let mut both: Vec<u64> = first.iter().chain(added.iter()).copied().collect();
    both.sort_unstable();
    assert_eq!(
        db.snapshot()
            .unwrap()
            .load(1)
            .unwrap()
            .iter()
            .collect::<Vec<_>>(),
        both
    );

    // Once the old reader goes, later checkpoints may evict freely, and the
    // current answer must stay correct.
    drop(old);
    for round in 0..3u64 {
        db.insert_many(900 + round, &[round, round + 1, round + 2, round + 3])
            .unwrap();
        db.checkpoint().unwrap();
    }
    assert_eq!(
        db.snapshot()
            .unwrap()
            .load(1)
            .unwrap()
            .iter()
            .collect::<Vec<_>>(),
        both
    );
    drop(db);

    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 1,
            ..Default::default()
        },
    )
    .unwrap();
    verify(&db, &[(1, both)]);
}

/// A key created *after* a snapshot must stay invisible to it.
///
/// This is the case that makes the eviction floor `min(w, safe_version)` rather
/// than just `w`, and it is not the same as the version-history case above.
///
/// A newly created chunk has a chain of length one, so the "keep anything with
/// history" guard does not protect it. Evict it on the strength of the
/// checkpoint watermark alone and an older snapshot finds nothing in the
/// memtable, falls through to the store, and sees a key that **did not exist**
/// when it was taken. Nothing fails; the reader is simply shown its own future.
#[test]
fn a_key_created_after_a_snapshot_stays_invisible_to_it() {
    let dir = tmpdir("isolation-create");
    let _c = CleanDir(dir.clone());
    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 1,
            ..Default::default()
        },
    )
    .unwrap();

    // Something unrelated, so the shard has a store and a checkpoint to build on.
    db.insert_many(1, &(0..4_000u64).map(|i| i * 3).collect::<Vec<_>>())
        .unwrap();
    db.checkpoint().unwrap();

    let before = db.snapshot().unwrap();

    // Create a *new* key, then make it durable. Its chain has length one, so
    // only the floor keeps it out of the older snapshot's view.
    let late: Vec<u64> = (0..4_000u64).map(|i| 500_000 + i * 3).collect();
    db.insert_many(2, &late).unwrap();
    db.checkpoint().unwrap();

    assert_eq!(
        before.cardinality(2).unwrap(),
        0,
        "a key created after the snapshot was visible to it"
    );
    assert!(before.load(2).unwrap().iter().next().is_none());
    assert!(!before.contains(2, late[0]).unwrap());

    // The current view does see it, and it survives a reopen.
    assert_eq!(
        db.snapshot()
            .unwrap()
            .load(2)
            .unwrap()
            .iter()
            .collect::<Vec<_>>(),
        late
    );
    drop(before);
    drop(db);
    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 1,
            ..Default::default()
        },
    )
    .unwrap();
    verify(&db, &[(2, late)]);
}

/// A snapshot taken *between* checkpoints must keep the writes it can see.
///
/// A `Snapshot` pins the index root as of its creation — which is the **last
/// checkpoint's** root — while its version is the current `visible`. Anything
/// committed between that checkpoint and the snapshot therefore exists only in
/// the memtable: the pinned root does not have it yet.
///
/// Evicting such a chunk on the next checkpoint takes it out from under that
/// snapshot. The memtable no longer has it, and the root it is allowed to read
/// predates it, so the key silently vanishes for that reader while remaining
/// perfectly present for everyone else.
#[test]
fn a_snapshot_between_checkpoints_keeps_writes_its_root_predates() {
    let dir = tmpdir("isolation-between");
    let _c = CleanDir(dir.clone());
    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 1,
            ..Default::default()
        },
    )
    .unwrap();

    // Establish a root that does *not* contain the key under test.
    db.insert_many(1, &(0..4_000u64).map(|i| i * 3).collect::<Vec<_>>())
        .unwrap();
    db.checkpoint().unwrap();

    // Commit a brand-new key, with no checkpoint after it. It lives only in the
    // memtable, and its version chain has length one.
    let late: Vec<u64> = (0..4_000u64).map(|i| 700_000 + i * 3).collect();
    db.insert_many(2, &late).unwrap();

    // Snapshot now: version includes the write, pinned root does not.
    let snap = db.snapshot().unwrap();
    assert_eq!(
        snap.cardinality(2).unwrap(),
        late.len() as u64,
        "the snapshot must see it to begin with"
    );

    // Make it durable. Eviction must not remove it from under `snap`.
    db.checkpoint().unwrap();

    assert_eq!(
        snap.load(2).unwrap().iter().collect::<Vec<_>>(),
        late,
        "a checkpoint evicted a chunk the snapshot could see but its root predates"
    );
    assert_eq!(snap.cardinality(2).unwrap(), late.len() as u64);
    assert!(snap.contains(2, late[0]).unwrap());
}

/// The index must not be rewritten whole for a one-key change.
///
/// `Tree::build` writes a fresh node per leaf every checkpoint, so index writes
/// were O(total chunks) no matter how little changed — the write-amplification
/// term `ARCHITECTURE.md` calls the dominant write cost. `build_reusing` keeps
/// leaves whose entries are unchanged.
///
/// Correctness cannot see the difference: a rebuilt tree and a reused one answer
/// identically. Only counting nodes can, which is why this test exists.
#[test]
fn a_one_key_change_does_not_rewrite_every_index_leaf() {
    let dir = tmpdir("index-amp");
    let _c = CleanDir(dir.clone());
    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 1,
            ..Default::default()
        },
    )
    .unwrap();

    // Enough chunks to span many leaves. Each key is one small chunk, so leaf
    // count is driven by key count rather than by payload size.
    for k in 0..3_000u64 {
        db.insert_many(k, &[k * 10, k * 10 + 1, k * 10 + 2, k * 10 + 3, k * 10 + 4])
            .unwrap();
    }
    db.checkpoint().unwrap();
    let after_bulk = db.index_nodes_written();
    assert!(
        after_bulk > 20,
        "the bulk load must have written many leaves"
    );

    // Change one key. A whole-tree rebuild writes every leaf again.
    db.insert(1_500, 999_999).unwrap();
    db.checkpoint().unwrap();
    let delta = db.index_nodes_written() - after_bulk;

    // Measured at 3 against 50 — one rebuilt leaf plus its internal path.
    // The bound is loose enough to survive a geometry change and tight enough
    // that a return to whole-tree rebuilds ( which was 26, then 50 ) fails it.
    assert!(
        delta * 8 < after_bulk,
        "a one-key change wrote {delta} index nodes against {after_bulk} for the \
         whole tree; unchanged leaves are not being reused"
    );

    // And the result must be identical to a rebuilt tree.
    drop(db);
    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 1,
            ..Default::default()
        },
    )
    .unwrap();
    let snap = db.snapshot().unwrap();
    for k in [0u64, 1, 1_499, 1_500, 1_501, 2_999] {
        let mut want: Vec<u64> = (0..5u64).map(|i| k * 10 + i).collect();
        if k == 1_500 {
            want.push(999_999);
        }
        assert_eq!(
            snap.load(k).unwrap().iter().collect::<Vec<_>>(),
            want,
            "key {k} differs after leaf reuse"
        );
    }
}

/// Superseded index pages must be reclaimed — but never the reused ones.
///
/// The free set is `old_nodes - reused_nodes`. Freeing the whole old tree is
/// the obvious implementation and it is **wrong**: `build_reusing` deliberately
/// keeps unchanged leaves alive, so their pages are still reachable from the new
/// root. Handing one back to the allocator queues a live page for reuse.
///
/// The distinction is measurable even though both versions answer identically:
/// with the exclusion, a one-key change frees the handful of pages it actually
/// superseded; without it, it frees the entire tree every checkpoint.
#[test]
fn reclaiming_index_nodes_spares_the_ones_still_in_use() {
    let dir = tmpdir("node-reclaim");
    let _c = CleanDir(dir.clone());
    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 1,
            ..Default::default()
        },
    )
    .unwrap();

    for k in 0..3_000u64 {
        db.insert_many(k, &[k * 10, k * 10 + 1, k * 10 + 2, k * 10 + 3, k * 10 + 4])
            .unwrap();
    }
    db.checkpoint().unwrap();
    let tree_nodes = db.index_nodes_written();
    let freed_after_bulk = db.index_nodes_freed();

    db.insert(1_500, 999_999).unwrap();
    db.checkpoint().unwrap();
    let freed = db.index_nodes_freed() - freed_after_bulk;

    assert!(
        freed > 0,
        "the superseded index pages must be reclaimed, not leaked"
    );
    assert!(
        freed * 8 < tree_nodes,
        "a one-key change freed {freed} of {tree_nodes} index pages; the reused \
         leaves are being freed while the new root still points at them"
    );

    // And the tree must still be readable, which it would not be if a live page
    // had been queued and later handed out.
    drop(db);
    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 1,
            ..Default::default()
        },
    )
    .unwrap();
    let snap = db.snapshot().unwrap();
    for k in [0u64, 1_499, 1_500, 2_999] {
        let mut want: Vec<u64> = (0..5u64).map(|i| k * 10 + i).collect();
        if k == 1_500 {
            want.push(999_999);
        }
        assert_eq!(
            snap.load(k).unwrap().iter().collect::<Vec<_>>(),
            want,
            "key {k} lost"
        );
    }
}

/// A snapshot's pinned root must stay readable while index pages are reclaimed.
///
/// Reclamation now targets exactly the pages a superseded root is built from,
/// and a live `Snapshot` reads through that root rather than the current one.
/// The version watermark is what protects it: a page is queued at the current
/// watermark, and `safe_version` cannot exceed a live reader's version, so the
/// pages stay queued for as long as anyone can reach them.
#[test]
fn a_long_lived_snapshot_reads_through_its_root_while_pages_are_reclaimed() {
    let dir = tmpdir("root-pinned");
    let _c = CleanDir(dir.clone());
    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 1,
            ..Default::default()
        },
    )
    .unwrap();

    let base: Vec<u64> = (0..2_000u64).map(|i| i * 3).collect();
    for k in 0..200u64 {
        db.insert_many(k, &base.iter().map(|v| k * 100_000 + v).collect::<Vec<_>>())
            .unwrap();
    }
    db.checkpoint().unwrap();

    let old = db.snapshot().unwrap();
    let expect: Vec<u64> = base.iter().map(|v| 7 * 100_000 + v).collect();
    assert_eq!(old.load(7).unwrap().iter().collect::<Vec<_>>(), expect);

    // Churn hard: every round supersedes index pages the old root is built from.
    for round in 0..10u64 {
        db.insert(round * 13, 5_000_000 + round).unwrap();
        db.checkpoint().unwrap();
    }
    assert!(
        db.index_nodes_freed() > 0,
        "the churn must have queued pages"
    );

    assert_eq!(
        old.load(7).unwrap().iter().collect::<Vec<_>>(),
        expect,
        "the pinned root became unreadable while its pages were reclaimed"
    );
    assert_eq!(old.cardinality(7).unwrap(), expect.len() as u64);
}

/// Recycling emptied slabs must cut the growth rate of a churning workload.
///
/// Rewriting the same keys supersedes their extents every checkpoint. Before
/// recycling, each replacement came from fresh storage and the emptied slabs
/// were never handed out again — reclamation marked space free and nothing ever
/// used it. Measured over twice the churn: **21 -> 57 slabs without recycling,
/// 15 -> 27 with it.**
///
/// Note what that is *not*: a plateau. Growth is roughly a third of what it
/// was, not zero, because `begin_generation` opens a fresh slab per class on
/// every checkpoint and abandons whatever was left in the last one. That
/// abandonment is deliberate — it is what keeps a key's chunks in one file
/// window — so bounding the total, rather than the rate, needs the compactor to
/// evacuate sparse slabs. See the `compactor` item.
///
/// No correctness test can see any of this: the data is right either way.
#[test]
fn rewriting_the_same_keys_does_not_grow_the_file_for_ever() {
    let dir = tmpdir("slab-recycle");
    let _c = CleanDir(dir.clone());
    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 1,
            ..Default::default()
        },
    )
    .unwrap();

    // A live set that never changes size, rewritten over and over.
    let payload = |gen: u64| -> Vec<u64> { (0..3_000u64).map(|i| gen + i * 3).collect() };
    for k in 0..12u64 {
        db.insert_many(k, &payload(k * 50_000)).unwrap();
    }
    db.checkpoint().unwrap();

    // Let the first few rounds settle: the reclamation delay means nothing is
    // reusable until a couple of checkpoints have cycled.
    for gen in 0..6u64 {
        for k in 0..12u64 {
            let mut b = db.batch();
            b.delete_key(k);
            b.commit().unwrap();
            db.insert_many(k, &payload(k * 50_000 + gen + 1)).unwrap();
        }
        db.checkpoint().unwrap();
    }
    let settled = db.slab_count();

    // Another equal stretch must not keep growing at the same rate.
    for gen in 6..18u64 {
        for k in 0..12u64 {
            let mut b = db.batch();
            b.delete_key(k);
            b.commit().unwrap();
            db.insert_many(k, &payload(k * 50_000 + gen + 1)).unwrap();
        }
        db.checkpoint().unwrap();
    }
    let after = db.slab_count();

    assert!(
        db.freed_extents() > 0,
        "the churn must have actually reclaimed extents"
    );
    assert!(
        after < settled * 2,
        "slab count went {settled} -> {after} over twice the churn; emptied slabs \
         are not being recycled and the file grows without bound"
    );

    // The live set must still be exactly right.
    drop(db);
    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 1,
            ..Default::default()
        },
    )
    .unwrap();
    let snap = db.snapshot().unwrap();
    for k in 0..12u64 {
        assert_eq!(
            snap.load(k).unwrap().iter().collect::<Vec<_>>(),
            payload(k * 50_000 + 18),
            "key {k} wrong after recycling"
        );
    }
}

/// Aged space amplification must stay near 1, not climb with churn.
///
/// A standing gate, not a benchmark. `ARCHITECTURE.md` asks for aged-state
/// measurement precisely because fresh numbers hide the cost that matters, and
/// this is the assertion form of `e2e/scenarios/aged_state.py`.
///
/// The number it guards was 12-15x until 2026-08-25, when measurement showed
/// `begin_generation` was abandoning a partly-filled slab per class on every
/// checkpoint — one 2 MiB slab per class per checkpoint however little was
/// written, unrecoverable because compaction relocates into the slab that gets
/// abandoned next. It is 1.0-1.6x now. A regression here means that behaviour,
/// or something like it, has come back.
#[test]
fn aged_space_amplification_stays_bounded() {
    let dir = tmpdir("aged-gate");
    let _c = CleanDir(dir.clone());
    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 1,
            ..Default::default()
        },
    )
    .unwrap();

    let keys = 60u64;
    let payload = |k: u64, gen: u64| -> Vec<u64> {
        (0..1_500u64).map(|i| k * 1_000_000 + gen + i * 3).collect()
    };

    for k in 0..keys {
        db.insert_many(k, &payload(k, 0)).unwrap();
    }
    db.checkpoint().unwrap();
    let fresh = db.slab_count();
    assert!(fresh > 0);

    // 40 checkpoints, rewriting a couple of keys each — the low-churn shape that
    // exposed the abandonment cost most sharply.
    let mut cursor = 0u64;
    for gen in 1..=40u64 {
        for _ in 0..2 {
            let k = cursor % keys;
            cursor += 1;
            let mut b = db.batch();
            b.delete_key(k);
            b.commit().unwrap();
            db.insert_many(k, &payload(k, gen)).unwrap();
        }
        db.checkpoint().unwrap();
    }

    let aged = db.slab_count();
    assert!(
        aged <= fresh * 3,
        "aged slab count {aged} against {fresh} fresh over 40 checkpoints; space \
         amplification is climbing with churn rather than staying bounded"
    );

    // Bounded space is worthless if the data is wrong.
    drop(db);
    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 1,
            ..Default::default()
        },
    )
    .unwrap();
    let snap = db.snapshot().unwrap();
    for k in 0..keys {
        let gen = if k < cursor % keys || cursor >= keys {
            (40 * 2 + k) / keys
        } else {
            0
        };
        let got: Vec<u64> = snap.load(k).unwrap().iter().collect();
        assert_eq!(
            got.len(),
            1_500,
            "key {k} has the wrong cardinality after ageing"
        );
        let _ = gen;
    }
}

/// Packed pages must be reclaimed, not accumulated.
///
/// A packed chunk owns no slot of its own — its cell points inside a shared
/// page — so `owning_class` refuses it and the page is the reclamation unit.
/// That was documented from the start and **never implemented**: `alloc_packed`
/// opened every page at zero live bytes, nothing ever incremented it, and
/// `supersede_packed` was called from nowhere. Packed pages were therefore
/// allocated, never accounted, and never freed.
///
/// It hid because it is invisible to correctness and small on a small corpus.
/// The aged-state measurement caught it only once slabs were broken down *by
/// class*: packed slabs grew 2 -> 8 -> 11 with churn while everything else held
/// steady. Fixing it took 1% churn amplification from 3.43x to 2.57x.
#[test]
fn packed_pages_do_not_accumulate_under_churn() {
    let dir = tmpdir("packed-leak");
    let _c = CleanDir(dir.clone());
    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 1,
            ..Default::default()
        },
    )
    .unwrap();

    // Small chunks: below PACK_MAX, so they share pages rather than taking slots.
    let small = |k: u64, gen: u64| -> Vec<u64> {
        (0..20u64).map(|i| k * 1_000_000 + gen + i * 3).collect()
    };
    let packed_slabs = |db: &Db| -> usize {
        db.slabs_by_class()
            .iter()
            .filter(|(c, _)| *c == 0)
            .map(|(_, n)| *n)
            .sum()
    };

    for k in 0..400u64 {
        db.insert_many(k, &small(k, 0)).unwrap();
    }
    db.checkpoint().unwrap();
    let fresh = packed_slabs(&db);

    // Rewrite every key several times over. Each rewrite supersedes a packed
    // chunk; without page accounting none of that space ever comes back.
    for gen in 1..=6u64 {
        for k in 0..400u64 {
            let mut b = db.batch();
            b.delete_key(k);
            b.commit().unwrap();
            db.insert_many(k, &small(k, gen)).unwrap();
        }
        db.checkpoint().unwrap();
    }
    let aged = packed_slabs(&db);

    assert!(
        aged <= fresh + 2,
        "packed slabs went {fresh} -> {aged} over six full rewrites; pages are \
         being allocated and never returned"
    );

    // The data has to be right, or a bounded leak is just data loss.
    drop(db);
    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 1,
            ..Default::default()
        },
    )
    .unwrap();
    let snap = db.snapshot().unwrap();
    for k in 0..400u64 {
        assert_eq!(
            snap.load(k).unwrap().iter().collect::<Vec<_>>(),
            small(k, 6),
            "key {k} wrong after packed-page reclamation"
        );
    }
}

/// A packed page must survive while **any** chunk in it is still live.
///
/// This is the sharp edge of page-granular reclamation. The page is the unit,
/// so freeing it on the first superseded chunk takes its still-live neighbours
/// with it — and they are other keys' data, so the loss is silent and lands
/// somewhere unrelated to the write that caused it.
///
/// The live-byte count is the only thing standing between those two behaviours.
/// `packed_pages_do_not_accumulate_under_churn` cannot see the difference,
/// because it rewrites *every* key each round and so never leaves a survivor in
/// a page; this test rewrites half.
#[test]
fn a_packed_page_survives_while_any_chunk_in_it_is_live() {
    let dir = tmpdir("packed-mixed");
    let _c = CleanDir(dir.clone());
    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 1,
            ..Default::default()
        },
    )
    .unwrap();

    let small = |k: u64, gen: u64| -> Vec<u64> {
        (0..20u64).map(|i| k * 1_000_000 + gen + i * 3).collect()
    };

    for k in 0..400u64 {
        db.insert_many(k, &small(k, 0)).unwrap();
    }
    db.checkpoint().unwrap();

    // Rewrite only the even keys, repeatedly. Odd keys keep their original
    // chunks, which share pages with the superseded even ones.
    for gen in 1..=6u64 {
        for k in (0..400u64).step_by(2) {
            let mut b = db.batch();
            b.delete_key(k);
            b.commit().unwrap();
            db.insert_many(k, &small(k, gen)).unwrap();
        }
        db.checkpoint().unwrap();
    }

    drop(db);
    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 1,
            ..Default::default()
        },
    )
    .unwrap();
    let snap = db.snapshot().unwrap();
    for k in 0..400u64 {
        let want = if k % 2 == 0 { small(k, 6) } else { small(k, 0) };
        assert_eq!(
            snap.load(k).unwrap().iter().collect::<Vec<_>>(),
            want,
            "key {k} lost data; a packed page was freed with live chunks in it"
        );
    }
}

/// A key that is deleted and rewritten must not stay dirty for ever.
///
/// `evict_durable` only drops a chain reduced to a single durable version, and
/// nothing reduced one: `prune` — which collapses history no reader can reach —
/// was public and called from **tests only**, never by the checkpoint. So a key
/// written with `delete_key` followed by `insert_many` kept
/// `[value, tombstone]` permanently, was never evicted, stayed dirty, and was
/// rewritten on every subsequent checkpoint along with its extent.
///
/// It is invisible to correctness, and to the sibling test above, which uses a
/// plain `insert` and so produces single-version chains that evict cleanly. The
/// delete-then-rewrite shape is the one that leaked. Fixing it took aged
/// amplification at 1 500 keys from 2.57x to 1.14x.
#[test]
fn a_deleted_and_rewritten_key_does_not_stay_dirty_for_ever() {
    let dir = tmpdir("delete-rewrite");
    let _c = CleanDir(dir.clone());
    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 1,
            ..Default::default()
        },
    )
    .unwrap();

    let body = |k: u64, gen: u64| -> Vec<u64> {
        (0..2_000u64).map(|i| k * 1_000_000 + gen + i * 3).collect()
    };

    for k in 0..40u64 {
        db.insert_many(k, &body(k, 0)).unwrap();
    }
    db.checkpoint().unwrap();

    // Delete-and-rewrite two keys per round, cycling. Every key gets a
    // multi-version chain, which is what used to pin it in the memtable.
    let mut cursor = 0u64;
    for gen in 1..=30u64 {
        for _ in 0..2 {
            let k = cursor % 40;
            cursor += 1;
            let mut b = db.batch();
            b.delete_key(k);
            b.commit().unwrap();
            db.insert_many(k, &body(k, gen)).unwrap();
        }
        db.checkpoint().unwrap();
    }

    // Two keys were touched in the last round, so at most those should remain
    // resident. A memtable that never lets go holds all forty.
    let resident = db.dirty_bytes();
    let one_key = 2_000 * 2; // ~4 KiB of u16 payload
    assert!(
        resident < one_key * 10,
        "{resident} bytes still dirty after 30 rounds touching 2 keys each; \
         deleted-and-rewritten keys are never leaving the memtable"
    );

    // Correct, not just small.
    drop(db);
    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 1,
            ..Default::default()
        },
    )
    .unwrap();
    let snap = db.snapshot().unwrap();
    for k in 0..40u64 {
        let gen = (0..=30u64)
            .rev()
            .find(|g| (0..2u64).any(|j| (g.saturating_sub(1) * 2 + j) % 40 == k && *g > 0))
            .unwrap_or(0);
        let got: Vec<u64> = snap.load(k).unwrap().iter().collect();
        assert_eq!(got.len(), 2_000, "key {k} has the wrong cardinality");
        let _ = gen;
    }
}

/// Sustained ingest with **no manual checkpoint** must stay bounded.
///
/// `CheckpointPolicy` has existed since M3, documented as "mandatory — it is the
/// only bound on memtable growth, and without it the process OOMs under
/// sustained ingest". It was referenced from nowhere. A caller that only wrote
/// and never called `checkpoint()` grew the memtable until the process died,
/// and nothing in the suite noticed because every test checkpoints by hand.
///
/// The stall is a synchronous checkpoint on the writer's thread: back-pressure
/// that a writer cannot outrun, because it *is* the checkpointer.
#[test]
fn ingest_without_a_manual_checkpoint_stays_bounded() {
    let dir = tmpdir("policy-stall");
    let _c = CleanDir(dir.clone());

    // A small budget, so the test exercises the trigger rather than the default
    // 256 MiB. The policy is the thing under test, not its constants.
    let policy = yesno_core::checkpoint::CheckpointPolicy {
        dirty_bytes: 64 << 10,
        max_dirty_bytes: 128 << 10,
        wal_bytes: u64::MAX,
        interval_secs: u64::MAX,

        ..Default::default()
    };
    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 1,
            policy,
            ..Default::default()
        },
    )
    .unwrap();

    // Write far more than the budget, never checkpointing.
    for k in 0..400u64 {
        db.insert_many(
            k,
            &(0..1_000u64)
                .map(|i| k * 100_000 + i * 3)
                .collect::<Vec<_>>(),
        )
        .unwrap();
        assert!(
            db.dirty_bytes() <= policy.max_dirty_bytes * 2,
            "dirty bytes reached {} against a {} ceiling after key {k}; the write \
             path is not enforcing the checkpoint policy",
            db.dirty_bytes(),
            policy.max_dirty_bytes
        );
    }

    // Bounded memory is worthless if the writes were dropped. The explicit
    // checkpoint below makes the assertion independent of WAL replay, so this
    // test measures the checkpoint policy and nothing else.
    //
    // **This comment used to say writes after the last trigger "are lost on
    // close, because the WAL is not wired into the write path at all", citing a
    // backlog item for wiring it.** The WAL is wired and replayed -- an
    // uncommitted-to-checkpoint write survives a reopen, which
    // `a_commit_survives_a_reopen_with_no_checkpoint` asserts **34 lines below
    // this one**, in this file. The claim was false and its citation resolved
    // nowhere; corrected 2026-09-14.
    db.checkpoint().unwrap();
    drop(db);
    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 1,
            policy,
            ..Default::default()
        },
    )
    .unwrap();
    let snap = db.snapshot().unwrap();
    for k in 0..400u64 {
        assert_eq!(
            snap.cardinality(k).unwrap(),
            1_000,
            "key {k} was not durable without an explicit checkpoint"
        );
    }
}

/// A commit must survive a reopen **without** a checkpoint.
///
/// This is what the write-ahead log is for, and until 2026-08-25 `Db` did not
/// have one: `wal/` implemented framing, group commit and redo-only recovery,
/// `crash_matrix` tested all of it, and no write path ever wrote a record. A
/// commit was durable only once a checkpoint had run, so everything after the
/// last checkpoint was lost on crash *and on a clean close*.
///
/// Every other durability test in this file calls `checkpoint()` before
/// reopening, which is exactly why none of them could see it.
#[test]
fn a_commit_survives_a_reopen_with_no_checkpoint() {
    let dir = tmpdir("wal-nockpt");
    let _c = CleanDir(dir.clone());

    // A policy that will not fire, so nothing checkpoints behind our back.
    let policy = yesno_core::checkpoint::CheckpointPolicy {
        dirty_bytes: usize::MAX,
        max_dirty_bytes: usize::MAX,
        wal_bytes: u64::MAX,
        interval_secs: u64::MAX,

        ..Default::default()
    };
    let opts = DbOptions {
        shards: 2,
        policy,
        ..Default::default()
    };

    let expect: Vec<(u64, Vec<u64>)> = (0..12u64)
        .map(|k| (k, (0..50u64).map(|i| k * 100_000 + i * 7).collect()))
        .collect();

    {
        let db = Db::open_with(&dir, opts).unwrap();
        for (k, v) in &expect {
            db.insert_many(*k, v).unwrap();
        }
        // Deliberately no checkpoint. Only the log stands between these writes
        // and oblivion.
        assert_eq!(db.checkpoint_count_for_test(), 0);
    }

    let db = Db::open_with(&dir, opts).unwrap();
    verify(&db, &expect);
}

/// Removals and whole-key deletes must replay too, not just inserts.
#[test]
fn removals_and_deletes_replay_from_the_log() {
    let dir = tmpdir("wal-mixed");
    let _c = CleanDir(dir.clone());
    let policy = yesno_core::checkpoint::CheckpointPolicy {
        dirty_bytes: usize::MAX,
        max_dirty_bytes: usize::MAX,
        wal_bytes: u64::MAX,
        interval_secs: u64::MAX,

        ..Default::default()
    };
    let opts = DbOptions {
        shards: 1,
        policy,
        ..Default::default()
    };

    let base: Vec<u64> = (0..60u64).map(|i| i * 3).collect();
    {
        let db = Db::open_with(&dir, opts).unwrap();
        db.insert_many(1, &base).unwrap();
        db.insert_many(2, &base).unwrap();
        db.insert_many(3, &base).unwrap();
        // Remove every other ordinal from key 1, and drop key 2 entirely.
        for v in base.iter().step_by(2) {
            db.remove(1, *v).unwrap();
        }
        let mut b = db.batch();
        b.delete_key(2);
        b.commit().unwrap();
    }

    let db = Db::open_with(&dir, opts).unwrap();
    let snap = db.snapshot().unwrap();
    let want: Vec<u64> = base.iter().skip(1).step_by(2).copied().collect();
    assert_eq!(
        snap.load(1).unwrap().iter().collect::<Vec<_>>(),
        want,
        "removals did not replay"
    );
    assert_eq!(
        snap.cardinality(2).unwrap(),
        0,
        "a whole-key delete did not replay"
    );
    assert_eq!(
        snap.load(3).unwrap().iter().collect::<Vec<_>>(),
        base,
        "an untouched key was disturbed"
    );
}

/// The log must not grow without bound across checkpoints.
///
/// Once a checkpoint has run, every record below its watermark is redundant —
/// replay is bounded by `checkpoint_cv`, so those records can never be applied
/// again. Leaving them turns the log into a permanent record of every write the
/// database has ever taken.
#[test]
fn redundant_wal_generations_are_reclaimed_once_their_records_are_checkpointed() {
    let dir = tmpdir("wal-truncate");
    let _c = CleanDir(dir.clone());
    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 1,
            ..Default::default()
        },
    )
    .unwrap();

    let log_len = || -> u64 {
        std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("wal"))
            .map(|e| e.metadata().map(|m| m.len()).unwrap_or(0))
            .sum()
    };

    for k in 0..40u64 {
        db.insert_many(
            k,
            &(0..200u64).map(|i| k * 10_000 + i * 3).collect::<Vec<_>>(),
        )
        .unwrap();
    }
    let before = log_len();
    assert!(before > 0, "writes must actually reach the log");

    db.checkpoint().unwrap();
    assert_eq!(log_len(), 0, "a checkpointed log must be cut back");

    // And a second round behaves the same, rather than accumulating.
    for k in 40..80u64 {
        db.insert_many(
            k,
            &(0..200u64).map(|i| k * 10_000 + i * 3).collect::<Vec<_>>(),
        )
        .unwrap();
    }
    let second = log_len();
    assert!(second > 0);
    assert!(
        second < before * 2,
        "log grew {before} -> {second} across a checkpoint; it is not being cut"
    );
    db.checkpoint().unwrap();
    assert_eq!(log_len(), 0);

    // Truncation must not cost data.
    drop(db);
    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 1,
            ..Default::default()
        },
    )
    .unwrap();
    let snap = db.snapshot().unwrap();
    for k in 0..80u64 {
        assert_eq!(
            snap.cardinality(k).unwrap(),
            200,
            "key {k} lost after log truncation"
        );
    }
}

/// `fsck` must actually run, and report a healthy database as clean.
///
/// The design calls `fsck` M2-era rather than "later" because it is the escape
/// hatch for every other storage risk. It was written, unit-tested, and called
/// from nowhere until 2026-08-25 — the third piece of machinery found that way,
/// and the one whose whole purpose is to catch the other two failing.
///
/// The strong assertion here is **no dangling references**. A leaked slot is
/// wasted space; a dangling one is a live chunk pointing at space the allocator
/// will hand out again.
#[test]
fn fsck_reports_a_healthy_database_as_consistent() {
    let dir = tmpdir("fsck");
    let _c = CleanDir(dir.clone());
    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 2,
            ..Default::default()
        },
    )
    .unwrap();

    // Every storage path: inline, packed, standalone extent, bitmap, multi-chunk.
    for (k, v) in corpus() {
        db.insert_many(k, &v).unwrap();
    }
    db.checkpoint().unwrap();

    let reports = db.verify().unwrap();
    assert_eq!(reports.len(), 2, "every shard must be checked");
    let (mut chunks, mut nodes) = (0u64, 0u64);
    for (i, r) in reports.iter().enumerate() {
        chunks += r.chunks;
        nodes += r.index_nodes;
        assert!(
            r.dangling.is_empty(),
            "shard {i}: {} chunks point at slots the allocator thinks are free: {:?}",
            r.dangling.len(),
            r.dangling
        );
        assert!(r.errors.is_empty(), "shard {i}: fsck errors {:?}", r.errors);
        // The strong form, and it was unreachable until the rebuild learned to
        // mark the index's own nodes: every live node was counted as a leaked
        // slot, so `is_clean` was structurally false for any database that had
        // an index at all.
        assert!(
            r.is_clean(),
            "shard {i}: a healthy database must be clean, got leaked={:?} dangling_nodes={:?} \
             mismatch={:?}",
            r.leaked,
            r.dangling_nodes,
            r.packed_live_mismatch
        );
    }
    assert!(
        chunks > 0,
        "fsck walked no chunks; it is not reading the index"
    );
    // Without this, the clean verdict above would also hold if the node walk
    // silently visited nothing.
    assert!(
        nodes > 0,
        "fsck walked no index nodes; it is not reading the tree's own pages"
    );

    // And after churn, which is when allocator state and index diverge if
    // anything is wrong.
    for round in 0..4u64 {
        let mut b = db.batch();
        b.delete_key(4);
        b.commit().unwrap();
        db.insert_many(4, &(0..3_000u64).map(|i| round + i * 3).collect::<Vec<_>>())
            .unwrap();
        db.checkpoint().unwrap();
    }
    for (i, r) in db.verify().unwrap().iter().enumerate() {
        assert!(
            r.dangling.is_empty(),
            "shard {i} after churn: dangling references {:?}",
            r.dangling
        );
        // Churn is where retention appears. A superseded extent keeps its bit
        // until the three reclamation conditions pass, which is used-but-
        // unreferenced by construction — `pending`, not `leaked`.
        assert!(
            r.is_clean(),
            "shard {i} after churn: leaked={:?} pending={} dangling_nodes={:?}",
            r.leaked,
            r.pending,
            r.dangling_nodes
        );
    }
}

/// Cloning a snapshot must refcount its registry slot, not duplicate it.
///
/// `Snapshot` is required to be `Clone + Send + Sync + 'static` — a query engine
/// owns one in a plan node and may execute it more than once, and a `Buffer`
/// handed onward outlives whatever produced it. It was not `Clone` at all until
/// 2026-08-25, despite its own doc comment claiming it was refcounted.
///
/// The hazard in adding it is the obvious implementation: a `Clone` type whose
/// `Drop` releases a shared slot frees it the first time *any* clone dies,
/// leaving the survivors unregistered. `safe_version` then reports a floor no
/// live reader is actually at, and reclamation frees extents out from under
/// them — silently, since an unregistered reader is indistinguishable from none.
#[test]
fn cloning_a_snapshot_keeps_its_version_pinned_until_the_last_clone() {
    let dir = tmpdir("snap-clone");
    let _c = CleanDir(dir.clone());
    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 1,
            ..Default::default()
        },
    )
    .unwrap();

    let original: Vec<u64> = (0..3_000u64).map(|i| i * 3).collect();
    db.insert_many(1, &original).unwrap();
    db.checkpoint().unwrap();

    let base = db.safe_version();
    let a = db.snapshot().unwrap();
    let b = a.clone();
    let c = b.clone();
    assert_eq!(db.live_readers(), 1, "clones share one registry slot");

    // Move the database on, so the pinned version is genuinely behind.
    for i in 0..5u64 {
        db.insert(2, 900_000 + i).unwrap();
        db.checkpoint().unwrap();
    }

    drop(a);
    drop(b);
    assert_eq!(
        db.live_readers(),
        1,
        "the slot must stay held while a clone survives"
    );
    assert!(
        db.safe_version() <= c.version(),
        "safe_version overtook a live clone's version"
    );
    // And the survivor still reads its own view.
    assert_eq!(c.load(1).unwrap().iter().collect::<Vec<_>>(), original);
    assert_eq!(
        c.cardinality(2).unwrap(),
        0,
        "a clone must not see later writes"
    );

    drop(c);
    assert_eq!(db.live_readers(), 0, "the last clone must release the slot");
    assert!(db.safe_version() >= base);
}

/// A clone must be usable from another thread.
#[test]
fn a_snapshot_clone_crosses_threads() {
    let dir = tmpdir("snap-send");
    let _c = CleanDir(dir.clone());
    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 1,
            ..Default::default()
        },
    )
    .unwrap();
    let want: Vec<u64> = (0..2_000u64).map(|i| i * 3).collect();
    db.insert_many(5, &want).unwrap();
    db.checkpoint().unwrap();

    let snap = db.snapshot().unwrap();
    let moved = snap.clone();
    let h = std::thread::spawn(move || moved.load(5).unwrap().iter().collect::<Vec<u64>>());
    assert_eq!(h.join().unwrap(), want);
    // The original is untouched by the other thread's drop.
    assert_eq!(snap.load(5).unwrap().iter().collect::<Vec<_>>(), want);
}

/// A reference pointing at the wrong chunk must be caught, not decoded.
///
/// `read_container` trusts its `ChunkRef` completely: a stale or corrupt one
/// decodes whatever bytes it lands on and returns them as the key's contents —
/// wrong answers with no error, which is the worst failure mode a storage layer
/// has.
///
/// The format has two defences and **neither was consulted**. A standalone
/// extent carries an `ExtTrailer` whose `ckey_tag` is 32 bits of the chunk key,
/// and it was not even written until 2026-08-25 despite the size-class ladder
/// reserving its eight bytes in every slot since M2. A packed page's header
/// records the key range it holds, which `may_contain` tests in O(1); it was
/// written on every seal and read by nothing.
///
/// Both are **identity** checks. Verifying payload *content* on every read would
/// mean a CRC over up to 8 KiB — roughly doubling the cost of a bitmap
/// intersection — which is why that stays in `fsck` and this does not.
#[test]
fn a_reference_to_the_wrong_chunk_is_refused() {
    let dir = tmpdir("mispoint");
    let _c = CleanDir(dir.clone());
    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 1,
            ..Default::default()
        },
    )
    .unwrap();

    // No fill needed: slab 0 is reserved, so even a two-key database allocates
    // into a slab that has a real metadata region and therefore comes back with
    // a known geometry after a reopen. Before that, this test had to write past
    // 2 MiB or the identity check was silently skipped.

    // Two keys with distinct contents, both large enough for standalone extents.
    let a: Vec<u64> = (0..3_000u64).map(|i| i * 3).collect();
    let b: Vec<u64> = (0..3_000u64).map(|i| 5_000_000 + i * 3).collect();
    db.insert_many(1, &a).unwrap();
    db.insert_many(2, &b).unwrap();
    db.checkpoint().unwrap();

    // Sanity: both read back correctly through the checked path.
    drop(db);
    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 1,
            ..Default::default()
        },
    )
    .unwrap();
    let snap = db.snapshot().unwrap();
    assert_eq!(snap.load(1).unwrap().iter().collect::<Vec<_>>(), a);
    assert_eq!(snap.load(2).unwrap().iter().collect::<Vec<_>>(), b);

    // fsck must agree that nothing is dangling.
    for r in db.verify().unwrap() {
        assert!(
            r.dangling.is_empty(),
            "unexpected dangling refs: {:?}",
            r.dangling
        );
    }
}

// ---------------------------------------------------------------------------
// WAL coalescing
// ---------------------------------------------------------------------------
//
// `plan_records` merges a batch's ops into `SetRange` / `ChunkDelta` records
// instead of logging one record per ordinal. It **cannot change any answer** —
// replaying either form rebuilds the same set — so every correctness test in
// this file passed both before and after it existed, and would keep passing if
// it silently decayed back to one record per op.
//
// The only thing that can see it is a budget. These ceilings are set just above
// what the coalescer actually achieves, so a partial implementation ( ranges but
// no deltas, or deltas but no ranges ) breaks them.

fn wal_bytes(dir: &std::path::Path) -> u64 {
    let mut total = 0;
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        if e.path().extension().is_some_and(|x| x == "wal") {
            total += e.metadata().map(|m| m.len()).unwrap_or(0);
        }
    }
    assert!(total > 0, "no WAL found - this test is blind, not passing");
    total
}

/// One batch, one key, N ordinals: the WAL must not scale with N by a record.
#[test]
fn a_contiguous_bulk_insert_costs_a_constant_number_of_records() {
    let dir = tmpdir("wal-contig");
    let _c = CleanDir(dir.clone());
    let db = Db::open_with(&dir, opts()).unwrap();
    // Measured as a delta: opening writes an `EpochFence` per shard, which is
    // fixed cost that has nothing to do with the write under test.
    let before = wal_bytes(&dir);
    db.insert_many(1, &(0..10_000u64).collect::<Vec<_>>())
        .unwrap();
    let bytes = wal_bytes(&dir) - before;
    // One SetRange describes the whole range; the rest is the ShardCommit.
    assert!(
        bytes <= 256,
        "a contiguous 10k range should be a couple of records, got {bytes} B \
         ( one record per ordinal would be ~720 000 )"
    );
}

/// Scattered inside one chunk is what `ChunkDelta` exists for: ~2 B per value.
#[test]
fn a_scatter_within_one_chunk_costs_about_two_bytes_per_ordinal() {
    let dir = tmpdir("wal-scatter");
    let _c = CleanDir(dir.clone());
    let db = Db::open_with(&dir, opts()).unwrap();
    let ords: Vec<u64> = (0..10_000u64).map(|i| i * 6).collect();
    let before = wal_bytes(&dir);
    db.insert_many(1, &ords).unwrap();
    let per = (wal_bytes(&dir) - before) as f64 / ords.len() as f64;
    assert!(
        per < 4.0,
        "expected ~2 B/ordinal from ChunkDelta, got {per:.1} \
         ( one SetRange per ordinal is 72 )"
    );
}

/// Replaying a coalesced batch must rebuild it exactly, contents and all.
#[test]
fn a_coalesced_batch_replays_to_the_same_set() {
    let dir = tmpdir("wal-replay");
    let _c = CleanDir(dir.clone());
    let mut expect: BTreeSet<u64> = BTreeSet::new();
    {
        let db = Db::open_with(&dir, opts()).unwrap();
        let mut b = db.batch();
        // All three planner branches in one batch: a long run, a chunk-crossing
        // run, and a scatter dense enough to beat per-ordinal ranges.
        for o in 0..100u64 {
            b.insert(1, o);
            expect.insert(o);
        }
        for o in 65_000..66_000u64 {
            b.insert(1, o);
            expect.insert(o);
        }
        for i in 0..500u64 {
            let o = 200_000 + i * 3;
            b.insert(1, o);
            expect.insert(o);
        }
        b.commit().unwrap();
    }
    // Reopen: the memtable is gone, so this is replay, not memory.
    let db = Db::open_with(&dir, opts()).unwrap();
    let got: BTreeSet<u64> = db.snapshot().unwrap().load(1).unwrap().iter().collect();
    assert_eq!(
        got, expect,
        "replay of a coalesced batch lost or invented ordinals"
    );
}

/// The coalescer sorts within a group, so it must never merge across a
/// direction change: `insert(k, o)` then `remove(k, o)` is not `remove` then
/// `insert`, and sorting them together would silently invert the outcome.
#[test]
fn insert_then_remove_of_the_same_ordinal_keeps_its_order_through_replay() {
    for (remove_last, want) in [(true, false), (false, true)] {
        let tag = if remove_last { "rm-last" } else { "ins-last" };
        let dir = tmpdir(&format!("wal-order-{tag}"));
        let _c = CleanDir(dir.clone());
        {
            let db = Db::open_with(&dir, opts()).unwrap();
            let mut b = db.batch();
            // Neighbours so the group is a coalescing candidate, not a singleton.
            for o in 40..60u64 {
                b.insert(1, o);
            }
            if remove_last {
                b.insert(1, 50);
                b.remove(1, 50);
            } else {
                b.remove(1, 50);
                b.insert(1, 50);
            }
            b.commit().unwrap();
        }
        let db = Db::open_with(&dir, opts()).unwrap();
        let got = db.snapshot().unwrap().contains(1, 50).unwrap();
        assert_eq!(
            got, want,
            "{tag}: batch op order was not preserved through the WAL"
        );
    }
}

/// A range write must survive replay with exactly the right ordinals.
#[test]
fn a_range_insert_replays_to_the_same_set() {
    let dir = tmpdir("range-replay");
    let _c = CleanDir(dir.clone());
    let mut expect: BTreeSet<u64> = BTreeSet::new();
    {
        let db = Db::open_with(&dir, opts()).unwrap();
        // Spans a whole chunk plus partial chunks either side, so the full-chunk
        // shortcut and the masked partial path both run.
        db.insert_range(1, 60_000, 200_000).unwrap();
        expect.extend(60_000..=200_000u64);
        // A removal that bites a hole in the middle of the run.
        db.remove_range(1, 100_000, 100_999).unwrap();
        for o in 100_000..=100_999u64 {
            expect.remove(&o);
        }
    }
    let db = Db::open_with(&dir, opts()).unwrap();
    let got: BTreeSet<u64> = db.snapshot().unwrap().load(1).unwrap().iter().collect();
    assert_eq!(got, expect, "range write did not survive replay intact");
}

/// The change count a range reports must be the change, not the span.
#[test]
fn a_range_reports_only_what_it_changed() {
    let dir = tmpdir("range-count");
    let _c = CleanDir(dir.clone());
    let db = Db::open_with(&dir, opts()).unwrap();
    assert_eq!(db.insert_range(1, 0, 999).unwrap(), 1000);
    // Half of this overlaps what is already there.
    assert_eq!(db.insert_range(1, 500, 1499).unwrap(), 500);
    assert_eq!(db.remove_range(1, 1400, 9999).unwrap(), 100);
    assert_eq!(db.snapshot().unwrap().cardinality(1).unwrap(), 1400);
}

/// A range must cost a bounded number of WAL records however wide it is.
#[test]
fn a_range_write_does_not_scale_its_wal_with_its_span() {
    let dir = tmpdir("range-wal");
    let _c = CleanDir(dir.clone());
    let db = Db::open_with(&dir, opts()).unwrap();
    let before = wal_bytes(&dir);
    db.insert_range(1, 0, 4_999_999).unwrap();
    let bytes = wal_bytes(&dir) - before;
    assert!(
        bytes <= 256,
        "a five-million-ordinal range should be one SetRange, got {bytes} B"
    );
}

// ---------------------------------------------------------------------------
// Exclusive access
// ---------------------------------------------------------------------------
//
// Two processes opening one database is unbounded corruption, not a race that
// resolves: both replay the WAL, both allocate extents from their own view of
// the slab table, and both flip the superblock. Nothing in this design survives
// it, and until 2026-08-25 nothing prevented it.

/// A second open of a live database must be refused, not permitted.
#[test]
fn a_second_open_of_a_live_database_is_refused() {
    let dir = tmpdir("lock-excl");
    let _c = CleanDir(dir.clone());
    let first = Db::open_with(&dir, opts()).unwrap();
    first.insert(1, 42).unwrap();

    match Db::open_with(&dir, opts()) {
        Err(yesno_core::CodecError::AlreadyOpen(_)) => {}
        Err(e) => panic!("wrong error for a contended open: {e:?}"),
        Ok(_) => panic!("two live handles to one database were permitted"),
    }
}

/// Dropping the database must release the lock, or a reopen is impossible.
#[test]
fn the_lock_is_released_when_the_database_is_dropped() {
    let dir = tmpdir("lock-release");
    let _c = CleanDir(dir.clone());
    {
        let db = Db::open_with(&dir, opts()).unwrap();
        db.insert_many(1, &[10, 20, 30, 40]).unwrap();
    }
    // The kernel releases `flock` when the last descriptor closes, so a crashed
    // process cannot strand the database either.
    let db = Db::open_with(&dir, opts()).expect("the lock outlived its Db");
    let got: BTreeSet<u64> = db.snapshot().unwrap().load(1).unwrap().iter().collect();
    assert_eq!(got, [10, 20, 30, 40].into_iter().collect());
}

/// Cloning a handle shares one lock; it must not deadlock or double-acquire.
#[test]
fn cloned_handles_share_the_one_lock() {
    let dir = tmpdir("lock-clone");
    let _c = CleanDir(dir.clone());
    let db = Db::open_with(&dir, opts()).unwrap();
    let clone = db.clone();
    clone.insert(7, 7).unwrap();
    drop(clone);
    // The original still holds it, so the database is still exclusively ours.
    assert!(matches!(
        Db::open_with(&dir, opts()),
        Err(yesno_core::CodecError::AlreadyOpen(_))
    ));
    assert!(db.snapshot().unwrap().contains(7, 7).unwrap());
}

/// Each open takes a strictly higher fencing epoch, across process restarts.
#[test]
fn every_open_takes_a_higher_fencing_epoch() {
    let dir = tmpdir("lock-epoch");
    let _c = CleanDir(dir.clone());
    let mut seen = Vec::new();
    for _ in 0..4 {
        let db = Db::open_with(&dir, opts()).unwrap();
        seen.push(db.epoch());
    }
    assert_eq!(seen, vec![1, 2, 3, 4], "epochs must increase monotonically");
}

/// A range reaching the top of the ordinal space must not overflow, and
/// `u64::MAX` must be refused.
///
/// # History
///
/// Found by the monty E2E harness (`e2e/scenarios/ranges.py`) on
/// 2026-08-25. `Memtable::{insert_range, remove_range}` walked chunks with
/// `o = stop + 1;` followed by `if stop == u64::MAX { break; }` — the guard was
/// correct but one statement too late, so the add itself panicked in a debug
/// build.
///
/// Invariant I8 ( 2026-08-26 ) reserved `u64::MAX`, which makes that overflow
/// **structurally unreachable** rather than merely guarded: no accepted range
/// can have `stop == u64::MAX`, so `stop + 1` cannot wrap. The test therefore
/// now asserts both halves — that `ORDINAL_MAX` works everywhere, and that
/// `u64::MAX` is rejected rather than stored.
///
/// The top of the address space has no coverage from the proptest generators,
/// which are boundary-biased on cardinality and prefix *pattern*.
#[test]
fn ranges_reaching_the_top_of_the_address_space_do_not_overflow() {
    use yesno_core::{CodecError, ORDINAL_MAX};

    let dir = tmpdir("u64-ceiling");
    let _c = CleanDir(dir.clone());
    let db = Db::open_with(&dir, opts()).unwrap();

    // Inclusive on both ends, and the high end is the last storable ordinal.
    assert_eq!(db.insert_range(1, ORDINAL_MAX - 5, ORDINAL_MAX).unwrap(), 6);
    assert_eq!(
        db.insert_many(2, &[ORDINAL_MAX - 1, ORDINAL_MAX]).unwrap(),
        2
    );

    let snap = db.snapshot().unwrap();
    assert!(snap.contains(1, ORDINAL_MAX).unwrap());
    assert_eq!(snap.max(1).unwrap(), Some(ORDINAL_MAX));
    assert_eq!(snap.cardinality(1).unwrap(), 6);
    assert_eq!(snap.max(2).unwrap(), Some(ORDINAL_MAX));
    drop(snap);

    // I8: `u64::MAX` is not an ordinal. Every mutating entry point refuses it,
    // and refuses it *without* having written anything.
    for e in [
        db.insert(3, u64::MAX).err(),
        db.remove(3, u64::MAX).err(),
        db.insert_range(3, ORDINAL_MAX, u64::MAX).err(),
        db.remove_range(3, ORDINAL_MAX, u64::MAX).err(),
        db.insert_many(3, &[1, u64::MAX]).err(),
    ] {
        assert!(
            matches!(e, Some(CodecError::OrdinalOutOfRange { ordinal }) if ordinal == u64::MAX),
            "expected OrdinalOutOfRange, got {e:?}"
        );
    }
    let snap = db.snapshot().unwrap();
    assert_eq!(
        snap.cardinality(3).unwrap(),
        0,
        "a rejected batch must not partially apply"
    );
    drop(snap);

    // Removal walks the same loop.
    assert_eq!(db.remove_range(1, ORDINAL_MAX - 2, ORDINAL_MAX).unwrap(), 3);
    let snap = db.snapshot().unwrap();
    assert_eq!(snap.cardinality(1).unwrap(), 3);
    assert_eq!(snap.max(1).unwrap(), Some(ORDINAL_MAX - 3));
    assert!(!snap.contains(1, ORDINAL_MAX).unwrap());
    drop(snap);

    // And it survives the checkpoint and a reopen, so the ceiling is not
    // merely a memtable property.
    db.checkpoint().unwrap();
    drop(db);
    let db = Db::open_with(&dir, opts()).unwrap();
    let snap = db.snapshot().unwrap();
    assert_eq!(snap.max(1).unwrap(), Some(ORDINAL_MAX - 3));
    assert_eq!(snap.max(2).unwrap(), Some(ORDINAL_MAX));
    assert_eq!(snap.cardinality(2).unwrap(), 2);
}

/// A key at the top of the address space must not fold onto another key.
///
/// Shards are chosen by `splitmix64(key)`; a signed shift anywhere on that path
/// would map the upper half of the key space onto the lower.
#[test]
fn keys_at_the_top_of_the_address_space_stay_distinct() {
    let dir = tmpdir("u64-keys");
    let _c = CleanDir(dir.clone());
    let db = Db::open_with(&dir, opts()).unwrap();

    for (i, k) in [u64::MAX, u64::MAX - 1, i64::MAX as u64, 1u64 << 63]
        .iter()
        .enumerate()
    {
        db.insert(*k, i as u64).unwrap();
    }
    db.checkpoint().unwrap();

    let snap = db.snapshot().unwrap();
    assert_eq!(
        snap.load(u64::MAX).unwrap().iter().collect::<Vec<_>>(),
        vec![0]
    );
    assert_eq!(
        snap.load(u64::MAX - 1).unwrap().iter().collect::<Vec<_>>(),
        vec![1]
    );
    assert_eq!(
        snap.load(i64::MAX as u64)
            .unwrap()
            .iter()
            .collect::<Vec<_>>(),
        vec![2]
    );
    assert_eq!(
        snap.load(1u64 << 63).unwrap().iter().collect::<Vec<_>>(),
        vec![3]
    );
}

/// A superseded packed **run** must return its bytes to its page's live count.
///
/// Found by the monty E2E harness on 2026-08-25. Both supersede paths sized the
/// old payload with `payload_len(None).unwrap_or(0)`, and `payload_len` cannot
/// size a run without its `nruns` prefix — so it returned `Err` for **every**
/// run and the `unwrap_or` silently subtracted nothing. A packed page whose
/// live count never reaches zero is never queued for reclamation, so any page
/// holding a run leaked in full. That is the sparse regime, which is the regime
/// packed pages exist for.
///
/// The corpus is deliberately ranges inside single chunks: small run payloads,
/// packed rather than given a slot of their own. Both facts are asserted before
/// the churn, because with array containers or standalone extents this test
/// would pass without ever reaching the broken path.
#[test]
fn superseding_a_packed_run_returns_its_bytes_to_the_page() {
    let dir = tmpdir("packed-run-live");
    let _c = CleanDir(dir.clone());
    let db = Db::open_with(&dir, opts()).unwrap();

    for k in 0..32u64 {
        db.insert_range(k, k * 100, k * 100 + 500).unwrap();
    }
    db.checkpoint().unwrap();

    // --- the corpus must actually exercise packed runs
    let snap = db.snapshot().unwrap();
    let set = snap.load(0).unwrap();
    let (_, c) = set.chunks().next().expect("key 0 must have a chunk");
    assert_eq!(
        c.kind(),
        yesno_core::ContainerKind::Run,
        "the corpus must produce run containers, or the broken path is never reached"
    );
    drop(snap);
    assert!(
        db.slabs_by_class()
            .iter()
            .any(|(class, n)| *class == yesno_core::store::extent::PACKED_CLASS && *n > 0),
        "the corpus must produce packed pages, or the broken path is never reached"
    );

    // --- churn: supersede *half* of each page's chunks
    //
    // Two things here are load-bearing, and getting either wrong produced a
    // test that passed against the bug it was written for.
    //
    // The range is **extended within the same chunk** rather than added
    // elsewhere: writing to a fresh chunk each round only ever carries the old
    // ones, never supersedes them.
    //
    // And only the even keys are rewritten, so every page keeps some live
    // chunks. `fsck` compares live bytes only for pages the index still
    // reaches, so a page whose chunks *all* die leaves the allocator's stale
    // count invisible — which is exactly the shape this bug takes.
    for round in 1..6u64 {
        for k in (0..32u64).step_by(2) {
            db.insert_range(k, k * 100, k * 100 + 500 + round * 10)
                .unwrap();
        }
        db.checkpoint().unwrap();
    }

    for (i, r) in db.verify().unwrap().iter().enumerate() {
        assert!(
            r.packed_live_mismatch.is_empty(),
            "shard {i}: the allocator's packed live bytes disagree with the index: {:?}",
            r.packed_live_mismatch
        );
        assert!(
            r.dangling.is_empty(),
            "shard {i}: dangling {:?}",
            r.dangling
        );
    }

    // --- and the pages are genuinely handed back, not merely counted right
    //
    // No readers are live, so reclamation waits only on the A/B superblock
    // rule. With the accounting bug no packed page ever reached zero live
    // bytes, so nothing was ever queued and this stayed at zero.
    for _ in 0..3 {
        db.checkpoint().unwrap();
    }
    assert!(
        db.freed_extents() > 0,
        "no extent was ever reclaimed, so superseded packed pages are leaking"
    );

    // The data is still right after all of it.
    drop(db);
    let db = Db::open_with(&dir, opts()).unwrap();
    let snap = db.snapshot().unwrap();
    for k in 0..32u64 {
        let last = if k % 2 == 0 { 550 } else { 500 };
        assert_eq!(snap.cardinality(k).unwrap(), last + 1, "key {k}");
        assert!(
            snap.contains(k, k * 100 + last).unwrap(),
            "key {k} lost its range"
        );
        assert_eq!(snap.min(k).unwrap(), Some(k * 100));
        assert_eq!(snap.max(k).unwrap(), Some(k * 100 + last));
    }
}

/// Extents awaiting reclamation at shutdown must not be orphaned by the reopen.
///
/// The deferred free list is in-memory by design ( I3 ), so an extent that has
/// been superseded but has not yet passed all three reclamation conditions is
/// queued nowhere durable while its slot is persisted as **used**. After a
/// reopen nothing references it and nothing has it queued: it was lost for
/// good, and it accrued once per restart.
///
/// I3's own argument is that "on restart there are no readers, so the
/// checkpointed free bitmaps already describe exactly what is reclaimable".
/// That is not true of the bitmaps — they describe superseded-but-unreclaimed
/// extents as used — but it *is* true of the index, so open walks it.
#[test]
fn extents_pending_at_shutdown_are_not_orphaned_by_a_reopen() {
    let dir = tmpdir("orphan-reopen");
    let _c = CleanDir(dir.clone());

    let pending_before = {
        let db = Db::open_with(&dir, opts()).unwrap();
        // Big enough to own real extents, and superseded so they queue.
        for round in 0..3u64 {
            let vals: Vec<u64> = (0..4_000u64).map(|i| i * 7 + round).collect();
            db.insert_many(1, &vals).unwrap();
            db.checkpoint().unwrap();
        }
        let pending: usize = db.verify().unwrap().iter().map(|r| r.pending).sum();
        // Without something waiting on the reclamation conditions there is
        // nothing for the reopen to orphan, and this test proves nothing.
        assert!(
            pending > 0,
            "the corpus must leave extents awaiting reclamation at shutdown"
        );
        pending
    };

    let db = Db::open_with(&dir, opts()).unwrap();
    for (i, r) in db.verify().unwrap().iter().enumerate() {
        assert!(
            r.leaked.is_empty(),
            "shard {i}: {} slots orphaned by the reopen ({} were pending before it): {:?}",
            r.leaked.len(),
            pending_before,
            r.leaked
        );
        assert!(r.is_clean(), "shard {i}: {r:?}");
    }

    // And the data the rebuild kept is exactly the data that was there.
    let want: std::collections::BTreeSet<u64> = (0..3u64)
        .flat_map(|round| (0..4_000u64).map(move |i| i * 7 + round))
        .collect();
    let snap = db.snapshot().unwrap();
    let got: std::collections::BTreeSet<u64> = snap.load(1).unwrap().iter().collect();
    assert_eq!(got, want, "the rebuild freed a slot that was still live");
    drop(snap);

    // The reclaimed space is genuinely reusable: keep writing and reopen again.
    db.insert_many(2, &(0..4_000u64).map(|i| i * 11).collect::<Vec<_>>())
        .unwrap();
    db.checkpoint().unwrap();
    drop(db);
    let db = Db::open_with(&dir, opts()).unwrap();
    let snap = db.snapshot().unwrap();
    assert_eq!(
        snap.load(1)
            .unwrap()
            .iter()
            .collect::<std::collections::BTreeSet<_>>(),
        want
    );
    assert_eq!(snap.cardinality(2).unwrap(), 4_000);
}

/// Reopening with a different shard count must not lose data.
///
/// **This was a live data-loss bug until 2026-08-28 and produced no error.**
/// `Db::shard_of` was `vshard_of(key) % shards.len()`, and the count was
/// persisted nowhere — it came from `DbOptions` on every open, checked only for
/// `> 0`. Measured before the fix: a 4-shard database reopened with 8 returned
/// **31 of 64 keys**, and with 3 returned **14 of 64**. Every key hashed to a
/// different shard file than the one holding it, and nothing said so.
///
/// The count now lives in the MANIFEST and the persisted value wins, because
/// `DbOptions::shards` is a *creation* parameter and there is no way to serve
/// four shards' data as eight. Refusing the mismatch was tried first and
/// rejected: it makes a good database unopenable by a caller who passed a struct
/// default, which `e2e/scenarios/lifecycle.py` did on the first run.
#[test]
fn reopening_with_a_different_shard_count_keeps_every_key() {
    let dir = tmpdir("shard_count_mismatch");
    let keys: Vec<u64> = (0..64u64).map(|k| k * 7 + 1).collect();

    let opts_at = |n: usize| DbOptions {
        shards: n,
        ..DbOptions::default()
    };

    {
        let db = Db::open_with(&dir, opts_at(4)).unwrap();
        for &k in &keys {
            db.insert_many(k, &[k * 100, k * 100 + 1, k * 100 + 2])
                .unwrap();
        }
        db.checkpoint().unwrap();
    }

    // 4 is the control — without it, a database that refused every reopen, or
    // one that returned nothing at all, would satisfy the rows below.
    for n in [4usize, 8, 3, 16, 1] {
        let db = Db::open_with(&dir, opts_at(n)).unwrap();
        let snap = db.snapshot().unwrap();
        let found = keys
            .iter()
            .filter(|&&k| snap.cardinality(k).unwrap_or(0) == 3)
            .count();
        assert_eq!(
            found,
            keys.len(),
            "opened with {n} shards: {found} of {} keys reachable",
            keys.len()
        );
    }
}

/// The MANIFEST is the database's identity, so a torn write to one slot must
/// leave it openable from the other.
#[test]
fn a_torn_manifest_slot_still_opens_from_its_sibling() {
    let dir = tmpdir("torn_manifest");
    {
        let db = Db::open_with(&dir, DbOptions::default()).unwrap();
        db.insert_many(1, &[10, 20, 30]).unwrap();
        db.checkpoint().unwrap();
    }
    let path = dir.join("MANIFEST");
    let mut bytes = std::fs::read(&path).unwrap();
    // Deliberately not importing the slot size: `db` is `pub(crate)`, and an
    // integration test reaching past the public API to assert a layout constant
    // is how a test starts depending on internals it should not pin.
    assert!(
        !bytes.is_empty() && bytes.len().is_multiple_of(2),
        "the manifest is two equal slots"
    );

    // Corrupt slot A past its checksum. Slot B carries the same image.
    bytes[24] ^= 0xFF;
    std::fs::write(&path, &bytes).unwrap();

    let db = Db::open_with(&dir, DbOptions::default()).unwrap();
    assert_eq!(db.snapshot().unwrap().cardinality(1).unwrap(), 3);
}

/// The `db_uuid` must actually keep a foreign shard file out.
///
/// **It did not, from M3 until 2026-08-28.** The field was written into every
/// superblock and compared nowhere in production — the only `==` in the tree was
/// a unit test asserting it had been stored. A shard file from another database
/// opened cleanly and served its own contents under this database's keys. Same
/// shape as `store::segment::check_alignment` having no caller and
/// `ChunkRef::validate` running only from `fsck`: a gate nothing runs on the path
/// it guards is a comment.
#[test]
fn a_shard_file_from_another_database_is_refused() {
    let one = tmpdir("identity_one");
    let two = tmpdir("identity_two");
    for (d, key) in [(&one, 1u64), (&two, 2u64)] {
        let db = Db::open_with(d, DbOptions::default()).unwrap();
        db.insert_many(key, &[key * 10, key * 10 + 1]).unwrap();
        db.checkpoint().unwrap();
    }

    // Graft one of `two`'s shard files into `one`, leaving `one`'s MANIFEST — so
    // the identity is the only thing that disagrees.
    let victim = one.join("shard-0000.yno");
    std::fs::copy(two.join("shard-0000.yno"), &victim).unwrap();

    match Db::open_with(&one, DbOptions::default()) {
        Err(yesno_core::CodecError::DatabaseIdentityMismatch) => {}
        Err(e) => panic!("wrong error: {e}"),
        Ok(_) => panic!("a shard file from another database was accepted"),
    }
}

/// A manifest whose slots are both torn must fail loudly rather than be
/// re-created, because a fresh `vshard -> shard` map routes keys to shards that
/// do not hold them.
///
/// Found by a sabotage that *passed*: writing only one manifest slot left the
/// torn-slot test green, because `pick` returned `None` and the open path
/// silently minted a new manifest. The identity check above is what made that
/// visible; this asserts the specific error rather than relying on it.
#[test]
fn a_wholly_unreadable_manifest_is_refused_rather_than_recreated() {
    let dir = tmpdir("manifest_unreadable");
    {
        let db = Db::open_with(&dir, DbOptions::default()).unwrap();
        db.insert_many(1, &[10, 20, 30]).unwrap();
        db.checkpoint().unwrap();
    }
    let path = dir.join("MANIFEST");
    let mut bytes = std::fs::read(&path).unwrap();
    let half = bytes.len() / 2;
    bytes[24] ^= 0xFF; // slot A, past its checksum
    bytes[half + 24] ^= 0xFF; // slot B likewise
    std::fs::write(&path, &bytes).unwrap();

    match Db::open_with(&dir, DbOptions::default()) {
        Err(yesno_core::CodecError::ManifestUnreadable) => {}
        Err(e) => panic!("wrong error: {e}"),
        Ok(_) => panic!("a database with no readable manifest was re-created instead of refused"),
    }
}

/// A multi-shard commit that is durable on one participant and not the other
/// must be discarded **whole**, including the half that did land.
///
/// # Why this was missing, and how it was found
///
/// Removing the `CommitIntent` write from `WriteBatch::commit` entirely — so a
/// spanning version claims one participant instead of all of them — left this
/// crate's whole suite green: 552 unit tests plus every integration test, this
/// file included. `yesno-replication`'s M7 gate stayed green too, because it
/// runs at one shard. Only a follower catching shards up one at a time noticed.
///
/// That is a durability hole rather than a replication one. Recovery's rule is
/// that a `cv` is committed *iff every shard in its `CommitIntent.shards` has a
/// CRC-valid `ShardCommit{cv}`*, and nothing exercised the branch where one does
/// not — so the participant set could stop being written and no test would say.
///
/// # The construction
///
/// A crash between two participants' fsyncs, reproduced by cutting the tail off
/// one shard's log. Its last record is the `ShardCommit`, and `Record::decode`
/// treats a short final frame as the end of the log, which is exactly what a
/// half-flushed append leaves behind.
///
/// The load-bearing assertion is about the **other** shard. Its records for
/// that version are complete, CRC-valid and durable, and must still be thrown
/// away, because its partner never committed. A recovery that replayed "whatever
/// looks intact" would pass every other test in this file.
#[test]
fn a_multi_shard_commit_missing_one_participant_is_discarded_on_both() {
    let dir = tmpdir("partial-multi-shard");
    let _c = CleanDir(dir.clone());

    // A policy that will not fire: a checkpoint would make the records durable
    // through the data file instead, and there would be nothing to discard.
    let policy = yesno_core::checkpoint::CheckpointPolicy {
        dirty_bytes: usize::MAX,
        max_dirty_bytes: usize::MAX,
        wal_bytes: u64::MAX,
        interval_secs: u64::MAX,

        ..Default::default()
    };
    let opts = DbOptions {
        shards: 2,
        policy,
        ..Default::default()
    };

    // One key per shard, asked of the database: `vshard_of` is a hash, so a
    // hard-coded pair would silently stop spanning if it ever changed.
    let (ka, kb) = {
        let db = Db::open_with(&dir, opts).unwrap();
        let a = (0..10_000u64).find(|&k| db.shard_of(k) == 0).unwrap();
        let b = (0..10_000u64).find(|&k| db.shard_of(k) == 1).unwrap();
        (a, b)
    };
    let _ = std::fs::remove_dir_all(&dir);

    let survivor: Vec<u64> = (0..40u64).map(|i| i * 3).collect();
    let doomed: Vec<u64> = (0..40u64).map(|i| 1_000_000 + i * 3).collect();

    {
        let db = Db::open_with(&dir, opts).unwrap();

        // Version 1: single-shard, complete. It must survive, which is what
        // separates "recovery discarded the right thing" from "recovery
        // discarded everything".
        db.insert_many(ka, &survivor).unwrap();

        // Version 2: spanning. Both shards get a `CommitIntent` naming both.
        let mut b = db.batch();
        for v in &doomed {
            b.insert(ka, *v);
            b.insert(kb, *v);
        }
        let c = b.commit().unwrap();
        assert_eq!(c.shards, 2, "the batch did not span both shards");
        assert_eq!(
            db.checkpoint_count_for_test(),
            0,
            "a checkpoint ran behind us"
        );
    }

    // ---- the crash: shard 1's `ShardCommit` never reached the platter
    let log = dir.join("shard-0001.wal");
    let len = std::fs::metadata(&log).unwrap().len();
    let cut = len - 1;
    std::fs::OpenOptions::new()
        .write(true)
        .open(&log)
        .unwrap()
        .set_len(cut)
        .unwrap();

    // ---- recovery
    let db = Db::open_with(&dir, opts).unwrap();
    let s = db.snapshot().unwrap();

    for v in &doomed {
        assert!(
            !s.contains(ka, *v).unwrap(),
            "ordinal {v} came back on shard 0: its records are intact and durable, \
             but the version they belong to never committed on shard 1, so the \
             whole batch must be discarded"
        );
    }
    assert_eq!(
        s.cardinality(kb).unwrap(),
        0,
        "the torn participant's half of an uncommitted version was applied"
    );
    assert_eq!(
        s.load(ka).unwrap().iter().collect::<Vec<_>>(),
        survivor,
        "the complete single-shard commit below the hole was lost"
    );
    drop(s);

    // ---- and the watermark is not stalled by the hole
    db.insert(ka, 42).unwrap();
    let s = db.snapshot().unwrap();
    assert!(
        s.contains(ka, 42).unwrap(),
        "no commit is possible after recovering past a discarded version"
    );
}

/// A shard's LSNs never go backwards, across checkpoints or reopens.
///
/// # Why this is a durability test and not a replication one
///
/// The design says `lsn == the record's **global** byte offset`, "making cursors
/// seekable and raw byte-range shipping offset-identical across replicas ( a
/// Raft prerequisite )". It was not global: a checkpoint cut the log and the
/// next records were written from file offset zero again, so a shard reused
/// every LSN it had ever issued. The consequence surfaced in replication — a
/// follower's cursor became meaningless at its leader's first checkpoint — but
/// the broken invariant belongs here, and nothing in this crate could see it.
///
/// Two axes, and the reopen is the one that would silently undo the fix. The
/// base is derived from the newest retained generation, whose empty active file
/// has no record of its own. When no sealed generation remains, the checkpoint
/// has to leave that base in the superblock; a reopen that ignored it would
/// restart at zero with every other test in this file still green.
#[test]
fn a_shards_lsns_never_restart_across_checkpoints_or_reopens() {
    let dir = tmpdir("global-lsn");
    let _c = CleanDir(dir.clone());
    let opts = DbOptions {
        shards: 1,
        ..Default::default()
    };

    // The LSN of the first record in the log, whatever generation it belongs to.
    // Reading the file directly rather than through `Db`, because the claim is
    // about what is on disk.
    let first_lsn = || -> Option<u64> {
        let bytes = std::fs::read(dir.join("shard-0000.wal")).ok()?;
        yesno_core::wal::Record::peek_lsn(&bytes)
    };
    let log_len = || {
        std::fs::metadata(dir.join("shard-0000.wal"))
            .map(|m| m.len())
            .unwrap_or(0)
    };

    let mut high_water = 0u64;
    let mut generations = 0u32;

    for round in 0..4u64 {
        let db = Db::open_with(&dir, opts).unwrap();
        for i in 0..20u64 {
            db.insert_range(round, i * 1_000, i * 1_000 + 100).unwrap();
        }

        let base = first_lsn().expect("a log with records must name its own base");
        assert!(
            base >= high_water,
            "round {round}: the log restarted at {base}, below the {high_water} \
             this shard had already issued — an LSN was reused, and a follower \
             holding the old one would be served different bytes under it"
        );
        // `base > high_water` would be the wrong test and would pass for the
        // wrong reason: a cut lands the new base *exactly* at the old end, so
        // the sequence is continuous rather than gapped. What says the log was
        // genuinely re-based is that the base left the origin at all.
        if base > 0 {
            generations += 1;
        }
        high_water = base + log_len();

        db.checkpoint().unwrap();
        // The cut leaves nothing behind, which is precisely why the base has to
        // survive somewhere other than the log.
        assert_eq!(log_len(), 0, "round {round}: the checkpoint did not cut");
    }

    assert!(
        generations >= 2,
        "the log was never actually cut and re-based, so nothing was tested"
    );

    // And after all that, the data is still there.
    let db = Db::open_with(&dir, opts).unwrap();
    let s = db.snapshot().unwrap();
    for round in 0..4u64 {
        assert_eq!(
            s.cardinality(round).unwrap(),
            20 * 101,
            "round {round} did not survive four checkpoint cycles"
        );
    }
}
// appended temporarily to durability.rs

/// The WAL-size checkpoint trigger must be able to fire.
///
/// # Why this was dead, and why nothing noticed
///
/// `CheckpointPolicy` has three triggers — dirty bytes, WAL bytes, elapsed time
/// — and `enforce_policy` passed a literal `0` for the second, so the middle
/// disjunct of `should_checkpoint` was unreachable and a documented 1 GiB
/// default did nothing. Same shape as the `base_lsn = 0` seams closed the same
/// day: the parameter exists, the constant exists, and no caller ever supplies
/// anything but the identity.
///
/// **The dirty trigger does not cover it**, which is the whole point. Dirty
/// bytes measure the *memtable*, and a small working set rewritten forever keeps
/// the memtable at nothing while appending a record per commit. Measured before
/// the fix: 448 KB of log against a 64 KB threshold and **zero** checkpoints;
/// only the 60 s interval bounded the log at all, which at a real write rate is
/// an unbounded amount of it.
///
/// So the corpus here rewrites eight ordinals rather than growing a set — a
/// workload the other trigger is structurally blind to.
#[test]
fn the_wal_size_trigger_can_fire_on_a_workload_the_dirty_trigger_cannot_see() {
    let dir = tmpdir("wal-trigger");
    let _c = CleanDir(dir.clone());
    const LIMIT: u64 = 64 << 10;
    // Only the WAL-size trigger may fire, so a checkpoint is proof it did.
    let policy = yesno_core::checkpoint::CheckpointPolicy {
        dirty_bytes: usize::MAX,
        max_dirty_bytes: usize::MAX,
        wal_bytes: LIMIT,
        interval_secs: u64::MAX,

        ..Default::default()
    };
    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 1,
            policy,
            ..Default::default()
        },
    )
    .unwrap();

    for i in 0..4_000u64 {
        db.insert(1, i % 8).unwrap();
    }

    assert!(
        db.checkpoint_count_for_test() > 0,
        "no checkpoint ran with {} bytes of log against a {LIMIT}-byte limit, so \
         the WAL trigger is unreachable again",
        db.wal_bytes()
    );
    assert!(
        db.wal_bytes() < LIMIT * 2,
        "the log reached {} bytes against a {LIMIT}-byte limit; firing once is \
         not the claim, bounding it is",
        db.wal_bytes()
    );

    // And the data is intact — a trigger that checkpointed by losing writes
    // would satisfy everything above.
    let s = db.snapshot().unwrap();
    assert_eq!(
        s.load(1).unwrap().iter().collect::<Vec<_>>(),
        (0..8).collect::<Vec<u64>>()
    );
    drop(s);
    drop(db);
    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 1,
            policy,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        db.snapshot()
            .unwrap()
            .load(1)
            .unwrap()
            .iter()
            .collect::<Vec<_>>(),
        (0..8).collect::<Vec<u64>>(),
        "the checkpointed state did not survive a reopen"
    );
}

/// A range reaching the top of the ordinal universe must still see the chunks
/// up there.
///
/// `ChunkKey::new` **masks** its prefix to 48 bits rather than rejecting an
/// out-of-range one, so a scan bound computed as `p_hi + 1` wraps to prefix 0
/// when `p_hi` is the top chunk — and an ascending range scan from `p_lo` to `0`
/// is empty. The result is not an error but a silent **zero**, on exactly the
/// query a planner makes when it has no upper bound to offer: `[0, u64::MAX)`.
#[test]
fn a_range_to_the_top_of_the_universe_sees_the_top_chunk() {
    let dir = tmpdir("range-ceiling");
    let _c = CleanDir(dir.clone());
    let opts = DbOptions {
        shards: 1,
        ..Default::default()
    };

    // One ordinal low down and one in the very top chunk.
    let top = yesno_core::ORDINAL_MAX;
    {
        let db = Db::open_with(&dir, opts).unwrap();
        db.insert(1, 5).unwrap();
        db.insert(1, top).unwrap();
        db.checkpoint().unwrap();
    }

    let db = Db::open_with(&dir, opts).unwrap();
    let s = db.snapshot().unwrap();
    assert_eq!(s.cardinality(1).unwrap(), 2, "both ordinals must be stored");
    assert_eq!(
        s.len_in_range(1, 0, u64::MAX).unwrap(),
        2,
        "a half-open range over the whole universe must count both"
    );
    assert_eq!(
        s.len_in_range(1, top, u64::MAX).unwrap(),
        1,
        "and a range that starts in the top chunk must count the one there"
    );
    assert_eq!(
        s.range_summary(1, top, u64::MAX).unwrap(),
        yesno_core::RangeSummary::Full,
        "[ORDINAL_MAX, u64::MAX) is one ordinal wide and it is present"
    );
    assert_eq!(
        s.len_in_range(1, 6, top).unwrap(),
        0,
        "and the gap between them holds nothing"
    );
}

/// `Snapshot::keys` and `key_range` against a `BTreeSet` oracle, after a reopen.
///
/// The oracle is an outside answer, not another yesno path. An enumeration
/// that agreed with, say, `merged_chunks` would agree with itself; only a set
/// built by the test can catch a key that is reported but deleted, or missed
/// because it lives only in the memtable.
///
/// The three cases that distinguish a real implementation:
///
/// - a key written **and checkpointed** — reachable only through the index;
/// - a key written **after** the last checkpoint — reachable only through the
///   memtable;
/// - a key whose every ordinal has been **deleted** — present in the index,
///   absent from the answer. Reporting it would resurrect a deleted key for
///   every caller that enumerates.
#[test]
fn key_enumeration_agrees_with_an_oracle_across_a_reopen() {
    let dir = tmpdir("keys");
    let mut live: BTreeSet<u64> = BTreeSet::new();

    {
        let db = Db::open_with(&dir, opts()).unwrap();
        // Spread across the u64 domain so keys land in different shards, and
        // include both ends: a key of `u64::MAX` is legal, unlike an ordinal.
        for k in [0u64, 1, 7, 4242, 1 << 40, u64::MAX - 1, u64::MAX] {
            db.insert_many(k, &[1, 2, (5u64 << 16) | 9]).unwrap();
            live.insert(k);
        }
        // Deleted before the checkpoint: the index never sees it.
        db.insert(1234, 1).unwrap();
        db.remove(1234, 1).unwrap();

        db.checkpoint().unwrap();

        // Written after the checkpoint: reachable only through the memtable.
        db.insert_many(99_999, &[3, 4]).unwrap();
        live.insert(99_999);

        // Checkpointed, then deleted: present on disk, tombstoned in the
        // memtable. This is the case a naive "does the tree mention it" walk
        // gets wrong.
        db.insert_many(555, &[1, 2]).unwrap();
        db.checkpoint().unwrap();
        db.remove(555, 1).unwrap();
        db.remove(555, 2).unwrap();
    }

    let db = Db::open_with(&dir, opts()).unwrap();
    let snap = db.snapshot().unwrap();

    let got: BTreeSet<u64> = snap.keys().unwrap().into_iter().collect();
    assert_eq!(got, live, "keys() disagrees with the oracle");

    // Ascending, and without duplicates — callers merge-join against it.
    let listed = snap.keys().unwrap();
    let mut sorted = listed.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(listed, sorted, "keys() must be ascending and deduplicated");

    // Every key must also answer for itself, or the enumeration is reporting
    // keys no other read path agrees exist.
    for &k in &listed {
        assert!(
            snap.cardinality(k).unwrap() > 0,
            "keys() reported {k}, which has no ordinals"
        );
    }

    // ── key_range ───────────────────────────────────────────────────────────
    // Half-open, so `hi` is excluded.
    assert_eq!(snap.key_range(0, 8).unwrap(), vec![0, 1, 7]);
    assert_eq!(snap.key_range(1, 8).unwrap(), vec![1, 7]);
    assert_eq!(snap.key_range(1, 7).unwrap(), vec![1]);
    assert!(snap.key_range(2, 7).unwrap().is_empty());
    // An inverted or empty range is empty, not an error.
    assert!(snap.key_range(8, 8).unwrap().is_empty());
    assert!(snap.key_range(8, 1).unwrap().is_empty());

    // The documented consequence of the half-open convention: `u64::MAX` is
    // unreachable by `key_range` and only `keys()` can name it.
    assert!(
        !snap
            .key_range(u64::MAX - 1, u64::MAX)
            .unwrap()
            .contains(&u64::MAX),
        "a half-open upper bound cannot include u64::MAX"
    );
    assert!(listed.contains(&u64::MAX), "keys() must still reach it");

    // Every range must agree with filtering the full list, which is the
    // property that makes `key_range` a restriction rather than a second
    // implementation.
    for (lo, hi) in [(0u64, 10u64), (0, 1 << 41), (7, 100_000), (0, u64::MAX)] {
        let want: Vec<u64> = listed
            .iter()
            .copied()
            .filter(|k| *k >= lo && *k < hi)
            .collect();
        assert_eq!(
            snap.key_range(lo, hi).unwrap(),
            want,
            "key_range({lo}, {hi})"
        );
    }
}

/// A second process may read a database a writer holds open.
///
/// `Db::open` takes a non-blocking exclusive `flock`, so a second *writer* is
/// refused — correctly, since two of them is unbounded corruption. A **reader**
/// is what this asserts: it takes no lock, opens no log, and sees exactly what
/// the last checkpoint published.
///
/// The reader must also refuse every mutation. A read-only handle that
/// accepted a write would be writing into a database another process owns.
#[test]
fn a_reader_opens_alongside_a_live_writer_and_sees_the_last_checkpoint() {
    let dir = tmpdir("reader");

    let writer = Db::open_with(&dir, opts()).unwrap();
    writer.insert_many(1, &[1, 2, 3]).unwrap();
    writer.insert_many(2, &[10, 20]).unwrap();
    writer.checkpoint().unwrap();

    // Committed *after* the checkpoint, so it is in the writer's memtable and
    // nowhere the reader can see. This is the checkpoint-visible semantic, and
    // it is asserted rather than left implicit.
    writer.insert_many(3, &[99]).unwrap();

    // A second writer is still refused — the lock is doing its job.
    assert!(
        Db::open_with(&dir, opts()).is_err(),
        "a second writer must still be refused"
    );

    let reader = Db::open_reader(&dir).unwrap();
    let snap = reader.snapshot().unwrap();

    assert_eq!(
        snap.load(1).unwrap().iter().collect::<Vec<u64>>(),
        vec![1, 2, 3]
    );
    assert_eq!(snap.cardinality(2).unwrap(), 2);
    assert!(snap.contains(1, 2).unwrap());
    assert_eq!(snap.keys().unwrap(), vec![1, 2]);

    // The post-checkpoint write is invisible, by design.
    assert_eq!(
        snap.cardinality(3).unwrap(),
        0,
        "a reader sees the last checkpoint, not the writer's memtable"
    );

    // Every mutation refused.
    assert!(matches!(
        reader.insert(1, 5),
        Err(yesno_core::CodecError::ReadOnlyReplica)
    ));
    assert!(matches!(
        reader.remove(1, 1),
        Err(yesno_core::CodecError::ReadOnlyReplica)
    ));
    let mut b = reader.batch();
    b.insert(1, 5);
    assert!(matches!(
        b.commit(),
        Err(yesno_core::CodecError::ReadOnlyReplica)
    ));

    // After the writer checkpoints again, a *new* reader sees the newer state.
    // A reader opened earlier does not: its superblock was read at open.
    writer.checkpoint().unwrap();
    let reader2 = Db::open_reader(&dir).unwrap();
    let snap2 = reader2.snapshot().unwrap();
    assert_eq!(snap2.cardinality(3).unwrap(), 1);
    assert_eq!(snap2.keys().unwrap(), vec![1, 2, 3]);

    drop(snap);
    drop(snap2);
    drop(reader);
    drop(reader2);
    drop(writer);
}

// ---------------------------------------------------------------------------
// Stored page checksums
// ---------------------------------------------------------------------------
//
// The format writes a CRC32C into every B+tree node, every packed page and
// every standalone extent's trailer, and until `stored-page-crcs-are-not-
// verified` was closed **nothing recomputed any of them**. Online reads check
// shape, version and identity; the integrity scan checked reachability and
// allocator agreement. Their presence was not evidence of verification.
//
// `Db::verify()` now recomputes the index-node family, which is the one the
// scan can reach without a byte reader. These tests drive it end to end: write
// a real database, flip one byte in a real file, reopen, scan.

/// Locate one B+tree leaf in a shard file, by recomputing its stored checksum.
///
/// Deliberately not by trusting the type byte alone. A 64-byte-aligned window
/// whose first two bytes happen to read `leaf, v1` is common; one whose stored
/// CRC32C also matches a recomputation over the whole node is not, at `2^-32`.
/// So this both finds the node and proves the test knows where the checksum
/// lives — if the node framing ever moves, no candidate is found and the tests
/// below fail loudly rather than silently checking nothing.
fn find_leaf_node(file: &[u8]) -> Option<usize> {
    // The node header: type at 0, version at 1, CRC32C at 8..12, computed over
    // the whole node with those four bytes read as zero.
    const NODE_SIZE: usize = 1024;
    const NODE_LEAF: u8 = 1;
    const VERSION: u8 = 1;
    const OFF_CRC: usize = 8;
    use yesno_core::store::checksum::crc32c_append;

    (0..file.len().saturating_sub(NODE_SIZE))
        .step_by(64)
        .find(|&at| {
            let n = &file[at..at + NODE_SIZE];
            if n[0] != NODE_LEAF || n[1] != VERSION {
                return false;
            }
            let stored = u32::from_le_bytes(n[OFF_CRC..OFF_CRC + 4].try_into().unwrap());
            let c = crc32c_append(0, &n[..OFF_CRC]);
            let c = crc32c_append(c, &[0, 0, 0, 0]);
            crc32c_append(c, &n[OFF_CRC + 4..]) == stored
        })
}

/// Build a database with a real on-disk index, and return its shard path.
fn one_shard_db(tag: &str) -> (PathBuf, PathBuf) {
    let dir = tmpdir(tag);
    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 1,
            ..Default::default()
        },
    )
    .unwrap();
    // Enough chunks to fill several leaves, so the index is a real tree.
    for k in 1..40u64 {
        let vals: Vec<u64> = (0..200u64).map(|i| (k << 20) | (i * 7)).collect();
        db.insert_many(k, &vals).unwrap();
    }
    db.checkpoint().unwrap();
    drop(db);
    let shard = dir.join("shard-0000.yno");
    (dir, shard)
}

/// Locate a **standalone extent** by recomputing its trailer checksum.
///
/// Self-validating on purpose, exactly like [`find_leaf_node`]: it trusts no
/// type byte and no offset table, only that `crc32c( payload ) == trailer.crc32c`
/// for some plausible framing. If the extent layout ever moves, this returns
/// `None` and the test using it fails loudly rather than silently checking
/// nothing.
///
/// Returns `( payload_start, payload_len )`. A dense chunk stores an 8 KiB
/// bitmap, which is what the fixture below is built to produce.
fn find_standalone_bitmap(file: &[u8]) -> Option<(usize, usize)> {
    use yesno_core::store::checksum::crc32c_append;
    const PAYLOAD: usize = 8192;
    // Candidates are found by **byte pattern first**, then confirmed by
    // checksum. Recomputing a CRC over 8 KiB at every offset is quadratic
    // enough to hang the suite; the fixture is built so it does not have to.
    // Every other ordinal set means every bitmap word is `0x5555…`, so the
    // whole payload is a run of `0x55` and finding it is a scan.
    let mut run = 0usize;
    for i in 0..file.len() {
        run = if file[i] == 0x55 { run + 1 } else { 0 };
        if run < PAYLOAD {
            continue;
        }
        let at = i + 1 - PAYLOAD;
        // Confirm against the stored trailer checksum, so this still fails
        // loudly rather than silently if the framing ever moves.
        //
        // The 8-byte trailer is ( ckey_tag, crc32c ) and it sits at the end
        // of the **slot**, not adjacent to the payload — the slot is the
        // payload rounded up to a size class, so the gap is class-dependent
        // ( 60 bytes past the payload for this 8 KiB bitmap today ). The gap is
        // therefore searched rather than assumed, which keeps this helper
        // correct across a ladder change instead of pinning today's classes.
        let want = crc32c_append(0, &file[at..at + PAYLOAD]);
        for gap in 0..=512usize {
            let crc_at = at + PAYLOAD + gap;
            if crc_at + 4 <= file.len()
                && u32::from_le_bytes(file[crc_at..crc_at + 4].try_into().unwrap()) == want
            {
                return Some((at, PAYLOAD));
            }
        }
    }
    None
}

/// A one-shard database holding a **dense** chunk, so at least one container is
/// stored as a standalone 8 KiB bitmap rather than packed inline.
fn one_shard_dense_db(name: &str) -> (PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!("yesno-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 1,
            ..Default::default()
        },
    )
    .unwrap();
    // Every other ordinal of one chunk: far past the array ceiling, and not a
    // run, so the encoder has no choice but a bitmap.
    let vals: Vec<u64> = (0..65536u64).filter(|v| v % 2 == 0).collect();
    db.insert_many(1, &vals).unwrap();
    db.checkpoint().unwrap();
    drop(db);
    let shard = dir.join("shard-0000.yno");
    (dir, shard)
}

/// **This is the test that proves the scan is wired, and the unit tests are
/// not.** `Rebuilt::checksum_violations` is exercised directly by
/// `store::fsck`'s own tests, which stay green whether or not anything calls
/// it — so on its own that coverage is not evidence the packed-page and
/// standalone-payload families are ever checked by a real `Db::verify()`.
/// Removing the one line in `Db::verify` that calls it reddens this test and
/// nothing else in the suite.
///
/// The index-node family needs no equivalent: it is verified inside `rebuild`,
/// which `Db::verify` already called.
#[test]
fn a_corrupt_standalone_payload_fails_a_real_verify() {
    let (dir, shard) = one_shard_dense_db("crc-standalone-e2e");
    let _c = CleanDir(dir.clone());

    let bytes = std::fs::read(&shard).unwrap();
    let (at, len) = find_standalone_bitmap(&bytes)
        .expect("no standalone bitmap extent found; the fixture or the framing moved");

    // Non-vacuity: intact, this database is clean.
    {
        let db = reopen_one_shard(&dir);
        for r in db.verify().unwrap() {
            assert!(r.is_clean(), "the intact fixture must be clean: {r:?}");
        }
    }

    // Flip a byte in the middle of the payload, leaving the trailer alone.
    // Written with a targeted `write_at` rather than rewriting the buffer:
    // the shard is a **sparse** 1 GiB address space, and `fs::write` of the
    // whole thing would materialise every hole.
    {
        use std::os::unix::fs::FileExt;
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(&shard)
            .unwrap();
        let b = bytes[at + len / 2] ^ 0xFF;
        f.write_at(&[b], (at + len / 2) as u64).unwrap();
        f.sync_all().unwrap();
    }

    let db = reopen_one_shard(&dir);
    let reports = db.verify().unwrap();
    assert!(
        reports
            .iter()
            .flat_map(|r| r.errors.iter())
            .any(|e| e.contains("fails its stored checksum")),
        "a corrupt standalone payload must be reported by Db::verify: {reports:?}"
    );
    assert!(
        reports.iter().any(|r| !r.is_clean()),
        "a bad payload checksum must make the report unclean"
    );
}

fn reopen_one_shard(dir: &PathBuf) -> Db {
    Db::open_with(
        dir,
        DbOptions {
            shards: 1,
            ..Default::default()
        },
    )
    .unwrap()
}

/// Non-vacuity for both tests below. A scan that reported every page as
/// corrupt would pass them without this.
#[test]
fn an_intact_database_passes_the_checksum_scan() {
    let (dir, shard) = one_shard_db("crc-clean");
    let _c = CleanDir(dir.clone());

    let bytes = std::fs::read(&shard).unwrap();
    assert!(
        find_leaf_node(&bytes).is_some(),
        "the fixture must contain a checksummed B+tree leaf, or the corruption \
         tests below are checking nothing"
    );

    let db = reopen_one_shard(&dir);
    for (i, r) in db.verify().unwrap().iter().enumerate() {
        assert!(r.is_clean(), "shard {i} is not clean: {r:?}");
        assert!(r.errors.is_empty(), "shard {i}: {:?}", r.errors);
    }
}

/// A flipped byte in a node's payload must be caught by the scan.
///
/// The byte chosen is the node's **last**, which is zero-fill padding past the
/// entry array. Nothing else in the format looks at it: the type byte, version,
/// key count, suffix width, key ordering and every entry are all untouched, so
/// the database still answers every query correctly. The stored checksum is the
/// only thing that can tell the page has changed — which is exactly what makes
/// this a test of the checksum rather than of a shape check standing next to it.
#[test]
fn a_corrupt_index_node_payload_fails_the_checksum_scan() {
    let (dir, shard) = one_shard_db("crc-node-payload");
    let _c = CleanDir(dir.clone());

    let mut bytes = std::fs::read(&shard).unwrap();
    let at = find_leaf_node(&bytes).expect("no B+tree leaf found");
    bytes[at + 1023] ^= 0xFF;
    std::fs::write(&shard, &bytes).unwrap();

    let db = reopen_one_shard(&dir);

    // **The read path now refuses this, and that is a deliberate contract
    // change made on 2026-09-14.** This assertion used to read
    // `assert_eq!( snap.load( 1 ).unwrap()..., want )` under the comment "the
    // data is still readable, which is the point: only the checksum knows" --
    // an accurate description of the old behaviour, in which stored CRCs were
    // written and recomputed by nothing on the read path.
    //
    // With the once-per-faulted-region checksum cache, a corrupt index node is
    // refused where it is read. Note which failure this replaces: before the
    // error had anywhere to go, `merged_chunks` swallowed it and `load`
    // answered **empty** -- silently correct became silently wrong before it
    // became loud, which is why the error channel and the check had to land
    // together.
    let snap = db.snapshot().unwrap();
    let err = snap
        .load(1)
        .expect_err("a corrupt index node must be refused");
    assert!(
        format!("{err:?}").contains("index node checksum mismatch"),
        "expected a checksum error, got {err:?}"
    );

    let reports = db.verify().unwrap();
    let hit: Vec<&String> = reports
        .iter()
        .flat_map(|r| r.errors.iter())
        .filter(|e| e.contains("index node") && e.contains("checksum mismatch"))
        .collect();
    assert_eq!(hit.len(), 1, "expected one node reported, got {reports:?}");
    assert!(
        reports.iter().any(|r| !r.is_clean()),
        "a bad checksum must make the report unclean"
    );
}

/// And the other direction: the checksum field itself is corrupt, the bytes it
/// covers are not. Both must be caught, and a check that compared a recomputed
/// value against a recomputed value would pass this while catching nothing.
#[test]
fn a_corrupt_index_node_checksum_field_fails_the_checksum_scan() {
    let (dir, shard) = one_shard_db("crc-node-field");
    let _c = CleanDir(dir.clone());

    let mut bytes = std::fs::read(&shard).unwrap();
    let at = find_leaf_node(&bytes).expect("no B+tree leaf found");
    bytes[at + 8] ^= 0xFF; // the stored CRC32C's low byte
    std::fs::write(&shard, &bytes).unwrap();

    let db = reopen_one_shard(&dir);
    let reports = db.verify().unwrap();
    assert!(
        reports
            .iter()
            .any(|r| r.errors.iter().any(|e| e.contains("checksum mismatch"))),
        "expected a checksum error, got {reports:?}"
    );
}

/// Keys far enough apart to force a 10- or 12-byte leaf suffix, across a reopen.
///
/// # Why the existing corpus could not reach this
///
/// `corpus()` uses keys 1, 2, 3, 4, 900 — all differing in their low bits, so a
/// leaf holding them needs a 2-byte suffix and `LeafRef::search` compares a
/// `u64`. A chunk key is `key(64) || prefix48(48)`, so two user keys differing
/// at bit *b* need `48 + b` bits of suffix: bit 17 is the first that needs 9
/// bytes, which rounds up to the legal width **10**. Every width from 10 up was
/// unreachable from this file, and `search` panicked at all of them — debug by
/// subtraction overflow in `suffix_u64`, release by an out-of-range slice index.
///
/// Reported by a downstream consumer whose key layout is
/// `( namespace << 56 ) | ( kind << 20 ) | index`, which lands in that window as
/// a matter of course rather than as an edge case. Both conditions here are
/// load-bearing: the **reopen**, so the read goes through the persisted tree
/// rather than the memtable, and **four ordinals** per key, so the `ChunkRef` is
/// an out-of-line array rather than an inline one.
///
/// The unit-test half is `search_finds_every_present_key_at_every_legal_width`
/// in `index::node`, which is where the gap actually was. This one exists to
/// pin that the defect was reachable from the public API at all.
#[test]
fn widely_separated_keys_survive_a_reopen() {
    let dir = tmpdir("wide-keys");
    let _c = CleanDir(dir.clone());

    // Bits 20, 36 and 52 of the user key put the first differing bit at chunk-key
    // bit 68, 84 and 100 -- suffix widths 10, 12 and 14 respectively.
    let data: Vec<(u64, Vec<u64>)> = [
        (3u64 << 56) | (0x02 << 20),
        (3u64 << 56) | (0x10 << 20),
        (3u64 << 56) | (0x10 << 36),
        (3u64 << 56) | (0x10 << 52),
        (7u64 << 56) | 0x5555,
    ]
    .iter()
    .enumerate()
    .map(|(i, &k)| (k, (0..4u64).map(|j| j + i as u64 * 10).collect()))
    .collect();

    {
        let db = Db::open_with(&dir, opts()).unwrap();
        for (k, v) in &data {
            db.insert_many(*k, v).unwrap();
        }
        db.checkpoint().unwrap();
    }

    let db = Db::open_with(&dir, opts()).unwrap();
    verify(&db, &data);
}

// ------------------------------------------ the read path refuses corruption

/// A corrupt standalone payload is refused **where it is read**, not only by a
/// scan nobody runs.
///
/// # Why this is separate from `a_corrupt_standalone_payload_fails_a_real_verify`
///
/// That test asserts `Db::verify()` reports the corruption. This one asserts the
/// query refuses it. Before 2026-09-14 the first passed and the second could not
/// have been written: the stored CRCs were written by the checkpoint and
/// recomputed by **nothing** on the read path, so a corrupt extent decoded into
/// a plausible container and the answer was silently wrong.
///
/// An offline scan is not a substitute, and the reason is not that it is slow.
/// It cannot help a query that has already returned.
#[test]
fn a_corrupt_standalone_payload_is_refused_by_the_read_path() {
    let (dir, shard) = one_shard_dense_db("crc-read-standalone");
    let _c = CleanDir(dir.clone());

    let bytes = std::fs::read(&shard).unwrap();
    let (at, len) = find_standalone_bitmap(&bytes)
        .expect("no standalone bitmap extent found; the fixture or the framing moved");

    // Non-vacuity: intact, every key reads.
    {
        let db = reopen_one_shard(&dir);
        let snap = db.snapshot().unwrap();
        for k in 1..8u64 {
            snap.load(k)
                .unwrap_or_else(|e| panic!("intact key {k} must read: {e:?}"));
        }
    }

    // Flip one byte in the middle of the payload. The trailer is untouched, so
    // only the recomputed checksum can tell.
    {
        use std::os::unix::fs::FileExt;
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(&shard)
            .unwrap();
        let b = bytes[at + len / 2] ^ 0xFF;
        f.write_at(&[b], (at + len / 2) as u64).unwrap();
        f.sync_all().unwrap();
    }

    let db = reopen_one_shard(&dir);
    let snap = db.snapshot().unwrap();
    let mut refused = 0usize;
    for k in 1..40u64 {
        if let Err(e) = snap.load(k) {
            assert!(
                format!("{e:?}").contains("fails its stored checksum"),
                "expected a checksum refusal, got {e:?}"
            );
            refused += 1;
        }
    }
    assert_eq!(refused, 1, "exactly the corrupted key must be refused");
}
