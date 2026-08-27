//! Modular arithmetic: Barrett reduction, and modular exponentiation on top of
//! it.
//!
//! # Why Barrett and not Montgomery
//!
//! **Barrett takes any modulus; Montgomery needs an odd one.** The modulus here
//! is a caller's arbitrary integer — very likely one read out of an `OrdSet` —
//! and a general `pow_mod` cannot assume its user's parity. Barrett also spends
//! its work in two multiplications, so it inherits the [`mul`](BigUint::mul)
//! ladder for free as soon as that ladder exists, where Montgomery would need
//! its own reduction kernel.
//!
//! Montgomery remains a later specialized arm under QG §4, gated on an odd
//! modulus and admitted only with a benchmark. If it lands it must be a
//! **separate type**, not a flag on this one: Montgomery changes what a
//! [`BigUint`] operand *means*, and a `mul_mod` that sometimes expects entered
//! operands and sometimes ordinary ones produces a well-formed wrong answer. That
//! is the packed-`L`-and-`U` trap from [`matrix::lu`](crate::matrix) with no
//! escape hatch. Two types make the representation a compile-time fact, and
//! even-modulus traffic must route through Barrett forever, so the generic path
//! cannot rot.
//!
//! # The precomputation is the point, so it is a value the caller holds
//!
//! `mu = floor( b^2k / m )` costs a division. Recomputing it per operation would
//! make every `mul_mod` more expensive than the `divrem` it replaces, so
//! [`Barrett`] is a value constructed once per modulus — the same argument
//! [`BitLu`](crate::matrix::BitLu) makes for factorising once and answering many
//! right-hand sides.
//!
//! # Nothing here is constant-time
//!
//! [`Barrett::reduce`] branches on how many conditional subtractions it needs and
//! on whether its operand is small enough for the Barrett path at all;
//! [`Barrett::pow_mod`] branches on every bit of the exponent and its running
//! time is proportional to the exponent's **bit length**, which is therefore
//! leaked. Do not use this for key material. See the `bignum` module header:
//! this is an index, and a partially hardened module is worse than an honestly
//! variable-time one.

use super::BigUint;

// These counters exist for the same reason div.rs's do: a test that claims to
// cover a bound or a path has to assert it was reached, not hope. `NEGATIVES` in
// particular guards a branch that 200 000 random reductions never took.
#[cfg(test)]
thread_local! {
    static SUBTRACTIONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static FALLBACKS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static NEGATIVES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Barrett's estimate is short by at most two multiples of the modulus ( HAC
/// 14.42 ), so the correction is a bounded loop rather than a division in
/// disguise. See [`Barrett::reduce`] for why the bound is enforced rather than
/// merely asserted after the fact.
const MAX_CORRECTIONS: u32 = 2;

/// `x >> (64 * n)`, limb-granular.
fn shr_limbs(x: &BigUint, n: usize) -> BigUint {
    let l = x.limbs();
    if n >= l.len() {
        BigUint::zero()
    } else {
        BigUint::from_limbs_le(l[n..].to_vec())
    }
}

/// `x mod b^n`, limb-granular.
fn trunc_limbs(x: &BigUint, n: usize) -> BigUint {
    let l = x.limbs();
    BigUint::from_limbs_le(l[..n.min(l.len())].to_vec())
}

/// Arithmetic modulo a fixed modulus, by Barrett reduction.
///
/// Construct once per modulus and reuse; see the module header for why the
/// precomputation is not folded into the operations.
///
/// ```
/// use yesno_core::bignum::{BigUint, Barrett};
///
/// let m = BigUint::from_u64(1_000_000_007);
/// let bar = Barrett::new(&m).unwrap();
/// let base = BigUint::from_u64(2);
/// let exp = BigUint::from_u64(1_000_000_006);
/// // Fermat: 2^(p-1) == 1 (mod p) for prime p.
/// assert_eq!(bar.pow_mod(&base, &exp), BigUint::one());
/// ```
#[derive(Clone, Debug)]
pub struct Barrett {
    m: BigUint,
    /// `floor( b^2k / m )`, where `k` is `m`'s limb count.
    mu: BigUint,
    k: usize,
}

impl Barrett {
    /// Precompute for modulus `m`, or `None` if `m` is zero.
    ///
    /// `None` rather than an error for the same reason
    /// [`BigUint::divrem`] returns it: there is no arithmetic modulo zero, and a
    /// zero read out of a sparse series is ordinary data rather than a caller
    /// bug.
    pub fn new(m: &BigUint) -> Option<Barrett> {
        if m.is_zero() {
            return None;
        }
        let k = m.limbs().len();
        // b^2k, as a 2k+1 limb value.
        let b2k = BigUint::one().shl(128 * k as u64);
        let (mu, _) = b2k.divrem(m)?;
        Some(Barrett {
            m: m.clone(),
            mu,
            k,
        })
    }

    /// The modulus.
    #[inline]
    pub fn modulus(&self) -> &BigUint {
        &self.m
    }

    /// `x mod m`.
    ///
    /// Barrett's estimate is exact to within two multiples of `m`, so the
    /// correction is a bounded loop rather than a division. The estimate is
    /// only valid for `x < b^2k`; a larger operand falls back to
    /// [`BigUint::divrem`], which is always correct and is what keeps this
    /// function total. A caller doing `mul_mod` on reduced operands never reaches
    /// the fallback, because `m^2 < b^2k`.
    pub fn reduce(&self, x: &BigUint) -> BigUint {
        debug_assert!(x.is_normalized());
        // Already reduced. Not merely an optimization: it is what makes
        // `add_mod` and `sub_mod` linear instead of paying a full reduction to
        // learn nothing.
        if *x < self.m {
            return x.clone();
        }
        if x.limbs().len() > 2 * self.k {
            #[cfg(test)]
            FALLBACKS.with(|c| c.set(c.get() + 1));
            return x.divrem(&self.m).expect("the modulus is non-zero").1;
        }

        let k = self.k;
        // q3 = floor( floor( x / b^(k-1) ) * mu / b^(k+1) ) -- the estimate of
        // the quotient, never more than two too small.
        let q1 = shr_limbs(x, k - 1);
        let q3 = shr_limbs(&q1.mul(&self.mu), k + 1);
        // Both remainders are taken mod b^(k+1), so the subtraction below stays
        // inside k+1 limbs and the high halves that would cancel are never
        // formed.
        let r1 = trunc_limbs(x, k + 1);
        let r2 = trunc_limbs(&q3.mul(&self.m), k + 1);
        let mut r = if r1 >= r2 {
            r1.sub(&r2).expect("just compared")
        } else {
            // `r1 - r2` is negative, so add back the `b^(k+1)` that taking
            // both remainders mod `b^(k+1)` borrowed against. Reachable only
            // when `x` sits just above a multiple of `b^(k+1)` — its low `k+1`
            // limbs smaller than the true residue — which happens for roughly
            // `3m / b^(k+1)`, about `2^-64`, of operands. **Zero of 200 000
            // random reductions took it.** So it has a constructed corpus and a
            // counter, on the same terms as Algorithm D's add-back.
            #[cfg(test)]
            NEGATIVES.with(|c| c.set(c.get() + 1));
            r1.add(&BigUint::one().shl(64 * (k as u64 + 1)))
                .sub(&r2)
                .expect("b^(k+1) exceeds any k+1 limb value")
        };
        let mut corrections = 0u32;
        while r >= self.m && corrections < MAX_CORRECTIONS {
            r = r.sub(&self.m).expect("just compared");
            corrections += 1;
        }
        if r >= self.m {
            // Unreachable while the estimate is correct, and the bound is the
            // point rather than the subtraction it saves. An unbounded `while`
            // here does not *fail* when the estimate breaks — it **hangs**, for
            // `r/m` iterations, which for an off-by-one in the `q3` shift is
            // astronomically many. Bounding it turns a test that times out into
            // one that reports, and leaves release builds correct-but-slower
            // rather than wrong.
            //
            // Found by sabotaging the `q1` shift: the suite hung instead of
            // going red, and a `debug_assert` placed after an unbounded loop is
            // never reached.
            debug_assert!(
                false,
                "Barrett's correction exceeded its two-subtraction bound"
            );
            r = r.divrem(&self.m).expect("the modulus is non-zero").1;
        }
        #[cfg(test)]
        SUBTRACTIONS.with(|c| c.set(c.get() + corrections as u64));
        r
    }

    /// `( a + b ) mod m`.
    ///
    /// One comparison and at most one subtraction once both operands are
    /// reduced. Spelling it as `reduce( a.add( b ) )` is correct and does more
    /// work — a Barrett reduction is two multiplications, where the sum of two
    /// reduced values is below `2m` and needs only a conditional subtract.
    ///
    /// That is the argument for the *shape*, not evidence anyone needs it:
    /// nothing outside the tests calls this. It is here because a modulus type
    /// without addition is an odd surface to hand a caller, which is a judgment
    /// rather than a measurement.
    pub fn add_mod(&self, a: &BigUint, b: &BigUint) -> BigUint {
        let s = self.reduce(a).add(&self.reduce(b));
        if s >= self.m {
            s.sub(&self.m).expect("just compared")
        } else {
            s
        }
    }

    /// `( a - b ) mod m`, wrapping into `[0, m)` rather than failing.
    ///
    /// The one place in this module where a subtraction below zero is *not*
    /// `None`: modulo `m` the answer exists, and it is `a + m - b`. That is a
    /// statement about the ring, not a softening of
    /// [`BigUint::sub`](BigUint::sub)'s contract.
    pub fn sub_mod(&self, a: &BigUint, b: &BigUint) -> BigUint {
        let (a, b) = (self.reduce(a), self.reduce(b));
        match a.sub(&b) {
            Some(d) => d,
            None => a.add(&self.m).sub(&b).expect("b < m <= a + m"),
        }
    }

    /// `( a * b ) mod m`.
    pub fn mul_mod(&self, a: &BigUint, b: &BigUint) -> BigUint {
        let p = self.reduce(a).mul(&self.reduce(b));
        self.reduce(&p)
    }

    /// `base^exp mod m`, by left-to-right square-and-multiply.
    ///
    /// One squaring per exponent bit and one multiplication per **set** exponent
    /// bit, each reduced immediately so no intermediate exceeds `2k` limbs.
    ///
    /// `m == 1` makes every residue zero, **including `base^0`**. A
    /// square-and-multiply that starts its accumulator at one and never reduces
    /// it returns `1` here, which is the classic wrong answer and the reason this
    /// case is named and tested rather than assumed to fall out.
    pub fn pow_mod(&self, base: &BigUint, exp: &BigUint) -> BigUint {
        if self.m == BigUint::one() {
            return BigUint::zero();
        }
        if exp.is_zero() {
            // `m > 1`, so one is already its own residue.
            return BigUint::one();
        }
        let b = self.reduce(base);
        let mut acc = BigUint::one();
        let mut i = exp.bit_len();
        while i > 0 {
            i -= 1;
            acc = self.mul_mod(&acc, &acc);
            if exp.bit(i) {
                acc = self.mul_mod(&acc, &b);
            }
        }
        acc
    }
}

impl BigUint {
    /// `self^exp mod m`, or `None` if `m` is zero.
    ///
    /// Builds a [`Barrett`] and discards it. For repeated work under one
    /// modulus, construct the [`Barrett`] once instead — the precomputation is a
    /// division, and paying it per call is the whole cost this module exists to
    /// amortize.
    pub fn pow_mod(&self, exp: &BigUint, m: &BigUint) -> Option<BigUint> {
        Some(Barrett::new(m)?.pow_mod(self, exp))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn big(limbs: &[u64]) -> BigUint {
        BigUint::from_limbs_le(limbs.to_vec())
    }

    fn lcg(state: &mut u64) -> u64 {
        *state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *state
    }

    fn random_big(state: &mut u64, limbs: usize) -> BigUint {
        BigUint::from_limbs_le((0..limbs).map(|_| lcg(state)).collect())
    }

    /// Reports ( conditional subtractions, divrem fallbacks, negative
    /// corrections ).
    fn counters<T>(f: impl FnOnce() -> T) -> (T, u64, u64, u64) {
        SUBTRACTIONS.with(|c| c.set(0));
        FALLBACKS.with(|c| c.set(0));
        NEGATIVES.with(|c| c.set(0));
        let out = f();
        (
            out,
            SUBTRACTIONS.with(|c| c.get()),
            FALLBACKS.with(|c| c.get()),
            NEGATIVES.with(|c| c.get()),
        )
    }

    /// **Constructed, not sampled**, and the third branch in `bignum/` that
    /// needs to be. `r1 < r2` requires `x` to sit just above a multiple of
    /// `b^(k+1)` — low `k+1` limbs below the true residue — which is about
    /// `3m / b^(k+1)`, roughly `2^-64`, of operands. A search over **200 000**
    /// random reductions found **zero**.
    ///
    /// Derived by fixing the low `k+1` limbs tiny and a high limb non-zero, then
    /// verifying against the true remainder. Only ever grows; this is the sole
    /// coverage of that branch, and `random_reductions_never_take_the_negative_branch`
    /// is the complement that says why sampling cannot replace it.
    const NEGATIVE_CORRECTION_CORPUS: &[(&[u64], &[u64])] = &[
        // k = 2, one conditional subtraction
        (
            &[9368018003224643702, 0, 0, 10835337665455605073],
            &[15970126346341786989, 15806332507635138087],
        ),
        // k = 2, none
        (
            &[11325541433240190585, 0, 0, 3434298343398869075],
            &[3406382097061735289, 9443047600179407145],
        ),
        // k = 3
        (
            &[
                772555797688975202,
                66981889739,
                0,
                0,
                17796639924064796019,
                12062741615009560746,
            ],
            &[
                8238189578454333843,
                2615658569448273025,
                9937141309157814053,
            ],
        ),
        // k = 4
        (
            &[
                11811297284913640262,
                18,
                0,
                0,
                0,
                84740076263776809,
                1567766003366382319,
                12078958286256443959,
            ],
            &[
                14353566573471694356,
                6019049990461352110,
                10901277510591751608,
                12826415475112627693,
            ],
        ),
    ];

    #[test]
    fn the_negative_correction_corpus_reaches_that_branch() {
        for (i, (x_limbs, m_limbs)) in NEGATIVE_CORRECTION_CORPUS.iter().enumerate() {
            let x = big(x_limbs);
            let m = big(m_limbs);
            let bar = Barrett::new(&m).unwrap();
            let (r, subs, fb, neg) = counters(|| bar.reduce(&x));
            assert_eq!(r, x.divrem(&m).unwrap().1, "entry {i}");
            assert_eq!(fb, 0, "entry {i} must take the Barrett path");
            assert!(subs <= 2, "entry {i}: {subs} subtractions");
            assert!(
                neg > 0,
                "entry {i} no longer reaches the negative correction"
            );
        }
    }

    /// The complement, and the reason the corpus exists.
    #[test]
    fn random_reductions_never_take_the_negative_branch() {
        let mut st = 0xc0ff_ee15_0d0d_beefu64;
        let (_, _, _, neg) = counters(|| {
            for k in 1usize..=3 {
                let m = random_big(&mut st, k);
                if m.is_zero() {
                    return;
                }
                let bar = Barrett::new(&m).unwrap();
                for _ in 0..300 {
                    let x = random_big(&mut st, 2 * k);
                    let _ = bar.reduce(&x);
                }
            }
        });
        assert_eq!(neg, 0, "randomized reduction took the negative branch");
    }

    /// `divrem` is the oracle: Barrett is a specialized arm that must agree with
    /// it on every operand, which is QG §4's rule applied inside this module.
    #[test]
    fn barrett_reduction_agrees_with_the_general_remainder() {
        let mut st = 0x9e37_79b9_7f4a_7c15u64;
        for m_len in 1usize..=4 {
            let m = random_big(&mut st, m_len);
            if m.is_zero() {
                continue;
            }
            let bar = Barrett::new(&m).unwrap();
            for x_len in 0usize..=8 {
                let x = random_big(&mut st, x_len);
                let expected = x.divrem(&m).unwrap().1;
                assert_eq!(bar.reduce(&x), expected, "m_len {m_len}, x_len {x_len}");
            }
        }
    }

    /// The boundary the Barrett estimate is proved against, and the one either
    /// side of it. Asserts the **fallback counter**, because "x is within 2k
    /// limbs" is a precondition a test can satisfy by accident.
    #[test]
    fn operands_at_the_two_k_limb_boundary_take_the_barrett_path() {
        let mut st = 0x0123_4567_89ab_cdefu64;
        for k in 1usize..=4 {
            let mut m = random_big(&mut st, k);
            // Force exactly k limbs.
            if m.limbs().len() != k {
                m = BigUint::from_limbs_le(
                    (0..k)
                        .map(|i| if i + 1 == k { 1 } else { lcg(&mut st) })
                        .collect(),
                );
            }
            let bar = Barrett::new(&m).unwrap();
            let at = random_big(&mut st, 2 * k);
            let (_, _, fb, _) = counters(|| {
                assert_eq!(bar.reduce(&at), at.divrem(&m).unwrap().1);
            });
            assert_eq!(fb, 0, "k = {k}: 2k limbs must not fall back");

            let over = random_big(&mut st, 2 * k + 1);
            let (_, _, fb, _) = counters(|| {
                assert_eq!(bar.reduce(&over), over.divrem(&m).unwrap().1);
            });
            assert_eq!(fb, 1, "k = {k}: 2k+1 limbs must fall back");
        }
    }

    /// The bound is what makes the correction `O(1)`; nothing else observes it.
    #[test]
    fn the_correction_never_exceeds_two_subtractions_per_reduction() {
        let mut st = 0xdead_beef_feed_faceu64;
        for m_len in 1usize..=4 {
            let m = random_big(&mut st, m_len);
            if m.is_zero() {
                continue;
            }
            let bar = Barrett::new(&m).unwrap();
            for x_len in 1usize..=2 * m_len {
                let x = random_big(&mut st, x_len);
                let (_, subs, _, _) = counters(|| bar.reduce(&x));
                assert!(
                    subs <= 2,
                    "m_len {m_len}, x_len {x_len}: {subs} subtractions"
                );
            }
        }
        // And a modulus that is a bare power of the limb base, which maximizes
        // `mu` and is where the bound is tightest.
        let m = big(&[0, 0, 1]);
        let bar = Barrett::new(&m).unwrap();
        for x in [big(&[u64::MAX; 4]), big(&[0, 0, u64::MAX]), big(&[1, 0, 1])] {
            let (r, subs, _, _) = counters(|| bar.reduce(&x));
            assert_eq!(r, x.divrem(&m).unwrap().1);
            assert!(subs <= 2, "{subs} subtractions");
        }
    }

    #[test]
    fn a_modulus_of_zero_has_no_barrett_form() {
        assert!(Barrett::new(&BigUint::zero()).is_none());
        assert_eq!(
            BigUint::from_u64(5).pow_mod(&BigUint::from_u64(3), &BigUint::zero()),
            None
        );
    }

    /// Including the zero exponent, which is where a square-and-multiply that
    /// starts at one and never reduces returns `1`.
    #[test]
    fn a_modulus_of_one_reduces_everything_to_zero() {
        let one = BigUint::one();
        let bar = Barrett::new(&one).unwrap();
        assert!(bar.reduce(&big(&[u64::MAX, u64::MAX])).is_zero());
        assert!(bar
            .pow_mod(&BigUint::from_u64(7), &BigUint::from_u64(9))
            .is_zero());
        assert!(bar
            .pow_mod(&BigUint::from_u64(7), &BigUint::zero())
            .is_zero());
        assert!(bar
            .mul_mod(&BigUint::from_u64(3), &BigUint::from_u64(5))
            .is_zero());
    }

    #[test]
    fn the_degenerate_exponent_and_base_cases_are_what_arithmetic_says() {
        let m = BigUint::from_u64(97);
        let bar = Barrett::new(&m).unwrap();
        // x^0 == 1 for m > 1, including 0^0.
        assert_eq!(
            bar.pow_mod(&BigUint::from_u64(5), &BigUint::zero()),
            BigUint::one()
        );
        assert_eq!(
            bar.pow_mod(&BigUint::zero(), &BigUint::zero()),
            BigUint::one()
        );
        // 0^e == 0 for e > 0.
        assert!(bar
            .pow_mod(&BigUint::zero(), &BigUint::from_u64(5))
            .is_zero());
        // 1^e == 1.
        assert_eq!(
            bar.pow_mod(&BigUint::one(), &big(&[u64::MAX, u64::MAX])),
            BigUint::one()
        );
    }

    /// The slow way: `e` modular multiplications, no reference implementation
    /// needed. Bounded to a few hundred so it stays a test rather than a
    /// benchmark.
    #[test]
    fn pow_mod_agrees_with_repeated_modular_multiplication() {
        let mut st = 0x1357_9bdf_2468_ace0u64;
        for m_len in 1usize..=3 {
            let m = random_big(&mut st, m_len);
            if m.is_zero() || m == BigUint::one() {
                continue;
            }
            let bar = Barrett::new(&m).unwrap();
            let base = random_big(&mut st, m_len);
            for e in [0u64, 1, 2, 3, 17, 64, 255] {
                let mut slow = BigUint::one();
                let br = bar.reduce(&base);
                for _ in 0..e {
                    slow = bar.mul_mod(&slow, &br);
                }
                assert_eq!(bar.pow_mod(&base, &BigUint::from_u64(e)), slow, "e = {e}");
            }
        }
    }

    /// A **structural** oracle: `a^(p-1) == 1 (mod p)` for prime `p` and `a` not
    /// a multiple of `p`. It needs no reference implementation at all, and it
    /// catches a whole class of exponent-loop errors — an off-by-one in the bit
    /// walk fails it immediately.
    #[test]
    fn fermats_little_theorem_holds_for_these_primes() {
        for p in [3u64, 5, 97, 65_537, 2_147_483_647, 1_000_000_007] {
            let m = BigUint::from_u64(p);
            let bar = Barrett::new(&m).unwrap();
            let e = BigUint::from_u64(p - 1);
            for a in [2u64, 3, 7, p - 1] {
                // The theorem needs `p` not to divide `a`. Dropping this
                // guard is not a stronger test, it is a false one: `3^2 mod 3`
                // is legitimately 0.
                if a % p == 0 {
                    continue;
                }
                let r = bar.pow_mod(&BigUint::from_u64(a), &e);
                assert_eq!(r, BigUint::one(), "a = {a}, p = {p}");
            }
        }
        // And one prime wider than a limb: 2^89 - 1 is prime.
        let p = BigUint::one().shl(89).sub(&BigUint::one()).unwrap();
        let bar = Barrett::new(&p).unwrap();
        let e = p.sub(&BigUint::one()).unwrap();
        assert_eq!(bar.pow_mod(&BigUint::from_u64(3), &e), BigUint::one());
    }

    #[test]
    fn modular_multiplication_is_commutative_and_associative() {
        let mut st = 0xfeed_c0de_1234_5678u64;
        for m_len in 1usize..=3 {
            let m = random_big(&mut st, m_len);
            if m.is_zero() {
                continue;
            }
            let bar = Barrett::new(&m).unwrap();
            let a = random_big(&mut st, m_len + 1);
            let b = random_big(&mut st, m_len);
            let c = random_big(&mut st, m_len + 2);
            assert_eq!(bar.mul_mod(&a, &b), bar.mul_mod(&b, &a));
            assert_eq!(
                bar.mul_mod(&bar.mul_mod(&a, &b), &c),
                bar.mul_mod(&a, &bar.mul_mod(&b, &c))
            );
            // And that it agrees with the unreduced product reduced once.
            assert_eq!(bar.mul_mod(&a, &b), bar.reduce(&a.mul(&b)));
        }
    }

    #[test]
    fn add_mod_and_sub_mod_are_mutually_inverse_and_agree_with_the_long_way() {
        let mut st = 0xabad_1dea_dead_10ccu64;
        for m_len in 1usize..=3 {
            let m = random_big(&mut st, m_len);
            if m.is_zero() {
                continue;
            }
            let bar = Barrett::new(&m).unwrap();
            for _ in 0..8 {
                let a = random_big(&mut st, m_len + 1);
                let b = random_big(&mut st, m_len);
                let s = bar.add_mod(&a, &b);
                assert!(s < m);
                assert_eq!(s, bar.reduce(&a.add(&b)));
                assert_eq!(bar.sub_mod(&s, &b), bar.reduce(&a));
                let d = bar.sub_mod(&a, &b);
                assert!(d < m);
                assert_eq!(bar.add_mod(&d, &b), bar.reduce(&a));
            }
        }
    }

    /// An even modulus is the case Montgomery could never serve, so it is the
    /// property that keeps Barrett reachable once Montgomery lands.
    #[test]
    fn an_even_modulus_is_still_accepted() {
        for m in [big(&[2]), big(&[1 << 63]), big(&[0, 1]), big(&[0xfffe, 4])] {
            let bar = Barrett::new(&m).unwrap();
            let x = big(&[u64::MAX, u64::MAX, 3]);
            assert_eq!(bar.reduce(&x), x.divrem(&m).unwrap().1);
            assert_eq!(
                bar.pow_mod(&BigUint::from_u64(3), &BigUint::from_u64(20)),
                BigUint::from_u64(3)
                    .pow_mod(&BigUint::from_u64(20), &m)
                    .unwrap()
            );
        }
    }

    /// `m = b^(k-1)` maximizes `mu`, which is where the limb bookkeeping in
    /// `reduce` is tightest: `mu` needs `k+2` limbs rather than the usual `k+1`.
    #[test]
    fn a_modulus_that_is_a_bare_power_of_the_limb_base_reduces_correctly() {
        for k in 1usize..=4 {
            let mut limbs = vec![0u64; k];
            limbs[k - 1] = 1;
            let m = BigUint::from_limbs_le(limbs);
            let bar = Barrett::new(&m).unwrap();
            let mut st = 0x5a5a_5a5a_5a5a_5a5au64 ^ k as u64;
            for len in 0..=2 * k {
                let x = random_big(&mut st, len);
                assert_eq!(bar.reduce(&x), x.divrem(&m).unwrap().1, "k {k}, len {len}");
            }
        }
    }
}
