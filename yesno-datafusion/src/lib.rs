//! DataFusion integration for `yesno`.
//!
//! # Version coupling
//!
//! This crate's **major version tracks DataFusion's**, so compatibility is
//! answerable from the version number alone: `yesno-datafusion = "55"` works
//! with DataFusion 55.x. That is the `datafusion-federation` convention, and it
//! exists because DataFusion moves fast enough that any other scheme turns into
//! a compatibility matrix nobody maintains.
//!
//! It is deliberately a satellite crate rather than a feature of the core.
//! DataFusion's dependency tree is enormous; coupling the storage engine to it
//! would make every DataFusion release a storage-engine release.

pub mod pushdown;
pub mod udtf;

pub use pushdown::{lower, HashEncoder, LoweredFilter, SetExpr, TermEncoder};
pub use udtf::{MapSource, PostingSource, SnapshotSource, YesnoLookup};
