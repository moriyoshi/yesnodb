//! The `u64` ordinal ↔ PostgreSQL `bigint` mapping, and what it costs.
//!
//! # The mapping
//!
//! A yesno ordinal is a `u64` in `[0, ORDINAL_MAX]`. PostgreSQL has no unsigned
//! 64-bit type, so the column is `bigint` and the mapping is a **bit
//! reinterpretation**: ordinals at or above `2^63` surface as negative numbers.
//!
//! That is a bijection, so it is exact for equality, `IN`, membership, joins and
//! `count(*)`. It is **not** order-preserving, and that single fact is the
//! source of every subtlety in this module: `int8` sorts `[2^63, 2^64)` *below*
//! `[0, 2^63)`, so the two types disagree about which of two values is larger
//! whenever one of them is ≥ `2^63`.
//!
//! Three consequences, each a wrong-answer bug if missed:
//!
//! - **No `pathkeys` may be declared on any scan path.** yesno emits ordinals
//!   in `u64` order; PostgreSQL would believe that is `int8` order and skip a
//!   sort it needs.
//! - **A range qual can require two ranges.** See [`i64_range_to_ordinals`].
//! - **`min` / `max` must not be pushed down.** `Snapshot::{min,max}` are
//!   `u64` extremes; PostgreSQL wants `int8` extremes. They differ for any set
//!   spanning `2^63`.
//!
//! # Bound conventions
//!
//! yesno uses **both** conventions, deliberately, and this crate touches both.
//! `Db::insert_range` / `remove_range` are **inclusive** `[lo, hi]`;
//! `Expr::Range`, `Snapshot::len_in_range` and `range_summary` are **half-open**
//! `[lo, hi)`. Everything here produces the half-open form, because that is what
//! the read path consumes; the write path converts at its own boundary.
//!
//! `u64::MAX` is not a valid ordinal — `ORDINAL_MAX` is `u64::MAX - 1` — which
//! is exactly the headroom that lets a half-open upper bound name the whole
//! universe as `[0, u64::MAX)` without needing the unrepresentable `2^64`. The
//! saturating arithmetic below relies on that and is correct *because* of it.

/// Reinterpret an ordinal as the `bigint` PostgreSQL will see.
///
/// Total and lossless in both directions; see [`i64_to_ordinal`].
#[inline]
pub const fn ordinal_to_i64(ordinal: u64) -> i64 {
    ordinal as i64
}

/// Reinterpret a `bigint` datum as an ordinal.
#[inline]
pub const fn i64_to_ordinal(value: i64) -> u64 {
    value as u64
}

/// Up to two half-open ordinal ranges.
///
/// Two, not one, because an `int8` interval that straddles zero is two disjoint
/// intervals in `u64` order. Fixed capacity rather than a `Vec`: this is built
/// per qual during planning and there is never a third.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OrdinalRanges {
    ranges: [(u64, u64); 2],
    len: u8,
}

impl OrdinalRanges {
    /// The empty set — a qual that can match nothing.
    pub const EMPTY: Self = OrdinalRanges {
        ranges: [(0, 0); 2],
        len: 0,
    };

    fn one(lo: u64, hi: u64) -> Self {
        OrdinalRanges {
            ranges: [(lo, hi), (0, 0)],
            len: 1,
        }
    }

    fn two(a: (u64, u64), b: (u64, u64)) -> Self {
        OrdinalRanges {
            ranges: [a, b],
            len: 2,
        }
    }

    /// The half-open `[lo, hi)` ranges, in ascending `u64` order.
    #[inline]
    pub fn as_slice(&self) -> &[(u64, u64)] {
        &self.ranges[..self.len as usize]
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whether `ordinal` falls in any of the ranges. Used only by tests and by
    /// the brute-force oracle; the planner lowers to set operations instead.
    pub fn contains(&self, ordinal: u64) -> bool {
        self.as_slice()
            .iter()
            .any(|&(lo, hi)| ordinal >= lo && ordinal < hi)
    }
}

/// Lower an **inclusive** `int8` interval `[lo, hi]` to half-open ordinal ranges.
///
/// Inclusive on the way in because that is the shape a SQL `BETWEEN` and a pair
/// of `>=` / `<=` quals arrive as; half-open on the way out because that is what
/// `Expr::Range` takes.
///
/// # Why two ranges
///
/// `int8` order and `u64` order agree on the non-negative half and on the
/// negative half separately, but not across the boundary. An interval entirely
/// on one side maps to one contiguous `u64` range. An interval that **straddles
/// zero** — `BETWEEN -5 AND 5`, say — is the union of the *top* of the `u64`
/// space and the *bottom* of it, with everything in between excluded:
///
/// ```text
///   int8:   -5 ......... -1 | 0 ......... 5
///   u64:    2^64-5 ... 2^64-1 | 0 ......... 5
///           \___ range 2 ___/   \_ range 1 _/
/// ```
///
/// Lowering that to the single range `[lo as u64, hi as u64 + 1)` yields
/// `[2^64-5, 6)`, which is empty — the qual silently returns **no rows**. That
/// is the failure this function exists to prevent, and it is why the two-range
/// case has its own test.
pub fn i64_range_to_ordinals(lo: i64, hi: i64) -> OrdinalRanges {
    if lo > hi {
        return OrdinalRanges::EMPTY;
    }

    // `hi` is inclusive, so the exclusive bound is `hi + 1`. Saturating is not a
    // fudge: the only value that saturates is `hi as u64 == u64::MAX`, and
    // `u64::MAX` is reserved rather than an ordinal ( `ORDINAL_MAX` is
    // `u64::MAX - 1` ), so `[lo, u64::MAX)` already covers every ordinal at or
    // above `lo`. This is the crate's own "a half-open range can name the whole
    // universe" convention.
    let excl = |v: i64| (v as u64).saturating_add(1);

    match (lo < 0, hi < 0) {
        // Wholly negative: both map into the upper half, order preserved.
        (true, true) => OrdinalRanges::one(lo as u64, excl(hi)),
        // Wholly non-negative: both map into the lower half, order preserved.
        (false, false) => OrdinalRanges::one(lo as u64, excl(hi)),
        // Straddles zero. Emitted low-range-first so `as_slice` is ascending in
        // `u64` order, which is the order every consumer here wants.
        (true, false) => OrdinalRanges::two((0, excl(hi)), (lo as u64, u64::MAX)),
        // `lo >= 0 && hi < 0` means `lo > hi` in `int8` order, handled above.
        (false, true) => unreachable!("lo > hi is rejected before this match"),
    }
}

/// Lower a single-point `int8` equality to a half-open range.
///
/// Immune to the ordering hazard: a bit reinterpretation is a bijection, so one
/// `bigint` is one ordinal whatever its sign.
pub fn i64_point_to_ordinals(value: i64) -> OrdinalRanges {
    let o = i64_to_ordinal(value);
    // `u64::MAX` is not an ordinal, so a qual naming it matches nothing rather
    // than overflowing into an empty range by accident.
    if o == u64::MAX {
        return OrdinalRanges::EMPTY;
    }
    OrdinalRanges::one(o, o + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The values where the mapping's behaviour changes. Every test below is
    /// driven from these rather than from uniform random `i64`s, which would
    /// essentially never land on one.
    const BOUNDARY: &[i64] = &[
        i64::MIN,     // ordinal 2^63
        i64::MIN + 1, // ordinal 2^63 + 1
        -2,
        -1, // ordinal u64::MAX — reserved, not a valid ordinal
        0,
        1,
        2,
        i64::MAX - 1,
        i64::MAX, // ordinal 2^63 - 1
    ];

    #[test]
    fn the_mapping_round_trips_on_every_boundary() {
        for &v in BOUNDARY {
            assert_eq!(ordinal_to_i64(i64_to_ordinal(v)), v, "i64 {v}");
        }
        for &o in &[0u64, 1, (1 << 63) - 1, 1 << 63, u64::MAX - 1, u64::MAX] {
            assert_eq!(i64_to_ordinal(ordinal_to_i64(o)), o, "ordinal {o}");
        }
    }

    #[test]
    fn a_range_straddling_zero_becomes_two_ranges() {
        let r = i64_range_to_ordinals(-5, 5);
        assert_eq!(r.as_slice().len(), 2, "straddling range must split");
        assert_eq!(r.as_slice()[0], (0, 6));
        assert_eq!(r.as_slice()[1], (u64::MAX - 4, u64::MAX));

        // The whole point: the values at both ends are present, and nothing in
        // the enormous gap between them is.
        for v in [-5i64, -2, 0, 5] {
            assert!(r.contains(i64_to_ordinal(v)), "{v} must be in range");
        }
        for v in [-6i64, 6] {
            assert!(!r.contains(i64_to_ordinal(v)), "{v} must not be in range");
        }
        assert!(!r.contains(1 << 63), "the gap must be excluded");

        // `-1` is inside the *int8* interval and is still absent, because it
        // maps to `u64::MAX`, which is reserved rather than an ordinal. Not an
        // off-by-one: a set cannot contain it, so a qual cannot select it.
        assert_eq!(i64_to_ordinal(-1), u64::MAX);
        assert!(!r.contains(u64::MAX));
    }

    #[test]
    fn wholly_negative_and_wholly_positive_ranges_stay_single() {
        // `-10 as u64` is `2^64 - 10` = `u64::MAX - 9`; the exclusive bound is
        // `(-5 as u64) + 1` = `2^64 - 4` = `u64::MAX - 3`.
        let neg = i64_range_to_ordinals(-10, -5);
        assert_eq!(neg.as_slice(), &[(u64::MAX - 9, u64::MAX - 3)]);
        assert!(neg.contains(i64_to_ordinal(-10)));
        assert!(neg.contains(i64_to_ordinal(-5)));
        assert!(!neg.contains(i64_to_ordinal(-4)));
        assert!(!neg.contains(i64_to_ordinal(-11)));

        let pos = i64_range_to_ordinals(5, 10);
        assert_eq!(pos.as_slice(), &[(5, 11)]);
    }

    /// The boundary `u64::MAX` is reserved exists for. An inclusive `hi` of
    /// `-1` is ordinal `u64::MAX`, whose exclusive successor is unrepresentable;
    /// saturating to `u64::MAX` is correct because no ordinal is excluded by it.
    #[test]
    fn the_reserved_top_of_the_universe_saturates_rather_than_wrapping() {
        let r = i64_range_to_ordinals(-1, -1);
        assert_eq!(r.as_slice(), &[(u64::MAX, u64::MAX)]);
        // Empty as a half-open range, which is right: u64::MAX is not an ordinal.
        assert!(!r.contains(u64::MAX));
        assert!(!r.contains(u64::MAX - 1));

        let full_top = i64_range_to_ordinals(i64::MIN, -1);
        assert_eq!(full_top.as_slice(), &[(1 << 63, u64::MAX)]);
        assert!(full_top.contains(1 << 63));
        assert!(
            full_top.contains(u64::MAX - 1),
            "ORDINAL_MAX must be included"
        );
    }

    #[test]
    fn the_full_int8_domain_covers_every_ordinal_but_the_reserved_one() {
        let r = i64_range_to_ordinals(i64::MIN, i64::MAX);
        assert_eq!(r.as_slice().len(), 2);
        for o in [0u64, 1, (1 << 63) - 1, 1 << 63, u64::MAX - 1] {
            assert!(r.contains(o), "ordinal {o} must be covered");
        }
        assert!(
            !r.contains(u64::MAX),
            "u64::MAX is reserved, never an ordinal"
        );
    }

    #[test]
    fn an_inverted_range_is_empty() {
        assert!(i64_range_to_ordinals(5, -5).is_empty());
        assert!(i64_range_to_ordinals(1, 0).is_empty());
        assert!(i64_range_to_ordinals(i64::MAX, i64::MIN).is_empty());
    }

    #[test]
    fn a_point_range_holds_exactly_one_ordinal() {
        for &v in BOUNDARY {
            let r = i64_point_to_ordinals(v);
            let o = i64_to_ordinal(v);
            if o == u64::MAX {
                assert!(r.is_empty(), "u64::MAX is not an ordinal");
                continue;
            }
            assert!(r.contains(o), "{v} must contain itself");
            assert!(!r.contains(o.wrapping_add(1)));
            assert!(!r.contains(o.wrapping_sub(1)));
        }
    }

    /// The property that actually pins the lowering: for every pair of boundary
    /// endpoints, membership in the lowered ranges must agree with evaluating
    /// the original `int8` predicate directly, over a probe set that includes
    /// both sides of the sign boundary.
    ///
    /// This is the test that fails if the straddling case is lowered as one
    /// range. Verified by doing exactly that on 2026-08-29: replacing the
    /// straddling arm with `OrdinalRanges::one(lo as u64, excl(hi))` turns this
    /// test red, along with `a_range_straddling_zero_becomes_two_ranges` and
    /// `the_full_int8_domain_covers_every_ordinal_but_the_reserved_one`.
    #[test]
    fn lowering_agrees_with_the_int8_predicate_on_every_boundary_pair() {
        let probes: Vec<i64> = BOUNDARY
            .iter()
            .flat_map(|&v| [v.saturating_sub(1), v, v.saturating_add(1)])
            .collect();

        for &lo in BOUNDARY {
            for &hi in BOUNDARY {
                let lowered = i64_range_to_ordinals(lo, hi);
                for &p in &probes {
                    let want = p >= lo && p <= hi && i64_to_ordinal(p) != u64::MAX;
                    let got = lowered.contains(i64_to_ordinal(p));
                    assert_eq!(
                        want,
                        got,
                        "BETWEEN {lo} AND {hi}, probe {p} \
                         (ordinal {}): predicate says {want}, lowering says {got}",
                        i64_to_ordinal(p)
                    );
                }
            }
        }
    }
}
