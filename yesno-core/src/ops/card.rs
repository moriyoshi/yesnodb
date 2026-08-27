//! Cardinality-only paths that never materialize a result container.
//!
//! The key reduction: **all four cardinalities derive from `and_cardinality`
//! plus the cached lengths.**
//!
//! ```text
//! |A ∩ B| = and_card(A, B)
//! |A ∪ B| = |A| + |B| − and_card(A, B)
//! |A ⊕ B| = |A| + |B| − 2·and_card(A, B)
//! |A \ B| = |A| − and_card(A, B)
//! ```
//!
//! So one non-allocating kernel family buys four zero-allocation queries. This
//! is why `Container::len()` must be O(1) on every representation.

use crate::container::Container;

/// `|A ∩ B|` without allocating.
pub fn and_cardinality(a: &Container, b: &Container) -> u32 {
    if a.is_empty() || b.is_empty() {
        return 0;
    }
    if a.is_full() {
        return b.len();
    }
    if b.is_full() {
        return a.len();
    }

    // Bitmap × bitmap is the one arm worth specializing immediately: zipped
    // word-wise popcount, no stores, fully vectorizable.
    if let (Container::Bitmap(x), Container::Bitmap(y)) = (a, b) {
        if let (Some(wx), Some(wy)) = (x.bits.try_words(), y.bits.try_words()) {
            return crate::ops::bitmap::words_and_cardinality(wx, wy);
        }
    }

    // Array x array: the most common shape in a sparse posting-list workload,
    // and until 2026-08-26 the only pair here with no arm at all — it fell
    // through to the generic merge at the bottom of this function, which
    // dispatches on the container kind once per element on *both* sides. Flat
    // 2.5x, m = 32 to 4096. Delegated to `ops::array` rather than written here so
    // it shares `and`'s gallop-vs-merge decision; see the note there.
    if let (Container::Array(x), Container::Array(y)) = (a, b) {
        return crate::ops::array::and_cardinality(x.as_slice(), y.as_slice());
    }

    // Run x run: sum the interval intersections. Two sorted interval lists meet
    // in O(nruns) — or O(min · log max) when they are skewed — where merging
    // their *values* costs their cardinality: 559x slower on eight full-chunk
    // runs, in the path this module exists to make cheap. No allocation, which
    // `tests/allocation.rs` pins.
    //
    // Delegated to `ops::run` rather than open-coded here, for the reason the
    // array arm above is: this module is a **second** dispatch over the same kind
    // pairs, so a two-pointer written here does not inherit a fix made to
    // `ops::run::try_apply`, and the crate has twice shipped exactly that.
    if let (Container::Run(x), Container::Run(y)) = (a, b) {
        return crate::ops::run::and_cardinality(x.as_flat(), y.as_flat());
    }

    // Bitmap x run: popcount the bitmap under each interval's mask. O(nruns +
    // words touched) rather than O(run cardinality).
    let bitmap_run = match (a, b) {
        (Container::Bitmap(x), Container::Run(y)) => Some((x, y)),
        (Container::Run(y), Container::Bitmap(x)) => Some((x, y)),
        _ => None,
    };
    if let Some((bm, rn)) = bitmap_run {
        if let Some(bw) = bm.bits.try_words() {
            let mut total = 0u32;
            for i in 0..rn.nruns() {
                total += crate::ops::mixed::masked_and_popcount(bw, rn.start(i), rn.end(i));
            }
            return total;
        }
    }

    // Array x run: probe the array against the run's binary search. Costs the
    // array's cardinality, never the run's.
    let array_run = match (a, b) {
        (Container::Array(x), Container::Run(y)) => Some((x, y)),
        (Container::Run(y), Container::Array(x)) => Some((x, y)),
        _ => None,
    };
    if let Some((arr, rn)) = array_run {
        // Two-pointer, not a binary search per value.
        //
        // Both sides are sorted, so one merge answers the whole join in
        // O(n + nruns) where probing costs O(n log nruns) — and the probe pays
        // that logarithm on every element even when the two are interleaved,
        // which is the common shape.
        let vals = arr.as_slice();
        let m = rn.nruns();
        let (mut i, mut j, mut total) = (0usize, 0u32, 0u32);
        while i < vals.len() && j < m {
            let v = vals[i];
            if v < rn.start(j) {
                i += 1;
            } else if v > rn.end(j) {
                j += 1;
            } else {
                total += 1;
                i += 1;
            }
        }
        return total;
    }

    // Probe the sparser side against the denser one: O(min) probes rather than
    // O(n + m) merge steps.
    let (probe, target) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    if let (Container::Array(p), Container::Bitmap(t)) = (probe, target) {
        // Direct word test rather than `Container::contains`, which re-dispatches
        // on the container kind for every value.
        if let Some(tw) = t.bits.try_words() {
            return p
                .as_slice()
                .iter()
                .filter(|v| tw[**v as usize >> 6] & (1u64 << (**v & 63)) != 0)
                .count() as u32;
        }
    }
    if matches!(target, Container::Bitmap(_)) {
        return probe.iter().filter(|&v| target.contains(v)).count() as u32;
    }

    let mut ai = a.iter().peekable();
    let mut bi = b.iter().peekable();
    let mut n = 0u32;
    while let (Some(x), Some(y)) = (ai.peek().copied(), bi.peek().copied()) {
        if x < y {
            ai.next();
        } else if y < x {
            bi.next();
        } else {
            ai.next();
            bi.next();
            n += 1;
        }
    }
    n
}

#[inline]
pub fn or_cardinality(a: &Container, b: &Container) -> u32 {
    a.len() + b.len() - and_cardinality(a, b)
}

#[inline]
pub fn xor_cardinality(a: &Container, b: &Container) -> u32 {
    a.len() + b.len() - 2 * and_cardinality(a, b)
}

#[inline]
pub fn andnot_cardinality(a: &Container, b: &Container) -> u32 {
    a.len() - and_cardinality(a, b)
}

/// Short-circuiting emptiness test for the intersection. Arrow has no
/// short-circuiting binary reducer, so this is hand-written.
///
/// # These arms are not optional, and their absence was expensive
///
/// Until 2026-08-26 this had exactly **one** specialized arm — bitmap x bitmap —
/// and the other five kind-pairs fell through to
/// `probe.iter().any(|v| target.contains(v))`, which dispatches on the container
/// kind once per element on the iterating side *and again* on the probing side.
/// For a `Run` that is catastrophic rather than merely slow: `iter()` enumerates
/// every **value** of a container whose whole point is that it stores
/// **intervals**, so `run x run` cost 124 us where the interval two-pointer below
/// costs about a hundred steps. Measured against the one arm that did exist:
///
/// | pair | before | | pair | before |
/// | --- | --- | --- | --- | --- |
/// | bitmap x bitmap | 274 ns | | array x run | 29.4 us |
/// | array x bitmap | 7.2 us | | array x array | 34.5 us |
/// | bitmap x run | 114.7 us | | run x run | 124.2 us |
///
/// That is 26x to 453x the specialized arm, and it made `is_disjoint` **more
/// expensive than `and_cardinality(a, b) == 0`** — a predicate that computes
/// strictly less than a count, costing strictly more. `OrdSet::is_subset` was
/// 8.7x slower than the `roaring` reference as a direct consequence.
///
/// Each arm below is the corresponding [`and_cardinality`] arm with the
/// accumulator replaced by an early return, which is deliberate: the two must
/// make the same algorithmic choice, or the predicate silently becomes the
/// slower way to ask a weaker question. `is_disjoint on {kind} x {kind}` in
/// `tests/proptest_oracle.rs` pins every pair against `and_cardinality == 0`.
///
/// # The invariant broke again on 2026-08-28, in the arm that had always
/// existed
///
/// The 2026-08-26 repair added the five missing arms and left the original
/// bitmap x bitmap one alone, because it was already "a word loop". It was —
/// but a *short-circuiting* one, which LLVM cannot widen, against a counting
/// loop it fully vectorizes. On two disjoint 8 KiB bitmaps that cost **274.8 ns
/// against `and_cardinality`'s 167.6 ns**: 1.64x more to compute strictly less.
/// Both predicates are now blocked ( [`crate::ops::bitmap::words_disjoint`] )
/// and cost 82.6 / 83.1 ns, i.e. 0.87x of the count.
///
/// The generalizable form: **the same source, in a short-circuiting and a
/// non-short-circuiting form, is two different kernels.** The arm that had been
/// specialized the longest was the one still violating the rule, and the
/// benchmark that named the pair could not see it because it never compared the
/// pair against the count. `bitmap_predicate_vs_count` in `benches/setops.rs`
/// is the group that does.
pub fn is_disjoint(a: &Container, b: &Container) -> bool {
    if a.is_empty() || b.is_empty() {
        return true;
    }
    // Both are non-empty, so a full container meets whatever the other holds.
    if a.is_full() || b.is_full() {
        return false;
    }

    if let (Container::Bitmap(x), Container::Bitmap(y)) = (a, b) {
        if let (Some(wx), Some(wy)) = (x.bits.try_words(), y.bits.try_words()) {
            // Not `zip(..).all(..)`: that form does not vectorize and made
            // this predicate 1.63x the cost of the count it must not exceed.
            // See `ops::bitmap::words_disjoint`.
            return crate::ops::bitmap::words_disjoint(wx, wy);
        }
    }

    if let (Container::Array(x), Container::Array(y)) = (a, b) {
        return crate::ops::array::is_disjoint(x.as_slice(), y.as_slice());
    }

    // Run x run: the intervals meet in O(nruns), not O(cardinality) — and in
    // O(min · log max) when the two are skewed, which is the same decision
    // `and_cardinality` makes, taken in the same place so the predicate cannot
    // become the slower way to ask the weaker question.
    if let (Container::Run(x), Container::Run(y)) = (a, b) {
        return crate::ops::run::is_disjoint(x.as_flat(), y.as_flat());
    }

    // Bitmap x run: test the bitmap under each interval's mask. The closure
    // cannot return early, so the exit is per interval rather than per word —
    // still O(nruns + words touched) rather than O(run cardinality).
    let bitmap_run = match (a, b) {
        (Container::Bitmap(x), Container::Run(y)) => Some((x, y)),
        (Container::Run(y), Container::Bitmap(x)) => Some((x, y)),
        _ => None,
    };
    if let Some((bm, rn)) = bitmap_run {
        if let Some(bw) = bm.bits.try_words() {
            for i in 0..rn.nruns() {
                // `masked_intersects` exits at the first word that meets the
                // interval; the closure form this replaced could not, because
                // `for_each_masked_word` has no way to stop.
                if crate::ops::mixed::masked_intersects(bw, rn.start(i), rn.end(i)) {
                    return false;
                }
            }
            return true;
        }
    }

    // Array x run: one merge, costing the array's cardinality and never the run's.
    let array_run = match (a, b) {
        (Container::Array(x), Container::Run(y)) => Some((x, y)),
        (Container::Run(y), Container::Array(x)) => Some((x, y)),
        _ => None,
    };
    if let Some((arr, rn)) = array_run {
        let vals = arr.as_slice();
        let m = rn.nruns();
        let (mut i, mut j) = (0usize, 0u32);
        while i < vals.len() && j < m {
            let v = vals[i];
            if v < rn.start(j) {
                i += 1;
            } else if v > rn.end(j) {
                j += 1;
            } else {
                return false;
            }
        }
        return true;
    }

    let (probe, target) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    // Array x bitmap: direct word test rather than `Container::contains`, which
    // re-dispatches on the container kind for every value.
    if let (Container::Array(p), Container::Bitmap(t)) = (probe, target) {
        if let Some(tw) = t.bits.try_words() {
            return !p
                .as_slice()
                .iter()
                .any(|v| tw[*v as usize >> 6] & (1u64 << (*v & 63)) != 0);
        }
    }
    !probe.iter().any(|v| target.contains(v))
}

/// `b ⊆ a`, with early exit.
///
/// Same history and same rule as [`is_disjoint`]: one specialized arm until
/// 2026-08-26, five kind-pairs falling through to `b.iter().all(|v|
/// a.contains(v))`, and `run x run` at 86.6 us against the bitmap arm's 272 ns
/// because `iter()` enumerates the values of an interval container.
///
/// Unlike the other three this is **asymmetric** — `b` drives and the sides
/// cannot be swapped — so the arms are matched on the ordered pair, and the two
/// bitmap/run directions are genuinely different problems rather than one
/// problem written twice.
pub fn contains_all(a: &Container, b: &Container) -> bool {
    if b.is_empty() {
        return true;
    }
    if b.len() > a.len() {
        return false;
    }
    // `b` is non-empty and no larger, so a full `a` contains it.
    if a.is_full() {
        return true;
    }

    if let (Container::Bitmap(x), Container::Bitmap(y)) = (a, b) {
        if let (Some(wx), Some(wy)) = (x.bits.try_words(), y.bits.try_words()) {
            // See `ops::bitmap::words_contains`; the short-circuiting form
            // here cost 271 ns against `and_cardinality`'s 168 ns.
            return crate::ops::bitmap::words_contains(wx, wy);
        }
    }

    if let (Container::Array(x), Container::Array(y)) = (a, b) {
        return crate::ops::array::contains_all(x.as_slice(), y.as_slice());
    }

    // Run ⊇ run: every interval of `b` must sit inside one interval of `a`.
    // `a`'s intervals are disjoint and ordered, so the containing one is unique
    // and a single forward walk — or, when `a` has far more intervals than `b`,
    // a gallop — finds it.
    if let (Container::Run(x), Container::Run(y)) = (a, b) {
        return crate::ops::run::contains_all(x.as_flat(), y.as_flat());
    }

    // Bitmap ⊇ run: every word the interval covers must be *fully* set under the
    // mask. The dual of the `is_disjoint` arm, testing `== m` rather than `!= 0`.
    if let (Container::Bitmap(x), Container::Run(y)) = (a, b) {
        if let Some(bw) = x.bits.try_words() {
            for i in 0..y.nruns() {
                // Same reason as the `is_disjoint` arm: this exits at the
                // first word that is not fully covered.
                if !crate::ops::mixed::masked_covers(bw, y.start(i), y.end(i)) {
                    return false;
                }
            }
            return true;
        }
    }

    // Bitmap ⊇ array: probe by direct word test, no per-value kind dispatch.
    if let (Container::Bitmap(x), Container::Array(y)) = (a, b) {
        if let Some(bw) = x.bits.try_words() {
            return y
                .as_slice()
                .iter()
                .all(|v| bw[*v as usize >> 6] & (1u64 << (*v & 63)) != 0);
        }
    }

    // Run ⊇ array: one merge over the array's values and the run's intervals.
    if let (Container::Run(x), Container::Array(y)) = (a, b) {
        let (vals, n) = (y.as_slice(), x.nruns());
        let mut i = 0u32;
        for &v in vals {
            while i < n && x.end(i) < v {
                i += 1;
            }
            if i >= n || x.start(i) > v {
                return false;
            }
        }
        return true;
    }

    // The remaining directions have `a` sparser in *representation* than `b`
    // ( array ⊇ bitmap, array ⊇ run, run ⊇ bitmap ). Each is bounded by
    // `b.len() <= a.len()`, so `b` is small, and the matching `and_cardinality`
    // arm already walks them without allocating — a full count of a small `b` at
    // specialized speed beats a short-circuiting walk at dispatch speed.
    and_cardinality(a, b) == b.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container::{BitmapContainer, RunContainer};
    use crate::ops::generic::{apply, SetOp};

    fn kinds(v: &[u16]) -> Vec<Container> {
        vec![
            Container::from_sorted(v),
            Container::Bitmap(BitmapContainer::from_sorted(v)),
            Container::Run(RunContainer::from_sorted_values(v.iter().copied())),
        ]
    }

    #[test]
    fn cardinality_identities_match_materialized_results() {
        let av: Vec<u16> = (0..400u16).map(|i| i * 3).collect();
        let bv: Vec<u16> = (0..400u16).map(|i| i * 4).collect();

        for a in kinds(&av) {
            for b in kinds(&bv) {
                let mat = |op| apply(op, &a, &b).map(|c| c.len()).unwrap_or(0);
                assert_eq!(and_cardinality(&a, &b), mat(SetOp::And));
                assert_eq!(or_cardinality(&a, &b), mat(SetOp::Or));
                assert_eq!(xor_cardinality(&a, &b), mat(SetOp::Xor));
                assert_eq!(andnot_cardinality(&a, &b), mat(SetOp::AndNot));
            }
        }
    }

    /// The array x array arm must take *both* of its branches.
    ///
    /// `cardinality_identities_agree_for_every_kind_pair` pins the answer for
    /// every pair, but it cannot promise it ever generated operands skewed past
    /// `GALLOP_RATIO` — and the gallop branch is a separate algorithm, not a
    /// tuning of the merge. An off-by-one there is invisible on balanced
    /// operands, which is what the existing coverage mostly produces.
    #[test]
    fn the_array_arm_agrees_on_both_the_merge_and_the_gallop_branch() {
        // Balanced: ratio 1, so the merge branch.
        let bal_a: Vec<u16> = (0..600u16).map(|i| i * 3).collect();
        let bal_b: Vec<u16> = (0..600u16).map(|i| i * 4).collect();
        // Skewed 100:1, comfortably past GALLOP_RATIO = 32.
        let skew_a: Vec<u16> = (0..30u16).map(|i| i * 700).collect();
        let skew_b: Vec<u16> = (0..3000u16).map(|i| i * 7).collect();
        // Skewed and *disjoint*, which walks the small side to exhaustion, and
        // skewed where the small side runs past the large one's maximum, which
        // is the `j >= large.len()` early exit.
        let past_a: Vec<u16> = (0..20u16).map(|i| 40000 + i).collect();
        let past_b: Vec<u16> = (0..2000u16).collect();

        for (av, bv) in [
            (&bal_a, &bal_b),
            (&skew_a, &skew_b),
            (&skew_b, &skew_a),
            (&past_a, &past_b),
            (&past_b, &past_a),
        ] {
            let a = Container::from_sorted(av);
            let b = Container::from_sorted(bv);
            assert!(matches!(a, Container::Array(_)) && matches!(b, Container::Array(_)));
            assert_eq!(
                and_cardinality(&a, &b),
                apply(SetOp::And, &a, &b).map(|c| c.len()).unwrap_or(0),
                "array x array disagrees at {} x {}",
                av.len(),
                bv.len()
            );
        }
    }

    /// The run x run arms must take *both* of their branches, in both orders.
    ///
    /// The sibling of `the_array_arm_agrees_on_both_the_merge_and_the_gallop_branch`
    /// and it exists for the same reason: the gallop is a separate algorithm, not
    /// a tuning of the merge, and an off-by-one in it is invisible on the
    /// balanced operands the rest of the coverage produces.
    ///
    /// It is also the test that would have failed had this module kept its own
    /// copy of the two-pointer. These three arms delegate to `ops::run`, so a
    /// change made there reaches them; before 2026-08-28 they were a second
    /// implementation that inherited nothing.
    #[test]
    fn the_run_arms_agree_on_both_the_merge_and_the_gallop_branch() {
        let mk = |v: &[(u16, u16)]| Container::Run(RunContainer::from_pairs(v));
        // 1024 short intervals, and 8 long ones that each span many of them:
        // ratio 128, comfortably past GALLOP_RATIO = 32.
        let many: Vec<(u16, u16)> = (0..1024u16).map(|k| (k * 64 + 3, k * 64 + 40)).collect();
        let few: Vec<(u16, u16)> = (0..8u16)
            .map(|k| (k * 8192 + 100, k * 8192 + 4000))
            .collect();
        // Ratio 16, so the *same* shapes take the merge branch instead.
        let mid: Vec<(u16, u16)> = (0..64u16).map(|k| (k * 1024 + 7, k * 1024 + 600)).collect();
        // A genuine subset of `many`, for a `contains_all` that answers true.
        let sub: Vec<(u16, u16)> = many.iter().step_by(128).copied().collect();

        assert!(many.len() >= few.len() * crate::GALLOP_RATIO, "must gallop");
        assert!(many.len() < mid.len() * crate::GALLOP_RATIO, "must merge");

        let mut checked = 0usize;
        for av in [&many, &few, &mid, &sub] {
            for bv in [&many, &few, &mid, &sub] {
                let (a, b) = (mk(av), mk(bv));
                let n = apply(SetOp::And, &a, &b).map(|c| c.len()).unwrap_or(0);
                assert_eq!(
                    and_cardinality(&a, &b),
                    n,
                    "run x run cardinality disagrees at {} x {}",
                    av.len(),
                    bv.len()
                );
                assert_eq!(
                    is_disjoint(&a, &b),
                    n == 0,
                    "is_disjoint at {} x {}",
                    av.len(),
                    bv.len()
                );
                assert_eq!(
                    contains_all(&a, &b),
                    n == b.len(),
                    "contains_all at {} x {}",
                    av.len(),
                    bv.len()
                );
                checked += 1;
            }
        }
        assert_eq!(checked, 16);
        // Non-vacuity: the subset case really is a `true`, so `contains_all`
        // walked to the end rather than rejecting on its first interval.
        assert!(contains_all(&mk(&many), &mk(&sub)));
    }

    /// Every ordered kind-pair of both predicates against the counting oracle.
    ///
    /// `is_disjoint` and `contains_all` gained eight specialized arms on
    /// 2026-08-26, several with boundary logic that has no counterpart in the
    /// cardinality arms they mirror — the run-containment walk in particular has
    /// to reject an interval of `b` that *starts* inside one of `a` but *ends*
    /// past it, which no accumulator ever has to notice.
    ///
    /// The proptest layer pins these relations too, but its generators are
    /// boundary-biased on cardinality and prefix pattern, not on **interval
    /// alignment**, so it cannot promise it ever produced a `b` interval
    /// straddling the end of an `a` interval. These cases are chosen to.
    #[test]
    fn both_predicates_agree_with_the_counting_oracle_on_every_ordered_pair() {
        // Shapes chosen so that, once crossed with each other, they cover: exact
        // interval coincidence, a `b` interval starting inside an `a` interval
        // and running past its end, a `b` interval sitting strictly inside,
        // adjacency without overlap, and total disjointness.
        let shapes: Vec<Vec<u16>> = vec![
            vec![],
            vec![0],
            vec![65535],
            vec![0, 65535],
            (0..300u16).collect(),
            (0..300u16).map(|i| i * 2).collect(),
            (100..400u16).collect(),
            (150..250u16).collect(),
            (300..600u16).collect(),
            (299..301u16).collect(),
            (0..600u16).collect(),
            (0..5000u16).collect(),
            (0..5000u16).map(|i| i * 13).collect(),
            (2500..7500u16).collect(),
            (0..65536u32).map(|i| i as u16).collect(),
            (0..4096u16).map(|i| i * 16).collect(),
            vec![0, 1, 2, 64, 65, 127, 128, 8191, 8192],
        ];

        let mut checked = 0usize;
        for av in &shapes {
            for bv in &shapes {
                for a in kinds(av) {
                    for b in kinds(bv) {
                        // `kinds` forces a representation; skip the ones a real
                        // container could not be in, so this does not assert
                        // about states the crate never constructs.
                        if a.is_empty() != av.is_empty() || b.is_empty() != bv.is_empty() {
                            continue;
                        }
                        let n = and_cardinality(&a, &b);
                        assert_eq!(
                            is_disjoint(&a, &b),
                            n == 0,
                            "is_disjoint {:?}({}) x {:?}({}) disagrees with and_cardinality = {n}",
                            a.kind(),
                            av.len(),
                            b.kind(),
                            bv.len()
                        );
                        assert_eq!(
                            contains_all(&a, &b),
                            n == b.len(),
                            "contains_all {:?}({}) ⊇ {:?}({}) disagrees; and_cardinality = {n}, |b| = {}",
                            a.kind(),
                            av.len(),
                            b.kind(),
                            bv.len(),
                            b.len()
                        );
                        checked += 1;
                    }
                }
            }
        }
        // Guards against the loop silently skipping everything, which is how a
        // test like this stops testing without failing.
        assert!(checked > 2000, "only {checked} pairs exercised");
    }

    #[test]
    fn disjoint_and_subset_predicates() {
        let a = Container::from_sorted(&[1, 2, 3, 4]);
        let b = Container::from_sorted(&[3, 4]);
        let c = Container::from_sorted(&[90, 91]);

        assert!(!is_disjoint(&a, &b));
        assert!(is_disjoint(&a, &c));
        assert!(contains_all(&a, &b));
        assert!(!contains_all(&b, &a));
        assert!(!contains_all(&a, &c));
    }

    #[test]
    fn bitmap_specialization_agrees_with_generic_path() {
        let av: Vec<u16> = (0..5000u16).collect();
        let bv: Vec<u16> = (2500..7500u16).collect();
        let ab = Container::Bitmap(BitmapContainer::from_sorted(&av));
        let bb = Container::Bitmap(BitmapContainer::from_sorted(&bv));
        let ar = Container::Run(RunContainer::from_sorted_values(av.iter().copied()));
        let br = Container::Run(RunContainer::from_sorted_values(bv.iter().copied()));

        assert_eq!(and_cardinality(&ab, &bb), 2500);
        assert_eq!(and_cardinality(&ar, &br), 2500);
        assert!(!is_disjoint(&ab, &bb));
        assert!(contains_all(&ab, &ab));
    }

    #[test]
    fn full_container_fast_paths() {
        let full = Container::Run(RunContainer::from_pairs(&[(0, 65535)]));
        let a = Container::from_sorted(&[7, 8, 9]);
        assert_eq!(and_cardinality(&full, &a), 3);
        assert_eq!(and_cardinality(&a, &full), 3);
        assert_eq!(andnot_cardinality(&a, &full), 0);
        assert!(contains_all(&full, &a));
    }
}
