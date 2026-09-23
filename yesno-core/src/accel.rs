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
//! # What is offered, and why the contract is deferred
//!
//! One operation: `| row AND filter |` over the rows of a chunk against a set
//! of filters. Narrow because it is the only shape with a measurement behind
//! it, and two-dimensional because `count_blocked` is.
//!
//! **It is enqueue-and-flush rather than compute-now, and that is the whole
//! design.** The first version computed per chunk, which is the cadence
//! `ViewIntersectionCounter::push` produces -- and it measured *slower than
//! the CPU it replaces*, 0.36x to 0.89x. Isolating the cause in a standalone
//! harness, changing only the launch granularity and nothing else, cost
//! **16.69x, or 15 microseconds per chunk**, against roughly 1 microsecond of
//! actual work. Adding the per-chunk readback and accumulation on top took the
//! wired path to 39 microseconds per chunk.
//!
//! A streaming producer and a device that needs batch submission are at odds,
//! and the synchronous contract resolved that tension in the wrong direction.
//! Deferring lets one launch cover a scan: 256 launches become 1, 256
//! readbacks become 1, and 256 accumulate passes become 1.
//!
//! # The obligation deferral creates
//!
//! **If [`Accelerator::enqueue`] returns `true`, the caller must not compute
//! those counts**, so [`Accelerator::flush`] either delivers them or the
//! result is silently short. That is why `flush` reports failure and why
//! `ViewIntersectionCounter::finish` is fallible: an accelerator that loses
//! enqueued work must produce an error, never a plausible undercount.
//!
//! It is also why declining is still free. An implementation that cannot take
//! responsibility -- no device, a full queue, a batch too small to amortize --
//! returns `false` from `enqueue`, and the caller counts it inline as before.
//!
//! # Identity, because residency needs it
//!
//! [`ChunkId`] accompanies the words. An implementation that keeps device-side
//! copies needs to know when two calls are about the same chunk, and it cannot
//! learn that from the payload without hashing 8 KiB -- which costs more than
//! the intersection it is trying to accelerate.
//!
//! The contract has two halves and **both are the caller's responsibility**:
//!
//! * **Equal payloads, equal id.** Otherwise there is no reuse to find. A
//!   pointer fails this: the `Arc<OrdSet>` behind a query leaf is rebuilt per
//!   query, so pointer identity makes every chunk look new and a cache built
//!   on it reports a zero hit rate for every workload. That mistake is
//!   recorded in `crate::hotspot`, which measures the very recurrence this
//!   cache would exploit. Derive it from something durable instead, such as
//!   the posting-list key and the chunk prefix.
//! * **Different payloads, different id.** This half is the dangerous one. A
//!   posting list rewritten by a commit keeps its key and its prefix, so an id
//!   built from those alone would let a *stale* device copy answer for the new
//!   contents -- wrong counts, with nothing anywhere reporting an error. Fold
//!   in something that moves when the bytes move: a snapshot version is the
//!   blunt instrument, a per-chunk generation the precise one.
//!
//! An implementation may check the cheap half of the second condition, by
//! noticing that a chunk it believes resident has arrived at a different
//! width. That catches a resize and not a rewrite, so it is a safety net and
//! not a substitute for the contract.

use std::sync::Arc;

/// Stable identity for one chunk payload, for residency accounting.
///
/// Opaque and caller-assigned: this crate never interprets it, only compares
/// it. Two chunks with equal contents at different identities are two chunks,
/// which is correct -- nothing here deduplicates them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChunkId(pub u64);

/// A fresh filter-set epoch, distinct from every other this process hands out.
///
/// Callers that hold a fixed filter set for a scan should take one at
/// construction and reuse it for every chunk. See
/// [`Accelerator::and_cardinalities`] for why equality of epochs is a promise
/// about the filters themselves.
pub fn next_filters_epoch() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Identifies one scan, so an accelerator can hold work for several at once.
///
/// Taken by a counter at construction and used for every call it makes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ScanId(pub u64);

/// A fresh scan identity, distinct from every other this process hands out.
pub fn next_scan() -> ScanId {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    ScanId(NEXT.fetch_add(1, Ordering::Relaxed))
}

/// A device that can count bitmap intersections in batches.
///
/// See the module documentation for the contract. In short: take
/// responsibility for a chunk or decline it, and deliver everything taken.
pub trait Accelerator: Send + Sync {
    /// Take responsibility for one chunk's counts, or decline.
    ///
    /// `data` is `rows * row_words` words; every filter is `row_words` long.
    /// `owner_base` is where this chunk's first row lands in each filter's
    /// count vector, so the accelerator can place results without knowing
    /// anything about views.
    ///
    /// `true` means the caller **must not** count this chunk itself. `false`
    /// means it must.
    #[allow(clippy::too_many_arguments)]
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
    ) -> bool;

    /// Deliver every count enqueued for `scan`, adding into `counts`.
    ///
    /// `counts[ f ]` is filter `f`'s vector over owners. Accumulates rather
    /// than overwrites, because chunks the accelerator declined were counted
    /// into the same vectors by the caller.
    ///
    /// **Failure is not recoverable by the caller**: it skipped the work on
    /// the strength of `enqueue`. Returning `false` must therefore be
    /// surfaced as an error, and an implementation should prefer declining at
    /// `enqueue` over failing here.
    ///
    /// Clears the scan's state either way.
    fn flush(&self, scan: ScanId, counts: &mut [&mut [u64]]) -> bool;

    /// Discard a scan's state without delivering it.
    ///
    /// Called when a counter is dropped without finishing. Nothing was
    /// promised to anyone, so there is nothing to report.
    fn cancel(&self, scan: ScanId);

    /// Advisory: is a batch of this width worth offering at all?
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
    fn enqueue(
        &self,
        _: ScanId,
        _: ChunkId,
        _: &[u64],
        _: usize,
        _: &[&[u64]],
        _: u64,
        _: usize,
        _: usize,
    ) -> bool {
        false
    }

    fn flush(&self, _: ScanId, _: &mut [&mut [u64]]) -> bool {
        // Nothing was ever taken, so nothing can be lost.
        true
    }

    fn cancel(&self, _: ScanId) {}

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

    /// See [`Accelerator::enqueue`].
    #[allow(clippy::too_many_arguments)]
    #[inline]
    pub fn enqueue(
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
        debug_assert!(row_words > 0 && data.len() == rows * row_words);
        self.0.enqueue(
            scan,
            chunk,
            data,
            row_words,
            filters,
            filters_epoch,
            owner_base,
            rows,
        )
    }

    /// See [`Accelerator::flush`].
    #[inline]
    pub fn flush(&self, scan: ScanId, counts: &mut [&mut [u64]]) -> bool {
        self.0.flush(scan, counts)
    }

    /// See [`Accelerator::cancel`].
    #[inline]
    pub fn cancel(&self, scan: ScanId) {
        self.0.cancel(scan);
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

    /// The CPU path an accelerator must agree with.
    fn reference(row: &[u64], filter: &[u64]) -> u32 {
        row.iter()
            .zip(filter)
            .map(|(a, b)| (a & b).count_ones())
            .sum()
    }

    /// Takes everything and delivers it at flush, which is the contract in
    /// its simplest honest form.
    #[derive(Default)]
    struct Deferring {
        taken: Mutex<Vec<(usize, Vec<u32>, usize)>>,
        fail_flush: bool,
    }

    impl Accelerator for Deferring {
        fn enqueue(
            &self,
            _: ScanId,
            _: ChunkId,
            data: &[u64],
            row_words: usize,
            filters: &[&[u64]],
            _: u64,
            owner_base: usize,
            rows: usize,
        ) -> bool {
            let mut block = vec![0u32; filters.len() * rows];
            for (f, filter) in filters.iter().enumerate() {
                for r in 0..rows {
                    block[f * rows + r] =
                        reference(&data[r * row_words..(r + 1) * row_words], filter);
                }
            }
            self.taken
                .lock()
                .expect("lock")
                .push((owner_base, block, rows));
            true
        }

        fn flush(&self, _: ScanId, counts: &mut [&mut [u64]]) -> bool {
            if self.fail_flush {
                self.taken.lock().expect("lock").clear();
                return false;
            }
            for (owner_base, block, rows) in self.taken.lock().expect("lock").drain(..) {
                for (f, owners) in counts.iter_mut().enumerate() {
                    for r in 0..rows {
                        owners[owner_base + r] += u64::from(block[f * rows + r]);
                    }
                }
            }
            true
        }

        fn cancel(&self, _: ScanId) {
            self.taken.lock().expect("lock").clear();
        }
    }

    #[test]
    fn the_default_declines_everything_and_flushes_successfully() {
        // Nothing was taken, so nothing can be lost -- a `false` here would
        // make every caller report an error for a scan that went fine.
        let a = Accel::none();
        let data = [u64::MAX; 4];
        let f = [u64::MAX; 2];
        let filters: Vec<&[u64]> = vec![&f, &f];
        assert!(!a.worth_offering(64));
        assert!(!a.enqueue(ScanId(1), ChunkId(1), &data, 2, &filters, 1, 0, 2));
        let mut c0 = vec![0u64; 2];
        let mut c1 = vec![0u64; 2];
        let mut counts: Vec<&mut [u64]> = vec![&mut c0, &mut c1];
        assert!(a.flush(ScanId(1), &mut counts));
        assert_eq!((c0, c1), (vec![0, 0], vec![0, 0]));
    }

    #[test]
    fn enqueued_work_arrives_at_flush_and_accumulates() {
        let a = Accel::new(Deferring::default());
        // Two chunks of two rows, two filters, landing at owner 0 and 2.
        let d0 = [0b1011u64, 0xff, 0b0110, 0x0f];
        let d1 = [0b1111u64, 0x01, 0b1000, 0xf0];
        let f0 = [0b0011u64, 0x0f];
        let f1 = [u64::MAX, 0];
        let filters: Vec<&[u64]> = vec![&f0, &f1];
        assert!(a.enqueue(ScanId(9), ChunkId(1), &d0, 2, &filters, 1, 0, 2));
        assert!(a.enqueue(ScanId(9), ChunkId(2), &d1, 2, &filters, 1, 2, 2));

        let mut c0 = vec![0u64; 4];
        let mut c1 = vec![0u64; 4];
        {
            let mut counts: Vec<&mut [u64]> = vec![&mut c0, &mut c1];
            assert!(a.flush(ScanId(9), &mut counts));
        }
        for (f, filter) in filters.iter().enumerate() {
            for (chunk, data) in [&d0, &d1].iter().enumerate() {
                for r in 0..2 {
                    let want = u64::from(reference(&data[r * 2..(r + 1) * 2], filter));
                    let got = if f == 0 { &c0 } else { &c1 }[chunk * 2 + r];
                    assert_eq!(got, want, "filter {f} chunk {chunk} row {r}");
                }
            }
        }
    }

    #[test]
    fn flush_adds_to_what_the_caller_already_counted() {
        // Declined chunks are counted inline by the caller into the same
        // vectors, so flush must accumulate rather than overwrite.
        let a = Accel::new(Deferring::default());
        let d = [0xffff_ffff_ffff_ffffu64, u64::MAX];
        let f = [u64::MAX, u64::MAX];
        let filters: Vec<&[u64]> = vec![&f];
        assert!(a.enqueue(ScanId(3), ChunkId(1), &d, 2, &filters, 1, 0, 1));
        let mut c0 = vec![100u64];
        {
            let mut counts: Vec<&mut [u64]> = vec![&mut c0];
            assert!(a.flush(ScanId(3), &mut counts));
        }
        assert_eq!(
            c0[0],
            100 + 128,
            "the caller's existing count was discarded"
        );
    }

    #[test]
    fn a_failing_flush_reports_rather_than_undercounting() {
        // The hazard deferral introduces: the caller skipped this work, so a
        // silent `true` here produces a plausible, wrong, smaller answer.
        let a = Accel::new(Deferring {
            fail_flush: true,
            ..Default::default()
        });
        let d = [u64::MAX; 2];
        let f = [u64::MAX; 2];
        let filters: Vec<&[u64]> = vec![&f];
        assert!(a.enqueue(ScanId(5), ChunkId(1), &d, 2, &filters, 1, 0, 1));
        let mut c0 = vec![0u64];
        let mut counts: Vec<&mut [u64]> = vec![&mut c0];
        assert!(!a.flush(ScanId(5), &mut counts));
    }

    #[test]
    fn cancelling_discards_without_delivering() {
        let a = Accel::new(Deferring::default());
        let d = [u64::MAX; 2];
        let f = [u64::MAX; 2];
        let filters: Vec<&[u64]> = vec![&f];
        assert!(a.enqueue(ScanId(7), ChunkId(1), &d, 2, &filters, 1, 0, 1));
        a.cancel(ScanId(7));
        let mut c0 = vec![0u64];
        let mut counts: Vec<&mut [u64]> = vec![&mut c0];
        assert!(a.flush(ScanId(7), &mut counts));
        assert_eq!(c0[0], 0, "a cancelled scan must deliver nothing");
    }

    #[test]
    fn identity_compares_by_value_because_residency_depends_on_it() {
        assert_eq!(ChunkId(3), ChunkId(3));
        assert_ne!(ChunkId(3), ChunkId(17));
        assert_ne!(next_scan(), next_scan());
    }
}
