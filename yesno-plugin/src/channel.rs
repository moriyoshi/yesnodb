//! The server side of the out-of-process plugin channel.
//!
//! yesnod stays the only process that opens the database. A peer connects over a
//! Unix socket, is handed a shared arena as a file descriptor, and asks for data.
//! The protocol is [`crate::ipc`]; the reasoning behind its shape is in
//! that module's header.
//!
//! # Protocol logic is separated from the socket, deliberately
//!
//! [`Session::handle`] takes a frame and answers a frame. It never reads or writes
//! the socket, so every behaviour worth testing -- an unknown handle, a block
//! advanced twice, a snapshot closed while lanes derived from it live -- is a
//! function call in a test rather than a conversation through a file descriptor.
//! [`serve_blocking`] is the thin loop that owns the socket, and there is almost
//! nothing in it to get wrong.
//!
//! # The arena is sparse, which is what makes fixed slots affordable
//!
//! Each handle reserves `max_lanes * LANE_BYTES` regardless of how many lanes it
//! asked for, so its offset is `index * slot` and there is no allocator. That looks
//! wasteful -- a three-lane handle reserving two megabytes -- and is not: the arena
//! is a `memfd`, so a page costs nothing until it is written. The reservation is
//! address space, and the resident cost is the lanes actually used.
//!
//! # Liveness is the connection
//!
//! A `Session` owns the snapshots it opened. Dropping it drops them, and the socket
//! closing is what drops it -- peer exit, crash, container stop, `SIGKILL`. So
//! reclamation needs no cooperation from the peer and no pid, which is the property
//! the in-process ABI cannot have, where only `ReaderSlot::drop` frees a lease.

use std::collections::HashMap;

use crate::ipc::{
    Block as WireBlock, Frame, Kind, Lane, LaneKind, Role as WireRole, LANE_BYTES,
    MAX_INLINE_PAYLOAD, MAX_LANES, MAX_PAGE,
};
use yesno_core::{Container, KeyLanes, Snapshot};

use crate::abi::{Role, Status};
use crate::Host;

/// How many concurrent lane handles one session may hold, and how wide each may be.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_handles: usize,
    pub max_lanes: usize,
    /// Blocks one response may carry, and therefore sub-slots per handle.
    ///
    /// The whole point of a batch is fewer notifications: measurement put the
    /// per-block cost almost entirely in round trips rather than in the copy, so
    /// this is the factor that moves it. One is the unbatched behaviour.
    pub max_blocks: usize,
    /// Snapshots one session may hold open at once.
    ///
    /// **Unlike the three above, this one does not bound the arena.** Every
    /// snapshot claims a slot in the process-wide reader registry -- 4096 for the
    /// whole database -- and pins the reclamation floor, so an unbounded session
    /// does not merely cost itself: it exhausts a table the server's own queries
    /// draw from and stops space being reclaimed while it holds on. A peer needs
    /// one per query in flight.
    pub max_snapshots: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_handles: 4,
            max_lanes: MAX_LANES,
            max_blocks: 1,
            max_snapshots: 64,
        }
    }
}

impl Limits {
    /// Bytes one block of one handle reserves.
    fn block_bytes(&self) -> usize {
        self.max_lanes * LANE_BYTES
    }

    /// Bytes one handle reserves, across its whole batch.
    fn slot_bytes(&self) -> usize {
        self.max_blocks.max(1) * self.block_bytes()
    }

    /// Bytes the whole arena spans.
    pub fn arena_bytes(&self) -> usize {
        self.max_handles * self.slot_bytes()
    }
}

/// The shared region, and the descriptor a peer maps.
pub struct Arena {
    map: memmap2::MmapMut,
    /// Kept so it can be sent over the socket. Closed with the arena.
    fd: std::os::fd::OwnedFd,
}

impl Arena {
    /// Create an anonymous shared region of `bytes`, sized once and sealed.
    ///
    /// # Why a `memfd` and not POSIX or System V shared memory
    ///
    /// All three would work on one host. The differences decide it:
    ///
    /// - **It crosses a container boundary.** A `memfd` travels as a descriptor over
    ///   the control socket, so it needs no shared filesystem and no shared
    ///   namespace. POSIX `shm_open` lives in `/dev/shm`, which Docker gives each
    ///   container privately ( and caps at 64 MiB by default ); System V segments
    ///   live in the IPC namespace, which containers also get separately. Either
    ///   would need a deployment to share something extra, and a deployment that
    ///   forgot would fail at connect time rather than at review time.
    /// - **It cannot leak.** The region is freed when the last descriptor closes and
    ///   the last mapping goes. A POSIX segment outlives a crashed creator until
    ///   `shm_unlink` or reboot, and a System V segment outlives it until `ipcrm` --
    ///   both survivable with discipline ( unlink or `IPC_RMID` immediately after
    ///   creation ), and both a discipline that a crash between two lines defeats.
    /// - **It has no name to guess.** Only the peer we handed the descriptor to can
    ///   reach it. A POSIX path exists in the filesystem for at least a moment, and
    ///   a System V key is reachable by anything in the namespace.
    ///
    /// The one thing they have and this does not is portability: `memfd_create` is
    /// Linux-only. If a non-Linux host is ever needed, POSIX `shm_open` with an
    /// immediate `shm_unlink` is the fallback, and it gives up the size and
    /// namespace properties above rather than correctness.
    ///
    /// # Sealing, which neither alternative offers
    ///
    /// `F_SEAL_SHRINK` is the point. A mapping whose file is truncated underneath it
    /// raises `SIGBUS` on the next touch, and that is not catchable as a `Result` --
    /// the same hazard invariant I6 states for the shard files, arriving here by a
    /// different route. Sealing makes the size immutable for **both** sides, so a
    /// bug on either end cannot produce it. `F_SEAL_GROW` pairs with it so the peer's
    /// mapping length stays the whole region, and `F_SEAL_SEAL` stops anything
    /// adding further seals later -- notably `F_SEAL_WRITE`, which would break the
    /// host's own writes.
    #[cfg(target_os = "linux")]
    pub fn new(bytes: usize) -> std::io::Result<Arena> {
        use std::os::fd::FromRawFd;
        let name = c"yesno-plugin-arena";
        // SAFETY: `name` is a valid NUL-terminated C string that outlives the call,
        // and the flags are documented ones. ALLOW_SEALING is required at creation;
        // a memfd made without it can never be sealed.
        let raw = unsafe {
            libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING)
        };
        if raw < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: `raw` is a fresh descriptor this call owns.
        let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) };
        let file = std::fs::File::from(fd.try_clone()?);
        file.set_len(bytes as u64)?;
        // Sealed after sizing and before anyone else can hold it, which is the only
        // moment both halves of that are true.
        let seals = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_SEAL;
        // SAFETY: `fd` is a live memfd created with MFD_ALLOW_SEALING.
        let rc = unsafe {
            libc::fcntl(
                std::os::fd::AsRawFd::as_raw_fd(&fd),
                libc::F_ADD_SEALS,
                seals,
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: the descriptor is a memfd sized to `bytes` above, and nothing
        // else has it yet.
        let map = unsafe { memmap2::MmapOptions::new().len(bytes).map_mut(&file)? };
        Ok(Arena { map, fd })
    }

    #[cfg(not(target_os = "linux"))]
    pub fn new(_bytes: usize) -> std::io::Result<Arena> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "the plugin channel's shared arena needs memfd_create, which is Linux-only",
        ))
    }

    /// The descriptor to send to a peer.
    pub fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        std::os::fd::AsFd::as_fd(&self.fd)
    }

    /// Write into the arena. Exposed for tests that assert the region a peer
    /// receives is the same memory rather than a copy of it.
    #[doc(hidden)]
    pub fn write_at_for_test(&mut self, off: usize, bytes: &[u8]) {
        self.write_at(off, bytes)
    }

    fn write_at(&mut self, off: usize, bytes: &[u8]) {
        let end = off + bytes.len();
        // Bounds are the session's arithmetic, checked here rather than trusted: a
        // slot offset is derived from a handle index the session assigned, so a
        // failure means this file is wrong, not that a peer sent something.
        debug_assert!(end <= self.map.len(), "a lane wrote past the arena");
        if end <= self.map.len() {
            self.map[off..end].copy_from_slice(bytes);
        }
    }
}

/// One acquired lanes handle.
struct Handle {
    lanes: KeyLanes,
    /// Which fixed slot of the arena this handle owns.
    index: usize,
    arity: usize,
    block_open: bool,
    /// The snapshot id this came from, so closing that snapshot can refuse while
    /// handles derived from it are live.
    snapshot: u64,
}

/// One peer's conversation.
pub struct Session {
    host: Host,
    /// `None` on a transport with no shared region, where payloads travel in the
    /// frames instead. See [`Session::new_inline`].
    arena: Option<Arena>,
    limits: Limits,
    snapshots: HashMap<u64, Snapshot>,
    handles: HashMap<u64, Handle>,
    /// Which arena slots are in use, by index.
    slots: Vec<bool>,
    next_id: u64,
    greeted: bool,
}

impl Session {
    pub fn new(host: Host, arena: Arena, limits: Limits) -> Session {
        Session::with_arena(host, Some(arena), limits)
    }

    /// A session for a transport that cannot share memory.
    ///
    /// Payloads then travel inside the response frames. This is what makes the
    /// channel portable: the protocol is bytes and makes no system calls, the
    /// stream is whatever the platform offers, and **only the shared region is
    /// platform-specific**. Where one cannot be had, this path still works and
    /// costs a second copy and a smaller batch.
    pub fn new_inline(host: Host, limits: Limits) -> Session {
        Session::with_arena(host, None, limits)
    }

    /// Most lanes one inline block can carry.
    ///
    /// An inline block puts every present lane's payload in the frame, so the worst
    /// case is `lanes * LANE_BYTES` and it must fit [`MAX_INLINE_PAYLOAD`]. With the
    /// arena this does not arise: a `Block` frame carries only descriptors, five
    /// bytes each, so a thousand lanes is five kilobytes.
    pub const INLINE_MAX_LANES: usize = MAX_INLINE_PAYLOAD / LANE_BYTES;

    fn with_arena(host: Host, arena: Option<Arena>, mut limits: Limits) -> Session {
        // **Inline capacity is a property of the transport, so the advertised limits
        // have to come from it.** Configuration alone produced a greeting that
        // promised more than the frame could hold: at the default 1024 lanes, one
        // block of bitmap lanes is 8 MiB against a 1 MiB cap, so `Frame::encode`
        // refused and the connection died -- for a configuration the server itself
        // advertised as legal.
        //
        // Clamped here rather than only in the greeting, so that what is advertised,
        // what `lanes_acquire` enforces, and what can actually be encoded are one
        // number and cannot drift apart.
        if arena.is_none() {
            limits.max_lanes = limits.max_lanes.clamp(1, Self::INLINE_MAX_LANES);
            // Whatever is left of the budget after the lanes, at least one block.
            let per_batch = Self::INLINE_MAX_LANES / limits.max_lanes.max(1);
            limits.max_blocks = limits.max_blocks.clamp(1, per_batch.max(1));
        }
        Session {
            host,
            arena,
            limits,
            snapshots: HashMap::new(),
            handles: HashMap::new(),
            slots: vec![false; limits.max_handles],
            // Ids start at 1 so zero is never a valid handle, which makes an
            // uninitialised field in a peer fail immediately rather than address
            // something.
            next_id: 1,
            greeted: false,
        }
    }

    /// The greeting to send before any request is answered.
    pub fn hello(&self) -> Frame {
        Frame::ServerHello {
            protocol: crate::ipc::VERSION as u32,
            generation: self.host.generation(),
            role: match self.host.role() {
                Role::Leader => WireRole::Leader,
                Role::Follower => WireRole::Follower,
            },
            shards: self.host.db().map(|d| d.shard_count() as u32).unwrap_or(0),
            // Zero is the advertisement that there is no shared region, so a peer
            // knows to expect inline payloads without a second negotiation field.
            arena_bytes: match self.arena {
                Some(_) => self.limits.arena_bytes() as u64,
                None => 0,
            },
            max_lanes: self.limits.max_lanes as u32,
            max_handles: self.limits.max_handles as u32,
            max_blocks: self.limits.max_blocks.max(1) as u32,
        }
    }

    /// Whether this session has a shared region at all.
    pub fn has_arena(&self) -> bool {
        self.arena.is_some()
    }

    /// The arena descriptor to send the peer, when there is one.
    ///
    /// The socket loop passes this with `SCM_RIGHTS` once, right after the greeting.
    /// A peer maps it read-only and never writes: every byte in it is produced by
    /// this side, so a writable mapping on the peer would let a bug there corrupt
    /// what the next block reports.
    pub fn arena_fd(&self) -> Option<std::os::fd::BorrowedFd<'_>> {
        self.arena.as_ref().map(|a| a.as_fd())
    }

    /// Snapshots and handles this session is holding.
    pub fn outstanding(&self) -> (usize, usize) {
        (self.snapshots.len(), self.handles.len())
    }

    fn fault(status: Status, message: &str) -> Frame {
        Frame::Fault {
            status: status as u32,
            message: message.to_string(),
        }
    }

    /// Answer one request.
    ///
    /// A frame that is not a request is refused by its kind rather than interpreted:
    /// the protocol puts requests, responses and notifications in separate
    /// discriminant ranges precisely so this check is one comparison.
    pub fn handle(&mut self, frame: Frame) -> Frame {
        if frame.kind().direction() != crate::ipc::Direction::Request {
            return Self::fault(
                Status::InvalidArgument,
                "only request frames are accepted here",
            );
        }
        match frame {
            Frame::ClientHello { protocol, .. } => {
                if protocol != crate::ipc::VERSION as u32 {
                    return Self::fault(Status::AbiMismatch, "unsupported protocol version");
                }
                self.greeted = true;
                Frame::Done
            }
            _ if !self.greeted => Self::fault(
                Status::InvalidArgument,
                "the first frame must be ClientHello",
            ),
            Frame::SnapshotOpen => self.snapshot_open(),
            Frame::SnapshotClose { snapshot } => self.snapshot_close(snapshot),
            Frame::LanesAcquire { snapshot, keys } => self.lanes_acquire(snapshot, &keys),
            Frame::LanesRelease { lanes } => self.lanes_release(lanes),
            Frame::BlockAdvance { lanes } => self.block_advance(lanes),
            Frame::BlockRelease { lanes } => self.block_release(lanes),
            Frame::BlockAdvanceMany { lanes, max_blocks } => {
                self.block_advance_many(lanes, max_blocks)
            }
            Frame::SnapshotCardinality { snapshot, key } => self.with_snapshot(snapshot, |s| {
                s.cardinality(key).map(|value| Frame::Count { value })
            }),
            Frame::SnapshotContains {
                snapshot,
                key,
                ordinal,
            } => self.with_snapshot(snapshot, |s| {
                s.contains(key, ordinal).map(|yes| Frame::Bool {
                    value: u8::from(yes),
                })
            }),
            Frame::SnapshotMax { snapshot, key } => self.with_snapshot(snapshot, |s| {
                s.max(key).map(|m| Frame::Ordinal {
                    present: u8::from(m.is_some()),
                    value: m.unwrap_or(0),
                })
            }),
            Frame::SnapshotLoad {
                snapshot,
                key,
                after,
                has_after,
                limit,
            } => {
                let after = (has_after == 1).then_some(after);
                let want = (limit as usize).clamp(1, MAX_PAGE);
                self.with_snapshot(snapshot, |s| load_page(s, key, after, want))
            }
            Frame::SnapshotKeyRange {
                snapshot,
                lo,
                hi,
                limit,
            } => {
                let want = (limit as usize).clamp(1, MAX_PAGE);
                self.with_snapshot(snapshot, |s| {
                    // One extra, so "is there another page" is answered by what the
                    // engine returned rather than by guessing from a full page.
                    //
                    // **Bounded in the engine, not here.** This used to call
                    // `key_range` and truncate, so a one-key request cost a
                    // whole-range enumeration of every shard -- a peer could buy a
                    // full scan for the price of the smallest possible page, which a
                    // 2026-09-29 security review named as request amplification.
                    let mut keys = s.key_range_limited(lo, hi, want.saturating_add(1))?;
                    let more = keys.len() > want;
                    keys.truncate(want);
                    Ok(Frame::Keys {
                        values: keys,
                        more: u8::from(more),
                    })
                })
            }
            // Every request kind is handled above; the range check made this
            // unreachable, and an unreachable arm is cheaper than a panic.
            other => Self::fault(
                Status::Internal,
                &format!("unhandled request kind {:?}", other.kind()),
            ),
        }
    }

    /// Run `f` against a named snapshot, turning a core error into a fault.
    ///
    /// Shared by every read below so that "no such snapshot" and
    /// `SNAPSHOT_TOO_OLD` are reported one way rather than five, and so adding a
    /// sixth read cannot get the mapping wrong.
    fn with_snapshot(
        &self,
        id: u64,
        f: impl FnOnce(&Snapshot) -> yesno_core::Result<Frame>,
    ) -> Frame {
        let Some(snap) = self.snapshots.get(&id) else {
            return Self::fault(Status::InvalidArgument, "no such snapshot");
        };
        match f(snap) {
            Ok(frame) => frame,
            Err(e) => Self::fault(Status::from_core(&e), "the read failed"),
        }
    }

    fn snapshot_open(&mut self) -> Frame {
        if self.snapshots.len() >= self.limits.max_snapshots {
            // Refused rather than evicting an older one: the peer holds handles
            // that name these, and silently invalidating one would turn a quota
            // into a wrong answer on a snapshot the peer still believes in.
            return Self::fault(
                Status::InvalidArgument,
                "too many open snapshots on this connection; close one first",
            );
        }
        let Some(db) = self.host.db() else {
            return Self::fault(Status::Unavailable, "no database is open");
        };
        let snap = match db.snapshot() {
            Ok(s) => s,
            Err(e) => return Self::fault(Status::from_core(&e), "cannot take a snapshot"),
        };
        let id = self.next_id;
        self.next_id += 1;
        let version = snap.version();
        self.snapshots.insert(id, snap);
        Frame::SnapshotOpened {
            snapshot: id,
            version,
        }
    }

    fn snapshot_close(&mut self, id: u64) -> Frame {
        if self.handles.values().any(|h| h.snapshot == id) {
            // Refused rather than allowed-and-ignored. A peer closing a snapshot
            // whose lanes it still holds has a bug, and the in-process ABI's answer
            // -- derived handles keep the version alive -- is not available here
            // because the session owns the snapshot, not the handle.
            return Self::fault(
                Status::InvalidArgument,
                "lane handles derived from this snapshot are still open",
            );
        }
        if self.snapshots.remove(&id).is_none() {
            return Self::fault(Status::InvalidArgument, "no such snapshot");
        }
        Frame::Done
    }

    fn lanes_acquire(&mut self, snapshot: u64, keys: &[u64]) -> Frame {
        if keys.len() > self.limits.max_lanes {
            return Self::fault(Status::InvalidArgument, "too many lanes for this server");
        }
        let Some(snap) = self.snapshots.get(&snapshot) else {
            return Self::fault(Status::InvalidArgument, "no such snapshot");
        };
        let Some(index) = self.slots.iter().position(|used| !used) else {
            return Self::fault(Status::InvalidArgument, "no free arena slot");
        };
        let lanes = match KeyLanes::new(snap, keys) {
            Ok(l) => l,
            Err(e) => return Self::fault(Status::from_core(&e), "cannot open the lanes"),
        };
        self.slots[index] = true;
        let id = self.next_id;
        self.next_id += 1;
        self.handles.insert(
            id,
            Handle {
                lanes,
                index,
                arity: keys.len(),
                block_open: false,
                snapshot,
            },
        );
        Frame::LanesAcquired {
            lanes: id,
            arena_off: (index * self.limits.slot_bytes()) as u64,
        }
    }

    fn lanes_release(&mut self, id: u64) -> Frame {
        match self.handles.remove(&id) {
            Some(h) => {
                self.slots[h.index] = false;
                Frame::Done
            }
            None => Self::fault(Status::InvalidArgument, "no such lane handle"),
        }
    }

    fn block_advance(&mut self, id: u64) -> Frame {
        let slot_bytes = self.limits.slot_bytes();
        let Some(h) = self.handles.get_mut(&id) else {
            return Self::fault(Status::InvalidArgument, "no such lane handle");
        };
        if h.block_open {
            return Self::fault(
                Status::BlockState,
                "release the open block before advancing; its arena slice is still lent",
            );
        }
        let advanced = match h.lanes.advance() {
            Ok(a) => a,
            Err(e) => return Self::fault(Status::from_core(&e), "cannot advance"),
        };
        let Some(prefix) = advanced else {
            return Frame::BlockDone;
        };
        h.block_open = true;
        let base = h.index * slot_bytes;
        let arity = h.arity;
        // Collected before touching the arena, because writing needs `&mut self`
        // while the containers are borrowed from the handle.
        let mut descs = Vec::with_capacity(arity);
        for i in 0..arity {
            match h.lanes.lane(i) {
                None => descs.push((
                    Lane {
                        kind: LaneKind::Absent,
                        count: 0,
                    },
                    Vec::new(),
                )),
                Some(c) => descs.push(encode_lane(c)),
            }
        }
        match self.arena.as_mut() {
            Some(arena) => {
                for (i, (_, bytes)) in descs.iter().enumerate() {
                    if !bytes.is_empty() {
                        arena.write_at(base + i * LANE_BYTES, bytes);
                    }
                }
                Frame::Block {
                    prefix,
                    lanes: descs.into_iter().map(|(l, _)| l).collect(),
                }
            }
            None => {
                let mut payload = Vec::new();
                let mut lanes = Vec::with_capacity(descs.len());
                for (l, bytes) in descs {
                    payload.extend_from_slice(&bytes);
                    lanes.push(l);
                }
                Frame::BlocksInline {
                    blocks: vec![WireBlock { prefix, lanes }],
                    payload,
                }
            }
        }
    }

    /// Advance up to `want` blocks, releasing the previous batch implicitly.
    ///
    /// The implicit release is what makes a batch worth having: keeping a separate
    /// round trip to say "done with the last one" would spend one notification per
    /// batch restating what asking for the next batch already proves.
    fn block_advance_many(&mut self, id: u64, want: u32) -> Frame {
        let block_bytes = self.limits.block_bytes();
        let slot_bytes = self.limits.slot_bytes();
        let cap = self.limits.max_blocks.max(1).min(want.max(1) as usize);
        let Some(h) = self.handles.get_mut(&id) else {
            return Self::fault(Status::InvalidArgument, "no such lane handle");
        };
        h.block_open = false;
        let base = h.index * slot_bytes;
        let arity = h.arity;
        let mut out: Vec<WireBlock> = Vec::with_capacity(cap);
        // Payloads are collected before the arena is touched, because writing needs
        // `&mut self` while the containers are borrowed from the handle.
        let mut writes: Vec<(usize, Vec<u8>)> = Vec::new();
        for j in 0..cap {
            let advanced = match h.lanes.advance() {
                Ok(a) => a,
                Err(e) => return Self::fault(Status::from_core(&e), "cannot advance"),
            };
            let Some(prefix) = advanced else { break };
            let mut lanes = Vec::with_capacity(arity);
            for i in 0..arity {
                match h.lanes.lane(i) {
                    None => lanes.push(Lane {
                        kind: LaneKind::Absent,
                        count: 0,
                    }),
                    Some(c) => {
                        let (lane, bytes) = encode_lane(c);
                        if !bytes.is_empty() {
                            writes.push((base + j * block_bytes + i * LANE_BYTES, bytes));
                        }
                        lanes.push(lane);
                    }
                }
            }
            out.push(WireBlock { prefix, lanes });
        }
        // A batch that returned anything leaves the last block's payloads lent until
        // the next request, which is what `block_open` records for the single-block
        // path and what the implicit release handles here.
        if !out.is_empty() {
            if let Some(h) = self.handles.get_mut(&id) {
                h.block_open = true;
            }
        }
        match self.arena.as_mut() {
            Some(arena) => {
                for (off, bytes) in writes {
                    arena.write_at(off, &bytes);
                }
                Frame::Blocks { blocks: out }
            }
            None => {
                // Inline order is ( block, lane ), which is the order `writes` was
                // built in, so concatenating it needs no sort and no offsets.
                let mut payload = Vec::new();
                for (_, bytes) in writes {
                    payload.extend_from_slice(&bytes);
                }
                Frame::BlocksInline {
                    blocks: out,
                    payload,
                }
            }
        }
    }

    fn block_release(&mut self, id: u64) -> Frame {
        match self.handles.get_mut(&id) {
            Some(h) => {
                h.block_open = false;
                h.lanes.release_block();
                Frame::Done
            }
            None => Self::fault(Status::InvalidArgument, "no such lane handle"),
        }
    }
}

/// A lane's descriptor and the bytes to place in its arena slot.
///
/// Copies, and that is the honest cost of the process boundary: the in-process ABI
/// lends the mapping's own bytes, and nothing can be lent across an address space
/// that the peer does not already map. What the arena buys is that the copy is one
/// `memcpy` into a page the peer already sees, rather than a write and a read
/// through a socket.
fn encode_lane(c: &Container) -> (Lane, Vec<u8>) {
    match c {
        Container::Array(a) => {
            let s = a.as_slice();
            (
                Lane {
                    kind: LaneKind::Array,
                    count: s.len() as u32,
                },
                s.iter().flat_map(|v| v.to_le_bytes()).collect(),
            )
        }
        Container::Run(r) => {
            // **Converted, not copied.** In memory a run is `( start, len_minus_1 )`
            // pairs -- the Roaring spec's on-disk form, which is what makes the
            // serialized bytes identical to it -- while the wire says
            // `[ start, end ]`. Emitting the stored pairs gave a consumer following
            // the contract a silently wrong set: a run of 1000..=5999 is
            // `( 1000, 4999 )`, read as an inclusive range that is a thousand values
            // short, and a run whose start exceeds its length reads as a reversed
            // interval.
            let flat = r.as_flat();
            let mut bytes = Vec::with_capacity(flat.len() * 2);
            for pair in flat.chunks_exact(2) {
                let (start, len_minus_1) = (pair[0], pair[1]);
                bytes.extend_from_slice(&start.to_le_bytes());
                bytes.extend_from_slice(&(start + len_minus_1).to_le_bytes());
            }
            (
                Lane {
                    kind: LaneKind::Run,
                    count: (flat.len() / 2) as u32,
                },
                bytes,
            )
        }
        Container::Bitmap(b) => {
            let mut words = vec![0u64; yesno_core::BITMAP_WORDS];
            // `copy_words_into` rather than `try_words`, because the unaligned arm
            // has to work too and a borrow here would only save a copy that the
            // arena write makes anyway.
            let ok = b.copy_words_into(&mut words);
            debug_assert!(ok, "a bitmap payload is always BITMAP_WORDS long");
            (
                Lane {
                    kind: LaneKind::Bitmap,
                    count: yesno_core::BITMAP_WORDS as u32,
                },
                words.iter().flat_map(|w| w.to_le_bytes()).collect(),
            )
        }
    }
}

/// Which request kinds a session will accept before `ClientHello`.
///
/// Exposed so a test can assert the greeting requirement without reaching into the
/// session's private state.
pub fn requires_greeting(kind: Kind) -> bool {
    !matches!(kind, Kind::ClientHello)
}

/// Read one frame from `r`, growing `buf` until a whole frame is present.
///
/// `Ok( None )` on a clean end of stream, which is the peer having gone away -- the
/// signal that the session may be dropped and its snapshots released.
pub fn read_frame<R: std::io::Read>(
    r: &mut R,
    buf: &mut Vec<u8>,
) -> std::io::Result<Option<Frame>> {
    loop {
        match Frame::decode(buf) {
            Ok((f, used)) => {
                buf.drain(..used);
                return Ok(Some(f));
            }
            Err(crate::ipc::IpcError::Truncated) => {}
            Err(e) => {
                return Err(<std::io::Error as InvalidData>::new_invalid(e));
            }
        }
        let mut chunk = [0u8; 4096];
        let n = r.read(&mut chunk)?;
        if n == 0 {
            // A clean close with nothing buffered is the peer leaving. A clean close
            // mid-frame is a peer that died between writes, which is the same
            // outcome for us and not worth a different error.
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// Serve one connection to completion, blocking.
///
/// Returns when the peer closes. The caller drops the session afterwards, which is
/// what releases its snapshots -- so a peer that crashes mid-scan costs nothing
/// beyond the work already done.
///
/// Blocking rather than async, and on purpose: every request touches the engine,
/// which faults mmap pages and may read from disk, so this belongs on a blocking
/// pool exactly as `do_get` does. An async signature would invite it onto a
/// reactor thread.
pub fn serve_blocking<S>(session: &mut Session, mut stream: S) -> std::io::Result<()>
where
    S: std::io::Read + std::io::Write,
{
    let hello = session
        .hello()
        .encode()
        .map_err(<std::io::Error as InvalidData>::new_invalid)?;
    stream.write_all(&hello)?;
    let mut buf = Vec::new();
    while let Some(frame) = read_frame(&mut stream, &mut buf)? {
        let reply = session.handle(frame);
        let bytes = reply
            .encode()
            .map_err(<std::io::Error as InvalidData>::new_invalid)?;
        stream.write_all(&bytes)?;
    }
    Ok(())
}

/// Turn a protocol error into an `io::Error`, so the loop has one error type.
trait InvalidData {
    fn new_invalid(e: crate::ipc::IpcError) -> std::io::Error;
}

impl InvalidData for std::io::Error {
    fn new_invalid(e: crate::ipc::IpcError) -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
    }
}

/// Send `fd` to the peer on `stream`, with one byte of payload.
///
/// # Why a byte of payload
///
/// `sendmsg` with only ancillary data and an empty `iovec` is permitted but is not
/// reliably *received*: a zero-length datagram on a stream socket is
/// indistinguishable from nothing arriving, so a reader cannot tell it apart from a
/// peer that has not spoken yet. One byte makes the descriptor's arrival an event
/// the reader can wait for.
///
/// The byte is the protocol version, so a peer that reads it and disagrees learns
/// that before it maps anything.
#[cfg(target_os = "linux")]
pub fn send_fd<S: std::os::fd::AsRawFd>(
    stream: &S,
    fd: std::os::fd::BorrowedFd<'_>,
) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    let mut byte = [crate::ipc::VERSION; 1];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr() as *mut libc::c_void,
        iov_len: 1,
    };
    // The control buffer must be aligned for `cmsghdr`, which a plain byte array is
    // not guaranteed to be. A `u64` array gives 8-byte alignment, which is enough on
    // every platform this builds for.
    const SPACE: usize = 32;
    let mut control = [0u64; SPACE / 8];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = SPACE as _;

    // SAFETY: `msg.msg_control` points at `SPACE` writable, suitably aligned bytes,
    // which is what CMSG_FIRSTHDR requires to return a usable header.
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<libc::c_int>() as u32) as _;
        let raw = fd.as_raw_fd();
        std::ptr::copy_nonoverlapping(
            &raw as *const libc::c_int,
            libc::CMSG_DATA(cmsg) as *mut libc::c_int,
            1,
        );
        msg.msg_controllen = (*cmsg).cmsg_len;
    }

    loop {
        // SAFETY: `msg` describes one writable byte and one well-formed control
        // message, both alive for the call.
        let n = unsafe { libc::sendmsg(stream.as_raw_fd(), &msg, 0) };
        if n >= 0 {
            return Ok(());
        }
        let e = std::io::Error::last_os_error();
        // A signal here is not a failure; anything else is.
        if e.kind() != std::io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

/// Receive a descriptor sent by [`send_fd`], with the version byte it carried.
///
/// Written here rather than left to each peer because the two are one protocol: a
/// receiver that sizes its control buffer differently, or forgets `MSG_CMSG_CLOEXEC`,
/// fails in ways that look like the sender's fault.
#[cfg(target_os = "linux")]
pub fn recv_fd<S: std::os::fd::AsRawFd>(stream: &S) -> std::io::Result<(std::os::fd::OwnedFd, u8)> {
    use std::os::fd::FromRawFd;

    let mut byte = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr() as *mut libc::c_void,
        iov_len: 1,
    };
    const SPACE: usize = 32;
    let mut control = [0u64; SPACE / 8];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = SPACE as _;

    let n = loop {
        // SAFETY: `msg` describes one writable byte and a control buffer of SPACE
        // aligned bytes. MSG_CMSG_CLOEXEC is what stops a received descriptor
        // leaking across an exec in the receiver.
        let n = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC) };
        if n >= 0 {
            break n;
        }
        let e = std::io::Error::last_os_error();
        if e.kind() != std::io::ErrorKind::Interrupted {
            return Err(e);
        }
    };
    if n == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "the peer closed before sending the arena descriptor",
        ));
    }
    // SAFETY: `msg` was filled by `recvmsg` above.
    let cmsg = unsafe { libc::CMSG_FIRSTHDR(&msg) };
    if cmsg.is_null() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "no descriptor accompanied the arena message",
        ));
    }
    // SAFETY: non-null, and produced by `recvmsg` for this buffer.
    unsafe {
        if (*cmsg).cmsg_level != libc::SOL_SOCKET || (*cmsg).cmsg_type != libc::SCM_RIGHTS {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "the ancillary message was not SCM_RIGHTS",
            ));
        }
        let mut raw: libc::c_int = -1;
        std::ptr::copy_nonoverlapping(libc::CMSG_DATA(cmsg) as *const libc::c_int, &mut raw, 1);
        Ok((std::os::fd::OwnedFd::from_raw_fd(raw), byte[0]))
    }
}

/// Serve one connection, writing responses through a shared, lockable writer.
///
/// [`serve_blocking`] owns the whole stream, which is right for a test and wrong for
/// a server: a notification has to reach a peer while its serving thread is blocked
/// in `read`, so the write half must be reachable from elsewhere. The lock is held
/// for exactly one frame, and a frame is the protocol's unit, so a notification may
/// land between responses but never inside one.
pub fn serve_locked<R>(
    session: &mut Session,
    mut reader: R,
    writer: &std::sync::Mutex<impl std::io::Write>,
) -> std::io::Result<()>
where
    R: std::io::Read,
{
    let write_frame = |f: &Frame| -> std::io::Result<()> {
        // A reply this side cannot encode is **our** bug, and killing the connection
        // is the worst way to report it: the peer sees a closed socket and cannot
        // tell a protocol defect from a crash or a restart. Answer a fault instead,
        // which is a status it can log and act on.
        let bytes = match f.encode() {
            Ok(b) => b,
            Err(e) => Frame::Fault {
                status: crate::abi::Status::Internal as u32,
                message: format!("the server could not encode its reply: {e}"),
            }
            .encode()
            .map_err(<std::io::Error as InvalidData>::new_invalid)?,
        };
        let mut w = writer
            .lock()
            .map_err(|_| std::io::Error::other("the channel writer lock was poisoned"))?;
        w.write_all(&bytes)
    };

    write_frame(&session.hello())?;
    let mut buf = Vec::new();
    while let Some(frame) = read_frame(&mut reader, &mut buf)? {
        let reply = session.handle(frame);
        write_frame(&reply)?;
    }
    Ok(())
}

/// One page of a key's ordinals, resuming strictly above `after`.
///
/// # Why this walks chunks instead of calling `load`
///
/// `Snapshot::load` materializes the whole set, so paging with it would
/// re-materialize the remainder for every page -- quadratic in the number of pages,
/// which is the opposite of what paging is for. `key_stream` is lazy and can `seek`,
/// so a page costs one seek plus the chunks it actually reads.
///
/// The `after` ordinal may sit in the middle of a chunk, so the chunk containing it
/// is decoded and its ordinals at or below it are skipped. Only that first chunk
/// pays that.
fn load_page(
    snap: &Snapshot,
    key: u64,
    after: Option<u64>,
    want: usize,
) -> yesno_core::Result<Frame> {
    use yesno_core::ChunkStream;

    let mut stream = snap.key_stream(key)?;
    if let Some(a) = after {
        let (prefix, _) = yesno_core::split(a);
        stream.seek(prefix)?;
    }
    // One more than asked, so `more` is answered by what was found rather than
    // inferred from the page being full -- a full page is not evidence of another.
    let mut out: Vec<u64> = Vec::with_capacity(want.min(1024) + 1);
    'outer: while out.len() <= want {
        let Some((prefix, container)) = stream.next_chunk()? else {
            break;
        };
        for low in container.iter() {
            let ordinal = yesno_core::join(prefix, low);
            if after.is_some_and(|a| ordinal <= a) {
                continue;
            }
            out.push(ordinal);
            if out.len() > want {
                break 'outer;
            }
        }
    }
    let more = out.len() > want;
    out.truncate(want);
    Ok(Frame::Ordinals {
        values: out,
        more: u8::from(more),
    })
}
