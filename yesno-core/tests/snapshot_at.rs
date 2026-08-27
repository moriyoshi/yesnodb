//! Reading at a version the caller names.
//!
//! # Why this layer
//!
//! `Db::snapshot_at` exists so that N readers answering one query can be pinned
//! to one instant. Its whole value is in what it **refuses**, and a refusal is
//! only worth anything if the alternative would have been wrong — so every test
//! here that expects an error first establishes that the answer would otherwise
//! have differed. A test that merely asserts `is_err()` would pass against an
//! implementation that refused everything.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use yesno_core::{CodecError, Db, DbOptions};

/// No `tempfile`, for the reason `replica.rs` and `durability.rs` give: the
/// crate's dependency count is itself a checked property.
struct CleanDir(PathBuf);

impl CleanDir {
    fn new(tag: &str) -> CleanDir {
        let mut p = std::env::temp_dir();
        p.push(format!("yesno-snapat-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        CleanDir(p)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for CleanDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn put(db: &Db, key: u64, ordinals: &[u64]) -> u64 {
    let mut wb = db.batch();
    for &o in ordinals {
        wb.insert(key, o);
    }
    wb.commit().unwrap();
    db.visible()
}

fn read_at(db: &Db, version: u64, key: u64) -> BTreeSet<u64> {
    let snap = db.snapshot_at(version).unwrap();
    snap.load(key).unwrap().iter().collect()
}

/// The point of the whole feature: an older version still answers as itself.
///
/// The second assertion is the load-bearing one. Without it this test passes
/// against an implementation that ignores its argument entirely and opens a
/// current snapshot — which is exactly what `do_get` did before this landed, and
/// exactly the defect the feature exists to close.
#[test]
fn an_older_version_answers_with_the_state_it_had() {
    let db = Db::new();
    let v1 = put(&db, 7, &[1, 2, 3]);
    let v2 = put(&db, 7, &[4, 5]);
    assert!(v2 > v1, "the second commit must advance the clock");

    assert_eq!(read_at(&db, v1, 7), BTreeSet::from([1, 2, 3]));
    assert_eq!(read_at(&db, v2, 7), BTreeSet::from([1, 2, 3, 4, 5]));
    // Stated as an inequality as well, so the test still means something if
    // someone "fixes" the sets above to match each other.
    assert_ne!(
        read_at(&db, v1, 7),
        read_at(&db, v2, 7),
        "if these agree the argument is being ignored and every other \
         assertion here is vacuous"
    );
}

/// A removal is history too, and it is the direction a set-difference bug hides
/// in: an implementation that replays only insertions would pass the test above.
#[test]
fn an_older_version_still_holds_what_was_since_removed() {
    let db = Db::new();
    let v1 = put(&db, 7, &[10, 20, 30]);
    let mut wb = db.batch();
    wb.remove(7, 20);
    wb.commit().unwrap();
    let v2 = db.visible();

    assert_eq!(read_at(&db, v1, 7), BTreeSet::from([10, 20, 30]));
    assert_eq!(read_at(&db, v2, 7), BTreeSet::from([10, 30]));
}

/// Every shard answers from the same instant, which is what "consistent" means
/// here — a cross-shard read is the case a per-shard version would tear.
#[test]
fn one_version_pins_every_shard() {
    let db = Db::with_options(DbOptions {
        shards: 4,
        ..Default::default()
    });
    // Keys chosen to land on different shards; the assertion below does not
    // depend on which, only that more than one is involved.
    let keys: Vec<u64> = (0..16).collect();
    let mut wb = db.batch();
    for &k in &keys {
        wb.insert(k, 1);
    }
    wb.commit().unwrap();
    let v1 = db.visible();

    let mut wb = db.batch();
    for &k in &keys {
        wb.insert(k, 2);
    }
    wb.commit().unwrap();

    let shards: BTreeSet<usize> = keys.iter().map(|&k| db.shard_of(k)).collect();
    assert!(
        shards.len() > 1,
        "the corpus must span shards or this proves nothing"
    );

    let snap = db.snapshot_at(v1).unwrap();
    for &k in &keys {
        assert_eq!(
            snap.load(k).unwrap().iter().collect::<BTreeSet<_>>(),
            BTreeSet::from([1]),
            "shard {} answered from a later instant",
            db.shard_of(k)
        );
    }
}

/// A version this database has not assigned is refused, not clamped.
#[test]
fn a_version_above_visible_is_refused() {
    let db = Db::new();
    let v = put(&db, 7, &[1]);
    match db.snapshot_at(v + 1) {
        Err(CodecError::VersionNotVisible { requested, visible }) => {
            assert_eq!(requested, v + 1);
            assert_eq!(visible, v);
        }
        Ok(_) => panic!("granted a snapshot at a version this database never assigned"),
        Err(e) => panic!("expected VersionNotVisible, got {e:?}"),
    }
    // And it is *only* the future that is refused. A feature that rejected
    // the boundary too would be useless: `get_flight_info` mints tickets at
    // exactly `visible`.
    assert!(
        db.snapshot_at(v).is_ok(),
        "the newest visible version must be readable"
    );
}

/// A version whose history a checkpoint has collapsed is refused rather than
/// answered from what survives.
///
/// The `pre` assertion is what makes this a test of the *refusal* rather than
/// of the floor's arithmetic: it establishes that `v1` really did name a
/// different set, so answering it after the checkpoint would have been a wrong
/// answer and not merely a stale one.
#[test]
fn a_reclaimed_version_is_refused_rather_than_answered_from_the_floor() {
    let dir = CleanDir::new("reclaimed");
    let db = Db::open(dir.path()).unwrap();
    let v1 = put(&db, 7, &[1, 2, 3]);
    let pre = read_at(&db, v1, 7);
    assert_eq!(pre, BTreeSet::from([1, 2, 3]));

    let v2 = put(&db, 7, &[4, 5]);
    db.checkpoint().unwrap();

    assert!(
        db.read_floor() > v1,
        "a checkpoint with no live reader must collapse history above {v1}; \
         floor is {}",
        db.read_floor()
    );
    match db.snapshot_at(v1) {
        Err(CodecError::VersionReclaimed { requested, floor }) => {
            assert_eq!(requested, v1);
            assert!(floor > v1);
        }
        Ok(s) => panic!(
            "granted a snapshot at a reclaimed version; it answers {:?} where {pre:?} was asked for",
            s.load(7).unwrap().iter().collect::<BTreeSet<_>>()
        ),
        Err(e) => panic!("expected VersionReclaimed, got {e:?}"),
    }
    // The current version is unaffected — the floor rose, it did not close.
    assert_eq!(read_at(&db, v2, 7), BTreeSet::from([1, 2, 3, 4, 5]));
}

/// A held snapshot keeps its own version readable across a checkpoint.
///
/// This is the property a fan-out coordinator actually depends on, and it is
/// the one the ordering inside `snapshot_at` exists for: registering at `v`
/// drags `safe_version` down to `v`, so the checkpoint cannot prune past it.
#[test]
fn a_live_reader_holds_the_floor_down_for_its_own_version() {
    let dir = CleanDir::new("held");
    let db = Db::open(dir.path()).unwrap();
    let v1 = put(&db, 7, &[1, 2, 3]);
    let held = db.snapshot_at(v1).unwrap();

    put(&db, 7, &[4, 5]);
    db.checkpoint().unwrap();

    assert!(
        db.read_floor() <= v1,
        "a live reader at {v1} must hold the floor down; it is {}",
        db.read_floor()
    );
    // A *second* reader can still be granted the same version — which is the
    // whole fan-out case: N endpoints, one instant.
    assert_eq!(read_at(&db, v1, 7), BTreeSet::from([1, 2, 3]));
    assert_eq!(
        held.load(7).unwrap().iter().collect::<BTreeSet<_>>(),
        BTreeSet::from([1, 2, 3])
    );
    drop(held);
}

/// The floor only rises.
///
/// Monotonicity is not decoration: a caller that has been refused at `v` must
/// be able to conclude that retrying `v` is pointless. If the floor could fall,
/// the honest advice would be "retry", and the error's own text says otherwise.
#[test]
fn the_read_floor_never_falls() {
    let dir = CleanDir::new("monotone");
    let db = Db::open(dir.path()).unwrap();
    let mut last = db.read_floor();
    for round in 0..6u64 {
        put(&db, 7, &[round]);
        db.checkpoint().unwrap();
        let now = db.read_floor();
        assert!(
            now >= last,
            "floor fell from {last} to {now} in round {round}"
        );
        last = now;
        // A reader taken and dropped must not push it back down either.
        let s = db.snapshot().unwrap();
        drop(s);
        assert!(db.read_floor() >= last);
    }
}

/// `snapshot()` and `snapshot_at(visible())` are the same request.
#[test]
fn snapshot_is_snapshot_at_visible() {
    let db = Db::new();
    put(&db, 7, &[1, 2, 3]);
    let v = db.visible();
    let a = db.snapshot().unwrap();
    let b = db.snapshot_at(v).unwrap();
    assert_eq!(a.version(), b.version());
    assert_eq!(
        a.load(7).unwrap().iter().collect::<BTreeSet<_>>(),
        b.load(7).unwrap().iter().collect::<BTreeSet<_>>()
    );
}

/// A refused request leaves no reader registered.
///
/// `snapshot_at` claims a slot *before* it validates, so the failure path has
/// to give it back. A leak here is invisible in every functional test and shows
/// up much later as "snapshot registry is full", or as a floor that never rises
/// because a phantom reader pins it — the reclamation bug this crate has already
/// recorded once under `reader-registry-pid-reuse`.
#[test]
fn a_refused_snapshot_releases_its_slot() {
    let dir = CleanDir::new("noleak");
    let db = Db::open(dir.path()).unwrap();
    let v1 = put(&db, 7, &[1]);
    put(&db, 7, &[2]);
    db.checkpoint().unwrap();
    assert!(
        db.snapshot_at(v1).is_err(),
        "the corpus must produce a refusal"
    );

    let before = db.live_readers();
    for _ in 0..64 {
        assert!(db.snapshot_at(v1).is_err());
    }
    assert_eq!(
        db.live_readers(),
        before,
        "a refused request kept its reader slot"
    );
    // And the registry is still usable, which `live_readers` alone would not
    // prove if the count and the slots ever disagreed.
    assert!(db.snapshot().is_ok());
}
