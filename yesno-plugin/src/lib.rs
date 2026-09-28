//! Host side of the yesnod plugin ABI.
//!
//! The C contract is `include/yesno_plugin.h`; this module implements the host
//! table it declares. The design and the reasoning behind each rule are in
//! `.agents/docs/LTM/hosted-plugin-abi-design.md`.
//!
//! # What this crate does not do
//!
//! It does not `dlopen` anything and it does not know about roles, listeners or
//! replication. It turns a live `Db` into a function table and nothing else, so
//! that the loading, the lifecycle and the policy live in `yesno-server` where the
//! slot and the role already are. A plugin facility that also owned the ABI would
//! make the ABI untestable without a server.
//!
//! # Why every handle counts as a lease
//!
//! A [`Snapshot`] holds `Arc<DbInner>`, and `DbInner` holds the directory lock
//! `File`. So a snapshot -- or a lane handle derived from one -- **pins the
//! flock**, and the host cannot reopen the database until the last of them is
//! gone. `Db::live_readers()` cannot be used to track this: `Snapshot::clone`
//! refcounts the registry slot rather than taking a second one, so N handles from
//! one snapshot read as **one**, and an unrelated in-flight query holds a slot
//! too. It over-counts what is ours and under-counts how many of ours there are.
//!
//! So the facility counts what it mints. [`Host::leases`] is incremented for every
//! snapshot and every lane handle issued and decremented on release, which is
//! attributable by construction and correct at zero lanes.
//!
//! # Why there is no forcible revocation
//!
//! `Db::evict_oldest_reader` looks like the remedy and is not: its own doc says
//! "Does not free the slot", so the slot stays claimed, `live_readers()` does not
//! fall, and the `Snapshot` keeps holding `Arc<DbInner>` -- the lock is no closer
//! to being released. It *does* release the reclamation floor, which exposes any
//! container the plugin still holds. For this purpose it is strictly the worst of
//! both, and it is not called here. Only the plugin releasing its handles restores
//! the host's ability to reopen, which is why the drain in `on_unavailable` is a
//! contract and the host's recourse is to report loudly.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};

use yesno_core::{Container, Db, KeyLanes, Snapshot};

pub mod abi;
pub mod channel;
pub mod ipc;
pub mod loader;
pub mod table;

/// Path to the C fixture plugin this crate's build script compiles.
///
/// `None` when no C compiler was available, in which case
/// [`fixture_plugin_error`] says so. Exists so **other crates' tests** can load
/// the same fixture -- a `cargo:rustc-env` from this crate's build script reaches
/// only this crate's own compilation, so a `yesno-server` test cannot read it
/// directly. Hidden rather than feature-gated because cargo unifies features
/// across normal and dev dependencies, so a `fixture` feature would be on in
/// production too.
#[doc(hidden)]
pub fn fixture_plugin_path() -> Option<&'static str> {
    option_env!("YESNO_TEST_PLUGIN")
}

/// Why [`fixture_plugin_path`] is `None`, when it is.
#[doc(hidden)]
pub fn fixture_plugin_error() -> &'static str {
    option_env!("YESNO_TEST_PLUGIN_ERROR").unwrap_or("no reason was recorded")
}

/// The slot a server keeps its database in. `None` during a rebootstrap.
///
/// Structurally identical to `yesno_server::guard::DbSlot`, and deliberately
/// redeclared rather than imported: this crate must not depend on the server.
pub type DbSlot = Arc<RwLock<Option<Arc<Db>>>>;

/// What a plugin sees behind `yesno_host_db`.
///
/// Cheap to clone; every field is shared.
#[derive(Clone)]
pub struct Host {
    slot: DbSlot,
    /// Bumped every time the database in the slot is replaced.
    ///
    /// Lives here rather than on `Db` because the core has no concept of being
    /// replaced, and because a plugin must be able to read it *while there is no
    /// database* -- which is exactly when it matters most.
    generation: Arc<AtomicU64>,
    role: Arc<AtomicU64>,
    /// Handles this facility has issued and not yet had returned.
    leases: Arc<AtomicUsize>,
}

impl Host {
    pub fn new(slot: DbSlot, generation: u64, role: abi::Role) -> Host {
        Host {
            slot,
            generation: Arc::new(AtomicU64::new(generation)),
            role: Arc::new(AtomicU64::new(role as u64)),
            leases: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// The database now, or `None` while the slot is empty.
    pub fn db(&self) -> Option<Arc<Db>> {
        self.slot.read().ok().and_then(|g| g.clone())
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Record that the database was replaced. Returns the new generation.
    ///
    /// The caller is the server, after a successful reopen. Callbacks into the
    /// plugin are the server's business too; this only moves the number.
    pub fn bump_generation(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::AcqRel) + 1
    }

    pub fn role(&self) -> abi::Role {
        abi::Role::from_raw(self.role.load(Ordering::Acquire))
    }

    pub fn set_role(&self, role: abi::Role) {
        self.role.store(role as u64, Ordering::Release);
    }

    /// Handles outstanding. **This, not `Db::live_readers()`, is the drain
    /// signal**; see the module header.
    pub fn leases(&self) -> usize {
        self.leases.load(Ordering::Acquire)
    }
}

/// One outstanding lease, counted for as long as this lives.
///
/// # Why RAII and not a pair of `fetch_add` / `fetch_sub` calls
///
/// The obvious spelling increments, builds the handle, and hands it out, with the
/// matching decrement in the handle's `Drop`. **That has a window.** A panic
/// between the increment and the handle escaping leaves the count permanently
/// high and the host's drain never completes; move the increment after
/// construction instead and a panic during construction drops a handle whose
/// `Drop` decrements a count that was never incremented, which underflows a
/// `usize` into billions. Both failures are silent and both break the drain,
/// which is the one contract with no backstop.
///
/// Creating the guard first and moving it into the handle removes the window
/// rather than narrowing it: the count is incremented exactly when the guard
/// exists and decremented exactly when it stops existing, whichever path is
/// taken out of the function.
pub struct LeaseGuard {
    host: Host,
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        self.host.leases.fetch_sub(1, Ordering::AcqRel);
    }
}

/// What a plugin sees behind `yesno_snapshot`.
pub struct SnapshotHandle {
    snap: Snapshot,
    host: Host,
    _lease: LeaseGuard,
}

impl Host {
    /// Count one lease, released when the returned guard drops.
    pub fn lease(&self) -> LeaseGuard {
        self.leases.fetch_add(1, Ordering::AcqRel);
        LeaseGuard { host: self.clone() }
    }
}

/// What a plugin sees behind `yesno_lanes`.
pub struct LanesHandle {
    lanes: KeyLanes,
    /// Whether a block is open.
    ///
    /// Tracked here rather than in `KeyLanes` on purpose. The core type is a
    /// clean iterator whose `advance` simply releases the previous block; the
    /// release-before-advance discipline is an **ABI** affordance, existing so a
    /// caller that holds a borrowed pointer across a block boundary can be told
    /// `YESNO_BLOCK_STATE` instead of reading freed memory. Putting it in the
    /// core would impose a protocol on Rust callers that do not need one.
    block_open: bool,
    _lease: LeaseGuard,
}

impl LanesHandle {
    fn lane(&self, i: usize) -> Option<&Container> {
        self.lanes.lane(i)
    }
}
