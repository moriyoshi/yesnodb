//! Turning one WAL record into memtable state.
//!
//! # One decoder, and this is it
//!
//! This was the body of `replay_logs`'s `match`, extracted rather than
//! copied. Crash recovery and a live replica answer the same question — *what
//! does this record mean* — and the whole replication design rests on their
//! answering it with the same code: "the leader ships raw on-disk WAL frames, so
//! the follower's apply path is byte-identical to crash recovery: one framing,
//! one decoder, one fuzz target". That claim is stated in three separate module
//! headers, and a second `match rec.rtype` anywhere would quietly retire it.
//!
//! So: do not add a second one. If a record type needs handling, it needs it
//! here, and both callers get it.

use super::memtable::Memtable;
use super::Shard;
use crate::error::{CodecError, Result};
use crate::mvcc::Version;
use crate::wal::record::{self, RecType, Record};

/// Whether a record carries redo material at all.
///
/// Markers — commit intents, shard commits, aborts, padding, checkpoint bounds
/// and the epoch fence — say *when* something happened rather than *what*, and
/// are consumed by the commit table instead.
pub(super) fn is_redo_material(r: &Record) -> bool {
    matches!(
        r.rtype,
        RecType::SetRange
            | RecType::ChunkDelta
            | RecType::ChunkDelete
            | RecType::ChunkImage
            | RecType::ChunkPatch
    )
}

/// Apply one record to `mem`, at the version the record carries.
///
/// The caller holds the memtable's write lock, because a batch applies many
/// records and re-taking it per record would let a reader observe a partially
/// applied commit.
pub(super) fn apply_record(sh: &Shard, mem: &mut Memtable, rec: &Record) -> Result<()> {
    let at: Version = rec.commit_version;
    match rec.rtype {
        RecType::SetRange => {
            let (key, lo, hi, remove) = record::decode_set_range(&rec.body)?;
            // Through the range path, not a per-ordinal loop: replaying a bulk
            // load is the one time this is guaranteed to be hot, and recovery
            // has no reason to be slower than the write it redoes.
            if remove {
                mem.remove_range(key, lo, hi, at, |prefix| sh.disk_chunk(key, prefix));
            } else {
                mem.insert_range(key, lo, hi, at, |prefix| sh.disk_chunk(key, prefix));
            }
        }
        RecType::ChunkDelta => {
            // Scattered values inside one chunk, coalesced by `plan_records`.
            // Adds and removes never coexist in a body the planner builds, but
            // the format allows both, so apply both.
            let (key, prefix, add, rem) = record::decode_chunk_delta(&rec.body)?;
            let base = prefix << crate::CHUNK_BITS;
            for v in add {
                mem.insert(key, base | v as u64, at, || sh.disk_chunk(key, prefix));
            }
            for v in rem {
                mem.remove(key, base | v as u64, at, || sh.disk_chunk(key, prefix));
            }
        }
        RecType::ChunkDelete => {
            let key = u64::from_le_bytes(
                rec.body
                    .get(..8)
                    .and_then(|b| b.try_into().ok())
                    .ok_or(CodecError::Invariant("short ChunkDelete body"))?,
            );
            let on_disk = sh
                .store
                .as_ref()
                .map(|st| st.lock().unwrap().key_prefixes(key).unwrap_or_default())
                .unwrap_or_default();
            mem.delete_key(key, at, on_disk);
        }
        RecType::ChunkPatch => {
            // **The same routine the live path calls.** `ChunkImage` above is
            // the cautionary case: it replaces live and unions here, and the
            // two agree only because its one producer emits a delete first.
            // There is no second implementation here to keep in step.
            let (key, prefix, clear, set) = record::decode_chunk_patch(&rec.body)?;
            mem.patch_chunk(key, prefix, clear.as_ref(), set.as_ref(), at, || {
                sh.disk_chunk(key, prefix)
            });
        }
        RecType::ChunkImage => {
            // key, then the chunk's ordinals in full.
            //
            // **This record unions. The live path that writes it replaces.**
            // `Op::PutChunk` applies to the memtable through
            // `Memtable::put_chunk`, which overwrites the chunk's MVCC value,
            // and logs as this record, which replay applies with the
            // per-ordinal `insert` below. The two agree only because the sole
            // producer — `WriteBatch::store_set` — emits a `ChunkDelete` first,
            // so both are unioning into an emptied key.
            //
            // Do not add a `PutChunk` producer that omits that delete: it would
            // commit one state and replay another, and no test in the tree
            // would see it until a crash. `WriteBatch::merge_set` exists and
            // deliberately does *not* take that route; its doc explains why.
            // `store_set_replays_to_what_it_committed` pins the agreement.
            //
            // The comment here used to say this was "written by `store_chunk`,
            // which replaces a chunk wholesale" — describing semantics this
            // code does not implement, and naming a function that does not
            // exist anywhere in the tree.
            if rec.body.len() < 8 || !(rec.body.len() - 8).is_multiple_of(8) {
                return Err(CodecError::Invariant("malformed ChunkImage body"));
            }
            let key = u64::from_le_bytes(rec.body[..8].try_into().unwrap());
            for &o in rec.body[8..].as_chunks::<8>().0 {
                let ordinal = u64::from_le_bytes(o);
                let prefix = crate::split(ordinal).0;
                mem.insert(key, ordinal, at, || sh.disk_chunk(key, prefix));
            }
        }
        // Everything else is a marker the caller has already excluded.
        _ => {}
    }
    Ok(())
}

/// What one applied batch moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Applied {
    pub shard: u32,
    /// Bytes accepted into this shard's log.
    pub bytes: u64,
    /// Records that carried redo material.
    pub records: u64,
    /// Where the next batch for this shard must start. This is the ack value.
    pub next_lsn: u64,
    /// The replica's visible watermark after this call.
    pub visible: Version,
}

/// Everything a replica needs to remember between batches.
#[derive(Debug, Default)]
pub(crate) struct ApplyState {
    /// Shared with crash recovery **deliberately** — see
    /// [`crate::wal::recover::CommitTable`]. Both
    /// answer "which versions are complete?" from the same records, and two
    /// implementations of a rule this subtle would drift.
    pub(crate) table: crate::wal::recover::CommitTable,
}
