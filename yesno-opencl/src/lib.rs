//! Batched bitmap intersection counts on an accelerator, admitted by residency.
//!
//! # What this crate is for
//!
//! One operation: `| row AND filter |` over the rows of one chunk and a set of
//! filters.
//! [`yesno_core::accel`] explains why the batch is the unit -- the measured win
//! is entirely in reusing a chunk across filters, and an interface offering one
//! intersection at a time cannot express it.
//!
//! This crate implements [`yesno_core::accel::Accelerator`]. `yesno-core`
//! depends on nothing here and mentions no device; a host that wants offload
//! constructs an [`Offload`] and hands it over, exactly as it hands over an
//! executor through [`yesno_core::dispatch`].
//!
//! # Declining is the normal case, and that is the design
//!
//! Every path here can answer "no", and the caller always has a CPU path that
//! is also the correctness oracle. A wrong decline costs one CPU operation
//! that was going to happen anyway. That asymmetry is what licenses a crude
//! admission heuristic: **a wrong admission wastes some bandwidth, a wrong
//! eviction costs one operation, and neither can produce a wrong answer.**
//!
//! It also means there is no break-even to clear. Under a fill-on-miss policy
//! a cache must be right more often than some threshold or it loses; here the
//! worst possible workload merely equals the CPU.
//!
//! # The three parts, and why they are separate
//!
//! * [`residency`] -- what to keep, and when it is worth filling. Every
//!   interesting mistake lives here, and it holds no device memory, so it is
//!   tested exhaustively on machines with no GPU.
//! * [`backend`] -- where payloads live and how a batch is launched. A trait,
//!   with [`backend::HostBackend`] as the oracle a device backend must agree
//!   with.
//! * [`Offload`] -- the join: it turns a [`yesno_core::accel::ChunkId`] into a
//!   slot, or into a decline.
//!
//! # Status: correct, measured, and currently slower than the CPU
//!
//! [`crate::opencl`] is a real device backend and it agrees with the CPU on
//! every count. **It is also slower than the CPU at every shape measured but
//! one**, and the reason is the call site rather than the device:
//!
//! ```text
//!   chunks  filters      cpu      opencl warm    vs cpu
//!      256       64   8.71ms        10.01ms      0.87x
//!     1024       16   8.58ms        24.15ms      0.36x
//! ```
//!
//! `ViewIntersectionCounter::push` hands over one chunk at a time, so this
//! launches, finishes and reads back once per chunk -- about **23.6 us of
//! round trip against 0.3 us of work**. The 6.71x-11.08x this project measured
//! for the same kernel batched 4096 chunks into a single grid, which the
//! present interface cannot express.
//!
//! Fixing it means making the hook *deferred*: accumulate across `push` and
//! flush once at `finish`. Until then, do not enable this expecting a
//! speedup, and do not spend effort tuning the kernel -- no work-group size
//! rescues a 78:1 overhead ratio. See `benches/offload.rs` and the JOURNAL
//! entry of 2026-09-23.
//!
//! [`backend::HostBackend`] is not an accelerator either: it copies a payload
//! it did not need to copy, and exists so the policy and plumbing have an
//! oracle that runs anywhere.

pub mod backend;
#[cfg(feature = "opencl")]
pub mod opencl;
pub mod residency;

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// Per-flush phase timings on stderr when `YESNO_GPU_TRACE` is set.
///
/// Read once. It exists because the phase breakdown is what found a 100x
/// swing in `run_batch` that came from allocating the output buffer per
/// flush, and a breakdown nobody can get at is a breakdown nobody uses.
static TRACE: OnceLock<bool> = OnceLock::new();

use yesno_core::accel::{Accelerator, ChunkId, ScanId};

use crate::backend::{total_rows, Backend, HostBackend, Job};
use crate::residency::{Decision, Policy, Residency, Stats};

/// Filters below which a batch is not worth offering at all.
///
/// The measured win comes from reusing one row across many filters, so a batch
/// of one has no reuse to harvest. A floor, not a tuned threshold.
pub const MIN_BATCH_FILTERS: usize = 8;

/// Scans an accelerator will hold work for at once.
///
/// A counter dropped without finishing releases its scan through
/// `Accelerator::cancel`, so this is a backstop rather than the usual path.
/// Exceeding it declines new scans, which is safe: declining always is.
pub const MAX_LIVE_SCANS: usize = 64;

/// Work queued for one scan, and the filters it is against.
struct ScanState {
    row_words: usize,
    /// Flattened once per scan rather than per chunk. Copying them per chunk
    /// was measured as 2.45 ms of an 12.22 ms scan.
    filters: Vec<u64>,
    nfilters: usize,
    epoch: u64,
    jobs: Vec<Job>,
    /// Chunks pinned on behalf of `jobs`, released at flush or cancel.
    pinned: Vec<ChunkId>,
}

/// Residency and queued work, under one lock.
///
/// One lock rather than two because the two are not independent: a job names
/// a slot, so queuing must pin it in the same critical section that admitted
/// it. Splitting them would create a window in which a slot is queued but not
/// yet pinned.
struct Shared {
    residency: Residency,
    scans: HashMap<ScanId, ScanState>,
    /// Reused across flushes rather than allocated per flush.
    ///
    /// # Why this is not a micro-optimization
    ///
    /// A scan's output is megabytes, which `malloc` serves with `mmap` --
    /// fresh, zero-filled, *cold* pages that the driver faults in one by one
    /// while copying the results back. Measured, a per-flush allocation made
    /// `run_batch` swing between 0.32 ms and 34.5 ms on identical work, while
    /// the same call against a reused buffer held steady at 0.27 ms. The
    /// bimodality is glibc's dynamic mmap threshold adapting after a few
    /// frees, which is why it looked like four slow rounds and then fast ones
    /// rather than uniform slowness.
    out: Vec<u32>,
}

/// An accelerator: a backend, plus the policy deciding what it holds.
pub struct Offload<B: Backend> {
    backend: B,
    shared: Mutex<Shared>,
    min_filters: usize,
}

impl<B: Backend> Offload<B> {
    /// Use `backend`, sized by the backend's own slot count.
    pub fn new(backend: B) -> Offload<B> {
        let policy = Policy::measured(backend.capacity());
        Offload::with_policy(backend, policy)
    }

    /// Use `backend` under an explicit policy.
    ///
    /// # Panics
    ///
    /// If the policy would hold more chunks than the backend has slots.
    /// Silently clamping would make the residency stats describe a cache that
    /// does not exist.
    pub fn with_policy(backend: B, policy: Policy) -> Offload<B> {
        assert!(
            policy.capacity <= backend.capacity(),
            "policy wants {} slots, backend has {}",
            policy.capacity,
            backend.capacity()
        );
        Offload {
            backend,
            shared: Mutex::new(Shared {
                residency: Residency::new(policy),
                scans: HashMap::new(),
                out: Vec::new(),
            }),
            min_filters: MIN_BATCH_FILTERS,
        }
    }

    /// Override the batch floor. Mostly for tests.
    pub fn with_min_filters(mut self, n: usize) -> Self {
        self.min_filters = n;
        self
    }

    /// Admission and slot statistics.
    pub fn stats(&self) -> Stats {
        self.shared
            .lock()
            .map(|s| s.residency.stats())
            .unwrap_or_default()
    }

    /// Scans currently holding queued work.
    pub fn live_scans(&self) -> usize {
        self.shared.lock().map(|s| s.scans.len()).unwrap_or(0)
    }

    pub fn backend_name(&self) -> &'static str {
        self.backend.name()
    }
}

impl Offload<HostBackend> {
    /// A host-memory accelerator: correct, testable anywhere, and not fast.
    pub fn host(capacity: usize, slot_words: usize) -> Offload<HostBackend> {
        Offload::new(HostBackend::new(capacity, slot_words))
    }
}

impl Shared {
    /// Drop a scan's queue and release everything it pinned.
    fn release(&mut self, scan: ScanId) -> Option<ScanState> {
        let state = self.scans.remove(&scan)?;
        for chunk in &state.pinned {
            self.residency.unpin(*chunk);
        }
        Some(state)
    }
}

impl<B: Backend> Accelerator for Offload<B> {
    fn enqueue(
        &self,
        scan: ScanId,
        chunk: ChunkId,
        data: &[u64],
        row_words: usize,
        filters: &[&[u64]],
        filters_epoch: u64,
        owner_base: usize,
        rows: usize,
    ) -> bool {
        if filters.len() < self.min_filters
            || row_words == 0
            || rows == 0
            || data.len() != rows * row_words
            || data.len() > self.backend.slot_words()
            || filters.iter().any(|f| f.len() != row_words)
        {
            return false;
        }
        let Ok(mut shared) = self.shared.lock() else {
            // A poisoned lock means some thread panicked mid-decision and the
            // slot map may not describe the device. Declining forever is the
            // safe reading; the CPU path is exact.
            return false;
        };

        // A scan's filters are fixed, so a mismatch is a caller bug. Decline
        // rather than mixing two filter sets into one batch.
        if let Some(existing) = shared.scans.get(&scan) {
            if existing.epoch != filters_epoch
                || existing.row_words != row_words
                || existing.nfilters != filters.len()
            {
                return false;
            }
        } else if shared.scans.len() >= MAX_LIVE_SCANS {
            return false;
        }

        let slot = match shared.residency.touch(chunk) {
            Decision::Decline => return false,
            Decision::Resident(slot) => {
                // The cheap half of "different payload, different id": a chunk
                // resident at another width cannot be the same bytes.
                if self.backend.slot_len(slot) != data.len() && !self.backend.upload(slot, data) {
                    shared.residency.abandon(chunk);
                    return false;
                }
                slot
            }
            Decision::Admit(slot) => {
                if !self.backend.upload(slot, data) {
                    shared.residency.abandon(chunk);
                    return false;
                }
                slot
            }
        };

        // Pin before queuing, in the same critical section that admitted the
        // slot: a job names a slot, and a slot recycled under a queued job
        // counts the wrong chunk without reporting anything.
        if !shared.residency.pin(chunk) {
            return false;
        }

        let state = shared.scans.entry(scan).or_insert_with(|| ScanState {
            row_words,
            filters: filters.iter().flat_map(|f| f.iter().copied()).collect(),
            nfilters: filters.len(),
            epoch: filters_epoch,
            jobs: Vec::new(),
            pinned: Vec::new(),
        });
        state.jobs.push(Job {
            slot,
            rows,
            owner_base,
        });
        state.pinned.push(chunk);
        true
    }

    fn flush(&self, scan: ScanId, counts: &mut [&mut [u64]]) -> bool {
        let Ok(mut shared) = self.shared.lock() else {
            return false;
        };
        let Some(state) = shared.release(scan) else {
            // Nothing was taken for this scan, so nothing can be lost.
            return true;
        };

        if state.nfilters != counts.len() {
            return false;
        }
        let trace = *TRACE.get_or_init(|| std::env::var_os("YESNO_GPU_TRACE").is_some());
        let t0 = std::time::Instant::now();
        let total = total_rows(&state.jobs);
        // Taken out of the shared state and put back at the end, so the pages
        // stay warm across scans. See `Shared::out`.
        let mut out = std::mem::take(&mut shared.out);
        out.clear();
        out.resize(state.nfilters * total, 0);
        let t_alloc = t0.elapsed();
        let filters: Vec<&[u64]> = state
            .filters
            .chunks_exact(state.row_words)
            .take(state.nfilters)
            .collect();
        let t1 = std::time::Instant::now();
        let ok = self.backend.run_batch(
            &state.jobs,
            state.row_words,
            &filters,
            state.epoch,
            &mut out,
        );
        if !ok {
            shared.out = out;
            return false;
        }
        let t_batch = t1.elapsed();
        let t2 = std::time::Instant::now();

        // One accumulate pass for the whole scan. The per-chunk version of
        // this was 256 passes and part of why the synchronous contract lost.
        let mut g0 = 0usize;
        for job in &state.jobs {
            for (f, owners) in counts.iter_mut().enumerate() {
                if job.owner_base + job.rows > owners.len() {
                    shared.out = out;
                    return false;
                }
                let block = &out[f * total + g0..f * total + g0 + job.rows];
                for (owner, got) in owners[job.owner_base..job.owner_base + job.rows]
                    .iter_mut()
                    .zip(block)
                {
                    *owner += u64::from(*got);
                }
            }
            g0 += job.rows;
        }
        shared.out = out;
        if trace {
            eprintln!(
                "flush: alloc {:.2}ms  run_batch {:.2}ms  accumulate {:.2}ms  ( {} jobs, {} rows )",
                t_alloc.as_secs_f64() * 1e3,
                t_batch.as_secs_f64() * 1e3,
                t2.elapsed().as_secs_f64() * 1e3,
                state.jobs.len(),
                total
            );
        }
        true
    }

    fn cancel(&self, scan: ScanId) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.release(scan);
        }
    }

    fn worth_offering(&self, filters: usize) -> bool {
        filters >= self.min_filters
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use yesno_core::accel::{next_scan, Accel};

    /// Four rows of four words: wide enough that a row is not the whole slot,
    /// which is the case `count_blocked` actually presents.
    const ROW: usize = 4;
    const ROWS: usize = 4;
    const W: usize = ROW * ROWS;

    fn reference(row: &[u64], filter: &[u64]) -> u32 {
        row.iter()
            .zip(filter)
            .map(|(a, b)| (a & b).count_ones())
            .sum()
    }

    fn mix(mut x: u64) -> u64 {
        x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
        x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        x ^ (x >> 31)
    }

    fn payload(seed: u64, words: usize) -> Vec<u64> {
        (0..words as u64).map(|i| mix(seed * 1013 + i)).collect()
    }

    fn filter_set(n: usize, seed: u64) -> Vec<Vec<u64>> {
        (0..n as u64)
            .map(|i| payload(seed * 7919 + i, ROW))
            .collect()
    }

    fn borrow(f: &[Vec<u64>]) -> Vec<&[u64]> {
        f.iter().map(|v| v.as_slice()).collect()
    }

    /// An accelerator that takes a chunk on first sight.
    ///
    /// `Offload::host` uses `Policy::measured`, whose `admit_after` is five --
    /// right for a workload, wrong for a test that wants to exercise the
    /// delivery path rather than the admission one. Tests that mean to
    /// exercise admission say so by using the measured policy.
    fn eager(capacity: usize) -> Offload<HostBackend> {
        Offload::with_policy(
            HostBackend::new(capacity, W),
            Policy {
                capacity,
                admit_after: 1,
                half_life: None,
            },
        )
        .with_min_filters(1)
    }

    /// What the CPU would put in `counts` for one chunk at `owner_base`.
    fn expect_into(
        counts: &mut [Vec<u64>],
        chunk: &[u64],
        filters: &[Vec<u64>],
        owner_base: usize,
    ) {
        for (f, filter) in filters.iter().enumerate() {
            for r in 0..chunk.len() / ROW {
                counts[f][owner_base + r] +=
                    u64::from(reference(&chunk[r * ROW..(r + 1) * ROW], filter));
            }
        }
    }

    fn flush_into(a: &impl Accelerator, scan: ScanId, counts: &mut [Vec<u64>]) -> bool {
        let mut borrowed: Vec<&mut [u64]> = counts.iter_mut().map(|v| v.as_mut_slice()).collect();
        a.flush(scan, &mut borrowed)
    }

    /// Offer the same chunk until it is taken, then flush.
    fn take_and_flush(
        a: &impl Accelerator,
        scan: ScanId,
        id: u64,
        chunk: &[u64],
        f: &[&[u64]],
        counts: &mut [Vec<u64>],
    ) {
        for _ in 0..16 {
            if a.enqueue(scan, ChunkId(id), chunk, ROW, f, 1, 0, chunk.len() / ROW) {
                assert!(flush_into(a, scan, counts));
                return;
            }
        }
        panic!("never taken");
    }

    #[test]
    fn what_is_taken_is_delivered_and_equals_the_cpu() {
        let a = eager(4);
        let scan = next_scan();
        let chunk = payload(1, W);
        let filters = filter_set(12, 2);
        let refs = borrow(&filters);
        let mut got = vec![vec![0u64; ROWS]; filters.len()];
        take_and_flush(&a, scan, 1, &chunk, &refs, &mut got);
        let mut want = vec![vec![0u64; ROWS]; filters.len()];
        expect_into(&mut want, &chunk, &filters, 0);
        assert_eq!(got, want);
    }

    #[test]
    fn several_chunks_land_at_their_own_owner_offsets() {
        // The layout that goes wrong silently: one chunk's counts attributed
        // to another's owners is a plausible answer, not an error.
        let a = eager(4);
        let scan = next_scan();
        let filters = filter_set(3, 5);
        let refs = borrow(&filters);
        let chunks: Vec<Vec<u64>> = (0..3).map(|s| payload(s + 10, W)).collect();
        for (i, chunk) in chunks.iter().enumerate() {
            assert!(a.enqueue(
                scan,
                ChunkId(i as u64),
                chunk,
                ROW,
                &refs,
                1,
                i * ROWS,
                ROWS
            ));
        }
        let mut got = vec![vec![0u64; ROWS * 3]; filters.len()];
        assert!(flush_into(&a, scan, &mut got));
        let mut want = vec![vec![0u64; ROWS * 3]; filters.len()];
        for (i, chunk) in chunks.iter().enumerate() {
            expect_into(&mut want, chunk, &filters, i * ROWS);
        }
        assert_eq!(got, want);
    }

    #[test]
    fn flush_adds_to_counts_the_caller_already_had() {
        let a = eager(2);
        let scan = next_scan();
        let chunk = payload(7, W);
        let filters = filter_set(2, 8);
        let refs = borrow(&filters);
        assert!(a.enqueue(scan, ChunkId(1), &chunk, ROW, &refs, 1, 0, ROWS));
        let mut got = vec![vec![50u64; ROWS]; filters.len()];
        assert!(flush_into(&a, scan, &mut got));
        let mut want = vec![vec![50u64; ROWS]; filters.len()];
        expect_into(&mut want, &chunk, &filters, 0);
        assert_eq!(got, want, "the caller's own counts were overwritten");
    }

    #[test]
    fn a_small_batch_is_declined_without_touching_residency() {
        let a = Offload::host(4, W);
        let scan = next_scan();
        let chunk = payload(3, W);
        let filters = filter_set(1, 4);
        let refs = borrow(&filters);
        assert!(!a.worth_offering(1));
        assert!(!a.enqueue(scan, ChunkId(1), &chunk, ROW, &refs, 1, 0, ROWS));
        assert_eq!(a.stats().touches, 0, "no evidence was spent");
    }

    #[test]
    fn a_cold_chunk_is_declined_until_it_has_recurred() {
        let a = Offload::host(4, W);
        let scan = next_scan();
        let chunk = payload(5, W);
        let filters = filter_set(8, 6);
        let refs = borrow(&filters);
        let mut declines = 0;
        for _ in 0..Policy::measured(4).admit_after {
            if !a.enqueue(scan, ChunkId(2), &chunk, ROW, &refs, 1, 0, ROWS) {
                declines += 1;
            }
        }
        assert_eq!(declines, 4, "five sights, the fifth of which admits");
        assert_eq!(a.stats().admissions, 1);
    }

    #[test]
    fn a_queued_slot_is_not_recycled_by_a_later_chunk() {
        // Capacity two, three chunks queued in one scan. Without pinning the
        // third would take the first's slot and the batch would count the
        // wrong payload.
        let a = eager(2);
        let scan = next_scan();
        let filters = filter_set(2, 21);
        let refs = borrow(&filters);
        let chunks: Vec<Vec<u64>> = (0..3).map(|s| payload(s + 30, W)).collect();
        let mut taken = Vec::new();
        for (i, chunk) in chunks.iter().enumerate() {
            if a.enqueue(
                scan,
                ChunkId(i as u64),
                chunk,
                ROW,
                &refs,
                1,
                i * ROWS,
                ROWS,
            ) {
                taken.push(i);
            }
        }
        assert_eq!(
            taken,
            vec![0, 1],
            "the third must be declined, not admitted"
        );
        let mut got = vec![vec![0u64; ROWS * 3]; filters.len()];
        assert!(flush_into(&a, scan, &mut got));
        let mut want = vec![vec![0u64; ROWS * 3]; filters.len()];
        for i in taken {
            expect_into(&mut want, &chunks[i], &filters, i * ROWS);
        }
        assert_eq!(
            got, want,
            "a queued slot was served another chunk's payload"
        );
    }

    #[test]
    fn cancel_releases_the_pins_it_took() {
        // Otherwise an abandoned scan would hold slots forever and the device
        // would decline every later scan.
        let a = eager(2);
        let filters = filter_set(2, 31);
        let refs = borrow(&filters);
        let first = next_scan();
        for i in 0..2u64 {
            assert!(a.enqueue(
                first,
                ChunkId(i),
                &payload(i + 40, W),
                ROW,
                &refs,
                1,
                0,
                ROWS
            ));
        }
        a.cancel(first);
        assert_eq!(a.live_scans(), 0);
        let second = next_scan();
        assert!(
            a.enqueue(second, ChunkId(9), &payload(99, W), ROW, &refs, 1, 0, ROWS),
            "the cancelled scan's slots were never released"
        );
    }

    #[test]
    fn flushing_an_unknown_scan_succeeds_because_nothing_was_taken() {
        let a = eager(2);
        let mut counts = vec![vec![0u64; ROWS]; 2];
        assert!(flush_into(&a, next_scan(), &mut counts));
        assert_eq!(counts, vec![vec![0u64; ROWS]; 2]);
    }

    #[test]
    fn a_flushed_scan_is_gone() {
        let a = eager(2);
        let scan = next_scan();
        let filters = filter_set(2, 41);
        let refs = borrow(&filters);
        let chunk = payload(50, W);
        assert!(a.enqueue(scan, ChunkId(1), &chunk, ROW, &refs, 1, 0, ROWS));
        assert_eq!(a.live_scans(), 1);
        let mut once = vec![vec![0u64; ROWS]; 2];
        assert!(flush_into(&a, scan, &mut once));
        assert_eq!(a.live_scans(), 0);
        // Flushing again must deliver nothing rather than double-count.
        let mut twice = once.clone();
        assert!(flush_into(&a, scan, &mut twice));
        assert_eq!(twice, once, "a second flush delivered the work again");
    }

    #[test]
    fn mixing_two_filter_sets_into_one_scan_is_declined() {
        let a = eager(4);
        let scan = next_scan();
        let chunk = payload(61, W);
        let f1 = filter_set(2, 62);
        let f2 = filter_set(2, 63);
        assert!(a.enqueue(scan, ChunkId(1), &chunk, ROW, &borrow(&f1), 1, 0, ROWS));
        assert!(
            !a.enqueue(scan, ChunkId(2), &chunk, ROW, &borrow(&f2), 2, ROWS, ROWS),
            "a second filter set in one scan must not join the batch"
        );
    }

    #[test]
    fn a_wrongly_sized_chunk_is_declined_rather_than_truncated() {
        let a = Offload::host(4, W).with_min_filters(1);
        let scan = next_scan();
        let filters = filter_set(4, 13);
        let refs = borrow(&filters);
        assert!(!a.enqueue(
            scan,
            ChunkId(1),
            &payload(1, W + 1),
            ROW,
            &refs,
            1,
            0,
            ROWS + 1
        ));
        assert!(!a.enqueue(scan, ChunkId(1), &payload(2, W), ROW, &refs, 1, 0, ROWS + 1));
        assert_eq!(a.stats().touches, 0);
    }

    #[test]
    fn the_handle_forwards_the_whole_contract() {
        let a = Accel::new(Offload::host(4, W).with_min_filters(2));
        let scan = next_scan();
        assert!(a.worth_offering(4));
        assert!(!a.worth_offering(1));
        let chunk = payload(17, W);
        let filters = filter_set(4, 18);
        let refs = borrow(&filters);
        // Stop at the first success: every accepted enqueue adds a job, so
        // continuing would batch the same chunk a dozen times and count it a
        // dozen times -- correctly, and not what this is asserting.
        let mut taken = false;
        for _ in 0..16 {
            if a.enqueue(scan, ChunkId(4), &chunk, ROW, &refs, 1, 0, ROWS) {
                taken = true;
                break;
            }
        }
        assert!(taken, "the handle must reach a real device");
        let mut got = vec![vec![0u64; ROWS]; filters.len()];
        {
            let mut borrowed: Vec<&mut [u64]> = got.iter_mut().map(|v| v.as_mut_slice()).collect();
            assert!(a.flush(scan, &mut borrowed));
        }
        let mut want = vec![vec![0u64; ROWS]; filters.len()];
        expect_into(&mut want, &chunk, &filters, 0);
        assert_eq!(got, want);
    }

    #[test]
    fn a_policy_larger_than_the_device_is_a_configuration_error() {
        let r = std::panic::catch_unwind(|| {
            Offload::with_policy(HostBackend::new(2, W), Policy::measured(64))
        });
        assert!(r.is_err(), "clamping would make the stats describe a lie");
    }
}
