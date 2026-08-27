//! The superblock: two slots, alternating, and the only atomicity requirement.
//!
//! # Why this is the whole commit protocol
//!
//! Extents and index nodes are shadow-paged — always written to space no
//! published root can reach — so a half-written one is unreachable and therefore
//! harmless. The single moment a checkpoint becomes real is the superblock flip.
//!
//! That flip does **not** require the device to write 4 KiB atomically. Two
//! slots alternate by `seq`, each self-checksummed; a torn write to the slot
//! being replaced simply fails its CRC and the older slot is chosen. Which is
//! also why the store needs no Postgres-style full-page writes: the WAL's jobs
//! are commit durability and replication, not torn-page repair. Re-adding
//! full-page writes would be pure cost, and this comment exists because it is
//! the kind of thing that gets helpfully re-added.
//!
//! # The two-slot delay is load-bearing
//!
//! While slot A is live, slot B still names the *previous* root. Anything that
//! root reaches must stay intact, which is why an extent freed by checkpoint N
//! cannot be reused until N+2 is durable ( `RECLAIM_CKPT_DELAY` ). The same
//! two-transaction delay LMDB uses, for the same reason.
//!
//! # What the persisted descriptors actually enforce
//!
//! The image records its own geometry — physical shard number, base page size,
//! slab size, packed threshold, size-class ladder, index node size — and the
//! stated purpose of those fields is that a file stays readable by a build
//! tuned differently. **That was a claim, not a mechanism.** Every address
//! calculation outside this module reads the *compiled* geometry, so a file
//! carrying a different ladder or slab size was not read compatibly; it was
//! read wrongly, silently, at every extent offset.
//!
//! Until one decoded geometry object drives that arithmetic, the only sound
//! reading of a descriptor this build cannot honour is a **refusal**, and
//! [`SuperBlock::decode`] now issues one — the same stance I1 takes on
//! endianness, for the same reason: reinterpreting bytes the writer meant
//! differently returns wrong answers rather than an error. `node_size` is the
//! exception, and the only one, because the index really does read it.
//!
//! A zero in `page_size` or `slab_size` means a file written before the field
//! was compared, exactly as for [`SuperBlock::node_size`]; those were written
//! at the compiled values and are read at them.
//!
//! # Identity is two fields, and only one of them was checked
//!
//! `db_uuid` refuses a shard file from *another* database. It cannot refuse one
//! from **this** database — every shard of a database carries the same uuid, so
//! exchanging two shard images passed every check there was, and each shard
//! then answered its queries out of the other's extents. [`shard_id`] is the
//! field that separates them, and [`SuperBlock::check_identity`] compares both.
//!
//! **It is called at open**, before an extent is read, and that placement is
//! the point: a database that opens and then answers from the wrong shard is
//! precisely the failure being closed, and a query that found nothing has
//! already returned by the time a read-path check could fire. Both of the shard
//! store's open paths call it, the read-write one and the read-only one.
//!
//! Do not reduce either call site back to a bare `db_uuid` comparison to
//! quiet something. A check with no caller refuses nothing, which this
//! repository has now been bitten by often enough to name: `check_alignment`
//! had no caller, `validate` ran only from `fsck`. `tests/shard_identity.rs`
//! asserts the refusal twice for that reason — once against this module and
//! once through `Db::open` — and only the second fails if a call site goes.
//!
//! [`shard_id`]: SuperBlock::shard_id

use super::extent::{validate_ladder, CLASS_SIZES};
use crate::error::{CodecError, Result};
use crate::store::checksum::crc32c_append;
use crate::store::PAGE;

pub const MAGIC: u64 = 0x594E_4F53_4E44_4231; // "YNOSNDB1"
pub const FMT_MAJOR: u16 = 1;
pub const FMT_MINOR: u16 = 0;

/// Written natively so a foreign-endian file is detected at open rather than
/// silently misread. I1 refuses rather than byte-swapping.
pub const ENDIAN_PROBE: u32 = 0x0102_0304;

/// Features a reader must understand or refuse to open the file.
pub mod feat {
    /// An alternative array encoding is in use ( `ChunkRef.enc == 1` ).
    pub const ALT_ARRAY_ENC: u64 = 1 << 0;
    /// Leaf nodes use a v2 layout.
    pub const LEAF_V2: u64 = 1 << 1;
    /// The shard exceeds the default address-space cap.
    pub const LARGE_SHARD: u64 = 1 << 2;
    /// Packed pages use a v2 layout.
    pub const PAGE_V2: u64 = 1 << 3;

    /// Everything this build implements.
    pub const SUPPORTED: u64 = 0;
}

const OFF_MAGIC: usize = 0;
const OFF_ENDIAN: usize = 8;
const OFF_MAJOR: usize = 12;
const OFF_MINOR: usize = 14;
const OFF_FEAT_REQ: usize = 16;
const OFF_FEAT_COMPAT: usize = 24;
const OFF_UUID: usize = 32;
const OFF_SEQ: usize = 48;
const OFF_SHARD: usize = 56;
const OFF_PAGE_SIZE: usize = 60;
const OFF_SLAB_SIZE: usize = 64;
const OFF_PACK_MAX: usize = 68;
const OFF_N_CLASSES: usize = 72;
const OFF_CLASSES: usize = 76; // u32 * n_classes
const OFF_ROOT: usize = 120;
const OFF_HEIGHT: usize = 124;
const OFF_CKPT_CV: usize = 128;
const OFF_WAL_LSN: usize = 136;
const OFF_LIVE_BYTES: usize = 144;
const OFF_N_SLABS: usize = 152;
const OFF_CKPT_SEQ: usize = 160;
const OFF_NODE_SIZE: usize = 168;
const OFF_COMMIT_CLOCK: usize = 176;
const OFF_CRC: usize = PAGE - 4;

/// Maximum classes the fixed-size field can hold.
pub const MAX_CLASSES: usize = 11;

/// One superblock image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SuperBlock {
    pub seq: u64,
    pub db_uuid: [u8; 16],
    pub shard_id: u32,
    pub feat_required: u64,
    pub feat_compat: u64,
    /// The size-class ladder the file was written with.
    ///
    /// It used to read "persisted rather than compiled in, so retuning it is
    /// not a format break and a differently-tuned binary's file stays
    /// readable". Nothing implemented that: `slab_capacity`, `slot_offset` and
    /// every allocation read [`CLASS_SIZES`], so a file with any other ladder
    /// was read at the wrong offsets rather than compatibly. The field is
    /// therefore a **guard** today — a ladder this build cannot honour is
    /// refused at [`SuperBlock::decode`] — and becomes a compatibility
    /// mechanism only once the allocator takes its ladder from here.
    pub class_sizes: Vec<u32>,
    pub pack_max: u32,
    /// Root of the chunk index, and its height. `None` for an empty shard —
    /// writing a root page that says nothing would put a special case in every
    /// reader.
    pub root: Option<(u32, u8)>,
    /// Commit version this image reflects. By I4 the whole file is a consistent
    /// snapshot at this version.
    pub checkpoint_cv: u64,
    pub checkpoint_seq: u64,
    /// Index node size, in bytes.
    ///
    /// Persisted for the same reason `class_sizes` is: it describes the file's
    /// geometry, so a build tuned differently must read what is there rather
    /// than what it would have written. It was a compile-time constant, and
    /// `node()` read exactly that many bytes — so changing `INDEX_NODE` would
    /// have silently misparsed every existing node rather than failing.
    ///
    /// Zero means a file written before this field existed; those are read at
    /// [`crate::store::INDEX_NODE`], which is what they were written at.
    pub node_size: u32,
    /// Where WAL replay must begin.
    pub wal_replay_lsn: u64,
    pub live_bytes: u64,
    pub n_slabs: u32,
    /// An upper bound on every commit time assigned before this checkpoint.
    ///
    /// The WAL is the only other place commit times live, and a checkpoint is
    /// exactly the point at which the WAL below it stops being read — so without
    /// this field, a restart right after a checkpoint resumes the commit clock
    /// from the system clock alone. A clock that stepped backwards in the
    /// meantime ( NTP, a VM restored from a snapshot ) would then stamp new
    /// commits *earlier* than checkpointed ones and break **I9**.
    ///
    /// Deliberately the oracle's high-water mark rather than the watermark
    /// version's own stamp: the watermark's ring slot may already have been
    /// recycled by a later commit, and over-approximating is safe here — the
    /// clock only ever needs a floor it must not stamp below.
    ///
    /// Zero means a file written before this field existed, exactly as for
    /// [`SuperBlock::node_size`]. Recovery then falls back to the stamps in the
    /// replayed WAL, and to the system clock when there are none.
    pub commit_clock: u64,
}

impl SuperBlock {
    /// A fresh shard: no root, nothing committed.
    pub fn initial(db_uuid: [u8; 16], shard_id: u32) -> Self {
        SuperBlock {
            seq: 1,
            db_uuid,
            shard_id,
            feat_required: 0,
            feat_compat: 0,
            class_sizes: CLASS_SIZES.to_vec(),
            pack_max: super::extent::PACK_MAX as u32,
            root: None,
            checkpoint_cv: 0,
            checkpoint_seq: 0,
            node_size: crate::store::INDEX_NODE as u32,
            wal_replay_lsn: 0,
            live_bytes: 0,
            n_slabs: 0,
            commit_clock: 0,
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        if self.class_sizes.len() > MAX_CLASSES {
            return Err(CodecError::Invariant(
                "too many size classes for the superblock",
            ));
        }
        let mut b = vec![0u8; PAGE];
        b[OFF_MAGIC..OFF_MAGIC + 8].copy_from_slice(&MAGIC.to_le_bytes());
        // Native, not little-endian: that is the point of a probe.
        b[OFF_ENDIAN..OFF_ENDIAN + 4].copy_from_slice(&ENDIAN_PROBE.to_ne_bytes());
        b[OFF_MAJOR..OFF_MAJOR + 2].copy_from_slice(&FMT_MAJOR.to_le_bytes());
        b[OFF_MINOR..OFF_MINOR + 2].copy_from_slice(&FMT_MINOR.to_le_bytes());
        b[OFF_FEAT_REQ..OFF_FEAT_REQ + 8].copy_from_slice(&self.feat_required.to_le_bytes());
        b[OFF_FEAT_COMPAT..OFF_FEAT_COMPAT + 8].copy_from_slice(&self.feat_compat.to_le_bytes());
        b[OFF_UUID..OFF_UUID + 16].copy_from_slice(&self.db_uuid);
        b[OFF_SEQ..OFF_SEQ + 8].copy_from_slice(&self.seq.to_le_bytes());
        b[OFF_SHARD..OFF_SHARD + 4].copy_from_slice(&self.shard_id.to_le_bytes());
        b[OFF_PAGE_SIZE..OFF_PAGE_SIZE + 4].copy_from_slice(&(PAGE as u32).to_le_bytes());
        b[OFF_SLAB_SIZE..OFF_SLAB_SIZE + 4]
            .copy_from_slice(&(super::SLAB_SIZE as u32).to_le_bytes());
        b[OFF_PACK_MAX..OFF_PACK_MAX + 4].copy_from_slice(&self.pack_max.to_le_bytes());
        b[OFF_N_CLASSES..OFF_N_CLASSES + 4]
            .copy_from_slice(&(self.class_sizes.len() as u32).to_le_bytes());
        for (i, s) in self.class_sizes.iter().enumerate() {
            let o = OFF_CLASSES + i * 4;
            b[o..o + 4].copy_from_slice(&s.to_le_bytes());
        }
        let (root, height) = self.root.unwrap_or((u32::MAX, 0));
        b[OFF_NODE_SIZE..OFF_NODE_SIZE + 4].copy_from_slice(&self.node_size.to_le_bytes());
        b[OFF_ROOT..OFF_ROOT + 4].copy_from_slice(&root.to_le_bytes());
        b[OFF_HEIGHT] = height;
        b[OFF_CKPT_CV..OFF_CKPT_CV + 8].copy_from_slice(&self.checkpoint_cv.to_le_bytes());
        b[OFF_WAL_LSN..OFF_WAL_LSN + 8].copy_from_slice(&self.wal_replay_lsn.to_le_bytes());
        b[OFF_LIVE_BYTES..OFF_LIVE_BYTES + 8].copy_from_slice(&self.live_bytes.to_le_bytes());
        b[OFF_N_SLABS..OFF_N_SLABS + 4].copy_from_slice(&self.n_slabs.to_le_bytes());
        b[OFF_CKPT_SEQ..OFF_CKPT_SEQ + 8].copy_from_slice(&self.checkpoint_seq.to_le_bytes());
        b[OFF_COMMIT_CLOCK..OFF_COMMIT_CLOCK + 8].copy_from_slice(&self.commit_clock.to_le_bytes());

        let crc = compute_crc(&b);
        b[OFF_CRC..OFF_CRC + 4].copy_from_slice(&crc.to_le_bytes());
        Ok(b)
    }

    /// Parse and validate. A failing CRC is `Ok(None)`, not an error: that is
    /// the expected state of the slot being replaced when a crash interrupts a
    /// flip, and the caller simply takes the other one.
    pub fn decode(b: &[u8]) -> Result<Option<Self>> {
        if b.len() < PAGE {
            return Ok(None);
        }
        if u64::from_le_bytes(b[OFF_MAGIC..OFF_MAGIC + 8].try_into().unwrap()) != MAGIC {
            return Ok(None);
        }
        let stored = u32::from_le_bytes(b[OFF_CRC..OFF_CRC + 4].try_into().unwrap());
        if compute_crc(b) != stored {
            return Ok(None); // torn slot
        }

        // Past the checksum, problems are real errors rather than "try the other
        // slot" — a valid checksum means the writer meant these bytes.
        let probe = u32::from_ne_bytes(b[OFF_ENDIAN..OFF_ENDIAN + 4].try_into().unwrap());
        if probe != ENDIAN_PROBE {
            return Err(CodecError::Invariant(
                "file was written on a host of the opposite endianness (I1)",
            ));
        }
        let major = u16::from_le_bytes(b[OFF_MAJOR..OFF_MAJOR + 2].try_into().unwrap());
        if major != FMT_MAJOR {
            return Err(CodecError::UnsupportedEncoding);
        }
        let feat_required =
            u64::from_le_bytes(b[OFF_FEAT_REQ..OFF_FEAT_REQ + 8].try_into().unwrap());
        if feat_required & !feat::SUPPORTED != 0 {
            // Refuse at open rather than at read: a reader that skipped an
            // unknown feature could return wrong results instead of an error.
            return Err(CodecError::UnsupportedEncoding);
        }

        let n_classes =
            u32::from_le_bytes(b[OFF_N_CLASSES..OFF_N_CLASSES + 4].try_into().unwrap()) as usize;
        if n_classes == 0 || n_classes > MAX_CLASSES {
            return Err(CodecError::Invariant("bad size-class count"));
        }
        let class_sizes: Vec<u32> = (0..n_classes)
            .map(|i| {
                let o = OFF_CLASSES + i * 4;
                u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
            })
            .collect();
        // The ladder is data, so a foreign file could carry one that breaks the
        // 64-byte alignment guarantee everything else rests on.
        validate_ladder(&class_sizes)?;
        // ...and a well-formed ladder that is not *this* ladder is worse than a
        // malformed one, because nothing downstream would notice. See the
        // module header: the arithmetic is compiled in, so the only sound
        // reading of a geometry this build cannot honour is a refusal.
        if class_sizes != CLASS_SIZES {
            return Err(CodecError::Invariant(
                "shard was written with a size-class ladder this build does not implement",
            ));
        }

        // A zero is a file written before the field was compared; it was
        // written at the compiled value, exactly as for `node_size`.
        let page_size = u32::from_le_bytes(b[OFF_PAGE_SIZE..OFF_PAGE_SIZE + 4].try_into().unwrap());
        if page_size != 0 && page_size != PAGE as u32 {
            return Err(CodecError::Invariant(
                "shard was written with a base page size this build does not implement",
            ));
        }
        let slab_size = u32::from_le_bytes(b[OFF_SLAB_SIZE..OFF_SLAB_SIZE + 4].try_into().unwrap());
        if slab_size != 0 && slab_size as u64 != super::SLAB_SIZE {
            return Err(CodecError::Invariant(
                "shard was written with a slab size this build does not implement",
            ));
        }

        let root_raw = u32::from_le_bytes(b[OFF_ROOT..OFF_ROOT + 4].try_into().unwrap());
        let height = b[OFF_HEIGHT];
        let root = if root_raw == u32::MAX || height == 0 {
            None
        } else {
            Some((root_raw, height))
        };

        Ok(Some(SuperBlock {
            seq: u64::from_le_bytes(b[OFF_SEQ..OFF_SEQ + 8].try_into().unwrap()),
            db_uuid: b[OFF_UUID..OFF_UUID + 16].try_into().unwrap(),
            shard_id: u32::from_le_bytes(b[OFF_SHARD..OFF_SHARD + 4].try_into().unwrap()),
            feat_required,
            feat_compat: u64::from_le_bytes(
                b[OFF_FEAT_COMPAT..OFF_FEAT_COMPAT + 8].try_into().unwrap(),
            ),
            class_sizes,
            pack_max: u32::from_le_bytes(b[OFF_PACK_MAX..OFF_PACK_MAX + 4].try_into().unwrap()),
            root,
            checkpoint_cv: u64::from_le_bytes(b[OFF_CKPT_CV..OFF_CKPT_CV + 8].try_into().unwrap()),
            checkpoint_seq: u64::from_le_bytes(
                b[OFF_CKPT_SEQ..OFF_CKPT_SEQ + 8].try_into().unwrap(),
            ),
            node_size: {
                let v = u32::from_le_bytes(b[OFF_NODE_SIZE..OFF_NODE_SIZE + 4].try_into().unwrap());
                // Zero: written before the field existed, so it was written at
                // whatever `INDEX_NODE` was then — which is this value.
                if v == 0 {
                    crate::store::INDEX_NODE as u32
                } else {
                    v
                }
            },
            wal_replay_lsn: u64::from_le_bytes(b[OFF_WAL_LSN..OFF_WAL_LSN + 8].try_into().unwrap()),
            live_bytes: u64::from_le_bytes(
                b[OFF_LIVE_BYTES..OFF_LIVE_BYTES + 8].try_into().unwrap(),
            ),
            n_slabs: u32::from_le_bytes(b[OFF_N_SLABS..OFF_N_SLABS + 4].try_into().unwrap()),
            commit_clock: u64::from_le_bytes(
                b[OFF_COMMIT_CLOCK..OFF_COMMIT_CLOCK + 8]
                    .try_into()
                    .unwrap(),
            ),
        }))
    }

    /// Refuse a shard image that is not the one the caller meant to open.
    ///
    /// **Both halves are needed and the second is the one that was missing.**
    /// `db_uuid` separates databases; every shard *within* a database shares it,
    /// so two shard images of one database could be exchanged on disk and the
    /// uuid check passed unchanged. Each shard then served the other's extents
    /// under its own keys: no error, no corruption, wrong answers.
    ///
    /// `shard_id` is compared with no legacy tolerance, unlike
    /// [`node_size`](Self::node_size). It is an original field of format major
    /// 1 — written by [`SuperBlock::initial`] since the format existed and never
    /// mutated afterwards — so there is no zero-means-unwritten case to
    /// accommodate, and inventing one would be self-defeating: shard 0's number
    /// *is* zero, so tolerating a zero would re-open the swap for every pair
    /// involving it.
    ///
    /// Must be called at open, before any extent is read. A database that
    /// opens and then answers from the wrong shard is the failure being closed;
    /// detecting it at first read would already be too late for a query that
    /// found nothing and reported an empty result.
    pub fn check_identity(&self, db_uuid: [u8; 16], shard_id: u32) -> Result<()> {
        if self.db_uuid != db_uuid {
            return Err(CodecError::DatabaseIdentityMismatch);
        }
        if self.shard_id != shard_id {
            return Err(CodecError::ShardIdentityMismatch {
                expected: shard_id,
                found: self.shard_id,
            });
        }
        Ok(())
    }

    /// Byte offset of the slot this image's successor should be written to.
    ///
    /// Alternating by sequence number is what makes the flip survive a torn
    /// write: the slot being overwritten is never the one currently live.
    #[inline]
    pub fn next_slot_offset(&self) -> u64 {
        ((self.seq + 1) & 1) * PAGE as u64
    }
}

fn compute_crc(b: &[u8]) -> u32 {
    let c = crc32c_append(0, &b[..OFF_CRC]);
    crc32c_append(c, &[0, 0, 0, 0])
}

/// Choose the live superblock from the two slots.
///
/// Returns the valid slot with the higher sequence number. `None` when neither
/// parses, which for a non-empty file means the shard is unrecoverable — both
/// roots are gone, and no amount of extent scanning reconstructs the index.
pub fn pick(slot_a: &[u8], slot_b: &[u8]) -> Result<Option<SuperBlock>> {
    let a = SuperBlock::decode(slot_a)?;
    let b = SuperBlock::decode(slot_b)?;
    Ok(match (a, b) {
        (Some(a), Some(b)) => Some(if a.seq >= b.seq { a } else { b }),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Node size must survive a round trip, and a legacy zero must read as the
    /// size such a file was written at.
    ///
    /// It was a compile-time constant that `node()` read blindly, so a build
    /// with a different `INDEX_NODE` would have misparsed every existing node
    /// rather than failing — the same hazard the persisted `class_sizes` exists
    /// to avoid, on a field nobody had noticed shared it.
    #[test]
    fn node_size_round_trips_and_defaults_for_legacy_files() {
        let mut sb = SuperBlock::initial([1u8; 16], 0);
        sb.node_size = 2048;
        let back = SuperBlock::decode(&sb.encode().unwrap()).unwrap().unwrap();
        assert_eq!(back.node_size, 2048, "a retuned node size must survive");

        // A file written before the field existed has zeros there.
        let mut bytes = SuperBlock::initial([1u8; 16], 0).encode().unwrap();
        bytes[OFF_NODE_SIZE..OFF_NODE_SIZE + 4].fill(0);
        // Re-checksum, so it fails on the missing field rather than the CRC.
        let crc = compute_crc(&bytes);
        bytes[OFF_CRC..OFF_CRC + 4].copy_from_slice(&crc.to_le_bytes());
        let legacy = SuperBlock::decode(&bytes).unwrap().unwrap();
        assert_eq!(
            legacy.node_size,
            crate::store::INDEX_NODE as u32,
            "a zero must read as the size the file was written at"
        );
    }

    fn sb(seq: u64) -> SuperBlock {
        let mut s = SuperBlock::initial([7u8; 16], 3);
        s.seq = seq;
        s.checkpoint_cv = seq * 10;
        s.root = Some((42, 3));
        s
    }

    #[test]
    fn roundtrip_preserves_every_field() {
        let s = sb(5);
        let back = SuperBlock::decode(&s.encode().unwrap()).unwrap().unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn an_empty_shard_has_no_root() {
        let s = SuperBlock::initial([0u8; 16], 0);
        assert_eq!(s.root, None);
        let back = SuperBlock::decode(&s.encode().unwrap()).unwrap().unwrap();
        assert_eq!(
            back.root, None,
            "an empty shard must not invent a root page"
        );
    }

    /// The ladder is persisted, and what is persisted is what is read back —
    /// the round trip is a precondition for comparing it at all.
    #[test]
    fn the_ladder_is_persisted_not_compiled_in() {
        let s = SuperBlock::initial([1u8; 16], 0);
        let back = SuperBlock::decode(&s.encode().unwrap()).unwrap().unwrap();
        assert_eq!(back.class_sizes, s.class_sizes);
        assert_eq!(
            back.class_sizes,
            CLASS_SIZES.to_vec(),
            "decode must report the ladder the bytes carry, not a constant it \
             substituted — a check that compares a value against itself proves \
             nothing"
        );
    }

    /// A **well-formed** ladder that is not this build's is refused, and the
    /// assertion shows why rather than only that.
    ///
    /// This used to round-trip and be accepted, on the documented claim that
    /// persisting the ladder made retuning "not a format break". Nothing
    /// implemented the claim: `slot_offset` multiplies by the *compiled*
    /// `class_size`, so the same `( slab, class, slot )` names a different byte
    /// under the two ladders and every extent read lands inside a neighbouring
    /// slot. Refusing is the interim; honouring it means the allocator taking
    /// its ladder from here.
    #[test]
    fn a_ladder_this_build_cannot_honour_is_refused_rather_than_misread() {
        let mut s = SuperBlock::initial([1u8; 16], 0);
        s.class_sizes = vec![4096, 576, 1088, 2112, 8256];
        validate_ladder(&s.class_sizes)
            .expect("the retuned ladder is well-formed; shape is not what refuses it");

        // The divergence the refusal exists to prevent, stated in bytes.
        let under_stored = 3u64 * s.class_sizes[2] as u64;
        let under_compiled = 3u64 * CLASS_SIZES[2] as u64;
        assert_ne!(
            under_stored, under_compiled,
            "slot 3 of class 2 must land somewhere else under the stored ladder, \
             or this file would be harmless and there would be nothing to refuse"
        );
        assert_eq!(
            crate::store::alloc::slot_offset(1, 2, 3),
            crate::store::SLAB_SIZE + crate::store::SLAB_META + under_compiled,
            "addressing is driven by the compiled ladder, which is the whole \
             reason a differently-tuned file cannot simply be opened"
        );

        assert!(
            matches!(
                SuperBlock::decode(&s.encode().unwrap()),
                Err(CodecError::Invariant(_))
            ),
            "a ladder this build does not implement must be refused at open"
        );
    }

    /// One changed entry is enough; it is the ladder that is compared, not its
    /// length.
    #[test]
    fn a_single_retuned_class_is_enough_to_refuse() {
        let mut s = SuperBlock::initial([1u8; 16], 0);
        s.class_sizes = CLASS_SIZES.to_vec();
        s.class_sizes[5] = 1664; // was 1600; still aligned and still ascending
        validate_ladder(&s.class_sizes).expect("still well-formed");
        assert!(SuperBlock::decode(&s.encode().unwrap()).is_err());
    }

    /// Base page size and slab size are *compared*, and a legacy zero is not.
    ///
    /// They were written into every superblock and read by nothing, so a file
    /// written with a 8 KiB page or a 4 MiB slab opened cleanly and then had
    /// every slab base computed at the compiled stride.
    #[test]
    fn page_and_slab_size_are_compared_and_a_legacy_zero_is_accepted() {
        for (off, bad) in [
            (OFF_PAGE_SIZE, 8192u32),
            (OFF_SLAB_SIZE, 4 * 1024 * 1024u32),
        ] {
            let mut bytes = SuperBlock::initial([1u8; 16], 0).encode().unwrap();
            bytes[off..off + 4].copy_from_slice(&bad.to_le_bytes());
            let crc = compute_crc(&bytes);
            bytes[OFF_CRC..OFF_CRC + 4].copy_from_slice(&crc.to_le_bytes());
            assert!(
                SuperBlock::decode(&bytes).is_err(),
                "geometry at offset {off} was recorded and not compared"
            );

            // Zero: written before the field was compared. Accepted, at the
            // compiled value, on the `node_size` precedent.
            let mut legacy = SuperBlock::initial([1u8; 16], 0).encode().unwrap();
            legacy[off..off + 4].fill(0);
            let crc = compute_crc(&legacy);
            legacy[OFF_CRC..OFF_CRC + 4].copy_from_slice(&crc.to_le_bytes());
            assert!(
                SuperBlock::decode(&legacy).unwrap().is_some(),
                "a legacy zero at offset {off} must not refuse an existing file"
            );
        }
    }

    /// The swap the uuid check structurally cannot see.
    ///
    /// Two shards of one database share a `db_uuid`, so exchanging their images
    /// left every check satisfied and each shard answered out of the other's
    /// extents.
    #[test]
    fn a_shard_image_from_a_sibling_shard_is_refused() {
        let uuid = [9u8; 16];
        let shard_one = SuperBlock::initial(uuid, 1);

        assert_eq!(
            shard_one.check_identity(uuid, 1),
            Ok(()),
            "the shard it really is must open"
        );
        assert_eq!(
            shard_one.check_identity(uuid, 0),
            Err(CodecError::ShardIdentityMismatch {
                expected: 0,
                found: 1
            }),
            "shard 1's image presented as shard 0 must be refused"
        );
        assert_eq!(
            shard_one.db_uuid,
            SuperBlock::initial(uuid, 0).db_uuid,
            "the uuid is identical across siblings, which is why it cannot \
             refuse this and shard_id must"
        );
    }

    /// Shard 0 is the case a "zero means unwritten" tolerance would have
    /// re-opened, so there is deliberately no such tolerance.
    #[test]
    fn shard_zero_is_not_treated_as_an_unwritten_field() {
        let uuid = [3u8; 16];
        let shard_zero = SuperBlock::initial(uuid, 0);
        assert_eq!(shard_zero.check_identity(uuid, 0), Ok(()));
        assert!(matches!(
            shard_zero.check_identity(uuid, 2),
            Err(CodecError::ShardIdentityMismatch { .. })
        ));
    }

    /// The database check still fires, and fires first: a foreign file is
    /// reported as foreign rather than as a shard-number mismatch.
    #[test]
    fn a_foreign_database_is_reported_as_such_not_as_a_shard_mismatch() {
        let sb = SuperBlock::initial([1u8; 16], 4);
        assert_eq!(
            sb.check_identity([2u8; 16], 0),
            Err(CodecError::DatabaseIdentityMismatch)
        );
    }

    #[test]
    fn a_ladder_breaking_the_alignment_guarantee_is_rejected() {
        // Everything rests on every slot being 64-byte aligned; a foreign file
        // could carry a ladder that breaks it.
        let mut s = SuperBlock::initial([1u8; 16], 0);
        s.class_sizes = vec![4096, 100, 8256]; // 100 is not a multiple of 64
        assert!(SuperBlock::decode(&s.encode().unwrap()).is_err());
    }

    #[test]
    fn a_torn_slot_yields_none_rather_than_an_error() {
        // The expected state of the slot being replaced when a crash interrupts
        // a flip. The caller must fall back, not fail.
        let mut bytes = sb(1).encode().unwrap();
        bytes[100] ^= 0xFF;
        assert!(SuperBlock::decode(&bytes).unwrap().is_none());
    }

    #[test]
    fn pick_takes_the_higher_sequence() {
        let a = sb(4).encode().unwrap();
        let b = sb(5).encode().unwrap();
        assert_eq!(pick(&a, &b).unwrap().unwrap().seq, 5);
        assert_eq!(pick(&b, &a).unwrap().unwrap().seq, 5);
    }

    /// The property that makes the flip survive a crash without atomic sector
    /// writes: tearing the slot being written must leave the older one usable.
    #[test]
    fn a_torn_flip_falls_back_to_the_previous_checkpoint() {
        let live = sb(7).encode().unwrap();
        let mut half_written = sb(8).encode().unwrap();
        half_written[2000] ^= 0xFF; // torn mid-write

        let chosen = pick(&live, &half_written).unwrap().unwrap();
        assert_eq!(chosen.seq, 7, "must fall back to the intact older slot");
        assert_eq!(chosen.checkpoint_cv, 70);
    }

    #[test]
    fn slots_alternate_so_the_live_one_is_never_overwritten() {
        for seq in 1..10u64 {
            let s = sb(seq);
            let target = s.next_slot_offset();
            let own = (seq & 1) * PAGE as u64;
            assert_ne!(target, own, "seq {seq} would overwrite its own live slot");
        }
    }

    #[test]
    fn both_slots_unreadable_is_reported_as_such() {
        let junk = vec![0u8; PAGE];
        assert!(pick(&junk, &junk).unwrap().is_none());
    }

    #[test]
    fn an_unknown_required_feature_is_refused_at_open() {
        // Refusing here rather than at read time is deliberate: a reader that
        // ignored the bit could return wrong results instead of an error.
        let mut s = SuperBlock::initial([1u8; 16], 0);
        s.feat_required = feat::ALT_ARRAY_ENC;
        assert!(matches!(
            SuperBlock::decode(&s.encode().unwrap()),
            Err(CodecError::UnsupportedEncoding)
        ));
    }

    #[test]
    fn a_compat_feature_bit_is_ignored() {
        let mut s = SuperBlock::initial([1u8; 16], 0);
        s.feat_compat = 0xDEAD_BEEF;
        let back = SuperBlock::decode(&s.encode().unwrap()).unwrap().unwrap();
        assert_eq!(
            back.feat_compat, 0xDEAD_BEEF,
            "compat bits are informational"
        );
    }

    #[test]
    fn a_future_major_version_is_refused() {
        let mut bytes = sb(1).encode().unwrap();
        bytes[OFF_MAJOR..OFF_MAJOR + 2].copy_from_slice(&99u16.to_le_bytes());
        let crc = compute_crc(&bytes);
        bytes[OFF_CRC..OFF_CRC + 4].copy_from_slice(&crc.to_le_bytes());
        assert!(SuperBlock::decode(&bytes).is_err());
    }

    #[test]
    fn opposite_endianness_is_detected_rather_than_misread() {
        let mut bytes = sb(1).encode().unwrap();
        // What a big-endian host's probe would look like to us.
        bytes[OFF_ENDIAN..OFF_ENDIAN + 4].copy_from_slice(&ENDIAN_PROBE.swap_bytes().to_ne_bytes());
        let crc = compute_crc(&bytes);
        bytes[OFF_CRC..OFF_CRC + 4].copy_from_slice(&crc.to_le_bytes());
        assert!(
            SuperBlock::decode(&bytes).is_err(),
            "I1 refuses rather than byte-swapping"
        );
    }

    #[test]
    fn corruption_anywhere_in_the_slot_is_detected() {
        let bytes = sb(3).encode().unwrap();
        for &i in &[0usize, 48, 120, 160, 2048, PAGE - 5] {
            let mut bad = bytes.clone();
            bad[i] ^= 0xFF;
            let r = SuperBlock::decode(&bad);
            assert!(
                matches!(r, Ok(None)) || r.is_err(),
                "corruption at byte {i} went undetected"
            );
        }
    }

    #[test]
    fn decode_never_panics_on_garbage() {
        let mut seed = 0xABCDEFu64;
        for _ in 0..500 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let n = (seed % (PAGE as u64 + 16)) as usize;
            let bytes: Vec<u8> = (0..n).map(|i| (seed >> (i % 56)) as u8).collect();
            let _ = SuperBlock::decode(&bytes);
        }
    }
}
