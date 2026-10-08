//! Host side of the yesnod plugin channel.
//!
//! An out-of-process peer connects over a Unix socket, is handed a shared arena as
//! a file descriptor, and asks for data. [`ipc`] is the wire format and [`channel`]
//! is the server that speaks it; `yesno-server` owns the socket's lifetime, the
//! role and the notifications, because that is where the slot and the role already
//! are.
//!
//! # Liveness is the connection, and it replaced a contract
//!
//! A `Snapshot` holds `Arc<DbInner>`, and `DbInner` holds the directory lock
//! `File` -- so a snapshot pins the flock and the host cannot reopen the database
//! until the last one is gone. `Db::live_readers()` cannot track that:
//! `Snapshot::clone` refcounts the registry slot rather than taking a second one,
//! so N handles from one snapshot read as **one**, while an unrelated in-flight
//! query holds a slot too. It over-counts what is ours and under-counts how many of
//! ours there are.
//!
//! **This crate used to answer that by counting.** An in-process plugin held
//! leases the host could not take back -- `Db::evict_oldest_reader` is not the
//! remedy, since its own doc says "Does not free the slot", so the lock is no
//! closer to released while the reclamation floor *is* dropped, exposing containers
//! the plugin still holds. So every handle was counted, the host asked the plugin
//! to drain, and its only recourse when that failed was to report loudly.
//!
//! A peer's snapshots belong to its **session**, and dropping the session drops
//! them. The socket closing is what drops it -- peer exit, crash, container stop,
//! `SIGKILL`, or this side dropping the listener. So the host takes them back by
//! closing, with no cooperation, no timer and no pid. The lease counter and the
//! drain contract were removed with the in-process ABI on 2026-09-29; see
//! `.agents/docs/LTM/removed-cdylib-plugin-abi.md`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use yesno_core::Db;

pub mod cabi;
pub mod channel;

/// The peer half of the channel, re-exported from `yesno-channel`.
///
/// Those three modules moved out on 2026-10-08 so that a peer -- `yesno-pg`
/// above all -- can link the client without the storage engine coming with it.
/// They are re-exported rather than relocated in the public API, so every
/// existing `yesno_plugin::ipc`, `yesno_plugin::client` and
/// `yesno_plugin::abi` path still resolves and no consumer had to change.
pub use yesno_channel::{abi, client, ipc};

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
}

impl Host {
    pub fn new(slot: DbSlot, generation: u64, role: abi::Role) -> Host {
        Host {
            slot,
            generation: Arc::new(AtomicU64::new(generation)),
            role: Arc::new(AtomicU64::new(role as u64)),
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
}
