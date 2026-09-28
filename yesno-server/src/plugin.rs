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
