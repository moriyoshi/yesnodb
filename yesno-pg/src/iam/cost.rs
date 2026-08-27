//! `amcostestimate`: an **exact** row count, which no built-in AM can give.
//!
//! `Snapshot::cardinality` sums container popcounts from the B+tree leaves
//! and decodes no payload extent, so the number handed to the planner is the
//! truth rather than an estimate from `pg_statistic`. That is the single
//! strongest reason to index with yesno rather than with a btree or a GIN.
//!
//! When the server is unreachable the estimate falls back rather than
//! erroring: planning must not fail because a network hiccup, and `EXPLAIN` is
//! often exactly what someone runs while diagnosing one.

use pgrx::prelude::*;

use super::{index_key_for_path, open_transport_for_index};
use crate::transport::Transport;

/// # Safety
///
/// Called by the planner with valid pointers.
#[pg_guard]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C-unwind" fn amcostestimate(
    _root: *mut pg_sys::PlannerInfo,
    path: *mut pg_sys::IndexPath,
    _loop_count: f64,
    startup: *mut pg_sys::Cost,
    total: *mut pg_sys::Cost,
    selectivity: *mut pg_sys::Selectivity,
    correlation: *mut f64,
    pages: *mut f64,
) {
    let index = unsafe { (*(*path).indexinfo).indexoid };
    let rows = unsafe { exact_rows(path, index) };

    unsafe {
        let parent_rows = (*(*(*path).indexinfo).rel).tuples.max(1.0);
        *selectivity = match rows {
            Some(n) => (n as f64 / parent_rows).clamp(0.0, 1.0),
            // Not zero. A zero selectivity tells the planner the index
            // returns nothing, which would make it always the cheapest path —
            // the opposite of a conservative fallback.
            None => 0.1,
        };
        *startup = 0.0;
        // The scan is one round trip plus the postings themselves. Costed low
        // because it genuinely is: no ordinal is read that the answer does not
        // contain.
        *total = 1.0 + rows.unwrap_or(100) as f64 * 0.01;
        // Zero correlation, always. A yesno posting list is in `u64` order,
        // which for TIDs is heap order — but claiming correlation would invite
        // the planner to prefer an ordered scan this AM cannot serve.
        *correlation = 0.0;
        *pages = 1.0;
    }
}

/// The exact number of rows the index will return, when it can be had.
///
/// # Safety
///
/// Called by the planner with valid pointers.
unsafe fn exact_rows(path: *mut pg_sys::IndexPath, index: pg_sys::Oid) -> Option<u64> {
    let key = unsafe { index_key_for_path(path) }?;
    let rel = unsafe { pg_sys::RelationIdGetRelation(index) };
    if rel.is_null() {
        return None;
    }
    let out = (|| {
        let mut transport = unsafe { open_transport_for_index(rel) }.ok()?;
        let cmd = crate::transport::flight::FlightTransport::key_cmd(key);
        transport.cardinality(&cmd).ok()
    })();
    unsafe { pg_sys::RelationClose(rel) };
    out
}
