//! Lending a contiguous run of bitmap payloads as one Arrow buffer.
//!
//! # Why a test can see this at all
//!
//! A borrowed buffer and a copied one hold the same bytes, so no assertion about
//! *contents* can tell them apart -- the same trap `tests/allocation.rs` exists for. What
//! separates them is the **address**: a lent buffer starts where the mapping does. That is
//! what these assert, and it is why they would fail against a `dense_span` that gathered.
//!
//! The capability arrived with the 2026-09-30 slot change. While a standalone bitmap sat in
//! an 8256-byte slot, consecutive payloads were 64 bytes apart and no window was ever
//! contiguous, so this file could not have been written.

use std::collections::BTreeSet;
use std::path::PathBuf;

use yesno_core::unstable_arrow::{bitmap_words, dense_span};
use yesno_core::{Db, DbOptions, BITMAP_BYTES, CHUNK_CARD};

struct CleanDir(PathBuf);
impl Drop for CleanDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn dir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "yesno-densespan-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&p);
    p
}

/// Every other ordinal of `chunks` consecutive chunks: dense enough that each chunk must be
/// a bitmap, and not a run, so the encoder has no other choice.
fn dense(chunks: u64) -> Vec<u64> {
    (0..chunks * CHUNK_CARD as u64)
        .filter(|o| o % 2 == 0)
        .collect()
}

#[test]
fn a_contiguous_window_is_lent_and_not_copied() {
    let d = dir("lent");
    let _c = CleanDir(d.clone());
    let chunks = 8u64;

    let db = Db::open_with(&d, DbOptions::default()).unwrap();
    db.insert_many(1, &dense(chunks)).unwrap();
    // Checkpointed, because only a store-backed payload can be lent at all -- a
    // memtable-resident one has no mapping behind it.
    db.checkpoint().unwrap();

    let snap = db.snapshot().unwrap();
    let (span, covered) =
        dense_span(&snap, 1, 0, chunks).expect("a fully dense window must be lendable");
    assert_eq!(covered, chunks, "a window inside one slab is covered whole");
    assert_eq!(
        span.len(),
        chunks as usize * BITMAP_BYTES,
        "the span covers every chunk's payload and nothing else"
    );

    // **The assertion that distinguishes a lend from a copy.** The container the ordinary
    // read path returns already aliases the mapping, so the span must begin at the same
    // address; a gathered buffer would be somewhere else entirely.
    let set = snap.load(1).unwrap();
    let (_, first) = set.chunks().next().expect("the key has chunks");
    let words = bitmap_words(first).expect("a checkpointed bitmap lends its words");
    assert_eq!(
        span.as_ptr(),
        words.as_ptr() as *const u8,
        "dense_span copied instead of lending"
    );

    // And the bytes really are the set, read the way a consumer would: bit j of chunk i is
    // ordinal i * CHUNK_CARD + j.
    let want: BTreeSet<u64> = dense(chunks).into_iter().collect();
    let mut got = BTreeSet::new();
    for (byte, &v) in span.as_slice().iter().enumerate() {
        for bit in 0..8u32 {
            if v & (1 << bit) != 0 {
                got.insert(byte as u64 * 8 + bit as u64);
            }
        }
    }
    assert_eq!(got, want, "the lent bytes are not the set that went in");
}

/// Each refusal, because `None` is what keeps a wrong answer off the fast path.
#[test]
fn a_gap_stops_the_run_rather_than_refusing_it() {
    let d = dir("gap");
    let _c = CleanDir(d.clone());

    let db = Db::open_with(&d, DbOptions::default()).unwrap();
    // Chunks 0 and 2 dense, chunk 1 empty: a hole has no stride, so bit arithmetic over the
    // window would silently shift every position after it.
    let mut vals: Vec<u64> = (0..CHUNK_CARD as u64).filter(|o| o % 2 == 0).collect();
    vals.extend((2 * CHUNK_CARD as u64..3 * CHUNK_CARD as u64).filter(|o| o % 2 == 0));
    db.insert_many(1, &vals).unwrap();
    db.checkpoint().unwrap();

    let snap = db.snapshot().unwrap();
    // **Stopped at, not refused.** Chunk 0 is lendable and the run ends at the hole, so a
    // caller borrows what is there and asks again past it -- which is what keeps a key with
    // one interior gap from gathering everything after it.
    let (span, covered) = dense_span(&snap, 1, 0, 3).expect("the dense prefix is lendable");
    assert_eq!(covered, 1, "the run must stop at the gap, not span it");
    assert_eq!(span.len(), BITMAP_BYTES);
    // Asking from inside the hole yields nothing, since not one chunk qualifies.
    assert!(dense_span(&snap, 1, 1, 3).is_none());
    // And past it, the far side is lendable on its own.
    let (_, covered) = dense_span(&snap, 1, 2, 3).expect("chunk 2 is lendable");
    assert_eq!(covered, 3);
}

#[test]
fn a_memtable_override_is_refused() {
    let d = dir("overlay");
    let _c = CleanDir(d.clone());
    let chunks = 4u64;

    let db = Db::open_with(&d, DbOptions::default()).unwrap();
    db.insert_many(1, &dense(chunks)).unwrap();
    db.checkpoint().unwrap();
    assert!(
        dense_span(&db.snapshot().unwrap(), 1, 0, chunks).is_some(),
        "the window is lendable before the overlay"
    );

    // One ordinal the disk copy does not have, uncommitted to the store. Lending the disk
    // bytes now would serve a set that is missing it.
    db.insert_many(1, &[1]).unwrap();
    let snap = db.snapshot().unwrap();
    assert!(
        dense_span(&snap, 1, 0, chunks).is_none(),
        "a memtable opinion anywhere in the window must be refused"
    );
    // And the ordinary path still sees the newer value, which is what makes the refusal
    // necessary rather than merely cautious.
    assert!(snap.load(1).unwrap().contains(1));
}

#[test]
fn a_sparse_chunk_stops_the_run() {
    let d = dir("sparse");
    let _c = CleanDir(d.clone());

    let db = Db::open_with(&d, DbOptions::default()).unwrap();
    // Chunk 0 dense, chunk 1 holding three ordinals: an array, whose payload is neither
    // 8192 bytes nor at the stride the window's arithmetic assumes.
    let mut vals: Vec<u64> = (0..CHUNK_CARD as u64).filter(|o| o % 2 == 0).collect();
    vals.extend([
        CHUNK_CARD as u64,
        CHUNK_CARD as u64 + 5,
        CHUNK_CARD as u64 + 9,
    ]);
    db.insert_many(1, &vals).unwrap();
    db.checkpoint().unwrap();

    let snap = db.snapshot().unwrap();
    // The bitmap prefix is lendable and the run stops at the array chunk.
    let (_, covered) = dense_span(&snap, 1, 0, 2).expect("chunk 0 is a lendable bitmap");
    assert_eq!(covered, 1, "the run must stop at the non-bitmap chunk");
}

/// A key spanning a slab boundary is lent **in full**, across several calls.
///
/// This is the regression test for the defect the partial-coverage contract exists to fix.
/// A slab body holds 254 chunks of the bitmap class, so a fixed window straddles a boundary
/// roughly every fourth time; while a straddle was reported as a refusal, a caller stopped
/// borrowing at the first one and gathered every later slab even though each is contiguous
/// within itself.
///
/// The second assertion is what stops this passing vacuously: if no call ever came back
/// short, the fixture never reached a boundary and the test proves nothing about it.
#[test]
fn a_key_spanning_a_slab_boundary_is_lent_in_full() {
    let d = dir("boundary");
    let _c = CleanDir(d.clone());
    // Comfortably past one slab body's 254 chunks.
    let chunks = 300u64;

    let db = Db::open_with(&d, DbOptions::default()).unwrap();
    db.insert_many(1, &dense(chunks)).unwrap();
    db.checkpoint().unwrap();
    let snap = db.snapshot().unwrap();

    let window = 64u64;
    let mut at = 0u64;
    let mut short_answers = 0;
    let mut bytes = 0usize;
    while at < chunks {
        let end = (at + window).min(chunks);
        let (span, covered) =
            dense_span(&snap, 1, at, end).unwrap_or_else(|| panic!("nothing lendable at {at}"));
        assert!(covered > at, "a lend must make progress");
        assert_eq!(
            span.len(),
            (covered - at) as usize * BITMAP_BYTES,
            "the buffer must cover exactly the chunks reported"
        );
        if covered < end {
            short_answers += 1;
        }
        bytes += span.len();
        at = covered;
    }
    assert_eq!(
        bytes,
        chunks as usize * BITMAP_BYTES,
        "every chunk must be lent, across however many calls it takes"
    );
    assert!(
        short_answers > 0,
        "the fixture never crossed a slab boundary, so it does not test one"
    );
}
