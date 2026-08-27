//! The `TableAmRoutine`: `CREATE TABLE … USING yesno`.
//!
//! # This is not a general heap, and the two restrictions are the design
//!
//! A yesno table is a **single-column `bigint` set**. That is not a subset of a
//! heap that could be widened later; it follows from what yesno stores.
//!
//! ## 1. Visibility does not come from stored xids
//!
//! PostgreSQL evaluates visibility from `xmin`/`xmax` per tuple. yesno has
//! nowhere to put them — it stores a set of `u64`, with no per-element value
//! storage — so this AM does not evaluate visibility per tuple at all.
//!
//! What that buys: a `ROLLBACK` that is correct for both `INSERT` and `DELETE`,
//! because writes are buffered to pre-commit and an abort simply discards the
//! buffer. What it avoids: freezing, `relfrozenxid`, and the cluster-wide
//! wraparound shutdown that a relation storing real xids and failing to advance
//! `relfrozenxid` eventually causes.
//!
//! **A transaction reads its own writes through an overlay, not through the
//! server.** Buffered writes are applied on top of what a scan fetched — see
//! `fdw::modify::overlay_pending`. Without that overlay `BEGIN; INSERT …;
//! SELECT …;` returns nothing, and a `ROLLBACK` test cannot detect it: that test
//! asserts the row is *absent* afterwards, which is equally true of a write that
//! was never visible at all.
//!
//! **`REPEATABLE READ` is honoured by pinning a Flight ticket for the
//! transaction.** A ticket records the version it was minted at and the server
//! answers at *that* version, so holding one across statements is what makes a
//! stable snapshot expressible without storing anything per tuple. The pin is
//! taken on first access and released by the transaction callback — on commit
//! as well as abort.
//!
//! **The pin lifetime is conditional on the isolation level, and that is
//! required rather than an optimization.** `READ COMMITTED` promises each
//! *statement* a fresh snapshot, so its pin lives from `ExecutorStart` through
//! `ExecutorEnd`: repeated scans in one statement agree, while the next
//! statement may see another session's newer commit. `REPEATABLE READ` and
//! `SERIALIZABLE` keep their pin until the transaction callback instead.
//!
//! **What it does not give, and no care inside this file can:** a yesno table
//! and a heap table in the same query can **tear**. yesno commits in the
//! pre-commit hook; PostgreSQL commits later, when it writes its commit record.
//! In that window another backend's PostgreSQL snapshot can exclude a
//! transaction whose yesno rows it already sees — so a join returns one side's
//! rows and not the other's, with no error. Storing xids is what would fix it,
//! by putting yesno on PostgreSQL's clock; see `tam-mvcc` in `TODO.md`.
//!
//! ## 2. The tuple **is** its TID, so the domain caps at ~2^42
//!
//! There is nowhere to store a value separately from its identity, so the value
//! is recovered from the TID by arithmetic. A TID is `( u32 block, offset )` and
//! the offset must be one PostgreSQL accepts, which bounds the domain — see
//! [`super::tid`]. An insert above the cap is rejected, not truncated.
//!
//! # What is rejected outright
//!
//! `UPDATE`, `SELECT … FOR UPDATE`, upsert, `CLUSTER`, `ALTER TABLE … SET
//! TABLESPACE`, and TOAST. Each errors with the reason rather than doing
//! something approximate.

use pgrx::prelude::*;
use pgrx::PgBox;

use super::exec;

const V1_API: pg_sys::Pg_finfo_record = pg_sys::Pg_finfo_record { api_version: 1 };

#[no_mangle]
#[doc(hidden)]
pub extern "C" fn pg_finfo_yesno_tam_handler() -> &'static pg_sys::Pg_finfo_record {
    &V1_API
}

/// The routine, built once per backend and never freed.
///
/// **A table AM's routine must outlive the memory context the handler is
/// called in, and this is not a detail.** `RelationInitTableAccessMethod` stores
/// what the handler returns straight into `rd_tableam` and never copies it —
/// heapam returns `&heapam_methods`, a `static`. An `IndexAmRoutine` is
/// different: `GetIndexAmRoutineByAmId` copies it into `CacheMemoryContext`,
/// which is why `super::super::iam` can palloc one safely and this cannot.
///
/// Allocating in the current context therefore leaves `rd_tableam` dangling as
/// soon as that context resets, and the next relcache use segfaults — observed
/// as a SIGSEGV in `SELECT` with `slot_callbacks` the last callback reached.
///
/// Do not "simplify" this back to a plain `alloc_node`.
static ROUTINE: std::sync::atomic::AtomicPtr<pg_sys::TableAmRoutine> =
    std::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

/// Returns the `TableAmRoutine` PostgreSQL dispatches through.
#[pg_guard]
#[no_mangle]
pub extern "C-unwind" fn yesno_tam_handler(_fcinfo: pg_sys::FunctionCallInfo) -> pg_sys::Datum {
    use std::sync::atomic::Ordering;

    // Built once per backend. PostgreSQL is single-threaded per backend, so the
    // race this `AtomicPtr` guards against cannot occur — it is used for its
    // interior mutability in a `static`, not for synchronisation.
    let cached = ROUTINE.load(Ordering::Relaxed);
    if !cached.is_null() {
        return pg_sys::Datum::from(cached);
    }

    // `TopMemoryContext` lives for the backend's whole life, which is the
    // lifetime `rd_tableam` requires.
    let old = unsafe { pg_sys::MemoryContextSwitchTo(pg_sys::TopMemoryContext) };
    let mut r =
        unsafe { PgBox::<pg_sys::TableAmRoutine>::alloc_node(pg_sys::NodeTag::T_TableAmRoutine) };

    // A **virtual** slot. The tuple is synthesized from its TID by
    // arithmetic, so there is nothing to deform and no heap tuple to build —
    // which is also why `tuple_fetch_row_version` can answer without any I/O.
    r.slot_callbacks = Some(exec::slot_callbacks);

    r.scan_begin = Some(exec::scan_begin);
    r.scan_end = Some(exec::scan_end);
    r.scan_rescan = Some(exec::scan_rescan);
    r.scan_getnextslot = Some(exec::scan_getnextslot);

    // A TID range maps onto an ordinal range exactly, because the tuple is
    // its TID. This is the one place where a yesno table is *better* than a heap
    // at something: `Expr::Range` answers it without scanning.
    r.scan_set_tidrange = Some(exec::scan_set_tidrange);
    r.scan_getnextslot_tidrange = Some(exec::scan_getnextslot_tidrange);

    r.parallelscan_estimate = Some(exec::parallelscan_estimate);
    r.parallelscan_initialize = Some(exec::parallelscan_initialize);
    r.parallelscan_reinitialize = Some(exec::parallelscan_reinitialize);

    r.index_fetch_begin = Some(exec::index_fetch_begin);
    r.index_fetch_reset = Some(exec::index_fetch_reset);
    r.index_fetch_end = Some(exec::index_fetch_end);
    r.index_fetch_tuple = Some(exec::index_fetch_tuple);

    r.tuple_fetch_row_version = Some(exec::tuple_fetch_row_version);
    r.tuple_tid_valid = Some(exec::tuple_tid_valid);
    r.tuple_get_latest_tid = Some(exec::tuple_get_latest_tid);
    r.tuple_satisfies_snapshot = Some(exec::tuple_satisfies_snapshot);
    r.index_delete_tuples = Some(exec::index_delete_tuples);

    r.tuple_insert = Some(exec::tuple_insert);
    r.tuple_insert_speculative = Some(exec::tuple_insert_speculative);
    r.tuple_complete_speculative = Some(exec::tuple_complete_speculative);
    r.multi_insert = Some(exec::multi_insert);
    r.tuple_delete = Some(exec::tuple_delete);
    r.tuple_update = Some(exec::tuple_update);
    r.tuple_lock = Some(exec::tuple_lock);
    r.finish_bulk_insert = Some(exec::finish_bulk_insert);

    r.relation_set_new_filelocator = Some(exec::relation_set_new_filelocator);
    r.relation_nontransactional_truncate = Some(exec::relation_nontransactional_truncate);
    r.relation_copy_data = Some(exec::relation_copy_data);
    r.relation_copy_for_cluster = Some(exec::relation_copy_for_cluster);
    r.relation_vacuum = Some(exec::relation_vacuum);
    r.scan_analyze_next_block = Some(exec::scan_analyze_next_block);
    r.scan_analyze_next_tuple = Some(exec::scan_analyze_next_tuple);
    r.index_build_range_scan = Some(exec::index_build_range_scan);
    r.index_validate_scan = Some(exec::index_validate_scan);

    r.relation_size = Some(exec::relation_size);
    r.relation_needs_toast_table = Some(exec::relation_needs_toast_table);
    r.relation_estimate_size = Some(exec::relation_estimate_size);

    #[cfg(feature = "pg17")]
    {
        r.scan_bitmap_next_block = Some(exec::scan_bitmap_next_block);
        r.scan_bitmap_next_tuple = Some(exec::scan_bitmap_next_tuple);
    }
    #[cfg(feature = "pg18")]
    {
        r.scan_bitmap_next_tuple = Some(exec::scan_bitmap_next_tuple);
    }
    r.scan_sample_next_block = Some(exec::scan_sample_next_block);
    r.scan_sample_next_tuple = Some(exec::scan_sample_next_tuple);

    let p = r.into_pg();
    unsafe { pg_sys::MemoryContextSwitchTo(old) };
    ROUTINE.store(p, Ordering::Relaxed);
    pg_sys::Datum::from(p)
}
