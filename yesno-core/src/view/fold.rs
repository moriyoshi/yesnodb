//! Folding across constituents, and the inverse image back out.
//!
//! [`OrdSet::view_fold`] reduces a view's `n` constituents to one set over the
//! same logical ordinals; [`OrdSet::view_expand`] does the opposite, filling
//! every constituent's slot for each logical ordinal present.
//!
//! # A fold is not a restriction, and the difference is the whole algebra
//!
//! `docs/formal-model.md` §14 Proposition 18 says a prefix-window restriction is
//! a homomorphism of the **whole** Boolean signature and pushes down with no
//! side condition — "a planner that declines to push a restriction down is
//! declining on cost; it can never be declining on correctness". A fold is
//! nothing like that. Each monoid is exact for exactly one operator:
//!
//! ```text
//!   Any    ( ∃ )   exact for ∪      only  ⊆  for ∩
//!   All    ( ∀ )   exact for ∩      only  ⊇  for ∪
//!   Parity ( ⊕ )   exact for △      neither for ∩ or ∪
//! ```
//!
//! and **nothing** commutes with `\`. So a fold may never be pushed through a
//! binary operator unconditionally, and a reader who has internalised
//! Proposition 18 will assume it may and be wrong.
//!
//! **The inverse direction is free.** [`OrdSet::view_expand`] is a
//! homomorphism of the entire signature, because inverse images always are, and
//! `∃ ⊣ ⁻¹ ⊣ ∀` is an adjoint triple. That is why `expand( coarse ) ∩ fine`
//! composes with everything while `fold( a ∩ b )` does not.
//!
//! # This generalises the planner's occupancy statistic
//!
//! §7.1's `α( X ) = { ⌊( p − β ) / 2^τ⌋ }` is [`Reduce::Any`] at stride 1 on the
//! prefix axis, and its Proposition 9 ( sound omission ) is the `∩` one-sided law
//! above. A fold is therefore a **caller-declared zone map**, sound in exactly
//! the same direction: it can prove disjointness, never non-emptiness.
//!
//! # Which arm answers
//!
//! Selecting each constituent and combining with ordinary set algebra is correct
//! for every descriptor, reuses the tuned kernels, and is the **oracle** — it
//! is never deleted when an arm is faster. Under
//! [`ViewLayout::Interleaved`](super::ViewLayout::Interleaved) that costs
//! `O( n · nnz )`, because each `view_select` is itself a strided filter over
//! everything; a single grouped walk does it in `O( nnz )`, and that is the
//! specialised arm.

use super::{View, ViewLayout};
use crate::container::Container;
use crate::{chunk_base, chunk_window, split, OrdSet, Prefix48, ORDINAL_MAX};

/// How a fold combines the constituents at one logical ordinal.
///
/// Closed at three for the reason [`Semiring`](crate::matrix::Semiring) is
/// closed at two: these are the monoids available on packed bits. `\` is not
/// associative and `∩`'s identity is the all-ones vector, so neither can be a
/// fold. One kernel serves all three — the walk counts, and each variant reads
/// its answer off the count.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Reduce {
    /// Set when **any** constituent holds the logical ordinal — their union.
    Any,
    /// Set when **every** constituent holds it — their intersection.
    All,
    /// Set when an **odd** number hold it — their symmetric difference.
    Parity,
}

impl Reduce {
    /// Does a logical ordinal held by `count` of `sets` constituents survive?
    #[inline]
    fn keep(self, count: u64, sets: u64) -> bool {
        match self {
            Reduce::Any => count > 0,
            Reduce::All => count == sets,
            Reduce::Parity => count % 2 == 1,
        }
    }
}

/// How much one `insert_range` costs, in units of one bulk-sorted ordinal.
///
/// **Measured, not tuned.** From `benches/view.rs` at `sets = 4` over 200 000
/// logical ordinals: the interval path spent ~207 ns per interval where the
/// generic path spent ~5.7 ns per emitted slot, a ratio near 36. Break-even is
/// therefore `intervals · 36 == slots`, which is what
/// [`OrdSet::expansion_coalesces`] tests.
///
/// It is a property of two *algorithms* — a container insert against a sorted
/// bulk build — not of this module, so it should move only when one of those
/// changes. Re-derive it by re-running `view/expand` and comparing the two rows
/// at a density where they are close, rather than by adjusting it until a
/// benchmark looks better.
const EXPAND_INTERVAL_COST: u64 = 36;

/// How many leading constituents of `v` can hold any of `s`'s ordinals.
///
/// A companion to [`View::addressable_sets`], which bounds by what the ordinal
/// space can address; this bounds by what the *data* reaches, which is the
/// tighter question when a `Blocked` stride is small enough that every
/// constituent is addressable and almost all are empty.
///
/// Both layouts put a constituent's ordinals above `i · stride` ( blocked ) or
/// give it residue `i` mod `sets` ( interleaved ), so in each case no
/// constituent past this index can contain an ordinal at or below the maximum.
fn occupied_sets(s: &OrdSet, v: &View) -> u32 {
    let Some(max) = s.max() else {
        return 0;
    };
    let last = match v.layout() {
        ViewLayout::Interleaved => max,
        ViewLayout::Blocked { stride } => {
            if stride == 0 {
                return 0;
            }
            max / stride
        }
    };
    last.saturating_add(1).min(v.sets() as u64) as u32
}

/// Accumulates ascending, half-open ordinal intervals into containers.
///
/// **Intervals, never an ordinal list.** Expanding multiplies cardinality by
/// the constituent count, so a `Vec<u64>` would be `O( output ordinals )` to
/// describe something whose run form is `O( output runs )`. Consecutive logical
/// ordinals expand to *adjacent* intervals under `Interleaved`, so a contiguous
/// input collapses to one run.
#[derive(Default)]
struct IntervalBuilder {
    chunks: Vec<(Prefix48, Container)>,
}

impl IntervalBuilder {
    /// Add `[lo, hi)`. Calls must be ascending and non-overlapping.
    fn add(&mut self, lo: u64, hi: u64) {
        if hi <= lo {
            return;
        }
        let (p_lo, _) = split(lo);
        let (p_hi, _) = split(hi - 1);
        for p in p_lo..=p_hi {
            let Some((l, h)) = chunk_window(p, lo, hi) else {
                continue;
            };
            if self.chunks.last().map(|(pp, _)| *pp) != Some(p) {
                self.chunks.push((p, Container::from_sorted(&[])));
            }
            let c = &mut self.chunks.last_mut().expect("just pushed").1;
            // `chunk_window` is half-open and `insert_range` is inclusive; `h`
            // may be CHUNK_CARD, so `h - 1` is the largest `u16`.
            c.insert_range(l as u16, (h - 1) as u16);
        }
    }

    fn build(mut self) -> OrdSet {
        for (_, c) in self.chunks.iter_mut() {
            c.ensure_demoted();
        }
        let mut s = OrdSet::from_chunks(self.chunks);
        s.optimize();
        s
    }
}

impl OrdSet {
    /// Reduce every constituent to one set over the shared logical ordinals.
    ///
    /// The result is bounded by the constituents rather than by the logical
    /// universe — a union is at most their total cardinality, an intersection at
    /// most the smallest — so no fold has to enumerate a domain. That is why
    /// there is no "count of addressable logical ordinals" anywhere here, and why
    /// [`Reduce::All`] does not need the vacuous-truth case a padded packing
    /// would force.
    pub fn view_fold(&self, v: &View, reduce: Reduce) -> OrdSet {
        if v.check().is_err() {
            return OrdSet::new();
        }
        if let Some(out) = self.fold_interleaved(v, reduce) {
            return out;
        }
        self.fold_via_select(v, reduce)
    }

    /// The oracle: extract each constituent and combine with set algebra.
    ///
    /// Correct for every descriptor, and it reuses the tuned pairwise kernels
    /// rather than reimplementing them. Never deleted because an arm is
    /// faster — same contract as [`ops::generic`](crate::ops::generic).
    fn fold_via_select(&self, v: &View, reduce: Reduce) -> OrdSet {
        // Constituents above this hold nothing, whatever the descriptor says.
        // Bounding by `sets` instead makes the cost proportional to a `u32` a
        // client chose rather than to the data — four billion `view_select`
        // calls against a set that might hold one ordinal.
        let occupied = occupied_sets(self, v);
        // `Any` and `Parity` take an empty constituent as their identity, so
        // stopping early drops nothing. `All` does not: an empty constituent
        // empties the intersection, so a truncated loop must answer empty.
        if occupied < v.sets() && matches!(reduce, Reduce::All) {
            return OrdSet::new();
        }
        let mut acc: Option<OrdSet> = None;
        for i in 0..occupied {
            let s = self.view_select(v, i);
            acc = Some(match acc {
                None => s,
                Some(a) => match reduce {
                    Reduce::Any => a.or(&s),
                    Reduce::All => a.and(&s),
                    Reduce::Parity => a.xor(&s),
                },
            });
        }
        let mut out = acc.unwrap_or_default();
        out.optimize();
        out
    }

    /// The interleaved arm: one grouped walk instead of `n` strided filters.
    ///
    /// Correct because under `Interleaved` the physical order **is** the logical
    /// order — `o = x·n + i` is monotone in `x` — so every slot of a logical
    /// ordinal is contiguous and the walk needs only a running count and the
    /// current `x`. That is exactly what fails under `Blocked`, where `x`
    /// restarts at every constituent, and is why this arm declines there rather
    /// than being generalised.
    fn fold_interleaved(&self, v: &View, reduce: Reduce) -> Option<OrdSet> {
        let ViewLayout::Interleaved = v.layout() else {
            return None;
        };
        let n = v.sets() as u64;
        let mut out = Vec::new();
        let mut cur: Option<u64> = None;
        let mut count = 0u64;

        for (p, c) in self.chunks() {
            for val in c.iter() {
                let x = (chunk_base(p) | val as u64) / n;
                if cur != Some(x) {
                    if let Some(prev) = cur {
                        if reduce.keep(count, n) {
                            out.push(prev);
                        }
                    }
                    cur = Some(x);
                    count = 0;
                }
                count += 1;
            }
        }
        if let Some(prev) = cur {
            if reduce.keep(count, n) {
                out.push(prev);
            }
        }
        let mut s = OrdSet::from_iter_unsorted(out);
        s.optimize();
        Some(s)
    }

    /// The inverse image: every constituent's slot, for each logical ordinal here.
    ///
    /// Unlike a fold this is a homomorphism of the whole Boolean signature, so
    /// `a.view_expand( v )` distributes over `∩`, `∪`, `△` and `\` alike. It is
    /// the direction that composes with the rest of the algebra, and
    /// `expand( coarse ).and( fine )` is the query shape it exists for.
    ///
    /// A logical ordinal whose slot is not addressable in some constituent
    /// contributes only the slots that are, rather than being dropped or
    /// erroring — matching [`OrdSet::view_select`], which likewise reports what
    /// is addressable rather than refusing.
    pub fn view_expand(&self, v: &View) -> OrdSet {
        if v.check().is_err() {
            return OrdSet::new();
        }
        if let Some(out) = self.expand_interleaved(v) {
            return out;
        }
        self.expand_generic(v)
    }

    /// Will the interval construction actually pay, on this input?
    ///
    /// **The interval arm is not uniformly better, and shipping it
    /// unconditionally was a real regression.** Measured on 200 000 logical
    /// ordinals at `sets = 4`:
    ///
    /// ```text
    ///                 interval arm   per ordinal
    ///   contiguous        2.01 ms       6.89 ms    arm 3.4x faster
    ///   every 7th         5.92 ms      0.652 ms    arm 9.1x SLOWER
    /// ```
    ///
    /// A contiguous input merges to **one** interval; a scattered one merges to
    /// none, so the builder makes an `insert_range` call per input ordinal while
    /// the generic path makes one bulk sorted build. That is the crate's oldest
    /// measured asymmetry in disguise — bulk build against per-ordinal insert,
    /// 133 µs against 3.70 ms in the README's table.
    ///
    /// So the arm asks first. A counting pre-pass is `O( nnz )` with no
    /// allocation, and both candidate paths are already `O( nnz )`, so it is
    /// bounded by what the work costs anyway.
    ///
    /// [`EXPAND_INTERVAL_COST`] is a **measured cost ratio, not a tuning
    /// knob** — see its rationale before changing it.
    fn expansion_coalesces(&self, sets: u64) -> bool {
        let mut intervals: u64 = 0;
        let mut prev_end: Option<u64> = None;
        for x in self.iter() {
            let Some(lo) = x.checked_mul(sets) else { break };
            if prev_end != Some(lo) {
                intervals += 1;
            }
            prev_end = Some(lo.saturating_add(sets));
        }
        // Emitted slots the generic path would push individually.
        let slots = self.len().saturating_mul(sets);
        intervals.saturating_mul(EXPAND_INTERVAL_COST) <= slots
    }

    /// The oracle: every addressable slot of every logical ordinal.
    ///
    /// **Bounded by [`View::addressable_sets`], not by `sets`.** A descriptor may
    /// legally declare more constituents than the ordinal space can hold, and
    /// looping to `sets` would then visit billions of slots that `ordinal_of`
    /// rejects one at a time — work proportional to the *descriptor* rather than
    /// to the data or the output, which is what turned a 30-byte request into
    /// four billion iterations. `ordinal_of` is monotone in the constituent, so
    /// the addressable ones are a prefix and stopping at its end drops nothing.
    fn expand_generic(&self, v: &View) -> OrdSet {
        let mut sink = crate::pack::OrdinalSink::new();
        for x in self.iter() {
            for i in 0..v.addressable_sets(x) {
                if let Some(o) = v.ordinal_of(i, x) {
                    sink.push(o);
                }
            }
        }
        sink.build()
    }

    /// The interleaved arm: one interval per logical ordinal, and adjacent
    /// logical ordinals produce adjacent intervals.
    ///
    /// Under `Interleaved` a logical ordinal's `n` slots are `[x·n, x·n + n)` —
    /// **contiguous** — so the whole expansion is a run construction rather than
    /// `n` ordinals per input bit. A contiguous input collapses to a single run.
    /// It does not apply to `Blocked`, whose slots are `n` scattered
    /// singletons, and that difference is real rather than an implementation gap.
    fn expand_interleaved(&self, v: &View) -> Option<OrdSet> {
        let ViewLayout::Interleaved = v.layout() else {
            return None;
        };
        let n = v.sets() as u64;
        if !self.expansion_coalesces(n) {
            return None;
        }
        let mut b = IntervalBuilder::default();
        let mut pending: Option<(u64, u64)> = None;
        for x in self.iter() {
            let Some(lo) = x.checked_mul(n) else { break };
            if lo > ORDINAL_MAX {
                break;
            }
            // Clip the top slot group to the universe rather than dropping it.
            let hi = lo.saturating_add(n).min(ORDINAL_MAX.saturating_add(1));
            pending = Some(match pending {
                Some((plo, phi)) if phi == lo => (plo, hi),
                Some((plo, phi)) => {
                    b.add(plo, phi);
                    (lo, hi)
                }
                None => (lo, hi),
            });
        }
        if let Some((plo, phi)) = pending {
            b.add(plo, phi);
        }
        Some(b.build())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::ViewSink;
    use std::collections::BTreeSet;

    fn as_btree(s: &OrdSet) -> BTreeSet<u64> {
        s.iter().collect()
    }

    fn parts() -> Vec<OrdSet> {
        vec![
            OrdSet::from_iter_unsorted([0u64, 1, 2, 5, 900]),
            OrdSet::from_iter_unsorted([1u64, 2, 3, 900]),
            OrdSet::from_iter_unsorted((0..600u64).map(|i| i * 3)),
        ]
    }

    fn pack(v: View, ps: &[OrdSet]) -> OrdSet {
        let mut s = ViewSink::new(v);
        for (i, p) in ps.iter().enumerate() {
            s.place(i as u32, p).unwrap();
        }
        s.build()
    }

    /// The blocked strides must exceed the fixture's largest logical ordinal
    /// ( 1 797, from the multiples-of-three constituent ), or `place` refuses it
    /// as out of the constituent's capacity. An earlier version used 1 000 and
    /// failed there — the descriptor's capacity is a real constraint on the
    /// data, not a formality.
    fn views() -> Vec<View> {
        vec![
            View::interleaved(3),
            View::blocked(3, 2000),
            View::blocked(3, 65_536),
        ]
    }

    /// The fold means what its name says, checked against an oracle built from
    /// the constituents rather than from the packed form.
    #[test]
    fn fold_agrees_with_combining_the_constituents() {
        let ps = parts();
        for v in views() {
            let packed = pack(v, &ps);
            let union = ps.iter().skip(1).fold(ps[0].clone(), |a, b| a.or(b));
            let inter = ps.iter().skip(1).fold(ps[0].clone(), |a, b| a.and(b));
            let sym = ps.iter().skip(1).fold(ps[0].clone(), |a, b| a.xor(b));

            assert_eq!(
                as_btree(&packed.view_fold(&v, Reduce::Any)),
                as_btree(&union),
                "{v:?} any"
            );
            assert_eq!(
                as_btree(&packed.view_fold(&v, Reduce::All)),
                as_btree(&inter),
                "{v:?} all"
            );
            assert_eq!(
                as_btree(&packed.view_fold(&v, Reduce::Parity)),
                as_btree(&sym),
                "{v:?} parity"
            );
            assert!(!inter.is_empty(), "the fixture must exercise a non-empty ∩");
        }
    }

    /// The specialised arm and the oracle are two total functions over the
    /// same domain; this is the diff that keeps them honest.
    #[test]
    fn the_interleaved_arm_agrees_with_the_select_oracle() {
        let ps = parts();
        let mut reached = 0u32;
        let mut nonempty = 0u32;
        for sets in [1u32, 2, 3] {
            let v = View::interleaved(sets);
            let packed = pack(v, &ps[..sets as usize]);
            for r in [Reduce::Any, Reduce::All, Reduce::Parity] {
                let want = packed.fold_via_select(&v, r);
                let got = packed.fold_interleaved(&v, r).expect("arm applies");
                reached += 1;
                nonempty += u32::from(!got.is_empty());
                assert_eq!(as_btree(&got), as_btree(&want), "sets={sets} {r:?}");
            }
        }
        assert!(reached >= 9, "the arm fired only {reached} times");
        assert!(nonempty > 5, "only {nonempty} non-empty results");
        // And it declines where it must.
        let b = View::blocked(2, 100);
        assert!(OrdSet::new().fold_interleaved(&b, Reduce::Any).is_none());
    }

    /// `All` is `Any`'s De Morgan dual. This is the law most likely to catch a
    /// miscount, because it relates the two arms through a complement.
    #[test]
    fn all_is_the_de_morgan_dual_of_any() {
        let ps = parts();
        for v in views() {
            let packed = pack(v, &ps);
            // Complement each constituent within a window, repack, and fold.
            let hi = 1000u64;
            let flipped: Vec<OrdSet> = ps.iter().map(|p| p.not_in_range(0, hi)).collect();
            let packed_not = pack(v, &flipped);

            let all_of_not = packed_not.view_fold(&v, Reduce::All);
            let not_any = packed.view_fold(&v, Reduce::Any).not_in_range(0, hi);
            assert_eq!(as_btree(&all_of_not), as_btree(&not_any), "{v:?}");
        }
    }

    /// The one-sided law, and the assertion that it is **sometimes strict** —
    /// without that second half the test passes on an implementation that is
    /// accidentally exact, which is the failure mode §11 warns about.
    #[test]
    fn fold_of_an_intersection_is_contained_and_sometimes_strictly() {
        let v = View::interleaved(2);
        let a = pack(
            v,
            &[
                OrdSet::from_iter_unsorted([0u64, 1]),
                OrdSet::from_iter_unsorted([2u64]),
            ],
        );
        let b = pack(
            v,
            &[
                OrdSet::from_iter_unsorted([2u64]),
                OrdSet::from_iter_unsorted([0u64, 1]),
            ],
        );
        let lhs = a.and(&b).view_fold(&v, Reduce::Any);
        let rhs = a
            .view_fold(&v, Reduce::Any)
            .and(&b.view_fold(&v, Reduce::Any));
        for x in lhs.iter() {
            assert!(rhs.contains(x), "⊆ fails at {x}");
        }
        assert!(
            lhs.len() < rhs.len(),
            "the inclusion must be strict here, or the test proves nothing"
        );
    }

    /// The Galois connection, which pins the two directions against each other.
    #[test]
    fn expand_and_fold_form_an_adjunction() {
        let ps = parts();
        for v in views() {
            let packed = pack(v, &ps);
            // S ⊆ expand( fold( S ) ) — the round trip only grows.
            let back = packed.view_fold(&v, Reduce::Any).view_expand(&v);
            for o in packed.iter() {
                assert!(back.contains(o), "{v:?} lost physical {o}");
            }
            // fold( expand( C ) ) == C — expanding then folding is the identity,
            // because every constituent gets the same bit.
            let coarse = OrdSet::from_iter_unsorted([0u64, 3, 4, 900]);
            let round = coarse.view_expand(&v).view_fold(&v, Reduce::Any);
            assert_eq!(as_btree(&round), as_btree(&coarse), "{v:?}");
            // And with every constituent set, `All` agrees too.
            let round_all = coarse.view_expand(&v).view_fold(&v, Reduce::All);
            assert_eq!(as_btree(&round_all), as_btree(&coarse), "{v:?} all");
        }
    }

    /// Expansion distributes over the whole signature — the property a fold
    /// does not have, and the reason this is the composable direction.
    #[test]
    fn expand_is_a_homomorphism_of_every_boolean_operator() {
        let v = View::interleaved(3);
        let a = OrdSet::from_iter_unsorted([0u64, 1, 5, 900]);
        let b = OrdSet::from_iter_unsorted([1u64, 2, 900]);
        for (name, want, got) in [
            (
                "and",
                a.and(&b).view_expand(&v),
                a.view_expand(&v).and(&b.view_expand(&v)),
            ),
            (
                "or",
                a.or(&b).view_expand(&v),
                a.view_expand(&v).or(&b.view_expand(&v)),
            ),
            (
                "xor",
                a.xor(&b).view_expand(&v),
                a.view_expand(&v).xor(&b.view_expand(&v)),
            ),
            (
                "andnot",
                a.and_not(&b).view_expand(&v),
                a.view_expand(&v).and_not(&b.view_expand(&v)),
            ),
        ] {
            assert_eq!(as_btree(&want), as_btree(&got), "{name}");
        }
    }

    /// Whichever arm answers, the result is the same — and the public entry
    /// point agrees with the generic path on every input, applied or declined.
    #[test]
    fn expand_agrees_whichever_arm_answers() {
        let v = View::interleaved(4);
        let mut applied = 0u32;
        let mut declined = 0u32;
        for src in [
            OrdSet::new(),
            OrdSet::from_iter_unsorted([0u64]),
            OrdSet::from_iter_unsorted([0u64, 1, 2, 3, 100, 65_535, 65_536]),
            OrdSet::from_iter_unsorted((0..5000u64).map(|i| i * 7)),
            OrdSet::from_iter_unsorted(0..20_000u64),
        ] {
            let want = src.expand_generic(&v);
            match src.expand_interleaved(&v) {
                Some(got) => {
                    applied += 1;
                    assert_eq!(as_btree(&got), as_btree(&want));
                }
                None => declined += 1,
            }
            // The public entry point is correct either way.
            let out = src.view_expand(&v);
            assert_eq!(as_btree(&out), as_btree(&want));
            assert_eq!(out.len(), src.len() * 4);
        }
        // Both branches must be reached, or the dispatch is untested.
        assert!(applied > 0, "the interval arm never applied");
        assert!(declined > 0, "the interval arm never declined");
    }

    /// **The dispatch itself**, which exists because the interval arm is
    /// *slower* on scattered input — 9.1x slower, measured. A contiguous range
    /// must take it and collapse to runs; an every-seventh input must not.
    #[test]
    fn the_interval_arm_is_taken_only_when_the_expansion_coalesces() {
        let v = View::interleaved(4);

        let dense = OrdSet::from_iter_unsorted(0..20_000u64);
        assert!(dense.expansion_coalesces(4), "one interval must qualify");
        let e = dense.view_expand(&v);
        assert_eq!(e.chunk_count(), 2, "80 000 bits is two chunks");
        for (_, c) in e.chunks() {
            assert_eq!(c.kind(), crate::ContainerKind::Run, "must coalesce to runs");
        }

        // Every seventh logical ordinal: no two slot groups are adjacent, so the
        // builder would make one `insert_range` per input ordinal.
        let scattered = OrdSet::from_iter_unsorted((0..5000u64).map(|i| i * 7));
        assert!(
            !scattered.expansion_coalesces(4),
            "a non-coalescing input must decline"
        );
        assert!(scattered.expand_interleaved(&v).is_none());
    }

    /// **The amplification regression.** `sets` is a `u32` a caller may declare
    /// far larger than any data justifies, and both loops used to run to it: a
    /// set holding two ordinals drove four billion `view_select` calls. The
    /// bound is `occupied_sets`, and the assertion that matters is not the
    /// value — it is that this test *returns*. If the bound regresses the test
    /// does not fail, it hangs, and CI reports the timeout.
    #[test]
    fn a_fold_costs_what_the_data_holds_not_what_the_descriptor_declares() {
        let v = View::blocked(u32::MAX, 1);
        // Stride 1 gives each constituent exactly logical ordinal 0, so this is
        // constituents 0 and 5 holding `{0}` and every other one empty.
        let s = OrdSet::from_iter_unsorted([0u64, 5]);
        assert_eq!(occupied_sets(&s, &v), 6, "bounded by the maximum ordinal");

        assert_eq!(
            as_btree(&s.view_fold(&v, Reduce::Any)),
            as_btree(&OrdSet::from_iter_unsorted([0u64]))
        );
        // Truncating the loop must not turn `All` into a union of what was
        // visited: constituent 1 is empty, so the intersection is empty.
        assert!(s.view_fold(&v, Reduce::All).is_empty());
        // Two constituents hold logical 0, so the parity is even.
        assert!(s.view_fold(&v, Reduce::Parity).is_empty());
    }

    /// The same property for expansion, which was the worse of the two — its
    /// inner loop ran per *input ordinal*, so the cost was cardinality x sets.
    ///
    /// **This is a wall-clock assertion, and it has to be.** The bounded and
    /// unbounded loops return byte-identical results — `ordinal_of` rejects the
    /// extra constituents one at a time — so no output, cardinality or
    /// allocation count can tell them apart. Time is the only observable, which
    /// is what makes the defect a defect.
    ///
    /// Sabotage-verified 2026-09-19: reverting the bound to `0..v.sets()` left
    /// this test **passing in 57.88 s** at one input ordinal, which is why the
    /// fixture carries four and asserts a deadline. Four ordinals put the
    /// unbounded path near 230 s in debug; the bounded path does 4 x 256 = 1024
    /// iterations and finishes in microseconds. The 10 s deadline therefore has
    /// roughly a 20x margin below the broken cost even in release, and about six
    /// orders of magnitude above the correct one.
    #[test]
    fn an_expansion_visits_only_addressable_constituents() {
        let stride = 1u64 << 56;
        let v = View::blocked(u32::MAX, stride);
        // `i * 2^56` leaves the ordinal universe after 256 constituents
        // ( `ORDINAL_MAX` is `u64::MAX - 1`, not `2^48` -- that is the *prefix*
        // width ), so the other four billion are unaddressable and must never
        // be visited.
        assert_eq!(v.addressable_sets(0), 256);

        let src = OrdSet::from_iter_unsorted([0u64, 1, 2, 3]);
        let t = std::time::Instant::now();
        let out = src.view_expand(&v);
        let elapsed = t.elapsed();

        assert_eq!(out.len(), 4 * 256, "one slot per addressable constituent");
        assert!(out.contains(0) && out.contains(255 * stride + 3));
        // The next slot would be `256 * stride`, which is `2^64` -- not
        // representable, which is exactly why the bound stops here.
        assert_eq!(v.ordinal_of(256, 0), None);
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "expansion took {elapsed:?}; the loop is running to `sets` again"
        );
    }

    /// `addressable_sets` is a prefix bound, and the prefix is exactly the
    /// constituents `ordinal_of` answers for. Checked against it directly,
    /// because a bound that disagreed would silently drop slots.
    #[test]
    fn addressable_sets_agrees_with_ordinal_of() {
        let cases = [
            (View::interleaved(8), 0u64),
            (View::interleaved(8), 5),
            (View::interleaved(8), ORDINAL_MAX),
            (View::blocked(8, 100), 0),
            (View::blocked(8, 100), 99),
            (View::blocked(8, 100), 100),
            (View::blocked(u32::MAX, 1 << 40), 0),
            (View::interleaved(u32::MAX), ORDINAL_MAX),
        ];
        for (v, x) in cases {
            let k = v.addressable_sets(x);
            // Everything below `k` addresses `x` ..
            for i in [0u32, k.saturating_sub(1)] {
                if i < k {
                    assert!(v.ordinal_of(i, x).is_some(), "{v:?} x={x} i={i}");
                }
            }
            // .. and `k` itself does not, when there is one to test.
            if k < v.sets() {
                assert!(v.ordinal_of(k, x).is_none(), "{v:?} x={x} k={k}");
            }
        }
    }

    /// The bound must not save work the caller actually asked for. Under
    /// `Interleaved` at `x = 0` every declared constituent *is* addressable, so
    /// the honest answer is a huge one and `addressable_sets` says so. This is
    /// the case the wire's `MAX_VIEW_SETS` exists for, and the reason the cap
    /// and this bound are both needed rather than either alone.
    #[test]
    fn an_addressable_expansion_is_not_truncated() {
        let v = View::interleaved(1 << 20);
        assert_eq!(v.addressable_sets(0), 1 << 20, "all of them are reachable");
    }

    #[test]
    fn a_degenerate_view_folds_and_expands_to_nothing() {
        let bad = View::interleaved(0);
        let s = OrdSet::from_iter_unsorted([1u64, 2]);
        assert!(s.view_fold(&bad, Reduce::Any).is_empty());
        assert!(s.view_expand(&bad).is_empty());
    }
}
