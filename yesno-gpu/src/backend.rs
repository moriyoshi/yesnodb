//! The device, reduced to the three things residency needs from it.
//!
//! Keeping this a trait is what lets every policy question in
//! [`crate::residency`] and every dispatch question in [`crate::Offload`] be
//! tested exhaustively on a machine with no GPU, against [`HostBackend`] --
//! which is also the oracle the device backend must agree with.
//!
//! A backend owns its slot memory and nothing else. It does not decide *what*
//! to keep; that is the residency table's job, and the split is deliberate:
//! the interesting mistakes are all in the policy, and a policy that can only
//! be tested by running CUDA is a policy that will not be tested.

use crate::residency::Slot;

/// Somewhere chunk payloads can live and be intersected in batches.
pub trait Backend: Send + Sync {
    /// Slots this device can hold. Fixed for the backend's lifetime, because
    /// the residency table sizes itself from it once.
    fn capacity(&self) -> usize;
    /// Largest payload a slot can hold. A shorter one is fine and common:
    /// `count_blocked` presents the prefix of a container that belongs to the
    /// view, which is usually narrower than the container.
    fn slot_words(&self) -> usize;

    /// Words currently valid in `slot`, or zero if it holds nothing.
    ///
    /// Exists so a caller can notice that a chunk it believes resident is
    /// there at a different width, which is the cheap half of detecting that
    /// an identity was reused for a payload that changed.
    fn slot_len(&self, slot: Slot) -> usize;

    /// Copy `words` into `slot`, replacing whatever was there.
    ///
    /// `words` may be shorter than [`Backend::slot_words`] and the slot then
    /// holds exactly that many. `false` means the payload did not land, and
    /// the caller must treat the slot as holding nothing.
    fn upload(&self, slot: Slot, words: &[u64]) -> bool;

    /// Set `out[ f * rows + r ]` to `| row r of slot AND filters[ f ] |`,
    /// where `rows` is `slot_len( slot ) / row_words`.
    ///
    /// `false` means nothing was computed and `out` is untouched.
    fn and_cardinalities(
        &self,
        slot: Slot,
        row_words: usize,
        filters: &[&[u64]],
        out: &mut [u32],
    ) -> bool;

    /// A short name for diagnostics. Never parsed.
    fn name(&self) -> &'static str;
}

/// A backend that keeps slots in ordinary memory and counts on the CPU.
///
/// # Not a toy, and not a fast path either
///
/// It exists so the admission policy, the slot lifecycle and the decline paths
/// have a correctness oracle that runs anywhere -- including in CI, where
/// there is no device. It is *slower* than simply doing the work inline,
/// because it copies a payload it did not need to copy, so nothing should ship
/// it as a real accelerator. [`crate::Offload::host`] exists for tests and for
/// answering "is the plumbing right" on a laptop.
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

    fn and_cardinalities(
        &self,
        slot: Slot,
        row_words: usize,
        filters: &[&[u64]],
        out: &mut [u32],
    ) -> bool {
        if slot.0 >= self.capacity || row_words == 0 {
            return false;
        }
        let Ok(mem) = self.slots.lock() else {
            return false;
        };
        let (data, lens) = &*mem;
        let len = lens[slot.0];
        if len == 0 || !len.is_multiple_of(row_words) {
            return false;
        }
        let rows = len / row_words;
        if out.len() != filters.len() * rows || filters.iter().any(|f| f.len() != row_words) {
            return false;
        }
        let base = slot.0 * self.slot_words;
        let chunk = &data[base..base + len];
        for (f, filter) in filters.iter().enumerate() {
            for r in 0..rows {
                let row = &chunk[r * row_words..(r + 1) * row_words];
                out[f * rows + r] = row
                    .iter()
                    .zip(*filter)
                    .map(|(a, b)| (a & b).count_ones())
                    .sum();
            }
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
    fn a_round_trip_returns_the_reference_answer() {
        // Four words per slot as two rows of two.
        let b = HostBackend::new(2, 4);
        let chunk = [0b1011u64, 0xffff, 0b0110, 7];
        assert!(b.upload(Slot(1), &chunk));
        let f0 = [0b0011u64, 0x00ff];
        let f1 = [u64::MAX, 7];
        let filters: Vec<&[u64]> = vec![&f0, &f1];
        let mut out = [0u32; 4];
        assert!(b.and_cardinalities(Slot(1), 2, &filters, &mut out));
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
    fn a_single_row_covers_the_whole_slot() {
        let b = HostBackend::new(1, 4);
        assert!(b.upload(Slot(0), &[u64::MAX; 4]));
        let f = [u64::MAX; 4];
        let filters: Vec<&[u64]> = vec![&f];
        let mut out = [0u32; 1];
        assert!(b.and_cardinalities(Slot(0), 4, &filters, &mut out));
        assert_eq!(out[0], 256);
    }

    #[test]
    fn slots_do_not_alias() {
        let b = HostBackend::new(3, 2);
        assert!(b.upload(Slot(0), &[u64::MAX, 0]));
        assert!(b.upload(Slot(2), &[0, u64::MAX]));
        let all = [u64::MAX; 2];
        let filters: Vec<&[u64]> = vec![&all];
        let mut a = [0u32; 1];
        let mut c = [0u32; 1];
        assert!(b.and_cardinalities(Slot(0), 2, &filters, &mut a));
        assert!(b.and_cardinalities(Slot(2), 2, &filters, &mut c));
        assert_eq!((a[0], c[0]), (64, 64));
        // Slot 1 was never written, so it has no payload to answer with. A
        // zero here would be a plausible wrong answer rather than a refusal.
        let mut empty = [7u32; 1];
        assert!(!b.and_cardinalities(Slot(1), 2, &filters, &mut empty));
        assert_eq!(empty[0], 7);
    }

    #[test]
    fn an_out_of_range_slot_is_refused_rather_than_wrapped() {
        let b = HostBackend::new(2, 4);
        assert!(!b.upload(Slot(2), &[0; 4]));
        let f = [0u64; 4];
        let filters: Vec<&[u64]> = vec![&f];
        assert!(!b.and_cardinalities(Slot(9), 4, &filters, &mut [0u32; 1]));
    }

    #[test]
    fn a_payload_wider_than_the_slot_is_refused_and_an_empty_one_too() {
        let b = HostBackend::new(2, 4);
        assert!(!b.upload(Slot(0), &[0; 5]), "wider than the slot");
        assert!(!b.upload(Slot(0), &[]), "nothing to hold");
        assert_eq!(b.slot_len(Slot(0)), 0);
    }

    #[test]
    fn a_partial_payload_is_held_at_its_own_width() {
        // `count_blocked` presents the prefix of a container that belongs to
        // the view, which is usually narrower than the container. Refusing
        // those would decline most of the real traffic.
        let b = HostBackend::new(1, 8);
        assert!(b.upload(Slot(0), &[u64::MAX; 4]));
        assert_eq!(b.slot_len(Slot(0)), 4);
        let f = [u64::MAX; 2];
        let filters: Vec<&[u64]> = vec![&f];
        let mut out = [0u32; 2];
        assert!(b.and_cardinalities(Slot(0), 2, &filters, &mut out));
        assert_eq!(out, [128, 128], "two rows of two words, not four");
        // And the untouched tail of the slot is not counted.
        let mut wrong = [0u32; 4];
        assert!(!b.and_cardinalities(Slot(0), 2, &filters, &mut wrong));
    }

    #[test]
    fn re_uploading_at_a_different_width_replaces_the_width_too() {
        let b = HostBackend::new(1, 8);
        assert!(b.upload(Slot(0), &[u64::MAX; 8]));
        assert_eq!(b.slot_len(Slot(0)), 8);
        assert!(b.upload(Slot(0), &[u64::MAX; 2]));
        assert_eq!(b.slot_len(Slot(0)), 2, "the old width must not survive");
    }

    #[test]
    fn a_row_width_that_does_not_divide_the_slot_is_refused() {
        // Silently truncating would drop the tail of every chunk and return
        // counts that are merely plausible.
        let b = HostBackend::new(1, 4);
        let f = [0u64; 3];
        let filters: Vec<&[u64]> = vec![&f];
        assert!(!b.and_cardinalities(Slot(0), 3, &filters, &mut [0u32; 1]));
        assert!(!b.and_cardinalities(Slot(0), 0, &filters, &mut [0u32; 1]));
    }

    #[test]
    fn a_filter_of_the_wrong_width_is_refused() {
        let b = HostBackend::new(1, 4);
        let wrong = [0u64; 4];
        let filters: Vec<&[u64]> = vec![&wrong];
        assert!(!b.and_cardinalities(Slot(0), 2, &filters, &mut [0u32; 2]));
    }

    #[test]
    fn a_wrongly_sized_output_is_refused() {
        let b = HostBackend::new(1, 4);
        let f = [0u64; 2];
        let filters: Vec<&[u64]> = vec![&f];
        assert!(
            !b.and_cardinalities(Slot(0), 2, &filters, &mut [0u32; 1]),
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
        let mut out = [9u32; 1];
        assert!(b.and_cardinalities(Slot(0), 2, &filters, &mut out));
        assert_eq!(out[0], 0, "the old payload must not survive");
    }
}
