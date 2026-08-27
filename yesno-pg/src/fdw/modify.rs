//! `INSERT` and `DELETE` on a yesno foreign table.
//!
//! # A PostgreSQL transaction is not a yesno transaction
//!
//! This is the defining limitation and it is structural, not an oversight.
//! yesno commits per `WriteBatch`; PostgreSQL commits when it writes its commit
//! record. Nothing lets one roll back the other.
//!
//! What that buys and what it costs:
//!
//! - Rows are **buffered per transaction** and flushed once, in a
//!   `XACT_EVENT_PRE_COMMIT` callback. So a `ROLLBACK` is correct — the buffer
//!   is discarded and nothing was ever sent — and a multi-row statement costs
//!   one server commit rather than one per row, which matters because a yesno
//!   commit appends to the WAL and fsyncs.
//! - A crash **between** that flush and PostgreSQL's commit record leaves the
//!   two disagreeing: yesno has the rows, PostgreSQL does not. This wrapper does
//!   **not** implement `PREPARE`, so there is no two-phase commit and no claim
//!   of one. Recorded in `TODO.md` as `fdw-two-phase-commit`.
//!
//! # `UPDATE` is rejected
//!
//! Changing the only column of a set is a delete plus an insert. Accepting
//! `UPDATE` would hide that from the planner — which costs it the chance to
//! notice, for instance, that the "update" moves a row between posting lists.
//!
//! # The row identity is the ordinal
//!
//! A single-column set has no other candidate, and it is a natural key: a
//! posting list holds each ordinal at most once, by construction.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};

use pgrx::prelude::*;

use super::scan::options_for_pub;
use crate::options::Transport as TransportKind;
use crate::transport::flight::FlightTransport;
use crate::transport::Transport;

/// Pending writes for the current transaction, keyed by yesno key.
///
/// Thread-local rather than in the `ResultRelInfo`'s `ri_FdwState`, because
/// the buffer must outlive the `ModifyTable` node: a transaction can contain
/// several statements, and flushing per statement would give up the atomicity
/// the buffering exists to provide.
struct Pending {
    /// `( endpoint, key ) -> ordinal -> was the last write a removal?`
    ///
    /// **A map, last write wins, rather than an insert list and a remove
    /// list.** Two lists cannot represent both orders. Applying removals first
    /// makes `DELETE` then `INSERT` end present — correct — but leaves
    /// `INSERT` then `DELETE` present too, which is the opposite of what the
    /// statements said; applying insertions first just moves the error onto the
    /// other case. Neither order is right because the ordering information has
    /// already been thrown away by the time `flush` runs. Keyed by ordinal, the
    /// last write is the one that survives, which is what SQL means, and
    /// re-inserting a value the set already holds stays idempotent.
    by_target: HashMap<(String, u64), HashMap<u64, bool>>,
}

impl Pending {
    fn new() -> Self {
        Pending {
            by_target: HashMap::new(),
        }
    }
}

thread_local! {
    static PENDING: RefCell<Pending> = RefCell::new(Pending::new());
    /// `( endpoint, key ) -> the Flight ticket this transaction pinned`.
    ///
    /// Only populated under `REPEATABLE READ` and above — see
    /// [`pinned_ticket`]. Cleared by the same transaction callback that clears
    /// `PENDING`, which is why pinning must register that callback too: a
    /// read-only transaction never buffers a write, so nothing else would.
    static PINNED: RefCell<HashMap<(String, u64), Vec<u8>>> =
        RefCell::new(HashMap::new());
    /// `( endpoint, key ) -> the Flight ticket this statement pinned`.
    ///
    /// `READ COMMITTED` needs exactly this lifetime: repeated scans inside one
    /// executor invocation share a version, while the next statement starts
    /// with an empty map and may see a newer commit. `depth` makes nested SPI
    /// executor calls part of the outer statement rather than accidentally
    /// clearing its view halfway through.
    static STATEMENT: RefCell<StatementPins> = RefCell::new(StatementPins::new());
    /// Whether the transaction callback has been registered for this backend.
    static REGISTERED: RefCell<bool> = const { RefCell::new(false) };
}

struct StatementPins {
    depth: usize,
    by_target: HashMap<(String, u64), Vec<u8>>,
}

impl StatementPins {
    fn new() -> Self {
        Self {
            depth: 0,
            by_target: HashMap::new(),
        }
    }

    fn start(&mut self) {
        // A non-empty map at depth zero means an earlier executor escaped via
        // PostgreSQL ERROR before `ExecutorEnd`. The transaction callback is
        // the usual cleanup path, but clearing here is the final guard against
        // carrying one statement's snapshot into another.
        if self.depth == 0 {
            self.by_target.clear();
        }
        self.depth += 1;
    }

    fn end(&mut self) {
        // `_PG_init` can install the end hook while CREATE EXTENSION's executor
        // is already running, so its matching start hook was never called.
        // Treat that first unmatched end as cleanup rather than underflow.
        if self.depth == 0 {
            self.by_target.clear();
            return;
        }
        self.depth -= 1;
        if self.depth == 0 {
            self.by_target.clear();
        }
    }

    fn reset(&mut self) {
        self.depth = 0;
        self.by_target.clear();
    }
}

// PostgreSQL is single-threaded within one backend. These globals retain the
// hook chain installed before yesno_pg was loaded; `_PG_init` assigns them once
// and the library remains loaded until backend exit.
static mut PREV_EXECUTOR_START: pg_sys::ExecutorStart_hook_type = None;
static mut PREV_EXECUTOR_END: pg_sys::ExecutorEnd_hook_type = None;

/// Install the statement boundary hooks used by `READ COMMITTED` ticket pins.
pub fn init() {
    // SAFETY: `_PG_init` runs once when this shared library is loaded in a
    // backend. Saving and chaining the existing hooks is PostgreSQL's required
    // hook protocol, and the function signatures are supplied by `pg_sys` for
    // the selected server major.
    unsafe {
        PREV_EXECUTOR_START = pg_sys::ExecutorStart_hook;
        PREV_EXECUTOR_END = pg_sys::ExecutorEnd_hook;
        pg_sys::ExecutorStart_hook = Some(executor_start);
        pg_sys::ExecutorEnd_hook = Some(executor_end);
    }
}

/// # Safety
///
/// Called by PostgreSQL with a valid `QueryDesc` and executor flags.
#[pg_guard]
unsafe extern "C-unwind" fn executor_start(
    query_desc: *mut pg_sys::QueryDesc,
    eflags: core::ffi::c_int,
) {
    STATEMENT.with(|s| s.borrow_mut().start());
    // SAFETY: this hook received the arguments from PostgreSQL unchanged. The
    // saved hook, when present, has the identical ABI; otherwise the standard
    // executor entry point is the required continuation.
    unsafe {
        if let Some(previous) = PREV_EXECUTOR_START {
            previous(query_desc, eflags);
        } else {
            pg_sys::standard_ExecutorStart(query_desc, eflags);
        }
    }
}

/// # Safety
///
/// Called by PostgreSQL with the `QueryDesc` whose executor is ending.
#[pg_guard]
unsafe extern "C-unwind" fn executor_end(query_desc: *mut pg_sys::QueryDesc) {
    // SAFETY: as in `executor_start`, the query descriptor and hook ABI come
    // directly from PostgreSQL.
    unsafe {
        if let Some(previous) = PREV_EXECUTOR_END {
            previous(query_desc);
        } else {
            pg_sys::standard_ExecutorEnd(query_desc);
        }
    }
    STATEMENT.with(|s| s.borrow_mut().end());
}

/// Which foreign tables may be written.
///
/// # Safety
///
/// Called by the planner with a valid `Relation`.
#[pg_guard]
pub unsafe extern "C-unwind" fn is_foreign_rel_updatable(
    _rel: pg_sys::Relation,
) -> core::ffi::c_int {
    // Bit positions are `CMD_INSERT`, `CMD_UPDATE`, `CMD_DELETE`. `UPDATE` is
    // absent on purpose — see this module's header.
    (1 << pg_sys::CmdType::CMD_INSERT) | (1 << pg_sys::CmdType::CMD_DELETE)
}

/// Add the row identity a `DELETE` needs.
///
/// # Safety
///
/// Called by the planner with valid pointers.
#[pg_guard]
pub unsafe extern "C-unwind" fn add_foreign_update_targets(
    root: *mut pg_sys::PlannerInfo,
    rtindex: pg_sys::Index,
    _target_rte: *mut pg_sys::RangeTblEntry,
    target_relation: pg_sys::Relation,
) {
    let relid = unsafe { (*target_relation).rd_id };
    let Some(attno) = (unsafe { super::scan::ordinal_attno_pub(relid) }) else {
        // Without a usable `ordinal` column there is nothing to identify a row
        // by; `DELETE` then fails at execution rather than deleting the wrong
        // thing.
        return;
    };
    unsafe {
        let var = pg_sys::makeVar(
            rtindex as core::ffi::c_int,
            attno,
            pg_sys::INT8OID,
            -1,
            pg_sys::Oid::INVALID,
            0,
        );
        pg_sys::add_row_identity_var(root, var, rtindex, c"ordinal".as_ptr());
    }
}

/// # Safety
///
/// Called by the planner with valid pointers.
#[pg_guard]
pub unsafe extern "C-unwind" fn plan_foreign_modify(
    _root: *mut pg_sys::PlannerInfo,
    plan: *mut pg_sys::ModifyTable,
    _result_relation: pg_sys::Index,
    _subplan_index: core::ffi::c_int,
) -> *mut pg_sys::List {
    // Rejected here rather than at execution: the planner is where a user
    // gets a comprehensible error, and `IsForeignRelUpdatable` alone produces
    // "cannot update foreign table", which does not say why.
    if unsafe { (*plan).operation } == pg_sys::CmdType::CMD_UPDATE {
        error!(
            "yesno_fdw: UPDATE is not supported on a yesno foreign table. \
             The ordinal is the row's identity, so changing it is a DELETE \
             followed by an INSERT — write it that way so the intent is visible."
        );
    }
    core::ptr::null_mut()
}

/// # Safety
///
/// Called by the executor with valid pointers.
#[pg_guard]
pub unsafe extern "C-unwind" fn begin_foreign_modify(
    _mtstate: *mut pg_sys::ModifyTableState,
    _rinfo: *mut pg_sys::ResultRelInfo,
    _fdw_private: *mut pg_sys::List,
    _subplan_index: core::ffi::c_int,
    _eflags: core::ffi::c_int,
) {
    ensure_xact_callback();
}

/// # Safety
///
/// Called by the executor with valid pointers.
#[pg_guard]
pub unsafe extern "C-unwind" fn exec_foreign_insert(
    _estate: *mut pg_sys::EState,
    rinfo: *mut pg_sys::ResultRelInfo,
    slot: *mut pg_sys::TupleTableSlot,
    _plan_slot: *mut pg_sys::TupleTableSlot,
) -> *mut pg_sys::TupleTableSlot {
    unsafe { buffer_row(rinfo, slot, false) };
    slot
}

/// # Safety
///
/// Called by the executor with valid pointers.
#[pg_guard]
pub unsafe extern "C-unwind" fn exec_foreign_delete(
    _estate: *mut pg_sys::EState,
    rinfo: *mut pg_sys::ResultRelInfo,
    slot: *mut pg_sys::TupleTableSlot,
    plan_slot: *mut pg_sys::TupleTableSlot,
) -> *mut pg_sys::TupleTableSlot {
    // The value comes from the **plan** slot, not the result slot. A `DELETE`
    // has no new tuple; the row identity added by `AddForeignUpdateTargets`
    // travels in the plan slot as a junk column.
    unsafe { buffer_row(rinfo, plan_slot, true) };
    slot
}

/// # Safety
///
/// Called by the executor with valid pointers.
#[pg_guard]
pub unsafe extern "C-unwind" fn end_foreign_modify(
    _estate: *mut pg_sys::EState,
    _rinfo: *mut pg_sys::ResultRelInfo,
) {
    // Nothing is flushed here. `EndForeignModify` runs at the end of the
    // *statement*, and flushing then would make each statement its own yesno
    // commit — so a two-statement transaction that rolled back would leave the
    // first statement's rows behind.
}

/// Record one row against its table's target.
///
/// # Safety
///
/// Called by the executor with valid pointers.
unsafe fn buffer_row(
    rinfo: *mut pg_sys::ResultRelInfo,
    slot: *mut pg_sys::TupleTableSlot,
    remove: bool,
) {
    let rel = unsafe { (*rinfo).ri_RelationDesc };
    if rel.is_null() {
        error!("yesno_fdw: modify has no relation");
    }
    let relid = unsafe { (*rel).rd_id };
    let (server, table) = match unsafe { options_for_pub(relid) } {
        Ok(v) => v,
        Err(e) => error!("yesno_fdw: {e}"),
    };
    let endpoint = match &server.transport {
        TransportKind::Flight { endpoint } => endpoint.clone(),
        TransportKind::Local { .. } => error!(
            "yesno_fdw: the \"data_dir\" transport cannot write; it is not available at all yet"
        ),
    };

    let Some(attno) = (unsafe { super::scan::ordinal_attno_pub(relid) }) else {
        error!("yesno_fdw: the table has no bigint \"ordinal\" column");
    };

    // `slot_getallattrs` is `static inline` in `executor/tuptable.h`, so
    // bindgen never emits it. Its body is the guard below plus a call to
    // `slot_getsomeattrs_int`, which *is* exported — deforming only when the
    // slot has not already been fully populated.
    let natts = unsafe { (*(*slot).tts_tupleDescriptor).natts } as usize;
    unsafe {
        if ((*slot).tts_nvalid as usize) < natts {
            pg_sys::slot_getsomeattrs_int(slot, natts as core::ffi::c_int);
        }
    }
    let idx = (attno - 1) as usize;
    if idx >= natts {
        error!("yesno_fdw: the ordinal column is not in the tuple");
    }
    // A NULL ordinal is rejected rather than skipped. A posting list is a set
    // of *present* values with no null member, so a NULL here is a query the
    // wrapper cannot honour — and silently dropping the row would make an
    // `INSERT` report success for a row that was never stored.
    if unsafe { *(*slot).tts_isnull.add(idx) } {
        error!("yesno_fdw: the ordinal column must not be NULL");
    }
    let raw = unsafe { (*(*slot).tts_values.add(idx)).value() } as i64;
    let ordinal = crate::ordinal::i64_to_ordinal(raw);
    if ordinal == u64::MAX {
        error!(
            "yesno_fdw: -1 maps to the reserved ordinal 2^64-1, which is not a \
             member of any set"
        );
    }

    buffer_ordinal(endpoint, table.key, ordinal, remove);
}

/// Apply this transaction's buffered writes on top of what the server returned.
///
/// **Without this a transaction cannot read its own writes.** Writes buffer
/// until pre-commit, so a scan that goes straight to the server sees the
/// pre-transaction state: `BEGIN; INSERT …; SELECT …;` returns nothing, and a
/// `DELETE` is invisible to the transaction that issued it until it commits.
///
/// A `ROLLBACK` test does not catch that. It asserts the row is *absent*
/// after the abort, which is equally true of a write that was never visible in
/// the first place — the two failures are indistinguishable from outside, which
/// is why the own-write case is asserted separately.
///
/// Returns ascending, deduplicated: an insert of a value the set already holds
/// is idempotent, which is what makes a set a set.
pub fn overlay_pending(endpoint: &str, key: u64, ordinals: Vec<u64>) -> Vec<u64> {
    PENDING.with(|p| {
        let p = p.borrow();
        let Some(writes) = p.by_target.get(&(endpoint.to_string(), key)) else {
            return ordinals;
        };
        if writes.is_empty() {
            return ordinals;
        }
        let mut set: BTreeSet<u64> = ordinals.into_iter().collect();
        for (&o, &removed) in writes.iter() {
            if removed {
                set.remove(&o);
            } else {
                set.insert(o);
            }
        }
        set.into_iter().collect()
    })
}

/// Whether this transaction has buffered writes against a target.
///
/// The cheap exact count from container popcounts is only exact while the
/// transaction has written nothing. Once it has, the count has to come from the
/// overlaid scan instead — see [`overlay_pending`].
pub fn has_pending(endpoint: &str, key: u64) -> bool {
    PENDING.with(|p| {
        p.borrow()
            .by_target
            .get(&(endpoint.to_string(), key))
            .is_some_and(|w| !w.is_empty())
    })
}

/// Buffer one `( key, ordinal )` against an endpoint, for flush at pre-commit.
///
/// Shared with the index access method rather than duplicated there. Both
/// write postings into yesno and both need the same two properties: a
/// `ROLLBACK` must leave nothing behind, and a multi-row statement must cost one
/// server commit rather than one per row. A second buffer would have its own
/// transaction callback and the two could flush in either order.
pub fn buffer_ordinal(endpoint: String, key: u64, ordinal: u64, remove: bool) {
    ensure_xact_callback();
    PENDING.with(|p| {
        let mut p = p.borrow_mut();
        let slot = p.by_target.entry((endpoint, key)).or_default();
        slot.insert(ordinal, remove);
    });
}

/// Register the transaction callback once per backend.
fn ensure_xact_callback() {
    REGISTERED.with(|r| {
        let mut r = r.borrow_mut();
        if *r {
            return;
        }
        // SAFETY: registering a callback is valid at any point in a backend's
        // life, and this runs exactly once.
        unsafe { pg_sys::RegisterXactCallback(Some(xact_callback), core::ptr::null_mut()) };
        *r = true;
    });
}

/// Flush on commit, discard on abort.
///
/// # Safety
///
/// Called by PostgreSQL's transaction machinery.
unsafe extern "C-unwind" fn xact_callback(
    event: pg_sys::XactEvent::Type,
    _arg: *mut core::ffi::c_void,
) {
    match event {
        // The pin is released on **commit as well as abort**, and forgetting
        // the commit half is not a leak that shows up as a leak: the next
        // transaction in this backend reads through the previous one's ticket
        // and silently reports stale data. Caught by
        // `tam_repeatable_read.spec`'s third read, which is the only assertion
        // that spans two transactions in one session.
        pg_sys::XactEvent::XACT_EVENT_PRE_COMMIT => {
            flush();
            PINNED.with(|p| p.borrow_mut().clear());
            STATEMENT.with(|s| s.borrow_mut().reset());
        }
        // Both abort events, and both must clear. A buffer surviving an abort
        // would be flushed by the *next* transaction to commit in this backend,
        // writing rows the user rolled back.
        pg_sys::XactEvent::XACT_EVENT_ABORT | pg_sys::XactEvent::XACT_EVENT_PARALLEL_ABORT => {
            PENDING.with(|p| p.borrow_mut().by_target.clear());
            PINNED.with(|p| p.borrow_mut().clear());
            STATEMENT.with(|s| s.borrow_mut().reset());
        }
        _ => {}
    }
}

/// The ticket this transaction reads `( endpoint, key )` through, if any.
///
/// # Why this is conditional on the isolation level
///
/// Pinning for the wrong lifetime would be **wrong for `READ COMMITTED`**,
/// whose contract is that each *statement* sees a fresh snapshot. Holding one
/// ticket for the transaction would hide other sessions' commits from a later
/// statement that is entitled to see them. So:
///
/// - `READ COMMITTED` — pin inside one executor statement and clear at its end.
///   Repeated scans agree, and the next statement can mint a newer ticket.
/// - `REPEATABLE READ` and `SERIALIZABLE` — pin on first access and reuse, so
///   every scan in the transaction reads the same version.
pub fn pinned_ticket<F>(endpoint: &str, key: u64, mint: F) -> Result<Option<Vec<u8>>, String>
where
    F: FnOnce() -> Result<Vec<u8>, String>,
{
    // `XactIsoLevel` is the level of the transaction in progress. Values are
    // ordered, so `>=` covers SERIALIZABLE without naming it.
    let k = (endpoint.to_string(), key);
    let transaction_pin = unsafe { pg_sys::XactIsoLevel } >= pg_sys::XACT_REPEATABLE_READ as i32;
    if transaction_pin {
        if let Some(t) = PINNED.with(|p| p.borrow().get(&k).cloned()) {
            return Ok(Some(t));
        }
        // Registered here as well as on the write path. A read-only
        // REPEATABLE READ transaction buffers nothing, so without this the pin
        // would outlive the transaction and the *next* one would read stale.
        ensure_xact_callback();
        let ticket = mint()?;
        PINNED.with(|p| p.borrow_mut().insert(k, ticket.clone()));
        return Ok(Some(ticket));
    }

    if let Some(t) = STATEMENT.with(|s| s.borrow().by_target.get(&k).cloned()) {
        return Ok(Some(t));
    }
    let in_statement = STATEMENT.with(|s| s.borrow().depth != 0);
    if !in_statement {
        return Ok(None);
    }
    // A PostgreSQL ERROR can bypass `ExecutorEnd`. The transaction callback
    // then resets both the nesting depth and this map, so a failed read-only
    // statement cannot leak its ticket into the next transaction.
    ensure_xact_callback();
    let ticket = mint()?;
    STATEMENT.with(|s| s.borrow_mut().by_target.insert(k, ticket.clone()));
    Ok(Some(ticket))
}

/// Whether this transaction has already pinned a version for a target.
pub fn has_pinned(endpoint: &str, key: u64) -> bool {
    let target = (endpoint.to_string(), key);
    PINNED.with(|p| p.borrow().contains_key(&target))
        || STATEMENT.with(|s| s.borrow().by_target.contains_key(&target))
}

/// Send everything buffered, then clear.
fn flush() {
    let work: Vec<((String, u64), HashMap<u64, bool>)> =
        PENDING.with(|p| p.borrow_mut().by_target.drain().collect());
    for ((endpoint, key), writes) in work {
        let mut transport = match FlightTransport::new(&endpoint) {
            Ok(t) => t,
            Err(e) => error!("yesno_fdw: {e}"),
        };
        let (removes, inserts): (Vec<u64>, Vec<u64>) = {
            let mut r = Vec::new();
            let mut i = Vec::new();
            for (&o, &removed) in writes.iter() {
                if removed {
                    r.push(o)
                } else {
                    i.push(o)
                }
            }
            (r, i)
        };
        // An ordinal appears in exactly one of the two lists, because the
        // buffer is keyed by ordinal — so unlike the two-list version this
        // replaced, the order of these two calls no longer decides the outcome.
        // Removals still go first so that a partial failure leaves the set
        // smaller rather than larger.
        if let Err(e) = transport.put(key, &removes, true) {
            error!("yesno_fdw: {e}");
        }
        if let Err(e) = transport.put(key, &inserts, false) {
            error!("yesno_fdw: {e}");
        }
    }
}

/// This transaction's buffered writes for a key, as `( inserts, removes )`.
///
/// Returned as sets rather than applied here, because the foreign scan is a
/// **stream**: removals are a filter applied per batch, and insertions can only
/// be emitted once the server's own rows are exhausted — otherwise an ordinal
/// the server already holds would be emitted twice, and a set has no duplicates.
pub fn pending_sets(endpoint: &str, key: u64) -> (BTreeSet<u64>, BTreeSet<u64>) {
    PENDING.with(|p| {
        let p = p.borrow();
        let mut ins = BTreeSet::new();
        let mut rem = BTreeSet::new();
        if let Some(w) = p.by_target.get(&(endpoint.to_string(), key)) {
            for (&o, &removed) in w.iter() {
                if removed {
                    rem.insert(o);
                } else {
                    ins.insert(o);
                }
            }
        }
        (ins, rem)
    })
}
