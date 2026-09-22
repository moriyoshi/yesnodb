//! Whole-expression benchmark for nested bitmap-DAG cardinality.
//!
//! Run cargo bench -p yesno-core --features jit --bench dag on AArch64 or
//! x86_64. Both timed paths receive the same unplanned Expr and therefore pay
//! for planning on every call. The JIT cache is warm; the first call is
//! measured separately. This measures the shipped cached JIT path, not the
//! generated loop alone.
//!
//! This benchmark settled x86_64 automatic admission on 2026-09-22 and is now
//! the regression watch for both hosts. At 256 chunks the six shapes measured
//! 6.65x / 13.39x / 6.25x / 1.64x / 1.82x / 16.71x on an Intel i9-9880H and
//! 7.86x / 11.51x / 5.14x / 1.51x / 1.66x / 12.50x on AArch64. A shape that
//! drops near or below 1.00x on either host is a finding, not noise.
//!
//! **The protocol, because two of its rules were learned the hard way.** Run
//! the whole binary at least three complete times on real silicon -- the Intel
//! i9-9880H that settled the `ops::bitmap` AVX2 arm is the machine of record --
//! and read the per-shape ratios *across* runs rather than within one. Absolute
//! timings shifted up to 28% between x86 runs while every ratio held to a few
//! percent, and a single run cannot tell those two apart. All six shapes
//! matter, because a win at four leaves and a loss at sixteen is a different
//! decision from a uniform one. `first_us` is compile plus first call and is
//! reported separately on purpose: it is the numerator of the break-even, and
//! an automatic gate needs both halves. `qemu-x86_64` must not be used for any
//! of it -- it establishes the counts and nothing about the timings, and it has
//! already inverted one SIMD verdict in this repository.
//!
//! **Both arms must be the shipped ones, and the corpus must overlap.** The
//! figures this file replaced were wrong on both counts -- they timed prepared
//! core evaluation, which no caller of `jit::cardinality` performs, over leaves
//! that did not intersect, which collapsed every AND-heavy shape. They read
//! 1.04x-1.12x where the fixed experiment reads 5x-17x, and they stood for a
//! day as the reason x86 was gated off.

use std::hint::black_box;
use std::sync::Arc;
use std::time::Instant;
use yesno_core::jit::DagJit;
use yesno_core::{Container, Expr, OrdSet};

const DRAWS_PER_CHUNK: u64 = 6000;
const ROUNDS: usize = 9;

fn mix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

fn bitmap_set(seed: u64, chunks: u64) -> Arc<OrdSet> {
    let values = (0..chunks).flat_map(|prefix| {
        (0..DRAWS_PER_CHUNK).map(move |i| {
            let sample = mix64(
                seed.wrapping_mul(0x9e37_79b9_7f4a_7c15)
                    .wrapping_add(prefix.wrapping_mul(0xd1b5_4a32_d192_ed03))
                    .wrapping_add(i),
            );
            (prefix << 16) | (sample & 0xffff)
        })
    });
    let set = Arc::new(OrdSet::from_iter_unsorted(values));
    assert_eq!(set.chunk_count(), chunks as usize);
    assert!(set.chunks().all(|(_, c)| matches!(c, Container::Bitmap(_))));
    set
}

fn mixed4(x: &[Expr]) -> Expr {
    x[0].clone()
        .and(x[1].clone())
        .or(x[2].clone().and_not(x[3].clone()))
}

fn balanced8(x: &[Expr]) -> Expr {
    x[0].clone()
        .and(x[1].clone())
        .or(x[2].clone().and(x[3].clone()))
        .xor(
            x[4].clone()
                .or(x[5].clone())
                .and(x[6].clone().and_not(x[7].clone())),
        )
}

fn deep8(x: &[Expr]) -> Expr {
    x[0].clone()
        .xor(x[1].clone())
        .and_not(x[2].clone())
        .or(x[3].clone())
        .and(x[4].clone())
        .xor(x[5].clone())
        .or(x[6].clone())
        .and_not(x[7].clone())
}

fn or8(x: &[Expr]) -> Expr {
    x[..8].iter().cloned().reduce(Expr::or).unwrap()
}

fn xor8(x: &[Expr]) -> Expr {
    x[0].clone()
        .xor(x[1].clone())
        .xor(x[2].clone().xor(x[3].clone()))
        .xor(
            x[4].clone()
                .xor(x[5].clone())
                .xor(x[6].clone().xor(x[7].clone())),
        )
}

fn mixed16(x: &[Expr]) -> Expr {
    balanced8(&x[..8]).and_not(balanced8(&x[8..]))
}

struct Case {
    name: &'static str,
    leaves: usize,
    build: fn(&[Expr]) -> Expr,
}

const CASES: &[Case] = &[
    Case {
        name: "mixed4",
        leaves: 4,
        build: mixed4,
    },
    Case {
        name: "balanced8",
        leaves: 8,
        build: balanced8,
    },
    Case {
        name: "deep8",
        leaves: 8,
        build: deep8,
    },
    Case {
        name: "or8",
        leaves: 8,
        build: or8,
    },
    Case {
        name: "xor8",
        leaves: 8,
        build: xor8,
    },
    Case {
        name: "mixed16",
        leaves: 16,
        build: mixed16,
    },
];

fn ns_per_call(iters: u64, mut call: impl FnMut() -> u64) -> f64 {
    let start = Instant::now();
    let mut checksum = 0u64;
    for _ in 0..iters {
        checksum = checksum.wrapping_add(black_box(call()));
    }
    black_box(checksum);
    start.elapsed().as_nanos() as f64 / iters as f64
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

fn bench_case(case: &Case, leaves: &[Expr], chunks: u64) {
    let expr = (case.build)(&leaves[..case.leaves]);
    let expected = expr.collect_set().unwrap().len();
    assert!(expected > 0 && expected < chunks * 65536);

    let mut jit = DagJit::new();
    let first_start = Instant::now();
    let first = jit
        .try_cardinality(&expr)
        .expect("a bitmap DAG must compile on a supported host")
        .unwrap();
    let first_us = first_start.elapsed().as_secs_f64() * 1e6;
    assert_eq!(jit.compiled_shapes(), 1);
    assert_eq!(first, expected);
    assert_eq!(expr.cardinality().unwrap(), expected);

    // Warm both paths and keep every timed iteration on the same expression.
    for _ in 0..4 {
        assert_eq!(expr.cardinality().unwrap(), expected);
        assert_eq!(jit.try_cardinality(&expr).unwrap().unwrap(), expected);
    }

    let iters = (8192 / chunks).max(64);
    let mut core_samples = Vec::with_capacity(ROUNDS);
    let mut jit_samples = Vec::with_capacity(ROUNDS);
    for round in 0..ROUNDS {
        // Alternate order so clock and thermal drift cannot favor one arm.
        let core = || ns_per_call(iters, || black_box(&expr).cardinality().unwrap());
        let mut jitted = || {
            ns_per_call(iters, || {
                jit.try_cardinality(black_box(&expr)).unwrap().unwrap()
            })
        };
        if round % 2 == 0 {
            core_samples.push(core());
            jit_samples.push(jitted());
        } else {
            jit_samples.push(jitted());
            core_samples.push(core());
        }
    }

    let core = median(core_samples);
    let jitted = median(jit_samples);
    println!(
        "{:10} {:2} {:3} {:7} {:8} {:10.1} {:10.1} {:5.2}x {:10.1}",
        case.name,
        case.leaves,
        chunks,
        expected,
        iters,
        core,
        jitted,
        core / jitted,
        first_us
    );
}

fn main() {
    if !cfg!(any(target_arch = "aarch64", target_arch = "x86_64")) {
        println!("JIT code generation is not enabled on this host; no timing was recorded");
        return;
    }
    println!("shape      leaves chunks  result   iters    core_ns     jit_ns  speedup   first_us");
    for chunks in [1, 16, 64, 256] {
        let sets: Vec<_> = (1..=16).map(|seed| bitmap_set(seed, chunks)).collect();
        // A disjoint corpus made the old four-leaf expression effectively one
        // leaf. Every adjacent pair must really overlap in this corpus.
        for pair in sets.windows(2) {
            assert!(!pair[0].and(&pair[1]).is_empty());
        }
        let leaves: Vec<_> = sets.into_iter().map(Expr::set).collect();
        for case in CASES {
            bench_case(case, &leaves, chunks);
        }
    }
}
