//! Allocator rebuild and consistency checking, driven entirely by the index.
//!
//! # Why the index is the only source of truth
//!
//! Under **I2** an extent is immutable, so a superseded extent's bytes stay
//! perfectly well-formed. Nothing local to a slot distinguishes a live extent
//! from a dead one — which is precisely why the format carries no extent header.
//! A header-scan rebuild would therefore have to back-check every slot against
//! the index anyway, making it `O(slots)` *plus* the index walk, where an
//! index-scan rebuild is `O(chunks)` and sequential.
//!
//! So: walk the B+tree in `ChunkKey` order, mark every referenced slot, and
//! recompute packed-page live bytes. Whatever the slabs claim is used but the
//! index never mentions is a **leak**. Whatever the index references but the
//! slabs think is free is a **dangling reference** — much worse, and the thing
//! this check exists to catch.
//!
//! # Stored checksums are recomputed here, and only here
//!
//! Three families of CRC32C are written by the format and were, until now,
//! never recomputed by anything: B+tree nodes, packed pages, and the
//! [`ExtTrailer`] of a standalone extent. Their presence was not evidence of
//! end-to-end verification, and the format reference said so.
//!
//! The scan is where they belong, because the online read path cannot afford
//! them: verifying a payload per read means a CRC over up to 8 KiB on every
//! chunk, which roughly doubles the cost of a bitmap intersection. What the
//! read path does instead is *identity* checking — `ckey_tag` and the packed
//! header's key range — which is four bytes and catches a mis-pointed
//! reference but says nothing about content.
//!
//! **What a failure means differs by family, and the difference is not
//! cosmetic.** `slabmeta` records the store's standing policy: that region is a
//! cache of derivable state, so a bad checksum there falls back to recomputing
//! it from the index. Nothing in *these* three families is derivable —
//!
//! - an **index node** is the authority on liveness, and the format carries no
//!   extent header to rebuild it from, so a bad node makes the liveness map
//!   incomplete. `rebuild_allocator_at_open` already refuses to adopt a rebuild
//!   that reported any error, which is exactly right: adopting an incomplete
//!   map does not fail to repair, it frees live data.
//! - a **packed page** or a **standalone payload** holds the data itself. A
//!   mismatch is unrecoverable data loss to be reported, not repaired.
//!
//! So all three are reported and none falls back. The only checksum in the
//! store that falls back is `slabmeta`'s, and it already did.
//!
//! # `class` is not in `ChunkRef`
//!
//! It was deleted to get the reference to 8 bytes, on the grounds that
//! `class = slab_table[cell >> 21].class` is an in-memory lookup off the hot
//! path. Rebuild is where that bill comes due: it needs the slab table to know
//! how large a slot is, which is why [`rebuild`] takes a `class_of_slab`
//! resolver rather than reading it from the reference.

use std::collections::{BTreeMap, BTreeSet};

use super::alloc::{slab_of, Allocator};
use super::checksum::crc32c;
use super::extent::{class_size, ChunkKey, ChunkRef, ExtTrailer, EXT_TRAILER_BYTES, PACKED_CLASS};
use super::packed::PackedHeader;
use super::{SLAB_META, SLAB_SIZE};
use crate::container::Container;
use crate::error::Result;
use crate::index::tree::{NodeReader, Tree};
use crate::ContainerKind;

/// Base offset of the slot containing `cell`, for a slab of `class`.
#[inline]
pub fn slot_base(cell: u64, class: u8) -> Option<u64> {
    let sz = class_size(class)? as u64;
    let slab = slab_of(cell);
    let body = slab as u64 * SLAB_SIZE + SLAB_META;
    if cell < body {
        return None;
    }
    let slot = (cell - body) / sz;
    Some(body + slot * sz)
}

/// Slot index within its slab.
#[inline]
pub fn slot_index(cell: u64, class: u8) -> Option<u32> {
    let sz = class_size(class)? as u64;
    let slab = slab_of(cell);
    let body = slab as u64 * SLAB_SIZE + SLAB_META;
    if cell < body {
        return None;
    }
    Some(((cell - body) / sz) as u32)
}

/// Liveness derived purely from the index.
#[derive(Debug, Default)]
pub struct Rebuilt {
    /// `slab -> slot -> one key that references it`.
    ///
    /// Keeping a key rather than a bare slot set costs a little memory and makes
    /// a dangling-reference report actionable: an operator needs to know *which*
    /// chunk points at freed space, not merely that one does.
    pub used: BTreeMap<u32, BTreeMap<u32, ChunkKey>>,
    /// `packed page slot base -> live payload bytes`.
    pub packed_live: BTreeMap<u64, u32>,
    /// `slab -> slots` occupied by the index's **own** nodes.
    ///
    /// Kept apart from [`Rebuilt::used`] because these slots have no
    /// `ChunkKey` to name them, and conflating them would make a dangling
    /// index node report as a dangling chunk.
    pub index_slots: BTreeMap<u32, BTreeSet<u32>>,
    pub chunks: u64,
    pub inline_chunks: u64,
    pub extent_chunks: u64,
    pub index_nodes: u64,
    /// Chunks at prefix `2^48 - 1`, the only ones that *can* violate invariant
    /// I8 ( ordinals `<= 2^64 - 2`, so low value `0xFFFF` is illegal there and
    /// legal everywhere else ).
    ///
    /// Collected rather than checked here because deciding it needs the
    /// **payload**, and `rebuild` deliberately does not decode one — it is a
    /// structural walk over references. The caller has the store and finishes
    /// the job; see [`Rebuilt::i8_violations`]. There is at most one such chunk
    /// per key, and in every realistic corpus there are none, so this costs one
    /// integer compare per chunk and an empty `Vec`.
    pub top_prefix_chunks: Vec<(ChunkKey, ChunkRef)>,
    /// Standalone extents the index reaches, with everything needed to find and
    /// recompute their stored checksum.
    ///
    /// Collected rather than checked in [`rebuild`] for the same reason as
    /// [`Rebuilt::top_prefix_chunks`]: the answer needs **bytes**, and `rebuild`
    /// is a structural walk over references that never reads a payload. The
    /// caller has the store and finishes the job; see
    /// [`Rebuilt::checksum_violations`].
    ///
    /// Packed pages need no equivalent list — their bases are already the keys
    /// of [`Rebuilt::packed_live`], and one CRC covers the whole page rather
    /// than one per chunk in it.
    pub extents: Vec<ExtentSite>,
}

/// One standalone extent, located well enough to check its trailer.
///
/// `payload_len` is carried rather than re-derived because a run's length is a
/// dependent read of its own `nruns` prefix, which needs the store — the same
/// dependency that forced [`ChunkRef::payload_len_with`] to exist. Re-deriving
/// it at check time would be a third copy of that rule, and the last time this
/// rule had three copies two of them were wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExtentSite {
    /// The chunk that references it, so a report names something actionable.
    pub key: ChunkKey,
    /// Where the payload starts. Also where the read path points.
    pub cell: u64,
    /// Size class, which is what puts the trailer at a fixed distance from the
    /// slot end — findable without knowing the payload length.
    pub class: u8,
    /// Bytes the trailer's CRC32C covers. The slot's remaining bytes are *not*
    /// zeroed on write, so the extent is exact, not the whole slot.
    pub payload_len: u32,
}

/// The one prefix at which low value `0xFFFF` would name `u64::MAX`.
pub const TOP_PREFIX: crate::Prefix48 = (1u64 << 48) - 1;

impl Rebuilt {
    pub fn slot_count(&self) -> usize {
        self.used.values().map(|s| s.len()).sum()
    }

    /// Finish the invariant-I8 check that [`rebuild`] could only start.
    ///
    /// `read` resolves a chunk to its container; the caller supplies it because
    /// `fsck` has references, not payloads. Returns one error string per chunk
    /// at prefix `2^48 - 1` that contains low value `0xFFFF` — which would be
    /// the ordinal `u64::MAX`, and [`crate::ORDINAL_MAX`] says there is no such
    /// ordinal.
    ///
    /// This is the *only* structural check of I8 that is possible, and it is
    /// possible only here. A container is **prefix-agnostic**, so
    /// `container::codec::validate` cannot know whether `0xFFFF` is legal in the
    /// payload it is looking at — the same 8192 bytes are valid at every other
    /// prefix. Only a walk that knows the `ChunkKey` can decide, which is why
    /// the check lives in `fsck` rather than in the codec.
    ///
    /// It guards a future write path that bypasses both the `Db` API and
    /// `WriteBatch::store_set`, not anything reachable today. A `read` that
    /// fails is reported rather than swallowed: a top-prefix chunk that cannot
    /// be decoded is exactly the case where "we could not check" must not read
    /// as "it is fine".
    pub fn i8_violations(
        &self,
        mut read: impl FnMut(ChunkKey, ChunkRef) -> Result<Option<Container>>,
    ) -> Vec<String> {
        let mut out = Vec::new();
        for &(key, cref) in &self.top_prefix_chunks {
            let low = (crate::CHUNK_CARD - 1) as u16; // 0xFFFF
            let holds = match read(key, cref) {
                Ok(Some(c)) => c.contains(low),
                Ok(None) => continue,
                Err(e) => {
                    out.push(format!(
                        "{key:?}: cannot decode the top-prefix chunk to check I8: {e}"
                    ));
                    continue;
                }
            };
            if holds {
                out.push(format!(
                    "{key:?}: holds low 0xFFFF at the top prefix, which is the \
                     ordinal u64::MAX; invariant I8 says there is no such ordinal"
                ));
            }
        }
        out
    }

    /// Recompute every stored CRC32C the index reaches: packed pages and the
    /// [`ExtTrailer`] of every standalone extent.
    ///
    /// `read( offset, len )` returns raw file bytes; the caller supplies it for
    /// the same reason it supplies [`Rebuilt::i8_violations`]'s reader — `fsck`
    /// has references, not bytes.
    ///
    /// The third family, B+tree nodes, is **not** here: [`rebuild`] already has
    /// every node in hand through its `NodeReader` and checks them inline, so
    /// routing them through a second reader would mean reading the whole index
    /// twice.
    ///
    /// One CRC per packed *page*, not per chunk in it. The page checksum covers
    /// the header and every payload it holds, so a page shared by 300 sparse
    /// chunks is verified once — which is what keeps the scan proportional to
    /// bytes rather than to chunks.
    ///
    /// A read that fails is reported rather than skipped, on the same reasoning
    /// as `i8_violations`: "we could not check" must not read as "it is fine".
    ///
    /// **This has to be called by the integrity scan**, next to
    /// [`Rebuilt::i8_violations`] and folded into the same `errors` list. Both
    /// exist because `rebuild` has references and the caller has bytes, and a
    /// check nothing runs on the path it guards is a comment — which is exactly
    /// what these three checksums were before this existed.
    pub fn checksum_violations(
        &self,
        mut read: impl FnMut(u64, usize) -> Result<Vec<u8>>,
    ) -> Vec<String> {
        let mut out = Vec::new();

        let Some(page_size) = class_size(PACKED_CLASS).map(|s| s as usize) else {
            out.push("the packed size class is missing from the ladder".to_string());
            return out;
        };
        for &base in self.packed_live.keys() {
            match read(base, page_size) {
                // `verify` is parse-then-CRC, and both halves belong here: a
                // page whose magic or version no longer reads is corrupt in a
                // way the checksum would also catch, and reporting whichever
                // fires first loses nothing.
                Ok(page) => {
                    if let Err(e) = PackedHeader::verify(&page) {
                        out.push(format!("packed page at {base}: {e}"));
                    }
                }
                Err(e) => out.push(format!(
                    "packed page at {base}: cannot be read to check its checksum: {e}"
                )),
            }
        }

        for site in &self.extents {
            let key = site.key;
            let Some(slot) = class_size(site.class) else {
                out.push(format!(
                    "{key:?}: extent at {} has unknown size class {}",
                    site.cell, site.class
                ));
                continue;
            };
            // The trailer sits at a fixed distance from the slot *end*, which is
            // what makes it findable without the payload length. The payload
            // itself is exactly `payload_len` bytes at the cell: the bytes
            // between are never written, so widening the extent to the whole
            // slot would checksum uninitialised space.
            let payload = match read(site.cell, site.payload_len as usize) {
                Ok(b) => b,
                Err(e) => {
                    out.push(format!(
                        "{key:?}: extent payload at {} cannot be read to check its checksum: {e}",
                        site.cell
                    ));
                    continue;
                }
            };
            let tail = site.cell + slot as u64 - EXT_TRAILER_BYTES as u64;
            let raw = match read(tail, EXT_TRAILER_BYTES) {
                Ok(b) => b,
                Err(e) => {
                    out.push(format!(
                        "{key:?}: extent trailer at {tail} cannot be read: {e}"
                    ));
                    continue;
                }
            };
            let Ok(bytes) = <[u8; EXT_TRAILER_BYTES]>::try_from(raw.as_slice()) else {
                out.push(format!("{key:?}: short extent trailer at {tail}"));
                continue;
            };
            let t = ExtTrailer::from_le_bytes(bytes);
            let got = crc32c(&payload);
            if got != t.crc32c {
                out.push(format!(
                    "{key:?}: extent payload at {} fails its stored checksum \
                     ( stored {:#010x}, recomputed {got:#010x} )",
                    site.cell, t.crc32c
                ));
            }
        }
        out
    }
}

/// What a check found.
///
/// **[`FsckReport::is_clean`] is the definition of consistent; a field-by-field
/// assertion is not.** This doc used to say "empty `leaked` and `dangling`
/// means consistent", which named two of the **five** fields `is_clean` checks
/// and so understated the predicate — a consumer whose churn suite asserted on
/// four fields individually found it had never looked at `leaked`, in the one
/// file whose whole purpose was driving free-then-reallocate ( reported
/// 2026-09-18 ). Assert `is_clean()` and read the fields to attribute a
/// failure, not to define one.
///
/// The fields answer three different questions, which is why picking a subset
/// is easy to get wrong:
///
/// - **waste**, recoverable: `leaked`;
/// - **corruption**, never acceptable: `dangling`, `dangling_nodes`,
///   `packed_live_mismatch`, and `errors` ( a region that could not be checked
///   at all );
/// - **retention**, not a defect: `pending` — space a live reclamation
///   condition is holding, which comes back on its own.
#[derive(Debug, Default)]
pub struct FsckReport {
    pub chunks: u64,
    pub inline_chunks: u64,
    pub extent_chunks: u64,
    /// Nodes in the committed B+tree. Their slots are live, not leaked.
    pub index_nodes: u64,
    /// Slots held by an extent that is queued for reclamation. Retention, not
    /// waste: the space comes back once the three conditions pass.
    pub pending: usize,
    /// Slots the allocator marks used that nothing references — neither a
    /// chunk, nor an index node, nor a pending reclamation. Recoverable by
    /// adopting the rebuilt bitmaps.
    pub leaked: Vec<(u32, u32)>,
    /// Cells holding an index node that the allocator believes is free.
    /// Corruption, and worse than a dangling chunk: that space can be handed
    /// out under the tree itself.
    pub dangling_nodes: Vec<u64>,
    /// Slots a chunk references that the allocator marks free. A live chunk is
    /// pointing at reusable space — corruption, not waste.
    pub dangling: Vec<(ChunkKey, u64)>,
    /// Packed pages whose in-RAM live-byte count disagrees with the index.
    pub packed_live_mismatch: Vec<(u64, u32, u32)>,
    pub errors: Vec<String>,
}

impl FsckReport {
    #[inline]
    pub fn is_clean(&self) -> bool {
        self.leaked.is_empty()
            && self.dangling.is_empty()
            && self.dangling_nodes.is_empty()
            && self.packed_live_mismatch.is_empty()
            && self.errors.is_empty()
    }
}

/// Walk the index and derive slot liveness.
///
/// `class_of_slab` resolves a slab's size class ( normally from the slab table ).
/// `nruns_at` peeks a run payload's leading `nruns` prefix, which is the only
/// case where a payload length is not derivable from the reference alone.
pub fn rebuild(
    tree: &Tree,
    nodes: &impl NodeReader,
    class_of_slab: impl Fn(u32) -> Option<u8>,
    nruns_at: impl Fn(u64) -> Result<u32>,
) -> Result<(Rebuilt, Vec<String>)> {
    let mut out = Rebuilt::default();
    let mut errors = Vec::new();

    // --- the index's own nodes, and their stored checksums
    //
    // A B+tree node occupies an allocator slot exactly like a chunk extent
    // does, and the entry walk below does not mark it: that walk follows chunk
    // *references*. Leaving them out meant every live index node was reported
    // as a leaked slot — measured at `leaked == index_nodes_written`, exactly,
    // on a database whose chunks were all inline and owned no extents at all.
    //
    // That is not cosmetic. `slabmeta` states that a torn slab-metadata write
    // "costs a rebuild rather than data" because every byte is derivable from
    // the index. A rebuild that adopted these bitmaps would have marked every
    // live index node free, and the allocator would then hand those slots out
    // and write chunk payloads over the tree.
    //
    // This runs **before** the entry walk, and the order is load-bearing. A
    // node whose bytes are corrupt still parses often enough to yield entries,
    // and those entries are then garbage keys pointing at garbage cells — a
    // cascade of "lands in unallocated slab" errors whose actual cause is one
    // flipped bit in one node. Checking the checksums first means the real
    // finding is reported first, and reported at all: `tree.iter` propagates a
    // parse failure with `?`, which would abandon the walk before it ever
    // reached a checksum.
    for id in tree.node_ids(nodes)? {
        out.index_nodes += 1;
        match nodes.node(id) {
            Ok(page) => {
                if let Err(e) = crate::index::node::verify_checksum(page.as_slice()) {
                    errors.push(format!("index node {id}: {e}"));
                }
            }
            // Unreadable is not clean. `node_ids` reached this id by descending
            // from the root, so the page is supposed to exist.
            Err(e) => errors.push(format!("index node {id}: cannot be read: {e}")),
        }
        let cell = crate::db::store::page_id_to_cell(id);
        let slab = slab_of(cell);
        let Some(class) = class_of_slab(slab) else {
            errors.push(format!(
                "index node {id} at cell {cell} lands in unallocated slab {slab}"
            ));
            continue;
        };
        let Some(slot) = slot_index(cell, class) else {
            errors.push(format!(
                "index node {id}: cell {cell} precedes the slab body"
            ));
            continue;
        };
        out.index_slots.entry(slab).or_default().insert(slot);
    }

    for item in tree.iter(nodes) {
        let (key, cref) = item?;
        out.chunks += 1;

        if let Err(e) = cref.validate() {
            errors.push(format!("{key:?}: invalid ChunkRef: {e}"));
            continue;
        }
        // One compare per chunk. The payload decision needs a store, so it is
        // deferred to `Rebuilt::i8_violations`; see that method for why this is
        // the only place the question is even answerable.
        if key.prefix() == TOP_PREFIX {
            out.top_prefix_chunks.push((key, cref));
        }

        let Some(cell) = cref.cell() else {
            out.inline_chunks += 1;
            continue;
        };
        out.extent_chunks += 1;

        let slab = slab_of(cell);
        let Some(class) = class_of_slab(slab) else {
            errors.push(format!(
                "{key:?}: cell {cell} lands in unallocated slab {slab}"
            ));
            continue;
        };
        let Some(slot) = slot_index(cell, class) else {
            errors.push(format!("{key:?}: cell {cell} precedes the slab body"));
            continue;
        };
        let Some(base) = slot_base(cell, class) else {
            errors.push(format!("{key:?}: cannot resolve slot base for {cell}"));
            continue;
        };

        out.used.entry(slab).or_default().entry(slot).or_insert(key);

        if class == PACKED_CLASS {
            // A packed chunk's cell points into the page, not at its start, and
            // a run's length is a dependent read of its own `nruns` prefix.
            // Shared with the two supersede paths through `payload_len_with`:
            // this was a third copy of that rule, and the copies disagreed —
            // this one was right and both of the others returned zero for every
            // run. Keeping one implementation is what stops that recurring.
            let len = match cref.payload_len_with(&nruns_at) {
                Ok(l) => l,
                Err(e) => {
                    errors.push(format!("{key:?}: cannot size packed payload: {e}"));
                    continue;
                }
            };
            if cref.kind() == ContainerKind::Bitmap {
                errors.push(format!("{key:?}: a bitmap must never be packed"));
                continue;
            }
            *out.packed_live.entry(base).or_default() += len as u32;
        } else {
            // Standalone: the trailer's CRC32C covers exactly the payload, so
            // the length has to be resolved here, while `nruns_at` is in scope.
            match cref.payload_len_with(&nruns_at) {
                Ok(len) => out.extents.push(ExtentSite {
                    key,
                    cell,
                    class,
                    payload_len: len as u32,
                }),
                Err(e) => errors.push(format!("{key:?}: cannot size extent payload: {e}")),
            }
        }
    }

    Ok((out, errors))
}

/// Compare a rebuild against an allocator's own bookkeeping.
pub fn verify(rebuilt: &Rebuilt, alloc: &Allocator, errors: Vec<String>) -> FsckReport {
    let mut rep = FsckReport {
        chunks: rebuilt.chunks,
        inline_chunks: rebuilt.inline_chunks,
        extent_chunks: rebuilt.extent_chunks,
        index_nodes: rebuilt.index_nodes,
        errors,
        ..Default::default()
    };

    // Space that is used-but-unreferenced *on purpose*: a superseded extent
    // keeps its bit until all three reclamation conditions pass. Retention is
    // not a leak, and counting it as one would make `is_clean` false for any
    // database that has ever superseded anything.
    let mut pending: BTreeMap<u32, BTreeSet<u32>> = BTreeMap::new();
    for (cell, class) in alloc.deferred_cells() {
        if let Some(slot) = slot_index(cell, class) {
            pending.entry(slab_of(cell)).or_default().insert(slot);
        }
    }

    let empty = BTreeMap::new();
    let empty_slots = BTreeSet::new();
    for slab_id in 0..alloc.slab_count() as u32 {
        let Some(slab) = alloc.slab(slab_id) else {
            continue;
        };
        let referenced = rebuilt.used.get(&slab_id).unwrap_or(&empty);
        let nodes = rebuilt.index_slots.get(&slab_id).unwrap_or(&empty_slots);
        let queued = pending.get(&slab_id).unwrap_or(&empty_slots);
        for slot in 0..slab.capacity() {
            if !slab.is_set(slot) || referenced.contains_key(&slot) || nodes.contains(&slot) {
                continue;
            }
            if queued.contains(&slot) {
                rep.pending += 1;
            } else {
                rep.leaked.push((slab_id, slot));
            }
        }
    }

    // An index node pointing at a slot the allocator believes is free is the
    // same corruption as a dangling chunk, and strictly worse in consequence:
    // that space can be handed out under the tree itself.
    for (&slab_id, slots) in &rebuilt.index_slots {
        match alloc.slab(slab_id) {
            Some(slab) => {
                for &slot in slots {
                    if !slab.is_set(slot) {
                        rep.dangling_nodes
                            .push(slab_id as u64 * SLAB_SIZE + SLAB_META + slot as u64);
                    }
                }
            }
            None => rep.errors.push(format!(
                "the index has a node in slab {slab_id}, which does not exist"
            )),
        }
    }

    // A reference into a slot the allocator believes is free is corruption, not
    // waste: that space can be handed out again beneath a live chunk.
    for (&slab_id, slots) in &rebuilt.used {
        match alloc.slab(slab_id) {
            Some(slab) => {
                for (&slot, &key) in slots {
                    if !slab.is_set(slot) {
                        rep.dangling
                            .push((key, slab_id as u64 * SLAB_SIZE + slot as u64));
                    }
                }
            }
            None => rep.errors.push(format!(
                "index references slab {slab_id}, which does not exist"
            )),
        }
    }

    for (&page, &live) in &rebuilt.packed_live {
        if let Some(claimed) = alloc.packed_live_bytes(page) {
            if claimed != live {
                rep.packed_live_mismatch.push((page, claimed, live));
            }
        }
    }

    // And the other direction, which the loop above structurally cannot see: a
    // page the index no longer reaches **at all**, still counted as holding
    // live bytes. `rebuilt.packed_live` only contains pages with at least one
    // live chunk, so a page whose chunks have every one died never appears
    // there and its stale count goes unchecked.
    //
    // That is not a corner case — it is what a leaked page *is*. A page only
    // reaches the reclamation queue when its live count hits zero, so a page
    // stuck above zero with nothing alive in it is permanently lost, and it was
    // invisible to the check written to catch exactly that.
    for (page, claimed) in alloc.packed_pages() {
        if claimed > 0 && !rebuilt.packed_live.contains_key(&page) {
            rep.packed_live_mismatch.push((page, claimed, 0));
        }
    }
    rep
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container::codec;
    use crate::index::tree::{NodeReader, NodeWriter, PageId};
    use crate::store::alloc::SlabState;
    use crate::store::extent::class_for;
    use crate::store::extent::{ckey_tag, ChunkRef};
    use crate::store::packed::{PackedPageBuilder, HEADER as PACKED_HEADER};
    use crate::store::{INDEX_NODE, PAGE};
    use arrow_buffer::Buffer;

    /// Index pages held by id, readable after the tree is built.
    #[derive(Default)]
    struct Nodes(BTreeMap<PageId, Buffer>);

    impl NodeReader for Nodes {
        fn node(&self, id: PageId) -> Result<crate::index::tree::Page> {
            self.0
                .get(&id)
                .cloned()
                .map(crate::index::tree::Page::from_buffer)
                .ok_or(crate::error::CodecError::Invariant("no such index page"))
        }
    }

    /// A node writer that **allocates a real slot per node**, as `ShardStore`
    /// does.
    ///
    /// `VecNodes` hands out dense ids from zero, which map to cell 0 — inside
    /// the reserved slab. A fixture built on it describes a tree whose nodes
    /// live nowhere, so it could not exercise the rebuild's node walk at all:
    /// the walk would only ever report "that slab does not exist".
    struct Allocating<'a> {
        alloc: &'a mut Allocator,
        out: &'a mut Nodes,
    }

    impl NodeWriter for Allocating<'_> {
        fn append_node(&mut self, bytes: &[u8]) -> Result<PageId> {
            let class = class_for(bytes.len()).ok_or(crate::error::CodecError::Invariant(
                "node exceeds the top class",
            ))?;
            let cell = self.alloc.alloc(class)?;
            let id = (cell / crate::db::store::PAGE_ID_SHIFT) as PageId;
            self.out.0.insert(id, Buffer::from_vec(bytes.to_vec()));
            Ok(id)
        }
    }

    /// The slab -> class map, exactly as `Db::verify` builds it.
    ///
    /// Tests used to pass a constant resolver. That is fine while the walk only
    /// follows chunk references, but the index's own nodes live in a slab of
    /// their own class, so a constant answer mislabels them.
    fn classes_of(alloc: &Allocator) -> BTreeMap<u32, u8> {
        (0..alloc.slab_count() as u32)
            .filter_map(|id| match alloc.slab(id).map(|s| s.state) {
                Some(SlabState::InUse { class, .. }) => Some((id, class)),
                _ => None,
            })
            .collect()
    }

    /// Build a tree whose nodes occupy real allocator slots.
    fn build_tree(alloc: &mut Allocator, entries: &[(ChunkKey, ChunkRef)]) -> (Tree, Nodes) {
        let mut nodes = Nodes::default();
        let tree = {
            let mut w = Allocating {
                alloc,
                out: &mut nodes,
            };
            Tree::build(&mut w, INDEX_NODE, entries).unwrap().unwrap()
        };
        (tree, nodes)
    }

    /// Build a tiny store: one class-1 slab of standalone extents plus one
    /// packed page, with an index that references them.
    struct Fixture {
        alloc: Allocator,
        nodes: Nodes,
        tree: Tree,
        packed_page: u64,
    }

    fn fixture(n_standalone: u64, n_packed: u64) -> Fixture {
        let mut alloc = Allocator::new();
        alloc.begin_generation();
        let mut entries: Vec<(ChunkKey, ChunkRef)> = Vec::new();

        // Standalone extents: 600-byte payloads land in a mid class.
        let class = class_for(600).unwrap();
        for i in 0..n_standalone {
            let cell = alloc.alloc(class).unwrap();
            entries.push((
                ChunkKey::new(1, i),
                ChunkRef::extent(cell, ContainerKind::Array, 300).unwrap(),
            ));
        }

        // One packed page holding several small arrays, registered with the
        // live-byte total the index implies so the fixture starts consistent.
        //
        // Allocated only when it will actually be referenced: an unreferenced
        // packed page is a genuine leak, and fsck rightly reports it — an
        // earlier version of this fixture allocated one unconditionally and the
        // resulting "failure" was the checker working.
        let mut page = u64::MAX;
        if n_packed > 0 {
            let live = (n_packed * 6) as u32; // 3 values -> 6 bytes each
            page = alloc.alloc_packed(live).unwrap();
            for i in 0..n_packed {
                let off = page + crate::store::packed::HEADER as u64 + i * 6;
                entries.push((
                    ChunkKey::new(2, i),
                    ChunkRef::extent(off, ContainerKind::Array, 3).unwrap(),
                ));
            }
        }

        entries.sort_by_key(|e| e.0);
        let (tree, nodes) = build_tree(&mut alloc, &entries);
        Fixture {
            alloc,
            nodes,
            tree,
            packed_page: page,
        }
    }

    fn no_runs(_: u64) -> Result<u32> {
        Ok(0)
    }

    #[test]
    fn slot_base_and_index_are_consistent() {
        let class = 1u8;
        let sz = class_size(class).unwrap() as u64;
        let body = SLAB_META;
        for slot in [0u64, 1, 5, 100] {
            let base = body + slot * sz;
            // Any cell within the slot resolves to the same base and index.
            for delta in [0u64, 1, sz / 2, sz - 1] {
                let cell = base + delta;
                assert_eq!(slot_base(cell, class), Some(base), "cell {cell}");
                assert_eq!(slot_index(cell, class), Some(slot as u32));
            }
        }
        // A cell inside the slab metadata region has no slot.
        assert_eq!(slot_base(0, class), None);
        assert_eq!(slot_index(SLAB_META - 1, class), None);
    }

    /// `rebuild` must single out the top prefix and **only** the top prefix.
    ///
    /// The selectivity is the load-bearing half: low `0xFFFF` is a perfectly
    /// legal ordinal at every prefix but the last, so a check that flagged it
    /// generally would reject ordinary data.
    #[test]
    fn rebuild_collects_top_prefix_chunks_and_nothing_else() {
        let mut alloc = Allocator::new();
        alloc.begin_generation();
        let entries = vec![
            (ChunkKey::new(1, 0), ChunkRef::inline(&[0xFFFF]).unwrap()),
            (
                ChunkKey::new(1, TOP_PREFIX - 1),
                ChunkRef::inline(&[0xFFFF]).unwrap(),
            ),
            (
                ChunkKey::new(1, TOP_PREFIX),
                ChunkRef::inline(&[7]).unwrap(),
            ),
            (
                ChunkKey::new(2, TOP_PREFIX),
                ChunkRef::inline(&[0xFFFF]).unwrap(),
            ),
        ];
        let (tree, nodes) = build_tree(&mut alloc, &entries);
        let (rb, errs) = rebuild(&tree, &nodes, |_| Some(1), no_runs).unwrap();
        assert!(errs.is_empty(), "unexpected errors: {errs:?}");

        let got: Vec<ChunkKey> = rb.top_prefix_chunks.iter().map(|(k, _)| *k).collect();
        assert_eq!(
            got,
            vec![ChunkKey::new(1, TOP_PREFIX), ChunkKey::new(2, TOP_PREFIX)],
            "only the top prefix, and one per key"
        );
    }

    /// The payload half of the I8 check, including both false-positive traps.
    #[test]
    fn i8_violations_flags_only_the_illegal_ordinal() {
        let holds_max = Container::from_sorted(&[0xFFFF]);
        // `0xFFFE` at the top prefix is `ORDINAL_MAX` itself — the largest
        // *legal* ordinal, and the value an off-by-one here would reject.
        let holds_ordinal_max = Container::from_sorted(&[0xFFFE]);

        let rb = Rebuilt {
            top_prefix_chunks: vec![
                (
                    ChunkKey::new(1, TOP_PREFIX),
                    ChunkRef::inline(&[1]).unwrap(),
                ),
                (
                    ChunkKey::new(2, TOP_PREFIX),
                    ChunkRef::inline(&[1]).unwrap(),
                ),
            ],
            ..Default::default()
        };

        let clean = rb.i8_violations(|_, _| Ok(Some(holds_ordinal_max.clone())));
        assert!(
            clean.is_empty(),
            "ORDINAL_MAX must not be flagged, got {clean:?}"
        );

        let dirty = rb.i8_violations(|_, _| Ok(Some(holds_max.clone())));
        assert_eq!(dirty.len(), 2, "one per offending chunk");
        assert!(dirty[0].contains("I8"), "the error must name the invariant");

        // A chunk that cannot be decoded must be reported, not skipped: "we
        // could not check" is not "it is fine".
        let unreadable = rb.i8_violations(|_, _| {
            Err(crate::error::CodecError::Truncated {
                expected: 2,
                found: 0,
            })
        });
        assert_eq!(unreadable.len(), 2, "an undecodable chunk is an error");

        // And an empty collection must produce nothing, so the check is silent
        // on every database that has no top-prefix chunk at all.
        let none = Rebuilt::default().i8_violations(|_, _| unreachable!("nothing to read"));
        assert!(none.is_empty());
    }

    #[test]
    fn rebuild_marks_every_referenced_slot() {
        let f = fixture(20, 10);
        let classes = |slab: u32| {
            f.alloc.slab(slab).and_then(|s| match s.state {
                super::super::alloc::SlabState::InUse { class, .. } => Some(class),
                _ => None,
            })
        };
        let (rb, errs) = rebuild(&f.tree, &f.nodes, classes, no_runs).unwrap();
        assert!(errs.is_empty(), "unexpected errors: {errs:?}");
        assert_eq!(rb.chunks, 30);
        assert_eq!(rb.extent_chunks, 30);
        assert_eq!(rb.inline_chunks, 0);

        // 20 standalone slots, plus the one packed page slot shared by 10 chunks.
        assert_eq!(rb.slot_count(), 21, "packed chunks must share one slot");
        assert_eq!(
            rb.packed_live.get(&f.packed_page),
            Some(&60),
            "10 chunks x 6 bytes"
        );
    }

    #[test]
    fn inline_chunks_consume_no_slot() {
        let mut alloc = Allocator::new();
        alloc.begin_generation();
        let mut entries = Vec::new();
        for i in 0..50u64 {
            entries.push((ChunkKey::new(1, i), ChunkRef::inline(&[1, 2, 3]).unwrap()));
        }
        let (tree, nodes) = build_tree(&mut alloc, &entries);

        let (rb, errs) = rebuild(&tree, &nodes, |_| Some(1u8), no_runs).unwrap();
        assert!(errs.is_empty());
        assert_eq!(rb.chunks, 50);
        assert_eq!(rb.inline_chunks, 50);
        assert_eq!(rb.extent_chunks, 0);
        assert_eq!(rb.slot_count(), 0, "inline chunks occupy no extent at all");

        let rep = verify(&rb, &alloc, errs);
        assert!(rep.is_clean(), "{rep:?}");
    }

    #[test]
    fn a_consistent_store_reports_clean() {
        let f = fixture(30, 0);
        let classes = |slab: u32| {
            f.alloc.slab(slab).and_then(|s| match s.state {
                super::super::alloc::SlabState::InUse { class, .. } => Some(class),
                _ => None,
            })
        };
        let (rb, errs) = rebuild(&f.tree, &f.nodes, classes, no_runs).unwrap();
        let rep = verify(&rb, &f.alloc, errs);
        assert!(rep.is_clean(), "expected a clean store, got {rep:?}");
        assert_eq!(rep.chunks, 30);
    }

    #[test]
    fn a_leaked_slot_is_detected() {
        // Allocate an extra extent that no chunk references — exactly what a
        // crash between allocation and index update would leave behind.
        let mut f = fixture(10, 0);
        let class = class_for(600).unwrap();
        let orphan = f.alloc.alloc(class).unwrap();

        let classes = |slab: u32| {
            f.alloc.slab(slab).and_then(|s| match s.state {
                super::super::alloc::SlabState::InUse { class, .. } => Some(class),
                _ => None,
            })
        };
        let (rb, errs) = rebuild(&f.tree, &f.nodes, classes, no_runs).unwrap();
        let rep = verify(&rb, &f.alloc, errs);

        assert!(!rep.is_clean());
        assert_eq!(rep.leaked.len(), 1, "one orphaned slot");
        assert!(
            rep.dangling.is_empty(),
            "a leak is not a dangling reference"
        );
        let (slab, slot) = rep.leaked[0];
        assert_eq!(slot_index(orphan, class), Some(slot));
        assert_eq!(slab_of(orphan), slab);
    }

    #[test]
    fn a_dangling_reference_is_detected() {
        // The serious case: the index points at a slot the allocator thinks is
        // free, so that space could be handed out again under a live chunk.
        let mut f = fixture(10, 0);
        // Free a slot that the index still references. The class is resolved
        // from the slab now, so the caller no longer supplies one.
        let first = f
            .tree
            .iter(&f.nodes)
            .next()
            .unwrap()
            .unwrap()
            .1
            .cell()
            .unwrap();
        f.alloc.defer_free(first, 0);
        f.alloc.reclaim(u64::MAX, u64::MAX, |_, _| false);

        let classes = |slab: u32| {
            f.alloc.slab(slab).and_then(|s| match s.state {
                super::super::alloc::SlabState::InUse { class, .. } => Some(class),
                _ => None,
            })
        };
        let (rb, errs) = rebuild(&f.tree, &f.nodes, classes, no_runs).unwrap();
        let rep = verify(&rb, &f.alloc, errs);

        assert!(!rep.is_clean());
        assert_eq!(
            rep.dangling.len(),
            1,
            "one live chunk points at freed space"
        );
    }

    #[test]
    fn a_bitmap_in_a_packed_page_is_rejected() {
        // Bitmaps are 8 KiB and must never be packed; catching this is cheap and
        // the alternative is a silently wrong live-byte total.
        let mut alloc = Allocator::new();
        alloc.begin_generation();
        let page = alloc.alloc_packed(0).unwrap();
        let entries = vec![(
            ChunkKey::new(1, 0),
            ChunkRef::extent(page + 40, ContainerKind::Bitmap, 5000).unwrap(),
        )];
        let (tree, nodes) = build_tree(&mut alloc, &entries);

        let (_rb, errs) = rebuild(&tree, &nodes, |_| Some(PACKED_CLASS), no_runs).unwrap();
        assert_eq!(errs.len(), 1, "expected one error, got {errs:?}");
        assert!(errs[0].contains("bitmap"), "{}", errs[0]);
    }

    #[test]
    fn a_reference_into_an_unallocated_slab_is_reported() {
        let mut alloc = Allocator::new();
        alloc.begin_generation();
        let entries = vec![(
            ChunkKey::new(1, 0),
            ChunkRef::extent(SLAB_SIZE * 99 + SLAB_META, ContainerKind::Array, 4).unwrap(),
        )];
        let (tree, nodes) = build_tree(&mut alloc, &entries);

        // Real classes for the slabs that exist, so the only unresolvable slab
        // is the one the bogus reference points at.
        let cls = classes_of(&alloc);
        let (_rb, errs) = rebuild(&tree, &nodes, |s| cls.get(&s).copied(), no_runs).unwrap();
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].contains("unallocated slab"), "{}", errs[0]);
    }

    #[test]
    fn run_payload_length_comes_from_the_payload_prefix() {
        // The one case a length is not derivable from the reference alone.
        let mut alloc = Allocator::new();
        alloc.begin_generation();
        let page = alloc.alloc_packed(0).unwrap();
        let entries = vec![(
            ChunkKey::new(1, 0),
            ChunkRef::extent(page + 40, ContainerKind::Run, 100).unwrap(),
        )];
        let (tree, nodes) = build_tree(&mut alloc, &entries);

        let (rb, errs) = rebuild(&tree, &nodes, |_| Some(PACKED_CLASS), |_| Ok(7)).unwrap();
        assert!(errs.is_empty(), "{errs:?}");
        // 2 + 4*7 = 30
        assert_eq!(rb.packed_live.get(&page), Some(&30));
    }

    // -----------------------------------------------------------------------
    // Stored checksums
    // -----------------------------------------------------------------------
    //
    // `stored-page-crcs-are-not-verified`: three families of CRC32C were
    // written by the format and recomputed by nothing. Each test below
    // corrupts one family in **two** places — a byte of the region the checksum
    // covers, and a byte of the checksum field itself — because the two fail in
    // opposite directions and a check that caught only one would still be
    // broken.
    //
    // `stored_checksums_pass_on_an_intact_store` is what makes the rest
    // non-vacuous. A scan that reported everything as corrupt would satisfy
    // every other test here.

    /// A stand-in for the shard file: real bytes at real offsets.
    ///
    /// The fixtures above never wrote a payload — they only ever needed
    /// references — so nothing existed to checksum. Recomputing a stored CRC
    /// needs the bytes it was computed over, which means the fixture has to be
    /// a file image rather than a reference graph.
    struct Image(Vec<u8>);

    impl Image {
        fn covering(alloc: &Allocator) -> Self {
            Image(vec![
                0u8;
                ((alloc.slab_count() as u64 + 1) * SLAB_SIZE) as usize
            ])
        }

        fn put(&mut self, off: u64, bytes: &[u8]) {
            let at = off as usize;
            self.0[at..at + bytes.len()].copy_from_slice(bytes);
        }

        fn read(&self, off: u64, len: usize) -> Result<Vec<u8>> {
            let at = off as usize;
            self.0.get(at..at + len).map(<[u8]>::to_vec).ok_or(
                crate::error::CodecError::Truncated {
                    expected: len,
                    found: self.0.len().saturating_sub(at),
                },
            )
        }

        /// Flip every bit of one byte. Loud enough that a weak check has no
        /// excuse, and still a single-byte error, which is what CRC32C
        /// guarantees to detect rather than merely probably detect.
        fn flip(&mut self, off: u64) {
            self.0[off as usize] ^= 0xFF;
        }
    }

    struct ChecksumFixture {
        alloc: Allocator,
        nodes: Nodes,
        tree: Tree,
        image: Image,
        /// Base of the one packed page.
        packed_page: u64,
        /// Cell and size class of one standalone extent.
        extent: (u64, u8),
        /// Id of one leaf node, and the node size, for corrupting the index.
        leaf: PageId,
    }

    /// A store whose bytes are actually written: standalone extents with real
    /// trailers, one real packed page, and an index over both.
    fn checksum_fixture() -> ChecksumFixture {
        let mut alloc = Allocator::new();
        alloc.begin_generation();
        let mut entries: Vec<(ChunkKey, ChunkRef)> = Vec::new();
        let mut writes: Vec<(u64, Vec<u8>)> = Vec::new();
        let mut extent = None;

        // Standalone extents: a 300-value array is 600 bytes, well past
        // PACK_MAX's packing threshold only in the sense that we place it
        // standalone deliberately — what matters is that it owns a whole slot
        // and therefore a trailer.
        for i in 0..4u64 {
            let vals: Vec<u16> = (0..300u16).map(|v| v * 3 + i as u16).collect();
            let c = Container::from_sorted(&vals);
            let payload = codec::encode(&c);
            let class = class_for(payload.len()).unwrap();
            let cell = alloc.alloc(class).unwrap();
            let key = ChunkKey::new(1, i);
            let slot = class_size(class).unwrap() as u64;
            let trailer = ExtTrailer {
                ckey_tag: ckey_tag(key),
                crc32c: crc32c(&payload),
            };
            writes.push((cell, payload));
            writes.push((
                cell + slot - EXT_TRAILER_BYTES as u64,
                trailer.to_le_bytes().to_vec(),
            ));
            entries.push((
                key,
                ChunkRef::extent(cell, ContainerKind::Array, c.len()).unwrap(),
            ));
            extent.get_or_insert((cell, class));
        }

        // One packed page holding several small arrays, sealed for real so its
        // header CRC covers real payloads.
        let mut builder = PackedPageBuilder::new(PAGE);
        let mut placed: Vec<(ChunkKey, usize, u32)> = Vec::new();
        let mut live = 0u32;
        for i in 0..8u16 {
            let vals: Vec<u16> = (0..5u16).map(|v| i * 37 + v).collect();
            let c = Container::from_sorted(&vals);
            let payload = codec::encode(&c);
            let key = ChunkKey::new(2, i as u64);
            let off = builder.push(key, &payload).unwrap().unwrap();
            live += payload.len() as u32;
            placed.push((key, off, c.len()));
        }
        let page_bytes = builder.seal().unwrap();
        let page = alloc.alloc_packed(live).unwrap();
        writes.push((page, page_bytes));
        for (key, off, card) in placed {
            entries.push((
                key,
                ChunkRef::extent(page + off as u64, ContainerKind::Array, card).unwrap(),
            ));
        }

        entries.sort_by_key(|e| e.0);
        let (tree, nodes) = build_tree(&mut alloc, &entries);
        // A *leaf*, chosen by its type byte rather than by id order: the id
        // ordering is an allocator detail, and corrupting an internal node
        // would exercise a different parse path than the one intended here.
        let leaf = *nodes
            .0
            .iter()
            .find(|(_, b)| b.as_slice().first() == Some(&crate::index::node::NODE_LEAF))
            .expect("the tree has a leaf")
            .0;

        let mut image = Image::covering(&alloc);
        for (off, bytes) in writes {
            image.put(off, &bytes);
        }
        ChecksumFixture {
            alloc,
            nodes,
            tree,
            image,
            packed_page: page,
            extent: extent.unwrap(),
            leaf,
        }
    }

    impl ChecksumFixture {
        fn scan(&self) -> (Vec<String>, Vec<String>) {
            let cls = classes_of(&self.alloc);
            let (rb, errs) = rebuild(
                &self.tree,
                &self.nodes,
                |s| cls.get(&s).copied(),
                |_| unreachable!("no run containers in this fixture"),
            )
            .expect("the walk itself must not fail");
            let sums = rb.checksum_violations(|off, len| self.image.read(off, len));
            (errs, sums)
        }

        /// Corrupt one byte of an index node, in the page store the reader sees.
        fn flip_in_leaf(&mut self, at: usize) {
            let page = self.nodes.0.get(&self.leaf).unwrap();
            let mut bytes = page.as_slice().to_vec();
            bytes[at] ^= 0xFF;
            self.nodes.0.insert(self.leaf, Buffer::from_vec(bytes));
        }
    }

    /// Non-vacuity. Everything below is worthless without it.
    #[test]
    fn stored_checksums_pass_on_an_intact_store() {
        let f = checksum_fixture();
        let cls = classes_of(&f.alloc);
        let (rb, errs) = rebuild(
            &f.tree,
            &f.nodes,
            |s| cls.get(&s).copied(),
            |_| unreachable!("no run containers in this fixture"),
        )
        .unwrap();
        assert!(errs.is_empty(), "intact store reported errors: {errs:?}");

        // The fixture must actually contain all three families, or a passing
        // scan proves nothing about any of them.
        assert!(rb.index_nodes > 0, "no index nodes to check");
        assert_eq!(rb.extents.len(), 4, "no standalone extents to check");
        assert_eq!(rb.packed_live.len(), 1, "no packed page to check");

        let sums = rb.checksum_violations(|off, len| f.image.read(off, len));
        assert!(sums.is_empty(), "intact store reported: {sums:?}");

        let rep = verify(&rb, &f.alloc, errs);
        assert!(rep.is_clean(), "{rep:?}");
    }

    /// B+tree node, payload side.
    ///
    /// The flipped byte is in the node's zero-filled tail. That is deliberate
    /// and it is the *strongest* available choice: nothing else in the format
    /// reads those bytes, so no shape check, version gate or key-order
    /// assertion can catch this. Only recomputing the checksum can.
    #[test]
    fn a_corrupt_index_node_payload_is_caught() {
        let mut f = checksum_fixture();
        f.flip_in_leaf(INDEX_NODE - 1);
        let (errs, sums) = f.scan();
        assert!(
            errs.iter()
                .any(|e| e.contains("index node") && e.contains("checksum mismatch")),
            "expected an index-node checksum error, got {errs:?}"
        );
        assert!(sums.is_empty(), "no data page was touched: {sums:?}");
    }

    /// B+tree node, checksum side. The stored value is what moves; the bytes it
    /// covers are untouched.
    #[test]
    fn a_corrupt_index_node_checksum_field_is_caught() {
        let mut f = checksum_fixture();
        f.flip_in_leaf(crate::index::node::OFF_CRC);
        let (errs, _) = f.scan();
        assert!(
            errs.iter()
                .any(|e| e.contains("index node") && e.contains("checksum mismatch")),
            "expected an index-node checksum error, got {errs:?}"
        );
    }

    /// Packed page, payload side.
    #[test]
    fn a_corrupt_packed_page_payload_is_caught() {
        let mut f = checksum_fixture();
        let base = f.packed_page;
        f.image.flip(base + PACKED_HEADER as u64 + 1);
        let (errs, sums) = f.scan();
        assert!(errs.is_empty(), "the index is intact: {errs:?}");
        assert_eq!(sums.len(), 1, "{sums:?}");
        assert!(
            sums[0].contains(&format!("packed page at {base}")) && sums[0].contains("checksum"),
            "{}",
            sums[0]
        );
    }

    /// Packed page, checksum side. Offset 4 is the header's own CRC field.
    #[test]
    fn a_corrupt_packed_page_checksum_field_is_caught() {
        let mut f = checksum_fixture();
        let base = f.packed_page;
        f.image.flip(base + 4);
        let (_, sums) = f.scan();
        assert_eq!(sums.len(), 1, "{sums:?}");
        assert!(sums[0].contains("checksum"), "{}", sums[0]);
    }

    /// Standalone extent, payload side.
    #[test]
    fn a_corrupt_standalone_payload_is_caught() {
        let mut f = checksum_fixture();
        let (cell, _) = f.extent;
        f.image.flip(cell + 7);
        let (errs, sums) = f.scan();
        assert!(errs.is_empty(), "the index is intact: {errs:?}");
        assert_eq!(sums.len(), 1, "{sums:?}");
        assert!(sums[0].contains("fails its stored checksum"), "{}", sums[0]);
    }

    /// Standalone extent, checksum side.
    ///
    /// The trailer's CRC32C is its second word, so it sits four bytes before
    /// the slot end. Flipping it must fail exactly as a corrupt payload does —
    /// the two are indistinguishable to a reader, and both mean the extent can
    /// no longer be trusted.
    #[test]
    fn a_corrupt_extent_trailer_checksum_is_caught() {
        let mut f = checksum_fixture();
        let (cell, class) = f.extent;
        let slot = class_size(class).unwrap() as u64;
        f.image.flip(cell + slot - 4);
        let (_, sums) = f.scan();
        assert_eq!(sums.len(), 1, "{sums:?}");
        assert!(sums[0].contains("fails its stored checksum"), "{}", sums[0]);
    }

    /// "We could not check" must not read as "it is fine".
    #[test]
    fn bytes_that_cannot_be_read_are_reported_not_skipped() {
        let f = checksum_fixture();
        let cls = classes_of(&f.alloc);
        let (rb, _) = rebuild(
            &f.tree,
            &f.nodes,
            |s| cls.get(&s).copied(),
            |_| unreachable!("no run containers in this fixture"),
        )
        .unwrap();
        let sums = rb.checksum_violations(|_, _| {
            Err(crate::error::CodecError::Truncated {
                expected: 8,
                found: 0,
            })
        });
        assert!(
            sums.iter().any(|m| m.contains("packed page")),
            "the unreadable packed page must be reported: {sums:?}"
        );
        assert!(
            sums.iter().any(|m| m.contains("cannot be read")),
            "{sums:?}"
        );
    }

    /// A run's payload length is a dependent read, and the trailer's CRC covers
    /// exactly that many bytes — so an extent whose length is mis-derived
    /// checksums the wrong extent and fails on perfectly good data.
    ///
    /// Pinned because `payload_len_with` exists precisely because that rule had
    /// three copies and two of them were wrong.
    #[test]
    fn a_standalone_run_is_sized_from_its_own_prefix() {
        let mut alloc = Allocator::new();
        alloc.begin_generation();
        // Contiguous, so `optimize` picks the run encoding — the one kind whose
        // payload length is not derivable from its `ChunkRef`.
        let vals: Vec<u16> = (0..5000u16).collect();
        let mut c = Container::from_sorted(&vals);
        c.optimize();
        assert_eq!(c.kind(), ContainerKind::Run, "the fixture needs a run");
        let payload = codec::encode(&c);
        let class = class_for(payload.len()).unwrap();
        let cell = alloc.alloc(class).unwrap();
        let key = ChunkKey::new(9, 1);
        let entries = vec![(
            key,
            ChunkRef::extent(cell, ContainerKind::Run, c.len()).unwrap(),
        )];
        let (tree, nodes) = build_tree(&mut alloc, &entries);

        let mut image = Image::covering(&alloc);
        let slot = class_size(class).unwrap() as u64;
        image.put(cell, &payload);
        image.put(
            cell + slot - EXT_TRAILER_BYTES as u64,
            &ExtTrailer {
                ckey_tag: ckey_tag(key),
                crc32c: crc32c(&payload),
            }
            .to_le_bytes(),
        );

        let cls = classes_of(&alloc);
        let nruns = |cell: u64| {
            let b = image.read(cell, 2)?;
            Ok(u16::from_le_bytes([b[0], b[1]]) as u32)
        };
        let (rb, errs) = rebuild(&tree, &nodes, |s| cls.get(&s).copied(), nruns).unwrap();
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(
            rb.extents[0].payload_len as usize,
            payload.len(),
            "the run's length must come from its own nruns prefix"
        );
        assert!(rb
            .checksum_violations(|off, len| image.read(off, len))
            .is_empty());
    }
}
