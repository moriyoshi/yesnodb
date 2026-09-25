//! `OrdSet` -> `BigUint`: the set read as a number.

use super::BigUint;
use crate::container::Container;
use crate::{OrdSet, CHUNK_CARD};

/// Limbs in one chunk. 65 536 bits over a 64-bit limb, exactly.
const LIMBS_PER_CHUNK: usize = CHUNK_CARD as usize / 64;

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
        let mut limbs = vec![0u64; words];

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
                // Arrays and runs have no word view, so they set what they hold.
                // `iter` yields an offset within the chunk, so the addition
                // cannot leave it.
                _ => {
                    for offset in container.iter() {
                        let ordinal = base + u64::from(offset);
                        if ordinal >= bits {
                            break;
                        }
                        limbs[(ordinal / 64) as usize] |= 1u64 << (ordinal % 64);
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
        BigUint::from_limbs_le(limbs)
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
