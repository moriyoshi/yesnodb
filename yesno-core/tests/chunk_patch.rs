//! `WriteBatch::patch_chunk`, against the point writer as oracle.
//!
//! # What this file is defending
//!
//! The operation is `new = ( old \ clear ) union set` on one chunk, and its
//! whole reason to exist is that it must mean **the same thing live and on
//! replay**. Its predecessor does not: `Op::PutChunk` replaces the chunk in the
//! memtable and logs a `RecType::ChunkImage`, which replay applies per ordinal
//! and therefore unions. Those two agree only because the one producer,
//! `store_set`, emits a `DeleteKey` ahead of them, so both act on an emptied
//! key. `store_set_replays_to_what_it_committed` in `db/mod.rs` pins that, and
//! is the test this file must not make redundant -- it is the demonstration
//! that a bare image record would diverge.
//!
//! So the rule here follows `durability.rs`: **reopen before asserting**, and
//! assert contents rather than cardinality. A patch that committed one state
//! and recovered another would pass every test that reads the live handle.
//!
//! The oracle is the slow path. Every case that can be expressed as point
//! inserts and removes is checked against a `Db` built that way, because the
//! point writer is the definition of what these sets mean.

use std::path::PathBuf;

use yesno_core::{Container, Db, DbOptions, OrdSet};

struct CleanDir(PathBuf);
impl Drop for CleanDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tmpdir(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("yesno-patch-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    p
}

fn opts() -> DbOptions {
    DbOptions {
        shards: 2,
        ..Default::default()
    }
}

/// A container holding exactly these low bits.
fn mask(bits: impl IntoIterator<Item = u16>) -> Container {
    let mut c = Container::new_array();
    for b in bits {
        c.insert(b);
    }
    c
}

fn members(db: &Db, key: u64) -> Vec<u64> {
    db.snapshot().unwrap().load(key).unwrap().iter().collect()
}

/// Reopen without checkpointing, so the answer comes from WAL replay.
fn replayed(dir: &PathBuf, key: u64) -> Vec<u64> {
    let db = Db::open_with(dir, opts()).unwrap();
    let out = members(&db, key);
    drop(db);
    out
}

#[test]
fn a_patch_is_clear_then_set_and_set_wins_on_overlap() {
    let dir = tmpdir("overlap");
    let _c = CleanDir(dir.clone());
    {
        let db = Db::open_with(&dir, opts()).unwrap();
        db.insert_many(1, &[1, 2, 3, 4, 5]).unwrap();

        // Clear 2..=4, set 3 and 9. 3 is in both masks, and set wins.
        let mut b = db.batch();
        b.patch_chunk(1, 0, &mask([2, 3, 4]), &mask([3, 9]));
        b.commit().unwrap();

        assert_eq!(members(&db, 1), vec![1, 3, 5, 9]);
    }
    assert_eq!(
        replayed(&dir, 1),
        vec![1, 3, 5, 9],
        "replay must reach the state the commit published"
    );
}

/// The oracle check: the same edit expressed as points must agree.
#[test]
fn a_patch_agrees_with_the_point_writer() {
    let patched = tmpdir("oracle-patch");
    let pointed = tmpdir("oracle-point");
    let _a = CleanDir(patched.clone());
    let _b = CleanDir(pointed.clone());

    let start: Vec<u64> = (0..300).map(|i| i * 7 % 65536).collect();
    let clear_bits: Vec<u16> = (0..120).map(|i| (i * 11 % 65536) as u16).collect();
    let set_bits: Vec<u16> = (0..200).map(|i| (i * 13 % 65536) as u16).collect();

    {
        let db = Db::open_with(&patched, opts()).unwrap();
        db.insert_many(1, &start).unwrap();
        let mut b = db.batch();
        b.patch_chunk(
            1,
            0,
            &mask(clear_bits.iter().copied()),
            &mask(set_bits.iter().copied()),
        );
        b.commit().unwrap();
    }
    {
        let db = Db::open_with(&pointed, opts()).unwrap();
        db.insert_many(1, &start).unwrap();
        let mut b = db.batch();
        for &bit in &clear_bits {
            b.remove(1, bit as u64);
        }
        for &bit in &set_bits {
            b.insert(1, bit as u64);
        }
        b.commit().unwrap();
    }

    let from_patch = replayed(&patched, 1);
    let from_points = replayed(&pointed, 1);
    assert_eq!(
        from_patch, from_points,
        "a patch must mean exactly what the point writer means"
    );
    assert!(!from_patch.is_empty());
}

#[test]
fn patches_compose_in_arrival_order_with_every_other_operation() {
    let dir = tmpdir("order");
    let _c = CleanDir(dir.clone());
    {
        let db = Db::open_with(&dir, opts()).unwrap();
        db.insert_many(1, &[1, 2, 3]).unwrap();

        // delete_key, then a patch, then a range, then a second patch of the
        // same prefix. Grouping these by kind would produce a different set,
        // which is what makes this fixture worth having.
        let mut b = db.batch();
        b.delete_key(1);
        b.patch_chunk(1, 0, &Container::new_array(), &mask([10, 11, 12]));
        b.insert_range(1, 20, 22);
        b.patch_chunk(1, 0, &mask([11, 21]), &mask([30]));
        b.commit().unwrap();

        assert_eq!(members(&db, 1), vec![10, 12, 20, 22, 30]);
    }
    assert_eq!(replayed(&dir, 1), vec![10, 12, 20, 22, 30]);
}

#[test]
fn a_patch_after_delete_key_starts_from_empty() {
    let dir = tmpdir("after-delete");
    let _c = CleanDir(dir.clone());
    {
        let db = Db::open_with(&dir, opts()).unwrap();
        db.insert_many(1, &[1, 2, 3, 4]).unwrap();
        db.checkpoint().unwrap();

        // The delete must be visible to the patch even though the old chunk is
        // on disk: a tombstone is a real answer and must not resurrect it.
        let mut b = db.batch();
        b.delete_key(1);
        b.patch_chunk(1, 0, &mask([1, 2]), &mask([7]));
        b.commit().unwrap();

        assert_eq!(members(&db, 1), vec![7]);
    }
    assert_eq!(replayed(&dir, 1), vec![7]);
}

#[test]
fn a_patch_that_empties_a_chunk_tombstones_it() {
    let dir = tmpdir("tombstone");
    let _c = CleanDir(dir.clone());
    {
        let db = Db::open_with(&dir, opts()).unwrap();
        db.insert_many(1, &[1, 2, 3]).unwrap();
        db.checkpoint().unwrap();

        let mut b = db.batch();
        b.patch_chunk(1, 0, &mask([1, 2, 3]), &Container::new_array());
        b.commit().unwrap();

        assert_eq!(members(&db, 1), Vec::<u64>::new());
    }
    assert_eq!(
        replayed(&dir, 1),
        Vec::<u64>::new(),
        "an emptied chunk must not show the checkpointed value through it"
    );
}

#[test]
fn repeated_patches_to_one_chunk_accumulate() {
    let dir = tmpdir("repeat");
    let _c = CleanDir(dir.clone());
    {
        let db = Db::open_with(&dir, opts()).unwrap();
        // Three separate commits, so each patch reads the previous one's
        // published chunk rather than a snapshot taken before it.
        for round in 0..3u16 {
            let mut b = db.batch();
            b.patch_chunk(
                1,
                0,
                &Container::new_array(),
                &mask([round * 10, round * 10 + 1]),
            );
            b.commit().unwrap();
        }
        assert_eq!(members(&db, 1), vec![0, 1, 10, 11, 20, 21]);

        // And one that clears part of what earlier rounds wrote.
        let mut b = db.batch();
        b.patch_chunk(1, 0, &mask([0, 10, 20]), &mask([99]));
        b.commit().unwrap();
        assert_eq!(members(&db, 1), vec![1, 11, 21, 99]);
    }
    assert_eq!(replayed(&dir, 1), vec![1, 11, 21, 99]);
}

#[test]
fn a_patch_touches_only_its_own_chunk() {
    let dir = tmpdir("boundary");
    let _c = CleanDir(dir.clone());
    {
        let db = Db::open_with(&dir, opts()).unwrap();
        // The same low bits in three adjacent chunks.
        db.insert_many(1, &[5, 65536 + 5, 131072 + 5]).unwrap();

        let mut b = db.batch();
        b.patch_chunk(1, 1, &mask([5]), &mask([6]));
        b.commit().unwrap();

        assert_eq!(members(&db, 1), vec![5, 65536 + 6, 131072 + 5]);
    }
    assert_eq!(replayed(&dir, 1), vec![5, 65536 + 6, 131072 + 5]);
}

#[test]
fn the_reserved_maximum_ordinal_cannot_be_patched() {
    let dir = tmpdir("i8");
    let _c = CleanDir(dir.clone());
    let db = Db::open_with(&dir, opts()).unwrap();

    // I8: `u64::MAX` is not an ordinal, so the top slot of the top chunk is
    // not addressable.
    let top = (1u64 << 48) - 1;
    let mut b = db.batch();
    b.patch_chunk(1, top, &Container::new_array(), &mask([u16::MAX]));
    assert!(
        b.commit().is_err(),
        "the reserved maximum ordinal must not become durable"
    );

    // One slot below it is fine, which is what makes the check a boundary
    // rather than a refusal of the whole chunk.
    let mut b = db.batch();
    b.patch_chunk(1, top, &Container::new_array(), &mask([u16::MAX - 1]));
    b.commit().unwrap();
    assert_eq!(members(&db, 1), vec![u64::MAX - 1]);
}

#[test]
fn a_prefix_beyond_48_bits_is_refused() {
    let dir = tmpdir("prefix");
    let _c = CleanDir(dir.clone());
    let db = Db::open_with(&dir, opts()).unwrap();
    let mut b = db.batch();
    b.patch_chunk(1, 1u64 << 48, &Container::new_array(), &mask([1]));
    assert!(b.commit().is_err());
}

#[test]
fn two_empty_masks_are_a_no_op() {
    let dir = tmpdir("noop");
    let _c = CleanDir(dir.clone());
    let db = Db::open_with(&dir, opts()).unwrap();
    db.insert_many(1, &[1, 2, 3]).unwrap();

    let mut b = db.batch();
    b.patch_chunk(1, 0, &Container::new_array(), &Container::new_array());
    let committed = b.commit().unwrap();
    assert_eq!(committed.changed, 0, "an empty patch changes nothing");
    assert_eq!(members(&db, 1), vec![1, 2, 3]);
}

#[test]
fn a_patch_survives_a_checkpoint_interleaving() {
    let dir = tmpdir("checkpoint");
    let _c = CleanDir(dir.clone());
    {
        let db = Db::open_with(&dir, opts()).unwrap();
        db.insert_many(1, &[1, 2, 3]).unwrap();

        let mut b = db.batch();
        b.patch_chunk(1, 0, &mask([2]), &mask([8]));
        b.commit().unwrap();
        db.checkpoint().unwrap();

        // A second patch after the checkpoint reads the persisted chunk.
        let mut b = db.batch();
        b.patch_chunk(1, 0, &mask([1]), &mask([9]));
        b.commit().unwrap();

        assert_eq!(members(&db, 1), vec![3, 8, 9]);
    }
    // Reopen: the first patch comes from the checkpoint, the second from WAL.
    let db = Db::open_with(&dir, opts()).unwrap();
    assert_eq!(members(&db, 1), vec![3, 8, 9]);
}

/// A patch of a large dense chunk, which is where the operation earns its keep.
#[test]
fn a_dense_bitmap_patch_replays_exactly() {
    let dir = tmpdir("dense");
    let _c = CleanDir(dir.clone());
    let set: Vec<u16> = (0..40000u32).map(|i| i as u16).collect();
    {
        let db = Db::open_with(&dir, opts()).unwrap();
        let mut b = db.batch();
        b.patch_chunk(1, 0, &Container::new_array(), &mask(set.iter().copied()));
        b.commit().unwrap();
        assert_eq!(members(&db, 1).len(), 40000);
    }
    let after = replayed(&dir, 1);
    assert_eq!(after.len(), 40000);
    assert_eq!(after.first(), Some(&0));
    assert_eq!(after.last(), Some(&39999));
}

/// Patching a key that also receives a whole-set store in the same batch.
#[test]
fn a_patch_composes_with_store_set_in_arrival_order() {
    let dir = tmpdir("with-store");
    let _c = CleanDir(dir.clone());
    {
        let db = Db::open_with(&dir, opts()).unwrap();
        let mut b = db.batch();
        b.patch_chunk(1, 0, &Container::new_array(), &mask([1, 2, 3]));
        b.store_set(1, &OrdSet::from_iter_unsorted([50u64, 51]));
        b.patch_chunk(1, 0, &Container::new_array(), &mask([60]));
        b.commit().unwrap();
        // The store replaces what the first patch wrote; the second patch
        // lands on the stored set.
        assert_eq!(members(&db, 1), vec![50, 51, 60]);
    }
    assert_eq!(replayed(&dir, 1), vec![50, 51, 60]);
}
