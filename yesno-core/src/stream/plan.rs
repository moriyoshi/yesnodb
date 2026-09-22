//! Expression planning: rewrite an [`Expr`] into a cheaper equivalent form.
//!
//! # Why a planner, and why these rules
//!
//! The cost of a stream expression is **the number of chunks it visits**, and
//! operators differ in whether they *visit* an operand or merely *seek* into it:
//!
//! | operator | left | right |
//! | --- | --- | --- |
//! | `And` | leapfrogs | leapfrogs |
//! | `AndNot` | visits | seeks |
//! | `Or` / `Xor` | visits | visits |
//! | `Not` | — | drives from the input for `cardinality` |
//!
//! That table is fine until an operand is a [`Expr::Range`] spanning the whole
//! universe, which under invariant I8 is `2^48` chunks. Then every "visits" cell
//! is unbounded. Measured against a 5 000-chunk set, before this module existed:
//!
//! ```text
//!   And(x, Range)          352 µs        AndNot(Range, x)     > 5 s
//!   AndNot(x, Range)       472 µs        Or(x, Range)         > 5 s
//!   !x                     199 µs        Xor(x, Range)        > 5 s
//! ```
//!
//! The three fast cases and the three slow ones compute closely related things.
//! `AndNot(Range, x)` *is* `!x` written longhand; `Xor(x, Range)` is the same
//! complement again when `x` fits inside the range. So the difference was not
//! inherent cost — it was which spelling the user happened to reach for. A
//! planner is the right place to fix that, because the alternative is asking
//! callers to know the table above.
//!
//! # Soundness
//!
//! Every rule preserves the *set*, not merely the cardinality. Several are
//! conditional on an operand being contained in a range, which [`bounds`]
//! answers conservatively — it may return a universe-wide bound and lose an optimization, but
//! it must never claim containment that does not hold. `plan_preserves_meaning`
//! in `tests/expr_equivalence.rs` checks planned against unplanned against the
//! `BTreeSet` oracle over random trees, which is what makes adding a rule safe.

use std::sync::Arc;

use super::sketch::{self, ChunkProfile, PrefixSketch};
use super::Expr;

/// Total `O(n)` statistic work a single `plan()` is willing to spend, in chunks.
///
/// # Why a budget and not just a per-operand cut-off
///
/// [`crate::stream::sketch::STATS_MAX_CHUNKS`] bounds what *one* operand costs
/// to summarize. It cannot bound what a *query* costs, because the planner
/// consults statistics at every node on every fixpoint pass — so cost scales
/// with `nodes x passes x chunks`, and a wide tree of individually-cheap
/// operands slips underneath the cut-off entirely. Measured: 16 leaves of 4 000
/// chunks each planned in **851 us against 240 us of execution**, with every
/// leaf comfortably below the per-operand limit.
///
/// The deeper the expression, the less each individual statistic can be worth,
/// because there are more of them and the same fixed answer has to pay for all
/// of them. So the effective per-operand limit is divided by the node count:
/// a two-operand query gets the full allowance, a 31-node tree gets a
/// thirty-first of it, and total planning work stays roughly flat across shapes.
///
/// This is a budget, not a correctness knob. Exhausting it costs
/// optimizations and never answers — every statistic gated by it is
/// conservative in the safe direction already.
pub const STATS_BUDGET: u64 = 16_384;

/// How many times the estimated execution cost the planner may spend on
/// statistics.
///
/// Not 1, and one of the two reasons it used to give was not true.
///
/// **What holds.** A rewrite is not worth one execution's saving, because it
/// can turn `O(range)` into `O(1)` — the saving is asymptotic, not a constant,
/// so a multiple of the *estimated* cost is the right shape of allowance. And it
/// has to cover several consults per node: an absorption asks about both
/// operands before it can decline.
///
/// **What does not.** This comment also said "a plan may be executed more
/// than once". Nothing in the crate reuses one. Every public entry re-plans from
/// scratch:
///
/// ```text
///   Expr::open()         self.plan().open_planned()
///   Expr::cardinality()  self.plan().count()
///   Expr::count()        inter.plan().count()   ( per nested intersection )
/// ```
///
/// A caller *can* hold `e.plan()` and open it repeatedly — both methods are
/// public — but no path here does, so the amortization that clause claimed is
/// not being collected.
///
/// **The fix is the reuse, not the number.** Making plans reusable would earn
/// the amortization this constant was partly justified by; changing the constant
/// only re-labels the gap. See `allowance-starves-the-rewrites-worth-most` and
/// `memoize-loads-not-statistics` in `TODO.md` — the latter's natural cache key,
/// `( key, version )`, is the same shape a plan cache would need.
pub const STATS_HEADROOM: u64 = 4;

// Remaining statistic allowance for the `plan()` currently running.
thread_local! {
    /// Defaults to the full allowance so that the public cost functions behave
    /// as documented when called outside `plan()` — there is no repetition to
    /// bound there, and a zero default would silently make them statistics-free.
    static REMAINING: std::cell::Cell<u64> = const { std::cell::Cell::new(STATS_BUDGET) };
}

/// Claim `n` chunks of statistic work, or refuse.
///
/// # Cumulative, because raggedness defeats anything per-operand
///
/// A per-operand ceiling cannot bound a *query*: 32 left-deep operands of 200
/// chunks each all sit under any sane ceiling and are re-summarized on every
/// pass, so the total is `nodes x passes x chunks` however tight the ceiling is.
/// Dividing the ceiling by node count does not fix it either — the operands just
/// fit under the smaller number. Measured with that scheme:
///
/// ```text
///   32 x 200c left-deep AND     plan 144.7 µs   exec 25.0 µs   5.8x
///   4000c AND 2c                plan   9.2 µs   exec  268 ns    34x
/// ```
///
/// Both are ragged in a way a balanced, uniform benchmark never shows: in the
/// second, one tiny operand makes execution trivial while the planner still
/// summarizes the big one in full.
///
/// So the allowance is spent, not divided. Cheap operands consume little and
/// leave room for an expensive one; an expensive one exhausts it and everything
/// after falls back to `bounds()`. Total work is bounded by [`STATS_BUDGET`]
/// whatever the shape.
///
/// **Which** statistics get computed therefore depends on traversal order
/// once the budget runs short. That changes which optimizations fire, never any
/// answer — every statistic gated by this is conservative in the safe direction.
/// It also means the earliest passes get the allowance, which is the right way
/// round: most rewrites fire on the first pass.
fn try_spend(n: u64) -> bool {
    REMAINING.with(|r| {
        let left = r.get();
        if n <= left {
            r.set(left - n);
            true
        } else {
            false
        }
    })
}

/// The per-operand ceiling, exposed for `dynamic.rs`'s occupancy gate.
pub fn effective_cutoff() -> u64 {
    sketch::STATS_MAX_CHUNKS
}

/// Execution cost estimated **without** consulting any statistic.
///
/// Chunk counts and range widths only — `O(nodes)`, no payload, no sketch. This
/// exists to answer "is this query even worth planning?", which cannot itself be
/// allowed to cost anything.
fn cheap_yield(e: &Expr) -> u64 {
    match e {
        Expr::Empty => 0,
        Expr::Set(s) => s.chunk_count() as u64,
        Expr::Source(src) => src.chunk_count().unwrap_or(OPAQUE_CHUNKS),
        Expr::Range(lo, hi) => chunks_in(*lo, *hi),
        Expr::Not(_, lo, hi) => chunks_in(*lo, *hi),
        Expr::And(a, b) => cheap_yield(a).min(cheap_yield(b)),
        Expr::AndNot(a, _) => cheap_yield(a),
        Expr::Or(a, b) | Expr::Xor(a, b) => cheap_yield(a).saturating_add(cheap_yield(b)),
    }
}

/// Installs a fresh allowance for one `plan()`, restoring the previous on exit.
///
/// Saves and restores rather than resetting, because `plan()` is re-entrant:
/// `Expr::count` plans the intersection term while inside a planned walk, and
/// clobbering the outer allowance would silently re-inflate it.
struct Budget(u64);

impl Budget {
    /// The allowance is the smaller of [`STATS_BUDGET`] and what the query is
    /// estimated to cost to *run*.
    ///
    /// # Never plan harder than the query costs to execute
    ///
    /// A fixed allowance is still wrong for a ragged query. `4000c AND 2c`
    /// executes in 147 ns — the tiny operand makes the leapfrog terminate almost
    /// immediately — and the planner was spending **4.34 us** summarizing the
    /// big one to find a rewrite that could not possibly repay that. Scaling the
    /// allowance by the estimated execution cost makes the planner's effort
    /// proportional to what there is to gain, which is the only stable way to
    /// pick it: a threshold in chunks has to be retuned whenever the workload
    /// changes, a ratio does not.
    ///
    /// [`cheap_yield`] deliberately consults no statistic — the decision about
    /// whether to afford statistics must not itself cost any.
    /// The allowance lives in a thread-local because the rule functions are
    /// free functions. That is the mechanism, not the design: the *owner* of
    /// planning state is the [`PlanStrategy`], which computes the allowance and
    /// hands it here for the duration of one `plan()`.
    ///
    /// Anything else a backend wants to remember belongs on the strategy too.
    /// A strategy instance has a
    /// lifetime the caller controls, which is exactly what a cache needs and
    /// what a free function cannot have.
    fn install(allowance: u64) -> Budget {
        let prev = REMAINING.with(|r| r.get());
        REMAINING.with(|r| r.set(allowance));
        Budget(prev)
    }
}

impl Drop for Budget {
    fn drop(&mut self) {
        REMAINING.with(|r| r.set(self.0));
    }
}

/// Chunks spanned by `[lo, hi)`./// Chunks spanned by `[lo, hi)`.
fn chunks_in(lo: u64, hi: u64) -> u64 {
    if hi <= lo {
        return 0;
    }
    ((hi - 1) >> crate::CHUNK_BITS) - (lo >> crate::CHUNK_BITS) + 1
}

/// Estimated chunks a stream yields when drained to the end.
///
/// **Read from the operands, not from the shape.** `Set` reports its real
/// `chunk_count`, `Range` its real width — so the same rewrite is accepted for
/// one dataset and declined for another. A rule that were decided structurally
/// would get `¬p ∩ ¬q` wrong in one direction or the other, because which side
/// is cheaper depends entirely on how big `p`, `q` and the range actually are.
pub fn yield_chunks(e: &Expr) -> u64 {
    match e {
        Expr::Empty => 0,
        Expr::Set(s) => s.chunk_count() as u64,
        Expr::Source(src) => src.chunk_count().unwrap_or(OPAQUE_CHUNKS),
        Expr::Range(lo, hi) => chunks_in(*lo, *hi),
        // A complement is dense: it yields a chunk almost everywhere in its
        // range, whatever the input was.
        Expr::Not(_, lo, hi) => chunks_in(*lo, *hi),
        // `And` yields only the shared chunks. Estimated from the prefixes when
        // they can be summarized, because `min` is merely an upper bound and is
        // wildly wrong for two large, barely-overlapping operands.
        Expr::And(a, b) => shared_prefixes(a, b)
            .unwrap_or_else(|| yield_chunks(a).min(yield_chunks(b)))
            .min(yield_chunks(a).min(yield_chunks(b))),
        // `AndNot` is driven by the left and only seeks the right.
        Expr::AndNot(a, _) => yield_chunks(a),
        // `Or` / `Xor` visit both sides, but yield the *union* — double-counting
        // the shared prefixes would make a self-union look twice its true size.
        Expr::Or(a, b) | Expr::Xor(a, b) => {
            let sum = yield_chunks(a).saturating_add(yield_chunks(b));
            sum.saturating_sub(shared_prefixes(a, b).unwrap_or(0))
        }
    }
}

/// Estimated chunks visited to answer `cardinality()`.
///
/// Distinct from [`yield_chunks`] because two operators answer cardinality by a
/// route that has nothing to do with what they yield, and those two exceptions
/// are the entire reason this planner is worth having:
///
/// - `Range` counts by subtraction — `O(1)` however wide it is.
/// - `Not` counts by `(hi - lo) - |input ∩ range|`, so it walks **the input**.
///   A complement that yields `2^48` chunks can be counted in a few thousand
///   steps.
///
/// Everything else pays for what its operands yield.
/// What one merge step costs relative to counting one chunk of a leaf.
///
/// Not 1. Counting a `SetStream` sums cached container lengths and touches no
/// payload; a merge pays a peek on both sides, a compare, and a kernel call per
/// prefix. Measured at ~2x ( two 10-chunk operands: 1.075 us merged against
/// 532 ns decomposed ). Treating them as equal is what made the cost model
/// decline the decomposition for a disjoint union and fall back on segmentation
/// to rescue it.
pub(crate) const MERGE_STEP: u64 = 2;

/// What a [`Expr::Source`](crate::Expr) that will not say its size is charged.
///
/// **Deliberately large.** These estimates decide which operand drives a join,
/// and a source that declines to report must not win that comparison against
/// one that did — the same rule `StreamStats::drain_cost` applies, and the same
/// constant. A source backed by a key always knows its chunk count, because the
/// index scan that built it counted them, so this is the genuinely-opaque case
/// rather than the common one.
pub(crate) const OPAQUE_CHUNKS: u64 = 1 << 20;

pub fn cardinality_cost(e: &Expr) -> u64 {
    match e {
        Expr::Empty => 0,
        Expr::Range(_, _) => 1,
        Expr::Set(s) => s.chunk_count() as u64,
        // Same shape as `Set`: counting a source walks its chunks without
        // decoding any of them, exactly as `SetStream::cardinality_dyn` sums
        // cached lengths. It is a per-chunk cost, not a per-ordinal one.
        Expr::Source(src) => src.chunk_count().unwrap_or(OPAQUE_CHUNKS),
        // Clipped to the window, because `Not::cardinality_dyn` is: it seeks to
        // the window start and stops at `last_prefix()`, so it walks the input
        // **intersected with the range**, never the whole input. The doc above
        // already says `( hi - lo ) - |input ∩ range|`; the code used to drop
        // the `∩ range` and price a 1-chunk window over a 10^6-chunk input at
        // 10^6. Reported by the neighbouring session, 2026-08-27.
        Expr::Not(x, lo, hi) => yield_chunks(x).min(chunks_in(*lo, *hi)).max(1),
        Expr::And(a, b) if disjoint(a, b) => 0,
        Expr::And(a, b) => yield_chunks(a).min(yield_chunks(b)),
        Expr::AndNot(a, _) => yield_chunks(a),
        Expr::Or(a, b) | Expr::Xor(a, b) => union_cost(a, b),
    }
}

/// What counting a two-way union or symmetric difference costs.
///
/// Normally a merge: both sides are walked, and every prefix pays a peek on each
/// side, a compare and a kernel call, which is what [`MERGE_STEP`] prices.
///
/// **But a union whose parts are disjoint in prefix order is not merged at
/// all.** `concat_disjoint_or` lowers it straight to `Concat`, which drains one
/// side and then the other — no per-prefix comparison, and `cardinality` is a
/// plain sum. Charging `MERGE_STEP` for that overstates the cheapest union shape
/// in the language by a factor of two.
///
/// The test is the same `O(1)` one the lowering runs: **strict** span
/// separation, because touching spans share a chunk and a shared chunk must be
/// merged. Kept deliberately weaker than `disjoint` — that would also accept
/// operands whose spans interleave but whose *prefixes* do not, and those still
/// lower to a merge, so accepting them here would swap an overcharge for an
/// undercharge.
///
/// See `disjoint-or-is-overcharged` in `JOURNAL.md` for why this matters beyond
/// tidiness: a split engine's acceptance gate compares against a `Concat`, so it
/// would systematically undervalue every split by this factor.
pub(crate) fn union_cost(a: &Expr, b: &Expr) -> u64 {
    let concatenable = match (prefix_span(a), prefix_span(b)) {
        (Some((la, ha)), Some((lb, hb))) => ha < lb || hb < la,
        // An empty side is dropped by the identity rules before this matters.
        _ => true,
    };
    let walked = yield_chunks(a).saturating_add(yield_chunks(b));
    if concatenable {
        walked.max(1)
    } else {
        walked.saturating_mul(MERGE_STEP).max(1)
    }
}

/// Take `candidate` only if it is strictly cheaper to count than `original`.
///
/// Strictness matters twice. It keeps the planner from swapping between two
/// equal-cost forms forever, and it gives [`plan`] a termination argument that
/// does not rely on the loop cap: every accepted cost-guided rewrite strictly
/// decreases a non-negative integer.
fn cheaper(original: &Expr, candidate: Expr) -> Expr {
    if cardinality_cost(&candidate) < cardinality_cost(original) {
        candidate
    } else {
        original.clone()
    }
}

/// Inclusive `(min, max)` of every ordinal an expression can yield, or `None`
/// when it is provably empty.
///
/// Conservative in the safe direction: a wider answer only costs an
/// optimization, while a wrong narrow one would change results. `And` is the
/// only operator that could tighten by intersecting both sides, and it does —
/// but it falls back to either side alone rather than guessing when one is
/// unknown. A source with no span gets the whole universe: `None` is reserved
/// for proven emptiness because identity rewrites use it to discard operands.
pub fn bounds(e: &Expr) -> Option<(u64, u64)> {
    match e {
        Expr::Empty => None,
        Expr::Set(s) => Some((s.min()?, s.max()?)),
        Expr::Range(lo, hi) => (hi > lo).then(|| (*lo, hi - 1)),
        // A source reports a *prefix* span, so this widens it to the ordinals
        // those chunks could hold. Wider than the truth by up to a chunk at each
        // end, which is the direction this function documents as safe: it costs
        // an optimization and cannot change a result. An unknown span covers
        // the universe: `None` is treated as empty by `pass_b` identity rules.
        Expr::Source(src) => Some(
            src.prefix_span()
                .map(|(lo, hi)| {
                    (
                        crate::join(lo, 0),
                        crate::join(hi, u16::MAX).min(crate::ORDINAL_MAX),
                    )
                })
                .unwrap_or((0, crate::ORDINAL_MAX)),
        ),
        // A complement is contained in its own range, whatever the input is.
        Expr::Not(_, lo, hi) => (hi > lo).then(|| (*lo, hi - 1)),
        // `a \ b` is contained in `a`.
        Expr::AndNot(a, _) => bounds(a),
        Expr::And(a, b) => match (bounds(a), bounds(b)) {
            (Some((la, ha)), Some((lb, hb))) => {
                let (lo, hi) = (la.max(lb), ha.min(hb));
                (lo <= hi).then_some((lo, hi))
            }
            // One side unknown: the intersection is still inside the other.
            (x, y) => x.or(y),
        },
        Expr::Or(a, b) | Expr::Xor(a, b) => match (bounds(a), bounds(b)) {
            (Some((la, ha)), Some((lb, hb))) => Some((la.min(lb), ha.max(hb))),
            (Some(x), None) | (None, Some(x)) => Some(x),
            (None, None) => None,
        },
    }
}

/// The chunk profile of an operand, when it is cheap to describe.
fn profile_of(e: &Expr) -> Option<ChunkProfile> {
    match e {
        Expr::Range(lo, hi) => Some(ChunkProfile::of_range(*lo, *hi)),
        // `Expr::Source` falls to `None` below: a profile describes how full
        // each chunk is, and a source knows its cardinality but not its
        // distribution. `None` is "not shown", which is the conservative
        // direction here -- it only forgoes an absorption.
        Expr::Set(s) => (s.chunk_count() as u64 <= sketch::STATS_MAX_CHUNKS
            && try_spend(s.chunk_count() as u64))
        .then(|| ChunkProfile::of_set(s)),
        _ => None,
    }
}

/// Inclusive prefix span of an expression.
///
/// Derived from [`bounds`], so it inherits its conservative widening: the answer
/// is never narrower than the truth. That is the direction a disjointness test
/// needs — if two widened spans do not overlap, the real ones cannot either.
pub(crate) fn prefix_span(e: &Expr) -> Option<(u64, u64)> {
    bounds(e).map(|(lo, hi)| (lo >> crate::CHUNK_BITS, hi >> crate::CHUNK_BITS))
}

/// Does `outer` contain every ordinal `inner` can yield?
///
/// # Why "full over the span" and not "is a range"
///
/// The absorption rules were written against `Expr::Range` literally. But a set
/// that is **1-filled** over a region *is* a range over that region — it holds
/// every ordinal of every chunk there — and every rule that recognizes a range
/// should recognize it too. `ChunkProfile` is what makes that visible: if
/// `outer` is `Full` across `inner`'s whole prefix span, then every chunk `inner`
/// touches is completely contained, so `inner ⊆ outer`.
///
/// Conservative: `false` means "not shown", never "shown false".
/// Does `outer` contain every ordinal an operand with these `bounds` can yield?
///
/// **`prefix_span` is `bounds` in disguise** — it is `bounds(e)` shifted right
/// by `CHUNK_BITS` — so the earlier `covers( b, a )` form re-walked the
/// whole of `a`. `pass_b` tries the absorption in **both directions** at every
/// `And` and `Or`, so one of the two always had the large subtree as its
/// `inner`, and that was the residual `O(nodes²)` left after the deep-clone fix:
/// left-deep planning was still growing 3.74x per doubling at k = 128. The
/// `outer` side is already `O(1)` — both `could_be_full_over` and `profile_of`
/// match only `Set` and `Range` and give up on anything else — so the inner
/// bounds was the entire cost.
fn covers_b(outer: &Expr, inner: Option<(u64, u64)>) -> bool {
    let Some((lo, hi)) = inner.map(|(lo, hi)| (lo >> crate::CHUNK_BITS, hi >> crate::CHUNK_BITS))
    else {
        return true; // an empty operand is contained in anything
    };
    if !could_be_full_over(outer, lo, hi) {
        return false;
    }
    profile_of(outer).is_some_and(|p| p.is_full_over(lo, hi))
}

/// `O(1)` necessary condition for `outer` to be 1-filled across `[lo, hi]`.
///
/// Being full over `n` chunks requires holding `n * CHUNK_CARD` ordinals, and
/// cardinality is a cached field — so an operand that cannot possibly qualify is
/// rejected without building a profile at all.
///
/// # Why this is load-bearing rather than a micro-optimization
///
/// `covers` is *speculative*: it is tried on both operands of every `And` / `Or`
/// / `AndNot`, and almost always fails. Without this filter each of those
/// attempts built a full `O(chunks)` profile, and since the statistic allowance
/// is spent in traversal order, big operands that could never be full consumed
/// it before the operand that actually *was* full got its turn.
///
/// Measured on the `( B ∪ C ) ∩ A` shape with `A` 1-filled over `[100,199]`:
/// profiling `B` ( 160 chunks ) and attempting `C` ( 1 000 ) exhausted the
/// allowance, so the absorption that eliminates the whole `A` intersection was
/// never reached. It fires only with an unbounded allowance — which is to say
/// the heuristic was starving the one rewrite that would have paid.
fn could_be_full_over(outer: &Expr, lo: u64, hi: u64) -> bool {
    if hi < lo {
        return true;
    }
    let chunks = hi - lo + 1;
    let needed = (chunks).saturating_mul(crate::CHUNK_CARD as u64);
    match outer {
        Expr::Set(s) => s.len() >= needed,
        Expr::Range(a, b) => b.saturating_sub(*a) >= needed,
        _ => false,
    }
}

/// A prefix sketch for an expression, where one is affordable.
///
/// `None` means "no statistics" — the caller falls back to interval bounds. Only
/// leaves and cheap compositions are summarized; a sub-expression whose operands
/// are unavailable stays unsummarized rather than guessed at.
fn sketch_of(e: &Expr) -> Option<PrefixSketch> {
    match e {
        Expr::Empty => Some(PrefixSketch::build(std::iter::empty())),
        Expr::Set(s) => (s.chunk_count() as u64 <= sketch::STATS_MAX_CHUNKS
            && try_spend(s.chunk_count() as u64))
        .then(|| PrefixSketch::build((0..s.chunk_count()).filter_map(|i| s.prefix_at(i)))),
        Expr::Range(lo, hi) => {
            let n = chunks_in(*lo, *hi);
            (n <= sketch::STATS_MAX_CHUNKS && try_spend(n)).then(|| {
                let first = lo >> crate::CHUNK_BITS;
                PrefixSketch::build(first..first + n)
            })
        }
        // A complement occupies its whole range, minus at most the input.
        Expr::Not(_, lo, hi) => sketch_of(&Expr::Range(*lo, *hi)),
        // `Expr::Source` falls to `None` below, which costs a real
        // optimization and is worth knowing: a sketch is what proves two
        // operands prefix-disjoint, so a union of sources is never rewritten
        // to `Empty` on that evidence. `KeySource` *could* supply one -- its
        // plan holds every prefix -- so this is a gap with a known fix rather
        // than a limit. `None` stays conservative meanwhile: it means "not
        // proven", never "proven false". Note the separate `concat_disjoint_or`
        // path does not depend on this and does work for sources, because it
        // tests exact prefix spans instead.
        // `a \ b` occupies a subset of `a`'s prefixes.
        Expr::AndNot(a, _) => sketch_of(a),
        _ => None,
    }
}

/// Estimated prefixes shared by two expressions, if it can be determined.
fn shared_prefixes(a: &Expr, b: &Expr) -> Option<u64> {
    // Two materialized sets: answer exactly, by merging the prefix arrays. This
    // is cheaper than the intersection being costed and admits no false zero,
    // which is what lets the planner rewrite to `Empty` on the strength of it.
    if let (Expr::Set(x), Expr::Set(y)) = (a, b) {
        let big = x.chunk_count().max(y.chunk_count()) as u64;
        if big <= sketch::STATS_MAX_CHUNKS && try_spend(big) {
            return Some(sketch::exact_shared_prefixes(x, y));
        }
    }
    Some(sketch_of(a)?.estimate_shared(&sketch_of(b)?))
}

/// Do the two provably share no *chunk*?
///
/// Stronger than [`disjoint`], which only compares intervals. Answered from an
/// exact source only — an exact prefix merge, or two complete sketches — because
/// the planner turns a `true` here into `Expr::Empty`.
fn prefix_disjoint(a: &Expr, b: &Expr) -> bool {
    if let (Expr::Set(x), Expr::Set(y)) = (a, b) {
        let big = x.chunk_count().max(y.chunk_count()) as u64;
        if big <= sketch::STATS_MAX_CHUNKS && try_spend(big) {
            return sketch::exact_shared_prefixes(x, y) == 0;
        }
    }
    match (sketch_of(a), sketch_of(b)) {
        (Some(x), Some(y)) => x.provably_disjoint(&y),
        _ => false,
    }
}

/// Can the two expressions share no ordinal at all?
///
/// Decided from [`bounds`], so it is **one-directional**: `true` means provably
/// disjoint, `false` means unknown. That asymmetry is the safe one — a missed
/// disjointness costs an optimization, a false one changes results.
///
/// This is also the weak point of the whole cost model. Two sets can share an
/// interval completely and still be disjoint chunk for chunk, and min/max bounds
/// cannot tell those apart — so overlap is only ever *detected*, never
/// *estimated*. Estimating it needs chunk-level prefix statistics rather than an
/// interval; see `planner-overlap-estimation` in `TODO.md`.
fn disjoint(a: &Expr, b: &Expr) -> bool {
    disjoint_b(a, bounds(a), b, bounds(b))
}

/// `[lo, hi)` of a range leaf.
fn as_range(e: &Expr) -> Option<(u64, u64)> {
    match e {
        Expr::Range(lo, hi) if hi > lo => Some((*lo, *hi)),
        _ => None,
    }
}

/// `a \ b` over two ranges, which may **split** into two.
///
/// This is the one rewrite that can make the tree bigger, and it is still worth
/// it: two range leaves are counted arithmetically, whereas the `AndNot` it
/// replaces would drive a merge across the left range's chunks.
fn range_difference(a: (u64, u64), b: (u64, u64)) -> Option<Expr> {
    let ((alo, ahi), (blo, bhi)) = (a, b);
    if bhi <= alo || ahi <= blo {
        return Some(Expr::Range(alo, ahi)); // disjoint: unchanged
    }
    if blo <= alo && bhi >= ahi {
        return Some(Expr::Empty); // fully covered
    }
    let left = (alo < blo).then(|| Expr::Range(alo, blo.min(ahi)));
    let right = (bhi < ahi).then(|| Expr::Range(bhi.max(alo), ahi));
    match (left, right) {
        (Some(l), Some(r)) => Some(Expr::Or(Box::new(l), Box::new(r))),
        (Some(x), None) | (None, Some(x)) => Some(x),
        (None, None) => Some(Expr::Empty),
    }
}

/// [`bounds`] of a node, from its children's bounds, in `O(1)`.
///
/// **This must mirror [`bounds`] arm for arm.** A `debug_assert` in
/// [`pass_b`] checks exactly that on every node of every planned expression, so
/// a drift between the two shows up in the first test that plans anything rather
/// than as a mysteriously worse plan much later. `ba` / `bb` are the children's
/// bounds; for a leaf, `ba` is the leaf's own.
fn bounds_from(e: &Expr, ba: Option<(u64, u64)>, bb: Option<(u64, u64)>) -> Option<(u64, u64)> {
    match e {
        Expr::Empty => None,
        // Leaves carry their own, already computed.
        Expr::Set(_) | Expr::Range(_, _) | Expr::Source(_) => ba,
        // A complement is contained in its own range, whatever the input is.
        Expr::Not(_, lo, hi) => (hi > lo).then(|| (*lo, hi - 1)),
        Expr::AndNot(_, _) => ba,
        Expr::And(_, _) => match (ba, bb) {
            (Some((la, ha)), Some((lb, hb))) => {
                let (lo, hi) = (la.max(lb), ha.min(hb));
                (lo <= hi).then_some((lo, hi))
            }
            (x, y) => x.or(y),
        },
        Expr::Or(_, _) | Expr::Xor(_, _) => match (ba, bb) {
            (Some((la, ha)), Some((lb, hb))) => Some((la.min(lb), ha.max(hb))),
            (Some(x), None) | (None, Some(x)) => Some(x),
            (None, None) => None,
        },
    }
}

/// [`disjoint`] with both operands' bounds already known.
fn disjoint_b(a: &Expr, ba: Option<(u64, u64)>, b: &Expr, bb: Option<(u64, u64)>) -> bool {
    match (ba, bb) {
        (Some((la, ha)), Some((lb, hb))) if ha < lb || hb < la => true,
        (Some(_), Some(_)) => prefix_disjoint(a, b),
        _ => true,
    }
}

/// Is every ordinal an operand can yield inside `[lo, hi)`, given its bounds?
///
/// An empty operand is contained in everything, which is why `None` is `true`
/// here rather than the cautious-looking `false`.
#[inline]
fn contained_b(b: Option<(u64, u64)>, lo: u64, hi: u64) -> bool {
    match b {
        None => true,
        Some((a, z)) => a >= lo && z < hi,
    }
}

/// A rewritten subtree and its [`bounds`].
struct Rewritten {
    expr: Expr,
    bounds: Option<(u64, u64)>,
}

/// One bottom-up rewrite pass.
fn pass(e: &Expr) -> Expr {
    pass_b(e).expr
}

/// One bottom-up rewrite pass, carrying each node's [`bounds`] back up.
///
/// # Why the bounds are threaded rather than recomputed
///
/// Every expensive guard here bottoms out in [`bounds`]: the emptiness tests are
/// `bounds(e).is_none()`, [`contained_b`] compares against it, and
/// [`disjoint`]'s first and cheapest arm is the interval test on both sides. A
/// pass that rebuilds a node and *then* calls those re-walks the whole subtree
/// two or three times per node, which is `O(nodes²)`.
///
/// Measured before this ( `plan_scaling` in `benches/setops.rs`, leaves held to
/// 4 chunks so the statistics are negligible ): a left-deep `And` chain
/// converged on **4.00x per doubling**, against 2.38x for a balanced tree of the
/// same leaf count — quadratic and `k log k` respectively, which is the exact
/// signature of per-node guards costing `O(subtree)`. At k = 64 planning cost
/// **39x** execution of the same expression, and because execution is linear the
/// ratio diverged without bound.
///
/// **Address-keyed memoization does not work here and should not be tried a
/// third time.** `pass` returns an `Expr` *by value* and the parent moves it into
/// a fresh `Box`, so the rewritten tree's node addresses are not the ones the
/// child pass saw — a memo keyed on the input tree is keyed on the wrong
/// objects. ( A different memo, `PrefixSketch` keyed by `OrdSet` `Arc` identity,
/// failed for a different reason: it missed on distinct operands. See
/// `planner-cost-is-o-chunks` in `TODO.md`. )
///
/// Nineteen of the twenty **top-level `match` arms** can state their result's
/// bounds in `O(1)` — `Empty` is `None`, a cloned child is that child's bounds,
/// a `Range` or `Not` reads its own endpoints, and an arm that does not fire
/// keeps the rebuilt node's. The two De Morgan arms are `O(1)` for a reason
/// worth naming: `And(Not(p, l, h), Not(q, l, h))` and `Not(_, l, h)` have
/// **identical** bounds, so which one [`cheaper`] picks cannot change the
/// answer. The single exception is double negation, which needs a grandchild's
/// bounds and falls back to a walk; it is rare and shrinks the tree.
///
/// **"Twenty" is top-level arms, and this function is counted four different
/// ways for four different purposes.** All four are correct and they are not
/// interchangeable: **20** top-level arms ( the unit here, because bounds are
/// stated per arm ), **30** outcome sites, **31** literal `Some((` ( one builds
/// an inline `Option<bounds>` as an outcome's second field, not an outcome ),
/// and **32** outcome *schemas*, because `range_difference` returns three
/// shapes from one site. `scripts/check-plan-measure.py` audits the termination
/// measure and counts schemas; a reader comparing its 32 against this 20 is
/// comparing units, not finding a discrepancy.
fn pass_b(e: &Expr) -> Rewritten {
    // Children first, so a rule sees already-simplified operands. Double
    // negation in particular only becomes visible after the inner `Not` is
    // recognized.
    let (e, ba, bb) = match e {
        Expr::And(a, b) => {
            let (ra, rb) = (pass_b(a), pass_b(b));
            (
                Expr::And(Box::new(ra.expr), Box::new(rb.expr)),
                ra.bounds,
                rb.bounds,
            )
        }
        Expr::Or(a, b) => {
            let (ra, rb) = (pass_b(a), pass_b(b));
            (
                Expr::Or(Box::new(ra.expr), Box::new(rb.expr)),
                ra.bounds,
                rb.bounds,
            )
        }
        Expr::Xor(a, b) => {
            let (ra, rb) = (pass_b(a), pass_b(b));
            (
                Expr::Xor(Box::new(ra.expr), Box::new(rb.expr)),
                ra.bounds,
                rb.bounds,
            )
        }
        Expr::AndNot(a, b) => {
            let (ra, rb) = (pass_b(a), pass_b(b));
            (
                Expr::AndNot(Box::new(ra.expr), Box::new(rb.expr)),
                ra.bounds,
                rb.bounds,
            )
        }
        Expr::Not(a, lo, hi) => {
            let ra = pass_b(a);
            (Expr::Not(Box::new(ra.expr), *lo, *hi), ra.bounds, None)
        }
        leaf => {
            let l = leaf.clone();
            let b = bounds(&l);
            (l, b, None)
        }
    };

    // Bounds of the rebuilt node, `O(1)`. Every guard below reads this or a
    // child's instead of walking.
    let eb = bounds_from(&e, ba, bb);
    debug_assert_eq!(
        eb,
        bounds(&e),
        "bounds_from must mirror bounds; they have drifted"
    );

    // `None` means no rule fired, and is the whole point of the `Option`: the
    // untouched node is then **moved** out below instead of deep-cloned. `Expr`
    // holds `Box<Expr>`, so `e.clone()` on a fall-through arm copies the entire
    // subtree — at every node, which is `O(nodes²)` allocations and was the real
    // cost here. Threading bounds removed two subtree *walks* per node and
    // bought 24%; removing this clone is what removes the quadratic.
    let rewritten: Option<(Expr, Option<(u64, u64)>)> = match e {
        // --- annihilators and identities -------------------------------------
        //
        // "is empty" is exactly `bounds(x).is_none()` — `bounds` of `Empty` is
        // `None` — so the threaded value decides these outright.
        Expr::And(_, _) if ba.is_none() || bb.is_none() => Some((Expr::Empty, None)),
        Expr::Or(ref a, _) if bb.is_none() => Some(((**a).clone(), ba)),
        Expr::Or(_, ref b) if ba.is_none() => Some(((**b).clone(), bb)),
        Expr::Xor(ref a, _) if bb.is_none() => Some(((**a).clone(), ba)),
        Expr::Xor(_, ref b) if ba.is_none() => Some(((**b).clone(), bb)),
        Expr::AndNot(_, _) if ba.is_none() => Some((Expr::Empty, None)),
        Expr::AndNot(ref a, _) if bb.is_none() => Some(((**a).clone(), ba)),

        // --- provable disjointness -------------------------------------------
        //
        // Must sit **above** the `AndNot` arm below: Rust takes the first
        // matching arm, and the generic `AndNot` rule would otherwise shadow
        // this one entirely. It did, and the test caught it.
        // Provably disjoint operands: the intersection is empty without looking
        // at a single payload.
        Expr::And(ref a, ref b) if disjoint_b(a, ba, b, bb) => Some((Expr::Empty, None)),
        // `a \ b` is just `a` when they cannot overlap.
        Expr::AndNot(ref a, ref b) if disjoint_b(a, ba, b, bb) => Some(((**a).clone(), ba)),
        // Disjoint XOR is a union, which reaches the n-ary accumulator. `Or` and
        // `Xor` share a bounds rule, so the rebuilt node's value still holds.
        Expr::Xor(ref a, ref b) if disjoint_b(a, ba, b, bb) => Some((
            Expr::Or(Box::new((**a).clone()), Box::new((**b).clone())),
            eb,
        )),

        // --- the rule that removes the hang ----------------------------------
        //
        // `Range(lo, hi) \ x` is the *definition* of the complement, so this is
        // unconditional. It is also the whole reason a universe-wide `AndNot`
        // used to run for longer than five seconds: `AndNot` drives its loop
        // from the left, and `Not` drives it from the input.
        Expr::AndNot(ref a, ref b) => match **a {
            // Range minus range: fuse, or **split** into the two surviving
            // pieces. Both outcomes are range leaves, which count arithmetically.
            Expr::Range(alo, ahi) if as_range(b).is_some() => {
                match range_difference((alo, ahi), as_range(b).unwrap()) {
                    // At most two range leaves, so this walk is `O(1)`.
                    Some(r) => {
                        let rb = bounds(&r);
                        Some((r, rb))
                    }
                    None => None,
                }
            }
            Expr::Range(lo, hi) => Some((
                Expr::Not(b.clone(), lo, hi),
                (hi > lo).then(|| (lo, hi - 1)),
            )),
            // `x \ R` is empty when x fits inside R.
            _ => match **b {
                Expr::Range(lo, hi) if contained_b(ba, lo, hi) => Some((Expr::Empty, None)),
                // `x \\ R` is empty when R is 1-filled across x's span.
                _ if covers_b(b, ba) => Some((Expr::Empty, None)),
                _ => None,
            },
        },

        // --- range absorption -------------------------------------------------
        //
        // Each needs `x ⊆ R`, which `bounds` decides conservatively. For a
        // universe-wide range that is free: under I8 every ordinal is inside it.
        Expr::And(ref a, ref b) => match (&**a, &**b) {
            // Fuse two ranges into their intersection.
            _ if as_range(a).is_some() && as_range(b).is_some() => {
                let ((alo, ahi), (blo, bhi)) = (as_range(a).unwrap(), as_range(b).unwrap());
                let (lo, hi) = (alo.max(blo), ahi.min(bhi));
                if lo < hi {
                    Some((Expr::Range(lo, hi), Some((lo, hi - 1))))
                } else {
                    Some((Expr::Empty, None))
                }
            }
            (Expr::Range(lo, hi), _) if contained_b(bb, *lo, *hi) => Some(((**b).clone(), bb)),
            (_, Expr::Range(lo, hi)) if contained_b(ba, *lo, *hi) => Some(((**a).clone(), ba)),
            // The same absorption, for an operand that is 1-filled over the
            // other's span rather than a range literal.
            _ if covers_b(a, bb) => Some(((**b).clone(), bb)),
            _ if covers_b(b, ba) => Some(((**a).clone(), ba)),
            // De Morgan: `¬p ∩ ¬q  ==  ¬(p ∪ q)`, over one shared range.
            //
            // Both forms have the same bounds — the `And` intersects two copies
            // of `[l, h-1)` and the `Not` is that interval — so whichever
            // `cheaper` returns, `eb` is right.
            (Expr::Not(p, l1, h1), Expr::Not(q, l2, h2)) if (l1, h1) == (l2, h2) => Some((
                cheaper(
                    &e,
                    Expr::Not(Box::new(Expr::Or(p.clone(), q.clone())), *l1, *h1),
                ),
                eb,
            )),
            _ => None,
        },
        Expr::Or(ref a, ref b) => match (&**a, &**b) {
            // Fuse two ranges that touch or overlap. Adjacency counts: `[0,10)`
            // and `[10,20)` are one range, and leaving them split would cost a
            // merge for nothing.
            _ if as_range(a)
                .zip(as_range(b))
                .is_some_and(|((alo, ahi), (blo, bhi))| alo.max(blo) <= ahi.min(bhi)) =>
            {
                let ((alo, ahi), (blo, bhi)) = (as_range(a).unwrap(), as_range(b).unwrap());
                let (lo, hi) = (alo.min(blo), ahi.max(bhi));
                Some((Expr::Range(lo, hi), (hi > lo).then(|| (lo, hi - 1))))
            }
            // `x ∪ R == R` when x ⊆ R. Note the result is still a huge range —
            // the win is that counting it is now arithmetic rather than a merge.
            (Expr::Range(lo, hi), _) if contained_b(bb, *lo, *hi) => Some(((**a).clone(), ba)),
            (_, Expr::Range(lo, hi)) if contained_b(ba, *lo, *hi) => Some(((**b).clone(), bb)),
            // `x ∪ R == R` when R is 1-filled across x's span.
            _ if covers_b(a, bb) => Some(((**a).clone(), ba)),
            _ if covers_b(b, ba) => Some(((**b).clone(), bb)),
            // De Morgan: `¬p ∪ ¬q  ==  ¬(p ∩ q)`. Same bounds either way, as
            // above.
            (Expr::Not(p, l1, h1), Expr::Not(q, l2, h2)) if (l1, h1) == (l2, h2) => Some((
                cheaper(
                    &e,
                    Expr::Not(Box::new(Expr::And(p.clone(), q.clone())), *l1, *h1),
                ),
                eb,
            )),
            _ => None,
        },
        Expr::Xor(ref a, ref b) => match (&**a, &**b) {
            // `x ⊕ R == R \ x` when x ⊆ R — a complement wearing a disguise,
            // and the reason XOR was as slow as the longhand `AndNot`.
            (Expr::Range(lo, hi), _) if contained_b(bb, *lo, *hi) => Some((
                Expr::Not(b.clone(), *lo, *hi),
                (hi > lo).then(|| (*lo, hi - 1)),
            )),
            (_, Expr::Range(lo, hi)) if contained_b(ba, *lo, *hi) => Some((
                Expr::Not(a.clone(), *lo, *hi),
                (hi > lo).then(|| (*lo, hi - 1)),
            )),
            _ => None,
        },

        // --- complement laws ---------------------------------------------------
        //
        // ¬¬x within one range is `x ∩ R`; the `And` rules above then drop the
        // intersection entirely when x ⊆ R, which is the usual case.
        Expr::Not(ref inner, lo, hi) => match &**inner {
            Expr::Not(x, ilo, ihi) if *ilo == lo && *ihi == hi => {
                // The one arm that cannot state its bounds in `O(1)`: `x` is a
                // grandchild, so its bounds were never threaded here. Rare, and
                // it shrinks the tree.
                let out = Expr::And(x.clone(), Box::new(Expr::Range(lo, hi)));
                let ob = bounds(&out);
                Some((out, ob))
            }
            _ => None,
        },

        _ => None,
    };

    // No rule fired: move the rebuilt node rather than cloning it.
    let (expr, bounds) = rewritten.unwrap_or((e, eb));
    Rewritten { expr, bounds }
}

/// Rewrite until stable.
///
/// Rules feed each other — ¬¬x becomes an `And`, which range absorption then
/// collapses — so one pass is not enough. The iteration cap is a safety net
/// against a future rule pair that oscillates; it is not expected to bind, and
/// stopping early only costs an optimization.
/// A pluggable planning backend.
///
/// # Why this seam exists
///
/// The obvious next step for the planner is to make *splitting* a rewrite rather
/// than a lowering step, so the fixpoint can alternate split and re-rule — see
/// `split-as-a-rewrite-not-a-lowering`. That is a loop-inductive process: a
/// pushdown grows the tree while an absorption shrinks it, so the two can
/// oscillate, and termination stops being obvious. Building it directly into the
/// one planner everything uses would put that whole issue surface on every
/// query.
///
/// So the strategy is a backend. An ambitious one can be developed and measured
/// against the same property tests without destabilizing the default, and a
/// query can opt into it explicitly.
///
/// # The contract
///
/// **A strategy must return an expression denoting the same set.** That is the
/// whole obligation — it may rewrite freely, decline to rewrite at all, or spend
/// arbitrary effort. `planning_preserves_meaning` in `tests/expr_equivalence.rs`
/// is run against every strategy, and a new one is not finished until it is
/// listed there.
pub trait PlanStrategy: Send + Sync {
    fn plan(&self, e: &Expr) -> Expr;
    /// For diagnostics and for naming the strategy in test failures.
    fn name(&self) -> &'static str;
}

/// Rewrite to a fixpoint with a given statistic allowance and pass cap.
fn fixpoint(e: &Expr, allowance: u64, passes: u32) -> Expr {
    let _budget = Budget::install(allowance);
    let mut cur = e.clone();
    for _ in 0..passes {
        let next = pass(&cur);
        if same_shape(&next, &cur) {
            return next;
        }
        cur = next;
    }
    cur
}

/// The default: every rule, with the statistic allowance scaled to the query's
/// estimated execution cost.
pub struct CostGuided;

impl PlanStrategy for CostGuided {
    fn plan(&self, e: &Expr) -> Expr {
        fixpoint(
            e,
            STATS_BUDGET.min(cheap_yield(e).saturating_mul(STATS_HEADROOM)),
            MAX_PASSES,
        )
    }
    fn name(&self) -> &'static str {
        "cost-guided"
    }
}

/// The deliberately dumb one: the same rules, but **no `O(n)` statistics at
/// all**.
///
/// Everything driven by `bounds()` still fires — the range and complement
/// rewrites, empty identities, interval disjointness — and those carry every
/// asymptotic win measured so far ( `AndNot( Range, x ) -> Not( x )` and friends
/// go from not-finishing to sub-microsecond ). What it gives up is chunk-level
/// disjointness for interleaved operands and the 1-filled absorption, neither of
/// which has yet been observed to pay at the query level.
///
/// Measured against `CostGuided`: identical execution on every ragged and
/// mixed-kind shape tried, at 2-12x less planning. It is offered as a backend
/// rather than made the default because `Expr` has no production consumer yet,
/// so the choice should be made against a real workload.
pub struct Conservative;

impl PlanStrategy for Conservative {
    fn plan(&self, e: &Expr) -> Expr {
        fixpoint(e, 0, MAX_PASSES)
    }
    fn name(&self) -> &'static str {
        "conservative"
    }
}

/// Passes before the fixpoint gives up.
///
/// A rule set that shrinks the tree or lowers cost converges well inside this;
/// the cap is a backstop against a future pair of rules that oscillate, which is
/// exactly the hazard a split/pushdown strategy would introduce.
pub const MAX_PASSES: u32 = 8;

/// Plan with the default strategy.
pub fn plan(e: &Expr) -> Expr {
    CostGuided.plan(e)
}

/// Structural equality, comparing set leaves by identity.
///
/// This was `format!("{next:?}") == format!("{cur:?}")`, which is correct and
/// catastrophic: `Debug` on `Expr::Set` renders **the entire set**, so the
/// fixpoint check serialized every operand twice per pass. On a 1 000-chunk
/// dense operand that made `plan()` take **192 ms for a query that executes in
/// 37 µs** — the planner costing five thousand times the work it was saving.
///
/// Two sets with equal contents but different `Arc`s compare unequal here. That
/// only risks an extra fixpoint pass, never a wrong plan, and it cannot loop:
/// the rewrite rules never fabricate a new `OrdSet`, so a `Set` leaf that
/// survives a pass survives as the same allocation.
fn same_shape(a: &Expr, b: &Expr) -> bool {
    match (a, b) {
        (Expr::Empty, Expr::Empty) => true,
        (Expr::Set(x), Expr::Set(y)) => Arc::ptr_eq(x, y),
        // Same identity rule, and same consequence: unequal `Arc`s cost at most
        // an extra fixpoint pass. Without this arm a source never compares equal
        // to itself, so any expression containing one runs all `MAX_PASSES`.
        (Expr::Source(x), Expr::Source(y)) => Arc::ptr_eq(x, y),
        (Expr::Range(a1, b1), Expr::Range(a2, b2)) => a1 == a2 && b1 == b2,
        (Expr::Not(x, l1, h1), Expr::Not(y, l2, h2)) => l1 == l2 && h1 == h2 && same_shape(x, y),
        (Expr::And(x1, y1), Expr::And(x2, y2))
        | (Expr::Or(x1, y1), Expr::Or(x2, y2))
        | (Expr::Xor(x1, y1), Expr::Xor(x2, y2))
        | (Expr::AndNot(x1, y1), Expr::AndNot(x2, y2)) => same_shape(x1, x2) && same_shape(y1, y2),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::ChunkStreamExt;
    use crate::OrdSet;
    use std::sync::Arc;

    /// A small set well inside the universe, so containment holds.
    fn x() -> Expr {
        Expr::set(Arc::new(OrdSet::from_iter_unsorted([1u64, 5, 70_000])))
    }
    fn full() -> Expr {
        Expr::Range(0, u64::MAX)
    }
    fn shape(e: &Expr) -> String {
        format!("{e:?}")
    }

    /// Each rule must *fire*, not merely be sound if it fires.
    ///
    /// `planning_preserves_meaning` cannot see a rule that stopped firing — a
    /// planner that rewrites nothing preserves meaning perfectly. These assert
    /// the rewritten shape, which is the only thing that does.
    #[test]
    fn the_range_absorption_rules_fire() {
        // And(x, R) -> x
        assert_eq!(shape(&x().and(full()).plan()), shape(&x()));
        assert_eq!(shape(&full().and(x()).plan()), shape(&x()));

        // Or(x, R) -> R
        assert_eq!(shape(&x().or(full()).plan()), shape(&full()));
        assert_eq!(shape(&full().or(x()).plan()), shape(&full()));

        // Xor(x, R) -> Not(x)
        assert!(matches!(x().xor(full()).plan(), Expr::Not(..)));
        assert!(matches!(full().xor(x()).plan(), Expr::Not(..)));

        // AndNot(R, x) -> Not(x); AndNot(x, R) -> Empty
        assert!(matches!(full().and_not(x()).plan(), Expr::Not(..)));
        assert!(matches!(x().and_not(full()).plan(), Expr::Empty));
    }

    /// ...and must *not* fire when containment does not hold.
    ///
    /// This is the half that keeps the rules honest: every absorption above is
    /// conditional on `x ⊆ R`, and a range that clips the input must be left
    /// alone. `[0, 10)` excludes 70_000, so none of these may collapse.
    #[test]
    fn absorption_is_declined_when_the_range_clips_the_input() {
        let narrow = Expr::Range(0, 10);
        assert_eq!(
            shape(&x().and(narrow.clone()).plan()),
            shape(&x().and(narrow.clone()))
        );
        assert_eq!(
            shape(&x().or(narrow.clone()).plan()),
            shape(&x().or(narrow.clone()))
        );
        assert_eq!(
            shape(&x().xor(narrow.clone()).plan()),
            shape(&x().xor(narrow.clone()))
        );
        assert!(!matches!(x().and_not(narrow).plan(), Expr::Empty));
    }

    #[test]
    fn empty_identities_fold() {
        assert!(matches!(x().and(Expr::Empty).plan(), Expr::Empty));
        assert_eq!(shape(&x().or(Expr::Empty).plan()), shape(&x()));
        assert_eq!(shape(&x().xor(Expr::Empty).plan()), shape(&x()));
        assert_eq!(shape(&x().and_not(Expr::Empty).plan()), shape(&x()));
        assert!(matches!(Expr::Empty.and_not(x()).plan(), Expr::Empty));
    }

    /// Double negation collapses through the `And` rule on a later pass, which
    /// is why `plan` iterates rather than making a single bottom-up sweep.
    #[test]
    fn double_negation_collapses_to_the_input() {
        let e = (!(!x())).plan();
        assert_eq!(shape(&e), shape(&x()), "¬¬x did not reduce to x");
    }

    /// De Morgan is applied *because the numbers say so*, not because the shape
    /// matches — and the numbers come from the operands.
    ///
    /// Measured on 3 000-chunk inputs over the whole universe, unplanned:
    ///
    /// ```text
    ///   (!a & !b) as written   > 5 s        !(a | b)   356 µs
    ///   (!a | !b) as written   > 5 s        !(a & b)   142 µs
    /// ```
    #[test]
    fn de_morgan_fires_when_the_data_says_it_is_cheaper() {
        let y = Expr::set(Arc::new(OrdSet::from_iter_unsorted([2u64, 9])));

        // ¬a ∩ ¬b -> ¬(a ∪ b)
        match ((!x()).and(!y.clone())).plan() {
            Expr::Not(inner, _, _) => {
                assert!(matches!(*inner, Expr::Or(..)), "got Not({inner:?})")
            }
            other => panic!("expected a single Not, got {other:?}"),
        }
        // ¬a ∪ ¬b -> ¬(a ∩ b)
        match ((!x()).or(!y)).plan() {
            Expr::Not(inner, _, _) => {
                assert!(matches!(*inner, Expr::And(..)), "got Not({inner:?})")
            }
            other => panic!("expected a single Not, got {other:?}"),
        }
    }

    /// ...and is declined here — which is a **known pessimization**, measured.
    ///
    /// **This test previously asserted the opposite and its premise was
    /// wrong.** It claimed `¬(p ∪ q)` "has to merge two 2 000-chunk inputs"
    /// while `¬p ∩ ¬q` "costs one chunk", and concluded the planner was right to
    /// decline. Both halves are false:
    ///
    /// * `Not` does **not** walk its whole input. `Not::cardinality_dyn` seeks
    ///   to the window and stops at `last_prefix()`, so it walks the input
    ///   *intersected with the range* — one chunk here, not 2 000. It then
    ///   counts by subtraction, building no container at all.
    /// * `And( ¬p, ¬q )` is the expensive one: it has to materialize **both**
    ///   complements ( 65 535 elements each ) and intersect them.
    ///
    /// Measured in release, 2 000 iterations, both answering 65 535:
    ///
    /// ```text
    ///   And( ¬p, ¬q )  ( what the planner picks )   53 095 ns
    ///   ¬( p ∪ q )     ( what it declines )            248 ns    214x faster
    /// ```
    ///
    /// So this pins current behaviour, not desirable behaviour. The planner
    /// declines a 214x improvement, and it would do so under **either** costing
    /// of `Not` — the clamped model prices both forms at 1 and `cheaper` is
    /// strict, so the rewrite is declined for want of a strict decrease rather
    /// than because it loses. Do not "fix" this by making `cheaper`
    /// non-strict: strictness is what gives `plan` its termination argument.
    /// The gap is that `cardinality_cost` does not charge `And` for
    /// materializing operands that cannot be counted arithmetically. See
    /// `cost-model-cannot-see-materialization` in `TODO.md`.
    /// A union that lowers to `Concat` must not be charged for a merge.
    ///
    /// `concat_disjoint_or` turns a prefix-disjoint union into `Concat`, which
    /// drains one side then the other: no per-prefix compare, no kernel call,
    /// and `cardinality` is a plain sum. Charging `MERGE_STEP` for it overstated
    /// the cheapest union shape in the language by exactly 2x.
    ///
    /// The separation must be **strict**, matching the lowering: touching
    /// spans share a chunk, and a shared chunk must be merged.
    #[test]
    fn a_prefix_disjoint_union_is_not_charged_for_a_merge() {
        let at = |base: u64| {
            Expr::set(Arc::new(OrdSet::from_sorted_slice(
                &(0..10u64).map(|i| (base + i) << 16).collect::<Vec<_>>(),
            )))
        };
        let walked = |e: &Expr| match e {
            Expr::Or(a, b) => yield_chunks(a) + yield_chunks(b),
            _ => unreachable!(),
        };

        // Disjoint spans: [0,9] and [20,29].
        let disjoint = at(0).or(at(20));
        assert_eq!(
            cardinality_cost(&disjoint),
            walked(&disjoint),
            "a Concat-able union costs a plain sum"
        );

        // Overlapping spans: both [0,9]. Still a merge, still weighted.
        let overlapping = at(0).or(at(0));
        assert_eq!(
            cardinality_cost(&overlapping),
            walked(&overlapping) * MERGE_STEP,
            "an overlapping union is a real merge and keeps MERGE_STEP"
        );

        // Touching spans — [0,9] and [9,18] share chunk 9 — must be treated as
        // overlapping. This is the boundary the lowering is strict about.
        let touching = at(0).or(at(9));
        assert_eq!(
            cardinality_cost(&touching),
            walked(&touching) * MERGE_STEP,
            "touching spans share a chunk, so they merge"
        );
    }

    #[test]
    fn de_morgan_is_declined_here_and_that_is_currently_wrong() {
        let big = |stride: u64| {
            Expr::set(Arc::new(OrdSet::from_sorted_slice(
                &(0..2000u64)
                    .map(|i| i * stride * 65_536)
                    .collect::<Vec<_>>(),
            )))
        };
        let (p, q) = (big(3), big(5));
        // A one-chunk universe for the complement.
        let narrow = |e: Expr| e.not_in(0, 65_536);
        let original = narrow(p.clone()).and(narrow(q.clone()));

        let candidate = Expr::Not(Box::new(Expr::Or(Box::new(p), Box::new(q))), 0, 65_536);
        // Both price at 1: `Not` is clipped to a one-chunk window, and `And`
        // takes the min of its operands. The model cannot separate them.
        assert_eq!(
            cardinality_cost(&original),
            cardinality_cost(&candidate),
            "the cost model rates these equal; execution differs 214x"
        );
        // Equal is not *strictly cheaper*, so the rewrite is declined.
        assert!(
            matches!(original.plan(), Expr::And(..)),
            "characterizing the current decision, which is the slow one"
        );
        // Both really do denote the same set, which is why the planner is free
        // to choose and why choosing wrong costs only time.
        assert_eq!(
            original.open_planned().cardinality_dyn().unwrap(),
            candidate.open_planned().cardinality_dyn().unwrap()
        );
    }

    /// The cost model must read the operands, not the shape.
    #[test]
    fn cost_depends_on_the_data_not_the_shape() {
        let small = Expr::set(Arc::new(OrdSet::from_iter_unsorted([1u64, 2, 3])));
        let large = Expr::set(Arc::new(OrdSet::from_sorted_slice(
            &(0..5000u64).map(|i| i * 65_536).collect::<Vec<_>>(),
        )));
        assert!(cardinality_cost(&small) < cardinality_cost(&large));

        // A universe-wide range costs 1 to count and 2^48 to drain. Conflating
        // those two is what made every slow spelling slow.
        let full = Expr::Range(0, u64::MAX);
        assert_eq!(cardinality_cost(&full), 1);
        assert!(yield_chunks(&full) > (1 << 47));

        // And a complement costs its *input*, not its range.
        let c = Expr::Not(Box::new(large.clone()), 0, u64::MAX);
        assert_eq!(cardinality_cost(&c), yield_chunks(&large));
    }

    /// A **1-filled** operand absorbs like a range literal.
    ///
    /// This is operand `A` from the design discussion: `[0,99]` 0-filled,
    /// `[100,199]` 1-filled. Over any expression confined to `[100,199]` it is
    /// a range in all but name, and every absorption rule must see that —
    /// `x ∩ A = x`, `x ∪ A = A`, `x \ A = ∅`, with no payload touched.
    fn one_filled(lo: u64, hi: u64) -> Expr {
        let mut v = Vec::new();
        for p in lo..=hi {
            v.extend((p << 16)..((p + 1) << 16));
        }
        Expr::set(Arc::new(OrdSet::from_sorted_slice(&v)))
    }

    #[test]
    fn a_one_filled_operand_absorbs_like_a_range() {
        let a = one_filled(100, 199);
        // An operand living inside A's 1-filled run.
        //
        // It has to be *substantial*. The planner's statistic allowance is
        // scaled by the query's estimated execution cost, and an `And` costs
        // roughly its sparser operand — so with a three-chunk `x` the absorption
        // cannot pay for itself ( profiling 100 chunks to save 3 steps ) and is
        // correctly declined. A fixture that small tests the economics, not the
        // rule. The decline is asserted explicitly below.
        let x = Expr::set(Arc::new(OrdSet::from_sorted_slice(
            &(100..200u64).map(|c| (c << 16) | 7).collect::<Vec<_>>(),
        )));

        assert_eq!(shape(&x.clone().and(a.clone()).plan()), shape(&x));
        assert_eq!(shape(&a.clone().and(x.clone()).plan()), shape(&x));
        assert_eq!(shape(&x.clone().or(a.clone()).plan()), shape(&a));
        assert!(matches!(x.clone().and_not(a.clone()).plan(), Expr::Empty));

        // And it must decline where A is not full: one chunk outside the run.
        let mut outside: Vec<u64> = (100..200u64).map(|c| (c << 16) | 7).collect();
        outside.insert(0, 50 << 16);
        let y = Expr::set(Arc::new(OrdSet::from_sorted_slice(&outside)));
        assert!(matches!(y.clone().and(a.clone()).plan(), Expr::And(..)));
        assert!(!matches!(y.and_not(a.clone()).plan(), Expr::Empty));

        // ...and it must decline when the rule cannot pay for itself, even
        // though it would be sound. A three-chunk operand makes the `And` cost
        // about three steps; profiling `a`'s hundred chunks to remove them is a
        // loss, and the allowance refuses it.
        let tiny = Expr::set(Arc::new(OrdSet::from_iter_unsorted([
            100u64 << 16,
            (150 << 16) | 7,
        ])));
        assert!(
            matches!(tiny.clone().and(a).plan(), Expr::And(..)),
            "the absorption fired on a query too cheap to repay the statistics"
        );
    }

    /// The rewrites must agree with evaluation, since `covers` claiming too much
    /// deletes results rather than slowing them down.
    #[test]
    fn one_filled_absorption_agrees_with_evaluation() {
        let a = one_filled(2, 4);
        let x = Expr::set(Arc::new(OrdSet::from_iter_unsorted([
            2u64 << 16,
            (3 << 16) | 5,
            (4 << 16) | 65_535,
        ])));
        // Straddling: partly inside A's run, partly outside.
        let z = Expr::set(Arc::new(OrdSet::from_iter_unsorted([
            (1u64 << 16) | 9,
            (3 << 16) | 5,
        ])));
        for e in [
            x.clone().and(a.clone()),
            x.clone().or(a.clone()),
            x.clone().and_not(a.clone()),
            z.clone().and(a.clone()),
            z.clone().or(a.clone()),
            z.and_not(a),
        ] {
            let planned = e.open().collect_set().unwrap();
            let unplanned = e.open_planned().collect_set().unwrap();
            assert_eq!(
                planned.iter().collect::<Vec<_>>(),
                unplanned.iter().collect::<Vec<_>>(),
                "planning changed {e:?}"
            );
        }
    }

    /// Guards that a soundness audit could invert with the **entire** suite
    /// still passing. Each assertion below fails under exactly one such
    /// inversion; without them these are one token away from deleting rows.
    #[test]
    fn the_absorption_and_fusion_guards_are_pinned() {
        let set = |v: &[u64]| Expr::set(Arc::new(OrdSet::from_iter_unsorted(v.to_vec())));
        let card = |e: &Expr| e.cardinality().unwrap();

        // `contained_b`'s upper bound is EXCLUSIVE. With `b <= hi` the absorption
        // swallows an ordinal sitting exactly on the range's open end.
        let e = Expr::Range(0, 100).and(set(&[50, 100]));
        assert_eq!(
            e.open().collect_set().unwrap().iter().collect::<Vec<_>>(),
            vec![50]
        );
        assert_eq!(card(&e), 1);

        // `Or` range fusion needs the ranges to touch or overlap. Relaxing it by
        // one invents the ordinal in the gap.
        let e = Expr::Range(0, 10).or(Expr::Range(11, 20));
        assert_eq!(card(&e), 19, "fusing across a gap invented an ordinal");

        // De Morgan on the `And` side is only valid when both complements share
        // one range: `(R1 \\ p) ∩ (R2 \\ q) == R1 \\ (p ∪ q)` needs `R1 == R2`.
        // The ranges must be wide enough that `cheaper()` accepts the rewrite —
        // with narrow ones the rule never fires and the guard is invisible.
        let e = set(&[1])
            .not_in(0, 1000 << 16)
            .and(set(&[2]).not_in(0, 500 << 16));
        assert_eq!(card(&e), 32_767_998);

        // Disjointness may only be claimed from an EXACT source. These operands
        // share exactly one prefix out of ~10 000; a bottom-K sample misses it
        // almost always, and `And` would be rewritten to `Empty`.
        let even = Arc::new(OrdSet::from_sorted_slice(
            &(0..5000u64).map(|i| (i * 2) << 16).collect::<Vec<_>>(),
        ));
        let mut odd: Vec<u64> = (0..5000u64).map(|i| ((i * 2 + 1) << 16) | 1).collect();
        // One shared prefix carrying the SAME ordinal, so the intersection is
        // genuinely non-empty. A shared *prefix* alone is not enough: with
        // different ordinals the intersection really is empty and rewriting to
        // `Empty` would be correct, which is what my first attempt asserted.
        odd.push(4000u64 << 16);
        odd.sort_unstable();
        let odd = Arc::new(OrdSet::from_sorted_slice(&odd));
        // Wrapped in AndNot so the exact Set-vs-Set path is bypassed and the
        // sketch is what answers.
        let lhs = Expr::set(even).and_not(set(&[12_345]));
        let rhs = Expr::set(odd).and_not(set(&[(1u64 << 16) | 12_345]));
        let e = lhs.and(rhs);
        assert!(
            !matches!(e.plan(), Expr::Empty),
            "an estimated disjointness proof rewrote a non-empty intersection to Empty"
        );
        assert_eq!(card(&e), 1);
    }

    /// A universe-wide range legitimately reports `u64::MAX` ordinals, and two
    /// of them overflow a merge operator's upper bound. `cardinality_hint` is
    /// public and reachable through `BoxedStream`.
    #[test]
    fn composing_cardinality_hints_does_not_overflow() {
        use crate::stream::ChunkStream;
        let full = || Expr::Range(0, u64::MAX);
        for e in [
            full().or(full()),
            full().xor(full()),
            full().or(full()).or(full()),
        ] {
            let (_, upper) = e.open_planned().cardinality_hint();
            assert_eq!(upper, Some(u64::MAX));
        }
    }

    /// Provable disjointness collapses three operators without touching data.
    #[test]
    fn disjoint_operands_collapse() {
        let lo = Expr::set(Arc::new(OrdSet::from_iter_unsorted([1u64, 2, 3])));
        let hi = Expr::set(Arc::new(OrdSet::from_iter_unsorted([900_000u64, 900_001])));

        assert!(matches!(lo.clone().and(hi.clone()).plan(), Expr::Empty));
        assert_eq!(shape(&lo.clone().and_not(hi.clone()).plan()), shape(&lo));
        // Disjoint XOR is a union, which can reach the n-ary accumulator.
        assert!(matches!(lo.clone().xor(hi.clone()).plan(), Expr::Or(..)));

        // Overlapping operands must be left alone.
        let mid = Expr::set(Arc::new(OrdSet::from_iter_unsorted([2u64, 900_000])));
        assert!(matches!(lo.and(mid).plan(), Expr::And(..)));
    }

    /// The headline capability of chunk-level statistics: two operands whose
    /// **intervals overlap almost entirely** but which share no chunk.
    ///
    /// `bounds()` sees `[0, 655_360_000)` against `[65_536, 655_425_536)` and
    /// can only say "unknown". The prefix merge sees alternating chunks and says
    /// "empty", so the intersection collapses without a single payload read.
    #[test]
    fn interleaved_chunks_are_disjoint_even_though_the_intervals_overlap() {
        let mk = |n: u64, odd: u64| {
            Expr::set(Arc::new(OrdSet::from_sorted_slice(
                &(0..n).map(|i| (i * 2 + odd) * 65_536).collect::<Vec<_>>(),
            )))
        };
        // Under `STATS_MAX_CHUNKS`, so the chunk-level statistics exist.
        let (a, b) = (mk(2000, 0), mk(2000, 1));

        // The intervals genuinely overlap, so the old interval test is useless.
        let (ba, bb) = (bounds(&a).unwrap(), bounds(&b).unwrap());
        assert!(ba.0 < bb.1 && bb.0 < ba.1, "the intervals must overlap");

        assert!(matches!(a.clone().and(b.clone()).plan(), Expr::Empty));
        // And the other two disjointness rules fire from the same evidence.
        assert_eq!(shape(&a.clone().and_not(b.clone()).plan()), shape(&a));
        assert!(matches!(a.clone().xor(b.clone()).plan(), Expr::Or(..)));

        // It must still be *right*: the collapse agrees with evaluation.
        assert_eq!(a.clone().and(b).cardinality().unwrap(), 0);

        // Above the cut-off the proof is simply unavailable: the statistics
        // are `O(chunks)` and are not computed, so the planner falls back to
        // interval bounds, which cannot separate interleaved operands. The
        // answer stays correct; only the optimization is lost. Pinned here so
        // the price of `STATS_MAX_CHUNKS` is a decision rather than a surprise.
        let n = crate::stream::sketch::STATS_MAX_CHUNKS + 1000;
        let (x, y) = (mk(n, 0), mk(n, 1));
        assert!(
            matches!(x.clone().and(y.clone()).plan(), Expr::And(..)),
            "above the cut-off there are no chunk statistics to prove disjointness with"
        );
        assert_eq!(
            x.and(y).cardinality().unwrap(),
            0,
            "still correct, just not free"
        );
    }

    /// Overlap is estimated, not just detected — so cost tracks how much two
    /// operands actually share, which is the thing `min()` could never express.
    #[test]
    fn cost_tracks_actual_overlap_not_just_size() {
        let mk = |start: u64| {
            Expr::set(Arc::new(OrdSet::from_sorted_slice(
                &(0..1000u64)
                    .map(|i| (start + i) * 65_536)
                    .collect::<Vec<_>>(),
            )))
        };
        let a = mk(0);
        let heavy = mk(0); // identical prefixes: 1000 shared
        let light = mk(900); // 100 shared
        let none = mk(5000); // disjoint

        let cost = |x: &Expr, y: &Expr| yield_chunks(&x.clone().and(y.clone()));
        assert_eq!(cost(&a, &heavy), 1000);
        assert_eq!(cost(&a, &light), 100);
        assert_eq!(cost(&a, &none), 0);

        // A union must not double-count the overlap.
        assert_eq!(yield_chunks(&a.clone().or(heavy)), 1000);
        assert_eq!(yield_chunks(&a.clone().or(light)), 1900);
    }

    /// Ranges fuse on intersection and union, and **split** on difference.
    #[test]
    fn ranges_fuse_and_split() {
        let r = |a, b| Expr::Range(a, b);

        // Intersection.
        assert_eq!(shape(&r(0, 100).and(r(50, 200)).plan()), shape(&r(50, 100)));
        assert!(matches!(r(0, 10).and(r(50, 60)).plan(), Expr::Empty));

        // Union, including the adjacent case `[0,10) ∪ [10,20)`.
        assert_eq!(shape(&r(0, 100).or(r(50, 200)).plan()), shape(&r(0, 200)));
        assert_eq!(shape(&r(0, 10).or(r(10, 20)).plan()), shape(&r(0, 20)));

        // Difference that cuts the middle out splits into two ranges — the one
        // rewrite here that makes the tree bigger, and still cheaper because
        // both pieces are counted arithmetically.
        match r(0, 100).and_not(r(40, 60)).plan() {
            Expr::Or(l, rr) => {
                assert_eq!(shape(&l), shape(&r(0, 40)));
                assert_eq!(shape(&rr), shape(&r(60, 100)));
            }
            other => panic!("expected a split into two ranges, got {other:?}"),
        }
        // Difference that clips one end, and one that removes everything.
        assert_eq!(
            shape(&r(0, 100).and_not(r(60, 200)).plan()),
            shape(&r(0, 60))
        );
        assert!(matches!(r(10, 20).and_not(r(0, 100)).plan(), Expr::Empty));
    }

    /// Range arithmetic must agree with actually evaluating it.
    ///
    /// Split and fuse are index arithmetic on bounds, which is exactly the kind
    /// of code that is off by one and still looks right.
    #[test]
    fn range_rewrites_agree_with_evaluation() {
        let r = |a, b| Expr::Range(a, b);
        let cases = [
            (0u64, 100u64, 40u64, 60u64),
            (0, 100, 0, 40),
            (0, 100, 60, 200),
            (0, 100, 100, 200),
            (0, 100, 0, 100),
            (10, 20, 0, 100),
            (0, 100, 200, 300),
        ];
        for (alo, ahi, blo, bhi) in cases {
            for e in [
                r(alo, ahi).and_not(r(blo, bhi)),
                r(alo, ahi).and(r(blo, bhi)),
                r(alo, ahi).or(r(blo, bhi)),
            ] {
                let planned = e.open().collect_set().unwrap();
                let unplanned = e.open_planned().collect_set().unwrap();
                assert_eq!(
                    planned.iter().collect::<Vec<_>>(),
                    unplanned.iter().collect::<Vec<_>>(),
                    "planning changed {e:?}"
                );
            }
        }
    }

    /// The rewrites must agree with the unplanned lowering on real data.
    #[test]
    fn planned_and_unplanned_agree_on_a_bounded_range() {
        let r = Expr::Range(0, 200);
        // Every expression here must be *bounded*, because `open_planned` is
        // the whole point of the comparison and an unplanned universe-wide
        // complement materializes 2^48 chunks. `!(!x())` belongs in the shape
        // tests above, not here.
        for e in [
            x().and(r.clone()),
            x().or(r.clone()),
            x().xor(r.clone()),
            x().and_not(r.clone()),
            r.clone().and_not(x()),
            x().not_in(0, 200).not_in(0, 200),
        ] {
            let planned = e.open().collect_set().unwrap();
            let unplanned = e.open_planned().collect_set().unwrap();
            assert_eq!(
                planned.iter().collect::<Vec<_>>(),
                unplanned.iter().collect::<Vec<_>>(),
                "planning changed the result of {e:?}"
            );
        }
    }
}
