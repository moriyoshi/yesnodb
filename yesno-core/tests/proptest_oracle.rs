//! Randomized property tests against a `BTreeSet<u64>` oracle.
//!
//! Generators are **boundary-biased on purpose**. Uniform random `u64`s would
//! put one ordinal in each chunk, so array containers would never fill, bitmaps
//! would never appear, and run containers would never be produced at all — the
//! interesting code would go untested while the suite stayed green.

use std::collections::BTreeSet;

use proptest::prelude::*;
use yesno_core::container::codec;
use yesno_core::roaring_format::{deserialize_u64, serialize_u64};
use yesno_core::view::View;
use yesno_core::{ContainerKind, OrdSet, RangeSummary, ORDINAL_MAX};

/// Ordinals clustered into a handful of chunks, so containers actually fill up.
fn clustered_ordinals() -> impl Strategy<Value = Vec<u64>> {
    let chunk = prop::sample::select(vec![0u64, 1, 2, 7, 1 << 20, (1u64 << 48) - 1]);
    let dense = prop::collection::vec(0u16..600, 0..600);
    prop::collection::vec((chunk, dense), 0..4).prop_map(|groups| {
        let mut v: Vec<u64> = groups
            .into_iter()
            .flat_map(|(c, lows)| lows.into_iter().map(move |l| (c << 16) | l as u64))
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    })
}

/// Contiguous stretches, which is the only way run containers get exercised.
/// Values for a **single container**, weighted to reach all three encodings.
///
/// Uniform `u16`s give an array every time, and the first version of this
/// produced a bitmap in 3 of 258 draws and never a bitmap×bitmap pair — so the
/// arms it was written to cover were barely exercised. The dense arms are
/// explicitly sized past `ARRAY_MAX` with a stride that defeats run-optimizing,
/// and weighted up, because a generator that cannot reach a kind cannot test it.
///
/// Check the distribution rather than trusting it: printing the `(kind, kind)`
/// pair per case is how the gap was found.
fn container_values() -> impl Strategy<Value = Vec<u16>> {
    prop_oneof![
        // Sparse: arrays.
        2 => prop::collection::vec(any::<u16>(), 0..64),
        // Dense with a stride, so it exceeds ARRAY_MAX *and* stays a bitmap
        // rather than collapsing into runs.
        4 => (4200u16..5000, 7u16..13).prop_map(|(n, k)| {
            (0..n).map(|i| i.wrapping_mul(k)).collect::<Vec<u16>>()
        }),
        // One long stretch: runs.
        2 => (0u16..40000, 1u16..8000).prop_map(|(s, l)| {
            (s..=s.saturating_add(l)).collect::<Vec<u16>>()
        }),
        // Several stretches, so run containers have more than one interval.
        2 => prop::collection::vec((0u16..60000, 1u16..500), 1..8).prop_map(|spans| {
            spans
                .into_iter()
                .flat_map(|(s, l)| (s..=s.saturating_add(l)).collect::<Vec<u16>>())
                .collect::<Vec<u16>>()
        }),
        // The ends of the chunk, where masks and interval arithmetic break.
        1 => prop::collection::vec(
            prop_oneof![Just(0u16), Just(1), Just(63), Just(64), Just(65534), Just(65535)],
            1..6,
        ),
    ]
    .prop_map(|mut v| {
        v.sort_unstable();
        v.dedup();
        v
    })
}

fn runny_ordinals() -> impl Strategy<Value = Vec<u64>> {
    prop::collection::vec((0u64..3000, 1u64..400), 0..40).prop_map(|spans| {
        let mut v: Vec<u64> = Vec::new();
        let mut cursor = 0u64;
        for (gap, run) in spans {
            cursor += gap;
            v.extend(cursor..cursor + run);
            cursor += run;
        }
        v.sort_unstable();
        v.dedup();
        v
    })
}

/// Ordinals at the top of the address space, and around the boundaries nearest
/// it.
///
/// The generators above are boundary-biased on *cardinality* and *prefix
/// pattern*, exactly as their comments promise — and the largest ordinal any of
/// them can emit is nowhere near `u64::MAX`. That read as "boundary-biased" in
/// general for four milestones, and it is how an arithmetic overflow in the
/// range walk ( `o = stop + 1` before the `stop == u64::MAX` break ) survived
/// in `Memtable::{insert_range, remove_range}` until an end-to-end scenario
/// happened to insert the last ordinal.
///
/// `1 << 63` and `i64::MAX` are here as well as the very top, because a signed
/// shift anywhere on the path folds the upper half of the space onto the lower
/// and only shows up on one side of that line.
fn ceiling_ordinals() -> impl Strategy<Value = Vec<u64>> {
    let anchors = prop::sample::select(vec![
        0u64,
        1,
        u16::MAX as u64,
        1 << 16,
        i64::MAX as u64,
        1u64 << 63,
        ORDINAL_MAX - (1 << 16),
        ORDINAL_MAX - 1,
        ORDINAL_MAX,
    ]);
    prop::collection::vec((anchors, 0u64..48), 1..12).prop_map(|pairs| {
        // Clamped to `ORDINAL_MAX`, not `u64::MAX`: under I8 the top value is
        // reserved and inserting it trips a `debug_assert`. Saturating at the
        // real ceiling keeps the generator pressed against the boundary it
        // exists to probe.
        let mut v: Vec<u64> = pairs
            .into_iter()
            .flat_map(|(a, d)| [a.saturating_sub(d), a.saturating_add(d).min(ORDINAL_MAX)])
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    })
}

/// A `[lo, hi]` range that can straddle any boundary, including the last
/// ordinal, while staying narrow enough for a `BTreeSet` oracle to enumerate.
///
/// Width has to be bounded and position must not be: a uniform `(lo, hi)` pair
/// over `u64` is almost always a span no oracle can materialize, which is why
/// there was no u64-level range property at all.
fn ceiling_range() -> impl Strategy<Value = (u64, u64)> {
    let anchors = prop::sample::select(vec![
        0u64,
        1,
        u16::MAX as u64,
        1 << 16,
        (1 << 16) + 1,
        1 << 20,
        i64::MAX as u64,
        1u64 << 63,
        ORDINAL_MAX - (1 << 16),
        ORDINAL_MAX - 1,
        ORDINAL_MAX,
    ]);
    // `hi` is *inclusive* here, so it clamps to `ORDINAL_MAX` — the exclusive
    // `hi` of `bounded_range` below clamps to `u64::MAX` instead. Under I8 those
    // two ceilings name the same last ordinal.
    (anchors, 0u64..96, 0u64..96).prop_map(|(a, back, fwd)| {
        (
            a.saturating_sub(back),
            a.saturating_add(fwd).min(ORDINAL_MAX),
        )
    })
}

fn any_ordinals() -> impl Strategy<Value = Vec<u64>> {
    prop_oneof![
        clustered_ordinals(),
        runny_ordinals(),
        ceiling_ordinals(),
        prop::collection::vec(any::<u64>(), 0..200).prop_map(|mut v| {
            v.sort_unstable();
            v.dedup();
            v
        }),
    ]
}

/// Every structural invariant a stored container must satisfy.
fn assert_invariants(s: &OrdSet) {
    let mut prev: Option<u64> = None;
    let mut total = 0u64;
    for (p, c) in s.chunks() {
        assert!(p < (1u64 << 48), "prefix {p} exceeds 48 bits");
        if let Some(pv) = prev {
            assert!(pv < p, "chunk prefixes must be strictly ascending");
        }
        prev = Some(p);
        assert!(!c.is_empty(), "an empty container must never be stored");
        codec::validate(c).unwrap_or_else(|e| panic!("container at prefix {p} invalid: {e}"));
        total += c.len() as u64;
    }
    assert_eq!(
        total,
        s.len(),
        "cached cardinality must equal the sum of containers"
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn contents_match_btreeset_oracle(vals in any_ordinals()) {
        let oracle: BTreeSet<u64> = vals.iter().copied().collect();
        let s = OrdSet::from_iter_unsorted(vals.iter().copied());
        assert_invariants(&s);
        prop_assert_eq!(s.len(), oracle.len() as u64);
        prop_assert_eq!(s.iter().collect::<Vec<_>>(), oracle.iter().copied().collect::<Vec<_>>());
    }

    #[test]
    fn optimize_is_content_preserving(vals in any_ordinals()) {
        let mut s = OrdSet::from_iter_unsorted(vals.iter().copied());
        let before: Vec<u64> = s.iter().collect();
        s.optimize();
        assert_invariants(&s);
        prop_assert_eq!(s.iter().collect::<Vec<_>>(), before);
    }

    #[test]
    fn set_ops_match_oracle(a in any_ordinals(), b in any_ordinals()) {
        let (sa, sb): (BTreeSet<u64>, BTreeSet<u64>) =
            (a.iter().copied().collect(), b.iter().copied().collect());
        let (x, y) = (OrdSet::from_iter_unsorted(a), OrdSet::from_iter_unsorted(b));

        let and = x.and(&y);
        let or = x.or(&y);
        let xor = x.xor(&y);
        let andnot = x.and_not(&y);
        for r in [&and, &or, &xor, &andnot] {
            assert_invariants(r);
        }

        prop_assert_eq!(and.iter().collect::<Vec<_>>(),
                        sa.intersection(&sb).copied().collect::<Vec<_>>());
        prop_assert_eq!(or.iter().collect::<Vec<_>>(),
                        sa.union(&sb).copied().collect::<Vec<_>>());
        prop_assert_eq!(xor.iter().collect::<Vec<_>>(),
                        sa.symmetric_difference(&sb).copied().collect::<Vec<_>>());
        prop_assert_eq!(andnot.iter().collect::<Vec<_>>(),
                        sa.difference(&sb).copied().collect::<Vec<_>>());
    }

    /// The non-materializing cardinality paths are a parallel implementation of
    /// the materializing ones, so they need a differential test of their own —
    /// this is the one that catches a broken identity.
    #[test]
    fn cardinality_paths_agree_with_materialized(a in any_ordinals(), b in any_ordinals()) {
        let (x, y) = (OrdSet::from_iter_unsorted(a), OrdSet::from_iter_unsorted(b));
        prop_assert_eq!(x.and_cardinality(&y), x.and(&y).len());
        prop_assert_eq!(x.or_cardinality(&y), x.or(&y).len());
        prop_assert_eq!(x.xor_cardinality(&y), x.xor(&y).len());
        prop_assert_eq!(x.andnot_cardinality(&y), x.and_not(&y).len());
        prop_assert_eq!(x.is_disjoint(&y), x.and(&y).is_empty());
    }

    #[test]
    fn serialization_roundtrips(vals in any_ordinals()) {
        let mut s = OrdSet::from_iter_unsorted(vals.iter().copied());
        s.optimize();
        let back = deserialize_u64(&serialize_u64(&s)).expect("roundtrip must parse");
        assert_invariants(&back);
        prop_assert_eq!(back.iter().collect::<Vec<_>>(), s.iter().collect::<Vec<_>>());
    }

    #[test]
    fn incremental_mutation_tracks_the_oracle(
        vals in any_ordinals(),
        removals in prop::collection::vec(any::<u64>(), 0..50),
    ) {
        let mut oracle: BTreeSet<u64> = BTreeSet::new();
        let mut s = OrdSet::new();
        for &v in &vals {
            prop_assert_eq!(s.insert(v), oracle.insert(v));
        }
        // Remove a mix of present and absent ordinals.
        for &v in vals.iter().take(20).chain(removals.iter()) {
            prop_assert_eq!(s.remove(v), oracle.remove(&v));
        }
        assert_invariants(&s);
        prop_assert_eq!(s.len(), oracle.len() as u64);
        prop_assert_eq!(s.iter().collect::<Vec<_>>(), oracle.iter().copied().collect::<Vec<_>>());
    }

    #[test]
    fn rank_select_are_mutually_inverse(vals in any_ordinals()) {
        let s = OrdSet::from_iter_unsorted(vals.iter().copied());
        let sorted: Vec<u64> = s.iter().collect();
        for (i, &v) in sorted.iter().enumerate() {
            prop_assert_eq!(s.select(i as u64), Some(v));
            prop_assert_eq!(s.rank(v), i as u64);
        }
        prop_assert_eq!(s.select(sorted.len() as u64), None);
    }

    /// **Every specialized kernel must be indistinguishable from the oracle.**
    ///
    /// `ops::apply` dispatches to a specialized arm where one exists and falls
    /// through to `ops::generic` otherwise, and as of 2026-08-25 all nine
    /// kind-pairs are specialized. Each has hand-written cases in its own
    /// module; this is the randomized version, and it is the layer that would
    /// catch an interval boundary or a word mask nobody thought to try.
    ///
    /// Both operand orders, because `AndNot` is asymmetric and a kernel that
    /// quietly normalized the pair would return the complement rather than fail.
    #[test]
    fn every_specialized_kernel_matches_the_generic_oracle(
        av in container_values(),
        bv in container_values(),
    ) {
        use yesno_core::container::Container;
        use yesno_core::ops::{self, SetOp};

        // `from_sorted` picks the encoding, and `optimize` may switch it, so the
        // pair that actually reaches the kernels covers whatever kinds these
        // values imply — which is the point of the boundary-biased generators.
        let mut a = Container::from_sorted(&av);
        let mut b = Container::from_sorted(&bv);
        a.optimize();
        b.optimize();

        for op in [SetOp::And, SetOp::Or, SetOp::Xor, SetOp::AndNot] {
            for (x, y) in [(&a, &b), (&b, &a)] {
                let fast = ops::apply(op, x, y);
                let slow = ops::generic::apply(op, x, y);

                let f: Vec<u16> = fast.as_ref().map(|c| c.iter().collect()).unwrap_or_default();
                let s: Vec<u16> = slow.as_ref().map(|c| c.iter().collect()).unwrap_or_default();
                prop_assert_eq!(
                    &f, &s,
                    "{:?} on {:?} x {:?} disagrees with the oracle",
                    op, x.kind(), y.kind()
                );

                // The cached length is a separate claim from the contents, and
                // a fused popcount is easy to get subtly wrong.
                prop_assert_eq!(
                    fast.as_ref().map(|c| c.len()),
                    slow.as_ref().map(|c| c.len()),
                    "{:?} on {:?} x {:?}: cardinality disagrees",
                    op, x.kind(), y.kind()
                );

                // An empty result is `None`, never an empty container, and a
                // non-empty one must satisfy its kind's invariants.
                if let Some(c) = fast.as_ref() {
                    prop_assert!(!c.is_empty(), "an empty container was returned");
                    prop_assert!(codec::validate(c).is_ok(), "invalid container: {:?}", c.kind());
                }
            }
        }
    }

    /// The cardinality identities must agree with materializing, for every pair.
    ///
    /// `ops::card` is a **parallel implementation** of the same joins that
    /// `ops::apply` performs, written to avoid allocating a result. Correctness
    /// tests cannot tell a correct fast path from a correct slow one, and the
    /// four mixed arms here were specialized on 2026-08-25 with only hand-picked
    /// cases — so this is the layer that would catch a mis-summed interval or a
    /// popcount under the wrong mask.
    ///
    /// All four identities are derived from `and_cardinality`, so an error in it
    /// shows up four times over; asserting each separately says *which*.
    #[test]
    fn cardinality_identities_agree_for_every_kind_pair(
        av in container_values(),
        bv in container_values(),
    ) {
        use yesno_core::container::Container;
        use yesno_core::ops::{self, SetOp};

        let mut a = Container::from_sorted(&av);
        let mut b = Container::from_sorted(&bv);
        a.optimize();
        b.optimize();

        for (x, y) in [(&a, &b), (&b, &a)] {
            let materialize = |op| {
                ops::apply(op, x, y).map(|c| c.len()).unwrap_or(0)
            };
            prop_assert_eq!(
                ops::and_cardinality(x, y), materialize(SetOp::And),
                "and_cardinality on {:?} x {:?}", x.kind(), y.kind()
            );
            prop_assert_eq!(
                ops::or_cardinality(x, y), materialize(SetOp::Or),
                "or_cardinality on {:?} x {:?}", x.kind(), y.kind()
            );
            prop_assert_eq!(
                ops::xor_cardinality(x, y), materialize(SetOp::Xor),
                "xor_cardinality on {:?} x {:?}", x.kind(), y.kind()
            );
            prop_assert_eq!(
                ops::andnot_cardinality(x, y), materialize(SetOp::AndNot),
                "andnot_cardinality on {:?} x {:?}", x.kind(), y.kind()
            );
            // The predicates are answered from the same counts.
            prop_assert_eq!(
                ops::is_disjoint(x, y), ops::and_cardinality(x, y) == 0,
                "is_disjoint on {:?} x {:?}", x.kind(), y.kind()
            );
            prop_assert_eq!(
                ops::contains_all(x, y), ops::and_cardinality(x, y) == y.len(),
                "contains_all on {:?} x {:?}", x.kind(), y.kind()
            );
        }
    }

    /// A range mutation must equal the same values applied one at a time.
    ///
    /// `insert_range` / `remove_range` are a **second implementation** of what
    /// `insert` / `remove` already do, existing purely so a bulk span costs one
    /// container call per chunk instead of one per ordinal. Nothing about the
    /// result may differ, which is exactly why only a differential property
    /// catches a mistake in them — and the array arm promotes to a bitmap on a
    /// conservative *upper bound*, so the two paths genuinely can end up in
    /// different representations of the same set.
    #[test]
    fn a_range_mutation_equals_the_same_values_one_at_a_time(
        base in container_values(),
        a in 0u16..=u16::MAX,
        b in 0u16..=u16::MAX,
        removing in proptest::bool::ANY,
    ) {
        use yesno_core::container::Container;
        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };

        let mut ranged = Container::from_sorted(&base);
        let mut looped = Container::from_sorted(&base);
        ranged.optimize();
        looped.optimize();

        let n = if removing {
            let n = ranged.remove_range(lo, hi);
            for v in lo..=hi {
                looped.remove(v);
            }
            n
        } else {
            let n = ranged.insert_range(lo, hi);
            for v in lo..=hi {
                looped.insert(v);
            }
            n
        };

        let got: Vec<u16> = ranged.iter().collect();
        let want: Vec<u16> = looped.iter().collect();
        prop_assert_eq!(&got, &want, "range vs per-value disagree on [{}, {}]", lo, hi);
        prop_assert_eq!(ranged.len(), want.len() as u32, "cached cardinality is stale");

        // The reported delta must be the actual change, not the span.
        let before = Container::from_sorted(&base).len();
        let delta = if removing { before - ranged.len() } else { ranged.len() - before };
        prop_assert_eq!(n, delta, "returned change count disagrees with the container");

        // Whatever representation it chose must still be a legal container —
        // unless the removal emptied it, which is legal in hand and becomes a
        // tombstone rather than a stored container ( `Memtable::remove_range` ).
        if !ranged.is_empty() {
            yesno_core::container::codec::validate(&ranged).map_err(|e| {
                TestCaseError::fail(format!("range produced an invalid container: {e:?}"))
            })?;
        }
    }

    /// A `u64` range mutation must equal doing the same values one at a time,
    /// **including at the top of the address space**.
    ///
    /// The container-level property above covers `[lo, hi]` within one chunk,
    /// across the whole `u16` span. What it cannot reach is the *multi-chunk
    /// walk* in `Memtable::{insert_range, remove_range}`, which advances a
    /// cursor chunk by chunk — and that is where the overflow was: `o = stop +
    /// 1` evaluated before the `stop == u64::MAX` break, so a range touching
    /// the last ordinal panicked in debug. Release builds were unaffected, the
    /// wrapped value being unread, which is why only a debug-build test can see
    /// it and why no test did.
    ///
    /// The oracle is a `BTreeSet`, so the range has to stay narrow; `ceiling_range`
    /// bounds the width without bounding the position.
    #[test]
    fn a_u64_range_mutation_equals_the_same_values_one_at_a_time(
        base in ceiling_ordinals(),
        (lo, hi) in ceiling_range(),
        removing in proptest::bool::ANY,
    ) {
        use yesno_core::Db;

        let db = Db::new();
        db.insert_many(7, &base).unwrap();

        let mut want: BTreeSet<u64> = base.iter().copied().collect();
        let n = if removing {
            let n = db.remove_range(7, lo, hi).unwrap();
            want.retain(|v| *v < lo || *v > hi);
            n
        } else {
            let n = db.insert_range(7, lo, hi).unwrap();
            // Inclusive on both ends, and `hi` may be `u64::MAX`, so this
            // cannot be written as `lo..=hi` folded into a `u64` counter
            // without the same overflow the subject has.
            let mut v = lo;
            loop {
                want.insert(v);
                if v == hi {
                    break;
                }
                v += 1;
            }
            n
        };

        let snap = db.snapshot().unwrap();
        let got: BTreeSet<u64> = snap.load(7).unwrap().iter().collect();
        prop_assert_eq!(&got, &want, "range [{}, {}] removing={}", lo, hi, removing);

        // The reported delta must be the actual change, not the span.
        let before: BTreeSet<u64> = base.iter().copied().collect();
        let delta = if removing {
            before.len() - want.len()
        } else {
            want.len() - before.len()
        };
        prop_assert_eq!(n, delta as u64, "returned change count disagrees");

        // `cardinality` is answered from the index and the memtable without
        // decoding a payload, so it is a second implementation of the count.
        prop_assert_eq!(snap.cardinality(7).unwrap(), want.len() as u64, "cardinality disagrees");
        prop_assert_eq!(snap.min(7).unwrap(), want.iter().next().copied(), "min disagrees");
        prop_assert_eq!(snap.max(7).unwrap(), want.iter().next_back().copied(), "max disagrees");
    }

    /// k-way union must equal folding the pairwise kernel, for any k.
    ///
    /// `union_all` is a **second implementation** of union that takes a
    /// different path per prefix depending on how many inputs contribute
    /// ( clone at one, pairwise at two, scratch accumulator at three and up ).
    /// All three must agree with the fold and with a `BTreeSet`, and the
    /// accumulator is reused across chunks, so a leak between prefixes is the
    /// failure this is really watching for.
    #[test]
    fn union_all_equals_folding_the_pairwise_union(
        sets in prop::collection::vec(
            prop::collection::vec(0u64..300_000, 0..80),
            0..7,
        ),
    ) {
        use yesno_core::OrdSet;

        let built: Vec<OrdSet> = sets
            .iter()
            .map(|v| {
                let mut s = OrdSet::from_iter_unsorted(v.iter().copied());
                s.optimize();
                s
            })
            .collect();
        let refs: Vec<&OrdSet> = built.iter().collect();

        let got = OrdSet::union_all(&refs);

        let mut folded = OrdSet::new();
        for s in &built {
            folded = folded.or(s);
        }

        let oracle: std::collections::BTreeSet<u64> =
            sets.iter().flat_map(|v| v.iter().copied()).collect();

        let g: Vec<u64> = got.iter().collect();
        let f: Vec<u64> = folded.iter().collect();
        let o: Vec<u64> = oracle.iter().copied().collect();
        prop_assert_eq!(&g, &f, "union_all disagrees with the fold");
        prop_assert_eq!(&g, &o, "union_all disagrees with the oracle");
        prop_assert_eq!(got.len(), o.len() as u64, "cached cardinality is stale");
    }

    /// Decode is a fuzz target: arbitrary bytes must Err or validate, never panic.
    #[test]
    fn deserialize_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..512)) {
        if let Ok(s) = deserialize_u64(&bytes) {
            assert_invariants(&s);
        }
    }

    /// The same contract, but on payloads that are already well-formed at the
    /// *length* level, so the generator actually reaches the content checks.
    ///
    /// `deserialize_never_panics` above feeds uniform random bytes to the
    /// whole-file parser, which almost never gets past the header — which is
    /// exactly why four content-level defects survived it until 2026-08-25
    /// ( descending arrays, bitmaps whose stated cardinality was not their
    /// popcount, overlapping runs, and runs past the end of the chunk, the last
    /// of which panicked ). Boundary-biasing the *structure* is the same
    /// principle this file already applies to cardinalities.
    #[test]
    fn decode_of_a_plausible_array_payload_errs_or_validates(
        vals in prop::collection::vec(any::<u16>(), 1..64)
    ) {
        let mut bytes = Vec::new();
        for v in &vals {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        if let Ok(c) = codec::decode(ContainerKind::Array, &bytes, vals.len() as u32) {
            prop_assert!(codec::validate(&c).is_ok(), "decode returned an invalid array");
        }
    }

    #[test]
    fn decode_of_a_plausible_bitmap_payload_errs_or_validates(
        words in prop::collection::vec(any::<u64>(), 1024..=1024),
        card in 1u32..=65_536,
    ) {
        let mut bytes = Vec::new();
        for w in &words {
            bytes.extend_from_slice(&w.to_le_bytes());
        }
        if let Ok(c) = codec::decode(ContainerKind::Bitmap, &bytes, card) {
            prop_assert!(codec::validate(&c).is_ok(), "decode returned an invalid bitmap");
        }
    }

    /// The run generator is biased at both ends, and both halves are load-bearing.
    ///
    /// Small starts and lengths are what actually produce *ascending, adjacent*
    /// runs, so they are the only way coalescing gets exercised — uniform `u16`
    /// pairs are almost always overlapping and take the same early `Err`. But a
    /// purely small generator can never make `start + len_minus_1` exceed
    /// `u16::MAX`, so it would miss the overflow that panicked. Hence the
    /// near-65535 arm: verified by disabling the guard and watching this fail.
    #[test]
    fn decode_of_a_plausible_run_payload_errs_or_validates(
        pairs in prop::collection::vec(
            (
                prop_oneof![0u16..200, 65_000u16..=65_535],
                prop_oneof![0u16..8, 60_000u16..=65_535],
            ),
            1..24,
        )
    ) {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(pairs.len() as u16).to_le_bytes());
        for (s, l) in &pairs {
            bytes.extend_from_slice(&s.to_le_bytes());
            bytes.extend_from_slice(&l.to_le_bytes());
        }
        if let Ok(c) = codec::decode(ContainerKind::Run, &bytes, 0) {
            prop_assert!(codec::validate(&c).is_ok(), "decode returned an invalid run");
            // Touching every ordinal is what caught the u16 overflow: the
            // container looked fine until something read `end`.
            prop_assert_eq!(c.iter().count() as u32, c.len());
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    /// Seventeen dense 4,096-bit rows force a bitmap first chunk and a partial
    /// array tail. Uniform ordinals would produce neither shape and could not
    /// exercise the blocked word-row arm or its mixed-container fallback.
    #[test]
    fn blocked_view_intersection_cardinalities_match_btreeset_oracle(
        data_divisor in 2u64..8,
        data_phase in 0u64..32,
        query_divisor in 2u64..13,
        query_phase in 0u64..32,
    ) {
        const SETS: u32 = 17;
        const STRIDE: u64 = 4_096;
        let rows: Vec<BTreeSet<u64>> = (0..SETS as u64)
            .map(|owner| {
                (0..STRIDE)
                    .filter(|x| (x + owner * 11 + data_phase) % data_divisor != 0)
                    .collect()
            })
            .collect();
        let query: BTreeSet<u64> = (0..STRIDE)
            .filter(|x| (x * 5 + query_phase) % query_divisor == 0)
            .collect();
        let packed = OrdSet::from_iter_unsorted(
            rows.iter().enumerate().flat_map(|(owner, row)| {
                row.iter().map(move |x| owner as u64 * STRIDE + x)
            }),
        );
        assert_invariants(&packed);
        prop_assert!(packed
            .chunks()
            .any(|(_, container)| container.kind() == ContainerKind::Bitmap));
        prop_assert!(packed
            .chunks()
            .any(|(_, container)| container.kind() != ContainerKind::Bitmap));

        let sibling_query: BTreeSet<u64> = (0..STRIDE)
            .filter(|x| (x * 11 + query_phase + 3) % (query_divisor + 2) == 0)
            .collect();
        let batch_queries = [query.clone(), sibling_query];
        let batch_filters: Vec<_> = batch_queries
            .iter()
            .map(|query| OrdSet::from_iter_unsorted(query.iter().copied()))
            .collect();
        let batch_refs: Vec<_> = batch_filters.iter().collect();
        let batch_want: Vec<Vec<u64>> = batch_queries
            .iter()
            .map(|query| {
                rows.iter()
                    .map(|row| row.intersection(query).count() as u64)
                    .collect()
            })
            .collect();
        prop_assert_eq!(
            packed.view_intersection_cardinalities_batch(
                &View::blocked(SETS, STRIDE),
                &batch_refs,
            ),
            batch_want
        );

        for query in [query, BTreeSet::new(), (0..STRIDE).collect()] {
            let filter = OrdSet::from_iter_unsorted(query.iter().copied());
            assert_invariants(&filter);
            let want: Vec<u64> = rows
                .iter()
                .map(|row| row.intersection(&query).count() as u64)
                .collect();
            prop_assert_eq!(
                packed.view_intersection_cardinalities(&View::blocked(SETS, STRIDE), &filter),
                want
            );
        }
    }

    /// Seventeen interleaved rows put logical boundaries at non-word and
    /// non-chunk offsets. The sparse query selects bounded windows while the
    /// dense sibling makes their batch select the full scan; both must retain
    /// filter-major ordering and agree with independent row sets.
    #[test]
    fn interleaved_view_intersection_batch_matches_btreeset_oracle(
        data_divisor in 2u64..11,
        data_phase in 0u64..32,
        sparse_divisor in 53u64..191,
        sparse_phase in 0u64..53,
    ) {
        const SETS: u32 = 17;
        const WIDTH: u64 = 8_000;
        let rows: Vec<BTreeSet<u64>> = (0..SETS as u64)
            .map(|owner| {
                (0..WIDTH)
                    .filter(|x| (x * 7 + owner * 11 + data_phase) % data_divisor != 0)
                    .collect()
            })
            .collect();
        let sparse: BTreeSet<u64> = (0..WIDTH)
            .filter(|x| (x + sparse_phase) % sparse_divisor == 0)
            .collect();
        let dense: BTreeSet<u64> = (0..WIDTH).filter(|x| x % 3 != 0).collect();
        let packed = OrdSet::from_iter_unsorted(
            rows.iter().enumerate().flat_map(|(owner, row)| {
                row.iter().map(move |x| x * SETS as u64 + owner as u64)
            }),
        );
        let sparse_filter = OrdSet::from_iter_unsorted(sparse.iter().copied());
        let dense_filter = OrdSet::from_iter_unsorted(dense.iter().copied());
        let want_sparse: Vec<u64> = rows
            .iter()
            .map(|row| row.intersection(&sparse).count() as u64)
            .collect();
        let want_dense: Vec<u64> = rows
            .iter()
            .map(|row| row.intersection(&dense).count() as u64)
            .collect();

        prop_assert_eq!(
            packed.view_intersection_cardinalities(
                &View::interleaved(SETS),
                &sparse_filter
            ),
            want_sparse.clone()
        );
        prop_assert_eq!(
            packed.view_intersection_cardinalities_batch(
                &View::interleaved(SETS),
                &[&sparse_filter, &dense_filter]
            ),
            vec![want_sparse, want_dense]
        );
    }
}

/// Ranges biased to chunk boundaries, because that is where the complement's
/// per-prefix universe slicing is easiest to get wrong: a range that starts
/// mid-chunk, ends mid-chunk, or covers exactly one whole chunk each take a
/// different arm.
///
/// Deliberately **narrow**. The complement of a sparse set over a wide range is
/// a large answer by definition — a generator drawing wide ranges would be
/// measuring the machine's memory rather than the algorithm.
fn bounded_range() -> impl Strategy<Value = (u64, u64)> {
    const CH: u64 = 1 << 16;
    let anchor = prop::sample::select(vec![
        0u64,
        1,
        CH - 1,
        CH,
        CH + 1,
        2 * CH,
        1 << 20,
        (1u64 << 48) - 1,
        // The ceiling. `hi` is exclusive, so `u64::MAX` is never an element and
        // the top chunk's exclusive end (2^64) is not representable — the arm
        // that has to saturate.
        u64::MAX - CH,
        u64::MAX - 1,
        u64::MAX,
    ]);
    (anchor, 0u64..(2 * CH)).prop_map(|(a, w)| (a, a.saturating_add(w)))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// NOT against a brute-force complement over the same explicit universe.
    #[test]
    fn not_in_range_matches_oracle(vals in any_ordinals(), (lo, hi) in bounded_range()) {
        let s = OrdSet::from_iter_unsorted(vals);
        let got = s.not_in_range(lo, hi);
        assert_invariants(&got);

        let oracle: Vec<u64> = (lo..hi).filter(|v| !s.contains(*v)).collect();
        prop_assert_eq!(got.iter().collect::<Vec<_>>(), oracle.clone());
        // `len` is maintained incrementally, so it is a separate claim from the
        // contents and has to be asserted separately.
        prop_assert_eq!(got.len(), oracle.len() as u64);
    }

    /// Complementing twice returns the part of the original inside the range.
    /// This is the property that would catch an off-by-one at either bound:
    /// a range slice that is one too wide survives the oracle comparison above
    /// only if the same error is made in both directions, and this catches that.
    #[test]
    fn double_complement_is_the_original_clipped_to_the_range(
        vals in any_ordinals(),
        (lo, hi) in bounded_range(),
    ) {
        let s = OrdSet::from_iter_unsorted(vals);
        let back = s.not_in_range(lo, hi).not_in_range(lo, hi);
        assert_invariants(&back);

        let clipped: Vec<u64> = s.iter().filter(|v| *v >= lo && *v < hi).collect();
        prop_assert_eq!(back.iter().collect::<Vec<_>>(), clipped);
    }

    /// The complement and the original partition the range exactly: disjoint,
    /// and together covering every ordinal in it.
    #[test]
    fn complement_and_original_partition_the_range(
        vals in any_ordinals(),
        (lo, hi) in bounded_range(),
    ) {
        let s = OrdSet::from_iter_unsorted(vals);
        let n = s.not_in_range(lo, hi);
        let inside = s.not_in_range(lo, hi).not_in_range(lo, hi);

        prop_assert!(n.is_disjoint(&inside));
        prop_assert_eq!(n.len() + inside.len(), hi - lo);
    }
}

proptest! {
    /// `len_in_range` and `range_summary` against a `BTreeSet`, over the
    /// boundary-biased corpora the rest of this file uses.
    ///
    /// The generator matters more than the assertion here. A uniform range
    /// over a sparse set is almost always `Partial`, so `Full` — the answer that
    /// lets a planner skip building a selection vector entirely — would be
    /// reached about never. The ranges below are drawn *from the set's own
    /// structure* ( chunk boundaries, an ordinal's neighbourhood, the whole
    /// universe ) so that all three verdicts occur.
    #[test]
    fn len_in_range_and_summary_agree_with_a_btreeset(
        vals in prop_oneof![clustered_ordinals(), runny_ordinals(), any_ordinals(), ceiling_ordinals()],
        picks in prop::collection::vec(0usize..64, 1..8),
    ) {
        let oracle: std::collections::BTreeSet<u64> = vals.iter().copied().collect();
        let set = OrdSet::from_iter_unsorted(vals.iter().copied());

        // Ranges worth asking about: around the values themselves, at chunk
        // edges, and the degenerate ones.
        let mut ranges: Vec<(u64, u64)> = vec![
            (0, 0),
            (0, 1),
            (0, u64::MAX),
        ];
        for &i in &picks {
            if let Some(&v) = vals.get(i % vals.len().max(1)) {
                let base = v & !0xFFFF;
                ranges.push((v, v.saturating_add(1)));
                ranges.push((base, base.saturating_add(65_536)));
                ranges.push((base.saturating_add(1), base.saturating_add(65_536)));
                ranges.push((v.saturating_sub(1_000), v.saturating_add(1_000)));
            }
        }

        for (lo, hi) in ranges {
            let want = oracle.range(lo..hi).count() as u64;
            prop_assert_eq!(
                set.len_in_range(lo, hi), want,
                "len_in_range({}, {})", lo, hi
            );

            let width = hi.saturating_sub(lo);
            let expect = if width == 0 || want == 0 {
                RangeSummary::Empty
            } else if want == width {
                RangeSummary::Full
            } else {
                RangeSummary::Partial
            };
            prop_assert_eq!(
                set.range_summary(lo, hi), expect,
                "range_summary({}, {}) with {} of {} present", lo, hi, want, width
            );
        }
    }
}

proptest! {
    /// `Container::is_range_empty` is a **parallel implementation** of
    /// `Container::count_in_range == 0`, per kind, sharing no line with it — the
    /// hazard this file's differential properties exist for. The count is the
    /// oracle because it is the question the predicate is weaker than, which is
    /// the relationship `ops::card` requires a predicate to keep.
    ///
    /// `container_values` is the generator here rather than the ordinal ones
    /// above, because it is the one weighted to reach **all three encodings** —
    /// its own comment records that uniform `u16`s give an array every time. An
    /// arm that is wrong for exactly one kind is invisible to a generator that
    /// cannot build that kind.
    ///
    /// # The windows are drawn from the container's *runs*, not its values
    ///
    /// The first version of this property anchored windows at `vals[i % len]`
    /// with `i < 64`, so on a container holding one 8 000-value stretch it only
    /// ever probed the stretch's first 64 positions — every one of them
    /// interior. **It caught a sabotaged array arm and neither of the other
    /// two**: the run arm binary-searches on interval `end`, and no window it
    /// generated had an interval end at its edge; the bitmap arm's masks are
    /// only off by a bit, and a wide window over a dense bitmap has the same
    /// verdict either way.
    ///
    /// So the anchors below are the **start and end of every maximal run**, and
    /// the windows around each one are the narrow ones — the single cell, the
    /// cell past the end, the pair straddling the boundary. Those are the only
    /// windows whose verdict a one-position error can change, and every arm has
    /// such an error available to it.
    #[test]
    fn is_range_empty_is_count_in_range_without_the_count(
        vals in container_values(),
        free in prop::collection::vec((0u32..=65_536, 0u32..=65_536), 1..12),
    ) {
        use yesno_core::container::Container;

        let mut c = Container::from_sorted(&vals);
        c.optimize();
        if c.is_empty() {
            // An empty container is not a stored one; the unit tests cover it.
            return Ok(());
        }

        // The boundaries of every maximal run. `vals` is sorted and deduped.
        let mut anchors: Vec<u32> = Vec::new();
        let mut i = 0usize;
        while i < vals.len() {
            let s = vals[i] as u32;
            let mut e = s;
            while i + 1 < vals.len() && vals[i + 1] as u32 == e + 1 {
                i += 1;
                e += 1;
            }
            anchors.push(s);
            anchors.push(e);
            i += 1;
        }
        // Thinned so a 5 000-value strided bitmap does not generate 80 000
        // windows, but keeping the last pair whichever side of the thinning it
        // fell on: the top of the chunk is where the masks and the interval
        // arithmetic break.
        let step = (anchors.len() / 24).max(1);
        let mut anchors: Vec<u32> = anchors.iter().copied().step_by(step).collect();
        if let Some(&v) = vals.last() {
            anchors.push(v as u32);
        }
        anchors.push(0);
        anchors.push(65_535);

        let mut windows: Vec<(u32, u32)> = vec![(0, 0), (0, 1), (0, 65_536), (65_535, 65_536)];
        for (a, b) in free {
            windows.push(if a <= b { (a, b) } else { (b, a) });
        }
        for a in anchors {
            let lo1 = a.saturating_sub(1);
            for (x, y) in [
                (a, a),
                (a, a + 1),
                (lo1, a + 1),
                (lo1, a),
                (a + 1, a + 2),
                (a, a + 2),
                (a, 65_536),
                (0, a + 1),
            ] {
                windows.push((x.min(65_536), y.min(65_536)));
            }
        }

        for (lo, hi) in windows {
            let want = !vals.iter().any(|&v| (v as u32) >= lo && (v as u32) < hi);
            prop_assert_eq!(
                c.is_range_empty(lo, hi), want,
                "{:?}: is_range_empty({}, {})", c.kind(), lo, hi
            );
            prop_assert_eq!(
                c.is_range_empty(lo, hi), c.count_in_range(lo, hi) == 0,
                "{:?}: is_range_empty({}, {}) disagrees with count_in_range",
                c.kind(), lo, hi
            );
        }
    }
}
