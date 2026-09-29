//! The plugin channel: a Unix socket yesnod serves out-of-process peers on.
//!
//! The protocol and the session live in `yesno-plugin`, which has no server and
//! is testable without one. What is here is only the part that needs a server:
//! the slot, the role, the listener's lifetime, and the notifications a
//! rebootstrap has to push.
//!
//! # There is no drain contract, and that is the point
//!
//! This module used to open with one, because the in-process facility needed it:
//! a plugin's lease was a `Snapshot`, a `Snapshot` holds `Arc<DbInner>` which
//! holds the directory lock, and **no host-side call takes a lease back** --
//! `Db::evict_oldest_reader` releases the reclamation floor without releasing the
//! lock, which is worse in both directions. So a plugin caching handles between
//! requests, the obvious optimization, would hold the lock indefinitely and
//! surface as an unattributable `AlreadyOpen` seconds later. The facility had to
//! ask the plugin to drain, then verify, then name it in the failure.
//!
//! A peer's snapshots belong to its **connection**. Closing the socket releases
//! them -- peer exit, crash, container stop, `SIGKILL`, or this side dropping the
//! listener -- so the host takes them back by closing, needs no cooperation, and
//! has nothing to verify. The in-process facility was removed on 2026-09-29 and
//! this paragraph is what replaced it.

use std::sync::Arc;

use yesno_plugin::abi::Role;
use yesno_plugin::channel::{Arena, Limits, Session};
use yesno_plugin::ipc::Frame;
use yesno_plugin::Host;

use crate::config::Config;

/// Bind the channel socket, refusing to clobber anything live.
///
/// # Why the old one-liner was wrong
///
/// This used to be an unconditional `remove_file` before `bind`, justified by "the
/// database lock already proves no other yesnod holds this directory". **That
/// argument does not cover the case that matters**: two instances with *different*
/// data directories and the same configured socket path. Each holds its own
/// directory lock, so neither is refused, and the second silently unlinks the
/// first's live socket and binds its own. New peers then reach a different
/// database while the first instance's existing peers carry on against the old
/// one -- a split nobody observes, because both sides look healthy.
///
/// `control.rs::bind_unix` already had this right, and this is the same sequence:
/// refuse a path that exists and is not a socket, probe it, refuse outright if
/// something is *already accepting*, and unlink only when the connection is
/// refused, which is what a socket left by a dead process does.
fn bind_guarded(path: &std::path::Path) -> std::io::Result<std::os::unix::net::UnixListener> {
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        use std::os::unix::fs::FileTypeExt as _;
        if !metadata.file_type().is_socket() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!(
                    "plugin channel socket path '{}' exists and is not a socket",
                    path.display()
                ),
            ));
        }
        match std::os::unix::net::UnixStream::connect(path) {
            Ok(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AddrInUse,
                    format!(
                        "plugin channel socket '{}' is already accepting connections; \
                         another yesnod is serving it",
                        path.display()
                    ),
                ));
            }
            // Nothing is listening, so the file is a leftover from a dead process.
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                std::fs::remove_file(path)?;
            }
            Err(e) => return Err(e),
        }
    }
    std::os::unix::net::UnixListener::bind(path)
}

/// Who may connect to the channel.
///
/// # `ClientHello.name` is a label, not a credential
///
/// The peer chooses it. So without this, every process the socket's permissions
/// admit could open a snapshot and read the whole database -- which on a shared or
/// group-writable mount is a wider set than the deployment intends. `SO_PEERCRED`
/// is the credential that a peer cannot choose: the kernel fills it in at connect
/// time from the peer's real identity.
///
/// The filesystem mode is the first gate and this is the second. Neither replaces
/// the other: a mode cannot express "this uid and no other in the group", and a
/// credential check cannot stop a peer that never gets to connect.
#[derive(Clone)]
struct AccessPolicy {
    /// The daemon's own uid, always allowed.
    own: u32,
    /// Extra uids from the configuration.
    allowed: Vec<u32>,
}

impl AccessPolicy {
    fn from_config(cfg: &Config) -> AccessPolicy {
        AccessPolicy {
            // SAFETY: `geteuid` reads process state and cannot fail.
            own: unsafe { libc_geteuid() },
            allowed: cfg.plugin.channel_allow_uids.clone(),
        }
    }

    /// Whether this connection's peer may be served.
    ///
    /// A peer whose credentials cannot be read is **refused**: the call fails only
    /// if the socket is not a connected Unix socket, so an error here means the
    /// assumption behind the whole check is untrue, and the safe reading of "I
    /// cannot tell who you are" on an authentication gate is no.
    fn admits(&self, stream: &std::os::unix::net::UnixStream) -> bool {
        let Some(uid) = peer_uid(stream) else {
            tracing::warn!("refusing a plugin channel peer: cannot read its credentials");
            return false;
        };
        if self.permits(uid) {
            return true;
        }
        tracing::warn!(
            peer_uid = uid,
            "refusing a plugin channel peer: its uid is not permitted"
        );
        false
    }

    /// The decision alone, separated so it can be tested without a second user.
    ///
    /// Root is allowed unconditionally: it can read the data directory directly, so
    /// refusing it protects nothing and breaks an administrator's diagnostic.
    fn permits(&self, uid: u32) -> bool {
        uid == self.own || uid == 0 || self.allowed.contains(&uid)
    }
}

/// The connected peer's user id, from `SO_PEERCRED`.
fn peer_uid(stream: &std::os::unix::net::UnixStream) -> Option<u32> {
    use std::os::fd::AsRawFd as _;
    let mut cred = Ucred {
        pid: 0,
        uid: u32::MAX,
        gid: u32::MAX,
    };
    let mut len = std::mem::size_of::<Ucred>() as u32;
    // SAFETY: `cred` is a live `SO_PEERCRED` payload of exactly `len` bytes, and
    // `stream` outlives the call. The kernel writes the struct and updates `len`.
    let rc = unsafe {
        libc_getsockopt(
            stream.as_raw_fd(),
            SOL_SOCKET,
            SO_PEERCRED,
            &mut cred as *mut Ucred as *mut std::ffi::c_void,
            &mut len,
        )
    };
    (rc == 0 && len as usize == std::mem::size_of::<Ucred>()).then_some(cred.uid)
}

/// `struct ucred`: three 32-bit fields, no padding, on every Linux ABI.
#[repr(C)]
struct Ucred {
    pid: i32,
    uid: u32,
    gid: u32,
}

// Declared rather than taking `libc`, as `yesno-core` does for `kill` and
// `fallocate`. `yesno-server` is not under the lean-core budget, but two
// three-argument declarations beside the code that reads them are clearer than a
// dependency added for them.
const SOL_SOCKET: i32 = 1;
const SO_PEERCRED: i32 = 17;
const SHUT_RDWR: i32 = 2;

extern "C" {
    #[link_name = "geteuid"]
    fn libc_geteuid() -> u32;
    #[link_name = "shutdown"]
    fn libc_shutdown(fd: i32, how: i32) -> i32;
    #[link_name = "getsockopt"]
    fn libc_getsockopt(
        fd: i32,
        level: i32,
        name: i32,
        value: *mut std::ffi::c_void,
        len: *mut u32,
    ) -> i32;
}

/// One admitted connection, counted for as long as this lives.
///
/// # Why a counter and not `peers.len()`
///
/// Admission used to read `peers.len()` and spawn, but a connection registers
/// itself in `peers` only after its arena is built and its descriptor sent -- so
/// several connects arriving together all saw room, all spawned, and all allocated
/// an arena before any of them appeared in the list. The cap held only when peers
/// arrived one at a time, which is exactly how a test that waits for each to
/// register would drive it.
///
/// The count is incremented **before** the thread is spawned and decremented when
/// it exits, whatever path it takes out, which is the same RAII argument the
/// removed lease guard used: increment-then-build leaves the count permanently
/// high if anything in between fails, and build-then-increment lets the failure
/// decrement a count that was never incremented.
struct AdmissionSlot(Arc<std::sync::atomic::AtomicUsize>);

impl Drop for AdmissionSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

/// Take a slot if one is free. `None` means the channel is full.
///
/// A compare-and-swap rather than load-then-store: two accept loops do not exist
/// today, but a check and a separate increment is the same shape as the bug this
/// replaces, and the loop costs nothing on an uncontended counter.
fn admit(live: &Arc<std::sync::atomic::AtomicUsize>, max: usize) -> Option<AdmissionSlot> {
    let mut seen = live.load(std::sync::atomic::Ordering::Acquire);
    loop {
        if seen >= max {
            return None;
        }
        match live.compare_exchange_weak(
            seen,
            seen + 1,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
        ) {
            Ok(_) => return Some(AdmissionSlot(live.clone())),
            Err(actual) => seen = actual,
        }
    }
}

/// One connected peer: a writer other threads serialise on, and a hangup that
/// never waits for it.
///
/// **The split is the point.** Frame writes must be mutually exclusive or a
/// notification interleaves inside a response, so the writer is behind a mutex.
/// But `disconnect_peers` used to take that same mutex to call `shutdown`, and a
/// peer that stops reading blocks its serving thread inside `write_all` while
/// holding it -- so the disconnect that exists to reclaim the database waited on
/// the very peer it was trying to hang up. `shutdown` takes `&self`, so a second
/// descriptor for the same socket needs no lock and cannot be blocked: it
/// interrupts the stalled write rather than queueing behind it.
pub(crate) struct PeerHandle {
    writer: std::sync::Mutex<std::os::unix::net::UnixStream>,
    hangup: std::os::unix::net::UnixStream,
}

impl PeerHandle {
    fn hang_up(&self) {
        let _ = self.hangup.shutdown(std::net::Shutdown::Both);
    }
}

/// How long a notification write may block before its peer is disconnected.
///
/// Short on purpose. A notification is tens of bytes, so a peer that cannot accept
/// one inside this window is not merely busy -- it has stopped reading, and the
/// frames behind it would queue without bound.
const NOTIFY_WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// How long **any** write to a peer may block before the connection is abandoned.
///
/// Set once on the socket at accept, so it covers ordinary responses and not only
/// notifications. Bounding notifications alone left the hole this closes: a peer
/// that stops reading stalls its serving thread inside a response `write_all`,
/// and everything that wants that peer's writer queues behind it.
///
/// Longer than the notification bound because a response can be a megabyte of
/// inline payload to a peer that is merely slow, and disconnecting that peer would
/// be wrong.
const PEER_WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// The out-of-process channel: a Unix socket yesnod serves peers on.
///
/// # What a connection owns, and why that is the whole lifecycle
///
/// One connection gets one arena, one [`Session`], and one serving thread. The
/// session owns every snapshot the peer opened, so **closing the socket releases
/// them** -- peer exit, crash, container stop, `SIGKILL`, or this side dropping the
/// listener. Nothing polls, nothing times out, and no pid is involved, which is what
/// makes the reclamation namespace-independent and why the in-process facility's
/// drain contract has no counterpart here.
///
/// # Blocking threads rather than a reactor
///
/// Every request touches the engine, which faults mmap pages and may read from
/// disk. That is exactly the reason `do_get` uses `spawn_blocking`, and a reactor
/// thread serving one would stall every other connection on it.
pub struct Channel {
    path: std::path::PathBuf,
    /// Write halves of live connections, for pushing notifications.
    ///
    /// A peer cannot ask whether its database went away -- it would have to poll --
    /// so availability and generation changes are pushed. The mutex is per
    /// connection and held only for one frame, and frames are the unit of the
    /// protocol, so a notification can interleave with responses but never inside
    /// one.
    peers: Arc<std::sync::Mutex<Vec<Arc<PeerHandle>>>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    /// Behind a mutex so [`Channel::stop`] takes `&self`.
    ///
    /// Both startup paths hold the channel in an `Arc` -- the follower's replication
    /// task needs it to outlive the call, and the leader's `Running` needs it to
    /// stop at teardown -- so a `stop( self )` would have forced one of them to be
    /// different from the other for no reason but this field.
    listener: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
    /// Connections past accept, whether or not they have registered yet.
    admitted: Arc<std::sync::atomic::AtomicUsize>,
    /// A second descriptor for the listening socket, used only to wake `accept`.
    ///
    /// **This is what removes the pathname from teardown.** `stop` used to connect
    /// to its own configured path so the blocking `accept` would return -- which
    /// trusts the directory entry to still be ours. If anything else can write the
    /// parent directory and swaps the path, that connect wakes a **different**
    /// listener, this one never returns, and the join hangs. Shutting our own
    /// descriptor cannot be redirected: it names the socket, not a name.
    wake: std::os::unix::net::UnixListener,
    /// The generation counter a peer reads in the greeting.
    ///
    /// **Held here because the in-process facility used to own it.** When a
    /// rebootstrap replaced the database, the facility bumped the generation and
    /// the channel reported whatever the facility returned -- so with the
    /// facility gone the channel would have notified `Available { generation: 0 }`
    /// for ever and never `GenerationChanged`, and every peer would have gone on
    /// using handles that name a database that no longer exists. Deleting the
    /// facility without moving this is the quiet half of that removal.
    ///
    /// Sessions hold clones of this `Host` and the counter is an `Arc<AtomicU64>`,
    /// so a bump here is what they read.
    host: Host,
}

impl Channel {
    /// Bind the configured socket and start accepting, or `None` when unconfigured.
    pub fn start(cfg: &Config, host: Host) -> std::io::Result<Option<Channel>> {
        if !cfg.plugin.channel_enabled() {
            return Ok(None);
        }
        let path = std::path::PathBuf::from(cfg.plugin.channel_socket.trim());
        let listener = bind_guarded(&path)?;
        if let Some(mode) = cfg.plugin.socket_mode() {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))?;
        }
        let policy = AccessPolicy::from_config(cfg);
        let max_peers = cfg.plugin.channel_max_peers.max(1);
        // Counted at admission rather than derived from `peers`, which a
        // connection joins only once it is fully set up; see `AdmissionSlot`.
        let admitted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let accept_admitted = admitted.clone();

        let limits = Limits {
            max_handles: cfg.plugin.channel_max_handles.max(1),
            max_lanes: cfg
                .plugin
                .channel_max_lanes
                .clamp(1, yesno_plugin::ipc::MAX_LANES),
            max_blocks: cfg
                .plugin
                .channel_max_blocks
                .clamp(1, yesno_plugin::ipc::MAX_BATCH),
            max_snapshots: cfg.plugin.channel_max_snapshots.max(1),
        };
        let peers: Arc<std::sync::Mutex<Vec<Arc<PeerHandle>>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let inline = cfg.plugin.channel_inline;
        let accept_peers = peers.clone();
        let accept_stop = stop.clone();
        // The accept loop takes the `Host`; the channel keeps a clone so it can
        // bump the generation on a rebootstrap. Both share one `Arc<AtomicU64>`.
        let kept = host.clone();
        let wake = listener.try_clone()?;
        let handle = std::thread::spawn(move || {
            for incoming in listener.incoming() {
                if accept_stop.load(std::sync::atomic::Ordering::Acquire) {
                    break;
                }
                let stream = match incoming {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!(error = %e, "plugin channel accept failed");
                        continue;
                    }
                };
                // **Refused rather than queued, and before anything is allocated.**
                // The arena and the thread are per connection, so admission has to
                // happen here; deciding later would mean paying for the peer in
                // order to turn it away.
                if !policy.admits(&stream) {
                    continue;
                }
                let Some(slot) = admit(&accept_admitted, max_peers) else {
                    tracing::warn!(
                        max_peers,
                        "refusing a plugin channel peer: the connection limit is reached"
                    );
                    // Closed at once so the peer sees a refusal now, instead of a
                    // connection that appears to succeed and never answers.
                    drop(stream);
                    continue;
                };
                let host = host.clone();
                let peers = accept_peers.clone();
                std::thread::spawn(move || serve_one(stream, host, limits, inline, peers, slot));
            }
        });

        tracing::info!(
            socket = %path.display(),
            max_lanes = limits.max_lanes,
            max_blocks = limits.max_blocks,
            arena_bytes = limits.arena_bytes(),
            "plugin channel listening"
        );
        Ok(Some(Channel {
            path,
            peers,
            stop,
            listener: std::sync::Mutex::new(Some(handle)),
            host: kept,
            wake,
            admitted,
        }))
    }

    /// Record that the database was replaced, and return the new generation.
    ///
    /// The counter the peers' sessions read in their greeting; see the `host`
    /// field for why it lives on the channel now.
    pub fn bump_generation(&self) -> u64 {
        self.host.bump_generation()
    }

    /// Connections occupying an admission slot.
    ///
    /// **Not the same number as [`Channel::peers`], and the difference matters.**
    /// A connection takes a slot at accept and releases it when its thread exits,
    /// while it joins the peer registry only once its arena is built and leaves it
    /// just before the thread ends. So `admitted` is at least `peers`, and the cap
    /// is enforced on this one -- enforcing it on the registry was the bug, because
    /// several connections could be past accept and not yet registered.
    pub fn admitted(&self) -> usize {
        self.admitted.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Connections currently served.
    pub fn peers(&self) -> usize {
        self.peers.lock().map(|p| p.len()).unwrap_or(0)
    }

    /// Push a notification to every live peer, dropping those that have gone.
    ///
    /// Best effort by design: a peer that has died is exactly the case the channel
    /// handles by releasing its snapshots, so a failed write here is information
    /// rather than an error to propagate.
    pub fn notify(&self, frame: &Frame) {
        let bytes = match frame.encode() {
            Ok(b) => b,
            Err(e) => {
                tracing::error!(error = %e, "cannot encode a channel notification");
                return;
            }
        };
        // **The global lock is released before any write.** It used to be held
        // across `write_all` on every peer, and a Unix socket write blocks once the
        // receiver's buffer fills -- so one peer that stopped reading blocked this
        // call for ever, and `stop` blocked behind the same mutex. A single
        // uncooperative peer could therefore stall a rebootstrap *and* the daemon's
        // shutdown. Taking a snapshot of the list first means a slow peer delays
        // only itself.
        let Some(targets) = self.peer_snapshot() else {
            return;
        };
        let mut dead = Vec::new();
        for peer in targets {
            if !Self::write_frame(&peer, &bytes) {
                dead.push(peer);
            }
        }
        self.drop_peers(&dead);
    }

    /// The live peers, cloned so writes happen with the lock released.
    fn peer_snapshot(&self) -> Option<Vec<Arc<PeerHandle>>> {
        self.peers.lock().ok().map(|p| p.clone())
    }

    /// Write one whole frame, or report the peer unusable.
    ///
    /// Bounded by a write timeout, because an unbounded write is what item 2 of the
    /// 2026-09-29 security review was about. **A timeout disconnects the peer
    /// rather than retrying**, and that is not harshness: `write_all` that stops
    /// half way has already put part of a frame on the wire, and the protocol has
    /// no resynchronisation point, so the connection is unusable whatever happens
    /// next. Dropping it is the honest outcome, and a peer reconnects and is told
    /// the current generation in its greeting.
    fn write_frame(peer: &Arc<PeerHandle>, bytes: &[u8]) -> bool {
        let Ok(mut stream) = peer.writer.lock() else {
            return false;
        };
        // Tightened for a notification and restored afterwards: the connection
        // already carries `PEER_WRITE_TIMEOUT` from accept, which is sized for a
        // large response to a merely slow peer, and a notification is tens of bytes
        // and should not wait that long behind one.
        if stream
            .set_write_timeout(Some(NOTIFY_WRITE_TIMEOUT))
            .is_err()
        {
            return false;
        }
        let ok = std::io::Write::write_all(&mut *stream, bytes).is_ok();
        let _ = stream.set_write_timeout(Some(PEER_WRITE_TIMEOUT));
        ok
    }

    /// Forget these peers and shut their sockets, which ends their serving threads.
    fn drop_peers(&self, dead: &[Arc<PeerHandle>]) {
        if dead.is_empty() {
            return;
        }
        if let Ok(mut peers) = self.peers.lock() {
            peers.retain(|p| !dead.iter().any(|d| Arc::ptr_eq(d, p)));
        }
        for peer in dead {
            peer.hang_up();
        }
    }

    /// Disconnect every peer, keeping the listener accepting.
    ///
    /// # This is what makes a rebootstrap able to finish
    ///
    /// A peer's `Session` owns the snapshots it opened, and a `Snapshot` holds
    /// `Arc<DbInner>`, which holds the directory lock. Telling a peer the database
    /// is going away does **not** take those back -- it is a courtesy, and a peer
    /// that is slow, busy or uncooperative keeps the lock alive. The follower would
    /// then drop its `Arc<Db>`, try to reopen, and fail `AlreadyOpen` for as long as
    /// the peer felt like holding on, surfacing as an unattributable error seconds
    /// later in `open_if_needed`.
    ///
    /// That is the same hazard the removed in-process facility had a drain contract
    /// for, and the channel's advantage is that it needs no contract: shutting the
    /// socket ends the serving thread, which drops the `Session`, which drops the
    /// snapshots. **The host takes them back rather than asking.** The listener is
    /// deliberately left up, so a peer reconnects on its own and is told the new
    /// generation in its greeting.
    ///
    /// Found by a security review on 2026-09-29, which is also when the drain this
    /// replaces was deleted; the journal entry for that removal claimed closing
    /// already happened on every path, and it happened only at shutdown.
    pub fn disconnect_peers(&self) -> usize {
        let Ok(mut peers) = self.peers.lock() else {
            return 0;
        };
        let taken: Vec<_> = peers.drain(..).collect();
        drop(peers);
        for peer in &taken {
            // Never takes the writer lock; see `PeerHandle`. A peer stalled inside
            // a response write is exactly the one this must be able to hang up.
            peer.hang_up();
        }
        taken.len()
    }

    /// Stop accepting and close every connection.
    ///
    /// Closing is what releases the peers' snapshots, so this is also what lets the
    /// database be dropped afterwards. It runs before the reader wait in teardown
    /// for the same reason the in-process drain does.
    pub fn stop(&self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        // Shutting each peer's socket unblocks its serving thread out of `read`,
        // which is what a `stop` flag alone cannot do. Shared with
        // [`Channel::disconnect_peers`], which does exactly this for a rebootstrap;
        // the only difference is that this one also stops the listener.
        self.disconnect_peers();
        // Wake the blocking `accept` by shutting our own listening descriptor,
        // rather than connecting to the path; see the `wake` field.
        {
            use std::os::fd::AsRawFd as _;
            // SAFETY: `shutdown` takes a descriptor and an integer and writes no
            // memory. `self.wake` outlives the call.
            unsafe { libc_shutdown(self.wake.as_raw_fd(), SHUT_RDWR) };
        }
        let handle = self.listener.lock().ok().and_then(|mut g| g.take());
        if let Some(h) = handle {
            let _ = h.join();
        }
        // Removed only while it is still a socket. That does not make the path
        // safe -- a replacement swapped in by anything that can write the parent
        // directory would also be a socket -- and the real boundary is a private
        // parent, which `docs/operations/backup.md` states. It does stop the
        // ordinary mistake of deleting a regular file somebody put there.
        if std::fs::symlink_metadata(&self.path)
            .map(|m| {
                use std::os::unix::fs::FileTypeExt as _;
                m.file_type().is_socket()
            })
            .unwrap_or(false)
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Serve one connection until the peer goes away.
fn serve_one(
    stream: std::os::unix::net::UnixStream,
    host: Host,
    limits: Limits,
    inline: bool,
    peers: Arc<std::sync::Mutex<Vec<Arc<PeerHandle>>>>,
    // Held for the connection's life; dropping it frees the admission slot,
    // whichever way this function returns.
    _admission: AdmissionSlot,
) {
    // # Why this falls back rather than failing
    //
    // `Session::new_inline` exists so a host with no shared memory can still serve,
    // and until this function used it that was a property of the *protocol* and not
    // of the running server -- which is a distinction a deployment document would
    // have got wrong. `Arena::new` answers `Unsupported` off Linux, and can fail on
    // Linux too: `memfd_create` needs a descriptor and the region needs backing
    // memory, so fd exhaustion and `ENOSPC` reach here as well. A peer being refused
    // a connection because the host could not allocate 2 MiB is a worse outcome than
    // a slower scan.
    let arena = if inline {
        None
    } else {
        match Arena::new(limits.arena_bytes()) {
            Ok(a) => Some(a),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "cannot create the channel arena; serving payloads in the frames                      instead, which costs a copy and a smaller batch"
                );
                None
            }
        }
    };

    let mut session = match arena {
        Some(arena) => {
            // The descriptor goes first, before any frame, so a peer has the region
            // mapped before it can be told about a block inside it.
            if let Err(e) = yesno_plugin::channel::send_fd(&stream, arena.as_fd()) {
                tracing::warn!(error = %e, "cannot hand the arena to the peer");
                return;
            }
            Session::new(host, arena, limits)
        }
        // No descriptor is sent, and the greeting's `arena_bytes = 0` is how the peer
        // learns not to wait for one. A peer that blocked on `recv_fd` regardless
        // would hang, which is why the zero is in the greeting rather than implied.
        None => Session::new_inline(host, limits),
    };

    // Two more descriptors for one socket: one the writes serialise on, one that
    // only ever calls `shutdown` and therefore never waits for them.
    let (writer, hangup) = match (stream.try_clone(), stream.try_clone()) {
        (Ok(w), Ok(h)) => (w, h),
        _ => {
            tracing::warn!("cannot split the channel socket");
            return;
        }
    };
    // **Every** write to this peer is bounded from here on, responses included.
    // Bounding only notifications left a peer that stops reading stalling its own
    // serving thread inside a response, with everything that wants its writer
    // queued behind that.
    if let Err(e) = writer.set_write_timeout(Some(PEER_WRITE_TIMEOUT)) {
        tracing::warn!(error = %e, "cannot bound writes to the channel peer");
        return;
    }
    let peer = Arc::new(PeerHandle {
        writer: std::sync::Mutex::new(writer),
        hangup,
    });
    if let Ok(mut p) = peers.lock() {
        p.push(peer.clone());
    }

    let result = yesno_plugin::channel::serve_locked(&mut session, stream, &peer.writer);
    if let Err(e) = result {
        tracing::warn!(error = %e, "plugin channel connection ended with an error");
    }
    if let Ok(mut p) = peers.lock() {
        p.retain(|q| !Arc::ptr_eq(q, &peer));
    }
    // `session` drops here, releasing every snapshot the peer held. That is the
    // reclamation story: no pid, no timeout, no cooperation.
    let (snapshots, handles) = session.outstanding();
    if snapshots != 0 || handles != 0 {
        tracing::debug!(
            snapshots,
            handles,
            "channel peer left holding handles; released"
        );
    }
}

/// The two things that must be told when the database changes underneath.
///
/// Bundled because they always travel together and always in the same order --
/// notify, act, notify -- so threading them separately through the replication path
/// meant two parameters that could get out of step at any call site. It also keeps
/// `one_pass` under clippy's argument limit, which was the signal that the threading
/// had gone too far.
#[derive(Clone, Copy, Default)]
pub struct Listeners<'a> {
    pub channel: Option<&'a Channel>,
}

impl Listeners<'_> {
    /// Tell peers the database is going away.
    ///
    /// **Nothing is asked to drain, and nothing is verified.** A peer's snapshots
    /// belong to its connection, so this side takes them back by closing the
    /// socket whether the peer cooperates or not. The in-process facility needed
    /// a drain-then-verify handshake here because a lease it held could keep the
    /// directory lock and there was no host-side call to take one back; the
    /// notification below is a courtesy so a peer can stop issuing requests it
    /// knows will fail, not a precondition for anything.
    pub fn before_close(&self) {
        let Some(c) = self.channel else {
            return;
        };
        // Announced first, so a peer that is reading learns why its connection is
        // about to end rather than seeing a bare EOF.
        c.notify(&Frame::Unavailable);
        // **Then taken back.** The notification is a courtesy and cannot be relied
        // on: a peer's snapshots hold the directory lock, and a peer under load or
        // simply uninterested would keep the reopen failing `AlreadyOpen`
        // indefinitely. Disconnecting ends each serving thread, which drops its
        // session and with it every snapshot it held.
        let dropped = c.disconnect_peers();
        if dropped > 0 {
            tracing::info!(
                peers = dropped,
                "disconnected channel peers so the database can be reopened"
            );
        }
    }

    /// Tell peers a different database is in the slot now.
    pub fn after_replace(&self) {
        let Some(c) = self.channel else {
            return;
        };
        let generation = c.bump_generation();
        // Two frames rather than one: a peer that only ever saw `Available` could
        // not distinguish "back after a rebuild" from "back unchanged", and every
        // handle it holds names the previous database.
        c.notify(&Frame::GenerationChanged {
            old: generation.saturating_sub(1),
            new: generation,
        });
        c.notify(&Frame::Available { generation });
    }
}

/// Everything a startup path needs to host plugins, built in one place.
///
/// # Why this exists rather than two call sites doing it
///
/// The pieces have an order that is easy to get wrong and impossible to see wrong.
/// A `Host` reads the database through a slot, and it must be **the same** slot the
/// server publishes -- build the facility against a slot the server does not use and
/// everything starts, every callback fires, and every read answers `UNAVAILABLE` for
/// ever. That failure already happened once on the leader path. One constructor
/// means the ordering is stated once.
pub struct Wiring {
    /// The slot to hand the startup path, so it fills the one the plugins read.
    pub slot: crate::guard::DbSlot,
    pub channel: Option<Arc<Channel>>,
}

/// Build the plugin wiring for `role`, or `None` when no channel is configured.
///
/// **This used to be `unsafe`**, because it `dlopen`ed a library named in the
/// configuration and ran its initializers. It is safe now: a peer is a separate
/// process, so nothing here loads or runs foreign code, and there is no trust
/// assertion left for a caller to make. That change of signature is the clearest
/// single statement of what removing the in-process facility bought.
pub fn wire(cfg: &Config, role: Role) -> Result<Option<Wiring>, String> {
    if !cfg.plugin.channel_enabled() {
        return Ok(None);
    }

    // **Refused rather than started.** A cold standby never opens a database, so a
    // channel on one would bind its socket, accept peers, and answer `UNAVAILABLE`
    // to every request for the life of the process -- which looks like a broken
    // peer rather than a misconfiguration. `follower.serve_reads` is what makes a
    // standby hold a database open at all.
    if role == Role::Follower && !cfg.follower.serve_reads {
        return Err("plugin.channel_socket is set on a follower with \
             follower.serve_reads = false; a cold standby never opens a database, so \
             every plugin read would answer UNAVAILABLE. Enable follower.serve_reads \
             or remove the plugin configuration."
            .to_string());
    }

    // Empty: the startup path fills it with the database it opens, and the plugins
    // read through the same one.
    let slot: crate::guard::DbSlot = Arc::new(std::sync::RwLock::new(None));
    let host = Host::new(slot.clone(), 1, role);

    let channel = Channel::start(cfg, host)
        .map_err(|e| format!("cannot start the plugin channel: {e}"))?
        .map(Arc::new);

    Ok(Some(Wiring { slot, channel }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `SO_PEERCRED` is read correctly, which nothing else can show.
    ///
    /// **The integration tests cannot prove this.** They connect as the same user
    /// the daemon runs as, so they pass whether the credential is read properly or
    /// the check is broken in the permissive direction -- and a wrong `SO_PEERCRED`
    /// constant, or a `struct ucred` whose layout does not match the kernel's,
    /// fails exactly that way: it returns something, and that something happens not
    /// to be compared against anything in a same-uid test.
    ///
    /// A socket pair is a connected Unix socket like any other, so the kernel fills
    /// the credentials in for it, and the answer has a known value to check against.
    #[test]
    fn the_peer_credential_is_read_from_the_socket() {
        let (a, _b) = std::os::unix::net::UnixStream::pair().unwrap();
        // SAFETY: `geteuid` reads process state and cannot fail.
        let me = unsafe { libc_geteuid() };
        assert_eq!(
            peer_uid(&a),
            Some(me),
            "the credential of a socket to ourselves is our own uid; a mismatch \
             means the struct layout or the option number is wrong"
        );
    }

    /// The policy admits its own uid and root, and refuses anything else.
    ///
    /// Asserted against the predicate rather than through a socket, because a test
    /// cannot connect as another user without privileges it should not need. What
    /// this covers is the decision; `the_peer_credential_is_read_from_the_socket`
    /// covers the input it decides on.
    #[test]
    fn the_access_policy_refuses_a_uid_it_was_not_given() {
        let policy = AccessPolicy {
            own: 1000,
            allowed: vec![1500],
        };
        assert!(policy.permits(1000), "its own uid");
        assert!(
            policy.permits(0),
            "root, which can read the directory anyway"
        );
        assert!(policy.permits(1500), "an explicitly allowed uid");
        assert!(!policy.permits(1001), "a neighbouring uid is not allowed");
        assert!(
            !policy.permits(u32::MAX),
            "and neither is the value a failed credential read would leave behind"
        );
    }

    /// An empty allow list is "this uid and root", not "anyone".
    #[test]
    fn an_empty_allow_list_is_not_permissive() {
        let policy = AccessPolicy {
            own: 1000,
            allowed: Vec::new(),
        };
        assert!(policy.permits(1000));
        assert!(!policy.permits(1234));
    }

    /// Hanging up never waits for the peer's writer.
    ///
    /// **This is the structural half of re-review item 2, and the half a test can
    /// pin.** `disconnect_peers` used to call `shutdown` through the writer mutex,
    /// so a peer stalled inside a response `write_all` -- which holds that mutex --
    /// blocked the very hang-up meant to reclaim its snapshots.
    ///
    /// The stall is simulated by simply holding the writer lock, which is what a
    /// blocked `write_all` does. Run on another thread with a deadline rather than
    /// inline, because the failure mode is a **hang**: inline, a regression would
    /// deadlock the test process instead of failing it, and this session has
    /// already spent two timeouts learning that a hang reads as a slow test.
    ///
    /// The other half -- that ordinary responses are bounded -- is by construction:
    /// the timeout is set on the socket once at accept, so it covers every write
    /// rather than only the notification path. Reproducing *that* needs a peer that
    /// fills its receive buffer, which is not deterministic at this level, and a
    /// test which cannot fail would be worse than saying so here.
    #[test]
    fn hanging_up_does_not_wait_for_the_writer() {
        let (a, _b) = std::os::unix::net::UnixStream::pair().unwrap();
        let peer = Arc::new(PeerHandle {
            writer: std::sync::Mutex::new(a.try_clone().unwrap()),
            hangup: a,
        });
        let held = peer.writer.lock().unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        let hanging = peer.clone();
        std::thread::spawn(move || {
            hanging.hang_up();
            let _ = tx.send(());
        });
        assert!(
            rx.recv_timeout(std::time::Duration::from_secs(5)).is_ok(),
            "hang_up waited on the writer lock; a peer stalled mid-response would \
             block the disconnect that exists to take its snapshots back"
        );
        drop(held);
    }
}
