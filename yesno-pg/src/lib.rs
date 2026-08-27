//! `yesno_pg` — yesno inside PostgreSQL.
//!
//! # What is here, and what deliberately is not
//!
//! All three planned integration surfaces live here:
//!
//! - a foreign data wrapper for remote ordinal sets, including pushdown and
//!   writes;
//! - an index access method over ordinary PostgreSQL heaps;
//! - a table access method for a single-column `bigint` set.
//!
//! It deliberately is not a general heap and does not share PostgreSQL's commit
//! clock. The narrower contracts and their reasons live in the corresponding
//! module headers.
//!
//! # Why the phase-0 build decision still matters
//!
//! A PostgreSQL extension is meaningful only against the exact server ABI it
//! is loaded into. Bazel supplies a pinned PostgreSQL, `pg_config`, and
//! hermetic libclang together; the hand-written extension SQL avoids pgrx's
//! schema helper executing the just-built shared library. The PostgreSQL-major
//! build flag selects the server, bindings, and crate features as one unit.
//!
//! # Every entry point needs `#[pg_guard]`
//!
//! A Rust panic unwinding across the C boundary corrupts PostgreSQL's state.
//! `#[pg_guard]` converts it into a PostgreSQL `ERROR`, which the backend can
//! recover from. `#[pg_extern]` applies the guard itself; a raw `extern "C"`
//! callback — which is what the access-method routines are —
//! does not, and must carry it explicitly.

use pgrx::prelude::*;

pub mod fdw;
pub mod iam;
pub mod options;
pub mod ordinal;
mod pg_compat;
pub mod tam;
pub mod transport;

::pgrx::pg_module_magic!();

/// Called once when the library is loaded into a backend.
///
/// Registering the GUC here rather than lazily: PostgreSQL requires a custom
/// GUC to be defined before it can be `SET`, and a lazily-defined one silently
/// loses a value set earlier in the session.
#[pg_guard]
pub extern "C-unwind" fn _PG_init() {
    iam::init_guc();
    fdw::modify::init();
}

/// The extension's version, as a smoke test that the library loads at all.
///
/// Deliberately not read from `CARGO_PKG_VERSION` at runtime through anything
/// clever: the whole value of this function is that it is the simplest thing
/// that can fail, so a failure points at the harness rather than at the code.
#[pg_extern]
fn yesno_pg_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    /// Not a `#[pg_test]`. Those need `cargo pgrx test`, which manages a
    /// cluster under `~/.pgrx` — the machine state Bazel exists to remove. SQL
    /// behaviour is covered by the `pg_regress`-style fixtures in `sql/` and
    /// `expected/`, run by `//e2e/postgresql:regress` against a hermetic cluster.
    #[test]
    fn version_is_the_package_version() {
        assert_eq!(super::yesno_pg_version(), env!("CARGO_PKG_VERSION"));
    }
}
