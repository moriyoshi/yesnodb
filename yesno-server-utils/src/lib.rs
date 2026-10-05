//! Operational backup and object-archive utilities for `yesnod`.
//!
//! This crate depends on the daemon's public control client and replication
//! protocol; `yesno-server` never depends back on it as a crate. The `yesnod`
//! binary can invoke the sibling `yesnoctl` binary before a follower starts to
//! seed an empty directory from an archive. Keeping the utilities on the client
//! side prevents object-storage and backup workflow policy from becoming daemon
//! library dependencies. Filesystem snapshot creation stays with the daemon;
//! this crate only consumes its leases.

pub mod archive;
pub mod basebackup;
mod deferred;
pub mod gc;
mod history;
pub mod lease;
pub mod metrics;
pub mod restore;
pub mod seed;
pub mod sidecar;
pub mod transport;
