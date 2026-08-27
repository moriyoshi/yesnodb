//! Operational backup and object-archive utilities for `yesnod`.
//!
//! This crate depends on the daemon's public control client and replication
//! protocol; `yesno-server` never depends back on it. Keeping the utilities on
//! the client side of that boundary prevents object-storage and backup workflow
//! policy from becoming daemon dependencies. Filesystem snapshot creation stays
//! with the daemon; this crate only consumes its leases.

pub mod archive;
pub mod basebackup;
mod deferred;
pub mod gc;
mod history;
pub mod lease;
pub mod metrics;
pub mod restore;
pub mod sidecar;
pub mod transport;
