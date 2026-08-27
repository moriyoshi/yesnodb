//! `count(*)` pushdown — the reason this wrapper is worth having.
//!
//! # Why this one aggregate and no other
//!
//! `get_flight_info` answers `total_records` from **container popcounts in the
//! B+tree leaves**, decoding no payload extent and moving no ordinal. So
//! `SELECT count(*) FROM t` costs one round trip and reads none of the data —
//! and with phase 2's qual pushdown in front of it, `count(*) … WHERE …` counts
//! a *filtered* set the same way, because `Expr::cardinality` composes the
//! operators' non-materializing walks.
//!
//! No other aggregate qualifies, and the reason is not effort. `sum`, `min`
//! and `max` would each have to fetch ordinals, so pushing them down would move
//! exactly as much data as not pushing them down while adding a code path that
//! can be wrong. `min`/`max` are worse than useless here: `Snapshot::{min,max}`
//! are `u64` extremes and PostgreSQL wants `int8` extremes, which differ for any
//! set spanning `2^63` — see [`crate::ordinal`].
//!
//! # What must be true before the path is offered
//!
//! Every one of these is a correctness condition, not a simplification:
//!
//! - **no `GROUP BY` and no grouping sets** — the server returns one number, and
//!   there is no way to attribute it to groups;
//! - **no `HAVING`** — it filters *groups*, which do not exist here;
//! - **exactly one aggregate in the target list, and nothing else** — a plain
//!   column beside the count would have no value to report;
//! - **`count`, with no `DISTINCT`, no `ORDER BY`, no `FILTER`** — each of those
//!   changes what is being counted, and the server counts the whole set.
//!
//! Declining is always safe: PostgreSQL falls back to aggregating the rows the
//! scan returns.

use pgrx::prelude::*;

use super::scan::{encode_private_parts, recount_private, PUSHDOWN_COUNT_STAR};

/// Offer a one-row `count(*)` path when the query allows it.
///
/// # Safety
///
/// Called by the planner with valid pointers.
#[pg_guard]
pub unsafe extern "C-unwind" fn get_foreign_upper_paths(
    root: *mut pg_sys::PlannerInfo,
    stage: pg_sys::UpperRelationKind::Type,
    input_rel: *mut pg_sys::RelOptInfo,
    output_rel: *mut pg_sys::RelOptInfo,
    _extra: *mut core::ffi::c_void,
) {
    if stage != pg_sys::UpperRelationKind::UPPERREL_GROUP_AGG {
        return;
    }
    // The callback fires once per upper stage per relation; without this the
    // same path is added twice and the planner costs a duplicate.
    if !unsafe { (*output_rel).fdw_private }.is_null() {
        return;
    }
    if !unsafe { is_bare_count(root, output_rel) } {
        return;
    }

    // The scan below the aggregate carries the pushed-down quals; reuse them so
    // `count(*) … WHERE …` counts the filtered set rather than the whole key.
    let Some(private) = (unsafe { count_private(root, input_rel) }) else {
        return;
    };

    unsafe {
        // Cost is deliberately near-zero rather than derived from the
        // input's. That is not optimism: the server answers from index
        // popcounts without reading a payload, so the work genuinely does not
        // scale with cardinality. Costing it like a scan would make the planner
        // prefer aggregating locally, which is the thing this path exists
        // to avoid.
        let path = crate::pg_compat::foreign_upper_path(root, output_rel, private);
        // Mark the relation so the duplicate-suppression above sees it, and so
        // `GetForeignPlan` can tell an aggregate rel from a base rel.
        (*output_rel).fdw_private = private.cast();
        pg_sys::add_path(output_rel, path.cast());
    }
}

/// Whether the upper relation is exactly one bare `count`.
///
/// # Safety
///
/// Called by the planner with valid pointers.
unsafe fn is_bare_count(
    root: *mut pg_sys::PlannerInfo,
    output_rel: *mut pg_sys::RelOptInfo,
) -> bool {
    let parse = unsafe { (*root).parse };
    if parse.is_null() {
        return false;
    }
    unsafe {
        if !(*parse).groupClause.is_null()
            || !(*parse).groupingSets.is_null()
            || !(*parse).havingQual.is_null()
            || !(*parse).distinctClause.is_null()
            || (*parse).hasWindowFuncs
        {
            return false;
        }
    }

    // The target list must be exactly one expression, and it must be an Aggref.
    let target = unsafe { (*output_rel).reltarget };
    if target.is_null() {
        return false;
    }
    let exprs = unsafe { super::scan::list_nodes_pub((*target).exprs) };
    let [only] = exprs.as_slice() else {
        return false;
    };
    if only.is_null() || unsafe { (**only).type_ } != pg_sys::NodeTag::T_Aggref {
        return false;
    }
    let agg = *only as *mut pg_sys::Aggref;
    unsafe {
        // Each of these changes what is counted. `FILTER` and `DISTINCT`
        // especially: the server counts the whole set and has no way to apply
        // either.
        if !(*agg).aggdistinct.is_null()
            || !(*agg).aggorder.is_null()
            || !(*agg).aggfilter.is_null()
            || (*agg).aggvariadic
            || (*agg).agglevelsup != 0
        {
            return false;
        }
        // `count(*)` has `aggstar`; `count(x)` does not. `count(x)` skips
        // NULLs — safe here only because the ordinal column is structurally
        // non-null ( a posting list is a set of *present* values ), which is why
        // both spellings may share this path.
        if (*agg).aggtype != pg_sys::INT8OID {
            return false;
        }
        // Name check rather than an OID: `count(*)` and `count(any)` are two
        // different functions, and there is no exported constant for either.
        let name = pg_sys::get_func_name((*agg).aggfnoid);
        if name.is_null() {
            return false;
        }
        let name = core::ffi::CStr::from_ptr(name);
        if name.to_bytes() != b"count" {
            return false;
        }
        // A built-in, for the same reason the comparison operators must be.
        (*agg).aggfnoid.to_u32() < 16384
    }
}

/// Build the `fdw_private` an aggregate scan needs: the pushed expression plus
/// a marker saying "return the count, not the rows".
///
/// # Safety
///
/// Called by the planner with valid pointers.
unsafe fn count_private(
    root: *mut pg_sys::PlannerInfo,
    input_rel: *mut pg_sys::RelOptInfo,
) -> Option<*mut pg_sys::List> {
    let rti = unsafe { (*input_rel).relid };
    if rti == 0 {
        // An input relation with no range-table index is a **pushed-down
        // join**, and it already carries the expression the count should run
        // over. Reusing it is what makes `count(*)` over an intersection cost
        // no ordinals at all — the headline case for the whole wrapper.
        //
        // Re-deriving it here is not possible: the join consumed the quals
        // that produced it, so there is nothing left to walk.
        let private = unsafe { (*input_rel).fdw_private }.cast::<pg_sys::List>();
        if private.is_null() {
            return None;
        }
        return unsafe { recount_private(private) };
    }
    // The *table* OID, not the range-table index. `simple_rte_array` is how
    // an upper relation — which has no `relid` of its own — reaches the
    // relation underneath it.
    let rte = unsafe { *(*root).simple_rte_array.add(rti as usize) };
    if rte.is_null() {
        return None;
    }
    let relid = unsafe { (*rte).relid };
    unsafe { encode_private_parts(relid, rti as i32, input_rel, PUSHDOWN_COUNT_STAR) }
}
