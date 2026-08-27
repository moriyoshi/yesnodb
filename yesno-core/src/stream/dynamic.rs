//! Runtime-constructed expression trees.
//!
//! The generic operators are monomorphized for the statically-known hot path.
//! A query planner does not know the shape ahead of time, so it builds an
//! [`Expr`] and opens it into a `Box<dyn ChunkStream>`. This is the form a
//! DataFusion pushdown lowers into.

use std::sync::Arc;

use super::{ChunkStream, ChunkStreamExt, Concat, EmptyStream, RangeStream, Restrict, SetStream};
use crate::container::Container;
use crate::stream::sketch::{Bucketing, PrefixOccupancy};
use crate::{OrdSet, Prefix48, Result};

/// A type-erased chunk stream.
pub type BoxedStream = Box<dyn ChunkStream>;

// Delegating impl so a boxed stream composes exactly like a concrete one.
impl ChunkStream for BoxedStream {
    fn next_chunk(&mut self) -> Result<Option<(Prefix48, Container)>> {
        (**self).next_chunk()
    }
    fn seek(&mut self, prefix: Prefix48) -> Result<()> {
        (**self).seek(prefix)
    }
    fn peek_prefix(&mut self) -> Result<Option<Prefix48>> {
        (**self).peek_prefix()
    }
    fn cardinality_hint(&self) -> (u64, Option<u64>) {
        (**self).cardinality_hint()
    }
    /// Must delegate. Without this the default `unknown()` applies the moment a
    /// stream is boxed — and every stream a planner sees is boxed, so the whole
    /// statistics channel would read as "no information" exactly where it is
    /// needed. Same trap as `cardinality_dyn` above.
    fn stats(&self) -> super::StreamStats {
        (**self).stats()
    }
    /// Must delegate, for the same reason as `stats` and `cardinality_dyn`:
    /// every stream an operator holds is boxed, so without this the whole
    /// non-materializing path reverts to cloning at the first `Box`.
    fn next_cardinality(&mut self) -> Result<Option<(Prefix48, u64)>> {
        (**self).next_cardinality()
    }
    /// Forwards to the inner operator's override rather than the materializing
    /// default — this delegation is the whole reason `cardinality_dyn` lives on
    /// the object-safe trait instead of only on the extension trait.
    fn cardinality_dyn(&mut self) -> Result<u64> {
        (**self).cardinality_dyn()
    }
}

/// Is `|A| + |B| - |A ∩ B|` cheaper than merging A and B?
///
/// Do not replace this with `true`. The identity is exact, so always taking
/// it is *correct* — and it costs an 8-way `Or` 27 538 allocations, because a
/// chain decomposes into nested `And`s and bypasses the `UnionAll` accumulator.
/// No correctness test can catch that; `tests/allocation.rs` is what does.
fn decomposing_is_cheaper(a: &Expr, b: &Expr, inter: &Expr) -> bool {
    use crate::stream::plan::cardinality_cost;
    // Weighted, because a merge step is not a chunk-count step. Taking the raw
    // chunk sum here made the two sides tie for a disjoint union of equal-sized
    // operands, so the decomposition was declined by a hair and segmentation had
    // to rescue the case instead — at the price of an O(n) occupancy map.
    //
    // This used to inline `( yield a + yield b ) * MERGE_STEP`, a **second
    // copy** of the rule in `plan::cardinality_cost`'s `Or` arm. When that arm
    // learned that a prefix-disjoint union lowers to `Concat` and costs a plain
    // sum, this copy did not, and would have kept overcharging exactly the shape
    // the fix was about. One implementation, asked directly.
    //
    // Asked **by reference**. The first version built an
    // `Expr::Or( Box::new( a.clone() ), Box::new( b.clone() ) )` to hand to
    // `cardinality_cost`. That is not free in general — `Expr` holds
    // `Box<Expr>`, so cloning a *composite* operand is a deep copy of its whole
    // subtree, which is the defect that made `pass_b` quadratic — even though it
    // costs nothing for the leaf operands this gate usually sees, where
    // `Expr::Set::clone` is an `Arc` bump. Measured: removing it moved
    // `rule_economics.py` by 0 ns, so this is a hazard closed rather than a
    // regression fixed.
    let merge = crate::stream::plan::union_cost(a, b);
    let decomposed = cardinality_cost(a)
        .saturating_add(cardinality_cost(b))
        .saturating_add(cardinality_cost(inter));
    decomposed < merge
}

/// Most segments a union may be split into.
///
/// Every segment re-opens each of its contributors, so segmentation stops paying
/// once there are many of them. Bucketed occupancy already bounds the count; this
/// is that bound stated where it is enforced.
const MAX_SEGMENTS: usize = 64;

/// Chunks an [`Expr::Source`] operand needs before segmenting it can pay.
///
/// **Measured 2026-09-13, and the loss below it is not small.** Segmentation's
/// setup is a constant -- exactly **+128 allocations** per evaluation at every
/// size tested -- so it amortizes only once the operands are large enough.
/// An 8-way union of nearly-disjoint sources, wall clock, segmented against not:
///
/// ```text
///   chunks/operand      segmented      plain      verdict
///               11        6 696 ns    3 321 ns    2.0x slower
///               22       14 992 ns    5 064 ns    3.0x slower
///               44       18 946 ns    8 276 ns    2.3x slower
///               88       27 190 ns   28 824 ns    break-even
///              110       16 305 ns   18 073 ns    1.11x faster
///              165       22 188 ns   25 943 ns    1.17x faster
///              220       27 211 ns   34 222 ns    1.26x faster
///              330       37 855 ns   50 132 ns    1.32x faster
/// ```
///
/// Break-even is ~88 and the win grows from there, so this sits above it with
/// margin rather than at it. A resident `Expr::Set` operand is **not** gated
/// this way and keeps its existing behaviour: the number above was measured for
/// sources, and applying it to sets would be changing a path this work never
/// measured. Construction and figures: JOURNAL 2026-09-13, under
/// `reopen-cost` -- recorded there rather than cited by path, because the crate
/// that produced them lived in the scratch directory and is gone.
///
/// # The first verified caller, 2026-09-14, and it sits below the gate
///
/// The figures above are synthetic. A downstream consumer measured the chunk
/// count of a real filter operand -- the only expression it builds -- by walking
/// `Snapshot::key_stream` over the persisted key after a checkpoint:
///
/// ```text
///   documents   blocks   scattered   clustered
///      65 536        1           1           1
///     262 144        4           4           1
/// ```
///
/// **One chunk per block the operand touches**, which is structural rather than
/// incidental: a block is 65 536 consecutive ordinals and a chunk is a 48-bit
/// prefix, so an aligned block is exactly one chunk. A scattered admit set
/// touches every block ( `chunks = ceil( N / 65536 )` ); a clustered one stays
/// at 1 at any corpus size. **Extrapolated, not measured**: 128 chunks is
/// `128 * 65536` = ~8.4M documents scattered, and unreachable clustered.
///
/// So this gate **protects** that caller rather than obstructing it -- it sits
/// below 128 for any corpus under ~8.4M scattered, and below it at any size once
/// clustered assignment lands. Note what this does and does not license: one
/// caller on one side is not a derivation of the threshold, and the threshold
/// still rests on the synthetic table above. See JOURNAL 2026-09-14, including
/// why this is the consumer's *third* position on its own operand size.
const SEGMENT_MIN_CHUNKS: u64 = 128;

/// Bucketed occupancy for an operand, where it can be computed.
///
/// A range is filled analytically — enumerating a `2^48`-prefix range to build a
/// 256-bit summary would be absurd, and it is exactly the operand most likely to
/// be involved.
fn occupancy_of(e: &Expr, b: Bucketing) -> Option<PrefixOccupancy> {
    match e {
        Expr::Set(s) => {
            (s.chunk_count() as u64 <= crate::stream::plan::effective_cutoff()).then(|| {
                PrefixOccupancy::from_prefixes(
                    b,
                    (0..s.chunk_count()).filter_map(|i| s.prefix_at(i)),
                )
            })
        }
        Expr::Range(lo, hi) if hi > lo => Some(PrefixOccupancy::from_range(
            b,
            lo >> crate::CHUNK_BITS,
            (hi - 1) >> crate::CHUNK_BITS,
        )),
        // A source builds this from its own prefixes. Bounded by the same
        // cutoff as a resident set, so a huge operand does not pay for a
        // summary it was never going to segment on.
        //
        // **Never from the prefix span.** Feeding a span to `from_range` would
        // claim every prefix between the ends is occupied, and a key's chunks
        // are scattered inside their span. Unlike `bounds`, where a wider answer
        // only costs an optimization, an overstatement here picks a worse split.
        Expr::Source(src) => {
            let n = src.chunk_count()?;
            // Declining here is how a too-small source opts out: `compute_segments`
            // already requires an occupancy from *every* operand, so `None` is
            // the existing decline path rather than a second gate. The upper
            // bound is the same cutoff a resident set gets; the lower one is
            // `SEGMENT_MIN_CHUNKS`, where segmenting starts to pay at all.
            (n >= SEGMENT_MIN_CHUNKS && n <= crate::stream::plan::effective_cutoff())
                .then(|| src.occupancy(b))
                .flatten()
        }
        _ => None,
    }
}

/// Can this expression be opened repeatedly without redoing real work?
///
/// Segmentation opens an operand once per segment it appears in, so it is only
/// worth it for leaves. Re-opening a composite subtree would duplicate its whole
/// evaluation, which is a far larger cost than the merge being avoided.
fn cheap_to_reopen(e: &Expr) -> bool {
    // `Expr::Source` is **included**, and that took two rounds of measurement
    // on 2026-09-13 to get right.
    //
    // **Reopening a source is cheap**, which is what makes it eligible at all.
    // `KeySource` shares one immutable `Plan` across opens, so an open is a
    // refcount bump and a fresh cursor. Per reopen, 8 keys over a shared domain:
    //
    // ```text
    //   chunks/key      lazy                 resident
    //           12      ~61 ns / 1.06 allocs    ~45 ns / 1
    //          125      ~61 ns / 1.10 allocs    ~45 ns / 1
    //         1250      ~61 ns / 1.49 allocs    ~45 ns / 1
    // ```
    //
    // Flat in chunk count, within ~1.4x of an `Arc` bump. **An earlier version
    // of this comment recorded ~0.8 / 3.8 / 35 us and O( chunks ), because
    // `open` rebuilt the plan every time; that measurement is dead and must not
    // be quoted.** It was true of the code as written and stopped being true
    // when the plan became shared, which is the hazard of pricing a design
    // rather than a property.
    //
    // **Being cheap to reopen is necessary and not sufficient.** Until
    // `ChunkSource::occupancy` existed, `occupancy_of` returned `None` for a
    // source and `compute_segments` declined for every union containing one --
    // so listing it here bought nothing and cost a wasted span collection.
    // `KeySource` now enumerates its prefixes from its plan, which is what makes
    // segmentation reachable at all.
    //
    // **Size is the remaining gate, and it is enforced in `occupancy_of`, not
    // here.** Segmentation's setup is a flat +128 allocations per evaluation,
    // so it loses 2-3x on small operands and wins up to 1.32x on large ones;
    // `SEGMENT_MIN_CHUNKS` carries the measured crossover. Putting that test
    // here instead would make this predicate answer a question its name does not
    // ask -- a source is cheap to reopen at *any* size, and whether segmenting
    // it pays is a different question with a different answer.
    //
    // Construction and figures: JOURNAL 2026-09-13, under `reopen-cost`.
    matches!(
        e,
        Expr::Set(_) | Expr::Range(_, _) | Expr::Empty | Expr::Source(_)
    )
}

/// Would segmentation have anything to work with, judging by spans alone?
///
/// Cuts the domain at the span endpoints and asks what fraction is covered by
/// segments with exactly one contributor. `O(k log k)` in the operand count,
/// touching no chunk data.
fn span_coverage_is_promising(spans: &[(Prefix48, Prefix48)]) -> bool {
    let mut cuts: Vec<Prefix48> = Vec::with_capacity(spans.len() * 2);
    for (lo, hi) in spans {
        cuts.push(*lo);
        cuts.push(hi.saturating_add(1));
    }
    cuts.sort_unstable();
    cuts.dedup();

    let (mut single, mut total) = (0u64, 0u64);
    for w in cuts.windows(2) {
        let (lo, hi) = (w[0], w[1] - 1);
        let n = spans.iter().filter(|(a, b)| *a <= hi && *b >= lo).count();
        if n == 0 {
            continue;
        }
        let width = hi.saturating_sub(lo).saturating_add(1);
        total = total.saturating_add(width);
        if n == 1 {
            single = single.saturating_add(width);
        }
    }
    total > 0 && single.saturating_mul(2) >= total
}

/// The segmentation itself: cut points, contributor attribution, fusion.
///
/// Extracted so the tests assert against **the planner's own computation**
/// rather than a re-implementation of it. A test that recomputes the logic it is
/// checking agrees with itself no matter what the planner does.
fn compute_segments(parts: &[&Expr]) -> Option<Vec<(Prefix48, Prefix48, Vec<usize>)>> {
    if parts.len() < 2 || !parts.iter().all(|e| cheap_to_reopen(e)) {
        return None;
    }
    // Decline before opening anything, when the outcome is already settled.
    //
    // `occupancy_of` returns `None` for a source outside its bounds, and this
    // function requires an occupancy from **every** operand -- so such a union
    // is already destined to decline. Without this it declines anyway, but only
    // after the span collection below has opened every operand and thrown the
    // result away. That is not a new policy; it is declining to pay for a
    // conclusion already reached.
    //
    // **Mirrors `occupancy_of`'s source arm exactly**, including the upper
    // cutoff and the unreported case. If that gate changes, change this with it
    // -- a short-circuit that is merely *close* to the condition it anticipates
    // would silently start declining unions that should have segmented.
    //
    // Deliberately only `Expr::Source`, where `chunk_count()` is `O( 1 )` off
    // the cached plan. A resident `Expr::Set` keeps its existing behaviour,
    // which this work never measured.
    if parts.iter().any(|e| match e {
        Expr::Source(src) => match src.chunk_count() {
            None => true,
            Some(n) => n < SEGMENT_MIN_CHUNKS || n > crate::stream::plan::effective_cutoff(),
        },
        _ => false,
    }) {
        return None;
    }
    // Spans come from the opened streams, not from the tree: this has to reflect
    // what actually backs each operand.
    //
    // **Taking them from `plan::prefix_span` instead was tried on 2026-09-07
    // and reverted, because it is neutral.** The theory was that this line is
    // the `O( total chunks )` setup that `segmentation-setup-is-unbounded`
    // blames for that benchmark's flat loss, since every operand is opened here
    // and thrown away. A/B/A on `segmentation_boundary` refuses it: no case
    // improved, and `k=16/c=1000` was reproducibly **3.4% worse** while the two
    // baseline runs differed by 0.3%. `OrdSet::min` / `max` are O(1) — first and
    // last container — so both spellings are O(1) per operand and there was
    // never a walk here to remove.
    //
    // Which also refutes that item's diagnosis: the decline path is O(k) and
    // cannot produce a loss proportional to chunk count. Do not retry this as
    // an optimization; whatever the benchmark is paying for is elsewhere.
    let spans: Vec<(Prefix48, Prefix48)> = parts
        .iter()
        .map(|e| e.open_planned().stats().prefix_span)
        .collect::<Option<Vec<_>>>()?;

    // Decline from spans alone, before any occupancy is built.
    //
    // This pre-check is not an optimization of the decline path, it *is* the
    // decline path's cost. `compute_segments` runs on every `open_planned()` —
    // every execution — and building an occupancy per operand plus running
    // per-segment attribution ( each `any_in` scanning a bucket window ) costs
    // more than the union it was trying to improve. Measured against a plain
    // `UnionAll` over identical streams, `k` interleaved sets: **1.99x slower at
    // k=8, 1.79x at k=16, 1.76x at k=32** — all of it spent deciding not to
    // segment.
    //
    // Sound to check here because occupancy only ever *removes* contributors, so
    // the span-only coverage is a lower bound on what the full computation would
    // find. A case that passes this may still be declined below; one that fails
    // it could never have passed.
    if !span_coverage_is_promising(&spans) {
        return None;
    }

    // Occupancy refines what spans can only bound. A span is a min and a max,
    // so an operand whose chunks are clustered at both ends is credited with the
    // whole gap between them, and every segment there is merged for nothing.
    // Bucketed occupancy is exact at its resolution and — crucially — exact in
    // the direction that *drops* a contributor.
    let domain = (
        spans.iter().map(|(lo, _)| *lo).min()?,
        spans.iter().map(|(_, hi)| *hi).max()?,
    );
    let bucketing = Bucketing::covering(domain.0, domain.1);
    // Required for *every* operand, not best-effort per operand.
    //
    // A per-operand fallback would mean an attribution branch for "this one has
    // no statistics", and that branch is unreachable in any test: occupancy is
    // unavailable only for a `Set` above `STATS_MAX_CHUNKS`, a million
    // chunks. An untestable branch on the path that *drops* contributors is
    // exactly where a silent wrong answer would live — inverting it to
    // `is_some_and` dropped operands wholesale and no test noticed. Declining
    // segmentation outright is one early return, and it costs only the case
    // where re-opening a millon-chunk operand per segment was dubious anyway.
    let occ: Vec<PrefixOccupancy> = parts
        .iter()
        .map(|e| occupancy_of(e, bucketing))
        .collect::<Option<Vec<_>>>()?;

    // Cut points: each span's start, one past each span's end, and every place
    // an operand's occupancy starts or stops.
    let mut cuts: Vec<Prefix48> = Vec::with_capacity(spans.len() * 2);
    for (lo, hi) in &spans {
        cuts.push(*lo);
        cuts.push(hi.saturating_add(1));
    }
    for o in &occ {
        cuts.extend(o.transitions());
    }
    cuts.retain(|c| *c >= domain.0 && *c <= domain.1.saturating_add(1));
    cuts.sort_unstable();
    cuts.dedup();
    if cuts.len() < 2 {
        return None;
    }

    let mut segments: Vec<(Prefix48, Prefix48, Vec<usize>)> = Vec::new();
    for w in cuts.windows(2) {
        let (lo, hi) = (w[0], w[1] - 1);
        let who: Vec<usize> = spans
            .iter()
            .enumerate()
            .filter(|(i, (a, b))| {
                // The span test is the sound floor; occupancy may then drop an
                // operand that provably has no chunk in this window.
                //
                // Note every occupancy transition is a cut point, so within a
                // segment an operand's bucket occupancy is **uniform** — which
                // is why `any_in` over the window and a test of any single
                // bucket in it agree. Anything that adds cut points must keep
                // that true, or `any_in` becomes the only correct form.
                *a <= hi && *b >= lo && occ[*i].any_in(lo, hi)
            })
            .map(|(i, _)| i)
            .collect();
        if !who.is_empty() {
            segments.push((lo, hi, who));
        }
    }

    // Adjacent segments with the same contributors are one segment. Without
    // this, occupancy transitions inside a single-contributor stretch would
    // shred it into a segment per bucket — more re-opens than the merge cost.
    let mut fused: Vec<(Prefix48, Prefix48, Vec<usize>)> = Vec::with_capacity(segments.len());
    for seg in segments {
        match fused.last_mut() {
            Some(prev) if prev.2 == seg.2 && prev.1.saturating_add(1) == seg.0 => prev.1 = seg.1,
            _ => fused.push(seg),
        }
    }
    let segments = fused;

    // Each segment re-opens its contributors, so a plan with many segments costs
    // more than the merge it replaces. Occupancy is capped at
    // `OCCUPANCY_BUCKETS`, which bounds this — the check is the explicit form of
    // that bound.
    if segments.len() > MAX_SEGMENTS {
        return None;
    }

    // Only worth the machinery if some segment escapes the merge entirely.
    // Segmentation pays only where a segment escapes the merge, and each
    // segment re-opens its contributors — so the single-contributor regions must
    // cover enough of the domain to earn that back.
    //
    // The gate used to be "at least one segment has a single contributor",
    // which is far too weak. Operands staggered by one chunk — `k` interleaved
    // sets over a shared interval — make the first and last few chunks
    // single-sided, and those handful of chunks then licensed segmentation
    // across the *whole* domain, where every interior segment holds all `k`
    // contributors and is re-opened per segment. Measured against a plain
    // `UnionAll` over the identical streams: **1.99x slower at k=8, 1.79x at
    // k=16, 1.76x at k=32**, same answers.
    let total: u64 = segments
        .iter()
        .map(|(lo, hi, _)| hi.saturating_sub(*lo).saturating_add(1))
        .sum();
    let single: u64 = segments
        .iter()
        .filter(|(_, _, who)| who.len() == 1)
        .map(|(lo, hi, _)| hi.saturating_sub(*lo).saturating_add(1))
        .sum();
    if segments.len() < 2 || total == 0 || single * 2 < total {
        return None;
    }

    Some(segments)
}

/// Lower a union whose parts cannot share a chunk to [`Concat`], skipping the
/// merge entirely.
///
/// # The test is on prefixes, not on ordinals
///
/// `Concat`'s precondition is that *every prefix of the left is below every
/// prefix of the right*, so the test is `π(max a) < π(min b)` — never
/// `max a < min b`. Two operands can be disjoint as sets while sharing a chunk:
/// `{0, 2}` and `{3, 5}` are ordinal-disjoint and ordinal-ordered, and both live
/// in prefix 0. Concatenating them emits prefix 0 twice, which every operator
/// above silently mis-merges rather than rejecting. The prefix form declines
/// that case; the ordinal form would not.
///
/// # Why this is not `segmented_or` with a cheaper gate
///
/// It is strictly more general in what it accepts, and strictly cheaper to
/// decide:
///
/// - It works on **composite** operands. `segmented_or` re-opens each operand
///   once per segment, so `cheap_to_reopen` confines it to leaves. Here every
///   part is opened exactly once, in place, so `Or( And(a,b), And(c,d) )` over
///   prefix-disjoint halves is lowered too — that pairing pays a full merge
///   today.
/// - It costs a [`plan::prefix_span`] walk and a sort: no occupancy map, no
///   speculative opens. `compute_segments` builds a `PrefixOccupancy` per
///   operand *before* deciding, which is what made the decline path cost more
///   than the union it was improving.
///
/// It subsumes `segmented_or`'s all-single-contributor case, which is why it is
/// tried first, and it declines everything else — a union with any genuine
/// overlap is still segmentation's problem.
fn concat_disjoint_or(parts: &[&Expr]) -> Option<BoxedStream> {
    if parts.len() < 2 {
        return None;
    }
    // `None` means "provably empty", which is droppable rather than disqualifying
    // — but declining is one line and the planner's identity rules have already
    // removed such operands from anything that went through `plan()`.
    let mut spans: Vec<(Prefix48, Prefix48, usize)> = Vec::with_capacity(parts.len());
    for (i, e) in parts.iter().enumerate() {
        let (lo, hi) = crate::stream::plan::prefix_span(e)?;
        spans.push((lo, hi, i));
    }
    spans.sort_unstable();
    // Strict: touching spans share a chunk, and a shared chunk must be merged.
    if spans.windows(2).any(|w| w[0].1 >= w[1].0) {
        return None;
    }

    let mut it = spans.into_iter().map(|(_, _, i)| parts[i].open_planned());
    let mut acc = it.next()?;
    for s in it {
        acc = Box::new(Concat::new(acc, s));
    }
    Some(acc)
}

/// Split a union into prefix segments, so each is merged only where it must be.
///
/// # The interleaving problem
///
/// A union of operands whose chunk ranges overlap partially is charged for a
/// full merge across the whole domain, even though most of that domain has only
/// one contributor. `Or( small_set, huge_range_above_it )` is the extreme case:
/// the merge peeks both sides once per prefix of the range and does not finish,
/// to produce a result that is simply one stream after the other.
///
/// # The simplification
///
/// Cut the prefix domain at every point where the set of contributing operands
/// changes — the operands' span endpoints. Within a segment the contributor set
/// is constant, so each segment is either:
///
/// - **one contributor** — a pass-through, no merge, and its `cardinality` is
///   the operand's own ( `O(1)` for a range ); or
/// - **several** — a real merge, but confined to the region that needs it.
///
/// The segments are ordered and disjoint by construction, so [`Concat`]
/// reassembles them with no comparisons at all. Splitting and concatenating are
/// inverses here, which is what makes the rewrite obviously meaning-preserving:
/// every chunk lands in exactly one segment.
///
/// Returns `None` when segmentation cannot pay — unknown spans, operands too
/// expensive to re-open, or too little of the domain escapes the merge.
fn segmented_or(parts: &[&Expr]) -> Option<BoxedStream> {
    let segments = compute_segments(parts)?;

    let mut built: Vec<BoxedStream> = Vec::with_capacity(segments.len());
    for (lo, hi, who) in &segments {
        let mut opened: Vec<BoxedStream> = who
            .iter()
            .map(|i| {
                let s: BoxedStream = Box::new(Restrict::new(parts[*i].open_planned(), *lo, *hi));
                s
            })
            .collect();
        built.push(if opened.len() == 1 {
            opened.pop().unwrap()
        } else if use_nary_union(&opened) {
            Box::new(crate::stream::nary::UnionAll::new(opened))
        } else {
            let mut it = opened.into_iter();
            let mut acc: BoxedStream = Box::new(it.next().unwrap().or(it.next().unwrap()));
            for s in it {
                acc = Box::new(acc.or(s));
            }
            acc
        });
    }

    // Segments are ordered and disjoint, so this needs no merge.
    let mut it = built.into_iter();
    let mut acc = it.next()?;
    for s in it {
        acc = Box::new(Concat::new(acc, s));
    }
    Some(acc)
}

/// Should a flattened `Or` go through the shared n-ary accumulator?
///
/// # Decided from the opened streams, not from the tree
///
/// The old rule was `parts.len() >= 3`, which is a property of the *expression*.
/// It is right on average and wrong whenever the operands are not what the shape
/// suggests: three streams of two chunks each do not need an 8 KiB scratch
/// accumulator, and two streams of a million chunks each might. Worse, a leaf can
/// be a `BoxedStream` produced elsewhere, about which the tree knows nothing at
/// all.
///
/// `UnionAll` pays a fixed setup ( the scratch buffer and a cursor per input )
/// and wins by not materializing an intermediate container per fold. So it earns
/// its keep when there is real per-fold work to avoid: enough inputs, and enough
/// chunks flowing through them.
pub(crate) fn use_nary_union(streams: &[BoxedStream]) -> bool {
    if streams.len() < 3 {
        // At k=2 the pairwise kernel measured 0.4x the accumulator; there is no
        // fold to eliminate.
        return false;
    }
    let total: u64 = streams
        .iter()
        .map(|s| s.stats().chunks.unwrap_or(u64::MAX / 64))
        .fold(0u64, |a, b| a.saturating_add(b));
    // Below this the pairwise folds are cheaper than the accumulator's setup.
    // The threshold is deliberately low: the measured crossover is a handful of
    // chunks, and being wrong here costs a constant, not an asymptote.
    total >= 8
}

/// A re-openable source of chunks, for a leaf whose payloads are not resident.
///
/// # Why a factory rather than a stream
///
/// [`Expr`] is `Clone + Debug`, and an expression is opened **more than once**:
/// segmentation opens an operand per segment, and `cardinality()` and
/// `collect_set()` each open the tree. A [`ChunkStream`] is a stateful cursor
/// and is none of those things, so a lazy leaf must be something that *makes*
/// streams rather than something that is one.
///
/// # Why the statistics live on the trait
///
/// [`crate::stream::plan`] rewrites an expression from statistics read off its
/// leaves, and until this trait existed **every leaf was a materialized
/// `OrdSet`**, so the planner read `chunk_count`, the span and the cardinality
/// straight off it. These three methods are what keep that working once a leaf
/// is lazy. A source that answers them plans as well as a resident set; one that
/// returns `None` is still correct and merely opaque, and the planner falls back
/// to what it does for any operand it cannot summarize.
///
/// **All three must be answerable without decoding a payload.** A source that
/// opened a stream to count its chunks would make planning cost what evaluation
/// costs, which is exactly what a lazy leaf exists to avoid.
pub trait ChunkSource: std::fmt::Debug + Send + Sync {
    /// A fresh stream over this source's contents.
    ///
    /// Must be repeatable: two calls yield the same chunks in the same order.
    /// A source that cannot open returns [`super::ErrStream`] rather than an
    /// empty stream, so the failure is reported and not silently swallowed.
    fn open(&self) -> BoxedStream;

    /// Chunks the source will yield, if known without opening one.
    fn chunk_count(&self) -> Option<u64> {
        None
    }

    /// Inclusive prefix span, if known without opening one.
    fn prefix_span(&self) -> Option<(Prefix48, Prefix48)> {
        None
    }

    /// Total ordinals, if known without decoding a payload.
    fn cardinality(&self) -> Option<u64> {
        None
    }

    /// Bucketed occupancy over `b`, if the prefixes can be enumerated without
    /// decoding a payload.
    ///
    /// # Why this and not a prefix iterator
    ///
    /// [`PrefixOccupancy::from_prefixes`] is generic over its iterator, so it
    /// cannot be called across a `dyn` boundary; handing back a boxed iterator
    /// or a `Vec` would allocate per planning call for a summary that is 256
    /// bits. The source builds it directly instead.
    ///
    /// # This is what gates segmentation
    ///
    /// `compute_segments` requires an occupancy for **every** operand and
    /// declines outright if any is missing, so a source returning `None` here
    /// makes a whole union unsegmentable. Returning a *wrong* one is far worse
    /// than returning `None`: occupancy decides which contributors a segment
    /// drops, so an overstatement merges for nothing and an understatement
    /// drops an operand that had chunks there. It must be exact at the
    /// bucketing's resolution, which means real prefixes -- never a span
    /// widened to look like one.
    fn occupancy(&self, b: crate::stream::sketch::Bucketing) -> Option<PrefixOccupancy> {
        let _ = b;
        None
    }

    // **There is deliberately no `backing()` here.** The first version had one
    // and nothing read it: a public trait method with a single caller, and that
    // caller a test. `StreamStats::backing` is where the planner actually reads
    // this, off the *opened stream*, which is the dynamic half that
    // `StreamStats`'s own docs call authoritative -- so a second copy on the
    // factory could only duplicate it or contradict it.
    //
    // ( It was also expensive when removed, costing a `KeySource` a full index
    // scan to return one of three enum values. That is no longer true -- the
    // plan is shared now, so it would be O( 1 ) -- and the reason it is recorded
    // as history rather than as a reason is that the removal never depended on
    // it. Nothing reading it is sufficient on its own. )
}

/// A set expression built at runtime.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum Expr {
    Set(Arc<OrdSet>),
    /// A lazy leaf: chunks produced on demand by a [`ChunkSource`].
    ///
    /// The planner reads its statistics through the trait rather than off a
    /// resident set. `Snapshot::key_expr` builds one over a single key.
    Source(Arc<dyn ChunkSource>),
    Range(u64, u64),
    Empty,
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Xor(Box<Expr>, Box<Expr>),
    AndNot(Box<Expr>, Box<Expr>),
    /// Complement of the inner expression within `[lo, hi)`.
    Not(Box<Expr>, u64, u64),
}

impl Expr {
    pub fn set(s: impl Into<Arc<OrdSet>>) -> Expr {
        Expr::Set(s.into())
    }

    pub fn and(self, rhs: Expr) -> Expr {
        Expr::And(Box::new(self), Box::new(rhs))
    }
    pub fn or(self, rhs: Expr) -> Expr {
        Expr::Or(Box::new(self), Box::new(rhs))
    }
    pub fn xor(self, rhs: Expr) -> Expr {
        Expr::Xor(Box::new(self), Box::new(rhs))
    }
    /// Complement within `[lo, hi)`.
    ///
    /// # Why this is a variant and not sugar for `AndNot(Range, x)`
    ///
    /// It was sugar, and the lowering is still exact — the tree really can say
    /// the same thing. But `AndNot` drives its cardinality loop from the left
    /// operand, so `AndNot(Range(0, u64::MAX), x)` counts by stepping `2^48`
    /// prefixes. Desugaring made [`Expr::Not`] a hang rather than a subtraction.
    ///
    /// A distinct variant lets `open` build [`crate::stream::Not`], which counts
    /// by walking `x` instead. The cost is the extra match arm — including in
    /// `flatten_or`, where the existing catch-all already handles it correctly.
    pub fn not_in(self, lo: u64, hi: u64) -> Expr {
        Expr::Not(Box::new(self), lo, hi)
    }

    pub fn and_not(self, rhs: Expr) -> Expr {
        Expr::AndNot(Box::new(self), Box::new(rhs))
    }

    /// Test hook for the `Or`-flattening, which is otherwise private.
    #[cfg(test)]
    pub(crate) fn flatten_or_for_test<'a>(&'a self, out: &mut Vec<&'a Expr>) {
        self.flatten_or(out)
    }

    /// Collect the leaves of a contiguous `Or` subtree, left to right.
    ///
    /// Order is preserved so the result is deterministic, though union does not
    /// depend on it.
    fn flatten_or<'a>(&'a self, out: &mut Vec<&'a Expr>) {
        match self {
            Expr::Or(a, b) => {
                a.flatten_or(out);
                b.flatten_or(out);
            }
            other => out.push(other),
        }
    }

    /// Rewrite into a cheaper equivalent form. See [`crate::stream::plan`].
    ///
    /// [`Expr::open`] applies this already; it is public because a planner that
    /// cannot be inspected cannot be trusted, and the tests compare planned
    /// against unplanned evaluation.
    pub fn plan(&self) -> Expr {
        crate::stream::plan::plan(self)
    }

    /// Plan with a chosen backend. See [`crate::stream::plan::PlanStrategy`].
    pub fn plan_with(&self, s: &dyn crate::stream::plan::PlanStrategy) -> Expr {
        s.plan(self)
    }

    /// Instantiate as a lazy stream.
    ///
    /// Plans first. Without it the cost of an expression depends on which of
    /// several equivalent spellings the caller reached for — `AndNot(Range, x)`
    /// ran for more than five seconds where the identical `!x` took 199 µs.
    pub fn open(&self) -> BoxedStream {
        self.plan().open_planned()
    }

    /// Lower without planning. `open` is almost always what you want.
    pub fn open_planned(&self) -> BoxedStream {
        match self {
            Expr::Set(s) => Box::new(SetStream::new(s.clone())),
            // The factory decides what to hand back, including an `ErrStream`
            // when it cannot open. Nothing here inspects the result: a lazy leaf
            // is opaque by construction, which is the whole point of the trait.
            Expr::Source(src) => src.open(),
            Expr::Range(lo, hi) => Box::new(RangeStream::new(*lo, *hi)),
            Expr::Empty => Box::new(EmptyStream),
            Expr::And(a, b) => Box::new(a.open_planned().and(b.open_planned())),
            // A chain of ORs is flattened and routed through the n-ary
            // accumulator, which is what the design asks for and what nothing
            // did: `UnionAll` was written, unit-tested, and reachable from no
            // public entry point at all.
            //
            // Folding pairwise materializes an intermediate container per
            // chunk per fold. Measured over 400 chunks, against one reusable
            // 8 KiB scratch:
            //
            // ```text
            //   k      allocations        time
            //   2      5 ->      4      1.6x
            //   3    409 ->      6      3.4x
            //   4    813 ->      7      4.4x
            //   8   2429 ->     11      7.9x
            //  16   5661 ->     19     10.1x
            // ```
            //
            // Hence the threshold of three, which is the design's: at two the
            // pairwise kernel is within noise and avoids the accumulator's
            // fixed cost.
            Expr::Or(_, _) => {
                let mut parts = Vec::new();
                self.flatten_or(&mut parts);
                debug_assert!(parts.len() >= 2, "an Or node has at least two leaves");
                // Open first, then decide. The children are the only things that
                // know what backs them, and `Expr` cannot: a leaf may be a
                // `BoxedStream` from elsewhere, and a chunk decoded from a page
                // is not a chunk read from a `Vec`. This is the dynamic half of
                // planning — `plan()` rewrote the tree on static information,
                // and this chooses the physical operator on measured ones.
                // Prefix-disjointness first: it is the cheapest question to ask
                // and the strongest answer to get, since it removes the merge
                // over the *whole* domain rather than over one segment of it.
                if let Some(s) = concat_disjoint_or(&parts) {
                    return s;
                }
                // Then segmentation, which can remove the merge for the parts of
                // the domain that have only one contributor.
                if let Some(s) = segmented_or(&parts) {
                    return s;
                }
                let opened: Vec<BoxedStream> = parts.iter().map(|e| e.open_planned()).collect();
                if use_nary_union(&opened) {
                    Box::new(crate::stream::nary::UnionAll::new(opened))
                } else {
                    let mut it = opened.into_iter();
                    let (a, b) = (it.next().unwrap(), it.next().unwrap());
                    let mut acc: BoxedStream = Box::new(a.or(b));
                    for s in it {
                        acc = Box::new(acc.or(s));
                    }
                    acc
                }
            }
            Expr::Xor(a, b) => Box::new(a.open_planned().xor(b.open_planned())),
            Expr::AndNot(a, b) => Box::new(a.open_planned().and_not(b.open_planned())),
            Expr::Not(a, lo, hi) => Box::new(a.open_planned().not_in_range(*lo, *hi)),
        }
    }

    /// Cardinality without materializing any intermediate.
    pub fn cardinality(&self) -> Result<u64> {
        self.plan().count()
    }

    /// Count an already-planned expression, using the cardinality identities
    /// where they are cheaper than a merge.
    ///
    /// # Why counting is not just "open and drain"
    ///
    /// `Or` / `Xor` answer cardinality by merging, which steps once per prefix
    /// **of both operands**. That is fine until one operand is enormous and the
    /// other is not: `Or( small, huge_range ).cardinality()` costs a step per
    /// chunk of the range and does not finish, to produce a number that
    ///
    /// ```text
    ///   |A ∪ B| = |A| + |B| - |A ∩ B|
    ///   |A ⊕ B| = |A| + |B| - 2·|A ∩ B|
    /// ```
    ///
    /// gives directly — a `Range` counts its own cardinality by subtraction, and
    /// `And` is seek-driven so the intersection costs `O(min)`. These are the
    /// same identities `ops::card` has always applied at the *container* level;
    /// the stream level simply never used them.
    ///
    /// # Gated, and the gate is load-bearing
    ///
    /// For a **binary** `Or` of similar-sized sets the decomposition is actually
    /// the faster route — the three traversals are not comparable work, since
    /// counting a `SetStream` sums cached container lengths and touches no
    /// payload while a merge pays a peek, a compare and a kernel call per
    /// prefix:
    ///
    /// ```text
    ///   operands        merge      decomposed
    ///   10 chunks      1.075 µs      532 ns
    ///   1 000          45.2 µs      43.0 µs
    ///   100 000        4.997 ms     5.049 ms
    /// ```
    ///
    /// That measurement is a trap, and I fell in it: on the strength of it I
    /// removed the gate, and an **8-way** `Or` went to 27 538 allocations over
    /// 400 chunks. A chain decomposes recursively into nested `And`s, whose
    /// intermediates multiply, and it also defeats the `Or`-flattening that
    /// routes three or more leaves through the shared `UnionAll` accumulator.
    /// `a_chain_of_ors_uses_the_nary_accumulator_not_a_pairwise_fold` caught it.
    ///
    /// So the gate stays: decompose only where the cost model says the merge is
    /// the more expensive route, which is exactly the case where one operand is
    /// vastly cheaper to *count* than to *traverse* — a range.
    fn count(&self) -> Result<u64> {
        let (a, b) = match self {
            Expr::Or(a, b) | Expr::Xor(a, b) => (a, b),
            _ => return self.open_planned().cardinality_dyn(),
        };
        let inter = Expr::And(a.clone(), b.clone());
        if !decomposing_is_cheaper(a, b, &inter) {
            return self.open_planned().cardinality_dyn();
        }
        // `u128` throughout: `|A| + |B|` can exceed `u64::MAX` when both operands
        // approach the universe, even though the result never does.
        let na = a.count()? as u128;
        let nb = b.count()? as u128;
        let ni = inter.plan().count()? as u128;
        let total = match self {
            Expr::Or(_, _) => na + nb - ni,
            _ => na + nb - 2 * ni,
        };
        debug_assert!(total <= u64::MAX as u128);
        Ok(total as u64)
    }

    pub fn collect_set(&self) -> Result<OrdSet> {
        self.open().collect_set()
    }
}

/// `!expr` — complement over the whole universe `[0, ORDINAL_MAX]`.
///
/// Spelled as the real `std::ops::Not` rather than an inherent `not()` method,
/// which is both idiomatic and what clippy's `should_implement_trait` asks for.
/// Well-defined only because of invariant I8; see [`crate::ORDINAL_MAX`].
///
/// Complementing over the whole universe spans nearly `2^48` chunks. This is
/// meant to be *counted* — `(!e).cardinality()` walks the input, not the
/// universe — not collected.
impl std::ops::Not for Expr {
    type Output = Expr;

    fn not(self) -> Expr {
        self.not_in(0, u64::MAX)
    }
}

#[cfg(test)]
pub(crate) fn use_nary_union_for_test(streams: &[BoxedStream]) -> bool {
    use_nary_union(streams)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(vals: &[u64]) -> Expr {
        Expr::set(Arc::new(OrdSet::from_iter_unsorted(vals.iter().copied())))
    }

    #[test]
    fn dynamic_tree_matches_static_composition() {
        let a: Vec<u64> = (0..800u64).map(|i| i * 3).collect();
        let b: Vec<u64> = (0..800u64).map(|i| i * 5).collect();
        let c: Vec<u64> = (0..800u64).map(|i| i * 7).collect();

        let e = s(&a).and(s(&b).or(s(&c)));
        let got = e.collect_set().unwrap();

        let (ea, eb, ec) = (
            OrdSet::from_iter_unsorted(a.iter().copied()),
            OrdSet::from_iter_unsorted(b.iter().copied()),
            OrdSet::from_iter_unsorted(c.iter().copied()),
        );
        assert_eq!(
            got.iter().collect::<Vec<_>>(),
            ea.and(&eb.or(&ec)).iter().collect::<Vec<_>>()
        );
    }

    #[test]
    fn boxed_cardinality_uses_the_override_not_the_default() {
        // If the BoxedStream delegation were missing, this would still be
        // *correct* but would materialize — so assert equality and rely on the
        // allocation regression test for the performance property.
        let a: Vec<u64> = (0..2000u64).map(|i| i * 11).collect();
        let b: Vec<u64> = (0..2000u64).map(|i| i * 13).collect();
        let e = s(&a).and(s(&b));
        assert_eq!(e.cardinality().unwrap(), e.collect_set().unwrap().len());
    }

    #[test]
    fn range_and_empty_literals() {
        let e = Expr::Range(0, 100).and(Expr::Range(50, 200));
        assert_eq!(e.cardinality().unwrap(), 50);
        assert_eq!(Expr::Empty.cardinality().unwrap(), 0);
        assert_eq!(
            Expr::Range(0, 10).and(Expr::Empty).cardinality().unwrap(),
            0
        );
        assert_eq!(
            Expr::Range(0, 10).or(Expr::Empty).cardinality().unwrap(),
            10
        );
    }

    #[test]
    fn deeply_nested_expression() {
        let e = s(&[1, 2, 3, 4, 5])
            .and_not(s(&[2]))
            .or(s(&[100]))
            .xor(s(&[3, 100]));
        // {1,3,4,5} | {100} = {1,3,4,5,100}; xor {3,100} = {1,4,5}
        assert_eq!(
            e.collect_set().unwrap().iter().collect::<Vec<_>>(),
            vec![1, 4, 5]
        );
        assert_eq!(e.cardinality().unwrap(), 3);
    }
}

#[cfg(test)]
mod segment_tests {
    use super::*;
    use crate::stream::ChunkStream;
    use crate::OrdSet;
    use std::sync::Arc;

    fn chunks_at(it: impl Iterator<Item = u64>) -> Expr {
        chunks_off(it, 0)
    }

    /// Chunks carrying a distinguishing low value.
    ///
    /// `chunks_at` puts the *same* ordinal in every chunk at a given prefix,
    /// so two operands sharing a prefix hold identical chunks — and dropping one
    /// of them loses nothing observable. A boundary bug that excluded a
    /// contributor from a shared segment therefore passed every test built on
    /// `chunks_at`, and was caught only by the property test. Use this wherever
    /// the point is *which* operand contributed.
    fn chunks_off(it: impl Iterator<Item = u64>, off: u64) -> Expr {
        Expr::set(Arc::new(OrdSet::from_sorted_slice(
            &it.map(|i| (i << 16) | off).collect::<Vec<_>>(),
        )))
    }

    /// Splitting and concatenating are inverses: every chunk lands in exactly
    /// one segment, so the union must equal a plain pairwise merge — same
    /// ordinals, same order, no duplicates.
    ///
    /// The reference is built by folding `ChunkStreamExt::or` over the parts
    /// **directly**, not via `Expr`. Segmentation lives inside `open_planned`,
    /// so an earlier version of this helper compared `open()` against
    /// `open_planned()` — segmentation against itself. It passed every sabotage
    /// of the segmentation logic, which is precisely the failure mode this file
    /// warns about elsewhere: a check that cannot see its own subject.
    fn agrees_with_the_merge(parts: &[Expr]) {
        let e = parts
            .iter()
            .skip(1)
            .fold(parts[0].clone(), |a, b| a.or(b.clone()));
        let segmented = e.open().collect_set().unwrap();

        let mut it = parts.iter().map(|p| p.open_planned());
        let mut reference: BoxedStream = it.next().unwrap();
        for s in it {
            reference = Box::new(reference.or(s));
        }
        let merged = reference.collect_set().unwrap();

        assert_eq!(
            segmented.iter().collect::<Vec<_>>(),
            merged.iter().collect::<Vec<_>>(),
            "segmentation changed the result of {e:?}"
        );
        assert_eq!(e.cardinality().unwrap(), merged.len());
    }

    /// **The `segmentation_boundary` corpus declines, and this pins where.**
    ///
    /// Segmentation measured as a flat loss on that benchmark — +7.9% / +6.6% /
    /// +9.6% / +2.2% — and the entry recording it could not say whether the
    /// planner was firing and planning worse, or declining and charging for the
    /// decision. It declines, at the span pre-check, at every one of the four
    /// points. So the loss was entirely the cost of *deciding not to segment*,
    /// which is what justifies taking spans from the tree rather than from
    /// opened streams.
    ///
    /// Do not weaken this to "returns `None` sometimes". The corpus shape —
    /// every operand covering the same prefix range — is the thing under test,
    /// and a change that made it fire would silently reintroduce the loss.
    #[test]
    fn the_overlapping_corpus_declines_before_building_anything() {
        use std::sync::Arc;
        for (k, chunks) in [(2usize, 200u64), (16, 200)] {
            let exprs: Vec<Expr> = (0..k)
                .map(|j| {
                    let vals: Vec<u64> =
                        (0..chunks).map(|ch| (ch << 16) | (j as u64 * 7)).collect();
                    Expr::set(Arc::new(crate::OrdSet::from_sorted_slice(&vals)))
                })
                .collect();
            let parts: Vec<&Expr> = exprs.iter().collect();
            assert!(
                compute_segments(&parts).is_none(),
                "k={k}: every operand covers the domain, so no segment can have a \
                 single contributor and the pre-check must decline"
            );
            assert!(
                segmented_or(&parts).is_none(),
                "k={k}: and so must the plan"
            );
        }
    }

    #[test]
    fn segments_agree_with_the_merge_however_the_operands_interleave() {
        // Disjoint and ordered: one segment each, pure concatenation.
        agrees_with_the_merge(&[chunks_at(0..100), chunks_at(200..300)]);
        // Partially overlapping: three segments — a only, both, b only.
        agrees_with_the_merge(&[chunks_at(0..100), chunks_at(50..150)]);
        agrees_with_the_merge(&[chunks_off(0..100, 1), chunks_off(50..150, 2)]);
        // Spans that **touch at exactly one prefix**, with distinguishable
        // contents so that dropping either contributor is observable.
        agrees_with_the_merge(&[chunks_off(0..50, 1), chunks_off(49..100, 2)]);
        agrees_with_the_merge(&[chunks_off(49..100, 2), chunks_off(0..50, 1)]);
        // Adjacent but not touching.
        agrees_with_the_merge(&[chunks_off(0..50, 1), chunks_off(50..100, 2)]);
        // Nested: b entirely inside a's span.
        agrees_with_the_merge(&[chunks_off(0..100, 1), chunks_off(40..60, 2)]);
        // Identical spans: one segment, and segmentation must decline.
        agrees_with_the_merge(&[chunks_off(0..100, 1), chunks_off(0..100, 2)]);
        // Finely interleaved: spans overlap completely, so span-based cutting
        // cannot separate them and the merge stands. Still must be correct.
        agrees_with_the_merge(&[
            chunks_off((0..100).map(|i| i * 2), 1),
            chunks_off((0..100).map(|i| i * 2 + 1), 2),
        ]);
        // Three-way, mixed.
        agrees_with_the_merge(&[
            chunks_off(0..100, 1),
            chunks_off(50..150, 2),
            chunks_off(400..500, 3),
        ]);
        // With a range literal on one side.
        agrees_with_the_merge(&[chunks_at(0..50), Expr::Range(100 << 16, 200 << 16)]);
    }

    /// The disjointness test is on prefixes. An ordinal-level one is wrong.
    ///
    /// `{0, 2}` and `{3, 5}` are disjoint **and** ordered as sets — `max a < min
    /// b` — and they live in the same chunk. Concatenating them emits prefix 0
    /// twice, and every operator above a `Concat` assumes strictly increasing
    /// prefixes, so it mis-merges silently rather than failing.
    ///
    /// The assertion is on the decision function itself because the wrong answer
    /// is not visible in this union's own output: both halves are already in
    /// order, so `collect_set` agrees either way. What breaks is the *next*
    /// operator, which this test would have to construct to observe indirectly.
    #[test]
    fn ordinal_disjointness_within_one_chunk_is_not_prefix_disjointness() {
        let a = Expr::set(Arc::new(OrdSet::from_sorted_slice(&[0, 2])));
        let b = Expr::set(Arc::new(OrdSet::from_sorted_slice(&[3, 5])));
        assert!(
            concat_disjoint_or(&[&a, &b]).is_none(),
            "operands sharing chunk 0 were lowered to a concatenation"
        );
        // The positive control: the same values one chunk apart do concatenate,
        // so the decline above is about the shared chunk and not about the rule
        // being unreachable.
        let c = Expr::set(Arc::new(OrdSet::from_sorted_slice(&[
            (1 << 16) | 3,
            (1 << 16) | 5,
        ])));
        assert!(concat_disjoint_or(&[&a, &c]).is_some());
        // Order of the parts must not matter — the rule sorts.
        assert!(concat_disjoint_or(&[&c, &a]).is_some());
        // Touching spans share a chunk at the boundary and must decline too.
        let wide = chunks_off(0..50, 1);
        let over = chunks_off(49..100, 2);
        assert!(concat_disjoint_or(&[&wide, &over]).is_none());
    }

    /// A prefix-disjoint union of **composite** operands must not merge.
    ///
    /// This is the case segmentation structurally cannot reach: `cheap_to_reopen`
    /// confines `compute_segments` to leaves, because it re-opens each operand
    /// once per segment. So `Or( set, Not(set, ..) )` over disjoint prefix ranges
    /// falls through to a plain merge, which peeks both sides once per prefix —
    /// here 2^30 of them, for an answer that is a sum of two numbers each already
    /// known in a few steps.
    ///
    /// The timeout is the assertion: without the `Concat` lowering this does not
    /// finish, and with it the work is proportional to the *inputs* rather than to
    /// the complement's width. `open_planned` is called directly so the test
    /// pins the lowering and not whatever `plan()` happens to rewrite.
    #[test]
    fn a_prefix_disjoint_union_of_composites_is_not_merged() {
        use std::sync::mpsc;
        use std::time::Duration;

        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let low = chunks_off(0..10, 7);
            let hi_set = Expr::set(Arc::new(OrdSet::from_sorted_slice(&[
                1u64 << 46,
                (1u64 << 46) + 5,
                (1u64 << 47) - 2,
            ])));
            // Composite, and its bounds are its own window: prefixes 2^30..2^31.
            let complement = Expr::Not(Box::new(hi_set), 1 << 46, 1 << 47);
            let e = low.or(complement);
            let _ = tx.send(e.open_planned().cardinality_dyn());
        });

        let got = rx
            .recv_timeout(Duration::from_secs(20))
            .expect("a prefix-disjoint union was merged across 2^30 chunks")
            .expect("counting failed");
        // 10 chunks of one ordinal each, plus the complement of 3 ordinals over
        // a window of 2^46.
        assert_eq!(got, 10 + ((1u64 << 46) - 3));
    }

    /// Occupancy finds what a span structurally cannot: a **gap**.
    ///
    /// `a` occupies `[0,99]` and `[900,999]`, so its span is `(0, 999)` and
    /// span-based cutting credits it with the whole middle. `b` sits at
    /// `[400,499]`, entirely inside that gap.
    ///
    /// Segment *count* cannot show the difference — spans alone also give three
    /// segments here. What changes is the **contributor sets**: with occupancy
    /// every segment has exactly one contributor, so the union is merged
    /// nowhere at all. Without it, the middle segment merges `a` against a
    /// region where `a` has no chunk.
    #[test]
    fn occupancy_drops_an_operand_from_the_gap_in_its_own_span() {
        let a = chunks_off((0..100).chain(900..1000), 1);
        let b = chunks_off(400..500, 2);

        // Sound first: the refined plan must still equal a plain merge.
        agrees_with_the_merge(&[a.clone(), b.clone()]);

        let segs = compute_segments(&[&a, &b]).expect("segmentation should apply");
        assert!(
            segs.iter().all(|(_, _, who)| who.len() == 1),
            "occupancy did not drop `a` from the gap: {segs:?}"
        );
        // Both operands still appear — dropping one entirely would be a lost
        // result, not an optimization.
        assert!(segs.iter().any(|(_, _, who)| who == &[0]));
        assert!(segs.iter().any(|(_, _, who)| who == &[1]));
    }

    /// Finely interleaved operands must be left alone, not shredded.
    ///
    /// At single-prefix resolution these separate perfectly — into `2n`
    /// segments, each re-opening its operands, which is far worse than the merge
    /// it replaces. The bucket cap is what makes "both, everywhere" the answer.
    #[test]
    fn alternating_operands_are_not_fragmented() {
        let a = chunks_off((0..2000).map(|i| i * 2), 1);
        let b = chunks_off((0..2000).map(|i| i * 2 + 1), 2);
        agrees_with_the_merge(&[a.clone(), b.clone()]);

        match compute_segments(&[&a, &b]) {
            None => {} // declined outright, which is also correct
            Some(segs) => {
                assert!(
                    segs.len() <= MAX_SEGMENTS,
                    "alternating operands fragmented into {} segments",
                    segs.len()
                );
                // The interior must stay merged: at this bucket size every
                // bucket holds 8 even and 8 odd prefixes, so occupancy cannot
                // separate them and must not pretend to. Only the two extreme
                // prefixes are legitimately single-sided — 0 belongs to `a`
                // alone and 3999 to `b` alone, which the *span* cuts find, not
                // occupancy.
                let both = segs.iter().filter(|(_, _, who)| who.len() == 2).count();
                assert!(
                    both >= segs.len().saturating_sub(2),
                    "occupancy claimed to separate interleaved operands: {segs:?}"
                );
            }
        }
    }

    /// Segmentation must actually fire, or the test above is comparing the merge
    /// against itself.
    #[test]
    fn segmentation_fires_when_it_pays_and_declines_when_it_does_not() {
        // Disjoint spans: the whole union becomes a concatenation.
        let disjoint = chunks_at(0..100).or(chunks_at(200..300));
        assert!(segmented_or(&[&chunks_at(0..100), &chunks_at(200..300)]).is_some());
        let _ = disjoint;

        // Partially overlapping: still worth it — the two single-sided regions
        // escape the merge.
        assert!(segmented_or(&[&chunks_at(0..100), &chunks_at(50..150)]).is_some());

        // Identical spans: every segment has both contributors, so there is
        // nothing to save and the machinery would be pure overhead.
        assert!(segmented_or(&[&chunks_at(0..100), &chunks_at(0..100)]).is_none());

        // A composite operand is too expensive to re-open once per segment.
        let composite = chunks_at(0..100).and(chunks_at(50..150));
        assert!(segmented_or(&[&composite, &chunks_at(400..500)]).is_none());
    }

    /// The motivating case: a union whose merge cannot finish.
    ///
    /// `Or` peeks both sides once per prefix, so a huge operand makes counting
    /// `O(chunks of that operand)` even when it contributes to no shared region.
    /// Split into segments it is a sum. Run against a deadline rather than timed
    /// afterwards — a regression here does not get slower, it stops finishing.
    #[test]
    fn a_union_with_a_huge_disjoint_operand_is_counted_not_walked() {
        use std::sync::mpsc;
        use std::time::Duration;

        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let small = chunks_at(0..3000);
            let huge = Expr::Range(1u64 << 40, u64::MAX);

            // Disjoint: |small| + |huge|.
            let n = small.clone().or(huge.clone()).cardinality().unwrap();
            assert_eq!(n, 3000 + (u64::MAX - (1u64 << 40)));

            // Partially overlapping: the 2000 ordinals of `small` that already
            // sit inside the range must not be counted twice.
            let overlap = Expr::Range(1000 << 16, u64::MAX);
            let n = small.or(overlap).cardinality().unwrap();
            assert_eq!(n, 1000 + (u64::MAX - (1000u64 << 16)));
            let _ = tx.send(());
        });
        assert!(
            rx.recv_timeout(Duration::from_secs(30)).is_ok(),
            "the union is walking its huge operand instead of counting it"
        );
    }

    /// `Or( small, huge_range ).cardinality()` must not walk the range.
    ///
    /// `segmented_or` was built so `Or( small, huge_range ).cardinality()` would
    /// stop walking the range. It fixes the case where the small operand sits
    /// entirely outside the range, which is the only case
    /// `a_union_with_a_huge_disjoint_operand_is_counted_not_walked` covers. When
    /// the operand *straddles* the range's low end, no absorption fires and
    /// segmentation emits one merged segment holding both — and the `Or` inside
    /// that segment walked the range chunk by chunk, 2^40 of them. Segmentation
    /// alone could not fix this: the merged segment is *correct*, it is the
    /// counting strategy inside it that had to change.
    ///
    /// Fixed on 2026-08-26 by counting through the cardinality identity
    /// `|A ∪ B| = |A| + |B| - |A ∩ B|` instead of merging — see `Expr::count`.
    #[test]
    fn straddling_union_with_a_huge_range_terminates() {
        use std::sync::mpsc;
        use std::time::Duration;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let straddle = chunks_at([0u64, 1 << 40, (1 << 40) + 1].into_iter());
            let r = Expr::Range(1u64 << 32, u64::MAX);

            // Terminating is not enough — a wrong identity terminates too.
            // |A| = 3, of which two ordinals ( 2^56 and 2^56 + 2^16 ) already lie
            // inside the range and one ( 0 ) does not.
            let n = straddle.clone().or(r.clone()).cardinality().unwrap();
            assert_eq!(n, (u64::MAX - (1u64 << 32)) + 1, "union");
            // XOR removes the two shared ordinals from both sides.
            let x = straddle.clone().xor(r.clone()).cardinality().unwrap();
            assert_eq!(x, (u64::MAX - (1u64 << 32)) - 1, "xor");
            // And the intersection itself, which the identity leans on.
            assert_eq!(straddle.and(r).cardinality().unwrap(), 2, "intersection");
            let _ = tx.send(n);
        });
        assert!(
            rx.recv_timeout(Duration::from_secs(20)).is_ok(),
            "Or(straddling set, huge range) did not finish"
        );
    }

    #[test]
    fn restrict_clips_to_its_window() {
        let s = chunks_at(0..100).open_planned();
        let mut r = Restrict::new(s, 10, 19);
        let mut got = Vec::new();
        while let Some((p, _)) = r.next_chunk().unwrap() {
            got.push(p);
        }
        assert_eq!(got, (10..20).collect::<Vec<_>>());
    }

    #[test]
    #[should_panic(expected = "not prefix-disjoint and ordered")]
    fn concat_rejects_overlapping_operands_in_debug() {
        let a = chunks_at(0..10).open_planned();
        let b = chunks_at(5..15).open_planned();
        let mut c = Concat::new(a, b);
        while c.next_chunk().unwrap().is_some() {}
    }
}
