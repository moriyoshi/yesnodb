//! The device, reduced to what a deferred batch needs from it.
//!
//! Keeping this a trait is what lets every policy question in
//! [`crate::residency`] and every scheduling question in [`crate::Offload`] be
//! tested on a machine with no GPU, against [`HostBackend`] -- which is also
//! the oracle a device backend must agree with.
//!
//! A backend owns slot memory and runs batches. It decides nothing about
//! *what* to keep or *when* to submit; those are the residency table's and
//! `Offload`'s jobs. The split is deliberate: the interesting mistakes are all
//! in the policy and the scheduling, and neither should need a device to test.
//!
//! # One submission, not one per chunk
//!
//! [`Backend::run_batch`] takes every job at once because the per-chunk
//! alternative was measured and lost. Changing only the launch granularity in
//! a standalone harness -- same device, data, kernel, total work and bytes
//! read back -- cost **16.69x**, about 15 microseconds per chunk against
//! roughly one microsecond of work.

use crate::residency::Slot;

/// One chunk's contribution to a batch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Job {
    /// Where the payload already lives. Residency put it there.
    pub slot: Slot,
    /// Rows of this chunk that belong to the view.
    pub rows: usize,
    /// Index of this chunk's first row in each filter's count vector.
    pub owner_base: usize,
}

/// Somewhere chunk payloads can live and be intersected in batches.
pub trait Backend: Send + Sync {
    /// Slots this device can hold.
    fn capacity(&self) -> usize;

    /// Largest payload a slot can hold. A shorter one is fine and common:
    /// `count_blocked` presents the prefix of a container that belongs to the
    /// view, which is usually narrower than the container.
    fn slot_words(&self) -> usize;

    /// Words currently valid in `slot`, or zero if it holds nothing.
    fn slot_len(&self, slot: Slot) -> usize;

    /// Copy `words` into `slot`, replacing whatever was there.
    ///
    /// `words` may be shorter than [`Backend::slot_words`]. `false` means the
    /// payload did not land and the caller must treat the slot as empty.
    fn upload(&self, slot: Slot, words: &[u64]) -> bool;

    /// Count every job against every filter in one submission.
    ///
    /// `out` is filter-major over the jobs' concatenated rows: with
    /// `total = jobs.iter().map( |j| j.rows ).sum()` and `g` the index of a
    /// row in that concatenation, `out[ f * total + g ]` is
    /// `| that row AND filters[ f ] |`.
    ///
    /// `filters_epoch` identifies the filter set; equal epochs promise
    /// identical filters, so a backend holding them device-side may skip the
    /// copy.
    fn run_batch(
        &self,
        jobs: &[Job],
        row_words: usize,
        filters: &[&[u64]],
        filters_epoch: u64,
        out: &mut [u32],
    ) -> bool;

    /// A short name for diagnostics. Never parsed.
    fn name(&self) -> &'static str;
}

/// Total rows across a batch, which is the stride of `out`.
pub fn total_rows(jobs: &[Job]) -> usize {
    jobs.iter().map(|j| j.rows).sum()
}

/// A backend that keeps slots in ordinary memory and counts on the CPU.
///
/// # Not a toy, and not a fast path either
///
/// It exists so the admission policy, the slot lifecycle, the batching and
/// the decline paths have a correctness oracle that runs anywhere, CI
/// included. It is *slower* than doing the work inline, because it copies a
/// payload it did not need to copy; nothing should ship it as an accelerator.
pub struct HostBackend {
    slots: std::sync::Mutex<(Vec<u64>, Vec<usize>)>,
    capacity: usize,
    slot_words: usize,
}

impl HostBackend {
    pub fn new(capacity: usize, slot_words: usize) -> HostBackend {
        assert!(capacity > 0 && slot_words > 0);
        HostBackend {
            slots: std::sync::Mutex::new((vec![0; capacity * slot_words], vec![0; capacity])),
            capacity,
            slot_words,
        }
    }
}

impl Backend for HostBackend {
    fn capacity(&self) -> usize {
        self.capacity
    }

    fn slot_words(&self) -> usize {
        self.slot_words
    }

    fn slot_len(&self, slot: Slot) -> usize {
        if slot.0 >= self.capacity {
            return 0;
        }
        self.slots.lock().map(|m| m.1[slot.0]).unwrap_or(0)
    }

    fn upload(&self, slot: Slot, words: &[u64]) -> bool {
        if slot.0 >= self.capacity || words.is_empty() || words.len() > self.slot_words {
            return false;
        }
        let Ok(mut mem) = self.slots.lock() else {
            return false;
        };
        let base = slot.0 * self.slot_words;
        let (data, lens) = &mut *mem;
        data[base..base + words.len()].copy_from_slice(words);
        lens[slot.0] = words.len();
        true
    }

    fn run_batch(
        &self,
        jobs: &[Job],
        row_words: usize,
        filters: &[&[u64]],
        _filters_epoch: u64,
        out: &mut [u32],
    ) -> bool {
        if row_words == 0 || jobs.is_empty() || filters.iter().any(|f| f.len() != row_words) {
            return false;
        }
        let total = total_rows(jobs);
        if out.len() != filters.len() * total {
            return false;
        }
        let Ok(mem) = self.slots.lock() else {
            return false;
        };
        let (data, lens) = &*mem;
        // Validate every job before writing anything, so a bad one cannot
        // leave the caller holding half an answer it believes is whole.
        for job in jobs {
            if job.slot.0 >= self.capacity || lens[job.slot.0] < job.rows * row_words {
                return false;
            }
        }
        let mut g0 = 0usize;
        for job in jobs {
            let base = job.slot.0 * self.slot_words;
            for (f, filter) in filters.iter().enumerate() {
                for r in 0..job.rows {
                    let row = &data[base + r * row_words..base + (r + 1) * row_words];
                    out[f * total + g0 + r] = row
                        .iter()
                        .zip(*filter)
                        .map(|(a, b)| (a & b).count_ones())
                        .sum();
                }
            }
            g0 += job.rows;
        }
        true
    }

    fn name(&self) -> &'static str {
        "host"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference(row: &[u64], filter: &[u64]) -> u32 {
        row.iter()
            .zip(filter)
            .map(|(a, b)| (a & b).count_ones())
            .sum()
    }

    #[test]
    fn a_single_job_returns_the_reference_answer() {
        let b = HostBackend::new(2, 4);
        let chunk = [0b1011u64, 0xffff, 0b0110, 7];
        assert!(b.upload(Slot(1), &chunk));
        let f0 = [0b0011u64, 0x00ff];
        let f1 = [u64::MAX, 7];
        let filters: Vec<&[u64]> = vec![&f0, &f1];
        let jobs = [Job {
            slot: Slot(1),
            rows: 2,
            owner_base: 0,
        }];
        let mut out = [0u32; 4];
        assert!(b.run_batch(&jobs, 2, &filters, 1, &mut out));
        for (f, filter) in filters.iter().enumerate() {
            for r in 0..2 {
                assert_eq!(
                    out[f * 2 + r],
                    reference(&chunk[r * 2..(r + 1) * 2], filter)
                );
            }
        }
    }

    #[test]
    fn several_jobs_are_concatenated_in_order() {
        // `out` is filter-major over the jobs' rows end to end. Getting this
        // layout wrong attributes one chunk's counts to another, which looks
        // like a plausible answer rather than an error.
        let b = HostBackend::new(3, 4);
        let a = [u64::MAX, u64::MAX, 0, 0];
        let c = [0u64, 0, u64::MAX, u64::MAX];
        assert!(b.upload(Slot(0), &a));
        assert!(b.upload(Slot(2), &c));
        let all = [u64::MAX; 2];
        let filters: Vec<&[u64]> = vec![&all];
        let jobs = [
            Job {
                slot: Slot(0),
                rows: 2,
                owner_base: 0,
            },
            Job {
                slot: Slot(2),
                rows: 2,
                owner_base: 2,
            },
        ];
        let mut out = [0u32; 4];
        assert!(b.run_batch(&jobs, 2, &filters, 1, &mut out));
        assert_eq!(out, [128, 0, 0, 128], "job 0 rows then job 1 rows");
    }

    #[test]
    fn a_partial_payload_is_held_at_its_own_width() {
        let b = HostBackend::new(1, 8);
        assert!(b.upload(Slot(0), &[u64::MAX; 4]));
        assert_eq!(b.slot_len(Slot(0)), 4);
        let f = [u64::MAX; 2];
        let filters: Vec<&[u64]> = vec![&f];
        let jobs = [Job {
            slot: Slot(0),
            rows: 2,
            owner_base: 0,
        }];
        let mut out = [0u32; 2];
        assert!(b.run_batch(&jobs, 2, &filters, 1, &mut out));
        assert_eq!(out, [128, 128]);
    }

    #[test]
    fn a_job_wider_than_its_slot_holds_is_refused_before_anything_is_written() {
        // Half an answer the caller believes is whole is worse than none.
        let b = HostBackend::new(2, 4);
        assert!(b.upload(Slot(0), &[u64::MAX; 4]));
        let f = [u64::MAX; 2];
        let filters: Vec<&[u64]> = vec![&f];
        let jobs = [
            Job {
                slot: Slot(0),
                rows: 2,
                owner_base: 0,
            },
            Job {
                slot: Slot(1),
                rows: 2,
                owner_base: 2,
            }, // never uploaded
        ];
        let mut out = [7u32; 4];
        assert!(!b.run_batch(&jobs, 2, &filters, 1, &mut out));
        assert_eq!(
            out, [7; 4],
            "nothing may be written when the batch is refused"
        );
    }

    #[test]
    fn an_empty_batch_is_refused() {
        let b = HostBackend::new(1, 2);
        let f = [0u64; 2];
        let filters: Vec<&[u64]> = vec![&f];
        assert!(!b.run_batch(&[], 2, &filters, 1, &mut []));
    }

    #[test]
    fn a_wrongly_sized_output_is_refused() {
        let b = HostBackend::new(1, 4);
        assert!(b.upload(Slot(0), &[0u64; 4]));
        let f = [0u64; 2];
        let filters: Vec<&[u64]> = vec![&f];
        let jobs = [Job {
            slot: Slot(0),
            rows: 2,
            owner_base: 0,
        }];
        assert!(
            !b.run_batch(&jobs, 2, &filters, 1, &mut [0u32; 1]),
            "needs 2"
        );
    }

    #[test]
    fn uploading_replaces_rather_than_merges() {
        let b = HostBackend::new(1, 2);
        assert!(b.upload(Slot(0), &[u64::MAX, u64::MAX]));
        assert!(b.upload(Slot(0), &[0, 0]));
        let all = [u64::MAX; 2];
        let filters: Vec<&[u64]> = vec![&all];
        let jobs = [Job {
            slot: Slot(0),
            rows: 1,
            owner_base: 0,
        }];
        let mut out = [9u32; 1];
        assert!(b.run_batch(&jobs, 2, &filters, 1, &mut out));
        assert_eq!(out[0], 0, "the old payload must not survive");
    }
}
