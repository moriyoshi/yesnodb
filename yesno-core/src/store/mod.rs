//! Persistent storage: extent addressing, slab allocation, and the mmap'd page
//! store.
//!
//! # Invariants
//!
//! These are named here because the rest of the module reasons in terms of them,
//! and because several are load-bearing far beyond their apparent scope.
//!
//! - **I1 (LE-only)** — the file format is little-endian. Opening on a
//!   big-endian host is refused rather than byte-swapped.
//! - **I2 (Extent immutability)** — a published extent is never mutated in
//!   place. Updates allocate a new extent. This is not a preference: containers
//!   hand out slices aliasing the mapping, so writing through one that a live
//!   snapshot can see is undefined behaviour, not merely a torn read.
//! - **I3 (Alloc-at-checkpoint)** — extent allocation happens only in the
//!   checkpointer, single-threaded and batched. The write path never allocates
//!   file space. This is what makes the deferred free list need no durability
//!   at all, and what keeps WAL replay from ever touching the allocator.
//! - **I4 (Checkpoint barrier)** — a checkpoint persists only state at or below
//!   the global visible watermark, so the on-disk image is always a consistent
//!   snapshot.
//! - **I5 (Per-shard cv monotonicity)** — each shard's WAL strictly increases in
//!   commit version.
//! - **I6 (Append-only mappings)** — mmap segments are created, never recreated,
//!   never unmapped while a `Buffer` may point into them, and the file never
//!   shrinks. Truncating under a live mapping raises SIGBUS, which is not
//!   catchable as a `Result`.
//! - **I7 (Key locality)** — all chunks of one key live in one shard.

pub mod alloc;
pub mod checksum;
pub mod extent;
pub mod fsck;
pub mod packed;
pub(crate) mod segment;
pub mod slabmeta;
pub mod superblock;

pub use checksum::crc32c;
pub use extent::{
    class_for, class_size, ChunkKey, ChunkRef, ExtTrailer, CLASS_SIZES, INLINE_MAX, PACK_MAX,
};

/// Slab size. Equal to the PMD huge-page size, and slabs are aligned to it, so
/// one slab is exactly one huge page or folio and **no extent straddles a folio
/// boundary** — the same reasoning as I6b one level down. Anything changing this
/// must preserve that.
pub const SLAB_SIZE: u64 = 2 * 1024 * 1024;

/// Per-slab metadata reserved at the head of each slab.
pub const SLAB_META: u64 = 8192;

/// Bytes of a slab available to objects.
pub const SLAB_BODY: u64 = SLAB_SIZE - SLAB_META;

/// Base page size: index nodes and the superblock.
pub const PAGE: usize = 4096;

/// Index node size.
///
/// 1 KiB, not the OS page size. Under a copy-on-write tree the node size sets
/// *write* amplification, and 4 KiB nodes rewrite ~4 KiB of index per changed
/// chunk — modelled at roughly 800x amplification at a realistic ingest rate,
/// which is the dominant write cost in the system. 1 KiB cuts that ~60% for
/// about 7% more index bytes and one extra tree level, which is nearly free
/// because upper levels stay cached.
pub const INDEX_NODE: usize = 1024;
