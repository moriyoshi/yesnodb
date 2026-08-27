//! Arbitrary-precision arithmetic: scaling curves, and the Karatsuba crossover.
//!
//! # There is no external reference here yet, so this follows `bitmatrix.rs`
//!
//! `setops.rs` measures against the `roaring` crate as an **absolute** reference,
//! which is the right convention when one exists. `num-bigint` would be the
//! equivalent here and is not yet a dev-dependency, so until it is, this file
//! uses the two honest substitutes `bitmatrix.rs` names: **scaling curves** over
//! size, and **an alternative algorithm on identical operands**.
//!
//! # This file did not fix `KARATSUBA_MIN`, and could not have
//!
//! The two multiply arms are `pub(crate)` and must stay that way ( `AGENTS.md`:
//! a `pub mod` in `src/` is a semver promise ), so a bench cannot call them
//! individually. A bench-local reimplementation would work, but `setops.rs`
//! records that a bench-local copy of `ops::run`'s merge measured **7-21% faster
//! than the identical source inside the crate** — a bias that would land straight
//! in the crossover.
//!
//! The constant was instead measured by timing the **real**
//! [`BigUint::mul`](yesno_core::bignum::BigUint::mul) twice, from two builds of
//! the same source with `KARATSUBA_MIN` patched high and low. Both curves then
//! come from one compilation of one code path, so the bias does not exist rather
//! than merely cancelling. The harness, the sweep and the numbers are in
//! `JOURNAL.md`; do not re-derive the threshold from this file.
//!
//! What this file *is* for: catching a regression in either arm, and showing the
//! shape of the win at sizes a reader can check against the recorded table.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use std::hint::black_box;
use yesno_core::bignum::{Barrett, BigUint, KARATSUBA_MIN};

/// Deterministic, and not an RNG dev-dependency — the convention `setops.rs` and
/// `bitmatrix.rs` already use.
fn lcg(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    *state
}

/// Asserts the operand really has the limb count claimed. `setops.rs` records
/// that `binary_ops_dense` once measured array containers while claiming bitmaps
/// and sent two specialization attempts the wrong way; the analogue here is a
/// generator whose top limb comes out zero, silently shortening the operand and
/// moving it to the other side of the crossover.
fn operand(state: &mut u64, limbs: usize) -> BigUint {
    let mut v: Vec<u64> = (0..limbs).map(|_| lcg(state)).collect();
    if let Some(top) = v.last_mut() {
        *top |= 1 << 63;
    }
    let x = BigUint::from_limbs_le(v);
    assert_eq!(x.limbs().len(), limbs, "operand is not the claimed width");
    x
}

/// Both arms, across the crossover. Below `KARATSUBA_MIN` this is schoolbook and
/// above it Karatsuba, so the curve bends at the threshold rather than showing a
/// step.
fn mul_scaling(c: &mut Criterion) {
    let mut g = c.benchmark_group("bignum/mul_scaling");
    let mut st = 0x9e37_79b9_7f4a_7c15u64;
    for &n in &[4usize, 8, 16, 20, 24, 32, 48, 64, 128, 256, 512, 1024] {
        let a = operand(&mut st, n);
        let b = operand(&mut st, n);
        // Limb-pairs, so a flat line in this metric is quadratic behaviour and a
        // falling one is the sub-quadratic arm.
        g.throughput(Throughput::Elements((n * n) as u64));
        g.bench_with_input(BenchmarkId::from_parameter(n), &n, |bn, _| {
            bn.iter(|| black_box(&a).mul(black_box(&b)))
        });
    }
    g.finish();
}

/// The sizes either side of the shipped threshold, where the two arms meet.
/// Derived from the constant, so a re-measurement moves the benchmark with it.
fn mul_crossover(c: &mut Criterion) {
    let mut g = c.benchmark_group("bignum/mul_crossover");
    let mut st = 0x0123_4567_89ab_cdefu64;
    let t = KARATSUBA_MIN;
    for &n in &[t - 2, t - 1, t, t + 1, t + 2, 2 * t, 4 * t] {
        let a = operand(&mut st, n);
        let b = operand(&mut st, n);
        g.bench_with_input(BenchmarkId::from_parameter(n), &n, |bn, _| {
            bn.iter(|| black_box(&a).mul(black_box(&b)))
        });
    }
    g.finish();
}

/// A very unbalanced product must stay linear in the long operand: it blocks
/// rather than splitting. A regression here reads as super-linear growth down the
/// column, which no correctness test would notice.
fn mul_unbalanced(c: &mut Criterion) {
    let mut g = c.benchmark_group("bignum/mul_unbalanced");
    let mut st = 0xfeed_face_dead_beefu64;
    let short = operand(&mut st, KARATSUBA_MIN);
    for &n in &[64usize, 256, 1024, 4096] {
        let long = operand(&mut st, n);
        g.throughput(Throughput::Elements(n as u64));
        g.bench_with_input(BenchmarkId::from_parameter(n), &n, |bn, _| {
            bn.iter(|| black_box(&long).mul(black_box(&short)))
        });
    }
    g.finish();
}

/// Knuth D at the three shapes that exercise different parts of it: a balanced
/// divide, a dividend twice the divisor, and a single-limb divisor ( which takes
/// the `divrem_u64` path entirely ).
fn divrem_shapes(c: &mut Criterion) {
    let mut g = c.benchmark_group("bignum/divrem");
    let mut st = 0xabcd_ef01_2345_6789u64;
    for &(n, m) in &[(16usize, 16usize), (32, 16), (64, 64), (128, 64), (256, 1)] {
        let u = operand(&mut st, n);
        let d = operand(&mut st, m);
        g.bench_with_input(
            BenchmarkId::from_parameter(format!("{n}x{m}")),
            &(n, m),
            |bn, _| bn.iter(|| black_box(&u).divrem(black_box(&d))),
        );
    }
    g.finish();
}

/// `pow_mod` is where the multiply ladder actually pays for a caller: one
/// squaring per exponent bit, each reduced. The exponent is held at 256 bits so
/// the column varies only in the modulus width.
fn pow_mod_widths(c: &mut Criterion) {
    let mut g = c.benchmark_group("bignum/pow_mod");
    let mut st = 0x2468_ace0_1357_9bdfu64;
    let exp = operand(&mut st, 4);
    for &k in &[2usize, 4, 8, 16, 32] {
        let m = operand(&mut st, k);
        let base = operand(&mut st, k);
        let bar = Barrett::new(&m).expect("non-zero modulus");
        g.bench_with_input(BenchmarkId::from_parameter(k), &k, |bn, _| {
            bn.iter(|| bar.pow_mod(black_box(&base), black_box(&exp)))
        });
    }
    g.finish();
}

/// The `OrdSet` boundary, contained against straddling.
///
/// **The gather now seeks**, via the arm shared with `matrix/`. Forcing the
/// generic path and re-running is the before/after, on this machine:
///
/// ```text
///                  generic   seeking
///   contained_0    11.15 us    404 ns   27.6x
///   straddling_6    8.67 us   6.93 us    1.25x
/// ```
///
/// **These two rows do not differ only in position, and reading them as a
/// straddling penalty is wrong.** At `width = 10 000` and half fill, `k = 0`
/// puts ~5 000 values in one chunk — above `ARRAY_MAX`, so a **bitmap**, which
/// the arm serves with a bit-block transfer. `k = 6` splits them across two
/// chunks of ~2 500 each — below `ARRAY_MAX`, so two **arrays**, which the arm
/// walks per value. The 27.6x and the 1.25x are the bitmap and array arms, not
/// contained and straddling, and the fixture cannot separate the two effects
/// because the container kind is a consequence of the placement.
///
/// It inherited its framing from `matrix/`'s equivalent row, where the
/// operand is one chunk either way and the comparison *is* positional. Do not
/// quote a straddling cost from this bench.
fn ordset_boundary(c: &mut Criterion) {
    use yesno_core::bignum::{IntLayout, IntSink};

    let mut g = c.benchmark_group("bignum/ordset_boundary");
    let mut st = 0x5555_aaaa_5555_aaaau64;
    let width = 10_000u32;
    let value = operand(&mut st, (width / 64) as usize);
    for &(name, k) in &[("contained_0", 0u64), ("straddling_6", 6)] {
        let layout = IntLayout::dense(width);
        let mut sink = IntSink::new(layout);
        sink.place(k, &value).expect("in range");
        let set = sink.build();
        g.bench_function(name, |bn| {
            bn.iter(|| black_box(&set).read_int(black_box(k), &layout))
        });
    }
    g.finish();
}

criterion_group!(
    benches,
    mul_scaling,
    mul_crossover,
    mul_unbalanced,
    divrem_shapes,
    pow_mod_widths,
    ordset_boundary
);
criterion_main!(benches);
