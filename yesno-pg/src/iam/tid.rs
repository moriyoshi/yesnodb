//! The TID ↔ ordinal mapping, and how an indexed value becomes a key.
//!
//! # Why an index AM over yesno is more than an adapter
//!
//! A PostgreSQL `ItemPointerData` is a 32-bit block number and a 16-bit offset —
//! **48 bits**. A yesno ordinal splits into a 48-bit `Prefix48` and a 16-bit
//! slot. Set `ordinal = ( block << 16 ) | offset` and the two coincide exactly:
//!
//! ```text
//!   TID      =  block:32                   | offset:16
//!   ordinal  =  prefix48 ( 32 used )        | low16
//!              \___ one yesno chunk ___/      \_ one heap tuple _/
//! ```
//!
//! **One container is exactly one heap block.** `Container::len()` is O(1) on
//! every representation, so "how many tuples on this page match" is free, and
//! `amgetbitmap` becomes one `tbm_add_tuples` per chunk using the offsets the
//! container already holds.
//!
//! **The counterweight, stated rather than discovered.** A heap page holds at
//! most `MaxHeapTuplesPerPage` tuples ( ≈291 at the default 8 KiB `BLCKSZ` ), so
//! a container never approaches `ARRAY_MAX` and the 8 KiB bitmap representation
//! is never selected. Every container is an array: two bytes per posting. That
//! is competitive with GIN's posting lists — but it is a sorted-array index, not
//! a compressed-bitmap one, and the Roaring compression thesis does not apply
//! here.

/// Build an `ItemPointerData` from a block and offset.
///
/// `ItemPointerSet` is a macro in `itemptr.h`, so bindgen never emits it —
/// the same class as `ExecClearTuple` and `slot_getallattrs`. The block number
/// is stored **split across two `uint16`s**, high half first; writing it as a
/// `u32` would byte-swap it on any host.
#[inline]
pub fn make_tid(block: u32, offset: u16) -> pgrx::pg_sys::ItemPointerData {
    pgrx::pg_sys::ItemPointerData {
        ip_blkid: pgrx::pg_sys::BlockIdData {
            bi_hi: (block >> 16) as u16,
            bi_lo: (block & 0xffff) as u16,
        },
        ip_posid: offset,
    }
}

/// The block and offset an `ItemPointerData` names.
///
/// # Safety
///
/// `tid` must point at a valid `ItemPointerData`.
#[inline]
pub unsafe fn split_tid(tid: pgrx::pg_sys::ItemPointer) -> (u32, u16) {
    unsafe {
        let b = (*tid).ip_blkid;
        (((b.bi_hi as u32) << 16) | b.bi_lo as u32, (*tid).ip_posid)
    }
}

/// A heap TID as a yesno ordinal.
///
/// Offset zero is not a valid `OffsetNumber` — PostgreSQL numbers items from
/// 1 — so ordinal `block << 16` is never produced. That is harmless and worth
/// knowing: the low slot of each chunk is simply unused.
#[inline]
pub const fn tid_to_ordinal(block: u32, offset: u16) -> u64 {
    ((block as u64) << 16) | offset as u64
}

/// The inverse. Total, because every ordinal below `2^48` is a valid TID.
#[inline]
pub const fn ordinal_to_tid(ordinal: u64) -> (u32, u16) {
    (
        ((ordinal >> 16) & 0xffff_ffff) as u32,
        (ordinal & 0xffff) as u16,
    )
}

/// Whether an ordinal could have come from a TID.
///
/// Checked rather than assumed on the way *out* of a posting list. A yesno
/// key is shared namespace — nothing stops the same key holding ordinals written
/// by the foreign data wrapper — and an ordinal above `2^48` is not a TID.
/// Truncating one into a plausible-looking block number would hand the executor
/// a TID pointing at an unrelated row.
#[inline]
pub const fn is_tid_ordinal(ordinal: u64) -> bool {
    ordinal < (1u64 << 48) && (ordinal & 0xffff) != 0
}

/// Map an indexed value to the yesno key holding its posting list.
///
/// **Not injective**, and that is the whole reason `amgetbitmap` must set
/// `recheck`. Two values can hash to the same key, so the posting list is a
/// *superset* of the rows that actually match. Bitmap Heap Scan re-evaluates the
/// original qual per tuple when `recheck` is true, which removes the collisions;
/// claiming `recheck = false` would return another value's rows as genuine
/// matches.
///
/// This is the same `Exact` / `Inexact` discipline `yesno-datafusion`'s
/// `TermEncoder` documents, arriving at a different call site.
pub fn hash_datum_bytes(bytes: &[u8]) -> u64 {
    // FNV, then splitmix64. Only avalanche is required — this is a bucket
    // assignment, not a checksum — and it matches `yesno-datafusion`'s
    // `HashEncoder` so the two agree on what a term's key is.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01B3);
    }
    h = (h ^ (h >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    h = (h ^ (h >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    h ^ (h >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every TID must survive the round trip, including the boundaries where a
    /// shift or a mask would go wrong.
    #[test]
    fn a_tid_round_trips_through_an_ordinal() {
        let cases = [
            (0u32, 1u16),
            (0, 291),
            (0, u16::MAX),
            (1, 1),
            (u32::MAX, 1),
            (u32::MAX, u16::MAX),
            (0x1234_5678, 0x9abc),
        ];
        for (blk, off) in cases {
            let o = tid_to_ordinal(blk, off);
            assert_eq!(ordinal_to_tid(o), (blk, off), "TID ({blk}, {off})");
        }
    }

    /// The alignment the whole design rests on: the ordinal's chunk prefix
    /// **is** the block number, so one container is one heap block.
    #[test]
    fn the_chunk_prefix_is_the_block_number() {
        for blk in [0u32, 1, 4096, u32::MAX] {
            for off in [1u16, 7, u16::MAX] {
                let o = tid_to_ordinal(blk, off);
                assert_eq!(
                    o >> 16,
                    blk as u64,
                    "prefix48 must equal the block number for ({blk}, {off})"
                );
                assert_eq!(o & 0xffff, off as u64, "low16 must be the offset");
            }
        }
    }

    /// An ordinal that cannot be a TID must be rejected, not truncated. A key
    /// can hold ordinals written by the foreign data wrapper, and folding one
    /// into a block number would produce a TID pointing at an unrelated row.
    #[test]
    fn an_ordinal_that_is_not_a_tid_is_rejected() {
        assert!(is_tid_ordinal(tid_to_ordinal(0, 1)));
        assert!(is_tid_ordinal(tid_to_ordinal(u32::MAX, u16::MAX)));

        // Above the 48-bit TID space.
        assert!(!is_tid_ordinal(1u64 << 48));
        assert!(!is_tid_ordinal(u64::MAX - 1));
        // Offset zero is not a valid OffsetNumber.
        assert!(!is_tid_ordinal(0));
        assert!(!is_tid_ordinal(tid_to_ordinal(7, 0)));
    }

    /// The block number is split across two `uint16`s in `BlockIdData`, high
    /// half first. Round-tripping through `make_tid` / `split_tid` is what
    /// catches writing it as a `u32`, which would byte-swap it.
    #[test]
    fn an_item_pointer_round_trips() {
        for (blk, off) in [
            (0u32, 1u16),
            (1, 2),
            (0x1234_5678, 0x9abc),
            (u32::MAX, u16::MAX),
        ] {
            let mut tid = make_tid(blk, off);
            let got = unsafe { split_tid(&mut tid) };
            assert_eq!(got, (blk, off), "TID ({blk}, {off})");
        }
    }

    /// The hash need not be injective — it must only be *stable*, since the key
    /// it produces is where a value's posting list lives across restarts.
    #[test]
    fn the_key_hash_is_stable_and_value_dependent() {
        assert_eq!(hash_datum_bytes(b"rust"), hash_datum_bytes(b"rust"));
        assert_ne!(hash_datum_bytes(b"rust"), hash_datum_bytes(b"go"));
        assert_ne!(hash_datum_bytes(b""), hash_datum_bytes(b"\0"));
        // Pinned so a future edit to the mixing cannot silently relocate
        // every existing index's posting lists — the data would still be there,
        // under keys nothing looks up any more. Computed from the algorithm,
        // not copied from a run.
        assert_eq!(hash_datum_bytes(b"rust"), 0xd509_555b_ad62_4015);
        assert_eq!(hash_datum_bytes(b"go"), 0x12f9_c108_d144_2d22);
    }
}
