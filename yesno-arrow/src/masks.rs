//! Selection masks — the path that never materializes ordinals.
//!
//! A bitmap container's bytes *are* an Arrow `BooleanBuffer`, so a posting list
//! becomes a row filter for the cost of a refcount bump. Applying a mask to a
//! columnar batch never needs the ordinals as integers, which is the whole
//! reason to prefer this over [`crate::batch`].

use arrow_array::BooleanArray;
use arrow_buffer::BooleanBuffer;
use yesno_core::stream::ChunkStream;
use yesno_core::unstable_arrow::{bitmap_mask, container_mask, empty_mask};
use yesno_core::{Container, Prefix48, Result, CHUNK_CARD};

/// A mask over one chunk: exactly [`CHUNK_CARD`] bits starting at
/// `base_ordinal`.
#[derive(Clone, Debug)]
pub struct MaskChunk {
    pub base_ordinal: u64,
    pub mask: BooleanBuffer,
}

impl MaskChunk {
    /// As an Arrow array, ready for a filter kernel. No validity buffer.
    #[inline]
    pub fn to_array(&self) -> BooleanArray {
        BooleanArray::new(self.mask.clone(), None)
    }

    /// Rows selected in this chunk.
    #[inline]
    pub fn selected(&self) -> usize {
        self.mask.count_set_bits()
    }

    /// A sub-window, for a consumer whose batch does not align to a chunk.
    ///
    /// `BooleanBuffer::slice` is O(1) and produces a non-zero bit offset, which
    /// Arrow's kernels handle. Do **not** normalize the offset to zero — that
    /// would copy, defeating the point.
    #[inline]
    pub fn slice(&self, offset: usize, len: usize) -> BooleanBuffer {
        self.mask.slice(offset, len)
    }
}

/// Turns a chunk stream into per-chunk selection masks.
///
/// By default only chunks the set actually touches are emitted. A consumer
/// scanning a contiguous ordinal space usually wants a mask for *every* chunk in
/// range, including empty ones — [`MaskStream::dense`] does that.
pub struct MaskStream<S> {
    inner: S,
    dense_upto: Option<Prefix48>,
    next_prefix: Prefix48,
    pending: Option<(Prefix48, Container)>,
    done: bool,
}

impl<S: ChunkStream> MaskStream<S> {
    pub fn new(inner: S) -> Self {
        MaskStream {
            inner,
            dense_upto: None,
            next_prefix: 0,
            pending: None,
            done: false,
        }
    }

    /// Emit a mask for every chunk in `[from, to]` inclusive, using an all-zero
    /// mask where the set has nothing.
    pub fn dense(inner: S, from: Prefix48, to: Prefix48) -> Self {
        MaskStream {
            inner,
            dense_upto: Some(to),
            next_prefix: from,
            pending: None,
            done: false,
        }
    }

    fn step(&mut self) -> Result<Option<MaskChunk>> {
        if self.done {
            return Ok(None);
        }
        // Sparse: just map each chunk the stream yields.
        let Some(upto) = self.dense_upto else {
            return Ok(self.inner.next_chunk()?.map(|(p, c)| MaskChunk {
                base_ordinal: yesno_core::chunk_base(p),
                mask: container_mask(&c),
            }));
        };

        if self.next_prefix > upto {
            self.done = true;
            return Ok(None);
        }
        if self.pending.is_none() {
            self.pending = self.inner.next_chunk()?;
        }
        let p = self.next_prefix;
        self.next_prefix += 1;

        let mask = match &self.pending {
            Some((pp, c)) if *pp == p => {
                let m = container_mask(c);
                self.pending = None;
                m
            }
            // Nothing here, or the stream has moved past this prefix.
            _ => empty_mask(),
        };
        Ok(Some(MaskChunk {
            base_ordinal: yesno_core::chunk_base(p),
            mask,
        }))
    }
}

impl<S: ChunkStream> Iterator for MaskStream<S> {
    type Item = Result<MaskChunk>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.step() {
            Ok(Some(m)) => Some(Ok(m)),
            Ok(None) => None,
            Err(e) => {
                self.done = true;
                Some(Err(e))
            }
        }
    }
}

/// Whether a container can lend its bits without building anything.
///
/// Useful for a planner deciding between the mask path and materialization:
/// only a bitmap is free.
#[inline]
pub fn is_zero_copy(c: &Container) -> bool {
    bitmap_mask(c).is_some()
}

/// Bits per mask chunk.
pub const MASK_BITS: usize = CHUNK_CARD as usize;

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use arrow_array::Array;
    use yesno_core::stream::{ChunkStreamExt, SetStream};
    use yesno_core::OrdSet;

    fn stream(vals: &[u64]) -> SetStream {
        SetStream::new(Arc::new(OrdSet::from_iter_unsorted(vals.iter().copied())))
    }

    #[test]
    fn a_mask_selects_exactly_the_sets_ordinals() {
        let vals: Vec<u64> = (0..1000u64).map(|i| i * 3).collect();
        let masks: Vec<MaskChunk> = MaskStream::new(stream(&vals))
            .collect::<Result<Vec<_>>>()
            .unwrap();

        let total: usize = masks.iter().map(|m| m.selected()).sum();
        assert_eq!(total, vals.len());

        for m in &masks {
            for bit in 0..MASK_BITS {
                let ordinal = m.base_ordinal + bit as u64;
                assert_eq!(
                    m.mask.value(bit),
                    vals.contains(&ordinal),
                    "mask disagrees at ordinal {ordinal}"
                );
            }
        }
    }

    #[test]
    fn masks_have_no_validity_buffer() {
        let m = MaskStream::new(stream(&[1, 2, 3])).next().unwrap().unwrap();
        let arr = m.to_array();
        assert!(
            arr.nulls().is_none(),
            "a mask must never carry a validity buffer"
        );
        assert_eq!(arr.len(), MASK_BITS);
    }

    #[test]
    fn a_dense_bitmap_chunk_is_zero_copy() {
        // Scattered values across one chunk, so it stays a bitmap.
        let vals: Vec<u64> = (0..5000u64).map(|i| i * 2).collect();
        let mut set = OrdSet::from_iter_unsorted(vals.iter().copied());
        set.optimize();
        let c = set.chunks().next().unwrap().1;
        assert!(
            is_zero_copy(c),
            "a bitmap container must lend its bits directly"
        );
    }

    #[test]
    fn sparse_and_run_chunks_still_produce_correct_masks() {
        // They cannot lend bits, but the mask must be identical to a bitmap's.
        let sparse: Vec<u64> = vec![1, 5, 9];
        let m = MaskStream::new(stream(&sparse)).next().unwrap().unwrap();
        assert_eq!(m.selected(), 3);
        for &v in &sparse {
            assert!(m.mask.value(v as usize));
        }
        assert!(!m.mask.value(0));
    }

    #[test]
    fn dense_mode_emits_a_mask_for_every_chunk_in_range() {
        // Ordinals only in chunks 0 and 3.
        let vals: Vec<u64> = vec![5, 3 * 65_536 + 7];
        let masks: Vec<MaskChunk> = MaskStream::dense(stream(&vals), 0, 4)
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(masks.len(), 5, "one per chunk in [0, 4]");
        assert_eq!(masks[0].selected(), 1);
        assert_eq!(masks[1].selected(), 0, "an absent chunk masks nothing");
        assert_eq!(masks[2].selected(), 0);
        assert_eq!(masks[3].selected(), 1);
        assert_eq!(masks[4].selected(), 0);
        // Base ordinals must advance by a whole chunk each time.
        for (i, m) in masks.iter().enumerate() {
            assert_eq!(m.base_ordinal, i as u64 * 65_536);
        }
    }

    #[test]
    fn a_mask_slice_is_a_view_not_a_copy() {
        let vals: Vec<u64> = (0..5000u64).map(|i| i * 2).collect();
        let m = MaskStream::new(stream(&vals)).next().unwrap().unwrap();
        let win = m.slice(64, 128);
        assert_eq!(win.len(), 128);
        for bit in 0..128usize {
            assert_eq!(win.value(bit), m.mask.value(64 + bit));
        }
    }

    #[test]
    fn masks_from_a_lazy_expression_match_the_materialized_set() {
        let a: Vec<u64> = (0..2000u64).map(|i| i * 3).collect();
        let b: Vec<u64> = (0..2000u64).map(|i| i * 5).collect();
        let expected = OrdSet::from_iter_unsorted(a.iter().copied())
            .and(&OrdSet::from_iter_unsorted(b.iter().copied()));

        let masks: Vec<MaskChunk> = MaskStream::new(stream(&a).and(stream(&b)))
            .collect::<Result<Vec<_>>>()
            .unwrap();
        let total: usize = masks.iter().map(|m| m.selected()).sum();
        assert_eq!(total as u64, expected.len());
    }

    #[test]
    fn an_empty_stream_yields_no_masks() {
        assert_eq!(MaskStream::new(stream(&[])).count(), 0);
    }
}
