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

/// Attach the path to an `std::io::Error` before it is boxed into an error type.
///
/// **`std::io::Error` carries no path.** `File::open( p )` failing yields
/// exactly "No such file or directory ( os error 2 )", and this crate's error
/// aliases are boxed `dyn Error`, so the `?` that boxes one is where the only
/// identifying information is lost. The archive sidecar is a detached task
/// whose error surfaces through a metric wait many seconds later and in another
/// process, so that string is the entire report.
///
/// This is not hypothetical. The sidecar died with a bare `os error 2` twice --
/// `pitr_retention.py` on 2026-09-28 and `archive.py` on 2026-09-29 -- and
/// neither occurrence could say **which** file was missing, so neither could be
/// diagnosed. See `the-archive-sidecar-fails-with-enoent-under-load`.
///
/// It lives here rather than in any one module because four of them need it and
/// none owns it; `transport` is used *by* `archive`, so importing it from there
/// would invert the dependency.
pub(crate) fn ctx(
    doing: &str,
    path: &std::path::Path,
    error: std::io::Error,
) -> Box<dyn std::error::Error + Send + Sync> {
    format!("cannot {doing} '{}': {error}", path.display()).into()
}
