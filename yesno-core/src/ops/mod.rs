//! Container-level set algebra.
//!
//! Every arm routes through [`generic`] unless a benchmark justified
//! specializing it. Two modules hold the arms that did:
//!
//! * [`bitmap`] — bitmap×bitmap, where the generic value-by-value merge was
//!   leaving a measured 1.3-1.4x gap against the `roaring` crate on dense
//!   operands. Since 2026-08-28 the word loops live there as named kernels with
//!   scalar oracles, the popcount is an AArch64 NEON `cnt`/`uadalp` ladder
//!   ( 1.77x over what LLVM emits for `count_ones().sum()` ), and the two
//!   *predicates* — which LLVM could not vectorize at all, because
//!   `Iterator::all` short-circuits — are blocked rather than per-word, 3.3x.
//! * [`mod@array`] — array×array, first for the dispatch ( 2.5x over the generic
//!   `Peekable<ContainerIter>` merge ) and since 2026-08-27 for the branch: its
//!   intersection merge is an AArch64 NEON kernel, up to 6.0x over the scalar
//!   two-pointer merge and, more usefully, *flat* in `m` where the scalar one is
//!   not. The module header carries the measurement and the one shape — skew
//!   past `GALLOP_RATIO` — where widening loses.
//!
//! The generic kernel stays as the differential oracle for every specialization,
//! which is what makes adding one safe.

pub mod array;
pub mod bitmap;
pub mod card;
pub mod generic;
pub mod mixed;
pub mod nary;
pub mod run;

pub use card::{
    and_cardinality, andnot_cardinality, contains_all, is_disjoint, or_cardinality, xor_cardinality,
};
pub use generic::SetOp;

use crate::container::Container;

/// Apply `op`, dispatching to a specialized arm where one exists.
///
/// **This is the entry point every caller must use.** Calling
/// `generic::apply` directly bypasses every specialization silently — which is
/// exactly what `OrdSet::binary` did, making two specializations look like they
/// had no effect when in truth they were never reached.
#[inline]
pub fn apply(op: SetOp, a: &Container, b: &Container) -> Option<Container> {
    if let Some(r) = array::try_apply(op, a, b) {
        return r;
    }
    if let Some(r) = bitmap::try_apply(op, a, b) {
        return r;
    }
    if let Some(r) = run::try_apply(op, a, b) {
        return r;
    }
    if let Some(r) = mixed::try_apply(op, a, b) {
        return r;
    }
    if let Some(r) = mixed::try_apply_array_bitmap(op, a, b) {
        return r;
    }
    if let Some(r) = mixed::try_apply_array_run(op, a, b) {
        return r;
    }
    generic::apply(op, a, b)
}

#[inline]
pub fn and(a: &Container, b: &Container) -> Option<Container> {
    apply(SetOp::And, a, b)
}

#[inline]
pub fn or(a: &Container, b: &Container) -> Option<Container> {
    apply(SetOp::Or, a, b)
}

#[inline]
pub fn xor(a: &Container, b: &Container) -> Option<Container> {
    apply(SetOp::Xor, a, b)
}

#[inline]
pub fn and_not(a: &Container, b: &Container) -> Option<Container> {
    apply(SetOp::AndNot, a, b)
}
