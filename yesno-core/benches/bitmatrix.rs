//! Benchmarks for the packed bit-matrix algebra.
//!
//! # There is no external reference, and this file does not invent one
//!
//! `benches/setops.rs` measures against the `roaring` crate as an *absolute*
//! reference, deliberately, so a regression reads as "3× slower than the
//! reference implementation" rather than "8% slower than last month". No such
//! reference exists here — `roaring` has no matrix algebra — so the honest
//! substitutes are:
//!
//! 1. **An alternative algorithm on identical operands.** The delta-swap
//!    transpose against a bit-by-bit one; fused `gemm` against `mul` then `add`.
//!    These are two implementations of one specification on one input, which is
//!    what a design decision actually needs.
//! 2. **Scaling curves.** How cost moves with size, density and semiring says
//!    more than any single number, and it is what the specialization arms in
//!    step 7 have to be argued from.
//!
//! **The baselines here are bench-local, and that biases every ratio the same
//! way.** `setops.rs` records that a bench-local copy of `ops::run`'s scalar
//! merge measured **7–21% faster than the identical source inside the crate** —
//! a different compilation of the same algorithm. So a bench-local baseline
//! looks *better* than it is, and every speedup reported here is therefore a
//! **lower bound**. Do not read an apparent tie as "no win".
//!
//! The alternative would be shipping reference implementations inside
//! `yesno-core` as `#[doc(hidden)] pub`. That is refused: `CLAUDE.md` is
//! explicit that measurement code does not go in `src/`, and a `pub` item there
//! is a semver promise for something nothing calls. Stating the bias is the
//! cheaper correct answer.
//!
//! Benchmarks are not a gate ( QG §1 ). They are a finding-generator, and per
//! QG §4 they are the *required* evidence before any specialized arm ships.

use criterion::{criterion_group, criterion_main, BatchSize, Criterion, Throughput};
use std::hint::black_box;
use yesno_core::matrix::{BitMatrix, Layout, MatrixSink, Order, Semiring};
use yesno_core::OrdSet;

/// Deterministic bit source; no RNG dev-dependency, matching `setops.rs`.
fn lcg(seed: u64) -> impl FnMut() -> u64 {
    let mut s = seed;
    move || {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        s >> 11
    }
}

/// A matrix with roughly `1 / one_in` of its bits set.
///
/// Density is the axis `gemm` cost actually tracks — it is
/// `O(nnz(A) · ceil(N/64))`, not `O(M·K·N)` — so a fixture that only ever
/// generates one density measures one point on the curve and reports it as the
/// cost of the operation.
fn dense_by(rows: u32, cols: u32, one_in: u64, seed: u64) -> BitMatrix {
    let mut r = lcg(seed);
    let mut m = BitMatrix::zeros(rows, cols);
    for i in 0..rows {
        for j in 0..cols {
            if r().is_multiple_of(one_in) {
                m.set(i, j, true);
            }
        }
    }
    m
}

/// Invertible by construction: the identity under random elementary row
/// operations. A random matrix is singular often enough that timing one would
/// be timing the early exit.
fn invertible(n: u32, seed: u64) -> BitMatrix {
    let mut r = lcg(seed);
    let mut m = BitMatrix::identity(n);
    for _ in 0..(4 * n) {
        let a = (r() % n as u64) as u32;
        let b = (r() % n as u64) as u32;
        if a != b {
            // Row `a` ^= row `b`, through the public API only — `xor_row_into`
            // is crate-private and a bench is a separate crate.
            for c in 0..n {
                if m.get(b, c) {
                    let v = m.get(a, c);
                    m.set(a, c, !v);
                }
            }
        }
    }
    m
}

/// The element-by-element transpose: `O(rows * cols)` whatever the contents.
///
/// This is **not** the baseline that can show a sparse crossover — it visits
/// every cell, so it is content-independent and always loses. Comparing only
/// against this would answer a different question than the one the module doc
/// asks. See [`scatter_transpose`].
fn naive_transpose(m: &BitMatrix) -> BitMatrix {
    let mut out = BitMatrix::zeros(m.cols(), m.rows());
    for r in 0..m.rows() {
        for c in 0..m.cols() {
            if m.get(r, c) {
                out.set(c, r, true);
            }
        }
    }
    out
}

/// The set-bit-walking transpose: `O(nnz)`, and the real rival on sparse input.
///
/// The delta swap costs `O(rows * cols / 4096)` blocks regardless of contents,
/// so *this* is the baseline that locates the crossover — the evidence a sparse
/// arm would need under QG §4.
fn scatter_transpose(m: &BitMatrix) -> BitMatrix {
    let mut out = BitMatrix::zeros(m.cols(), m.rows());
    for r in 0..m.rows() {
        for (wi, &word) in m.row_words(r).iter().enumerate() {
            let mut w = word;
            while w != 0 {
                let c = (wi * 64) as u32 + w.trailing_zeros();
                out.set(c, r, true);
                w &= w - 1;
            }
        }
    }
    out
}

// ---------------------------------------------------------------- read/write

/// Reading a matrix out of an `OrdSet`, across every axis that changes the
/// work: container kind, chunk containment, layout padding, and order.
///
/// `M*N < 65536` does not imply chunk containment. 100×100 is 10 000 bits, so
/// matrix 6 spans 60 000..70 000 and straddles; matrix 0 does not. Both are
/// measured, because the straddling case is the one the fast path in step 2
/// would have to decline.
fn read_conditions(c: &mut Criterion) {
    let mut g = c.benchmark_group("bitmatrix/read");

    // One 256x256 matrix is exactly one chunk — the shape a fast path wants.
    let square = Layout::dense(256, 256);
    // 100x100 straddles at k = 6 and is contained at k = 0.
    let small = Layout::dense(100, 100);

    // Three container kinds, all covering the first chunk.
    let array_set = OrdSet::from_iter_unsorted((0..1000u64).map(|i| i * 61));
    let mut bitmap_set = OrdSet::from_iter_unsorted((0..40_000u64).map(|i| i * 3 % 65_536));
    bitmap_set.optimize();
    let mut run_set = OrdSet::from_iter_unsorted(0..60_000u64);
    run_set.optimize();

    for (name, set) in [
        ("array", &array_set),
        ("bitmap", &bitmap_set),
        ("run", &run_set),
    ] {
        g.throughput(Throughput::Elements(65_536));
        g.bench_function(format!("256x256/{name}"), |b| {
            b.iter(|| black_box(set.read_matrix(black_box(0), &square)))
        });
    }

    // A set whose chunks are *different kinds*. This is the case the array
    // cursor is actually for: while the seeking arm declined arrays, one array
    // chunk anywhere in the span sent the whole read to the generic path and
    // the bitmap chunk lost its 27x too. A pure-array read cannot show that —
    // there the cursor merely matches the generic merge.
    let mut mixed = OrdSet::from_iter_unsorted(
        // Chunk 0: sparse, stays an array. Chunk 1: dense, becomes a bitmap.
        (0..900u64)
            .map(|i| i * 71)
            .chain((0..40_000u64).map(|i| 65_536 + i * 3 % 65_536)),
    );
    mixed.optimize();
    let kinds: Vec<_> = mixed.chunks().map(|(_, c)| c.kind()).collect();
    assert!(
        kinds.len() >= 2 && kinds[0] != kinds[1],
        "the mixed fixture must span two container kinds, got {kinds:?}"
    );
    // 512x256 spans both chunks: 131 072 bits.
    let across = Layout::dense(512, 256);
    g.throughput(Throughput::Elements(131_072));
    g.bench_function("512x256/mixed_array_and_bitmap", |b| {
        b.iter(|| black_box(mixed.read_matrix(black_box(0), &across)))
    });

    // Contained against straddling, same layout, same data volume.
    let mut wide = OrdSet::from_iter_unsorted(0..80_000u64);
    wide.optimize();
    g.throughput(Throughput::Elements(10_000));
    g.bench_function("100x100/contained_k0", |b| {
        b.iter(|| black_box(wide.read_matrix(black_box(0), &small)))
    });
    g.bench_function("100x100/straddling_k6", |b| {
        b.iter(|| black_box(wide.read_matrix(black_box(6), &small)))
    });

    // Layout shape: dense rows begin at arbitrary bit offsets, word-aligned ones
    // do not. Same element count either way.
    let padded = Layout::word_aligned(100, 100);
    let mut padded_set = OrdSet::from_iter_unsorted(0..(100 * 128u64));
    padded_set.optimize();
    g.bench_function("100x100/word_aligned", |b| {
        b.iter(|| black_box(padded_set.read_matrix(black_box(0), &padded)))
    });

    // ColMajor costs an extra transpose on the way out.
    let cm = Layout {
        order: Order::ColMajor,
        line_stride: 100,
        matrix_stride: 10_000,
        ..Layout::dense(100, 100)
    };
    g.bench_function("100x100/col_major", |b| {
        b.iter(|| black_box(wide.read_matrix(black_box(0), &cm)))
    });

    g.finish();
}

/// Writing a matrix back, which is where the single `optimize()` is paid.
fn write_conditions(c: &mut Criterion) {
    let mut g = c.benchmark_group("bitmatrix/write");
    for (name, one_in) in [("sparse_1in64", 64u64), ("half", 2), ("dense_63in64", 1)] {
        let m = dense_by(256, 256, one_in.max(1), 7);
        let l = Layout::dense(256, 256);
        g.throughput(Throughput::Elements(m.count_ones()));
        g.bench_function(format!("256x256/{name}"), |b| {
            b.iter_batched(
                || MatrixSink::new(l),
                |mut sink| {
                    sink.place(0, &m).unwrap();
                    black_box(sink.build())
                },
                BatchSize::SmallInput,
            )
        });
    }
    g.finish();
}

// ---------------------------------------------------------------------- gemm

/// `A·B` across size, density and semiring — the three axes that move the cost.
///
/// The two semirings run the same loop with one operator changed, so a gap
/// between them is a measurement artefact and not an algorithmic difference.
/// Reporting them side by side is what makes that checkable.
fn gemm_conditions(c: &mut Criterion) {
    let mut g = c.benchmark_group("bitmatrix/gemm");
    for n in [8u32, 64, 256, 1024] {
        for (dname, one_in) in [("sparse_1in64", 64u64), ("half", 2)] {
            let a = dense_by(n, n, one_in, 1);
            let b = dense_by(n, n, one_in, 2);
            // Work tracks A's set bits times B's row width, not n^3.
            g.throughput(Throughput::Elements(
                a.count_ones() * b.cols().div_ceil(64) as u64,
            ));
            for (sname, sr) in [("bool", Semiring::Boolean), ("gf2", Semiring::Gf2)] {
                g.bench_function(format!("{n}/{dname}/{sname}"), |bch| {
                    bch.iter(|| black_box(a.mul(black_box(&b), sr)))
                });
            }
        }
    }
    g.finish();
}

/// Fused `A·B + C` against `mul` then `add`, over a sum of several terms.
///
/// This is what says whether GEMM being the primitive pays. The fused form
/// allocates one matrix per term; the split form allocates two and walks the
/// result twice.
fn gemm_vs_mul_add(c: &mut Criterion) {
    let mut g = c.benchmark_group("bitmatrix/gemm_vs_mul_add");
    let n = 256u32;
    for terms in [2usize, 4, 8] {
        let pairs: Vec<(BitMatrix, BitMatrix)> = (0..terms)
            .map(|i| {
                (
                    dense_by(n, n, 16, i as u64 * 2 + 1),
                    dense_by(n, n, 16, i as u64 * 2 + 2),
                )
            })
            .collect();
        g.throughput(Throughput::Elements(terms as u64));
        g.bench_function(format!("{terms}_terms/fused"), |b| {
            b.iter(|| {
                let mut acc = BitMatrix::zeros(n, n);
                for (x, y) in &pairs {
                    acc = x.gemm(y, &acc, Semiring::Gf2).unwrap();
                }
                black_box(acc)
            })
        });
        g.bench_function(format!("{terms}_terms/mul_then_add"), |b| {
            b.iter(|| {
                let mut acc = BitMatrix::zeros(n, n);
                for (x, y) in &pairs {
                    acc = x
                        .mul(y, Semiring::Gf2)
                        .unwrap()
                        .add(&acc, Semiring::Gf2)
                        .unwrap();
                }
                black_box(acc)
            })
        });
    }
    g.finish();
}

// ----------------------------------------------------------------- transpose

/// Delta swap against the bit-by-bit form, and across density.
///
/// The kernel's cost is `O(rows*cols/4096)` blocks *whatever the contents*,
/// while the naive walk is `O(nnz)`. So the delta swap must win on dense input
/// and must eventually lose on sparse input — the sparse rows here exist to
/// locate that crossover, which is the evidence a sparse arm would need.
fn transpose_conditions(c: &mut Criterion) {
    let mut g = c.benchmark_group("bitmatrix/transpose");
    for n in [64u32, 256, 1024] {
        // Densities spanning four orders of magnitude, because the delta swap is
        // content-independent and the scatter is not — a single density cannot
        // locate a crossover.
        for (dname, one_in) in [
            ("dense_half", 2u64),
            ("d_1in8", 8),
            ("d_1in16", 16),
            ("d_1in32", 32),
            ("sparse_1in64", 64),
            ("sparse_1in1024", 1024),
            ("sparse_1in16384", 16384),
        ] {
            let m = dense_by(n, n, one_in, 5);
            g.throughput(Throughput::Elements(n as u64 * n as u64));
            g.bench_function(format!("{n}/{dname}/delta_swap"), |b| {
                b.iter(|| black_box(m.transpose()))
            });
            g.bench_function(format!("{n}/{dname}/scatter_nnz"), |b| {
                b.iter(|| black_box(scatter_transpose(black_box(&m))))
            });
            // The O(rows*cols) form, for the "how bad is bit-by-bit" number
            // only. It cannot win and is measured at one density.
            if one_in == 2 {
                g.bench_function(format!("{n}/{dname}/naive_all_cells"), |b| {
                    b.iter(|| black_box(naive_transpose(black_box(&m))))
                });
            }
        }
    }
    // Non-square and non-multiple-of-64, where the partial blocks are.
    for (r, cc) in [(65u32, 63u32), (1000, 37), (37, 1000)] {
        let m = dense_by(r, cc, 2, 6);
        g.throughput(Throughput::Elements(r as u64 * cc as u64));
        g.bench_function(format!("{r}x{cc}/delta_swap"), |b| {
            b.iter(|| black_box(m.transpose()))
        });
    }
    g.finish();
}

// --------------------------------------------------------------------- gf(2)

/// Inversion and rank. `O(n³/64)`, so the interesting number is the ratio
/// between adjacent sizes: doubling `n` should cost about 8×.
fn gf2_conditions(c: &mut Criterion) {
    let mut g = c.benchmark_group("bitmatrix/gf2");
    g.sample_size(20);
    for n in [64u32, 128, 256, 512] {
        let a = invertible(n, 3);
        g.throughput(Throughput::Elements(n as u64 * n as u64 * n as u64 / 64));
        g.bench_function(format!("{n}/invert"), |b| {
            b.iter(|| black_box(a.invert_gf2()))
        });
        g.bench_function(format!("{n}/rank"), |b| b.iter(|| black_box(a.rank_gf2())));
    }
    // The claim `lu.rs` is built on: one factorisation amortized over many
    // right-hand sides. `solve_gf2` factors every call, so `k` of them cost
    // `k · O(n³/64)`; holding the factorisation costs `O(n³/64)` once plus
    // `k · O(n²/64)`.
    //
    // **The two arms are identical at k = 1 and must be**, because
    // `solve_gf2` now *is* factor-then-substitute. An earlier version of this
    // group compared against the old Gauss-Jordan solver and reported ~3x at
    // one right-hand side; that implementation no longer ships, so measuring it
    // would be benchmarking deleted code. What is left to measure is reuse.
    for n in [128u32, 512] {
        let a = invertible(n, 5);
        let rhs: Vec<BitMatrix> = (0..16)
            .map(|s| {
                let mut v = BitMatrix::zeros(1, n);
                let mut r = lcg(s as u64 + 900);
                for j in 0..n {
                    if r().is_multiple_of(3) {
                        v.set(0, j, true);
                    }
                }
                v
            })
            .collect();
        for k in [1usize, 4, 16] {
            g.throughput(Throughput::Elements(k as u64));
            g.bench_function(format!("{n}/solve_per_rhs/{k}_rhs"), |b| {
                b.iter(|| {
                    for v in rhs.iter().take(k) {
                        black_box(a.solve_gf2(v));
                    }
                })
            });
            g.bench_function(format!("{n}/lu_once_then_solve/{k}_rhs"), |b| {
                b.iter(|| {
                    let f = a.lu_gf2().unwrap();
                    for v in rhs.iter().take(k) {
                        black_box(f.solve(v));
                    }
                })
            });
        }
    }

    // A singular matrix exits at the first missing pivot, so it must not be
    // used as the timing fixture — measured here only to show the gap.
    let z = BitMatrix::zeros(256, 256);
    g.bench_function("256/invert_singular_early_exit", |b| {
        b.iter(|| black_box(z.invert_gf2()))
    });
    g.finish();
}

// ------------------------------------------------------------------ reduce

/// The reductions, and the counted product against the boolean one.
///
/// `counted_mul` does the same AND the boolean kernel does and popcounts it
/// instead of accumulating, so the pair says what carrying multiplicities
/// actually costs.
fn reduce_conditions(c: &mut Criterion) {
    let mut g = c.benchmark_group("bitmatrix/reduce");
    for n in [64u32, 256, 1024] {
        let m = dense_by(n, n, 2, 9);
        g.throughput(Throughput::Elements(n as u64 * n as u64));
        g.bench_function(format!("{n}/row_weights"), |b| {
            b.iter(|| black_box(m.row_weights()))
        });
        g.bench_function(format!("{n}/col_weights"), |b| {
            b.iter(|| black_box(m.col_weights()))
        });
        g.bench_function(format!("{n}/argmax_weight"), |b| {
            b.iter(|| black_box(m.argmax_weight()))
        });
        g.bench_function(format!("{n}/argmin_by_row"), |b| {
            b.iter(|| black_box(m.argmin_by_row()))
        });
    }
    for n in [64u32, 128, 256] {
        let a = dense_by(n, n, 8, 11);
        let b = dense_by(n, n, 8, 12);
        g.throughput(Throughput::Elements(n as u64 * n as u64));
        g.bench_function(format!("{n}/counted_mul"), |bch| {
            bch.iter(|| black_box(a.counted_mul(black_box(&b))))
        });
        g.bench_function(format!("{n}/boolean_mul"), |bch| {
            bch.iter(|| black_box(a.mul(black_box(&b), Semiring::Boolean)))
        });
    }
    g.finish();
}

criterion_group!(
    benches,
    read_conditions,
    write_conditions,
    gemm_conditions,
    gemm_vs_mul_add,
    transpose_conditions,
    gf2_conditions,
    reduce_conditions,
);
criterion_main!(benches);
