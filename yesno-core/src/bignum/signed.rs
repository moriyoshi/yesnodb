//! `BigInt`: a sign beside a [`BigUint`] magnitude.
//!
//! # Why a separate type and never a flag
//!
//! A sign changes what an operand *means*, and this module's own rule for that
//! is recorded against Montgomery form: a reduction that sometimes receives
//! entered operands and sometimes ordinary ones "returns a well-formed wrong
//! answer" because it has no error path. A signed value carried as a `bool`
//! beside a [`BigUint`], or as a mode on the unsigned operations, is the same
//! hazard -- so [`BigUint`] is unchanged underneath and every unsigned identity
//! keeps holding.
//!
//! # Zero is never negative
//!
//! One value, one representation, for the reason the canonical limb form has no
//! trailing zero limb: `PartialEq` compares fields, so a negative zero would
//! make two equal numbers compare unequal. Every constructor and every operation
//! normalizes it away, and [`BigInt::is_canonical`] is the debug-time guard.
//!
//! # Division truncates toward zero
//!
//! The choice ARCHITECTURE recorded as open ( "a signed layer would have to
//! decide truncating versus Euclidean division" ), settled here and settled for
//! a reason rather than by convention.
//!
//! **Truncating division agrees with the unsigned primitive on the operands
//! they share.** For non-negative `a` and `b`, `BigInt::divrem` is
//! [`BigUint::divrem`] exactly -- same quotient, same remainder -- so the signed
//! operation is a *conservative extension* of the shipped one rather than a
//! second definition of it. That is what lets the existing oracle carry the
//! signed case: any disagreement on non-negative operands is a bug in the
//! wrapper, not a difference of convention.
//!
//! The consequence, which a caller must know: the remainder carries the sign of
//! the **dividend**, so `-7 / 2` is `-3` remainder `-1`. A caller wanting a
//! non-negative remainder ( Euclidean ) or one signed like the divisor
//! ( floored, which is Python's `divmod` and therefore the `e2e` oracle's )
//! derives it by adjusting when the remainder is non-zero and the signs differ.
//! [`BigInt::div_euclid_rem`] is that adjustment, spelled once here rather than
//! at every call site.

use super::BigUint;
use std::cmp::Ordering;

/// An arbitrary-precision signed integer.
#[derive(Clone, Default, Debug, PartialEq, Eq, Hash)]
pub struct BigInt {
    /// Never true when `magnitude` is zero.
    negative: bool,
    magnitude: BigUint,
}

impl BigInt {
    /// Zero.
    pub fn zero() -> BigInt {
        BigInt {
            negative: false,
            magnitude: BigUint::zero(),
        }
    }

    /// One.
    pub fn one() -> BigInt {
        BigInt::from_magnitude(false, BigUint::one())
    }

    /// A sign and a magnitude, normalized: a zero magnitude is never negative.
    pub fn from_magnitude(negative: bool, magnitude: BigUint) -> BigInt {
        BigInt {
            negative: negative && !magnitude.is_zero(),
            magnitude,
        }
    }

    /// A non-negative value.
    pub fn from_uint(magnitude: BigUint) -> BigInt {
        BigInt::from_magnitude(false, magnitude)
    }

    /// From a signed machine integer.
    pub fn from_i64(v: i64) -> BigInt {
        BigInt::from_magnitude(v < 0, BigUint::from_u64(v.unsigned_abs()))
    }

    /// Is this value negative? A zero never is.
    pub fn is_negative(&self) -> bool {
        self.negative
    }

    /// Is this value zero?
    pub fn is_zero(&self) -> bool {
        self.magnitude.is_zero()
    }

    /// The magnitude, without its sign.
    pub fn magnitude(&self) -> &BigUint {
        &self.magnitude
    }

    /// The magnitude, consuming the value.
    pub fn into_magnitude(self) -> BigUint {
        self.magnitude
    }

    /// This value as an `i128`, for tests and small-field callers.
    ///
    /// `None` if it does not fit, which is a fact about the value rather than
    /// an error -- the same shape as [`BigUint::to_u64`].
    pub fn to_i128(&self) -> Option<i128> {
        // `to_u64`, here, refused every value between 2^64 and i128::MAX --
        // values that fit the return type perfectly well. `None` is supposed to
        // mean "not representable", so answering it for a representable value
        // made the function narrower than its name and its doc.
        let m = self.magnitude.to_u128()?;
        if self.negative {
            // i128::MIN has no positive counterpart, so it is matched on the
            // magnitude rather than reached by negating.
            if m == 1u128 << 127 {
                return Some(i128::MIN);
            }
            let m = i128::try_from(m).ok()?;
            Some(-m)
        } else {
            i128::try_from(m).ok()
        }
    }

    /// The debug-time guard on the one-value-one-representation invariant.
    ///
    /// The direct analogue of [`BigUint::is_normalized`], and it exists for the
    /// same reason: without it every assertion about canonical form in this
    /// module would be vacuous.
    pub fn is_canonical(&self) -> bool {
        self.magnitude.is_normalized() && !(self.negative && self.magnitude.is_zero())
    }

    /// The additive inverse.
    pub fn neg(&self) -> BigInt {
        BigInt::from_magnitude(!self.negative, self.magnitude.clone())
    }

    /// The absolute value.
    pub fn abs(&self) -> BigInt {
        BigInt::from_uint(self.magnitude.clone())
    }

    /// Sum. **Total**, unlike [`BigUint::add`]'s partial counterpart `sub`.
    pub fn add(&self, rhs: &BigInt) -> BigInt {
        self.add_signed(&rhs.magnitude, rhs.negative)
    }

    /// Difference. **Total**: this is the operation a signed type exists for.
    pub fn sub(&self, rhs: &BigInt) -> BigInt {
        // `self.add( &rhs.neg() )` is the same function, and it was what this
        // did. The trouble is that `neg` clones the magnitude to flip one
        // bool, so every subtraction allocated a whole second operand to
        // throw away -- 6.7 ns of the 11.5 ns a narrow subtraction cost.
        // `add_signed` takes the sign as an argument instead, so the negation
        // never has to exist as a value.
        self.add_signed(&rhs.magnitude, !rhs.negative)
    }

    /// `self += rhs`, reusing this value's own buffer where the signs allow it.
    ///
    /// # Why this exists
    ///
    /// A reduction accumulates into one value, and `acc = acc.add( &x )` allocates a
    /// whole new magnitude of the accumulator's growing width at every step. Measured
    /// over 1024 operands on a quiet host: **2.66x** at 448 bits ( 18.2 us against
    /// 6.8 ), 1.34x at 4096 and 1.13x at 65 536 -- the ratio decays because the
    /// allocation is a fixed cost per step while the copy grows with the accumulator.
    /// So this pays most where values are narrow and many, which is the shape a fold
    /// over a view's constituents has.
    ///
    /// **Two of the three sign cases are in place and the third is not**, deliberately.
    /// Like signs add in place; opposite signs with this value the larger subtract in
    /// place. Opposite signs with `rhs` the larger needs `rhs.magnitude - self.magnitude`,
    /// which cannot be formed in this buffer without a temporary, so that case defers to
    /// [`Self::add`] and allocates exactly as it did before. A sum of same-signed terms --
    /// what a fold of counts or of magnitudes is -- never reaches it.
    pub fn add_assign(&mut self, rhs: &BigInt) {
        let negative = rhs.negative && !rhs.magnitude.is_zero();
        if self.negative == negative {
            self.magnitude.add_assign(&rhs.magnitude);
            return;
        }
        match self.magnitude.cmp(&rhs.magnitude) {
            Ordering::Equal => *self = BigInt::zero(),
            // Ordered to the larger, so the in-place subtraction cannot decline.
            Ordering::Greater => {
                let ok = self.magnitude.sub_assign(&rhs.magnitude);
                debug_assert!(ok, "ordered to the larger magnitude");
                // A magnitude that reached zero must not keep a sign: `is_canonical`
                // forbids a negative zero, and `sub_assign` normalizes the limbs
                // without knowing there is a sign above them.
                if self.magnitude.is_zero() {
                    self.negative = false;
                }
            }
            // `rhs - self` has no in-place form in this buffer.
            Ordering::Less => *self = self.add(rhs),
        }
    }

    /// `self + ( magnitude, negative )`, the shared body of `add` and `sub`.
    ///
    /// Taking the operand's sign as an argument rather than as part of a value
    /// is what lets `sub` negate without materializing the negation.
    fn add_signed(&self, magnitude: &BigUint, negative: bool) -> BigInt {
        let negative = negative && !magnitude.is_zero();
        if self.negative == negative {
            return BigInt::from_magnitude(self.negative, self.magnitude.add(magnitude));
        }
        // Opposite signs: the larger magnitude decides the sign, and the
        // subtraction cannot underflow because it is ordered to the larger.
        match self.magnitude.cmp(magnitude) {
            Ordering::Equal => BigInt::zero(),
            Ordering::Greater => BigInt::from_magnitude(
                self.negative,
                self.magnitude
                    .sub(magnitude)
                    .expect("ordered to the larger magnitude"),
            ),
            Ordering::Less => BigInt::from_magnitude(
                negative,
                magnitude
                    .sub(&self.magnitude)
                    .expect("ordered to the larger magnitude"),
            ),
        }
    }

    /// Product.
    pub fn mul(&self, rhs: &BigInt) -> BigInt {
        BigInt::from_magnitude(
            self.negative != rhs.negative,
            self.magnitude.mul(&rhs.magnitude),
        )
    }

    /// Quotient and remainder, truncating toward zero.
    ///
    /// `None` for a zero divisor, exactly as [`BigUint::divrem`] is: the answer
    /// is not in the domain, which is a fact about the operands rather than an
    /// error.
    ///
    /// The remainder carries the sign of the **dividend**, so
    /// `a == q * b + r` and `|r| < |b|` always hold. See the module header for
    /// why this convention and not another.
    pub fn divrem(&self, d: &BigInt) -> Option<(BigInt, BigInt)> {
        let (q, r) = self.magnitude.divrem(&d.magnitude)?;
        Some((
            BigInt::from_magnitude(self.negative != d.negative, q),
            BigInt::from_magnitude(self.negative, r),
        ))
    }

    /// Quotient and remainder with a **non-negative** remainder.
    ///
    /// The Euclidean convention: `0 <= r < |b|`. Derived from [`BigInt::divrem`]
    /// by one adjustment, spelled here so a caller does not repeat it.
    pub fn div_euclid_rem(&self, d: &BigInt) -> Option<(BigInt, BigInt)> {
        let (q, r) = self.divrem(d)?;
        if !r.is_negative() {
            return Some((q, r));
        }
        // Move the remainder up by |d| and the quotient one step the other way.
        let r = r.add(&d.abs());
        let q = if d.is_negative() {
            q.add(&BigInt::one())
        } else {
            q.sub(&BigInt::one())
        };
        Some((q, r))
    }

    /// Wrap into a `bits`-wide two's-complement field: the residue of `self`
    /// modulo `2^bits`, read back with the top bit as a sign.
    ///
    /// **The same target field as [`BigInt::saturate`], and that is the point.**
    /// The two are the wrap and the clamp for one field, so a caller choosing
    /// between them is choosing an overflow rule rather than also, silently, a
    /// different range. `BigUint::truncate` keeps the low bits of a *magnitude*
    /// and is the unsigned operation; using it on a signed value would let
    /// `truncate( 255, 8 )` be `255`, which no 8-bit signed field holds.
    ///
    /// `bits == 0` is zero, for the reason it is in `saturate`.
    pub fn truncate(&self, bits: u64) -> BigInt {
        if bits == 0 {
            return BigInt::zero();
        }
        let modulus = BigUint::one().shl(bits);
        let m = self.magnitude.truncate(bits);
        // The non-negative residue: for a negative value, count down from the
        // modulus. A zero magnitude is already its own residue.
        let residue = if self.negative && !m.is_zero() {
            modulus.sub(&m).expect("m < 2^bits")
        } else {
            m
        };
        if residue.bit(bits - 1) {
            BigInt::from_magnitude(true, modulus.sub(&residue).expect("residue < 2^bits"))
        } else {
            BigInt::from_uint(residue)
        }
    }

    /// Clamp into what a `bits`-wide two's-complement field can hold:
    /// `[ -2^(bits-1), 2^(bits-1) - 1 ]`.
    ///
    /// **Asymmetric, because two's complement is.** There is one more negative
    /// value than positive, and clamping to a symmetric range instead would be
    /// a different function that no storage agrees with.
    ///
    /// `bits == 0` is zero: a field of no bits holds no value, and zero is the
    /// only number whose clamp into it is not a lie.
    ///
    /// Composes with the signed reader the way [`BigUint::saturate`] does with
    /// the unsigned one: a value clamped to `bits` is recovered exactly by a
    /// two's-complement read at `bits`.
    pub fn saturate(&self, bits: u64) -> BigInt {
        if bits == 0 {
            return BigInt::zero();
        }
        let ceiling = BigUint::one().shl(bits - 1);
        if self.negative {
            // The negative side reaches `2^(bits-1)` itself.
            let magnitude = if self.magnitude > ceiling {
                ceiling
            } else {
                self.magnitude.clone()
            };
            return BigInt::from_magnitude(true, magnitude);
        }
        let max = ceiling.sub(&BigUint::one()).expect("2^(bits-1) >= 1");
        BigInt::from_uint(if self.magnitude > max {
            max
        } else {
            self.magnitude.clone()
        })
    }
}

impl Ord for BigInt {
    /// Negatives below non-negatives, and magnitudes reversed within the
    /// negatives -- the place a sign-and-magnitude ordering is usually wrong.
    fn cmp(&self, other: &BigInt) -> Ordering {
        match (self.negative, other.negative) {
            (false, true) => Ordering::Greater,
            (true, false) => Ordering::Less,
            (false, false) => self.magnitude.cmp(&other.magnitude),
            (true, true) => other.magnitude.cmp(&self.magnitude),
        }
    }
}

impl PartialOrd for BigInt {
    fn partial_cmp(&self, other: &BigInt) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn i(v: i64) -> BigInt {
        BigInt::from_i64(v)
    }

    /// In-place addition must agree with the allocating one on **every sign case**,
    /// and must leave the result canonical.
    ///
    /// The three arms are not symmetric -- two write into this buffer and the third
    /// defers -- so a test that only summed positives would exercise one of them and
    /// pass against an `add_assign` that is wrong wherever a fold does not go. The
    /// crossing cases are the point: a negative accumulator overtaken by a positive
    /// operand, and the exact cancellation that must yield a canonical zero rather than
    /// a negative one.
    #[test]
    fn in_place_addition_agrees_with_the_allocating_one_on_every_sign_case() {
        let interesting = [
            0i64,
            1,
            -1,
            5,
            -5,
            7,
            -7,
            1 << 20,
            -(1 << 20),
            i32::MAX as i64,
            -(i32::MAX as i64),
            i64::MAX / 2,
            -(i64::MAX / 2),
        ];
        for &a in &interesting {
            for &b in &interesting {
                let mut acc = i(a);
                acc.add_assign(&i(b));
                assert_eq!(
                    acc,
                    i(a).add(&i(b)),
                    "in-place {a} + {b} disagreed with the allocating sum"
                );
                assert!(
                    acc.is_canonical(),
                    "in-place {a} + {b} left a non-canonical value"
                );
                assert_eq!(acc.to_i128(), Some(a as i128 + b as i128));
            }
        }
    }

    /// Accumulating a sequence in place must equal folding it with `add`.
    ///
    /// Separate from the pairwise test above because the accumulator *grows*, and the
    /// in-place arms write into a buffer whose capacity the previous step chose. A
    /// pairwise check never reuses a buffer and so cannot see that.
    #[test]
    fn an_in_place_accumulation_equals_the_allocating_fold() {
        for sign in [1i64, -1] {
            for step in [1i64, 7, 1 << 30] {
                let values: Vec<BigInt> = (1..=64).map(|k| i(sign * step * k)).collect();
                let mut acc = BigInt::zero();
                for v in &values {
                    acc.add_assign(v);
                }
                let folded = values.iter().fold(BigInt::zero(), |a, v| a.add(v));
                assert_eq!(acc, folded, "sign {sign} step {step}");
                assert!(acc.is_canonical());
            }
        }
        // And a sequence that crosses zero repeatedly, which exercises all three arms
        // against one growing buffer.
        let values: Vec<BigInt> = (0..64)
            .map(|k| {
                i(if k % 3 == 0 {
                    -(k * 1_000_003)
                } else {
                    k * 7919
                })
            })
            .collect();
        let mut acc = BigInt::zero();
        for v in &values {
            acc.add_assign(v);
        }
        assert_eq!(acc, values.iter().fold(BigInt::zero(), |a, v| a.add(v)));
        assert!(acc.is_canonical());
    }

    /// Without this the guard could return `true` unconditionally and every
    /// canonical-form assertion here would be vacuous. The analogue of
    /// `is_normalized_notices_a_raw_limb_write`.
    #[test]
    fn is_canonical_notices_a_negative_zero() {
        let bad = BigInt {
            negative: true,
            magnitude: BigUint::zero(),
        };
        assert!(!bad.is_canonical());
        assert!(BigInt::zero().is_canonical());
        assert!(i(-5).is_canonical());
    }

    /// A negative zero must be unconstructible through the public surface, or
    /// the invariant is a comment rather than a property.
    #[test]
    fn no_public_path_reaches_a_negative_zero() {
        for v in [
            BigInt::from_magnitude(true, BigUint::zero()),
            i(0).neg(),
            i(5).sub(&i(5)),
            i(-5).add(&i(5)),
            i(0).mul(&i(-7)),
            i(-7).mul(&i(0)),
            i(0).abs(),
            i(3).divrem(&i(-5)).unwrap().0,
        ] {
            assert!(v.is_canonical(), "{v:?}");
            assert!(!v.is_negative());
            assert_eq!(v, BigInt::zero());
        }
    }

    #[test]
    fn addition_and_subtraction_are_total_and_mutually_inverse() {
        for a in [-9i64, -1, 0, 1, 7, 1 << 40] {
            for b in [-8i64, -1, 0, 1, 6, 1 << 41] {
                assert_eq!(i(a).add(&i(b)), i(a + b), "{a} + {b}");
                assert_eq!(i(a).sub(&i(b)), i(a - b), "{a} - {b}");
                // The property a signed type exists for: subtraction never
                // leaves the domain, where `BigUint::sub` returns `None`.
                assert_eq!(i(a).sub(&i(b)).add(&i(b)), i(a), "round trip {a},{b}");
            }
        }
    }

    #[test]
    fn multiplication_follows_the_sign_rule() {
        for a in [-9i64, -1, 0, 3, 1 << 31] {
            for b in [-7i64, 0, 1, 5, 1 << 30] {
                assert_eq!(i(a).mul(&i(b)), i(a * b), "{a} * {b}");
            }
        }
    }

    /// The convention, pinned by example because a reader cannot infer it.
    #[test]
    fn division_truncates_toward_zero_and_the_remainder_follows_the_dividend() {
        assert_eq!(i(7).divrem(&i(2)).unwrap(), (i(3), i(1)));
        assert_eq!(i(-7).divrem(&i(2)).unwrap(), (i(-3), i(-1)));
        assert_eq!(i(7).divrem(&i(-2)).unwrap(), (i(-3), i(1)));
        assert_eq!(i(-7).divrem(&i(-2)).unwrap(), (i(3), i(-1)));
        assert_eq!(i(1).divrem(&i(0)), None);
    }

    /// The identity that makes the convention checkable rather than asserted.
    #[test]
    fn the_division_identity_holds_at_every_sign() {
        for a in [-100i64, -7, -1, 0, 1, 7, 100] {
            for b in [-9i64, -2, -1, 1, 2, 9] {
                let (q, r) = i(a).divrem(&i(b)).unwrap();
                assert_eq!(q.mul(&i(b)).add(&r), i(a), "{a} / {b}");
                assert!(r.abs() < i(b).abs(), "{a} / {b}: |r| < |b|");
                // Truncating: Rust's own operators are the reference.
                assert_eq!(q, i(a / b), "{a} / {b} quotient");
                assert_eq!(r, i(a % b), "{a} / {b} remainder");
            }
        }
    }

    /// A conservative extension: on shared operands it *is* the unsigned
    /// primitive, which is what lets the unsigned oracle carry this.
    #[test]
    fn on_non_negative_operands_it_agrees_with_the_unsigned_primitive() {
        for a in [0u64, 1, 7, 100, u64::MAX] {
            for b in [1u64, 2, 9, u64::MAX] {
                let (uq, ur) = BigUint::from_u64(a).divrem(&BigUint::from_u64(b)).unwrap();
                let (sq, sr) = i(0)
                    .add(&BigInt::from_uint(BigUint::from_u64(a)))
                    .divrem(&BigInt::from_uint(BigUint::from_u64(b)))
                    .unwrap();
                assert_eq!(sq.magnitude(), &uq);
                assert_eq!(sr.magnitude(), &ur);
                assert!(!sq.is_negative() && !sr.is_negative());
            }
        }
    }

    #[test]
    fn the_euclidean_remainder_is_never_negative() {
        for a in [-100i64, -7, -1, 0, 1, 7, 100] {
            for b in [-9i64, -2, -1, 1, 2, 9] {
                let (q, r) = i(a).div_euclid_rem(&i(b)).unwrap();
                assert!(!r.is_negative(), "{a} / {b}: r = {r:?}");
                assert!(r.abs() < i(b).abs(), "{a} / {b}: r < |b|");
                assert_eq!(q.mul(&i(b)).add(&r), i(a), "{a} / {b}");
                // Rust's own Euclidean operators are the reference.
                assert_eq!(q, i(a.div_euclid(b)), "{a} / {b} quotient");
                assert_eq!(r, i(a.rem_euclid(b)), "{a} / {b} remainder");
            }
        }
    }

    /// Sign-and-magnitude ordering is wrong in exactly one place -- within the
    /// negatives, where the magnitudes reverse -- so that is what this pins.
    #[test]
    fn ordering_reverses_within_the_negatives() {
        let mut v = [i(3), i(-1), i(0), i(-100), i(100), i(-3), i(1)];
        v.sort();
        assert_eq!(v, [i(-100), i(-3), i(-1), i(0), i(1), i(3), i(100)]);
        assert!(i(-100) < i(-3));
        assert!(i(-1) < i(0));
        assert_eq!(i(0).cmp(&i(0)), Ordering::Equal);
    }
}

#[cfg(test)]
mod saturate_tests {
    use super::*;
    use crate::OrdSet;

    fn i(v: i64) -> BigInt {
        BigInt::from_i64(v)
    }

    /// Two's complement is asymmetric, so the clamp is too.
    #[test]
    fn the_signed_range_reaches_one_further_below_zero() {
        for bits in [1u64, 2, 8, 16, 64] {
            let max = (1i128 << (bits - 1)) - 1;
            let min = -(1i128 << (bits - 1));
            assert_eq!(i(10_000).saturate(bits).to_i128(), Some(max.min(10_000)));
            assert_eq!(i(-10_000).saturate(bits).to_i128(), Some(min.max(-10_000)));
        }
        assert_eq!(i(127).saturate(8), i(127));
        assert_eq!(i(128).saturate(8), i(127));
        assert_eq!(i(-128).saturate(8), i(-128));
        assert_eq!(i(-129).saturate(8), i(-128));
        assert_eq!(i(5).saturate(0), BigInt::zero());
    }

    /// The wrap and the clamp target **one** field, so they can be compared.
    /// Machine `i8` is the reference.
    #[test]
    fn truncate_wraps_where_saturate_clamps_into_the_same_field() {
        for v in [-300i64, -129, -128, -1, 0, 1, 127, 128, 255, 300] {
            assert_eq!(
                i(v).truncate(8).to_i128(),
                Some((v as i8 as i64) as i128),
                "truncate {v}"
            );
            assert_eq!(
                i(v).saturate(8).to_i128(),
                Some(v.clamp(-128, 127) as i128),
                "saturate {v}"
            );
            // Both land inside the field; neither can leave it.
            let (t, s) = (i(v).truncate(8), i(v).saturate(8));
            assert!(t >= i(-128) && t <= i(127), "truncate {v} left the field");
            assert!(s >= i(-128) && s <= i(127), "saturate {v} left the field");
        }
        // They coincide exactly where nothing overflowed.
        for v in [-128i64, -1, 0, 1, 127] {
            assert_eq!(i(v).truncate(8), i(v).saturate(8), "v={v}");
        }
        assert_ne!(i(128).truncate(8), i(128).saturate(8));
    }

    /// Saturation nests, so applying it twice is applying it at the narrower
    /// width. Checked rather than assumed -- the module's own history records
    /// that composability arguments about saturation were asserted and wrong.
    #[test]
    fn saturation_nests_at_both_signs() {
        for v in [-10_000i64, -129, -1, 0, 1, 127, 10_000] {
            for w1 in [1u64, 8, 16, 64] {
                for w2 in [1u64, 8, 16, 64] {
                    assert_eq!(
                        i(v).saturate(w1).saturate(w2),
                        i(v).saturate(w1.min(w2)),
                        "v={v} w1={w1} w2={w2}"
                    );
                }
            }
        }
    }

    /// **The property that answers the objection saturation used to be refused
    /// on.** It was rejected because "the reader cannot saturate" -- true while
    /// a write had to agree with a read at a declared width. Storage has no
    /// width now, and what remains is that the two compose: a value clamped to
    /// `w` survives a round trip through a set read back at `w`.
    #[test]
    fn a_saturated_value_survives_a_round_trip_at_its_own_width() {
        for bits in [1u64, 2, 8, 63, 64, 65, 200] {
            for v in [-10_000i64, -129, -1, 0, 1, 127, 10_000, i64::MAX] {
                let clamped = i(v).saturate(bits);

                // Unsigned reader, for a non-negative clamp.
                if !clamped.is_negative() {
                    let set = OrdSet::from_int(clamped.magnitude()).unwrap();
                    assert_eq!(&set.read_int(bits), clamped.magnitude(), "v={v} w={bits}");
                }

                // Signed reader: store the two's-complement pattern, read it
                // back at the same width, recover the value exactly.
                let pattern = if clamped.is_negative() {
                    BigUint::one()
                        .shl(bits)
                        .sub(clamped.magnitude())
                        .expect("magnitude is within the field")
                } else {
                    clamped.magnitude().clone()
                };
                let set = OrdSet::from_int(&pattern).unwrap();
                let raw = set.read_int(bits);
                let got = if bits > 0 && raw.bit(bits - 1) {
                    BigInt::from_magnitude(
                        true,
                        BigUint::one().shl(bits).sub(&raw).expect("below 2^bits"),
                    )
                } else {
                    BigInt::from_uint(raw)
                };
                assert_eq!(got, clamped, "v={v} w={bits}");
            }
        }
    }

    /// There is no `add_sat` because there is nothing in one: the arithmetic is
    /// exact, so the composition *is* the saturating operation rather than an
    /// approximation of it.
    #[test]
    fn composing_exact_arithmetic_with_a_clamp_is_the_saturating_operation() {
        for a in [-200i64, -1, 0, 1, 100, 127] {
            for b in [-200i64, -1, 0, 1, 100, 127] {
                assert_eq!(
                    i(a).add(&i(b)).saturate(8).to_i128(),
                    Some((a + b).clamp(-128, 127) as i128),
                    "{a} + {b}"
                );
                assert_eq!(
                    i(a).mul(&i(b)).saturate(8).to_i128(),
                    Some((a * b).clamp(-128, 127) as i128),
                    "{a} * {b}"
                );
            }
        }
    }
}

#[cfg(test)]
mod narrow_signed_tests {
    use super::*;

    /// `to_i128` must answer for every value the type can hold.
    ///
    /// It asked `to_u64`, so it returned `None` for everything between 2^64
    /// and `i128::MAX` -- values that fit the return type exactly. `None` is
    /// supposed to mean "not representable"; answering it for a representable
    /// value made the function narrower than its name.
    #[test]
    fn to_i128_covers_the_whole_signed_range() {
        assert_eq!(BigInt::from_i64(0).to_i128(), Some(0));
        assert_eq!(BigInt::from_i64(-1).to_i128(), Some(-1));

        // Just above the old `u64` ceiling, both signs.
        let big = BigUint::from_limbs_le(vec![0, 1]); // 2^64
        assert_eq!(BigInt::from_uint(big.clone()).to_i128(), Some(1i128 << 64));
        assert_eq!(
            BigInt::from_magnitude(true, big).to_i128(),
            Some(-(1i128 << 64))
        );

        // The extremes. i128::MIN has no positive counterpart, so it is the
        // one value that cannot be reached by negating a representable
        // magnitude, and it is matched on the magnitude instead.
        let max = BigUint::from_limbs_le(vec![u64::MAX, u64::MAX >> 1]);
        assert_eq!(BigInt::from_uint(max.clone()).to_i128(), Some(i128::MAX));
        let min_mag = BigUint::from_limbs_le(vec![0, 1 << 63]); // 2^127
        assert_eq!(
            BigInt::from_magnitude(true, min_mag.clone()).to_i128(),
            Some(i128::MIN)
        );
        // 2^127 is one past i128::MAX as a positive value.
        assert_eq!(BigInt::from_uint(min_mag).to_i128(), None);

        // And genuinely out of range stays None.
        let huge = BigUint::from_limbs_le(vec![0, 0, 1]);
        assert_eq!(BigInt::from_uint(huge).to_i128(), None);
    }

    /// `sub` no longer builds a negated copy of its operand, and still agrees
    /// with the definition it was written as.
    #[test]
    fn sub_agrees_with_adding_the_negation() {
        let cases = [
            (5i64, 3i64),
            (3, 5),
            (-5, 3),
            (5, -3),
            (-5, -3),
            (0, 7),
            (7, 0),
            (0, 0),
            (i64::MIN, 1),
        ];
        for (a, b) in cases {
            let (x, y) = (BigInt::from_i64(a), BigInt::from_i64(b));
            assert_eq!(x.sub(&y), x.add(&y.neg()), "{a} - {b}");
            assert_eq!(x.sub(&y).to_i128(), Some(i128::from(a) - i128::from(b)));
        }
    }
}
