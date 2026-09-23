//! The wired call site: a blocked view counted with and without a device.
//!
//! # Why this lives here
//!
//! `yesno-core` cannot depend on this crate -- that is the point of the
//! `Accelerator` trait -- so the differential between the accelerated path and
//! the CPU path cannot be a core test. It belongs on this side of the
//! dependency, where both halves are visible.
//!
//! # What it is actually asserting
//!
//! That `ViewIntersectionCounter::with_accelerator` changes *nothing* about
//! the answer. The CPU path is the oracle; an accelerator may decline but may
//! never disagree. Everything else here -- residency stats, admission counts --
//! is secondary to that one equality.

use yesno_core::accel::Accel;
use yesno_core::view::{IntersectionCountStrategy, View, ViewIntersectionCounter};
use yesno_core::{Container, OrdSet};
use yesno_gpu::residency::Policy;
use yesno_gpu::Offload;

/// 1024-bit rows, so 64 of them per 65 536-bit chunk.
const STRIDE: u64 = 1024;
const ROW_WORDS: usize = (STRIDE / 64) as usize;
const ROWS_PER_CHUNK: usize = 65_536 / STRIDE as usize;
const CHUNK_WORDS: usize = ROW_WORDS * ROWS_PER_CHUNK;

/// Enough constituents to cover every chunk the tests push.
///
/// **`count_blocked` returns early for any chunk whose first owner is past
/// `sets`**, so a view sized to one chunk silently ignores the rest -- and a
/// differential over chunks the view does not reach compares two empty
/// answers and passes. The first version of this file did exactly that.
const CHUNKS: usize = 6;
const SETS: u32 = (ROWS_PER_CHUNK * CHUNKS) as u32;

fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// A chunk dense enough to be a bitmap container, which is the only kind the
/// blocked batch path admits.
fn dense_chunk(seed: u64, prefix: u64) -> OrdSet {
    let values = (0..20_000u64).map(|i| (prefix << 16) | (mix(seed * 7919 + i) & 0xffff));
    let set = OrdSet::from_iter_unsorted(values);
    assert!(
        set.chunks().all(|(_, c)| matches!(c, Container::Bitmap(_))),
        "the blocked batch path only admits bitmap containers"
    );
    set
}

/// Filters over the within-row coordinate.
fn filters(n: usize, seed: u64) -> Vec<OrdSet> {
    (0..n as u64)
        .map(|f| {
            OrdSet::from_iter_unsorted(
                (0..400u64).map(|i| mix(seed * 104_729 + f * 31 + i) % STRIDE),
            )
        })
        .collect()
}

fn count(chunks: &[(u64, OrdSet)], filters: &[OrdSet], accel: Option<Accel>) -> Vec<Vec<u64>> {
    let view = View::blocked(SETS, STRIDE);
    let mut counter =
        ViewIntersectionCounter::new(view, filters.iter(), IntersectionCountStrategy::FullScan)
            .expect("counter");
    if let Some(a) = accel {
        counter = counter.with_accelerator(a, 0xfeed_beef);
    }
    for (prefix, set) in chunks {
        for (p, container) in set.chunks() {
            assert_eq!(p, *prefix);
            counter.push(p, container).expect("push");
        }
    }
    counter.finish().expect("flush must deliver what it took")
}

#[test]
fn an_accelerated_blocked_count_equals_the_cpu_count() {
    let chunks: Vec<(u64, OrdSet)> = (0..6).map(|p| (p, dense_chunk(p + 1, p))).collect();
    let f = filters(12, 3);

    let want = count(&chunks, &f, None);
    // `admit_after: 1`, deliberately. Under the measured policy a single pass
    // touches each chunk once, every touch is declined, and this test would
    // pass having never run the device at all -- green, and asserting nothing.
    let device = std::sync::Arc::new(
        Offload::with_policy(
            yesno_gpu::backend::HostBackend::new(8, CHUNK_WORDS),
            Policy {
                capacity: 8,
                admit_after: 1,
                half_life: None,
            },
        )
        .with_min_filters(1),
    );
    let got = count(&chunks, &f, Some(Accel::from_arc(device.clone())));

    assert_eq!(got, want, "an accelerator may decline but never disagree");
    assert_eq!(
        device.stats().admissions,
        chunks.len() as u64,
        "every chunk must have reached the device, or this asserts nothing"
    );
    assert!(
        want.iter().flatten().any(|&c| c > 0),
        "a test where every count is zero would pass without counting anything"
    );
}

#[test]
fn the_answer_is_the_same_whether_or_not_anything_was_admitted() {
    // Admission is a performance decision. A chunk seen once is declined and
    // runs on the CPU; a chunk seen six times is filled and runs on the
    // device. Both must produce identical counts, which is what makes the
    // policy safe to be wrong about.
    let chunks: Vec<(u64, OrdSet)> = (0..3).map(|p| (p, dense_chunk(p + 40, p))).collect();
    let f = filters(10, 11);
    let want = count(&chunks, &f, None);

    let device = std::sync::Arc::new(Offload::with_policy(
        yesno_gpu::backend::HostBackend::new(8, CHUNK_WORDS),
        Policy::measured(8),
    ));
    // Repeat the whole scan: the first passes are declined, later ones hit.
    for pass in 0..8 {
        let got = count(&chunks, &f, Some(Accel::from_arc(device.clone())));
        assert_eq!(got, want, "pass {pass} disagreed with the CPU");
    }
    let stats = device.stats();
    assert!(
        stats.admissions > 0,
        "nothing was ever admitted, so the device path never ran"
    );
    assert!(stats.hits > 0, "nothing was ever served from the device");
}

#[test]
fn a_declining_device_leaves_the_counts_untouched() {
    let chunks: Vec<(u64, OrdSet)> = (0..3).map(|p| (p, dense_chunk(p + 70, p))).collect();
    let f = filters(9, 13);
    let want = count(&chunks, &f, None);
    // `Accel::none()` is the default; installing it explicitly must be a no-op.
    let got = count(&chunks, &f, Some(Accel::none()));
    assert_eq!(got, want);
}

#[test]
fn a_batch_below_the_floor_never_reaches_the_device() {
    // The default floor is eight filters. A smaller batch has no reuse to
    // harvest and must not spend admission evidence.
    let chunks: Vec<(u64, OrdSet)> = (0..4).map(|p| (p, dense_chunk(p + 90, p))).collect();
    let f = filters(2, 17);
    let want = count(&chunks, &f, None);

    let device = std::sync::Arc::new(Offload::new(yesno_gpu::backend::HostBackend::new(
        8,
        CHUNK_WORDS,
    )));
    let got = count(&chunks, &f, Some(Accel::from_arc(device.clone())));
    assert_eq!(got, want);
    assert_eq!(
        device.stats().touches,
        0,
        "a batch under the floor must not reach residency at all"
    );
}
