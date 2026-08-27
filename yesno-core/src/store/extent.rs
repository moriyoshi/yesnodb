//! On-disk chunk addressing: [`ChunkKey`], [`ChunkRef`], the size-class ladder,
//! and [`ExtTrailer`].
//!
//! # There is no extent header
//!
//! An extent is a bare payload. Under invariant **I2** a published extent is
//! immutable, so a superseded extent would keep a perfectly valid header naming
//! a chunk that no longer points at it — a second source of truth guaranteed to
//! go stale, in a design whose stated rule is that *the index is the sole
//! authority* on liveness. Allocator rebuild therefore scans the index, which is
//! also strictly cheaper: one ordered B+tree walk, versus scanning every slot and
//! back-checking each against the index anyway.
//!
//! What remains at the tail of a slot is an 8-byte [`ExtTrailer`], whose
//! `ckey_tag` detects a *mis-pointed* `ChunkRef` without attempting identity
//! reconstruction — which is never needed, since losing both superblock roots
//! loses the file regardless.

use crate::error::{CodecError, Result};
use crate::{ContainerKind, Prefix48};

/// Identity of one chunk: `(key << 48) | prefix48`.
///
/// 112 significant bits of a `u128`. Ordered scan of every chunk for one key is
/// the range `[key << 48, (key + 1) << 48)`, so a single `u128` compare drives
/// the whole index descent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChunkKey(pub u128);

/// Significant bits in a [`ChunkKey`]. Not 128 — sizing anything to 128 wastes
/// two bytes per index entry, which matters at one entry per chunk.
pub const CHUNKKEY_BITS: u32 = 64 + 48;
/// Bytes needed to store a `ChunkKey` losslessly.
pub const CHUNKKEY_BYTES: usize = 14;

impl ChunkKey {
    #[inline]
    pub const fn new(key: u64, prefix: Prefix48) -> Self {
        ChunkKey(((key as u128) << 48) | (prefix as u128 & ((1 << 48) - 1)))
    }

    #[inline]
    pub const fn key(self) -> u64 {
        (self.0 >> 48) as u64
    }

    #[inline]
    pub const fn prefix(self) -> Prefix48 {
        (self.0 & ((1 << 48) - 1)) as u64
    }

    /// First chunk of `key` — the inclusive lower bound of its range scan.
    #[inline]
    pub const fn range_start(key: u64) -> Self {
        ChunkKey::new(key, 0)
    }

    /// One past the last chunk of `key` — the exclusive upper bound.
    #[inline]
    pub const fn range_end(key: u64) -> Self {
        ChunkKey((key as u128 + 1) << 48)
    }

    /// Big-endian bytes, high-order first.
    ///
    /// Index leaves store truncated *big-endian* suffixes so that byte
    /// comparison equals integer comparison at any width. This is the one
    /// deliberately big-endian part of the format; it never reaches Arrow.
    #[inline]
    pub fn to_be_bytes(self) -> [u8; CHUNKKEY_BYTES] {
        let full = self.0.to_be_bytes(); // 16 bytes
        let mut out = [0u8; CHUNKKEY_BYTES];
        out.copy_from_slice(&full[16 - CHUNKKEY_BYTES..]);
        out
    }

    #[inline]
    pub fn from_be_bytes(b: [u8; CHUNKKEY_BYTES]) -> Self {
        let mut full = [0u8; 16];
        full[16 - CHUNKKEY_BYTES..].copy_from_slice(&b);
        ChunkKey(u128::from_be_bytes(full))
    }
}

/// 32-bit tag used by [`ExtTrailer`] to detect a mis-pointed reference.
#[inline]
pub fn ckey_tag(k: ChunkKey) -> u32 {
    // splitmix64 over the two halves; we only need avalanche, not a hash family.
    let mut z = (k.key() ^ k.prefix().rotate_left(32)).wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    ((z ^ (z >> 31)) >> 32) as u32
}

// ---------------------------------------------------------------- ChunkRef

/// Where a chunk's payload lives, and the metadata reachable without touching it.
///
/// Exactly 8 bytes. At one entry per chunk the index dominates total size for
/// sparse data, so every bit here is contested; see the module docs for what was
/// removed and where it went.
///
/// ```text
///  [ 0:40) cell      byte offset in the shard address space (1 TiB)
///  [40:56) card_m1   cardinality - 1; a full container is 0xFFFF
///  [56:58) kind      0 Array | 1 Bitmap | 2 Run
///  [58:59) inline    payload lives in this word, no extent
///  [59:60) enc       0 = raw Roaring; MUST be 0 in v1
///  [60:64) reserved  MUST be 0
/// ```
///
/// Inline variant, for `card <= 3` — zero extents and zero page faults for the
/// tiny containers that dominate a sparse posting list:
///
/// ```text
///  [ 0:16) v0   [16:32) v1   [32:48) v2   (unused slots are zero)
///  [48:56) reserved   [56:58) kind = Array   [58:59) inline = 1
///  [59:61) n_m1 (0..2 => card 1..3)          [61:64) reserved
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChunkRef(u64);

/// Maximum values stored inline in a [`ChunkRef`].
///
/// A capacity limit, not an optimum: inline strictly dominates an extent
/// wherever it is legal, so this is simply what the word physically holds.
pub const INLINE_MAX: usize = 3;

/// Addressable byte offset. 40 bits is 1 TiB, 8x the 128 GiB per-shard cap.
pub const CELL_BITS: u32 = 40;
pub const CELL_MAX: u64 = (1 << CELL_BITS) - 1;

impl ChunkRef {
    /// A reference to an out-of-line extent.
    pub fn extent(cell: u64, kind: ContainerKind, card: u32) -> Result<Self> {
        if cell > CELL_MAX {
            return Err(CodecError::Invariant("extent offset exceeds 40-bit cell"));
        }
        if card == 0 || card > crate::CHUNK_CARD {
            return Err(CodecError::BadCardinality(card));
        }
        Ok(ChunkRef(
            cell | (((card - 1) as u64) << 40) | ((kind as u64) << 56),
        ))
    }

    /// An inline reference holding the values directly. `vals` must be sorted,
    /// unique and at most [`INLINE_MAX`] long.
    pub fn inline(vals: &[u16]) -> Result<Self> {
        if vals.is_empty() || vals.len() > INLINE_MAX {
            return Err(CodecError::Invariant(
                "inline container must hold 1..=3 values",
            ));
        }
        let mut w = 0u64;
        for (i, &v) in vals.iter().enumerate() {
            w |= (v as u64) << (16 * i);
        }
        w |= (ContainerKind::Array as u64) << 56;
        w |= 1 << 58;
        w |= ((vals.len() - 1) as u64) << 59;
        Ok(ChunkRef(w))
    }

    #[inline]
    pub const fn from_bits(w: u64) -> Self {
        ChunkRef(w)
    }

    #[inline]
    pub const fn to_bits(self) -> u64 {
        self.0
    }

    #[inline]
    pub const fn is_inline(self) -> bool {
        self.0 & (1 << 58) != 0
    }

    /// Discriminant **3 resolves to `Array`** through the wildcard arm. That
    /// is deliberate here and checked elsewhere: this is a `const fn` returning
    /// a `ContainerKind`, with nowhere to put an error. [`ChunkRef::validate`]
    /// is what refuses it, and the read path calls that first.
    #[inline]
    pub const fn kind(self) -> ContainerKind {
        match (self.0 >> 56) & 0b11 {
            1 => ContainerKind::Bitmap,
            2 => ContainerKind::Run,
            _ => ContainerKind::Array,
        }
    }

    /// Cardinality, answerable **without touching the payload**.
    ///
    /// This is the highest-value field in the format: it makes `cardinality`,
    /// `len_in_range` and `range_summary` pure index operations, which in turn
    /// makes the cardinality identities and query-planner statistics free.
    #[inline]
    pub const fn cardinality(self) -> u32 {
        if self.is_inline() {
            (((self.0 >> 59) & 0b11) as u32) + 1
        } else {
            (((self.0 >> 40) & 0xFFFF) as u32) + 1
        }
    }

    /// A container covering its whole chunk. O(1) from the index alone.
    #[inline]
    pub const fn is_full(self) -> bool {
        !self.is_inline() && ((self.0 >> 40) & 0xFFFF) == 0xFFFF
    }

    /// Byte offset of the payload. `None` when inline.
    #[inline]
    pub const fn cell(self) -> Option<u64> {
        if self.is_inline() {
            None
        } else {
            Some(self.0 & CELL_MAX)
        }
    }

    /// Values carried inline, if any.
    pub fn inline_values(self) -> Option<Vec<u16>> {
        if !self.is_inline() {
            return None;
        }
        let n = self.cardinality() as usize;
        Some(
            (0..n)
                .map(|i| ((self.0 >> (16 * i)) & 0xFFFF) as u16)
                .collect(),
        )
    }

    /// Payload length in bytes, derived entirely from the reference.
    ///
    /// Arrays and bitmaps are determined outright. A run payload is
    /// self-describing via the `nruns` prefix the Roaring spec mandates, so its
    /// length needs one dependent read from the payload's own first word —
    /// which is why `nelem` could be deleted from this struct.
    pub fn payload_len(self, run_nruns: Option<u32>) -> Result<usize> {
        if self.is_inline() {
            return Ok(0);
        }
        Ok(match self.kind() {
            ContainerKind::Array => crate::array_bytes(self.cardinality()),
            ContainerKind::Bitmap => crate::BITMAP_BYTES,
            ContainerKind::Run => {
                let n = run_nruns.ok_or(CodecError::Invariant(
                    "run payload length needs its nruns prefix",
                ))?;
                crate::run_bytes(n)
            }
        })
    }

    /// Payload length, resolving a run's `nruns` prefix through `read_nruns`.
    ///
    /// [`ChunkRef::payload_len`] deliberately cannot size a run on its own and
    /// returns `Err` without the prefix. Three call sites need that length —
    /// `fsck::rebuild`, and the two places a superseded packed chunk is
    /// returned to its page's live-byte count — and two of them were spelling
    /// the dependent read as `payload_len(None).unwrap_or(0)`, which is `Err`
    /// for **every** run and therefore subtracted nothing.
    ///
    /// The consequence was not a rounding error: a packed page whose live count
    /// never reaches zero is never queued for reclamation, so any page holding
    /// one run container leaked in full. This exists so that the dependent read
    /// is written once and cannot be skipped by accident.
    pub fn payload_len_with(self, read_nruns: impl FnOnce(u64) -> Result<u32>) -> Result<usize> {
        if self.is_inline() {
            return Ok(0);
        }
        if self.kind() == ContainerKind::Run {
            let cell = self
                .cell()
                .ok_or(CodecError::Invariant("a non-inline run has no cell"))?;
            return Ok(crate::run_bytes(read_nruns(cell)?));
        }
        self.payload_len(None)
    }

    /// Reject a reference a v1 reader must not act on.
    ///
    /// **Not a corruption check.** [`ExtTrailer::ckey_tag`] and the packed
    /// page header cover corruption; this is a *version gate*. The distinction
    /// is why it is called on the read path ( `ShardStore::read_container_for` )
    /// and not only from `fsck`, which is where it used to be called from
    /// exclusively — a gate nothing runs on the path it guards is a comment.
    pub fn validate(self) -> Result<()> {
        // `kind` is two bits with three assigned, so 3 is unused — and
        // `Self::kind` resolves it to `Array` through a wildcard arm, because it
        // is a `const fn` with nowhere to put an error. For a *forward*
        // compatibility question that is the wrong default: if a fourth
        // container kind is ever given discriminant 3, an older binary reading a
        // newer file would decode the payload as corruption-free `Array` data
        // and return wrong answers **silently**, where it must refuse the file.
        // The inline arm below catches this too, via its kind check; the
        // out-of-line arm did not, and that is the reachable case.
        if (self.0 >> 56) & 0b11 == 3 {
            return Err(CodecError::Invariant(
                "ChunkRef kind 3 is reserved: file written by a newer format",
            ));
        }
        if self.0 & (1 << 59) != 0 && !self.is_inline() {
            // enc = 1 selects an alternative array encoding we do not implement.
            return Err(CodecError::UnsupportedEncoding);
        }
        if self.is_inline() {
            if self.kind() != ContainerKind::Array {
                return Err(CodecError::Invariant("inline container must be an array"));
            }
            // **`n_m1` is two bits and only three of its four values are
            // assigned**, exactly like `kind` above, and until 2026-09-07 the
            // fourth was accepted. `n_m1 = 3` reports cardinality 4 while the
            // word holds three 16-bit slots, so `inline_values` read
            // `[48:64)` — the reserved bits, `kind`, `inline` and `n_m1`
            // themselves — as a fourth ordinal. A reference built from
            // `inline( &[10, 20, 30] )` with `n_m1` forced to 3 validated
            // clean and yielded `[10, 20, 30, 7168]`.
            //
            // The same silent-wrong-answer shape the `kind == 3` arm above
            // exists to prevent, and reachable the same way: `from_bits` is how
            // a B+tree node decodes a reference, so a file written by a newer
            // format reaches here with a valid checksum. Refusing is the only
            // safe answer for an encoding this reader does not implement.
            if (self.0 >> 59) & 0b11 == 3 {
                return Err(CodecError::Invariant(
                    "inline n_m1 = 3 is reserved: file written by a newer format",
                ));
            }
            // Reserved inline bits, refused for the reason the out-of-line arm
            // refuses its own: a reader that ignores them cannot tell a future
            // format from a valid file.
            if (self.0 >> 61) != 0 || (self.0 >> 48) & 0xFF != 0 {
                return Err(CodecError::Invariant("reserved ChunkRef bits must be zero"));
            }
            let vals = self.inline_values().expect("inline");
            if vals.windows(2).any(|w| w[0] >= w[1]) {
                return Err(CodecError::Invariant(
                    "inline values not strictly ascending",
                ));
            }
        } else if (self.0 >> 60) != 0 {
            return Err(CodecError::Invariant("reserved ChunkRef bits must be zero"));
        }
        Ok(())
    }
}

impl std::fmt::Debug for ChunkRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut d = f.debug_struct("ChunkRef");
        d.field("kind", &self.kind())
            .field("card", &self.cardinality());
        match self.cell() {
            Some(c) => d.field("cell", &c),
            None => d.field("inline", &self.inline_values().unwrap()),
        };
        d.finish()
    }
}

// -------------------------------------------------------------- ExtTrailer

/// Tail of a standalone extent slot, at `slot_base + class_size - 8`.
///
/// A fixed position, so it is findable without knowing the payload length, and
/// a fixed CRC extent. Packed pages carry no trailer — their page header's CRC
/// and `[first, last]` range serve both roles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExtTrailer {
    pub ckey_tag: u32,
    pub crc32c: u32,
}

pub const EXT_TRAILER_BYTES: usize = 8;

impl ExtTrailer {
    #[inline]
    pub fn to_le_bytes(self) -> [u8; EXT_TRAILER_BYTES] {
        let mut b = [0u8; EXT_TRAILER_BYTES];
        b[0..4].copy_from_slice(&self.ckey_tag.to_le_bytes());
        b[4..8].copy_from_slice(&self.crc32c.to_le_bytes());
        b
    }

    #[inline]
    pub fn from_le_bytes(b: [u8; EXT_TRAILER_BYTES]) -> Self {
        ExtTrailer {
            ckey_tag: u32::from_le_bytes(b[0..4].try_into().unwrap()),
            crc32c: u32::from_le_bytes(b[4..8].try_into().unwrap()),
        }
    }
}

// ------------------------------------------------------------- size classes

/// Slot size for the packed-page class. Packed pages hold many small
/// array/run payloads; see `store::packed`.
pub const PACKED_CLASS: u8 = 0;

/// Largest payload packed rather than given its own extent.
///
/// 2028, not 512. Tail waste per chunk is `4096/floor(4056/L) - L`, which is
/// non-monotonic and argues for 512 in the *monodisperse worst case* — but that
/// case does not arise in the sparse regime ( tiny chunks fill every tail, and
/// utilization is 96-98% at any cap ), while clustered corpora do produce
/// payloads near 1310 B where a 512 cap makes nothing packable and costs 17%.
pub const PACK_MAX: usize = 2028;

/// Slot sizes, indexed by class. Entry 0 is the packed-page class.
///
/// Every entry is a multiple of 64. Because slab bases are 2 MiB aligned, that
/// makes **every slot in every class 64-byte aligned** — so bitmap alignment is
/// a property of the ladder rather than a per-kind rule an allocator change
/// could silently break.
///
/// The ladder is shifted up by 64 from the obvious powers of two ( 576 not 512,
/// 2112 not 2048 ) so that a power-of-two payload plus its 8-byte trailer still
/// fits exactly. Without the shift a 2048-byte payload would round to 3072 and
/// waste 33%.
pub const CLASS_SIZES: [u32; 11] = [
    4096, // 0  PACKED
    576,  // 1  payload <=  568
    704,  // 2           <=  696
    896,  // 3           <=  888
    1088, // 4           <= 1080
    1600, // 5           <= 1592
    2112, // 6           <= 2104   exact for 2048
    3136, // 7           <= 3128
    4160, // 8           <= 4152   exact for 4096
    6208, // 9           <= 6200
    8256, // 10          <= 8248   exact for 8192
];

/// Smallest class whose slot holds `payload_len` plus a trailer, or `None` if
/// the payload exceeds the largest class.
pub fn class_for(payload_len: usize) -> Option<u8> {
    let need = payload_len + EXT_TRAILER_BYTES;
    CLASS_SIZES
        .iter()
        .enumerate()
        .skip(1) // class 0 is packed pages, never chosen by payload size
        .find(|(_, &sz)| sz as usize >= need)
        .map(|(i, _)| i as u8)
}

#[inline]
pub fn class_size(class: u8) -> Option<u32> {
    CLASS_SIZES.get(class as usize).copied()
}

/// Round up to the next multiple of 64.
#[inline]
pub const fn round_up_64(n: usize) -> usize {
    n.div_ceil(64) * 64
}

/// Assert the ladder's invariants. Called at open and by `fsck`, because the
/// ladder is persisted in the superblock and a foreign file could carry one that
/// breaks the alignment guarantee the whole format rests on.
pub fn validate_ladder(sizes: &[u32]) -> Result<()> {
    if sizes.len() < 2 {
        return Err(CodecError::Invariant("size-class ladder too short"));
    }
    for &s in sizes {
        if s % 64 != 0 {
            return Err(CodecError::Invariant(
                "every class size must be a multiple of 64",
            ));
        }
    }
    // Classes 1.. must ascend; class 0 (packed) sits outside that ordering.
    if sizes[1..].windows(2).any(|w| w[0] >= w[1]) {
        return Err(CodecError::Invariant("size classes must strictly ascend"));
    }
    let top = *sizes.last().unwrap() as usize;
    if top < crate::BITMAP_BYTES + EXT_TRAILER_BYTES {
        return Err(CodecError::Invariant(
            "top class cannot hold a bitmap payload",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunkkey_roundtrip_and_ordering() {
        let k = ChunkKey::new(0xDEAD_BEEF_CAFE_1234, 0x0000_ABCD_EF01);
        assert_eq!(k.key(), 0xDEAD_BEEF_CAFE_1234);
        assert_eq!(k.prefix(), 0x0000_ABCD_EF01);
        assert_eq!(ChunkKey::from_be_bytes(k.to_be_bytes()), k);

        // Big-endian byte order must equal integer order, which is what lets the
        // index binary-search truncated suffixes with a plain integer compare.
        let a = ChunkKey::new(1, 5);
        let b = ChunkKey::new(1, 6);
        let c = ChunkKey::new(2, 0);
        assert!(a < b && b < c);
        assert!(a.to_be_bytes() < b.to_be_bytes());
        assert!(b.to_be_bytes() < c.to_be_bytes());
    }

    #[test]
    fn chunkkey_range_covers_exactly_one_key() {
        let key = 42u64;
        let lo = ChunkKey::range_start(key);
        let hi = ChunkKey::range_end(key);
        assert!(ChunkKey::new(key, 0) >= lo);
        assert!(ChunkKey::new(key, (1 << 48) - 1) < hi);
        assert!(ChunkKey::new(key + 1, 0) >= hi);
        assert!(ChunkKey::new(key - 1, (1 << 48) - 1) < lo);
    }

    #[test]
    fn chunkkey_uses_112_bits_not_128() {
        let k = ChunkKey::new(u64::MAX, (1 << 48) - 1);
        assert_eq!(
            ChunkKey::from_be_bytes(k.to_be_bytes()),
            k,
            "no truncation at the top"
        );
        assert_eq!(CHUNKKEY_BYTES * 8, CHUNKKEY_BITS as usize);
    }

    #[test]
    fn extent_ref_roundtrip() {
        let r = ChunkRef::extent(1234, ContainerKind::Bitmap, 5000).unwrap();
        assert!(!r.is_inline());
        assert_eq!(r.cell(), Some(1234));
        assert_eq!(r.kind(), ContainerKind::Bitmap);
        assert_eq!(r.cardinality(), 5000);
        assert!(!r.is_full());
        r.validate().unwrap();
        assert_eq!(ChunkRef::from_bits(r.to_bits()), r);
    }

    #[test]
    fn full_container_detected_from_the_index_alone() {
        let r = ChunkRef::extent(64, ContainerKind::Run, crate::CHUNK_CARD).unwrap();
        assert!(r.is_full());
        assert_eq!(r.cardinality(), 65536);
    }

    #[test]
    fn inline_ref_roundtrip() {
        for vals in [vec![7u16], vec![1, 2], vec![0, 32000, 65535]] {
            let r = ChunkRef::inline(&vals).unwrap();
            assert!(r.is_inline());
            assert_eq!(r.cell(), None, "an inline chunk occupies no extent");
            assert_eq!(r.cardinality(), vals.len() as u32);
            assert_eq!(r.inline_values().unwrap(), vals);
            assert_eq!(r.payload_len(None).unwrap(), 0);
            r.validate().unwrap();
        }
    }

    #[test]
    fn inline_rejects_out_of_range_input() {
        assert!(ChunkRef::inline(&[]).is_err());
        assert!(ChunkRef::inline(&[1, 2, 3, 4]).is_err(), "capacity is 3");
    }

    #[test]
    fn chunkref_is_exactly_eight_bytes() {
        assert_eq!(std::mem::size_of::<ChunkRef>(), 8);
    }

    #[test]
    fn cell_bound_is_enforced() {
        assert!(ChunkRef::extent(CELL_MAX, ContainerKind::Array, 1).is_ok());
        assert!(ChunkRef::extent(CELL_MAX + 1, ContainerKind::Array, 1).is_err());
    }

    #[test]
    fn payload_len_derives_from_the_reference() {
        let a = ChunkRef::extent(0, ContainerKind::Array, 6).unwrap();
        assert_eq!(a.payload_len(None).unwrap(), 12);

        let b = ChunkRef::extent(0, ContainerKind::Bitmap, 5000).unwrap();
        assert_eq!(b.payload_len(None).unwrap(), crate::BITMAP_BYTES);

        // A run needs its self-describing nruns prefix; that dependency is
        // exactly what allowed `nelem` to be deleted from ChunkRef.
        let r = ChunkRef::extent(0, ContainerKind::Run, 100).unwrap();
        assert!(r.payload_len(None).is_err());
        assert_eq!(r.payload_len(Some(7)).unwrap(), 2 + 4 * 7);
    }

    #[test]
    fn validate_rejects_unsupported_encoding() {
        let bad = ChunkRef::from_bits(
            ChunkRef::extent(0, ContainerKind::Array, 1)
                .unwrap()
                .to_bits()
                | (1 << 59),
        );
        assert!(matches!(
            bad.validate(),
            Err(CodecError::UnsupportedEncoding)
        ));
    }

    /// Discriminant 3 must be refused rather than silently read as `Array`.
    ///
    /// The hazard is forward-compatibility, not corruption: nothing writes a 3
    /// today, so this guards the day something does. `kind()` resolves it to
    /// `Array` — asserted here, so the wildcard arm is documented by a test
    /// rather than only by a comment — which is exactly why `validate` has to be
    /// the thing that refuses it.
    /// The inline variant's reserved encodings are refused, not read.
    ///
    /// **`n_m1 = 3` is the sharp one.** Two bits, three assigned values, and
    /// the fourth used to validate clean: `cardinality()` reported 4 against
    /// three 16-bit slots, so `inline_values` read the header word itself as an
    /// ordinal and returned `[10, 20, 30, 7168]`. That is the silent wrong
    /// answer `validate_refuses_the_reserved_kind_discriminant` exists to
    /// prevent, in the arm that did not have it.
    ///
    /// Do not relax any of these three to "tolerate unknown bits". A reader
    /// that ignores a reserved field cannot distinguish a newer format from a
    /// valid file, which is the whole purpose of this gate.
    #[test]
    fn validate_refuses_the_inline_reserved_encodings() {
        let good = ChunkRef::inline(&[10, 20, 30]).unwrap();
        assert_eq!(
            good.validate(),
            Ok(()),
            "the shape under test must be legal"
        );

        // n_m1 = 3: claims a fourth value with no slot to hold it.
        let n3 = ChunkRef::from_bits(good.to_bits() | (0b11u64 << 59));
        assert_eq!(n3.cardinality(), 4, "the decode this refuses");
        assert!(
            n3.validate().is_err(),
            "n_m1 = 3 must be refused; it yielded {:?}",
            n3.inline_values()
        );

        // The two reserved windows of the inline layout.
        for (bits, what) in [(0b111u64 << 61, "[61:64)"), (0xFFu64 << 48, "[48:56)")] {
            let r = ChunkRef::from_bits(good.to_bits() | bits);
            assert!(r.validate().is_err(), "reserved {what} must be refused");
        }
    }

    #[test]
    fn validate_refuses_the_reserved_kind_discriminant() {
        let good = ChunkRef::extent(64, ContainerKind::Run, 10).unwrap();
        good.validate().unwrap();

        // Kind 2 ( Run ) with the high kind bit set is kind 3.
        let bad = ChunkRef::from_bits(good.to_bits() | (1 << 57) | (1 << 56));
        assert_eq!(
            bad.kind(),
            ContainerKind::Array,
            "the wildcard arm is why validate must catch this"
        );
        assert!(
            matches!(bad.validate(), Err(CodecError::Invariant(_))),
            "kind 3 must be refused, not decoded as Array"
        );

        // The inline form already refused it, via its kind check. Pinned so the
        // two arms are not assumed to behave alike.
        let inline = ChunkRef::inline(&[1, 2]).unwrap();
        inline.validate().unwrap();
        let bad_inline = ChunkRef::from_bits(inline.to_bits() | (1 << 57) | (1 << 56));
        assert!(bad_inline.validate().is_err(), "inline kind 3 too");
    }

    #[test]
    fn ckey_tag_distinguishes_neighbours() {
        // Adjacent chunk keys must not collide, or a mis-pointed reference to the
        // next chunk over would go undetected.
        let a = ckey_tag(ChunkKey::new(5, 100));
        let b = ckey_tag(ChunkKey::new(5, 101));
        let c = ckey_tag(ChunkKey::new(6, 100));
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_ne!(b, c);
    }

    #[test]
    fn trailer_roundtrip() {
        let t = ExtTrailer {
            ckey_tag: 0xDEAD_BEEF,
            crc32c: 0x1234_5678,
        };
        assert_eq!(ExtTrailer::from_le_bytes(t.to_le_bytes()), t);
    }

    #[test]
    fn ladder_invariants_hold() {
        validate_ladder(&CLASS_SIZES).unwrap();

        // Every slot 64-byte aligned is what makes bitmap alignment a property
        // of the ladder rather than a per-kind rule.
        for &s in &CLASS_SIZES {
            assert_eq!(s % 64, 0, "class size {s} breaks 64-byte alignment");
        }
        // Adjacent ratio bounds internal fragmentation.
        for w in CLASS_SIZES[1..].windows(2) {
            let ratio = w[1] as f64 / w[0] as f64;
            assert!(ratio <= 1.5, "adjacent class ratio {ratio} exceeds 1.5");
        }
    }

    #[test]
    fn ladder_rejects_a_broken_foreign_ladder() {
        assert!(
            validate_ladder(&[4096, 100]).is_err(),
            "not a multiple of 64"
        );
        assert!(validate_ladder(&[4096, 576, 576]).is_err(), "not ascending");
        assert!(
            validate_ladder(&[4096, 576]).is_err(),
            "cannot hold a bitmap"
        );
    }

    #[test]
    fn top_class_is_exact_fit_for_a_bitmap_and_a_full_array() {
        let bitmap_class = class_for(crate::BITMAP_BYTES).unwrap();
        assert_eq!(class_size(bitmap_class).unwrap(), 8256);

        // A 4096-element array has the same 8192-byte payload, so it lands in the
        // same class: array->bitmap promotion is a same-class rewrite with no
        // slab migration. Preserve this when editing the ladder.
        let full_array_class = class_for(crate::array_bytes(crate::ARRAY_MAX as u32)).unwrap();
        assert_eq!(full_array_class, bitmap_class);
    }

    #[test]
    fn class_selection_is_tight() {
        assert_eq!(class_size(class_for(1).unwrap()).unwrap(), 576);
        assert_eq!(class_size(class_for(568).unwrap()).unwrap(), 576);
        assert_eq!(class_size(class_for(569).unwrap()).unwrap(), 704);
        // Power-of-two payloads stay exact-fit thanks to the +64 shift.
        assert_eq!(class_size(class_for(2048).unwrap()).unwrap(), 2112);
        assert_eq!(class_size(class_for(4096).unwrap()).unwrap(), 4160);
        assert_eq!(class_for(8249), None, "beyond the top class");
    }

    #[test]
    fn round_up_64_is_exact_on_multiples() {
        assert_eq!(round_up_64(0), 0);
        assert_eq!(round_up_64(1), 64);
        assert_eq!(round_up_64(64), 64);
        assert_eq!(round_up_64(65), 128);
    }
}
