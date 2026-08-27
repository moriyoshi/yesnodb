//! One shard's on-disk state: the data file, its allocator, and its index root.
//!
//! Ties together [`SegmentedMmap`] ( bytes ), [`Allocator`] ( space ),
//! [`SuperBlock`] ( the commit point ) and [`Tree`] ( the index ), and supplies
//! the three writer traits a checkpoint needs.
//!
//! # Reads are zero-copy, writes go through `pwrite`
//!
//! Reading a container hands out an Arrow `Buffer` aliasing the mapping, so the
//! payload is never copied. Writing never touches the mapping — see the I2
//! discussion in [`crate::store::segment`]. The asymmetry is the whole design.
//!
//! # Index nodes share the extent allocator
//!
//! A 1 KiB node lands in the 1088-byte class, wasting ~6%. A dedicated class
//! would remove that, but the superblock's fixed class array is exactly full at
//! 11 entries, so adding one is a layout change rather than a tweak. The waste
//! applies only to index nodes, which are a few percent of the file, so the
//! trade is a format break against a fraction of a percent of total size --
//! which is the whole argument, and it is stated here rather than pointed at.

use std::path::Path;

use crate::container::{codec, Container};
use crate::error::{CodecError, Result};
use crate::index::tree::{NodeReader, NodeWriter, Page, PageId, Tree};
use crate::store::alloc::Allocator;
use crate::store::extent::{class_for, ChunkKey, ChunkRef};
use crate::store::segment::SegmentedMmap;
use crate::store::superblock::{self, SuperBlock};
use crate::store::{PAGE, SLAB_SIZE};
use crate::ContainerKind;
use arrow_buffer::Buffer;

/// Bytes reserved before the slab region: two superblock slots.
pub const RESERVED: u64 = 2 * PAGE as u64;

/// A `PageId` is a byte offset divided by 64, **not** a raw offset.
///
/// A raw offset would overflow the 32-bit id past 4 GiB, silently aliasing
/// distant pages. Every slot in every class is 64-byte aligned ( a ladder
/// invariant ), so dividing is lossless and extends the addressable range to
/// 256 GiB — comfortably past the 128 GiB per-shard cap, with the check below
/// making an overflow an error rather than a wrap.
pub const PAGE_ID_SHIFT: u64 = 64;

#[inline]
fn cell_to_page_id(cell: u64) -> Result<PageId> {
    debug_assert_eq!(
        cell % PAGE_ID_SHIFT,
        0,
        "index nodes must be 64-byte aligned"
    );
    let id = cell / PAGE_ID_SHIFT;
    u32::try_from(id).map_err(|_| CodecError::Invariant("index page id exceeds 32 bits"))
}

#[inline]
pub const fn page_id_to_cell(id: PageId) -> u64 {
    id as u64 * PAGE_ID_SHIFT
}

/// A shard's persistent store.
pub struct ShardStore {
    /// Shared so the durability sequence can run **without** the store lock.
    ///
    /// `Db::checkpoint` clones this `Arc`, releases `Mutex<ShardStore>`, and
    /// runs `commit_superblock`'s three `fsync`s against it. Readers on the
    /// shard proceed throughout: the active superblock is still the old one,
    /// the new one is written to the **inactive** slot, and new extents are
    /// unreachable from the old root, so a reader following that root sees a
    /// consistent old state for the whole window. The exclusion the `Arc`
    /// removes came from `&mut self`, never from the durability protocol.
    ///
    /// **This is what makes `segment.rs`'s interior mutexes load-bearing.**
    /// While the segment was reachable only through the store lock they guarded
    /// a race that could not happen; now a reader and a syncing checkpoint are
    /// genuinely concurrent inside it. Do not remove them.
    seg: std::sync::Arc<SegmentedMmap>,
    alloc: Allocator,
    sb: SuperBlock,
    /// Index pages written during the current checkpoint, keyed by page id.
    ///
    /// A page id is an offset in the file; this map is the write-side buffer,
    /// flushed as part of the checkpoint. Reads consult it first so an
    /// in-progress rebuild is self-consistent.
    pending_nodes: std::collections::BTreeMap<PageId, Vec<u8>>,
    nodes_written: u64,
    nodes_freed: u64,
}

/// A prepared superblock commit whose durability sequence has not yet run.
///
/// # Why this is a separate object
///
/// `commit_superblock` was three `fsync`s inside `Mutex<ShardStore>`, and every
/// reader on the shard blocked for all of them. Measured at 3-9 ms per
/// checkpoint depending on how much the rebuild wrote, and a consumer measured
/// the other side of it: p99.9 query latency 6.85 -> 54.92 ms with a writer
/// present, median unmoved.
///
/// Splitting lets [`ShardStore::prepare_superblock`] gather what the sequence
/// needs under the lock, the lock be released, and [`run`](Self::run) do the
/// I/O with readers proceeding.
pub(crate) struct SuperblockCommit {
    seg: std::sync::Arc<SegmentedMmap>,
    /// `( offset, bytes )` per non-opaque slab, computed under the lock because
    /// it reads the allocator.
    slab_writes: Vec<(u64, Vec<u8>)>,
    sb_off: u64,
    sb_bytes: Vec<u8>,
    pub(crate) superblock: SuperBlock,
}

impl SuperblockCommit {
    /// The durability sequence. **Runs with the store lock released.**
    ///
    /// # Why readers need not be excluded
    ///
    /// Nothing here changes what a reader can reach. The live superblock is
    /// still the old one -- `adopt_superblock` is what changes it, afterwards,
    /// under the lock. The new superblock is written to the slot `pick` is
    /// *not* returning, so a torn write there is invisible. The extents and
    /// nodes being flushed are unreachable from the old root. And the slab
    /// metadata region is not on any read path: it is a cache the committed
    /// index reproduces. So a reader following the old root sees a consistent
    /// old state for the entire window.
    ///
    /// # The ordering is a barrier sequence and must not be reordered
    ///
    /// Every new extent and index node must be durable **before** the pointer
    /// that makes them reachable. The three syncs are that guarantee; merging or
    /// dropping one breaks crash recovery in a way no test that does not tear
    /// the file will notice.
    /// Consumes the commit and returns the only token `adopt_superblock`
    /// accepts, so **adopting before the syncs cannot be expressed**.
    ///
    /// That is deliberately a type-level guarantee rather than a test. Adopting
    /// early changes no on-disk byte -- it only moves an in-memory field -- so
    /// no in-process test can observe it, and a sabotage that reorders the two
    /// passes every suite in the tree. It is still a real defect: if a sync then
    /// fails, `abandon_checkpoint` does not undo an adoption that already
    /// happened, leaving a live superblock whose data is not durable. A rule no
    /// test can enforce is worth moving into the type system.
    pub(crate) fn run(self) -> Result<Durable> {
        self.seg.sync().map_err(io_err)?;
        for (off, bytes) in &self.slab_writes {
            self.seg.write_at(*off, bytes).map_err(io_err)?;
        }
        self.seg.sync().map_err(io_err)?;
        self.seg
            .write_at(self.sb_off, &self.sb_bytes)
            .map_err(io_err)?;
        self.seg.sync().map_err(io_err)?;
        Ok(Durable(self.superblock))
    }
}

/// Proof that a superblock's durability sequence completed.
///
/// Obtainable only from [`SuperblockCommit::run`], and required by
/// [`ShardStore::adopt_superblock`].
pub(crate) struct Durable(SuperBlock);

impl ShardStore {
    /// Open, creating the file and an initial superblock if absent.
    /// Open an existing shard **read-only**, for a process that does not own
    /// the database.
    ///
    /// Differs from [`ShardStore::open`] in what it refuses, not in what it
    /// reads. There is no fresh-shard branch — a reader that created a shard
    /// file would have invented a database — and the allocator is restored
    /// exactly as usual, because it is consulted by `reclaim_deferred`, which a
    /// reader never calls, and costs nothing to keep truthful.
    ///
    /// The caller must not checkpoint, allocate, or commit through this
    /// store. Nothing here enforces that; `Db`'s read-only mode is what does.
    pub fn open_read_only(
        path: impl AsRef<Path>,
        db_uuid: [u8; 16],
        shard_id: u32,
    ) -> Result<Self> {
        let seg = SegmentedMmap::open_read_only(path.as_ref()).map_err(io_err)?;
        let len = seg.file_len().map_err(io_err)?;
        if len == 0 {
            return Err(CodecError::Invariant(
                "shard file is empty; a reader cannot initialise a database",
            ));
        }
        let a = seg.read_at(0, PAGE)?;
        let b = seg.read_at(PAGE as u64, PAGE)?;
        let sb = superblock::pick(&a, &b)?.ok_or(CodecError::Invariant(
            "both superblock slots are unreadable",
        ))?;
        sb.check_identity(db_uuid, shard_id)?;
        let mut restored = Vec::with_capacity(sb.n_slabs as usize);
        for id in 0..sb.n_slabs {
            let restored_slab = crate::store::slabmeta::offset_of(id)
                .and_then(|off| seg.read_at(off, crate::store::SLAB_META as usize).ok())
                .and_then(|b| crate::store::slabmeta::decode(&b));
            restored.push(restored_slab);
        }
        Ok(ShardStore {
            seg: std::sync::Arc::new(seg),
            alloc: Allocator::restore(restored),
            sb,
            pending_nodes: Default::default(),
            nodes_written: 0,
            nodes_freed: 0,
        })
    }

    pub fn open(path: impl AsRef<Path>, db_uuid: [u8; 16], shard_id: u32) -> Result<Self> {
        let seg = SegmentedMmap::open(path.as_ref()).map_err(io_err)?;
        let len = seg.file_len().map_err(io_err)?;

        if len == 0 {
            // Fresh shard: reserve the superblock slots and the first slab.
            seg.grow_to(RESERVED + SLAB_SIZE).map_err(io_err)?;
            let sb = SuperBlock::initial(db_uuid, shard_id);
            let bytes = sb.encode()?;
            // Both slots, so either can be picked before the first checkpoint.
            seg.write_at(0, &bytes).map_err(io_err)?;
            seg.write_at(PAGE as u64, &bytes).map_err(io_err)?;
            seg.sync().map_err(io_err)?;
            return Ok(ShardStore {
                seg: std::sync::Arc::new(seg),
                alloc: Allocator::new(),
                sb,
                pending_nodes: Default::default(),
                nodes_written: 0,
                nodes_freed: 0,
            });
        }

        let a = seg.read_at(0, PAGE)?;
        let b = seg.read_at(PAGE as u64, PAGE)?;
        let sb = superblock::pick(&a, &b)?.ok_or(CodecError::Invariant(
            "both superblock slots are unreadable",
        ))?;
        // This is what the `db_uuid` is *for*, and until 2026-08-28 nothing
        // did it: the field was written into every superblock and compared
        // nowhere, so a shard file from another database opened cleanly and
        // served its own contents under this database's keys.
        //
        // And the uuid alone cannot finish the job, because **every shard of
        // one database carries the same one**. Exchanging two shard images of a
        // single database satisfies it, and each shard then answers its keys out
        // of the other's extents — the same silent-wrong-answer failure one
        // scope down. `check_identity` compares the physical shard number too;
        // it reports `DatabaseIdentityMismatch` first, so the older behaviour is
        // unchanged for a genuinely foreign file.
        sb.check_identity(db_uuid, shard_id)?;
        // The existing slabs must be accounted for, or the first allocation
        // after this reopen lands in slab 0, on top of live extents. Read each
        // slab's persisted occupancy where it is intact; a slab whose block is
        // missing or torn stays Opaque, which is never allocated into.
        let mut restored = Vec::with_capacity(sb.n_slabs as usize);
        for id in 0..sb.n_slabs {
            let restored_slab = crate::store::slabmeta::offset_of(id)
                .and_then(|off| seg.read_at(off, crate::store::SLAB_META as usize).ok())
                .and_then(|b| crate::store::slabmeta::decode(&b));
            restored.push(restored_slab);
        }
        let alloc = Allocator::restore(restored);
        Ok(ShardStore {
            seg: std::sync::Arc::new(seg),
            alloc,
            sb,
            pending_nodes: Default::default(),
            nodes_written: 0,
            nodes_freed: 0,
        })
    }

    #[inline]
    pub fn superblock(&self) -> &SuperBlock {
        &self.sb
    }

    /// Index nodes written over this store's life.
    ///
    /// The measure of whether leaf reuse is working. A checkpoint that rebuilds
    /// the tree whole writes one node per leaf every time regardless of how
    /// little changed, and no correctness test can tell that apart from reusing
    /// them.
    #[inline]
    pub fn nodes_written(&self) -> u64 {
        self.nodes_written
    }

    /// Superseded index pages queued for reclamation.
    ///
    /// The check on the exclusion rule: if this tracked the *whole* old tree
    /// rather than `old - reused`, it would equal the tree's node count on every
    /// checkpoint instead of the handful the change actually superseded.
    #[inline]
    pub fn nodes_freed(&self) -> u64 {
        self.nodes_freed
    }

    /// The mapped address space, for the reclamation pin check.
    #[inline]
    pub fn segment(&self) -> &crate::store::segment::SegmentedMmap {
        &self.seg
    }

    #[inline]
    pub fn allocator(&mut self) -> &mut Allocator {
        &mut self.alloc
    }

    /// The committed index, if this shard has ever checkpointed.
    pub fn tree(&self) -> Option<Tree> {
        self.sb.root.map(|(root, height)| Tree {
            root,
            height,
            node_size: self.sb.node_size as usize,
        })
    }

    /// Prefixes of every chunk of `key` in the committed index.
    pub fn key_prefixes(&self, key: u64) -> Result<Vec<u64>> {
        let Some(t) = self.tree() else {
            return Ok(Vec::new());
        };
        t.range(self, ChunkKey::range_start(key), ChunkKey::range_end(key))
            .map(|r| r.map(|(k, _)| k.prefix()))
            .collect()
    }

    /// Materialize a chunk, zero-copy where the payload allows it.
    /// Bytes of payload a reference covers.
    ///
    /// A run's length lives in its own payload prefix, so it costs a two-byte
    /// read. Shared with the extent checksum check so the two cannot disagree
    /// about the span -- a CRC compared over a different length than the writer
    /// used fails on intact data, which is the failure mode that would have this
    /// check switched off rather than debugged.
    fn payload_len_of(&self, cref: ChunkRef, cell: u64) -> Result<usize> {
        Ok(match cref.kind() {
            ContainerKind::Run => {
                let hdr = self.seg.read_at(cell, 2)?;
                crate::run_bytes(u16::from_le_bytes([hdr[0], hdr[1]]) as u32)
            }
            _ => cref.payload_len(None)?,
        })
    }

    pub fn read_container(&self, cref: ChunkRef) -> Result<Option<Container>> {
        if let Some(vals) = cref.inline_values() {
            return Ok(Some(Container::from_sorted(&vals)));
        }
        let Some(cell) = cref.cell() else {
            return Ok(None);
        };
        let len = self.payload_len_of(cref, cell)?;
        // Zero-copy: the container aliases the mapping, and its Buffer keeps
        // that mapping alive independently of this store.
        let buf = self.seg.buffer_at(cell, len)?;
        Ok(Some(codec::decode_buffer(
            cref.kind(),
            &buf,
            0,
            len,
            cref.cardinality(),
        )?))
    }

    /// Reserve space for `n` more bytes of allocation.
    fn ensure_capacity(&self) -> Result<()> {
        let want = RESERVED + (self.alloc.slab_count() as u64 + 1) * SLAB_SIZE;
        self.seg.grow_to(want).map_err(io_err)
    }

    /// Read a chunk, checking that the reference really points at **this** key.
    ///
    /// `read_container` trusts its `ChunkRef` completely: a corrupt or stale one
    /// decodes whatever bytes it lands on and returns them as the key's
    /// contents. Wrong answers, no error. The format has two mechanisms against
    /// exactly that and neither was consulted —
    ///
    /// - a standalone extent carries an [`ExtTrailer`] whose `ckey_tag` is 32
    ///   bits of the chunk key, so a mis-point is caught with probability
    ///   `1 - 2^-32` ( it was never even *written* until 2026-08-25 );
    /// - a packed page's header records the `[first, last]` key range it holds,
    ///   which `may_contain` tests in O(1).
    ///
    /// Both are **identity** checks. Until 2026-09-14 they were the only checks
    /// here, and this comment argued they should be: *"verifying a payload means
    /// a CRC over up to 8 KiB on every read, which would roughly double the cost
    /// of a bitmap intersection, and `fsck` is where that belongs."*
    ///
    /// **The objection was to "on every read", and that is what changed.**
    /// `SegmentedMmap::verify_once` recomputes a stored checksum the first time a
    /// region is touched and not again until a write lands on it, so the pass is
    /// paid per faulted region rather than per read. All three families are now
    /// content-checked here: the packed page's whole-page CRC, the standalone
    /// extent's trailer CRC, and — in `NodeReader::node` — the index node's.
    ///
    /// **Why `fsck` was not where it belonged after all.** An identity check
    /// answers "is this the right chunk", not "are these the bytes that were
    /// written", and the gap between those is exactly a wrong-but-in-range
    /// reference — which `covers()` admits, the safe slice index admits, and
    /// *neither AddressSanitizer nor Valgrind can see*, both being structurally
    /// blind to a read that stays inside a mapping ( measured 2026-09-14 ). An
    /// offline scan cannot help a query that has already returned. See
    /// `stored-page-crcs-are-not-verified` and
    /// `miri-cannot-reach-the-mmap-unsafe-sites`.
    pub fn read_container_for(&self, key: ChunkKey, cref: ChunkRef) -> Result<Option<Container>> {
        // A version gate, not a corruption check: refuse format bits this build
        // does not understand *before* interpreting the payload they describe.
        // See `ChunkRef::validate` for why silently decoding kind 3 as `Array`
        // is the wrong default.
        cref.validate()?;

        let Some(cell) = cref.cell() else {
            // Inline: the payload is in the index entry, so there is no
            // reference to be wrong about.
            return self.read_container(cref);
        };

        if self.alloc.is_packed_cell(cell) {
            let base = cell - (cell % crate::store::PAGE as u64);
            let hdr = self.seg.read_at(base, crate::store::packed::HEADER)?;
            let h = crate::store::packed::PackedHeader::parse(&hdr)?;
            if !h.may_contain(key) {
                return Err(CodecError::Invariant(
                    "packed reference points at a page that does not hold this key",
                ));
            }
            // The payload region starts immediately after the header, so a cell
            // below that is decoding the header as a payload. `may_contain` is
            // a *range* check over the page's `[first, last]` — it says the page
            // could hold this key, not that the offset is where the key lives —
            // so it cannot catch this. Free: the page base is already computed
            // and no extra read is needed.
            if cell < base + crate::store::packed::HEADER as u64 {
                return Err(CodecError::Invariant(
                    "packed reference points into the page header",
                ));
            }
            // Whole-page CRC, once per page rather than once per chunk read --
            // which matters more here than for the other two families, because a
            // packed page holds many chunks and they share one checksum.
            //
            // **The sharing cuts both ways and the cost side is unmeasured.**
            // One cache entry covers all 4096 bytes, so a write intersecting
            // this page invalidates verification for **every key with a chunk in
            // it**, and each of their readers then re-CRCs the whole page rather
            // than its own chunk. The amplification is keys-per-page, and a
            // single-key benchmark cannot see it by construction. Tracked as
            // `packed-page-sharing-may-contend-across-keys`; do not treat the
            // once-per-page framing as a pure win until that is measured.
            self.seg.verify_once(base, crate::store::PAGE, || {
                let page = self.seg.read_at(base, crate::store::PAGE)?;
                crate::store::packed::PackedHeader::verify(&page).map(|_| ())
            })?;
        } else if let Some(class) = self.alloc.owning_class(cell) {
            let slot = crate::store::extent::class_size(class)
                .ok_or(CodecError::Invariant("unknown size class"))? as u64;
            let off = cell + slot - crate::store::extent::EXT_TRAILER_BYTES as u64;
            let raw = self
                .seg
                .read_at(off, crate::store::extent::EXT_TRAILER_BYTES)?;
            let t = crate::store::extent::ExtTrailer::from_le_bytes(
                raw.try_into()
                    .map_err(|_| CodecError::Invariant("short extent trailer"))?,
            );
            let expected = crate::store::extent::ckey_tag(key);
            if t.ckey_tag != expected {
                return Err(CodecError::MisPointedExtent {
                    key: key.0,
                    cell,
                    class,
                    trailer_off: off,
                    found: t.ckey_tag,
                    expected,
                });
            }
            // The trailer is already in hand, so the stored CRC costs no extra
            // read -- only the pass over the payload, and only the first time
            // this region is touched.
            //
            // **This is the check `ckey_tag` cannot make.** The tag proves the
            // reference names the right chunk; it says nothing about whether the
            // bytes are the ones that were written. A wrong-but-in-range offset
            // with a matching tag, or an intact reference over a corrupted
            // payload, both pass the tag and fail here.
            let payload_len = self.payload_len_of(cref, cell)?;
            self.seg.verify_once(cell, payload_len, || {
                // Zero-copy: the CRC reads the mapping directly. Copying the
                // payload out with `read_at` first -- which is what this did
                // when it landed -- measured **1.99 us per 8 KiB region**
                // against **1.25 us** this way, so the copy was a third of the
                // one-off cost. Both figures are a first touch; steady state is
                // zero either way.
                let buf = self.seg.buffer_at(cell, payload_len)?;
                if crate::store::checksum::crc32c(buf.as_slice()) != t.crc32c {
                    return Err(CodecError::Invariant(
                        "extent payload fails its stored checksum",
                    ));
                }
                Ok(())
            })?;
        }
        // An Opaque slab has no known geometry, so there is nothing to check
        // against; the read proceeds unverified rather than failing.

        self.read_container(cref)
    }

    /// Free deferred extents whose three conditions have all been met.
    ///
    /// Kept here rather than in `Db::checkpoint` because it needs a **split
    /// borrow**: the allocator mutably, the mapping immutably, both fields of
    /// this struct. Doing it from outside would need the allocator moved out —
    /// which is exactly the shape that caused index nodes to be written over
    /// live extents ( see `checkpoint::AllocSource` ).
    ///
    /// The predicate is reclamation **condition 3**. Conditions 1 and 2 live
    /// inside `Allocator::reclaim`; all three must hold, and none implies
    /// another.
    pub fn reclaim_deferred(&mut self, reader_ckpt_floor: u64, ckpt_seq: u64) -> usize {
        let seg = &self.seg;
        self.alloc
            .reclaim(reader_ckpt_floor, ckpt_seq, |cell, size| {
                seg.any_pinned_in(cell, size)
            })
    }

    /// Recompute occupancy from the committed index and adopt it, returning
    /// how many orphaned slots came back.
    ///
    /// # The leak this repairs
    ///
    /// The deferred free list is in-memory ( I3 ), so an extent superseded but
    /// not yet past the three reclamation conditions is queued nowhere durable
    /// while its slot is persisted as **used**. After a reopen nothing
    /// references it and nothing has it queued: it is lost for good. Measured
    /// as `pending` before a reopen becoming `leaked` after it, permanently,
    /// and it accrues once per restart.
    ///
    /// I3's own argument — "on restart there are no readers, so the
    /// checkpointed free bitmaps already describe exactly what is reclaimable"
    /// — is not true of the bitmaps, but it *is* true of the index. So walk it.
    ///
    /// # Why this is safe here and nowhere else
    ///
    /// At open there is one root, no readers, and an empty deferred queue, so
    /// "unreachable from the committed root" and "free" are the same set. See
    /// [`Allocator::adopt_live_at_open`].
    ///
    /// # Why it refuses on any doubt
    ///
    /// A rebuild that reported an error or a dangling reference has an
    /// **incomplete** liveness map, and adopting an incomplete map does not
    /// lose a repair — it frees live data. Refusing leaves the orphans, which
    /// is the failure this method exists to fix and is strictly better than the
    /// alternative.
    pub fn rebuild_allocator_at_open(&mut self) -> Result<usize> {
        let Some(tree) = self.tree() else {
            return Ok(0);
        };
        let classes: std::collections::BTreeMap<u32, u8> = (0..self.alloc.slab_count() as u32)
            .filter_map(|id| match self.alloc.slab(id).map(|s| s.state) {
                Some(crate::store::alloc::SlabState::InUse { class, .. }) => Some((id, class)),
                _ => None,
            })
            .collect();

        let (rebuilt, errors) = crate::store::fsck::rebuild(
            &tree,
            // Raw: this walk must be able to read a corrupt node to report it.
            &RawNodes(self),
            |slab| classes.get(&slab).copied(),
            |cell| {
                let hdr = self.seg.read_at(cell, 2)?;
                Ok(u16::from_le_bytes([hdr[0], hdr[1]]) as u32)
            },
        )?;

        let report = crate::store::fsck::verify(&rebuilt, &self.alloc, errors);
        if !report.errors.is_empty()
            || !report.dangling.is_empty()
            || !report.dangling_nodes.is_empty()
        {
            return Ok(0);
        }

        let mut live: std::collections::BTreeMap<u32, std::collections::BTreeSet<u32>> =
            Default::default();
        for (slab, slots) in &rebuilt.used {
            live.entry(*slab).or_default().extend(slots.keys().copied());
        }
        for (slab, slots) in &rebuilt.index_slots {
            live.entry(*slab).or_default().extend(slots.iter().copied());
        }
        Ok(self.alloc.adopt_live_at_open(&live))
    }

    /// Flip the superblock in one call. **This is the commit point** --
    /// everything written before it is unreachable, everything after it is
    /// durable.
    ///
    /// **Test-only since 2026-09-14.** Production goes through
    /// `prepare_superblock` / `run` / `adopt_superblock` so the three `fsync`s
    /// can happen with `Mutex<ShardStore>` released. This shorthand is kept
    /// because the store's own unit tests drive a `ShardStore` directly and have
    /// no lock to release; marked `cfg(test)` rather than left public, since a
    /// production caller using it would silently reinstate the stall.
    #[cfg(test)]
    pub fn commit_superblock(&mut self, sb: SuperBlock) -> Result<()> {
        let durable = self.prepare_superblock(sb)?.run()?;
        self.adopt_superblock(durable);
        Ok(())
    }

    /// Everything the durability sequence needs, gathered under the store lock.
    ///
    /// Split out so [`SuperblockCommit::run`] -- which is three `fsync`s and
    /// nothing else -- can execute with `Mutex<ShardStore>` **released**. See
    /// that method for why readers need not be excluded from it.
    pub(crate) fn prepare_superblock(&self, sb: SuperBlock) -> Result<SuperblockCommit> {
        // Each slab's occupancy, for the region it reserves. Collected rather
        // than written here: the writes belong in `run`, outside the lock, and
        // this loop needs the allocator.
        //
        // Ordered before the superblock flip and separately synced, so the
        // metadata is on disk before the root that makes the slabs reachable.
        // It is one of only two in-place writes in the store, and it is safe for
        // the reason `slabmeta` gives: every byte is derivable from the index,
        // so a torn write costs a rebuild rather than data.
        let mut slab_writes = Vec::new();
        for (id, slab) in self.alloc.slabs().iter().enumerate() {
            // An Opaque slab is one this process never learned the occupancy of.
            // Skipping it leaves whatever was there -- either valid metadata from
            // the writer that did know, or nothing. Both beat persisting a guess.
            if matches!(slab.state, crate::store::alloc::SlabState::Opaque) {
                continue;
            }
            // `None` for slab 0: its metadata region is the superblock.
            let Some(off) = crate::store::slabmeta::offset_of(id as u32) else {
                continue;
            };
            slab_writes.push((off, crate::store::slabmeta::encode(slab)?));
        }
        Ok(SuperblockCommit {
            seg: self.seg.clone(),
            slab_writes,
            sb_off: self.sb.next_slot_offset(),
            sb_bytes: sb.encode()?,
            superblock: sb,
        })
    }

    /// Take the new superblock as live. Must follow a successful
    /// [`SuperblockCommit::run`], and must happen under the store lock.
    pub(crate) fn adopt_superblock(&mut self, durable: Durable) {
        self.sb = durable.0;
        self.pending_nodes.clear();
    }

    /// Discard a half-built checkpoint. Safe at any point before the flip,
    /// because nothing written is reachable from the live superblock.
    ///
    /// **Must be called on every error path out of a checkpoint**, and for a
    /// while was called on none. `pending_nodes` holds an owned copy of every
    /// index node written so far — it exists because the bottom-up build has to
    /// read back pages that are not yet reachable from any superblock — and it
    /// is cleared only by [`Self::commit_superblock`], on success. A checkpoint
    /// that failed after writing nodes therefore retained all of them until the
    /// next successful checkpoint.
    ///
    /// The cells stay allocated too, so within one process a failed checkpoint
    /// cannot hand the same `PageId` out twice and the stale entries cannot
    /// *shadow* a rewritten page. That makes this a bounded leak rather than a
    /// correctness bug today — but the shadowing is what
    /// [`NodeReader::node`] would do if allocator rollback were ever added, so
    /// the two must not drift apart.
    pub fn abandon_checkpoint(&mut self) {
        self.pending_nodes.clear();
    }

    /// Index nodes buffered by an in-flight checkpoint. Diagnostics, and the
    /// only way to observe [`Self::abandon_checkpoint`] having run.
    #[cfg(test)]
    pub fn pending_node_count(&self) -> usize {
        self.pending_nodes.len()
    }
}

fn io_err(e: std::io::Error) -> CodecError {
    // The store's error type predates io; map rather than widen it here.
    CodecError::Invariant(match e.kind() {
        std::io::ErrorKind::NotFound => "shard file not found",
        std::io::ErrorKind::PermissionDenied => "permission denied on shard file",
        _ => "shard file I/O error",
    })
}

/// A [`NodeReader`] that deliberately does **not** recompute node checksums.
///
/// `ShardStore`'s own reader refuses a node whose stored CRC does not match, so
/// a scan built on it could not read a corrupt node in order to *report* one —
/// the walk would abort at the first bad page and attribute nothing. `rebuild`
/// already checks every node inline and names the page it found, which is
/// strictly more useful than an error propagating out.
///
/// Stateless on purpose. The alternative considered was a "diagnostic mode" flag
/// on `ShardStore`, which makes the refusal depend on when it is read rather than
/// on who is reading, and leaves a way for production to be in the wrong mode.
pub(crate) struct RawNodes<'a>(pub(crate) &'a ShardStore);

impl NodeReader for RawNodes<'_> {
    fn node(&self, id: PageId) -> Result<Page> {
        self.0.node_unverified(id)
    }
}

impl ShardStore {
    /// Node bytes with no checksum recomputation. See [`RawNodes`].
    pub(crate) fn node_unverified(&self, id: PageId) -> Result<Page> {
        if let Some(b) = self.pending_nodes.get(&id) {
            return Ok(Page::from_buffer(Buffer::from_vec(b.clone())));
        }
        self.seg
            .buffer_at(page_id_to_cell(id), self.sb.node_size as usize)
            .map(Page::from_buffer)
    }
}

impl NodeReader for ShardStore {
    /// Zero-copy from the mapping, checksum recomputed once per faulted region.
    ///
    /// Pages written earlier in the *current* checkpoint are not yet reachable
    /// from any superblock, but the bottom-up build needs to read them back, so
    /// they are served from the in-flight buffer until the flip.
    fn node(&self, id: PageId) -> Result<Page> {
        if let Some(b) = self.pending_nodes.get(&id) {
            // In flight and not yet on disk, so there is no stored checksum to
            // compare against — this build wrote these bytes itself.
            return Ok(Page::from_buffer(Buffer::from_vec(b.clone())));
        }
        let cell = page_id_to_cell(id);
        let len = self.sb.node_size as usize;
        let buf = self.seg.buffer_at(cell, len)?;
        // Recompute the node's stored CRC the first time this region is read,
        // and not again until a write lands on it. Until 2026-09-14 nothing on
        // the read path recomputed it: online reads checked node shape and
        // version, and only `Db::verify()` compared the checksum. This is the
        // family with the widest blast radius of the three, because a corrupt
        // internal node misdirects a search rather than corrupting one answer.
        self.seg.verify_once(cell, len, || {
            crate::index::node::verify_checksum(buf.as_slice())
        })?;
        Ok(Page::from_buffer(buf))
    }
}

impl NodeWriter for ShardStore {
    fn append_node(&mut self, bytes: &[u8]) -> Result<PageId> {
        self.nodes_written += 1;
        self.ensure_capacity()?;
        let class = class_for(bytes.len()).ok_or(CodecError::Invariant(
            "index node exceeds the top size class",
        ))?;
        let cell = self.alloc.alloc(class)?;
        self.seg.write_at(cell, bytes).map_err(io_err)?;
        let id = cell_to_page_id(cell)?;
        self.pending_nodes.insert(id, bytes.to_vec());
        Ok(id)
    }
}

impl crate::checkpoint::NodeSpace for ShardStore {
    fn free_node(&mut self, id: PageId, ckpt_seq: u64) {
        self.nodes_freed += 1;
        // Refused if the page does not start a slot — which is how a bad
        // page id fails safely instead of clearing a stranger's bit.
        self.alloc.defer_free(page_id_to_cell(id), ckpt_seq);
    }
}

impl crate::checkpoint::AllocSource for ShardStore {
    fn allocator(&mut self) -> &mut Allocator {
        &mut self.alloc
    }
}

impl crate::checkpoint::ExtentWriter for ShardStore {
    fn write_extent(&mut self, cell: u64, bytes: &[u8]) -> Result<()> {
        self.ensure_capacity()?;
        self.seg.write_at(cell, bytes).map_err(io_err)
    }

    /// Through the mapping, which is `PROT_READ` — reads are zero-copy and
    /// writes never go this way.
    fn read_extent(&self, cell: u64, len: usize) -> Result<Vec<u8>> {
        self.seg.read_at(cell, len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checkpoint::{self, DirtyChunk, ExtentWriter};
    use crate::store::extent::ChunkKey;
    use crate::store::INDEX_NODE;

    fn tmp(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("yesno-store-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_file(&p);
        p
    }

    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// Build one packed page holding `n` chunks under *distinct* keys.
    ///
    /// Returns the page's base cell and a `( key, ChunkRef )` per chunk.
    fn packed_page_of(s: &mut ShardStore, keys: &[u64]) -> (u64, Vec<(ChunkKey, ChunkRef)>) {
        use crate::store::packed::PackedPageBuilder;
        s.alloc.begin_generation();
        let cell = s.alloc.alloc_packed(0).unwrap();
        let mut b = PackedPageBuilder::new(crate::store::PAGE);
        let mut refs = Vec::new();
        let mut total = 0u32;
        for &k in keys {
            let key = ChunkKey::new(k, 0);
            let vals: Vec<u16> = (0..8u16).map(|i| i * 3 + (k as u16)).collect();
            let payload = codec::encode(&Container::from_sorted(&vals));
            let off = b
                .push(key, &payload)
                .unwrap()
                .expect("every chunk must fit one page; shrink the fixture if not");
            total += payload.len() as u32;
            refs.push((
                key,
                ChunkRef::extent(cell + off as u64, ContainerKind::Array, 8).unwrap(),
            ));
        }
        let page = b.seal().unwrap();
        s.alloc.add_packed(cell, total);
        crate::checkpoint::ExtentWriter::write_extent(s, cell, &page).unwrap();
        (cell, refs)
    }

    /// A `ChunkRef` naming an extent that belongs to a **different** key is
    /// refused, with the evidence in the error.
    ///
    /// # Why this test exists at all
    ///
    /// `CodecError::MisPointedExtent` was added on 2026-09-15 to carry the
    /// evidence for a real corruption -- an allocator fault that let two size
    /// classes hand out overlapping cells, so a `ChunkRef` resolved to another
    /// chunk's bytes. It is the diagnostic that names that failure.
    ///
    /// **Nothing had ever produced it.** A branch-coverage audit on 2026-09-16
    /// found this return site at zero across the whole suite, alongside three
    /// other refusal paths. The pattern a consumer observed the same day
    /// explains why: differential tests cover every path that *produces an
    /// answer* exhaustively, so the gaps collect in paths that **refuse** to
    /// produce one, which are covered only if someone thinks of them.
    ///
    /// The trailer is written here exactly as `checkpoint::run` writes it --
    /// tag plus CRC at a fixed distance from the slot end -- because a fixture
    /// that invents its own layout would pass while the real one was broken.
    #[test]
    fn an_extent_belonging_to_another_key_is_refused_with_evidence() {
        use crate::store::extent::{
            ckey_tag, class_for, class_size, ExtTrailer, EXT_TRAILER_BYTES,
        };

        let p = tmp("mispointed-extent");
        let _c = Cleanup(p.clone());
        let mut s = ShardStore::open(&p, [9u8; 16], 0).unwrap();

        let owner = ChunkKey::new(4242, 0);
        let other = ChunkKey::new(7777, 0);
        assert_ne!(ckey_tag(owner), ckey_tag(other), "the tags must differ");

        // A payload too large to pack, so it takes a standalone slot with a
        // trailer of its own.
        let vals: Vec<u16> = (0..1400u16).map(|i| i * 3).collect();
        let c = crate::Container::from_sorted(&vals);
        let payload = crate::container::codec::encode(&c);
        assert!(
            payload.len() > crate::store::extent::PACK_MAX,
            "fixture must produce a standalone extent, not a packed one"
        );

        s.alloc.begin_generation();
        let class = class_for(payload.len()).unwrap();
        let cell = s.alloc.alloc(class).unwrap();
        crate::checkpoint::ExtentWriter::write_extent(&mut s, cell, &payload).unwrap();
        let slot = class_size(class).unwrap() as u64;
        let trailer = ExtTrailer {
            ckey_tag: ckey_tag(owner),
            crc32c: crate::store::checksum::crc32c(&payload),
        };
        crate::checkpoint::ExtentWriter::write_extent(
            &mut s,
            cell + slot - EXT_TRAILER_BYTES as u64,
            &trailer.to_le_bytes(),
        )
        .unwrap();

        let cref = ChunkRef::extent(cell, c.kind(), c.len()).unwrap();

        // The owner reads it back.
        assert!(s.read_container_for(owner, cref).unwrap().is_some());

        // Another key must not, and the error must say enough to locate it.
        match s.read_container_for(other, cref) {
            Err(crate::CodecError::MisPointedExtent {
                key,
                cell: c2,
                class: cl,
                trailer_off,
                found,
                expected,
            }) => {
                assert_eq!(key, other.0);
                assert_eq!(c2, cell);
                assert_eq!(cl, class);
                assert_eq!(trailer_off, cell + slot - EXT_TRAILER_BYTES as u64);
                assert_eq!(found, ckey_tag(owner));
                assert_eq!(expected, ckey_tag(other));
            }
            other_result => panic!("expected MisPointedExtent, got {other_result:?}"),
        }
    }

    /// One packed page holds chunks for several keys, and the read-path checksum
    /// cache verifies it **once for all of them**.
    ///
    /// # What this pins, and why it is not obviously a good thing
    ///
    /// The stored CRC for a packed page covers the whole 4096 bytes, so
    /// `read_container_for` caches one entry per **page**, not per chunk. Reading
    /// a second key whose chunk lives in the same page is then free.
    ///
    /// The same sharing is the cost side tracked as
    /// `packed-page-sharing-may-contend-across-keys`: one entry for many keys
    /// means one write invalidates all of them, which the next test pins.
    /// Neither test says the trade is good; together they say what it is.
    #[test]
    fn one_packed_page_is_verified_once_for_every_key_it_holds() {
        let p = tmp("packed-share-verify");
        let _c = Cleanup(p.clone());
        let mut s = ShardStore::open(&p, [9u8; 16], 0).unwrap();

        let keys = [11u64, 22, 33, 44];
        let (_cell, refs) = packed_page_of(&mut s, &keys);
        assert_eq!(s.segment().verified_count(), 0, "nothing verified yet");

        // Reading the first key verifies the page.
        assert!(s
            .read_container_for(refs[0].0, refs[0].1)
            .unwrap()
            .is_some());
        assert_eq!(
            s.segment().verified_count(),
            1,
            "the first read must verify exactly one region -- the page"
        );

        // Every other key in the same page reads without adding an entry.
        for (key, cref) in &refs[1..] {
            assert!(s.read_container_for(*key, *cref).unwrap().is_some());
        }
        assert_eq!(
            s.segment().verified_count(),
            1,
            "{} keys sharing one page must still be one verified region",
            keys.len()
        );
    }

    /// A write into a packed page invalidates verification for **every key**
    /// whose chunk lives in it, not just the one written.
    ///
    /// # Why this is the test the hypothesis needs
    ///
    /// This is the amplification `packed-page-sharing-may-contend-across-keys`
    /// describes, made observable. A single-key fixture cannot express it: with
    /// one chunk per page, invalidating "the whole page" and invalidating "that
    /// key" are the same event, and the test would pass against an
    /// implementation with no sharing at all. **The fixture must carry several
    /// keys per page for the assertion to mean anything**, which is why
    /// `packed_page_of` takes a list and the count is asserted below.
    #[test]
    fn a_write_into_a_packed_page_invalidates_it_for_every_key_in_it() {
        let p = tmp("packed-share-invalidate");
        let _c = Cleanup(p.clone());
        let mut s = ShardStore::open(&p, [9u8; 16], 0).unwrap();

        let keys = [11u64, 22, 33, 44];
        let (cell, refs) = packed_page_of(&mut s, &keys);

        for (key, cref) in &refs {
            assert!(s.read_container_for(*key, *cref).unwrap().is_some());
        }
        assert_eq!(s.segment().verified_count(), 1, "one page, one entry");

        // A write intersecting the page, **writing the bytes already there**.
        //
        // Content-preserving on purpose. Writing anything else corrupts the
        // page, the re-read fails its checksum, and the assertion below would
        // pass on the error rather than on the re-verification -- which is what
        // the first version of this test did. The subject is invalidation, not
        // corruption; corruption is covered by the read-path refusal tests.
        let tail_off = cell + crate::store::PAGE as u64 - 8;
        let tail = s.segment().read_at(tail_off, 8).unwrap();
        s.segment().write_at(tail_off, &tail).unwrap();
        assert_eq!(
            s.segment().verified_count(),
            0,
            "a write intersecting the page must drop its verification"
        );

        // The cost lands on a key that was never written: reading key[1] must
        // succeed *and* re-verify the whole page.
        assert!(
            s.read_container_for(refs[1].0, refs[1].1)
                .unwrap()
                .is_some(),
            "an untouched key must still read after an unrelated write to its page"
        );
        assert_eq!(
            s.segment().verified_count(),
            1,
            "reading an untouched key must re-verify the whole shared page"
        );
    }

    /// A packed reference pointing into its page's header must be refused.
    ///
    /// Packed chunks carry no per-chunk trailer: the design's position is that
    /// the page header's CRC and its `[first, last]` `ChunkKey` range serve
    /// both roles. But `may_contain` is a **range** check — it says the page
    /// could hold this key, not that the offset is where that key lives — so a
    /// reference below the payload region passed it and decoded the header
    /// bytes as a container.
    ///
    /// The check costs nothing: the page base is already computed in order to
    /// read the header. It replaces `store::packed::payload_at`, which
    /// performed the same validation for no caller and could not be wired in —
    /// it takes a `&[u8]` page, so using it on the read path would copy 4 KiB
    /// per chunk and give up the zero-copy property the store is built on.
    #[test]
    fn a_packed_reference_into_the_page_header_is_refused() {
        use crate::store::packed::{PackedPageBuilder, HEADER};

        let p = tmp("packed-header-ref");
        let _c = Cleanup(p.clone());
        let mut s = ShardStore::open(&p, [9u8; 16], 0).unwrap();

        // One packed page holding one small array, written where a checkpoint
        // would put it.
        s.alloc.begin_generation();
        let cell = s.alloc.alloc_packed(0).unwrap();
        let key = ChunkKey::new(4, 0);
        let payload = codec::encode(&Container::from_sorted(&[1, 2, 3, 4, 5]));
        let mut b = PackedPageBuilder::new(crate::store::PAGE);
        let off = b.push(key, &payload).unwrap().unwrap();
        let page = b.seal().unwrap();
        s.alloc.add_packed(cell, payload.len() as u32);
        crate::checkpoint::ExtentWriter::write_extent(&mut s, cell, &page).unwrap();

        let good = ChunkRef::extent(cell + off as u64, ContainerKind::Array, 5).unwrap();
        assert!(
            s.read_container_for(key, good).unwrap().is_some(),
            "the genuine reference must read"
        );

        // Aimed inside the header, which `may_contain` cannot see.
        let bad = ChunkRef::extent(cell + 8, ContainerKind::Array, 5).unwrap();
        assert!(
            (8u64) < HEADER as u64,
            "the fixture must aim inside the header"
        );
        let err = s
            .read_container_for(key, bad)
            .expect_err("a reference inside the page header must be refused");
        assert!(
            format!("{err:?}").contains("header"),
            "expected the header check to fire, got {err:?}"
        );
    }

    /// The read path must refuse format bits it does not understand.
    ///
    /// `ChunkRef::validate` existed and was called **only from `fsck`**, so a
    /// reference carrying an unassigned `kind` or a set reserved bit was
    /// refused by a tool nobody runs on the path it guards, and acted on by the
    /// path that actually decodes the payload. Kind 3 is the interesting case:
    /// `ChunkRef::kind` resolves it to `Array` through a wildcard arm, so a file
    /// written by a future format would have been read as corruption-free array
    /// data rather than refused.
    #[test]
    fn the_read_path_refuses_a_reference_from_a_newer_format() {
        use crate::store::packed::PackedPageBuilder;

        let p = tmp("kind3");
        let _c = Cleanup(p.clone());
        let mut s = ShardStore::open(&p, [9u8; 16], 0).unwrap();

        // A packed page, so `read_container_for`'s own checks pass and the only
        // thing that can refuse the altered references is `validate`.
        s.alloc.begin_generation();
        let cell = s.alloc.alloc_packed(0).unwrap();
        let key = ChunkKey::new(7, 0);
        let c = Container::from_sorted(&[1, 2, 3, 4, 5, 6, 7, 8]);
        let payload = codec::encode(&c);
        let mut b = PackedPageBuilder::new(crate::store::PAGE);
        let off = b.push(key, &payload).unwrap().unwrap();
        let page = b.seal().unwrap();
        s.alloc.add_packed(cell, payload.len() as u32);
        crate::checkpoint::ExtentWriter::write_extent(&mut s, cell, &page).unwrap();

        let good = ChunkRef::extent(cell + off as u64, ContainerKind::Array, c.len()).unwrap();
        assert!(
            s.read_container_for(key, good).unwrap().is_some(),
            "the genuine reference must read"
        );

        for (name, bits) in [
            ("kind 3", good.to_bits() | (1 << 57) | (1 << 56)),
            ("enc = 1", good.to_bits() | (1 << 59)),
            ("reserved bit 62", good.to_bits() | (1 << 62)),
        ] {
            let bad = ChunkRef::from_bits(bits);
            assert!(
                s.read_container_for(key, bad).is_err(),
                "{name} must be refused by the read path, not only by fsck"
            );
        }
    }

    /// A half-built checkpoint's node buffer must be discardable.
    ///
    /// **This tests the function, not the wiring, and that distinction is the
    /// honest part.** `abandon_checkpoint` had no caller at all until the
    /// unwired-`pub fn` sweep found it; it is now called on both error paths out
    /// of `Db::checkpoint`. Those paths are *not* covered by a failing test,
    /// because `Db::checkpoint` has no fault-injection seam — `checkpoint::run`
    /// and `commit_superblock` take the concrete `ShardStore`, so a test cannot
    /// make either fail after nodes are written. Recorded as a gap rather than
    /// papered over; see JOURNAL 2026-08-27.
    #[test]
    fn abandoning_a_checkpoint_discards_its_buffered_nodes() {
        use crate::index::tree::NodeWriter;

        let p = tmp("abandon");
        let _c = Cleanup(p.clone());
        let mut s = ShardStore::open(&p, [7u8; 16], 0).unwrap();
        s.alloc.begin_generation();

        assert_eq!(s.pending_node_count(), 0, "nothing in flight yet");
        let node = vec![0u8; crate::store::INDEX_NODE];
        for _ in 0..3 {
            s.append_node(&node).unwrap();
        }
        assert_eq!(
            s.pending_node_count(),
            3,
            "the bottom-up build reads these back before any superblock names them"
        );

        s.abandon_checkpoint();
        assert_eq!(
            s.pending_node_count(),
            0,
            "a failed attempt must not retain them"
        );
    }

    #[test]
    fn a_fresh_shard_gets_an_initial_superblock() {
        let p = tmp("fresh");
        let _c = Cleanup(p.clone());
        let s = ShardStore::open(&p, [1u8; 16], 3).unwrap();
        assert_eq!(s.superblock().shard_id, 3);
        assert_eq!(s.superblock().checkpoint_cv, 0);
        assert!(s.superblock().root.is_none(), "nothing checkpointed yet");
        assert!(s.tree().is_none());
    }

    #[test]
    fn reopening_recovers_the_superblock() {
        let p = tmp("reopen");
        let _c = Cleanup(p.clone());
        {
            let mut s = ShardStore::open(&p, [2u8; 16], 1).unwrap();
            let mut sb = s.superblock().clone();
            sb.seq += 1;
            sb.checkpoint_cv = 42;
            s.commit_superblock(sb).unwrap();
        }
        let s = ShardStore::open(&p, [2u8; 16], 1).unwrap();
        assert_eq!(
            s.superblock().checkpoint_cv,
            42,
            "the flip survived the reopen"
        );
        assert_eq!(s.superblock().db_uuid, [2u8; 16]);
    }

    #[test]
    fn the_flip_alternates_slots() {
        let p = tmp("alternate");
        let _c = Cleanup(p.clone());
        let mut s = ShardStore::open(&p, [3u8; 16], 0).unwrap();

        let mut seqs = Vec::new();
        for cv in 1..=5u64 {
            let mut sb = s.superblock().clone();
            sb.seq += 1;
            sb.checkpoint_cv = cv;
            seqs.push(sb.seq);
            s.commit_superblock(sb).unwrap();
        }
        // Reopening must find the newest.
        drop(s);
        let s = ShardStore::open(&p, [3u8; 16], 0).unwrap();
        assert_eq!(s.superblock().checkpoint_cv, 5);
        assert_eq!(s.superblock().seq, *seqs.last().unwrap());
    }

    #[test]
    fn extents_round_trip_through_the_file() {
        let p = tmp("extents");
        let _c = Cleanup(p.clone());
        let mut s = ShardStore::open(&p, [4u8; 16], 0).unwrap();
        s.allocator().begin_generation();

        let vals: Vec<u16> = (0..300u16).map(|i| i * 7).collect();
        let c = Container::from_sorted(&vals);
        let payload = codec::encode(&c);
        let class = class_for(payload.len()).unwrap();
        let cell = s.allocator().alloc(class).unwrap();
        s.write_extent(cell, &payload).unwrap();

        let cref = ChunkRef::extent(cell, c.kind(), c.len()).unwrap();
        let back = s.read_container(cref).unwrap().unwrap();
        assert_eq!(back.iter().collect::<Vec<_>>(), vals);
    }

    #[test]
    fn a_run_container_reads_back_using_its_own_length_prefix() {
        // The one kind whose payload length is not derivable from the reference.
        let p = tmp("run");
        let _c = Cleanup(p.clone());
        let mut s = ShardStore::open(&p, [5u8; 16], 0).unwrap();
        s.allocator().begin_generation();

        let mut c = Container::from_sorted(&(0..3000u16).collect::<Vec<_>>());
        c.optimize();
        assert_eq!(c.kind(), ContainerKind::Run);

        let payload = codec::encode(&c);
        let class = class_for(payload.len()).unwrap();
        let cell = s.allocator().alloc(class).unwrap();
        s.write_extent(cell, &payload).unwrap();

        let cref = ChunkRef::extent(cell, ContainerKind::Run, c.len()).unwrap();
        let back = s.read_container(cref).unwrap().unwrap();
        assert_eq!(back.len(), 3000);
        assert_eq!(
            back.iter().collect::<Vec<_>>(),
            (0..3000u16).collect::<Vec<_>>()
        );
    }

    /// The store's module doc claims reads are zero-copy; this checks it.
    #[test]
    fn a_bitmap_read_aliases_the_mapping_rather_than_copying() {
        let p = tmp("zerocopy");
        let _c = Cleanup(p.clone());
        let mut s = ShardStore::open(&p, [11u8; 16], 0).unwrap();
        s.allocator().begin_generation();

        // Scattered, so it stays a bitmap.
        let scattered: Vec<u16> = (0..5000u16).map(|i| i * 2).collect();
        let c = Container::from_sorted(&scattered);
        assert_eq!(c.kind(), ContainerKind::Bitmap);
        let payload = codec::encode(&c);
        let class = class_for(payload.len()).unwrap();
        let cell = s.allocator().alloc(class).unwrap();
        s.write_extent(cell, &payload).unwrap();

        let cref = ChunkRef::extent(cell, ContainerKind::Bitmap, c.len()).unwrap();
        let a = s.read_container(cref).unwrap().unwrap();
        let b = s.read_container(cref).unwrap().unwrap();

        // Two independent reads of the same extent must share bytes. If the read
        // path copied, these would be distinct allocations.
        let pa = crate::unstable_arrow::bitmap_mask(&a).unwrap();
        let pb = crate::unstable_arrow::bitmap_mask(&b).unwrap();
        assert_eq!(
            pa.values().as_ptr(),
            pb.values().as_ptr(),
            "reads are copying; the zero-copy path is not wired"
        );
        assert_eq!(a.iter().collect::<Vec<_>>(), scattered);
    }

    #[test]
    fn a_container_outlives_the_store_it_came_from() {
        // The ExtentGuard property, exercised through the real read path.
        let p = tmp("outlive-store");
        let _c = Cleanup(p.clone());
        let scattered: Vec<u16> = (0..5000u16).map(|i| i * 2).collect();

        let held = {
            let mut s = ShardStore::open(&p, [12u8; 16], 0).unwrap();
            s.allocator().begin_generation();
            let c = Container::from_sorted(&scattered);
            let payload = codec::encode(&c);
            let class = class_for(payload.len()).unwrap();
            let cell = s.allocator().alloc(class).unwrap();
            s.write_extent(cell, &payload).unwrap();
            let cref = ChunkRef::extent(cell, ContainerKind::Bitmap, c.len()).unwrap();
            s.read_container(cref).unwrap().unwrap()
        }; // store dropped, mapping must survive via the guard

        assert_eq!(held.iter().collect::<Vec<_>>(), scattered);
    }

    #[test]
    fn an_inline_reference_needs_no_file_access() {
        let p = tmp("inline");
        let _c = Cleanup(p.clone());
        let s = ShardStore::open(&p, [6u8; 16], 0).unwrap();
        let cref = ChunkRef::inline(&[3, 9, 27]).unwrap();
        let c = s.read_container(cref).unwrap().unwrap();
        assert_eq!(c.iter().collect::<Vec<_>>(), vec![3, 9, 27]);
    }

    #[test]
    fn a_checkpoint_writes_an_index_readable_within_the_same_checkpoint() {
        let p = tmp("checkpoint");
        let _c = Cleanup(p.clone());
        let mut s = ShardStore::open(&p, [7u8; 16], 0).unwrap();

        let dirty: Vec<DirtyChunk> = (0..200u64)
            .map(|i| DirtyChunk {
                key: ChunkKey::new(1, i),
                container: Container::from_sorted(&[1, 3, 5, 7, 9]),
                version: 1,
                previous: None,
            })
            .collect();

        let prev = s.superblock().clone();
        let mut alloc = std::mem::take(s.allocator());
        let res = {
            // The checkpointer needs the store for extents and nodes at once, so
            // run it against a detached allocator and hand the results back.
            let mut nodes = crate::index::tree::VecNodes::new();
            let mut ext = Vec::<(u64, Vec<u8>)>::new();
            struct Cap<'a>(&'a mut Vec<(u64, Vec<u8>)>);
            impl ExtentWriter for Cap<'_> {
                fn write_extent(&mut self, cell: u64, bytes: &[u8]) -> Result<()> {
                    self.0.push((cell, bytes.to_vec()));
                    Ok(())
                }

                /// The chunks here have no `previous`, so nothing is ever
                /// superseded and this is unreachable. It errors rather than
                /// returning zeroes: a silent zero here is exactly the bug the
                /// dependent read exists to fix.
                fn read_extent(&self, _cell: u64, _len: usize) -> Result<Vec<u8>> {
                    Err(crate::CodecError::Invariant(
                        "this test sink never supersedes an extent",
                    ))
                }
            }
            let mut cap = Cap(&mut ext);
            let mut sink = checkpoint::SplitSink {
                extents: &mut cap,
                nodes: &mut nodes,
                alloc: &mut alloc,
            };
            let r = checkpoint::run(
                1,
                dirty,
                &Default::default(),
                None,
                &mut sink,
                INDEX_NODE,
                &prev,
                1,
                0,
                0,
            )
            .unwrap();
            // The index must resolve every chunk it just wrote.
            let t = Tree {
                root: r.superblock.root.unwrap().0,
                height: r.superblock.root.unwrap().1,
                node_size: INDEX_NODE,
            };
            for i in 0..200u64 {
                assert!(
                    t.get(&nodes, ChunkKey::new(1, i)).unwrap().is_some(),
                    "chunk {i} missing from the fresh index"
                );
            }
            r
        };
        *s.allocator() = alloc;
        assert_eq!(res.chunks_written, 200);
        assert_eq!(res.inlined, 0, "5 values exceeds the inline capacity");
    }

    #[test]
    fn the_file_grows_but_never_shrinks() {
        let p = tmp("grow");
        let _c = Cleanup(p.clone());
        let mut s = ShardStore::open(&p, [8u8; 16], 0).unwrap();
        let initial = s.seg.file_len().unwrap();
        assert!(initial >= RESERVED + SLAB_SIZE);

        s.allocator().begin_generation();
        for _ in 0..100 {
            let cell = s.allocator().alloc(10).unwrap();
            s.write_extent(cell, &[0u8; 8192]).unwrap();
        }
        let grown = s.seg.file_len().unwrap();
        assert!(grown >= initial);

        drop(s);
        let s = ShardStore::open(&p, [8u8; 16], 0).unwrap();
        assert_eq!(
            s.seg.file_len().unwrap(),
            grown,
            "I6: reopening must not shrink"
        );
    }

    #[test]
    fn a_corrupt_slot_falls_back_to_the_other() {
        let p = tmp("corrupt");
        let _c = Cleanup(p.clone());
        {
            let mut s = ShardStore::open(&p, [9u8; 16], 0).unwrap();
            let mut sb = s.superblock().clone();
            sb.seq += 1;
            sb.checkpoint_cv = 11;
            s.commit_superblock(sb).unwrap();
        }
        // Corrupt whichever slot is newer.
        {
            use std::io::{Read, Seek, SeekFrom, Write};
            let mut f = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&p)
                .unwrap();
            let mut a = vec![0u8; PAGE];
            let mut b = vec![0u8; PAGE];
            f.read_exact(&mut a).unwrap();
            f.read_exact(&mut b).unwrap();
            let sa = SuperBlock::decode(&a).unwrap();
            let sb = SuperBlock::decode(&b).unwrap();
            let newer_is_b = match (&sa, &sb) {
                (Some(x), Some(y)) => y.seq > x.seq,
                (None, Some(_)) => true,
                _ => false,
            };
            let off = if newer_is_b { PAGE as u64 } else { 0 };
            f.seek(SeekFrom::Start(off + 100)).unwrap();
            f.write_all(&[0xFF; 16]).unwrap();
        }
        // Opening must still succeed, on the intact older slot.
        let s = ShardStore::open(&p, [9u8; 16], 0).unwrap();
        assert!(s.superblock().checkpoint_cv <= 11);
    }
}
