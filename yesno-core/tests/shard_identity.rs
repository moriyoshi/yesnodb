//! Two shard images of one database must not be interchangeable.
//!
//! # The hole this closes
//!
//! `db_uuid` is what refuses a shard file from *another* database, and it was
//! itself uncompared until 2026-08-28. It cannot refuse a shard file from
//! **this** database: every shard of a database carries the same uuid by
//! construction, so exchanging `shard-0000.yno` with `shard-0001.yno` — a `mv`
//! in a restore script, a rsync that raced, a backup reassembled in the wrong
//! order — left every check satisfied. The database opened, and each shard then
//! answered its keys out of the other shard's extents: no error, no corruption
//! report, wrong results.
//!
//! The physical shard number is in every superblock and the shard number is in
//! the filename. Comparing them is the whole fix.
//!
//! # Two layers, and why both assertions are here
//!
//! The refusal is asserted twice on purpose. `check_identity` is asserted
//! against the decoded superblock, which is the layer that owns the comparison
//! and pins its two error cases apart. `Db::open` is asserted separately,
//! because a check nothing calls refuses nothing — and that is precisely the
//! shape this repository has been bitten by repeatedly ( `check_alignment` had
//! no caller; `validate` ran only from `fsck` ). The unit-level assertion would
//! stay green if the wiring in the shard store's open paths were removed, so it
//! is not evidence on its own.
//!
//! Every assertion below is paired with its pre-swap counterpart, so the file
//! cannot pass by refusing everything.

use std::path::PathBuf;

use yesno_core::store::superblock::{self, SuperBlock};
use yesno_core::store::PAGE;
use yesno_core::{CodecError, Db, DbOptions};

struct CleanDir(PathBuf);
impl Drop for CleanDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tmpdir(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("yesno-shardid-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    p
}

/// The live superblock of one shard image on disk.
fn superblock_of(dir: &std::path::Path, shard: u32) -> SuperBlock {
    let bytes = std::fs::read(dir.join(format!("shard-{shard:04}.yno"))).unwrap();
    assert!(bytes.len() >= 2 * PAGE, "shard image is too short");
    superblock::pick(&bytes[..PAGE], &bytes[PAGE..2 * PAGE])
        .unwrap()
        .expect("both superblock slots unreadable")
}

/// Build a two-shard database with real checkpointed content in both shards.
fn two_shard_db(dir: &std::path::Path) {
    let db = Db::open_with(
        dir,
        DbOptions {
            shards: 2,
            ..Default::default()
        },
    )
    .unwrap();
    // Enough keys that both shards certainly hold chunks, and enough ordinals
    // per key that they are stored as extents rather than inline in the leaf.
    for key in 0..64u64 {
        let vals: Vec<u64> = (0..500u64).map(|i| key * 1_000_000 + i * 7).collect();
        db.insert_many(key, &vals).unwrap();
    }
    db.checkpoint().unwrap();
    drop(db);
}

#[test]
fn swapping_two_shard_images_of_one_database_is_refused() {
    let dir = tmpdir("swap");
    let _clean = CleanDir(dir.clone());
    two_shard_db(&dir);

    let uuid = yesno_core::database_uuid(&dir).unwrap();

    // Non-vacuity: in their own places, both images are accepted, and their
    // uuids really are equal — which is exactly why the uuid cannot refuse the
    // swap and `shard_id` has to.
    let sb0 = superblock_of(&dir, 0);
    let sb1 = superblock_of(&dir, 1);
    assert_eq!(sb0.check_identity(uuid, 0), Ok(()));
    assert_eq!(sb1.check_identity(uuid, 1), Ok(()));
    assert_eq!(
        sb0.db_uuid, sb1.db_uuid,
        "sibling shards share a database identity; if this ever differs, this \
         test is no longer testing the swap it was written for"
    );
    assert_ne!(
        sb0.shard_id, sb1.shard_id,
        "the two images must be distinguishable at all, or nothing can refuse them"
    );

    // The swap.
    let a = dir.join("shard-0000.yno");
    let b = dir.join("shard-0001.yno");
    let tmp = dir.join("shard-swap.tmp");
    std::fs::rename(&a, &tmp).unwrap();
    std::fs::rename(&b, &a).unwrap();
    std::fs::rename(&tmp, &b).unwrap();

    // Each file now holds the other shard's image, and each is refused for the
    // shard its filename names.
    assert_eq!(
        superblock_of(&dir, 0).check_identity(uuid, 0),
        Err(CodecError::ShardIdentityMismatch {
            expected: 0,
            found: 1
        }),
        "shard 1's image sitting at shard-0000.yno must be refused"
    );
    assert_eq!(
        superblock_of(&dir, 1).check_identity(uuid, 1),
        Err(CodecError::ShardIdentityMismatch {
            expected: 1,
            found: 0
        }),
        "shard 0's image sitting at shard-0001.yno must be refused"
    );

    // And the check must actually be reached. The two assertions above hold
    // whether or not anything calls `check_identity`; this one is what fails if
    // the shard store stops asking.
    assert!(
        yesno_core::Db::open(&dir).is_err(),
        "a database whose two shard images have been exchanged must not open"
    );
}

/// A shard image from a *different* database is still refused, and is reported
/// as a database mismatch rather than as a shard-number one.
///
/// The two checks are independent: shard 1 of database X presented as shard 1
/// of database Y matches on number and differs on identity.
#[test]
fn a_shard_image_from_another_database_is_still_refused() {
    let dir_x = tmpdir("dbx");
    let _cx = CleanDir(dir_x.clone());
    let dir_y = tmpdir("dby");
    let _cy = CleanDir(dir_y.clone());
    two_shard_db(&dir_x);
    two_shard_db(&dir_y);

    let uuid_x = yesno_core::database_uuid(&dir_x).unwrap();
    let uuid_y = yesno_core::database_uuid(&dir_y).unwrap();
    assert_ne!(
        uuid_x, uuid_y,
        "two fresh databases must not share identity"
    );

    let foreign = superblock_of(&dir_y, 1);
    assert_eq!(foreign.check_identity(uuid_y, 1), Ok(()));
    assert_eq!(
        foreign.check_identity(uuid_x, 1),
        Err(CodecError::DatabaseIdentityMismatch),
        "the shard number matches; only the database identity separates these"
    );
}

/// Every shard image a real database writes carries its own number.
///
/// If they did not — if the number were only written for shard 0, say — the
/// refusal above would be enforcing an accident.
#[test]
fn every_shard_image_records_its_own_physical_number() {
    let dir = tmpdir("numbers");
    let _clean = CleanDir(dir.clone());
    let db = Db::open_with(
        dir.clone(),
        DbOptions {
            shards: 6,
            ..Default::default()
        },
    )
    .unwrap();
    for key in 0..64u64 {
        db.insert_many(
            key,
            &[key * 100, key * 100 + 1, key * 100 + 2, key * 100 + 3],
        )
        .unwrap();
    }
    db.checkpoint().unwrap();
    drop(db);

    let uuid = yesno_core::database_uuid(&dir).unwrap();
    for shard in 0..6u32 {
        let sb = superblock_of(&dir, shard);
        assert_eq!(
            sb.shard_id, shard,
            "shard-{shard:04}.yno must say it is shard {shard}"
        );
        assert_eq!(sb.check_identity(uuid, shard), Ok(()));
    }
}
