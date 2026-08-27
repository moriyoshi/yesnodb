//! Per-slab occupancy, written into the `SLAB_META` region each slab reserves.
//!
//! # Why this has to exist
//!
//! The region was reserved from the start and never written, so a reopened
//! shard could not tell which slots in an existing slab were live — or even what
//! size class the slab held. That had two consequences, one bad and one worse:
//!
//! - freed space in an old slab was unreachable forever, because the only safe
//!   assumption about an unknown slab is that all of it is live;
//! - and the allocator came back with *no slabs at all*, so the first
//!   allocation after a reopen claimed slab 0 and wrote over the extents
//!   already in it ( JOURNAL, 2026-08-25, bug 5 ).
//!
//! That second one was first patched by an `Allocator::reopened` constructor
//! that filled in opaque placeholder slabs. It was **deleted on 2026-09-06**,
//! uncalled: once this region is written and read back, the open path restores
//! real per-slab occupancy, which is strictly better than a placeholder. The
//! bug is fixed by *this file existing*, not by that constructor.
//!
//! # The index remains the authority
//!
//! This is a **cache of derivable state**, not a second source of truth. Every
//! byte here can be recomputed by walking the index ( `fsck::rebuild` ), which is
//! exactly what a failed CRC falls back to. That keeps the rule the rest of the
//! store follows: a superseded extent's own bytes never get a vote on whether it
//! is live.
//!
//! Being derivable is also what makes it safe to write in place. Slab metadata
//! is the one exception to shadow paging, along with the superblock, precisely
//! because a torn write here costs a rebuild rather than data.
//!
//! # Layout
//!
//! ```text
//!  0  u16  magic
//!  2  u8   version
//!  3  u8   state      0 = Free, 1 = InUse
//!  4  u8   class
//!  5  [3]  reserved
//!  8  u32  generation
//! 12  u32  used_count
//! 16  u32  capacity
//! 20  u32  crc32c     over the whole region with this field zeroed
//! 24  ..   reserved to 32
//! 32  ..   occupancy bitmap, `capacity` bits, LE u64 words
//! ```
//!
//! The widest bitmap is `SLAB_BODY / 576` = 3626 bits = 454 bytes, so the
//! reserved 8 KiB is never close to full. [`fits`] asserts that rather than
//! trusting it.

use crate::error::{CodecError, Result};
use crate::store::alloc::{slab_capacity, Slab, SlabState};
use crate::store::checksum::crc32c;
use crate::store::SLAB_META;

pub const MAGIC: u16 = 0x5953; // "SY"
pub const VERSION: u8 = 1;

const OFF_MAGIC: usize = 0;
const OFF_VER: usize = 2;
const OFF_STATE: usize = 3;
const OFF_CLASS: usize = 4;
const OFF_GEN: usize = 8;
const OFF_USED: usize = 12;
const OFF_CAP: usize = 16;
const OFF_CRC: usize = 20;
const OFF_BITMAP: usize = 32;

const STATE_FREE: u8 = 0;
const STATE_IN_USE: u8 = 1;

/// Does a slab of `capacity` slots fit its bitmap in the reserved region?
#[inline]
pub const fn fits(capacity: u32) -> bool {
    OFF_BITMAP + (capacity as usize).div_ceil(8) <= SLAB_META as usize
}

/// Encode one slab's occupancy into a `SLAB_META`-sized block.
///
/// An [`SlabState::Opaque`] slab encodes as `Free`-with-no-bitmap only if it is
/// genuinely empty; otherwise it is an error, because writing "unknown" as
/// "free" is how live extents get handed out twice.
pub fn encode(slab: &Slab) -> Result<Vec<u8>> {
    let mut b = vec![0u8; SLAB_META as usize];
    b[OFF_MAGIC..OFF_MAGIC + 2].copy_from_slice(&MAGIC.to_le_bytes());
    b[OFF_VER] = VERSION;

    match slab.state {
        SlabState::InUse { class, gen } => {
            if !fits(slab.capacity()) {
                return Err(CodecError::Invariant(
                    "slab occupancy bitmap does not fit the reserved region",
                ));
            }
            b[OFF_STATE] = STATE_IN_USE;
            b[OFF_CLASS] = class;
            b[OFF_GEN..OFF_GEN + 4].copy_from_slice(&gen.to_le_bytes());
            b[OFF_USED..OFF_USED + 4].copy_from_slice(&slab.used_count().to_le_bytes());
            b[OFF_CAP..OFF_CAP + 4].copy_from_slice(&slab.capacity().to_le_bytes());
            for (i, w) in slab.used_words().iter().enumerate() {
                let o = OFF_BITMAP + i * 8;
                b[o..o + 8].copy_from_slice(&w.to_le_bytes());
            }
        }
        SlabState::Free => b[OFF_STATE] = STATE_FREE,
        SlabState::Opaque => {
            // Refusing is the point: an Opaque slab's occupancy is unknown, and
            // persisting a guess would let a later open allocate over live data.
            return Err(CodecError::Invariant(
                "cannot persist an Opaque slab: its occupancy is unknown",
            ));
        }
    }

    let crc = crc32c(&b);
    b[OFF_CRC..OFF_CRC + 4].copy_from_slice(&crc.to_le_bytes());
    Ok(b)
}

/// Decode one slab, or `None` if the block is absent, torn, or from a format we
/// do not understand.
///
/// `None` is not an error: it means "fall back to deriving this from the index",
/// which is always available. Returning `Err` here would turn a recoverable
/// staleness into a failed open.
pub fn decode(b: &[u8]) -> Option<Slab> {
    if b.len() < SLAB_META as usize {
        return None;
    }
    if u16::from_le_bytes([b[OFF_MAGIC], b[OFF_MAGIC + 1]]) != MAGIC || b[OFF_VER] != VERSION {
        return None;
    }
    let stored = u32::from_le_bytes(b[OFF_CRC..OFF_CRC + 4].try_into().ok()?);
    let mut probe = b[..SLAB_META as usize].to_vec();
    probe[OFF_CRC..OFF_CRC + 4].fill(0);
    if crc32c(&probe) != stored {
        return None;
    }

    match b[OFF_STATE] {
        STATE_FREE => Some(Slab::free()),
        STATE_IN_USE => {
            let class = b[OFF_CLASS];
            let gen = u32::from_le_bytes(b[OFF_GEN..OFF_GEN + 4].try_into().ok()?);
            let used_count = u32::from_le_bytes(b[OFF_USED..OFF_USED + 4].try_into().ok()?);
            let capacity = u32::from_le_bytes(b[OFF_CAP..OFF_CAP + 4].try_into().ok()?);

            // The class ladder is persisted in the superblock and may have been
            // retuned, so a stored capacity that disagrees with what this build
            // computes means the block describes a different geometry. Deriving
            // from the index is correct; trusting it is not.
            if capacity != slab_capacity(class) || !fits(capacity) {
                return None;
            }
            if used_count > capacity {
                return None;
            }

            let words = (capacity as usize).div_ceil(64);
            let mut used = Vec::with_capacity(words);
            for i in 0..words {
                let o = OFF_BITMAP + i * 8;
                used.push(u64::from_le_bytes(b[o..o + 8].try_into().ok()?));
            }
            // The bitmap and the count are redundant, which makes them a
            // cross-check: disagreement means the block is not trustworthy even
            // though its CRC held.
            if used.iter().map(|w| w.count_ones()).sum::<u32>() != used_count {
                return None;
            }
            Some(Slab::restored(class, gen, used, used_count, capacity))
        }
        _ => None,
    }
}

/// Byte offset of slab `id`'s metadata block, or `None` if it has none.
///
/// # Slab 0 has no metadata region
///
/// `SLAB_META` is 8192 and the shard's reserved prefix — the two 4 KiB
/// superblock slots — is also 8192, at offset 0. Slab 0's body starts right
/// after it ( `slot_offset` adds `SLAB_META` for every slab, including slab 0 ),
/// so slab 0's notional metadata region **is** the superblock area, byte for
/// byte. Writing it would destroy both superblock slots, taking the A/B
/// redundancy with it — the one structure the whole commit protocol rests on.
///
/// Returning `None` puts that in the type instead of in a comment two call sites
/// have to remember. Slab 0 therefore stays [`SlabState::Opaque`] across a
/// reopen: never allocated into, never reclaimed. That costs the reuse of one
/// slab, which is 2 MiB and currently unreclaimed anyway.
#[inline]
pub fn offset_of(id: u32) -> Option<u64> {
    if id == 0 {
        return None;
    }
    Some(id as u64 * crate::store::SLAB_SIZE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::extent::{class_for, CLASS_SIZES};

    fn in_use(class: u8, slots: &[u32]) -> Slab {
        let mut s = Slab::new_for_test(class, 7);
        for &i in slots {
            s.set_for_test(i);
        }
        s
    }

    #[test]
    fn every_class_bitmap_fits_the_reserved_region() {
        // The reason 8 KiB is enough, asserted rather than assumed.
        for class in 0..CLASS_SIZES.len() as u8 {
            let cap = slab_capacity(class);
            assert!(
                fits(cap),
                "class {class} needs {cap} bits, which does not fit SLAB_META"
            );
        }
    }

    #[test]
    fn round_trips_an_in_use_slab() {
        let class = class_for(crate::BITMAP_BYTES).unwrap();
        let s = in_use(class, &[0, 1, 5, 63, 64, 100]);
        let back = decode(&encode(&s).unwrap()).expect("must decode");
        assert_eq!(back.used_count(), 6);
        assert_eq!(back.capacity(), s.capacity());
        for i in [0u32, 1, 5, 63, 64, 100] {
            assert!(back.is_set_for_test(i), "slot {i} lost");
        }
        assert!(!back.is_set_for_test(2));
    }

    #[test]
    fn round_trips_a_free_slab() {
        let back = decode(&encode(&Slab::free()).unwrap()).unwrap();
        assert_eq!(back.used_count(), 0);
        assert!(matches!(back.state, SlabState::Free));
    }

    #[test]
    fn an_opaque_slab_refuses_to_encode() {
        // Persisting a guess about unknown occupancy is how live extents get
        // handed out twice.
        assert!(encode(&Slab::opaque_for_test()).is_err());
    }

    #[test]
    fn a_torn_block_decodes_as_none_rather_than_wrongly() {
        let class = class_for(crate::BITMAP_BYTES).unwrap();
        let good = encode(&in_use(class, &[3, 9])).unwrap();
        for byte in [0usize, 3, 4, 12, 16, OFF_BITMAP, OFF_BITMAP + 1] {
            let mut torn = good.clone();
            torn[byte] ^= 0xFF;
            assert!(
                decode(&torn).is_none(),
                "corruption at byte {byte} was not detected"
            );
        }
    }

    #[test]
    fn a_block_of_zeros_is_not_mistaken_for_a_valid_slab() {
        // An unwritten region reads as zeros, and must not look like "free".
        assert!(decode(&vec![0u8; SLAB_META as usize]).is_none());
    }

    #[test]
    fn a_count_disagreeing_with_the_bitmap_is_rejected() {
        let class = class_for(crate::BITMAP_BYTES).unwrap();
        let mut b = encode(&in_use(class, &[1, 2, 3])).unwrap();
        b[OFF_USED..OFF_USED + 4].copy_from_slice(&9u32.to_le_bytes());
        let crc = {
            let mut p = b.clone();
            p[OFF_CRC..OFF_CRC + 4].fill(0);
            crc32c(&p)
        };
        b[OFF_CRC..OFF_CRC + 4].copy_from_slice(&crc.to_le_bytes());
        // CRC is valid, so only the redundancy between count and bitmap catches it.
        assert!(decode(&b).is_none());
    }

    /// Slab 0 must have no metadata offset, because that offset is the
    /// superblock. Getting this wrong destroys the A/B redundancy silently: one
    /// slot is rewritten by the flip that follows, so the shard still opens, and
    /// only a torn *other* slot reveals that the spare was gone.
    #[test]
    fn slab_zero_has_no_metadata_region_because_it_is_the_superblock() {
        assert_eq!(offset_of(0), None);
        assert_eq!(
            SLAB_META,
            2 * crate::store::PAGE as u64,
            "the reserved superblock prefix and SLAB_META must stay the same size, \
             or slab 0's body no longer starts where the superblocks end"
        );
        assert_eq!(offset_of(1), Some(crate::store::SLAB_SIZE));
        assert_eq!(offset_of(2), Some(2 * crate::store::SLAB_SIZE));
    }
}
