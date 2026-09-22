//! Allocation regression tests.
//!
//! The non-materializing cardinality walk is a *parallel implementation* of the
//! materializing one. Correctness tests cannot tell them apart — both return the
//! right number — so the only thing stopping `cardinality()` from silently
//! decaying into `collect_set().len()` is a test that counts allocations.
//!
//! These are asserted as **tests, not benchmarks**, because a benchmark
//! regression gets triaged next quarter and a failing test gets fixed today.
//! A counted-source work budget below also checks payload reads: an allocation
//! counter cannot reliably see a zero-copy source consuming chunks it should
//! have sought past.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;

use yesno_core::stream::ChunkStreamExt;
use yesno_core::{Expr, OrdSet};

// Counters are THREAD-LOCAL, not global. A process-wide counter would be
// polluted by sibling test threads, making these tests pass only under
// `--test-threads=1` — a requirement that is easy to state in a comment and
// easy to forget on CI. Const-initialized `Cell`s do not allocate on first
// touch, so there is no recursion into the allocator here.
thread_local! {
    static ALLOCS: Cell<u64> = const { Cell::new(0) };
    static BYTES: Cell<u64> = const { Cell::new(0) };
    static COUNTING: Cell<bool> = const { Cell::new(false) };
}

struct Counting;

#[inline]
fn bump(bytes: usize) {
    // `try_with` rather than `with`: during thread teardown the TLS may already
    // be destroyed, and panicking inside the allocator would abort.
    let _ = COUNTING.try_with(|on| {
        if on.get() {
            let _ = ALLOCS.try_with(|c| c.set(c.get() + 1));
            let _ = BYTES.try_with(|c| c.set(c.get().saturating_add(bytes as u64)));
        }
    });
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        bump(l.size());
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        bump(new);
        unsafe { System.realloc(p, l, new) }
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

/// Count allocations made by *this thread* during `f`.
fn count_allocs<T>(f: impl FnOnce() -> T) -> (T, u64) {
    ALLOCS.with(|c| c.set(0));
    COUNTING.with(|c| c.set(true));
    let out = f();
    COUNTING.with(|c| c.set(false));
    (out, ALLOCS.with(|c| c.get()))
}

/// Count total requested allocation bytes made by this thread during `f`.
///
/// This is cumulative requested memory, not live or peak memory. It sees the
/// capacity of a metadata plan even when that plan is later filtered or freed.
fn count_alloc_bytes<T>(f: impl FnOnce() -> T) -> (T, u64) {
    BYTES.with(|c| c.set(0));
    COUNTING.with(|c| c.set(true));
    let out = f();
    COUNTING.with(|c| c.set(false));
    (out, BYTES.with(|c| c.get()))
}

/// Sets spanning many chunks, so "per chunk" and "constant" differ by a lot.
fn wide_sets() -> (Arc<OrdSet>, Arc<OrdSet>, Arc<OrdSet>) {
    let n_chunks = 2000u64;
    let mk = |stride: u64| {
        let vals: Vec<u64> = (0..n_chunks)
            .flat_map(|c| (0..50u64).map(move |i| (c << 16) | (i * stride)))
            .collect();
        Arc::new(OrdSet::from_sorted_slice(&vals))
    };
    (mk(3), mk(5), mk(7))
}

#[test]
fn cardinality_of_a_nested_expression_is_allocation_bounded() {
    let (a, b, c) = wide_sets();
    assert_eq!(
        a.chunk_count(),
        2000,
        "the test needs many chunks to be meaningful"
    );

    // Warm up: first call may allocate lazily-initialized machinery.
    let _ = Expr::set(a.clone())
        .and(Expr::set(b.clone()).or(Expr::set(c.clone())))
        .cardinality()
        .unwrap();

    let (n, allocs) = count_allocs(|| {
        Expr::set(a.clone())
            .and(Expr::set(b.clone()).or(Expr::set(c.clone())))
            .cardinality()
            .unwrap()
    });
    assert!(n > 0, "expression should be non-empty");

    // The OR arm must build one merged container per shared prefix so that AND
    // has something to intersect against — that is inherent to this expression
    // shape, not a defect. Everything else should be free.
    //
    // Measured baseline: **1.00 allocations per chunk**, i.e. exactly the one
    // inherent OR container. Getting here required three fixes, each of which
    // this test would catch a regression of:
    //   - freezing containers so single-sided pass-through clones by refcount
    //     rather than copying (was +2/chunk),
    //   - pre-sizing the merge output so it does not grow by doubling (+5/chunk),
    //   - `from_sorted_vec` taking ownership instead of copying (+1/chunk).
    // The threshold leaves headroom but still fails if any of them regress.
    let per_chunk = allocs as f64 / a.chunk_count() as f64;
    assert!(
        per_chunk < 1.5,
        "cardinality() allocated {allocs} times for {} chunks ({per_chunk:.2}/chunk, \
         baseline 1.00); it is materializing rather than using the cardinality identities",
        a.chunk_count()
    );

    // And it must be strictly cheaper than the materializing path.
    let (_, collect_allocs) = count_allocs(|| {
        Expr::set(a.clone())
            .and(Expr::set(b.clone()).or(Expr::set(c.clone())))
            .collect_set()
            .unwrap()
            .len()
    });
    assert!(
        allocs < collect_allocs,
        "cardinality() ({allocs}) should allocate less than collect_set() ({collect_allocs})"
    );
}

#[test]
fn and_cardinality_of_two_leaves_allocates_almost_nothing() {
    let (a, b, _) = wide_sets();

    // Warm up.
    let _ = a.stream().and(b.stream()).cardinality().unwrap();

    let (n, allocs) = count_allocs(|| a.stream().and(b.stream()).cardinality().unwrap());
    assert!(n > 0);

    // AND between two leaves touches `ops::and_cardinality`, which is
    // documented as non-allocating: a counting merge, no output container.
    // Only the two SetStream Arc clones and the boxed nothing remain.
    assert!(
        allocs < 32,
        "and_cardinality path allocated {allocs} times over {} chunks; \
         expected O(1) — the non-allocating kernel is not being used",
        a.chunk_count()
    );
}

#[test]
fn is_disjoint_short_circuits_without_scanning() {
    // Disjoint in the very first chunk: `is_disjoint` must stop, not walk both.
    let a = Arc::new(OrdSet::from_sorted_slice(
        &(0..2000u64).map(|c| c << 16).collect::<Vec<_>>(),
    ));
    let b = Arc::new(OrdSet::from_sorted_slice(
        &(0..2000u64).map(|c| (c << 16) | 1).collect::<Vec<_>>(),
    ));

    let _ = a.is_disjoint(&b);
    let (disjoint, allocs) = count_allocs(|| a.is_disjoint(&b));
    assert!(disjoint, "the two sets share no ordinals");
    assert!(
        allocs < 16,
        "is_disjoint allocated {allocs} times; it should never build a container"
    );
}

#[test]
fn set_level_cardinality_identities_do_not_materialize() {
    let (a, b, _) = wide_sets();

    let _ = a.or_cardinality(&b);
    let (n, allocs) = count_allocs(|| a.or_cardinality(&b));
    assert_eq!(n, a.or(&b).len());
    assert!(
        allocs < 16,
        "or_cardinality allocated {allocs} times; the identity \
         |A ∪ B| = |A| + |B| − |A ∩ B| should build nothing"
    );
}

/// A streaming k-way union must count without building a container per prefix.
///
/// This is the operator the required-`cardinality` rule was written for. The
/// default body materializes a result **and** runs `optimize()` on it to pick
/// an encoding, per prefix, only to read `len()` off it and drop it. With 2000
/// chunks and eight contributors that is 2000 containers built for a number
/// that the accumulator's popcount already has.
#[test]
fn a_streaming_k_way_union_counts_without_materializing() {
    use yesno_core::stream::nary::UnionAll;
    use yesno_core::stream::ChunkStream;

    let (a, b, c) = wide_sets();
    let build = || {
        let streams: Vec<Box<dyn ChunkStream>> = (0..8)
            .map(|i| {
                let s = match i % 3 {
                    0 => a.clone(),
                    1 => b.clone(),
                    _ => c.clone(),
                };
                Box::new(s.stream()) as Box<dyn ChunkStream>
            })
            .collect();
        UnionAll::new(streams)
    };

    // Warm up the lazily-allocated accumulator.
    let _ = build().cardinality_dyn().unwrap();

    let (n, allocs) = count_allocs(|| build().cardinality_dyn().unwrap());
    assert!(n > 0, "the union should be non-empty");

    // Eight boxed streams and their cursors are set-up cost; what must not
    // appear is anything scaling with the 2000 chunks.
    assert!(
        allocs < 200,
        "k-way union cardinality allocated {allocs} times over 2000 chunks - \
         it is materializing a container per prefix"
    );

    // And the number must still be right.
    let eager = OrdSet::union_all(&[&a, &b, &c]);
    assert_eq!(
        n,
        eager.len(),
        "streaming union cardinality disagrees with eager"
    );
}

/// `Snapshot::cardinality` must be answered from the index, not by
/// materializing every chunk.
///
/// `ChunkRef` carries `card_m1` precisely so that `len(key)` and `is_empty(key)`
/// are answerable from an index range scan alone, never touching a payload
/// extent. The README, `ARCHITECTURE.md` and `ChunkRef::cardinality`'s own doc
/// all state it as a headline property — and it was not implemented:
/// `Snapshot::cardinality` called `merged_chunks`, which decodes every
/// container and then sums `len()`.
///
/// Correctness tests cannot see the difference, because both return the right
/// number. Only an allocation count can.
///
/// The database is **reopened** before reading, so the answer has to come off
/// disk rather than out of the memtable — see the header of `durability.rs`.
#[test]
fn cardinality_is_answered_from_the_index_not_by_materializing() {
    use yesno_core::{Db, DbOptions};

    let dir = std::env::temp_dir().join(format!("yesno-alloc-card-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let chunks = 500u64;
    let opts = || DbOptions {
        shards: 1,
        ..Default::default()
    };
    {
        let db = Db::open_with(&dir, opts()).unwrap();
        // One ordinal per chunk keeps each container tiny, so any per-chunk
        // cost is unmistakably per-chunk rather than per-byte.
        let vals: Vec<u64> = (0..chunks).map(|c| (c << 16) | 7).collect();
        db.insert_many(1, &vals).unwrap();
        db.checkpoint().unwrap();
    }

    let db = Db::open_with(&dir, opts()).unwrap();
    let snap = db.snapshot().unwrap();
    let (n, allocs) = count_allocs(|| snap.cardinality(1).unwrap());
    assert_eq!(n, chunks, "the count itself must still be right");

    // Materializing is at least one container per chunk; it measured 2 129
    // before this was wired to the index, and 122 after. The budget is half the
    // chunk count — loose enough not to be brittle, tight enough that anything
    // per-chunk fails it.
    //
    // Most of the remaining cost is the `BTreeMap` of per-chunk
    // cardinalities, which is inherent to merging the memtable over the scan.
    // Do not raise this budget to accommodate a change; the whole point is
    // that the cost must not scale with chunk count.
    assert!(
        allocs < chunks / 2,
        "cardinality made {allocs} allocations for {chunks} chunks — it is \
         materializing containers rather than reading `card_m1`"
    );

    drop(snap);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A chain of three or more `Or`s must go through the n-ary accumulator, not
/// fold pairwise.
///
/// `stream::nary::UnionAll` exists because folding materializes an intermediate
/// container per chunk per fold, while the accumulator reuses one 8 KiB scratch
/// across the whole walk. It was written, unit-tested, and reachable from no
/// public entry point: `Expr::open` folded `Or` strictly pairwise, so the path
/// the design specifies for "three or more members" was unreachable from any
/// query.
///
/// Measured over 400 chunks before wiring it up — allocations, then speedup:
/// k=2 `5 -> 4` (1.6x), k=3 `409 -> 6` (3.4x), k=4 `813 -> 7` (4.4x),
/// k=8 `2429 -> 11` (7.9x), k=16 `5661 -> 19` (10.1x). Hence the threshold of
/// three: at two the pairwise kernel is within noise.
///
/// Correctness tests cannot see this — both paths return the same set — which
/// is why it is asserted here.
#[test]
fn a_chain_of_ors_uses_the_nary_accumulator_not_a_pairwise_fold() {
    let n_chunks = 400u64;
    let k = 8usize;
    let sets: Vec<Arc<OrdSet>> = (0..k)
        .map(|j| {
            let vals: Vec<u64> = (0..n_chunks)
                .flat_map(|c| (0..40u64).map(move |i| (c << 16) | (i * 7 + j as u64)))
                .collect();
            Arc::new(OrdSet::from_sorted_slice(&vals))
        })
        .collect();

    let build = || {
        let mut e = Expr::set(sets[0].clone());
        for s in &sets[1..] {
            e = e.or(Expr::set(s.clone()));
        }
        e
    };

    let (n, allocs) = count_allocs(|| build().cardinality().unwrap());
    assert!(n > 0, "the operands must actually contain something");

    // The fold allocated 2 429 for this shape; the accumulator 11. The budget
    // is deliberately far below the per-chunk figure and far above the
    // accumulator's, so it fails on a return to folding and does not fail on
    // an incidental allocation.
    //
    // Do not raise this to accommodate a change. The property is that the
    // cost does not scale with `k * chunks`.
    assert!(
        allocs < n_chunks,
        "an {k}-way OR made {allocs} allocations over {n_chunks} chunks — it is \
         folding pairwise rather than using the n-ary accumulator"
    );

    // And the answer is still the union.
    let mut want: std::collections::BTreeSet<u64> = std::collections::BTreeSet::new();
    for s in &sets {
        want.extend(s.iter());
    }
    assert_eq!(n, want.len() as u64, "the n-ary path changed the answer");
}

/// The same guarantee for a chain whose operands are **prefix-disjoint**.
///
/// **The test above cannot see this shape.** It builds all `k` operands over
/// the *same* 400 chunks, so their prefix spans overlap completely. That is the
/// case where a union really is a merge, and the cost model priced it correctly
/// all along. A chain of *disjoint* operands lowers to `Concat` — no merge, no
/// per-prefix compare — and `cardinality_cost` used to charge it `MERGE_STEP`
/// anyway, which made `decomposing_is_cheaper` prefer the `|A| + |B| - |A ∩ B|`
/// route. A chain decomposes **recursively**, which is precisely the pathology
/// the test above exists to prevent, reached through the one shape it does not
/// build. Measured: `Expr::cardinality()` on an 8-way disjoint chain went
/// 4.18 us -> 1.41 us when the overcharge was removed, a 2.97x speedup.
///
/// See `disjoint-or-is-overcharged` in `JOURNAL.md`.
#[test]
fn a_disjoint_chain_of_ors_also_avoids_the_pairwise_fold() {
    let per = 50u64;
    let k = 8usize;
    // Operand `j` occupies prefixes `[j*per, (j+1)*per)` — strictly separated,
    // so every `Or` in the chain is `Concat`-able.
    let sets: Vec<Arc<OrdSet>> = (0..k)
        .map(|j| {
            let base = j as u64 * per;
            let vals: Vec<u64> = (0..per)
                .flat_map(|c| (0..40u64).map(move |i| ((base + c) << 16) | (i * 7)))
                .collect();
            Arc::new(OrdSet::from_sorted_slice(&vals))
        })
        .collect();

    let build = || {
        let mut e = Expr::set(sets[0].clone());
        for s in &sets[1..] {
            e = e.or(Expr::set(s.clone()));
        }
        e
    };

    let (n, allocs) = count_allocs(|| build().cardinality().unwrap());
    assert!(n > 0, "the operands must actually contain something");

    // Measured: **218** allocations when the overcharge made this decompose,
    // **88** when it concatenates. Only 2.5x apart, not the orders of
    // magnitude the overlapping-chain test above enjoys, so this budget is
    // deliberately tight — 150 sits clear of both. The precise instrument for
    // this property is `a_prefix_disjoint_union_is_not_charged_for_a_merge` in
    // `stream::plan`, which asserts the cost directly; this is the behavioural
    // backstop that would notice the decision changing for some other reason.
    //
    // Do not raise it to accommodate a change.
    assert!(
        allocs < 150,
        "an {k}-way disjoint OR made {allocs} allocations ( concatenating is 88, \
         decomposing is 218 ) — it is decomposing rather than concatenating"
    );

    let mut want: std::collections::BTreeSet<u64> = std::collections::BTreeSet::new();
    for s in &sets {
        want.extend(s.iter());
    }
    assert_eq!(n, want.len() as u64, "the disjoint path changed the answer");
}

#[test]
fn cardinality_of_a_complement_does_not_materialize_the_complement() {
    let (a, _, _) = wide_sets();
    let n_chunks = a.chunk_count() as u64;
    let (lo, hi) = (0u64, n_chunks << 16);

    // The complement here is enormous — ~131 million ordinals against 100 000
    // in `a` — which is the whole point. If `cardinality()` materialized it, the
    // allocation count would scale with the answer rather than with the input.
    let expect = (hi - lo) - a.len();

    let _ = a.stream().not_in_range(lo, hi).cardinality().unwrap();
    let (n, allocs) = count_allocs(|| a.stream().not_in_range(lo, hi).cardinality().unwrap());
    assert_eq!(n, expect, "complement cardinality is wrong");

    // Measured baseline: 1.00 allocations per chunk. That one is `RangeStream`
    // building the universe slice for each prefix as a one-interval run — the
    // complement's *left* operand, not the complement itself. It is not free
    // and this test does not pretend it is; what it pins is that the count
    // tracks the number of chunks in the range and not the 131 million
    // ordinals in the answer.
    // Measured: **1.000 allocations per chunk**. That one is `RangeStream`
    // building the universe slice for each prefix as a one-interval run — the
    // complement's *left* operand, not the complement itself. This test does not
    // pretend that is free; what it pins is that the count tracks the number of
    // chunks in the range and not the 131 million ordinals in the answer.
    // Measured: **zero**. `Not::cardinality_dyn` applies
    // `|[lo,hi) \ S| == (hi - lo) - |S ∩ [lo,hi)|`, so it walks `S`'s chunks and
    // never builds the universe slice at all — the complement here is ~131
    // million ordinals and not one container is constructed for it.
    //
    // Pinned at exactly 0 rather than a ratio. The earlier `AndNot<RangeStream, S>`
    // formulation measured 1.00/chunk ( one universe run per prefix ), and it is
    // that per-prefix loop which made an unbounded `not()` a hang rather than a
    // subtraction. A regression to it would show up here as 2000, not as drift.
    assert_eq!(
        allocs, 0,
        "complement cardinality() allocated {allocs} times over {n_chunks} chunks; \
         it is stepping the range instead of walking the input"
    );

    // The separation from the materializing path is what gives the assertion
    // teeth: `collect_set` must still build a container per chunk.
    let (_, collect_allocs) =
        count_allocs(|| a.stream().not_in_range(lo, hi).collect_set().unwrap().len());
    assert!(
        collect_allocs > n_chunks * 4,
        "materializing the complement allocated only {collect_allocs} over {n_chunks} chunks; \
         the two paths have converged and this test no longer distinguishes them"
    );
}

/// The unbounded complement must be answered by subtraction, not by walking.
///
/// `x.not()` spans `2^48` prefixes. Counting it by stepping the range would take
/// ~10^14 iterations; counting it by walking `x` takes one step per stored chunk.
/// This is the test that tells those apart — a correctness test cannot, because
/// both return the same number, and only one of them returns it this decade.
#[test]
fn unbounded_complement_counts_without_walking_the_universe() {
    let (a, _, _) = wide_sets();
    let n_chunks = a.chunk_count() as u64;

    // Run it on a worker with a deadline, **warm-up included**. Without `Not::cardinality_dyn` this
    // test does detect the regression — by **hanging**, since counting the
    // complement then steps 2^48 prefixes — and a hang is a CI timeout with no
    // diagnosis attached. Verified 2026-08-26: disabling the override made this
    // run past 60 s rather than fail.
    //
    // `count_allocs` is thread-local by design, so the counting happens inside
    // the worker and the count is sent back with the answer.
    let (tx, rx) = std::sync::mpsc::channel();
    let set = a.clone();
    std::thread::spawn(move || {
        // The warm-up must be inside the deadline too. Leaving it on the main
        // thread reproduced the exact hang this guard was added to remove, and
        // I shipped that version — a deadline around part of the work is not a
        // deadline.
        let _ = set.stream().not().cardinality().unwrap();
        let _ = tx.send(count_allocs(|| set.stream().not().cardinality().unwrap()));
    });
    let (n, allocs) = rx
        .recv_timeout(std::time::Duration::from_secs(20))
        .expect("counting the complement walked the universe instead of the input");

    // |universe| - |a|, where the universe is [0, ORDINAL_MAX] under I8.
    assert_eq!(n, u64::MAX - a.len());
    assert_eq!(
        allocs, 0,
        "unbounded not().cardinality() allocated {allocs} times over {n_chunks} stored chunks"
    );
}

/// `range_summary` over a wide range must read two payloads, not one per chunk.
///
/// # The claim, and why only an allocation count can check it
///
/// The design calls `range_summary` and `len_in_range` the pair that "carries
/// the entire pushdown story", and the story is a ratio: a 1 M-row Parquet row
/// group spans about sixteen chunks, and deciding `skip` / `scan` /
/// `scan_selection` for it should cost **two** container reads — one at each
/// end of the range — with everything between answered from the `card_m1`
/// already in the leaf entries.
///
/// A correctness test cannot see the difference: an implementation that
/// decoded every chunk in range would return exactly the same number. This is
/// the same shape as `cardinality_is_answered_from_the_index_not_by_materializing`
/// above, and it exists for the same reason — the cheap path and the expensive
/// one are a parallel implementation, and nothing but a count keeps them apart.
///
/// The database is **reopened** before reading, so the answer comes off disk
/// rather than out of the memtable.
#[test]
fn a_range_summary_reads_two_payloads_not_one_per_chunk() {
    use yesno_core::{Db, DbOptions, RangeSummary};

    let dir = std::env::temp_dir().join(format!("yesno-alloc-range-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let chunks = 400u64;
    let opts = || DbOptions {
        shards: 1,
        ..Default::default()
    };
    {
        let db = Db::open_with(&dir, opts()).unwrap();
        // Every ordinal of every chunk, so wide sub-ranges come back `Full` and
        // the whole-chunk fast path is the one under test.
        db.insert_range(1, 0, chunks * 65_536 - 1).unwrap();
        db.checkpoint().unwrap();
    }

    let db = Db::open_with(&dir, opts()).unwrap();
    let snap = db.snapshot().unwrap();

    // A range spanning most of the key, cut mid-chunk at both ends so both
    // partial-window branches run.
    let lo = 7 * 65_536 + 100;
    let hi = (chunks - 5) * 65_536 - 100;
    let (summary, allocs) = count_allocs(|| snap.range_summary(1, lo, hi).unwrap());
    assert_eq!(
        summary,
        RangeSummary::Full,
        "the range is wholly inside a contiguous key"
    );

    // Materializing would be at least one container per chunk in range — about
    // 390 here. The budget is a quarter of that: loose enough not to be
    // brittle, tight enough that anything per-chunk fails it.
    let spanned = (hi >> 16) - (lo >> 16) + 1;
    assert!(
        allocs < spanned / 4,
        "{allocs} allocations to summarize {spanned} chunks — that is per-chunk \
         work, so the payloads are being decoded rather than the leaf entries read"
    );

    // And the count underneath agrees, over the same range.
    let (n, _) = count_allocs(|| snap.len_in_range(1, lo, hi).unwrap());
    assert_eq!(n, hi - lo, "every ordinal in the range is present");

    // A range past the end is `Empty` and must cost nothing extra.
    assert_eq!(
        snap.range_summary(1, chunks * 65_536, chunks * 65_536 + 1_000)
            .unwrap(),
        RangeSummary::Empty
    );

    drop(snap);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A chunk a set does not have must cost a refcount bump, not 8 KiB.
///
/// # Why this is a real cost and not a micro-optimization
///
/// A mask stream over a contiguous ordinal space emits one mask per chunk
/// whether or not the set has anything there, which is the whole point — a
/// consumer aligning against its own row numbering needs the gaps. Over a sparse
/// set most chunks *are* gaps, so `BooleanBuffer::new_unset` allocating and
/// zeroing 8 KiB per call turns "the set has nothing here" into the most
/// expensive answer in the stream.
///
/// The design says gaps should be "literally free" and names the mechanism: one
/// shared zero buffer. Only an allocation count can tell the two apart — both
/// produce an all-zero mask of the right length, and every correctness test
/// passes either way.
#[test]
fn absent_chunks_share_one_zero_buffer() {
    use yesno_core::unstable_arrow::empty_mask;

    // Warm the `OnceLock`, so the one-off initialization is not what is counted.
    let _ = empty_mask();

    let gaps = 200usize;
    let (total, allocs) = count_allocs(|| {
        let masks: Vec<_> = (0..gaps).map(|_| empty_mask()).collect();
        masks.iter().map(|m| m.len()).sum::<usize>()
    });
    assert_eq!(total, gaps * 65_536, "each gap is still a whole chunk wide");

    // The `Vec` of 200 masks is itself a handful of reallocations; anything
    // per-gap is not. 8 KiB * 200 would be 200 allocations plus the memset.
    assert!(
        allocs < 20,
        "{allocs} allocations for {gaps} absent chunks — that is per-gap work, \
         so each gap is allocating and zeroing its own 8 KiB rather than sharing"
    );

    // And the shared buffer is genuinely shared, not merely cheap to make.
    let a = empty_mask();
    let b = empty_mask();
    assert_eq!(a.len(), 65_536);
    assert_eq!(a.count_set_bits(), 0, "a gap selects nothing");
    assert!(
        std::ptr::eq(a.values().as_ptr(), b.values().as_ptr()),
        "two gaps must point at the same bytes"
    );
}

/// Karatsuba's recursion tree has `3^log2(n)` nodes. Allocating working space per
/// node would be `Theta(n^1.585)` allocations for one product; the design is one
/// buffer sized up front and sub-sliced by depth.
///
/// The budget is **constant across sizes**, not a measurement. Comparing two
/// operand sizes is what QG §3's instrument table prescribes for "does this scale"
/// — a single-size budget would be satisfied by an implementation that allocates
/// per level, and one per node at small `n` looks like a small number.
#[test]
fn the_recursive_multiply_allocates_a_fixed_number_of_buffers_at_every_size() {
    use yesno_core::bignum::{BigUint, KARATSUBA_MIN};

    let big = |n: usize, seed: u64| {
        let mut s = seed;
        BigUint::from_limbs_le(
            (0..n)
                .map(|_| {
                    s = s
                        .wrapping_mul(6364136223846793005)
                        .wrapping_add(1442695040888963407);
                    s
                })
                .collect(),
        )
    };

    // Warm up, so lazily-initialized machinery is not counted.
    let (a, b) = (big(64, 1), big(64, 2));
    let _ = a.mul(&b);

    let mut counts = Vec::new();
    for &n in &[KARATSUBA_MIN, 64usize, 256, 1024] {
        let (a, b) = (big(n, 7), big(n, 11));
        let (p, allocs) = count_allocs(|| a.mul(&b));
        assert!(!p.is_zero());
        counts.push((n, allocs));
    }

    // The result buffer and the scratch buffer. `from_limbs_le` trims in place,
    // so the returned value reuses the result's allocation.
    for &(n, allocs) in &counts {
        assert!(
            allocs <= 4,
            "{allocs} allocations for a {n}-limb product — the recursion is \
             allocating per level or per node instead of sub-slicing one buffer"
        );
    }
    let first = counts[0].1;
    for &(n, allocs) in &counts {
        assert_eq!(
            allocs, first,
            "a {n}-limb product allocated {allocs} times against {first} at \
             {} limbs — the count must not grow with the operand",
            counts[0].0
        );
    }
}

/// Below the crossover there is no recursion, so there is nothing to size and the
/// scratch must not be allocated at all. `ops::nary`'s header records the same
/// mistake made with its accumulator: allocating up front made the common case
/// slower.
#[test]
fn a_schoolbook_multiply_allocates_no_scratch() {
    use yesno_core::bignum::{BigUint, KARATSUBA_MIN};

    let small = BigUint::from_limbs_le(vec![0x1234_5678_9abc_def0; KARATSUBA_MIN - 1]);
    let _ = small.mul(&small);
    let (p, allocs) = count_allocs(|| small.mul(&small));
    assert!(!p.is_zero());
    assert!(
        allocs <= 2,
        "{allocs} allocations below the Karatsuba crossover — the scratch buffer \
         is being allocated above the dispatch rather than inside the arm"
    );
}

/// A very unbalanced product blocks the long operand rather than splitting it,
/// and blocking must reuse one buffer. Compared across two lengths of the long
/// operand: a per-block allocation is invisible at one size.
#[test]
fn an_unbalanced_product_does_not_allocate_per_block() {
    use yesno_core::bignum::{BigUint, KARATSUBA_MIN};

    let short = BigUint::from_limbs_le(vec![0x9e37_79b9_7f4a_7c15; KARATSUBA_MIN]);
    let long_n = |n: usize| BigUint::from_limbs_le(vec![0xdead_beef_feed_face; n]);
    let _ = long_n(256).mul(&short);

    let (_, a) = count_allocs(|| long_n(256).mul(&short));
    let (_, b) = count_allocs(|| long_n(4096).mul(&short));
    assert_eq!(
        a, b,
        "{a} allocations at 256 limbs against {b} at 4096 — that is one per \
         block, so the block product buffer is not being reused"
    );
}

/// `Snapshot::keys` must cost O( distinct keys ), not O( chunks ).
///
/// A key's chunks are a contiguous `ChunkKey` range, so enumeration finds a key
/// and then **seeks past** it via `ChunkKey::range_end`. Walking its chunks one
/// by one instead returns the identical list — so no correctness test can tell
/// the two apart, and only a cost measurement can.
///
/// This is the same shape as `cardinality_is_answered_from_the_index`: the
/// dangerous implementation is not wrong, it is slow in a way that scales with
/// data nobody looked at.
///
/// The database is **reopened** before reading, so the walk has to come off disk
/// rather than out of the memtable.
#[test]
fn key_enumeration_seeks_past_a_key_rather_than_scanning_its_chunks() {
    use yesno_core::{Db, DbOptions};

    let dir = std::env::temp_dir().join(format!("yesno-alloc-keys-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // Few keys, many chunks each: the shape that separates the two
    // implementations. A per-chunk walk does `keys * chunks_per_key` steps; a
    // seeking one does `keys`.
    let keys = 4u64;
    let chunks_per_key = 400u64;
    let opts = || DbOptions {
        shards: 1,
        ..Default::default()
    };
    {
        let db = Db::open_with(&dir, opts()).unwrap();
        for k in 0..keys {
            let vals: Vec<u64> = (0..chunks_per_key).map(|c| (c << 16) | 7).collect();
            db.insert_many(k, &vals).unwrap();
        }
        db.checkpoint().unwrap();
    }

    let db = Db::open_with(&dir, opts()).unwrap();
    let snap = db.snapshot().unwrap();
    let (found, allocs) = count_allocs(|| snap.keys().unwrap());
    assert_eq!(
        found,
        (0..keys).collect::<Vec<u64>>(),
        "the key list itself must be right"
    );

    // A per-chunk walk touches 1 600 entries here. The budget is generous
    // enough to absorb the per-key descents and the candidate vectors, and
    // tight enough that anything scaling with `chunks_per_key` fails it.
    //
    // Do not raise this to accommodate a change. The property being pinned is
    // that the cost does not scale with chunks, and a budget that grows with
    // them measures nothing.
    let budget = keys * 40;
    assert!(
        allocs < budget,
        "keys() made {allocs} allocations for {keys} keys of {chunks_per_key} \
         chunks each ( budget {budget} ) — it is walking chunks rather than \
         seeking past each key"
    );

    drop(snap);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// `min` and `max` are a **parallel implementation** of `load( key ).min()`
/// and `.max()`, and they return the same values, so no correctness test can
/// tell them apart. This is the guard, in the same shape and for the same
/// reason as `cardinality_is_answered_from_the_index_not_by_materializing`.
///
/// The database is **reopened** before reading, so the answer comes off disk
/// rather than out of the memtable.
#[test]
fn an_endpoint_is_not_answered_by_materializing() {
    use yesno_core::{Db, DbOptions};

    let dir = std::env::temp_dir().join(format!("yesno-alloc-endpoint-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let chunks = 500u64;
    let opts = || DbOptions {
        shards: 1,
        ..Default::default()
    };
    {
        let db = Db::open_with(&dir, opts()).unwrap();
        // One ordinal per chunk, so any per-chunk cost is unmistakably
        // per-chunk rather than per-byte.
        let vals: Vec<u64> = (0..chunks).map(|c| (c << 16) | 7).collect();
        db.insert_many(1, &vals).unwrap();
        db.checkpoint().unwrap();
    }

    let db = Db::open_with(&dir, opts()).unwrap();
    let snap = db.snapshot().unwrap();

    let (lo, lo_allocs) = count_allocs(|| snap.min(1).unwrap());
    let (hi, hi_allocs) = count_allocs(|| snap.max(1).unwrap());
    assert_eq!(lo, Some(7), "the minimum itself must still be right");
    assert_eq!(
        hi,
        Some(((chunks - 1) << 16) | 7),
        "the maximum itself must still be right"
    );

    // Materializing allocates at least one container per chunk. Both ends now
    // decode exactly one, whatever the chunk count.
    // Do not raise this budget to accommodate a change; the whole point is
    // that the cost must not scale with chunk count.
    assert!(
        lo_allocs < chunks / 2,
        "min made {lo_allocs} allocations for {chunks} chunks — it is \
         materializing the posting list rather than reading one chunk"
    );
    assert!(
        hi_allocs < chunks / 2,
        "max made {hi_allocs} allocations for {chunks} chunks — it is \
         materializing the posting list rather than reading one chunk"
    );

    drop(snap);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// `Snapshot::key_stream` must not decode the key it streams.
///
/// This is the budget the whole operation exists for. `Snapshot::load` builds
/// every container before a consumer can look at one, so a key with `10^9`
/// ordinals pays ~15 000 of them whether or not the consumer reads past the
/// first chunk. A stream that resolved every chunk to a reference and then
/// decoded them all anyway would return the same values and pass every
/// correctness test in the tree.
///
/// **What that costs is allocations, not payload bytes.** `read_container`
/// aliases the mapping rather than copying it, so the figure below is the
/// honest one: this test counts allocations because that is what `load`
/// actually spends. Do not restate it as megabytes.
///
/// Two budgets, because they fail differently:
///
/// * **opening plus one chunk** must be far below what `load` costs. A stream
///   that eagerly decoded would match `load` instead.
/// * **counting** must not scale with chunk count at all — the counts are in
///   `ChunkRef::card_m1` and no payload needs reading, exactly as
///   `cardinality_is_answered_from_the_index_not_by_materializing` asserts for
///   the non-streaming path.
#[test]
fn a_key_stream_does_not_decode_the_key_it_streams() {
    use yesno_core::stream::ChunkStream;
    use yesno_core::{Db, DbOptions};

    let dir = std::env::temp_dir().join(format!("yesno-alloc-keystream-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let chunks = 500u64;
    let opts = || DbOptions {
        shards: 1,
        ..Default::default()
    };
    {
        let db = Db::open_with(&dir, opts()).unwrap();
        // Several ordinals per chunk, so a decoded container is a real
        // allocation rather than something the inline path can hold.
        let vals: Vec<u64> = (0..chunks)
            .flat_map(|c| (0..8u64).map(move |i| (c << 16) | (i * 97)))
            .collect();
        db.insert_many(1, &vals).unwrap();
        db.checkpoint().unwrap();
    }

    let db = Db::open_with(&dir, opts()).unwrap();
    let snap = db.snapshot().unwrap();

    let (eager, load_allocs) = count_allocs(|| snap.load(1).unwrap());
    assert_eq!(eager.len(), chunks * 8);

    let (first, open_allocs) = count_allocs(|| {
        let mut s = snap.key_stream(1).unwrap();
        s.next_chunk().unwrap()
    });
    assert!(first.is_some(), "the stream must actually yield a chunk");

    // Opening resolves every chunk to a reference, so it is not free — one
    // `Vec` of `Step`s plus the memtable probe. What it must not do is scale
    // like `load`. **Measured** on this fixture ( 500 chunks, 8 ordinals each,
    // reopened so the answer comes off disk ): `load` 1 631 allocations, open
    // plus one chunk 49, counting 47. A quarter of `load` is loose enough not
    // to be brittle and tight enough that decoding every container fails it.
    assert!(
        open_allocs < load_allocs / 4,
        "opening a key stream and reading one chunk made {open_allocs} allocations \
         against {load_allocs} for `load` over {chunks} chunks — it is decoding \
         the key rather than resolving it to references"
    );

    // Counting must not scale with chunk count. Same reasoning and the same
    // budget as the non-streaming path: do not raise it to accommodate a
    // change, because the point is that the cost is not per chunk.
    let (n, count_allocs_) = count_allocs(|| {
        let mut s = snap.key_stream(1).unwrap();
        s.cardinality_dyn().unwrap()
    });
    assert_eq!(n, chunks * 8, "the count itself must still be right");
    assert!(
        count_allocs_ < chunks / 2,
        "counting a key stream made {count_allocs_} allocations for {chunks} chunks — \
         it is decoding payloads rather than reading `card_m1`"
    );

    drop(snap);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A prefix-bounded stream must bound metadata planning, not merely payloads.
///
/// Correct contents cannot distinguish a range-restricted B+tree cursor from a
/// full-key plan filtered after construction. Allocation count is weak here too:
/// a geometrically growing `Vec` uses only logarithmically more allocation
/// calls. Requested bytes expose the retained plan capacity and all temporary
/// full-plan growth directly.
#[test]
fn a_prefix_bounded_key_stream_does_not_plan_the_whole_key() {
    use yesno_core::{Db, DbOptions};

    let dir = std::env::temp_dir().join(format!(
        "yesno-alloc-prefix-keystream-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let chunks = 4_096u64;
    let width = 8u64;
    let start = 3_000u64;
    let options = DbOptions {
        shards: 1,
        ..Default::default()
    };
    {
        let db = Db::open_with(&dir, options.clone()).unwrap();
        let values: Vec<u64> = (0..chunks)
            .flat_map(|prefix| [1u64, 97, 193, 389].map(move |low| (prefix << 16) | low))
            .collect();
        db.insert_many(1, &values).unwrap();
        db.checkpoint().unwrap();
    }

    let db = Db::open_with(&dir, options).unwrap();
    let snap = db.snapshot().unwrap();

    let (full, full_bytes) = count_alloc_bytes(|| snap.key_stream(1).unwrap());
    assert_eq!(full.chunks_remaining(), chunks as usize);
    drop(full);

    let (bounded, bounded_bytes) = count_alloc_bytes(|| {
        snap.key_stream_prefix_range(1, start, start + width)
            .unwrap()
    });
    assert_eq!(
        bounded.chunks_remaining(),
        width as usize,
        "the fixture has one visible chunk at every requested prefix"
    );

    // The ratio, not either host-dependent byte total, is the assertion. A
    // full-key plan followed by `retain` requests essentially the full arm's
    // memory before returning the right eight chunks. Eightfold headroom is
    // loose against a 512:1 width ratio while still rejecting that decay.
    assert!(
        bounded_bytes.saturating_mul(8) < full_bytes,
        "bounded planning requested {bounded_bytes} bytes for {width} chunks, against {full_bytes} for {chunks}; it appears to plan the whole key"
    );

    drop(bounded);
    drop(snap);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A lazy leaf inside an `And` must not decode the chunks the `And` skips.
///
/// This is the payoff `Snapshot::key_expr` exists for, and the one thing no
/// correctness test can see: the lazy and the eager form return the same set,
/// so only a count of allocations distinguishes "skipped it" from "decoded it
/// and then discarded it".
///
/// The fixture is deliberately skewed -- a wide key and a narrow one overlapping
/// at the far end -- because that is the shape where a seek-driven intersection
/// can skip almost everything, and it is the shape a filtered query actually
/// has. With both operands materialized up front the skew buys nothing: `load`
/// has already paid for every chunk before the operator gets to be clever.
#[test]
fn a_lazy_leaf_does_not_decode_what_the_operator_skips() {
    use yesno_core::{Db, DbOptions, Expr};

    let dir = std::env::temp_dir().join(format!("yesno-alloc-lazyleaf-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let wide = 600u64;
    let opts = || DbOptions {
        shards: 1,
        ..Default::default()
    };
    {
        let db = Db::open_with(&dir, opts()).unwrap();
        // Key 1 spans 600 chunks; key 2 occupies only the last 5 of them.
        let a: Vec<u64> = (0..wide)
            .flat_map(|c| (0..8u64).map(move |i| (c << 16) | (i * 97)))
            .collect();
        db.insert_many(1, &a).unwrap();
        let b: Vec<u64> = (wide - 5..wide)
            .flat_map(|c| (0..8u64).map(move |i| (c << 16) | (i * 97)))
            .collect();
        db.insert_many(2, &b).unwrap();
        db.checkpoint().unwrap();
    }

    let db = Db::open_with(&dir, opts()).unwrap();
    let snap = db.snapshot().unwrap();

    let (eager_n, eager_allocs) = count_allocs(|| {
        let e = Expr::set(snap.load(1).unwrap()).and(Expr::set(snap.load(2).unwrap()));
        e.cardinality().unwrap()
    });
    let (lazy_n, lazy_allocs) = count_allocs(|| {
        let e = snap.key_expr(1).and(snap.key_expr(2));
        e.cardinality().unwrap()
    });

    // Same answer, or the comparison is meaningless.
    assert_eq!(eager_n, lazy_n, "the two forms must agree on the count");
    assert_eq!(lazy_n, 40, "5 overlapping chunks of 8 ordinals");

    // **Measured** on this fixture ( 600 chunks against 5, reopened so the read
    // comes off disk ): **1 994 allocations eager, 108 lazy** -- an 18.5x
    // reduction. ( It was 169 / 11.8x before `KeySource` began sharing one
    // immutable plan across opens; the figure moved because that removed a
    // rebuild, not because this test changed. ) The budget below is half, which is loose enough not to be
    // brittle and tight enough that a lazy leaf decoding every chunk anyway
    // would fail it.
    //
    // The lazy figure is not zero and should not be: opening resolves all 600
    // chunks of key 1 to references, which is a `Vec` of steps whose growth
    // amortizes, and that cost is deliberately kept. What it does not pay is a
    // decoded container per chunk the `And` never looks at.
    assert!(
        lazy_allocs < eager_allocs / 2,
        "a lazy leaf made {lazy_allocs} allocations against {eager_allocs} eager over \
         {wide} chunks with a 5-chunk overlap -- it is decoding chunks the `And` skips"
    );

    drop(snap);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The early decline must not swallow a union that should segment.
///
/// `compute_segments` short-circuits before collecting spans when an
/// `Expr::Source` falls outside `occupancy_of`'s bounds. That check *anticipates*
/// a decision made later, so its failure mode is asymmetric: declining too
/// eagerly loses segmentation **silently**, with correct results and no error,
/// which no correctness test can see. Only the cost changes, so only a cost test
/// can hold the line.
///
/// The fixture is above `SEGMENT_MIN_CHUNKS` and nearly disjoint, which is the
/// shape segmentation accepts -- so if the short-circuit drifts away from the
/// condition it mirrors, the segmented cost disappears and this fails.
#[test]
fn segmentation_still_engages_for_sources_above_the_threshold() {
    use yesno_core::{Db, DbOptions};

    let dir = std::env::temp_dir().join(format!("yesno-alloc-seg-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    const K: u64 = 8;
    let (step, lap) = (200u64, 20u64);
    let opts = || DbOptions {
        shards: 1,
        ..Default::default()
    };
    {
        let db = Db::open_with(&dir, opts()).unwrap();
        for k in 0..K {
            let v: Vec<u64> = (k * step..k * step + step + lap)
                .map(|c| (c << 16) | 7)
                .collect();
            db.insert_many(k, &v).unwrap();
        }
        db.checkpoint().unwrap();
    }
    let db = Db::open_with(&dir, opts()).unwrap();
    let snap = db.snapshot().unwrap();

    let mut e = snap.key_expr(0);
    for k in 1..K {
        e = e.or(snap.key_expr(k));
    }
    // Warm every source's plan, so this measures evaluation and not the
    // one-off index scan each `KeySource` performs on first use.
    let _ = e.clone().cardinality().unwrap();

    let (n, allocs) = count_allocs(|| e.clone().cardinality().unwrap());
    assert_eq!(n, K * (step + lap) - (K - 1) * lap, "count must be right");

    // **Measured on this exact fixture**: 1 038 allocations with segmentation
    // engaged, 910 with it suppressed -- the flat +128 setup that holds at every
    // size from 400 to 6 400 chunks. The floor sits between them, nearer the
    // broken value than the working one so a benign improvement in the
    // segmented path does not trip it.
    //
    // **The first version of this assertion used 800 and caught nothing**, being
    // below both numbers; suppressing segmentation entirely left it green. Do
    // not lower it without re-deriving both figures, and do not raise it to the
    // working value either -- an equality assertion on an allocation count is a
    // tripwire for every unrelated change.
    //
    // Deliberately a *lower* bound, the unusual direction for this file: every
    // other budget here caps a cost, while this one asserts a cost is still
    // being paid, because what it guards against is an optimization silently
    // not happening.
    assert!(
        allocs >= 970,
        "an 8-way union of 220-chunk sources made only {allocs} allocations -- \
         segmentation is no longer engaging, so the early decline in \
         `compute_segments` has drifted from `occupancy_of`'s bounds"
    );

    drop(snap);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A checkpoint must not decode every index leaf to rebuild the tree.
///
/// # Why this needs an allocation count rather than an assertion on behaviour
///
/// `Tree::build_updating` reuses a leaf no key touched **by page id, without
/// decoding its entries**. Its slow path decodes the leaf and then reuses it
/// anyway, so the two are semantically identical: deleting the fast path leaves
/// every correctness test in the tree green and every answer unchanged, while
/// silently restoring an `O( total entries )` cost to the checkpoint's exclusive
/// region -- which is 70-82% of the reader stall it was measured against.
///
/// Decoding a leaf allocates. So the allocation count is the only signal that
/// separates "reused it" from "reused it the expensive way", which is the same
/// argument the non-materializing cardinality walk in this file rests on.
///
/// Measured 2026-09-16 at 20 000 resident keys ( about 320 leaves ): **2095**
/// allocations with the fast path, **3375** without it -- the difference being
/// roughly four per leaf for the decoded `Vec` and its growth. The bound sits
/// between them and must never be raised to make a failing run pass; a rise
/// means the reuse path stopped being taken.
///
/// **The first version of this test had a bound of 4000, above both numbers, so
/// it passed either way.** It was caught by measuring the sabotaged build
/// instead of assuming the gap was large -- a threshold picked without both
/// sides of it measured is not a test, and it looks exactly like one.
#[test]
fn a_checkpoint_does_not_decode_every_leaf() {
    use yesno_core::{Db, DbOptions};

    let dir = std::env::temp_dir().join(format!("alloc-ckpt-leaves-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 1,
            ..Default::default()
        },
    )
    .unwrap();

    const KEYS: u64 = 20_000;
    for k in 0..KEYS {
        let v: Vec<u64> = (0..24u64).map(|i| (k << 20) | (i * 7)).collect();
        db.insert_many(k, &v).unwrap();
    }
    db.checkpoint().unwrap();

    // One key dirty: exactly one leaf can have changed.
    db.insert_many(7, &[(7u64 << 20) | 999]).unwrap();
    let (r, allocs) = count_allocs(|| db.checkpoint());
    r.unwrap();

    drop(db);
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        allocs < 2_800,
        "a one-key checkpoint allocated {allocs} times over ~320 leaves; \
         the untouched-leaf reuse path is no longer being taken \
         ( 2095 expected with it, 3375 without )"
    );
}

#[cfg(all(feature = "jit", any(target_arch = "aarch64", target_arch = "x86_64")))]
mod jit_admission_work {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use yesno_core::stream::{BoxedStream, ChunkSource, ChunkStream, SetStream};
    use yesno_core::{jit, Container, Expr, OrdSet, Prefix48, Result};

    #[derive(Debug)]
    struct CountedSource {
        set: Arc<OrdSet>,
        payloads: Arc<AtomicUsize>,
    }

    struct CountedStream {
        inner: SetStream,
        payloads: Arc<AtomicUsize>,
    }

    impl ChunkStream for CountedStream {
        fn next_chunk(&mut self) -> Result<Option<(Prefix48, Container)>> {
            let next = self.inner.next_chunk()?;
            if next.is_some() {
                self.payloads.fetch_add(1, Ordering::Relaxed);
            }
            Ok(next)
        }

        fn seek(&mut self, prefix: Prefix48) -> Result<()> {
            self.inner.seek(prefix)
        }

        fn peek_prefix(&mut self) -> Result<Option<Prefix48>> {
            self.inner.peek_prefix()
        }
    }

    impl ChunkSource for CountedSource {
        fn open(&self) -> BoxedStream {
            Box::new(CountedStream {
                inner: SetStream::new(self.set.clone()),
                payloads: self.payloads.clone(),
            })
        }

        fn chunk_count(&self) -> Option<u64> {
            Some(self.set.chunk_count() as u64)
        }

        fn prefix_span(&self) -> Option<(Prefix48, Prefix48)> {
            let lo = self.set.chunk_at(0)?.0;
            let hi = self.set.chunk_at(self.set.chunk_count() - 1)?.0;
            Some((lo, hi))
        }

        fn all_bitmap_chunks(&self) -> Option<bool> {
            Some(true)
        }
    }

    fn bitmap_source(prefixes: impl Iterator<Item = u64>) -> (Expr, Arc<AtomicUsize>) {
        let values = prefixes.flat_map(|prefix| {
            (0..5_000u64).map(move |low| (prefix << 16) | ((low * 37) & 0xffff))
        });
        let set = Arc::new(OrdSet::from_iter_unsorted(values));
        assert!(set
            .chunks()
            .all(|(_, chunk)| matches!(chunk, Container::Bitmap(_))));
        let payloads = Arc::new(AtomicUsize::new(0));
        let source = CountedSource {
            set,
            payloads: payloads.clone(),
        };
        (Expr::Source(Arc::new(source)), payloads)
    }

    /// A selective AND should use the seek-driven core path, whose payload
    /// work is bounded by the matching prefixes, not the wide leaf's size.
    #[test]
    fn automatic_jit_does_not_decode_the_wide_side_of_a_selective_and() {
        let (wide, wide_reads) = bitmap_source(0..256);
        let (narrow, narrow_reads) = bitmap_source(std::iter::once(128));
        let expr = wide.and(narrow);
        let expected = expr.cardinality().unwrap();
        assert!(expected > 0);
        assert_eq!(wide_reads.swap(0, Ordering::Relaxed), 1);
        assert_eq!(narrow_reads.swap(0, Ordering::Relaxed), 1);

        assert_eq!(jit::cardinality(&expr).unwrap(), expected);
        assert_eq!(wide_reads.load(Ordering::Relaxed), 1);
        assert_eq!(narrow_reads.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn explicit_jit_seeks_past_wide_side_of_selective_and() {
        let (wide, wide_reads) = bitmap_source(0..256);
        let (narrow, narrow_reads) = bitmap_source(std::iter::once(128));
        let expr = wide.and(narrow);
        let expected = expr.cardinality().unwrap();
        assert!(expected > 0);
        assert_eq!(wide_reads.swap(0, Ordering::Relaxed), 1);
        assert_eq!(narrow_reads.swap(0, Ordering::Relaxed), 1);

        let mut jit = jit::DagJit::new();
        assert_eq!(jit.try_cardinality(&expr).unwrap().unwrap(), expected);
        assert_eq!(wide_reads.load(Ordering::Relaxed), 1);
        assert_eq!(narrow_reads.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn explicit_jit_skips_an_inactive_and_branch_beneath_or() {
        let (wide, wide_reads) = bitmap_source(0..256);
        let (narrow, narrow_reads) = bitmap_source(std::iter::once(128));
        let (other, other_reads) = bitmap_source(std::iter::once(0));
        let expr = wide.and(narrow).or(other);
        let expected = expr.cardinality().unwrap();
        assert!(expected > 0);
        wide_reads.store(0, Ordering::Relaxed);
        narrow_reads.store(0, Ordering::Relaxed);
        other_reads.store(0, Ordering::Relaxed);

        let mut jit = jit::DagJit::new();
        assert_eq!(jit.try_cardinality(&expr).unwrap().unwrap(), expected);
        assert_eq!(wide_reads.load(Ordering::Relaxed), 1);
        assert_eq!(narrow_reads.load(Ordering::Relaxed), 1);
        assert_eq!(other_reads.load(Ordering::Relaxed), 1);
    }
}
