//! Predicates, magnitude, and conversion to and from the fixed-width types.
//!
//! Nothing here carries or borrows. Everything is a read of the canonical form,
//! which is what makes it the right place for the questions the arithmetic
//! kernels ask about their own operands — `bit_len` decides how many limbs a
//! product needs, and `is_zero` is the divisor test.
//!
//! No decimal formatting. Rendering base 10 needs repeated division, so it
//! belongs with the divider rather than here; [`BigUint`] formats as hex until
//! then, which is exact, cheap, and reads limb-for-limb against the canonical
//! form when a test fails.

use super::BigUint;

impl BigUint {
    /// Is this zero?
    ///
    /// `O(1)`: normalization makes zero the empty limb vector, so there is no
    /// all-zero-limbs case to scan for.
    #[inline]
    pub fn is_zero(&self) -> bool {
        debug_assert!(self.is_normalized());
        self.limbs().is_empty()
    }

    /// Position of the highest set bit, plus one. Zero has bit length zero.
    ///
    /// `O(1)` from the top limb, which is why no cached length is carried — see
    /// the module header.
    ///
    /// Returns `u64`, not `u32`: a value of `2^32` bits is 512 MiB and
    /// entirely representable here, so a `u32` would be a silent ceiling on the
    /// one type in the crate that deliberately has none.
    #[inline]
    pub fn bit_len(&self) -> u64 {
        debug_assert!(self.is_normalized());
        match self.limbs().last() {
            None => 0,
            Some(&top) => {
                let below = (self.limbs().len() as u64 - 1) * 64;
                below + (64 - top.leading_zeros() as u64)
            }
        }
    }

    /// Is bit `i` set? Bits at or above [`Self::bit_len`] read as `false`.
    #[inline]
    pub fn bit(&self, i: u64) -> bool {
        let limb = (i / 64) as usize;
        match self.limbs().get(limb) {
            None => false,
            Some(&w) => (w >> (i % 64)) & 1 == 1,
        }
    }

    /// How many bits are set.
    #[inline]
    pub fn count_ones(&self) -> u64 {
        self.limbs().iter().map(|w| w.count_ones() as u64).sum()
    }

    /// The value as a `u64`, or `None` if it does not fit.
    ///
    /// `None` rather than a truncation, for the same reason
    /// [`IntLayout::ordinal_at`](super::IntLayout::ordinal_at) returns `None` at
    /// the ceiling: a value that does not fit is not a value that fits with some
    /// bits removed.
    #[inline]
    pub fn to_u64(&self) -> Option<u64> {
        match self.limbs() {
            [] => Some(0),
            [w] => Some(*w),
            _ => None,
        }
    }

    /// Read a little-endian byte string.
    ///
    /// Little-endian because the canonical form is, and because the crate's
    /// on-disk containers are — a bitmap container's bytes and a `BigUint`'s
    /// bytes describe the same bit positions in the same order.
    pub fn from_le_bytes(bytes: &[u8]) -> BigUint {
        let mut limbs = Vec::with_capacity(bytes.len().div_ceil(8));
        for c in bytes.chunks(8) {
            let mut w = [0u8; 8];
            w[..c.len()].copy_from_slice(c);
            limbs.push(u64::from_le_bytes(w));
        }
        BigUint::from_limbs_le(limbs)
    }

    /// Write the value as a little-endian byte string, with no leading zero
    /// byte. Zero produces an empty slice.
    pub fn to_le_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.limbs().len() * 8);
        for w in self.limbs() {
            out.extend_from_slice(&w.to_le_bytes());
        }
        while out.last() == Some(&0) {
            out.pop();
        }
        out
    }

    /// Big-endian hexadecimal, with no leading zeros. Zero renders as `"0"`.
    pub fn to_hex_string(&self) -> String {
        match self.limbs().split_last() {
            None => "0".to_string(),
            Some((top, rest)) => {
                let mut s = format!("{top:x}");
                for w in rest.iter().rev() {
                    s.push_str(&format!("{w:016x}"));
                }
                s
            }
        }
    }
}

impl std::fmt::LowerHex for BigUint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_hex_string())
    }
}

impl std::fmt::Debug for BigUint {
    /// Hex, tagged, so a failing assertion prints something a reader can line up
    /// against the limbs rather than a wall of decimal digits.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "BigUint(0x{})", self.to_hex_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bit_len_counts_from_the_top_set_bit() {
        assert_eq!(BigUint::zero().bit_len(), 0);
        assert_eq!(BigUint::one().bit_len(), 1);
        assert_eq!(BigUint::from_u64(u64::MAX).bit_len(), 64);
        assert_eq!(BigUint::from_limbs_le(vec![0, 1]).bit_len(), 65);
        assert_eq!(BigUint::from_limbs_le(vec![0, u64::MAX]).bit_len(), 128);
    }

    #[test]
    fn bit_reads_past_the_top_as_zero_rather_than_panicking() {
        let a = BigUint::from_limbs_le(vec![0b1010, 1]);
        assert!(!a.bit(0));
        assert!(a.bit(1));
        assert!(a.bit(64));
        assert!(!a.bit(65));
        assert!(!a.bit(u64::MAX));
    }

    #[test]
    fn bytes_round_trip_and_drop_the_leading_zeros() {
        for v in [0u64, 1, 255, 256, u64::MAX] {
            let a = BigUint::from_u64(v);
            assert_eq!(BigUint::from_le_bytes(&a.to_le_bytes()), a);
        }
        let wide = BigUint::from_limbs_le(vec![0xdead_beef, 0x00ff]);
        assert_eq!(BigUint::from_le_bytes(&wide.to_le_bytes()), wide);
        // Trailing zero bytes are not part of the value and are not emitted.
        assert_eq!(BigUint::from_u64(1).to_le_bytes(), vec![1u8]);
        assert!(BigUint::zero().to_le_bytes().is_empty());
        // Nor do trailing zero *input* bytes create a trailing zero limb.
        assert!(BigUint::from_le_bytes(&[0, 0, 0]).is_zero());
    }

    #[test]
    fn to_u64_declines_rather_than_truncating() {
        assert_eq!(BigUint::zero().to_u64(), Some(0));
        assert_eq!(BigUint::from_u64(u64::MAX).to_u64(), Some(u64::MAX));
        assert_eq!(BigUint::from_limbs_le(vec![0, 1]).to_u64(), None);
    }

    #[test]
    fn hex_has_no_leading_zeros_but_keeps_the_interior_ones() {
        assert_eq!(BigUint::zero().to_hex_string(), "0");
        assert_eq!(BigUint::from_u64(0xabc).to_hex_string(), "abc");
        // The low limb keeps its full 16 digits; only the top limb is trimmed.
        assert_eq!(
            BigUint::from_limbs_le(vec![1, 1]).to_hex_string(),
            "10000000000000001"
        );
    }
}
