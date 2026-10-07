//! Client side of the yesnod plugin channel.
//!
//! [`crate::channel`] is the server and [`ipc`] is the wire format; until now nothing in
//! this workspace spoke the protocol as a *client*, so the one existing consumer
//! wrote its own. That is the shape that let the Flight ticket header widen from 40
//! to 48 bytes with three independently written clients left behind, and the reason
//! this module exists: one client implementation, with `yesno-pg` using it directly
//! and a C ABI restating it for the MySQL handler.
//!
//! # The handshake is not symmetrical, and that is the easiest thing to get wrong
//!
//! The **server speaks first**. In arena mode it sends one byte with `SCM_RIGHTS`
//! carrying the arena descriptor, *before* [`Frame::ServerHello`]; in inline mode
//! there is no descriptor and the stream opens with the frame. So the first read
//! must be `recvmsg` -- an ordinary `read` succeeds and silently drops the
//! descriptor, and the failure then surfaces much later as a missing arena.
//!
//! Only **after** that greeting does the client send [`Frame::ClientHello`], which
//! is answered with [`Frame::Done`] rather than another hello, and the server
//! refuses every other request until it arrives.
//!
//! The byte is read with a one-byte `iovec` on purpose. A larger buffer also works
//! and is what the existing consumer does, but then the same `recvmsg` can pull in
//! part of `ServerHello` and the remainder has to be pushed back into the decode
//! buffer. One byte leaves the rest in the socket, so there is nothing to push back
//! in arena mode -- and in inline mode exactly one byte needs pushing back, because
//! that byte is already the first byte of the frame.
//!
//! # A closed connection under a pinned snapshot is expiry, not failure
//!
//! yesnod disconnects every old session before a follower cutover and need not get a
//! notification out first, so six `io::ErrorKind`s mean "this version is gone" and
//! not "something broke". Reporting them as I/O errors would turn a routine cutover
//! into an incident; [`Error::SnapshotExpired`] names the version instead, and the
//! connection latches `stale` so a handle cannot be used again.
//!
//! # Status
//!
//! **Written without being compiled**, deliberately and at the user's instruction:
//! a peer session holds cores 5-9 and 15-17 for a multi-hour measurement, and
//! `cargo check` would both contend for its cache -- its pinned set spans both L3
//! clusters, so no core on this host is outside its footprint -- and write `target/`
//! under its disk gate. Treat every signature here as unverified until the gate has
//! run. The protocol facts are not guesses: they are read from [`crate::channel`] and from
//! the existing consumer's client, cited in
//! `.agents/docs/peer-sockets-for-the-db-plugins.md`.
use crate::channel::read_frame;
use crate::ipc::{self, Block, Frame, Lane, Role, Write};
use std::io::{self, Write as _};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// What a client call can go wrong with.
#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    /// The frame codec refused to encode or decode.
    Codec(ipc::IpcError),
    /// The server did something the protocol does not allow. Distinct from
    /// [`Error::Fault`]: that is the server reporting, this is the server misbehaving.
    Protocol(String),
    /// The server answered with [`Frame::Fault`] and it was not one of the statuses
    /// classified below.
    Fault {
        status: u32,
        message: String,
    },
    /// Every handle on this connection is stale: the database was replaced, or the
    /// pinned version was evicted. Reconnect; retrying the call cannot help.
    ///
    /// Connection-wide rather than per-snapshot on purpose -- [`Frame::GenerationChanged`]
    /// says "every handle the peer holds is stale", so scoping this to one snapshot
    /// would understate it. A version-pinned caller sees [`Error::SnapshotExpired`]
    /// instead, which is this error with the version attached.
    Stale,
    /// The server has no database in its slot, which is a follower rebootstrapping.
    /// **Retryable**, unlike every other error here: a peer that started in the gap
    /// between a follower binding its channel and bootstrap opening the database will
    /// see this and should back off rather than fail.
    Unavailable {
        message: String,
    },
    /// The request needed a leader and this connection is to a read-only replica.
    WrongRole,
    /// The pinned version is gone. Take a fresh snapshot; do not retry the call.
    SnapshotExpired {
        version: u64,
    },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "plugin channel I/O: {e}"),
            Error::Codec(e) => write!(f, "plugin channel codec: {e:?}"),
            Error::Protocol(m) => write!(f, "plugin channel protocol violation: {m}"),
            Error::Fault { status, message } => {
                write!(f, "yesnod fault {status}: {message}")
            }
            Error::Stale => write!(f, "every handle on this plugin channel is stale"),
            Error::Unavailable { message } => {
                write!(f, "yesnod has no database in its slot: {message}")
            }
            Error::WrongRole => {
                write!(f, "this plugin channel is to a read-only replica")
            }
            Error::SnapshotExpired { version } => {
                write!(f, "pinned snapshot version {version} has expired")
            }
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Does this I/O error mean yesnod closed the channel, rather than a real fault?
///
/// Enumerated rather than matched loosely, because the point is to be *narrow*: a
/// permission error or a malformed frame must not be laundered into an expiry.
fn is_disconnect(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::NotConnected
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::UnexpectedEof
            | io::ErrorKind::WriteZero
    )
}

fn protocol<T>(message: impl Into<String>) -> Result<T> {
    Err(Error::Protocol(message.into()))
}

/// Everything the server advertised in its greeting.
///
/// `max_blocks` and `max_writes` are advertised rather than published as constants
/// precisely so a peer need not guess: guessing low on the first is merely slow for
/// an invisible reason, and guessing high on the second fails in the peer's own
/// encoder before anything is sent.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub protocol: u32,
    pub generation: u64,
    pub role: Role,
    pub shards: u32,
    pub arena_bytes: u64,
    pub max_lanes: u32,
    pub max_handles: u32,
    pub max_blocks: u32,
    pub max_writes: u32,
}

/// A server-initiated frame that arrived while we were waiting for a response.
///
/// The server interleaves these between responses but never inside one, so a request
/// loop steps over them. **`GenerationChanged` is not here**, because the protocol
/// documents it as "every handle the peer holds is stale" -- it is terminal and
/// surfaces as [`Error::Stale`]. The three below are not terminal, and treating them
/// as if they were costs the caller something it did not have to lose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Notice {
    /// No database: a follower is rebootstrapping. Requests fail with
    /// [`Error::Unavailable`] until [`Notice::Available`], and **handles survive** --
    /// the protocol says requests will fail until `Available`, not that anything is
    /// destroyed. So the loop records this and keeps reading: the pending request
    /// gets its own `Fault( Unavailable )` from the server, which is retryable.
    ///
    /// Returning early here instead would also **desynchronize the stream**, because
    /// the frames carry no request identifiers and the unread response would be
    /// mispaired with the next request.
    Unavailable,
    /// The database is back. A generation equal to the one greeted means handles are
    /// still good; a different one is terminal and does not reach here.
    Available { generation: u64 },
    /// Leadership moved. Reads stay valid; a write may now get [`Error::WrongRole`].
    RoleChanged { from: Role, to: Role },
}

struct Connection {
    socket: UnixStream,
    buf: Vec<u8>,
    /// The generation from `ServerHello`. An `Available` naming a different one means
    /// the database was replaced while we were away, so it is not good news.
    generation: u64,
    stale: bool,
    notices: Vec<Notice>,
}

impl Connection {
    /// Send one frame and return the matching response.
    ///
    /// Steps over server-initiated notices, recording them. A [`Frame::Fault`]
    /// becomes [`Error::Fault`] here so that no call site has to remember to check.
    fn request(&mut self, frame: Frame) -> Result<Frame> {
        if self.stale {
            return Err(Error::Stale);
        }
        let bytes = frame.encode().map_err(Error::Codec)?;
        if let Err(e) = self.socket.write_all(&bytes) {
            if is_disconnect(&e) {
                self.stale = true;
            }
            return Err(Error::Io(e));
        }
        loop {
            let frame = match read_frame(&mut self.socket, &mut self.buf) {
                Ok(Some(frame)) => frame,
                Ok(None) => {
                    self.stale = true;
                    return Err(Error::Io(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "yesnod closed the plugin channel",
                    )));
                }
                Err(e) => {
                    if is_disconnect(&e) {
                        self.stale = true;
                    }
                    return Err(Error::Io(e));
                }
            };
            match frame {
                // Terminal, and the only notice that is: the protocol says every
                // handle the peer holds is stale and every answer derived from one
                // describes a database that no longer exists.
                Frame::GenerationChanged { .. } => {
                    self.stale = true;
                    return Err(Error::Stale);
                }
                // **Transient, not terminal.** The slot is empty while a follower
                // rebootstraps and the server answers the request itself with
                // `Fault( Unavailable )`, which `classify` makes retryable. Keep
                // reading so that response is consumed and the stream stays paired.
                Frame::Unavailable => self.notices.push(Notice::Unavailable),
                // Good news only if it names the generation we greeted. A different
                // one means the database was replaced while we were not looking,
                // which is `GenerationChanged` by another route.
                Frame::Available { generation } => {
                    if generation != self.generation {
                        self.stale = true;
                        return Err(Error::Stale);
                    }
                    self.notices.push(Notice::Available { generation });
                }
                // Not terminal: leadership moved, reads remain valid, and a write
                // will find out for itself with `WrongRole`.
                Frame::RoleChanged { from, to } => {
                    self.notices.push(Notice::RoleChanged { from, to });
                }
                Frame::Fault { status, message } => return Err(self.classify(status, message)),
                other => return Ok(other),
            }
        }
    }

    /// Turn a [`Frame::Fault`] status into the right error.
    ///
    /// **This classification is why the client belongs in this crate.** The status
    /// table is [`crate::abi::Status`], declared here, yet until now the only code
    /// that interpreted it lived in a consumer -- so every future consumer would have
    /// had to rediscover that 4 and 5 are fatal, 3 is worth retrying and 6 means the
    /// caller reached a replica. A status table without an interpreter beside it is
    /// an invitation to N inconsistent interpreters.
    fn classify(&mut self, status: u32, message: String) -> Error {
        use crate::abi::Status;
        if status == Status::SnapshotTooOld as u32 || status == Status::GenerationChanged as u32 {
            self.stale = true;
            return Error::Stale;
        }
        if status == Status::Unavailable as u32 {
            return Error::Unavailable { message };
        }
        if status == Status::WrongRole as u32 {
            return Error::WrongRole;
        }
        Error::Fault { status, message }
    }
}

/// Map a connection error to expiry when a version is pinned.
fn under_version<T>(version: u64, result: Result<T>) -> Result<T> {
    match result {
        Err(Error::Io(e)) if is_disconnect(&e) => Err(Error::SnapshotExpired { version }),
        Err(Error::Stale) => Err(Error::SnapshotExpired { version }),
        other => other,
    }
}

/// Receive the server's greeting byte, and its arena descriptor if it sent one.
///
/// A one-byte `iovec`, so nothing beyond the byte is consumed from the socket.
/// Tolerating an absent descriptor is the whole reason this is not
/// [`crate::channel::recv_fd`], which requires one and is right to for its own
/// caller: here, no descriptor is how inline mode announces itself.
fn greet(socket: &UnixStream) -> io::Result<(Option<OwnedFd>, u8)> {
    use std::os::fd::{AsRawFd, FromRawFd};

    let mut byte = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr() as *mut libc::c_void,
        iov_len: 1,
    };
    const SPACE: usize = 32;
    let mut control = [0u64; SPACE / 8];
    // SAFETY: `msghdr` is plain data with no invalid bit patterns; every field used
    // below is assigned before the call.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = SPACE as _;

    let n = loop {
        // SAFETY: `msg` describes one writable byte and a control buffer of SPACE
        // aligned bytes, both live for this call. MSG_CMSG_CLOEXEC is what stops a
        // received descriptor leaking across an exec.
        let n = unsafe { libc::recvmsg(socket.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC) };
        if n >= 0 {
            break n;
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    };
    if n == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "yesnod closed before its channel greeting",
        ));
    }
    // SAFETY: `msg` was filled by the `recvmsg` above.
    let cmsg = unsafe { libc::CMSG_FIRSTHDR(&msg) };
    if cmsg.is_null() {
        return Ok((None, byte[0]));
    }
    // SAFETY: non-null and produced by `recvmsg` for this control buffer.
    unsafe {
        if (*cmsg).cmsg_level != libc::SOL_SOCKET || (*cmsg).cmsg_type != libc::SCM_RIGHTS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the greeting's ancillary message was not SCM_RIGHTS",
            ));
        }
        // One `SCM_RIGHTS` can carry *several* descriptors, so the count has to come
        // from `cmsg_len` rather than be assumed. **Take ownership of all of them
        // before rejecting anything**: the kernel has already installed every one of
        // them in this process, so returning early without owning them leaks a
        // descriptor on a path that a misbehaving server controls.
        let payload = (*cmsg).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
        let count = payload / std::mem::size_of::<libc::c_int>();
        let mut received: Vec<OwnedFd> = Vec::with_capacity(count);
        for i in 0..count {
            let mut raw: libc::c_int = -1;
            std::ptr::copy_nonoverlapping(
                (libc::CMSG_DATA(cmsg) as *const libc::c_int).add(i),
                &mut raw,
                1,
            );
            received.push(OwnedFd::from_raw_fd(raw));
        }
        // Any further ancillary message is also a protocol violation, and anything it
        // carried is already owned above only if it shared this header -- so check it
        // after taking what we can, not before.
        let extra = !libc::CMSG_NXTHDR(&msg, cmsg).is_null();
        if count != 1 || extra {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("yesnod's greeting carried {count} descriptors, expected exactly one"),
            ));
        }
        Ok((received.pop(), byte[0]))
    }
}

/// A connected channel client.
pub struct Client {
    conn: Arc<Mutex<Connection>>,
    arena: Option<Arc<memmap2::Mmap>>,
    limits: Limits,
}

impl Client {
    /// Connect, complete the handshake, and map the arena if the server sent one.
    ///
    /// `name` is what the server logs this peer as.
    pub fn connect(path: impl AsRef<Path>, name: &str) -> Result<Self> {
        let socket = UnixStream::connect(path.as_ref())?;
        let (fd, byte) = greet(&socket)?;

        // In arena mode the byte is the protocol version and the frame follows it.
        // In inline mode the byte is already the frame's first byte, so it is pushed
        // back for the decoder -- which is why the `iovec` above is one byte and not
        // a buffer: exactly one byte ever needs pushing back.
        let mut buf = Vec::with_capacity(ipc::HEADER_LEN);
        match &fd {
            Some(_) if byte != ipc::VERSION => {
                return protocol(format!(
                    "arena greeting announced version {byte}, this build speaks {}",
                    ipc::VERSION
                ));
            }
            Some(_) => {}
            None => buf.push(byte),
        }

        let mut conn = Connection {
            socket,
            buf,
            generation: 0,
            stale: false,
            notices: Vec::new(),
        };

        let greeting = match read_frame(&mut conn.socket, &mut conn.buf)? {
            Some(frame) => frame,
            None => {
                return protocol("yesnod closed before sending ServerHello");
            }
        };
        let Frame::ServerHello {
            protocol: version,
            generation,
            role,
            shards,
            arena_bytes,
            max_lanes,
            max_handles,
            max_blocks,
            max_writes,
        } = greeting
        else {
            return protocol("the first frame from yesnod was not ServerHello");
        };
        if version != u32::from(ipc::VERSION) {
            return protocol(format!(
                "yesnod speaks channel version {version}, this build speaks {}",
                ipc::VERSION
            ));
        }
        if max_lanes == 0
            || max_handles == 0
            || max_blocks == 0
            || max_writes == 0
            || max_lanes as usize > ipc::MAX_LANES
            || max_writes as usize > ipc::MAX_WRITES
        {
            return protocol("yesnod advertised an unusable channel limit");
        }

        // The descriptor and the advertised size must agree, and each direction is
        // its own fault rather than something to paper over.
        let arena = match (fd, arena_bytes) {
            (Some(_), 0) => {
                return protocol("yesnod sent an arena descriptor but advertised inline mode");
            }
            (None, n) if n > 0 => {
                return protocol("yesnod advertised an arena but sent no descriptor");
            }
            (None, _) => None,
            (Some(fd), n) => {
                let file = std::fs::File::from(fd);
                if file.metadata()?.len() < n {
                    return protocol("yesnod's arena descriptor is shorter than advertised");
                }
                let len = usize::try_from(n).map_err(|_| {
                    Error::Protocol("yesnod's arena exceeds this address space".into())
                })?;
                // SAFETY: the descriptor is live and sized to at least `len`, checked
                // above; the server seals it against shrink before transfer. The
                // mapping is read-only, and a lane's bytes are consumed before this
                // client advances the handle that owns them.
                let map = unsafe { memmap2::MmapOptions::new().len(len).map(&file)? };
                Some(Arc::new(map))
            }
        };

        // Set before the first `request`, because that loop compares an incoming
        // `Available { generation }` against it.
        conn.generation = generation;

        // Only now may the client speak, and `ClientHello` is answered with `Done`.
        match conn.request(Frame::ClientHello {
            protocol: version,
            name: name.to_owned(),
        })? {
            Frame::Done => {}
            _ => return protocol("yesnod did not accept the channel hello"),
        }

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            arena,
            limits: Limits {
                protocol: version,
                generation,
                role,
                shards,
                arena_bytes,
                max_lanes,
                max_handles,
                max_blocks,
                max_writes,
            },
        })
    }

    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// Is this connection serving blocks through the shared arena, or inline?
    pub fn is_arena(&self) -> bool {
        self.arena.is_some()
    }

    /// Server-initiated notices seen so far, oldest first, drained from the client.
    pub fn take_notices(&self) -> Result<Vec<Notice>> {
        let mut conn = self.locked()?;
        Ok(std::mem::take(&mut conn.notices))
    }

    fn locked(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|_| Error::Protocol("the plugin channel lock was poisoned".into()))
    }

    /// Apply writes, in one commit. Returns the committed version and the number of
    /// ordinals actually changed.
    ///
    /// **Not a method on [`Snapshot`], because [`Frame::Apply`] carries no snapshot
    /// id.** A write is connection-scoped and a pinned snapshot is a *read* view, so
    /// hanging this off the snapshot would have implied an isolation relationship the
    /// wire does not have -- and a caller could reasonably have expected the write to
    /// be visible to that snapshot afterwards, which it is not.
    ///
    /// The length bound comes from the server's advertised `max_writes`, not from
    /// [`ipc::MAX_WRITES`]. Those two have already disagreed once in this protocol's
    /// life: a consumer trusted the published constant, the server's configured limit
    /// was lower, and the frame was refused by the consumer's own encoder before
    /// anything reached the socket.
    pub fn apply(&self, writes: Vec<Write>) -> Result<(u64, u64)> {
        if writes.len() > self.limits.max_writes as usize {
            return protocol(format!(
                "{} writes exceeds the server's advertised limit of {}",
                writes.len(),
                self.limits.max_writes
            ));
        }
        match self.locked()?.request(Frame::Apply { writes })? {
            Frame::Committed { version, changed } => Ok((version, changed)),
            _ => protocol("yesnod did not answer Apply with Committed"),
        }
    }

    /// Pin a snapshot. The version it names is held for as long as the handle lives.
    pub fn snapshot(&self) -> Result<Snapshot> {
        let frame = self.locked()?.request(Frame::SnapshotOpen)?;
        let Frame::SnapshotOpened { snapshot, version } = frame else {
            return protocol("yesnod did not answer SnapshotOpen with SnapshotOpened");
        };
        Ok(Snapshot {
            conn: Arc::clone(&self.conn),
            arena: self.arena.clone(),
            id: snapshot,
            version,
            max_lanes: self.limits.max_lanes,
            max_blocks: self.limits.max_blocks,
        })
    }
}

/// One pinned version, released when this handle drops.
pub struct Snapshot {
    conn: Arc<Mutex<Connection>>,
    arena: Option<Arc<memmap2::Mmap>>,
    id: u64,
    version: u64,
    max_lanes: u32,
    max_blocks: u32,
}

impl Snapshot {
    pub fn version(&self) -> u64 {
        self.version
    }

    fn request(&self, frame: Frame) -> Result<Frame> {
        let result = self
            .conn
            .lock()
            .map_err(|_| Error::Protocol("the plugin channel lock was poisoned".into()))
            .and_then(|mut conn| conn.request(frame));
        under_version(self.version, result)
    }

    pub fn contains(&self, key: u64, ordinal: u64) -> Result<bool> {
        match self.request(Frame::SnapshotContains {
            snapshot: self.id,
            key,
            ordinal,
        })? {
            Frame::Bool { value } => Ok(value != 0),
            _ => protocol("yesnod did not answer SnapshotContains with Bool"),
        }
    }

    pub fn cardinality(&self, key: u64) -> Result<u64> {
        match self.request(Frame::SnapshotCardinality {
            snapshot: self.id,
            key,
        })? {
            Frame::Count { value } => Ok(value),
            _ => protocol("yesnod did not answer SnapshotCardinality with Count"),
        }
    }

    pub fn max(&self, key: u64) -> Result<Option<u64>> {
        match self.request(Frame::SnapshotMax {
            snapshot: self.id,
            key,
        })? {
            Frame::Ordinal { present, value } => Ok((present != 0).then_some(value)),
            _ => protocol("yesnod did not answer SnapshotMax with Ordinal"),
        }
    }

    /// Populated keys in `lo..=hi`, at most `limit`. The flag is `true` when more
    /// remain past the last one returned.
    pub fn key_range(&self, lo: u64, hi: u64, limit: u32) -> Result<(Vec<u64>, bool)> {
        match self.request(Frame::SnapshotKeyRange {
            snapshot: self.id,
            lo,
            hi,
            limit,
        })? {
            Frame::Keys { values, more } => Ok((values, more != 0)),
            _ => protocol("yesnod did not answer SnapshotKeyRange with Keys"),
        }
    }

    /// Acquire a lane handle over `keys` and read their blocks in order.
    pub fn lanes(&self, keys: Vec<u64>) -> Result<LaneCursor> {
        let frame = self.request(Frame::LanesAcquire {
            snapshot: self.id,
            keys,
        })?;
        let Frame::LanesAcquired { lanes, arena_off } = frame else {
            return protocol("yesnod did not answer LanesAcquire with LanesAcquired");
        };
        Ok(LaneCursor {
            conn: Arc::clone(&self.conn),
            arena: self.arena.clone(),
            version: self.version,
            handle: lanes,
            arena_off: arena_off as usize,
            max_lanes: self.max_lanes as usize,
            max_blocks: self.max_blocks,
            blocks: Vec::new(),
            inline: Vec::new(),
            inline_offsets: Vec::new(),
            at: 0,
            done: false,
        })
    }
}

impl Drop for Snapshot {
    /// Release the pinned version. Errors are dropped on purpose: this runs on every
    /// path including an early return, and there is nothing a caller could do with a
    /// failure to close something it is finished with.
    fn drop(&mut self) {
        if let Ok(mut conn) = self.conn.lock() {
            let _ = conn.request(Frame::SnapshotClose { snapshot: self.id });
        }
    }
}

/// An ordered walk over the blocks of an acquired lane set.
///
/// Deliberately a *cursor* rather than an iterator yielding a borrowing block. Both
/// would work in Rust, but the whole point of this type is that the C ABI restates
/// it, and `advance` / `prefix` / `lane` map onto C functions one-for-one where a
/// borrowing return type does not. Arena mode stays zero-copy either way: [`lane`]
/// returns a slice of the mapping, valid until the next [`advance`].
///
/// [`lane`]: LaneCursor::lane
/// [`advance`]: LaneCursor::advance
pub struct LaneCursor {
    conn: Arc<Mutex<Connection>>,
    arena: Option<Arc<memmap2::Mmap>>,
    version: u64,
    handle: u64,
    arena_off: usize,
    max_lanes: usize,
    max_blocks: u32,
    blocks: Vec<Block>,
    inline: Vec<u8>,
    inline_offsets: Vec<Vec<usize>>,
    at: usize,
    done: bool,
}

impl LaneCursor {
    /// Step to the next block, fetching a batch when the current one is exhausted.
    /// `false` means the walk is over.
    pub fn advance(&mut self) -> Result<bool> {
        if self.at + 1 < self.blocks.len() {
            self.at += 1;
            return Ok(true);
        }
        if self.done {
            return Ok(false);
        }
        self.fetch()?;
        Ok(!self.blocks.is_empty())
    }

    /// One round trip per batch of at most `max_blocks`; an empty batch ends the walk.
    fn fetch(&mut self) -> Result<()> {
        let result = self
            .conn
            .lock()
            .map_err(|_| Error::Protocol("the plugin channel lock was poisoned".into()))
            .and_then(|mut conn| {
                conn.request(Frame::BlockAdvanceMany {
                    lanes: self.handle,
                    max_blocks: self.max_blocks,
                })
            });
        let response = under_version(self.version, result)?;

        // The response form must match the connection's mode. Either crossed is the
        // server contradicting its own greeting, not something to adapt to.
        let (blocks, payload) = match response {
            Frame::Blocks { blocks } if self.arena.is_some() => (blocks, Vec::new()),
            Frame::BlocksInline { blocks, payload } if self.arena.is_none() => (blocks, payload),
            Frame::Blocks { .. } | Frame::BlocksInline { .. } => {
                return protocol("yesnod's block response contradicts its arena mode");
            }
            _ => return protocol("yesnod did not answer BlockAdvanceMany with blocks"),
        };

        // Validate the server's output. A client that trusts its server cannot tell
        // a yesnod bug from its own.
        if blocks.iter().any(|b| b.lanes.len() > self.max_lanes) {
            return protocol("a block carries more lanes than the server advertised");
        }
        if blocks.windows(2).any(|w| w[0].prefix >= w[1].prefix) {
            return protocol("blocks are not strictly ascending by prefix");
        }
        for block in &blocks {
            for lane in &block.lanes {
                if lane.kind.payload_bytes(lane.count) > ipc::LANE_BYTES {
                    return protocol("a lane exceeds its fixed payload slot");
                }
            }
        }

        // Inline payloads are packed and must account for every byte; arena payloads
        // are a fixed stride and need no offset table.
        self.inline_offsets.clear();
        if self.arena.is_none() {
            let mut offset = 0usize;
            for block in &blocks {
                let mut per_lane = Vec::with_capacity(block.lanes.len());
                for lane in &block.lanes {
                    per_lane.push(offset);
                    offset += lane.kind.payload_bytes(lane.count);
                }
                self.inline_offsets.push(per_lane);
            }
            if offset != payload.len() {
                return protocol("the inline payload does not match its lane descriptors");
            }
        }

        // **Fewer blocks than asked for means the scan ended** -- the frame's own
        // documented convention, and `is_empty()` is not the same test: a batch that
        // comes back short is final, so stopping only on an empty batch costs one
        // wasted round trip at the end of every handle. Exactly as many may or may
        // not have more, which is why that case asks again.
        self.done = blocks.len() < self.max_blocks as usize;
        self.blocks = blocks;
        self.inline = payload;
        self.at = 0;
        Ok(())
    }

    /// The current block's key prefix. `None` before the first [`advance`].
    ///
    /// [`advance`]: LaneCursor::advance
    pub fn prefix(&self) -> Option<u64> {
        self.blocks.get(self.at).map(|b| b.prefix)
    }

    pub fn lane_count(&self) -> usize {
        self.blocks.get(self.at).map_or(0, |b| b.lanes.len())
    }

    /// The `index`th lane of the current block, with its payload.
    ///
    /// In arena mode the slice points into the shared mapping and is valid until the
    /// next [`advance`]. That borrow is the reason this module is not in `yesno-c`,
    /// whose cursor deliberately materializes to avoid exactly it.
    ///
    /// [`advance`]: LaneCursor::advance
    pub fn lane(&self, index: usize) -> Result<(Lane, &[u8])> {
        let block = match self.blocks.get(self.at) {
            Some(block) => block,
            None => return protocol("the lane cursor has no current block"),
        };
        let lane = match block.lanes.get(index) {
            Some(lane) => *lane,
            None => return protocol("lane index is past the current block"),
        };
        let len = lane.kind.payload_bytes(lane.count);
        let bytes = match &self.arena {
            Some(arena) => {
                // The shared helper, not the arithmetic inline. `ipc.rs` says why:
                // "the two sides getting it separately right is the failure this
                // layout exists to prevent." Recomputing it here would be a second
                // place for the stride to drift, which is the whole reason this
                // client exists rather than a third hand-rolled peer.
                let offset =
                    ipc::batched_lane_offset(self.arena_off as u64, self.max_lanes, self.at, index)
                        as usize;
                arena.get(offset..offset + len).ok_or_else(|| {
                    Error::Protocol("an arena lane lies outside the mapping".into())
                })?
            }
            None => {
                let offset = self.inline_offsets[self.at][index];
                self.inline.get(offset..offset + len).ok_or_else(|| {
                    Error::Protocol("an inline lane lies outside the frame".into())
                })?
            }
        };
        Ok((lane, bytes))
    }
}

impl Drop for LaneCursor {
    /// Release the handle. Errors dropped, for the reason [`Snapshot::drop`] gives.
    fn drop(&mut self) {
        if let Ok(mut conn) = self.conn.lock() {
            let _ = conn.request(Frame::LanesRelease { lanes: self.handle });
        }
    }
}
