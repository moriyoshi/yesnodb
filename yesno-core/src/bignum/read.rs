//! `OrdSet` -> `BigUint`: the set read as a number.

use super::{BigUint, INLINE_LIMBS};
use crate::container::Container;
use crate::{OrdSet, CHUNK_CARD};

/// Limbs in one chunk. 65 536 bits over a 64-bit limb, exactly.
const LIMBS_PER_CHUNK: usize = CHUNK_CARD as usize / 64;

/// Set every bit in `[lo, hi]` inclusive, by whole words where it can.
///
/// The two partial ends are masked and the middle is a `fill`, so the cost is
/// proportional to the words the interval covers rather than to the bits it
/// contains. `lo <= hi` and `hi / 64` must be in range, which both callers
/// establish by clamping to the requested width first.
fn fill_bits(limbs: &mut [u64], lo: u64, hi: u64) {
    debug_assert!(lo <= hi);
    let (wlo, whi) = ((lo / 64) as usize, (hi / 64) as usize);
    debug_assert!(whi < limbs.len());
    // All ones at and above `lo % 64`; all ones at and below `hi % 64`.
    let low_mask = u64::MAX << (lo % 64);
    let high_mask = u64::MAX >> (63 - (hi % 64));
    if wlo == whi {
        // One word: both ends land in it, so the masks intersect.
        limbs[wlo] |= low_mask & high_mask;
        return;
    }
    limbs[wlo] |= low_mask;
    limbs[wlo + 1..whi].fill(u64::MAX);
    limbs[whi] |= high_mask;
}

impl OrdSet {
    /// This set read as an integer, keeping only its low `width_bits` bits.
    ///
    /// The ordinal `j` carries the `2^j` term, so the set and the value are the
    /// same object and this is a change of view rather than a conversion.
    ///
    /// **Reading at a narrower `width_bits` than the value occupies yields
    /// exactly `x mod 2^width_bits`.** That is a consequence of the
    /// least-significant-bit-first ordering, and it is the only thing the width
    /// argument does. Pass `u64::MAX` for the whole set.
    ///
    /// # Cost
    ///
    /// Allocates one limb per 64 bits of `min( width_bits, max + 1 )`, so an
    /// over-large width costs nothing extra -- the bits above the set's highest
    /// member are zero and are not materialized. **The set's own extent is the
    /// other half of that bound and is not capped here**: a sparse set with a
    /// distant maximum is cheap to store and expensive to render, so a caller
    /// admitting one from outside must bound the width itself. The wire format
    /// does exactly that.
    pub fn read_int(&self, width_bits: u64) -> BigUint {
        let Some(max) = self.max() else {
            return BigUint::zero();
        };
        // `max` is an ordinal, so `max + 1` cannot overflow: `u64::MAX` is not
        // an ordinal ( invariant I8 ).
        let bits = width_bits.min(max + 1);
        let words = bits.div_ceil(64) as usize;

        // **Two cases, and they want opposite buffers.**
        //
        // A value of `INLINE_LIMBS` or fewer is stored in registers, so nothing
        // keeps the buffer it was assembled in -- the `vec![0u64; words]` this
        // used to take unconditionally was a malloc and a free of a scratch
        // space that was then discarded. It goes on the stack instead. No
        // allocator, no thread-local, and the one- or two-word move into inline
        // storage happens either way, so avoiding the allocation costs no copy.
        if words <= INLINE_LIMBS {
            let mut limbs = [0u64; INLINE_LIMBS];
            self.fill_limbs(&mut limbs[..words], bits);
            let len = limbs.iter().rposition(|&w| w != 0).map_or(0, |i| i + 1);
            return BigUint::from_slice(&limbs, len);
        }

        // A wider value **owns** its buffer: `from_limbs_le` takes the vector by
        // value and moves it. So it is allocated once, here, and filled in
        // place. Routing this through a shared buffer instead forces a copy out
        // of it and regressed a 65 536-bit read from 123 ns to 198 ns -- which
        // is the trade `from_limbs_le`'s own comment records from the other
        // direction. A shared buffer pays where the buffer is transient; it
        // cannot pay where the buffer becomes the result.
        let mut limbs = vec![0u64; words];
        self.fill_limbs(&mut limbs, bits);
        BigUint::from_limbs_le(limbs)
    }

    /// Set the bits of `self` below `bits` into `limbs`, which must be exactly
    /// `bits.div_ceil( 64 )` words and start zeroed.
    ///
    /// Shared by both buffer cases above so the two cannot drift: the size-class
    /// choice is about *where the words live*, never about which bits are set.
    #[inline]
    fn fill_limbs(&self, limbs: &mut [u64], bits: u64) {
        debug_assert_eq!(limbs.len(), bits.div_ceil(64) as usize);
        // Re-slice to the exact length once. Without this the bounds check on
        // every `limbs[...]` write reloads the length from the fat pointer,
        // where the single-path version had it as a known local.
        let words = limbs.len();
        let limbs = &mut limbs[..words];
        // Per container, not per ordinal. A chunk is 65 536 bits and a limb is
        // 64, so a chunk covers exactly `LIMBS_PER_CHUNK` whole limbs starting
        // at a limb boundary -- which is what makes a bitmap a block transfer
        // rather than a bit loop, and is the property the stride used to break.
        for (prefix, container) in self.chunks() {
            let base = prefix * CHUNK_CARD as u64;
            if base >= bits {
                // Chunks ascend, so nothing below this one contributes either.
                break;
            }
            let lo = (base / 64) as usize;
            match container {
                Container::Bitmap(b) => {
                    let src = b.words();
                    let n = LIMBS_PER_CHUNK.min(words - lo);
                    limbs[lo..lo + n].copy_from_slice(&src[..n]);
                }
                // A run is a list of intervals, and an interval of set bits
                // maps onto whole words of `u64::MAX` with a partial word at
                // each end. **Walking it ordinal by ordinal was measured at
                // 105 ns per limb against a bitmap's 0.1, at every width from
                // 64 to 65 536 bits** -- and it is the *densest* values that
                // land here, because a contiguous stretch of ones coalesces
                // into one run. An all-ones integer is the simplest bit
                // pattern there is and it was the worst case.
                Container::Run(r) => {
                    for i in 0..r.nruns() {
                        let lo = base + u64::from(r.start(i));
                        let hi = base + u64::from(r.end(i)); // inclusive
                        if lo >= bits {
                            break;
                        }
                        fill_bits(limbs, lo, hi.min(bits - 1));
                    }
                }
                // An array has no word view and no interval structure, so it
                // sets what it holds -- but over `as_slice` rather than
                // `Container::iter`, which wraps the slice iterator in an enum
                // and so pays a discriminant branch on every value. At half
                // density that is 32 values per limb.
                Container::Array(a) => {
                    // Values are **sorted**, and that is load-bearing twice
                    // over. It means a limb's values arrive consecutively, so a
                    // register accumulator and one store per limb replaces a
                    // load-or-store per value. And it means **one comparison
                    // settles eight**: if the first and last of a block of
                    // eight share a limb, so does everything between, so the
                    // per-value compare-and-branch disappears for the block.
                    //
                    // A NEON `vceqq_u16` was measured doing the same eight-wide
                    // test at 1.50x, against 2.13x here, at 32 values per limb
                    // -- and it *lost* below about ten values per limb where
                    // this still wins 1.40x. The vector compare was a proxy for
                    // the sortedness; using the sortedness directly is both
                    // faster and free of `unsafe`.
                    let vals = a.as_slice();
                    let mut cur = usize::MAX;
                    let mut acc = 0u64;
                    let mut i = 0;
                    'values: while i < vals.len() {
                        if i + 8 <= vals.len() {
                            let first = base + u64::from(vals[i]);
                            let last = base + u64::from(vals[i + 7]);
                            // `last < bits` is **defensive, not load-bearing**,
                            // and deleting it passes every test here -- so do
                            // not delete it on that evidence. The fast path can
                            // only write to `cur`, which a scalar step already
                            // proved is a valid limb, so a spurious bit can
                            // only land in the final limb, where the tail mask
                            // below clears it. The guard keeps the bound local
                            // instead of resting on that distant argument.
                            if last < bits
                                && (first / 64) as usize == cur
                                && (last / 64) as usize == cur
                            {
                                for k in 0..8 {
                                    let o = base + u64::from(vals[i + k]);
                                    acc |= 1u64 << (o % 64);
                                }
                                i += 8;
                                continue;
                            }
                            // A miss consumes the whole block, so the test is
                            // paid **once per eight and not once per value**.
                            // Consuming one and retesting made the sparse rows
                            // twice as slow as no blocking at all, because the
                            // test never hits when a limb holds fewer than
                            // eight values and every value then paid for it.
                            for _ in 0..8 {
                                let ordinal = base + u64::from(vals[i]);
                                if ordinal >= bits {
                                    break 'values;
                                }
                                let w = (ordinal / 64) as usize;
                                if w != cur {
                                    if cur != usize::MAX {
                                        limbs[cur] |= acc;
                                    }
                                    cur = w;
                                    acc = 0;
                                }
                                acc |= 1u64 << (ordinal % 64);
                                i += 1;
                            }
                            continue;
                        }
                        let ordinal = base + u64::from(vals[i]);
                        if ordinal >= bits {
                            break;
                        }
                        let w = (ordinal / 64) as usize;
                        if w != cur {
                            if cur != usize::MAX {
                                limbs[cur] |= acc;
                            }
                            cur = w;
                            acc = 0;
                        }
                        acc |= 1u64 << (ordinal % 64);
                        i += 1;
                    }
                    if cur != usize::MAX {
                        limbs[cur] |= acc;
                    }
                }
            }
        }

        // A block transfer can carry bits above the requested width, and a
        // partial top limb must not keep them: the value is `x mod 2^bits`.
        let tail = bits % 64;
        if tail != 0 {
            if let Some(last) = limbs.last_mut() {
                *last &= (1u64 << tail) - 1;
            }
        }

        // The one place the canonical invariant is established, so no kernel
        // ever has to wonder.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set_of(ordinals: &[u64]) -> OrdSet {
        OrdSet::from_iter_unsorted(ordinals.iter().copied())
    }

    #[test]
    fn an_empty_set_is_zero() {
        assert_eq!(set_of(&[]).read_int(u64::MAX), BigUint::zero());
        assert_eq!(set_of(&[]).read_int(0), BigUint::zero());
    }

    #[test]
    fn the_set_and_the_value_are_the_same_object() {
        // 0b1011 = 11.
        assert_eq!(set_of(&[0, 1, 3]).read_int(u64::MAX), BigUint::from_u64(11));
        assert_eq!(set_of(&[63]).read_int(u64::MAX), BigUint::from_u64(1 << 63));
    }

    /// The identity the ordering exists to buy.
    #[test]
    fn a_narrower_read_is_the_value_modulo_two_to_the_width() {
        let s = set_of(&[0, 1, 3, 64, 130]);
        let whole = s.read_int(u64::MAX);
        for w in [0u64, 1, 2, 4, 63, 64, 65, 129, 130, 131, 4096] {
            assert_eq!(s.read_int(w), whole.truncate(w), "width {w}");
        }
    }

    /// A chunk is 65 536 bits and a limb is 64, so a chunk boundary is a limb
    /// boundary. Nothing straddles, which is what removing the stride bought.
    #[test]
    fn a_value_spanning_chunks_reads_whole() {
        let s = set_of(&[0, 65_535, 65_536, 131_071, 200_000]);
        let v = s.read_int(u64::MAX);
        assert_eq!(v.bit_len(), 200_001);
        for bit in [0u64, 65_535, 65_536, 131_071, 200_000] {
            assert!(v.bit(bit), "bit {bit}");
        }
        assert!(!v.bit(1));
        assert!(!v.bit(65_537));
    }

    /// Every representation must read the same, and the block-transfer arm is
    /// the one that can differ -- it copies whole words where the others set
    /// bits, so a value that reads correctly as an array and wrongly as a
    /// bitmap is exactly the bug this shape invites.
    ///
    /// The three constructions are chosen to *land* on the three kinds, and the
    /// test asserts which kind it got rather than assuming: an encoding policy
    /// that re-selected representations would otherwise leave an arm untested
    /// while this still passed.
    #[test]
    fn every_container_representation_reads_the_same() {
        use crate::container::ContainerKind;

        // Sparse -> array. Dense -> bitmap. One long stretch -> run.
        let array: Vec<u64> = (0..64u64).map(|i| i * 900).collect();
        let bitmap: Vec<u64> = (0..40_000u64).map(|i| i * 3 / 2).collect();
        let run: Vec<u64> = (1_000..50_000u64).collect();

        for (name, ordinals, want) in [
            ("array", array, ContainerKind::Array),
            ("bitmap", bitmap, ContainerKind::Bitmap),
            ("run", run, ContainerKind::Run),
        ] {
            let mut s = set_of(&ordinals);
            // `optimize` re-selects by serialized size, which is what promotes
            // a single long stretch to a run; the bulk builder alone does not.
            s.optimize();
            let kind = s.chunk_at(0).expect("one chunk").1.kind();
            assert_eq!(kind, want, "{name} did not land on its representation");

            let v = s.read_int(u64::MAX);
            // Against the definition, bit by bit, rather than against another
            // path through the same code.
            for o in &ordinals {
                assert!(v.bit(*o), "{name}: bit {o}");
            }
            assert_eq!(v.count_ones(), ordinals.len() as u64, "{name}: popcount");
            assert_eq!(v.bit_len(), ordinals.last().unwrap() + 1, "{name}: bit_len");
            // And the round trip recovers the set from every representation.
            assert_eq!(OrdSet::from_int(&v).unwrap(), s, "{name}: round trip");
        }
    }

    /// The block-transfer arm copies whole limbs, so a width that stops inside
    /// a bitmap chunk must still mask the bits above it.
    #[test]
    fn a_narrow_read_of_a_bitmap_masks_the_bits_above_the_width() {
        let s = set_of(&(0..40_000u64).map(|i| i * 3 / 2).collect::<Vec<_>>());
        assert_eq!(
            s.chunk_at(0).unwrap().1.kind(),
            crate::container::ContainerKind::Bitmap
        );
        let whole = s.read_int(u64::MAX);
        for w in [1u64, 63, 64, 65, 1_000, 59_999, 60_000] {
            let got = s.read_int(w);
            assert_eq!(got, whole.truncate(w), "width {w}");
            assert!(got.bit_len() <= w, "width {w} leaked a bit above itself");
        }
    }

    /// A value whose bits are interleaved with other values' in one ordinal
    /// space is read by selecting the constituent first. The composition is
    /// what makes several integers per set a *view* question rather than a
    /// layout one.
    #[test]
    fn a_constituent_of_an_interleaved_view_reads_as_its_own_value() {
        use crate::view::View;

        let sets = 3u32;
        let view = View::interleaved(sets);
        let values = [
            BigUint::from_u64(0b1011),
            BigUint::from_u64(0),
            BigUint::from_limbs_le(vec![0xDEAD_BEEF, 0x99]),
        ];

        // Pack them: constituent i's logical ordinal x sits at x * sets + i.
        let mut packed: Vec<u64> = Vec::new();
        for (i, v) in values.iter().enumerate() {
            for x in 0..v.bit_len() {
                if v.bit(x) {
                    packed.push(x * u64::from(sets) + i as u64);
                }
            }
        }
        let packed = set_of(&packed);

        for (i, want) in values.iter().enumerate() {
            let constituent = packed.view_select(&view, i as u32);
            assert_eq!(&constituent.read_int(u64::MAX), want, "constituent {i}");
        }
    }

    /// An over-large width is free: the cost follows the data, not the ask.
    #[test]
    fn an_over_large_width_costs_no_extra_limbs() {
        let s = set_of(&[0, 1]);
        assert_eq!(s.read_int(u64::MAX).limbs().len(), 1);
        assert_eq!(s.read_int(u64::MAX), s.read_int(2));
    }
}

#[cfg(test)]
mod run_fill_tests {
    use super::*;
    use crate::ContainerKind;

    /// The obvious implementation, one ordinal at a time. This is what the run
    /// arm used to be, kept here as the oracle it should agree with.
    fn naive_read(s: &OrdSet, bits: u64) -> Vec<u64> {
        let mut limbs = vec![0u64; bits.div_ceil(64) as usize];
        for o in s.iter() {
            if o >= bits {
                break;
            }
            limbs[(o / 64) as usize] |= 1u64 << (o % 64);
        }
        while limbs.last() == Some(&0) {
            limbs.pop();
        }
        limbs
    }

    /// Build a set from intervals and force it to choose a representation.
    fn from_intervals(intervals: &[(u64, u64)]) -> OrdSet {
        let mut ords = Vec::new();
        for &(lo, hi) in intervals {
            ords.extend(lo..=hi);
        }
        ords.sort_unstable();
        ords.dedup();
        let mut s = OrdSet::from_sorted_slice(&ords);
        s.optimize();
        s
    }

    /// The bulk interval fill must agree with the ordinal walk everywhere.
    ///
    /// The cases are chosen for the word boundaries `fill_bits` branches on:
    /// an interval inside one word, one spanning exactly two, one starting and
    /// ending mid-word, whole-word-aligned ends, and several runs in one chunk.
    #[test]
    fn a_run_container_reads_the_same_bits_as_an_ordinal_walk() {
        let cases: Vec<Vec<(u64, u64)>> = vec![
            vec![(0, 0)],
            vec![(0, 63)],
            vec![(0, 64)],
            vec![(1, 62)],
            vec![(63, 64)],
            vec![(5, 200)],
            vec![(64, 127)],
            vec![(63, 128)],
            vec![(0, 4095)],
            vec![(0, 100), (200, 300), (1000, 4000)],
            vec![(7, 7), (9, 9), (11, 2000)],
            // Crosses a chunk boundary, so two containers contribute.
            vec![(65_000, 70_000)],
            vec![(0, 200_000)],
        ];
        for intervals in cases {
            let s = from_intervals(&intervals);
            for bits in [1u64, 63, 64, 65, 127, 128, 999, 4096, 65_536, 200_001] {
                let want = naive_read(&s, bits);
                let got = s.read_int(bits);
                assert_eq!(
                    got.limbs(),
                    want.as_slice(),
                    "intervals {intervals:?} at {bits} bits"
                );
                assert!(got.is_normalized(), "{intervals:?} at {bits} bits");
            }
        }
    }

    /// Sparse and scattered shapes, which land in **array** containers rather
    /// than runs, against the same ordinal-walk oracle.
    ///
    /// The interval cases above almost all coalesce into runs, so without these
    /// the grouped array scatter -- which accumulates a limb in a register and
    /// stores once per limb rather than once per value -- would be untested.
    #[test]
    fn an_array_container_reads_the_same_bits_as_an_ordinal_walk() {
        let shapes: Vec<Vec<u64>> = vec![
            vec![0],
            vec![63],
            vec![64],
            vec![0, 63, 64, 127, 128],
            // Every value in one limb, so the group never flushes early.
            (0..64).collect(),
            // Exactly one value per limb, so it flushes on every value.
            (0..64).map(|i| i * 64).collect(),
            // Two limbs apart, so `cur` jumps rather than advancing.
            (0..64).map(|i| i * 128 + 7).collect(),
            // Half density over several limbs.
            (0..2048).step_by(2).collect(),
            // Sparse and irregular.
            (0..600u64).map(|i| i * i % 4093).collect(),
            // Crosses a chunk boundary.
            (65_000..65_100).collect(),
        ];
        for shape in shapes {
            let mut ords = shape.clone();
            ords.sort_unstable();
            ords.dedup();
            let mut s = OrdSet::from_sorted_slice(&ords);
            s.optimize();
            for bits in [1u64, 63, 64, 65, 127, 128, 1000, 4096, 66_000] {
                let want = naive_read(&s, bits);
                let got = s.read_int(bits);
                assert_eq!(
                    got.limbs(),
                    want.as_slice(),
                    "shape starting {:?} at {bits} bits",
                    &ords[..ords.len().min(4)]
                );
                assert!(got.is_normalized());
            }
        }
    }

    /// At least one of those shapes really is an array container.
    #[test]
    fn the_sparse_case_really_is_an_array_container() {
        let ords: Vec<u64> = (0..2048u64).step_by(2).collect();
        let mut s = OrdSet::from_sorted_slice(&ords);
        s.optimize();
        let kinds: Vec<ContainerKind> = s.chunks().map(|(_, c)| c.kind()).collect();
        assert_eq!(
            kinds,
            vec![ContainerKind::Array],
            "or the array scatter above is not being exercised"
        );
    }

    /// The fast path is actually reached, so the test above is not vacuously
    /// exercising the array arm instead.
    #[test]
    fn the_dense_case_really_is_a_run_container() {
        let s = from_intervals(&[(0, 60_000)]);
        let kinds: Vec<ContainerKind> = s.chunks().map(|(_, c)| c.kind()).collect();
        assert_eq!(
            kinds,
            vec![ContainerKind::Run],
            "a contiguous stretch must coalesce to one run, or this suite proves nothing"
        );
        // And it reads back as 60 001 consecutive set bits.
        let v = s.read_int(60_001);
        assert_eq!(v.bit_len(), 60_001);
        assert_eq!(v.limbs().iter().filter(|w| **w == u64::MAX).count(), 937);
    }

    /// An interval that extends past the requested width is clamped, not wrapped.
    #[test]
    fn a_run_past_the_requested_width_is_truncated() {
        let s = from_intervals(&[(0, 10_000)]);
        for bits in [1u64, 7, 64, 65, 100] {
            let v = s.read_int(bits);
            assert_eq!(
                v.bit_len(),
                bits,
                "read at {bits} must be exactly that wide"
            );
            // Every bit below `bits` is set, so the value is 2^bits - 1.
            let all =
                BigUint::from_limbs_le(vec![u64::MAX; bits.div_ceil(64) as usize]).truncate(bits);
            assert_eq!(v, all, "read at {bits}");
        }
    }
}
