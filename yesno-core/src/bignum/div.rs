//! Division: Knuth's Algorithm D, and the oracle Burnikel-Ziegler will answer to.
//!
//! `divrem` is the only primitive. A quotient without its remainder is the same
//! work, and returning both costs nothing over returning either.
//!
//! # This is the oracle
//!
//! `O(m*n)`, correct for every pair of operands, and never deleted when a
//! recursive divider is faster — Burnikel-Ziegler's base case *is* this function,
//! so it could not be deleted even if the policy allowed it. Same contract as
//! [`BigUint::mul_schoolbook`](super::BigUint::mul_schoolbook).
//!
//! # A zero divisor is `None`, not a panic
//!
//! `u64 / 0` panics and so does `num-bigint`, so this is a deliberate divergence.
//! The reason is [`OrdSet::read_int`](crate::OrdSet::read_int): an index no
//! writer ever filled reads as **zero**, and zero is a legitimate value rather
//! than an absence. So a zero divisor here is an ordinary consequence of reading
//! a sparse series, not a caller bug — and a panic would turn ordinary data into
//! a crash while making the reader's `Option` pointless. `None` follows
//! [`BitMatrix::invert_gf2`](crate::matrix::BitMatrix::invert_gf2) on a singular
//! matrix: the answer is not in the domain.
//!
//! # The four places this goes wrong, and what pins each
//!
//! **D1, normalization.** Both operands are shifted left by
//! `s = v[n-1].leading_zeros()` so the divisor's top bit is set, which is what
//! bounds the D3 estimate's error to one. Two traps. The dividend gains an
//! extra high limb and it must be allocated **even when `s == 0`**, because the
//! loop indexes `un[j + n]` at `j == m` regardless; allocating it only "when
//! needed" reads out of bounds on exactly the inputs a uniform generator
//! produces least often, since a uniform limb has its top bit set half the time
//! and `s == 0` is therefore the *common* case a lazy test would cover by
//! accident. And `x >> (64 - s)` is the undefined full-width shift when `s == 0`,
//! so the shift must be **branched**, not computed.
//!
//! **D3, the quotient-digit estimate.** Three sub-steps, three ways to be wrong:
//! the `qhat >= 2^64` clamp, reachable only when the leading limbs are equal; the
//! `rhat >= 2^64` break, which is the refinement loop's *termination* condition
//! rather than an optimization; and the refinement itself, which after
//! normalization runs at most twice.
//!
//! **D4 to D6, the add-back correction.** **This branch fires with probability
//! about `2/2^64` on uniform operands, so it is not under-sampled — it is
//! structurally unreachable by random testing.** It has a checked-in constructed
//! corpus and a counter the tests assert is non-zero, because a corpus that
//! silently stopped reaching the branch would leave the test passing and
//! measuring nothing.
//!
//! **D8, unnormalization.** The **remainder** is shifted back right by `s`; the
//! **quotient is not**. Getting that backwards produces a plausible wrong answer
//! on roughly half of all inputs.

use super::addsub::{adc, sbb};
use super::BigUint;

// Counts D6 add-back corrections so a test can assert its corpus reaches the
// branch rather than hoping. Thread-local because libtest runs each test on its
// own thread, so the counter needs no synchronization and no `--test-threads=1`.
// ( A `///` comment here is an `unused_doc_comment` error: rustdoc does not
// document items produced by a macro invocation. )
#[cfg(test)]
thread_local! {
    static ADD_BACKS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static CLAMPS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn note_add_back() {
    ADD_BACKS.with(|c| c.set(c.get() + 1));
}

#[cfg(test)]
fn note_clamp() {
    CLAMPS.with(|c| c.set(c.get() + 1));
}

impl BigUint {
    /// `( self / d, self % d )`, or `None` if `d` is zero.
    ///
    /// The remainder is always strictly less than `d`. See the module header for
    /// why a zero divisor is `None` rather than a panic.
    pub fn divrem(&self, d: &BigUint) -> Option<(BigUint, BigUint)> {
        self.divrem_knuth(d)
    }

    /// `( self / d, self % d )` for a single-limb divisor, or `None` if `d` is
    /// zero.
    ///
    /// One `u128` division per limb, most significant first. This is the `n == 1`
    /// case Algorithm D structurally cannot serve — D3 reads `v[n-2]`.
    pub fn divrem_u64(&self, d: u64) -> Option<(BigUint, u64)> {
        debug_assert!(self.is_normalized());
        if d == 0 {
            return None;
        }
        let u = self.limbs();
        let mut q = vec![0u64; u.len()];
        let mut rem = 0u128;
        for i in (0..u.len()).rev() {
            // `rem < d <= u64::MAX`, so this cannot overflow the u128.
            let cur = (rem << 64) | u[i] as u128;
            q[i] = (cur / d as u128) as u64;
            rem = cur % d as u128;
        }
        Some((BigUint::from_limbs_le(q), rem as u64))
    }

    /// Knuth's Algorithm D. The generic kernel; see the module header.
    pub(crate) fn divrem_knuth(&self, d: &BigUint) -> Option<(BigUint, BigUint)> {
        debug_assert!(self.is_normalized() && d.is_normalized());
        if d.is_zero() {
            return None;
        }
        if self < d {
            return Some((BigUint::zero(), self.clone()));
        }
        let n = d.limbs().len();
        if n == 1 {
            let (q, r) = self.divrem_u64(d.limbs()[0])?;
            return Some((q, BigUint::from_u64(r)));
        }
        // No leading-zero-limb strip is needed here, and that is a property of
        // the type rather than an omission: normalization is a `BigUint`
        // invariant, so `v[n-1]` is non-zero by construction. A limb-slice API
        // would have to strip first, and forgetting to is what breaks D1.
        let u = self.limbs();
        let v = d.limbs();
        let len_u = u.len();
        // `self >= d` and both are normalized, so the dividend is at least as
        // long as the divisor.
        let m = len_u - n;

        // D1: normalize. Branched on `s == 0` because `x >> 64` is undefined.
        let s = v[n - 1].leading_zeros();
        let mut vn = vec![0u64; n];
        // One limb longer than the dividend, unconditionally: `un[j + n]` is read
        // at `j == m` whatever `s` is.
        let mut un = vec![0u64; len_u + 1];
        if s == 0 {
            vn.copy_from_slice(v);
            un[..len_u].copy_from_slice(u);
        } else {
            for i in (1..n).rev() {
                vn[i] = (v[i] << s) | (v[i - 1] >> (64 - s));
            }
            vn[0] = v[0] << s;
            un[len_u] = u[len_u - 1] >> (64 - s);
            for i in (1..len_u).rev() {
                un[i] = (u[i] << s) | (u[i - 1] >> (64 - s));
            }
            un[0] = u[0] << s;
        }

        let mut q = vec![0u64; m + 1];
        let base = 1u128 << 64;
        let vn1 = vn[n - 1] as u128;
        let vn2 = vn[n - 2] as u128;

        // D2: loop over quotient digits, most significant first.
        for j in (0..=m).rev() {
            // D3: estimate this digit from the top two limbs, then refine with
            // the third. `qhat * vn2` is guarded by the short-circuit: it is
            // evaluated only when `qhat < 2^64`, where the product is at most
            // `2^128 - 2^64`. `rhat << 64` is likewise reached only with
            // `rhat < 2^64`, because the loop breaks the moment it is not.
            let num = ((un[j + n] as u128) << 64) | (un[j + n - 1] as u128);
            let mut qhat = num / vn1;
            let mut rhat = num % vn1;
            #[cfg(test)]
            if qhat >= base {
                note_clamp();
            }
            while qhat >= base || qhat * vn2 > (rhat << 64) + (un[j + n - 2] as u128) {
                qhat -= 1;
                rhat += vn1;
                if rhat >= base {
                    break;
                }
            }
            // The algorithm's invariant `un[j+n] <= vn[n-1]` bounds the raw
            // estimate at `2^64 + 1`. At exactly `2^64 + 1` the first decrement
            // leaves `rhat == num - vn1*2^64 < 2^64`, so the loop does *not*
            // break there and runs again — which is why the exit can never leave
            // `qhat` above a limb.
            debug_assert!(qhat < base, "the refined estimate must fit one limb");

            // D4: un[j ..= j+n] -= qhat * vn, fused, with no intermediate product
            // materialized. Allocating `qhat * vn` per digit is the decay mode
            // the allocation budget exists to catch.
            let mut borrow = 0u64;
            let mut carry = 0u64;
            for i in 0..n {
                let p = qhat * (vn[i] as u128) + carry as u128;
                carry = (p >> 64) as u64;
                let (diff, bo) = sbb(un[i + j], p as u64, borrow);
                un[i + j] = diff;
                borrow = bo;
            }
            let (diff, over) = sbb(un[j + n], carry, borrow);
            un[j + n] = diff;

            // D5 / D6: the estimate was one too large. Give back one multiple of
            // the divisor; the carry out of that addition exactly cancels the
            // borrow above and is discarded.
            if over != 0 {
                #[cfg(test)]
                note_add_back();
                qhat -= 1;
                let mut c = 0u64;
                for i in 0..n {
                    let (sum, cc) = adc(un[i + j], vn[i], c);
                    un[i + j] = sum;
                    c = cc;
                }
                un[j + n] = un[j + n].wrapping_add(c);
            }
            q[j] = qhat as u64;
        }

        // D8: the remainder is unnormalized, the quotient is not.
        let mut rem = un[..n].to_vec();
        if s != 0 {
            for i in 0..n - 1 {
                rem[i] = (rem[i] >> s) | (rem[i + 1] << (64 - s));
            }
            rem[n - 1] >>= s;
        }
        Some((BigUint::from_limbs_le(q), BigUint::from_limbs_le(rem)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn big(limbs: &[u64]) -> BigUint {
        BigUint::from_limbs_le(limbs.to_vec())
    }

    /// Runs `f` and reports ( add-back corrections, saturating estimates ).
    /// Both branches are structurally unreachable by sampling, so every test
    /// that claims to cover one asserts on the count rather than on the answer.
    fn counters<T>(f: impl FnOnce() -> T) -> (T, u64, u64) {
        ADD_BACKS.with(|c| c.set(0));
        CLAMPS.with(|c| c.set(0));
        let out = f();
        (out, ADD_BACKS.with(|c| c.get()), CLAMPS.with(|c| c.get()))
    }

    /// Deterministic, cheap, and not an RNG dependency — the convention
    /// `matrix/gf2.rs` and `benches/setops.rs` already use.
    fn lcg(state: &mut u64) -> u64 {
        *state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *state
    }

    fn random_big(state: &mut u64, limbs: usize) -> BigUint {
        BigUint::from_limbs_le((0..limbs).map(|_| lcg(state)).collect())
    }

    /// **Constructed, not sampled.** D6 fires with probability about `2/2^64`
    /// on uniform operands, so no generator will ever reach it. These pairs were
    /// derived by searching for operands whose refined D3 estimate is exactly one
    /// too large, then verified against a simulation of this algorithm. They
    /// cover a zero and a non-zero normalization shift, and three- and four-limb
    /// divisors.
    ///
    /// Treat this like `tests/*.proptest-regressions`: it only grows. If a
    /// later run finds an input that trips the counter, add it.
    const ADD_BACK_CORPUS: &[(&[u64], &[u64])] = &[
        // s = 0, divisor 3 limbs
        (
            &[0xffffffffffffffff, 0x0, 0x0, 0x1],
            &[0x8000000000000000, 0x0, 0x8000000000000000],
        ),
        (
            &[0xfffffffffffffffe, 0x0, 0x0, 0x1],
            &[0x8000000000000000, 0x0, 0x8000000000000000],
        ),
        // s = 0, divisor 4 limbs
        (
            &[0xffffffffffffffff, 0x0, 0xfffffffffffffffe, 0x1, 0x1],
            &[
                0x8000000000000000,
                0x0,
                0xffffffffffffffff,
                0x8000000000000000,
            ],
        ),
        // s = 1, divisor 3 limbs
        (
            &[0xffffffffffffffff, 0x0, 0x0, 0x1],
            &[0x4000000000000000, 0x0, 0x4000000000000000],
        ),
        (
            &[0xfffffffffffffffe, 0x0, 0x0, 0x1],
            &[0x4000000000000000, 0x0, 0x4000000000000000],
        ),
        // s = 1, divisor 4 limbs
        (
            &[0xffffffffffffffff, 0x0, 0xfffffffffffffffe, 0x1, 0x1],
            &[
                0x4000000000000000,
                0x8000000000000000,
                0x7fffffffffffffff,
                0x4000000000000000,
            ],
        ),
    ];

    /// Both halves, and they are independent: a quotient digit one too *small*
    /// still satisfies `q*d + r == u`, because the excess lands in `r`. Only
    /// `r < d` catches it. A division test asserting only the first identity
    /// is the classic vacuous one.
    fn assert_division_identity(u: &BigUint, d: &BigUint) {
        let (q, r) = u.divrem(d).expect("non-zero divisor");
        assert_eq!(q.mul(d).add(&r), *u, "q*d + r == u");
        assert!(r < *d, "r < d");
        assert!(q.is_normalized() && r.is_normalized());
    }

    #[test]
    fn dividing_by_zero_is_none_and_not_a_panic() {
        assert_eq!(big(&[1, 2, 3]).divrem(&BigUint::zero()), None);
        assert_eq!(BigUint::zero().divrem(&BigUint::zero()), None);
        assert_eq!(big(&[5]).divrem_u64(0), None);
    }

    #[test]
    fn a_divisor_larger_than_the_dividend_yields_a_zero_quotient() {
        let u = big(&[7]);
        let d = big(&[0, 1]);
        assert_eq!(u.divrem(&d), Some((BigUint::zero(), u.clone())));
        assert_eq!(
            BigUint::zero().divrem(&d),
            Some((BigUint::zero(), BigUint::zero()))
        );
    }

    #[test]
    fn small_divisions_are_exact() {
        assert_eq!(
            big(&[100]).divrem(&big(&[7])),
            Some((BigUint::from_u64(14), BigUint::from_u64(2)))
        );
        // 2^128 / 2^64 == 2^64, remainder 0.
        assert_eq!(
            big(&[0, 0, 1]).divrem(&big(&[0, 1])),
            Some((big(&[0, 1]), BigUint::zero()))
        );
        assert_eq!(
            big(&[u64::MAX, u64::MAX]).divrem(&big(&[u64::MAX])),
            Some((big(&[1, 1]), BigUint::zero()))
        );
    }

    #[test]
    fn division_and_multiplication_are_mutually_inverse() {
        let mut st = 0x1234_5678_9abc_def0u64;
        for du in [1usize, 2, 3, 5, 8, 17] {
            for dd in [1usize, 2, 3, 4, 9] {
                let u = random_big(&mut st, du);
                let d = random_big(&mut st, dd);
                if d.is_zero() {
                    continue;
                }
                assert_division_identity(&u, &d);
            }
        }
    }

    /// The exact-multiple case, which is where a quotient digit one too small
    /// hides most easily: the remainder is legitimately zero either way unless
    /// the quotient is checked.
    #[test]
    fn an_exact_multiple_divides_with_no_remainder() {
        let mut st = 0xfeed_face_dead_beefu64;
        for (a_len, b_len) in [(1usize, 1usize), (3, 2), (5, 3), (8, 5), (2, 8)] {
            let a = random_big(&mut st, a_len);
            let b = random_big(&mut st, b_len);
            if a.is_zero() || b.is_zero() {
                continue;
            }
            let p = a.mul(&b);
            let (q, r) = p.divrem(&b).unwrap();
            assert!(r.is_zero(), "a*b should divide by b exactly");
            assert_eq!(q, a);
        }
    }

    /// The assertion that matters is on the **counter**, not on the answers.
    /// Without it this test still passes on a corpus that has stopped reaching
    /// D6, and then it measures nothing at all.
    #[test]
    fn the_add_back_corpus_actually_reaches_the_correction() {
        for (i, (u_limbs, v_limbs)) in ADD_BACK_CORPUS.iter().enumerate() {
            let u = big(u_limbs);
            let d = big(v_limbs);
            let (_, hits, _) = counters(|| assert_division_identity(&u, &d));
            assert!(hits > 0, "corpus entry {i} no longer reaches D6");
        }
    }

    /// **Also constructed, and the construction is less obvious than the
    /// add-back one.** `qhat` reaches `2^64` only when the running remainder's
    /// top limb equals the divisor's — but that alone is not enough, because the
    /// refinement's `vn2` clause then decrements anyway and masks a missing
    /// clamp. The clamp is load-bearing only when the **second** limb matches
    /// too, which makes the `vn2` comparison come out equal rather than greater.
    ///
    /// A two-limb divisor cannot express that: `R < V` with equal top limbs
    /// forces `R`'s low limb strictly below `V`'s, which is exactly the condition
    /// that fires the `vn2` clause. So the corpus needs `n >= 3`, and a suite
    /// testing only two-limb divisors covers this branch zero times while looking
    /// like it covers it. That is not hypothetical — it is what an earlier
    /// version of this test did, and a sabotage weakening `>=` to `>` passed.
    const CLAMP_CORPUS: &[(&[u64], &[u64])] = &[
        (
            &[0x1234, 0x3, 0x5, 0x8000000000000000],
            &[0x9, 0x5, 0x8000000000000000],
        ),
        (
            &[0xffffffffffffffff, 0x0, 0x0, 0x8000000000000000],
            &[0x1, 0x0, 0x8000000000000000],
        ),
        (
            &[0xabcdef, 0x7, 0x1234, 0x800000000000004d],
            &[0xdeadbeef, 0x1234, 0x800000000000004d],
        ),
    ];

    #[test]
    fn the_clamp_corpus_reaches_the_saturating_estimate() {
        for (i, (u_limbs, v_limbs)) in CLAMP_CORPUS.iter().enumerate() {
            let u = big(u_limbs);
            let d = big(v_limbs);
            let (_, _, clamps) = counters(|| assert_division_identity(&u, &d));
            assert!(clamps > 0, "corpus entry {i} no longer reaches the clamp");
        }
    }

    /// The complement of the above, and the reason the corpus has to exist:
    /// ordinary operands never take the branch, so a suite without the corpus
    /// covers D6 zero times while looking thorough.
    #[test]
    fn random_operands_reach_neither_rare_branch() {
        let mut st = 0x0f0f_0f0f_0f0f_0f0fu64;
        let (_, hits, clamps) = counters(|| {
            for _ in 0..200 {
                let u = random_big(&mut st, 6);
                let d = random_big(&mut st, 3);
                if d.is_zero() {
                    continue;
                }
                let _ = u.divrem(&d);
            }
        });
        assert_eq!(hits, 0, "randomized division reached D6 by chance");
        assert_eq!(clamps, 0, "randomized division reached the clamp by chance");
    }

    /// D1 with `s == 0` still needs the extra dividend limb, and `s == 63` is the
    /// largest shift the branch has to handle.
    #[test]
    fn every_normalization_shift_divides_correctly() {
        let mut st = 0xabcd_ef01_2345_6789u64;
        for s in [0u32, 1, 31, 32, 62, 63] {
            // A divisor whose top limb has exactly `s` leading zeros.
            let top = if s == 0 { 1u64 << 63 } else { 1u64 << (63 - s) };
            let d = big(&[lcg(&mut st), lcg(&mut st), top]);
            assert_eq!(d.limbs()[2].leading_zeros(), s);
            for extra in 0..4 {
                let u = random_big(&mut st, 3 + extra);
                if u < d {
                    continue;
                }
                assert_division_identity(&u, &d);
            }
        }
    }

    /// The `qhat == 2^64` clamp is reachable only when the dividend window's top
    /// limb equals the divisor's, so it needs operands built to make that happen
    /// rather than sampled ones.
    #[test]
    fn the_quotient_estimate_saturates_when_the_leading_limbs_are_equal() {
        // Kept as identity coverage, but these operands do *not* reach the
        // clamp — see `CLAMP_CORPUS` for why matching one leading limb is not
        // enough, and `the_clamp_corpus_reaches_the_saturating_estimate` for the
        // ones that do.
        let d = big(&[1, 0, 1u64 << 63]);
        let u = big(&[u64::MAX, u64::MAX, 0, 1u64 << 63]);
        assert_division_identity(&u, &d);
        let u2 = big(&[0, 0, u64::MAX, 1u64 << 63]);
        assert_division_identity(&u2, &d);
    }

    /// `divrem_u64` is a parallel implementation of the general path, and only a
    /// comparison can see it drift. Scaling both operands by `2^64` leaves the
    /// quotient unchanged and pushes the divisor to two limbs, which is exactly
    /// the smallest case Algorithm D can serve.
    #[test]
    fn single_limb_division_agrees_with_algorithm_d() {
        let mut st = 0x5555_aaaa_5555_aaaau64;
        for _ in 0..40 {
            let u = random_big(&mut st, 4);
            let d = lcg(&mut st) | 1;
            let (q1, r1) = u.divrem_u64(d).unwrap();
            let (q2, r2) = u.shl(64).divrem(&BigUint::from_u64(d).shl(64)).unwrap();
            assert_eq!(q1, q2);
            assert_eq!(BigUint::from_u64(r1).shl(64), r2);
        }
    }

    #[test]
    fn dividing_by_a_power_of_two_equals_a_right_shift() {
        let mut st = 0x2468_ace0_1357_9bdfu64;
        let u = random_big(&mut st, 6);
        for k in [1u64, 63, 64, 65, 127, 128, 200] {
            let d = BigUint::one().shl(k);
            let (q, r) = u.divrem(&d).unwrap();
            assert_eq!(q, u.shr(k), "k = {k}");
            assert_eq!(r, u.truncate(k), "k = {k}");
        }
    }

    /// Operands made almost entirely of all-ones limbs drive the longest carry
    /// and borrow chains D4 and D6 can produce.
    #[test]
    fn all_ones_operands_divide_correctly() {
        for un in 1usize..6 {
            for dn in 1usize..=un {
                let u = big(&vec![u64::MAX; un]);
                let d = big(&vec![u64::MAX; dn]);
                assert_division_identity(&u, &d);
                // And one below, which changes every borrow.
                assert_division_identity(&u.sub(&BigUint::one()).unwrap(), &d);
            }
        }
    }
}
