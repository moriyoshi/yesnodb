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

use num_bigint::BigUint as Ref;
use proptest::prelude::*;
use yesno_core::bignum::{Barrett, BigUint, IntLayout, IntSink, KARATSUBA_MIN};
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
    #[test]
    fn a_series_round_trips_through_the_ordset_boundary(
        values in prop::collection::vec(big(), 1..6),
        stride_pad in 0u64..3,
    ) {
        let width = values.iter().map(|v| v.bit_len()).max().unwrap_or(0).max(1);
        prop_assume!(width <= 20_000);
        let width = width as u32;
        let layout = IntLayout { width_bits: width, stride: width as u64 + stride_pad };
        let mut sink = IntSink::new(layout);
        for (k, v) in values.iter().enumerate() {
            sink.place(k as u64, v).expect("width was taken from the widest value");
        }
        let set = sink.build();
        for (k, v) in values.iter().enumerate() {
            let got = set.read_int(k as u64, &layout).expect("addressable");
            prop_assert_eq!(&got, v);
            prop_assert_eq!(to_ref(&got), to_ref(v));
        }
        // `int_count` reaches the highest **set ordinal**, so a zero *above*
        // every non-zero value is invisible — absence and zero write the same
        // thing, which is nothing. A zero *below* one is still counted. Asserting
        // `>= values.len()` is therefore wrong, and this is the exact property
        // instead.
        let last_nonzero = values.iter().rposition(|v| !v.is_zero());
        prop_assert_eq!(
            set.int_count(&layout),
            last_nonzero.map_or(0, |i| i as u64 + 1)
        );
    }
}

/// The seam. `width_bits < 65536` does not imply chunk containment, and a
/// generator that never places a straddling integer leaves the gather's hardest
/// path untested while every property above still passes.
///
/// Copied in spirit from `matrix/read.rs`'s `saw_straddle` flag: the assertion at
/// the end is what makes the coverage a fact rather than a hope.
#[test]
fn the_boundary_round_trip_actually_covers_a_straddling_integer() {
    let mut seen_straddle = 0usize;
    let mut seen_contained = 0usize;
    let mut st = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = || {
        st = st
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        st
    };

    // 10 000 bits: integer 6 spans 60 000..70 000 and crosses 65 536, and
    // `dense(100)` puts integer 655's *first limb* across the same boundary.
    for &width in &[10_000u32, 100, 65_536, 64] {
        let layout = IntLayout::dense(width);
        for k in 0..8u64 {
            let limbs = (width as usize).div_ceil(64);
            let v =
                BigUint::from_limbs_le((0..limbs).map(|_| next()).collect()).truncate(width as u64);
            let mut sink = IntSink::new(layout);
            sink.place(k, &v).expect("in range");
            let set = sink.build();
            let got = set.read_int(k, &layout).expect("addressable");
            assert_eq!(got, v, "width {width}, index {k}");
            assert_eq!(to_ref(&got), to_ref(&v));
            match layout.straddles(k) {
                Some(true) => seen_straddle += 1,
                Some(false) => seen_contained += 1,
                None => {}
            }
        }
    }
    assert!(
        seen_straddle > 0,
        "no placement straddled a chunk boundary, so the seam went untested"
    );
    assert!(seen_contained > 0, "no placement was chunk-contained");
}

/// An unwritten index reads as zero, and the oracle agrees that is what an empty
/// span means. Absence is not `None` — see `bignum::read`'s header.
#[test]
fn an_empty_set_reads_as_zero_everywhere_addressable() {
    let layout = IntLayout::dense(256);
    let set = OrdSet::new();
    for k in [0u64, 1, 7, 1_000] {
        let got = set.read_int(k, &layout).expect("addressable");
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

/// A systematic sweep over **widths**, which the randomized properties above
/// under-cover.
///
/// `a_series_round_trips_through_the_ordset_boundary` derives its width from
/// the generated values' bit lengths, and those come from whole-limb generators —
/// so its widths cluster near multiples of 64. The top-limb mask in `truncate`
/// and the partial-limb handling in the gather are exactly the code that is
/// trivial at a multiple of 64 and interesting three bits either side, so that
/// clustering leaves the interesting case to chance.
///
/// This walks every width in a dense low band plus the chunk boundaries, against
/// four strides and several indices, and checks both the round trip and the
/// narrow-read reduction. Deterministic, so a failure names one triple.
#[test]
fn every_width_round_trips_and_reduces_at_a_narrower_one() {
    let mut st = 0xa5a5_5a5a_c3c3_3c3cu64;
    let mut next = || {
        st = st
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        st
    };

    let mut widths: Vec<u32> = (1..=200).collect();
    // The boundaries the module's own comments call out: a limb, a chunk, and
    // one either side of each.
    widths.extend([255, 256, 257, 511, 512, 513, 1023, 1024, 1025]);
    widths.extend([65_535, 65_536, 65_537]);

    let mut saw_straddle = 0usize;
    let mut saw_partial_limb = 0usize;

    for &w in &widths {
        for pad in [0u64, 1, 7, 64] {
            let layout = IntLayout {
                width_bits: w,
                stride: w as u64 + pad,
            };
            assert!(layout.check().is_ok(), "w={w} pad={pad}");
            if w % 64 != 0 {
                saw_partial_limb += 1;
            }
            for k in [0u64, 1, 6, 7, 655] {
                let v = BigUint::from_limbs_le(
                    (0..(w as usize).div_ceil(64)).map(|_| next()).collect(),
                )
                .truncate(w as u64);

                let mut sink = IntSink::new(layout);
                sink.place(k, &v).expect("value was truncated to the width");
                let set = sink.build();

                let got = set.read_int(k, &layout).expect("addressable");
                assert_eq!(got, v, "round trip: w={w} pad={pad} k={k}");
                assert_eq!(to_ref(&got), to_ref(&v), "oracle: w={w} pad={pad} k={k}");

                if layout.straddles(k) == Some(true) {
                    saw_straddle += 1;
                }

                // Reading narrower is exactly `v mod 2^narrow`, including at
                // widths that are not multiples of 64.
                for nw in [1u32, 63, 64, 65, w.div_ceil(2)] {
                    if nw > w {
                        continue;
                    }
                    let narrow = IntLayout {
                        width_bits: nw,
                        stride: layout.stride,
                    };
                    let g = set.read_int(k, &narrow).expect("addressable");
                    assert_eq!(g, v.truncate(nw as u64), "narrow: w={w} nw={nw} k={k}");
                }
            }
        }
    }

    // Anti-vacuity, in the shape `matrix/seek.rs` uses: without these the
    // sweep could pass having exercised neither of the two things it exists for.
    assert!(saw_straddle > 0, "no placement straddled a chunk boundary");
    assert!(
        saw_partial_limb > 100,
        "only {saw_partial_limb} widths had a partial top limb"
    );
}
