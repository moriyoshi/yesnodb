//! The M0 gate: differential tests against the published `roaring` crate.
//!
//! Two levels, and the second is the one that matters:
//!
//! 1. **Semantic** — our set algebra must agree with `RoaringBitmap` over the
//!    32-bit subrange, and with `BTreeSet` everywhere.
//! 2. **Byte-level** — because we deliberately adopted the spec's payload
//!    encodings, our serialized bytes must be *identical* to the `roaring`
//!    crate's. This catches whole classes of codec bug that oracle testing
//!    misses, and it is what makes `O(container count)` import legitimate.

use std::collections::BTreeSet;

use yesno_core::roaring_format::{deserialize_u64, serialize_u64, Roaring32};
use yesno_core::OrdSet;

/// Deterministic pseudo-random values — no dev-dependency on a RNG crate.
fn lcg(seed: u64) -> impl FnMut() -> u64 {
    let mut s = seed;
    move || {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        s >> 11
    }
}

fn sorted_unique_u32(n: usize, modulus: u32, seed: u64) -> Vec<u32> {
    let mut r = lcg(seed);
    let mut v: Vec<u32> = (0..n).map(|_| (r() % modulus as u64) as u32).collect();
    v.sort_unstable();
    v.dedup();
    v
}

// ---------------------------------------------------------------- byte level

#[test]
fn serialized_bytes_are_identical_to_the_roaring_crate() {
    // Shapes chosen to exercise array containers, bitmap containers, multiple
    // container keys, and the container-count thresholds around the offset rule.
    let cases: Vec<Vec<u32>> = vec![
        vec![],
        vec![0],
        vec![1, 2, 3],
        vec![0, 65535, 65536, 131072],
        (0..5000u32).collect(),               // forces a bitmap container
        (0..3u32).map(|i| i << 16).collect(), // 3 containers: no offsets
        (0..4u32).map(|i| i << 16).collect(), // 4 containers: offsets
        (0..10u32).map(|i| i << 16).collect(),
        sorted_unique_u32(20_000, 1 << 20, 0xABCD),
    ];

    for vals in cases {
        // Neither side run-optimized: this pins the SERIAL_COOKIE_NO_RUNCONTAINER
        // path, where the offset array is always present. The run path is covered
        // by `run_encoded_bytes_are_identical_to_the_roaring_crate`, which
        // optimizes *both* sides.
        let ours = Roaring32::from_sorted_u32(&vals).serialize();

        let theirs_bm: roaring::RoaringBitmap = vals.iter().copied().collect();
        let mut theirs = Vec::new();
        theirs_bm.serialize_into(&mut theirs).unwrap();

        assert_eq!(
            ours,
            theirs,
            "byte mismatch for {} values (first few: {:?})",
            vals.len(),
            &vals[..vals.len().min(5)]
        );
    }
}

#[test]
fn run_encoded_bytes_are_identical_to_the_roaring_crate() {
    // Runny data is where the `nruns` prefix and the offset-header rule bite.
    // Small container counts (< 4) deliberately included: those carry no offsets.
    let cases: Vec<Vec<u32>> = vec![
        (0..5000u32).collect(),
        (0..70_000u32).collect(),
        (0..3u32)
            .flat_map(|i| (i << 16)..(i << 16) + 1000)
            .collect(),
        (0..8u32)
            .flat_map(|i| (i << 16)..(i << 16) + 5000)
            .collect(),
    ];

    for vals in cases {
        let mut ours_bm = Roaring32::from_sorted_u32(&vals);
        ours_bm.optimize();
        let ours = ours_bm.serialize();

        let mut theirs_bm: roaring::RoaringBitmap = vals.iter().copied().collect();
        theirs_bm.optimize();
        let mut theirs = Vec::new();
        theirs_bm.serialize_into(&mut theirs).unwrap();

        assert_eq!(
            ours,
            theirs,
            "run-encoded byte mismatch for {} values",
            vals.len()
        );
    }
}

#[test]
fn we_can_parse_what_the_roaring_crate_writes() {
    for &n in &[0usize, 1, 100, 5000, 70_000] {
        let vals = sorted_unique_u32(n, 1 << 22, n as u64 + 7);
        for optimize in [false, true] {
            let mut bm: roaring::RoaringBitmap = vals.iter().copied().collect();
            if optimize {
                bm.optimize();
            }
            let mut bytes = Vec::new();
            bm.serialize_into(&mut bytes).unwrap();

            let parsed = Roaring32::deserialize(&bytes).unwrap_or_else(|e| {
                panic!("failed to parse roaring output (n={n}, opt={optimize}): {e}")
            });
            assert_eq!(parsed.values(), vals, "n={n}, optimize={optimize}");
        }
    }
}

#[test]
fn the_roaring_crate_can_parse_what_we_write() {
    for &n in &[0usize, 1, 100, 5000, 70_000] {
        let vals = sorted_unique_u32(n, 1 << 22, n as u64 + 11);
        for optimize in [false, true] {
            let mut ours = Roaring32::from_sorted_u32(&vals);
            if optimize {
                ours.optimize();
            }
            let bytes = ours.serialize();
            let theirs = roaring::RoaringBitmap::deserialize_from(&bytes[..]).unwrap_or_else(|e| {
                panic!("roaring rejected our bytes (n={n}, opt={optimize}): {e}")
            });
            assert_eq!(theirs.iter().collect::<Vec<u32>>(), vals);
        }
    }
}

// ----------------------------------------------------------------- semantic

#[test]
fn set_algebra_agrees_with_roaring_over_the_32_bit_subrange() {
    let av = sorted_unique_u32(30_000, 1 << 20, 1);
    let bv = sorted_unique_u32(30_000, 1 << 20, 2);

    let a: OrdSet = av.iter().map(|&v| v as u64).collect();
    let b: OrdSet = bv.iter().map(|&v| v as u64).collect();
    let ra: roaring::RoaringBitmap = av.iter().copied().collect();
    let rb: roaring::RoaringBitmap = bv.iter().copied().collect();

    let as_u64 = |r: roaring::RoaringBitmap| r.iter().map(|v| v as u64).collect::<Vec<u64>>();

    assert_eq!(a.and(&b).iter().collect::<Vec<_>>(), as_u64(&ra & &rb));
    assert_eq!(a.or(&b).iter().collect::<Vec<_>>(), as_u64(&ra | &rb));
    assert_eq!(a.xor(&b).iter().collect::<Vec<_>>(), as_u64(&ra ^ &rb));
    assert_eq!(a.and_not(&b).iter().collect::<Vec<_>>(), as_u64(&ra - &rb));

    // Cardinality identities must agree without materializing.
    assert_eq!(a.and_cardinality(&b), (&ra & &rb).len());
    assert_eq!(a.or_cardinality(&b), (&ra | &rb).len());
    assert_eq!(a.xor_cardinality(&b), (&ra ^ &rb).len());
    assert_eq!(a.andnot_cardinality(&b), (&ra - &rb).len());
}

#[test]
fn full_u64_range_agrees_with_roaring_treemap() {
    let mut r = lcg(99);
    let mut vals: Vec<u64> = (0..20_000).map(|_| r()).collect();
    // Force some clustering so chunks are shared across the two sets.
    vals.extend((0..5000u64).map(|i| i * 3));
    vals.sort_unstable();
    vals.dedup();

    let ours = OrdSet::from_sorted_slice(&vals);
    let theirs: roaring::RoaringTreemap = vals.iter().copied().collect();

    assert_eq!(ours.len(), theirs.len());
    assert_eq!(
        ours.iter().collect::<Vec<u64>>(),
        theirs.iter().collect::<Vec<u64>>()
    );
    assert_eq!(ours.min(), theirs.min());
    assert_eq!(ours.max(), theirs.max());
}

// ------------------------------------------------- boundary-biased oracle

/// The cardinalities where representation changes and off-by-ones live.
const BOUNDARY_CARDS: [usize; 10] = [0, 1, 2, 3583, 3584, 4095, 4096, 4097, 65535, 65536];

#[test]
fn boundary_cardinalities_roundtrip_and_match_oracle() {
    for &card in &BOUNDARY_CARDS {
        let vals: Vec<u64> = (0..card as u64).collect();
        let oracle: BTreeSet<u64> = vals.iter().copied().collect();

        let mut s = OrdSet::from_sorted_slice(&vals);
        assert_eq!(s.len(), card as u64, "card {card}");
        assert_eq!(s.iter().collect::<Vec<_>>(), vals, "card {card}");

        // Optimization must not change contents at any boundary.
        s.optimize();
        assert_eq!(
            s.iter().collect::<Vec<_>>(),
            vals,
            "after optimize, card {card}"
        );
        assert_eq!(s.len(), oracle.len() as u64);

        // Serialization must survive every boundary too.
        let back = deserialize_u64(&serialize_u64(&s)).unwrap();
        assert_eq!(
            back.iter().collect::<Vec<_>>(),
            vals,
            "after roundtrip, card {card}"
        );
    }
}

#[test]
fn prefix_patterns_exercise_the_merge_join() {
    // Uniform random data essentially never produces interesting prefix overlap,
    // so these shapes are constructed explicitly.
    let identical: Vec<u64> = (0..1000u64).collect();
    let disjoint_a: Vec<u64> = (0..1000u64).collect();
    let disjoint_b: Vec<u64> = (1u64 << 40..(1u64 << 40) + 1000).collect();
    let interleaved_a: Vec<u64> = (0..500u64).map(|i| i * (1 << 16) * 2).collect();
    let interleaved_b: Vec<u64> = (0..500u64).map(|i| (i * 2 + 1) * (1 << 16)).collect();
    let sparse: Vec<u64> = vec![7, 1 << 30, 1 << 45];
    let dense: Vec<u64> = (0..100_000u64).collect();

    let cases: Vec<(&str, Vec<u64>, Vec<u64>)> = vec![
        ("identical", identical.clone(), identical),
        ("disjoint", disjoint_a, disjoint_b),
        ("interleaved", interleaved_a, interleaved_b),
        ("sparse-vs-dense", sparse, dense),
    ];

    for (name, av, bv) in cases {
        let (a, b) = (
            OrdSet::from_sorted_slice(&av),
            OrdSet::from_sorted_slice(&bv),
        );
        let (sa, sb): (BTreeSet<u64>, BTreeSet<u64>) =
            (av.iter().copied().collect(), bv.iter().copied().collect());

        assert_eq!(
            a.and(&b).iter().collect::<Vec<_>>(),
            sa.intersection(&sb).copied().collect::<Vec<_>>(),
            "{name}: AND"
        );
        assert_eq!(
            a.or(&b).iter().collect::<Vec<_>>(),
            sa.union(&sb).copied().collect::<Vec<_>>(),
            "{name}: OR"
        );
        assert_eq!(
            a.xor(&b).iter().collect::<Vec<_>>(),
            sa.symmetric_difference(&sb).copied().collect::<Vec<_>>(),
            "{name}: XOR"
        );
        assert_eq!(
            a.and_not(&b).iter().collect::<Vec<_>>(),
            sa.difference(&sb).copied().collect::<Vec<_>>(),
            "{name}: ANDNOT"
        );
        // The non-materializing path must agree with the materializing one.
        assert_eq!(
            a.and_cardinality(&b),
            a.and(&b).len(),
            "{name}: and_cardinality"
        );
        assert_eq!(
            a.or_cardinality(&b),
            a.or(&b).len(),
            "{name}: or_cardinality"
        );
    }
}

#[test]
fn runny_generator_exercises_run_containers() {
    // Random data never produces runs, so the run kernels would otherwise go
    // untested. Build explicitly runny sets and verify through optimize().
    let mut r = lcg(4242);
    let mut vals: Vec<u64> = Vec::new();
    let mut cursor = 0u64;
    for _ in 0..200 {
        let gap = r() % 50;
        let run = 1 + r() % 400;
        cursor += gap;
        vals.extend(cursor..cursor + run);
        cursor += run;
    }
    vals.sort_unstable();
    vals.dedup();

    let mut s = OrdSet::from_sorted_slice(&vals);
    s.optimize();
    assert_eq!(s.iter().collect::<Vec<_>>(), vals);

    // Ops between two runny sets, against the oracle.
    let shifted: Vec<u64> = vals.iter().map(|v| v + 100).collect();
    let mut t = OrdSet::from_sorted_slice(&shifted);
    t.optimize();

    let (sa, sb): (BTreeSet<u64>, BTreeSet<u64>) = (
        vals.iter().copied().collect(),
        shifted.iter().copied().collect(),
    );
    assert_eq!(
        s.and(&t).iter().collect::<Vec<_>>(),
        sa.intersection(&sb).copied().collect::<Vec<_>>()
    );
    assert_eq!(
        s.xor(&t).iter().collect::<Vec<_>>(),
        sa.symmetric_difference(&sb).copied().collect::<Vec<_>>()
    );

    // And the bytes must still match the roaring crate after optimization.
    let back = deserialize_u64(&serialize_u64(&s)).unwrap();
    assert_eq!(back.iter().collect::<Vec<_>>(), vals);
}

/// Sets built by **removal from promoted chunks**, round-tripped both ways.
///
/// # The generator gap this closes
///
/// Every other generator in this file builds by insertion, through
/// `Roaring32::from_sorted_u32`, which assigns each container its canonical kind
/// from its final cardinality. So the byte-identity gate — the layer whose whole
/// job is to catch encoding defects — could not construct the one state that
/// breaks the encoding: a container whose kind disagrees with its cardinality.
/// Kind is a function of *history* here ( `BITMAP_DEMOTE` is 3584, and `remove`
/// never demotes ), and no generator had any history.
///
/// This one promotes chunks past `ARRAY_MAX`, removes a pseudo-random subset to
/// land at cardinalities on both sides of the hysteresis band, and asserts on
/// **contents**. Cardinality alone is not enough: a misparse at `card == 1`
/// returns one element, just not the right one.
#[test]
fn sets_shrunk_from_bitmaps_survive_both_round_trips() {
    let mut rng = lcg(0xC0FFEE);
    for chunk in [0u64, 1, 7] {
        for target in [1usize, 2, 1000, 3583, 3584, 4095, 4096, 4097, 5000] {
            let base = chunk << 16;
            // 6000 > ARRAY_MAX, so every chunk starts as a Bitmap.
            let mut s =
                OrdSet::from_sorted_slice(&(0..6000u64).map(|i| base + i).collect::<Vec<_>>());
            let mut live: Vec<u64> = (0..6000u64).map(|i| base + i).collect();
            while live.len() > target {
                let i = (rng() as usize) % live.len();
                let v = live.swap_remove(i);
                s.remove(v);
            }
            live.sort_unstable();
            assert_eq!(s.iter().collect::<Vec<_>>(), live, "fixture mismatch");

            // 64-bit path.
            let back = deserialize_u64(&serialize_u64(&s)).unwrap_or_else(|e| {
                panic!("chunk {chunk} card {target}: u64 export unreadable: {e}")
            });
            assert_eq!(
                back.iter().collect::<Vec<_>>(),
                live,
                "chunk {chunk} card {target}: u64"
            );

            // 32-bit path, checked against the `roaring` crate, which infers
            // kind from cardinality exactly as we do.
            let bytes = serialize_32(&s);
            let r = roaring::RoaringBitmap::deserialize_from(&bytes[..]).unwrap_or_else(|e| {
                panic!("chunk {chunk} card {target}: roaring rejected our export: {e}")
            });
            assert_eq!(
                r.iter().map(u64::from).collect::<Vec<_>>(),
                live,
                "chunk {chunk} card {target}: roaring read different contents"
            );
        }
    }
}

/// A chunk that was promoted to `Bitmap` and then shrunk must export as an
/// `Array` if its cardinality has fallen to `ARRAY_MAX` or below.
///
/// # Why this is a format bug and not a tuning question
///
/// The portable Roaring format carries **no kind field** for non-run containers.
/// A reader infers array-vs-bitset purely from the descriptive header's
/// cardinality against `ARRAY_MAX` — ours does, the `roaring` crate does, and
/// CRoaring does. But `BITMAP_DEMOTE = 3584` deliberately makes our in-memory
/// kind a function of *history*, so kind is not recoverable from cardinality.
///
/// A retained `Bitmap` with `card <= 4096` therefore wrote an 8192-byte payload
/// beneath a header promising `2 * card` bytes, and the reader resynchronised on
/// the wrong boundary.
///
/// **Asserted on contents, not cardinality.** At `card == 1` the misparse
/// happens to produce a well-formed one-element array, so the count is right and
/// the value is wrong — a cardinality assertion passes while the data is
/// corrupt.
///
/// The exposed region is `kind == Bitmap && card <= ARRAY_MAX`, which is wider
/// than the hysteresis band, because `remove` does not demote on this path at
/// all.
#[test]
fn a_shrunk_bitmap_exports_as_an_array() {
    for target in [1usize, 100, 1000, 3583, 3600, 4096, 4097] {
        // Promote to Bitmap, then shrink by removing from the front.
        let mut s = OrdSet::from_sorted_slice(&(0..5000u64).collect::<Vec<_>>());
        assert_eq!(
            s.chunk_at(0).unwrap().1.kind(),
            yesno_core::ContainerKind::Bitmap,
            "the fixture must start as a Bitmap or it tests nothing"
        );
        for v in 0..(5000 - target as u64) {
            s.remove(v);
        }
        assert_eq!(s.len(), target as u64);

        let expected: Vec<u64> = s.iter().collect();
        let bytes = serialize_u64(&s);
        let back = deserialize_u64(&bytes)
            .unwrap_or_else(|e| panic!("card {target}: export could not be read back: {e}"));

        assert_eq!(
            back.iter().collect::<Vec<_>>(),
            expected,
            "card {target}: round trip changed the contents"
        );

        // And the `roaring` crate must agree, since it infers kind the same way.
        let r =
            roaring::RoaringBitmap::deserialize_from(&serialize_32(&s)[..]).unwrap_or_else(|e| {
                panic!("card {target}: the roaring crate rejected our export: {e}")
            });
        assert_eq!(
            r.iter().map(u64::from).collect::<Vec<_>>(),
            expected,
            "card {target}: the roaring crate read different contents"
        );
    }
}

/// 32-bit serialization of a set known to live below 2^32, **preserving the
/// containers' actual kinds**.
///
/// Not `Roaring32::from_sorted_u32`. That rebuilds every container in its
/// canonical kind, which silently repairs the very defect under test — and it is
/// how the whole M0 layer missed this: every generator here builds by insertion
/// through `from_sorted_u32`, so a retained `Bitmap` below `ARRAY_MAX` was never
/// once handed to `serialize`.
fn serialize_32(s: &OrdSet) -> Vec<u8> {
    Roaring32 {
        containers: s.chunks().map(|(p, c)| (p as u16, c.clone())).collect(),
    }
    .serialize()
}
