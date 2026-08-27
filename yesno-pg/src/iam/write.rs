//! `ambuild`, `ambuildempty`, `aminsert`.
//!
//! Writes go through the same per-transaction buffer the foreign data
//! wrapper uses, for the same reason: a yesno commit appends to the WAL and
//! fsyncs, so one commit per indexed row would make `CREATE INDEX` unusable, and
//! a `ROLLBACK` must leave nothing behind.

use pgrx::prelude::*;

use super::{index_key_for_datum, index_target};
use crate::fdw::modify::buffer_ordinal;

/// Build the index by scanning the heap.
///
/// # Safety
///
/// Called by the executor with valid `Relation`s.
#[pg_guard]
pub unsafe extern "C-unwind" fn ambuild(
    heap: pg_sys::Relation,
    index: pg_sys::Relation,
    index_info: *mut pg_sys::IndexInfo,
) -> *mut pg_sys::IndexBuildResult {
    let mut state = BuildState { rows: 0 };
    // Dispatched through the table AM directly. `table_index_build_scan` is
    // a `static inline` wrapper in `tableam.h`, so bindgen never emits it — the
    // same class as `ExecClearTuple` and `ItemPointerSet`. The wrapper's whole
    // body is this call with `start_blockno = 0` and `numblocks =
    // InvalidBlockNumber`, meaning "the whole heap".
    let n = unsafe {
        let am = (*heap).rd_tableam;
        let scan_fn = (*am)
            .index_build_range_scan
            .expect("every table AM provides index_build_range_scan");
        scan_fn(
            heap,
            index,
            index_info,
            true, // allow_sync
            // `anyvisible = false`: only tuples visible to this snapshot are
            // indexed. Passing true would index dead tuples, and this AM has no
            // per-tuple visibility of its own to filter them out later.
            false,
            true, // progress
            0,
            u32::MAX, // InvalidBlockNumber: to the end
            Some(build_callback),
            (&mut state as *mut BuildState).cast(),
            core::ptr::null_mut(),
        )
    };

    let mut result = unsafe { PgBox::<pg_sys::IndexBuildResult>::alloc0() };
    result.heap_tuples = n;
    result.index_tuples = state.rows as f64;
    result.into_pg()
}

struct BuildState {
    rows: u64,
}

/// # Safety
///
/// Called by `table_index_build_scan` with a valid TID and datum array.
unsafe extern "C-unwind" fn build_callback(
    index: pg_sys::Relation,
    tid: pg_sys::ItemPointer,
    values: *mut pg_sys::Datum,
    isnull: *mut bool,
    _tuple_is_alive: bool,
    state: *mut core::ffi::c_void,
) {
    let st = state as *mut BuildState;
    if unsafe { record_tuple(index, values, isnull, tid) } {
        unsafe {
            (*st).rows += 1;
        }
    }
}

/// # Safety
///
/// Called by the executor with a valid `Relation`.
#[pg_guard]
pub unsafe extern "C-unwind" fn ambuildempty(_index: pg_sys::Relation) {
    // Nothing to initialise. The index's data lives in yesno, not in a
    // relation fork, so there is no metapage to write — which is also why the
    // index is outside PostgreSQL's WAL. See `super::handler`.
}

/// # Safety
///
/// Called by the executor with valid pointers.
#[pg_guard]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C-unwind" fn aminsert(
    index: pg_sys::Relation,
    values: *mut pg_sys::Datum,
    isnull: *mut bool,
    heap_tid: pg_sys::ItemPointer,
    _heap: pg_sys::Relation,
    _check_unique: pg_sys::IndexUniqueCheck::Type,
    _index_unchanged: bool,
    _index_info: *mut pg_sys::IndexInfo,
) -> bool {
    unsafe { record_tuple(index, values, isnull, heap_tid) }
}

/// Buffer one heap tuple's posting. Returns whether anything was recorded.
///
/// # Safety
///
/// Called with valid pointers from the executor.
unsafe fn record_tuple(
    index: pg_sys::Relation,
    values: *mut pg_sys::Datum,
    isnull: *mut bool,
    tid: pg_sys::ItemPointer,
) -> bool {
    // A NULL indexed value is **skipped**, not stored under a "null key". A
    // posting list is a set of present values, `amsearchnulls` is false, and
    // `col IS NULL` is therefore never answered from this index — so storing one
    // would create rows only a query that cannot run could find.
    if unsafe { *isnull.add(0) } {
        return false;
    }
    let Some(key) = (unsafe { index_key_for_datum(index, *values.add(0)) }) else {
        return false;
    };
    let Some((endpoint, _)) = (unsafe { index_target(index) }) else {
        error!("yesno_iam: the indexed table has no reachable yesno server");
    };
    let (block, offset) = unsafe { super::tid::split_tid(tid) };
    let ordinal = super::tid::tid_to_ordinal(block, offset);
    buffer_ordinal(endpoint, key, ordinal, false);
    true
}
