//! Byte-offset crash coverage for the MANIFEST update, over promotion.
//!
//! # What this exists to catch
//!
//! The manifest has two checksummed slots and its reader takes the valid one
//! with the higher `seq`, which is only worth anything if an *update* writes one
//! slot. Until 2026-09-06 it wrote both: every manifest write truncated the file
//! and put the same new image in slot A and slot B. The two slots then held two
//! copies of one generation, and a crash after the truncate but before the first
//! copy landed left no readable manifest at all — the database's identity, shard
//! count and routing map, gone, with nothing to fall back to.
//!
//! `promote_database` is the operation that ran that risk in the worst place: an
//! operator invokes it during a failover, under time pressure, on the node that
//! is about to become the only copy.
//!
//! # The property, and why it is stronger than "the write is atomic"
//!
//! **At every byte offset at which a crash could occur, the manifest still
//! reads, and reads as either the outgoing generation or the incoming one.**
//! Not "the update is all-or-nothing" — a single slot write is not atomic and
//! does not need to be. The old slot is untouched, and a partially written new
//! slot fails its CRC, so its higher `seq` is never observable.
//!
//! Two things are asserted, and the first is what makes the second meaningful:
//!
//! 1. **An update changes exactly one slot, and it alternates.** Checked on the
//!    real bytes an actual `promote_database` leaves behind. The truncating
//!    writer fails here immediately, because it rewrites both.
//! 2. **Every crash prefix of that one changed slot still reads.** Simulated by
//!    reconstructing the file with the first *k* bytes of the new slot in place
//!    for every *k*, which is legitimate precisely because ( 1 ) holds: nothing
//!    outside that region is in flight.
//!
//! The crash is simulated by writing the reconstructed bytes into a temporary
//! directory rather than by killing a process. A real kill cannot be aimed at a
//! byte offset, which is the whole point of the sweep.
//!
//! Only public API is used here — `promote_database`, `database_term`,
//! `database_uuid`, `Db::open_with` — so the assertions are about what a caller
//! can observe after a crash, not about the writer's internals.

use std::path::{Path, PathBuf};

use yesno_core::{database_term, database_uuid, promote_database, Db, DbOptions};

/// Both slots. Hard-coded rather than imported because `db::manifest` is
/// crate-private, and because a test that silently followed a change to the
/// geometry would stop testing the geometry.
const SLOT: usize = 4096;
const MANIFEST_BYTES: usize = SLOT * 2;

struct CleanDir(PathBuf);
impl Drop for CleanDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tmpdir(tag: &str) -> CleanDir {
    let mut p = std::env::temp_dir();
    p.push(format!("yesno-mancrash-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    CleanDir(p)
}

/// A database with something in it, closed. The manifest is then whatever
/// creation wrote, and the directory lock is free for `promote_database`.
fn make_db(dir: &Path) {
    let db = Db::open_with(
        dir,
        DbOptions {
            shards: 4,
            ..Default::default()
        },
    )
    .unwrap();
    db.insert(1, 7).unwrap();
    db.insert(2, 9).unwrap();
    db.checkpoint().unwrap();
    drop(db);
}

fn manifest_path(dir: &Path) -> PathBuf {
    dir.join("MANIFEST")
}

fn read_manifest(dir: &Path) -> Vec<u8> {
    std::fs::read(manifest_path(dir)).unwrap()
}

fn write_manifest_bytes(dir: &Path, bytes: &[u8]) {
    let p = manifest_path(dir);
    let _ = std::fs::remove_file(&p);
    std::fs::write(&p, bytes).unwrap();
}

/// Which 4 KiB slots differ between two manifest images.
fn changed_slots(before: &[u8], after: &[u8]) -> Vec<usize> {
    assert_eq!(before.len(), MANIFEST_BYTES, "manifest changed size");
    assert_eq!(after.len(), MANIFEST_BYTES, "manifest changed size");
    (0..2)
        .filter(|i| before[i * SLOT..(i + 1) * SLOT] != after[i * SLOT..(i + 1) * SLOT])
        .collect()
}

/// ( 1 ) — a promotion rewrites one slot, never both, and successive promotions
/// alternate.
///
/// This is the assertion a truncate-and-duplicate writer cannot satisfy: it
/// puts the new image in both slots, so `changed_slots` returns two, and the
/// slot that was live is gone along with the only fallback.
#[test]
fn a_promotion_rewrites_exactly_one_slot_and_they_alternate() {
    let dir = tmpdir("alternate");
    let dir = &dir.0;
    make_db(dir);

    let mut image = read_manifest(dir);
    assert_eq!(
        image.len(),
        MANIFEST_BYTES,
        "creation must write both slots — there is no prior generation to lose"
    );

    let mut touched = Vec::new();
    for term in 1..=6u32 {
        assert_eq!(promote_database(dir, term).unwrap(), term);
        let after = read_manifest(dir);
        let changed = changed_slots(&image, &after);
        assert_eq!(
            changed.len(),
            1,
            "promotion to term {term} rewrote slots {changed:?}; an update must \
             leave the live slot intact or a crash has nothing to fall back to"
        );
        touched.push(changed[0]);
        image = after;
    }

    // Creation leaves both slots at the same seq and the reader breaks that tie
    // towards A, so the first update goes to B and they alternate from there.
    assert_eq!(
        touched,
        vec![1, 0, 1, 0, 1, 0],
        "updates must alternate slots, not settle on one"
    );
    assert_eq!(database_term(dir).unwrap(), 6);
}

/// ( 2 ) — the sweep. Every crash point of the promotion's single slot write
/// leaves a manifest that reads, and reads as term 2 or term 3.
///
/// Term 1 must never come back. Two promotions precede the one under test, so
/// a writer that overwrote the *live* slot would leave the generation before it
/// as the survivor — a manifest that is valid but silently two behind, which is
/// a fence going backwards rather than a torn file.
#[test]
fn a_crash_at_any_byte_of_a_promotion_leaves_a_readable_manifest() {
    let dir = tmpdir("sweep");
    let dir = &dir.0;
    make_db(dir);
    let uuid = database_uuid(dir).unwrap();

    promote_database(dir, 1).unwrap();
    promote_database(dir, 2).unwrap();
    let before = read_manifest(dir);
    promote_database(dir, 3).unwrap();
    let after = read_manifest(dir);

    let changed = changed_slots(&before, &after);
    assert_eq!(changed.len(), 1, "the sweep is only valid over one slot");
    let off = changed[0] * SLOT;

    for k in 0..=SLOT {
        // The prefix of the new slot that reached the platter before the crash.
        let mut torn = before.clone();
        torn[off..off + k].copy_from_slice(&after[off..off + k]);
        check_crashed(dir, &torn, uuid, k, "prefix");

        // A torn sector is not obliged to be a clean prefix; the tail can hold
        // anything. The CRC has to be what rejects it, not the shape.
        for fill in [0x00u8, 0xFF, 0x5A] {
            let mut junk = torn.clone();
            junk[off + k..off + SLOT].fill(fill);
            check_crashed(dir, &junk, uuid, k, "garbage tail");
        }
    }

    // The endpoints pin the direction: nothing written is still term 2, and
    // everything written is term 3.
    write_manifest_bytes(dir, &before);
    assert_eq!(database_term(dir).unwrap(), 2);
    write_manifest_bytes(dir, &after);
    assert_eq!(database_term(dir).unwrap(), 3);

    // And the database still opens on a crashed-mid-promotion manifest, which is
    // the thing an operator actually needs at 03:00.
    let mut half = before.clone();
    half[off..off + SLOT / 2].copy_from_slice(&after[off..off + SLOT / 2]);
    write_manifest_bytes(dir, &half);
    let db = Db::open_with(
        dir,
        DbOptions {
            shards: 4,
            ..Default::default()
        },
    )
    .unwrap();
    let snap = db.snapshot().unwrap();
    assert!(snap.contains(1, 7).unwrap());
    assert!(snap.contains(2, 9).unwrap());
}

fn check_crashed(dir: &Path, bytes: &[u8], uuid: [u8; 16], k: usize, shape: &str) {
    write_manifest_bytes(dir, bytes);
    let term = database_term(dir)
        .unwrap_or_else(|e| panic!("no readable manifest at k={k} ( {shape} ): {e:?}"));
    assert!(
        term == 2 || term == 3,
        "k={k} ( {shape} ) resurrected term {term}; only the outgoing and \
         incoming generations are permissible outcomes"
    );
    assert_eq!(
        database_uuid(dir).unwrap(),
        uuid,
        "k={k} ( {shape} ) lost the database identity"
    );
}
