use thiserror::Error;

/// Reasons a byte range failed to decode into a container.
///
/// Decode is a fuzz target: it must return one of these for *any* input rather
/// than panicking, and must never construct a container violating its invariants.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CodecError {
    #[error("payload length {len} is not valid for {kind:?} with cardinality {card}")]
    BadLength {
        kind: &'static str,
        len: usize,
        card: u32,
    },

    #[error("cardinality {0} out of range (must be 1..=65536)")]
    BadCardinality(u32),

    #[error("run count {0} out of range (must be 1..=32768)")]
    BadRunCount(u32),

    #[error("payload at offset {off} with length {len} exceeds buffer of {buf_len} bytes")]
    OutOfBounds {
        off: usize,
        len: usize,
        buf_len: usize,
    },

    #[error("unknown container kind byte {0}")]
    UnknownKind(u8),

    /// A standalone extent's trailer does not carry the tag of the key that
    /// reached it.
    ///
    /// **Carries its evidence on purpose.** This fires for two very different
    /// causes and the message has to say which: either the reference is genuinely
    /// mis-pointed ( a chunk was reclaimed and its cell reused, or an index
    /// search returned a neighbouring entry ), or the *trailer offset* is being
    /// computed wrongly, which happens if the slab's recorded class disagrees
    /// with the class the extent was written under. Those need opposite fixes,
    /// and `cell` plus `class` plus `trailer_off` distinguish them without a
    /// debugger: a plausible tag at the wrong offset is the second, garbage at
    /// the right offset is the first.
    #[error(
        "extent for chunk key {key:#034x} at cell {cell} points at another chunk: \
         slab class {class} puts the trailer at {trailer_off}, holding tag \
         {found:#010x} where {expected:#010x} was required"
    )]
    MisPointedExtent {
        key: u128,
        cell: u64,
        class: u8,
        trailer_off: u64,
        found: u32,
        expected: u32,
    },

    #[error("alternative array encoding (enc=1) is not supported by this build")]
    UnsupportedEncoding,

    #[error("this build is little-endian only; the host is big-endian")]
    UnsupportedEndianness,

    #[error("database at {0} is already open by another process")]
    AlreadyOpen(String),

    /// The snapshot was invalidated to bound space amplification.
    ///
    /// Carries the version it held and the last key it is known to have read,
    /// so a long scan can retry at a newer snapshot from where it stopped
    /// rather than from the beginning.
    ///
    /// Only *future* reads fail. Data already materialized through this
    /// snapshot stays sound, because reclamation condition 3 is the `Buffer`
    /// refcount and is independent of condition 1.
    #[error(
        "this database is open as a read-only replica; writes must go to its leader. \
         Promote it first if this node is meant to lead."
    )]
    ReadOnlyReplica,
    #[error("snapshot at version {version} was evicted to bound space amplification")]
    SnapshotTooOld { version: u64, last_key: Option<u64> },

    /// A read was asked for at a version whose history a checkpoint has since
    /// discarded.
    ///
    /// **Distinct from [`SnapshotTooOld`](Self::SnapshotTooOld)**, which
    /// retires a snapshot that was already granted. This one refuses to grant it
    /// at all, and the difference matters to the caller: `SnapshotTooOld` says
    /// "you were reading and must restart", this says "that version is gone,
    /// ask for a current one".
    ///
    /// A checkpoint collapses every chunk's version chain down to `floor`, so
    /// below it the database no longer holds the intermediate states — reading
    /// there would answer from `floor` while claiming to answer from
    /// `requested`. That silent substitution is the whole reason this variant
    /// exists: `Db::snapshot_at` could have returned the newer state and nobody
    /// would have noticed.
    #[error(
        "version {requested} is no longer readable: a checkpoint has discarded history \
         below version {floor}. Take a fresh snapshot instead of retrying this one."
    )]
    VersionReclaimed { requested: u64, floor: u64 },

    /// A read was asked for at a version that has not been committed yet.
    ///
    /// Almost always a caller reading its own writes through a *stale* handle —
    /// or a Flight ticket minted against one leader and presented to another.
    /// Not retryable by waiting: a version this database never assigned will
    /// never become visible here.
    #[error("version {requested} is not visible; the newest readable version is {visible}")]
    VersionNotVisible { requested: u64, visible: u64 },

    /// A wait for a version to become readable ran out of time.
    ///
    /// Distinct from [`VersionNotVisible`](Self::VersionNotVisible) on purpose,
    /// and the distinction is the point: that one says the version is not
    /// readable *now*, this one says a caller asked to be told when it became
    /// readable and it did not. Retrying is meaningful. It does **not**
    /// prove the version exists — a version this database never assigned times
    /// out identically, because the watermark carries no record of versions
    /// above it.
    #[error(
        "version {requested} was still not readable after {waited_micros} us; \
         the newest readable version is {visible}"
    )]
    VisibilityTimeout {
        requested: u64,
        visible: u64,
        waited_micros: u64,
    },

    #[error("structural invariant violated: {0}")]
    Invariant(&'static str),
    /// A shard file belongs to a different database.
    ///
    /// The `db_uuid` has been written into every superblock since M3 and was
    /// **compared nowhere** until 2026-08-28 — its stated purpose, "a stable
    /// identity so a shard file cannot be mixed into another database", was
    /// enforced by nothing. Same shape as `check_alignment` having no caller and
    /// `ChunkRef::validate` running only from `fsck`.
    #[error("shard file belongs to a different database")]
    DatabaseIdentityMismatch,

    /// A shard file belongs to this database but to a *different shard of it*.
    ///
    /// [`DatabaseIdentityMismatch`](Self::DatabaseIdentityMismatch) cannot
    /// catch this: two shard images of one database carry the same `db_uuid`,
    /// so exchanging `shard-0000.yno` with `shard-0001.yno` passed every check
    /// there was and the database opened cleanly, answering each shard's
    /// queries out of the other shard's extents. Wrong answers, no error.
    ///
    /// The physical shard number has been in every superblock since the format
    /// existed, and the shard number in the filename is what the opener already
    /// passes down, so the two are simply compared.
    #[error("shard file holds physical shard {found}, but shard {expected} was opened")]
    ShardIdentityMismatch { expected: u32, found: u32 },

    /// The MANIFEST exists but neither slot is readable.
    ///
    /// Deliberately not recoverable by writing a fresh one: the manifest
    /// carries the `vshard -> shard` map, and a re-created map would route keys
    /// to shards that do not hold them. An absent manifest beside existing shard
    /// files is a different case — that is a pre-MANIFEST database, and is
    /// migrated.
    #[error("both manifest slots are unreadable")]
    ManifestUnreadable,

    /// A follower asked to resume from an offset that is inside this log but is
    /// not the start of a record in it.
    ///
    /// The cursor is almost always *stale* rather than corrupt: a checkpoint
    /// cuts the shard's log and the next records are written from offset zero
    /// again, so every LSN a follower holds becomes meaningless the moment its
    /// leader checkpoints. Until 2026-08-28 this was answered with a **heartbeat**
    /// — "you have everything I have" — while the leader held megabytes the
    /// follower had never seen, and the catch-up returned `Ok`.
    ///
    /// It is reported rather than repaired because there is no local repair: the
    /// only correct response is to bootstrap again from a fresh base image.
    #[error("requested lsn {0} is inside this log but is not a record boundary")]
    WalCursorNotOnRecordBoundary(u64),

    #[error("truncated input: expected {expected} bytes, found {found}")]
    Truncated { expected: usize, found: usize },

    #[error("bad magic/cookie value {0:#010x}")]
    BadCookie(u32),

    /// An ordinal exceeded [`ORDINAL_MAX`](crate::ORDINAL_MAX) (`2^64 - 2`).
    ///
    /// `u64::MAX` is reserved so that every cardinality fits a `u64` and a
    /// half-open range can name the whole universe. The most likely source is a
    /// foreign CRoaring 64-bit file, which may legitimately contain `2^64 - 1`.
    #[error("ordinal {ordinal} exceeds ORDINAL_MAX (2^64-2); u64::MAX is reserved")]
    OrdinalOutOfRange { ordinal: u64 },
}

pub type Result<T, E = CodecError> = std::result::Result<T, E>;
