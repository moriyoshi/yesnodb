//! Bring your own accelerator for batched bitmap intersection counts.
//!
//! # Why the accelerator is supplied rather than created
//!
//! The same argument [`crate::dispatch`] makes for executors. `yesno-core` is
//! embedded in somebody else's process; a GPU context owned by this crate
//! would claim a device the host never offered, on a schedule it cannot see.
//! So this crate opens nothing, ever. [`Declines`] is the default and answers
//! every request with "no", which is exactly what every embedder gets today.
//!
//! The satellite that implements this is `yesno-gpu`. It depends on this
//! crate; nothing here depends on it, and nothing here mentions CUDA.
//!
//! # What is offered, and why only this
//!
//! One operation: **`| row AND filter |` for every `( row, filter )` pair of
//! one chunk against a set of filters.** A deliberately narrow surface, narrow
//! because it is the only shape with a measurement behind it -- and
//! two-dimensional because its call site is.
//!
//! Measured on an NVIDIA GB10, the naive arrangement -- one thread block per
//! `( chunk, filter )` pair -- ran at **1.06x to 1.23x** of the CPU, because
//! blocks touching the same chunk were scattered across the grid and each
//! re-read it from memory. Restructured to **one block per chunk, with the
//! chunk held in registers and every filter applied to it**, the same work
//! measured **6.71x to 11.08x**. The win is entirely in the reuse of a chunk
//! across filters, so the batch *is* the unit: an interface that offered one
//! intersection at a time could not express the thing that pays.
//!
//! This is why the trait takes a slice of filters rather than one, and why it
//! is not a general "evaluate this expression" hook.
//!
//! **It is two-dimensional because `count_blocked` is.** A blocked view splits
//! one container into rows of `stride` bits and counts every row against every
//! filter, so the batch is `rows x filters` and the reuse runs both ways: a
//! row is read once for all filters, a filter once for all rows. An interface
//! offering one row at a time would have had to be called in a loop by the one
//! call site it exists for, which is a good sign the interface is wrong.
//!
//! # Declining is normal, not an error
//!
//! [`Accelerator::and_cardinalities`] returns `bool`, and `false` means the
//! caller should use its ordinary CPU path. Every caller must have one, and
//! the CPU path is the oracle: no accelerator may return an answer that
//! differs from it.
//!
//! **That is what makes the whole arrangement safe to be wrong about.** An
//! implementation declines when the device is busy, when the batch is too
//! small to amortize a launch, when the chunk is not resident and admission
//! says not to make it so, or when there is no device at all. A wrong decline
//! costs one CPU operation that was going to happen anyway; there is no tail
//! regression to trade against, which is the same property that lets
//! [`crate::jit::cardinality`] fall back transparently.
//!
//! # Identity, because residency needs it
//!
//! [`ChunkId`] accompanies the words. An implementation that keeps device-side
//! copies needs to know when two calls are about the same chunk, and it cannot
//! learn that from the payload without hashing 8 KiB -- which costs more than
//! the intersection it is trying to accelerate.
//!
//! The identity must be **stable across calls and across snapshots** for a
//! cache to work at all. A pointer is not: the `Arc<OrdSet>` behind a query
//! leaf is rebuilt per query, so pointer identity makes every chunk look new
//! and a cache built on it reports a zero hit rate for every workload. That
//! mistake is recorded in `crate::hotspot`, which measures the recurrence this
//! cache would exploit. Callers should derive `ChunkId` from something
//! durable, such as the posting-list key and the chunk prefix.

use std::sync::Arc;

/// Stable identity for one chunk payload, for residency accounting.
///
/// Opaque and caller-assigned: this crate never interprets it, only compares
/// it. Two chunks with equal contents at different identities are two chunks,
/// which is correct -- nothing here deduplicates them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChunkId(pub u64);

/// A device that can count bitmap intersections in batches.
///
/// See the module documentation for the contract. In short: fill every element
/// of `out` and return `true`, or touch nothing and return `false`.
pub trait Accelerator: Send + Sync {
    /// Set `out[ f * rows + r ]` to `| row r AND filters[ f ] |`.
    ///
    /// `rows` is `data.len() / row_words`. Every filter is `row_words` long,
    /// and `out` is filter-major with `filters.len() * rows` slots.
    ///
    /// Returns `false` to decline, leaving `out` untouched and the work to the
    /// caller. An implementation returning `true` must have written every slot,
    /// and each must equal what the CPU path would have produced.
    ///
    /// The counts are *absolute*, not accumulated: callers add them into their
    /// own totals. An accelerator that accumulated would need to know which
    /// chunk of a scan it was in the middle of, which is exactly the state
    /// this interface exists not to carry.
    fn and_cardinalities(
        &self,
        chunk: ChunkId,
        data: &[u64],
        row_words: usize,
        filters: &[&[u64]],
        out: &mut [u32],
    ) -> bool;

    /// Advisory: is a batch of this size worth offering at all?
    ///
    /// Lets a caller skip gathering filter slices for a batch that would be
    /// declined anyway. Answering `true` and then declining is allowed; the
    /// reverse is not, because the caller may not ask.
    fn worth_offering(&self, filters: usize) -> bool {
        let _ = filters;
        true
    }
}

/// Answers "no" to everything. The default.
///
/// Opens nothing, allocates nothing, and is what the crate does today.
#[derive(Debug, Default, Clone, Copy)]
pub struct Declines;

impl Accelerator for Declines {
    fn and_cardinalities(
        &self,
        _: ChunkId,
        _: &[u64],
        _: usize,
        _: &[&[u64]],
        _: &mut [u32],
    ) -> bool {
        false
    }

    fn worth_offering(&self, _: usize) -> bool {
        false
    }
}

/// A handle to an accelerator, cheap to clone.
///
/// A concrete type rather than `Option<Arc<dyn Accelerator>>` for the reason
/// [`crate::dispatch::Dispatcher`] gives: `None` meaning "no device" is a
/// second way to say what [`Declines`] already says, and every internal use
/// site would otherwise unwrap the option and write the fallback inline.
#[derive(Clone)]
pub struct Accel(Arc<dyn Accelerator>);

impl Accel {
    /// Wrap a device.
    pub fn new<A: Accelerator + 'static>(a: A) -> Self {
        Self(Arc::new(a))
    }

    /// Wrap a device already behind an `Arc`, sharing it rather than
    /// re-boxing -- one device serving several databases hands each a clone.
    pub fn from_arc(a: Arc<dyn Accelerator>) -> Self {
        Self(a)
    }

    /// The default: no device, everything on the CPU.
    pub fn none() -> Self {
        Self::new(Declines)
    }

    /// See [`Accelerator::worth_offering`].
    #[inline]
    pub fn worth_offering(&self, filters: usize) -> bool {
        self.0.worth_offering(filters)
    }

    /// See [`Accelerator::and_cardinalities`].
    #[inline]
    pub fn and_cardinalities(
        &self,
        chunk: ChunkId,
        data: &[u64],
        row_words: usize,
        filters: &[&[u64]],
        out: &mut [u32],
    ) -> bool {
        debug_assert!(row_words > 0 && data.len().is_multiple_of(row_words));
        debug_assert_eq!(
            out.len(),
            filters.len() * (data.len() / row_words),
            "filter-major, one count per ( filter, row )"
        );
        self.0
            .and_cardinalities(chunk, data, row_words, filters, out)
    }
}

impl From<Arc<dyn Accelerator>> for Accel {
    fn from(a: Arc<dyn Accelerator>) -> Self {
        Self(a)
    }
}

impl Default for Accel {
    fn default() -> Self {
        Self::none()
    }
}

impl std::fmt::Debug for Accel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Accel")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// The CPU path this trait's implementations must agree with.
    fn reference(row: &[u64], filter: &[u64]) -> u32 {
        row.iter()
            .zip(filter)
            .map(|(a, b)| (a & b).count_ones())
            .sum()
    }

    struct Counting {
        seen: Mutex<Vec<ChunkId>>,
        answer: bool,
    }

    impl Accelerator for Counting {
        fn and_cardinalities(
            &self,
            chunk: ChunkId,
            data: &[u64],
            row_words: usize,
            filters: &[&[u64]],
            out: &mut [u32],
        ) -> bool {
            self.seen.lock().expect("lock").push(chunk);
            if !self.answer {
                return false;
            }
            let rows = data.len() / row_words;
            for (f, filter) in filters.iter().enumerate() {
                for r in 0..rows {
                    out[f * rows + r] =
                        reference(&data[r * row_words..(r + 1) * row_words], filter);
                }
            }
            true
        }
    }

    #[test]
    fn the_default_declines_and_leaves_the_output_alone() {
        let a = Accel::none();
        let data = [u64::MAX; 4];
        let f = [u64::MAX; 2];
        let filters: Vec<&[u64]> = vec![&f, &f, &f];
        let mut out = [7u32; 6];
        assert!(!a.and_cardinalities(ChunkId(1), &data, 2, &filters, &mut out));
        assert_eq!(out, [7; 6], "a decline must not scribble on the caller");
        assert!(!a.worth_offering(64));
    }

    #[test]
    fn a_device_that_accepts_fills_every_slot_filter_major() {
        let a = Accel::new(Counting {
            seen: Mutex::new(Vec::new()),
            answer: true,
        });
        // Two rows of two words, two filters.
        let data = [0b1011u64, 0xff, 0b0110, 0x0f];
        let f0 = [0b0011u64, 0x0f];
        let f1 = [u64::MAX, 0];
        let filters: Vec<&[u64]> = vec![&f0, &f1];
        let mut out = [0u32; 4];
        assert!(a.and_cardinalities(ChunkId(9), &data, 2, &filters, &mut out));
        for (f, filter) in filters.iter().enumerate() {
            for r in 0..2 {
                assert_eq!(
                    out[f * 2 + r],
                    reference(&data[r * 2..(r + 1) * 2], filter),
                    "filter {f} row {r}"
                );
            }
        }
    }

    #[test]
    fn one_row_is_the_degenerate_case_and_still_works() {
        let a = Accel::new(Counting {
            seen: Mutex::new(Vec::new()),
            answer: true,
        });
        let data = [0b1111u64];
        let f = [0b0101u64];
        let filters: Vec<&[u64]> = vec![&f];
        let mut out = [0u32; 1];
        assert!(a.and_cardinalities(ChunkId(1), &data, 1, &filters, &mut out));
        assert_eq!(out[0], 2);
    }

    #[test]
    fn identity_compares_by_value_because_residency_depends_on_it() {
        // An implementation that could not tell two chunks apart would have to
        // hash 8 KiB to find out, which costs more than the work it is
        // accelerating. See the module header on why a pointer will not do.
        assert_eq!(ChunkId(3), ChunkId(3));
        assert_ne!(ChunkId(3), ChunkId(17));
    }

    #[test]
    fn worth_offering_defaults_to_yes_for_a_real_device() {
        let a = Accel::new(Counting {
            seen: Mutex::new(Vec::new()),
            answer: true,
        });
        assert!(a.worth_offering(1), "the default trait method says yes");
    }
}
