//! The zero-copy / MVCC soundness boundary.
//!
//! This is a distinct failure class from every other test file, which is why it
//! is its own layer rather than a case appended to one of them.
//!
//! Containers returned by a read **alias the mmap directly**. That makes the
//! interaction between two otherwise-independent mechanisms load-bearing:
//!
//!   - the allocator may reclaim and reuse an extent once its three conditions
//!     are met ( see `ARCHITECTURE.md` § MVCC );
//!   - a reader may hold a `Container` — and therefore an Arrow `Buffer` into
//!     that extent — for arbitrarily long, including across checkpoints, and
//!     `Buffer` is `'static` so it can outlive the `Snapshot` entirely.
//!
//! If reclamation ever runs ahead of a live reader, the reader does not observe
//! a torn value. It reads **memory that has been rewritten underneath it**,
//! which is undefined behaviour rather than a wrong answer. `ARCHITECTURE.md`
//! names this the project's #1 correctness risk.
//!
//! # What the other layers cannot catch
//!
//! - `crash_matrix` restarts the process; the hazard here needs a *live* reader.
//! - `allocation` counts allocations; this is about bytes that were never
//!   allocated on the heap at all.
//! - the unit tests in `store::alloc` check the three reclamation conditions in
//!   isolation, against a hand-driven allocator — they cannot show that the real
//!   read path actually holds the guard that makes condition 3 true.
//! - `db::tests::a_snapshot_is_isolated_across_a_checkpoint` looks like this
//!   test but is not: it uses a 3-element set, which lives **inline in the index
//!   leaf** and never touches an extent, and it asks for `cardinality`, which is
//!   answered from the index without reading a payload.
//!
//! # Two traps these tests have to avoid, and did not at first
//!
//! **The set must be big enough to be a real extent.** Three ordinals or fewer
//! live inline in the index leaf and never touch the mapping at all.
//!
//! **The read must actually reach the disk.** `checkpoint()` does *not* clear
//! the memtable — `prune()` is a separate call the checkpointer never makes — so
//! a `load()` on the same `Db` instance is answered from memory and never
//! touches the mapping. Every test below therefore **reopens** the database
//! first, so the memtable is empty and `ShardStore::read_container` takes the
//! `decode_buffer` path that aliases the mmap. The first draft of this file
//! missed that and tested nothing.
//!
//! # What these establish, and what they do not
//!
//! Rigorously: **the mapping stays alive while a container points into it**,
//! including after the `Db`, the `Snapshot`, and the `ShardStore` are all gone.
//! Under Valgrind an access to an unmapped page is reported directly, which is
//! what makes that a soundness result rather than a hopeful one.
//!
//! Only probabilistically: **that a freed slot is reused while still held**. The
//! churn loops make reuse likely, not certain, and nothing here can observe
//! which slot the allocator picked. Treat a failure as real and a pass as
//! evidence rather than proof.
//!
//! Valgrind is the tool that turns these from correctness tests into soundness
//! tests — see `QUALITY_GATE.md` § Valgrind. Under plain `cargo test` a rewritten
//! page usually still returns plausible bytes, so an assertion on contents is
//! what fails; under Valgrind an access to a genuinely unmapped page is reported
//! directly.

use std::path::PathBuf;

use yesno_core::{Db, DbOptions, OrdSet};

struct CleanDir(PathBuf);
impl Drop for CleanDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tmpdir(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("yesno-zcmvcc-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    p
}

/// Big enough that the chunk is a real extent rather than an inline reference.
///
/// Inline holds 3 values; anything above that allocates. 5000 scattered values
/// in one chunk is comfortably a bitmap, so the read returns a container
/// pointing straight into the mapping.
fn wide(base: u64, n: u64) -> Vec<u64> {
    (0..n).map(|i| base + i * 3).collect()
}

/// A held container must keep its contents while the extent behind it is freed,
/// reclaimed, and handed to another chunk.
///
/// Weaker than its name suggests, and kept for the reopen path rather than
/// for the guard. Because it reopens, the chunk under test lands in **slab 0**,
/// which has no metadata region ( it is the superblock ), stays `Opaque` after a
/// reopen, and is therefore never reclaimable — so its extent is never actually
/// freed and reclamation is never exercised. That is why removing condition 3
/// left this passing for three iterations.
/// `a_pinned_extent_is_not_handed_out_while_its_reader_lives` is the one that
/// reaches the guard.
///
/// The churn loop is not decoration: `RECLAIM_CKPT_DELAY` is 2, so an extent
/// freed by checkpoint N cannot be reused before N+2 is durable. A test that
/// checkpointed once would pass whether or not the guard worked.
#[test]
fn a_held_container_survives_reclamation_of_its_extent() {
    let dir = tmpdir("reclaim");
    let _c = CleanDir(dir.clone());
    let original = wide(0, 5_000);
    {
        let db = Db::open_with(
            &dir,
            DbOptions {
                shards: 1,
                ..Default::default()
            },
        )
        .unwrap();
        db.insert_many(1, &original).unwrap();
        db.checkpoint().unwrap();
    }
    // Reopen: an empty memtable is the only way to force the read to disk.
    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 1,
            ..Default::default()
        },
    )
    .unwrap();

    // Materialize through the real read path, then drop the snapshot. The
    // container must now be keeping the mapping alive entirely on its own.
    let held: OrdSet = {
        let snap = db.snapshot().unwrap();
        snap.load(1).unwrap()
    };
    assert_eq!(held.len(), original.len() as u64);

    // Free the extent, then churn hard enough that its slot is genuinely
    // reusable and very likely reused.
    let mut b = db.batch();
    b.delete_key(1);
    b.commit().unwrap();
    for round in 0..6u64 {
        let filler = wide(1_000_000 + round * 500_000, 5_000);
        db.insert_many(100 + round, &filler).unwrap();
        db.checkpoint().unwrap();
    }

    assert_eq!(
        held.iter().collect::<Vec<_>>(),
        original,
        "a container held across reclamation of its own extent changed contents"
    );
}

/// The same property when the reader outlives the `Db` itself.
///
/// `Buffer` is `'static`, so a container can escape into a structure that
/// outlives everything that produced it — a `RecordBatch` handed to a query
/// engine is the real-world shape. Nothing may unmap while it lives.
#[test]
fn a_container_outlives_the_database_that_produced_it() {
    let dir = tmpdir("outlive-db");
    let _c = CleanDir(dir.clone());
    let original = wide(7, 4_000);

    {
        let db = Db::open_with(
            &dir,
            DbOptions {
                shards: 1,
                ..Default::default()
            },
        )
        .unwrap();
        db.insert_many(42, &original).unwrap();
        db.checkpoint().unwrap();
    }

    let held = {
        let db = Db::open_with(
            &dir,
            DbOptions {
                shards: 1,
                ..Default::default()
            },
        )
        .unwrap();
        let snap = db.snapshot().unwrap();
        let s = snap.load(42).unwrap();
        // Everything that produced the container is now gone. Only the Buffer's
        // own Arc keeps the mapping alive; if it did not, the read below would
        // touch unmapped memory.
        drop(snap);
        drop(db);
        s
    };

    assert_eq!(held.iter().collect::<Vec<_>>(), original);
}

/// Reopening the database must not disturb a container held from the previous
/// instance — the second `Db` maps the same file independently.
#[test]
fn reopening_does_not_disturb_a_container_from_the_previous_instance() {
    let dir = tmpdir("reopen");
    let _c = CleanDir(dir.clone());
    let original = wide(11, 4_000);

    {
        let db = Db::open_with(
            &dir,
            DbOptions {
                shards: 1,
                ..Default::default()
            },
        )
        .unwrap();
        db.insert_many(9, &original).unwrap();
        db.checkpoint().unwrap();
    }
    let held = {
        let db = Db::open_with(
            &dir,
            DbOptions {
                shards: 1,
                ..Default::default()
            },
        )
        .unwrap();
        let snap = db.snapshot().unwrap();
        snap.load(9).unwrap()
    };

    let db2 = Db::open_with(
        &dir,
        DbOptions {
            shards: 1,
            ..Default::default()
        },
    )
    .unwrap();
    let mut b = db2.batch();
    b.delete_key(9);
    b.commit().unwrap();
    for round in 0..4u64 {
        db2.insert_many(200 + round, &wide(5_000_000 + round * 400_000, 4_000))
            .unwrap();
        db2.checkpoint().unwrap();
    }

    assert_eq!(
        held.iter().collect::<Vec<_>>(),
        original,
        "reopening and churning disturbed a container held from the first instance"
    );
}

/// Many readers holding many extents across sustained churn.
///
/// A single held container can survive by luck — its slot simply may not be the
/// one reused. Holding a spread of them makes accidental survival much less
/// likely, which is what gives the assertion teeth.
#[test]
fn many_held_containers_survive_sustained_churn() {
    let dir = tmpdir("many");
    let _c = CleanDir(dir.clone());
    let mut expected = Vec::new();
    {
        let db = Db::open_with(
            &dir,
            DbOptions {
                shards: 2,
                ..Default::default()
            },
        )
        .unwrap();
        for k in 0..12u64 {
            let vals = wide(k * 100_000, 3_000);
            db.insert_many(k, &vals).unwrap();
            expected.push((k, vals));
        }
        db.checkpoint().unwrap();
    }
    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 2,
            ..Default::default()
        },
    )
    .unwrap();

    let snap = db.snapshot().unwrap();
    let held: Vec<(u64, OrdSet)> = expected
        .iter()
        .map(|(k, _)| (*k, snap.load(*k).unwrap()))
        .collect();
    drop(snap);

    let mut b = db.batch();
    for (k, _) in &expected {
        b.delete_key(*k);
    }
    b.commit().unwrap();
    for round in 0..8u64 {
        db.insert_many(9_000 + round, &wide(20_000_000 + round * 300_000, 3_000))
            .unwrap();
        db.checkpoint().unwrap();
    }

    for ((k, set), (ek, evals)) in held.iter().zip(expected.iter()) {
        assert_eq!(k, ek);
        assert_eq!(
            set.iter().collect::<Vec<_>>(),
            *evals,
            "held container for key {k} changed under churn"
        );
    }
}

/// Reclamation must actually happen, or every test above passes vacuously.
///
/// This is the control for the whole file. A held container surviving churn
/// proves nothing if nothing was ever reclaimed — which was exactly the state
/// before deletes worked: no extent was ever superseded, so the queue was always
/// empty and the guard was never exercised.
#[test]
fn superseded_extents_are_queued_and_then_actually_freed() {
    let dir = tmpdir("reclaim-works");
    let _c = CleanDir(dir.clone());
    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 1,
            ..Default::default()
        },
    )
    .unwrap();

    db.insert_many(1, &wide(0, 5_000)).unwrap();
    db.checkpoint().unwrap();
    assert_eq!(db.deferred_extents(), 0, "nothing superseded yet");

    // Rewriting the key supersedes its extent.
    db.insert_many(1, &wide(1, 5_000)).unwrap();
    db.checkpoint().unwrap();
    assert!(
        db.deferred_extents() > 0,
        "a rewritten key must queue its old extent"
    );

    // Churn until the watermark and the checkpoint counter both advance past
    // the delay, at which point the queue must drain.
    for round in 0..8u64 {
        db.insert_many(50 + round, &wide(3_000_000 + round * 200_000, 500))
            .unwrap();
        db.checkpoint().unwrap();
    }
    // Not `deferred_extents() == 0`: the carry-forward rewrites every chunk on
    // every checkpoint, so each one supersedes the whole dataset and the queue
    // is steady-state non-empty. Only the cumulative count separates a working
    // reclaimer from one that queues and never drains.
    assert!(
        db.freed_extents() > 0,
        "with no readers, queued extents must actually be freed"
    );
}

/// Condition 3, end to end: a live reader blocks reclamation of its own extent.
///
/// **This test does not currently fail if condition 3 is removed**, and that
/// is a statement about the system, not the test. `free_now` only clears a bit
/// in the allocator's occupancy map; `begin_generation` clears the active-slab
/// table every checkpoint, so bump allocation always opens a *fresh* slab and a
/// freed slot is never handed out again. Nothing reuses reclaimed space, so
/// nothing can yet be reused *wrongly*.
///
/// The check is implemented and unit-tested where it is load-bearing today
/// ( `store::segment::tests`, which do fail if `any_pinned_in` is broken ). It
/// becomes load-bearing here the moment the compactor makes freed slots
/// allocatable — which is exactly when getting it wrong would corrupt data, so
/// it is deliberately built first rather than alongside.
///
/// The version watermark cannot see this case — the reader may have dropped its
/// `Snapshot` entirely — so if the pin check were removed, the slot would be
/// handed out while the `Buffer` still pointed at it.
#[test]
fn a_live_reader_blocks_reclamation_of_the_extent_it_holds() {
    let dir = tmpdir("reclaim-blocked");
    let _c = CleanDir(dir.clone());
    let original = wide(0, 5_000);

    {
        let db = Db::open_with(
            &dir,
            DbOptions {
                shards: 1,
                ..Default::default()
            },
        )
        .unwrap();
        db.insert_many(1, &original).unwrap();
        db.checkpoint().unwrap();
    }

    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 1,
            ..Default::default()
        },
    )
    .unwrap();
    let held = {
        let snap = db.snapshot().unwrap();
        snap.load(1).unwrap()
    };
    assert!(
        db.pinned_extents() > 0,
        "a materialized container must pin the slot it aliases"
    );

    // Supersede it, then churn well past the reclamation delay.
    db.insert_many(1, &wide(1, 5_000)).unwrap();
    for round in 0..8u64 {
        db.insert_many(50 + round, &wide(4_000_000 + round * 200_000, 500))
            .unwrap();
        db.checkpoint().unwrap();
    }

    // The held slot must still be pinned, and its contents intact, even though
    // everything around it has been reclaimed several times over.
    assert!(
        db.pinned_extents() > 0,
        "the held extent must still be pinned while a Buffer points into it"
    );
    assert_eq!(
        held.iter().collect::<Vec<_>>(),
        original,
        "a pinned extent's contents must be intact"
    );

    drop(held);
    assert_eq!(
        db.pinned_extents(),
        0,
        "dropping the last reader must release the pin"
    );
}

/// **The test that makes reclamation condition 3 load-bearing.**
///
/// Until slab evacuation existed, removing `any_pinned_in` broke nothing, and I
/// twice recorded that honestly rather than claim the guard was proven. It is
/// proven now: with the check removed this test fails, and the held container
/// comes back holding another key's data.
///
/// Two details are what make it reach the guard at all, and both were why the
/// earlier attempts could not:
///
/// - **No reopen.** A reopened shard restores slab occupancy from metadata, but
///   slab 0 has no metadata region ( it is the superblock ), so it stays
///   `Opaque` forever and nothing in it is ever freeable. A small test's chunk
///   lands in slab 0, so its extent could never be reclaimed and the guard was
///   never consulted. Here the read is forced to disk by *eviction* instead —
///   `checkpoint` empties the memtable, so the next `load` must go to the store.
/// - **Fill first.** The chunk under test has to live in a slab past 0 for the
///   same reason.
///
/// The failure mode this prevents is not a torn read. The reader holds a
/// `Buffer` aliasing the mapping, so reusing that slot means another key's bytes
/// appear underneath a live reader with nothing to signal it.
#[test]
fn a_pinned_extent_is_not_handed_out_while_its_reader_lives() {
    let dir = tmpdir("cond3");
    let _c = CleanDir(dir.clone());
    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 1,
            ..Default::default()
        },
    )
    .unwrap();

    // Push past slab 0, which is permanently Opaque and never reclaimable.
    for k in 0..30u64 {
        db.insert_many(500 + k, &wide(20_000_000 + k * 400_000, 5_000))
            .unwrap();
    }
    db.checkpoint().unwrap();

    let original = wide(0, 5_000);
    db.insert_many(1, &original).unwrap();
    // This checkpoint evicts the memtable, so the load below reads the store and
    // the container it returns aliases the mapping.
    db.checkpoint().unwrap();

    let held = {
        let snap = db.snapshot().unwrap();
        snap.load(1).unwrap()
    };
    assert!(
        db.pinned_extents() > 0,
        "the read must have pinned its extent"
    );

    // Supersede it and churn hard enough that evacuation empties and recycles
    // slabs around it.
    let mut b = db.batch();
    b.delete_key(1);
    b.commit().unwrap();
    for round in 0..10u64 {
        db.insert_many(100 + round, &wide(1_000_000 + round * 500_000, 5_000))
            .unwrap();
        db.checkpoint().unwrap();
    }

    assert!(
        db.freed_extents() > 0,
        "the churn must have reclaimed real extents"
    );
    assert!(
        db.deferred_extents() > 0,
        "the pinned extent must still be queued rather than freed"
    );
    assert_eq!(
        held.iter().collect::<Vec<u64>>(),
        original,
        "a pinned extent was handed out and overwritten while its reader lived"
    );

    // Releasing the reader must let it go.
    drop(held);
    for round in 0..6u64 {
        db.insert_many(300 + round, &wide(9_000_000 + round * 500_000, 5_000))
            .unwrap();
        db.checkpoint().unwrap();
    }
    assert_eq!(
        db.pinned_extents(),
        0,
        "the pin must clear once the reader drops"
    );
}

/// Eviction bounds retention without waiting for the reader to go away.
///
/// **This is the test the 2x space-amplification bound was blocked on**, and the
/// reason it was blocked is worth keeping: the design bounds space
/// amplification at 2x by aborting the oldest snapshot, and enforcing that
/// requires reads through an aborted snapshot to **fail**. Making
/// `Snapshot::{contains, load, cardinality, is_empty, min, max}` return `Result`
/// is a breaking change to the primary read API, so the mechanism could not
/// land until that was decided. It was, on 2026-08-27.
///
/// Three things have to hold together, and only the first two are obvious:
///
/// 1. an evicted reader stops **pinning** retention, so a checkpoint can reclaim;
/// 2. reads through it fail rather than returning stale or freed data;
/// 3. eviction must **not** free the slot — a freed slot can be re-claimed by
///    a new snapshot while the old `ReaderSlot` is still alive, and that `Drop`
///    would then clear a stranger's registration.
#[test]
fn evicting_the_oldest_reader_releases_what_it_was_pinning() {
    let dir = tmpdir("evict");
    let _c = CleanDir(dir.clone());
    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 1,
            ..Default::default()
        },
    )
    .unwrap();

    db.insert_many(1, &(0..40_000u64).map(|i| i * 3).collect::<Vec<_>>())
        .unwrap();
    db.checkpoint().unwrap();

    let reader = db.snapshot().unwrap();
    assert!(!reader.is_evicted(), "a fresh snapshot is live");
    assert_eq!(reader.cardinality(1).unwrap(), 40_000);

    for round in 0..3u64 {
        db.insert_many(
            1,
            &(0..40_000u64)
                .map(|i| i * 3 + 1 + round)
                .collect::<Vec<_>>(),
        )
        .unwrap();
        db.checkpoint().unwrap();
    }

    let reader_version = reader.version();
    let visible_before = db.visible();
    let floor_before = db.safe_version();

    // **Drive the primitive, not the threshold.** `enforce_space_amp` only
    // acts once deferred bytes reach live bytes, and on a corpus this size it
    // never does — measured at 40 KiB deferred against 8 MiB allocated. A test
    // that called it and passed would be asserting that the policy *declined*,
    // while reading as though it had verified eviction. That is the vacuous
    // shape this suite keeps finding, so the threshold is checked separately
    // below and the mechanism is exercised directly here.
    assert_eq!(
        db.enforce_space_amp(),
        0,
        "under the bound, the policy must decline"
    );
    assert!(!reader.is_evicted(), "and must not have touched the reader");

    let evicted = usize::from(db.evict_oldest_reader());
    assert_eq!(evicted, 1, "there is exactly one live reader to evict");

    // ( 2 ) reads through it now fail, and say why.
    assert!(reader.is_evicted());
    let err = reader
        .cardinality(1)
        .expect_err("an evicted read must fail");
    assert!(
        matches!(err, yesno_core::CodecError::SnapshotTooOld { .. }),
        "expected SnapshotTooOld, got {err:?}"
    );
    assert!(reader.load(1).is_err(), "every read path, not just one");
    assert!(reader.contains(1, 0).is_err());
    assert!(reader.min(1).is_err());

    // ( 1 ) it has stopped pinning, so the floor can advance.
    //
    // This assertion was `safe_version() >= floor_before`, which is **vacuous**
    // — `safe_version` only ever moves forward, so it holds whether or not
    // eviction does anything. A sabotage run ( evicted slots keep pinning )
    // passed against it. The property is that the reader was holding the floor
    // *down* to its own version and no longer is, so both halves must be
    // asserted against `visible`.
    assert_eq!(
        floor_before, reader_version,
        "before eviction the floor must be pinned to the reader's version"
    );
    assert!(
        visible_before > reader_version,
        "the fixture must advance visible past the reader, or there is nothing to release"
    );
    assert_eq!(
        db.safe_version(),
        visible_before,
        "an evicted reader must stop holding the safe version down"
    );

    // ( 3 ) the slot is still held, not recycled: the guard is alive.
    assert_eq!(db.evicted_reader_count(), 1);
    drop(reader);
    assert_eq!(
        db.evicted_reader_count(),
        0,
        "dropping the guard must clear the flag, or the next occupant inherits it"
    );

    // And a new snapshot gets a clean slot.
    let fresh = db.snapshot().unwrap();
    assert!(!fresh.is_evicted());
    assert!(fresh.cardinality(1).is_ok());
}

/// A long reader must retain space, and releasing it must give the space back.
///
/// This is the mechanism behind the design's 2x space-amplification bound: the
/// three reclamation conditions hold superseded extents for as long as a
/// snapshot is *entitled* to read them. The bound itself is not enforced yet --
/// enforcing it means aborting the oldest snapshot, which means every
/// `Snapshot` read method returning `Result`, a breaking change to the primary
/// read API -- so what is asserted here is that the
/// machinery underneath it works in both directions — retention while the
/// reader lives, and **release once it is gone**.
///
/// The release direction is the one worth pinning. A leak there is invisible:
/// every read returns correct data, and the file simply grows for ever.
#[test]
fn a_long_reader_retains_space_and_dropping_it_returns_the_space() {
    let dir = tmpdir("space-amp");
    let _c = CleanDir(dir.clone());
    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 1,
            ..Default::default()
        },
    )
    .unwrap();

    // Enough non-inline chunks that superseding them is measurable.
    db.insert_many(1, &(0..40_000u64).map(|i| i * 3).collect::<Vec<_>>())
        .unwrap();
    db.checkpoint().unwrap();
    let baseline = db.deferred_bytes();

    // Pin a view, then churn every chunk so the originals are superseded.
    let reader = db.snapshot().unwrap();
    for round in 0..3 {
        db.insert_many(
            1,
            &(0..40_000u64)
                .map(|i| i * 3 + 1 + round)
                .collect::<Vec<_>>(),
        )
        .unwrap();
        db.checkpoint().unwrap();
    }

    let pinned = db.deferred_bytes();
    assert!(
        pinned > baseline,
        "churn under a live reader retained nothing ( {baseline} -> {pinned} ) - \
         either the reader is not pinning, or nothing was superseded"
    );

    // The reader still sees its own version, which is the point of retaining.
    assert_eq!(reader.cardinality(1).unwrap(), 40_000);

    // Release it. The next checkpoints must be able to let the space go.
    drop(reader);
    for _ in 0..crate_reclaim_delay() + 1 {
        db.checkpoint().unwrap();
    }

    let after = db.deferred_bytes();
    assert!(
        after < pinned,
        "dropping the reader released nothing ( {pinned} -> {after} ) - \
         retained space is never coming back"
    );
}

/// `RECLAIM_CKPT_DELAY` checkpoints must pass before a free is visible.
fn crate_reclaim_delay() -> usize {
    2
}

/// A snapshot created *during* a checkpoint must keep that checkpoint's
/// superseded extents alive.
///
/// **Reclamation condition 1 is a test on versions standing in for a question
/// about reachability**, and the two come apart under concurrency because a
/// `Snapshot`'s version and its captured roots are read at different moments:
///
/// ```text
///   db/mod.rs   let version = self.inner.oracle.visible();   // (A)
///               compare_exchange(FREE, version, ..)          // (B) published
///               for s in &shards { let g = st.lock(); .. }   // (C) roots
/// ```
///
/// Nothing orders (A) against (C). So a checkpoint that sampled `w` can be
/// overtaken by a commit, and a snapshot taken after that commit gets
/// `version > w` while still capturing a **pre-flip root** for any shard the
/// checkpoint has not reached yet. That reader can reach exactly what the
/// checkpoint is about to supersede, and `safe_version > obsolete_at` passes for
/// it because its version is higher.
///
/// The two defences that look like they cover this do not. Sampling `safe`
/// once before the shard loop stops it advancing *mid-loop*, but the extent is
/// not freed in this checkpoint — it waits `RECLAIM_CKPT_DELAY`, and the later
/// reclaim re-samples with the new reader included. And condition 2 is a
/// **delay** ( `ckpt_seq + 2` ), not a reachability test, so a reader alive
/// across two checkpoints passes it.
///
/// Deterministic rather than threaded: the hook fires inside `checkpoint` after
/// the watermarks are sampled and before any shard lock, so the interleaving is
/// produced reentrantly with no timing to get lucky about.
///
/// Reported by the neighbouring session, 2026-08-27; verified against source
/// before being believed.
///
/// **Fixed 2026-08-27.** It failed when written ( 40 000 -> 41 500 ) and
/// passes now that reclamation gates on `Pending::obsolete_ckpt` against
/// `Db::reader_ckpt_floor` — a reachability test rather than a proxy for one.
/// Do not relax it: this failing and then passing is the whole evidence that
/// the fix works.
#[test]
fn a_snapshot_created_during_a_checkpoint_still_pins_what_it_can_reach() {
    let dir = tmpdir("ckpt-race");
    let _c = CleanDir(dir.clone());
    let db = std::sync::Arc::new(
        Db::open_with(
            &dir,
            DbOptions {
                shards: 1,
                ..Default::default()
            },
        )
        .unwrap(),
    );

    // Non-inline chunks, so superseding them defers real extents.
    db.insert_many(1, &(0..20_000u64).map(|i| i * 3).collect::<Vec<_>>())
        .unwrap();
    db.checkpoint().unwrap();

    let caught: std::sync::Arc<std::sync::Mutex<Option<yesno_core::Snapshot>>> =
        std::sync::Arc::new(std::sync::Mutex::new(None));

    // Supersede every chunk, and during that checkpoint advance `visible` past
    // the `w` it sampled and take a snapshot.
    db.insert_many(1, &(0..20_000u64).map(|i| i * 3 + 1).collect::<Vec<_>>())
        .unwrap();
    {
        let (d, c) = (db.clone(), caught.clone());
        db.set_checkpoint_hook(Some(std::sync::Arc::new(move || {
            if c.lock().unwrap().is_some() {
                return;
            }
            // (2) a commit lands after `w` was sampled.
            d.insert(2, 7).unwrap();
            // (3)+(4) version is now above `w`; the root is still pre-flip.
            *c.lock().unwrap() = Some(d.snapshot().unwrap());
        })));
    }
    db.checkpoint().unwrap();
    db.set_checkpoint_hook(None);

    let reader = caught.lock().unwrap().take().expect("hook must have run");
    let pinned_version = reader.version();

    // Read through `load`, not `cardinality`. `Snapshot::cardinality` answers
    // from `card_m1` in the index **without touching a payload** — that is a
    // headline property of the format, and it means it cannot notice an extent
    // that was recycled underneath it. Only a path that decodes can.
    let before: Vec<u64> = reader.load(1).unwrap().iter().collect();
    assert_eq!(
        before.len(),
        40_000,
        "the pinned view is both inserts: the second committed before the checkpoint began"
    );

    // Now push past RECLAIM_CKPT_DELAY while the reader stays alive.
    for round in 0..4u64 {
        db.insert_many(
            1,
            &(0..500u64)
                .map(|i| i * 7 + 900_000 + round)
                .collect::<Vec<_>>(),
        )
        .unwrap();
        db.checkpoint().unwrap();
    }

    // The reader is still live and still entitled to its version.
    assert!(!reader.is_evicted(), "nothing evicted it");
    assert!(
        db.safe_version() <= pinned_version,
        "a live reader at {pinned_version} must hold the safe version at or below it, \
         got {} — condition 1 would then permit freeing what its root reaches",
        db.safe_version()
    );

    // And its view must be unchanged, byte for byte.
    let after: Vec<u64> = reader.load(1).unwrap().iter().collect();
    // Observed failure: 40 000 -> 41 500, gaining exactly the ordinals committed
    // *after* this snapshot's version ( 900 000.. ) and losing none. It is
    // reading through a stale root whose pages have been recycled and now hold a
    // newer tree — a use-after-free of a file slot, which no memory sanitiser
    // can see because it is not an address.
    //
    // Isolated by `control_a_normal_snapshot_does_not_drift_under_later_churn`,
    // which runs the identical sequence with the snapshot taken normally and
    // holds steady at 40 000. The difference is the window, not the churn.
    // Compare the **contents**, not the count. This asserted
    // `after.len() == before.len()` while the prose above claimed "byte for
    // byte" — an equal-cardinality substitution would have passed it, which is
    // exactly the recycled-slot outcome most worth catching, since a recycled
    // index node yields a *coherent* tree rather than a short one. Caught in
    // review, 2026-08-27.
    assert_eq!(
        after.len(),
        before.len(),
        "the pinned view changed size under the reader"
    );
    assert_eq!(
        after, before,
        "the pinned view changed under the reader — an extent its root reaches was recycled"
    );
}

/// CONTROL for `a_snapshot_created_during_a_checkpoint_still_pins_what_it_can_reach`:
/// the identical sequence with the snapshot taken **normally**.
#[test]
fn control_a_normal_snapshot_does_not_drift_under_later_churn() {
    let dir = tmpdir("ckpt-race-ctl");
    let _c = CleanDir(dir.clone());
    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 1,
            ..Default::default()
        },
    )
    .unwrap();
    db.insert_many(1, &(0..20_000u64).map(|i| i * 3).collect::<Vec<_>>())
        .unwrap();
    db.checkpoint().unwrap();
    db.insert_many(1, &(0..20_000u64).map(|i| i * 3 + 1).collect::<Vec<_>>())
        .unwrap();
    db.checkpoint().unwrap();
    db.insert(2, 7).unwrap();
    let reader = db.snapshot().unwrap();

    let before: Vec<u64> = reader.load(1).unwrap().iter().collect();
    for round in 0..4u64 {
        db.insert_many(
            1,
            &(0..500u64)
                .map(|i| i * 7 + 900_000 + round)
                .collect::<Vec<_>>(),
        )
        .unwrap();
        db.checkpoint().unwrap();
    }
    let after: Vec<u64> = reader.load(1).unwrap().iter().collect();
    // Contents, for the same reason: the control's job is to show this sequence
    // is harmless *outside* the window, and a drift that preserved cardinality
    // would have left it saying so falsely.
    assert_eq!(after, before, "a normally-taken snapshot drifted");
}

// `a_reader_does_not_block_extents_superseded_before_it_existed` lived here and
// was removed on 2026-08-27. Its property is real and load-bearing — a fix that
// simply stops reclaiming is safe, wrong, and otherwise indistinguishable from a
// working one — but `Db::deferred_extents()` cannot express it: the checkpoints
// needed to advance past `RECLAIM_CKPT_DELAY` defer index nodes of their own, so
// the count is a mixture and every threshold on it was arbitrary. Two earlier
// versions read *correct* retention ( a reader legitimately blocking extents its
// root reaches ) as a defect.
//
// The property now lives where it can be stated exactly, as
// `reclamation_gates_on_the_superseding_checkpoint_not_the_version` in
// `store::alloc`: floor 6 blocks, floor 0 blocks, floor 7 reclaims.

/// An idle database reclaims what it superseded, and stops once it has.
///
/// **This test asserted the opposite until 2026-08-27**, and the history is
/// the useful part. Removing the version gate was expected to fix idle
/// reclamation — `safe_version` only advances on a commit — and it did not.
/// Two other causes, one of which is not a bug:
///
/// 1. `Db::checkpoint` early-outs when there is nothing dirty, so it never
///    reached `reclaim_deferred` at all.
/// 2. Condition 2 is the **A/B superblock rule**. After checkpoint `k`, slot B
///    still names `root_{k-1}`, from which a superseded extent *is* reachable,
///    and that slot is durable — a crash would recover to it. Blocking is
///    correct, and it means **there is no way to return this space without
///    writing**.
///
/// So the idle path now flips the superblock when something is queued. Do not
/// "optimize" that flip away: it is not bookkeeping, it is what makes the extent
/// unreachable from every durable root.
///
/// What made this urgent was not the retention itself — the deferred list is
/// non-durable, so a restart returns the space — but that the design
/// promises a **2x bound by construction**, and evicting the oldest reader
/// returned nothing while the database was idle.
#[test]
fn an_idle_database_reclaims_and_then_goes_quiet() {
    let dir = tmpdir("idle-reclaim");
    let _c = CleanDir(dir.clone());
    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 1,
            ..Default::default()
        },
    )
    .unwrap();

    db.insert_many(1, &(0..20_000u64).map(|i| i * 3).collect::<Vec<_>>())
        .unwrap();
    db.checkpoint().unwrap();
    db.insert_many(1, &(0..20_000u64).map(|i| i * 3 + 1).collect::<Vec<_>>())
        .unwrap();
    db.checkpoint().unwrap();

    let queued = db.deferred_extents();
    assert!(queued > 0, "the fixture must actually defer something");
    let visible_when_idle = db.visible();

    // From here: no inserts, no batches — nothing that advances `visible`.
    for _ in 0..4 {
        db.checkpoint().unwrap();
    }
    assert_eq!(
        db.visible(),
        visible_when_idle,
        "the fixture is only meaningful while nothing commits"
    );
    assert_eq!(
        db.deferred_extents(),
        0,
        "an idle database retained {queued} superseded extents"
    );

    // ...and then stops writing. Self-limiting is the whole reason the flip is
    // gated on queued work: a quiescent database must not spin on the disk.
    let seq_after_drain = db.checkpoint_count_for_test();
    for _ in 0..4 {
        db.checkpoint().unwrap();
    }
    assert_eq!(
        db.checkpoint_count_for_test(),
        seq_after_drain,
        "with nothing queued the idle path must write nothing at all"
    );
}

/// Evicting the oldest reader must actually return the space, idle or not.
///
/// **The bound the design promises is "2x by construction", and until
/// the idle path flipped and reclaimed it silently lapsed the moment writes
/// stopped**: eviction succeeded, `is_evicted` was true, and deferred extents
/// went *up*. Eviction is only the mechanism; a checkpoint is what returns the
/// space, and an idle database ran none.
#[test]
fn eviction_returns_space_even_when_the_database_is_idle() {
    let dir = tmpdir("idle-evict");
    let _c = CleanDir(dir.clone());
    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 1,
            ..Default::default()
        },
    )
    .unwrap();
    db.insert_many(1, &(0..20_000u64).map(|i| i * 3).collect::<Vec<_>>())
        .unwrap();
    db.checkpoint().unwrap();

    let reader = db.snapshot().unwrap();
    db.insert_many(1, &(0..20_000u64).map(|i| i * 3 + 1).collect::<Vec<_>>())
        .unwrap();
    db.checkpoint().unwrap();
    let pinned = db.deferred_extents();
    assert!(pinned > 0, "the reader must be pinning something");

    assert!(db.evict_oldest_reader(), "there is a reader to evict");
    assert!(reader.is_evicted());

    // Idle from here: no commits, only checkpoints.
    for _ in 0..4 {
        db.checkpoint().unwrap();
    }
    assert_eq!(
        db.deferred_extents(),
        0,
        "evicting returned nothing ( {pinned} still queued ) — the 2x bound does \
         not hold while the database is idle"
    );
}

/// A foreign reader must hold back the writer's reclamation floor.
///
/// **Reclamation condition 1 — no live snapshot can reach the extent — is
/// enforced from `DbInner::readers`, which is process-local memory.** A reader
/// opened by another process is invisible to it, so without the shared
/// registry the writer computes its floor as if nothing were reading and reuses
/// extents the reader is still following.
///
/// Condition 3 does not cover this. That one is the live `Buffer` refcount,
/// also process-local; the foreign process's mapping keeps its own file pages
/// alive but does nothing to stop the writer reusing the slot underneath them.
///
/// This test runs the registration in-process — a second `Db` handle on the
/// same directory, which is exactly what a second process would have — so it
/// asserts the *floor arithmetic* rather than needing a real fork.
#[test]
fn a_foreign_reader_holds_back_the_writers_reclamation_floor() {
    use yesno_core::{Db, DbOptions};

    let dir = std::env::temp_dir().join(format!("yesno-foreign-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let opts = || DbOptions {
        shards: 1,
        ..Default::default()
    };

    let writer = Db::open_with(&dir, opts()).unwrap();
    writer.insert_many(1, &[1, 2, 3]).unwrap();
    writer.checkpoint().unwrap();

    // With nothing reading, the floor is unconstrained.
    let unconstrained = writer.reader_ckpt_floor();
    assert_eq!(
        unconstrained,
        u64::MAX,
        "with no readers at all the floor must free everything eligible"
    );

    {
        let reader = Db::open_reader(&dir).unwrap();
        let snap = reader.snapshot().unwrap();
        assert_eq!(snap.cardinality(1).unwrap(), 3);

        // The assertion the registry exists for. The writer has no local
        // reader, yet its floor must now be finite.
        let held = writer.reader_ckpt_floor();
        assert!(
            held < u64::MAX,
            "a foreign reader must hold back the floor; got {held}"
        );
        assert!(
            writer.safe_version() <= writer.visible(),
            "a foreign reader must not raise safe_version above visible"
        );

        // The writer keeps working while the reader is attached — this is a
        // retention constraint, not a lock.
        writer.insert_many(2, &[7]).unwrap();
        writer.checkpoint().unwrap();
        assert!(
            writer.reader_ckpt_floor() < u64::MAX,
            "the floor must stay held across the writer's own checkpoints"
        );

        drop(snap);
        drop(reader);
    }

    // Released on drop, so retention returns to unconstrained. A registry
    // that leaked slots would pin space for the life of the writer.
    assert_eq!(
        writer.reader_ckpt_floor(),
        u64::MAX,
        "a released reader must stop holding the floor"
    );

    drop(writer);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A snapshot keeps its own view of keys that **share a packed page** with a key
/// that has since been rewritten.
///
/// # The interaction this covers, which neither neighbour does
///
/// `a_packed_page_survives_while_any_chunk_in_it_is_live` is about **space**:
/// the allocator must not free a page whose neighbours are still referenced.
/// This is about **visibility**: a page is repacked whole, so rewriting key `A`
/// produces a new page containing a fresh copy of untouched key `B` — and an
/// older snapshot must keep reading `B` (and `A`) through the *previous* page,
/// at the values that were current when it was taken.
///
/// That makes packing an MVCC question and not only an allocation one: one
/// key's write moves another key's bytes. Before this test nothing in
/// `zero_copy_mvcc.rs` mentioned packed pages at all.
///
/// # Why the fixture is shaped this way
///
/// Values are small enough to pack (well under `PACK_MAX`), and **several keys
/// are written in one batch** so they land in the same page. Only *half* are
/// rewritten: a round that rewrote every key would leave no survivor sharing a
/// page with a rewritten neighbour, which is the entire subject — the same trap
/// `a_packed_page_survives_while_any_chunk_in_it_is_live` records about its own
/// neighbour.
#[test]
fn a_snapshot_reads_packed_neighbours_at_its_own_version() {
    let dir = tmpdir("mvcc-packed-neighbours");
    let _c = CleanDir(dir.clone());
    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 1,
            ..Default::default()
        },
    )
    .unwrap();

    // Small values: a handful of ordinals in one chunk packs, rather than
    // taking a standalone extent.
    let vals =
        |k: u64, gen: u64| -> Vec<u64> { (0..6u64).map(|i| (k << 20) | (gen * 100 + i)).collect() };

    const KEYS: u64 = 8;
    for k in 0..KEYS {
        db.insert_many(k, &vals(k, 0)).unwrap();
    }
    db.checkpoint().unwrap();

    // The snapshot under test, taken before anything is rewritten.
    let before = db.snapshot().unwrap();
    for k in 0..KEYS {
        assert_eq!(
            before.load(k).unwrap().iter().collect::<Vec<_>>(),
            vals(k, 0),
            "key {k} must read its generation-0 value before any rewrite"
        );
    }

    // Rewrite HALF the keys. The rest survive in whatever page they shared.
    {
        let mut b = db.batch();
        for k in (0..KEYS).step_by(2) {
            // Whole-key replace, so generation 0 is genuinely superseded rather
            // than merged with -- the old bytes must survive only via the old
            // snapshot, not because they are still part of the current set.
            b.store_set(k, &OrdSet::from_iter_unsorted(vals(k, 1)));
        }
        b.commit().unwrap();
    }
    db.checkpoint().unwrap();

    // The old snapshot is unmoved: rewritten keys AND their untouched
    // page-neighbours both read generation 0.
    for k in 0..KEYS {
        assert_eq!(
            before.load(k).unwrap().iter().collect::<Vec<_>>(),
            vals(k, 0),
            "key {k} moved under a snapshot taken before the rewrite"
        );
    }

    // A fresh snapshot sees the rewrite, and sees the untouched keys unchanged.
    let after = db.snapshot().unwrap();
    for k in 0..KEYS {
        let gen = u64::from(k % 2 == 0);
        assert_eq!(
            after.load(k).unwrap().iter().collect::<Vec<_>>(),
            vals(k, gen),
            "key {k} wrong at the new version ( expected generation {gen} )"
        );
    }

    // Non-vacuity: the two snapshots must actually disagree somewhere, or the
    // assertions above would hold for a database that never rewrote anything.
    assert_ne!(
        before.load(0).unwrap().iter().collect::<Vec<_>>(),
        after.load(0).unwrap().iter().collect::<Vec<_>>(),
        "the fixture must produce a visible difference between the snapshots"
    );
    drop(before);
    drop(after);
}
