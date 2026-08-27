//! Apache Arrow interchange for `yesno`.
//!
//! Two paths, and they are not equally good:
//!
//! 1. **[`masks`] — selection masks.** A bitmap container *is* an Arrow
//!    `BooleanBuffer`, bit for bit, so a posting list becomes a row filter for
//!    the cost of a refcount bump. A filter never needs the ordinals as
//!    integers, so this path never materializes them.
//! 2. **[`batch`] — ordinals as `UInt64Array`.** The general-purpose escape
//!    hatch. It materializes, which is exactly what path 1 exists to avoid, so
//!    prefer masks wherever the consumer can take one.
//!
//! # There are no nulls
//!
//! Every field is `nullable = false` and every array is built with
//! `nulls: None`. A posting list is a set of *present* values; absence is a
//! total, closed-world fact already encoded by a zero bit. A validity buffer
//! would double the mask allocation, destroy the zero-copy handoff, and force
//! null-handling branches into every downstream kernel.
//!
//! # Version coupling
//!
//! This crate pins `arrow = 59` and re-exports it, so downstreams provably link
//! the same version. Abstracting over Arrow versions is not attempted — nobody
//! has made that work, and pretending otherwise would just move the breakage.

pub mod batch;
pub mod containers;
pub mod masks;
pub mod schema;

pub use batch::{BatchPolicy, OrdinalBatchReader};
pub use containers::{read_containers, ContainerBatchBuilder};
pub use masks::{MaskChunk, MaskStream};
pub use schema::{containers_schema, mask_chunk_schema, ordinals_schema, pairs_schema};

/// Re-exported so downstreams provably link the same Arrow.
pub use arrow_array;
pub use arrow_buffer;
pub use arrow_schema;
