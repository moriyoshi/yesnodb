//! The table AM's TID packing — **different from the index AM's**.
//!
//! # Two packings, and why they cannot be one
//!
//! This is the sharpest footgun in the extension. Both packings are
//! `u64 → ( u32, u16 )` with identical signatures, and confusing them yields
//! plausible TIDs pointing at wrong rows.
//!
//! | | who chooses the TID | offset range | packing |
//! |---|---|---|---|
//! | [`crate::iam::tid`] | **PostgreSQL** — TIDs come from a real heap | `1 ..= MaxOffsetNumber` as stored | `( block << 16 ) \| offset` |
//! | this module | **we do** — the tuple *is* its TID | `1 ..= ORDINALS_PER_BLOCK` | `block * ORDINALS_PER_BLOCK + ( offset - 1 )` |
//!
//! The index AM can use the full 16-bit offset field because it only ever
//! *reproduces* a TID PostgreSQL already assigned. A table AM must *invent*
//! TIDs, and an invented offset has to be one PostgreSQL will accept: at most
//! `MaxOffsetNumber`, which is `( BLCKSZ - SizeOfPageHeaderData ) /
//! sizeof( ItemIdData )` — 2042 at the default 8 KiB, not 65535.
//!
//! # The domain restriction that follows
//!
//! Because the tuple is its TID and a TID is `( u32, offset )`, the value
//! domain caps at [`TAM_ORDINAL_MAX`] ≈ 2^42 — not the full `u64` an ordinal can
//! be. A yesno table therefore holds a **subset** of what a yesno *set* can
//! hold, and an insert above the cap is rejected rather than truncated.
//!
//! There is no way around this short of storing the value separately from its
//! identity, which means storing tuples, which is a heap.

/// Ordinals per block. A power of two well under `MaxOffsetNumber` ( 2042 at
/// the default 8 KiB `BLCKSZ` ), so the arithmetic is shifts and the offset is
/// always acceptable to PostgreSQL.
///
/// Do not raise this to 2048: `MaxOffsetNumber` is a function of `BLCKSZ`,
/// and a cluster built with a smaller block size would then produce offsets
/// PostgreSQL rejects — at insert time, on data that was fine yesterday.
pub const ORDINALS_PER_BLOCK: u64 = 1024;

/// The largest ordinal a yesno **table** can hold.
///
/// Far below `ORDINAL_MAX`. See this module's header.
pub const TAM_ORDINAL_MAX: u64 = (u32::MAX as u64) * ORDINALS_PER_BLOCK + (ORDINALS_PER_BLOCK - 1);

/// The TID that carries `ordinal`, or `None` if it is out of the table's domain.
#[inline]
pub const fn ordinal_to_tid(ordinal: u64) -> Option<(u32, u16)> {
    if ordinal > TAM_ORDINAL_MAX {
        return None;
    }
    let block = (ordinal / ORDINALS_PER_BLOCK) as u32;
    // `+ 1` because PostgreSQL numbers items from one; offset zero is not a
    // valid `OffsetNumber` and a TID carrying it is rejected by the executor.
    let offset = (ordinal % ORDINALS_PER_BLOCK) as u16 + 1;
    Some((block, offset))
}

/// The ordinal a TID carries, or `None` if the TID is not one we minted.
#[inline]
pub const fn tid_to_ordinal(block: u32, offset: u16) -> Option<u64> {
    // An offset outside the range this packing produces is not ours. It could
    // come from an index built when `ORDINALS_PER_BLOCK` differed, and folding
    // it in would silently name a different row.
    if offset == 0 || offset as u64 > ORDINALS_PER_BLOCK {
        return None;
    }
    Some((block as u64) * ORDINALS_PER_BLOCK + (offset as u64 - 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ordinal_round_trips_through_a_tid() {
        let cases = [
            0u64,
            1,
            ORDINALS_PER_BLOCK - 1,
            ORDINALS_PER_BLOCK,
            ORDINALS_PER_BLOCK + 1,
            12345,
            TAM_ORDINAL_MAX - 1,
            TAM_ORDINAL_MAX,
        ];
        for o in cases {
            let (b, off) = ordinal_to_tid(o).expect("within the domain");
            assert_eq!(tid_to_ordinal(b, off), Some(o), "ordinal {o}");
        }
    }

    /// Every offset this packing produces must be one PostgreSQL accepts.
    /// `MaxOffsetNumber` is 2042 at the default 8 KiB block size.
    #[test]
    fn every_offset_is_a_valid_offset_number() {
        for o in [0u64, 1, ORDINALS_PER_BLOCK - 1, TAM_ORDINAL_MAX] {
            let (_, off) = ordinal_to_tid(o).unwrap();
            assert!(off >= 1, "offset {off} must be at least 1");
            assert!(
                off as u64 <= ORDINALS_PER_BLOCK,
                "offset {off} exceeds the packing's range"
            );
            assert!(off < 2042, "offset {off} exceeds MaxOffsetNumber at 8 KiB");
        }
    }

    /// Out of domain must be rejected, not wrapped. An insert above the cap
    /// is a value this table cannot represent.
    #[test]
    fn an_ordinal_above_the_cap_has_no_tid() {
        assert_eq!(ordinal_to_tid(TAM_ORDINAL_MAX + 1), None);
        assert_eq!(ordinal_to_tid(u64::MAX), None);
        assert_eq!(ordinal_to_tid(1 << 43), None);
    }

    #[test]
    fn a_tid_we_did_not_mint_is_rejected() {
        assert_eq!(tid_to_ordinal(0, 0), None, "offset zero is not valid");
        assert_eq!(
            tid_to_ordinal(0, (ORDINALS_PER_BLOCK + 1) as u16),
            None,
            "an offset past the packing's range is not ours"
        );
        assert_eq!(tid_to_ordinal(0, u16::MAX), None);
    }

    /// **The footgun test.** The two packings agree only where the offset is
    /// 1 and the block is 0 — everywhere else they name different things, and a
    /// call site that reached for the wrong one would produce plausible TIDs
    /// pointing at wrong rows.
    #[test]
    fn the_two_packings_are_genuinely_different() {
        use crate::iam::tid as iam;

        // Same ordinal, different TID.
        let o = 5000u64;
        let (tam_b, tam_off) = ordinal_to_tid(o).unwrap();
        let (iam_b, iam_off) = iam::ordinal_to_tid(o);
        assert_ne!(
            (tam_b, tam_off),
            (iam_b, iam_off),
            "the packings must not silently coincide"
        );

        // Same TID, different ordinal.
        let (b, off) = (3u32, 7u16);
        assert_ne!(
            tid_to_ordinal(b, off).unwrap(),
            iam::tid_to_ordinal(b, off),
            "reading a TID with the wrong packing yields a different ordinal"
        );

        // The domains differ too: the index AM spans the full 48-bit TID space.
        assert!(TAM_ORDINAL_MAX < (1u64 << 48));
    }
}
