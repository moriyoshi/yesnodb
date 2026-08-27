//! `yesno-core` — Roaring-style sparse ordinal sets.
//!
//! An ordinal is a `u64`. The low 16 bits select a slot within a *chunk*; the
//! high 48 bits (`ordinal >> 16`) are the chunk's [`Prefix48`]. A set is a sorted
//! sequence of `(Prefix48, Container)`.
//!
//! Container payloads are byte-identical to the portable Roaring serialization
//! format, which buys a byte-level differential test against the `roaring` crate
//! and `O(container count)` import of `.roaring` files.

#![deny(unsafe_op_in_unsafe_fn)]

pub mod bignum;
// Crate-private: `arrow_buffer` types must not appear in this crate's stable
// public API, so an arrow-buffer major bump stays a patch release ( policy R1 ).
// The deliberate handoff points live in `unstable_arrow`, which is documented as
// semver-exempt.
pub(crate) mod buffer;
pub mod checkpoint;
pub mod container;
pub(crate) mod db;
pub mod dispatch;
pub(crate) mod error;
pub mod events;
pub(crate) mod index;
pub mod matrix;
pub mod mvcc;
pub mod ops;
pub mod pack;
pub mod repl;
pub mod roaring_format;
pub(crate) mod set;
pub mod store;
pub mod stream;
pub mod unstable_arrow;
pub mod view;
pub mod wal;

/// The identity of the database in `dir`, without opening it.
///
/// The replication bootstrap needs this before it has a `Db` — a follower checks
/// it against the leader's before accepting a byte of WAL. It exists as a
/// supported call because the alternative is what `yesno-replication` did until
/// 2026-08-28: read `dir/UUID` directly with `unwrap_or_default()`, which
/// reached behind the crate's back **and** degraded to sixteen zero bytes when
/// the file was absent, so a missing identity compared equal to another missing
/// identity.
///
/// Falls back to the pre-MANIFEST `UUID` file so an older database still
/// answers.
pub fn database_uuid(dir: impl AsRef<std::path::Path>) -> Result<[u8; 16]> {
    let dir = dir.as_ref();
    if let Ok(b) = std::fs::read(dir.join("MANIFEST")) {
        return match db::manifest::pick(&b)? {
            Some(m) => Ok(m.uuid),
            None => Err(CodecError::ManifestUnreadable),
        };
    }
    match std::fs::read(dir.join("UUID")) {
        Ok(b) if b.len() == 16 => Ok(b.try_into().unwrap()),
        _ => Err(CodecError::Invariant(
            "no database identity in that directory",
        )),
    }
}

/// Where WAL replay must begin for a shard image — read out of the image.
///
/// **This is not the log's current length**, and shipping that instead is a
/// silent data-loss bug rather than an inefficiency. A checkpoint records the
/// first logical LSN its image does not contain. Everything committed between
/// that checkpoint and the moment the image is copied lies between that replay
/// position and the log's present end, so a follower told to resume at the end
/// skips exactly those commits — no error, no gap, a replica quietly missing a
/// window's worth of writes.
///
/// The image and the offset have to come from the same source for that reason:
/// the superblock is the only place that knows which records its own contents
/// already include.
///
/// Takes the bytes rather than a path because the caller shipping the image is
/// already holding them.
pub fn wal_replay_offset(shard_image: &[u8]) -> Result<u64> {
    let page = store::PAGE;
    if shard_image.len() < 2 * page {
        return Err(CodecError::Invariant(
            "shard image is too short to hold a superblock",
        ));
    }
    match store::superblock::pick(&shard_image[..page], &shard_image[page..2 * page])? {
        Some(sb) => Ok(sb.wal_replay_lsn),
        None => Err(CodecError::Invariant(
            "both superblock slots of that shard image are unreadable",
        )),
    }
}

/// The leadership term recorded in `dir`'s MANIFEST, without opening it.
///
/// **Not [`Db::epoch`]**, which looks like the same thing and is not. An epoch
/// is a per-directory lock counter that increments on *every* open, including an
/// ordinary restart — so two nodes' epochs are unrelated numbers and comparing
/// them says nothing. A term is inherited with the MANIFEST, which is exactly
/// what makes it comparable across nodes and therefore usable for fencing.
///
/// A database that has never failed over is at term 0, and so is one written
/// before the field existed.
pub fn database_term(dir: impl AsRef<std::path::Path>) -> Result<u32> {
    let b = std::fs::read(dir.as_ref().join("MANIFEST"))
        .map_err(|_| CodecError::Invariant("no manifest in that directory"))?;
    match db::manifest::pick(&b)? {
        Some(m) => Ok(m.term),
        None => Err(CodecError::ManifestUnreadable),
    }
}

/// Record a new leadership term for `dir`, and return it.
///
/// This is what a promotion does, and it must happen **before** the database is
/// opened as a leader: the term is read at open and stamped onto every record
/// written afterwards, so setting it later would leave a leadership's first
/// records claiming the previous one.
///
/// Refuses to go backwards. A term that could decrease is not a fence — the
/// whole mechanism is that a higher number wins, and a caller that has computed
/// a lower one has misread something rather than decided something.
///
/// Refuses while the database is open, because the open `Db` holds the
/// manifest it read and would keep stamping the old term.
///
/// **Crash-safe at every byte.** The manifest update writes only the slot the
/// reader is not currently returning, so a crash anywhere in this call leaves
/// the directory at either the old term or the new one — never at an unreadable
/// manifest. That matters here more than anywhere else: this is the call an
/// operator makes during a failover, on what is about to be the only copy. See
/// `db::write_manifest`.
pub fn promote_database(dir: impl AsRef<std::path::Path>, term: u32) -> Result<u32> {
    let dir = dir.as_ref();
    let path = dir.join("MANIFEST");
    let b =
        std::fs::read(&path).map_err(|_| CodecError::Invariant("no manifest in that directory"))?;
    let Some(mut m) = db::manifest::pick(&b)? else {
        return Err(CodecError::ManifestUnreadable);
    };
    if term <= m.term {
        return Err(CodecError::Invariant(
            "a promotion must raise the term; a term that can decrease is not a fence",
        ));
    }
    // Taking the lock is how "is anything using this directory" is asked
    // everywhere else, and the answer has to be no.
    let _guard = db::acquire_lock(dir)?;
    m.term = term;
    m.seq += 1;
    db::write_manifest(&path, &m)?;
    Ok(term)
}

pub use container::{Container, ContainerKind};
// `SpaceAmpPolicy` is here because `DbOptions::on_space_amp` is a **public
// field of a public struct**, and without the re-export its type had no name
// outside this crate — so a caller could read the field but could not construct
// the struct with anything but its default. That is a hole in the API rather
// than a widening of it; the type is already `#[non_exhaustive]`.
pub use db::keystream::{KeySource, KeyStream};
pub use db::{BackupLease, Db, DbOptions, Snapshot, SpaceAmpPolicy, WriteBatch};
pub use error::{CodecError, Result};
pub use set::{OrdSet, RangeSummary};
pub use stream::{ChunkStream, ChunkStreamExt, Expr};

/// High 48 bits of an ordinal: the chunk address. Invariant: `< 1 << 48`.
pub type Prefix48 = u64;

/// Largest storable ordinal: `2^64 - 2`. **`u64::MAX` is not an ordinal.**
///
/// # Why the universe stops one short (invariant I8)
///
/// Reserving the top value buys three things that are otherwise unobtainable,
/// and they are the same thing seen from three sides:
///
/// - **Every cardinality fits a `u64`.** The universe holds `2^64 - 1` ordinals,
///   so a full set's `len()` is exactly `u64::MAX`. Without this, a set of every
///   `u64` would have cardinality `2^64` and `len()` would wrap to `0`.
/// - **The complement is countable.** `!s` over the whole universe has
///   cardinality `ORDINAL_MAX + 1 - len()`, always a valid `u64`. This is what
///   makes an unbounded complement expressible at all — as the *lazy*
///   [`stream::ChunkStreamExt::not`] and `!`[`Expr`], whose `cardinality()`
///   costs one step per chunk of the input. There is deliberately no eager
///   `OrdSet::not()`: see [`OrdSet::not_in_range`].
/// - **A half-open range can name the whole universe.** `[0, u64::MAX)` covers
///   every ordinal, so the exclusive upper bound never needs the unrepresentable
///   `2^64`. The two conventions in this crate now line up exactly: an inclusive
///   bound maxes out at `ORDINAL_MAX`, an exclusive one at `u64::MAX`.
///
/// The cost is real and worth stating: a foreign CRoaring 64-bit file containing
/// `2^64 - 1` cannot be represented, and
/// [`roaring_format::deserialize_u64`]
/// rejects it rather than silently dropping the value — dropping it would break
/// the round-trip identity that makes `O(container count)` import legitimate.
///
/// # Where this is enforced
///
/// At the fallible boundaries: [`Db`] mutators, [`WriteBatch::commit`], and the
/// roaring import path all return [`CodecError::OrdinalOutOfRange`]. Since every
/// path to durable state is one of those, an out-of-range ordinal cannot reach
/// the WAL, a container, or a page.
///
/// The in-memory [`OrdSet`] mutators are infallible by design and carry it as a
/// documented precondition with a `debug_assert`. Nothing checks it
/// structurally *on disk*: a container is prefix-agnostic, so
/// [`container::codec::validate`] cannot see which chunk it belongs to, and
/// `fsck` does not decode payloads. A release build that calls
/// `OrdSet::insert(u64::MAX)` directly and then hands the set to
/// [`WriteBatch::store_set`] is caught there by a `max()` check — but a
/// hypothetical future path that bypasses both would not be.
pub const ORDINAL_MAX: u64 = u64::MAX - 1;

/// Is `ordinal` storable? See [`ORDINAL_MAX`].
#[inline]
pub const fn is_valid_ordinal(ordinal: u64) -> bool {
    ordinal <= ORDINAL_MAX
}

/// `Err(OrdinalOutOfRange)` if `ordinal` is not storable.
#[inline]
pub fn check_ordinal(ordinal: u64) -> Result<()> {
    if is_valid_ordinal(ordinal) {
        Ok(())
    } else {
        Err(CodecError::OrdinalOutOfRange { ordinal })
    }
}

/// Bits of an ordinal handled *within* a container.
pub const CHUNK_BITS: u32 = 16;
/// Ordinals per chunk. Fixed by the `u16` container value width, and by the
/// resulting 8 KiB bitmap being L1-resident — not a tuning knob.
pub const CHUNK_CARD: u32 = 1 << CHUNK_BITS;

/// Max values in an array container. Above this it must become a bitmap:
/// `4096 * 2 B == 8192 B == the bitmap size`, so an array can never be larger.
pub const ARRAY_MAX: usize = 4096;
/// Words in a bitmap container.
pub const BITMAP_WORDS: usize = 1024;
/// Bytes in a bitmap container.
pub const BITMAP_BYTES: usize = 8192;

/// Demote bitmap -> array *below* this, not at 4096.
///
/// The 512-value gap is deliberate hysteresis. Without it, alternating
/// insert/remove at exactly 4096 costs an 8 KiB conversion on every operation.
/// It is asymmetric on purpose: bitmaps are cheap to hold and mutate, so being
/// lazy about demotion is free, while being late about promotion is not an option.
pub const BITMAP_DEMOTE: u32 = 3584;

/// Max intervals our writer emits in a run container: `2 + 4*2032 = 8130 <= 8192`.
///
/// This is a capacity choice for our size classes, *not* a spec limit. Decode
/// accepts up to [`RUN_DECODE_MAX`] so foreign CRoaring files stay readable.
pub const RUN_MAX_INTERVALS: u32 = 2032;
/// Theoretical maximum intervals in a 2^16 chunk, accepted on decode.
pub const RUN_DECODE_MAX: u32 = 32768;

/// Switch encodings only on a >= 12.5% byte saving: `new * 8 <= old * 7`.
/// Stops a container oscillating between encodings on every commit.
pub const OPT_GAIN_NUM: usize = 7;
pub const OPT_GAIN_DEN: usize = 8;

/// Length ratio above which array-array intersection gallops instead of merging.
pub const GALLOP_RATIO: usize = 32;

/// Split an ordinal into `(prefix48, low16)`.
#[inline]
pub const fn split(ordinal: u64) -> (Prefix48, u16) {
    (ordinal >> CHUNK_BITS, (ordinal & 0xFFFF) as u16)
}

/// Rejoin a `(prefix48, low16)` pair. `prefix` must be `< 1 << 48`.
#[inline]
pub const fn join(prefix: Prefix48, low: u16) -> u64 {
    (prefix << CHUNK_BITS) | low as u64
}

/// First ordinal of a chunk.
#[inline]
pub const fn chunk_base(prefix: Prefix48) -> u64 {
    prefix << CHUNK_BITS
}

/// The half-open ordinal window `[lo, hi)` intersected with chunk `prefix`,
/// expressed as offsets within that chunk, or `None` if they do not overlap.
///
/// **Half-open**, and the whole file is careful about this because yesno
/// spells ranges both ways on purpose — `Db::insert_range` is inclusive
/// `[lo, hi]`, `Expr::range` and this are `[lo, hi)`. The half-open form is what
/// a Parquet row group's `[start, start + count)` already is, and the pushdown
/// path is the reason this exists.
///
/// The returned `hi` may be `CHUNK_CARD` ( 65 536 ), which is one past the
/// largest `u16`, so both bounds are `u32`. A window of exactly
/// `(0, CHUNK_CARD)` means the chunk is wholly covered, which is the case a
/// caller answers from `card_m1` without touching a payload.
#[inline]
pub(crate) fn chunk_window(prefix: Prefix48, lo: u64, hi: u64) -> Option<(u32, u32)> {
    let base = chunk_base(prefix);
    let end = base.saturating_add(CHUNK_CARD as u64);
    if hi <= base || lo >= end {
        return None;
    }
    let l = lo.saturating_sub(base).min(CHUNK_CARD as u64) as u32;
    // `hi` is exclusive and may sit past this chunk, in which case the window
    // runs to the chunk's end.
    let h = hi.saturating_sub(base).min(CHUNK_CARD as u64) as u32;
    (l < h).then_some((l, h))
}

/// Serialized payload size of an array container, in bytes.
#[inline]
pub const fn array_bytes(card: u32) -> usize {
    2 * card as usize
}

/// Serialized payload size of a run container, in bytes: the `nruns` u16 prefix
/// plus four bytes per interval, exactly as the Roaring spec defines it.
#[inline]
pub const fn run_bytes(nruns: u32) -> usize {
    2 + 4 * nruns as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_join_roundtrip() {
        for &o in &[0u64, 1, 65535, 65536, 65537, u64::MAX, (1 << 48) - 1] {
            let (p, l) = split(o);
            assert_eq!(join(p, l), o, "roundtrip failed for {o}");
        }
    }

    #[test]
    fn prefix_fits_48_bits() {
        let (p, _) = split(u64::MAX);
        assert_eq!(p, (1u64 << 48) - 1);
    }

    #[test]
    fn size_models_match_roaring_spec() {
        // A full array and a bitmap are the same payload size: that is what makes
        // array->bitmap promotion a same-class rewrite.
        assert_eq!(array_bytes(ARRAY_MAX as u32), BITMAP_BYTES);
        // Our run cap must fit the universal 8192-byte payload bound.
        assert!(run_bytes(RUN_MAX_INTERVALS) <= BITMAP_BYTES);
        // Run beats bitmap iff 2 + 4n < 8192, i.e. n <= 2047.
        assert!(run_bytes(2047) < BITMAP_BYTES);
        assert!(run_bytes(2048) > BITMAP_BYTES - 64);
    }
}
