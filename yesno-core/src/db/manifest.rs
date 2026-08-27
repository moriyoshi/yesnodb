//! The database MANIFEST: identity, shard count, and the `vshard -> shard` map.
//!
//! # Why this exists, and what it fixes
//!
//! `Db::shard_of` was `vshard_of(key) % shards.len()`. The virtual-shard layer
//! was there — [`crate::db::VSHARDS`] is 256 and `vshard_of` is a splitmix64
//! finalizer — but the *indirection* was not, so the shard count entered the
//! routing function directly and **the count was persisted nowhere**. It came
//! from `DbOptions` on every open, validated only for `> 0`.
//!
//! That was not cosmetic. Measured before this module existed: a database
//! written with 4 shards and reopened with 8 returned **31 of 64 keys**, and
//! reopened with 3 returned **14 of 64** — silently, with no error, because
//! every key hashed to a different shard file than the one holding it. The
//! design's deferred list records "online shard-count change ( the vshard
//! indirection is already in place )", so the missing half was written down as
//! already delivered, which is how it survived to M7.
//!
//! # What the map buys
//!
//! Routing is `map[vshard_of(key)]`, and the map is data. Changing the shard
//! count then means rewriting 256 `u16`s and moving the chunks that changed
//! owner — not rehashing every key in the database. That is the whole point of
//! the indirection and it is why widening [`crate::db::VSHARDS`] or changing
//! the hash addresses none of it.
//!
//! # Two slots, for the same reason the superblock has two
//!
//! The manifest is written when a database is created and whenever the map or
//! the term changes. A torn write during that update must not lose the
//! database's identity, so the file carries two self-checksummed slots and the
//! reader takes the valid one with the higher `seq` — the same rule as
//! [`crate::store::superblock::pick`], and deliberately not a different one.
//!
//! **Two slots only help if an update writes one of them.** Until 2026-09-06
//! every manifest write truncated the file and wrote the *same* new image to
//! both slots, which meant the two slots held two copies of one generation and
//! the redundancy was decorative: a crash between the truncate and the first
//! copy landing left no readable manifest at all, and the database would not
//! open. The operation most exposed to it was [`crate::promote_database`], run
//! by an operator under failover pressure.
//!
//! So an update now writes **only the slot [`pick`] is not currently
//! returning**, and [`next_slot_offset`] is where that choice lives. The live
//! slot is never touched, so at every byte offset at which a crash can occur
//! `pick` still returns a manifest: the old one while the new slot is
//! incomplete, the new one once it is whole.
//!
//! # Why the checksum, not the write width, is what makes this safe
//!
//! A slot becomes authoritative by carrying the higher `seq`, and `seq` lives
//! *inside* the CRC-covered image. A partially written slot fails its CRC, so
//! its `seq` is never observable — there is no window in which a reader can see
//! the new generation number without also having every byte the writer intended.
//! That is why this needs no atomic 4 KiB sector write, exactly as for the
//! superblock.
//!
//! The `fsync` after the write is load-bearing for the *next* update rather than
//! this one: update N+1 targets the slot update N left alone, so N's bytes must
//! be durable before N+1 is allowed to start overwriting the only other copy.
//! Skipping it would let one crash lose both generations.
//!
//! **Creation is the one case that legitimately writes both slots** — see
//! [`initial_file`]. There is no prior generation to preserve, and leaving slot
//! B unwritten would make a torn write to slot A fatal.

use crate::db::VSHARDS;
use crate::error::{CodecError, Result};
use crate::store::checksum::crc32c;

/// `"YNMANIF1"`, little-endian.
const MAGIC: u64 = 0x3146_494E_414D_4E59;
/// 2 since the leadership term landed. Bumped **deliberately**, even though
/// the term went into slack the CRC already covered and an older binary would
/// therefore checksum a v2 slot correctly: what such a binary would do next is
/// use the file while ignoring the term, which is precisely the unfenced
/// behaviour the term exists to stop. Refusing is the point.
const FMT_MAJOR: u16 = 2;
/// Still read, so a database written before the term existed opens with term 0
/// and is rewritten as v2 by the next manifest write.
const FMT_MAJOR_NO_TERM: u16 = 1;

/// One slot. Generous: the payload is 556 bytes and the slack is what lets a
/// field be added without moving the second slot, which would make an older
/// binary read slot B as garbage rather than as a version it can refuse.
pub const SLOT: usize = 4096;
/// The whole file: two slots.
pub const MANIFEST_BYTES: usize = SLOT * 2;

const OFF_MAGIC: usize = 0;
const OFF_MAJOR: usize = 8;
const OFF_VSHARDS: usize = 10;
const OFF_SEQ: usize = 16;
const OFF_UUID: usize = 24;
const OFF_SHARDS: usize = 40;
/// **In the four unused bytes between `shards` and the map, and that choice
/// is the whole migration story.** Appending after the map would move `OFF_CRC`,
/// and an older binary reading the new file would checksum the wrong range,
/// call a perfectly good slot torn, and report the database unreadable. Here
/// the CRC range is unchanged, so an older binary validates the slot and then
/// refuses it on the version — a specific error instead of a scary one.
///
/// A `u32` rather than the `u64` the WAL record's `term` field uses, because
/// that is what fits without moving anything. Four billion promotions is not a
/// limit anyone reaches; a worse migration story is a cost paid on every
/// upgrade.
const OFF_TERM: usize = 44;
const OFF_MAP: usize = 48;
const OFF_CRC: usize = OFF_MAP + 2 * VSHARDS as usize;
const END: usize = OFF_CRC + 4;

/// Identity and routing for one database.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    pub seq: u64,
    pub uuid: [u8; 16],
    pub shards: u32,
    /// The leadership term this database is at.
    ///
    /// **Inherited, never locally generated.** A standby takes it with the
    /// MANIFEST and a promotion sets it to one above the highest it has seen, so
    /// terms from different nodes are comparable — which is the entire point.
    /// Not `Db::epoch()`, which looks like the same thing and is not: that
    /// increments on every open, including an ordinary restart, and lives in a
    /// per-directory lock file, so two nodes' epochs are unrelated numbers.
    pub term: u32,
    /// `vshard -> shard`, one entry per [`VSHARDS`].
    pub map: Vec<u16>,
}

impl Manifest {
    /// The map a freshly created database gets.
    ///
    /// `v % shards` reproduces exactly what `shard_of` computed before this
    /// module existed, so an existing database's routing is unchanged by
    /// introducing the indirection — the map is only free to differ once
    /// something rewrites it.
    pub fn create(uuid: [u8; 16], shards: u32) -> Self {
        assert!(shards > 0, "a database needs at least one shard");
        Manifest {
            seq: 1,
            uuid,
            shards,
            // A database that has never failed over is at term 0. The first
            // promotion makes it 1.
            term: 0,
            map: (0..VSHARDS).map(|v| (v % shards) as u16).collect(),
        }
    }

    /// The shard owning `vshard`.
    #[inline]
    pub fn shard_of_vshard(&self, vshard: u32) -> usize {
        self.map[vshard as usize] as usize
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut b = vec![0u8; SLOT];
        b[OFF_MAGIC..OFF_MAGIC + 8].copy_from_slice(&MAGIC.to_le_bytes());
        b[OFF_MAJOR..OFF_MAJOR + 2].copy_from_slice(&FMT_MAJOR.to_le_bytes());
        b[OFF_VSHARDS..OFF_VSHARDS + 2].copy_from_slice(&(VSHARDS as u16).to_le_bytes());
        b[OFF_SEQ..OFF_SEQ + 8].copy_from_slice(&self.seq.to_le_bytes());
        b[OFF_UUID..OFF_UUID + 16].copy_from_slice(&self.uuid);
        b[OFF_SHARDS..OFF_SHARDS + 4].copy_from_slice(&self.shards.to_le_bytes());
        b[OFF_TERM..OFF_TERM + 4].copy_from_slice(&self.term.to_le_bytes());
        for (i, &s) in self.map.iter().enumerate() {
            let o = OFF_MAP + i * 2;
            b[o..o + 2].copy_from_slice(&s.to_le_bytes());
        }
        let crc = crc32c(&b[..OFF_CRC]);
        b[OFF_CRC..OFF_CRC + 4].copy_from_slice(&crc.to_le_bytes());
        b
    }

    /// `Ok(None)` for a slot that is absent, unwritten or torn — the caller
    /// takes the other one. Past the checksum, problems are real errors: a
    /// valid CRC means the writer meant these bytes.
    pub fn decode(b: &[u8]) -> Result<Option<Self>> {
        if b.len() < END {
            return Ok(None);
        }
        if u64::from_le_bytes(b[OFF_MAGIC..OFF_MAGIC + 8].try_into().unwrap()) != MAGIC {
            return Ok(None);
        }
        let stored = u32::from_le_bytes(b[OFF_CRC..OFF_CRC + 4].try_into().unwrap());
        if crc32c(&b[..OFF_CRC]) != stored {
            return Ok(None); // torn slot
        }

        let major = u16::from_le_bytes(b[OFF_MAJOR..OFF_MAJOR + 2].try_into().unwrap());
        if major != FMT_MAJOR && major != FMT_MAJOR_NO_TERM {
            return Err(CodecError::UnsupportedEncoding);
        }
        // Refused rather than adapted. The map's length *is* `VSHARDS`, so a
        // file written under a different value cannot be reinterpreted — it
        // routes differently, and a reader that guessed would return wrong
        // answers instead of an error.
        let vshards = u16::from_le_bytes(b[OFF_VSHARDS..OFF_VSHARDS + 2].try_into().unwrap());
        if u32::from(vshards) != VSHARDS {
            return Err(CodecError::Invariant(
                "manifest was written with a different VSHARDS",
            ));
        }
        let shards = u32::from_le_bytes(b[OFF_SHARDS..OFF_SHARDS + 4].try_into().unwrap());
        if shards == 0 {
            return Err(CodecError::Invariant("manifest names zero shards"));
        }
        let map: Vec<u16> = (0..VSHARDS as usize)
            .map(|i| {
                let o = OFF_MAP + i * 2;
                u16::from_le_bytes(b[o..o + 2].try_into().unwrap())
            })
            .collect();
        // Every entry must name a shard that exists, or routing would index off
        // the end of the shard vector at the first key that hashed there.
        if map.iter().any(|&s| u32::from(s) >= shards) {
            return Err(CodecError::Invariant(
                "manifest maps a vshard to a nonexistent shard",
            ));
        }
        Ok(Some(Manifest {
            seq: u64::from_le_bytes(b[OFF_SEQ..OFF_SEQ + 8].try_into().unwrap()),
            uuid: b[OFF_UUID..OFF_UUID + 16].try_into().unwrap(),
            shards,
            // A v1 file has zeroes here, which reads as term 0 — the right
            // answer for a database that predates failover, and the reason no
            // separate migration step is needed.
            term: u32::from_le_bytes(b[OFF_TERM..OFF_TERM + 4].try_into().unwrap()),
            map,
        }))
    }
}

/// The live slot's index and image: the valid slot with the higher `seq`.
///
/// The tie goes to A, and that matters rather than being arbitrary: a
/// freshly created file has the *same* `seq` in both slots ( see
/// [`initial_file`] ), so without a fixed rule "which slot is live" would be
/// undefined exactly once per database — at the first update, which is the one
/// that has to get it right.
fn pick_slot(bytes: &[u8]) -> Result<Option<(usize, Manifest)>> {
    let a = Manifest::decode(bytes.get(..SLOT).unwrap_or(&[]))?;
    let b = Manifest::decode(bytes.get(SLOT..MANIFEST_BYTES).unwrap_or(&[]))?;
    Ok(match (a, b) {
        (Some(a), Some(b)) => Some(if b.seq > a.seq { (1, b) } else { (0, a) }),
        (Some(a), None) => Some((0, a)),
        (None, Some(b)) => Some((1, b)),
        (None, None) => None,
    })
}

/// The live manifest: the valid slot with the higher `seq`.
pub fn pick(bytes: &[u8]) -> Result<Option<Manifest>> {
    Ok(pick_slot(bytes)?.map(|(_, m)| m))
}

/// Byte offset an in-place update must be written to, given the file as it
/// currently stands: the slot [`pick`] is **not** returning.
///
/// The live slot is left untouched, so a crash at any point during the write
/// leaves `pick` returning the old manifest ( the new slot is incomplete and
/// fails its CRC ) or the new one ( it is whole ) — never nothing.
///
/// `Ok(None)` when neither slot is readable. There is then no live generation to
/// preserve and the caller should write the whole file; `Err` when a slot
/// checksums correctly but this build cannot use it, because clobbering bytes
/// the writer demonstrably meant is not a recovery.
pub fn next_slot_offset(bytes: &[u8]) -> Result<Option<u64>> {
    Ok(pick_slot(bytes)?.map(|(live, _)| ((1 - live) * SLOT) as u64))
}

/// Serialize both slots for a freshly created database.
///
/// **The one write that legitimately touches both slots**, and only because
/// there is no prior generation to lose: until something rewrites the manifest
/// there is no older version worth keeping, and leaving slot B unwritten would
/// mean a torn write to slot A during creation loses the database's identity
/// entirely. Every *later* write goes to one slot — see [`next_slot_offset`].
pub fn initial_file(m: &Manifest) -> Vec<u8> {
    let mut out = m.encode();
    out.extend_from_slice(&m.encode());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m() -> Manifest {
        Manifest::create([7u8; 16], 4)
    }

    #[test]
    fn a_manifest_round_trips() {
        let a = m();
        let got = Manifest::decode(&a.encode()).unwrap().unwrap();
        assert_eq!(got, a);
        assert_eq!(got.map.len(), VSHARDS as usize);
    }

    /// The map a new database gets must reproduce the modulo that `shard_of`
    /// computed before the indirection existed, or introducing it would silently
    /// re-route every existing database.
    #[test]
    fn a_fresh_map_reproduces_the_modulo_it_replaced() {
        for shards in [1u32, 3, 4, 8, 300] {
            let a = Manifest::create([0; 16], shards);
            for v in 0..VSHARDS {
                assert_eq!(
                    a.shard_of_vshard(v),
                    (v % shards) as usize,
                    "shards={shards} vshard={v}"
                );
            }
        }
    }

    #[test]
    fn a_torn_slot_yields_none_rather_than_an_error() {
        let mut b = m().encode();
        b[OFF_UUID] ^= 0xFF; // after the CRC was computed
        assert_eq!(Manifest::decode(&b).unwrap(), None);
        assert_eq!(Manifest::decode(&[]).unwrap(), None);
        assert_eq!(Manifest::decode(&vec![0u8; SLOT]).unwrap(), None);
    }

    /// The whole reason there are two slots: a torn write to one must leave the
    /// other usable, in both directions.
    #[test]
    fn pick_takes_the_higher_sequence_and_survives_either_slot_tearing() {
        let old = m();
        let mut new = m();
        new.seq = 2;
        new.map[0] = 3;

        let mut f = old.encode();
        f.extend_from_slice(&new.encode());
        assert_eq!(pick(&f).unwrap().unwrap(), new, "higher seq wins");

        let mut torn_b = old.encode();
        torn_b.extend_from_slice(&new.encode());
        torn_b[SLOT + OFF_UUID] ^= 0xFF;
        assert_eq!(pick(&torn_b).unwrap().unwrap(), old, "tearing B leaves A");

        let mut torn_a = old.encode();
        torn_a[OFF_UUID] ^= 0xFF;
        torn_a.extend_from_slice(&new.encode());
        assert_eq!(pick(&torn_a).unwrap().unwrap(), new, "tearing A leaves B");

        assert_eq!(pick(&vec![0u8; MANIFEST_BYTES]).unwrap(), None);
    }

    /// Past the checksum these are errors, not `None`. A valid CRC means the
    /// bytes are what the writer intended, so falling back to the other slot
    /// would hide a real incompatibility behind a stale manifest.
    #[test]
    fn a_checksummed_but_unusable_manifest_is_an_error_not_a_fallback() {
        let reseal = |mut b: Vec<u8>| {
            let crc = crc32c(&b[..OFF_CRC]);
            b[OFF_CRC..OFF_CRC + 4].copy_from_slice(&crc.to_le_bytes());
            b
        };

        let mut b = m().encode();
        b[OFF_MAJOR..OFF_MAJOR + 2].copy_from_slice(&(FMT_MAJOR + 1).to_le_bytes());
        assert!(Manifest::decode(&reseal(b)).is_err(), "future format");

        let mut b = m().encode();
        b[OFF_VSHARDS..OFF_VSHARDS + 2].copy_from_slice(&64u16.to_le_bytes());
        assert!(Manifest::decode(&reseal(b)).is_err(), "different VSHARDS");

        let mut b = m().encode();
        b[OFF_SHARDS..OFF_SHARDS + 4].copy_from_slice(&0u32.to_le_bytes());
        assert!(Manifest::decode(&reseal(b)).is_err(), "zero shards");

        // The one that would be a panic rather than an error if unchecked: an
        // entry naming a shard the database does not have.
        let mut b = m().encode();
        b[OFF_MAP..OFF_MAP + 2].copy_from_slice(&9u16.to_le_bytes());
        assert!(Manifest::decode(&reseal(b)).is_err(), "map out of range");
    }

    /// The write side's whole contract: an update targets the slot `pick` is
    /// not returning, so the live one survives the write.
    #[test]
    fn an_update_targets_the_slot_that_is_not_live() {
        // A freshly created file: both slots carry seq 1, `pick` breaks the tie
        // towards A, so the first update must go to B.
        let f = initial_file(&m());
        assert_eq!(next_slot_offset(&f).unwrap(), Some(SLOT as u64));

        // B now holds the higher seq and is live, so the next update goes to A.
        let mut newer = m();
        newer.seq = 2;
        let mut f2 = m().encode();
        f2.extend_from_slice(&newer.encode());
        assert_eq!(next_slot_offset(&f2).unwrap(), Some(0));

        // And back again.
        let mut newest = m();
        newest.seq = 3;
        let mut f3 = newest.encode();
        f3.extend_from_slice(&newer.encode());
        assert_eq!(next_slot_offset(&f3).unwrap(), Some(SLOT as u64));

        // A torn slot is not live however high its `seq` would have been, so the
        // update reclaims it rather than overwriting the one good copy.
        let mut torn_b = m().encode();
        torn_b.extend_from_slice(&newer.encode());
        torn_b[SLOT + OFF_UUID] ^= 0xFF;
        assert_eq!(next_slot_offset(&torn_b).unwrap(), Some(SLOT as u64));

        let mut torn_a = m().encode();
        torn_a[OFF_UUID] ^= 0xFF;
        torn_a.extend_from_slice(&newer.encode());
        assert_eq!(next_slot_offset(&torn_a).unwrap(), Some(0));
    }

    /// Nothing readable means nothing to preserve, and the caller writes the
    /// whole file instead of picking a slot.
    #[test]
    fn no_live_slot_names_no_target() {
        assert_eq!(next_slot_offset(&[]).unwrap(), None);
        assert_eq!(next_slot_offset(&vec![0u8; MANIFEST_BYTES]).unwrap(), None);
    }

    /// Simulates a crash at **every** byte offset of a one-slot update and
    /// asserts the invariant the two slots exist for: `pick` always returns a
    /// manifest, and it is either the outgoing generation or the incoming one —
    /// never nothing, and never something older than either.
    #[test]
    fn a_crash_at_any_offset_of_a_one_slot_update_leaves_a_usable_manifest() {
        let old = {
            let mut o = m();
            o.seq = 9;
            o
        };
        let new = {
            let mut n = m();
            n.seq = 10;
            n.term = 4;
            n
        };
        // Slot A live at seq 9; B holds an older generation that must never be
        // the answer once A exists.
        let mut stale = m();
        stale.seq = 8;
        let mut before = old.encode();
        before.extend_from_slice(&stale.encode());

        let off = next_slot_offset(&before).unwrap().unwrap() as usize;
        assert_eq!(off, SLOT, "the live slot must not be the target");
        let image = new.encode();

        for k in 0..=SLOT {
            // The prefix the writer managed to land before dying.
            let mut torn = before.clone();
            torn[off..off + k].copy_from_slice(&image[..k]);
            let got = pick(&torn).unwrap().expect("no manifest at any offset");
            assert!(got == old || got == new, "k={k} yielded {got:?}");
            if k == 0 {
                assert_eq!(got, old);
            }
            if k == SLOT {
                assert_eq!(got, new);
            }

            // A torn sector need not be a clean prefix: the tail may hold
            // anything. The CRC is what has to catch it, not the shape.
            for fill in [0x00u8, 0xFF, 0x5A] {
                let mut junk = torn.clone();
                junk[off + k..off + SLOT].fill(fill);
                let got = pick(&junk).unwrap().expect("no manifest at any offset");
                assert!(got == old || got == new, "k={k} fill={fill:#x}");
            }
        }
    }

    #[test]
    fn the_initial_file_writes_both_slots() {
        let a = m();
        let f = initial_file(&a);
        assert_eq!(f.len(), MANIFEST_BYTES);
        assert_eq!(Manifest::decode(&f[..SLOT]).unwrap().unwrap(), a);
        assert_eq!(Manifest::decode(&f[SLOT..]).unwrap().unwrap(), a);
    }
}
