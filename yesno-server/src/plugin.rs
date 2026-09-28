//! The plugin facility: when to load, and when to tell a plugin things.
//!
//! The ABI, the loader and the negotiation live in `yesno-plugin`, which has no
//! server and is testable without one. What is here is only the part that needs a
//! server: the slot, the role, the generation, and the ordering of the callbacks
//! around a rebootstrap.
//!
//! # The drain is the whole reason this type exists
//!
//! A plugin's lease is a `Snapshot`, and a `Snapshot` holds `Arc<DbInner>`, which
//! holds the directory lock `File`. So a lease outstanding when the host wants to
//! reopen makes `Db::open_replica` fail `AlreadyOpen` -- and there is **no
//! host-side call that takes a lease back**. `Db::evict_oldest_reader` does not:
//! its own doc says "Does not free the slot", so it releases the reclamation floor
//! without releasing the lock, which is worse in both directions.
//!
//! The existing code already tolerates a narrow version of this. `close_for_rebuild`
//! notes that an in-flight `do_get` holds a snapshot, "so the lock may outlive this
//! by the length of one read" -- self-limiting, so a later pass succeeds. **A
//! scoring lease has no such bound.** A plugin caching handles between requests,
//! which is the obvious optimization, would hold the lock open indefinitely and
//! surface as an unattributable `AlreadyOpen` seconds later in `open_if_needed`.
//!
//! So `before_close` drains and then *verifies*, and reports the plugin by name
//! with a count when it does not. Verification uses the facility's own lease
//! counter rather than `Db::live_readers()`, which cannot answer this in either
//! direction: it over-counts, because any in-flight query holds a slot, and
//! under-counts, because `Snapshot::clone` refcounts one slot so N handles read as
//! one.

use std::sync::Arc;

use yesno_plugin::abi::{Role, Status};
use yesno_plugin::channel::{Arena, Limits, Session};
use yesno_plugin::ipc::Frame;
use yesno_plugin::loader::{LoadError, LoadedPlugin};
use yesno_plugin::Host;

use crate::config::Config;
use crate::guard::DbSlot;

/// A loaded plugin and the host state it reads through.
pub struct Facility {
    plugin: LoadedPlugin,
    host: Host,
    /// Empty when the plugin should be loaded but never asked to serve.
    listen: String,
    library: String,
}

/// What a drain attempt observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Drained {
    /// The plugin released everything. The host may reopen.
    Clean,
    /// Leases are still outstanding. Reopening will fail `AlreadyOpen`.
    Outstanding(usize),
    /// The plugin's callback answered a failure, and leases are as reported.
    Refused(Status, usize),
}

impl Facility {
    /// Load the configured plugin, if any, and announce the current state.
    ///
    /// `None` when no library is configured, which is the default.
    ///
    /// # Safety
    ///
    /// Loads and runs arbitrary code from `cfg.plugin.library`; see
    /// `PluginConfig`. The caller is asserting the operator's configuration is
    /// trusted.
    pub unsafe fn load(
        cfg: &Config,
        slot: DbSlot,
        role: Role,
    ) -> Result<Option<Facility>, LoadError> {
        if !cfg.plugin.enabled() {
            return Ok(None);
        }
        let host = Host::new(slot, 1, role);
        // SAFETY: The caller's assertion, restated above.
        let plugin =
            unsafe { LoadedPlugin::load(std::ffi::OsStr::new(&cfg.plugin.library), host.clone()) }?;
        let f = Facility {
            plugin,
            host,
            listen: cfg.plugin.listen.clone(),
            library: cfg.plugin.library.clone(),
        };
        tracing::info!(
            library = %f.library,
            abi = f.plugin.header().version,
            table_bytes = f.plugin.header().size,
            "plugin loaded"
        );
        Ok(Some(f))
    }

    /// The library this facility loaded, for a message an operator can act on.
    pub fn library(&self) -> &str {
        &self.library
    }

    /// The ABI version the plugin declared.
    pub fn header_version(&self) -> u32 {
        self.plugin.header().version
    }

    /// The table size the plugin declared.
    pub fn header_size(&self) -> u32 {
        self.plugin.header().size
    }

    pub fn generation(&self) -> u64 {
        self.host.generation()
    }

    /// Leases the plugin holds right now.
    pub fn leases(&self) -> usize {
        self.host.leases()
    }

    /// A database is available at `generation`. Announce it, then start serving.
    pub fn after_open(&self, generation: u64) {
        let st = self.plugin.on_available(generation);
        if st != Status::Ok {
            tracing::warn!(
                library = %self.library,
                status = st.name(),
                "the plugin refused on_available; it will not be asked to serve"
            );
            return;
        }
        self.start_serving();
    }

    /// Ask the plugin to serve, when an address is configured.
    pub fn start_serving(&self) {
        if self.listen.trim().is_empty() {
            return;
        }
        match self.plugin.serve_start(&self.listen) {
            Ok(Status::Ok) => {
                tracing::info!(library = %self.library, addr = %self.listen, "plugin serving")
            }
            Ok(st) => tracing::warn!(
                library = %self.library,
                status = st.name(),
                "the plugin refused to start serving"
            ),
            Err(e) => {
                tracing::warn!(library = %self.library, error = %e, "cannot pass the listen address")
            }
        }
    }

    /// Stop serving. Best effort: a refusal is logged, not propagated.
    pub fn stop_serving(&self) {
        if self.listen.trim().is_empty() {
            return;
        }
        let st = self.plugin.serve_stop();
        if st != Status::Ok {
            tracing::warn!(
                library = %self.library,
                status = st.name(),
                "the plugin refused to stop serving; its threads may still be running"
            );
        }
    }

    /// Tell the plugin the database is going away, then check that it let go.
    ///
    /// Called **before** the last `Arc<Db>` is dropped, because the plugin's
    /// handles hold `Arc<DbInner>` and the host's drop is not what releases them.
    pub fn before_close(&self) -> Drained {
        self.stop_serving();
        let st = self.plugin.on_unavailable();
        let left = self.host.leases();
        if st != Status::Ok {
            tracing::error!(
                library = %self.library,
                status = st.name(),
                leases = left,
                "the plugin refused the drain; reopening will fail while leases remain"
            );
            return Drained::Refused(st, left);
        }
        if left != 0 {
            // Named, counted, and attributed. The alternative is this resurfacing
            // as `AlreadyOpen` from a reopen several seconds later, with nothing
            // connecting the two.
            tracing::error!(
                library = %self.library,
                leases = left,
                "the plugin returned from on_unavailable holding leases; the database \
                 cannot be reopened until it releases them, and no host-side call can \
                 take them back"
            );
            return Drained::Outstanding(left);
        }
        Drained::Clean
    }

    /// The database was replaced. Bump the generation and tell the plugin.
    ///
    /// Returns the new generation, which is what a plugin compares against.
    pub fn after_replace(&self) -> u64 {
        let old = self.host.generation();
        let new = self.host.bump_generation();
        let st = self.plugin.on_generation_change(old, new);
        if st != Status::Ok {
            tracing::warn!(
                library = %self.library,
                status = st.name(),
                "the plugin refused on_generation_change; its handles are stale regardless"
            );
        }
        new
    }

    /// This node's role changed.
    pub fn set_role(&self, to: Role) {
        let from = self.host.role();
        if from == to {
            return;
        }
        self.host.set_role(to);
        let st = self.plugin.on_role_change(from, to);
        if st != Status::Ok {
            tracing::warn!(
                library = %self.library,
                status = st.name(),
                "the plugin refused on_role_change"
            );
        }
    }
}

/// Shared handle, so the follower task and the startup path see one facility.
pub type SharedFacility = Option<Arc<Facility>>;

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
    peers: Arc<std::sync::Mutex<Vec<Arc<std::sync::Mutex<std::os::unix::net::UnixStream>>>>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    listener: Option<std::thread::JoinHandle<()>>,
}

impl Channel {
    /// Bind the configured socket and start accepting, or `None` when unconfigured.
    pub fn start(cfg: &Config, host: Host) -> std::io::Result<Option<Channel>> {
        if !cfg.plugin.channel_enabled() {
            return Ok(None);
        }
        let path = std::path::PathBuf::from(cfg.plugin.channel_socket.trim());
        // A stale socket from a previous run refuses `bind` with `EADDRINUSE`, which
        // is indistinguishable from a live server. Removing it is safe because the
        // database lock already proves no other yesnod holds this directory.
        let _ = std::fs::remove_file(&path);
        let listener = std::os::unix::net::UnixListener::bind(&path)?;

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
        };
        let peers: Arc<
            std::sync::Mutex<Vec<Arc<std::sync::Mutex<std::os::unix::net::UnixStream>>>>,
        > = Arc::new(std::sync::Mutex::new(Vec::new()));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let accept_peers = peers.clone();
        let accept_stop = stop.clone();
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
                let host = host.clone();
                let peers = accept_peers.clone();
                std::thread::spawn(move || serve_one(stream, host, limits, peers));
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
            listener: Some(handle),
        }))
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
        let Ok(mut peers) = self.peers.lock() else {
            return;
        };
        peers.retain(|p| match p.lock() {
            Ok(mut s) => std::io::Write::write_all(&mut *s, &bytes).is_ok(),
            Err(_) => false,
        });
    }

    /// Stop accepting and close every connection.
    ///
    /// Closing is what releases the peers' snapshots, so this is also what lets the
    /// database be dropped afterwards. It runs before the reader wait in teardown
    /// for the same reason the in-process drain does.
    pub fn stop(mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        // Shutting each peer's socket unblocks its serving thread out of `read`,
        // which is what a `stop` flag alone cannot do.
        if let Ok(mut peers) = self.peers.lock() {
            for p in peers.drain(..) {
                if let Ok(s) = p.lock() {
                    let _ = s.shutdown(std::net::Shutdown::Both);
                }
            }
        }
        // And connect to our own listener once, so its blocking `accept` returns and
        // the thread sees the flag.
        let _ = std::os::unix::net::UnixStream::connect(&self.path);
        if let Some(h) = self.listener.take() {
            let _ = h.join();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Serve one connection until the peer goes away.
fn serve_one(
    stream: std::os::unix::net::UnixStream,
    host: Host,
    limits: Limits,
    peers: Arc<std::sync::Mutex<Vec<Arc<std::sync::Mutex<std::os::unix::net::UnixStream>>>>>,
) {
    let arena = match Arena::new(limits.arena_bytes()) {
        Ok(a) => a,
        Err(e) => {
            tracing::error!(error = %e, "cannot create the channel arena");
            return;
        }
    };
    // The descriptor goes first, before any frame, so a peer has the region mapped
    // before it can be told about a block inside it.
    if let Err(e) = yesno_plugin::channel::send_fd(&stream, arena.as_fd()) {
        tracing::warn!(error = %e, "cannot hand the arena to the peer");
        return;
    }
    let mut session = Session::new(host, arena, limits);

    let writer = match stream.try_clone() {
        Ok(w) => Arc::new(std::sync::Mutex::new(w)),
        Err(e) => {
            tracing::warn!(error = %e, "cannot split the channel socket");
            return;
        }
    };
    if let Ok(mut p) = peers.lock() {
        p.push(writer.clone());
    }

    let result = yesno_plugin::channel::serve_locked(&mut session, stream, &writer);
    if let Err(e) = result {
        tracing::warn!(error = %e, "plugin channel connection ended with an error");
    }
    if let Ok(mut p) = peers.lock() {
        p.retain(|q| !Arc::ptr_eq(q, &writer));
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
    pub facility: Option<&'a Arc<Facility>>,
    pub channel: Option<&'a Channel>,
}

impl Listeners<'_> {
    /// Tell everyone the database is going away, and report whether the in-process
    /// plugin actually let go.
    ///
    /// A channel peer is not asked to drain: closing its socket is what releases its
    /// snapshots, and that happens whether it cooperates or not.
    pub fn before_close(&self) {
        if let Some(c) = self.channel {
            c.notify(&Frame::Unavailable);
        }
        if let Some(f) = self.facility {
            match f.before_close() {
                Drained::Clean => {}
                other => tracing::error!(
                    library = f.library(),
                    outcome = ?other,
                    "the plugin did not drain; the reopen may fail until it releases"
                ),
            }
        }
    }

    /// Tell everyone a different database is in the slot now.
    pub fn after_replace(&self) {
        let generation = self.facility.map(|f| {
            let g = f.after_replace();
            f.after_open(g);
            g
        });
        if let Some(c) = self.channel {
            // Two frames rather than one: a peer that only ever saw `Available`
            // could not distinguish "back after a rebuild" from "back unchanged",
            // and every handle it holds names the previous database.
            if let Some(g) = generation {
                c.notify(&Frame::GenerationChanged {
                    old: g.saturating_sub(1),
                    new: g,
                });
            }
            c.notify(&Frame::Available {
                generation: generation.unwrap_or(0),
            });
        }
    }
}
