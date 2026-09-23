//! Is the wired path actually faster than the CPU it falls back to?
//!
//! Everything measured before this was either the kernel in isolation or a
//! correctness differential. This times the thing a caller would actually
//! get: `ViewIntersectionCounter` over a blocked view, with and without an
//! accelerator installed, on the same corpus in the same process.
//!
//! Run with `cargo bench -p yesno-gpu --features opencl --bench offload`,
//! pinned ( `taskset -c 3` ). Without a device it prints why and exits, rather
//! than reporting the CPU arm twice as though that meant something.
//!
//! # Protocol
//!
//! Both arms see the same corpus and the same filters. The accelerated arm is
//! run to *warm* residency first, because a cold cache measures admission
//! rather than offload and the two answer different questions -- both are
//! reported. Repetitions are interleaved and medians taken across whole
//! rounds: absolute timings on this machine drift by double digits between
//! runs while ratios hold, and a single round cannot tell those apart.

use std::time::Instant;

use yesno_core::accel::Accel;
use yesno_core::view::{IntersectionCountStrategy, View, ViewIntersectionCounter};
use yesno_core::{Container, OrdSet};
use yesno_gpu::backend::HostBackend;
use yesno_gpu::opencl::OpenClBackend;
use yesno_gpu::residency::Policy;
use yesno_gpu::Offload;

const STRIDE: u64 = 1024;
const ROWS_PER_CHUNK: usize = (65_536 / STRIDE) as usize;
const ROW_WORDS: usize = (STRIDE / 64) as usize;
const SLOT_WORDS: usize = ROW_WORDS * ROWS_PER_CHUNK;
const ROUNDS: usize = 7;

fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

fn dense_chunk(prefix: u64) -> OrdSet {
    let set = OrdSet::from_iter_unsorted(
        (0..20_000u64).map(|i| (prefix << 16) | (mix(prefix * 31 + i) & 0xffff)),
    );
    assert!(set.chunks().all(|(_, c)| matches!(c, Container::Bitmap(_))));
    set
}

fn scan(
    chunks: &[(u64, OrdSet)],
    filters: &[OrdSet],
    sets: u32,
    accel: Option<Accel>,
) -> Vec<Vec<u64>> {
    let mut c = ViewIntersectionCounter::new(
        View::blocked(sets, STRIDE),
        filters.iter(),
        IntersectionCountStrategy::FullScan,
    )
    .expect("counter");
    if let Some(a) = accel {
        c = c.with_accelerator(a, 0x5eed);
    }
    for (_prefix, set) in chunks {
        for (p, container) in set.chunks() {
            c.push(p, container).expect("push");
        }
    }
    c.finish().expect("flush must deliver what it took")
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).expect("finite"));
    v[v.len() / 2]
}

fn main() {
    let n_chunks: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(256);
    let n_filters: usize = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(64);

    let chunks: Vec<(u64, OrdSet)> = (0..n_chunks as u64).map(|p| (p, dense_chunk(p))).collect();
    let filters: Vec<OrdSet> = (0..n_filters as u64)
        .map(|f| OrdSet::from_iter_unsorted((0..400u64).map(|i| mix(f * 977 + i) % STRIDE)))
        .collect();
    let sets = (ROWS_PER_CHUNK * n_chunks) as u32;

    println!(
        "corpus  {n_chunks} chunks x {ROWS_PER_CHUNK} rows x {ROW_WORDS} words, {n_filters} filters"
    );

    let want = scan(&chunks, &filters, sets, None);
    assert!(
        want.iter().flatten().any(|&c| c > 0),
        "an all-zero corpus measures nothing"
    );

    let mut cpu = Vec::new();
    for _ in 0..ROUNDS {
        let t = Instant::now();
        let got = scan(&chunks, &filters, sets, None);
        cpu.push(t.elapsed().as_secs_f64() * 1e3);
        assert_eq!(got, want);
    }

    let Some(dev) = OpenClBackend::open(n_chunks.max(1), SLOT_WORDS) else {
        println!("\nno OpenCL device: the accelerated arm did not run, and the CPU");
        println!(
            "numbers below are all there is. Median {:.2} ms.",
            median(&mut cpu)
        );
        return;
    };
    println!("device  {}", dev.device_name());

    let offload = std::sync::Arc::new(
        Offload::with_policy(
            dev,
            Policy {
                capacity: n_chunks.max(1),
                admit_after: 1,
                half_life: None,
            },
        )
        .with_min_filters(1),
    );

    // Cold: the first scan fills every chunk. Measured separately because it
    // answers "what does admission cost", not "what does offload buy".
    let t = Instant::now();
    let got = scan(
        &chunks,
        &filters,
        sets,
        Some(Accel::from_arc(offload.clone())),
    );
    let cold = t.elapsed().as_secs_f64() * 1e3;
    assert_eq!(got, want, "the device disagreed with the CPU");

    let mut warm = Vec::new();
    for _ in 0..ROUNDS {
        let t = Instant::now();
        let got = scan(
            &chunks,
            &filters,
            sets,
            Some(Accel::from_arc(offload.clone())),
        );
        warm.push(t.elapsed().as_secs_f64() * 1e3);
        assert_eq!(got, want);
    }

    // The host backend isolates how much of any gap is the device and how much
    // is the offload plumbing itself -- it runs the same code path and the same
    // copies, on the CPU.
    let host = std::sync::Arc::new(
        Offload::with_policy(
            HostBackend::new(n_chunks.max(1), SLOT_WORDS),
            Policy {
                capacity: n_chunks.max(1),
                admit_after: 1,
                half_life: None,
            },
        )
        .with_min_filters(1),
    );
    let _ = scan(&chunks, &filters, sets, Some(Accel::from_arc(host.clone())));
    let mut plumbing = Vec::new();
    for _ in 0..ROUNDS {
        let t = Instant::now();
        let _ = scan(&chunks, &filters, sets, Some(Accel::from_arc(host.clone())));
        plumbing.push(t.elapsed().as_secs_f64() * 1e3);
    }

    println!(
        "\nwarm rounds in order: {}",
        warm.iter()
            .map(|v| format!("{v:.2}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    let (mc, mw, mp) = (median(&mut cpu), median(&mut warm), median(&mut plumbing));
    println!();
    println!(
        "{:<22} {:>10} {:>9} {:>9} {:>8}",
        "arm", "median", "min", "max", "vs cpu"
    );
    let row = |name: &str, v: &[f64], m: f64| {
        println!(
            "{name:<22} {m:>8.2}ms {:>7.2}ms {:>7.2}ms {:>7.2}x",
            v[0],
            v[v.len() - 1],
            mc / m
        )
    };
    row("cpu ( no accel )", &cpu, mc);
    row("opencl, warm", &warm, mw);
    row("host backend", &plumbing, mp);
    println!("{:<22} {:>8.2}ms", "opencl, cold fill", cold);
    let s = offload.stats();
    println!(
        "\nresidency: {} admissions, {} hits, {} declines",
        s.admissions, s.hits, s.declines
    );
    println!("( >1.00x is faster than the CPU. The host-backend row is the same");
    println!("  plumbing without a device, so cpu/host is what the offload path");
    println!("  costs before any device work happens at all. )");
}
