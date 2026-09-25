//! Randomized property tests for `bignum` against a `num-bigint` oracle.
//!
//! # Why a separate file from `proptest_oracle.rs`
//!
//! That file's oracle is a `BTreeSet<u64>`, its generators produce *ordinals*,
//! and its shared checker takes an `&OrdSet`. Nothing here shares any of those:
//! the oracle is `num_bigint::BigUint`, the generators produce **limb vectors**,
//! and the invariant is normalization. Two unrelated oracle stacks in one file is
//! what `expr_equivalence.rs` already exists to avoid.
//!
//! It is also deliberately **not** in `differential.rs`. That file *is* the M0
//! gate and its contract is byte-level identity with the `roaring` crate in both
//! directions. There is no wire format here to be byte-identical to, so folding a
//! semantic-only differential in would dilute what an M0 pass means.
//!
//! # What this buys that the in-module tests cannot
//!
//! Everything under `src/bignum/` is validated against oracles this project
//! wrote: schoolbook for the multiply arms, Knuth D for division, Fermat and the
//! ring identities for the modular layer. Those are strong, and they share an
//! author. `num-bigint` is a mature independent implementation of the same
//! specification, so a systematic error common to all of the internal oracles —
//! a wrong limb order, an off-by-one in normalization, a misread of what `divrem`
//! should return — shows up here and nowhere else.
//!
//! **It is a semantic differential only**, and that is weaker than what
//! `roaring` buys. `differential.rs` gets **byte identity** because our container
//! payloads are spec-identical, which catches codec bugs no semantic test can.
//! Here the strongest available claim is limb-vector equality of the canonical
//! form — stronger than value equality, because it also pins normalization, and
//! weaker than byte identity, because no format is being agreed on.

use num_bigint::BigInt as SRef;
use num_bigint::BigUint as Ref;
use proptest::prelude::*;
use yesno_core::bignum::{Barrett, BigInt, BigUint, KARATSUBA_MIN};
use yesno_core::OrdSet;

fn to_ref(x: &BigUint) -> Ref {
    // `num-bigint` takes 32-bit digits little-endian; splitting each limb keeps
    // the bridge exact and endianness-explicit rather than going through bytes.
    let mut d = Vec::with_capacity(x.limbs().len() * 2);
    for &w in x.limbs() {
        d.push(w as u32);
        d.push((w >> 32) as u32);
    }
    Ref::new(d)
}

fn from_ref(x: &Ref) -> BigUint {
    let d = x.to_u32_digits();
    let mut limbs = Vec::with_capacity(d.len().div_ceil(2));
    for c in d.chunks(2) {
        limbs.push(c[0] as u64 | ((*c.get(1).unwrap_or(&0) as u64) << 32));
    }
    BigUint::from_limbs_le(limbs)
}

/// Every value produced by the crate must satisfy its own canonical-form
/// invariant, whatever the oracle says about its magnitude.
fn assert_canonical(x: &BigUint) {
    assert!(x.is_normalized(), "trailing zero limb in {x:?}");
    assert_eq!(
        x.limbs().len() as u64,
        x.bit_len().div_ceil(64),
        "limb count disagrees with bit length for {x:?}"
    );
}

/// Limbs biased the way `proptest_oracle.rs` biases ordinals, and for the same
/// reason: uniform limbs never produce the shapes the kernels branch on.
///
/// - A uniform limb has its top bit set half the time, so a uniform divisor is
///   **already normalized** and Knuth D's shift is never exercised at an
///   interesting value.
/// - Carry chains in uniform data are about one limb long, where `adc`'s
///   propagation loop and Karatsuba's spare sum limb both need runs of
///   `u64::MAX`.
/// - Uniform limb *counts* almost never land on the Karatsuba crossover.
fn limb_vec() -> impl Strategy<Value = Vec<u64>> {
    let lens = prop::sample::select(vec![
        0usize,
        1,
        2,
        3,
        KARATSUBA_MIN - 1,
        KARATSUBA_MIN,
        KARATSUBA_MIN + 1,
        2 * KARATSUBA_MIN,
        2 * KARATSUBA_MIN + 1,
        3 * KARATSUBA_MIN,
        41,
        64,
    ]);
    lens.prop_flat_map(|n| {
        prop_oneof![
            // Uniform: kept, but not dominant.
            2 => prop::collection::vec(any::<u64>(), n..=n),
            // Runs of all-ones: the carry-chain driver, and the shape that makes
            // Karatsuba's (a0+a1) overflow into its spare limb.
            3 => Just(vec![u64::MAX; n]),
            // All-ones with one hole, so the chain stops somewhere interesting.
            2 => (0usize..n.max(1)).prop_map(move |h| {
                let mut v = vec![u64::MAX; n];
                if h < v.len() { v[h] = 0; }
                v
            }),
            // Interior zeros: breaks a "strip zeros" written in the wrong
            // direction and forces short Toom-style intermediates.
            1 => (0usize..n.max(1)).prop_map(move |h| {
                let mut v = vec![0u64; n];
                if h < v.len() { v[h] = 1; }
                v
            }),
            // A top limb with a chosen number of leading zeros, which fixes
            // Knuth D's normalization shift `s` at each interesting value.
            2 => (prop::sample::select(vec![0u32, 1, 31, 32, 63]), prop::collection::vec(any::<u64>(), n..=n))
                .prop_map(|(lz, mut v)| {
                    if let Some(top) = v.last_mut() {
                        // Exactly `lz` leading zeros, with the rest kept random
                        // so the limb is not a bare power of two.
                        //
                        // Branched at 63 because `x >> 64` is the undefined
                        // full-width shift — the same hazard `div.rs` documents
                        // for its `64 - s`, met again here.
                        *top = if lz >= 63 { 1 } else { (1u64 << (63 - lz)) | (*top >> (lz + 1)) };
                    }
                    v
                }),
        ]
    })
}

fn big() -> impl Strategy<Value = BigUint> {
    limb_vec().prop_map(BigUint::from_limbs_le)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    #[test]
    fn the_limb_bridge_round_trips_through_the_oracle(a in big()) {
        prop_assert_eq!(from_ref(&to_ref(&a)), a.clone());
        assert_canonical(&a);
    }

    #[test]
    fn addition_agrees_with_num_bigint(a in big(), b in big()) {
        let got = a.add(&b);
        assert_canonical(&got);
        prop_assert_eq!(to_ref(&got), to_ref(&a) + to_ref(&b));
    }

    #[test]
    fn subtraction_agrees_with_num_bigint_or_declines(a in big(), b in big()) {
        match a.sub(&b) {
            Some(d) => {
                assert_canonical(&d);
                prop_assert!(a >= b);
                prop_assert_eq!(to_ref(&d), to_ref(&a) - to_ref(&b));
            }
            None => prop_assert!(a < b, "declined a subtraction that does not underflow"),
        }
    }

    #[test]
    fn multiplication_agrees_with_num_bigint(a in big(), b in big()) {
        let got = a.mul(&b);
        assert_canonical(&got);
        prop_assert_eq!(to_ref(&got), to_ref(&a) * to_ref(&b));
    }

    #[test]
    fn division_agrees_with_num_bigint_and_satisfies_its_identity(a in big(), b in big()) {
        match a.divrem(&b) {
            None => prop_assert!(b.is_zero()),
            Some((q, r)) => {
                assert_canonical(&q);
                assert_canonical(&r);
                let (ra, rb) = (to_ref(&a), to_ref(&b));
                prop_assert_eq!(to_ref(&q), &ra / &rb);
                prop_assert_eq!(to_ref(&r), &ra % &rb);
                // Both halves. A quotient digit one too small still satisfies
                // the first, because the excess lands in the remainder.
                prop_assert_eq!(q.mul(&b).add(&r), a.clone());
                prop_assert!(r < b);
            }
        }
    }

    #[test]
    fn shifts_and_truncation_agree_with_num_bigint(a in big(), n in 0u64..300) {
        prop_assert_eq!(to_ref(&a.shl(n)), to_ref(&a) << n as usize);
        prop_assert_eq!(to_ref(&a.shr(n)), to_ref(&a) >> n as usize);
        // truncate is `a mod 2^n`.
        let modulus = Ref::from(1u32) << n as usize;
        prop_assert_eq!(to_ref(&a.truncate(n)), to_ref(&a) % modulus);
        assert_canonical(&a.shl(n));
        assert_canonical(&a.truncate(n));
    }

    #[test]
    fn barrett_reduction_agrees_with_num_bigint(a in big(), m in big()) {
        match Barrett::new(&m) {
            None => prop_assert!(m.is_zero()),
            Some(bar) => {
                let got = bar.reduce(&a);
                assert_canonical(&got);
                prop_assert_eq!(to_ref(&got), to_ref(&a) % to_ref(&m));
                prop_assert!(got < m);
            }
        }
    }

    #[test]
    fn pow_mod_agrees_with_num_bigint_modpow(
        base in big(),
        e in 0u64..2048,
        m in big(),
    ) {
        let exp = BigUint::from_u64(e);
        match base.pow_mod(&exp, &m) {
            None => prop_assert!(m.is_zero()),
            Some(got) => {
                assert_canonical(&got);
                let want = to_ref(&base).modpow(&Ref::from(e), &to_ref(&m));
                prop_assert_eq!(to_ref(&got), want);
            }
        }
    }

    /// The whole `OrdSet` boundary, oracle-checked at both ends.
    ///
    /// One set is one integer, so the boundary is a pair of maps that must
    /// compose to the identity in both directions.
    #[test]
    fn a_value_round_trips_through_the_ordset_boundary(v in big()) {
        let set = OrdSet::from_int(&v).expect("bit length is far below the ceiling");
        let got = set.read_int(u64::MAX);
        prop_assert_eq!(&got, &v);
        prop_assert_eq!(to_ref(&got), to_ref(&v));
        // And back again: the set is recovered from the value it denotes.
        prop_assert_eq!(OrdSet::from_int(&got).expect("same value"), set);
    }

    /// Reading fewer bits than are there is exactly `v mod 2^w`, which is the
    /// only thing a width argument does.
    #[test]
    fn a_narrow_read_is_the_value_modulo_two_to_the_width(
        v in big(),
        w in 0u64..1_024,
    ) {
        let set = OrdSet::from_int(&v).expect("bit length is far below the ceiling");
        let got = set.read_int(w);
        prop_assert_eq!(&got, &v.truncate(w));
        prop_assert_eq!(to_ref(&got), to_ref(&v) % (Ref::from(1u32) << w as u32));
    }
}

/// The seam, restated for a model that no longer has a stride -- and the
/// systematic **width** sweep the randomized properties under-cover.
///
/// Those properties derive their values from whole-limb generators, so their
/// bit lengths cluster near multiples of 64. The top-limb mask in `truncate`
/// and the partial-limb handling in the read are exactly the code that is
/// trivial at a multiple of 64 and interesting three bits either side, which is
/// why the widths here are enumerated rather than sampled.
///
/// An integer used to be able to start at any bit offset, so a *limb* could
/// cross the 65 536-bit chunk boundary and the gather had a shift-and-carry
/// path for it. With the base pinned to zero that cannot happen -- 65 536 bits
/// is exactly 1 024 limbs, so a chunk boundary is always a limb boundary. What
/// remains worth covering is a value **spanning** several chunks, and the
/// assertion at the end is what makes that coverage a fact rather than a hope.
#[test]
fn the_boundary_round_trip_actually_covers_a_multi_chunk_value() {
    let mut seen_multi_chunk = 0usize;
    let mut seen_single_chunk = 0usize;
    let mut seen_partial_limb = 0usize;
    let mut st = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = || {
        st = st
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        st
    };

    // The boundaries the module's own comments call out: a limb, a chunk, and
    // one either side of each.
    let mut widths: Vec<u64> = (1..=200).collect();
    widths.extend([255, 256, 257, 511, 512, 513, 1023, 1024, 1025]);
    widths.extend([65_535, 65_536, 65_537, 131_071, 131_072, 200_000]);

    for &w in &widths {
        if w % 64 != 0 {
            seen_partial_limb += 1;
        }
        let v = BigUint::from_limbs_le((0..(w as usize).div_ceil(64)).map(|_| next()).collect())
            .truncate(w);
        let set = OrdSet::from_int(&v).expect("in range");

        let got = set.read_int(u64::MAX);
        assert_eq!(got, v, "round trip: w={w}");
        assert_eq!(to_ref(&got), to_ref(&v), "oracle: w={w}");

        if v.bit_len() > 65_536 {
            seen_multi_chunk += 1;
        } else if v.bit_len() > 0 {
            seen_single_chunk += 1;
        }

        // Reading narrower is exactly `v mod 2^narrow`, including at widths
        // that are not multiples of 64.
        for nw in [1u64, 63, 64, 65, 65_536, w.div_ceil(2)] {
            if nw > w {
                continue;
            }
            assert_eq!(set.read_int(nw), v.truncate(nw), "narrow: w={w} nw={nw}");
        }
    }

    // Anti-vacuity, in the shape `matrix/seek.rs` uses: without these the sweep
    // could pass having exercised neither of the two things it exists for.
    assert!(seen_multi_chunk > 0, "no value spanned more than one chunk");
    assert!(seen_single_chunk > 0, "no value stayed inside one chunk");
    assert!(
        seen_partial_limb > 100,
        "only {seen_partial_limb} widths had a partial top limb"
    );
}

/// An unwritten index reads as zero, and the oracle agrees that is what an empty
/// span means. Absence is not `None` — see `bignum::read`'s header.
#[test]
fn an_empty_set_reads_as_zero_everywhere_addressable() {
    let set = OrdSet::new();
    for w in [0u64, 1, 7, 256, u64::MAX] {
        let got = set.read_int(w);
        assert!(got.is_zero());
        assert_eq!(to_ref(&got), Ref::from(0u32));
    }
}

/// **Mandatory, and the reason is in `proptest_oracle.rs`'s own history**: the
/// first version of `container_values` produced a bitmap in 3 of 258 draws and
/// never a bitmap-by-bitmap pair, so the arms it existed to cover were barely
/// exercised while every property passed. "Check the distribution rather than
/// trusting it."
///
/// The analogue here is a generator that never reaches the Karatsuba arm, never
/// produces a divisor needing a normalization shift, and never builds a carry
/// chain — leaving the whole ladder covered only by the in-module tests.
#[test]
fn the_generators_reach_every_shape_the_kernels_branch_on() {
    use proptest::strategy::{Strategy, ValueTree};
    use proptest::test_runner::TestRunner;

    let mut runner = TestRunner::deterministic();
    let strat = limb_vec();
    let (mut karatsuba, mut needs_shift, mut long_carry, mut empty, mut has_zero_limb) =
        (0, 0, 0, 0, 0);
    const DRAWS: usize = 600;

    for _ in 0..DRAWS {
        let v = strat.new_tree(&mut runner).unwrap().current();
        let x = BigUint::from_limbs_le(v);
        let l = x.limbs();
        if l.len() >= KARATSUBA_MIN {
            karatsuba += 1;
        }
        if l.is_empty() {
            empty += 1;
        } else {
            if l[l.len() - 1].leading_zeros() > 0 {
                needs_shift += 1;
            }
            if l.iter().filter(|&&w| w == u64::MAX).count() >= 4 {
                long_carry += 1;
            }
            if l.contains(&0) {
                has_zero_limb += 1;
            }
        }
    }

    // Each is a shape some kernel branches on; a zero here means that branch is
    // covered by nothing in this file.
    assert!(karatsuba > 0, "no draw reached the Karatsuba arm");
    assert!(
        needs_shift > 0,
        "no divisor needed a non-zero normalization shift"
    );
    assert!(long_carry > 0, "no draw built a long carry chain");
    assert!(empty > 0, "zero was never drawn");
    assert!(
        has_zero_limb > 0,
        "no draw contained an interior or trailing zero limb"
    );
    // And not so skewed that the uniform case vanished.
    assert!(
        karatsuba < DRAWS,
        "every draw was large; the small-operand paths went untested"
    );
}

// ---------------------------------------------------------------------------
// Signed arithmetic
// ---------------------------------------------------------------------------

fn to_sref(x: &BigInt) -> SRef {
    let m = SRef::from(to_ref(x.magnitude()));
    if x.is_negative() {
        -m
    } else {
        m
    }
}

/// Every signed value must satisfy its own canonical form, whatever the oracle
/// says about the number: a normalized magnitude and no negative zero.
fn assert_signed_canonical(x: &BigInt) {
    assert!(x.is_canonical(), "non-canonical signed value: {x:?}");
}

prop_compose! {
    /// Boundary-biased like `big()`, then signed. A uniform sign is right here
    /// -- the interesting structure is in the magnitude, and the sign's own
    /// boundary ( zero, which cannot be negative ) is reached by `big()`
    /// producing zero.
    fn signed()(m in big(), negative in any::<bool>()) -> BigInt {
        BigInt::from_magnitude(negative, m)
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn signed_add_sub_mul_match_the_oracle(a in signed(), b in signed()) {
        for (got, want) in [
            (a.add(&b), to_sref(&a) + to_sref(&b)),
            (a.sub(&b), to_sref(&a) - to_sref(&b)),
            (a.mul(&b), to_sref(&a) * to_sref(&b)),
        ] {
            assert_signed_canonical(&got);
            prop_assert_eq!(to_sref(&got), want);
        }
    }

    /// `num-bigint`'s `/` and `%` truncate toward zero, which is the convention
    /// this type chose -- so the oracle checks the convention and not just the
    /// magnitudes.
    #[test]
    fn signed_division_matches_the_oracle_including_its_signs(a in signed(), b in signed()) {
        prop_assume!(!b.is_zero());
        let (q, r) = a.divrem(&b).expect("non-zero divisor");
        assert_signed_canonical(&q);
        assert_signed_canonical(&r);
        prop_assert_eq!(to_sref(&q), to_sref(&a).clone() / to_sref(&b));
        prop_assert_eq!(to_sref(&r), to_sref(&a) % to_sref(&b));
        // And the identity, independently of the oracle.
        prop_assert_eq!(to_sref(&q.mul(&b).add(&r)), to_sref(&a));
    }

    #[test]
    fn the_euclidean_remainder_is_non_negative_and_reconstructs(a in signed(), b in signed()) {
        prop_assume!(!b.is_zero());
        let (q, r) = a.div_euclid_rem(&b).expect("non-zero divisor");
        assert_signed_canonical(&q);
        assert_signed_canonical(&r);
        prop_assert!(!r.is_negative());
        prop_assert!(r.abs() < b.abs());
        prop_assert_eq!(to_sref(&q.mul(&b).add(&r)), to_sref(&a));
    }

    /// Ordering is the operation a sign-and-magnitude representation most
    /// easily gets wrong, because the magnitudes reverse below zero.
    #[test]
    fn signed_ordering_matches_the_oracle(a in signed(), b in signed()) {
        prop_assert_eq!(a.cmp(&b), to_sref(&a).cmp(&to_sref(&b)));
    }

    /// A conservative extension: on non-negative operands the signed operations
    /// are the unsigned ones, limb for limb.
    #[test]
    fn signed_operations_reduce_to_the_unsigned_ones(a in big(), b in big()) {
        let (sa, sb) = (BigInt::from_uint(a.clone()), BigInt::from_uint(b.clone()));
        let (ssum, usum) = (sa.add(&sb), a.add(&b));
        prop_assert_eq!(ssum.magnitude().limbs(), usum.limbs());
        let (sprod, uprod) = (sa.mul(&sb), a.mul(&b));
        prop_assert_eq!(sprod.magnitude().limbs(), uprod.limbs());
        if !b.is_zero() {
            let (uq, ur) = a.divrem(&b).expect("non-zero");
            let (sq, sr) = sa.divrem(&sb).expect("non-zero");
            prop_assert_eq!(sq.magnitude().limbs(), uq.limbs());
            prop_assert_eq!(sr.magnitude().limbs(), ur.limbs());
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Saturation against the oracle, at both signs, plus the two properties
    /// the module claims for it: it nests, and it lands inside the field.
    #[test]
    fn saturation_clamps_to_the_field_and_nests(a in signed(), w1 in 0u64..300, w2 in 0u64..300) {
        let got = a.saturate(w1);
        assert_signed_canonical(&got);

        // Against the oracle: clamp into the two's-complement range for w1.
        let want = if w1 == 0 {
            SRef::from(0)
        } else {
            let ceiling = SRef::from(1) << (w1 - 1) as u32;
            let max = ceiling.clone() - SRef::from(1);
            let min = -ceiling;
            to_sref(&a).clamp(min, max)
        };
        prop_assert_eq!(to_sref(&got), want);

        // Nests: applying twice is applying at the narrower width.
        prop_assert_eq!(a.saturate(w1).saturate(w2), a.saturate(w1.min(w2)));
    }

    /// The unsigned clamp, same two claims.
    #[test]
    fn unsigned_saturation_clamps_and_nests(a in big(), w1 in 0u64..300, w2 in 0u64..300) {
        let got = a.saturate(w1);
        assert_canonical(&got);
        let ceiling = (Ref::from(1u32) << w1 as u32) - Ref::from(1u32);
        prop_assert_eq!(to_ref(&got), to_ref(&a).min(ceiling));
        prop_assert_eq!(a.saturate(w1).saturate(w2), a.saturate(w1.min(w2)));
    }

    /// Truncation and saturation are different rules, and **naming when they
    /// differ is harder than it looks** -- two drafts of this property were
    /// wrong before this one. They agree whenever nothing is clamped, and also
    /// whenever the low `w` bits happen to be all ones, because the wrap and
    /// the ceiling land on the same number. So the checkable statement is what
    /// each rule *is*, not when they disagree.
    #[test]
    fn saturation_is_the_ceiling_where_truncation_is_the_wrap(
        a in big(),
        w in 0u64..300,
    ) {
        let (t, s) = (a.truncate(w), a.saturate(w));
        assert_canonical(&t);
        assert_canonical(&s);

        // Truncation is the residue; saturation is the minimum with the ceiling.
        let ceiling = (Ref::from(1u32) << w as u32) - Ref::from(1u32);
        prop_assert_eq!(to_ref(&t), to_ref(&a) % (Ref::from(1u32) << w as u32));
        prop_assert_eq!(to_ref(&s), to_ref(&a).min(ceiling.clone()));

        // Neither ever leaves the field, which is the whole point of both.
        prop_assert!(t.bit_len() <= w);
        prop_assert!(s.bit_len() <= w);

        // And they coincide exactly when the residue is already the ceiling --
        // which includes, but is not limited to, the un-clamped case.
        prop_assert_eq!(t == s, to_ref(&t) == ceiling || a.bit_len() <= w);
    }
}
