//! The checkpointer: the only thing that allocates file space, and the only
//! thing that makes data durable outside the WAL.
//!
//! # The barrier (I4)
//!
//! A checkpoint picks `W = oracle.visible()` and persists **only** state at or
//! below it. Nothing from an unresolved commit ever reaches the data file, which
//! is what makes recovery redo-only — "undo" becomes "don't redo" — and what
//! makes the on-disk image a globally consistent snapshot at `W` with no
//! coordination.
//!
//! It is also the abstraction that makes consensus a small change later: today
//! `W` is the local durable watermark; under Raft it becomes the quorum-committed
//! watermark and *nothing else here moves*.
//!
//! # Ordering is the durability argument
//!
//! The sequence below is not a suggestion. Each fsync separates things that must
//! not be reordered, and the superblock flip is the sole commit point:
//!
//! 1. `CheckpointBegin` to each shard's WAL.
//! 2. Serialize dirty chunks **in `ChunkKey` order**, applying deferred
//!    demotion and run-optimization, allocating extents ( I3 ) and writing them.
//! 3. Merge in any compaction relocations.
//! 4. Bottom-up copy-on-write rebuild of the index.
//! 5. `fdatasync` — every new extent and node is on disk.
//! 6. Write slab metadata; `fdatasync`.
//! 7. **Flip the superblock; `fdatasync`.** ← the checkpoint exists from here
//! 8. `CheckpointEnd`; advance WAL retention; release deferred extents.
//!
//! Steps 1-6 write only to space no published root can reach, so a crash
//! anywhere in them loses nothing that was ever visible. That is why no
//! full-page writes are needed anywhere in this store.
//!
//! # ChunkKey order is free performance
//!
//! Because the writer iterates in key order and the allocator bump-allocates per
//! class within one generation, consecutive chunks of one key land in
//! consecutive slots. The hot path — an ordered range scan over one key — gets
//! near-sequential access as a side effect. Preserve the ordering.

use crate::container::Container;
use crate::error::Result;
use crate::index::tree::{NodeReader, NodeWriter, Tree};
use crate::mvcc::Version;
use crate::store::alloc::Allocator;
use crate::store::extent::{class_for, ChunkKey, ChunkRef, INLINE_MAX, PACK_MAX};
use crate::store::packed::PackedPageBuilder;
use crate::store::superblock::SuperBlock;
use crate::store::PAGE;
use crate::ContainerKind;

/// Trigger thresholds. A write stall at `max_dirty_bytes` is mandatory — it is
/// the only bound on memtable growth, and without it the process OOMs under
/// sustained ingest.
#[derive(Clone, Copy, Debug)]
pub struct CheckpointPolicy {
    pub dirty_bytes: usize,
    pub max_dirty_bytes: usize,
    pub wal_bytes: u64,
    pub interval_secs: u64,
    /// The most log to retain for a lagging follower before abandoning it.
    ///
    /// A hard bound, and it is what makes honouring a follower's retention
    /// floor safe at all: without it a follower that keeps acking while falling
    /// further behind holds generations until the disk fills. Past this the
    /// checkpoint reclaims through its own replay position regardless and the
    /// follower must bootstrap again, which it is told in as many words.
    ///
    /// The same call the design makes for snapshot space with
    /// `on_space_amp: AbortOldestReader` — "a reporting query should not be able
    /// to halt ingestion" — applied to the log.
    pub max_wal_bytes: u64,
}

impl Default for CheckpointPolicy {
    fn default() -> Self {
        CheckpointPolicy {
            dirty_bytes: 256 << 20,
            max_dirty_bytes: 512 << 20,
            wal_bytes: 1 << 30,
            interval_secs: 60,
            // Four times the trigger: room for a follower to be a few
            // checkpoint intervals behind, and nowhere near a disk.
            max_wal_bytes: 4 << 30,
        }
    }
}

impl CheckpointPolicy {
    pub fn should_checkpoint(&self, dirty: usize, wal: u64, elapsed_secs: u64) -> bool {
        dirty >= self.dirty_bytes || wal >= self.wal_bytes || elapsed_secs >= self.interval_secs
    }

    /// Writers must stall above this, rather than accumulating without bound.
    pub fn should_stall(&self, dirty: usize) -> bool {
        dirty >= self.max_dirty_bytes
    }
}

/// A chunk to be written, with the version at which it became current.
pub struct DirtyChunk {
    pub key: ChunkKey,
    pub container: Container,
    pub version: Version,
    /// Where the previous copy lives, if any. Freed at `W`, not now.
    pub previous: Option<ChunkRef>,
}

/// Somewhere to write extent bytes.
pub trait ExtentWriter {
    fn write_extent(&mut self, cell: u64, bytes: &[u8]) -> Result<()>;

    /// Read `len` bytes of **already-published** extent space at `cell`.
    ///
    /// The only caller is the superseded-packed-run path, which needs the two
    /// `nruns` bytes at the head of a run payload to know how much of its
    /// page's live count to return. That is a dependent read the format
    /// requires — `ChunkRef` dropped `nelem` precisely because the Roaring
    /// spec already mandates the prefix — so there is no way to avoid it.
    ///
    /// Published space is immutable under **I2**. This reads what a previous
    /// checkpoint wrote and a live snapshot may still be holding; it must never
    /// become half of a read-modify-write.
    ///
    /// It costs a potential page fault on a page the checkpoint would otherwise
    /// not touch. The alternative — recording each packed chunk's length in RAM
    /// at pack time — would add a map entry per packed chunk, and packed chunks
    /// are the sparse regime, where payloads are ~13 bytes and the entry would
    /// cost more than the data. Two bytes off a page that was written recently
    /// enough to still be superseded is the cheaper side of that trade.
    fn read_extent(&self, cell: u64, len: usize) -> Result<Vec<u8>>;
}

/// The `nruns` prefix at the head of the run payload at `cell`.
///
/// Pairs with [`crate::store::extent::ChunkRef::payload_len_with`].
pub fn run_nruns_at(src: &impl ExtentWriter, cell: u64) -> Result<u32> {
    let b = src.read_extent(cell, 2)?;
    Ok(u16::from_le_bytes([b[0], b[1]]) as u32)
}

/// Where a checkpoint gets space from.
///
/// # Why this is on the sink rather than a separate parameter
///
/// It was a separate `&mut Allocator` parameter, and that was a **silent
/// data-loss bug**. The sink is the store, and the store owns the allocator, so
/// passing both meant `Db::checkpoint` had to `mem::take` the allocator out —
/// leaving a *default, empty* one behind. Extents were then allocated from the
/// real allocator while `ShardStore::append_node` allocated from the empty one,
/// which handed out cells starting from zero again. Every checkpoint wrote its
/// index nodes directly on top of the extents it had just written, and the loss
/// was invisible until a reopen forced a read from disk.
///
/// Routing allocation through the sink makes a second allocator unreachable
/// rather than merely discouraged. Do not reintroduce an `alloc` parameter
/// alongside `sink`.
pub trait AllocSource {
    fn allocator(&mut self) -> &mut Allocator;
}

/// Where superseded index pages go.
///
/// Separate from [`NodeWriter`] because the page-id-to-cell mapping is a store
/// detail the tree does not know. An in-memory sink implements it as a no-op.
pub trait NodeSpace {
    /// Queue an index node for reclamation, subject to the usual three
    /// conditions. Never call this for a page the new tree reused.
    fn free_node(&mut self, id: crate::index::tree::PageId, ckpt_seq: u64);
}

/// Everything a checkpoint writes to, and allocates from.
///
/// One trait rather than several parameters because a real store *is* all of
/// them, and two `&mut` parameters cannot name the same value. Blanket-
/// implemented, so an in-memory test can still combine separate sinks.
pub trait CheckpointSink: ExtentWriter + NodeWriter + NodeReader + AllocSource + NodeSpace {}
impl<T: ExtentWriter + NodeWriter + NodeReader + AllocSource + NodeSpace> CheckpointSink for T {}

/// Pairs an extent sink with a node sink, for callers where they differ.
pub struct SplitSink<'a, E, N> {
    pub extents: &'a mut E,
    pub nodes: &'a mut N,
    pub alloc: &'a mut Allocator,
}

impl<E, N> AllocSource for SplitSink<'_, E, N> {
    fn allocator(&mut self) -> &mut Allocator {
        self.alloc
    }
}

impl<E: ExtentWriter, N> ExtentWriter for SplitSink<'_, E, N> {
    fn write_extent(&mut self, cell: u64, bytes: &[u8]) -> Result<()> {
        self.extents.write_extent(cell, bytes)
    }

    fn read_extent(&self, cell: u64, len: usize) -> Result<Vec<u8>> {
        self.extents.read_extent(cell, len)
    }
}

impl<E, N> NodeSpace for SplitSink<'_, E, N> {
    /// In-memory nodes are dropped with the sink; there is no space to return.
    fn free_node(&mut self, _id: crate::index::tree::PageId, _seq: u64) {}
}

impl<E, N: NodeReader> NodeReader for SplitSink<'_, E, N> {
    fn node(&self, id: crate::index::tree::PageId) -> Result<crate::index::tree::Page> {
        self.nodes.node(id)
    }
}

impl<E, N: NodeWriter> NodeWriter for SplitSink<'_, E, N> {
    fn append_node(&mut self, bytes: &[u8]) -> Result<crate::index::tree::PageId> {
        self.nodes.append_node(bytes)
    }
}

/// What a checkpoint produced.
#[derive(Debug)]
pub struct CheckpointResult {
    pub watermark: Version,
    /// Chunks kept at their existing extent, neither read nor rewritten.
    pub carried: u64,
    /// Superseded index pages queued for reclamation.
    pub nodes_freed: u64,
    pub chunks_written: usize,
    pub inlined: usize,
    pub packed: usize,
    pub standalone: usize,
    pub skipped_above_watermark: usize,
    pub extent_bytes: u64,
    pub superblock: SuperBlock,
}

/// Run one checkpoint.
///
/// `dirty` need not be sorted; it is sorted here, because ChunkKey order is what
/// gives the locality described above and relying on the caller to preserve it
/// would be a silent performance cliff.
#[allow(clippy::too_many_arguments)]
pub fn run(
    watermark: Version,
    mut dirty: Vec<DirtyChunk>,
    carried: &[(ChunkKey, ChunkRef)],
    prev_tree: Option<Tree>,
    sink: &mut impl CheckpointSink,
    node_size: usize,
    prev: &SuperBlock,
    checkpoint_seq: u64,
    wal_replay_lsn: u64,
    commit_clock: crate::mvcc::Micros,
) -> Result<CheckpointResult> {
    // I4: a chunk whose only version is above the watermark is not part of this
    // snapshot and must not be persisted.
    let before = dirty.len();
    dirty.retain(|d| d.version <= watermark);
    let skipped = before - dirty.len();

    dirty.sort_by_key(|d| d.key);

    // Each checkpoint claims a fresh generation, so its classes occupy a
    // contiguous slab run.
    sink.allocator().begin_generation();

    let mut entries: Vec<(ChunkKey, ChunkRef)> = Vec::with_capacity(dirty.len() + carried.len());
    let mut res = CheckpointResult {
        watermark,
        carried: 0,
        nodes_freed: 0,
        chunks_written: 0,
        inlined: 0,
        packed: 0,
        standalone: 0,
        skipped_above_watermark: skipped,
        extent_bytes: 0,
        superblock: prev.clone(),
    };

    // One open packed page. Sealing happens when the next payload does not fit.
    let mut page: Option<(u64, PackedPageBuilder)> = None;

    for d in &dirty {
        let mut c = d.container.clone();
        // Deferred size-reducing conversions happen here and nowhere else: they
        // are optimizations, so putting them on the write path would risk
        // thrashing for no durability benefit.
        c.optimize();

        let cref = if let Some(vals) = inline_values(&c) {
            res.inlined += 1;
            ChunkRef::inline(&vals)?
        } else {
            let payload = crate::container::codec::encode(&c);
            res.extent_bytes += payload.len() as u64;
            if payload.len() <= PACK_MAX && c.kind() != ContainerKind::Bitmap {
                res.packed += 1;
                pack(d.key, &payload, &mut page, sink, &c)?
            } else {
                res.standalone += 1;
                let class = class_for(payload.len()).ok_or(crate::CodecError::Invariant(
                    "payload exceeds the top class",
                ))?;
                let cell = sink.allocator().alloc(class)?;
                sink.write_extent(cell, &payload)?;
                // The trailer is what makes a mis-pointed `ChunkRef` detectable.
                // Its eight bytes are already reserved by the ladder — every
                // slot is `round_up_64(payload + 8)` — and were simply never
                // written, so the detection the design describes did not exist.
                //
                // It sits at a fixed distance from the slot *end*, so it can be
                // found without knowing the payload length.
                let slot = crate::store::extent::class_size(class)
                    .ok_or(crate::CodecError::Invariant("unknown size class"))?
                    as u64;
                let trailer = crate::store::extent::ExtTrailer {
                    ckey_tag: crate::store::extent::ckey_tag(d.key),
                    crc32c: crate::store::checksum::crc32c(&payload),
                };
                sink.write_extent(
                    cell + slot - crate::store::extent::EXT_TRAILER_BYTES as u64,
                    &trailer.to_le_bytes(),
                )?;
                ChunkRef::extent(cell, c.kind(), c.len())?
            }
        };
        entries.push((d.key, cref));
        res.chunks_written += 1;

        // The previous copy becomes reclaimable at the watermark, not now: an
        // older snapshot may still reach it through the previous root.
        if let Some(old) = d.previous {
            if let Some(cell) = old.cell() {
                // A packed chunk owns no slot: its page is the reclamation unit,
                // returned only once every chunk in it is dead.
                if sink.allocator().is_packed_cell(cell) {
                    // A run's length is a dependent read of its own `nruns`
                    // prefix. Spelling this `payload_len(None).unwrap_or(0)`
                    // returned zero for every run, so the page's live count
                    // never reached zero and the page was never reclaimed.
                    let bytes = old.payload_len_with(|c| run_nruns_at(sink, c))? as u32;
                    sink.allocator()
                        .supersede_packed_chunk(cell, bytes, checkpoint_seq);
                    continue;
                }
                // The class comes from the slab, not from the payload length —
                // see `Allocator::owning_class`. A packed chunk's cell points
                // inside a shared page rather than at a slot of its own, so it
                // is refused here and freed with its page instead.
                sink.allocator().defer_free(cell, checkpoint_seq);
            }
        }
    }

    if let Some((cell, b)) = page.take() {
        if !b.is_empty() {
            let bytes = b.seal()?;
            sink.write_extent(cell, &bytes)?;
        }
    }

    // Bottom-up COW rebuild. The previous root is untouched, so a reader holding
    // it keeps seeing a consistent tree throughout.
    // Chunks this checkpoint did not touch keep the extent they already have:
    // no read, no re-encode, no allocation, and — crucially — no supersede.
    //
    // They used to be read back off disk, decoded, re-encoded and written to a
    // *new* extent, which made every checkpoint cost the size of the whole
    // database rather than the size of its delta, and superseded every chunk in
    // it. The index is still rebuilt whole; only the payloads are spared.
    res.carried = carried.len() as u64;
    entries.extend_from_slice(carried);
    entries.sort_unstable_by_key(|(k, _)| *k);
    debug_assert!(
        entries.windows(2).all(|w| w[0].0 < w[1].0),
        "a carried chunk collided with a written one: the touched set is wrong"
    );

    // The old tree's pages, before the new one is built. Anything not carried
    // over becomes unreachable from the new root the moment it is published.
    let old_nodes = match prev_tree {
        Some(t) => t.node_ids(&*sink)?,
        None => Vec::new(),
    };

    let mut reused: Vec<crate::index::tree::PageId> = Vec::new();
    let tree = Tree::build_reusing(sink, node_size, prev_tree, &entries, &mut reused)?;

    // `old - reused`, never `old`. `build_reusing` keeps unchanged leaves alive
    // by design, and freeing one would hand out a page the new root points at.
    let kept: std::collections::HashSet<_> = reused.into_iter().collect();
    for id in old_nodes {
        if !kept.contains(&id) {
            sink.free_node(id, checkpoint_seq);
            res.nodes_freed += 1;
        }
    }

    let mut sb = prev.clone();
    sb.seq = prev.seq + 1;
    sb.root = tree.map(|t| (t.root, t.height));
    sb.checkpoint_cv = watermark;
    sb.checkpoint_seq = checkpoint_seq;
    sb.wal_replay_lsn = wal_replay_lsn;
    // Monotone: a checkpoint must never lower the floor the next open's clock
    // resumes from, and `prev` may already carry a higher value than this
    // instance ever stamped ( a reopened database that has not committed yet ).
    sb.commit_clock = prev.commit_clock.max(commit_clock);
    sb.n_slabs = sink.allocator().slab_count() as u32;
    sb.live_bytes = res.extent_bytes;
    res.superblock = sb;
    Ok(res)
}

/// Values if this container is small enough to live entirely in the index entry.
fn inline_values(c: &Container) -> Option<Vec<u16>> {
    if c.len() as usize > INLINE_MAX || c.kind() == ContainerKind::Bitmap {
        return None;
    }
    Some(c.iter().collect())
}

/// Append a payload to the open packed page, sealing and opening a new one when
/// it no longer fits.
fn pack(
    key: ChunkKey,
    payload: &[u8],
    page: &mut Option<(u64, PackedPageBuilder)>,
    sink: &mut impl CheckpointSink,
    c: &Container,
) -> Result<ChunkRef> {
    loop {
        if page.is_none() {
            let cell = sink.allocator().alloc_packed(0)?;
            *page = Some((cell, PackedPageBuilder::new(PAGE)));
        }
        let (cell, b) = page.as_mut().expect("just opened");
        let cell = *cell;
        match b.push(key, payload)? {
            Some(off) => {
                // Account the bytes now, or the page's live count stays at the
                // zero `alloc_packed` opened it with and it is never reclaimable.
                sink.allocator().add_packed(cell, payload.len() as u32);
                return ChunkRef::extent(cell + off as u64, c.kind(), c.len());
            }
            None => {
                // Full: seal it and open another.
                let (cell, b) = page.take().expect("open");
                let bytes = b.seal()?;
                sink.write_extent(cell, &bytes)?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::tree::VecNodes;
    use crate::store::INDEX_NODE;
    use std::collections::BTreeMap;

    #[derive(Default)]
    struct MemExtents {
        writes: BTreeMap<u64, Vec<u8>>,
    }

    impl ExtentWriter for MemExtents {
        fn write_extent(&mut self, cell: u64, bytes: &[u8]) -> Result<()> {
            self.writes.insert(cell, bytes.to_vec());
            Ok(())
        }

        /// Writes are whole sealed pages keyed by page base, so a chunk's cell
        /// lands *inside* one of them — find the containing write and index in.
        /// A plain `writes.get(&cell)` would miss every packed chunk and make
        /// this sink silently disagree with the real store.
        fn read_extent(&self, cell: u64, len: usize) -> Result<Vec<u8>> {
            let (base, bytes) =
                self.writes
                    .range(..=cell)
                    .next_back()
                    .ok_or(crate::CodecError::Invariant(
                        "no extent written at or below cell",
                    ))?;
            let off = (cell - base) as usize;
            bytes
                .get(off..off + len)
                .map(<[u8]>::to_vec)
                .ok_or(crate::CodecError::Invariant("read past the written extent"))
        }
    }

    fn chunk(key: u64, prefix: u64, vals: &[u16], version: Version) -> DirtyChunk {
        DirtyChunk {
            key: ChunkKey::new(key, prefix),
            container: Container::from_sorted(vals),
            version,
            previous: None,
        }
    }

    fn run_one(
        dirty: Vec<DirtyChunk>,
        watermark: Version,
    ) -> (CheckpointResult, VecNodes, Allocator, MemExtents) {
        let mut alloc = Allocator::new();
        let mut ext = MemExtents::default();
        let mut nodes = VecNodes::new();
        let prev = SuperBlock::initial([1u8; 16], 0);
        let mut sink = SplitSink {
            extents: &mut ext,
            nodes: &mut nodes,
            alloc: &mut alloc,
        };
        let res = run(
            watermark,
            dirty,
            &[],
            None,
            &mut sink,
            INDEX_NODE,
            &prev,
            1,
            4096,
            0,
        )
        .unwrap();
        (res, nodes, alloc, ext)
    }

    #[test]
    fn chunks_above_the_watermark_are_not_persisted() {
        // I4: the on-disk image must be a consistent snapshot at W.
        let dirty = vec![
            chunk(1, 0, &[1, 2, 3, 4, 5], 5),
            chunk(1, 1, &[1, 2, 3, 4, 5], 10), // above W
            chunk(1, 2, &[1, 2, 3, 4, 5], 7),
        ];
        let (res, _n, _a, _e) = run_one(dirty, 7);
        assert_eq!(res.chunks_written, 2);
        assert_eq!(res.skipped_above_watermark, 1);
        assert_eq!(res.superblock.checkpoint_cv, 7);
    }

    #[test]
    fn tiny_containers_are_inlined_and_occupy_no_extent() {
        let dirty: Vec<DirtyChunk> = (0..20u64).map(|i| chunk(1, i, &[7, 9], 1)).collect();
        let (res, nodes, alloc, ext) = run_one(dirty, 1);
        assert_eq!(res.inlined, 20);
        assert_eq!(res.packed, 0);
        assert_eq!(res.standalone, 0);
        assert!(ext.writes.is_empty(), "inline chunks write no extent bytes");
        assert_eq!(alloc.slab_count(), 0, "and allocate nothing");

        // And they read back through the index.
        let t = Tree {
            root: res.superblock.root.unwrap().0,
            height: res.superblock.root.unwrap().1,
            node_size: INDEX_NODE,
        };
        let got = t.get(&nodes, ChunkKey::new(1, 5)).unwrap().unwrap();
        assert_eq!(got.inline_values().unwrap(), vec![7, 9]);
    }

    #[test]
    fn small_containers_share_a_packed_page() {
        let dirty: Vec<DirtyChunk> = (0..200u64)
            .map(|i| chunk(1, i, &[1, 3, 5, 7, 9, 11], 1))
            .collect();
        let (res, _n, _a, ext) = run_one(dirty, 1);
        assert_eq!(res.packed, 200);
        assert_eq!(res.standalone, 0);
        assert!(
            ext.writes.len() <= 2,
            "200 small chunks should share one or two pages, wrote {}",
            ext.writes.len()
        );
    }

    #[test]
    fn a_bitmap_is_never_packed() {
        // Scattered, so it stays a bitmap through `optimize()`. Contiguous
        // values would run-optimize to a 6-byte payload and be packed — which is
        // correct behaviour, just not what this test is about.
        let scattered: Vec<u16> = (0..5000u16).map(|i| i * 2).collect();
        let dirty = vec![chunk(1, 0, &scattered, 1)];
        let (res, _n, _a, ext) = run_one(dirty, 1);
        assert_eq!(res.packed, 0, "a bitmap must never share a packed page");
        assert_eq!(res.standalone, 1);
        assert_eq!(
            ext.writes.values().next().unwrap().len(),
            crate::BITMAP_BYTES
        );
    }

    #[test]
    fn a_contiguous_range_run_optimizes_and_then_packs() {
        // The complement of the test above: a dense range becomes a tiny Run at
        // checkpoint, so it belongs in a packed page rather than an 8 KiB slot.
        let dirty = vec![chunk(1, 0, &(0..5000u16).collect::<Vec<_>>(), 1)];
        let (res, _n, _a, _e) = run_one(dirty, 1);
        assert_eq!(res.packed, 1);
        assert_eq!(res.standalone, 0);
        assert!(
            res.extent_bytes < 64,
            "a single run should cost a handful of bytes, not {}",
            res.extent_bytes
        );
    }

    #[test]
    fn entries_are_written_in_chunkkey_order_regardless_of_input_order() {
        // The locality property: consecutive chunks of one key must land in
        // consecutive slots even if the caller hands them over shuffled.
        let mut dirty: Vec<DirtyChunk> = (0..50u64)
            .map(|i| chunk(1, i, &[1, 3, 5, 7, 9, 11], 1))
            .collect();
        dirty.reverse();
        let (res, nodes, _a, _e) = run_one(dirty, 1);

        let t = Tree {
            root: res.superblock.root.unwrap().0,
            height: res.superblock.root.unwrap().1,
            node_size: INDEX_NODE,
        };
        let keys: Vec<ChunkKey> = t.iter(&nodes).map(|r| r.unwrap().0).collect();
        assert!(
            keys.windows(2).all(|w| w[0] < w[1]),
            "index must be key-ordered"
        );
        assert_eq!(keys.len(), 50);
    }

    #[test]
    fn the_previous_extent_is_deferred_not_freed() {
        // An older snapshot may still reach it through the previous root.
        let mut alloc = Allocator::new();
        alloc.begin_generation();
        let old_cell = alloc.alloc(class_for(600).unwrap()).unwrap();
        let old = ChunkRef::extent(old_cell, ContainerKind::Array, 300).unwrap();

        let dirty = vec![DirtyChunk {
            key: ChunkKey::new(1, 0),
            container: Container::from_sorted(&(0..300u16).collect::<Vec<_>>()),
            version: 1,
            previous: Some(old),
        }];

        let mut ext = MemExtents::default();
        let mut nodes = VecNodes::new();
        let prev = SuperBlock::initial([1u8; 16], 0);
        let mut sink = SplitSink {
            extents: &mut ext,
            nodes: &mut nodes,
            alloc: &mut alloc,
        };
        run(5, dirty, &[], None, &mut sink, INDEX_NODE, &prev, 3, 0, 0).unwrap();

        assert_eq!(
            alloc.deferred_count(),
            1,
            "the old extent must be queued, not freed"
        );
        // And it stays queued until the reachability and checkpoint gates pass.
        assert_eq!(
            alloc.reclaim(2, 99, |_, _| false),
            0,
            "a reader holding the root of checkpoint 2 can still reach it"
        );
        assert_eq!(
            alloc.reclaim(u64::MAX, 3, |_, _| false),
            0,
            "needs checkpoint_seq >= 3 + 2"
        );
        assert_eq!(alloc.reclaim(u64::MAX, 5, |_, _| false), 1);
    }

    #[test]
    fn the_superblock_advances_and_names_the_new_root() {
        let dirty: Vec<DirtyChunk> = (0..30u64).map(|i| chunk(1, i, &[1, 2], 4)).collect();
        let (res, _n, _a, _e) = run_one(dirty, 4);
        assert_eq!(res.superblock.seq, 2, "one past the previous image");
        assert!(res.superblock.root.is_some());
        assert_eq!(res.superblock.checkpoint_cv, 4);
        assert_eq!(res.superblock.checkpoint_seq, 1);
        assert_eq!(res.superblock.wal_replay_lsn, 4096);
    }

    #[test]
    fn an_empty_checkpoint_produces_no_root() {
        let (res, _n, _a, _e) = run_one(vec![], 9);
        assert_eq!(res.chunks_written, 0);
        assert_eq!(res.superblock.root, None);
        assert_eq!(
            res.superblock.checkpoint_cv, 9,
            "the watermark still advances"
        );
    }

    #[test]
    fn run_optimization_is_applied_at_checkpoint() {
        // A contiguous run should be stored as a Run container, which is a
        // storage decision deliberately deferred off the write path.
        let vals: Vec<u16> = (0..3000u16).collect();
        let dirty = vec![chunk(1, 0, &vals, 1)];
        let (res, nodes, _a, _e) = run_one(dirty, 1);

        let t = Tree {
            root: res.superblock.root.unwrap().0,
            height: res.superblock.root.unwrap().1,
            node_size: INDEX_NODE,
        };
        let got = t.get(&nodes, ChunkKey::new(1, 0)).unwrap().unwrap();
        assert_eq!(
            got.kind(),
            ContainerKind::Run,
            "a contiguous range must run-optimize"
        );
        assert_eq!(got.cardinality(), 3000);
    }

    #[test]
    fn policy_triggers_and_the_stall_bound() {
        let p = CheckpointPolicy::default();
        assert!(!p.should_checkpoint(0, 0, 0));
        assert!(p.should_checkpoint(p.dirty_bytes, 0, 0));
        assert!(p.should_checkpoint(0, p.wal_bytes, 0));
        assert!(p.should_checkpoint(0, 0, p.interval_secs));

        // The stall is the only bound on memtable growth.
        assert!(!p.should_stall(p.dirty_bytes));
        assert!(p.should_stall(p.max_dirty_bytes));
        assert!(
            p.max_dirty_bytes > p.dirty_bytes,
            "the stall threshold must sit above the trigger, or writers stall every checkpoint"
        );
    }

    #[test]
    fn a_generation_is_claimed_per_checkpoint() {
        let mut alloc = Allocator::new();
        let g0 = alloc.generation();
        let mut ext = MemExtents::default();
        let mut nodes = VecNodes::new();
        let prev = SuperBlock::initial([1u8; 16], 0);
        let dirty: Vec<DirtyChunk> = (0..5u64)
            .map(|i| chunk(1, i, &(0..500u16).collect::<Vec<_>>(), 1))
            .collect();
        let mut sink = SplitSink {
            extents: &mut ext,
            nodes: &mut nodes,
            alloc: &mut alloc,
        };
        run(1, dirty, &[], None, &mut sink, INDEX_NODE, &prev, 1, 0, 0).unwrap();
        assert!(
            alloc.generation() > g0,
            "each checkpoint claims a fresh generation"
        );
    }
}
