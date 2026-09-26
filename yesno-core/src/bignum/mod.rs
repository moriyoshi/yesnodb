//! Arbitrary-precision unsigned arithmetic: an [`OrdSet`](crate::OrdSet) read as
//! a big integer.
//!
//! An ordinal set is a bit vector over `[0, ORDINAL_MAX]`, and that bit vector
//! **is** an unsigned integer: the ordinal `j` carries the `2^j` term. One set
//! is one number. It is the same reinterpretation
//! [`matrix`](crate::matrix) performs, at a different shape -- there a chunk is
//! a 256x256 matrix, here a set is a number as wide as its highest member.
//!
//! # One set, one integer, and no layout
//!
//! An earlier form carried an `IntLayout` of `width_bits` and `stride`, so that
//! **several** integers shared a set and `k * stride + j` addressed bit `j` of
//! integer `k`. That is gone. Nothing ever packed several: every layout the
//! workspace constructed was dense, the padded constructors' only consumers were
//! tests asserting their own arithmetic, and a descriptor that can be
//! self-inconsistent -- `stride < width_bits` -- had to be checked everywhere it
//! was accepted. Several integers are now several **keys**, which is what a key
//! is for.
//!
//! Three things fall out, and the third is the one that paid for the change:
//!
//! - **Width is an argument, not a type.** It appears only where a *choice* is
//!   being made -- reading fewer bits than are there -- and
//!   [`OrdSet::read_int`](crate::OrdSet::read_int) is total, so there is no
//!   layout to validate and no unaddressable index to report.
//! - **Reading narrower is exactly `x mod 2^width`.** That identity was always
//!   the payoff of least-significant-bit-first ordering; with the base pinned to
//!   zero it is now the *only* thing a width does.
//! - **Nothing straddles a limb.** The hazard this header used to open with --
//!   an integer, or a single limb, crossing the 65 536-bit chunk boundary --
//!   existed because an arbitrary stride put an integer's base at a
//!   non-multiple of 64. With the base always zero and 65 536 bits being exactly
//!   1 024 limbs, every chunk maps onto a whole limb window. The shift-and-carry
//!   seam it required is gone rather than centralized.
//!
//! # Least significant bit first, and why there is no other order
//!
//! The ordinal set and the integer are the *same object*, and the arithmetic
//! reading falls out of the set reading for free: two integers with no bit in
//! common satisfy `a + b == a | b`, so [`OrdSet::or`](crate::OrdSet::or) **is**
//! their sum, and [`OrdSet::xor`](crate::OrdSet::xor) is addition without carry
//! -- that is, addition in `GF(2)[x]`. A most-significant-bit-first option would
//! reverse every read and write and buy none of that back, and under it the
//! value of every stored bit would depend on the width. So there is deliberately
//! no `Order` knob here as there is on [`Layout`](crate::matrix::Layout).
//!
//! # The canonical form has no trailing zero limb
//!
//! [`BigUint`] derives `PartialEq`, which compares limbs, and its ordering
//! compares lengths first. A trailing zero limb is therefore not "don't care" —
//! it makes two equal integers compare unequal and makes `bit_len` wrong. Every
//! operation that writes limbs must trim; [`BigUint::is_normalized`] is the
//! debug-time guard, and it is the direct analogue of
//! [`BitMatrix::tail_is_clear`](crate::matrix::BitMatrix::tail_is_clear).
//!
//! There is no cached bit length. [`BitMatrix`](crate::matrix::BitMatrix)
//! carries `ones` because a population count is `O(n)`; `bit_len` here is `O(1)`
//! from the top limb's `leading_zeros`, so a cache would be state to invalidate
//! in exchange for nothing.
//!
//! # What this module is not
//!
//! **Not a cryptographic library.** Nothing here is constant-time, and no
//! effort is made to avoid data-dependent branches or memory access. `divrem`
//! short-circuits on operand size and the shifts branch on the bit offset. This
//! is an index; do not use these operations on secret material and do not
//! add a "constant-time" variant without also adding the test layer that could
//! catch it regressing, which this crate does not have.
//!
//! **Unsigned only.** No sign, no rationals, no floats. A signed layer would
//! have to decide truncating versus Euclidean division, and nothing here asks
//! the question. A *signed reading* of a stored value is a different thing and
//! belongs to the reader, not to this type: two's complement needs a width to
//! be negative in, and a width is an argument here rather than a property of
//! anything stored.
//!
//! **No `std::ops` implementations.** `Sub`, `Div` and `Rem` cannot be total
//! on an unsigned arbitrary-precision type — underflow and a zero divisor have
//! no value to return — so they would have to panic, next to a
//! [`BigUint::sub`] that returns `None`. Two spellings of one operation, one of
//! which aborts the process, is worse than one honest spelling.

mod addsub;
mod div;
mod modular;
mod mul;
mod query;
mod read;
mod signed;
mod write;

pub use modular::Barrett;
pub use mul::KARATSUBA_MIN;
pub use signed::BigInt;

/// An owned arbitrary-precision unsigned integer, little-endian by `u64` limb.
///
/// `limbs[0]` carries `2^0..2^63`, `limbs[1]` carries `2^64..2^127`, and so on.
/// There is **never** a trailing zero limb — see the module header for why that
/// is an invariant rather than a convention — so zero is the empty limb vector
/// and `limbs.len()` is exactly `bit_len().div_ceil(64)`.
///
/// # Ordering is by magnitude, and is not derived
///
/// `#[derive(PartialOrd)]` on a little-endian limb vector would compare
/// `limbs[0]` first, which orders by the *low* word. `Ord` is therefore
/// hand-written: longer is greater, and equal lengths compare from the top limb
/// down. Normalization is what makes the length comparison sound, which is the
/// second reason it is an invariant.
/// Limbs held without touching the heap.
///
/// Two, which is 128 bits. The width was measured rather than chosen: at four
/// through sixty-four bits a `mul` cost 13.3 ns against 0.55 ns for the native
/// product, **flat across every width** -- the cost did not depend on how wide
/// the value was, because it was two heap allocations ( ~6.5 ns each ) and
/// ~0.4 ns of arithmetic. Two limbs covers every width that measured flat, and
/// `cmp`, which allocates nothing, was already 0.9 ns.
const INLINE_LIMBS: usize = 2;

/// Where a value's limbs live.
///
/// **Not observable.** [`BigUint`]'s equality, ordering and hashing all read
/// [`BigUint::limbs`] and never the variant, so one value compares and hashes
/// the same whichever arm holds it. That is deliberate and it is the whole
/// safety argument for the optimization: a representation that leaked into
/// `PartialEq` would give one number two identities, which is the defect the
/// canonical-form invariant exists to prevent, reintroduced one level down.
#[derive(Clone, Debug)]
enum Repr {
    /// `len` limbs in registers. `len <= INLINE_LIMBS`, and the limbs above it
    /// are zero so a stale value can never be read back.
    Inline { limbs: [u64; INLINE_LIMBS], len: u8 },
    /// Anything wider.
    Heap(Vec<u64>),
}

impl Default for Repr {
    #[inline]
    fn default() -> Repr {
        Repr::Inline {
            limbs: [0; INLINE_LIMBS],
            len: 0,
        }
    }
}

#[derive(Clone, Default)]
pub struct BigUint {
    repr: Repr,
}

impl BigUint {
    /// Zero.
    #[inline]
    pub fn zero() -> BigUint {
        BigUint {
            repr: Repr::default(),
        }
    }

    /// One.
    #[inline]
    pub fn one() -> BigUint {
        BigUint::from_u64(1)
    }

    /// A single-limb value.
    #[inline]
    pub fn from_u64(v: u64) -> BigUint {
        if v == 0 {
            return BigUint::zero();
        }
        BigUint {
            repr: Repr::Inline {
                limbs: [v, 0],
                len: 1,
            },
        }
    }

    /// Adopt a little-endian limb vector, trimming any trailing zero limbs.
    ///
    /// This is the only constructor that takes raw limbs, and it normalizes, so
    /// no caller can create an unnormalized value.
    pub fn from_limbs_le(mut limbs: Vec<u64>) -> BigUint {
        while limbs.last() == Some(&0) {
            limbs.pop();
        }
        // **Moved, not copied, when it stays on the heap.** This constructor
        // takes the vector by value, so a wide result should cost nothing here
        // -- routing it through the copying path made every heap-sized product
        // pay a second allocation, which showed up as a 128-bit multiply
        // regressing from 15.5 ns to 24.3 ns while the narrow widths improved.
        if limbs.len() > INLINE_LIMBS {
            return BigUint {
                repr: Repr::Heap(limbs),
            };
        }
        BigUint::from_slice(&limbs, limbs.len())
    }

    /// A value from a `u128`, without forming a limb vector.
    ///
    /// The constructor the narrow arithmetic arms return through: a product of
    /// two `u64`s is a `u128`, and routing it through `from_limbs_le` would
    /// allocate the vector the arm exists to avoid.
    #[inline]
    pub(crate) fn from_u128(v: u128) -> BigUint {
        let lo = v as u64;
        let hi = (v >> 64) as u64;
        let len = if hi != 0 {
            2
        } else if lo != 0 {
            1
        } else {
            0
        };
        BigUint {
            repr: Repr::Inline {
                limbs: [lo, hi],
                len,
            },
        }
    }

    /// The exact 256-bit product of two `u128`s, as up to four limbs.
    ///
    /// Rust has no `u256`, so this is a two-by-two schoolbook with `u128`
    /// intermediates -- the same arithmetic the general kernel does, without
    /// its generality. Every partial sum is shown to fit: the middle column
    /// adds one carry below `2^64` to two half-products below `2^64`, which is
    /// under `2^66`.
    #[inline]
    pub(crate) fn from_u128_product(a: u128, b: u128) -> BigUint {
        const LOW: u128 = u64::MAX as u128;
        let (a0, a1) = (a & LOW, a >> 64);
        let (b0, b1) = (b & LOW, b >> 64);

        let p00 = a0 * b0;
        let p01 = a0 * b1;
        let p10 = a1 * b0;
        let p11 = a1 * b1;

        let mid = (p00 >> 64) + (p01 & LOW) + (p10 & LOW);
        let hi = (mid >> 64) + (p01 >> 64) + (p10 >> 64) + (p11 & LOW);
        let top = (hi >> 64) + (p11 >> 64);

        let limbs = [p00 as u64, mid as u64, hi as u64, top as u64];
        let len = limbs.iter().rposition(|&w| w != 0).map_or(0, |i| i + 1);
        BigUint::from_slice(&limbs, len)
    }

    /// This value as a `u128`, if it fits. `None` above two limbs.
    #[inline]
    pub(crate) fn to_u128(&self) -> Option<u128> {
        let limbs = self.limbs();
        match limbs.len() {
            0 => Some(0),
            1 => Some(u128::from(limbs[0])),
            2 => Some(u128::from(limbs[0]) | (u128::from(limbs[1]) << 64)),
            _ => None,
        }
    }

    /// Build from a normalized slice, inline when it fits.
    #[inline]
    pub(crate) fn from_slice(src: &[u64], len: usize) -> BigUint {
        if len <= INLINE_LIMBS {
            let mut limbs = [0u64; INLINE_LIMBS];
            limbs[..len].copy_from_slice(&src[..len]);
            return BigUint {
                repr: Repr::Inline {
                    limbs,
                    len: len as u8,
                },
            };
        }
        BigUint {
            repr: Repr::Heap(src[..len].to_vec()),
        }
    }

    /// The limbs, little-endian, with no trailing zero.
    #[inline]
    pub fn limbs(&self) -> &[u64] {
        match &self.repr {
            Repr::Inline { limbs, len } => &limbs[..*len as usize],
            Repr::Heap(v) => v,
        }
    }

    /// The limbs, mutably. The caller re-normalizes.
    #[inline]
    pub(crate) fn limbs_mut_slice(&mut self) -> &mut [u64] {
        match &mut self.repr {
            Repr::Inline { limbs, len } => &mut limbs[..*len as usize],
            Repr::Heap(v) => v,
        }
    }

    /// Append a limb, spilling to the heap if the inline arm is full.
    #[inline]
    pub(crate) fn push_limb(&mut self, v: u64) {
        match &mut self.repr {
            Repr::Inline { limbs, len } if (*len as usize) < INLINE_LIMBS => {
                limbs[*len as usize] = v;
                *len += 1;
            }
            Repr::Inline { limbs, len } => {
                let mut spilled = limbs[..*len as usize].to_vec();
                spilled.push(v);
                self.repr = Repr::Heap(spilled);
            }
            Repr::Heap(heap) => heap.push(v),
        }
    }

    /// Grow to `n` limbs, filling with `value`. Never shrinks.
    #[inline]
    pub(crate) fn resize_limbs(&mut self, n: usize, value: u64) {
        if n <= self.limbs().len() {
            return;
        }
        match &mut self.repr {
            Repr::Inline { limbs, len } if n <= INLINE_LIMBS => {
                for slot in limbs.iter_mut().take(n).skip(*len as usize) {
                    *slot = value;
                }
                *len = n as u8;
            }
            Repr::Inline { limbs, len } => {
                let mut spilled = limbs[..*len as usize].to_vec();
                spilled.resize(n, value);
                self.repr = Repr::Heap(spilled);
            }
            Repr::Heap(heap) => heap.resize(n, value),
        }
    }

    /// Keep the low `n` limbs. The caller re-normalizes.
    #[inline]
    pub(crate) fn truncate_limbs(&mut self, n: usize) {
        match &mut self.repr {
            Repr::Inline { limbs, len } => {
                if n < *len as usize {
                    // Clear above the new length: an inline limb outlives the
                    // length that hid it, and a later `push_limb` would
                    // otherwise resurrect a stale value.
                    for slot in limbs.iter_mut().skip(n) {
                        *slot = 0;
                    }
                    *len = n as u8;
                }
            }
            Repr::Heap(heap) => heap.truncate(n),
        }
    }

    /// Drop the top limb. The caller has checked it is zero.
    #[inline]
    fn pop_limb(&mut self) {
        let len = self.limbs().len();
        if len > 0 {
            self.truncate_limbs(len - 1);
        }
    }

    /// Does this value satisfy the no-trailing-zero-limb invariant?
    ///
    /// The debug-time guard every limb-writing operation asserts, and the
    /// analogue of
    /// [`BitMatrix::tail_is_clear`](crate::matrix::BitMatrix::tail_is_clear).
    #[inline]
    pub fn is_normalized(&self) -> bool {
        self.limbs().last() != Some(&0)
    }

    /// Trim trailing zero limbs, restoring the invariant.
    #[inline]
    pub(crate) fn normalize(&mut self) {
        while self.limbs().last() == Some(&0) {
            self.pop_limb();
        }
        // A heap value that has shrunk into the inline range moves back, so a
        // number has one representation for its width rather than depending on
        // how it was reached. Equality does not observe this -- see `Repr` --
        // but keeping it true is what makes the inline arm reachable at all
        // after a division or a truncation.
        if let Repr::Heap(heap) = &self.repr {
            if heap.len() <= INLINE_LIMBS {
                *self = BigUint::from_slice(heap, heap.len());
            }
        }
    }
}

impl PartialEq for BigUint {
    /// Over the limbs, never the representation -- see the type-level note on
    /// why where a value lives is not observable.
    #[inline]
    fn eq(&self, other: &BigUint) -> bool {
        self.limbs() == other.limbs()
    }
}

impl Eq for BigUint {}

impl std::hash::Hash for BigUint {
    /// Over the limbs, so two equal values hash alike whichever arm holds
    /// them. A `Hash` that read the variant would break the `Eq`/`Hash`
    /// contract the moment a value spilled.
    #[inline]
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.limbs().hash(state);
    }
}

impl Ord for BigUint {
    fn cmp(&self, other: &BigUint) -> std::cmp::Ordering {
        debug_assert!(self.is_normalized() && other.is_normalized());
        // Normalized, so a longer limb vector is strictly larger. Without that
        // invariant this comparison is simply wrong, not merely slower.
        let (a, b) = (self.limbs(), other.limbs());
        a.len()
            .cmp(&b.len())
            .then_with(|| a.iter().rev().cmp(b.iter().rev()))
    }
}

impl PartialOrd for BigUint {
    #[inline]
    fn partial_cmp(&self, other: &BigUint) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl From<u64> for BigUint {
    #[inline]
    fn from(v: u64) -> BigUint {
        BigUint::from_u64(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_trailing_zero_limb_never_survives_construction() {
        let a = BigUint::from_limbs_le(vec![7, 0, 0]);
        assert_eq!(a.limbs(), &[7]);
        assert!(a.is_normalized());
        assert_eq!(a, BigUint::from_u64(7));
    }
    #[test]
    fn zero_is_the_empty_limb_vector() {
        assert_eq!(BigUint::zero().limbs(), &[] as &[u64]);
        assert_eq!(BigUint::from_u64(0), BigUint::zero());
        assert_eq!(BigUint::from_limbs_le(vec![0, 0]), BigUint::zero());
    }
    #[test]
    fn ordering_is_by_magnitude_not_by_the_low_limb() {
        let small = BigUint::from_limbs_le(vec![u64::MAX]);
        let large = BigUint::from_limbs_le(vec![0, 1]);
        assert!(small < large);
        let a = BigUint::from_limbs_le(vec![1, 5]);
        let b = BigUint::from_limbs_le(vec![9, 5]);
        assert!(a < b);
    }
    /// **The representation must not be observable.** A value that spilled to
    /// the heap and one that never left the inline arm are the same number, so
    /// they must compare and hash alike -- otherwise the optimization gives one
    /// value two identities, which is the defect the canonical form exists to
    /// prevent, one level down.
    #[test]
    fn where_the_limbs_live_is_not_observable() {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let hash_of = |v: &BigUint| {
            let mut h = DefaultHasher::new();
            v.hash(&mut h);
            h.finish()
        };

        // Reach 5 two ways: built small, and grown then shrunk back through
        // the heap. `normalize` moves a shrunken value back inline, so this
        // also pins that it is reachable at all after a truncation.
        let inline = BigUint::from_u64(5);
        let mut spilled = BigUint::from_limbs_le(vec![5, 7, 9]);
        spilled.truncate_limbs(1);
        spilled.normalize();

        assert_eq!(inline, spilled);
        assert_eq!(hash_of(&inline), hash_of(&spilled));
        assert_eq!(inline.cmp(&spilled), std::cmp::Ordering::Equal);
        assert_eq!(inline.limbs(), spilled.limbs());
    }

    /// Spilling past the inline width and coming back must preserve the value.
    #[test]
    fn a_value_survives_crossing_the_inline_boundary() {
        let mut v = BigUint::from_u64(u64::MAX);
        // Push past two limbs, so the inline arm spills.
        v.push_limb(u64::MAX);
        v.push_limb(3);
        assert_eq!(v.limbs(), &[u64::MAX, u64::MAX, 3]);
        // And back down.
        v.truncate_limbs(1);
        v.normalize();
        assert_eq!(v, BigUint::from_u64(u64::MAX));
    }

    /// A truncation must not leave a limb above the length for a later push to
    /// resurrect -- the one way an inline arm can hand back a stale value.
    #[test]
    fn truncating_inline_clears_the_limbs_above_the_length() {
        let mut v = BigUint::from_limbs_le(vec![1, 2]);
        v.truncate_limbs(1);
        assert_eq!(v.limbs(), &[1]);
        v.push_limb(0);
        // If the 2 survived, this would be `[1, 2]`.
        assert_eq!(v.limbs(), &[1, 0]);
        v.normalize();
        assert_eq!(v, BigUint::from_u64(1));
    }

    #[test]
    fn is_normalized_notices_a_raw_limb_write() {
        let mut a = BigUint::from_u64(5);
        assert!(a.is_normalized());
        a.push_limb(0);
        assert!(!a.is_normalized());
        a.normalize();
        assert!(a.is_normalized());
        assert_eq!(a, BigUint::from_u64(5));
    }
}
