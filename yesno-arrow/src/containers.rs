//! Containers as `RecordBatch`es — the dump, backup and interchange format.
//!
//! # Why this is not the ordinal path with extra steps
//!
//! [`crate::batch`] turns a set into `u64`s, which costs `8 * cardinality`
//! bytes and a decode per ordinal. This ships the **container payloads
//! themselves**, byte-identical to what the page store holds and to what a
//! `.roaring` file holds — so a dump is a copy, and reading one back is
//! `O( container count )` rather than `O( cardinality )`.
//!
//! For a dense bitmap chunk that is 8 KiB against 512 KiB, and the ratio only
//! widens as a set gets denser. It is the same argument that makes the physical
//! replication bootstrap beat an Arrow IPC one: moving bytes that are already in
//! the right shape beats rebuilding them.
//!
//! # What a row is
//!
//! `{ key, prefix48, kind, cardinality, payload }`. The middle three are not
//! decoration: `codec::decode` needs all of them, and none is derivable from the
//! payload alone — an array of `n` values and a run of `n / 2` intervals occupy
//! the same bytes, and a bitmap's cardinality is a popcount a reader should not
//! have to redo. The on-disk `ChunkRef` carries `kind` and `card_m1` beside the
//! cell for exactly the same reason.
//!
//! # Round-tripping is the contract
//!
//! [`ContainerBatchBuilder`] out, [`read_containers`] back, and the sets compare
//! equal — asserted rather than assumed, because "the payload is opaque" is only
//! true if nothing along the way reinterprets it.

use std::sync::Arc;

use arrow_array::{Array, BinaryArray, RecordBatch, UInt32Array, UInt64Array, UInt8Array};
use arrow_buffer::ScalarBuffer;
use yesno_core::container::codec;
use yesno_core::{CodecError, Container, ContainerKind, OrdSet, Prefix48, Result};

use crate::schema::containers_schema;

/// Accumulates containers and emits them as S4 batches.
///
/// One key at a time is the common case, but `key` is per row, so a whole
/// database dumps into one stream without a batch boundary per key.
#[derive(Default)]
pub struct ContainerBatchBuilder {
    key: Vec<u64>,
    prefix: Vec<u64>,
    kind: Vec<u8>,
    card: Vec<u32>,
    payload: Vec<u8>,
    offsets: Vec<i32>,
}

impl ContainerBatchBuilder {
    pub fn new() -> Self {
        ContainerBatchBuilder {
            offsets: vec![0],
            ..Default::default()
        }
    }

    /// Rows accumulated so far.
    #[inline]
    pub fn len(&self) -> usize {
        self.key.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.key.is_empty()
    }

    /// Append one container.
    ///
    /// `codec::encode` is the *same* function the page store and the
    /// `.roaring` writer use. Re-deriving the bytes here would be a second
    /// encoder, and a second encoder is a thing that drifts — the argument this
    /// project makes about WAL framing, applied to payloads.
    pub fn push(&mut self, key: u64, prefix: Prefix48, c: &Container) {
        self.key.push(key);
        self.prefix.push(prefix);
        self.kind.push(c.kind() as u8);
        self.card.push(c.len());
        self.payload.extend_from_slice(&codec::encode(c));
        self.offsets.push(self.payload.len() as i32);
    }

    /// Append every chunk of a set under one key.
    pub fn push_set(&mut self, key: u64, set: &OrdSet) {
        for (p, c) in set.chunks() {
            self.push(key, p, c);
        }
    }

    /// Emit what has accumulated, leaving the builder empty.
    ///
    /// `None` when there is nothing — an empty `RecordBatch` is a legal thing to
    /// build and a confusing thing to receive.
    pub fn finish(&mut self) -> Result<Option<RecordBatch>> {
        if self.key.is_empty() {
            return Ok(None);
        }
        let payload = std::mem::take(&mut self.payload);
        let offsets = std::mem::replace(&mut self.offsets, vec![0]);
        // No validity buffer: see the crate docs. `BinaryArray::new` takes the
        // offsets and values directly, so the payload bytes are moved rather
        // than copied per row.
        let values = BinaryArray::new(
            arrow_buffer::OffsetBuffer::new(ScalarBuffer::from(offsets)),
            payload.into(),
            None,
        );
        let batch = RecordBatch::try_new(
            containers_schema(),
            vec![
                Arc::new(UInt64Array::new(
                    ScalarBuffer::from(std::mem::take(&mut self.key)),
                    None,
                )),
                Arc::new(UInt64Array::new(
                    ScalarBuffer::from(std::mem::take(&mut self.prefix)),
                    None,
                )),
                Arc::new(UInt8Array::new(
                    ScalarBuffer::from(std::mem::take(&mut self.kind)),
                    None,
                )),
                Arc::new(UInt32Array::new(
                    ScalarBuffer::from(std::mem::take(&mut self.card)),
                    None,
                )),
                Arc::new(values),
            ],
        )
        .map_err(|_| CodecError::Invariant("failed to build a containers RecordBatch"))?;
        Ok(Some(batch))
    }
}

fn kind_of(v: u8) -> Result<ContainerKind> {
    match v {
        0 => Ok(ContainerKind::Array),
        1 => Ok(ContainerKind::Bitmap),
        2 => Ok(ContainerKind::Run),
        _ => Err(CodecError::UnsupportedEncoding),
    }
}

fn column<'a, T: 'static>(b: &'a RecordBatch, i: usize, name: &str) -> Result<&'a T> {
    b.column(i)
        .as_any()
        .downcast_ref::<T>()
        .ok_or(CodecError::Invariant(match name {
            "key" => "containers batch: key is not UInt64",
            "prefix48" => "containers batch: prefix48 is not UInt64",
            "kind" => "containers batch: kind is not UInt8",
            "cardinality" => "containers batch: cardinality is not UInt32",
            _ => "containers batch: payload is not Binary",
        }))
}

/// Read one S4 batch back into `(key, prefix, container)` rows.
///
/// Every payload goes through `codec::decode`, which validates. A dump is an
/// untrusted input the moment it has been on disk or over a wire, and the same
/// function is a fuzz target by contract — so a corrupt row is an `Err` here
/// rather than a container that violates its own invariants somewhere later.
pub fn read_containers(b: &RecordBatch) -> Result<Vec<(u64, Prefix48, Container)>> {
    if b.num_columns() != 5 {
        return Err(CodecError::Invariant(
            "containers batch: expected five columns",
        ));
    }
    let keys: &UInt64Array = column(b, 0, "key")?;
    let prefixes: &UInt64Array = column(b, 1, "prefix48")?;
    let kinds: &UInt8Array = column(b, 2, "kind")?;
    let cards: &UInt32Array = column(b, 3, "cardinality")?;
    let payloads: &BinaryArray = column(b, 4, "payload")?;

    let mut out = Vec::with_capacity(b.num_rows());
    for i in 0..b.num_rows() {
        let prefix = prefixes.value(i);
        if prefix >= 1 << 48 {
            return Err(CodecError::Invariant(
                "containers batch: prefix48 does not fit 48 bits",
            ));
        }
        let c = codec::decode(kind_of(kinds.value(i))?, payloads.value(i), cards.value(i))?;
        out.push((keys.value(i), prefix, c));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shapes() -> OrdSet {
        // One of each kind, plus a chunk that optimizes to a run.
        let mut vals: Vec<u64> = (0..500u64).map(|i| i * 7).collect(); // array
        vals.extend((1 << 16..(1 << 16) + 40_000).map(|i| i * 3 % 65_536 + (1 << 16))); // bitmap
        vals.extend((2 << 16)..(2 << 16) + 5_000); // run
        let mut s = OrdSet::from_iter_unsorted(vals);
        s.optimize();
        s
    }

    #[test]
    fn a_dump_round_trips_to_the_same_set() {
        let set = shapes();
        let mut b = ContainerBatchBuilder::new();
        b.push_set(7, &set);
        assert_eq!(b.len(), set.chunk_count());
        let batch = b.finish().unwrap().expect("a non-empty set yields a batch");
        assert_eq!(batch.num_rows(), set.chunk_count());
        assert!(b.is_empty(), "finish must leave the builder reusable");

        let rows = read_containers(&batch).unwrap();
        assert!(rows.iter().all(|(k, _, _)| *k == 7));
        let back = OrdSet::from_chunks(rows.into_iter().map(|(_, p, c)| (p, c)).collect());
        assert_eq!(back, set, "the dump did not reproduce the set");
    }

    /// The payload is the *store's* bytes, not a re-encoding.
    #[test]
    fn payloads_are_the_containers_own_encoding() {
        let set = shapes();
        let mut b = ContainerBatchBuilder::new();
        b.push_set(1, &set);
        let batch = b.finish().unwrap().unwrap();
        let payloads: &BinaryArray = column(&batch, 4, "payload").unwrap();
        for (i, (_, c)) in set.chunks().enumerate() {
            assert_eq!(
                payloads.value(i),
                codec::encode(c).as_slice(),
                "row {i} is not the container's own bytes"
            );
        }
    }

    /// Every kind must actually appear, or the round trip proves one path.
    #[test]
    fn the_corpus_covers_every_kind() {
        let set = shapes();
        let kinds: std::collections::BTreeSet<u8> =
            set.chunks().map(|(_, c)| c.kind() as u8).collect();
        assert_eq!(kinds.len(), 3, "expected all three kinds, got {kinds:?}");
    }

    #[test]
    fn an_empty_builder_yields_no_batch() {
        assert!(ContainerBatchBuilder::new().finish().unwrap().is_none());
    }

    /// A dump is untrusted input once it has been anywhere.
    #[test]
    fn a_bad_kind_is_refused_rather_than_guessed() {
        let set = shapes();
        let mut b = ContainerBatchBuilder::new();
        b.push_set(1, &set);
        let batch = b.finish().unwrap().unwrap();

        let mut kinds: Vec<u8> = (0..batch.num_rows()).map(|_| 9u8).collect();
        kinds[0] = 9;
        let broken = RecordBatch::try_new(
            containers_schema(),
            vec![
                batch.column(0).clone(),
                batch.column(1).clone(),
                Arc::new(UInt8Array::new(ScalarBuffer::from(kinds), None)),
                batch.column(3).clone(),
                batch.column(4).clone(),
            ],
        )
        .unwrap();
        assert!(read_containers(&broken).is_err(), "kind 9 does not exist");
    }

    #[test]
    fn a_prefix_past_48_bits_is_refused() {
        let mut b = ContainerBatchBuilder::new();
        b.push_set(1, &OrdSet::from_sorted_slice(&[1u64, 2, 3]));
        let batch = b.finish().unwrap().unwrap();
        let broken = RecordBatch::try_new(
            containers_schema(),
            vec![
                batch.column(0).clone(),
                Arc::new(UInt64Array::new(ScalarBuffer::from(vec![1u64 << 48]), None)),
                batch.column(2).clone(),
                batch.column(3).clone(),
                batch.column(4).clone(),
            ],
        )
        .unwrap();
        assert!(read_containers(&broken).is_err());
    }
}
