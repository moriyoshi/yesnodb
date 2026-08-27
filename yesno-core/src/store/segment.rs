//! The mmap'd shard address space, and the bridge to `arrow_buffer::Buffer`.
//!
//! **The mapping `unsafe` lives here**, at exactly two call sites:
//!
//! 1. `memmap2::MmapOptions::map` — unavoidable; mapping a file is unsafe by
//!    nature because another process can truncate it underneath us.
//! 2. `Buffer::from_custom_allocation` — hands Arrow a raw pointer whose
//!    lifetime is governed by an [`ExtentGuard`].
//!
//! **This header claimed until 2026-09-14 that "all `unsafe` in the crate lives
//! in this module" and that "nothing else in `yesno-core` uses `unsafe`".**
//! Counted with `#[cfg(test)]` stripped there are **25 `unsafe` blocks and 12
//! `unsafe fn` across six files** — two here, four in `db/readers.rs` ( two more
//! mappings plus `kill` and `errno` ), and nineteen SIMD-intrinsic blocks in
//! `ops/{array,bitmap,mixed,run}.rs`. The claim was true when written and the
//! NEON arms landed afterwards. Container casts do still go through `bytemuck`'s
//! checked variants, which is the part that remains true.
//!
//! # I6: mappings are append-only
//!
//! The file is mapped in fixed-size segments. On growth a **new** segment is
//! pushed; an existing one is never `mremap`ped, never unmapped while a
//! `Buffer` may point into it, and the file never shrinks.
//!
//! - A `Buffer` derived from segment *k* holds an `Arc<MmapSegment>`, so that
//!   address range stays mapped even if hundreds more segments are added later.
//! - `SEGMENT_SIZE` is a multiple of [`SLAB_SIZE`], so **no slab — and therefore
//!   no extent — straddles a segment boundary** and a single `Buffer` never
//!   needs two guards.
//! - Space is returned by punching holes, never by truncating. Truncating under
//!   a live mapping raises `SIGBUS`, which is not catchable as a `Result` and
//!   would abort the process.
//!
//! # I2: mapped pages are read-only and immutable once published
//!
//! The mapping is `PROT_READ`. All writes go through `pwrite` on a separate
//! descriptor. Two reasons, and the second is the important one:
//!
//! 1. `msync` gives no ordering control, so a writable mapping cannot express
//!    the write ordering a checkpoint needs.
//! 2. Containers hand out `&[u8]` / `&[u16]` / `&[u64]` slices aliasing the
//!    mapping. Writing through one of those while a snapshot can observe it is
//!    **undefined behaviour**, not merely a torn read. A `PROT_WRITE` mapping
//!    would turn that from impossible into merely unlikely.
//!
//! This relies on `pwrite` and `MAP_SHARED` being coherent through a unified
//! page cache — true on Linux and macOS, not portable to Windows.
//!
//! # Refcounting is one of three reclamation conditions, not the rule
//!
//! [`ExtentGuard`] keeps a segment mapped for as long as any `Buffer` derived
//! from it lives, which covers a `Buffer` that escaped into a result outliving
//! its snapshot. [`SegmentedMmap::any_pinned_in`] makes that observable, and it
//! is what answers reclamation **condition 3**.
//!
//! It is emphatically not sufficient on its own. A snapshot can be *entitled* to
//! an extent it has not read yet, at which moment nothing points into it and the
//! refcount is zero — freeing on that basis alone would pull the extent out from
//! under a reader that was about to materialize it. Condition 1 ( the version
//! watermark ) covers entitled-but-unread; condition 3 covers read-and-escaped.
//! Neither implies the other. See `alloc::Pending`.

use std::collections::BTreeMap;
use std::fs::File;
use std::path::Path;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex, Weak};

use arrow_buffer::alloc::Allocation;
use arrow_buffer::Buffer;

use crate::error::{CodecError, Result};
use crate::store::SLAB_SIZE;

/// Bytes per mmap segment. A multiple of [`SLAB_SIZE`] so no extent straddles a
/// segment boundary.
///
/// 1 GiB balances two costs: smaller segments mean more `Arc` clones on the read
/// path, larger ones mean a single long-lived `Buffer` pins more *virtual*
/// address space ( physical pages stay page-cache backed and evictable, so this
/// is address space, not RSS ). Do not go below 256 MiB or above 4 GiB.
pub const SEGMENT_SIZE: u64 = 1 << 30;

const _: () = assert!(
    SEGMENT_SIZE.is_multiple_of(SLAB_SIZE),
    "a slab must never straddle a segment boundary"
);

/// One mapped region of the shard file. Immutable for its whole lifetime.
pub struct MmapSegment {
    map: memmap2::Mmap,
    base: u64,
}

impl MmapSegment {
    #[inline]
    pub fn base(&self) -> u64 {
        self.base
    }

    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        &self.map
    }

    /// Does this segment cover `[cell, cell + len)`?
    #[inline]
    fn covers(&self, cell: u64, len: usize) -> bool {
        cell >= self.base && cell + len as u64 <= self.base + self.map.len() as u64
    }
}

// `MmapSegment` is `Send + Sync` by derivation: `memmap2::Mmap` already is, and
// `u64` is. No hand-written `unsafe impl` is needed, and adding one would be
// worse than redundant — it would silently keep holding if a future field made
// the type genuinely thread-unsafe. Asserted so that stays true.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<MmapSegment>();
};

/// Keeps a segment mapped for as long as an Arrow `Buffer` points into it.
///
/// Handed to `Buffer::from_custom_allocation` as the allocation owner. The
/// `Arc<MmapSegment>` is the load-bearing field: it is what makes the pointer
/// handed to Arrow remain valid for the `Buffer`'s whole life, however far that
/// `Buffer` travels.
pub struct ExtentGuard {
    /// Never read, and that is the entire point.
    ///
    /// This field exists for its `Drop` behaviour: holding the `Arc` is what
    /// keeps the mapping alive for as long as any `Buffer` derived from it. The
    /// compiler's "never read" warning is correct and must not be resolved by
    /// deleting the field — doing so would compile cleanly and produce a
    /// dangling pointer the moment the last other reference to the segment went
    /// away.
    #[allow(dead_code)]
    seg: Arc<MmapSegment>,
}

// `Allocation` has a blanket impl for `T: RefUnwindSafe + Send + Sync`, so this
// is a bound to satisfy rather than a trait to implement. The static assertion
// keeps a future field ( a `Cell`, an `Rc` ) from silently removing the property
// and turning the `from_custom_allocation` call below into a compile error far
// from its cause.
const _: fn() = || {
    fn assert_allocation<T: Allocation>() {}
    assert_allocation::<ExtentGuard>();
};

/// The shard's address space: an append-only vector of mapped segments plus a
/// separate write descriptor.
pub struct SegmentedMmap {
    file: File,
    /// Cells with at least one live Arrow `Buffer` pointing into them.
    ///
    /// This is reclamation condition 3 made answerable. Each `buffer_at` shares
    /// one [`ExtentGuard`] per cell and records a `Weak` to it, so a cell is
    /// pinned exactly while some `Buffer` derived from it is alive — including
    /// long after the `Snapshot`, the `ShardStore`, and the `Db` are gone,
    /// which is the case the version watermark cannot see.
    ///
    /// A `BTreeMap` rather than a `HashMap` because the question asked is a
    /// *range* one: a packed page is freed at its base cell while the `Buffer`s
    /// into it point at payload offsets scattered through the page.
    pinned: Mutex<BTreeMap<u64, Weak<ExtentGuard>>>,
    /// Append-only. Existing entries are never replaced, only added to.
    segs: Mutex<Vec<Arc<MmapSegment>>>,
    /// Regions whose stored checksum has been recomputed and matched, as
    /// `file offset -> byte length`.
    ///
    /// **Keyed on file offset, deliberately, not on a page id.** A freed and
    /// reallocated node recycles its id, so an id-keyed entry would go stale
    /// *silently* — the worst possible failure for a cache that exists to catch
    /// corruption. An offset names one region of one file for as long as the
    /// file exists.
    ///
    /// Emptied for every intersecting region by [`Self::write_at`], which is the
    /// only thing in the crate that changes bytes already in the file
    /// ( `set_len` only ever grows it, under I6 ). That invalidation is *exact*
    /// rather than conservative because I2 forbids rewriting a published extent:
    /// a write can only land on space no snapshot can reach.
    verified: Mutex<VerifiedCache>,
}

/// Bounded two-generation set of verified regions.
///
/// # Why this is bounded at all
///
/// It was not, until 2026-09-15. Entries were inserted on first verification
/// and removed **only** by a write that overlapped them, so the map grew with
/// the number of distinct regions ever read and nothing else ever shrank it.
/// Measured then: reading 1000 / 2000 / 3000 / 4000 distinct keys produced
/// 52 / 100 / 148 / 198 entries with no plateau. That is bounded by the
/// database's page count rather than by anything the cache controls -- a full
/// scan populates one entry per page and holds it for the life of the process.
///
/// # Why eviction is safe and staleness is not
///
/// The asymmetry is the whole design. Dropping an entry costs one recomputed
/// CRC over at most [`MAX_VERIFIED_SPAN`] bytes and can never produce a wrong
/// answer. **Keeping** an entry whose bytes have changed is exactly the failure
/// this cache exists to prevent. So the bound may evict as freely as it likes,
/// and `invalidate` must remain exact.
///
/// # Why two generations rather than LRU or clear-on-full
///
/// True LRU needs a per-entry clock the read path would have to maintain and a
/// way to find the minimum, which is a second structure. Clearing everything at
/// the cap is simplest but discards the hot set wholesale, so the next sweep
/// re-verifies **every** region at once. Two generations need no clock: a hit
/// in `old` is promoted to `young`, and when `young` fills, `old` is dropped and
/// `young` takes its place. Anything used since the last rotation survives it,
/// so the hot set persists and at most half the entries are lost at a time.
#[derive(Default)]
struct VerifiedCache {
    young: BTreeMap<u64, u32>,
    old: BTreeMap<u64, u32>,
}

/// Entries per generation; the cache holds at most twice this.
///
/// At roughly 24 bytes per `BTreeMap` entry this is about 0.8 MB per segment,
/// and one segment per shard. Sized for the *hot* set rather than the corpus:
/// a miss costs a single CRC over at most 8256 bytes, well under a microsecond,
/// so the cache is there to stop a hot page being re-verified on every read --
/// not to cover the database. Deliberately a constant and not a `DbOptions`
/// knob: a knob is public API under R1 / R6 / R7, and nothing has yet shown a
/// workload that needs a different number.
const VERIFIED_GENERATION: usize = 16_384;

impl VerifiedCache {
    fn is_verified(&mut self, cell: u64, len: usize) -> bool {
        if self.young.get(&cell).is_some_and(|&l| l as usize == len) {
            return true;
        }
        // A hit in the older generation is promoted, which is what lets the hot
        // set survive a rotation without any per-entry bookkeeping.
        if self.old.get(&cell).is_some_and(|&l| l as usize == len) {
            self.old.remove(&cell);
            self.insert(cell, len as u32);
            return true;
        }
        false
    }

    fn insert(&mut self, cell: u64, len: u32) {
        self.young.insert(cell, len);
        if self.young.len() >= VERIFIED_GENERATION {
            self.old = std::mem::take(&mut self.young);
        }
    }

    fn len(&self) -> usize {
        self.young.len() + self.old.len()
    }
}

/// The widest region [`SegmentedMmap::verify_once`] will ever be asked about.
///
/// Bounds the backward scan in invalidation: an entry starting more than this
/// far below a write cannot reach it. It is the top size class, which covers
/// every family — an extent payload is at most a slot, a packed page is class 0
/// at 4096, and an index node is allocated through the same ladder.
const MAX_VERIFIED_SPAN: u64 = 8256;

const _: () = assert!(
    MAX_VERIFIED_SPAN
        >= crate::store::extent::CLASS_SIZES[crate::store::extent::CLASS_SIZES.len() - 1] as u64,
    "MAX_VERIFIED_SPAN must cover the widest size class, or invalidation can miss an entry"
);

impl SegmentedMmap {
    /// Open ( creating if absent ) the shard file.
    pub fn open(path: impl AsRef<Path>) -> std::io::Result<Self> {
        Self::open_with_access(path, true)
    }

    /// Open an **existing** shard file for reading only.
    ///
    /// `create` is deliberately absent: a reader that conjures an empty shard
    /// file has invented a database rather than found one, and every later read
    /// would answer "no such key" instead of failing. The file must exist.
    ///
    /// The mapping is still `MAP_SHARED`, which is what makes a reader see a
    /// writer's committed pages — the page cache is the shared medium. What
    /// read-only buys is that no code path in this process *can* write through
    /// it, which is the guarantee a foreign reader has to offer the writer that
    /// owns the directory.
    pub fn open_read_only(path: impl AsRef<Path>) -> std::io::Result<Self> {
        Self::open_with_access(path, false)
    }

    fn open_with_access(path: impl AsRef<Path>, writable: bool) -> std::io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(writable)
            .create(writable)
            .truncate(false)
            .open(path)?;
        let this = SegmentedMmap {
            file,
            segs: Mutex::new(Vec::new()),
            pinned: Mutex::new(BTreeMap::new()),
            verified: Mutex::new(VerifiedCache::default()),
        };
        this.remap_to_file_len()?;
        Ok(this)
    }

    /// Current file length in bytes.
    pub fn file_len(&self) -> std::io::Result<u64> {
        Ok(self.file.metadata()?.len())
    }

    /// Grow the file to at least `len`, rounded up to a whole segment, and map
    /// the new tail.
    ///
    /// Only ever extends. Existing segments are untouched, so any live `Buffer`
    /// keeps pointing at a valid mapping.
    pub fn grow_to(&self, len: u64) -> std::io::Result<()> {
        let want = len.div_ceil(SEGMENT_SIZE) * SEGMENT_SIZE;
        let have = self.file_len()?;
        if want > have {
            self.file.set_len(want)?;
        }
        self.remap_to_file_len()
    }

    /// Map any segments the file has grown to cover. Never unmaps.
    fn remap_to_file_len(&self) -> std::io::Result<()> {
        let file_len = self.file_len()?;
        let mut segs = self.segs.lock().unwrap();
        while (segs.len() as u64) * SEGMENT_SIZE < file_len {
            let base = segs.len() as u64 * SEGMENT_SIZE;
            let len = SEGMENT_SIZE.min(file_len - base) as usize;
            // SAFETY: mapping a file read-only. The usual mmap caveat applies —
            // external truncation of this file would make later access raise
            // SIGBUS — which is why I6 forbids ever shrinking it, and why the
            // docs restrict deployment to local filesystems.
            let map = unsafe {
                memmap2::MmapOptions::new()
                    .offset(base)
                    .len(len)
                    .map(&self.file)?
            };
            segs.push(Arc::new(MmapSegment { map, base }));
        }
        Ok(())
    }

    /// Number of mapped segments. Test scaffolding — the growth and
    /// no-remap properties are what it exists to assert.
    #[cfg(test)]
    pub fn segment_count(&self) -> usize {
        self.segs.lock().unwrap().len()
    }

    fn segment_for(&self, cell: u64, len: usize) -> Result<Arc<MmapSegment>> {
        let segs = self.segs.lock().unwrap();
        let idx = (cell / SEGMENT_SIZE) as usize;
        let seg = segs.get(idx).ok_or(CodecError::OutOfBounds {
            off: cell as usize,
            len,
            buf_len: segs.len() * SEGMENT_SIZE as usize,
        })?;
        if !seg.covers(cell, len) {
            // Only possible if an extent straddles a segment boundary, which the
            // SEGMENT_SIZE % SLAB_SIZE assertion is meant to make impossible.
            return Err(CodecError::Invariant("extent straddles a segment boundary"));
        }
        Ok(seg.clone())
    }

    /// A zero-copy [`Buffer`] over `[cell, cell + len)`.
    ///
    /// The returned buffer keeps the underlying mapping alive independently of
    /// this `SegmentedMmap` and of any snapshot.
    pub fn buffer_at(&self, cell: u64, len: usize) -> Result<Buffer> {
        let seg = self.segment_for(cell, len)?;
        let off = (cell - seg.base()) as usize;
        let ptr = seg.as_slice()[off..off + len].as_ptr();
        // One guard per cell, shared by every live Buffer over it, so that a
        // `Weak` to it answers "is this cell still being read?".
        let guard = {
            let mut pinned = self.pinned.lock().unwrap();
            match pinned.get(&cell).and_then(Weak::upgrade) {
                Some(g) => g,
                None => {
                    let g = Arc::new(ExtentGuard { seg: seg.clone() });
                    pinned.insert(cell, Arc::downgrade(&g));
                    g
                }
            }
        };

        let nn = NonNull::new(ptr as *mut u8).ok_or(CodecError::Invariant("null mapping"))?;
        // SAFETY:
        // - `ptr` points `off` bytes into `seg`'s mapping and `seg.covers(cell,
        //   len)` was checked above, so `[ptr, ptr + len)` lies entirely within
        //   that mapping.
        // - `guard` holds an `Arc<MmapSegment>` for that same mapping, and
        //   `Buffer` keeps the allocation owner alive for its own lifetime, so
        //   the pointer cannot dangle however far the `Buffer` travels.
        // - The region is immutable: the mapping is PROT_READ, and I2 forbids
        //   rewriting a published extent, so no `&mut` to these bytes exists.
        // - `MmapSegment` is `Send + Sync`, so the `Allocation` bound holds.
        Ok(unsafe { Buffer::from_custom_allocation(nn, len, guard) })
    }

    /// Is any cell in `[start, start + len)` still being read?
    ///
    /// **Reclamation condition 3.** A `Buffer` is `'static`, so it can outlive
    /// every structure that produced it — a `RecordBatch` handed to a query
    /// engine is the ordinary case — and the version watermark cannot see that.
    /// Freeing a slot while this returns true is precisely the bug
    /// `tests/zero_copy_mvcc.rs` exists to catch.
    ///
    /// The range form matters: a packed page is freed at its base cell, but the
    /// `Buffer`s into it point at payload offsets scattered through the page, so
    /// an exact-match lookup would report a busy page as free.
    pub fn any_pinned_in(&self, start: u64, len: u64) -> bool {
        let mut pinned = self.pinned.lock().unwrap();
        // Dropping a Buffer cannot remove its own entry, so dead weaks
        // accumulate. Sweeping here keeps the map proportional to live readers
        // rather than to reads ever performed.
        pinned.retain(|_, w| w.strong_count() > 0);
        pinned
            .range(start..start.saturating_add(len))
            .next()
            .is_some()
    }

    /// How many cells are currently pinned. Diagnostics and tests.
    pub fn pinned_count(&self) -> usize {
        let mut pinned = self.pinned.lock().unwrap();
        pinned.retain(|_, w| w.strong_count() > 0);
        pinned.len()
    }

    /// Read a range by copying, for callers that cannot accept the aliasing
    /// contract or that hit a misaligned extent.
    pub fn read_at(&self, cell: u64, len: usize) -> Result<Vec<u8>> {
        let seg = self.segment_for(cell, len)?;
        let off = (cell - seg.base()) as usize;
        Ok(seg.as_slice()[off..off + len].to_vec())
    }

    /// Recompute a stored checksum over `[cell, cell + len)`, at most once per
    /// faulted region.
    ///
    /// # Why this is a cache and not just a check
    ///
    /// The stored CRCs were written and never recomputed by a read. Doing it per
    /// read would charge every point lookup an `O( payload )` pass; doing it once
    /// per region and remembering costs that pass the first time a region is
    /// touched and nothing afterwards. Under I2 a published extent is immutable,
    /// so "has not changed since it verified" is a property the store already
    /// guarantees rather than one this has to police.
    ///
    /// # The caller supplies the verification, and that is the whole design
    ///
    /// This module cannot decide what covers what. It is handed a payload length
    /// for an extent and a node size for an index node and **cannot tell them
    /// apart**, and an extent's CRC lives in a trailer *outside* the range being
    /// read. So the region is the cache key and the caller owns the meaning:
    /// `db/store.rs` knows which family it is reading and passes the check.
    ///
    /// `verify` runs **outside** the lock. Holding a mutex across a CRC pass over
    /// up to 8 KiB would serialise every reader behind the first one to touch a
    /// cold region; the cost of the race is that two threads may verify the same
    /// region concurrently and agree, which is idempotent.
    pub fn verify_once(
        &self,
        cell: u64,
        len: usize,
        verify: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        if self.verified.lock().unwrap().is_verified(cell, len) {
            return Ok(());
        }
        verify()?;
        // A region wider than the scan bound could never be invalidated, so it
        // must not be cached. Nothing produces one today -- the assertion on
        // `MAX_VERIFIED_SPAN` pins that against the ladder -- and a silent
        // stale entry is exactly what this cache exists to prevent.
        if len as u64 <= MAX_VERIFIED_SPAN {
            self.verified.lock().unwrap().insert(cell, len as u32);
        }
        Ok(())
    }

    /// How many regions are currently remembered as verified. Tests.
    #[cfg(test)]
    pub fn verified_count(&self) -> usize {
        self.verified.lock().unwrap().len()
    }

    /// Forget every verified region intersecting `[start, start + len)`.
    fn invalidate_verified(&self, start: u64, len: u64) {
        let mut v = self.verified.lock().unwrap();
        if v.len() == 0 {
            return;
        }
        let hi = start.saturating_add(len);
        // An entry beginning more than `MAX_VERIFIED_SPAN` below the write
        // cannot reach into it, which is what bounds this scan.
        let lo = start.saturating_sub(MAX_VERIFIED_SPAN);
        // Both generations: an entry surviving in `old` is just as stale as one
        // in `young`, and invalidation is the half of this cache that must stay
        // exact.
        let VerifiedCache { young, old } = &mut *v;
        for gen in [young, old] {
            let doomed: Vec<u64> = gen
                .range(lo..hi)
                .filter(|(&s, &l)| s + l as u64 > start)
                .map(|(&s, _)| s)
                .collect();
            for k in doomed {
                gen.remove(&k);
            }
        }
    }

    /// Write bytes at `cell` through the write descriptor.
    ///
    /// Never writes through the mapping. Callers must only target space not
    /// reachable from any published snapshot — I2 is a contract this function
    /// cannot enforce.
    pub fn write_at(&self, cell: u64, data: &[u8]) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            let r = self.file.write_all_at(data, cell);
            // **After the write, not before.** Invalidating first leaves a
            // window in which a concurrent reader verifies the *old* bytes and
            // caches that verdict, which the write then falsifies -- a stale
            // entry that survives, the one outcome this cache must never
            // produce. Invalidating afterwards can only discard a verdict that
            // is still true, which costs one re-verification.
            self.invalidate_verified(cell, data.len() as u64);
            r
        }
        #[cfg(not(unix))]
        {
            let _ = (cell, data);
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "pwrite-based page store requires a unix target",
            ))
        }
    }

    pub fn sync(&self) -> std::io::Result<()> {
        self.file.sync_data()
    }
}

// `check_alignment<T>( &Buffer ) -> Result<()>` lived here and was deleted on
// 2026-08-26. It had no caller outside its own two tests, and its premise was
// wrong as well as unwired: it existed "so that a corrupt or foreign file
// produces an `Err` rather than the panic `ScalarBuffer::new` would raise", but
// production does not error on a misaligned payload — `container::codec::decode`
// **copies** ( `None => decode( kind, &buf.as_slice()[..], card )` ), which is a
// better outcome than either. The alignment decision is made at the two places
// that actually reinterpret bytes: `U16Store::try_shared_from_bytes` returns
// `None`, and `BitStore` is alignment-agnostic with a copying fallback in
// `words()`. A third implementation, reachable from nothing, could only drift.
//
// Do not reintroduce it to "encode alignment in the API" ( plan risk 4 ).
// That risk is about the *allocator* silently producing 8-byte-aligned bitmap
// slots; what guards it is `slot_offsets_yield_64_byte_aligned_buffers` below,
// which checks the ladder's promise against a real mapping, plus
// `validate_ladder`. Deleting this also removed one of the six R1 baseline
// violations, since it took an `arrow_buffer::Buffer` in a public signature.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{SLAB_META, SLAB_SIZE};

    fn tmp(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("yesno-seg-test-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_file(&p);
        p
    }

    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn segment_size_never_splits_a_slab() {
        assert_eq!(SEGMENT_SIZE % SLAB_SIZE, 0);
        // Every slab must sit wholly inside one segment.
        for slab in [0u64, 1, 511, 512, 1000] {
            let start = slab * SLAB_SIZE;
            let end = start + SLAB_SIZE - 1;
            assert_eq!(
                start / SEGMENT_SIZE,
                end / SEGMENT_SIZE,
                "slab {slab} straddles a segment boundary"
            );
        }
    }

    #[test]
    fn write_then_read_back_zero_copy() {
        let path = tmp("roundtrip");
        let _c = Cleanup(path.clone());
        let m = SegmentedMmap::open(&path).unwrap();
        m.grow_to(SLAB_SIZE).unwrap();

        let cell = SLAB_META;
        let data: Vec<u8> = (0..256u32).map(|i| i as u8).collect();
        m.write_at(cell, &data).unwrap();
        m.sync().unwrap();

        let buf = m.buffer_at(cell, data.len()).unwrap();
        assert_eq!(buf.as_slice(), &data[..]);
        assert_eq!(m.read_at(cell, data.len()).unwrap(), data);
    }

    #[test]
    fn buffer_outlives_the_segmented_mmap() {
        // The property the whole ExtentGuard design exists for: a Buffer that
        // escaped into a long-lived result must stay valid after the store that
        // produced it is gone.
        let path = tmp("outlive");
        let _c = Cleanup(path.clone());
        let data = vec![0xABu8; 512];

        let buf = {
            let m = SegmentedMmap::open(&path).unwrap();
            m.grow_to(SLAB_SIZE).unwrap();
            m.write_at(SLAB_META, &data).unwrap();
            m.buffer_at(SLAB_META, data.len()).unwrap()
        }; // SegmentedMmap dropped here

        assert_eq!(buf.as_slice(), &data[..], "mapping must still be live");

        // And a slice of it, which is what an Arrow kernel would keep.
        let sliced = buf.slice_with_length(64, 128);
        drop(buf);
        assert!(sliced.as_slice().iter().all(|&b| b == 0xAB));
    }

    #[test]
    fn growth_adds_segments_without_disturbing_existing_buffers() {
        let path = tmp("growth");
        let _c = Cleanup(path.clone());
        let m = SegmentedMmap::open(&path).unwrap();
        m.grow_to(SLAB_SIZE).unwrap();
        assert_eq!(m.segment_count(), 1);

        let data = vec![0x5Au8; 128];
        m.write_at(SLAB_META, &data).unwrap();
        let buf = m.buffer_at(SLAB_META, data.len()).unwrap();

        // Grow past the first segment. Under I6 this must push a new mapping,
        // not remap the old one.
        m.grow_to(SEGMENT_SIZE + SLAB_SIZE).unwrap();
        assert_eq!(m.segment_count(), 2, "growth must append a segment");
        assert_eq!(
            buf.as_slice(),
            &data[..],
            "existing buffer must be undisturbed"
        );

        // The new segment is addressable.
        let far = SEGMENT_SIZE + SLAB_META;
        m.write_at(far, &data).unwrap();
        assert_eq!(m.buffer_at(far, data.len()).unwrap().as_slice(), &data[..]);
    }

    /// The verified set must not grow without limit.
    ///
    /// Until 2026-09-15 it did: entries were inserted on first verification and
    /// removed only by an overlapping write, so the set grew with the number of
    /// distinct regions ever read. A full scan of a large database populated one
    /// entry per page and held it for the life of the process.
    ///
    /// The assertion is the **bound**, not a particular size, so the constant
    /// can be retuned without editing this test.
    #[test]
    fn the_verified_set_is_bounded_however_many_regions_are_read() {
        let path = tmp("verified-bounded");
        let _c = Cleanup(path.clone());
        let m = SegmentedMmap::open(&path).unwrap();

        // Four generations' worth of distinct regions, none overlapping, plus a
        // few so the test does not land exactly on a rotation boundary -- at an
        // exact multiple `young` is empty and the count is the generation size
        // to the entry, which is a state no assertion should straddle.
        let n = VERIFIED_GENERATION * 4 + 7;
        for i in 0..n {
            let cell = (i as u64 + 1) * MAX_VERIFIED_SPAN;
            m.verify_once(cell, 64, || Ok(())).unwrap();
        }

        // CONTROL: without this the test would pass against a cache that
        // silently stored nothing at all.
        assert!(
            m.verified_count() > VERIFIED_GENERATION / 2,
            "the cache stored almost nothing ({}); it is not being exercised",
            m.verified_count()
        );
        assert!(
            m.verified_count() <= 2 * VERIFIED_GENERATION,
            "verified set grew to {} after {n} distinct regions, above the {} bound",
            m.verified_count(),
            2 * VERIFIED_GENERATION
        );
    }

    /// A region still in use survives a rotation.
    ///
    /// This is what two generations buy over clearing at the cap, and it is the
    /// half that a bound alone does not give: an entry touched since the last
    /// rotation is promoted, so the hot set is not discarded wholesale.
    #[test]
    fn a_region_touched_since_the_last_rotation_survives_it() {
        let path = tmp("verified-promote");
        let _c = Cleanup(path.clone());
        let m = SegmentedMmap::open(&path).unwrap();

        let hot = MAX_VERIFIED_SPAN;
        let calls = std::cell::Cell::new(0usize);
        let touch_hot = || {
            m.verify_once(hot, 64, || {
                calls.set(calls.get() + 1);
                Ok(())
            })
            .unwrap()
        };

        touch_hot();
        assert_eq!(calls.get(), 1, "first touch verifies");

        // Fill one generation, re-touching the hot region as we go so it is
        // promoted rather than aging out.
        for i in 0..(VERIFIED_GENERATION * 2 + 8) {
            let cell = (i as u64 + 2) * MAX_VERIFIED_SPAN;
            m.verify_once(cell, 64, || Ok(())).unwrap();
            if i % 64 == 0 {
                touch_hot();
            }
        }

        touch_hot();
        assert_eq!(
            calls.get(),
            1,
            "a region touched throughout was evicted and had to re-verify"
        );
    }

    /// The cache verifies once per region, and a write to that region forgets it.
    ///
    /// # What each half would miss alone
    ///
    /// Without the **re-read** assertion, an implementation that verified on
    /// every read would pass -- correct, just not a cache, and the point is that
    /// the `O( payload )` pass is paid per faulted region rather than per read.
    ///
    /// Without the **invalidation** assertions, an implementation that never
    /// forgot would pass, and that is the dangerous one: it would keep answering
    /// "verified" for bytes since overwritten. It is also why the design insists
    /// the key be a **file offset** rather than a page id -- a recycled id makes
    /// a stale entry look live and nothing downstream would notice.
    ///
    /// The unrelated-write case is what stops "invalidate everything" passing.
    #[test]
    fn verify_once_caches_and_a_write_invalidates_exactly_the_overlap() {
        let path = tmp("verify-once");
        let _c = Cleanup(path.clone());
        let m = SegmentedMmap::open(&path).unwrap();
        m.grow_to(SLAB_SIZE).unwrap();

        let cell = 4096u64;
        m.write_at(cell, &[9u8; 64]).unwrap();

        let calls = std::cell::Cell::new(0usize);
        let run = || {
            m.verify_once(cell, 64, || {
                calls.set(calls.get() + 1);
                Ok(())
            })
            .unwrap()
        };

        run();
        assert_eq!(calls.get(), 1, "first touch must verify");
        run();
        run();
        assert_eq!(calls.get(), 1, "a verified region must not verify again");
        assert_eq!(m.verified_count(), 1);

        // Outside the region: must not disturb it. Without this, an
        // invalidation that simply cleared the map would still pass.
        m.write_at(cell + 4096, &[1u8; 8]).unwrap();
        run();
        assert_eq!(calls.get(), 1, "an unrelated write must not invalidate");

        // Overlapping from inside.
        m.write_at(cell + 32, &[7u8; 8]).unwrap();
        run();
        assert_eq!(calls.get(), 2, "a write into the region must re-verify");

        // Overlapping from below: begins before the region and reaches into it.
        // This is the case the backward scan bound exists for.
        m.write_at(cell - 8, &[3u8; 16]).unwrap();
        run();
        assert_eq!(
            calls.get(),
            3,
            "a write overlapping from below must invalidate"
        );

        // Touching but not overlapping, on both sides.
        m.write_at(cell - 8, &[3u8; 8]).unwrap();
        m.write_at(cell + 64, &[3u8; 8]).unwrap();
        run();
        assert_eq!(calls.get(), 3, "an adjacent write must not invalidate");
    }

    /// A failed verification is **not** remembered as a success.
    #[test]
    fn a_failed_verification_is_not_cached() {
        let path = tmp("verify-fail");
        let _c = Cleanup(path.clone());
        let m = SegmentedMmap::open(&path).unwrap();
        m.grow_to(SLAB_SIZE).unwrap();
        m.write_at(4096, &[9u8; 64]).unwrap();

        for _ in 0..3 {
            assert!(m
                .verify_once(4096, 64, || Err(CodecError::Invariant("bad")))
                .is_err());
        }
        assert_eq!(
            m.verified_count(),
            0,
            "a region that failed its checksum must never be cached as verified"
        );
    }

    #[test]
    fn out_of_range_access_errors_rather_than_panicking() {
        let path = tmp("oob");
        let _c = Cleanup(path.clone());
        let m = SegmentedMmap::open(&path).unwrap();
        m.grow_to(SLAB_SIZE).unwrap();

        assert!(
            m.buffer_at(SEGMENT_SIZE * 4, 16).is_err(),
            "unmapped segment"
        );
        assert!(m.read_at(SEGMENT_SIZE * 4, 16).is_err());
        // A length running past the end of a mapped segment.
        assert!(
            m.buffer_at(SEGMENT_SIZE - 8, 64).is_err(),
            "straddles a boundary"
        );
    }

    #[test]
    fn slot_offsets_yield_64_byte_aligned_buffers() {
        // The ladder promises this; here it is checked against a real mapping,
        // because it is what keeps `typed_data::<u64>()` from panicking.
        let path = tmp("align");
        let _c = Cleanup(path.clone());
        let m = SegmentedMmap::open(&path).unwrap();
        m.grow_to(SLAB_SIZE * 2).unwrap();

        for class in 1..=10u8 {
            for slot in [0u32, 1, 5] {
                let cell = crate::store::alloc::slot_offset(0, class, slot);
                let sz = crate::store::extent::class_size(class).unwrap() as usize;
                if cell + sz as u64 > SLAB_SIZE * 2 {
                    continue;
                }
                let buf = m.buffer_at(cell, sz).unwrap();
                let addr = buf.as_ptr() as usize;
                assert_eq!(
                    addr % 64,
                    0,
                    "class {class} slot {slot} at {cell} breaks 64-byte alignment"
                );
                // 64-byte alignment subsumes both of the ones the container
                // stores need, which is the property the ladder exists to give.
                assert_eq!(addr % std::mem::align_of::<u64>(), 0);
                assert_eq!(addr % std::mem::align_of::<u16>(), 0);
            }
        }
    }

    // `misaligned_extent_errors_instead_of_panicking` lived here and was deleted
    // on 2026-08-26 as **vacuous in two directions at once**. Its subject,
    // `check_alignment`, had no production caller, so it could not have failed
    // however production behaved; and the policy it asserted — misalignment is
    // an `Err` — is not the policy production has. What production does is fall
    // back to a copying decode and return a correct container, which is better
    // than an error and better than the panic both were guarding against.
    //
    // That real property is covered by `decode_buffer_handles_a_misaligned_payload`
    // in `container::codec`, which is where it belongs: the subject is
    // `decode_buffer`, not the segment map. Checked non-vacuous — replacing
    // the array arm's copying fallback with an `Err` fails it.

    #[test]
    fn file_never_shrinks_on_reopen() {
        let path = tmp("noshrink");
        let _c = Cleanup(path.clone());
        {
            let m = SegmentedMmap::open(&path).unwrap();
            m.grow_to(SEGMENT_SIZE + 1).unwrap();
            assert_eq!(m.file_len().unwrap(), SEGMENT_SIZE * 2);
        }
        let m = SegmentedMmap::open(&path).unwrap();
        assert_eq!(m.file_len().unwrap(), SEGMENT_SIZE * 2, "I6: never shrink");
        assert_eq!(m.segment_count(), 2);
        // grow_to with a smaller length must be a no-op, not a truncation.
        m.grow_to(1024).unwrap();
        assert_eq!(m.file_len().unwrap(), SEGMENT_SIZE * 2);
    }

    #[test]
    fn buffers_are_send_across_threads() {
        let path = tmp("send");
        let _c = Cleanup(path.clone());
        let m = SegmentedMmap::open(&path).unwrap();
        m.grow_to(SLAB_SIZE).unwrap();
        let data = vec![7u8; 64];
        m.write_at(SLAB_META, &data).unwrap();
        let buf = m.buffer_at(SLAB_META, 64).unwrap();

        let h = std::thread::spawn(move || buf.as_slice().iter().map(|&b| b as u32).sum::<u32>());
        assert_eq!(h.join().unwrap(), 7 * 64);
    }

    /// Condition 3, directly: a cell reads as pinned exactly while a `Buffer`
    /// over it is alive, and stops the moment the last one drops.
    #[test]
    fn a_cell_is_pinned_while_a_buffer_over_it_lives() {
        let p = tmp("pinned");
        let _c = Cleanup(p.clone());
        let m = SegmentedMmap::open(&p).unwrap();
        m.grow_to(SLAB_SIZE).unwrap();
        let data = vec![7u8; 64];
        m.write_at(SLAB_META, &data).unwrap();

        assert!(!m.any_pinned_in(SLAB_META, 64), "nothing read yet");
        let buf = m.buffer_at(SLAB_META, 64).unwrap();
        assert!(
            m.any_pinned_in(SLAB_META, 64),
            "a live Buffer must pin its cell"
        );
        drop(buf);
        assert!(
            !m.any_pinned_in(SLAB_META, 64),
            "the pin must clear on drop"
        );
    }

    /// Two reads of one cell share a guard, so the pin outlives the first drop.
    #[test]
    fn a_pin_survives_until_the_last_reader_drops() {
        let p = tmp("pinned-two");
        let _c = Cleanup(p.clone());
        let m = SegmentedMmap::open(&p).unwrap();
        m.grow_to(SLAB_SIZE).unwrap();
        m.write_at(SLAB_META, &[3u8; 64]).unwrap();

        let a = m.buffer_at(SLAB_META, 64).unwrap();
        let b = m.buffer_at(SLAB_META, 64).unwrap();
        drop(a);
        assert!(m.any_pinned_in(SLAB_META, 64), "one reader remains");
        drop(b);
        assert!(!m.any_pinned_in(SLAB_META, 64));
    }

    /// The range form is what makes packed pages safe.
    ///
    /// A page is freed at its base cell while `Buffer`s into it point at payload
    /// offsets scattered through it, so an exact-match check would report a busy
    /// page as reclaimable — reusing it under a live reader.
    #[test]
    fn a_pin_inside_a_page_marks_the_whole_page_busy() {
        let p = tmp("pinned-range");
        let _c = Cleanup(p.clone());
        let m = SegmentedMmap::open(&p).unwrap();
        m.grow_to(SLAB_SIZE).unwrap();
        m.write_at(SLAB_META, &[1u8; 4096]).unwrap();

        let page = SLAB_META;
        let payload = page + 40; // a packed payload, not the page base
        let buf = m.buffer_at(payload, 16).unwrap();

        assert!(!m.any_pinned_in(page, 0), "an empty range pins nothing");
        assert!(
            m.any_pinned_in(page, 4096),
            "a payload pin must make its whole page busy"
        );
        assert!(
            !m.any_pinned_in(page + 4096, 4096),
            "the next page is unaffected"
        );
        drop(buf);
        assert!(!m.any_pinned_in(page, 4096));
    }

    /// The registry must not grow with reads performed, only with live readers.
    #[test]
    fn dead_pins_are_swept_rather_than_accumulating() {
        let p = tmp("pinned-sweep");
        let _c = Cleanup(p.clone());
        let m = SegmentedMmap::open(&p).unwrap();
        m.grow_to(SLAB_SIZE).unwrap();
        m.write_at(SLAB_META, &[9u8; 4096]).unwrap();

        for i in 0..200u64 {
            let _ = m.buffer_at(SLAB_META + i * 8, 8).unwrap();
        }
        assert_eq!(
            m.pinned_count(),
            0,
            "every reader dropped; nothing may remain"
        );

        let held: Vec<_> = (0..5u64)
            .map(|i| m.buffer_at(SLAB_META + i * 8, 8).unwrap())
            .collect();
        assert_eq!(m.pinned_count(), 5);
        drop(held);
        assert_eq!(m.pinned_count(), 0);
    }
}
