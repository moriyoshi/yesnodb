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
//! # Status
//!
//! The device backend is **not built yet**. What exists is the policy, the
//! plumbing, and a host backend that proves both. `Offload::host` is for tests
//! and for answering "is the wiring right"; it is slower than doing the work
//! inline, because it copies a payload it did not need to copy, and it must
//! not be mistaken for an accelerator.

pub mod backend;
pub mod residency;

use std::sync::Mutex;

use yesno_core::accel::{Accelerator, ChunkId};

use crate::backend::{Backend, HostBackend};
use crate::residency::{Decision, Policy, Residency, Stats};

/// Filters below which a batch is not worth a device round trip.
///
/// The measured win comes from reusing one chunk across many filters, so a
/// batch of one is the case with no reuse to harvest and the launch cost paid
/// in full. This is a floor, not a tuned threshold: the right value is
/// device-specific and belongs in a measurement this crate has not taken yet.
pub const MIN_BATCH_FILTERS: usize = 8;

/// An accelerator: a backend, plus the policy deciding what it holds.
pub struct Offload<B: Backend> {
    backend: B,
    /// # Why one lock around the whole operation
    ///
    /// The residency table and the device slots must not disagree. Taking the
    /// table's lock, releasing it, and then uploading would let another thread
    /// evict the slot in between -- and the loser would launch against a
    /// payload that is no longer the chunk it asked for, returning **wrong
    /// counts with no error anywhere**. Silent wrong answers are the one
    /// failure this design cannot absorb, since every other failure degrades
    /// to the CPU path.
    ///
    /// So v1 serializes device work. That is a real throughput ceiling and it
    /// is the first thing to fix once there is a device backend to measure it
    /// against; the fix is a per-slot pin, not a finer lock here.
    state: Mutex<Residency>,
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
    /// If the policy would hold more chunks than the backend has slots. That
    /// is a configuration error rather than a runtime condition, and silently
    /// clamping it would make the residency stats describe a cache that does
    /// not exist.
    pub fn with_policy(backend: B, policy: Policy) -> Offload<B> {
        assert!(
            policy.capacity <= backend.capacity(),
            "policy wants {} slots, backend has {}",
            policy.capacity,
            backend.capacity()
        );
        Offload {
            backend,
            state: Mutex::new(Residency::new(policy)),
            min_filters: MIN_BATCH_FILTERS,
        }
    }

    /// Override the batch floor. Mostly for tests.
    pub fn with_min_filters(mut self, n: usize) -> Self {
        self.min_filters = n;
        self
    }

    /// Admission and slot statistics. See [`Stats`] for which ones diagnose
    /// what.
    pub fn stats(&self) -> Stats {
        self.state.lock().map(|r| r.stats()).unwrap_or_default()
    }

    pub fn backend_name(&self) -> &'static str {
        self.backend.name()
    }
}

impl Offload<HostBackend> {
    /// A host-memory accelerator: correct, testable anywhere, and not fast.
    ///
    /// See the crate header. This exists to prove the wiring, not to ship.
    pub fn host(capacity: usize, slot_words: usize) -> Offload<HostBackend> {
        Offload::new(HostBackend::new(capacity, slot_words))
    }
}

impl<B: Backend> Accelerator for Offload<B> {
    fn and_cardinalities(
        &self,
        chunk: ChunkId,
        data: &[u64],
        row_words: usize,
        filters: &[&[u64]],
        out: &mut [u32],
    ) -> bool {
        if row_words == 0 || data.len() != self.backend.slot_words() {
            return false;
        }
        let rows = data.len() / row_words;
        if filters.len() < self.min_filters
            || !data.len().is_multiple_of(row_words)
            || out.len() != filters.len() * rows
        {
            return false;
        }
        let Ok(mut state) = self.state.lock() else {
            // A poisoned table means some thread panicked mid-decision and the
            // slot map may not describe the device. Declining forever is the
            // safe reading; the CPU path is exact.
            return false;
        };
        let slot = match state.touch(chunk) {
            Decision::Decline => return false,
            Decision::Resident(slot) => slot,
            Decision::Admit(slot) => {
                if !self.backend.upload(slot, data) {
                    // The slot does not hold what the table thinks it holds.
                    state.abandon(chunk);
                    return false;
                }
                slot
            }
        };
        self.backend
            .and_cardinalities(slot, row_words, filters, out)
    }

    fn worth_offering(&self, filters: usize) -> bool {
        filters >= self.min_filters
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use yesno_core::accel::Accel;

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

    /// Every expected count, filter-major, as the CPU would produce them.
    fn expected(chunk: &[u64], filters: &[Vec<u64>]) -> Vec<u32> {
        let mut want = Vec::with_capacity(filters.len() * ROWS);
        for filter in filters {
            for r in 0..ROWS {
                want.push(reference(&chunk[r * ROW..(r + 1) * ROW], filter));
            }
        }
        want
    }

    fn offer_until_accepted(
        a: &impl Accelerator,
        id: u64,
        chunk: &[u64],
        f: &[&[u64]],
        out: &mut [u32],
    ) {
        for _ in 0..16 {
            if a.and_cardinalities(ChunkId(id), chunk, ROW, f, out) {
                return;
            }
        }
        panic!("never admitted");
    }

    #[test]
    fn an_accepted_batch_equals_the_cpu_reference() {
        // The whole contract: an accelerator may decline, but never disagree.
        let a = Offload::host(4, W).with_min_filters(1);
        let chunk = payload(1, W);
        let filters = filter_set(12, 2);
        let refs = borrow(&filters);
        let mut out = vec![0u32; filters.len() * ROWS];
        offer_until_accepted(&a, 1, &chunk, &refs, &mut out);
        assert_eq!(out, expected(&chunk, &filters));
    }

    #[test]
    fn a_small_batch_is_declined_without_touching_residency() {
        // Below the floor there is no reuse to harvest, and offering it would
        // spend admission evidence on a batch that can never pay.
        let a = Offload::host(4, W);
        let chunk = payload(3, W);
        let filters = filter_set(1, 4);
        let refs = borrow(&filters);
        let mut out = [7u32; ROWS];
        assert!(!a.worth_offering(1));
        assert!(!a.and_cardinalities(ChunkId(1), &chunk, ROW, &refs, &mut out));
        assert_eq!(
            out, [7u32; ROWS],
            "a decline must not scribble on the caller"
        );
        assert_eq!(a.stats().touches, 0, "no evidence was spent");
    }

    #[test]
    fn a_cold_chunk_is_declined_until_it_has_recurred() {
        let a = Offload::host(4, W);
        let chunk = payload(5, W);
        let filters = filter_set(8, 6);
        let refs = borrow(&filters);
        let mut out = vec![0u32; 8 * ROWS];
        let mut declines = 0;
        for _ in 0..Policy::measured(4).admit_after {
            if !a.and_cardinalities(ChunkId(2), &chunk, ROW, &refs, &mut out) {
                declines += 1;
            }
        }
        assert_eq!(declines, 4, "five sights, the fifth of which admits");
        assert_eq!(a.stats().admissions, 1);
    }

    #[test]
    fn a_resident_chunk_is_not_re_uploaded() {
        let a = Offload::host(4, W).with_min_filters(1);
        let chunk = payload(7, W);
        let filters = filter_set(4, 8);
        let refs = borrow(&filters);
        let mut out = vec![0u32; 4 * ROWS];
        offer_until_accepted(&a, 3, &chunk, &refs, &mut out);
        let before = a.stats().admissions;
        for _ in 0..20 {
            assert!(a.and_cardinalities(ChunkId(3), &chunk, ROW, &refs, &mut out));
        }
        assert_eq!(a.stats().admissions, before, "one fill, many uses");
        assert_eq!(a.stats().hits, 20);
    }

    #[test]
    fn every_chunk_gets_its_own_payload_back_under_eviction_pressure() {
        // The dangerous failure: a slot recycled to a new chunk while the
        // table still points an old chunk at it returns wrong counts with
        // nothing reporting an error.
        let a = Offload::host(2, W).with_min_filters(1);
        let filters = filter_set(4, 11);
        let refs = borrow(&filters);
        let chunks: Vec<Vec<u64>> = (0..6).map(|s| payload(s, W)).collect();
        let mut out = vec![0u32; 4 * ROWS];
        for round in 0..30u64 {
            let i = (round % 6) as usize;
            if a.and_cardinalities(ChunkId(i as u64), &chunks[i], ROW, &refs, &mut out) {
                assert_eq!(
                    out,
                    expected(&chunks[i], &filters),
                    "chunk {i} was served another chunk's payload"
                );
            }
        }
    }

    #[test]
    fn a_wrongly_sized_chunk_is_declined_rather_than_truncated() {
        let a = Offload::host(4, W).with_min_filters(1);
        let filters = filter_set(4, 13);
        let refs = borrow(&filters);
        let mut out = vec![0u32; 4 * ROWS];
        let short = payload(1, W - 1);
        assert!(!a.and_cardinalities(ChunkId(1), &short, ROW, &refs, &mut out));
        assert_eq!(a.stats().touches, 0);
    }

    #[test]
    fn a_row_width_that_does_not_divide_the_chunk_is_declined() {
        let a = Offload::host(4, W).with_min_filters(1);
        let chunk = payload(2, W);
        let odd = payload(3, 5);
        let refs: Vec<&[u64]> = vec![&odd];
        let mut out = vec![0u32; 4];
        assert!(!a.and_cardinalities(ChunkId(1), &chunk, 5, &refs, &mut out));
        assert!(!a.and_cardinalities(ChunkId(1), &chunk, 0, &refs, &mut out));
    }

    #[test]
    fn the_handle_forwards_both_methods() {
        let a = Accel::new(Offload::host(4, W).with_min_filters(2));
        assert!(a.worth_offering(4));
        assert!(!a.worth_offering(1));
        let chunk = payload(17, W);
        let filters = filter_set(4, 18);
        let refs = borrow(&filters);
        let mut out = vec![0u32; 4 * ROWS];
        let mut accepted = false;
        for _ in 0..16 {
            accepted |= a.and_cardinalities(ChunkId(4), &chunk, ROW, &refs, &mut out);
        }
        assert!(accepted, "the handle must reach a real device");
        assert_eq!(out, expected(&chunk, &filters));
    }

    #[test]
    fn a_policy_larger_than_the_device_is_a_configuration_error() {
        let r = std::panic::catch_unwind(|| {
            Offload::with_policy(HostBackend::new(2, W), Policy::measured(64))
        });
        assert!(r.is_err(), "clamping would make the stats describe a lie");
    }

    #[test]
    fn stats_account_for_every_offer_that_reached_the_policy() {
        let a = Offload::host(4, W).with_min_filters(1);
        let chunk = payload(21, W);
        let filters = filter_set(2, 22);
        let refs = borrow(&filters);
        let mut out = vec![0u32; 2 * ROWS];
        for _ in 0..25 {
            a.and_cardinalities(ChunkId(5), &chunk, ROW, &refs, &mut out);
        }
        let s = a.stats();
        assert_eq!(s.touches, 25);
        assert_eq!(s.hits + s.admissions + s.declines, s.touches);
    }
}
