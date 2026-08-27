//! Baseline benchmarks, with the `roaring` crate as the absolute reference.
//!
//! Measuring against `roaring` rather than against our own past numbers is
//! deliberate: it makes a regression visible as "we are now 3× slower than the
//! reference implementation", which is actionable, instead of "we are 8% slower
//! than last month", which is not.
//!
//! The headline case is `and_cardinality_sparse_vs_dense`: it is what the
//! seek-driven, galloping AND exists for, and the only benchmark here whose
//! shape the algorithm is specifically designed around.

use std::sync::Arc;

use criterion::{criterion_group, criterion_main, BatchSize, Criterion, Throughput};
use std::hint::black_box;
use yesno_core::stream::ChunkStreamExt;
use yesno_core::OrdSet;

fn lcg(seed: u64) -> impl FnMut() -> u64 {
    let mut s = seed;
    move || {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        s >> 11
    }
}

fn sparse_u32(n: usize, modulus: u32, seed: u64) -> Vec<u32> {
    let mut r = lcg(seed);
    let mut v: Vec<u32> = (0..n).map(|_| (r() % modulus as u64) as u32).collect();
    v.sort_unstable();
    v.dedup();
    v
}

fn ours(vals: &[u32]) -> OrdSet {
    let mut s = OrdSet::from_sorted_slice(&vals.iter().map(|&v| v as u64).collect::<Vec<_>>());
    s.optimize();
    s
}

fn theirs(vals: &[u32]) -> roaring::RoaringBitmap {
    vals.iter().copied().collect()
}

/// The reference, **run-optimized** — which is the only form it may be compared
/// against on run-shaped operands.
///
/// [`theirs`] never calls `run_optimize`, so a `roaring` bitmap built from a
/// run-shaped value list is held as a *bitmap* while `ours` ( which does call
/// `optimize` ) holds runs. Comparing those two measures which representation
/// each library chose and not the kernel: it is what produced the "run x run is
/// 13.9x slower" figure retracted on 2026-08-28, where the honest ratio with both
/// sides run-encoded was 1.03x. Do not use `theirs` in a run x run group.
fn theirs_runs(vals: &[u32]) -> roaring::RoaringBitmap {
    let mut b: roaring::RoaringBitmap = vals.iter().copied().collect();
    let _ = b.optimize();
    b
}

/// Dense-ish operands in the same chunk range: the common posting-list shape.
fn binary_ops(c: &mut Criterion) {
    let av = sparse_u32(200_000, 1 << 22, 1);
    let bv = sparse_u32(200_000, 1 << 22, 2);
    let (a, b) = (ours(&av), ours(&bv));
    let (ra, rb) = (theirs(&av), theirs(&bv));

    let mut g = c.benchmark_group("binary_ops");
    g.throughput(Throughput::Elements((av.len() + bv.len()) as u64));

    g.bench_function("yesno/and", |z| z.iter(|| black_box(a.and(&b)).len()));
    g.bench_function("roaring/and", |z| z.iter(|| black_box(&ra & &rb).len()));

    g.bench_function("yesno/or", |z| z.iter(|| black_box(a.or(&b)).len()));
    g.bench_function("roaring/or", |z| z.iter(|| black_box(&ra | &rb).len()));

    g.bench_function("yesno/xor", |z| z.iter(|| black_box(a.xor(&b)).len()));
    g.bench_function("roaring/xor", |z| z.iter(|| black_box(&ra ^ &rb).len()));

    g.bench_function("yesno/andnot", |z| {
        z.iter(|| black_box(a.and_not(&b)).len())
    });
    g.bench_function("roaring/andnot", |z| z.iter(|| black_box(&ra - &rb).len()));
    g.finish();
}

/// Genuinely dense operands, so containers are **bitmaps** rather than arrays.
///
/// The default `binary_ops` operands are 200k values over 64 chunks — about
/// 3 100 per chunk, under `ARRAY_MAX`, so every container there is an array.
/// This group exists because that was not obvious and led to specializing the
/// wrong kernel once already.
fn binary_ops_dense(c: &mut Criterion) {
    // Values must be *scattered*, not contiguous: a contiguous 40 000-value run
    // run-optimizes to a Run container, not a bitmap. Getting this wrong is how
    // the first version of this benchmark measured the wrong kernel entirely.
    let mk = |off: u32| -> Vec<u32> {
        (0..8u32)
            .flat_map(move |ch| (0..20_000u32).map(move |i| (ch << 16) | ((i * 3 + off) % 65536)))
            .collect::<std::collections::BTreeSet<u32>>()
            .into_iter()
            .collect()
    };
    let av = mk(0);
    let bv = mk(1);
    let (a, b) = (ours(&av), ours(&bv));
    let (ra, rb) = (theirs(&av), theirs(&bv));

    // Assert the shape rather than trusting it. A benchmark that silently
    // measures a different container kind than its name claims is worse than no
    // benchmark: it produces confident, wrong conclusions.
    assert!(
        a.chunks()
            .all(|(_, c)| c.kind() == yesno_core::ContainerKind::Bitmap),
        "binary_ops_dense must operate on bitmaps, got {:?}",
        a.chunks().map(|(_, c)| c.kind()).collect::<Vec<_>>()
    );

    let mut g = c.benchmark_group("binary_ops_dense");
    g.bench_function("yesno/and", |z| z.iter(|| black_box(a.and(&b)).len()));
    g.bench_function("roaring/and", |z| z.iter(|| black_box(&ra & &rb).len()));
    g.bench_function("yesno/or", |z| z.iter(|| black_box(a.or(&b)).len()));
    g.bench_function("roaring/or", |z| z.iter(|| black_box(&ra | &rb).len()));
    g.bench_function("yesno/xor", |z| z.iter(|| black_box(a.xor(&b)).len()));
    g.bench_function("roaring/xor", |z| z.iter(|| black_box(&ra ^ &rb).len()));
    g.finish();
}

/// The **mixed-kind** arms, which nothing else here measures.
///
/// `binary_ops` operates on arrays and `binary_ops_dense` on bitmaps, so both
/// specialized kernels are covered and the four arms that still route through
/// `ops::generic` — array x bitmap, array x run, bitmap x run, run x run — were
/// not benchmarked at all. `kernel-specialization-simd` says to specialize only
/// when a benchmark shows an arm mattering, which could not be evaluated for
/// exactly the arms it is about.
///
/// Each pair is built so the operands really are the kinds named; the assertion
/// is there because getting that wrong is how `binary_ops_dense` once measured
/// arrays while claiming bitmaps.
fn binary_ops_mixed(c: &mut Criterion) {
    // Scattered, so it stays a bitmap rather than run-optimizing.
    let dense: Vec<u32> = (0..8u32)
        .flat_map(|ch| (0..20_000u32).map(move |i| (ch << 16) | ((i * 3) % 65536)))
        .collect::<std::collections::BTreeSet<u32>>()
        .into_iter()
        .collect();
    // Sparse enough per chunk to stay an array.
    let sparse: Vec<u32> = (0..8u32)
        .flat_map(|ch| (0..3_000u32).map(move |i| (ch << 16) | (i * 7)))
        .collect();
    // Contiguous, so it run-optimizes.
    let runs: Vec<u32> = (0..8u32)
        .flat_map(|ch| (0..30_000u32).map(move |i| (ch << 16) | i))
        .collect();

    let (a, b, r) = (ours(&sparse), ours(&dense), ours(&runs));
    use yesno_core::ContainerKind::*;
    let kinds = |s: &OrdSet| s.chunks().map(|(_, c)| c.kind()).collect::<Vec<_>>();
    assert!(
        kinds(&a).iter().all(|k| *k == Array),
        "want arrays, got {:?}",
        kinds(&a)
    );
    assert!(
        kinds(&b).iter().all(|k| *k == Bitmap),
        "want bitmaps, got {:?}",
        kinds(&b)
    );
    assert!(
        kinds(&r).iter().all(|k| *k == Run),
        "want runs, got {:?}",
        kinds(&r)
    );

    let (ra, rb, rr) = (theirs(&sparse), theirs(&dense), theirs(&runs));

    let mut g = c.benchmark_group("binary_ops_mixed");
    for (name, x, y, rx, ry) in [
        ("array_x_bitmap", &a, &b, &ra, &rb),
        ("array_x_run", &a, &r, &ra, &rr),
        ("bitmap_x_run", &b, &r, &rb, &rr),
        ("run_x_run", &r, &r, &rr, &rr),
    ] {
        g.bench_function(format!("yesno/{name}/and"), |z| {
            z.iter(|| black_box(x.and(y)).len())
        });
        g.bench_function(format!("roaring/{name}/and"), |z| {
            z.iter(|| black_box(rx & ry).len())
        });
        g.bench_function(format!("yesno/{name}/or"), |z| {
            z.iter(|| black_box(x.or(y)).len())
        });
        g.bench_function(format!("roaring/{name}/or"), |z| {
            z.iter(|| black_box(rx | ry).len())
        });
    }
    g.finish();
}

/// The non-materializing path versus building the result and counting it.
/// The gap here is the whole justification for the cardinality identities.
fn cardinality_paths(c: &mut Criterion) {
    let av = sparse_u32(200_000, 1 << 22, 3);
    let bv = sparse_u32(200_000, 1 << 22, 4);
    let (a, b) = (ours(&av), ours(&bv));
    let (ra, rb) = (theirs(&av), theirs(&bv));

    let mut g = c.benchmark_group("cardinality");
    g.bench_function("yesno/and_cardinality", |z| {
        z.iter(|| black_box(a.and_cardinality(&b)))
    });
    g.bench_function("yesno/and_then_len", |z| {
        z.iter(|| black_box(a.and(&b).len()))
    });
    g.bench_function("roaring/intersection_len", |z| {
        z.iter(|| black_box(ra.intersection_len(&rb)))
    });

    g.bench_function("yesno/or_cardinality", |z| {
        z.iter(|| black_box(a.or_cardinality(&b)))
    });
    g.bench_function("yesno/or_then_len", |z| {
        z.iter(|| black_box(a.or(&b).len()))
    });
    g.finish();
}

/// Cardinality across **mixed kinds**, which `cardinality_paths` does not cover.
///
/// `ops::card` specializes bitmap x bitmap and probes anything against a
/// bitmap, and merges values for everything else — the same pathology the
/// binary kernels had, in the path the README calls headline. `cardinality_paths`
/// uses arrays only, so it could not show it.
fn cardinality_mixed(c: &mut Criterion) {
    let dense: Vec<u32> = (0..8u32)
        .flat_map(|ch| (0..20_000u32).map(move |i| (ch << 16) | ((i * 3) % 65536)))
        .collect::<std::collections::BTreeSet<u32>>()
        .into_iter()
        .collect();
    let sparse: Vec<u32> = (0..8u32)
        .flat_map(|ch| (0..3_000u32).map(move |i| (ch << 16) | (i * 7)))
        .collect();
    let runs: Vec<u32> = (0..8u32)
        .flat_map(|ch| (0..30_000u32).map(move |i| (ch << 16) | i))
        .collect();
    let (a, b, r) = (ours(&sparse), ours(&dense), ours(&runs));
    let (ra, rb, rr) = (theirs(&sparse), theirs(&dense), theirs(&runs));

    let mut g = c.benchmark_group("cardinality_mixed");
    for (name, x, y, rx, ry) in [
        ("array_x_bitmap", &a, &b, &ra, &rb),
        ("array_x_run", &a, &r, &ra, &rr),
        ("bitmap_x_run", &b, &r, &rb, &rr),
        ("run_x_run", &r, &r, &rr, &rr),
    ] {
        g.bench_function(format!("yesno/{name}"), |z| {
            z.iter(|| black_box(x.and_cardinality(y)))
        });
        g.bench_function(format!("roaring/{name}"), |z| {
            z.iter(|| black_box(rx.intersection_len(ry)))
        });
    }
    g.finish();
}

/// The headline: a 3-chunk operand against a 15 000-chunk one.
///
/// Leapfrog seek should make this scale with the *sparse* side, so the dense
/// operand's size should barely move the number. That is the property to watch;
/// a regression here means `seek` stopped galloping.
fn and_cardinality_sparse_vs_dense(c: &mut Criterion) {
    let mut g = c.benchmark_group("sparse_and_dense");

    let dense_vals: Vec<u32> = (0..15_000u32)
        .flat_map(|ch| (0..40u32).map(move |i| (ch << 16) | i))
        .collect();
    let dense = Arc::new(ours(&dense_vals));
    let rdense = theirs(&dense_vals);

    for &ratio in &[10usize, 1_000, 100_000] {
        let n = (dense_vals.len() / ratio).max(1);
        let sparse_vals: Vec<u32> = dense_vals.iter().step_by(ratio).copied().take(n).collect();
        let sparse = Arc::new(ours(&sparse_vals));
        let rsparse = theirs(&sparse_vals);

        g.bench_function(format!("yesno/1:{ratio}"), |z| {
            z.iter(|| black_box(sparse.and_cardinality(&dense)))
        });
        g.bench_function(format!("yesno-stream/1:{ratio}"), |z| {
            z.iter(|| black_box(sparse.stream().and(dense.stream()).cardinality().unwrap()))
        });
        g.bench_function(format!("roaring/1:{ratio}"), |z| {
            z.iter(|| black_box(rsparse.intersection_len(&rdense)))
        });
    }
    g.finish();
}

/// Lazy `a AND (b OR c)` versus eagerly materializing each step.
fn nested_expression(c: &mut Criterion) {
    let av = sparse_u32(100_000, 1 << 21, 5);
    let bv = sparse_u32(100_000, 1 << 21, 6);
    let cv = sparse_u32(100_000, 1 << 21, 7);
    let (a, b, cc) = (
        Arc::new(ours(&av)),
        Arc::new(ours(&bv)),
        Arc::new(ours(&cv)),
    );

    let mut g = c.benchmark_group("nested");
    g.bench_function("yesno/lazy_cardinality", |z| {
        z.iter(|| {
            black_box(
                a.stream()
                    .and(b.stream().or(cc.stream()))
                    .cardinality()
                    .unwrap(),
            )
        })
    });
    g.bench_function("yesno/eager_cardinality", |z| {
        z.iter(|| black_box(a.and(&b.or(&cc)).len()))
    });
    g.finish();
}

/// Bulk build versus per-ordinal insert. The plan claims the former should be
/// dramatically faster because it builds each container in its final form.
fn build(c: &mut Criterion) {
    let vals: Vec<u64> = sparse_u32(200_000, 1 << 22, 8)
        .iter()
        .map(|&v| v as u64)
        .collect();

    let mut g = c.benchmark_group("build");
    g.throughput(Throughput::Elements(vals.len() as u64));
    g.bench_function("yesno/bulk", |z| {
        z.iter(|| black_box(OrdSet::from_sorted_slice(&vals)).len())
    });
    g.bench_function("yesno/insert_each", |z| {
        z.iter_batched(
            OrdSet::new,
            |mut s| {
                for &v in &vals {
                    s.insert(v);
                }
                black_box(s.len())
            },
            BatchSize::SmallInput,
        )
    });
    g.finish();
}

/// Serialization, which must stay competitive since the payloads are identical.
fn serde(c: &mut Criterion) {
    let vals = sparse_u32(200_000, 1 << 22, 9);
    let s = ours(&vals);
    let r = theirs(&vals);
    let bytes = yesno_core::roaring_format::serialize_u64(&s);

    let mut g = c.benchmark_group("serde");
    g.bench_function("yesno/serialize", |z| {
        z.iter(|| black_box(yesno_core::roaring_format::serialize_u64(&s)).len())
    });
    g.bench_function("roaring/serialize", |z| {
        z.iter(|| {
            let mut out = Vec::new();
            r.serialize_into(&mut out).unwrap();
            black_box(out).len()
        })
    });
    g.bench_function("yesno/deserialize", |z| {
        z.iter(|| black_box(yesno_core::roaring_format::deserialize_u64(&bytes).unwrap()).len())
    });
    g.finish();
}

/// `is_disjoint` and `contains_all`, which nothing else here measures.
///
/// # Why these need their own group, and why the operands say "yes"
///
/// Both short-circuit, so their cheap case is the **false** answer: one common
/// element ends `is_disjoint`, one missing element ends `contains_all`. The
/// price is paid entirely by the **true** answer, which cannot exit early and
/// must walk. So every pair here is built to answer *yes*, which is the only
/// shape that measures the kernel rather than the exit.
///
/// # The asymmetry this existed to expose, and what it measures now
///
/// **This header described a gap that has since been closed, and said so for
/// long enough to mislead** ( corrected 2026-09-06 ). It claimed `is_disjoint`
/// and `contains_all` had "one each — bitmap x bitmap" against
/// `and_cardinality`'s six, with everything else falling through to
/// `probe.iter().any(|v| target.contains(v))` — an algorithmic gap, not merely a
/// dispatch one, since two balanced arrays cost `m log m` probes where a merge
/// is `2m` steps.
///
/// Both predicates now specialize **all six** unordered kind-pairs, the same
/// as `and_cardinality`. The generic fallback still stands at the end of
/// `is_disjoint` and is reachable only in two residual shapes: a bitmap that is
/// *smaller* than the array it is paired with ( the operands are normalized so
/// the shorter side probes ), and a bitmap whose words are not directly
/// addressable. Neither is what this group builds.
///
/// So the group no longer measures specialized-versus-generic. What it
/// measures is the six arms against each other, and — with
/// `bitmap_predicate_vs_count` — the discipline `ops::card` states and proves
/// twice: *a predicate must never cost more than the count it is weaker than.*
/// Do not delete it as redundant on the strength of the old header: the
/// claim it now carries is the one worth keeping measured, and on 2026-09-06
/// the bitmap x bitmap arm answered `is_disjoint` at **0.78x** its
/// `and_cardinality == 0` equivalent and `contains_all` at **0.77x** of its
/// own, with the early-exit shapes 31x and 41x below the walk. Ratios only —
/// that run was on a machine at load 24.
///
/// Whether that gap is worth closing is what this measures; it is not assumed.
/// `kernel-specialization-simd` says to specialize when a benchmark shows an arm
/// mattering, and the arms in question were not benchmarked at all — which is
/// the same hole that hid the missing `array x array` cardinality arm.
fn predicate_paths(c: &mut Criterion) {
    use yesno_core::container::{ArrayContainer, BitmapContainer, RunContainer};
    use yesno_core::Container;

    let arr = |v: &[u16]| Container::Array(ArrayContainer::from_sorted_vec(v.to_vec()));
    let bmp = |v: &[u16]| Container::Bitmap(BitmapContainer::from_sorted(v));
    let run = |v: &[u16]| Container::Run(RunContainer::from_sorted_values(v.iter().copied()));

    // Disjointness is made **structural** — every `a` operand lives in
    // [0, 32768) and every `b` operand in [32768, 65536) — rather than arranged
    // by strides. The first version of this group staggered strides instead
    // ( multiples of 8 against values ≡ 1 mod 3 ) and those sets intersect at
    // 16, 40, 64, ...; the assertion below caught it, which is the reason it is
    // an assertion and not a comment.
    const HI: u16 = 32768;
    let sparse_a: Vec<u16> = (0..4000u16).map(|i| i * 8).collect();
    let sparse_b: Vec<u16> = (0..4000u16).map(|i| HI + i * 8).collect();
    // Scattered, so these stay bitmaps instead of run-optimizing.
    let dense_a: Vec<u16> = (0..15_000u16).map(|i| i * 2).collect();
    let dense_b: Vec<u16> = (0..15_000u16).map(|i| HI + i * 2).collect();
    // Contiguous blocks, so these really are runs.
    let runs_a: Vec<u16> = (0..54u16)
        .flat_map(|k| (0..300u16).map(move |i| k * 600 + i))
        .collect();
    let runs_b: Vec<u16> = (0..54u16)
        .flat_map(|k| (0..300u16).map(move |i| HI + k * 600 + i))
        .collect();

    // Subsets, all drawn from the `a` side so containment holds by construction
    // and `contains_all` walks every element of `sub` without failing a probe.
    // Array subs are capped at ARRAY_MAX; run subs keep whole blocks so they
    // stay runs rather than degenerating into singletons.
    let sub_sparse: Vec<u16> = sparse_a.iter().step_by(2).copied().collect();
    let sub_dense: Vec<u16> = dense_a.iter().step_by(2).copied().collect();
    let sub_dense_small: Vec<u16> = dense_a.iter().take(4000).copied().collect();
    let sub_runs: Vec<u16> = (0..54u16)
        .step_by(2)
        .flat_map(|k| (0..300u16).map(move |i| k * 600 + i))
        .collect();
    let sub_runs_small: Vec<u16> = runs_a.iter().take(4000).copied().collect();

    let mut g = c.benchmark_group("predicate_paths");
    for (name, a, b, sup, sub) in [
        (
            "array_x_array",
            arr(&sparse_a),
            arr(&sparse_b),
            arr(&sparse_a),
            arr(&sub_sparse),
        ),
        (
            "array_x_bitmap",
            arr(&sparse_a),
            bmp(&dense_b),
            bmp(&dense_a),
            arr(&sub_dense_small),
        ),
        (
            "array_x_run",
            arr(&sparse_a),
            run(&runs_b),
            run(&runs_a),
            arr(&sub_runs_small),
        ),
        (
            "run_x_run",
            run(&runs_a),
            run(&runs_b),
            run(&runs_a),
            run(&sub_runs),
        ),
        (
            "bitmap_x_run",
            bmp(&dense_a),
            run(&runs_b),
            bmp(&runs_a),
            run(&sub_runs),
        ),
        (
            "bitmap_x_bitmap",
            bmp(&dense_a),
            bmp(&dense_b),
            bmp(&dense_a),
            bmp(&sub_dense),
        ),
    ] {
        // The measurement is only about the expensive answer, so pin it.
        assert!(
            yesno_core::ops::is_disjoint(&a, &b),
            "{name}: operands must be disjoint or this measures the early exit"
        );
        assert!(
            yesno_core::ops::contains_all(&sup, &sub),
            "{name}: subset must hold or this measures the early exit"
        );
        g.bench_function(format!("is_disjoint/{name}"), |z| {
            z.iter(|| black_box(yesno_core::ops::is_disjoint(&a, &b)))
        });
        g.bench_function(format!("contains_all/{name}"), |z| {
            z.iter(|| black_box(yesno_core::ops::contains_all(&sup, &sub)))
        });
    }
    g.finish();

    // The absolute baseline the file's policy asks for. Whole-set, so it
    // includes the chunk merge as well as the kernels.
    let av = sparse_u32(200_000, 1 << 22, 21);
    let bv: Vec<u32> = av.iter().map(|v| v + (1 << 24)).collect();
    let (a, b) = (ours(&av), ours(&bv));
    let (ra, rb) = (theirs(&av), theirs(&bv));
    let mut g = c.benchmark_group("predicate_baseline");
    g.bench_function("yesno/is_disjoint", |z| {
        z.iter(|| black_box(a.is_disjoint(&b)))
    });
    g.bench_function("roaring/is_disjoint", |z| {
        z.iter(|| black_box(ra.is_disjoint(&rb)))
    });
    g.bench_function("yesno/is_subset", |z| z.iter(|| black_box(a.is_subset(&a))));
    g.bench_function("roaring/is_subset", |z| {
        z.iter(|| black_box(ra.is_subset(&ra)))
    });
    g.finish();
}

/// Where does array intersection stop being cheaper than bitmap intersection?
///
/// # The question this answers
///
/// `ARRAY_MAX` is a **space** threshold: at `m = 4096` an array payload reaches
/// the 8192 bytes a bitmap always costs, so above it the array is strictly
/// larger. Nothing in that derivation is about time. The two kernels have
/// different shapes — bitmap∩bitmap is 1024 word ANDs and a popcount,
/// `Theta(1)` per chunk and *density-independent*; array∩array is a merge,
/// `Theta(m)` — so a **time** crossover `m*` exists and there is no reason at
/// all for it to land on the same constant. If `m* < ARRAY_MAX`, then over the
/// band `m* <= m <= 4096` Roaring stores an array that is space-optimal and
/// slower to intersect than the bitmap it is declining to use, which is what
/// `kind-aware-temporary-promotion` would exploit.
///
/// It must be measured **in one setup**. The two per-chunk figures otherwise
/// available come from `binary_ops` and `binary_ops_dense`, which differ in
/// corpus, chunk count and operand construction; dividing one by the other is
/// the cross-setup error, and it is why the estimate was deliberately not made
/// from the numbers already on hand.
///
/// # Three columns, because the answer depends on which array kernel is meant
///
/// **This paragraph described a missing arm that was added on 2026-08-26,
/// and went on saying so until 2026-09-06.** It read: `and_cardinality` has a
/// specialized arm for bitmap×bitmap, run×run, bitmap×run, array×run and
/// array×bitmap — and *none for array×array*, which falls through to the generic
/// `Peekable<ContainerIter>` merge. That is no longer true, and the error was
/// not cosmetic: this group's whole three-column design rests on `array/generic`
/// being "what the crate executes today", so a reader taking the columns at face
/// value would locate the crossover against a path the crate no longer takes.
///
/// `and_cardinality` now specializes **all six** unordered kind-pairs;
/// array×array delegates to `ops::array` so it shares `and`'s gallop-versus-merge
/// decision rather than re-deciding it. The generic `Peekable<ContainerIter>`
/// merge still sits at the bottom of the function and still dispatches on the
/// container kind once per element on *both* sides, but no pair of the three
/// kinds reaches it.
///
/// So the columns mean something different now, and the group is still worth
/// keeping for it. **`array/generic` is current behaviour, not a
/// counterfactual** — the name predates `ops::array::and_cardinality` landing
/// on 2026-08-26, and this doc went on calling it "what array×array cost before
/// it had an arm" for three weeks after that stopped being true. It calls
/// `ops::and_cardinality`, which delegates to `ops::array` and therefore runs
/// whichever vector arm the host has. Corrected 2026-09-18, caught by an A/B on
/// real x86: disabling the x86 dispatch moved this column by up to 4x, which is
/// impossible for a path that bypasses `ops::array`.
///
/// `array/slice_merge` is the same algorithm over `&[u16]` with the dispatch
/// removed, and the difference between them is the dispatch overhead alone.
/// Do not delete either: a measured before is the only thing that keeps
/// "flat 2.5x, m = 32 to 4096" falsifiable, and the way to get a real
/// counterfactual now is to force the arch dispatch off and re-run.
///
/// Equal cardinalities on both sides, so the gallop path is not taken and this
/// is the merge case throughout. Operands stay inside one chunk: this is a
/// kernel measurement, not a merge-join one, and `roaring` has no container-level
/// public API to serve as a reference here.
fn intersect_crossover(c: &mut Criterion) {
    use yesno_core::container::{ArrayContainer, BitmapContainer};
    use yesno_core::Container;

    let mut g = c.benchmark_group("intersect_crossover");
    // `ARRAY_MAX` is the top of the sweep because an `Array` above it violates a
    // container invariant; if no crossing appears by 4096 the answer is
    // "m* >= ARRAY_MAX", which is itself the finding.
    for m in [32usize, 64, 128, 256, 512, 1024, 2048, 3072, 4096] {
        let av = chunk_vals(m, 0xA1);
        let bv = chunk_vals(m, 0xB2);

        let a_arr = Container::Array(ArrayContainer::from_sorted_vec(av.clone()));
        let b_arr = Container::Array(ArrayContainer::from_sorted_vec(bv.clone()));
        let a_bm = Container::Bitmap(BitmapContainer::from_sorted(&av));
        let b_bm = Container::Bitmap(BitmapContainer::from_sorted(&bv));

        // Same answer from all three, or the comparison is meaningless.
        assert_eq!(
            yesno_core::ops::and_cardinality(&a_arr, &b_arr),
            yesno_core::ops::and_cardinality(&a_bm, &b_bm)
        );
        assert_eq!(
            yesno_core::ops::and_cardinality(&a_arr, &b_arr),
            slice_merge_card(&av, &bv)
        );

        g.bench_function(format!("array/generic/m={m}"), |z| {
            z.iter(|| black_box(yesno_core::ops::and_cardinality(&a_arr, &b_arr)))
        });
        g.bench_function(format!("array/slice_merge/m={m}"), |z| {
            z.iter(|| black_box(slice_merge_card(&av, &bv)))
        });
        g.bench_function(format!("bitmap/m={m}"), |z| {
            z.iter(|| black_box(yesno_core::ops::and_cardinality(&a_bm, &b_bm)))
        });
        // What `kind-aware-temporary-promotion` would have to pay before it can
        // spend the bitmap kernel's flat cost. One operand only: the rule is
        // pointless unless it is charged, and a crossover quoted without it
        // describes a bitmap the caller does not have.
        g.bench_function(format!("promote/m={m}"), |z| {
            z.iter(|| black_box(BitmapContainer::from_sorted(&av)).len())
        });
    }
    g.finish();
}

/// The crossover again, with the working set too large to stay resident.
///
/// # Why the previous group is not enough on its own
///
/// `intersect_crossover` intersects the *same two containers* on every
/// iteration, so both operands sit in L1 throughout and the bitmap arm never
/// pays for a fetch. That is the one parameter it holds fixed, and it is the one
/// that could reverse its conclusion: a bitmap operand is 8192 bytes whatever
/// its cardinality, so a stream of bitmap chunks moves 16 KiB per intersection
/// against `4m` bytes for a pair of arrays. At `m = 140` that is a 29x traffic
/// ratio, and a kernel that wins on instructions can still lose on bandwidth.
///
/// So this cycles `K` distinct pairs, sized so the bitmap working set clears the
/// caches while the array working set does not — which is exactly the asymmetry
/// a real chunk stream has, and exactly what `kind-aware-temporary-promotion`
/// would have to survive. Reporting `m*` from the resident measurement alone
/// would be quoting a number whose generality was never tested.
fn intersect_crossover_streamed(c: &mut Criterion) {
    use yesno_core::container::{ArrayContainer, BitmapContainer};
    use yesno_core::Container;

    let mut g = c.benchmark_group("intersect_crossover_streamed");
    // K = 512 pairs: 16 MiB of bitmap operands, past any L2 and most L3 slices,
    // against 4m*512 bytes of array operands ( 288 KiB at m=144 ).
    const K: usize = 512;
    for m in [8usize, 16, 32, 64, 144, 1024, 4096] {
        let vals: Vec<(Vec<u16>, Vec<u16>)> = (0..K)
            .map(|i| {
                (
                    chunk_vals(m, 0x100 + i as u64),
                    chunk_vals(m, 0x9000 + i as u64),
                )
            })
            .collect();
        let arrs: Vec<(Container, Container)> = vals
            .iter()
            .map(|(a, b)| {
                (
                    Container::Array(ArrayContainer::from_sorted_vec(a.clone())),
                    Container::Array(ArrayContainer::from_sorted_vec(b.clone())),
                )
            })
            .collect();
        let bms: Vec<(Container, Container)> = vals
            .iter()
            .map(|(a, b)| {
                (
                    Container::Bitmap(BitmapContainer::from_sorted(a)),
                    Container::Bitmap(BitmapContainer::from_sorted(b)),
                )
            })
            .collect();

        g.throughput(Throughput::Elements(K as u64));
        g.bench_function(format!("array/m={m}"), |z| {
            z.iter(|| {
                let mut n = 0u64;
                for (a, b) in &arrs {
                    n += yesno_core::ops::and_cardinality(a, b) as u64;
                }
                black_box(n)
            })
        });
        g.bench_function(format!("bitmap/m={m}"), |z| {
            z.iter(|| {
                let mut n = 0u64;
                for (a, b) in &bms {
                    n += yesno_core::ops::and_cardinality(a, b) as u64;
                }
                black_box(n)
            })
        });
    }
    g.finish();
}

/// A sorted-`u16` chunk of `m` distinct values scattered over the whole chunk.
///
/// Shared by the two groups below and by `intersect_crossover`, which had its
/// own copy. This is the shape of the corpus's leading waste cell — `m` in
/// 256..1023 with `r` in 513..1024, i.e. essentially one run per value — so it
/// is the shape any claim about the array arm has to be quoted at.
fn chunk_vals(m: usize, seed: u64) -> Vec<u16> {
    let mut r = lcg(seed);
    let mut seen = vec![false; 1 << 16];
    let mut v: Vec<u16> = Vec::with_capacity(m);
    while v.len() < m {
        let x = (r() % (1 << 16)) as u16;
        if !std::mem::replace(&mut seen[x as usize], true) {
            v.push(x);
        }
    }
    v.sort_unstable();
    v
}

/// The two-pointer merge, as `ops::array` had it before the vector arm. The
/// baseline every number in `intersect_vector_arm` is quoted against.
fn slice_merge_card(a: &[u16], b: &[u16]) -> u32 {
    let (mut i, mut j, mut n) = (0usize, 0usize, 0u32);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                i += 1;
                j += 1;
                n += 1;
            }
        }
    }
    n
}

fn slice_merge_and(a: &[u16], b: &[u16]) -> Vec<u16> {
    let mut out = Vec::with_capacity(a.len().min(b.len()));
    let (mut i, mut j) = (0usize, 0usize);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    out
}

/// **The control that says how much of a kernel measurement is real.**
///
/// `intersect_crossover` intersects the *same two containers* on every
/// iteration. That holds the working set in L1, which its sibling group
/// already accounts for — but it also holds the **branch history** fixed, and
/// nothing accounted for that. A two-pointer merge takes one data-dependent
/// branch per element consumed, so a predictor that has seen the same 2 000-step
/// decision sequence a few thousand times gets it right essentially every time,
/// and the loop reports a number no query will ever see.
///
/// Three arms, and the third is what makes the reading unambiguous:
///
/// * `one_pair` — one pair, repeated. 4 KiB live, branch history memorizable.
/// * `k_clones` — `K` pairs holding the **same** values, in one contiguous
///   arena. Same footprint and same stride as `k_distinct`, same branch
///   sequence as `one_pair`.
/// * `k_distinct` — `K` pairs holding **different** values, in an arena laid
///   out identically.
///
/// `k_clones` isolates it: it differs from `one_pair` only in footprint and
/// from `k_distinct` only in the values. Whatever separates `k_clones` from
/// `k_distinct` is attributable to branch predictability and to nothing else.
///
/// Measured on `aarch64-unknown-linux-gnu`, ns per pair:
///
/// ```text
///          scalar merge                vector arm
///     one_pair  k_clones k_distinct   one_pair k_clones k_distinct
///  256   266.4     256.3     450.3      231.3    232.2     239.5
///  512   528.7     518.1    1838.3      474.0    480.4     482.7
/// 1024  1087.1    1088.9    4760.2      952.2    972.3     975.4
/// ```
///
/// `one_pair` and `k_clones` agree to within 2% at every size, so footprint is
/// not what separates them from `k_distinct`; the values are. The scalar merge
/// pays 4.4x for that at m = 1024 and the vector arm pays 2%.
///
/// Do not quote a scalar `array x array` figure from a group that reuses its
/// operands. It is measuring the branch predictor's memory, not the kernel.
fn intersect_branch_history(c: &mut Criterion) {
    use yesno_core::container::ArrayContainer;
    use yesno_core::Container;

    /// The `k`th pair inside an arena of `K` interleaved `(a, b)` blocks.
    fn pair(arena: &[u16], m: usize, k: usize) -> (&[u16], &[u16]) {
        let base = k * 2 * m;
        (&arena[base..base + m], &arena[base + m..base + 2 * m])
    }

    const K: usize = 64;
    let mut g = c.benchmark_group("intersect_branch_history");
    for m in [256usize, 512, 1024] {
        // One contiguous arena per arm, so layout, footprint and stride are
        // byte-identical between `k_clones` and `k_distinct`.
        let (one_a, one_b) = (chunk_vals(m, 0xA100), chunk_vals(m, 0xB200));
        let mut clones: Vec<u16> = Vec::with_capacity(K * 2 * m);
        let mut distinct: Vec<u16> = Vec::with_capacity(K * 2 * m);
        for k in 0..K {
            clones.extend_from_slice(&one_a);
            clones.extend_from_slice(&one_b);
            distinct.extend_from_slice(&chunk_vals(m, 0xA100 + k as u64));
            distinct.extend_from_slice(&chunk_vals(m, 0xB200 + k as u64));
        }
        // The crate's own arm, over the same arenas, so the two rows of the
        // table are the same corpus and differ only in the kernel.
        let boxed = |arena: &[u16]| -> Vec<(Container, Container)> {
            (0..K)
                .map(|k| {
                    let (a, b) = pair(arena, m, k);
                    (
                        Container::Array(ArrayContainer::from_sorted_vec(a.to_vec())),
                        Container::Array(ArrayContainer::from_sorted_vec(b.to_vec())),
                    )
                })
                .collect()
        };
        let (cc, cd) = (boxed(&clones), boxed(&distinct));

        g.throughput(Throughput::Elements(K as u64));
        for (name, arena, boxed) in [("clones", &clones, &cc), ("distinct", &distinct, &cd)] {
            g.bench_function(format!("scalar/k_{name}/m={m}"), |z| {
                z.iter(|| {
                    let mut n = 0u64;
                    for k in 0..K {
                        let (a, b) = pair(arena, m, k);
                        n += slice_merge_card(a, b) as u64;
                    }
                    black_box(n)
                })
            });
            g.bench_function(format!("vector/k_{name}/m={m}"), |z| {
                z.iter(|| {
                    let mut n = 0u64;
                    for (a, b) in boxed {
                        n += yesno_core::ops::and_cardinality(a, b) as u64;
                    }
                    black_box(n)
                })
            });
        }
        let (a0, b0) = pair(&clones, m, 0);
        g.bench_function(format!("scalar/one_pair/m={m}"), |z| {
            z.iter(|| {
                let mut n = 0u64;
                for _ in 0..K {
                    n += slice_merge_card(black_box(a0), black_box(b0)) as u64;
                }
                black_box(n)
            })
        });
        g.bench_function(format!("vector/one_pair/m={m}"), |z| {
            let (a, b) = &cc[0];
            z.iter(|| {
                let mut n = 0u64;
                for _ in 0..K {
                    n += yesno_core::ops::and_cardinality(black_box(a), black_box(b)) as u64;
                }
                black_box(n)
            })
        });
    }
    g.finish();
}

/// What the vector `array x array` merge is worth, and where it is worth
/// nothing.
///
/// `K` distinct pairs per sweep, for the reason `intersect_branch_history`
/// establishes: reusing one pair would report the scalar baseline about 4.7x
/// faster than a query ever sees it and understate the arm by the same factor.
///
/// The skewed rows past `GALLOP_RATIO` are here because they are the ones that
/// say **not** to widen: `O(small · log large)` beats `O(small + large)` at any
/// vector width, so those rows must show the arm declining to fire, not winning.
fn intersect_vector_arm(c: &mut Criterion) {
    use yesno_core::container::ArrayContainer;
    use yesno_core::Container;

    const K: usize = 64;
    let mut g = c.benchmark_group("intersect_vector_arm");
    for (ma, mb) in [
        (32usize, 32usize),
        (128, 128),
        (256, 256),
        (512, 512),
        (1024, 1024),
        (2048, 2048),
        (4096, 4096),
        // Skewed but under GALLOP_RATIO: still the merge.
        (128, 1024),
        // Skewed past GALLOP_RATIO: the gallop, which the arm must leave alone.
        (32, 1024),
        (128, 4096),
    ] {
        let vals: Vec<(Vec<u16>, Vec<u16>)> = (0..K)
            .map(|k| {
                (
                    chunk_vals(ma, 0xA100 + k as u64),
                    chunk_vals(mb, 0xB200 + k as u64),
                )
            })
            .collect();
        let arrs: Vec<(Container, Container)> = vals
            .iter()
            .map(|(a, b)| {
                (
                    Container::Array(ArrayContainer::from_sorted_vec(a.clone())),
                    Container::Array(ArrayContainer::from_sorted_vec(b.clone())),
                )
            })
            .collect();
        // Same answer from both kernels, or the comparison is meaningless.
        for ((a, b), (ca, cb)) in vals.iter().zip(&arrs) {
            assert_eq!(
                yesno_core::ops::and_cardinality(ca, cb),
                slice_merge_card(a, b)
            );
            let ours: Vec<u16> = yesno_core::ops::and(ca, cb)
                .as_ref()
                .map(|c| c.iter().collect())
                .unwrap_or_default();
            assert_eq!(ours, slice_merge_and(a, b));
        }

        g.throughput(Throughput::Elements(K as u64));
        g.bench_function(format!("card/scalar/{ma}x{mb}"), |z| {
            z.iter(|| {
                let mut n = 0u64;
                for (a, b) in &vals {
                    n += slice_merge_card(a, b) as u64;
                }
                black_box(n)
            })
        });
        g.bench_function(format!("card/crate/{ma}x{mb}"), |z| {
            z.iter(|| {
                let mut n = 0u64;
                for (a, b) in &arrs {
                    n += yesno_core::ops::and_cardinality(a, b) as u64;
                }
                black_box(n)
            })
        });
        g.bench_function(format!("and/scalar/{ma}x{mb}"), |z| {
            z.iter(|| {
                let mut n = 0u64;
                for (a, b) in &vals {
                    n += slice_merge_and(a, b).len() as u64;
                }
                black_box(n)
            })
        });
        g.bench_function(format!("and/crate/{ma}x{mb}"), |z| {
            z.iter(|| {
                let mut n = 0u64;
                for (a, b) in &arrs {
                    n += yesno_core::ops::and(a, b).map_or(0, |c| c.len()) as u64;
                }
                black_box(n)
            })
        });
    }
    g.finish();
}

/// `Snapshot::load` against chunk count, decomposed so a superlinear term is
/// attributable rather than merely visible.
///
/// `load-is-superlinear` in `JOURNAL.md` records 3.82 us / 52.05 us / 1.59 ms at
/// 100 / 1 000 / 10 000 chunks — 100x the data for 416x the time — and asks for
/// a profile before anything is built on top of it. Reading the code answers
/// nothing: `merged_chunks` is a range scan into a `BTreeMap` and
/// `OrdSet::from_chunks` is a `reserve` plus a push per chunk, so both are
/// linear by inspection and the superlinear term is somewhere the source does
/// not show.
///
/// Three columns, all public API, chosen so each strips one layer off the one
/// above it:
///
/// * `load` — the subject: scan, decode/clone per chunk, collect.
/// * `cardinality` — the same walk over the same corpus with **no payload
///   touched and nothing cloned**. It is the control for the scan itself.
/// * `from_chunks` — the tail half alone, over a `Vec` built outside the timed
///   region.
///
/// If `load` is superlinear while both of the others are linear, the cost is in
/// the per-chunk clone or in the intermediate map, not in the walk.
fn load_scaling(c: &mut Criterion) {
    use yesno_core::Db;

    // One key, `n` chunks, four ordinals each — small enough that every
    // container is an array and the payload is not what is being measured.
    fn corpus(n: usize) -> Db {
        let db = Db::new();
        for p in 0..n as u64 {
            for l in 0..4u64 {
                db.insert(1, (p << 16) | (l * 4096)).unwrap();
            }
        }
        db
    }

    let mut g = c.benchmark_group("load_scaling");
    g.sample_size(20);
    for n in [100usize, 1_000, 10_000, 100_000] {
        let db = corpus(n);
        let snap = db.snapshot().unwrap();
        // Sanity: the corpus really has `n` chunks, or every column below is
        // measuring a different curve than the one it is labelled with.
        assert_eq!(
            snap.load(1).unwrap().chunk_count(),
            n,
            "corpus shape for n={n}"
        );

        g.throughput(Throughput::Elements(n as u64));
        g.bench_function(format!("load/n={n}"), |z| {
            z.iter(|| black_box(snap.load(black_box(1))))
        });
        g.bench_function(format!("cardinality/n={n}"), |z| {
            z.iter(|| black_box(snap.cardinality(black_box(1))))
        });

        let chunks: Vec<_> = snap
            .load(1)
            .unwrap()
            .chunks()
            .map(|(p, c)| (p, c.clone()))
            .collect();
        g.bench_function(format!("from_chunks/n={n}"), |z| {
            z.iter_batched(
                || chunks.clone(),
                |v| black_box(OrdSet::from_chunks(v)),
                BatchSize::SmallInput,
            )
        });
    }
    g.finish();
}

/// Planning cost against **node count**, with the operands held tiny.
///
/// `planner-cost-is-o-chunks` closed its statistics half with
/// `STATS_MAX_CHUNKS`, and recorded that what remains is the planner's own tree
/// walk: `pass()` recurses bottom-up, and at each node its guards call
/// `is_empty_expr` ( `bounds`, `O(subtree)` ), `disjoint` ( `O(subtree)` ) and
/// `cheaper` ( `cardinality_cost` -> `yield_chunks`, `O(subtree)` ). For a
/// left-deep chain that is `O(nodes²)` per pass, times up to `MAX_PASSES`.
///
/// Leaves are **4 chunks each** so the `O(chunks)` statistics are negligible and
/// what is left on the clock is the walk. Both tree shapes are measured because
/// the quadratic term is shape-dependent: a left-deep chain has subtree sizes
/// `1..k`, a balanced tree has `O(k log k)` total, so if the walk is the cost
/// they must separate — and if they do not, the model is wrong.
fn plan_scaling(c: &mut Criterion) {
    use yesno_core::stream::dynamic::Expr;

    fn leaf(i: u64) -> Expr {
        // Four chunks, four ordinals each. Distinct per leaf so no `Arc` recurs
        // and nothing is memoizable by identity.
        let vals: Vec<u64> = (0..4u64)
            .flat_map(|ch| (0..4u64).map(move |v| (ch << 16) | (v * 97 + i)))
            .collect();
        Expr::set(Arc::new(OrdSet::from_sorted_slice(&vals)))
    }

    fn left_deep(k: usize) -> Expr {
        (1..k).fold(leaf(0), |acc, i| acc.and(leaf(i as u64)))
    }

    fn balanced(k: usize) -> Expr {
        let mut level: Vec<Expr> = (0..k).map(|i| leaf(i as u64)).collect();
        while level.len() > 1 {
            level = level
                .chunks(2)
                .map(|w| match w {
                    [a, b] => a.clone().and(b.clone()),
                    [a] => a.clone(),
                    _ => unreachable!(),
                })
                .collect();
        }
        level.pop().unwrap()
    }

    let mut g = c.benchmark_group("plan_scaling");
    g.sample_size(30);
    for k in [4usize, 8, 16, 32, 64, 128] {
        for (shape, e) in [("left", left_deep(k)), ("balanced", balanced(k))] {
            g.throughput(Throughput::Elements(k as u64));
            g.bench_function(format!("plan/{shape}/k={k}"), |z| {
                z.iter(|| black_box(yesno_core::stream::plan::plan(black_box(&e))))
            });
            // The number that decides whether planning cost matters at all.
            // `open_planned` lowers without planning, so this is execution
            // alone — and the item is only real while plan > exec.
            let planned = e.plan();
            g.bench_function(format!("exec/{shape}/k={k}"), |z| {
                z.iter(|| black_box(planned.open_planned().cardinality_dyn()))
            });
            // Same rules, **zero statistic allowance**. If this is linear while
            // `plan` above is quadratic, the residual quadratic term is the
            // `O(chunks)` statistics ( `prefix_disjoint`, `covers` ), not the
            // tree walk — which is the only way to tell them apart.
            g.bench_function(format!("noStats/{shape}/k={k}"), |z| {
                z.iter(|| {
                    black_box(black_box(&e).plan_with(&yesno_core::stream::plan::Conservative))
                })
            });
        }
    }
    g.finish();
}

/// `Expr::cardinality()` across the union cost gate, both branches.
///
/// **Nothing measured this path before.** `cardinality_cost` and
/// `decomposing_is_cheaper` decide whether a union is counted by merging or by
/// the identity `|A| + |B| - |A ∩ B|`, and every existing benchmark here calls
/// `OrdSet` methods directly — no `Expr`, no planner. When
/// `disjoint-or-is-overcharged` was fixed I re-ran the `cardinality` groups as a
/// before/after and they moved by up to 14%, which was **entirely drift**: those
/// benches cannot reach the code that changed. The control column proved it —
/// `roaring/intersection_len`, an untouched reference implementation, moved too.
///
/// Both branches are measured on purpose. The gate's history is a one-sided
/// measurement: removing it on the strength of a two-operand result sent an
/// 8-way `Or` to 27 538 allocations, because a chain decomposes recursively and
/// defeats the n-ary accumulator.
fn union_gate(c: &mut Criterion) {
    use yesno_core::stream::dynamic::Expr;

    fn leaf(base: u64, n: u64) -> Expr {
        let vals: Vec<u64> = (0..n).map(|i| (base + i) << 16).collect();
        Expr::set(Arc::new(OrdSet::from_sorted_slice(&vals)))
    }

    let mut g = c.benchmark_group("union_gate");
    for n in [10u64, 1_000] {
        // Prefix-disjoint: lowers to `Concat`, so counting is a plain sum and
        // `MERGE_STEP` must not be charged.
        let d = leaf(0, n).or(leaf(n, n));
        // Overlapping spans: a real merge, where `MERGE_STEP` is right.
        let o = leaf(0, n).or(leaf(0, n));
        // A chain, the shape whose regression the gate exists to prevent.
        let chain = (1..8u64).fold(leaf(0, n), |acc, k| acc.or(leaf(k * n, n)));

        for (name, e) in [("disjoint", &d), ("overlap", &o), ("chain8", &chain)] {
            g.bench_function(format!("{name}/n={n}"), |z| {
                z.iter(|| black_box(black_box(e).cardinality().unwrap()))
            });
        }
    }
    g.finish();
}

/// `Snapshot::load` against deriving the planner's statistics from the result.
///
/// `memoize-loads-not-statistics` rests on one measured ratio: materializing the
/// set costs **4.5x / 6.2x / 16.1x** what deriving all three statistics from it
/// costs, at 100 / 1 000 / 10 000 chunks, "and the gap widens with size". That
/// is the whole argument for memoizing below the planner rather than in it.
///
/// **Its numerator moved.** `load-is-superlinear` closed on 2026-08-26 and
/// `Snapshot::load` got 1.30x / 1.73x / 2.29x faster at those exact sizes — and
/// the *widening* was the `BTreeMap`, which is gone. So the ratio has to be
/// re-derived before anything is built on it. Measured here rather than
/// arithmetic on the old figures, because the statistics side was never measured
/// on this machine either.
fn load_vs_statistics(c: &mut Criterion) {
    use yesno_core::stream::sketch::{ChunkProfile, PrefixSketch};
    use yesno_core::Db;

    let mut g = c.benchmark_group("load_vs_statistics");
    g.sample_size(20);
    for n in [100usize, 1_000, 10_000] {
        let db = Db::new();
        for p in 0..n as u64 {
            for l in 0..4u64 {
                db.insert(1, (p << 16) | (l * 4096)).unwrap();
            }
        }
        let snap = db.snapshot().unwrap();
        assert_eq!(
            snap.load(1).unwrap().chunk_count(),
            n,
            "corpus shape for n={n}"
        );
        let set = snap.load(1).unwrap();

        g.bench_function(format!("load/n={n}"), |z| {
            z.iter(|| black_box(snap.load(black_box(1))))
        });
        // All three O(chunks) statistics the planner can derive from a set.
        g.bench_function(format!("statistics/n={n}"), |z| {
            z.iter(|| {
                let sk = PrefixSketch::build(
                    (0..set.chunk_count()).filter_map(|i| set.chunk_at(i).map(|(p, _)| p)),
                );
                let pr = ChunkProfile::of_set(&set);
                let sh = yesno_core::stream::sketch::exact_shared_prefixes(&set, &set);
                black_box((sk, pr, sh))
            })
        });
    }
    g.finish();
}

/// The one boundary `segmentation-setup-is-unbounded` left unmeasured.
///
/// That entry records segmentation winning 9.8-19x on disjoint operands with a
/// cardinality terminal and losing on `collect_set` over **overlapping** large
/// ones ( `or16/overlap/100000c` +10.5%, +17.4 ms ). Then a span-only pre-check
/// landed, and `concat_disjoint_or` began lowering prefix-disjoint unions to
/// `Concat` before `segmented_or` is reached — so segmentation's best case no
/// longer goes through it at all, and **the entry says the overlapping
/// `collect_set` boundary is unmeasured since that change.**
///
/// Overlapping operands on purpose: that is what is left for `segmented_or` now,
/// and it is where its `O(total chunks)` setup is hardest to justify.
fn segmentation_boundary(c: &mut Criterion) {
    fn overlapping(k: usize, chunks: u64) -> Vec<Arc<OrdSet>> {
        (0..k)
            .map(|j| {
                // Every operand spans the same prefix range, so spans overlap
                // completely and the pre-check cannot decline on spans alone.
                let vals: Vec<u64> = (0..chunks).map(|ch| (ch << 16) | (j as u64 * 7)).collect();
                Arc::new(OrdSet::from_sorted_slice(&vals))
            })
            .collect()
    }

    let mut g = c.benchmark_group("segmentation_boundary");
    g.sample_size(20);
    for (k, chunks) in [(2usize, 1_000u64), (16, 1_000), (2, 20_000), (16, 20_000)] {
        let sets = overlapping(k, chunks);
        // Must go through `Expr`. `segmented_or` is reached from
        // `Expr::open_planned`'s `Or` arm; `ChunkStreamExt::or` is the
        // stream-level operator and bypasses it entirely. Measuring the latter
        // and calling it a segmentation benchmark is the mistake this group was
        // written to avoid making twice.
        let e = {
            use yesno_core::stream::dynamic::Expr;
            let mut it = sets.iter().cloned().map(Expr::set);
            let first = it.next().unwrap();
            it.fold(first, |acc, s| acc.or(s))
        };
        g.bench_function(format!("collect/k={k}/c={chunks}"), |z| {
            z.iter(|| black_box(black_box(&e).open().collect_set()))
        });
    }
    g.finish();
}

/// A bitmap container of the given density, built word by word.
///
/// `d_shift` is how many independent random words are ANDed together, so the
/// density is `2^-d_shift`: 1 gives ~0.5, 2 gives ~0.25, 3 gives ~0.125. Going
/// through words rather than a sorted value list is what makes `K` distinct
/// operands cheap enough to build, and it is also the only way to hold the
/// *footprint* fixed while varying the *values*, which is what the reuse control
/// below needs.
fn bitmap_words(d_shift: u32, seed: u64) -> (Vec<u64>, u32) {
    let mut r = lcg(seed);
    let mut w = Vec::with_capacity(1024);
    for _ in 0..1024 {
        // `lcg` returns `s >> 11`, so only the low 53 bits vary; two draws are
        // combined to fill the whole word.
        let mut x = r() ^ (r() << 24);
        for _ in 1..d_shift {
            x &= r() ^ (r() << 24);
        }
        w.push(x);
    }
    let len = w.iter().map(|x| x.count_ones()).sum();
    (w, len)
}

/// A run container with exactly `nruns` intervals, spread over the chunk.
///
/// The chunk is cut into `nruns` equal cells and one interval is placed inside
/// each, never touching the cell's last position — so the intervals are
/// non-adjacent and the container really has the interval count it is labelled
/// with. That label is the whole independent variable of `run_kernel_shape`;
/// building runs by handing sorted values to `from_sorted_values` would let the
/// count drift with the seed and quietly change what the sweep is a sweep over.
fn run_intervals(nruns: usize, seed: u64) -> Vec<(u16, u16)> {
    let mut r = lcg(seed);
    let cell = 65536 / nruns;
    assert!(
        cell >= 4,
        "nruns={nruns} leaves no room for a non-adjacent run"
    );
    (0..nruns)
        .map(|k| {
            let base = k * cell;
            let off = (r() as usize) % (cell / 2);
            let len = 1 + (r() as usize) % (cell / 2 - 1);
            let s = base + off;
            let e = s + len - 1;
            (s as u16, e as u16)
        })
        .collect()
}

fn run_container(nruns: usize, seed: u64) -> yesno_core::Container {
    use yesno_core::container::RunContainer;
    let c = yesno_core::Container::Run(RunContainer::from_pairs(&run_intervals(nruns, seed)));
    assert_eq!(
        match &c {
            yesno_core::Container::Run(r) => r.nruns() as usize,
            _ => unreachable!(),
        },
        nruns,
        "the generator must produce the interval count the sweep is labelled with"
    );
    c
}

/// **The reused-operand control, applied to the bitmap kernels.**
///
/// `intersect_branch_history` established that a group intersecting the *same
/// two containers* every iteration reports the scalar `array x array` merge 4.4x
/// faster than a query ever sees it, because the branch predictor memorizes the
/// merge's decision sequence. Every bitmap figure this file quotes comes from a
/// group with the same construction — `intersect_crossover` reuses one pair,
/// `cardinality_mixed` reuses one `OrdSet` pair — so the same question has to be
/// asked of them before any of those numbers is quoted again.
///
/// The mechanism cannot be the same one: a bitmap word loop is branchless, so
/// there is no history to memorize. What a reused pair *does* buy a bitmap
/// kernel is residency — two operands are 16 KiB and stay in L1 forever, where a
/// stream of chunks does not. So the three arms are the same three, and the
/// reading is the same reading:
///
/// * `one_pair` — one pair, repeated. 16 KiB live.
/// * `k_clones` — `K` pairs holding the **same** words. Same footprint and
///   stride as `k_distinct`, same word values as `one_pair`.
/// * `k_distinct` — `K` pairs holding **different** words, laid out identically.
///
/// `one_pair` vs `k_clones` isolates footprint; `k_clones` vs `k_distinct`
/// isolates the values. For a branchless kernel the second gap should be zero,
/// and saying so with a measurement is the point.
fn bitmap_kernel_reuse(c: &mut Criterion) {
    use yesno_core::container::BitmapContainer;
    use yesno_core::Container;

    const K: usize = 64;
    let mut g = c.benchmark_group("bitmap_kernel_reuse");
    g.sample_size(50);
    g.warm_up_time(std::time::Duration::from_secs(1));
    g.measurement_time(std::time::Duration::from_secs(2));

    for d_shift in [1u32, 3] {
        let bm = |seed: u64| {
            let (w, n) = bitmap_words(d_shift, seed);
            Container::Bitmap(BitmapContainer::from_words(w, n))
        };
        let (o_a, o_b) = (bm(0xB1), bm(0xB2));
        let clones: Vec<(Container, Container)> =
            (0..K).map(|_| (o_a.clone(), o_b.clone())).collect();
        let distinct: Vec<(Container, Container)> = (0..K)
            .map(|k| (bm(0x1000 + k as u64), bm(0x9000 + k as u64)))
            .collect();

        // **The `arrow-buffer` column is not decoration.** ARCHITECTURE's
        // containment policy says to reuse Arrow where it is good, so a
        // hand-written bitmap kernel has to be shown to beat what is already a
        // dependency rather than merely to exist. Two arms, because Arrow's
        // shape is not this crate's shape:
        //
        // * `arrowCard` — `bit_chunks().zip(..).map(count_ones).sum()`, which is
        //   Arrow's non-allocating equivalent of `and_cardinality`.
        // * `arrowAnd` — `from_bitwise_binary_op` then `count_set_bits`, which
        //   is the closest Arrow gets to `apply` with a cardinality. Arrow has
        //   **no fused binary-op-plus-popcount**: the count is a second pass
        //   over the 8 KiB result, which is exactly the pass this module's
        //   header says the fused loop exists to avoid.
        let arrow_pairs: Vec<(arrow_buffer::BooleanBuffer, arrow_buffer::BooleanBuffer)> = (0..K)
            .map(|k| {
                let mk = |seed: u64| {
                    let (w, _) = bitmap_words(d_shift, seed);
                    arrow_buffer::BooleanBuffer::new(arrow_buffer::Buffer::from_vec(w), 0, 1 << 16)
                };
                (mk(0x1000 + k as u64), mk(0x9000 + k as u64))
            })
            .collect();
        // Same answer as the crate's arm, or the column means nothing.
        for ((a, b), (x, y)) in distinct.iter().zip(&arrow_pairs) {
            let n: u32 = x
                .bit_chunks()
                .iter()
                .zip(y.bit_chunks().iter())
                .map(|(p, q)| (p & q).count_ones())
                .sum();
            assert_eq!(n, yesno_core::ops::and_cardinality(a, b), "arrow disagrees");
        }
        g.bench_function(format!("arrowCard/k_distinct/d=2^-{d_shift}"), |z| {
            z.iter(|| {
                let mut n = 0u64;
                for (x, y) in &arrow_pairs {
                    n += x
                        .bit_chunks()
                        .iter()
                        .zip(y.bit_chunks().iter())
                        .map(|(p, q)| (p & q).count_ones())
                        .sum::<u32>() as u64;
                }
                black_box(n)
            })
        });
        g.bench_function(format!("arrowAnd/k_distinct/d=2^-{d_shift}"), |z| {
            z.iter(|| {
                let mut n = 0u64;
                for (x, y) in &arrow_pairs {
                    let r = arrow_buffer::BooleanBuffer::from_bitwise_binary_op(
                        x.values(),
                        0,
                        y.values(),
                        0,
                        1 << 16,
                        |p, q| p & q,
                    );
                    n += r.count_set_bits() as u64;
                }
                black_box(n)
            })
        });

        // Both operands really are bitmaps, or this measures another kernel.
        assert_eq!(o_a.kind(), yesno_core::ContainerKind::Bitmap);

        g.throughput(Throughput::Elements(K as u64));
        for (name, pairs) in [("k_clones", &clones), ("k_distinct", &distinct)] {
            g.bench_function(format!("card/{name}/d=2^-{d_shift}"), |z| {
                z.iter(|| {
                    let mut n = 0u64;
                    for (a, b) in pairs {
                        n += yesno_core::ops::and_cardinality(a, b) as u64;
                    }
                    black_box(n)
                })
            });
            g.bench_function(format!("and/{name}/d=2^-{d_shift}"), |z| {
                z.iter(|| {
                    let mut n = 0u64;
                    for (a, b) in pairs {
                        n += yesno_core::ops::and(a, b).map_or(0, |c| c.len()) as u64;
                    }
                    black_box(n)
                })
            });
        }
        g.bench_function(format!("card/one_pair/d=2^-{d_shift}"), |z| {
            z.iter(|| {
                let mut n = 0u64;
                for _ in 0..K {
                    n += yesno_core::ops::and_cardinality(black_box(&o_a), black_box(&o_b)) as u64;
                }
                black_box(n)
            })
        });
        // The other three ops, on the distinct arm only. They differ from `and`
        // in one vector instruction, but `AndNot` also differs in *operand
        // order*, and `Or` / `Xor` produce a result too dense to demote where
        // `And` at `d = 2^-3` produces one that is not — so the four do not
        // share a cost even though they share a loop.
        for (name, f) in [
            ("or", yesno_core::ops::or as fn(_, _) -> _),
            ("xor", yesno_core::ops::xor),
            ("andnot", yesno_core::ops::and_not),
        ] {
            g.bench_function(format!("{name}/k_distinct/d=2^-{d_shift}"), |z| {
                z.iter(|| {
                    let mut n = 0u64;
                    for (a, b) in &distinct {
                        n += f(a, b).map_or(0, |c| c.len()) as u64;
                    }
                    black_box(n)
                })
            });
        }
        g.bench_function(format!("and/one_pair/d=2^-{d_shift}"), |z| {
            z.iter(|| {
                let mut n = 0u64;
                for _ in 0..K {
                    n += yesno_core::ops::and(black_box(&o_a), black_box(&o_b))
                        .map_or(0, |c| c.len()) as u64;
                }
                black_box(n)
            })
        });
    }
    g.finish();
}

/// The two bitmap **predicates** against the count that is supposed to bound
/// them.
///
/// `ops::card` records the invariant that `is_disjoint(a, b)` must never cost
/// more than `and_cardinality(a, b) == 0`, and its bitmap arms are written as
/// `all(|(p, q)| p & q == 0)` — a short-circuiting reducer, which is a different
/// kernel from the counting one whatever it looks like in source. The `roaring`
/// crate is the reference for the pair; there is no container-level public API
/// for it, so the reference column here is `and_cardinality` itself, which is
/// the thing the invariant names.
///
/// Two shapes, because the predicates have two costs:
///
/// * `late` — genuinely disjoint / genuinely a subset, so no exit is possible
///   and the whole 8 KiB is walked. This is what the invariant is about.
/// * `early` — the answer is settled by the first word. This is what an
///   early exit buys, and what any blocked or widened rewrite risks.
fn bitmap_predicate_vs_count(c: &mut Criterion) {
    use yesno_core::container::BitmapContainer;
    use yesno_core::Container;

    let bmp = |w: Vec<u64>| {
        let n = w.iter().map(|x| x.count_ones()).sum();
        Container::Bitmap(BitmapContainer::from_words(w, n))
    };
    // Disjoint by construction over the whole 8 KiB: `a` holds only even bits,
    // `b` only odd ones, so no word pair intersects and neither predicate can
    // exit before the end.
    let (wa, _) = bitmap_words(1, 0xD1);
    let even: Vec<u64> = wa.iter().map(|w| w & 0x5555_5555_5555_5555).collect();
    let odd: Vec<u64> = wa.iter().map(|w| !w & 0xAAAA_AAAA_AAAA_AAAA).collect();
    let (a_late, b_late) = (bmp(even.clone()), bmp(odd.clone()));
    // Same operands, but `b` also carries a bit `a` has, in word 0.
    let mut early = odd;
    early[0] |= even[0];
    let b_early = bmp(early);
    // Superset / subset: `sub` is every other word of `sup`.
    let sub_words: Vec<u64> = wa
        .iter()
        .enumerate()
        .map(|(i, w)| if i % 2 == 0 { *w } else { 0 })
        .collect();
    let sup = bmp(wa.clone());
    let sub = bmp(sub_words.clone());
    let mut miss = sub_words;
    miss[0] |= !wa[0];
    let sub_early = bmp(miss);

    assert!(yesno_core::ops::is_disjoint(&a_late, &b_late));
    assert!(!yesno_core::ops::is_disjoint(&a_late, &b_early));
    assert!(yesno_core::ops::contains_all(&sup, &sub));
    assert!(!yesno_core::ops::contains_all(&sup, &sub_early));

    let mut g = c.benchmark_group("bitmap_predicate_vs_count");
    g.sample_size(50);
    g.warm_up_time(std::time::Duration::from_secs(1));
    g.measurement_time(std::time::Duration::from_secs(2));
    g.bench_function("is_disjoint/late", |z| {
        z.iter(|| black_box(yesno_core::ops::is_disjoint(&a_late, &b_late)))
    });
    g.bench_function("is_disjoint/early", |z| {
        z.iter(|| black_box(yesno_core::ops::is_disjoint(&a_late, &b_early)))
    });
    // The bound the invariant states, measured rather than argued.
    g.bench_function("and_cardinality_eq_0/late", |z| {
        z.iter(|| black_box(yesno_core::ops::and_cardinality(&a_late, &b_late) == 0))
    });
    g.bench_function("contains_all/late", |z| {
        z.iter(|| black_box(yesno_core::ops::contains_all(&sup, &sub)))
    });
    g.bench_function("contains_all/early", |z| {
        z.iter(|| black_box(yesno_core::ops::contains_all(&sup, &sub_early)))
    });
    g.bench_function("and_cardinality_eq_len/late", |z| {
        z.iter(|| black_box(yesno_core::ops::and_cardinality(&sup, &sub) == sub.len()))
    });
    g.finish();
}

/// The run kernels against **interval count**, which is the variable that
/// decides them.
///
/// `cardinality_mixed` and `binary_ops_mixed` build their run operand from
/// `(0..30_000).map(|i| (ch << 16) | i)` — one contiguous stretch per chunk, so
/// **every run container in this file has exactly one interval**, and
/// `run_x_run` additionally passes the same `OrdSet` on both sides. `run x run`
/// cardinality at one interval is a single comparison; quoting a ratio from it
/// says nothing about a run container with a thousand intervals, which is the
/// shape `RUN_MAX_INTERVALS` exists to bound. So the sweep is over `nruns`, and
/// `K` distinct pairs per point for the reason `intersect_branch_history` gives.
fn run_kernel_shape(c: &mut Criterion) {
    use yesno_core::container::BitmapContainer;
    use yesno_core::Container;

    const K: usize = 32;
    let mut g = c.benchmark_group("run_kernel_shape");
    g.sample_size(50);
    g.warm_up_time(std::time::Duration::from_secs(1));
    g.measurement_time(std::time::Duration::from_secs(2));

    for nruns in [1usize, 8, 64, 512, 2048] {
        let runs_a: Vec<Container> = (0..K)
            .map(|k| run_container(nruns, 0x300 + k as u64))
            .collect();
        let runs_b: Vec<Container> = (0..K)
            .map(|k| run_container(nruns, 0x700 + k as u64))
            .collect();
        let bms: Vec<Container> = (0..K)
            .map(|k| {
                let (w, n) = bitmap_words(1, 0xC00 + k as u64);
                Container::Bitmap(BitmapContainer::from_words(w, n))
            })
            .collect();

        g.throughput(Throughput::Elements(K as u64));
        g.bench_function(format!("run_x_run/card/n={nruns}"), |z| {
            z.iter(|| {
                let mut n = 0u64;
                for (a, b) in runs_a.iter().zip(&runs_b) {
                    n += yesno_core::ops::and_cardinality(a, b) as u64;
                }
                black_box(n)
            })
        });
        g.bench_function(format!("run_x_run/disjoint/n={nruns}"), |z| {
            z.iter(|| {
                let mut n = 0u64;
                for (a, b) in runs_a.iter().zip(&runs_b) {
                    n += u64::from(yesno_core::ops::is_disjoint(a, b));
                }
                black_box(n)
            })
        });
        g.bench_function(format!("bitmap_x_run/card/n={nruns}"), |z| {
            z.iter(|| {
                let mut n = 0u64;
                for (a, b) in bms.iter().zip(&runs_b) {
                    n += yesno_core::ops::and_cardinality(a, b) as u64;
                }
                black_box(n)
            })
        });
        g.bench_function(format!("bitmap_x_run/and/n={nruns}"), |z| {
            z.iter(|| {
                let mut n = 0u64;
                for (a, b) in bms.iter().zip(&runs_b) {
                    n += yesno_core::ops::and(a, b).map_or(0, |c| c.len()) as u64;
                }
                black_box(n)
            })
        });
        // `AndNot` is the one op that does **not** normalize the pair order,
        // so `run \ bitmap` is a different arm from `bitmap \ run` and has to be
        // measured as one.
        g.bench_function(format!("bitmap_x_run/andnot/n={nruns}"), |z| {
            z.iter(|| {
                let mut n = 0u64;
                for (a, b) in bms.iter().zip(&runs_b) {
                    n += yesno_core::ops::and_not(a, b).map_or(0, |c| c.len()) as u64;
                }
                black_box(n)
            })
        });
        g.bench_function(format!("run_x_bitmap/andnot/n={nruns}"), |z| {
            z.iter(|| {
                let mut n = 0u64;
                for (a, b) in runs_b.iter().zip(&bms) {
                    n += yesno_core::ops::and_not(a, b).map_or(0, |c| c.len()) as u64;
                }
                black_box(n)
            })
        });
    }
    g.finish();
}

/// Run x run at **skewed** interval counts, which nothing else here measures.
///
/// Every other run x run group in this file — `run_kernel_shape`,
/// `cardinality_mixed`, `cardinality_mixed_distinct`, `binary_ops_mixed`,
/// `predicate_paths` — passes the **same `nruns` on both sides**. So the whole
/// file could report a run kernel as healthy while it walked 1024 intervals to
/// intersect them with 8, which is the shape `ops::array` has had a gallop for
/// since it was written and `ops::run` had none. A group that only ever measures
/// balanced operands cannot see an `O(n + m)` where an `O(min · log max)` was
/// available.
///
/// Construction, and each point of it is load-bearing:
///
/// * **Distinct operands**, `K` pairs per point, for the reason
///   `intersect_branch_history` gives — a reused pair lets the predictor memorize
///   the walk, and the run merge is exactly as branch-bound as the array one.
/// * **Both operand orders.** The kernels are written as `x` against `y` and the
///   small side may be either; an arm that gallops only when the *left* side is
///   the small one is a real and easily-missed bug.
/// * **Ratios either side of [`GALLOP_RATIO`]** ( 32 ): 128:1 and 32:1 are
///   galloped, 16:1 is not, and the balanced points are the in-group control that
///   says whether the gallop decision costs anything where it does not fire.
/// * `contains_all` gets a **true** case, built by thinning the large operand, or
///   it would answer `false` at the first interval and measure nothing.
fn run_kernel_skew(c: &mut Criterion) {
    use yesno_core::container::RunContainer;
    use yesno_core::Container;

    const K: usize = 32;

    /// Every `stride`-th interval of `big`. Dropping intervals cannot make two
    /// of them adjacent, so the result is still a valid run container, and it is
    /// a subset by construction — which is what gives `contains_all` a `true`.
    fn thin(big: &[(u16, u16)], stride: usize) -> Vec<(u16, u16)> {
        big.iter().step_by(stride).copied().collect()
    }

    let mut g = c.benchmark_group("run_kernel_skew");
    g.sample_size(50);
    g.warm_up_time(std::time::Duration::from_secs(1));
    g.measurement_time(std::time::Duration::from_secs(2));

    // (small, large): 128:1 and 32:1 gallop, 16:1 does not, and 1:1 is the
    // control for what the decision costs when it declines.
    for (ns, nl) in [(8usize, 1024usize), (32, 1024), (64, 1024), (1024, 1024)] {
        let bigs: Vec<Vec<(u16, u16)>> = (0..K)
            .map(|k| run_intervals(nl, 0x900 + k as u64))
            .collect();
        let large: Vec<Container> = bigs
            .iter()
            .map(|p| Container::Run(RunContainer::from_pairs(p)))
            .collect();
        let small: Vec<Container> = (0..K)
            .map(|k| {
                Container::Run(RunContainer::from_pairs(&run_intervals(
                    ns,
                    0xD00 + k as u64,
                )))
            })
            .collect();
        // A genuine subset of the large operand with `ns` intervals.
        let subset: Vec<Container> = bigs
            .iter()
            .map(|p| Container::Run(RunContainer::from_pairs(&thin(p, nl / ns))))
            .collect();

        g.throughput(Throughput::Elements(K as u64));
        for (tag, l, r) in [
            ("small_x_large", &small, &large),
            ("large_x_small", &large, &small),
        ] {
            g.bench_function(format!("card/{tag}/{ns}x{nl}"), |z| {
                z.iter(|| {
                    let mut n = 0u64;
                    for (a, b) in l.iter().zip(r.iter()) {
                        n += yesno_core::ops::and_cardinality(a, b) as u64;
                    }
                    black_box(n)
                })
            });
            g.bench_function(format!("disjoint/{tag}/{ns}x{nl}"), |z| {
                z.iter(|| {
                    let mut n = 0u64;
                    for (a, b) in l.iter().zip(r.iter()) {
                        n += u64::from(yesno_core::ops::is_disjoint(a, b));
                    }
                    black_box(n)
                })
            });
            g.bench_function(format!("and/{tag}/{ns}x{nl}"), |z| {
                z.iter(|| {
                    let mut n = 0u64;
                    for (a, b) in l.iter().zip(r.iter()) {
                        n += yesno_core::ops::and(a, b).map_or(0, |c| c.len()) as u64;
                    }
                    black_box(n)
                })
            });
            g.bench_function(format!("andnot/{tag}/{ns}x{nl}"), |z| {
                z.iter(|| {
                    let mut n = 0u64;
                    for (a, b) in l.iter().zip(r.iter()) {
                        n += yesno_core::ops::and_not(a, b).map_or(0, |c| c.len()) as u64;
                    }
                    black_box(n)
                })
            });
        }
        // `contains_all` is asymmetric — `b` drives and the sides cannot be
        // swapped — so the only skew it can exploit is a large `a` over a small
        // `b`, and the answer has to be `true` or the walk stops immediately.
        g.bench_function(format!("contains/large_superset/{ns}x{nl}"), |z| {
            z.iter(|| {
                let mut n = 0u64;
                for (a, b) in large.iter().zip(subset.iter()) {
                    n += u64::from(yesno_core::ops::contains_all(a, b));
                }
                black_box(n)
            })
        });
    }
    g.finish();
}

/// The same skew against the reference, at `OrdSet` level so `roaring` can be
/// asked the same question.
///
/// Both sides are run-encoded — ours by `optimize()`, theirs by
/// [`theirs_runs`]. A `roaring` bitmap built from these values without
/// `optimize()` would be held as a *bitmap*, and the ratio would then be a
/// statement about representation rather than about either kernel.
fn run_skew_vs_reference(c: &mut Criterion) {
    fn runny(nruns: usize, seed: u64) -> Vec<u32> {
        (0..8u32)
            .flat_map(|ch| {
                run_intervals(nruns, seed + ch as u64)
                    .into_iter()
                    .flat_map(move |(s, e)| (s..=e).map(move |v| (ch << 16) | v as u32))
            })
            .collect()
    }

    let mut g = c.benchmark_group("run_skew_vs_reference");
    g.sample_size(50);
    g.warm_up_time(std::time::Duration::from_secs(1));
    g.measurement_time(std::time::Duration::from_secs(2));

    for (ns, nl) in [(8usize, 1024usize), (32, 1024)] {
        let sv = runny(ns, 0x11);
        let lv = runny(nl, 0x8800);
        let (s, l) = (ours(&sv), ours(&lv));
        use yesno_core::ContainerKind::Run;
        assert!(
            s.chunks().all(|(_, c)| c.kind() == Run) && l.chunks().all(|(_, c)| c.kind() == Run),
            "both operands must be run-encoded at {ns}x{nl}"
        );
        let (rs, rl) = (theirs_runs(&sv), theirs_runs(&lv));

        g.bench_function(format!("yesno/small_x_large/{ns}x{nl}"), |z| {
            z.iter(|| black_box(s.and_cardinality(&l)))
        });
        g.bench_function(format!("roaring/small_x_large/{ns}x{nl}"), |z| {
            z.iter(|| black_box(rs.intersection_len(&rl)))
        });
        g.bench_function(format!("yesno/large_x_small/{ns}x{nl}"), |z| {
            z.iter(|| black_box(l.and_cardinality(&s)))
        });
        g.bench_function(format!("roaring/large_x_small/{ns}x{nl}"), |z| {
            z.iter(|| black_box(rl.intersection_len(&rs)))
        });
    }
    g.finish();
}

/// `cardinality_mixed` again, with the two defects removed: distinct operands,
/// and a run operand whose interval count is a parameter rather than 1.
///
/// This is the group that says whether `kernel-specialization-simd`'s "`run x
/// run` cardinality is **35x** faster than the reference and `bitmap x run`
/// **2.3x**" survives. Same reference ( `roaring::intersection_len` ), same
/// eight chunks, same container kinds — the only changes are that the two
/// operands are no longer the *same set* and that the runs are no longer one
/// interval per chunk.
fn cardinality_mixed_distinct(c: &mut Criterion) {
    fn runny(nruns: usize, seed: u64) -> Vec<u32> {
        (0..8u32)
            .flat_map(|ch| {
                run_intervals(nruns, seed + ch as u64)
                    .into_iter()
                    .flat_map(move |(s, e)| (s..=e).map(move |v| (ch << 16) | v as u32))
            })
            .collect()
    }
    let dense: Vec<u32> = (0..8u32)
        .flat_map(|ch| (0..20_000u32).map(move |i| (ch << 16) | ((i * 3) % 65536)))
        .collect::<std::collections::BTreeSet<u32>>()
        .into_iter()
        .collect();

    let mut g = c.benchmark_group("cardinality_mixed_distinct");
    g.sample_size(50);
    g.warm_up_time(std::time::Duration::from_secs(1));
    g.measurement_time(std::time::Duration::from_secs(2));
    for nruns in [1usize, 64, 1024] {
        let ra_v = runny(nruns, 0x50);
        let rb_v = runny(nruns, 0x4000);
        let (ra, rb) = (ours(&ra_v), ours(&rb_v));
        let bm = ours(&dense);
        use yesno_core::ContainerKind::*;
        assert!(
            ra.chunks().all(|(_, c)| c.kind() == Run),
            "want runs at n={nruns}, got {:?}",
            ra.chunks().map(|(_, c)| c.kind()).collect::<Vec<_>>()
        );
        assert!(bm.chunks().all(|(_, c)| c.kind() == Bitmap), "want bitmaps");
        let (rra, rrb, rbm) = (theirs(&ra_v), theirs(&rb_v), theirs(&dense));

        g.bench_function(format!("yesno/run_x_run/n={nruns}"), |z| {
            z.iter(|| black_box(ra.and_cardinality(&rb)))
        });
        g.bench_function(format!("roaring/run_x_run/n={nruns}"), |z| {
            z.iter(|| black_box(rra.intersection_len(&rrb)))
        });
        g.bench_function(format!("yesno/bitmap_x_run/n={nruns}"), |z| {
            z.iter(|| black_box(bm.and_cardinality(&rb)))
        });
        g.bench_function(format!("roaring/bitmap_x_run/n={nruns}"), |z| {
            z.iter(|| black_box(rbm.intersection_len(&rrb)))
        });
    }
    g.finish();
}

/// The interval two-pointer as `ops::run` had it before the vector arm — the
/// hoisted, gallop-free counting merge. The baseline every number in
/// [`run_vector_arm`] is quoted against.
///
/// A copy, deliberately, exactly as [`slice_merge_card`] is a copy of the
/// pre-vector array merge. `ops::run::scalar_merge_cardinality` is private and
/// making it public to be benchmarked would put a kernel in the public API,
/// which R1 / R6 / R7 turn into a semver promise. A copy in the bench file costs
/// a divergence risk that the group's own equal-answer assertion catches.
///
/// **It takes the container's own flat payload, not a `Vec<(start, end)>`.**
/// The first version of this baseline walked `(start, end)` tuples built by
/// [`run_intervals`] — one add per advance *less* than the kernel it was
/// standing in for, and it measured **41% slower** at `nruns = 2048`, which
/// would have inflated the arm's apparent gain from 2.13x to 3.00x. A baseline
/// on a different representation is a different kernel; the only faithful one
/// reads the same bytes the crate reads and computes the end the same way.
///
/// **Even so, a copy is a different compilation of the same algorithm.**
/// This one measures **7-21% faster** than the identical source inside
/// `yesno-core` ( 79.1 against 96.1 ns per pair at `nruns = 64`; 3 763 against
/// 4 017 at 2 048 ), so every ratio this group reports is a **lower bound** on
/// what the change bought. The before-and-after figure comes from building
/// `ops::run` twice and is recorded in that module's header, not here.
fn interval_merge_card(xf: &[u16], yf: &[u16]) -> u32 {
    fn cast(f: &[u16]) -> &[[u16; 2]] {
        bytemuck::cast_slice(&f[..f.len() & !1])
    }
    let (xp, yp) = (cast(xf), cast(yf));
    // The sparser side first, as the merge branch does.
    let (xp, yp) = if xp.len() <= yp.len() {
        (xp, yp)
    } else {
        (yp, xp)
    };
    let (mut xi, mut yi) = (xp.iter(), yp.iter());
    let (Some(&x0), Some(&y0)) = (xi.next(), yi.next()) else {
        return 0;
    };
    let (mut xs, mut xe) = (x0[0], x0[0] + x0[1]);
    let (mut ys, mut ye) = (y0[0], y0[0] + y0[1]);
    let mut total = 0u32;
    loop {
        let (s, t) = (xs.max(ys), xe.min(ye));
        if s <= t {
            total += (t - s) as u32 + 1;
        }
        if xe < ye {
            let Some(&v) = xi.next() else { return total };
            (xs, xe) = (v[0], v[0] + v[1]);
        } else {
            let Some(&v) = yi.next() else { return total };
            (ys, ye) = (v[0], v[0] + v[1]);
        }
    }
}

/// What the vector `run x run` counting merge is worth, and where it is worth
/// nothing.
///
/// `K` distinct pairs per point, for the reason `intersect_branch_history`
/// establishes: the interval merge takes one data-dependent branch per interval
/// retired and is exactly as memorizable as the array one, so a reused pair
/// would report the scalar baseline faster than a query ever sees it and
/// understate the arm by the same factor.
///
/// Three blocks of rows, and each answers a different question:
///
/// * **Balanced, from below the block width upward.** `n = 1` and `n = 4` cannot
///   run a block at all, so the arm must decline there and not lose: paying for
///   the decision is the whole cost of a call that takes 2 ns. The `1x1` row
///   still reads as **0.44x**, and that is not the arm — the baseline is a
///   direct slice call while the crate column pays the `Container` match, the
///   `as_flat`, and the gallop gate, together **2.15 ns** on a 3.80 ns call.
///   Below about `n = 8` this group measures dispatch, not kernels.
/// * **Skewed but under [`GALLOP_RATIO`]**, where the merge is still what the
///   crate runs, so the block kernel is what the ratio measures.
/// * **Skewed past the ratio.** Here the crate takes the *probe* and this
///   file's baseline does not, so those ratios are the gallop Tier 1 landed and
///   **not** this arm. They are in the group for one reason: run
///   unconditionally, the block kernel measured **0.62-0.75x** at 8 x 1024 and
///   8 x 2048, so a row that showed the crate near the merge baseline there
///   would mean the gate had stopped working. `run_kernel_skew` is the group
///   that prices the gallop itself.
fn run_vector_arm(c: &mut Criterion) {
    use yesno_core::container::RunContainer;
    use yesno_core::Container;

    const K: usize = 32;
    let mut g = c.benchmark_group("run_vector_arm");
    g.sample_size(50);
    g.warm_up_time(std::time::Duration::from_secs(1));
    g.measurement_time(std::time::Duration::from_secs(2));

    /// The two run containers of one pair, and their flat payloads.
    fn flat(c: &Container) -> &[u16] {
        match c {
            Container::Run(r) => r.as_flat(),
            _ => unreachable!("the group builds runs"),
        }
    }

    for (na, nb) in [
        (1usize, 1usize),
        (4, 4),
        (8, 8),
        (16, 16),
        (32, 32),
        (64, 64),
        (256, 256),
        (1024, 1024),
        (2048, 2048),
        // Skewed but under GALLOP_RATIO: still the merge, so still the block.
        (64, 1024),
        (128, 1024),
        // Skewed past GALLOP_RATIO: the gallop, which the arm must leave alone.
        (32, 1024),
        (8, 1024),
        (8, 2048),
    ] {
        let runs: Vec<(Container, Container)> = (0..K)
            .map(|k| {
                (
                    Container::Run(RunContainer::from_pairs(&run_intervals(
                        na,
                        0x300 + k as u64,
                    ))),
                    Container::Run(RunContainer::from_pairs(&run_intervals(
                        nb,
                        0x700 + k as u64,
                    ))),
                )
            })
            .collect();
        let flats: Vec<(&[u16], &[u16])> = runs.iter().map(|(a, b)| (flat(a), flat(b))).collect();
        // Same answer from both kernels, or the comparison is meaningless.
        for ((ca, cb), (a, b)) in runs.iter().zip(&flats) {
            assert_eq!(
                yesno_core::ops::and_cardinality(ca, cb),
                interval_merge_card(a, b),
                "the bench's own baseline disagrees with the crate at {na} x {nb}"
            );
        }

        g.throughput(Throughput::Elements(K as u64));
        g.bench_function(format!("card/scalar/{na}x{nb}"), |z| {
            z.iter(|| {
                let mut n = 0u64;
                for (a, b) in &flats {
                    n += interval_merge_card(a, b) as u64;
                }
                black_box(n)
            })
        });
        g.bench_function(format!("card/crate/{na}x{nb}"), |z| {
            z.iter(|| {
                let mut n = 0u64;
                for (a, b) in &runs {
                    n += yesno_core::ops::and_cardinality(a, b) as u64;
                }
                black_box(n)
            })
        });
    }
    g.finish();
}

criterion_group!(
    benches,
    bitmap_kernel_reuse,
    bitmap_predicate_vs_count,
    run_kernel_shape,
    run_vector_arm,
    run_kernel_skew,
    run_skew_vs_reference,
    cardinality_mixed_distinct,
    segmentation_boundary,
    load_vs_statistics,
    union_gate,
    binary_ops,
    binary_ops_dense,
    binary_ops_mixed,
    cardinality_paths,
    cardinality_mixed,
    and_cardinality_sparse_vs_dense,
    nested_expression,
    build,
    serde,
    predicate_paths,
    intersect_branch_history,
    intersect_vector_arm,
    intersect_crossover,
    intersect_crossover_streamed,
    load_scaling,
    plan_scaling
);
criterion_main!(benches);
