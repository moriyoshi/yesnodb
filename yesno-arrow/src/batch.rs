//! Ordinals as `RecordBatch`es.
//!
//! The general-purpose path, and the slower one: it materializes ordinals as
//! `u64`s, which is precisely what [`crate::masks`] exists to avoid. Reach for
//! it when the consumer genuinely needs integers — a join key, an export — not
//! when it needs a filter.
//!
//! Implements [`arrow_array::RecordBatchReader`], so it plugs into the IPC and Parquet
//! writers without an adapter.

use std::sync::Arc;

use arrow_array::{Array, RecordBatch, UInt64Array};
use arrow_buffer::ScalarBuffer;
use arrow_schema::{ArrowError, SchemaRef};
use yesno_core::container::DecodeCursor;
use yesno_core::stream::ChunkStream;
use yesno_core::Container;
use yesno_core::Result;

use crate::schema::ordinals_schema;

/// How ordinals are grouped into batches.
#[derive(Clone, Copy, Debug)]
pub struct BatchPolicy {
    /// Rows to aim for. 8192 `u64`s is 64 KiB, which fits L2 and matches the
    /// batch size query engines default to.
    pub target_rows: usize,
    /// Emit one batch per chunk, so every batch's ordinals share a prefix.
    /// Required by consumers doing chunk-wise joins.
    pub chunk_aligned: bool,
}

impl Default for BatchPolicy {
    fn default() -> Self {
        BatchPolicy {
            target_rows: 8192,
            chunk_aligned: false,
        }
    }
}

/// Streams ordinals as `{ ordinal: UInt64 }` batches.
///
/// **Coalesces across chunks.** An array container holds at most 4096 values, so
/// emitting one batch per chunk would halve throughput for sparse data; the
/// reader accumulates until `target_rows` unless `chunk_aligned` is set.
pub struct OrdinalBatchReader<S> {
    inner: S,
    policy: BatchPolicy,
    schema: SchemaRef,
    /// The chunk being drained, and how far into it.
    ///
    /// A `Container` and a cursor, **not** its decoded ordinals. This used to
    /// be a `std::vec::IntoIter<u64>` filled by `c.iter().collect()`, so peak
    /// extra memory was one full container's worth of `u64` — **512 KiB to emit
    /// an 8 192-row batch**, and every batch after the first read out of a buffer
    /// that had already been built in full. The design says never fully decode a
    /// bitmap before slicing, and `Container::fill_from` is what makes that
    /// possible across a borrow: cloning a `Container` is a refcount bump, and
    /// the cursor survives the `Chunk<'_>` the stream lent us.
    carry: Option<(Container, u64, DecodeCursor)>,
    done: bool,
}

impl<S: ChunkStream> OrdinalBatchReader<S> {
    pub fn new(inner: S) -> Self {
        Self::with_policy(inner, BatchPolicy::default())
    }

    pub fn with_policy(inner: S, policy: BatchPolicy) -> Self {
        OrdinalBatchReader {
            inner,
            policy,
            schema: ordinals_schema(),
            carry: None,
            done: false,
        }
    }

    fn next_batch(&mut self) -> Result<Option<RecordBatch>> {
        if self.done {
            return Ok(None);
        }
        let mut out: Vec<u64> = Vec::with_capacity(self.policy.target_rows);

        loop {
            // Drain whatever is left of the current chunk first.
            if let Some((c, base, cur)) = self.carry.as_mut() {
                let want = self.policy.target_rows - out.len();
                if c.fill_from(cur, *base, want, &mut out) < want {
                    // Exhausted: nothing of this chunk is left to resume into.
                    self.carry = None;
                }
                if out.len() >= self.policy.target_rows {
                    return Ok(Some(self.build(out)?));
                }
            }
            match self.inner.next_chunk()? {
                Some((prefix, c)) => {
                    let base = yesno_core::chunk_base(prefix);
                    if self.policy.chunk_aligned && !out.is_empty() {
                        // Do not mix two chunks into one batch. The container is
                        // kept, not decoded, so this costs a refcount bump.
                        self.carry = Some((c.clone(), base, DecodeCursor::default()));
                        return Ok(Some(self.build(out)?));
                    }
                    if self.policy.chunk_aligned {
                        let mut vals = Vec::with_capacity(c.len() as usize);
                        let mut cur = DecodeCursor::default();
                        c.fill_from(&mut cur, base, c.len() as usize, &mut vals);
                        return Ok(Some(self.build(vals)?));
                    }
                    self.carry = Some((c.clone(), base, DecodeCursor::default()));
                }
                None => {
                    self.done = true;
                    return if out.is_empty() {
                        Ok(None)
                    } else {
                        Ok(Some(self.build(out)?))
                    };
                }
            }
        }
    }

    /// Build without a validity buffer.
    ///
    /// Deliberately not `UInt64Array::from(vec)`, which allocates one. There are
    /// no nulls here by construction, and a validity buffer would double the
    /// allocation and force null handling into every downstream kernel.
    fn build(&self, vals: Vec<u64>) -> Result<RecordBatch> {
        let arr = UInt64Array::new(ScalarBuffer::from(vals), None);
        RecordBatch::try_new(self.schema.clone(), vec![Arc::new(arr)])
            .map_err(|_| yesno_core::CodecError::Invariant("failed to build a RecordBatch"))
    }

    #[inline]
    pub fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }
}

impl<S: ChunkStream> Iterator for OrdinalBatchReader<S> {
    type Item = std::result::Result<RecordBatch, ArrowError>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.next_batch() {
            Ok(Some(b)) => Some(Ok(b)),
            Ok(None) => None,
            Err(e) => {
                self.done = true;
                Some(Err(ArrowError::ExternalError(Box::new(e))))
            }
        }
    }
}

impl<S: ChunkStream> arrow_array::RecordBatchReader for OrdinalBatchReader<S> {
    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }
}

/// Build an [`OrdSet`](yesno_core::OrdSet) from an Arrow column.
///
/// Sorts and dedups if needed, then builds each container directly in its final
/// representation rather than inserting one value at a time — the reason bulk
/// load is dramatically faster than repeated `insert`.
pub fn set_from_array(a: &UInt64Array) -> yesno_core::OrdSet {
    debug_assert!(a.nulls().is_none(), "a posting list has no nulls");
    let mut vals: Vec<u64> = a.values().to_vec();
    if vals.windows(2).any(|w| w[0] >= w[1]) {
        vals.sort_unstable();
        vals.dedup();
    }
    yesno_core::OrdSet::from_sorted_slice(&vals)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use yesno_core::stream::{ChunkStreamExt, SetStream};
    use yesno_core::OrdSet;

    fn stream(vals: &[u64]) -> SetStream {
        SetStream::new(Arc::new(OrdSet::from_iter_unsorted(vals.iter().copied())))
    }

    fn collect(r: OrdinalBatchReader<SetStream>) -> Vec<u64> {
        let mut out = Vec::new();
        for b in r {
            let b = b.unwrap();
            let a = b.column(0).as_any().downcast_ref::<UInt64Array>().unwrap();
            out.extend(a.values().iter().copied());
        }
        out
    }

    #[test]
    fn batches_reproduce_the_set_in_order() {
        let vals: Vec<u64> = (0..30_000u64).map(|i| i * 7).collect();
        assert_eq!(collect(OrdinalBatchReader::new(stream(&vals))), vals);
    }

    #[test]
    fn batches_have_no_validity_buffer() {
        let b = OrdinalBatchReader::new(stream(&[1, 2, 3]))
            .next()
            .unwrap()
            .unwrap();
        assert!(b.column(0).nulls().is_none(), "no nulls by construction");
        assert_eq!(b.num_rows(), 3);
    }

    #[test]
    fn batches_coalesce_across_chunks() {
        // Three sparse chunks: one batch per chunk would be wasteful, so the
        // reader must merge them.
        let vals: Vec<u64> = vec![1, 2, 65_536, 65_537, 131_072];
        let batches: Vec<_> = OrdinalBatchReader::new(stream(&vals))
            .map(|b| b.unwrap())
            .collect();
        assert_eq!(
            batches.len(),
            1,
            "five ordinals should not need three batches"
        );
        assert_eq!(batches[0].num_rows(), 5);
    }

    #[test]
    fn target_rows_bounds_each_batch() {
        let vals: Vec<u64> = (0..25_000u64).collect();
        let policy = BatchPolicy {
            target_rows: 1000,
            chunk_aligned: false,
        };
        let batches: Vec<_> = OrdinalBatchReader::with_policy(stream(&vals), policy)
            .map(|b| b.unwrap())
            .collect();
        assert!(batches.len() >= 25);
        for b in &batches {
            assert!(
                b.num_rows() <= 1000,
                "batch of {} exceeds the target",
                b.num_rows()
            );
        }
        let total: usize = batches.iter().map(|b| b.num_rows()).sum();
        assert_eq!(total, vals.len());
    }

    #[test]
    fn chunk_aligned_batches_never_mix_prefixes() {
        let vals: Vec<u64> = vec![1, 2, 65_536, 65_537, 131_072];
        let policy = BatchPolicy {
            target_rows: 8192,
            chunk_aligned: true,
        };
        let batches: Vec<_> = OrdinalBatchReader::with_policy(stream(&vals), policy)
            .map(|b| b.unwrap())
            .collect();
        assert_eq!(batches.len(), 3, "one batch per chunk");
        for b in &batches {
            let a = b.column(0).as_any().downcast_ref::<UInt64Array>().unwrap();
            let prefixes: std::collections::BTreeSet<u64> =
                a.values().iter().map(|v| v >> 16).collect();
            assert_eq!(
                prefixes.len(),
                1,
                "a chunk-aligned batch must span one prefix"
            );
        }
    }

    #[test]
    fn an_empty_stream_yields_no_batches() {
        assert_eq!(OrdinalBatchReader::new(stream(&[])).count(), 0);
    }

    #[test]
    fn a_lazy_expression_streams_correctly() {
        let a: Vec<u64> = (0..5000u64).map(|i| i * 3).collect();
        let b: Vec<u64> = (0..5000u64).map(|i| i * 5).collect();
        let expected: Vec<u64> = OrdSet::from_iter_unsorted(a.iter().copied())
            .and(&OrdSet::from_iter_unsorted(b.iter().copied()))
            .iter()
            .collect();

        let mut out = Vec::new();
        for batch in OrdinalBatchReader::new(stream(&a).and(stream(&b))) {
            let batch = batch.unwrap();
            let arr = batch
                .column(0)
                .as_any()
                .downcast_ref::<UInt64Array>()
                .unwrap();
            out.extend(arr.values().iter().copied());
        }
        assert_eq!(out, expected);
    }

    #[test]
    fn set_from_array_round_trips() {
        let vals: Vec<u64> = (0..10_000u64).map(|i| i * 11).collect();
        let arr = UInt64Array::new(ScalarBuffer::from(vals.clone()), None);
        let set = set_from_array(&arr);
        assert_eq!(set.len(), vals.len() as u64);
        assert_eq!(set.iter().collect::<Vec<_>>(), vals);
    }

    #[test]
    fn set_from_array_handles_unsorted_and_duplicate_input() {
        let arr = UInt64Array::new(ScalarBuffer::from(vec![9u64, 1, 5, 1, 9]), None);
        let set = set_from_array(&arr);
        assert_eq!(set.iter().collect::<Vec<_>>(), vec![1, 5, 9]);
    }

    #[test]
    fn the_reader_is_a_record_batch_reader() {
        use arrow_array::RecordBatchReader;
        let r = OrdinalBatchReader::new(stream(&[1, 2, 3]));
        // The trait impl is what makes IPC and Parquet writers work unmodified.
        assert_eq!(RecordBatchReader::schema(&r).field(0).name(), "ordinal");
    }
}
