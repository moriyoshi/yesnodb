//! Extracting and querying one constituent of a [`View`].
//!
//! # The generic walk is the oracle
//!
//! [`OrdSet::view_select`] and [`OrdSet::view_cardinality`] both have a
//! specialised arm and a generic one. The generic one visits the constituent's
//! physical span and maps every ordinal through [`View::logical_of`]; it is
//! correct for every descriptor and is never deleted when an arm is faster —
//! same contract as [`ops::generic`](crate::ops::generic).
//!
//! # Why `range_summary` is still not used here
//!
//! **The reason changed on 2026-09-06 and the conclusion did not.** It used
//! to be a cost argument: [`OrdSet::range_summary`](crate::OrdSet::range_summary)
//! counted the whole range and then compared, with no short-circuit, so "is
//! anything in this slot" cost "how many are in it". That half is now false —
//! `range_summary` answers `Empty` from
//! [`Container::is_range_empty`](crate::container::Container::is_range_empty),
//! which stops at the first set value, and on a bitmap it reads only the words
//! the window covers rather than every word below `hi`.
//!
//! What survives is the **shape** of the question, and it is the load-bearing
//! half. Neither path here asks whether a slot is empty:
//! [`OrdSet::view_select`] needs every ordinal of the constituent and
//! [`OrdSet::view_cardinality`] needs a count, and no emptiness predicate,
//! however cheap, answers either. Reaching for one would mean *one call per
//! slot*, and per-slot probing is **quadratic** because the slots sweep forward
//! — every call re-enters at `partition_point` over the chunk directory, and a
//! predicate that is `O(window / 64)` still sums to `O(slots × chunk)` when the
//! windows tile the chunk. That is the same mistake `matrix/read.rs` records as
//! `O(lines × container size)` — 5.5 ms to move 8 KiB — so both paths here walk
//! once instead, which records the whole per-slot shape in one pass.
//!
//! # The one case that is nearly free
//!
//! Under [`ViewLayout::Blocked`] with a stride that is a multiple of 65 536, a
//! constituent occupies a whole number of chunks and its logical ordinals differ
//! from its physical ones by a multiple of the chunk width. So the low 16 bits
//! are unchanged, **every container is bit-for-bit the answer**, and extracting a
//! constituent is a prefix relabel with the payloads shared by refcount rather
//! than rebuilt. That is [`OrdSet::view_select`]'s specialised arm, and it is
//! `O(chunks)` with no payload access at all.

use super::{View, ViewLayout};
use crate::{chunk_base, split, OrdSet, ORDINAL_MAX};

/// The half-open physical range constituent `set` can occupy.
///
/// `None` when the constituent is not addressable at all. Under
/// [`ViewLayout::Interleaved`] this is the **whole universe** — a constituent's
/// ordinals are scattered at stride `sets`, not confined to a region — so it
/// bounds the walk only for `Blocked`.
fn physical_span(v: &View, set: u32) -> Option<(u64, u64)> {
    match v.layout() {
        ViewLayout::Interleaved => Some((set as u64, u64::MAX)),
        ViewLayout::Blocked { stride } => {
            let base = (set as u64).checked_mul(stride)?;
            (base <= ORDINAL_MAX).then(|| (base, base.saturating_add(stride)))
        }
    }
}

impl OrdSet {
    /// Visit every logical ordinal of constituent `set`, ascending.
    ///
    /// The generic path behind both public entry points.
    fn for_each_logical(&self, v: &View, set: u32, mut f: impl FnMut(u64)) {
        let Some((lo, hi)) = physical_span(v, set) else {
            return;
        };
        if hi <= lo {
            return;
        }
        let last = hi - 1;
        let (p_lo, _) = split(lo);
        let (p_hi, _) = split(last);

        let n = self.chunk_count();
        let mut i = self.partition_point_in(0, n, p_lo);
        while i < n {
            let Some((p, c)) = self.chunk_at(i) else {
                break;
            };
            if p > p_hi {
                break;
            }
            let cb = chunk_base(p);
            for val in c.iter() {
                let o = cb | val as u64;
                // Only the first chunk can hold values below `lo`; the container
                // iterates ascending, so the upper bound can break outright.
                if o < lo {
                    continue;
                }
                if o > last {
                    break;
                }
                if let Some((owner, x)) = v.logical_of(o) {
                    if owner == set {
                        f(x);
                    }
                }
            }
            i += 1;
        }
    }

    /// Extract constituent `set` as a set over its own logical ordinals.
    ///
    /// An empty set for a descriptor that does not [`View::check`] or a `set`
    /// that is out of range — absence, not an error, because a constituent that
    /// was never written is legitimately empty and the caller cannot tell the two
    /// apart from the data anyway.
    pub fn view_select(&self, v: &View, set: u32) -> OrdSet {
        if v.check().is_err() || set >= v.sets() {
            return OrdSet::new();
        }
        if let Some(out) = self.select_blocked_aligned(v, set) {
            return out;
        }
        let mut xs = Vec::new();
        self.for_each_logical(v, set, |x| xs.push(x));
        let mut out = OrdSet::from_iter_unsorted(xs);
        out.optimize();
        out
    }

    /// The chunk-aligned blocked arm: a prefix relabel, payloads shared.
    ///
    /// `None` when the layout is not blocked or the stride is not a whole number
    /// of chunks, in which case the generic walk answers.
    ///
    /// The containers are **cloned, which is a refcount bump** rather than a
    /// copy ( `Container::freeze` ), so this arm moves no bits at all. It is
    /// correct precisely because a multiple-of-65 536 shift leaves the low 16
    /// bits — the container's own value space — untouched.
    fn select_blocked_aligned(&self, v: &View, set: u32) -> Option<OrdSet> {
        let ViewLayout::Blocked { stride } = v.layout() else {
            return None;
        };
        if !stride.is_multiple_of(crate::CHUNK_CARD as u64) {
            return None;
        }
        let (lo, hi) = physical_span(v, set)?;
        let shift = lo >> crate::CHUNK_BITS;
        let p_end = hi >> crate::CHUNK_BITS;

        let n = self.chunk_count();
        let mut i = self.partition_point_in(0, n, lo >> crate::CHUNK_BITS);
        let mut chunks = Vec::new();
        while i < n {
            let Some((p, c)) = self.chunk_at(i) else {
                break;
            };
            if p >= p_end {
                break;
            }
            chunks.push((p - shift, c.clone()));
            i += 1;
        }
        Some(OrdSet::from_chunks(chunks))
    }

    /// Does constituent `set` contain logical ordinal `x`?
    ///
    /// One address computation and one membership test, so it costs what
    /// [`OrdSet::contains`] costs and never materialises the constituent. This is
    /// the one question both layouts answer equally cheaply.
    pub fn view_contains(&self, v: &View, set: u32, x: u64) -> bool {
        v.ordinal_of(set, x).is_some_and(|o| self.contains(o))
    }

    /// How many logical ordinals constituent `set` holds.
    ///
    /// **The two layouts differ by more than a constant here**, which is the
    /// whole reason [`ViewLayout`] is a parameter. Under `Blocked` a constituent
    /// is one contiguous range, so this is
    /// [`OrdSet::len_in_range`](crate::OrdSet::len_in_range) — `O(chunks
    /// touched)` with payload access at no more than two of them. Under
    /// `Interleaved` the constituent's ordinals are scattered at stride `sets`,
    /// so there is nothing to do but walk, and it is `O(nnz)`.
    pub fn view_cardinality(&self, v: &View, set: u32) -> u64 {
        if v.check().is_err() || set >= v.sets() {
            return 0;
        }
        if let ViewLayout::Blocked { .. } = v.layout() {
            let Some((lo, hi)) = physical_span(v, set) else {
                return 0;
            };
            return self.len_in_range(lo, hi);
        }
        let mut n = 0u64;
        self.for_each_logical(v, set, |_| n += 1);
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn set_of(xs: &[u64]) -> OrdSet {
        OrdSet::from_iter_unsorted(xs.iter().copied())
    }

    /// The independent recomputation every arm is checked against.
    fn oracle_select(packed: &OrdSet, v: &View, set: u32) -> BTreeSet<u64> {
        let mut out = BTreeSet::new();
        for (p, c) in packed.chunks() {
            for val in c.iter() {
                let o = chunk_base(p) | val as u64;
                if let Some((owner, x)) = v.logical_of(o) {
                    if owner == set {
                        out.insert(x);
                    }
                }
            }
        }
        out
    }

    fn as_btree(s: &OrdSet) -> BTreeSet<u64> {
        let mut out = BTreeSet::new();
        for (p, c) in s.chunks() {
            for val in c.iter() {
                out.insert(chunk_base(p) | val as u64);
            }
        }
        out
    }

    #[test]
    fn interleaved_select_picks_out_its_own_slots() {
        // Constituent 1 of 3 holds logical 0 and 2 -> physical 1 and 7.
        let packed = set_of(&[1, 7]);
        let v = View::interleaved(3);
        assert_eq!(as_btree(&packed.view_select(&v, 1)), BTreeSet::from([0, 2]));
        assert!(packed.view_select(&v, 0).is_empty());
        assert!(packed.view_select(&v, 2).is_empty());
    }

    #[test]
    fn blocked_select_shifts_by_the_region_base() {
        let v = View::blocked(3, 100);
        // Constituent 2 owns [200, 300); logical 5 and 99.
        let packed = set_of(&[205, 299]);
        assert_eq!(
            as_btree(&packed.view_select(&v, 2)),
            BTreeSet::from([5, 99])
        );
        assert!(packed.view_select(&v, 0).is_empty());
    }

    #[test]
    fn contains_and_cardinality_agree_with_select() {
        for v in [
            View::interleaved(3),
            View::blocked(3, 100),
            View::blocked(3, 65_536),
            View::blocked(3, 131_072),
        ] {
            let packed = OrdSet::from_iter_unsorted((0..4000u64).map(|i| i * 37));
            for set in 0..3u32 {
                let sel = packed.view_select(&v, set);
                assert_eq!(packed.view_cardinality(&v, set), sel.len(), "{v:?} {set}");
                for x in [0u64, 1, 5, 99, 100, 1000] {
                    assert_eq!(
                        packed.view_contains(&v, set, x),
                        sel.contains(x),
                        "{v:?} set={set} x={x}"
                    );
                }
            }
        }
    }

    /// The specialised arm and the generic walk are two total functions over
    /// the same domain, and this is the diff that keeps them honest. A stride of
    /// 65 536 and 131 072 reaches the arm; 100 000 does not, because it is not a
    /// whole number of chunks.
    #[test]
    fn the_aligned_arm_agrees_with_the_generic_walk() {
        let sources: Vec<OrdSet> = vec![
            OrdSet::new(),
            set_of(&[0, 1, 65_535, 65_536, 131_071, 131_072, 262_143]),
            OrdSet::from_iter_unsorted((0..300_000u64).filter(|i| i.is_multiple_of(3))),
            OrdSet::from_iter_unsorted(0..200_000u64),
            OrdSet::from_iter_unsorted((0..900u64).map(|i| i * 701)),
        ];
        let mut reached = 0u32;
        let mut nonempty = 0u32;
        for src in &sources {
            for stride in [65_536u64, 131_072, 100_000, 65_535] {
                let v = View::blocked(4, stride);
                for set in 0..4u32 {
                    let mut xs = Vec::new();
                    src.for_each_logical(&v, set, |x| xs.push(x));
                    let mut want = OrdSet::from_iter_unsorted(xs);
                    want.optimize();

                    if let Some(got) = src.select_blocked_aligned(&v, set) {
                        reached += 1;
                        assert_eq!(as_btree(&got), as_btree(&want), "stride={stride} set={set}");
                        nonempty += u32::from(!got.is_empty());
                    }
                    // And the public entry point agrees either way.
                    assert_eq!(
                        as_btree(&src.view_select(&v, set)),
                        as_btree(&want),
                        "stride={stride} set={set}"
                    );
                }
            }
        }
        // Without these the test passes on an arm that never fires, or on
        // pairs of empty sets.
        assert!(reached > 20, "the aligned arm fired only {reached} times");
        assert!(nonempty > 5, "only {nonempty} non-empty aligned results");
    }

    #[test]
    fn select_agrees_with_the_oracle_on_both_layouts() {
        let sources: Vec<OrdSet> = vec![
            OrdSet::new(),
            set_of(&[0, 1, 2, 65_535, 65_536, ORDINAL_MAX]),
            OrdSet::from_iter_unsorted((0..5000u64).map(|i| i * 13)),
            OrdSet::from_iter_unsorted(0..70_000u64),
        ];
        let mut compared = 0u32;
        let mut nonempty = 0u32;
        for src in &sources {
            for v in [
                View::interleaved(1),
                View::interleaved(2),
                View::interleaved(7),
                View::blocked(3, 1),
                View::blocked(3, 100),
                View::blocked(2, 65_536),
            ] {
                for set in 0..v.sets() {
                    let got = src.view_select(&v, set);
                    let want = oracle_select(src, &v, set);
                    assert_eq!(as_btree(&got), want, "{v:?} set={set}");
                    assert_eq!(src.view_cardinality(&v, set), want.len() as u64, "{v:?}");
                    compared += 1;
                    nonempty += u32::from(!want.is_empty());
                }
            }
        }
        assert!(compared > 50, "only {compared} comparisons");
        assert!(
            nonempty > 20,
            "only {nonempty} of {compared} were non-empty"
        );
    }

    /// A constituent whose region starts past the ordinal ceiling holds nothing,
    /// rather than wrapping into another constituent's ordinals.
    ///
    /// Constituent 4 is the one that matters: `4 * 2^62` is `2^64`, which
    /// overflows a `u64` outright, so `ordinal_of` must decline rather than wrap
    /// to zero and hand back constituent 0's contents. Constituent 3 at
    /// `3 * 2^62` is still addressable and is included so the test distinguishes
    /// "out of range" from "merely large".
    #[test]
    fn an_unaddressable_constituent_is_empty_not_wrapped() {
        let v = View::blocked(5, 1 << 62);
        let packed = set_of(&[0, 1 << 62, 3 * (1u64 << 62)]);
        assert_eq!(packed.view_select(&v, 0).len(), 1);
        assert_eq!(packed.view_select(&v, 1).len(), 1);
        assert_eq!(packed.view_select(&v, 3).len(), 1, "still addressable");

        assert_eq!(v.ordinal_of(4, 0), None, "4 * 2^62 overflows a u64");
        assert!(packed.view_select(&v, 4).is_empty());
        assert_eq!(packed.view_cardinality(&v, 4), 0);
    }
}
