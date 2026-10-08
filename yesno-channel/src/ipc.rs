//! The out-of-process plugin channel: yesnod serves, a peer asks.
//!
//! # Why this is here and not in `yesno-wire`
//!
//! It was there briefly. `yesno-wire` exists so that **one** definition of a format
//! is compiled into two first-party crates that cannot depend on each other --
//! `yesno-flight` decodes and `yesno-pg` encodes, and under Bazel they resolve
//! third-party crates through different hubs. That reasoning does not apply to this
//! protocol: its two sides are yesnod and a third-party peer **outside this
//! workspace**, so there is no crate boundary for a shared definition to bridge, and
//! the only first-party consumer is this crate.
//!
//! It also put pressure where it did not belong. `yesno-wire` must stay
//! dependency-free, and that rule is load-bearing for `yesno-pg`; a second,
//! unrelated format living there means a future need of *this* protocol becomes a
//! reason to weaken it.
//!
//! **This module has no dependencies of its own**, deliberately. If an external
//! Rust peer ever wants the codec without the engine, splitting it into its own
//! crate is then a file move rather than an untangling -- and it should wait for
//! that consumer to exist.
//!
//! # The shape, and why it is this one
//!
//! yesnod remains the **only** process that opens the database directory. A peer
//! runs beside it -- another process, possibly another container -- connects over a
//! Unix socket, and asks for data. It never touches the data directory, never takes
//! the lock, and needs no filesystem access at all.
//!
//! The alternative considered and rejected was a shared database: the peer calling
//! `Db::open_reader` and mapping the same extents itself. That needs no protocol,
//! but it costs three things this shape keeps. The peer would need read-write
//! filesystem access to the data directory, which is a far larger trust surface than
//! a socket. Its reads would be limited to **checkpoint-visible** state, because a
//! foreign reader replays no log and has no memtable. And the database would have
//! two processes reading its files, which is the invariant the locking design exists
//! to keep down to one writer and makes every later question about backup, restore
//! and volume layout harder.
//!
//! # Liveness is the connection, and that is the whole reclamation story
//!
//! The snapshot a peer reads through belongs to **yesnod**, keyed by the connection
//! that asked for it. So when the socket closes -- peer exit, crash, container stop,
//! `SIGKILL` -- the kernel tells yesnod, and yesnod drops the snapshot itself.
//!
//! That is the forcible reclamation the in-process ABI structurally cannot have,
//! where only `ReaderSlot::drop` frees a lease and no host call takes one back. It
//! is also namespace-independent, unlike the `kill( pid, 0 )` and
//! `/proc/<pid>/stat` liveness the cross-process `READERS` registry uses, which is
//! why a peer here registers nothing in that file: it holds no reader of its own to
//! keep alive.
//!
//! An alive-but-hung peer still holds its snapshot, and that is correct rather than
//! a gap. Declaring a slow reader dead is the one direction that loses data, which
//! is the same reason `db::readers` refuses a heartbeat.
//!
//! # Where the bytes travel
//!
//! Control frames go over the socket. Chunk payloads go through a shared memory
//! region that yesnod creates and passes to the peer as a file descriptor over the
//! same socket, so **no shared filesystem is needed** -- an anonymous `memfd` sent
//! with `SCM_RIGHTS` crosses a container boundary that a bind-mounted path may not.
//!
//! Establishing that region is the transport's job and not this module's: passing a
//! descriptor is a socket operation, not a frame. What is defined here is the
//! arithmetic both sides must agree on, below.
//!
//! # The arena, and why it needs no allocator
//!
//! **A container's payload never exceeds 8192 bytes.** That is not a chosen margin,
//! it is the exact maximum of the three representations: an array holds at most
//! `ARRAY_MAX` = 4096 `u16` values, a bitmap is exactly `BITMAP_WORDS` = 1024 `u64`
//! words, and a run holds at most `RUN_MAX_INTERVALS` = 2032 four-byte intervals --
//! 8192, 8192 and 8128 bytes. So [`LANE_BYTES`] is a true upper bound and not a
//! guess.
//!
//! Each acquired lanes handle therefore owns a fixed arena of `lanes * LANE_BYTES`,
//! and **lane `i` always begins at `arena_off + i * LANE_BYTES`**. There is no free
//! list, no fragmentation, and no offset in the wire format: a lane's position is a
//! function of its index, so the two sides cannot disagree about where it is. A
//! descriptor carries only the kind and the count, and the byte length follows from
//! those, so there is no length field to contradict them either.
//!
//! [`Frame::BlockRelease`] is what says the arena may be overwritten. In the
//! in-process ABI the equivalent call existed to make a use-after-release
//! diagnosable; here it is load-bearing flow control, and a peer that skips it will
//! read a later block's bytes.
//!
//! # One request at a time
//!
//! A connection is strictly request-then-response. A peer wanting concurrency opens
//! more connections, which also matches the in-process rule that one lanes handle is
//! never shared between threads. There is no correlation identifier because there is
//! nothing to correlate.
//!
//! The one exception is that the server may send a **notification** at any point,
//! including where the peer expected a response. Availability and generation changes
//! cannot wait for the peer to ask, and a peer that discovered a replacement only on
//! its next request would serve answers from a database that no longer exists.
//! Notifications are a separate discriminant range so this is unambiguous.
//!
//! # This is a parser of untrusted bytes
//!
//! Same contract as `yesno_wire::SetExpr::decode`: for **any** input, [`Frame::decode`]
//! answers `Err` or a valid frame and never panics. There is no recursion, so the
//! bounds are on sizes alone -- [`MAX_PAYLOAD`], [`MAX_STRING`] and [`MAX_LANES`].
//! The last one is not decoration: without it a twelve-byte header could declare
//! millions of lanes and the server would size an arena from a peer's arithmetic.
//!
//! # Framing
//!
//! ```text
//!   0      "YSNL"          4 bytes, MAGIC
//!   4      version         u8, VERSION
//!   5      flags           u8, must be 0
//!   6      kind            u8, a Kind discriminant
//!   7      reserved        u8, must be 0
//!   8      payload length  u32 little-endian, <= MAX_PAYLOAD
//!  12      payload         that many bytes
//! ```
//!
//! The magic is on every frame, not only the first. A lifecycle-and-request stream
//! is low-rate, so six bytes cost nothing and they let a reader that has lost sync
//! refuse the rest rather than interpret it.
//!
//! Strings are a `u32` little-endian length then that many bytes of **UTF-8**,
//! validated rather than replaced: a name or address that is not UTF-8 is a
//! configuration error worth reporting, and a lossy conversion moves the failure
//! somewhere it cannot be explained.

/// `YSNL`: yesno lifecycle. Distinct from `yesno-wire`'s `YSNX`, which introduces a
/// set expression, so neither stream can be decoded as the other.
pub const MAGIC: &[u8; 4] = b"YSNL";

/// Protocol version. Bumped when a field changes meaning, never for a new kind.
pub const VERSION: u8 = 2;

/// Magic, version, flags, kind, reserved, length.
pub const HEADER_LEN: usize = 12;

/// Bytes reserved per lane in the shared arena.
///
/// The exact maximum of a container's three encodings, not a margin: array
/// `4096 * 2`, bitmap `1024 * 8`, run `2032 * 4`.
pub const LANE_BYTES: usize = 8192;

/// Most lanes one frame may declare.
///
/// # Structural, not policy, and the difference is why this was wrong
///
/// This was 256, chosen to bound "the arena a peer can ask the server to reserve".
/// That is a **policy** limit, and the server already advertises its own as
/// `max_lanes` in [`Frame::ServerHello`] -- so the constant was doing nothing except
/// refusing valid requests. A consumer found it by trying to open 265 lanes, 256
/// query dimensions plus nine planes, which is one ordinary exact query and which a
/// direct adapter could not express at all.
///
/// What a structural cap must actually bound is decode: a declared lane count
/// causes one `Vec` reservation, at 8 bytes per key in a request and 5 bytes per
/// descriptor in a response. [`MAX_PAYLOAD`] already bounds both -- 64 KiB holds
/// 8190 keys or 13 104 descriptors -- so this is the second, tighter bound and
/// wants only to be *a* bound, not a small one. 4096 matches
/// `yesno_wire::MAX_VIEW_SETS`, which is the same kind of limit on the same kind of
/// fan-out.
///
/// **A peer's real limit is the one in the greeting**, which is configuration and
/// may be far lower. Exceeding that is a refusal it can read, rather than a
/// protocol wall it cannot negotiate past.
pub const MAX_LANES: usize = 4096;

/// Largest frame payload. Requests and descriptors only; bulk goes through the arena.
pub const MAX_PAYLOAD: usize = 64 * 1024;

/// Longest string in any payload.
pub const MAX_STRING: usize = 4096;

/// Why a frame could not be decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IpcError {
    /// Fewer bytes than the frame declares; call again with more.
    Truncated,
    /// No frame magic: this is not this protocol.
    NotAFrame,
    Version(u8),
    /// A reserved or flags byte was non-zero, so a later version's meaning would be
    /// assumed rather than negotiated.
    Reserved,
    UnknownKind(u8),
    /// A length exceeded [`MAX_PAYLOAD`], [`MAX_STRING`] or [`MAX_LANES`].
    TooLarge,
    /// The payload does not match its kind, or a string is not UTF-8.
    Malformed,
}

impl core::fmt::Display for IpcError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            IpcError::Truncated => write!(f, "the frame is incomplete"),
            IpcError::NotAFrame => write!(f, "the bytes do not begin with a frame magic"),
            IpcError::Version(v) => write!(f, "protocol version {v} is not supported"),
            IpcError::Reserved => write!(f, "a reserved field was not zero"),
            IpcError::UnknownKind(k) => write!(f, "message kind {k} is not known"),
            IpcError::TooLarge => write!(f, "a declared length exceeds the permitted maximum"),
            IpcError::Malformed => write!(f, "the payload does not match its kind"),
        }
    }
}

impl std::error::Error for IpcError {}

/// Which end of a replication pair the server is. Values match the in-process ABI's
/// `yesno_role`, so one vocabulary serves both transports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Role {
    Leader = 0,
    Follower = 1,
}

impl Role {
    fn from_raw(v: u8) -> Result<Role, IpcError> {
        match v {
            0 => Ok(Role::Leader),
            1 => Ok(Role::Follower),
            _ => Err(IpcError::Malformed),
        }
    }
}

/// How a lane's payload is encoded in the arena.
///
/// Values match the in-process ABI's `yesno_chunk_kind` for the same reason as
/// [`Role`]: a peer that speaks both should not need two tables.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum LaneKind {
    /// `count` `u16` values, ascending.
    Array = 0,
    /// `count` `u64` words; always 1024.
    Bitmap = 1,
    /// `count` `[start, end]` `u16` pairs, ascending.
    Run = 2,
    /// This lane holds nothing at this block. Reported, never omitted, so the
    /// peer's lane indices never shift under it.
    Absent = 3,
}

/// What one entry of a [`Frame::Apply`] does.
///
/// Discriminants match Flight's `OP_*` constants and `yesno-wire`'s mutation ops for the
/// same reason [`Role`] and [`LaneKind`] match the in-process ABI: a peer that speaks both
/// transports should not need two tables, and a mismatched table is the kind of error that
/// shows up as the wrong operation rather than as a decode failure.
///
/// Every one of these is **idempotent**: applying it twice is applying it once. That is not
/// an incidental property, it is what makes a write channel possible here at all -- see
/// [`Frame::Apply`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum WriteOp {
    /// One ordinal. `lo == hi`.
    Insert = 0,
    /// One ordinal. `lo == hi`.
    Remove = 1,
    /// `lo ..= hi`, inclusive and ascending.
    InsertRange = 2,
    /// `lo ..= hi`, inclusive and ascending.
    RemoveRange = 3,
    /// The whole key. `lo == hi == 0`, because a whole-key delete names no range.
    DeleteKey = 4,
}

impl WriteOp {
    fn from_raw(v: u8) -> Result<WriteOp, IpcError> {
        Ok(match v {
            0 => WriteOp::Insert,
            1 => WriteOp::Remove,
            2 => WriteOp::InsertRange,
            3 => WriteOp::RemoveRange,
            4 => WriteOp::DeleteKey,
            _ => return Err(IpcError::Malformed),
        })
    }
}

/// One entry of a write batch: 25 bytes on the wire, matching the Flight mutation row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Write {
    pub key: u64,
    pub lo: u64,
    pub hi: u64,
    pub op: WriteOp,
}

/// Bytes one [`Write`] occupies in an [`Frame::Apply`] payload: three `u64` and the op.
pub const WRITE_BYTES: usize = 25;

/// Exact payload bytes an [`Frame::Apply`] of `writes` entries occupies.
///
/// A `u32` count followed by `writes` fixed-size entries. `const` so the cap below is
/// *derived* from the entry limit rather than asserted to match it.
pub const fn apply_payload_bytes(writes: usize) -> usize {
    4 + WRITE_BYTES * writes
}

/// Entries one [`Frame::Apply`] may carry.
///
/// # This number used to be a lie, and the shape of the lie is the lesson
///
/// It was `16 * 1024` while `Apply` was bound by [`MAX_PAYLOAD`] like every other request,
/// so the effective limit was `( 65 536 - 4 ) / 25` = **2 621** and a frame of 2 622 failed
/// in `Frame::encode` before it was sent. The doc comment claimed 400 KiB was "comfortably
/// inside [`MAX_INLINE_PAYLOAD`]" -- true of that constant and irrelevant, because nothing
/// gave `Apply` that cap. A consumer found it by exceeding it: a 15 000-write pilot failed
/// at encode, and their real-socket regression pinned the boundary at exactly 2 621 and
/// 2 622 ( reported 2026-10-04 ).
///
/// **The same file already recorded this exact failure** for lanes, in
/// `Session::with_arena`: "Inline capacity is a property of the transport, so the advertised
/// limits have to come from it. Configuration alone produced a greeting that promised more
/// than the frame could hold". I wrote an advertised limit that the frame could not hold,
/// one module away from the note saying not to.
///
/// So the cap is no longer written down twice. [`APPLY_MAX_PAYLOAD`] is computed from this
/// number by [`apply_payload_bytes`], and the two cannot disagree without failing to
/// compile. A peer should still read `max_writes` from the greeting rather than this
/// constant, because a server may be configured lower.
pub const MAX_WRITES: usize = 16 * 1024;

/// Payload cap for [`Kind::Apply`], derived from [`MAX_WRITES`].
///
/// 409 604 bytes. `Apply` is **bulk**, not a descriptor, so it does not belong under
/// [`MAX_PAYLOAD`] -- and it does not simply borrow [`Kind::BlocksInline`]'s cap either,
/// because the stated reason for having two caps is that "raising the inline limit never
/// widens what a control frame may demand". A third class keeps that property in both
/// directions: raising either of the other two cannot widen `Apply`, and raising `Apply`
/// cannot widen them.
pub const APPLY_MAX_PAYLOAD: usize = apply_payload_bytes(MAX_WRITES);

/// Fixed bytes of one [`ChunkWrite`], before its payload: key, prefix, kind, count, card.
pub const CHUNK_WRITE_FIXED: usize = 8 + 8 + 1 + 4 + 4;

/// Largest payload one chunk can carry.
///
/// A bitmap is the dense ceiling and is always [`BITMAP_WORDS`] words, so an
/// array of `ARRAY_MAX` `u16`s and a run of `RUN_MAX_INTERVALS` pairs both land at or
/// under it. Taken from the core constant rather than written as 8192, because the two
/// disagreeing is the bug this derivation exists to make impossible.
pub const CHUNK_PAYLOAD_MAX: usize = BITMAP_WORDS * 8;

/// Words in a dense bitmap container.
///
/// **A deliberate copy of the storage engine's `BITMAP_WORDS`**, because this
/// crate must not depend on it -- see the crate docs. It is a format constant
/// rather than a tunable, so copying it is safe in the way copying a threshold
/// would not be, and `yesno-plugin` carries a test asserting the two are equal:
/// that crate can see both and this one cannot.
pub const BITMAP_WORDS: usize = 1024;

/// Chunks one [`Frame::ChunkPut`] may carry.
///
/// **64 because that is the measured case**: `channel-dense-chunk-writes` records a
/// 64-chunk half-dense page at 34 126 004 bytes on the point wire against 526 948 as
/// container payloads, so one frame holds exactly the page the win was measured on.
pub const MAX_CHUNK_WRITES: usize = 64;

/// Bytes a [`Frame::ChunkPut`] of `writes` chunks occupies at its worst case.
pub const fn chunk_put_payload_bytes(writes: usize) -> usize {
    4 + writes * (CHUNK_WRITE_FIXED + CHUNK_PAYLOAD_MAX)
}

/// Payload cap for [`Kind::ChunkPut`], derived from the two constants above.
///
/// **Denominated in bytes, and that is the whole point.** Counting a chunk's *ordinals*
/// against an entry bound is what refused an 8 MiB page on the Flight path -- a dense
/// chunk is 65 536 ordinals and 8 192 bytes, so an ordinal-denominated bound rejects
/// exactly the payloads this frame exists to carry. Nothing here ever adds up
/// cardinalities, and [`MAX_WRITES`] is deliberately not consulted: these are different
/// quantities on the same protocol and conflating them is the recorded trap.
pub const CHUNK_PUT_MAX_PAYLOAD: usize = chunk_put_payload_bytes(MAX_CHUNK_WRITES);
const _: () = assert!(CHUNK_PUT_MAX_PAYLOAD <= MAX_INLINE_PAYLOAD);
const _: () = assert!(MAX_CHUNK_WRITES <= u32::MAX as usize);

impl LaneKind {
    fn from_raw(v: u8) -> Result<LaneKind, IpcError> {
        Ok(match v {
            0 => LaneKind::Array,
            1 => LaneKind::Bitmap,
            2 => LaneKind::Run,
            3 => LaneKind::Absent,
            _ => return Err(IpcError::Malformed),
        })
    }

    /// Bytes this lane occupies in the arena for `count` elements.
    ///
    /// Derived rather than transmitted: a length field could contradict the kind and
    /// the count, and the peer has to trust one of the three.
    pub fn payload_bytes(self, count: u32) -> usize {
        let n = count as usize;
        match self {
            LaneKind::Array => n * 2,
            LaneKind::Bitmap => n * 8,
            LaneKind::Run => n * 4,
            LaneKind::Absent => 0,
        }
    }
}

/// One chunk written as its container payload, rather than as one entry per set bit.
///
/// Reuses [`LaneKind`], which is how the **read** side already describes a chunk payload
/// in the arena and whose discriminants match the in-process ABI's `yesno_chunk_kind`.
/// Forking a second table for the write direction is what Flight declined to do when its
/// container wire reused `containers_schema` from its own read side.
///
/// `count` is elements as [`LaneKind::payload_bytes`] defines them -- values, words or
/// interval pairs -- so the payload's length is **derived and never transmitted**, for the
/// reason that method gives: a length field could contradict the kind and the count, and
/// the peer would have to trust one of the three.
///
/// `card` is the chunk's cardinality, which the storage engine's container decoder
/// needs and which is not derivable from a bitmap's fixed word count. It is checked
/// against `count` where the two must agree rather than trusted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChunkWrite {
    pub key: u64,
    /// The chunk's 48-bit prefix.
    pub prefix: u64,
    pub kind: LaneKind,
    /// Elements, as [`LaneKind::payload_bytes`] counts them.
    pub count: u32,
    /// Ordinals the chunk holds.
    pub card: u32,
    pub payload: Vec<u8>,
}

impl ChunkWrite {
    /// Whether the three self-describing fields agree with each other.
    ///
    /// Checked on both encode and decode, so a malformed record cannot reach
    /// `codec::decode` with a payload whose length its kind would misread. The
    /// container's own invariants stay `codec::decode`'s job -- that function is a fuzz
    /// target by contract, so a hostile payload is an error there rather than a panic,
    /// and re-implementing its checks here would fork them.
    fn self_consistent(&self) -> bool {
        if self.kind == LaneKind::Absent {
            // Absent describes a lane the read side chose to omit. There is no such
            // thing as writing it, and accepting it would mean a no-op frame that looks
            // like a write.
            return false;
        }
        if self.payload.len() != self.kind.payload_bytes(self.count) {
            return false;
        }
        if self.payload.len() > CHUNK_PAYLOAD_MAX {
            return false;
        }
        match self.kind {
            // An array stores one `u16` per ordinal, so the two counts are the same
            // number and a disagreement is a malformed record rather than a judgement.
            LaneKind::Array => self.count == self.card,
            // A bitmap is always the full word count whatever it holds.
            LaneKind::Bitmap => self.count as usize == BITMAP_WORDS,
            // A run's cardinality is the sum of its intervals, which `codec::decode`
            // derives from the payload; nothing is claimed about it here.
            LaneKind::Run => true,
            LaneKind::Absent => false,
        }
    }
}

/// One lane of an open block.
///
/// Carries no offset. Lane `i` begins at `arena_off + i * LANE_BYTES`, so its
/// position is a function of its index and cannot be misreported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lane {
    pub kind: LaneKind,
    pub count: u32,
}

/// One block of a batch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    pub prefix: u64,
    pub lanes: Vec<Lane>,
}

/// Largest payload an inline frame may carry.
///
/// Larger than [`MAX_PAYLOAD`] because an inline frame carries the lane payloads
/// themselves, and a single block of `L` lanes is up to `L * LANE_BYTES` -- 512 KiB
/// at 64 lanes. **This is the coupling shared memory exists to break**: with the
/// arena, batch size is limited by the arena; inline, it is limited by this, so a
/// wide scan gets a smaller batch and pays more round trips. The number is a policy
/// choice bounded by what a host is willing to allocate for one frame.
pub const MAX_INLINE_PAYLOAD: usize = 1024 * 1024;

/// Bulk needs more room than descriptors, checked at compile time because it is a
/// relationship between two constants rather than a property of any run.
const _: () = assert!(MAX_INLINE_PAYLOAD > MAX_PAYLOAD);

// `Apply` is bulk and must be able to carry more than a descriptor frame, or the entry
// limit is unreachable again -- which is precisely the defect this derivation exists to
// make impossible.
const _: () = assert!(APPLY_MAX_PAYLOAD > MAX_PAYLOAD);
// And it stays inside the largest frame the reader will ever buffer, so one cap bounds
// what a connection can hold.
const _: () = assert!(APPLY_MAX_PAYLOAD <= MAX_INLINE_PAYLOAD);
// The count is transmitted as a `u32`.
const _: () = assert!(MAX_WRITES <= u32::MAX as usize);

/// Most `u64` values one page of [`Frame::Ordinals`] or [`Frame::Keys`] may carry.
///
/// `4096 * 8` is 32 KiB, half of [`MAX_PAYLOAD`], so a page leaves room for the
/// frame around it and for the cap to rise without the two colliding.
pub const MAX_PAGE: usize = 4096;

/// Most blocks one [`Frame::Blocks`] may carry.
///
/// Bounds the arena a batched handle reserves -- `max_blocks * max_lanes *
/// LANE_BYTES` -- and bounds what a single frame can declare.
pub const MAX_BATCH: usize = 64;

/// Where lane `i` of block `j` begins, for a batched handle.
///
/// One function rather than the arithmetic at each call site, for the same reason
/// as [`lane_offset`]: the two sides getting it separately right is the failure
/// this layout exists to prevent.
pub fn batched_lane_offset(arena_off: u64, max_lanes: usize, block: usize, lane: usize) -> u64 {
    arena_off
        + (block as u64) * (max_lanes as u64) * (LANE_BYTES as u64)
        + (lane as u64) * (LANE_BYTES as u64)
}

/// Which direction a kind travels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Peer to server.
    Request,
    /// Server to peer, answering a request.
    Response,
    /// Server to peer, unsolicited.
    Notification,
}

/// Message kinds. Discriminants are part of the protocol.
///
/// Three ranges rather than two: a server that receives a notification, or a peer
/// that receives a request, has a wiring mistake that the kind byte alone reveals.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    // requests, 1..63
    ClientHello = 1,
    SnapshotOpen = 2,
    SnapshotClose = 3,
    LanesAcquire = 4,
    LanesRelease = 5,
    BlockAdvance = 6,
    BlockRelease = 7,
    BlockAdvanceMany = 8,
    SnapshotCardinality = 9,
    SnapshotContains = 10,
    SnapshotMax = 11,
    SnapshotLoad = 12,
    SnapshotKeyRange = 13,
    Apply = 14,
    ChunkPut = 15,
    // responses, 64..127
    ServerHello = 64,
    SnapshotOpened = 65,
    LanesAcquired = 66,
    Block = 67,
    BlockDone = 68,
    Done = 69,
    Fault = 70,
    Blocks = 71,
    BlocksInline = 72,
    Count = 73,
    Bool = 74,
    Ordinal = 75,
    Ordinals = 76,
    Keys = 77,
    Committed = 78,
    // notifications, 128..191
    Unavailable = 128,
    Available = 129,
    GenerationChanged = 130,
    RoleChanged = 131,
}

impl Kind {
    fn from_raw(v: u8) -> Result<Kind, IpcError> {
        Ok(match v {
            1 => Kind::ClientHello,
            2 => Kind::SnapshotOpen,
            3 => Kind::SnapshotClose,
            4 => Kind::LanesAcquire,
            5 => Kind::LanesRelease,
            6 => Kind::BlockAdvance,
            7 => Kind::BlockRelease,
            8 => Kind::BlockAdvanceMany,
            9 => Kind::SnapshotCardinality,
            10 => Kind::SnapshotContains,
            11 => Kind::SnapshotMax,
            12 => Kind::SnapshotLoad,
            13 => Kind::SnapshotKeyRange,
            14 => Kind::Apply,
            15 => Kind::ChunkPut,
            64 => Kind::ServerHello,
            65 => Kind::SnapshotOpened,
            66 => Kind::LanesAcquired,
            67 => Kind::Block,
            68 => Kind::BlockDone,
            69 => Kind::Done,
            70 => Kind::Fault,
            71 => Kind::Blocks,
            72 => Kind::BlocksInline,
            73 => Kind::Count,
            74 => Kind::Bool,
            75 => Kind::Ordinal,
            76 => Kind::Ordinals,
            77 => Kind::Keys,
            78 => Kind::Committed,
            128 => Kind::Unavailable,
            129 => Kind::Available,
            130 => Kind::GenerationChanged,
            131 => Kind::RoleChanged,
            other => return Err(IpcError::UnknownKind(other)),
        })
    }

    /// Largest payload a frame of this kind may carry.
    ///
    /// **Three classes, and the split is deliberate in both directions.** Descriptors and
    /// requests get [`MAX_PAYLOAD`]; `BlocksInline` carries lane payloads and gets its own
    /// larger bound; `Apply` carries a write batch and gets a bound derived from
    /// [`MAX_WRITES`]. Raising any one of the three cannot widen the others, which is the
    /// property the original two-cap comment asked for.
    ///
    /// **One function rather than a `matches!` in encode and another in decode.** Those two
    /// expressions were the same by inspection and had to stay that way; an asymmetric pair
    /// would let a peer send what the server would not accept, or the reverse, and neither
    /// is visible from either site alone.
    pub fn payload_cap(self) -> usize {
        match self {
            Kind::BlocksInline => MAX_INLINE_PAYLOAD + MAX_PAYLOAD,
            Kind::Apply => APPLY_MAX_PAYLOAD,
            Kind::ChunkPut => CHUNK_PUT_MAX_PAYLOAD,
            _ => MAX_PAYLOAD,
        }
    }

    /// Which way this kind travels.
    pub fn direction(self) -> Direction {
        match self as u8 {
            0..=63 => Direction::Request,
            64..=127 => Direction::Response,
            _ => Direction::Notification,
        }
    }
}

/// One message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Frame {
    // --- requests ---
    ClientHello {
        protocol: u32,
        name: String,
    },
    SnapshotOpen,
    /// Apply a batch of writes as **one commit**, answered by [`Frame::Committed`].
    ///
    /// # Why this is one frame and not a transaction
    ///
    /// The channel was read-only until 2026-10-04, and the recorded reason was that
    /// extending it to writes "requires a transaction identity, an unambiguous commit point,
    /// and a response for a disconnect after commit; socket-close reclamation does not
    /// answer those questions". A single-frame batch of idempotent operations answers all
    /// three, which is the whole argument for this shape:
    ///
    /// * **Identity** exists so that retrying a write that may already have landed is safe.
    ///   Every [`WriteOp`] is idempotent and a frame's entries are ordered and
    ///   deterministic, so replaying a frame leaves the same state as applying it once --
    ///   `[ DeleteKey, Insert ]` twice is `[ DeleteKey, Insert ]`. There is no
    ///   non-idempotent retry for an identity to make safe.
    /// * **The commit point** is this frame. One `Apply` is one `WriteBatch::commit`, and
    ///   the version it produced comes back in the response. There is no staging, so there
    ///   is nothing whose commit point could be in doubt.
    /// * **A disconnect after commit** is recoverable without a reply: the peer retries,
    ///   which is safe by the first point, or reads the key back, because this channel also
    ///   serves reads and [`Frame::SnapshotOpened`] carries a version.
    ///
    /// **What this shape does not offer is atomicity across frames.** A batch larger than
    /// [`MAX_WRITES`] is several commits, exactly as Flight's `PUT_INSERT` is per record
    /// batch. A caller needing one atomic bundle wider than a frame still wants Flight
    /// `PUT_APPLY`. That is the price of not needing the three answers above, and it is
    /// deliberate: a multi-frame transaction here would reintroduce every one of them.
    ///
    /// Refused with [`crate::abi::Status::WrongRole`] on a follower -- a replica that
    /// accepted a local write would diverge from its leader with nothing to detect it --
    /// and with `Unavailable` while the slot is empty during a rebootstrap.
    Apply {
        writes: Vec<Write>,
    },
    /// Write whole chunks as container payloads, in one commit.
    ///
    /// The dense counterpart of [`Frame::Apply`], and **interchangeable with it**: a
    /// chunk write unions into the key exactly as `Apply`'s inserts do, so the same
    /// final set is reachable either way and a differential test between the two
    /// encodings is the whole correctness claim. It replaces nothing -- `patch_chunk`
    /// is given an empty clear set, which is what `insert` already means.
    ///
    /// Bounded by [`CHUNK_PUT_MAX_PAYLOAD`] in **bytes**, never by ordinals; see that
    /// constant for why the distinction is load-bearing.
    ///
    /// Refused on a follower with [`crate::abi::Status::WrongRole`] and while the slot
    /// is empty with `Unavailable`, identically to [`Frame::Apply`]: the reasons are
    /// properties of writing at all, not of the encoding.
    ChunkPut {
        writes: Vec<ChunkWrite>,
    },
    SnapshotClose {
        snapshot: u64,
    },
    /// Every key as a lane, under one snapshot.
    ///
    /// One request rather than one per key, for the reason the in-process ABI gives:
    /// acquiring lanes separately takes a snapshot each, and a checkpoint between
    /// two of them scores one block against two database states.
    ///
    /// # When a query needs more lanes than the server allows
    ///
    /// Acquire **several handles on the same snapshot**. That preserves both
    /// properties a wide query needs: every handle reads the one pinned version, so
    /// the split cannot straddle a checkpoint, and handles coexist up to the
    /// advertised `max_handles`, so every lane is readable at once rather than in
    /// passes. The arena gives each handle its own slice, so their payloads do not
    /// alias.
    ///
    /// What is *not* safe is opening a second snapshot for the overflow, which is
    /// the mistake this frame exists to prevent in the first place.
    LanesAcquire {
        snapshot: u64,
        keys: Vec<u64>,
    },
    LanesRelease {
        lanes: u64,
    },
    BlockAdvance {
        lanes: u64,
    },
    /// The arena slice for this handle may be overwritten. Flow control, not hygiene.
    BlockRelease {
        lanes: u64,
    },
    /// How many ordinals a key holds.
    SnapshotCardinality {
        snapshot: u64,
        key: u64,
    },
    /// Whether a key holds an ordinal.
    SnapshotContains {
        snapshot: u64,
        key: u64,
        ordinal: u64,
    },
    /// The largest ordinal a key holds, if any.
    SnapshotMax {
        snapshot: u64,
        key: u64,
    },
    /// One page of a key's ordinals, ascending.
    ///
    /// # Continuation is by value, not by cursor
    ///
    /// `after` resumes strictly above an ordinal the caller already has, so there is
    /// no server-side cursor to leak, to expire, or to invalidate. That is sound
    /// **because the snapshot pins the version**: resuming from a value gives a
    /// consistent sequence with no state between calls, which a cursor handle would
    /// have to hold and account for.
    ///
    /// A set is unbounded and a frame is not, which is why this pages at all. It is
    /// not the hot path -- lanes are -- so the page travels in the frame rather than
    /// the arena.
    SnapshotLoad {
        snapshot: u64,
        key: u64,
        /// Resume strictly above this ordinal when `has_after` is 1.
        after: u64,
        has_after: u8,
        /// Capped at [`MAX_PAGE`] by the server whatever is asked.
        limit: u32,
    },
    /// One page of the keys present in `[lo, hi)`, ascending.
    ///
    /// Continue by setting `lo` above the last key returned, for the same reason as
    /// [`Frame::SnapshotLoad`]. **Each page costs a scan of the remaining range**,
    /// because the engine's key enumeration materializes rather than streaming, so a
    /// caller wanting many keys should ask for a large `limit` once rather than many
    /// small pages.
    SnapshotKeyRange {
        snapshot: u64,
        lo: u64,
        hi: u64,
        limit: u32,
    },
    /// Advance up to `max_blocks` at once, releasing the previous batch implicitly.
    ///
    /// **The implicit release is the point, and it is safe here for a reason the
    /// single-block path does not have.** [`Frame::BlockAdvance`] keeps release
    /// separate so that holding a pointer across a block boundary is diagnosable.
    /// Over a request-response channel the request *is* the boundary: a peer cannot
    /// ask for the next batch while still reading the last one, because asking is
    /// what ends the last one. A second round trip to say so would cost one
    /// notification per batch to restate what the first already proved.
    BlockAdvanceMany {
        lanes: u64,
        max_blocks: u32,
    },

    // --- responses ---
    /// Sent first, before any request is answered.
    ///
    /// `arena_bytes` is the whole shared region's size; the descriptor for it arrives
    /// out of band on the same socket. `max_lanes` and `max_handles` are the server's
    /// limits, at or below [`MAX_LANES`], so a peer learns them rather than
    /// discovering them by refusal.
    ServerHello {
        protocol: u32,
        generation: u64,
        role: Role,
        shards: u32,
        arena_bytes: u64,
        max_lanes: u32,
        max_handles: u32,
        /// Most blocks one [`Frame::Blocks`] will carry, whatever a peer asks for.
        ///
        /// Advertised rather than discovered, because it is the knob that decides
        /// the channel's cost: one block per round trip is several times slower than
        /// a batched one, so a peer that guessed low would be slow for a reason it
        /// could not see.
        max_blocks: u32,
        /// Entries one [`Frame::Apply`] may carry, for this server.
        ///
        /// **Advertised for the reason `max_blocks` is, and then some**: a peer that
        /// guesses high does not merely run slowly, it gets `TooLarge` from its own
        /// encoder before anything is sent. A consumer hit exactly that against a
        /// published constant the frame could not honour, so the number a peer should
        /// trust is this one -- it is the server's configured limit, already clamped to
        /// what the protocol and the frame allow.
        max_writes: u32,
    },
    SnapshotOpened {
        snapshot: u64,
        version: u64,
    },
    /// `arena_off` is where this handle's `lanes * LANE_BYTES` slice begins.
    LanesAcquired {
        lanes: u64,
        arena_off: u64,
    },
    /// A block is open. Lane `i` of `lanes` describes the arena slot at index `i`.
    Block {
        prefix: u64,
        lanes: Vec<Lane>,
    },
    /// The scan is finished; no block is open.
    BlockDone,
    /// A request with nothing to return succeeded.
    Done,
    /// Up to `max_blocks` consecutive blocks.
    ///
    /// Block `j` occupies arena sub-slot `j`, so lane `i` of block `j` begins at
    /// `arena_off + j * max_lanes * LANE_BYTES + i * LANE_BYTES` -- the same
    /// index-derived arithmetic as a single block, with one more factor. See
    /// [`batched_lane_offset`].
    ///
    /// **Fewer blocks than asked for means the scan ended.** Exactly as many may or
    /// may not have more, which is the usual convention and is why a peer asks
    /// again rather than assuming.
    Blocks {
        blocks: Vec<Block>,
    },
    /// Blocks with their payloads **in the frame**, for a transport with no shared
    /// memory region.
    ///
    /// `payload` is every present lane's bytes concatenated in `( block, lane )`
    /// order, absent lanes contributing nothing. There are no offsets, for the same
    /// reason the arena has none: each lane's length follows from its kind and
    /// count, so position is derived rather than transmitted and the two sides
    /// cannot disagree about it.
    ///
    /// A server offers this when it advertised `arena_bytes == 0`. It costs a copy
    /// into the frame and a copy out, against the arena's one, and it caps the batch
    /// by [`MAX_INLINE_PAYLOAD`] rather than by the arena.
    ///
    /// # The cap bounds width times depth, not depth alone
    ///
    /// A block's worst case is `lanes * LANE_BYTES`, so `lanes * blocks` must fit
    /// [`MAX_INLINE_PAYLOAD`] -- 128 lane-blocks at the current constants. An inline
    /// server therefore advertises a `max_lanes` it can actually encode, which may be
    /// far below what its configuration asked for, and a peer needing more lanes
    /// splits across handles on one snapshot exactly as it would against any other
    /// advertised limit.
    ///
    /// **This bit once**, before the limits were derived from capacity: a server
    /// configured for 1024 lanes advertised them, a peer legally asked for 265, and
    /// one block of bitmap lanes was 2 MiB against a 1 MiB cap -- so the encoder
    /// refused a frame the server had promised and the connection died. The arena
    /// path never had this shape, because a `Block` frame carries only descriptors.
    BlocksInline {
        blocks: Vec<Block>,
        payload: Vec<u8>,
    },
    /// A count.
    Count {
        value: u64,
    },
    /// A yes or no.
    Bool {
        value: u8,
    },
    /// An ordinal that may be absent, which is what `max` of an empty key is.
    Ordinal {
        present: u8,
        value: u64,
    },
    /// One page of ordinals, ascending. `more` is 1 when another page follows.
    Ordinals {
        values: Vec<u64>,
        more: u8,
    },
    /// One page of keys, ascending. `more` is 1 when another page follows.
    Keys {
        values: Vec<u64>,
        more: u8,
    },
    /// What an [`Frame::Apply`] committed.
    ///
    /// `changed` is how many of the batch's operations actually altered the set, which is
    /// what the engine reports; a peer can use it to tell "already present" from "written"
    /// without reading back.
    Committed {
        version: u64,
        changed: u64,
    },
    /// A request failed. `status` is a [`crate::abi::Status`] discriminant.
    Fault {
        status: u32,
        message: String,
    },

    // --- notifications ---
    /// No database: a follower is rebootstrapping. Requests will fail until
    /// [`Frame::Available`].
    Unavailable,
    Available {
        generation: u64,
    },
    /// The database was replaced. Every handle the peer holds is stale, and every
    /// answer derived from one describes a database that no longer exists.
    GenerationChanged {
        old: u64,
        new: u64,
    },
    RoleChanged {
        from: Role,
        to: Role,
    },
}

impl Frame {
    pub fn kind(&self) -> Kind {
        match self {
            Frame::ClientHello { .. } => Kind::ClientHello,
            Frame::SnapshotOpen => Kind::SnapshotOpen,
            Frame::Apply { .. } => Kind::Apply,
            Frame::ChunkPut { .. } => Kind::ChunkPut,
            Frame::SnapshotClose { .. } => Kind::SnapshotClose,
            Frame::LanesAcquire { .. } => Kind::LanesAcquire,
            Frame::LanesRelease { .. } => Kind::LanesRelease,
            Frame::BlockAdvance { .. } => Kind::BlockAdvance,
            Frame::BlockRelease { .. } => Kind::BlockRelease,
            Frame::BlockAdvanceMany { .. } => Kind::BlockAdvanceMany,
            Frame::SnapshotCardinality { .. } => Kind::SnapshotCardinality,
            Frame::SnapshotContains { .. } => Kind::SnapshotContains,
            Frame::SnapshotMax { .. } => Kind::SnapshotMax,
            Frame::SnapshotLoad { .. } => Kind::SnapshotLoad,
            Frame::SnapshotKeyRange { .. } => Kind::SnapshotKeyRange,
            Frame::ServerHello { .. } => Kind::ServerHello,
            Frame::SnapshotOpened { .. } => Kind::SnapshotOpened,
            Frame::LanesAcquired { .. } => Kind::LanesAcquired,
            Frame::Block { .. } => Kind::Block,
            Frame::BlockDone => Kind::BlockDone,
            Frame::Done => Kind::Done,
            Frame::Fault { .. } => Kind::Fault,
            Frame::Blocks { .. } => Kind::Blocks,
            Frame::BlocksInline { .. } => Kind::BlocksInline,
            Frame::Count { .. } => Kind::Count,
            Frame::Bool { .. } => Kind::Bool,
            Frame::Ordinal { .. } => Kind::Ordinal,
            Frame::Ordinals { .. } => Kind::Ordinals,
            Frame::Keys { .. } => Kind::Keys,
            Frame::Committed { .. } => Kind::Committed,
            Frame::Unavailable => Kind::Unavailable,
            Frame::Available { .. } => Kind::Available,
            Frame::GenerationChanged { .. } => Kind::GenerationChanged,
            Frame::RoleChanged { .. } => Kind::RoleChanged,
        }
    }

    /// Append this frame's bytes to `out`.
    ///
    /// `Err` only where a field exceeds a documented cap, so an encoder cannot build
    /// a frame its own decoder would refuse. A round trip that fails one way is
    /// worse than an error where the value was supplied.
    pub fn encode_to(&self, out: &mut Vec<u8>) -> Result<(), IpcError> {
        let mut p = Vec::new();
        match self {
            Frame::ClientHello { protocol, name } => {
                p.extend_from_slice(&protocol.to_le_bytes());
                put_string(&mut p, name)?;
            }
            Frame::SnapshotOpen | Frame::BlockDone | Frame::Done | Frame::Unavailable => {}
            Frame::SnapshotClose { snapshot } => p.extend_from_slice(&snapshot.to_le_bytes()),
            Frame::Apply { writes } => {
                if writes.len() > MAX_WRITES {
                    return Err(IpcError::TooLarge);
                }
                p.extend_from_slice(&(writes.len() as u32).to_le_bytes());
                for w in writes {
                    p.extend_from_slice(&w.key.to_le_bytes());
                    p.extend_from_slice(&w.lo.to_le_bytes());
                    p.extend_from_slice(&w.hi.to_le_bytes());
                    p.push(w.op as u8);
                }
            }
            Frame::ChunkPut { writes } => {
                if writes.len() > MAX_CHUNK_WRITES {
                    return Err(IpcError::TooLarge);
                }
                p.extend_from_slice(&(writes.len() as u32).to_le_bytes());
                for w in writes {
                    if !w.self_consistent() {
                        return Err(IpcError::Malformed);
                    }
                    p.extend_from_slice(&w.key.to_le_bytes());
                    p.extend_from_slice(&w.prefix.to_le_bytes());
                    p.push(w.kind as u8);
                    p.extend_from_slice(&w.count.to_le_bytes());
                    p.extend_from_slice(&w.card.to_le_bytes());
                    p.extend_from_slice(&w.payload);
                }
            }
            Frame::LanesAcquire { snapshot, keys } => {
                if keys.len() > MAX_LANES {
                    return Err(IpcError::TooLarge);
                }
                p.extend_from_slice(&snapshot.to_le_bytes());
                p.extend_from_slice(&(keys.len() as u32).to_le_bytes());
                for k in keys {
                    p.extend_from_slice(&k.to_le_bytes());
                }
            }
            Frame::LanesRelease { lanes }
            | Frame::BlockAdvance { lanes }
            | Frame::BlockRelease { lanes } => p.extend_from_slice(&lanes.to_le_bytes()),
            Frame::ServerHello {
                protocol,
                generation,
                role,
                shards,
                arena_bytes,
                max_lanes,
                max_handles,
                max_blocks,
                max_writes,
            } => {
                p.extend_from_slice(&protocol.to_le_bytes());
                p.extend_from_slice(&generation.to_le_bytes());
                p.push(*role as u8);
                p.extend_from_slice(&shards.to_le_bytes());
                p.extend_from_slice(&arena_bytes.to_le_bytes());
                p.extend_from_slice(&max_lanes.to_le_bytes());
                p.extend_from_slice(&max_handles.to_le_bytes());
                p.extend_from_slice(&max_blocks.to_le_bytes());
                p.extend_from_slice(&max_writes.to_le_bytes());
            }
            Frame::SnapshotOpened { snapshot, version } => {
                p.extend_from_slice(&snapshot.to_le_bytes());
                p.extend_from_slice(&version.to_le_bytes());
            }
            Frame::Committed { version, changed } => {
                p.extend_from_slice(&version.to_le_bytes());
                p.extend_from_slice(&changed.to_le_bytes());
            }
            Frame::LanesAcquired { lanes, arena_off } => {
                p.extend_from_slice(&lanes.to_le_bytes());
                p.extend_from_slice(&arena_off.to_le_bytes());
            }
            Frame::Block { prefix, lanes } => {
                if lanes.len() > MAX_LANES {
                    return Err(IpcError::TooLarge);
                }
                p.extend_from_slice(&prefix.to_le_bytes());
                p.extend_from_slice(&(lanes.len() as u32).to_le_bytes());
                for l in lanes {
                    p.push(l.kind as u8);
                    p.extend_from_slice(&l.count.to_le_bytes());
                }
            }
            Frame::BlockAdvanceMany { lanes, max_blocks } => {
                p.extend_from_slice(&lanes.to_le_bytes());
                p.extend_from_slice(&max_blocks.to_le_bytes());
            }
            Frame::Blocks { blocks } => {
                if blocks.len() > MAX_BATCH {
                    return Err(IpcError::TooLarge);
                }
                p.extend_from_slice(&(blocks.len() as u32).to_le_bytes());
                for b in blocks {
                    if b.lanes.len() > MAX_LANES {
                        return Err(IpcError::TooLarge);
                    }
                    p.extend_from_slice(&b.prefix.to_le_bytes());
                    p.extend_from_slice(&(b.lanes.len() as u32).to_le_bytes());
                    for l in &b.lanes {
                        p.push(l.kind as u8);
                        p.extend_from_slice(&l.count.to_le_bytes());
                    }
                }
            }
            Frame::BlocksInline { blocks, payload } => {
                if blocks.len() > MAX_BATCH || payload.len() > MAX_INLINE_PAYLOAD {
                    return Err(IpcError::TooLarge);
                }
                let mut want = 0usize;
                p.extend_from_slice(&(blocks.len() as u32).to_le_bytes());
                for b in blocks {
                    if b.lanes.len() > MAX_LANES {
                        return Err(IpcError::TooLarge);
                    }
                    p.extend_from_slice(&b.prefix.to_le_bytes());
                    p.extend_from_slice(&(b.lanes.len() as u32).to_le_bytes());
                    for l in &b.lanes {
                        p.push(l.kind as u8);
                        p.extend_from_slice(&l.count.to_le_bytes());
                        want += l.kind.payload_bytes(l.count);
                    }
                }
                // Refused where the value was supplied rather than on decode: an
                // encoder that can emit a frame its own decoder rejects is worse
                // than an error at the point of construction.
                if want != payload.len() {
                    return Err(IpcError::Malformed);
                }
                p.extend_from_slice(&(payload.len() as u32).to_le_bytes());
                p.extend_from_slice(payload);
            }
            Frame::SnapshotCardinality { snapshot, key } | Frame::SnapshotMax { snapshot, key } => {
                p.extend_from_slice(&snapshot.to_le_bytes());
                p.extend_from_slice(&key.to_le_bytes());
            }
            Frame::SnapshotContains {
                snapshot,
                key,
                ordinal,
            } => {
                p.extend_from_slice(&snapshot.to_le_bytes());
                p.extend_from_slice(&key.to_le_bytes());
                p.extend_from_slice(&ordinal.to_le_bytes());
            }
            Frame::SnapshotLoad {
                snapshot,
                key,
                after,
                has_after,
                limit,
            } => {
                p.extend_from_slice(&snapshot.to_le_bytes());
                p.extend_from_slice(&key.to_le_bytes());
                p.extend_from_slice(&after.to_le_bytes());
                p.push(*has_after);
                p.extend_from_slice(&limit.to_le_bytes());
            }
            Frame::SnapshotKeyRange {
                snapshot,
                lo,
                hi,
                limit,
            } => {
                p.extend_from_slice(&snapshot.to_le_bytes());
                p.extend_from_slice(&lo.to_le_bytes());
                p.extend_from_slice(&hi.to_le_bytes());
                p.extend_from_slice(&limit.to_le_bytes());
            }
            Frame::Count { value } => p.extend_from_slice(&value.to_le_bytes()),
            Frame::Bool { value } => p.push(*value),
            Frame::Ordinal { present, value } => {
                p.push(*present);
                p.extend_from_slice(&value.to_le_bytes());
            }
            Frame::Ordinals { values, more } | Frame::Keys { values, more } => {
                if values.len() > MAX_PAGE {
                    return Err(IpcError::TooLarge);
                }
                p.extend_from_slice(&(values.len() as u32).to_le_bytes());
                for v in values {
                    p.extend_from_slice(&v.to_le_bytes());
                }
                p.push(*more);
            }
            Frame::Fault { status, message } => {
                p.extend_from_slice(&status.to_le_bytes());
                put_string(&mut p, message)?;
            }
            Frame::Available { generation } => p.extend_from_slice(&generation.to_le_bytes()),
            Frame::GenerationChanged { old, new } => {
                p.extend_from_slice(&old.to_le_bytes());
                p.extend_from_slice(&new.to_le_bytes());
            }
            Frame::RoleChanged { from, to } => {
                p.push(*from as u8);
                p.push(*to as u8);
            }
        }
        if p.len() > self.kind().payload_cap() {
            return Err(IpcError::TooLarge);
        }
        out.extend_from_slice(MAGIC);
        out.push(VERSION);
        out.push(0);
        out.push(self.kind() as u8);
        out.push(0);
        out.extend_from_slice(&(p.len() as u32).to_le_bytes());
        out.extend_from_slice(&p);
        Ok(())
    }

    pub fn encode(&self) -> Result<Vec<u8>, IpcError> {
        let mut v = Vec::new();
        self.encode_to(&mut v)?;
        Ok(v)
    }

    /// Decode one frame, returning it and the bytes consumed.
    ///
    /// `Err( Truncated )` means "call again with more", which is what lets a caller
    /// drive this from a stream without the codec owning the socket.
    pub fn decode(bytes: &[u8]) -> Result<(Frame, usize), IpcError> {
        if bytes.len() < HEADER_LEN {
            // Distinguished deliberately: whether a caller waits for more bytes or
            // gives up depends on it, and answering `NotAFrame` for a short read
            // turns a slow peer into a rejected one.
            if bytes.len() >= 4 && &bytes[..4] != MAGIC.as_slice() {
                return Err(IpcError::NotAFrame);
            }
            return Err(IpcError::Truncated);
        }
        if &bytes[..4] != MAGIC.as_slice() {
            return Err(IpcError::NotAFrame);
        }
        if bytes[4] != VERSION {
            return Err(IpcError::Version(bytes[4]));
        }
        if bytes[5] != 0 || bytes[7] != 0 {
            return Err(IpcError::Reserved);
        }
        let kind = Kind::from_raw(bytes[6])?;
        let len = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize;
        // Bulk kinds have their own, larger bounds; everything else is descriptors and
        // requests. Separate caps rather than one big one, so raising a bulk limit never
        // widens what a control frame may demand -- and checked here, before `len` is used
        // to size anything, so an over-cap declaration is refused rather than buffered.
        if len > kind.payload_cap() {
            return Err(IpcError::TooLarge);
        }
        let end = HEADER_LEN + len;
        if bytes.len() < end {
            return Err(IpcError::Truncated);
        }
        let mut r = Reader {
            b: &bytes[HEADER_LEN..end],
            at: 0,
        };
        let frame = match kind {
            Kind::ClientHello => Frame::ClientHello {
                protocol: r.u32()?,
                name: r.string()?,
            },
            Kind::SnapshotOpen => Frame::SnapshotOpen,
            Kind::SnapshotClose => Frame::SnapshotClose { snapshot: r.u64()? },
            Kind::LanesAcquire => {
                let snapshot = r.u64()?;
                let n = r.u32()? as usize;
                if n > MAX_LANES {
                    return Err(IpcError::TooLarge);
                }
                let mut keys = Vec::with_capacity(n);
                for _ in 0..n {
                    keys.push(r.u64()?);
                }
                Frame::LanesAcquire { snapshot, keys }
            }
            Kind::LanesRelease => Frame::LanesRelease { lanes: r.u64()? },
            Kind::Apply => {
                let n = r.u32()? as usize;
                // Checked before allocating, like every other counted field here: a
                // declared length is attacker-controlled and `with_capacity` on it is the
                // allocation an `an_enormous_declared_length_is_refused_before_allocating`
                // test exists to prevent.
                if n > MAX_WRITES {
                    return Err(IpcError::TooLarge);
                }
                let mut writes = Vec::with_capacity(n);
                for _ in 0..n {
                    let key = r.u64()?;
                    let lo = r.u64()?;
                    let hi = r.u64()?;
                    let op = WriteOp::from_raw(r.u8()?)?;
                    writes.push(Write { key, lo, hi, op });
                }
                Frame::Apply { writes }
            }
            Kind::ChunkPut => {
                let n = r.u32()? as usize;
                // Checked before allocating, for the reason the `Apply` arm gives: a
                // declared length is attacker-controlled.
                if n > MAX_CHUNK_WRITES {
                    return Err(IpcError::TooLarge);
                }
                let mut writes = Vec::with_capacity(n);
                for _ in 0..n {
                    let key = r.u64()?;
                    let prefix = r.u64()?;
                    let kind = LaneKind::from_raw(r.u8()?)?;
                    let count = r.u32()?;
                    let card = r.u32()?;
                    // The length comes from the kind and the count, so a short frame
                    // fails in `take` rather than yielding a truncated payload.
                    let payload = r.take(kind.payload_bytes(count))?.to_vec();
                    let w = ChunkWrite {
                        key,
                        prefix,
                        kind,
                        count,
                        card,
                        payload,
                    };
                    if !w.self_consistent() {
                        return Err(IpcError::Malformed);
                    }
                    writes.push(w);
                }
                Frame::ChunkPut { writes }
            }
            Kind::Committed => Frame::Committed {
                version: r.u64()?,
                changed: r.u64()?,
            },
            Kind::BlockAdvance => Frame::BlockAdvance { lanes: r.u64()? },
            Kind::BlockRelease => Frame::BlockRelease { lanes: r.u64()? },
            Kind::ServerHello => Frame::ServerHello {
                protocol: r.u32()?,
                generation: r.u64()?,
                role: Role::from_raw(r.u8()?)?,
                shards: r.u32()?,
                arena_bytes: r.u64()?,
                max_lanes: r.u32()?,
                max_handles: r.u32()?,
                max_blocks: r.u32()?,
                max_writes: r.u32()?,
            },
            Kind::SnapshotOpened => Frame::SnapshotOpened {
                snapshot: r.u64()?,
                version: r.u64()?,
            },
            Kind::LanesAcquired => Frame::LanesAcquired {
                lanes: r.u64()?,
                arena_off: r.u64()?,
            },
            Kind::Block => {
                let prefix = r.u64()?;
                let n = r.u32()? as usize;
                if n > MAX_LANES {
                    return Err(IpcError::TooLarge);
                }
                let mut lanes = Vec::with_capacity(n);
                for _ in 0..n {
                    let kind = LaneKind::from_raw(r.u8()?)?;
                    let count = r.u32()?;
                    // A count whose payload would not fit its arena slot is a
                    // disagreement about the encoding, not a large chunk: the bound
                    // is exact, so exceeding it means one side is wrong about what a
                    // container can hold.
                    if kind.payload_bytes(count) > LANE_BYTES {
                        return Err(IpcError::Malformed);
                    }
                    lanes.push(Lane { kind, count });
                }
                Frame::Block { prefix, lanes }
            }
            Kind::BlockAdvanceMany => Frame::BlockAdvanceMany {
                lanes: r.u64()?,
                max_blocks: r.u32()?,
            },
            Kind::Blocks => {
                let n = r.u32()? as usize;
                if n > MAX_BATCH {
                    return Err(IpcError::TooLarge);
                }
                let mut blocks = Vec::with_capacity(n);
                for _ in 0..n {
                    let prefix = r.u64()?;
                    let m = r.u32()? as usize;
                    if m > MAX_LANES {
                        return Err(IpcError::TooLarge);
                    }
                    let mut lanes = Vec::with_capacity(m);
                    for _ in 0..m {
                        let kind = LaneKind::from_raw(r.u8()?)?;
                        let count = r.u32()?;
                        if kind.payload_bytes(count) > LANE_BYTES {
                            return Err(IpcError::Malformed);
                        }
                        lanes.push(Lane { kind, count });
                    }
                    blocks.push(Block { prefix, lanes });
                }
                Frame::Blocks { blocks }
            }
            Kind::BlocksInline => {
                let n = r.u32()? as usize;
                if n > MAX_BATCH {
                    return Err(IpcError::TooLarge);
                }
                let mut blocks = Vec::with_capacity(n);
                let mut want = 0usize;
                for _ in 0..n {
                    let prefix = r.u64()?;
                    let m = r.u32()? as usize;
                    if m > MAX_LANES {
                        return Err(IpcError::TooLarge);
                    }
                    let mut lanes = Vec::with_capacity(m);
                    for _ in 0..m {
                        let kind = LaneKind::from_raw(r.u8()?)?;
                        let count = r.u32()?;
                        if kind.payload_bytes(count) > LANE_BYTES {
                            return Err(IpcError::Malformed);
                        }
                        want += kind.payload_bytes(count);
                        lanes.push(Lane { kind, count });
                    }
                    blocks.push(Block { prefix, lanes });
                }
                let len = r.u32()? as usize;
                if len > MAX_INLINE_PAYLOAD {
                    return Err(IpcError::TooLarge);
                }
                // The descriptors say exactly how many payload bytes there must be.
                // A frame that disagrees with itself is refused rather than trusted
                // in one direction over the other.
                if len != want {
                    return Err(IpcError::Malformed);
                }
                Frame::BlocksInline {
                    blocks,
                    payload: r.take(len)?.to_vec(),
                }
            }
            Kind::SnapshotCardinality => Frame::SnapshotCardinality {
                snapshot: r.u64()?,
                key: r.u64()?,
            },
            Kind::SnapshotMax => Frame::SnapshotMax {
                snapshot: r.u64()?,
                key: r.u64()?,
            },
            Kind::SnapshotContains => Frame::SnapshotContains {
                snapshot: r.u64()?,
                key: r.u64()?,
                ordinal: r.u64()?,
            },
            Kind::SnapshotLoad => Frame::SnapshotLoad {
                snapshot: r.u64()?,
                key: r.u64()?,
                after: r.u64()?,
                has_after: r.u8()?,
                limit: r.u32()?,
            },
            Kind::SnapshotKeyRange => Frame::SnapshotKeyRange {
                snapshot: r.u64()?,
                lo: r.u64()?,
                hi: r.u64()?,
                limit: r.u32()?,
            },
            Kind::Count => Frame::Count { value: r.u64()? },
            Kind::Bool => Frame::Bool { value: r.u8()? },
            Kind::Ordinal => Frame::Ordinal {
                present: r.u8()?,
                value: r.u64()?,
            },
            Kind::Ordinals | Kind::Keys => {
                let n = r.u32()? as usize;
                if n > MAX_PAGE {
                    return Err(IpcError::TooLarge);
                }
                let mut values = Vec::with_capacity(n);
                for _ in 0..n {
                    values.push(r.u64()?);
                }
                let more = r.u8()?;
                if matches!(kind, Kind::Ordinals) {
                    Frame::Ordinals { values, more }
                } else {
                    Frame::Keys { values, more }
                }
            }
            Kind::BlockDone => Frame::BlockDone,
            Kind::Done => Frame::Done,
            Kind::Fault => Frame::Fault {
                status: r.u32()?,
                message: r.string()?,
            },
            Kind::Unavailable => Frame::Unavailable,
            Kind::Available => Frame::Available {
                generation: r.u64()?,
            },
            Kind::GenerationChanged => Frame::GenerationChanged {
                old: r.u64()?,
                new: r.u64()?,
            },
            Kind::RoleChanged => Frame::RoleChanged {
                from: Role::from_raw(r.u8()?)?,
                to: Role::from_raw(r.u8()?)?,
            },
        };
        // Trailing bytes are refused rather than ignored. A payload longer than its
        // kind needs means the two sides disagree about that kind's shape, and
        // accepting the prefix lets the disagreement persist until it matters.
        if !r.done() {
            return Err(IpcError::Malformed);
        }
        Ok((frame, end))
    }
}

/// Where lane `i` of a handle based at `arena_off` begins.
///
/// One function rather than the arithmetic written at each call site, because the
/// two sides getting it separately right is the failure this layout exists to
/// prevent.
pub fn lane_offset(arena_off: u64, lane: usize) -> u64 {
    arena_off + (lane as u64) * (LANE_BYTES as u64)
}

fn put_string(out: &mut Vec<u8>, s: &str) -> Result<(), IpcError> {
    if s.len() > MAX_STRING {
        return Err(IpcError::TooLarge);
    }
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
    Ok(())
}

/// A cursor that answers `Err` rather than panicking on a short payload.
struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], IpcError> {
        let end = self.at.checked_add(n).ok_or(IpcError::Malformed)?;
        if end > self.b.len() {
            return Err(IpcError::Malformed);
        }
        let s = &self.b[self.at..end];
        self.at = end;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, IpcError> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32, IpcError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn u64(&mut self) -> Result<u64, IpcError> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }
    fn string(&mut self) -> Result<String, IpcError> {
        let n = self.u32()? as usize;
        if n > MAX_STRING {
            return Err(IpcError::TooLarge);
        }
        let b = self.take(n)?;
        core::str::from_utf8(b)
            .map(|s| s.to_string())
            .map_err(|_| IpcError::Malformed)
    }
    fn done(&self) -> bool {
        self.at == self.b.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every frame, so no variant's encoder and decoder can disagree unnoticed.
    fn every_frame() -> Vec<Frame> {
        vec![
            Frame::ClientHello {
                protocol: 1,
                name: "haiiie".into(),
            },
            Frame::SnapshotOpen,
            // Every op, because the decoder maps each discriminant by hand, and both
            // bounds of the batch: empty is legal and the loop must not assume otherwise.
            Frame::Apply { writes: vec![] },
            // Both bounds here too, and all three kinds: the decoder derives each
            // payload's length from its kind and count, so a kind it mapped wrongly
            // would consume the wrong number of bytes and desynchronise the frame
            // rather than fail a field.
            Frame::ChunkPut { writes: vec![] },
            Frame::ChunkPut {
                writes: vec![
                    ChunkWrite {
                        key: 5,
                        prefix: 9,
                        kind: LaneKind::Array,
                        count: 3,
                        card: 3,
                        payload: vec![7, 0, 9, 0, 11, 0],
                    },
                    ChunkWrite {
                        key: 6,
                        prefix: 1,
                        kind: LaneKind::Run,
                        count: 2,
                        card: 11,
                        payload: vec![1, 0, 5, 0, 20, 0, 25, 0],
                    },
                    ChunkWrite {
                        key: 7,
                        prefix: 0,
                        kind: LaneKind::Bitmap,
                        count: BITMAP_WORDS as u32,
                        card: 64,
                        payload: {
                            let mut v = vec![0u8; BITMAP_WORDS * 8];
                            v[0] = 0xff;
                            v[1] = 0xff;
                            v[2] = 0xff;
                            v[3] = 0xff;
                            v[4] = 0xff;
                            v[5] = 0xff;
                            v[6] = 0xff;
                            v[7] = 0xff;
                            v
                        },
                    },
                ],
            },
            Frame::Apply {
                writes: vec![
                    Write {
                        key: 10,
                        lo: 5,
                        hi: 5,
                        op: WriteOp::Insert,
                    },
                    Write {
                        key: 10,
                        lo: 6,
                        hi: 6,
                        op: WriteOp::Remove,
                    },
                    Write {
                        key: 11,
                        lo: 100,
                        hi: 200,
                        op: WriteOp::InsertRange,
                    },
                    Write {
                        key: 11,
                        lo: 150,
                        hi: 160,
                        op: WriteOp::RemoveRange,
                    },
                    Write {
                        key: 12,
                        lo: 0,
                        hi: 0,
                        op: WriteOp::DeleteKey,
                    },
                ],
            },
            Frame::Committed {
                version: 42,
                changed: 7,
            },
            Frame::SnapshotClose { snapshot: 7 },
            Frame::LanesAcquire {
                snapshot: 7,
                keys: vec![10, 20, 30],
            },
            Frame::LanesAcquire {
                snapshot: 7,
                keys: vec![],
            },
            Frame::LanesRelease { lanes: 3 },
            Frame::BlockAdvance { lanes: 3 },
            Frame::BlockRelease { lanes: 3 },
            Frame::SnapshotCardinality {
                snapshot: 7,
                key: 10,
            },
            Frame::SnapshotContains {
                snapshot: 7,
                key: 10,
                ordinal: 99,
            },
            Frame::SnapshotMax {
                snapshot: 7,
                key: 10,
            },
            Frame::SnapshotLoad {
                snapshot: 7,
                key: 10,
                after: 0,
                has_after: 0,
                limit: 100,
            },
            Frame::SnapshotLoad {
                snapshot: 7,
                key: 10,
                after: 1234,
                has_after: 1,
                limit: MAX_PAGE as u32,
            },
            Frame::SnapshotKeyRange {
                snapshot: 7,
                lo: 0,
                hi: u64::MAX,
                limit: 64,
            },
            Frame::Count { value: 5000 },
            Frame::Bool { value: 1 },
            Frame::Ordinal {
                present: 0,
                value: 0,
            },
            Frame::Ordinals {
                values: vec![],
                more: 0,
            },
            Frame::Ordinals {
                values: vec![1, 2, 3],
                more: 1,
            },
            Frame::Keys {
                values: vec![10, 20, 30],
                more: 0,
            },
            Frame::BlockAdvanceMany {
                lanes: 3,
                max_blocks: 16,
            },
            Frame::Blocks { blocks: vec![] },
            Frame::BlocksInline {
                blocks: vec![],
                payload: vec![],
            },
            Frame::BlocksInline {
                blocks: vec![Block {
                    prefix: 9,
                    lanes: vec![
                        Lane {
                            kind: LaneKind::Array,
                            count: 3,
                        },
                        Lane {
                            kind: LaneKind::Absent,
                            count: 0,
                        },
                    ],
                }],
                payload: vec![1, 0, 2, 0, 3, 0],
            },
            Frame::Blocks {
                blocks: vec![
                    Block {
                        prefix: 0,
                        lanes: vec![Lane {
                            kind: LaneKind::Array,
                            count: 3,
                        }],
                    },
                    Block {
                        prefix: 1,
                        lanes: vec![
                            Lane {
                                kind: LaneKind::Bitmap,
                                count: 1024,
                            },
                            Lane {
                                kind: LaneKind::Absent,
                                count: 0,
                            },
                        ],
                    },
                ],
            },
            Frame::ServerHello {
                protocol: 1,
                generation: 4,
                role: Role::Follower,
                shards: 32,
                arena_bytes: 2 * 1024 * 1024,
                max_lanes: 256,
                max_handles: 8,
                max_blocks: 16,
                max_writes: MAX_WRITES as u32,
            },
            Frame::SnapshotOpened {
                snapshot: 7,
                version: 99,
            },
            Frame::LanesAcquired {
                lanes: 3,
                arena_off: 16384,
            },
            Frame::Block {
                prefix: 5,
                lanes: vec![
                    Lane {
                        kind: LaneKind::Array,
                        count: 3,
                    },
                    Lane {
                        kind: LaneKind::Bitmap,
                        count: 1024,
                    },
                    Lane {
                        kind: LaneKind::Run,
                        count: 1,
                    },
                    Lane {
                        kind: LaneKind::Absent,
                        count: 0,
                    },
                ],
            },
            Frame::BlockDone,
            Frame::Done,
            Frame::Fault {
                status: 3,
                message: "no database".into(),
            },
            Frame::Unavailable,
            Frame::Available { generation: 5 },
            Frame::GenerationChanged { old: 4, new: 5 },
            Frame::RoleChanged {
                from: Role::Leader,
                to: Role::Follower,
            },
        ]
    }

    #[test]
    fn every_frame_round_trips_and_consumes_exactly_its_bytes() {
        for f in every_frame() {
            let bytes = f.encode().unwrap();
            let (back, used) = Frame::decode(&bytes).unwrap();
            assert_eq!(back, f, "round trip changed the frame");
            assert_eq!(used, bytes.len(), "decode must consume the whole frame");
            assert_eq!(back.kind(), f.kind());
        }
    }

    /// Two frames back to back decode independently, which is what makes the
    /// `( frame, used )` return usable on a stream.
    #[test]
    fn frames_decode_one_at_a_time_from_a_concatenated_buffer() {
        let mut buf = Vec::new();
        for f in every_frame() {
            f.encode_to(&mut buf).unwrap();
        }
        let mut at = 0;
        let mut seen = Vec::new();
        while at < buf.len() {
            let (f, used) = Frame::decode(&buf[at..]).unwrap();
            at += used;
            seen.push(f);
        }
        assert_eq!(seen, every_frame());
    }

    /// Every prefix of every frame is `Truncated`, never a panic and never a
    /// half-decoded frame. This is the property a stream reader depends on.
    #[test]
    fn every_short_prefix_asks_for_more_rather_than_failing() {
        for f in every_frame() {
            let bytes = f.encode().unwrap();
            for cut in 0..bytes.len() {
                match Frame::decode(&bytes[..cut]) {
                    Err(IpcError::Truncated) => {}
                    other => panic!(
                        "a {:?}-byte prefix of {:?} answered {other:?}, not Truncated",
                        cut,
                        f.kind()
                    ),
                }
            }
        }
    }

    /// The direction ranges hold, so a misrouted frame is caught by its kind.
    /// What the dense encoding costs on the wire, against the point encoding.
    ///
    /// The comparison is one frame against one frame, at exactly [`MAX_WRITES`] ordinals,
    /// because that is the largest batch the point wire can carry atomically and so the
    /// most favourable case it has. A dense half-chunk is 16 384 ordinals, which is
    /// `4 + 16384 * 25` bytes as points and `4 + 25 + 8192` as a bitmap payload.
    ///
    /// Asserted as a **bound rather than a ratio**: the exact figure moves if a header
    /// field is ever added, and a test that fails on a harmless field is a test people
    /// delete. The claim worth keeping is the order of magnitude. Measured at 49.8x when
    /// written, against the 64.8x the Flight path measured for the same question on a
    /// different transport -- the difference is the transports' per-entry overheads, not
    /// the encodings.
    #[test]
    fn the_dense_encoding_is_an_order_of_magnitude_smaller_on_the_wire() {
        let ordinals = MAX_WRITES as u64;
        let points = Frame::Apply {
            writes: (0..ordinals)
                .map(|o| Write {
                    key: 1,
                    lo: o,
                    hi: o,
                    op: WriteOp::Insert,
                })
                .collect(),
        };
        let dense = Frame::ChunkPut {
            writes: vec![ChunkWrite {
                key: 1,
                prefix: 0,
                kind: LaneKind::Bitmap,
                count: BITMAP_WORDS as u32,
                card: ordinals as u32,
                payload: vec![0u8; BITMAP_WORDS * 8],
            }],
        };
        let point_bytes = points
            .encode()
            .expect("the point batch is at its limit")
            .len();
        let dense_bytes = dense
            .encode()
            .expect("one chunk is well under the cap")
            .len();
        assert!(
            point_bytes >= 40 * dense_bytes,
            "the dense encoding should be far smaller: {point_bytes} against {dense_bytes}"
        );
        // And the dense frame carrying the measured 64-chunk page still fits one frame,
        // which is what makes it one commit rather than 128.
        assert!(chunk_put_payload_bytes(MAX_CHUNK_WRITES) <= Kind::ChunkPut.payload_cap());
    }

    #[test]
    fn each_kind_declares_the_direction_its_range_implies() {
        for f in every_frame() {
            let k = f.kind();
            let expected = match k {
                Kind::ClientHello
                | Kind::SnapshotOpen
                | Kind::SnapshotClose
                | Kind::LanesAcquire
                | Kind::LanesRelease
                | Kind::BlockAdvance
                | Kind::BlockRelease
                | Kind::BlockAdvanceMany
                | Kind::SnapshotCardinality
                | Kind::SnapshotContains
                | Kind::SnapshotMax
                | Kind::SnapshotLoad
                | Kind::SnapshotKeyRange
                | Kind::Apply
                | Kind::ChunkPut => Direction::Request,
                Kind::ServerHello
                | Kind::SnapshotOpened
                | Kind::LanesAcquired
                | Kind::Block
                | Kind::BlockDone
                | Kind::Done
                | Kind::Fault
                | Kind::Blocks
                | Kind::BlocksInline
                | Kind::Count
                | Kind::Bool
                | Kind::Ordinal
                | Kind::Ordinals
                | Kind::Keys
                | Kind::Committed => Direction::Response,
                Kind::Unavailable
                | Kind::Available
                | Kind::GenerationChanged
                | Kind::RoleChanged => Direction::Notification,
            };
            assert_eq!(k.direction(), expected, "{k:?} is in the wrong range");
        }
    }

    /// The arena bound is exact, so the largest legal payload of each kind fits and
    /// one element more does not.
    #[test]
    fn the_lane_bound_is_exact_for_every_encoding() {
        assert_eq!(LaneKind::Array.payload_bytes(4096), LANE_BYTES);
        assert_eq!(LaneKind::Bitmap.payload_bytes(1024), LANE_BYTES);
        assert_eq!(LaneKind::Run.payload_bytes(2032), 8128);
        assert_eq!(LaneKind::Absent.payload_bytes(0), 0);
        assert!(LaneKind::Array.payload_bytes(4097) > LANE_BYTES);
        assert!(LaneKind::Bitmap.payload_bytes(1025) > LANE_BYTES);
    }

    /// A lane count that would not fit its arena slot is refused, because the bound
    /// is exact: exceeding it means the two sides disagree about the encoding.
    #[test]
    fn a_lane_that_would_overflow_its_slot_is_refused() {
        let f = Frame::Block {
            prefix: 0,
            lanes: vec![Lane {
                kind: LaneKind::Array,
                count: 5000,
            }],
        };
        let bytes = f.encode().expect("the encoder does not police this field");
        assert_eq!(
            Frame::decode(&bytes),
            Err(IpcError::Malformed),
            "the decoder must refuse a count whose payload cannot fit LANE_BYTES"
        );
    }

    #[test]
    fn lane_offsets_are_a_function_of_the_index() {
        assert_eq!(lane_offset(0, 0), 0);
        assert_eq!(lane_offset(0, 1), LANE_BYTES as u64);
        assert_eq!(lane_offset(16384, 2), 16384 + 2 * LANE_BYTES as u64);
    }

    #[test]
    fn a_foreign_magic_is_not_a_frame() {
        let mut bytes = Frame::Unavailable.encode().unwrap();
        // `yesno-wire`'s set-expression magic, written literally rather than
        // imported: this module has no dependencies, and re-adding one to assert a
        // four-byte constant would undo the reason it lives here.
        bytes[..4].copy_from_slice(b"YSNX");
        assert_eq!(Frame::decode(&bytes), Err(IpcError::NotAFrame));
        // And a short buffer that already disagrees is refused rather than awaited.
        assert_eq!(Frame::decode(&bytes[..5]), Err(IpcError::NotAFrame));
    }

    #[test]
    fn a_future_version_is_refused_by_number() {
        let mut bytes = Frame::Unavailable.encode().unwrap();
        bytes[4] = VERSION + 1;
        assert_eq!(Frame::decode(&bytes), Err(IpcError::Version(VERSION + 1)));
    }

    /// A non-zero reserved or flags byte is refused, so those fields stay free for a
    /// later version to define rather than being silently assumed.
    #[test]
    fn a_non_zero_reserved_field_is_refused() {
        for at in [5usize, 7] {
            let mut bytes = Frame::Unavailable.encode().unwrap();
            bytes[at] = 1;
            assert_eq!(
                Frame::decode(&bytes),
                Err(IpcError::Reserved),
                "byte {at} must be reserved"
            );
        }
    }

    #[test]
    fn an_unknown_kind_names_itself() {
        let mut bytes = Frame::Unavailable.encode().unwrap();
        bytes[6] = 200;
        assert_eq!(Frame::decode(&bytes), Err(IpcError::UnknownKind(200)));
    }

    /// A declared length beyond the cap is refused before anything is allocated,
    /// which is the whole allocation-attack surface of a format with no recursion.
    #[test]
    fn an_enormous_declared_length_is_refused_before_allocating() {
        let mut bytes = Frame::Unavailable.encode().unwrap();
        bytes[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(Frame::decode(&bytes), Err(IpcError::TooLarge));
    }

    /// A key count beyond MAX_LANES is refused even though the frame is otherwise
    /// well formed, so a peer cannot size the server's arena by arithmetic.
    #[test]
    fn a_lane_count_beyond_the_cap_is_refused_in_both_directions() {
        let too_many: Vec<u64> = (0..(MAX_LANES as u64 + 1)).collect();
        assert_eq!(
            Frame::LanesAcquire {
                snapshot: 1,
                keys: too_many.clone(),
            }
            .encode(),
            Err(IpcError::TooLarge),
            "the encoder refuses it at the point the value was supplied"
        );

        // And a hand-built frame claiming as many is refused by the decoder.
        let mut p = Vec::new();
        p.extend_from_slice(&1u64.to_le_bytes());
        p.extend_from_slice(&((MAX_LANES + 1) as u32).to_le_bytes());
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&[VERSION, 0, Kind::LanesAcquire as u8, 0]);
        bytes.extend_from_slice(&(p.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&p);
        assert_eq!(Frame::decode(&bytes), Err(IpcError::TooLarge));
    }

    /// A string that is not UTF-8 is refused rather than replaced.
    #[test]
    fn invalid_utf8_in_a_string_is_refused() {
        let mut p = Vec::new();
        p.extend_from_slice(&1u32.to_le_bytes()); // protocol
        p.extend_from_slice(&2u32.to_le_bytes()); // name length
        p.extend_from_slice(&[0xff, 0xfe]);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&[VERSION, 0, Kind::ClientHello as u8, 0]);
        bytes.extend_from_slice(&(p.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&p);
        assert_eq!(Frame::decode(&bytes), Err(IpcError::Malformed));
    }

    /// Trailing bytes are refused, because a payload longer than its kind needs means
    /// the two sides disagree about that kind's shape.
    #[test]
    fn a_payload_longer_than_its_kind_is_refused() {
        let mut bytes = Frame::Available { generation: 5 }.encode().unwrap();
        bytes.push(0);
        let len = (bytes.len() - HEADER_LEN) as u32;
        bytes[8..12].copy_from_slice(&len.to_le_bytes());
        assert_eq!(Frame::decode(&bytes), Err(IpcError::Malformed));
    }

    /// Arbitrary bytes never panic: the contract this crate holds for untrusted
    /// input, exercised over every kind byte and a spread of lengths rather than
    /// trusted to the cases above.
    #[test]
    fn arbitrary_bytes_answer_err_and_never_panic() {
        let mut state = 0x243f_6a88_85a3_08d3u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for kind in 0u16..=255 {
            for len in [0usize, 1, 4, 11, 12, 13, 40, 200] {
                let mut bytes = Vec::with_capacity(HEADER_LEN + len);
                bytes.extend_from_slice(MAGIC);
                bytes.extend_from_slice(&[VERSION, 0, kind as u8, 0]);
                bytes.extend_from_slice(&(len as u32).to_le_bytes());
                for _ in 0..len {
                    bytes.push(next() as u8);
                }
                // Must not panic. Either answer is acceptable; a crash is not.
                let _ = Frame::decode(&bytes);
            }
        }
        // And wholly random buffers, magic included.
        for _ in 0..2000 {
            let n = (next() % 64) as usize;
            let buf: Vec<u8> = (0..n).map(|_| next() as u8).collect();
            let _ = Frame::decode(&buf);
        }
    }
}

#[cfg(test)]
mod batch_tests {
    use super::*;

    /// A batch beyond the cap is refused at the point the value was supplied, and
    /// again on decode, so a peer cannot size the server's arena by arithmetic.
    #[test]
    fn a_batch_beyond_the_cap_is_refused_in_both_directions() {
        let blocks: Vec<Block> = (0..MAX_BATCH as u64 + 1)
            .map(|p| Block {
                prefix: p,
                lanes: vec![],
            })
            .collect();
        assert_eq!(
            Frame::Blocks { blocks }.encode(),
            Err(IpcError::TooLarge),
            "the encoder refuses it where the value was supplied"
        );

        let mut p = Vec::new();
        p.extend_from_slice(&((MAX_BATCH + 1) as u32).to_le_bytes());
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&[VERSION, 0, Kind::Blocks as u8, 0]);
        bytes.extend_from_slice(&(p.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&p);
        assert_eq!(Frame::decode(&bytes), Err(IpcError::TooLarge));
    }

    /// The batched offset reduces to the single-block one at block zero, so the two
    /// layouts cannot drift apart.
    #[test]
    fn the_batched_offset_agrees_with_the_single_block_one_at_block_zero() {
        for lanes in [1usize, 4, 64] {
            for lane in [0usize, 1, 3] {
                assert_eq!(
                    batched_lane_offset(0, lanes, 0, lane),
                    lane_offset(0, lane),
                    "block zero of a batch is the unbatched layout"
                );
            }
        }
        // And each block is a whole lane-block further on.
        assert_eq!(
            batched_lane_offset(0, 4, 1, 0),
            4 * LANE_BYTES as u64,
            "block 1 begins after block 0's four lanes"
        );
        assert_eq!(
            batched_lane_offset(1024, 4, 2, 3),
            1024 + (2 * 4 + 3) * LANE_BYTES as u64
        );
    }
}

#[cfg(test)]
mod inline_tests {
    use super::*;

    /// An inline frame whose payload length disagrees with its own descriptors is
    /// refused, in both directions.
    ///
    /// The descriptors say exactly how many bytes must follow, so a mismatch means
    /// the two halves of one frame disagree. Trusting either over the other would
    /// hand a peer a lane whose bytes belong to a different lane.
    #[test]
    fn an_inline_payload_must_match_its_descriptors() {
        let blocks = vec![Block {
            prefix: 0,
            lanes: vec![Lane {
                kind: LaneKind::Array,
                count: 3,
            }],
        }];
        // Three u16 values is six bytes; five is a disagreement.
        assert_eq!(
            Frame::BlocksInline {
                blocks: blocks.clone(),
                payload: vec![0; 5],
            }
            .encode(),
            Err(IpcError::Malformed),
            "the encoder refuses it where the value was supplied"
        );
        // Six is right, and round trips.
        let good = Frame::BlocksInline {
            blocks,
            payload: vec![9; 6],
        };
        let bytes = good.encode().unwrap();
        assert_eq!(Frame::decode(&bytes).unwrap().0, good);
    }

    /// The inline cap is separate from the control cap, so raising one never widens
    /// what the other may demand.
    #[test]
    fn the_two_payload_caps_are_independent() {
        // A control frame is still bounded by the smaller cap.
        let mut bytes = Frame::Unavailable.encode().unwrap();
        bytes[8..12].copy_from_slice(&((MAX_PAYLOAD + 1) as u32).to_le_bytes());
        assert_eq!(Frame::decode(&bytes), Err(IpcError::TooLarge));
    }

    /// Inline mode caps the batch by frame size, which is the coupling the arena
    /// exists to break. Asserted as arithmetic so the trade-off is checkable rather
    /// than only described.
    #[test]
    fn inline_mode_caps_the_batch_by_frame_size() {
        let one_block = |lanes: usize| lanes * LANE_BYTES;
        assert!(
            one_block(64) <= MAX_INLINE_PAYLOAD,
            "a single 64-lane block must at least fit, or inline is unusable there"
        );
        let max_batch_at_64 = MAX_INLINE_PAYLOAD / one_block(64);
        assert_eq!(
            max_batch_at_64, 2,
            "at 64 lanes an inline batch is two blocks, against the arena's {MAX_BATCH}"
        );
        // With the arena there is no frame-size limit on the batch at all: the
        // descriptors for a full batch are small.
        let descriptors = MAX_BATCH * 64 * 5;
        assert!(
            descriptors < MAX_PAYLOAD,
            "a full batch of descriptors fits the control cap with room to spare"
        );
    }
}

#[cfg(test)]
mod page_tests {
    use super::*;

    /// A page beyond the cap is refused where the value was supplied, and on decode.
    /// The widest legal `Apply` encodes and decodes, and one more does not.
    ///
    /// # This is the test that was missing
    ///
    /// `MAX_WRITES` shipped as 16 384 while `Apply` was capped at `MAX_PAYLOAD`, so the
    /// real boundary was 2 621 and a frame at the advertised limit failed in `encode`.
    /// Nothing caught it because no test ever built a frame at the limit -- the codec table
    /// carries a five-entry `Apply`, which proves the encoding and says nothing about the
    /// cap. A consumer found it by trying to use the number.
    ///
    /// So the assertion is specifically at `MAX_WRITES` and `MAX_WRITES + 1`, against the
    /// *derived* cap, which is what makes the two impossible to disagree: if a later change
    /// moves either, this fails rather than the advertised number quietly becoming a lie
    /// again.
    #[test]
    fn the_widest_legal_apply_round_trips_and_one_more_is_refused() {
        let one = Write {
            key: 1,
            lo: 2,
            hi: 2,
            op: WriteOp::Insert,
        };

        let widest = Frame::Apply {
            writes: vec![one; MAX_WRITES],
        };
        let bytes = widest
            .encode()
            .expect("a frame at the advertised limit must encode");
        assert_eq!(
            bytes.len(),
            HEADER_LEN + apply_payload_bytes(MAX_WRITES),
            "the payload is the derived size"
        );
        let (back, used) = Frame::decode(&bytes).expect("and must decode");
        assert_eq!(used, bytes.len());
        assert_eq!(
            back, widest,
            "the widest frame does not survive a round trip"
        );

        // One over fails on both sides, and on the count rather than incidentally on bytes.
        let over = Frame::Apply {
            writes: vec![one; MAX_WRITES + 1],
        };
        assert_eq!(over.encode(), Err(IpcError::TooLarge));

        // Decode-side: a header declaring one entry too many, which an encoder would never
        // produce but a hostile or mismatched peer can send.
        let mut forged = bytes.clone();
        let n = (MAX_WRITES + 1) as u32;
        let payload = apply_payload_bytes(MAX_WRITES + 1) as u32;
        forged[8..12].copy_from_slice(&payload.to_le_bytes());
        forged[HEADER_LEN..HEADER_LEN + 4].copy_from_slice(&n.to_le_bytes());
        forged.resize(HEADER_LEN + payload as usize, 0);
        assert_eq!(
            Frame::decode(&forged),
            Err(IpcError::TooLarge),
            "an over-cap declaration must be refused, not buffered"
        );
    }

    /// `Apply` has its own cap, and the three classes do not leak into each other.
    ///
    /// The two-cap comment asked that raising the inline limit "never widen what a control
    /// frame may demand". With `Apply` added that has to hold in every direction, which is
    /// only checkable by comparing the caps a kind reports rather than by reading the
    /// `match`.
    #[test]
    fn each_kind_gets_the_cap_its_class_implies() {
        assert_eq!(Kind::Apply.payload_cap(), APPLY_MAX_PAYLOAD);
        assert_eq!(
            Kind::BlocksInline.payload_cap(),
            MAX_INLINE_PAYLOAD + MAX_PAYLOAD
        );
        for k in [
            Kind::ClientHello,
            Kind::SnapshotOpen,
            Kind::SnapshotLoad,
            Kind::ServerHello,
            Kind::Blocks,
            Kind::Keys,
            Kind::Committed,
            Kind::RoleChanged,
        ] {
            assert_eq!(
                k.payload_cap(),
                MAX_PAYLOAD,
                "{k:?} is a descriptor frame and must stay under the small cap"
            );
        }
        // And the one that was wrong: a bulk kind must be able to exceed the small cap, or
        // its advertised entry limit is unreachable.
        assert!(
            Kind::Apply.payload_cap() > MAX_PAYLOAD,
            "Apply is bulk; capping it like a descriptor is the defect this test exists for"
        );
    }

    /// The response to a write is fixed-size, whatever the request carried.
    ///
    /// The handoff asked for request and response limits to be symmetric. They are not
    /// equal and should not be: `Committed` is sixteen bytes, so a peer sending the widest
    /// legal `Apply` cannot provoke a large reply. Asserted because "symmetric" could
    /// otherwise be read as "the reply needs the same cap", which would be the wrong fix.
    #[test]
    fn a_write_reply_is_fixed_size_however_wide_the_request() {
        let reply = Frame::Committed {
            version: u64::MAX,
            changed: u64::MAX,
        };
        let bytes = reply.encode().unwrap();
        assert_eq!(bytes.len(), HEADER_LEN + 16);
        assert!(bytes.len() < MAX_PAYLOAD);
    }

    #[test]
    fn a_page_beyond_the_cap_is_refused_in_both_directions() {
        let values: Vec<u64> = (0..MAX_PAGE as u64 + 1).collect();
        for f in [
            Frame::Ordinals {
                values: values.clone(),
                more: 0,
            },
            Frame::Keys { values, more: 0 },
        ] {
            let kind = f.kind();
            assert_eq!(f.encode(), Err(IpcError::TooLarge), "{kind:?}");
        }

        let mut p = Vec::new();
        p.extend_from_slice(&((MAX_PAGE + 1) as u32).to_le_bytes());
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&[VERSION, 0, Kind::Ordinals as u8, 0]);
        bytes.extend_from_slice(&(p.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&p);
        assert_eq!(Frame::decode(&bytes), Err(IpcError::TooLarge));
    }

    /// A full page of `u64`s fits the control cap with room to spare, which is why
    /// `MAX_PAGE` is half of it rather than as large as it could be.
    #[test]
    fn a_full_page_fits_the_control_cap() {
        let fits = MAX_PAGE * 8 + 16;
        assert!(
            fits < MAX_PAYLOAD,
            "a full page plus its frame must fit MAX_PAYLOAD"
        );
        let f = Frame::Ordinals {
            values: (0..MAX_PAGE as u64).collect(),
            more: 1,
        };
        let bytes = f.encode().expect("a full page must be encodable");
        assert_eq!(Frame::decode(&bytes).unwrap().0, f);
    }
}
