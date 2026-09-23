//! The OpenCL backend against the CPU oracle, on whatever device is present.
//!
//! # Skipping is allowed; skipping silently is not
//!
//! A machine with no ICD, no GPU, or a driver that will not build the program
//! cannot run these. They skip -- and say so on stderr with the reason, because
//! a test that passes in 0.00s having executed nothing is indistinguishable
//! from a test that passes, and this project has been bitten by exactly that
//! three times. `cargo test -- --nocapture` shows the line.
//!
//! Where a device *is* present, the assertions are the real ones: identical
//! counts to the host backend, and the admission path exercised rather than
//! declined.

use yesno_core::accel::{next_scan, Accel, Accelerator, ChunkId};
use yesno_core::view::{IntersectionCountStrategy, View, ViewIntersectionCounter};
use yesno_core::{Container, OrdSet};
use yesno_opencl::backend::{Backend, HostBackend, Job};
use yesno_opencl::opencl::OpenClBackend;
use yesno_opencl::residency::Policy;
use yesno_opencl::Offload;

const ROW_WORDS: usize = 16; // 1024-bit rows
const ROWS: usize = 64; // a 65536-bit chunk
const SLOT_WORDS: usize = ROW_WORDS * ROWS;

fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

fn words(seed: u64, n: usize) -> Vec<u64> {
    (0..n as u64).map(|i| mix(seed * 1013 + i)).collect()
}

fn device(capacity: usize) -> Option<OpenClBackend> {
    match OpenClBackend::open(capacity, SLOT_WORDS) {
        Some(d) => {
            eprintln!("opencl device: {}", d.device_name());
            Some(d)
        }
        None => {
            eprintln!(
                "SKIPPED: no usable OpenCL GPU ( no ICD, no device, or the program \
                 would not build ). These assertions did not run."
            );
            None
        }
    }
}

#[test]
fn the_device_agrees_with_the_host_backend_on_every_count() {
    let Some(dev) = device(4) else { return };
    let host = HostBackend::new(4, SLOT_WORDS);

    for (seed, nfilters) in [(1u64, 16usize), (2, 64), (3, 1), (4, 37)] {
        let chunk = words(seed, SLOT_WORDS);
        let filters: Vec<Vec<u64>> = (0..nfilters as u64)
            .map(|f| words(seed * 7919 + f, ROW_WORDS))
            .collect();
        let refs: Vec<&[u64]> = filters.iter().map(|v| v.as_slice()).collect();

        assert!(dev.upload(yesno_opencl::residency::Slot(2), &chunk));
        assert!(host.upload(yesno_opencl::residency::Slot(2), &chunk));
        let mut got = vec![0u32; nfilters * ROWS];
        let mut want = vec![0u32; nfilters * ROWS];
        let jobs = [Job {
            slot: yesno_opencl::residency::Slot(2),
            rows: ROWS,
            owner_base: 0,
        }];
        assert!(dev.run_batch(&jobs, ROW_WORDS, &refs, 1, &mut got));
        assert!(host.run_batch(&jobs, ROW_WORDS, &refs, 1, &mut want));
        assert_eq!(got, want, "seed {seed}, {nfilters} filters");
        assert!(
            want.iter().any(|&c| c > 0),
            "all-zero counts would pass without counting anything"
        );
    }
}

#[test]
fn a_partial_chunk_is_counted_at_its_own_width() {
    let Some(dev) = device(2) else { return };
    let host = HostBackend::new(2, SLOT_WORDS);
    let rows = 5;
    let chunk = words(11, ROW_WORDS * rows);
    let filters: Vec<Vec<u64>> = (0..8u64).map(|f| words(900 + f, ROW_WORDS)).collect();
    let refs: Vec<&[u64]> = filters.iter().map(|v| v.as_slice()).collect();

    let s = yesno_opencl::residency::Slot(1);
    assert!(dev.upload(s, &chunk));
    assert!(host.upload(s, &chunk));
    let mut got = vec![0u32; 8 * rows];
    let mut want = vec![0u32; 8 * rows];
    // `rows` here is 5, not the full 64 a chunk holds: `count_blocked`
    // presents only the prefix of a container that belongs to the view.
    let jobs = [Job {
        slot: s,
        rows,
        owner_base: 0,
    }];
    assert!(dev.run_batch(&jobs, ROW_WORDS, &refs, 1, &mut got));
    assert!(host.run_batch(&jobs, ROW_WORDS, &refs, 1, &mut want));
    assert_eq!(got, want);
    assert!(
        want.iter().any(|&c| c > 0),
        "all-zero counts assert nothing"
    );
}

#[test]
fn a_slot_never_uploaded_is_refused_rather_than_answering_from_stale_memory() {
    let Some(dev) = device(2) else { return };
    let f = words(1, ROW_WORDS);
    let refs: Vec<&[u64]> = vec![&f];
    let mut out = [7u32; ROWS];
    assert!(!dev.run_batch(
        &[Job {
            slot: yesno_opencl::residency::Slot(0),
            rows: ROWS,
            owner_base: 0
        }],
        ROW_WORDS,
        &refs,
        1,
        &mut out
    ));
    assert_eq!(
        out, [7u32; ROWS],
        "a refusal must not scribble on the caller"
    );
}

#[test]
fn changing_the_filter_epoch_replaces_the_device_copy() {
    // The filter set is cached device-side and keyed by epoch, so a caller
    // that changes filters must change the epoch. If the cache ignored the
    // epoch -- or if it keyed on something that collides -- the second set
    // would silently be counted against the first set's filters, and every
    // count would be wrong with nothing reporting an error.
    let Some(dev) = device(2) else { return };
    let host = HostBackend::new(2, SLOT_WORDS);
    let chunk = words(31, SLOT_WORDS);
    let s = yesno_opencl::residency::Slot(0);
    assert!(dev.upload(s, &chunk));
    assert!(host.upload(s, &chunk));

    for (epoch, seed) in [(10u64, 40u64), (11, 41), (12, 42)] {
        let filters: Vec<Vec<u64>> = (0..8u64)
            .map(|f| words(seed * 100 + f, ROW_WORDS))
            .collect();
        let refs: Vec<&[u64]> = filters.iter().map(|v| v.as_slice()).collect();
        let mut got = vec![0u32; 8 * ROWS];
        let mut want = vec![0u32; 8 * ROWS];
        assert!(dev.run_batch(
            &[Job {
                slot: s,
                rows: ROWS,
                owner_base: 0
            }],
            ROW_WORDS,
            &refs,
            epoch,
            &mut got
        ));
        assert!(host.run_batch(
            &[Job {
                slot: s,
                rows: ROWS,
                owner_base: 0
            }],
            ROW_WORDS,
            &refs,
            epoch,
            &mut want
        ));
        assert_eq!(got, want, "epoch {epoch} was served another set's filters");
        assert!(
            want.iter().any(|&c| c > 0),
            "all-zero counts assert nothing"
        );
    }
}

#[test]
fn a_repeated_epoch_still_produces_the_right_counts() {
    // The other half: reusing an epoch for the *same* filters must be correct
    // as well as cheap, which is the case the whole optimization exists for.
    let Some(dev) = device(2) else { return };
    let host = HostBackend::new(2, SLOT_WORDS);
    let chunk = words(51, SLOT_WORDS);
    let s = yesno_opencl::residency::Slot(1);
    assert!(dev.upload(s, &chunk));
    assert!(host.upload(s, &chunk));
    let filters: Vec<Vec<u64>> = (0..8u64).map(|f| words(600 + f, ROW_WORDS)).collect();
    let refs: Vec<&[u64]> = filters.iter().map(|v| v.as_slice()).collect();
    let mut want = vec![0u32; 8 * ROWS];
    assert!(host.run_batch(
        &[Job {
            slot: s,
            rows: ROWS,
            owner_base: 0
        }],
        ROW_WORDS,
        &refs,
        7,
        &mut want
    ));
    for _ in 0..5 {
        let mut got = vec![0u32; 8 * ROWS];
        assert!(dev.run_batch(
            &[Job {
                slot: s,
                rows: ROWS,
                owner_base: 0
            }],
            ROW_WORDS,
            &refs,
            7,
            &mut got
        ));
        assert_eq!(got, want);
    }
}

#[test]
fn a_blocked_view_counted_on_the_device_equals_the_cpu() {
    // The real call site, end to end: `count_blocked` offering chunks to a
    // device through `Offload`, against the same scan with no accelerator.
    let Some(dev) = device(8) else { return };

    const CHUNKS: usize = 4;
    let sets = (ROWS * CHUNKS) as u32;
    let stride = 1024u64;

    let chunks: Vec<(u64, OrdSet)> = (0..CHUNKS as u64)
        .map(|p| {
            let set = OrdSet::from_iter_unsorted(
                (0..20_000u64).map(|i| (p << 16) | (mix(p * 31 + i) & 0xffff)),
            );
            assert!(set.chunks().all(|(_, c)| matches!(c, Container::Bitmap(_))));
            (p, set)
        })
        .collect();
    let filters: Vec<OrdSet> = (0..12u64)
        .map(|f| OrdSet::from_iter_unsorted((0..400u64).map(|i| mix(f * 977 + i) % stride)))
        .collect();

    let count = |accel: Option<Accel>| -> Vec<Vec<u64>> {
        let mut c = ViewIntersectionCounter::new(
            View::blocked(sets, stride),
            filters.iter(),
            IntersectionCountStrategy::FullScan,
        )
        .expect("counter");
        if let Some(a) = accel {
            c = c.with_accelerator(a, 0xabcd_1234);
        }
        for (prefix, set) in &chunks {
            for (p, container) in set.chunks() {
                assert_eq!(p, *prefix);
                c.push(p, container).expect("push");
            }
        }
        c.finish().expect("flush")
    };

    let want = count(None);
    let offload = std::sync::Arc::new(
        Offload::with_policy(
            dev,
            Policy {
                capacity: 8,
                admit_after: 1,
                half_life: None,
            },
        )
        .with_min_filters(1),
    );
    let got = count(Some(Accel::from_arc(offload.clone())));

    assert_eq!(got, want, "the device disagreed with the CPU");
    assert_eq!(
        offload.stats().admissions,
        CHUNKS as u64,
        "every chunk must have reached the device, or this asserts nothing"
    );
    assert!(
        want.iter().flatten().any(|&c| c > 0),
        "all-zero counts would pass without counting anything"
    );
}

#[test]
fn a_resident_chunk_is_served_from_the_device_without_re_uploading() {
    let Some(dev) = device(2) else { return };
    let offload = Offload::with_policy(
        dev,
        Policy {
            capacity: 2,
            admit_after: 1,
            half_life: None,
        },
    )
    .with_min_filters(1);

    let chunk = words(21, SLOT_WORDS);
    let filters: Vec<Vec<u64>> = (0..8u64).map(|f| words(500 + f, ROW_WORDS)).collect();
    let refs: Vec<&[u64]> = filters.iter().map(|v| v.as_slice()).collect();

    // Against the host oracle, not only against itself. Comparing the device
    // to its own earlier answer is how an earlier version of this test passed
    // while the backend returned all zeros: zeros equal zeros, ten times over.
    let host = HostBackend::new(1, SLOT_WORDS);
    let s0 = yesno_opencl::residency::Slot(0);
    assert!(host.upload(s0, &chunk));
    let mut want_flat = vec![0u32; 8 * ROWS];
    assert!(host.run_batch(
        &[Job {
            slot: s0,
            rows: ROWS,
            owner_base: 0
        }],
        ROW_WORDS,
        &refs,
        7,
        &mut want_flat
    ));
    let mut want = vec![vec![0u64; ROWS]; 8];
    for f in 0..8 {
        for r in 0..ROWS {
            want[f][r] = u64::from(want_flat[f * ROWS + r]);
        }
    }
    assert!(
        want.iter().flatten().any(|&c| c > 0),
        "all-zero asserts nothing"
    );

    // Ten separate scans over the same chunk: one fill, nine hits.
    for pass in 0..10 {
        let scan = next_scan();
        assert!(offload.enqueue(scan, ChunkId(77), &chunk, ROW_WORDS, &refs, 7, 0, ROWS));
        let mut got = vec![vec![0u64; ROWS]; 8];
        {
            let mut borrowed: Vec<&mut [u64]> = got.iter_mut().map(|v| v.as_mut_slice()).collect();
            assert!(offload.flush(scan, &mut borrowed));
        }
        assert_eq!(got, want, "pass {pass}");
    }
    let s = offload.stats();
    assert_eq!(s.admissions, 1, "one fill");
    assert_eq!(s.hits, 9, "nine reuses");
}
