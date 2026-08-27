//! `amgetbitmap`: a posting list becomes a `TIDBitmap`.
//!
//! **`recheck` is always true, and that is a correctness requirement rather
//! than caution.** The key is a *hash* of the indexed value, so two values can
//! share a posting list and the TIDs returned are a **superset** of the rows
//! that match. Bitmap Heap Scan re-evaluates the original qual per tuple when
//! `recheck` is set, which removes the collisions.
//!
//! Passing `recheck = false` would tell the executor it may trust the bitmap,
//! and a colliding value's rows would be returned as genuine matches — a wrong
//! answer with no error. This is the index-AM twin of the `Exact`/`Inexact`
//! mistake `crate::fdw::qual` exists to prevent.

use pgrx::prelude::*;

use super::tid::{is_tid_ordinal, make_tid, ordinal_to_tid};
use super::{index_key_for_scan, open_transport_for_index};
use crate::transport::Transport;

/// Per-scan state: the ordinals fetched for the key this scan was rescanned to.
struct ScanState {
    ordinals: Vec<u64>,
    fetched: bool,
}

/// # Safety
///
/// Called by the executor with a valid `Relation`.
#[pg_guard]
pub unsafe extern "C-unwind" fn ambeginscan(
    index: pg_sys::Relation,
    nkeys: core::ffi::c_int,
    norderbys: core::ffi::c_int,
) -> pg_sys::IndexScanDesc {
    let scan = unsafe { pg_sys::RelationGetIndexScan(index, nkeys, norderbys) };
    let state = Box::new(ScanState {
        ordinals: Vec::new(),
        fetched: false,
    });
    unsafe {
        (*scan).opaque = Box::into_raw(state).cast();
    }
    scan
}

/// # Safety
///
/// Called by the executor with a valid `IndexScanDesc`.
#[pg_guard]
pub unsafe extern "C-unwind" fn amrescan(
    scan: pg_sys::IndexScanDesc,
    keys: pg_sys::ScanKey,
    nkeys: core::ffi::c_int,
    _orderbys: pg_sys::ScanKey,
    _norderbys: core::ffi::c_int,
) {
    unsafe {
        if nkeys > 0 && !keys.is_null() {
            // `RelationGetIndexScan` allocated `keyData`; copying the caller's
            // keys into it is what every AM does here.
            core::ptr::copy(keys, (*scan).keyData, nkeys as usize);
        }
        (*scan).numberOfKeys = nkeys;
        let state = (*scan).opaque as *mut ScanState;
        if !state.is_null() {
            (*state).ordinals.clear();
            (*state).fetched = false;
        }
    }
}

/// Fill the bitmap with every TID in the key's posting list.
///
/// # Safety
///
/// Called by the executor with a valid `IndexScanDesc` and `TIDBitmap`.
#[pg_guard]
pub unsafe extern "C-unwind" fn amgetbitmap(
    scan: pg_sys::IndexScanDesc,
    tbm: *mut pg_sys::TIDBitmap,
) -> i64 {
    let state = unsafe { (*scan).opaque as *mut ScanState };
    if state.is_null() {
        return 0;
    }
    let state = unsafe { &mut *state };

    if !state.fetched {
        state.fetched = true;
        let Some(key) = (unsafe { index_key_for_scan(scan) }) else {
            // No usable equality key: no rows, rather than every row.
            return 0;
        };
        let index = unsafe { (*scan).indexRelation };
        let mut transport = match unsafe { open_transport_for_index(index) } {
            Ok(t) => t,
            Err(e) => error!("yesno_iam: {e}"),
        };
        let cmd = crate::transport::flight::FlightTransport::key_cmd(key);
        if let Err(e) = transport.open_scan(&cmd) {
            error!("yesno_iam: {e}");
        }
        loop {
            match transport.next_batch() {
                Err(e) => error!("yesno_iam: {e}"),
                Ok(None) => break,
                Ok(Some(b)) => state
                    .ordinals
                    .extend(b.into_iter().map(crate::ordinal::i64_to_ordinal)),
            }
        }
    }

    // Emitted **per chunk**, which here means per heap block: the ordinal's
    // high bits are the block number, so ordinals arriving in `u64` order are
    // already grouped by page. One `tbm_add_tuples` per group is what makes this
    // cheap, and it is the alignment `super::tid` documents.
    let mut total: i64 = 0;
    let mut batch: Vec<pg_sys::ItemPointerData> = Vec::new();
    let mut current_block: Option<u32> = None;

    for &o in &state.ordinals {
        // An ordinal that is not a TID is skipped, not truncated. A key is
        // shared namespace with the foreign data wrapper, and folding a large
        // ordinal into a block number would hand the executor a TID pointing at
        // an unrelated row.
        if !is_tid_ordinal(o) {
            continue;
        }
        let (block, offset) = ordinal_to_tid(o);
        if current_block != Some(block) && !batch.is_empty() {
            unsafe {
                pg_sys::tbm_add_tuples(tbm, batch.as_mut_ptr(), batch.len() as i32, true);
            }
            total += batch.len() as i64;
            batch.clear();
        }
        current_block = Some(block);
        batch.push(make_tid(block, offset));
    }
    if !batch.is_empty() {
        unsafe {
            pg_sys::tbm_add_tuples(tbm, batch.as_mut_ptr(), batch.len() as i32, true);
        }
        total += batch.len() as i64;
    }
    total
}

/// # Safety
///
/// Called by the executor with a valid `IndexScanDesc`.
#[pg_guard]
pub unsafe extern "C-unwind" fn amendscan(scan: pg_sys::IndexScanDesc) {
    unsafe {
        let state = (*scan).opaque as *mut ScanState;
        if !state.is_null() {
            drop(Box::from_raw(state));
            (*scan).opaque = core::ptr::null_mut();
        }
    }
}
