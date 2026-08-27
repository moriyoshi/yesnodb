//! The table AM's callbacks.
//!
//! Mechanical by design. Every decision that could be wrong lives in
//! [`super::handler`]'s header and in [`super::tid`]; this file is the wiring.
//!
//! Callbacks that cannot be honoured `error!` with the reason rather than doing
//! something approximate — a table AM that silently ignores `SELECT … FOR
//! UPDATE` has told the caller its rows are locked when they are not.

use pgrx::prelude::*;

use super::tid::{ordinal_to_tid, tid_to_ordinal, TAM_ORDINAL_MAX};
use crate::fdw::modify::buffer_ordinal;
use crate::iam::{index_target_for_table, open_transport_for_table};
use crate::transport::Transport;

/// Scan state: the ordinals of the whole set, and where we are in them.
///
/// **`#[repr(C)]` is mandatory, not stylistic.** PostgreSQL is handed a
/// `TableScanDesc` — a `*mut TableScanDescData` — and reads its fields directly.
/// This struct is that data followed by our own, and the cast is only valid if
/// `base` is guaranteed to sit at offset zero. Without `repr(C)` Rust may
/// reorder fields, and PostgreSQL then reads `rs_rd` out of a `Vec`'s pointer:
/// an immediate backend crash, which is exactly what happened before this
/// attribute was added.
#[repr(C)]
struct Scan {
    base: pg_sys::TableScanDescData,
    ordinals: Vec<u64>,
    at: usize,
    /// Half-open ordinal bounds from `scan_set_tidrange`, if any.
    range: Option<(u64, u64)>,
}

/// # Safety
/// Called by the executor with a valid `Relation`.
#[pg_guard]
pub unsafe extern "C-unwind" fn slot_callbacks(
    _rel: pg_sys::Relation,
) -> *const pg_sys::TupleTableSlotOps {
    // A virtual slot: the value is synthesized from the TID, so there is no
    // stored tuple to deform.
    unsafe { &pg_sys::TTSOpsVirtual }
}

/// # Safety
/// Called by the executor with valid pointers.
#[pg_guard]
pub unsafe extern "C-unwind" fn scan_begin(
    rel: pg_sys::Relation,
    snapshot: pg_sys::Snapshot,
    nkeys: core::ffi::c_int,
    key: *mut pg_sys::ScanKeyData,
    pscan: pg_sys::ParallelTableScanDesc,
    flags: u32,
) -> pg_sys::TableScanDesc {
    let ordinals = unsafe { fetch_all(rel) };
    let scan = Box::new(Scan {
        base: pg_sys::TableScanDescData {
            rs_rd: rel,
            rs_snapshot: snapshot,
            rs_nkeys: nkeys,
            rs_key: key,
            rs_flags: flags,
            rs_parallel: pscan,
            ..Default::default()
        },
        ordinals,
        at: 0,
        range: None,
    });
    Box::into_raw(scan).cast()
}

/// # Safety
/// Called by the executor with a valid `TableScanDesc`.
#[pg_guard]
pub unsafe extern "C-unwind" fn scan_end(scan: pg_sys::TableScanDesc) {
    if !scan.is_null() {
        drop(unsafe { Box::from_raw(scan as *mut Scan) });
    }
}

/// # Safety
/// Called by the executor with a valid `TableScanDesc`.
#[pg_guard]
pub unsafe extern "C-unwind" fn scan_rescan(
    scan: pg_sys::TableScanDesc,
    _key: *mut pg_sys::ScanKeyData,
    _set_params: bool,
    _allow_strat: bool,
    _allow_sync: bool,
    _allow_pagemode: bool,
) {
    let s = scan as *mut Scan;
    if !s.is_null() {
        unsafe {
            (*s).at = 0;
        }
    }
}

/// # Safety
/// Called by the executor with valid pointers.
#[pg_guard]
pub unsafe extern "C-unwind" fn scan_getnextslot(
    scan: pg_sys::TableScanDesc,
    _direction: pg_sys::ScanDirection::Type,
    slot: *mut pg_sys::TupleTableSlot,
) -> bool {
    let s = scan as *mut Scan;
    if s.is_null() {
        return false;
    }
    let s = unsafe { &mut *s };
    while s.at < s.ordinals.len() {
        let o = s.ordinals[s.at];
        s.at += 1;
        if let Some((lo, hi)) = s.range {
            if o < lo || o >= hi {
                continue;
            }
        }
        unsafe { store_ordinal(slot, o) };
        return true;
    }
    unsafe { clear(slot) };
    false
}

/// # Safety
/// Called by the executor with valid pointers.
#[pg_guard]
pub unsafe extern "C-unwind" fn scan_set_tidrange(
    scan: pg_sys::TableScanDesc,
    mintid: pg_sys::ItemPointer,
    maxtid: pg_sys::ItemPointer,
) {
    // A TID range **is** an ordinal range here, exactly, because the tuple is
    // its TID. This is the one place a yesno table beats a heap outright.
    let s = scan as *mut Scan;
    if s.is_null() {
        return;
    }
    let lo = unsafe { tid_bound(mintid, 0) };
    let hi = unsafe { tid_bound(maxtid, TAM_ORDINAL_MAX) };
    unsafe {
        (*s).range = Some((lo, hi.saturating_add(1)));
        (*s).at = 0;
    }
}

/// # Safety
/// Called by the executor with valid pointers.
#[pg_guard]
pub unsafe extern "C-unwind" fn scan_getnextslot_tidrange(
    scan: pg_sys::TableScanDesc,
    direction: pg_sys::ScanDirection::Type,
    slot: *mut pg_sys::TupleTableSlot,
) -> bool {
    unsafe { scan_getnextslot(scan, direction, slot) }
}

// ── Parallel scan: not supported, and reported as size zero ─────────────────
//
// Returning 0 from `parallelscan_estimate` is how a table AM says "no
// parallel scan"; PostgreSQL then never builds one. Erroring here instead
// would break plain queries, because the planner asks before deciding.

/// # Safety
/// Called by the planner.
#[pg_guard]
pub unsafe extern "C-unwind" fn parallelscan_estimate(_rel: pg_sys::Relation) -> pg_sys::Size {
    0
}

/// # Safety
/// Called by the planner.
#[pg_guard]
pub unsafe extern "C-unwind" fn parallelscan_initialize(
    _rel: pg_sys::Relation,
    _pscan: pg_sys::ParallelTableScanDesc,
) -> pg_sys::Size {
    0
}

/// # Safety
/// Called by the planner.
#[pg_guard]
pub unsafe extern "C-unwind" fn parallelscan_reinitialize(
    _rel: pg_sys::Relation,
    _pscan: pg_sys::ParallelTableScanDesc,
) {
}

// ── Index fetch ─────────────────────────────────────────────────────────────

/// # Safety
/// Called by the executor with a valid `Relation`.
#[pg_guard]
pub unsafe extern "C-unwind" fn index_fetch_begin(
    rel: pg_sys::Relation,
) -> *mut pg_sys::IndexFetchTableData {
    let p = unsafe {
        pg_sys::palloc0(core::mem::size_of::<pg_sys::IndexFetchTableData>())
            .cast::<pg_sys::IndexFetchTableData>()
    };
    unsafe {
        (*p).rel = rel;
    }
    p
}

/// # Safety
/// Called by the executor.
#[pg_guard]
pub unsafe extern "C-unwind" fn index_fetch_reset(_scan: *mut pg_sys::IndexFetchTableData) {}

/// # Safety
/// Called by the executor.
#[pg_guard]
pub unsafe extern "C-unwind" fn index_fetch_end(scan: *mut pg_sys::IndexFetchTableData) {
    if !scan.is_null() {
        unsafe { pg_sys::pfree(scan.cast()) };
    }
}

/// # Safety
/// Called by the executor with valid pointers.
#[pg_guard]
pub unsafe extern "C-unwind" fn index_fetch_tuple(
    scan: *mut pg_sys::IndexFetchTableData,
    tid: pg_sys::ItemPointer,
    _snapshot: pg_sys::Snapshot,
    slot: *mut pg_sys::TupleTableSlot,
    call_again: *mut bool,
    all_dead: *mut bool,
) -> bool {
    unsafe {
        if !call_again.is_null() {
            *call_again = false;
        }
        if !all_dead.is_null() {
            *all_dead = false;
        }
        let rel = (*scan).rel;
        fetch_tid_into(rel, tid, slot)
    }
}

/// # Safety
/// Called by the executor with valid pointers.
#[pg_guard]
pub unsafe extern "C-unwind" fn tuple_fetch_row_version(
    rel: pg_sys::Relation,
    tid: pg_sys::ItemPointer,
    _snapshot: pg_sys::Snapshot,
    slot: *mut pg_sys::TupleTableSlot,
) -> bool {
    unsafe { fetch_tid_into(rel, tid, slot) }
}

/// # Safety
/// Called by the executor with valid pointers.
#[pg_guard]
pub unsafe extern "C-unwind" fn tuple_tid_valid(
    _scan: pg_sys::TableScanDesc,
    tid: pg_sys::ItemPointer,
) -> bool {
    unsafe { ordinal_of_tid(tid).is_some() }
}

/// # Safety
/// Called by the executor with valid pointers.
#[pg_guard]
pub unsafe extern "C-unwind" fn tuple_get_latest_tid(
    _scan: pg_sys::TableScanDesc,
    _tid: pg_sys::ItemPointer,
) {
    // Nothing to follow. There are no update chains, because `UPDATE` is
    // rejected — a tuple's identity is its value, so it cannot move.
}

/// # Safety
/// Called by the executor with valid pointers.
#[pg_guard]
pub unsafe extern "C-unwind" fn tuple_satisfies_snapshot(
    _rel: pg_sys::Relation,
    _slot: *mut pg_sys::TupleTableSlot,
    _snapshot: pg_sys::Snapshot,
) -> bool {
    // Always true, and this is restriction 1 in one line. Visibility came
    // from the yesno snapshot the scan read through; there are no per-tuple
    // xids to consult. See `super::handler`.
    true
}

/// # Safety
/// Called by the executor with valid pointers.
#[pg_guard]
pub unsafe extern "C-unwind" fn index_delete_tuples(
    _rel: pg_sys::Relation,
    delstate: *mut pg_sys::TM_IndexDeleteOp,
) -> pg_sys::TransactionId {
    // No opportunistic index deletion: it needs per-tuple visibility to know
    // a TID is dead, which this AM does not have. Reporting nothing deletable is
    // correct and costs only an optimisation.
    unsafe {
        if !delstate.is_null() {
            (*delstate).ndeltids = 0;
        }
    }
    pg_sys::InvalidTransactionId
}

// ── Writes ──────────────────────────────────────────────────────────────────

/// # Safety
/// Called by the executor with valid pointers.
#[pg_guard]
pub unsafe extern "C-unwind" fn tuple_insert(
    rel: pg_sys::Relation,
    slot: *mut pg_sys::TupleTableSlot,
    _cid: pg_sys::CommandId,
    _options: core::ffi::c_int,
    _bistate: *mut pg_sys::BulkInsertStateData,
) {
    let o = unsafe { ordinal_from_slot(slot) };
    let Some((endpoint, key)) = (unsafe { index_target_for_table(rel) }) else {
        error!("yesno_tam: this table has no reachable yesno server");
    };
    buffer_ordinal(endpoint, key, o, false);
    // The slot must carry the TID back: `INSERT … RETURNING` and any index
    // build read it, and a zero TID would name block 0 offset 0, which is not a
    // valid tuple pointer.
    if let Some((block, offset)) = ordinal_to_tid(o) {
        unsafe {
            (*slot).tts_tid = crate::iam::tid::make_tid(block, offset);
        }
    }
}

/// # Safety
/// Called by the executor with valid pointers.
#[pg_guard]
pub unsafe extern "C-unwind" fn multi_insert(
    rel: pg_sys::Relation,
    slots: *mut *mut pg_sys::TupleTableSlot,
    nslots: core::ffi::c_int,
    cid: pg_sys::CommandId,
    options: core::ffi::c_int,
    bistate: *mut pg_sys::BulkInsertStateData,
) {
    for i in 0..nslots as usize {
        unsafe { tuple_insert(rel, *slots.add(i), cid, options, bistate) };
    }
}

/// # Safety
/// Called by the executor with valid pointers.
#[pg_guard]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C-unwind" fn tuple_delete(
    rel: pg_sys::Relation,
    tid: pg_sys::ItemPointer,
    _cid: pg_sys::CommandId,
    _snapshot: pg_sys::Snapshot,
    _crosscheck: pg_sys::Snapshot,
    _wait: bool,
    _tmfd: *mut pg_sys::TM_FailureData,
    _changing_part: bool,
) -> pg_sys::TM_Result::Type {
    let Some(o) = (unsafe { ordinal_of_tid(tid) }) else {
        return pg_sys::TM_Result::TM_Invisible;
    };
    let Some((endpoint, key)) = (unsafe { index_target_for_table(rel) }) else {
        error!("yesno_tam: this table has no reachable yesno server");
    };
    buffer_ordinal(endpoint, key, o, true);
    pg_sys::TM_Result::TM_Ok
}

// ── Rejected outright ───────────────────────────────────────────────────────

/// # Safety
/// Called by the executor.
#[pg_guard]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C-unwind" fn tuple_update(
    _rel: pg_sys::Relation,
    _otid: pg_sys::ItemPointer,
    _slot: *mut pg_sys::TupleTableSlot,
    _cid: pg_sys::CommandId,
    _snapshot: pg_sys::Snapshot,
    _crosscheck: pg_sys::Snapshot,
    _wait: bool,
    _tmfd: *mut pg_sys::TM_FailureData,
    _lockmode: *mut pg_sys::LockTupleMode::Type,
    _update_indexes: *mut pg_sys::TU_UpdateIndexes::Type,
) -> pg_sys::TM_Result::Type {
    error!(
        "yesno_tam: UPDATE is not supported on a yesno table. The value is the \
         row's identity, so changing it is a DELETE followed by an INSERT — \
         write it that way so the intent is visible."
    );
}

/// # Safety
/// Called by the executor.
#[pg_guard]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C-unwind" fn tuple_lock(
    _rel: pg_sys::Relation,
    _tid: pg_sys::ItemPointer,
    _snapshot: pg_sys::Snapshot,
    _slot: *mut pg_sys::TupleTableSlot,
    _cid: pg_sys::CommandId,
    _mode: pg_sys::LockTupleMode::Type,
    _wait_policy: pg_sys::LockWaitPolicy::Type,
    _flags: u8,
    _tmfd: *mut pg_sys::TM_FailureData,
) -> pg_sys::TM_Result::Type {
    // Not silently ignored. Row locking needs per-tuple state this AM has
    // nowhere to keep, and pretending to lock would tell a caller its rows are
    // held when they are not — which is how two transactions both "win".
    error!(
        "yesno_tam: row locking ( SELECT … FOR UPDATE / FOR SHARE, and foreign \
         keys referencing this table ) is not supported on a yesno table"
    );
}

/// # Safety
/// Called by the executor.
#[pg_guard]
pub unsafe extern "C-unwind" fn tuple_insert_speculative(
    _rel: pg_sys::Relation,
    _slot: *mut pg_sys::TupleTableSlot,
    _cid: pg_sys::CommandId,
    _options: core::ffi::c_int,
    _bistate: *mut pg_sys::BulkInsertStateData,
    _spec_token: u32,
) {
    error!("yesno_tam: INSERT … ON CONFLICT is not supported on a yesno table");
}

/// # Safety
/// Called by the executor.
#[pg_guard]
pub unsafe extern "C-unwind" fn tuple_complete_speculative(
    _rel: pg_sys::Relation,
    _slot: *mut pg_sys::TupleTableSlot,
    _spec_token: u32,
    _succeeded: bool,
) {
    error!("yesno_tam: INSERT … ON CONFLICT is not supported on a yesno table");
}

/// # Safety
/// Called by the executor.
#[pg_guard]
pub unsafe extern "C-unwind" fn relation_copy_data(
    _rel: pg_sys::Relation,
    _newrlocator: *const pg_sys::RelFileLocator,
) {
    error!("yesno_tam: ALTER TABLE … SET TABLESPACE is not supported on a yesno table");
}

/// # Safety
/// Called by the executor.
#[pg_guard]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C-unwind" fn relation_copy_for_cluster(
    _old: pg_sys::Relation,
    _new: pg_sys::Relation,
    _old_index: pg_sys::Relation,
    _use_sort: bool,
    _oldest_xmin: pg_sys::TransactionId,
    _xid_cutoff: *mut pg_sys::TransactionId,
    _multi_cutoff: *mut pg_sys::MultiXactId,
    _num_tuples: *mut f64,
    _tups_vacuumed: *mut f64,
    _tups_recently_dead: *mut f64,
) {
    error!("yesno_tam: CLUSTER and VACUUM FULL are not supported on a yesno table");
}

// ── DDL and maintenance ─────────────────────────────────────────────────────

/// # Safety
/// Called by the executor with valid pointers.
#[pg_guard]
pub unsafe extern "C-unwind" fn relation_set_new_filelocator(
    _rel: pg_sys::Relation,
    newrlocator: *const pg_sys::RelFileLocator,
    persistence: core::ffi::c_char,
    freeze_xid: *mut pg_sys::TransactionId,
    minmulti: *mut pg_sys::MultiXactId,
) {
    // **An empty physical fork is still created**, even though nothing is
    // ever written to it. PostgreSQL's storage manager is consulted before this
    // AM is — `get_relation_info` asks smgr for the relation's block count
    // during planning — and a relation with no file crashes the backend there,
    // before a single table-AM callback runs. ( Observed exactly that: a SELECT
    // died with no callback of ours reached. )
    //
    // Do not remove this on the grounds that the data lives in yesno. The
    // fork is what makes the relation exist to the rest of PostgreSQL.
    unsafe {
        let srel = pg_sys::RelationCreateStorage(*newrlocator, persistence, true);
        pg_sys::smgrclose(srel);
    }

    // **This is where TRUNCATE actually lands.** A transactional `TRUNCATE`
    // does not call `relation_nontransactional_truncate` — it gives the relation
    // a **new relfilenode** through this callback and lets the old storage be
    // dropped. A yesno table's storage is keyed by the relation's OID, which
    // does not change, so without clearing here the rows survive a `TRUNCATE`
    // and the table reports them again. ( Observed exactly that: 7 rows before,
    // 7 rows after. )
    //
    // At `CREATE TABLE` the key is empty and this is a no-op, so one path serves
    // both — which is the honest reading of the callback anyway: "give this
    // relation fresh, empty storage".
    unsafe { clear_relation(_rel) };

    // `FrozenTransactionId` and no multixact: this AM stores no xids, so it
    // can never be the reason a wraparound freeze is needed. Reporting anything
    // else would enlist the table in a freezing schedule it cannot participate
    // in — which is the failure mode `tam-mvcc` in `TODO.md` describes.
    unsafe {
        if !freeze_xid.is_null() {
            *freeze_xid = pg_sys::FrozenTransactionId;
        }
        if !minmulti.is_null() {
            //  is a macro ( 0 ) rather than an exported constant.
            *minmulti = pg_sys::TransactionId::from(0u32);
        }
    }
}

/// # Safety
/// Called by the executor.
#[pg_guard]
pub unsafe extern "C-unwind" fn relation_nontransactional_truncate(rel: pg_sys::Relation) {
    unsafe { clear_relation(rel) };
}

/// Remove every ordinal in a relation's key.
///
/// # Safety
///
/// `rel` must be a valid `Relation`.
unsafe fn clear_relation(rel: pg_sys::Relation) {
    let Some((endpoint, key)) = (unsafe { index_target_for_table(rel) }) else {
        return;
    };
    // Read then remove, rather than a "delete key" call: the Flight surface
    // exposes `do_put` in remove mode and nothing that drops a whole key.
    let mut transport = match crate::transport::flight::FlightTransport::new(&endpoint) {
        Ok(t) => t,
        Err(e) => error!("yesno_tam: {e}"),
    };
    let cmd = crate::transport::flight::FlightTransport::key_cmd(key);
    if transport.open_scan(&cmd).is_err() {
        return;
    }
    let mut all = Vec::new();
    while let Ok(Some(b)) = transport.next_batch() {
        all.extend(b.into_iter().map(crate::ordinal::i64_to_ordinal));
    }
    transport.close_scan();
    if let Err(e) = transport.put(key, &all, true) {
        error!("yesno_tam: {e}");
    }
}

/// # Safety
/// Called by VACUUM.
#[pg_guard]
pub unsafe extern "C-unwind" fn relation_vacuum(
    _rel: pg_sys::Relation,
    _params: *mut pg_sys::VacuumParams,
    _bstrategy: pg_sys::BufferAccessStrategy,
) {
    // Nothing to do, and that is a consequence of storing no xids: there are
    // no dead tuples to collect and no `relfrozenxid` to advance. A heap's
    // VACUUM exists mostly to do those two things.
}

/// # Safety
/// Called by ANALYZE.
#[pg_guard]
pub unsafe extern "C-unwind" fn scan_analyze_next_block(
    _scan: pg_sys::TableScanDesc,
    _stream: *mut pg_sys::ReadStream,
) -> bool {
    // One "block" holding everything: the scan already has every ordinal in
    // memory, so there is nothing to page through. Returning true once and then
    // letting `scan_analyze_next_tuple` walk to exhaustion samples the whole
    // table, which is what makes `ANALYZE` produce real statistics here.
    true
}

/// # Safety
/// Called by ANALYZE with valid pointers.
#[pg_guard]
pub unsafe extern "C-unwind" fn scan_analyze_next_tuple(
    scan: pg_sys::TableScanDesc,
    _oldest_xmin: pg_sys::TransactionId,
    liverows: *mut f64,
    _deadrows: *mut f64,
    slot: *mut pg_sys::TupleTableSlot,
) -> bool {
    let got = unsafe { scan_getnextslot(scan, pg_sys::ScanDirection::ForwardScanDirection, slot) };
    if got && !liverows.is_null() {
        unsafe { *liverows += 1.0 };
    }
    got
}

/// # Safety
/// Called by CREATE INDEX with valid pointers.
#[pg_guard]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C-unwind" fn index_build_range_scan(
    table_rel: pg_sys::Relation,
    index_rel: pg_sys::Relation,
    index_info: *mut pg_sys::IndexInfo,
    _allow_sync: bool,
    _anyvisible: bool,
    _progress: bool,
    _start_blockno: pg_sys::BlockNumber,
    _numblocks: pg_sys::BlockNumber,
    callback: pg_sys::IndexBuildCallback,
    callback_state: *mut core::ffi::c_void,
    _scan: pg_sys::TableScanDesc,
) -> f64 {
    let Some(cb) = callback else { return 0.0 };
    let ordinals = unsafe { fetch_all(table_rel) };
    let mut n = 0.0;
    unsafe {
        let slot = pg_sys::MakeSingleTupleTableSlot((*table_rel).rd_att, &pg_sys::TTSOpsVirtual);
        for o in ordinals {
            let Some((block, offset)) = ordinal_to_tid(o) else {
                continue;
            };
            store_ordinal(slot, o);
            let mut tid = crate::iam::tid::make_tid(block, offset);
            let mut values = [pg_sys::Datum::from(crate::ordinal::ordinal_to_i64(o))];
            let mut isnull = [false];
            cb(
                index_rel,
                &mut tid,
                values.as_mut_ptr(),
                isnull.as_mut_ptr(),
                true,
                callback_state,
            );
            n += 1.0;
        }
        let _ = index_info;
        pg_sys::ExecDropSingleTupleTableSlot(slot);
    }
    n
}

/// # Safety
/// Called by CREATE INDEX CONCURRENTLY.
#[pg_guard]
pub unsafe extern "C-unwind" fn index_validate_scan(
    _table_rel: pg_sys::Relation,
    _index_rel: pg_sys::Relation,
    _index_info: *mut pg_sys::IndexInfo,
    _snapshot: pg_sys::Snapshot,
    _state: *mut pg_sys::ValidateIndexState,
) {
    error!("yesno_tam: CREATE INDEX CONCURRENTLY is not supported on a yesno table");
}

// ── Size ────────────────────────────────────────────────────────────────────

/// # Safety
/// Called by the planner.
#[pg_guard]
pub unsafe extern "C-unwind" fn relation_size(
    rel: pg_sys::Relation,
    _fork: pg_sys::ForkNumber::Type,
) -> u64 {
    // Synthetic: two bytes per ordinal, which is what an array container
    // actually costs. There is no file whose length could be reported — the
    // storage is yesno's — and zero would tell the planner the table is empty.
    let n = unsafe { count_rows(rel) };
    n.saturating_mul(2)
}

/// # Safety
/// Called by the planner.
#[pg_guard]
pub unsafe extern "C-unwind" fn relation_needs_toast_table(_rel: pg_sys::Relation) -> bool {
    // Never. A `bigint` is fixed-width, and no varlena is representable in a
    // set of `u64`.
    false
}

/// # Safety
/// Called by the planner with valid pointers.
#[pg_guard]
pub unsafe extern "C-unwind" fn relation_estimate_size(
    rel: pg_sys::Relation,
    _attr_widths: *mut i32,
    pages: *mut pg_sys::BlockNumber,
    tuples: *mut f64,
    allvisfrac: *mut f64,
) {
    // **Exact**, like the foreign data wrapper's: `Snapshot::cardinality`
    // answers from index popcounts without reading an ordinal, so the planner is
    // handed truth rather than an estimate.
    let n = unsafe { count_rows(rel) };
    unsafe {
        *tuples = n as f64;
        *pages = ((n * 2).div_ceil(8192)).max(1) as u32;
        // Every tuple is visible — restriction 1 again.
        *allvisfrac = 1.0;
    }
}

// ── Bitmap and sample scans ─────────────────────────────────────────────────

// Bitmap table scans are not supported yet. Pairing the index and table AMs
// needs per-block answers rather than the whole-set scan state. Returning false
// must remain unreachable: `relation_estimate_size` reports `allvisfrac = 1.0`
// so the planner prefers a sequential scan.

/// # Safety
/// Called by the PostgreSQL 17 executor.
#[cfg(feature = "pg17")]
#[pg_guard]
pub unsafe extern "C-unwind" fn scan_bitmap_next_block(
    _scan: pg_sys::TableScanDesc,
    _tbmres: *mut pg_sys::TBMIterateResult,
) -> bool {
    false
}

/// # Safety
/// Called by the PostgreSQL 17 executor.
#[cfg(feature = "pg17")]
#[pg_guard]
pub unsafe extern "C-unwind" fn scan_bitmap_next_tuple(
    _scan: pg_sys::TableScanDesc,
    _tbmres: *mut pg_sys::TBMIterateResult,
    _slot: *mut pg_sys::TupleTableSlot,
) -> bool {
    false
}

/// # Safety
/// Called by the PostgreSQL 18 executor.
#[cfg(feature = "pg18")]
#[pg_guard]
pub unsafe extern "C-unwind" fn scan_bitmap_next_tuple(
    _scan: pg_sys::TableScanDesc,
    _slot: *mut pg_sys::TupleTableSlot,
    _recheck: *mut bool,
    _lossy_pages: *mut u64,
    _exact_pages: *mut u64,
) -> bool {
    false
}

/// # Safety
/// Called by the executor.
#[pg_guard]
pub unsafe extern "C-unwind" fn scan_sample_next_block(
    _scan: pg_sys::TableScanDesc,
    _scanstate: *mut pg_sys::SampleScanState,
) -> bool {
    error!("yesno_tam: TABLESAMPLE is not supported on a yesno table");
}

/// # Safety
/// Called by the executor.
#[pg_guard]
pub unsafe extern "C-unwind" fn scan_sample_next_tuple(
    _scan: pg_sys::TableScanDesc,
    _scanstate: *mut pg_sys::SampleScanState,
    _slot: *mut pg_sys::TupleTableSlot,
) -> bool {
    error!("yesno_tam: TABLESAMPLE is not supported on a yesno table");
}

/// # Safety
/// Called by the executor.
#[pg_guard]
pub unsafe extern "C-unwind" fn finish_bulk_insert(
    _rel: pg_sys::Relation,
    _options: core::ffi::c_int,
) {
    // Nothing flushed here. The per-transaction buffer flushes at pre-commit;
    // see `crate::fdw::modify`.
}

// ── helpers ─────────────────────────────────────────────────────────────────

/// Every ordinal in the table's key.
///
/// # Safety
/// `rel` must be a yesno table.
unsafe fn fetch_all(rel: pg_sys::Relation) -> Vec<u64> {
    let Some((_endpoint, key)) = (unsafe { index_target_for_table(rel) }) else {
        error!("yesno_tam: this table has no reachable yesno server");
    };
    let mut transport = match unsafe { open_transport_for_table(rel) } {
        Ok(t) => t,
        Err(e) => error!("yesno_tam: {e}"),
    };
    let cmd = crate::transport::flight::FlightTransport::key_cmd(key);
    // Under REPEATABLE READ this reuses the ticket the transaction pinned on
    // its first access. Under READ COMMITTED it reuses a ticket only within the
    // current executor statement, so repeated scans agree without hiding a
    // commit from the next statement. See `fdw::modify::pinned_ticket`.
    let pinned = crate::fdw::modify::pinned_ticket(&_endpoint, key, || {
        transport.ticket_for(&cmd).map_err(|e| e.to_string())
    });
    let opened = match pinned {
        Err(e) => error!("yesno_tam: {e}"),
        Ok(Some(ticket)) => transport.open_scan_with_ticket(&ticket),
        Ok(None) => transport.open_scan(&cmd),
    };
    if let Err(e) = opened {
        error!("yesno_tam: {e}");
    }
    let mut out = Vec::new();
    loop {
        match transport.next_batch() {
            Err(e) => error!("yesno_tam: {e}"),
            Ok(None) => break,
            Ok(Some(b)) => out.extend(b.into_iter().map(crate::ordinal::i64_to_ordinal)),
        }
    }
    // The server holds what was committed *before* this transaction. Its own
    // writes are still buffered, so without this overlay an `INSERT` followed by
    // a `SELECT` in one transaction returns nothing.
    crate::fdw::modify::overlay_pending(&_endpoint, key, out)
}

/// # Safety
/// `rel` must be a yesno table.
unsafe fn count_rows(rel: pg_sys::Relation) -> u64 {
    let Some((_endpoint, key)) = (unsafe { index_target_for_table(rel) }) else {
        return 0;
    };
    // The cheap exact count is exact only while this transaction has written
    // nothing. Once it has, the popcount sum describes the pre-transaction set,
    // so the count has to come from the overlaid scan instead. Do not
    // "optimize" this back to an unconditional `cardinality`: the fast path is
    // still taken in the common case, which is every read-only statement.
    // Also when the transaction has pinned a version: `cardinality` asks the
    // server for a count at whatever is *current*, which is precisely the value
    // a pinned scan must not use. A count that disagrees with the rows the
    // same transaction can see is worse than a slow one.
    if crate::fdw::modify::has_pending(&_endpoint, key)
        || crate::fdw::modify::has_pinned(&_endpoint, key)
    {
        return unsafe { fetch_all(rel) }.len() as u64;
    }
    let Ok(mut transport) = (unsafe { open_transport_for_table(rel) }) else {
        return 0;
    };
    let cmd = crate::transport::flight::FlightTransport::key_cmd(key);
    transport.cardinality(&cmd).unwrap_or(0)
}

/// # Safety
/// `slot` must be a virtual slot with at least one attribute.
unsafe fn store_ordinal(slot: *mut pg_sys::TupleTableSlot, ordinal: u64) {
    unsafe {
        clear(slot);
        *(*slot).tts_values.add(0) = pg_sys::Datum::from(crate::ordinal::ordinal_to_i64(ordinal));
        *(*slot).tts_isnull.add(0) = false;
        if let Some((block, offset)) = ordinal_to_tid(ordinal) {
            (*slot).tts_tid = crate::iam::tid::make_tid(block, offset);
        }
        pg_sys::ExecStoreVirtualTuple(slot);
    }
}

/// # Safety
/// `slot` must be a valid slot.
unsafe fn clear(slot: *mut pg_sys::TupleTableSlot) {
    unsafe {
        if let Some(f) = (*(*slot).tts_ops).clear {
            f(slot);
        }
    }
}

/// # Safety
/// `tid` must be valid.
unsafe fn ordinal_of_tid(tid: pg_sys::ItemPointer) -> Option<u64> {
    if tid.is_null() {
        return None;
    }
    let (block, offset) = unsafe { crate::iam::tid::split_tid(tid) };
    tid_to_ordinal(block, offset)
}

/// # Safety
/// `tid` may be null, in which case `dflt` is used.
unsafe fn tid_bound(tid: pg_sys::ItemPointer, dflt: u64) -> u64 {
    unsafe { ordinal_of_tid(tid) }.unwrap_or(dflt)
}

/// # Safety
/// Called with valid pointers.
unsafe fn fetch_tid_into(
    rel: pg_sys::Relation,
    tid: pg_sys::ItemPointer,
    slot: *mut pg_sys::TupleTableSlot,
) -> bool {
    let Some(o) = (unsafe { ordinal_of_tid(tid) }) else {
        unsafe { clear(slot) };
        return false;
    };
    // Membership is checked, not assumed. A TID handed back by an index may
    // name a row that has since been deleted, and synthesising its value from
    // the TID alone would resurrect it.
    let Some((_endpoint, key)) = (unsafe { index_target_for_table(rel) }) else {
        unsafe { clear(slot) };
        return false;
    };
    let Ok(mut transport) = (unsafe { open_transport_for_table(rel) }) else {
        unsafe { clear(slot) };
        return false;
    };
    let expr = yesno_wire::SetExpr::And(vec![
        yesno_wire::SetExpr::Key(key),
        yesno_wire::SetExpr::Range(o, o + 1),
    ]);
    let present = transport.cardinality(&expr.encode()).unwrap_or(0) > 0;
    if present {
        unsafe { store_ordinal(slot, o) };
    } else {
        unsafe { clear(slot) };
    }
    present
}

/// # Safety
/// `slot` must hold at least one attribute.
unsafe fn ordinal_from_slot(slot: *mut pg_sys::TupleTableSlot) -> u64 {
    unsafe {
        let natts = (*(*slot).tts_tupleDescriptor).natts as usize;
        if natts < 1 {
            error!("yesno_tam: a yesno table has exactly one bigint column");
        }
        if ((*slot).tts_nvalid as usize) < natts {
            pg_sys::slot_getsomeattrs_int(slot, natts as core::ffi::c_int);
        }
        if *(*slot).tts_isnull.add(0) {
            error!("yesno_tam: the ordinal column must not be NULL");
        }
        let raw = (*(*slot).tts_values.add(0)).value() as i64;
        let o = crate::ordinal::i64_to_ordinal(raw);
        if o > TAM_ORDINAL_MAX {
            error!(
                "yesno_tam: {raw} is outside a yesno table's domain. The tuple is \
                 its own TID, so the value must fit a ( block, offset ) pair — at \
                 most {TAM_ORDINAL_MAX}."
            );
        }
        o
    }
}
