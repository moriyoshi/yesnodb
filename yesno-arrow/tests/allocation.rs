//! The M5 gate, asserted rather than described.
//!
//! The milestone gate reads *"Zero-copy round-trip asserted ( no allocation in
//! the bitmap arm )"*, and the design is more specific still: *"`masks()` over
//! an already-bitmap stream must allocate **zero**."*
//!
//! Nothing measured it. `is_zero_copy(c)` is `bitmap_mask(c).is_some()` — a
//! statement about the container *kind*, not evidence that the mask path
//! avoided a copy — and `a_mask_slice_is_a_view_not_a_copy` asserts the slice's
//! *values*, which a copy satisfies just as well as a view does.
//!
//! That is the same trap `yesno-core/tests/allocation.rs` exists to close: the
//! zero-copy path and the copying path return identical masks, so only a count
//! of allocations can tell them apart.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;

use yesno_arrow::masks::MaskStream;
use yesno_core::stream::SetStream;
use yesno_core::OrdSet;

// Thread-local, not global: a process-wide counter would be polluted by sibling
// test threads and would only pass under `--test-threads=1`.
thread_local! {
    static ALLOCS: Cell<u64> = const { Cell::new(0) };
    static COUNTING: Cell<bool> = const { Cell::new(false) };
}

struct Counting;

#[inline]
fn bump() {
    let _ = COUNTING.try_with(|on| {
        if on.get() {
            let _ = ALLOCS.try_with(|c| c.set(c.get() + 1));
        }
    });
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        bump();
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        bump();
        unsafe { System.realloc(p, l, new) }
    }
}

#[global_allocator]
static A: Counting = Counting;

fn count_allocs<T>(f: impl FnOnce() -> T) -> (T, u64) {
    ALLOCS.with(|c| c.set(0));
    COUNTING.with(|c| c.set(true));
    let out = f();
    COUNTING.with(|c| c.set(false));
    (out, ALLOCS.with(|c| c.get()))
}

/// A set of `n` chunks, every one dense enough to be stored as a bitmap.
fn bitmap_set(n_chunks: u64) -> Arc<OrdSet> {
    let vals: Vec<u64> = (0..n_chunks)
        .flat_map(|c| (0..20_000u64).map(move |i| (c << 16) | (i * 3)))
        .collect();
    let mut s = OrdSet::from_sorted_slice(&vals);
    s.optimize();
    Arc::new(s)
}

/// Streaming masks over bitmap chunks must not scale allocations with chunks.
#[test]
fn masks_over_a_bitmap_stream_do_not_allocate_per_chunk() {
    let few = bitmap_set(4);
    let many = bitmap_set(64);
    assert_eq!(
        many.chunk_count(),
        64,
        "the corpus must really span 64 chunks"
    );

    // Warm up any lazily-initialized machinery.
    let _ = MaskStream::new(few.stream()).count();

    let (n_few, a_few) = count_allocs(|| MaskStream::new(few.stream()).count());
    let (n_many, a_many) = count_allocs(|| MaskStream::new(many.stream()).count());
    assert_eq!((n_few, n_many), (4, 64), "every chunk must yield a mask");

    // 16x the chunks must not mean materially more allocation. A copying arm
    // would allocate an 8 KiB buffer per chunk and this would be ~16x.
    assert!(
        a_many <= a_few + 8,
        "masking 64 bitmap chunks allocated {a_many} times against {a_few} for 4 - \
         the bitmap arm is copying, not lending"
    );
}

/// Slicing a mask must produce a view, which only an allocation count shows.
#[test]
fn slicing_a_mask_allocates_nothing() {
    let set = bitmap_set(1);
    let m = MaskStream::new(set.stream()).next().unwrap().unwrap();

    let (len, allocs) = count_allocs(|| m.slice(64, 128).len());
    assert_eq!(len, 128);
    assert_eq!(
        allocs, 0,
        "slicing a mask allocated {allocs} times - it is copying, not viewing"
    );
}

/// The whole point: a bitmap chunk's mask must borrow the container's bits.
///
/// **This test used to measure nothing, and the obvious repair did not fix
/// it either.** It built the mask *outside* `count_allocs` and then counted
/// allocations while calling `m.mask.len()` — a `usize` read, which cannot
/// allocate under any implementation. Moving the construction inside the
/// counted region still did not catch it: a copy of the 8 KiB payload is one or
/// two allocations, which any threshold loose enough to tolerate the stream's
/// own setup will swallow.
///
/// Verified both times on 2026-08-26 by making `BitStore::to_boolean_buffer`
/// copy instead of sharing.
///
/// So the assertion is **structural, not statistical**: the mask's bytes must be
/// *the same memory* as the container's. That is what "is the container's own
/// buffer" means, it is what the zero-copy design promises, and a pointer
/// comparison cannot be tuned away by a threshold. Allocation counting is the
/// right instrument for "does this scale per chunk" — which is the sibling test
/// — and the wrong one for "is this the same object".
#[test]
fn a_bitmap_chunks_mask_is_the_containers_own_buffer() {
    let set = bitmap_set(1);
    let m = MaskStream::new(set.stream()).next().unwrap().unwrap();

    let (_, container) = set.chunk_at(0).expect("one chunk");
    let lent = yesno_core::unstable_arrow::bitmap_mask(container)
        .expect("the fixture is a bitmap container");

    assert_eq!(m.mask.len(), 65_536);
    assert_eq!(
        m.mask.values().as_ptr(),
        lent.values().as_ptr(),
        "a bitmap chunk's mask does not point at the container's own bytes —          the bitmap arm is copying, not lending"
    );
}

/// A batch must cost a batch, not a container.
///
/// # What this pins
///
/// The design says never fully decode a bitmap before slicing. A reader that
/// collects `c.iter()` into a `Vec<u64>` and then hands out 8 192-row slices of
/// it allocates **512 KiB to emit the first 64 KiB batch** — and every later
/// batch from that chunk reads out of a buffer that was already built whole.
/// `Container::fill_from` resumes into the container instead, so peak extra
/// memory is one batch.
///
/// Invisible to every correctness test in the crate: both readers emit the
/// same ordinals in the same order in the same batch sizes. The observable is
/// the *shape* of the allocation — bytes per batch against a full chunk's worth
/// — which is why this counts rather than asserts.
#[test]
fn a_batch_costs_a_batch_not_a_whole_container() {
    use yesno_arrow::{BatchPolicy, OrdinalBatchReader};

    // One dense chunk: 65 536 ordinals, so a materializing reader builds a
    // 512 KiB `Vec<u64>` before it can emit anything.
    let set = Arc::new(OrdSet::from_sorted_slice(
        &(0..65_536u64).collect::<Vec<u64>>(),
    ));
    let rows = 8_192usize;

    let take_one = |policy: BatchPolicy| {
        let mut r = OrdinalBatchReader::with_policy(SetStream::new(set.clone()), policy);
        let (b, allocs) = count_allocs(|| r.next().map(|b| b.unwrap().num_rows()));
        (b, allocs)
    };

    let (rows_got, allocs) = take_one(BatchPolicy {
        target_rows: rows,
        chunk_aligned: false,
    });
    assert_eq!(rows_got, Some(rows), "the batch itself must still be right");

    // A `Vec<u64>` grown to 65 536 by `collect` is ~17 reallocations on top of
    // the batch's own; the batch alone is a small handful. Ten is loose enough
    // not to be brittle and tight enough that materializing the chunk fails it.
    assert!(
        allocs < 10,
        "{allocs} allocations to emit one {rows}-row batch — the container is \
         being decoded whole before it is sliced"
    );

    // And the whole stream still reproduces the set, batch by batch.
    let r = OrdinalBatchReader::with_policy(
        SetStream::new(set.clone()),
        BatchPolicy {
            target_rows: rows,
            chunk_aligned: false,
        },
    );
    let mut all: Vec<u64> = Vec::new();
    for b in r {
        let b = b.unwrap();
        let a = b
            .column(0)
            .as_any()
            .downcast_ref::<arrow_array::UInt64Array>()
            .unwrap();
        all.extend(a.values().iter().copied());
    }
    assert_eq!(all, (0..65_536u64).collect::<Vec<u64>>());
}
