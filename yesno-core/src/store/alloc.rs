//! Slab and extent allocation.
//!
//! # Generational allocation
//!
//! Grouping is by **size class**, not by container kind, so a key whose chunks
//! vary in density scatters across one slab per class it uses. Under 2 MiB
//! folios each of those is a separate fault, so the scattering costs real I/O.
//!
//! The mitigation is that **each checkpoint claims a contiguous run of slabs**
//! and bump-allocates all classes within that run. Data written together is read
//! together — and during bulk load every chunk of a key is dirty in the same
//! checkpoint — so a key's chunks land in a small file window rather than
//! scattered across the whole shard.
//!
//! This splits the allocation policy in two, and the split is the point:
//!
//! - **New writes** bump within the current generation. Locality first — and a
//!   generation does *not* abandon a partly-filled slab when the next one
//!   begins; see [`Allocator::begin_generation`] for the measurement that
//!   settled that.
//! - **Occupancy-based reuse** ( "fewest-free partial slab first" ) is reserved
//!   for the compactor, which relocates in `ChunkKey` order into a fresh
//!   generation.
//!
//! Incremental updates rewrite only some of a key's chunks, so a key fragments
//! across generations over time and evacuation is what re-clusters it. That
//! makes compaction a *locality* mechanism, not merely space reclamation.
//!
//! # I3: allocation happens only at checkpoint
//!
//! Nothing here is called from the write path. That is what lets the deferred
//! free list be pure in-memory state: extents allocated since the last
//! checkpoint are unreachable from any durable root, so a crash simply loses
//! them. Weakening I3 breaks that argument.
//!
//! The rest of that argument used to read "and on restart there are no
//! readers, so the checkpointed free bitmaps already describe exactly what is
//! reclaimable. No durable structure, no orphan scan, no leak." **The bitmaps
//! do not.** An extent superseded but not yet past the three reclamation
//! conditions is queued nowhere durable while its slot is persisted as *used*,
//! so a reopen orphaned it permanently — once per restart. The claim is true of
//! the **index**, not of the bitmaps, which is why open walks the index and
//! adopts the result. See [`Allocator::adopt_live_at_open`].

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use super::extent::{class_size, CLASS_SIZES, PACKED_CLASS};
use crate::error::{CodecError, Result};
use crate::store::{SLAB_BODY, SLAB_META, SLAB_SIZE};

/// Objects a slab of `class` holds.
#[inline]
pub fn slab_capacity(class: u8) -> u32 {
    match class_size(class) {
        Some(sz) if sz > 0 => (SLAB_BODY / sz as u64) as u32,
        _ => 0,
    }
}

/// Byte offset of a slot within the shard address space.
#[inline]
pub fn slot_offset(slab_id: u32, class: u8, slot: u32) -> u64 {
    slab_id as u64 * SLAB_SIZE + SLAB_META + slot as u64 * class_size(class).unwrap_or(0) as u64
}

/// Which slab a byte offset falls in.
#[inline]
pub fn slab_of(cell: u64) -> u32 {
    (cell / SLAB_SIZE) as u32
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlabState {
    Free,
    /// In use for `class`, with `gen` recording the checkpoint that claimed it.
    InUse {
        class: u8,
        gen: u32,
    },
    /// Existed before this process opened the shard, contents unknown.
    ///
    /// Slab classes are not persisted yet ( the `SLAB_META` region is reserved
    /// and unwritten ), so a reopened shard cannot tell which slots in an
    /// existing slab are live. It must therefore treat them all as live:
    /// allocating into one would write over extents the current root still
    /// points at.
    ///
    /// Do not let a compactor treat this as reusable. It means "unknown",
    /// not "free" — the distinction is the whole point of the variant.
    Opaque,
}

/// One slab's occupancy.
#[derive(Clone, Debug)]
pub struct Slab {
    pub state: SlabState,
    /// One bit per slot; set means allocated.
    used: Vec<u64>,
    used_count: u32,
    capacity: u32,
}

impl Slab {
    /// Slab 0: exists, never allocated into. See `new_slab_for`.
    fn reserved() -> Self {
        Slab {
            state: SlabState::Opaque,
            used: Vec::new(),
            used_count: 0,
            capacity: 0,
        }
    }

    /// A slab that exists on disk but whose occupancy is unknown.
    ///
    /// Capacity zero, so `first_free` never offers a slot even if some future
    /// code makes it active.
    fn opaque() -> Self {
        Slab {
            state: SlabState::Opaque,
            used: Vec::new(),
            used_count: 0,
            capacity: 0,
        }
    }

    /// A slab with no live slots and no class yet.
    pub(crate) fn free() -> Self {
        Slab {
            state: SlabState::Free,
            used: Vec::new(),
            used_count: 0,
            capacity: 0,
        }
    }

    /// Rebuild from persisted metadata. Callers must have validated the inputs
    /// against each other; see `slabmeta::decode`.
    pub(crate) fn restored(
        class: u8,
        gen: u32,
        used: Vec<u64>,
        used_count: u32,
        capacity: u32,
    ) -> Self {
        Slab {
            state: SlabState::InUse { class, gen },
            used,
            used_count,
            capacity,
        }
    }

    /// The raw occupancy words, for persistence.
    #[inline]
    pub(crate) fn used_words(&self) -> &[u64] {
        &self.used
    }

    fn new(class: u8, gen: u32) -> Self {
        let capacity = slab_capacity(class);
        Slab {
            state: SlabState::InUse { class, gen },
            used: vec![0u64; (capacity as usize).div_ceil(64)],
            used_count: 0,
            capacity,
        }
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(class: u8, gen: u32) -> Self {
        Slab::new(class, gen)
    }

    #[cfg(test)]
    pub(crate) fn opaque_for_test() -> Self {
        Slab::opaque()
    }

    #[cfg(test)]
    pub(crate) fn set_for_test(&mut self, slot: u32) {
        self.set(slot);
    }

    #[cfg(test)]
    pub(crate) fn is_set_for_test(&self, slot: u32) -> bool {
        self.used
            .get(slot as usize / 64)
            .is_some_and(|w| w & (1u64 << (slot % 64)) != 0)
    }

    #[inline]
    pub fn capacity(&self) -> u32 {
        self.capacity
    }

    #[inline]
    pub fn used_count(&self) -> u32 {
        self.used_count
    }

    /// Fraction of slots in use. Drives the compactor's evacuation trigger.
    #[inline]
    pub fn live_fraction(&self) -> f64 {
        if self.capacity == 0 {
            return 1.0;
        }
        self.used_count as f64 / self.capacity as f64
    }

    /// Whether `slot` is allocated.
    ///
    /// Public because allocator rebuild needs it: `fsck` walks the index,
    /// marking every referenced slot, then compares against this to find leaked
    /// slots. That index-scan rebuild is the only path that produces a correct
    /// answer, since under I2 a superseded extent's bytes remain perfectly
    /// well-formed and cannot be distinguished from a live one locally.
    #[inline]
    pub fn is_set(&self, slot: u32) -> bool {
        match self.used.get((slot / 64) as usize) {
            Some(w) => w & (1u64 << (slot % 64)) != 0,
            None => false,
        }
    }

    fn set(&mut self, slot: u32) -> bool {
        let w = &mut self.used[(slot / 64) as usize];
        let m = 1u64 << (slot % 64);
        let was = *w & m != 0;
        *w |= m;
        self.used_count += !was as u32;
        !was
    }

    fn clear(&mut self, slot: u32) -> bool {
        let w = &mut self.used[(slot / 64) as usize];
        let m = 1u64 << (slot % 64);
        let was = *w & m != 0;
        *w &= !m;
        self.used_count -= was as u32;
        was
    }

    /// Lowest free slot, or `None` when full.
    fn first_free(&self) -> Option<u32> {
        for (i, &w) in self.used.iter().enumerate() {
            if w != !0u64 {
                let slot = i as u32 * 64 + (!w).trailing_zeros();
                if slot < self.capacity {
                    return Some(slot);
                }
            }
        }
        None
    }
}

/// An extent awaiting reclamation.
///
/// Reclaiming requires **all three** conditions, and none subsumes the others:
///
/// 1. `safe_version > obsolete_at` — no active snapshot can still reach it.
///
///    **Strict, and it has to be.** The tempting reading is that a reader at
///    version `V == obsolete_at` sees the *new* copy — which is true of what it
///    reads through the current root, and irrelevant. `Snapshot` captures
///    `roots` at creation, so a snapshot taken just before the superseding
///    checkpoint has `V == W` *and* pins the older root, which still points at
///    the old extent. Version alone cannot tell those two snapshots apart, so
///    the gate blocks until every reader at `W` or below is gone.
///
///    Loosening this to `>=` was tried on 2026-08-25 and is unsound. It does
///    not fail loudly: the generational allocator rarely re-hands-out a single
///    freed slot, so the corruption stays latent until the slab empties and is
///    recycled whole. See `stale-root-blocks-idle-reclamation` in JOURNAL.md for
///    the *correct* way to sharpen this ( gate on the oldest pinned root, not
///    on versions ).
/// 2. `checkpoint_seq >= reclaim_after_ckpt` — the A/B superblock rule; slot A
///    still names the previous root, so an extent freed by checkpoint N cannot
///    be reused until N+2 is durable. Same two-transaction delay as LMDB.
/// 3. no live `arrow_buffer::Buffer` points into it ( enforced at the segment
///    layer by an `Arc` refcount ).
///
/// Condition 1 covers a snapshot *entitled* to an extent it has not read yet;
/// condition 3 covers a `Buffer` that escaped into a `RecordBatch` outliving its
/// snapshot. Dropping either one is a use-after-free.
#[derive(Clone, Copy, Debug)]
pub struct Pending {
    pub cell: u64,
    pub class: u8,
    pub reclaim_after_ckpt: u64,
    /// The checkpoint that superseded this extent.
    ///
    /// **This is the reachability question that `obsolete_at` only proxies
    /// for.** An extent superseded by checkpoint `k` is reachable from the roots
    /// of checkpoints `< k` and from no others, so it is free exactly when no
    /// live reader holds a root from below `k`. `obsolete_at` is a *version*,
    /// and a version does not determine which root a reader captured: the two
    /// are read at different moments in `Db::snapshot`, so a reader can hold a
    /// version above `obsolete_at` and a root from before `k`. See
    /// `stale-root-blocks-idle-reclamation` in `JOURNAL.md`.
    pub obsolete_ckpt: u64,
}

/// Deferred-free delay in checkpoints, from the A/B superblock double buffer.
pub const RECLAIM_CKPT_DELAY: u64 = 2;

/// Evacuate a slab once live occupancy falls below this.
///
/// `spaceAmp = ln(1/C)/(1-C)` and `writeAmp = 1/(1-C)`, both independent of slab
/// and page size. At 0.40 that is 1.53x space for 1.67x writes. The curve is
/// sharp on the high side and flat on the low side ( 0.80 buys 27% space for 3x
/// the writes ), so err low. Product-optimal is 0.285.
pub const COMPACT_LIVE_FRACTION: f64 = 0.40;

/// Slab and extent allocator.
pub struct Allocator {
    slabs: Vec<Slab>,
    /// Bump slab per class within the current generation.
    active: [Option<u32>; CLASS_SIZES.len()],
    generation: u32,
    deferred: VecDeque<Pending>,
    freed_total: u64,
    /// Live payload bytes per packed page, keyed by the page's cell.
    ///
    /// Deliberately in RAM and never in the page: decrementing a counter inside
    /// a published, mapped page is an in-place write that live snapshots could
    /// observe through an aliasing `Buffer`, which is undefined behaviour. This
    /// needs no durability, by the same I3+I4 argument that makes the deferred
    /// free list non-durable, and an index scan recomputes it exactly.
    packed_live: std::collections::HashMap<u64, u32>,
}

impl Default for Allocator {
    fn default() -> Self {
        Self::new()
    }
}

impl Allocator {
    pub fn new() -> Self {
        Allocator {
            slabs: Vec::new(),
            active: [None; CLASS_SIZES.len()],
            generation: 0,
            deferred: VecDeque::new(),
            packed_live: std::collections::HashMap::new(),
            freed_total: 0,
        }
    }

    /// Extents freed over this allocator's life.
    ///
    /// `deferred_count` cannot answer "is reclamation working?": the
    /// carry-forward rewrites every chunk on every checkpoint, so each one
    /// supersedes the whole dataset and the queue is steady-state non-empty by
    /// construction. Only a cumulative count distinguishes a working reclaimer
    /// from an inert one, and an inert one passes every correctness test.
    #[inline]
    pub fn freed_total(&self) -> u64 {
        self.freed_total
    }

    /// An allocator for a shard that already has `n_slabs` slabs on disk.
    ///
    /// Reopening used to start from [`Allocator::new`], with **no slabs at
    /// all**. `new_slab_for` pushes at `slabs.len()`, so the first allocation
    /// after a reopen claimed slab 0 — and wrote straight over the extents
    /// already living there. It went unnoticed because every checkpoint carries
    /// forward and rewrites the entire dataset, so everything relocated together
    /// and stayed self-consistent; only a container held from *before* the
    /// rewrite could see it, and nothing held one until `zero_copy_mvcc`.
    ///
    /// The old slabs are [`SlabState::Opaque`]: never allocated into, so their
    /// free space is not reclaimed until slab metadata is persisted and the
    /// allocator can be rebuilt from the index. That is the conservative
    /// direction — it leaks space rather than data.
    /// Rebuild from persisted per-slab occupancy.
    ///
    /// `None` for a slab means its metadata was absent or torn. Those stay
    /// [`SlabState::Opaque`] — never allocated into — so a partial read degrades
    /// to leaked space rather than to reuse of live extents.
    pub fn restore(slabs: Vec<Option<Slab>>) -> Self {
        let mut a = Allocator::new();
        a.slabs = slabs
            .into_iter()
            .map(|s| s.unwrap_or_else(Slab::opaque))
            .collect();
        a
    }

    /// All slabs, for persistence.
    #[inline]
    pub fn slabs(&self) -> &[Slab] {
        &self.slabs
    }

    #[inline]
    pub fn generation(&self) -> u32 {
        self.generation
    }

    #[inline]
    pub fn slab_count(&self) -> usize {
        self.slabs.len()
    }

    pub fn slab(&self, id: u32) -> Option<&Slab> {
        self.slabs.get(id as usize)
    }

    /// Begin a new generation. Subsequent allocations bump into a fresh,
    /// contiguous run of slabs rather than reusing scattered free space.
    pub fn begin_generation(&mut self) {
        self.generation += 1;
        // The active slabs are deliberately **kept**.
        //
        // Clearing them here — starting every checkpoint on fresh slabs — is
        // what the generational rationale above reads as implying, and it costs
        // one 2 MiB slab per class per checkpoint no matter how little that
        // checkpoint wrote. Measured on a 200-key corpus over 60 checkpoints of
        // churn, that was **12-15x space amplification** against the fresh
        // state, and it is unrecoverable: compaction relocates a sparse slab's
        // chunks into the *active* slab, which the next checkpoint then abandons
        // in turn, so evacuating harder made it strictly worse
        // ( 61 slabs at 2 per checkpoint, 70 at 16 ).
        //
        // Keeping them costs the stated goal nothing. That goal is about bulk
        // load — "during bulk load every chunk of a key is dirty in the same
        // checkpoint" — and a bulk load is one checkpoint, which still claims a
        // contiguous run and bump-allocates every class within it, unchanged.
        // Abandonment only ever affected the *second and later* checkpoints,
        // where writes are small and packing them densely is better locality,
        // not worse. `alloc` already opens a new slab when the current one
        // fills, which is the only time a fresh one is actually needed.
        //
        // Same corpus after this change: 1.00x / 1.40x / 1.60x.
    }

    /// Open a slab for `class`, recycling an emptied one before extending.
    ///
    /// # Why recycling a whole slab is safe, and slot scavenging is not
    ///
    /// A slab reaches [`SlabState::Free`] only when its **last** slot is
    /// released by `free_now`, and every one of those releases had to pass all
    /// three reclamation conditions: no snapshot entitled to it, the A/B
    /// superblock cycled twice, and no live Arrow `Buffer` pointing into it. So
    /// an emptied slab is unreachable in its entirety, and handing it to a new
    /// class returns its 2 MiB to the file rather than growing the file.
    ///
    /// This deliberately does **not** scavenge free slots out of partially used
    /// slabs. That is the compactor's job, and mixing it into the write path
    /// would trade away the locality the generational design exists for:
    /// bump-allocating within a fresh slab is what keeps a key's chunks in one
    /// file window. Recycling a whole slab preserves that — it is still one
    /// slab handed to one class for one generation, only the storage is reused.
    ///
    /// The scan is linear in slab count. It runs once per slab exhausted, not
    /// per allocation, so it is far off the hot path.
    fn new_slab_for(&mut self, class: u8) -> u32 {
        if let Some(id) = self
            .slabs
            .iter()
            .position(|s| matches!(s.state, SlabState::Free))
        {
            // Re-initialize: a slab emptied by `free_now` keeps the geometry of
            // its previous class, and `Slab::free()` from restored metadata has
            // no geometry at all.
            self.slabs[id] = Slab::new(class, self.generation);
            self.active[class as usize] = Some(id as u32);
            return id as u32;
        }
        // Slab 0 is reserved and never allocated into.
        //
        // Every slab's first `SLAB_META` bytes hold its occupancy — except slab
        // 0, whose region *is* the two superblock slots, so it has nowhere to
        // record anything. A reopened shard therefore restores it as `Opaque`,
        // and `owning_class` returns `None` for everything in it, which silently
        // disables **reclamation** and **identity verification** for any chunk
        // that landed there. A database small enough to fit in slab 0 got
        // neither, and the failure is invisible: nothing errors, the machinery
        // simply does not run.
        //
        // That rule cost three separate debugging sessions, each ending at the
        // same discovery. Reserving the slab removes the special case rather
        // than documenting it again.
        //
        // The 2 MiB is **virtual**: `grow_to` extends with `set_len`, so an
        // untouched slab body is a hole and costs no blocks.
        if self.slabs.is_empty() {
            self.slabs.push(Slab::reserved());
        }
        let id = self.slabs.len() as u32;
        self.slabs.push(Slab::new(class, self.generation));
        self.active[class as usize] = Some(id);
        id
    }

    /// Allocate one slot of `class`, returning its byte offset.
    ///
    /// Bump-allocates within the current generation. Never scavenges partially
    /// free slabs from older generations — that is the compactor's job.
    pub fn alloc(&mut self, class: u8) -> Result<u64> {
        if class_size(class).is_none() {
            return Err(CodecError::Invariant("unknown size class"));
        }
        loop {
            let id = match self.active[class as usize] {
                Some(id) => id,
                None => self.new_slab_for(class),
            };
            let slab = &mut self.slabs[id as usize];
            match slab.first_free() {
                Some(slot) => {
                    slab.set(slot);
                    return Ok(slot_offset(id, class, slot));
                }
                None => {
                    // Current bump slab is full; open the next one.
                    self.active[class as usize] = None;
                }
            }
        }
    }

    /// Allocate a packed page and register its live-byte accounting.
    pub fn alloc_packed(&mut self, payload_bytes: u32) -> Result<u64> {
        let cell = self.alloc(PACKED_CLASS)?;
        self.packed_live.insert(cell, payload_bytes);
        Ok(cell)
    }

    /// Record bytes added to a packed page.
    ///
    /// `alloc_packed` opens a page at zero live bytes, so without this the
    /// accounting starts wrong and stays wrong — which is how packed pages came
    /// to be allocated, never accounted, and never freed.
    pub fn add_packed(&mut self, page_cell: u64, bytes: u32) {
        *self.packed_live.entry(page_cell).or_insert(0) += bytes;
    }

    /// Is `cell` inside a packed page?
    ///
    /// A packed chunk owns no slot of its own, so `owning_class` refuses it and
    /// the caller needs this to tell "packed, handle differently" from "not
    /// freeable at all".
    pub fn is_packed_cell(&self, cell: u64) -> bool {
        let id = slab_of(cell);
        matches!(
            self.slabs.get(id as usize).map(|s| s.state),
            Some(SlabState::InUse { class, .. }) if class == PACKED_CLASS
        )
    }

    /// Note that a packed chunk is superseded, and queue its **page** once every
    /// chunk in it is dead.
    ///
    /// A packed page is the reclamation unit: individual payloads inside it own
    /// no slot, so they can only be returned all at once. Returns whether the
    /// page was queued.
    pub fn supersede_packed_chunk(&mut self, cell: u64, bytes: u32, ckpt_seq: u64) -> bool {
        let page = cell - (cell % crate::store::PAGE as u64);
        let Some(live) = self.packed_live.get_mut(&page) else {
            return false;
        };
        *live = live.saturating_sub(bytes);
        if *live > 0 {
            return false;
        }
        // Last live byte gone. The page is queued like any other extent, so it
        // still waits on all three reclamation conditions.
        self.push_deferred(Pending {
            cell: page,
            class: PACKED_CLASS,
            reclaim_after_ckpt: ckpt_seq + RECLAIM_CKPT_DELAY,
            obsolete_ckpt: ckpt_seq,
        });
        true
    }

    pub fn packed_live_bytes(&self, page_cell: u64) -> Option<u32> {
        self.packed_live.get(&page_cell).copied()
    }

    /// Replace this allocator's occupancy with an index-derived liveness map,
    /// returning how many slots came back.
    ///
    /// This is the repair the design names when it says a torn slab-metadata
    /// write "costs a rebuild rather than data", and the reason it is only
    /// callable at open: the map must be **complete**, and it is complete only
    /// when there is exactly one root to walk and nothing in flight. At open
    /// there are no readers, the deferred queue is empty, and the previous root
    /// is gone — so anything the committed root does not reach is free.
    ///
    /// Never call this while the database is running. Mid-life, a slot can
    /// be legitimately used-but-unreferenced ( superseded and awaiting the
    /// three reclamation conditions, or reachable only from the previous root
    /// that the A/B rule still retains ), and clearing those frees live data.
    ///
    /// Never call it with a map derived from a rebuild that reported errors
    /// or dangling references. An incomplete map here is not a lost repair, it
    /// is data loss.
    pub fn adopt_live_at_open(&mut self, live: &BTreeMap<u32, BTreeSet<u32>>) -> usize {
        let empty = BTreeSet::new();
        let mut reclaimed = 0usize;
        for id in 0..self.slabs.len() as u32 {
            // An Opaque slab is one whose occupancy this process never learned.
            // It means "unknown", not "free"; clearing it would hand out slots
            // that a writer which *did* know is still using.
            let keep = live.get(&id).unwrap_or(&empty);
            let Some(slab) = self.slabs.get_mut(id as usize) else {
                continue;
            };
            if matches!(slab.state, SlabState::Opaque) {
                continue;
            }
            for slot in 0..slab.capacity {
                if slab.is_set(slot) && !keep.contains(&slot) {
                    slab.clear(slot);
                    reclaimed += 1;
                }
            }
            if slab.used_count == 0 {
                slab.state = SlabState::Free;
            }
        }
        reclaimed
    }

    /// Cells currently queued for reclamation, with their size class.
    ///
    /// `fsck` needs these to tell **retention** from a **leak**. A superseded
    /// extent keeps its bit set until all three reclamation conditions pass, so
    /// it is used-but-unreferenced by construction — which looks exactly like a
    /// leaked slot to an index-driven rebuild, and is not one.
    pub fn deferred_cells(&self) -> impl Iterator<Item = (u64, u8)> + '_ {
        self.deferred.iter().map(|p| (p.cell, p.class))
    }

    /// Every packed page the allocator is still accounting for, and its live
    /// byte count.
    ///
    /// `fsck` needs to iterate *the allocator's* pages, not only the ones the
    /// index still reaches. Comparing index-reachable pages alone cannot see a
    /// page whose chunks have **all** died while the allocator still believes
    /// it holds live bytes — and that is precisely the shape of a leak, since
    /// such a page is never queued for reclamation.
    pub fn packed_pages(&self) -> impl Iterator<Item = (u64, u32)> + '_ {
        self.packed_live.iter().map(|(&page, &live)| (page, live))
    }

    /// Queue an extent for reclamation once all three conditions are met.
    pub fn defer_free(&mut self, cell: u64, ckpt_seq: u64) -> bool {
        let Some(class) = self.owning_class(cell) else {
            return false;
        };
        self.push_deferred(Pending {
            cell,
            class,
            reclaim_after_ckpt: ckpt_seq + RECLAIM_CKPT_DELAY,
            obsolete_ckpt: ckpt_seq,
        });
        true
    }

    /// The class of the slot `cell` starts, or `None` if it does not start one.
    ///
    /// # Why the caller must not supply the class
    ///
    /// `free_now` derives a slot index from the class and clears that bit, so a
    /// class that disagrees with the slab clears **some other extent's** bit.
    /// The tempting caller-side answer — `class_for(payload_len)` — is wrong for
    /// every packed chunk: its cell points at a payload *inside* a shared page,
    /// not at a slot of its own, and the resulting index is meaningless.
    ///
    /// So the class comes from the slab, and three things must line up before a
    /// cell is treated as freeable:
    ///
    /// - the slab exists and is `InUse` ( an `Opaque` slab's geometry is
    ///   unknown, so nothing in it may be freed );
    /// - the cell is exactly slot-aligned for that class;
    /// - the class is not `PACKED_CLASS` — a packed page is freed as a whole
    ///   page once every chunk in it is superseded, which is the compactor's
    ///   job, not a per-chunk one.
    pub fn owning_class(&self, cell: u64) -> Option<u8> {
        let id = slab_of(cell);
        let slab = self.slabs.get(id as usize)?;
        let SlabState::InUse { class, .. } = slab.state else {
            return None;
        };
        if class == PACKED_CLASS {
            return None;
        }
        let sz = class_size(class)? as u64;
        let base = id as u64 * SLAB_SIZE + SLAB_META;
        if cell < base || !(cell - base).is_multiple_of(sz) {
            return None;
        }
        Some(class)
    }

    #[inline]
    pub fn deferred_count(&self) -> usize {
        self.deferred.len()
    }

    /// Bytes held by extents that are dead but not yet reclaimable.
    ///
    /// This is the numerator of the space amplification a long reader causes:
    /// every one of these is a slot that superseded data still occupies because
    /// some snapshot may still be entitled to read it. Counting extents is not
    /// a substitute — the classes span 576 to 8256 bytes, so a count says
    /// nothing about the space actually retained.
    pub fn deferred_bytes(&self) -> u64 {
        self.deferred
            .iter()
            .filter_map(|p| class_size(p.class))
            .map(|sz| sz as u64)
            .sum()
    }

    /// Reclaim every deferred extent whose version and checkpoint gates have
    /// passed, subject to `still_referenced` — the refcount condition, supplied
    /// by the segment layer because only it knows about live `Buffer`s.
    ///
    /// Returns how many were reclaimed.
    pub fn reclaim(
        &mut self,
        reader_ckpt_floor: u64,
        ckpt_seq: u64,
        mut still_referenced: impl FnMut(u64, u64) -> bool,
    ) -> usize {
        let mut freed = 0usize;
        // Entries are pushed in non-decreasing `(obsolete_at, reclaim_after_ckpt)`
        // order — both come from values that only move forward, the checkpoint
        // watermark and the checkpoint sequence — so the first entry that fails
        // either gate means every entry behind it fails too.
        //
        // Scanning the whole queue instead was quadratic, and not harmlessly so:
        // this runs inside the shard's write lock, so a queue that cannot drain
        // makes each checkpoint slower, which starves the writers, which stops
        // the watermark advancing, which is what stopped the queue draining. The
        // loop closes on itself. Breaking at the front removes the feedback.
        let mut pinned: VecDeque<Pending> = VecDeque::new();
        while let Some(front) = self.deferred.front() {
            let ckpt_ok = ckpt_seq >= front.reclaim_after_ckpt;
            // Reachability, asked directly rather than proxied: every live
            // reader must hold a root from checkpoint `obsolete_ckpt` or later,
            // because that is exactly the set of roots this extent is *not*
            // reachable from.
            let unreachable = reader_ckpt_floor >= front.obsolete_ckpt;
            if !ckpt_ok || !unreachable {
                break;
            }
            let p = self.deferred.pop_front().expect("front was just observed");
            // The size matters: a packed page is freed at its base while live
            // readers point at payload offsets inside it, so the caller must be
            // able to ask about the whole slot rather than one address.
            let size = class_size(p.class).unwrap_or(0) as u64;
            if still_referenced(p.cell, size) {
                // Pins are *not* ordered, so this one being held says nothing
                // about the next. Hold it aside and retry next checkpoint.
                pinned.push_back(p);
            } else {
                self.free_now(p.cell, p.class);
                self.freed_total += 1;
                freed += 1;
            }
        }
        // Back to the front, in their original order.
        while let Some(p) = pinned.pop_back() {
            self.deferred.push_front(p);
        }
        freed
    }

    /// Queue an entry, asserting the ordering [`Allocator::reclaim`] relies on.
    ///
    /// `reclaim` stops at the first entry that fails its gates, which is only
    /// correct while the queue is non-decreasing in both. Both fields come from
    /// values that only move forward — the checkpoint watermark and the
    /// checkpoint sequence — so this holds by construction; the assertion is
    /// here so that a future caller passing something else fails loudly instead
    /// of making `reclaim` silently skip reclaimable extents.
    fn push_deferred(&mut self, p: Pending) {
        debug_assert!(
            self.deferred
                .back()
                .is_none_or(|b| b.obsolete_ckpt <= p.obsolete_ckpt
                    && b.reclaim_after_ckpt <= p.reclaim_after_ckpt),
            "deferred queue must stay non-decreasing for reclaim's early break"
        );
        self.deferred.push_back(p);
    }

    fn free_now(&mut self, cell: u64, class: u8) {
        let id = slab_of(cell);
        let Some(slab) = self.slabs.get_mut(id as usize) else {
            return;
        };
        let Some(sz) = class_size(class) else { return };
        let within = cell - (id as u64 * SLAB_SIZE + SLAB_META);
        let slot = (within / sz as u64) as u32;
        if slot < slab.capacity {
            slab.clear(slot);
        }
        self.packed_live.remove(&cell);
        if slab.used_count == 0 {
            slab.state = SlabState::Free;
            // Do not shrink the file: space is returned by punching a hole, never
            // by truncating, because truncating under a live mapping is SIGBUS.
        }
    }

    /// Slabs whose live fraction has fallen below [`COMPACT_LIVE_FRACTION`],
    /// emptiest first. Excludes the current bump slabs.
    pub fn evacuation_candidates(&self) -> Vec<u32> {
        let mut c: Vec<u32> = self
            .slabs
            .iter()
            .enumerate()
            .filter(|(i, s)| {
                matches!(s.state, SlabState::InUse { .. })
                    && s.used_count > 0
                    && s.live_fraction() < COMPACT_LIVE_FRACTION
                    && !self.active.contains(&Some(*i as u32))
            })
            .map(|(i, _)| i as u32)
            .collect();
        c.sort_by(|a, b| {
            self.slabs[*a as usize]
                .live_fraction()
                .partial_cmp(&self.slabs[*b as usize].live_fraction())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        c
    }

    /// Total bytes backed by allocated slabs.
    /// Slab capacity, in 2 MiB steps. **Not** a measure of extents in use.
    pub fn allocated_bytes(&self) -> u64 {
        self.slabs.len() as u64 * SLAB_SIZE
    }

    /// Extents currently allocated, across every slab.
    ///
    /// The fine-grained counterpart to `allocated_bytes`, which moves only when
    /// a slab opens and so cannot see a handful of extents either way.
    pub fn used_extents(&self) -> u64 {
        self.slabs.iter().map(|s| s.used_count as u64).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::extent::class_for;
    use crate::store::fsck::slot_index;

    #[test]
    fn slab_capacity_matches_the_ladder() {
        // The bitmap class is exact-fit, so its capacity is what divides cleanly.
        let bitmap_class = class_for(crate::BITMAP_BYTES).unwrap();
        let cap = slab_capacity(bitmap_class);
        assert_eq!(cap, (SLAB_BODY / 8256) as u32);
        assert!(cap > 250, "expected 250+ bitmaps per slab, got {cap}");

        // Waste per slab must stay negligible.
        let waste = SLAB_BODY - cap as u64 * 8256;
        assert!(
            (waste as f64) / (SLAB_SIZE as f64) < 0.001,
            "slab waste {waste} exceeds 0.1%"
        );
    }

    #[test]
    fn slots_are_64_byte_aligned_in_every_class() {
        // This is the property that makes bitmap alignment a consequence of the
        // ladder rather than a per-kind rule an allocator change could break.
        for class in 0..CLASS_SIZES.len() as u8 {
            for slot in [0u32, 1, 7, slab_capacity(class).saturating_sub(1)] {
                let off = slot_offset(3, class, slot);
                assert_eq!(
                    off % 64,
                    0,
                    "class {class} slot {slot} at {off} is misaligned"
                );
            }
        }
    }

    #[test]
    fn allocation_is_dense_and_offsets_are_unique() {
        let mut a = Allocator::new();
        let class = 1u8;
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1000 {
            let cell = a.alloc(class).unwrap();
            assert!(seen.insert(cell), "offset {cell} handed out twice");
        }
        // Slab 1, not 0: slab 0 is reserved because its metadata region is the
        // superblock. See `new_slab_for`.
        assert_eq!(
            a.slab(1).unwrap().used_count(),
            1000.min(slab_capacity(class))
        );
    }

    #[test]
    fn allocation_rolls_over_to_a_new_slab_when_full() {
        let mut a = Allocator::new();
        let class = 10u8; // bitmaps: fewest slots per slab
        let cap = slab_capacity(class);
        for _ in 0..cap {
            a.alloc(class).unwrap();
        }
        // Two: the reserved slab 0 plus the one being filled.
        assert_eq!(a.slab_count(), 2);
        a.alloc(class).unwrap();
        assert_eq!(a.slab_count(), 3, "must open another slab, not fail");
    }

    #[test]
    fn generations_keep_a_checkpoints_classes_in_a_contiguous_run() {
        let mut a = Allocator::new();
        a.begin_generation();
        // One allocation in each of several classes, as a checkpoint writing
        // mixed-density chunks would do.
        let mut slabs = Vec::new();
        for class in 1..=5u8 {
            let cell = a.alloc(class).unwrap();
            slabs.push(slab_of(cell));
        }
        slabs.sort_unstable();
        // They should occupy consecutive slab ids, so a key's chunks land in a
        // small file window even though they span classes.
        // Contiguous, starting at 1 — slab 0 is reserved.
        assert_eq!(
            slabs,
            (1..6).collect::<Vec<u32>>(),
            "a generation's classes must claim a contiguous slab run"
        );

        // A new generation **continues** in the partly-filled slab rather than
        // abandoning it.
        //
        // This assertion used to be the opposite, on the reading that a fresh
        // generation means fresh slabs. Measurement refuted it: abandoning cost
        // one slab per class per checkpoint, 12-15x aged space amplification,
        // and compaction could not recover it because relocated chunks land in
        // the slab that gets abandoned next. See `begin_generation`.
        a.begin_generation();
        let cell = a.alloc(1).unwrap();
        assert_eq!(
            slab_of(cell),
            1,
            "a new generation must keep filling a slab that still has room"
        );
    }

    /// Adoption must clear unreferenced slots — and must never touch a slab
    /// whose occupancy this process did not learn.
    ///
    /// An `Opaque` slab means "unknown", not "free". It is what a reopen leaves
    /// behind when a slab's metadata block is missing or torn, and clearing it
    /// on the strength of an index walk would hand out slots that are still
    /// live. The skip is the difference between a repair and data loss.
    #[test]
    fn adoption_clears_orphans_and_leaves_opaque_slabs_alone() {
        let mut a = Allocator::new();
        a.begin_generation();

        // Three live slots in a known slab, and one that nothing references.
        let live: Vec<u64> = (0..3).map(|_| a.alloc(1).unwrap()).collect();
        let orphan = a.alloc(1).unwrap();
        let slab = slab_of(live[0]);
        assert_eq!(slab_of(orphan), slab, "the fixture wants them in one slab");

        let mut map: BTreeMap<u32, BTreeSet<u32>> = BTreeMap::new();
        let entry = map.entry(slab).or_default();
        for c in &live {
            entry.insert(slot_index(*c, 1).unwrap());
        }

        assert_eq!(
            a.adopt_live_at_open(&map),
            1,
            "exactly the orphan comes back"
        );
        for c in &live {
            assert!(
                a.slab(slab).unwrap().is_set(slot_index(*c, 1).unwrap()),
                "adoption freed a referenced slot"
            );
        }
        assert!(!a.slab(slab).unwrap().is_set(slot_index(orphan, 1).unwrap()));

        // A slab whose contents this process never learned must come through
        // untouched. Today that is enforced twice over: `Slab::opaque` carries
        // zero capacity, so the loop cannot reach a slot at all, *and* the
        // state is skipped explicitly. The capacity is the load-bearing half —
        // asserted here, because the explicit skip is unreachable while it
        // holds and so cannot be covered by a test.
        let mut b = Allocator::restore(vec![None, None]);
        assert!(
            (0..b.slab_count()).all(|i| b.slab(i as u32).is_some_and(|s| s.capacity() == 0)),
            "an Opaque slab must have no reachable slots"
        );
        assert_eq!(
            b.adopt_live_at_open(&BTreeMap::new()),
            0,
            "an Opaque slab means unknown, not free"
        );
    }

    /// All three surviving gates, each shown blocking on its own.
    ///
    /// This was `reclaim_requires_all_three_conditions` and its first case
    /// asserted the **version gate** ( `safe_version > obsolete_at` ), removed on
    /// 2026-08-27 as subsumed by the reachability gate. Reachability replaces it
    /// as the first case rather than the count dropping to two: the three are
    /// still "reader entitlement, recovery delay, escaped buffer", and only the
    /// first one changed how it is asked.
    #[test]
    fn reclaim_requires_all_three_conditions() {
        let mut a = Allocator::new();
        let cell = a.alloc(1).unwrap();
        a.defer_free(cell, 100);
        assert_eq!(a.deferred_count(), 1);

        // Reachability gate not passed: a reader holds a root from before the
        // checkpoint that superseded this.
        assert_eq!(
            a.reclaim(99, 200, |_, _| false),
            0,
            "a reader at checkpoint 99 can still reach what checkpoint 100 superseded"
        );
        // Checkpoint gate not passed.
        assert_eq!(
            a.reclaim(u64::MAX, 101, |_, _| false),
            0,
            "needs ckpt_seq >= freeing + 2"
        );
        // Refcount gate not passed: a live Buffer still points into it.
        assert_eq!(
            a.reclaim(u64::MAX, 102, |_, _| true),
            0,
            "a live Buffer must block reuse"
        );
        // All three satisfied.
        assert_eq!(a.reclaim(u64::MAX, 102, |_, _| false), 1);
        assert_eq!(a.deferred_count(), 0);
    }

    /// A packed chunk's cell must be refused, not freed.
    ///
    /// Its cell points at a payload *inside* a shared page, not at a slot of its
    /// own. Freeing it with a class derived from its payload length — the
    /// obvious caller-side guess — computes a slot index in the wrong geometry
    /// and clears **some other extent's** occupancy bit. That corruption is
    /// silent, and now that occupancy is persisted it would also outlive the
    /// process.
    #[test]
    fn a_packed_payload_cell_is_refused_rather_than_freeing_a_stranger() {
        let mut a = Allocator::new();
        let page = a.alloc_packed(0).unwrap();
        let used_before = a.slab(0).unwrap().used_count();

        // A payload sits past the page header, never at the page base.
        let payload = page + 40;
        assert!(
            !a.defer_free(payload, 0),
            "a cell inside a packed page owns no slot and must be refused"
        );
        assert_eq!(a.deferred_count(), 0);

        // Even the page base is refused: freeing a page is the compactor's job,
        // once every chunk in it is superseded.
        assert!(
            !a.defer_free(page, 0),
            "PACKED_CLASS is not per-chunk freeable"
        );
        assert_eq!(
            a.slab(0).unwrap().used_count(),
            used_before,
            "nothing may have been cleared"
        );
    }

    /// A cell that is not slot-aligned owns no slot.
    #[test]
    fn a_misaligned_cell_is_refused() {
        let mut a = Allocator::new();
        let cell = a.alloc(1).unwrap();
        assert!(a.owning_class(cell).is_some(), "the slot base resolves");
        assert!(
            a.owning_class(cell + 1).is_none(),
            "one byte into a slot is not a slot"
        );
        assert!(!a.defer_free(cell + 1, 0));
    }

    /// An `Opaque` slab's geometry is unknown, so nothing in it may be freed.
    #[test]
    fn nothing_in_an_opaque_slab_is_freeable() {
        let a = Allocator::restore(vec![None, None]);
        assert!(a.owning_class(SLAB_META).is_none());
        assert!(a.owning_class(SLAB_SIZE + SLAB_META).is_none());
    }

    #[test]
    fn reclaimed_slot_is_reused_only_after_a_fresh_generation() {
        let mut a = Allocator::new();
        let cell = a.alloc(1).unwrap();
        a.defer_free(cell, 0);
        assert_eq!(a.reclaim(u64::MAX, RECLAIM_CKPT_DELAY, |_, _| false), 1);
        assert_eq!(a.slab(0).unwrap().used_count(), 0);
    }

    /// Live bytes are tracked in RAM only, and drain to zero through the same
    /// call the checkpointer uses.
    ///
    /// This used to exercise a bare `supersede_packed` that decremented and
    /// nothing else. That helper was replaced by `supersede_packed_chunk`, which
    /// also queues the page once the last byte goes — and the old one lingered,
    /// called by this test alone, until an unwired-machinery sweep found it.
    #[test]
    fn packed_live_bytes_drain_to_zero_and_queue_the_page() {
        let mut a = Allocator::new();
        let page = a.alloc_packed(0).unwrap();
        a.add_packed(page, 4000);
        assert_eq!(a.packed_live_bytes(page), Some(4000));

        // A payload inside the page, not the page base.
        let inside = page + 40;
        assert!(
            !a.supersede_packed_chunk(inside, 1500, 0),
            "the page still has live bytes"
        );
        assert_eq!(a.packed_live_bytes(page), Some(2500));

        // Saturates rather than wrapping if accounting ever drifts, and the last
        // byte queues the page rather than merely reaching zero.
        assert!(
            a.supersede_packed_chunk(inside, 99_999, 0),
            "the last live byte must queue the page"
        );
        assert_eq!(a.packed_live_bytes(page), Some(0));
        assert_eq!(a.deferred_count(), 1);
    }

    #[test]
    fn evacuation_candidates_are_emptiest_first() {
        let mut a = Allocator::new();
        let class = 10u8;
        let cap = slab_capacity(class);

        // Fill three slabs, then free most of slab 0 and some of slab 1.
        let mut cells = Vec::new();
        for _ in 0..cap * 3 {
            cells.push(a.alloc(class).unwrap());
        }
        a.begin_generation(); // so none of the three are active bump slabs

        for (i, &c) in cells.iter().enumerate() {
            let slab = slab_of(c);
            // Slab 0 is reserved, so the allocatable ones start at 1.
            let keep = match slab {
                1 => i % 20 == 0, // ~5% live
                2 => i % 4 == 0,  // ~25% live
                _ => true,        // full
            };
            if !keep {
                a.defer_free(c, 0);
            }
        }
        a.reclaim(u64::MAX, RECLAIM_CKPT_DELAY, |_, _| false);

        let cands = a.evacuation_candidates();
        assert!(cands.contains(&1), "a 5%-live slab must be a candidate");
        assert!(cands.contains(&2), "a 25%-live slab must be a candidate");
        assert!(!cands.contains(&3), "a full slab must not be");
        assert_eq!(cands[0], 1, "emptiest slab must be evacuated first");
    }

    #[test]
    fn slot_liveness_is_queryable_for_rebuild() {
        let mut a = Allocator::new();
        let class = 1u8;
        let c0 = a.alloc(class).unwrap();
        let c1 = a.alloc(class).unwrap();
        let slab = a.slab(slab_of(c0)).unwrap();
        assert!(slab.is_set(0));
        assert!(slab.is_set(1));
        assert!(!slab.is_set(2), "unallocated slot must read as free");
        assert!(!slab.is_set(u32::MAX), "out-of-range slot must not panic");

        a.defer_free(c1, 0);
        a.reclaim(u64::MAX, RECLAIM_CKPT_DELAY, |_, _| false);
        assert!(
            !a.slab(slab_of(c1)).unwrap().is_set(1),
            "reclaimed slot reads free"
        );
    }

    #[test]
    fn slab_of_inverts_slot_offset() {
        for slab_id in [0u32, 1, 77] {
            for class in 1..=10u8 {
                let off = slot_offset(slab_id, class, 3);
                assert_eq!(slab_of(off), slab_id);
            }
        }
    }

    #[test]
    fn unknown_class_is_rejected() {
        let mut a = Allocator::new();
        assert!(a.alloc(99).is_err());
    }

    /// Reclamation must gate on **reachability**, not on a version proxy.
    ///
    /// An extent superseded by checkpoint `k` is reachable from the roots of
    /// checkpoints below `k` and from no others. So a reader that captured its
    /// root at `k-1` must block it and one that captured at `k` must not —
    /// regardless of versions, which is the whole point: a reader's version and
    /// its captured root are read at different moments in `Db::snapshot`, so a
    /// version can be arbitrarily high while the root is stale.
    ///
    /// Both directions matter. Only the first catches the use-after-free; only
    /// the second catches a "fix" that simply stops reclaiming, which is safe,
    /// wrong, and otherwise indistinguishable.
    #[test]
    fn reclamation_gates_on_the_superseding_checkpoint_not_the_version() {
        let mut a = Allocator::new();
        a.begin_generation();
        let cell = a.alloc(1).unwrap();
        // Superseded by checkpoint 7.
        assert!(a.defer_free(cell, 7));

        // The checkpoint delay is satisfied throughout, so the only thing
        // moving below is the reader floor.
        let now = 99u64;

        assert_eq!(
            a.reclaim(6, now, |_, _| false),
            0,
            "a reader holding the root of checkpoint 6 can still reach it"
        );
        assert_eq!(
            a.reclaim(0, now, |_, _| false),
            0,
            "and 0 -- the unpublished state -- must block, not permit"
        );
        assert_eq!(
            a.reclaim(7, now, |_, _| false),
            1,
            "a reader at checkpoint 7 cannot reach it, so it must go -- a floor \
             that never permits reclamation is not a fix"
        );
    }
}
