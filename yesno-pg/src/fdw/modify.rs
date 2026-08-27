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
//! # Subtransactions
//!
//! Buffering per transaction is not enough on its own, and the gap was a
//! **silent wrong answer in both directions** until 2026-09-19: writes made
//! after a rolled-back `SAVEPOINT` were still flushed at commit, and a `DELETE`
//! rolled back the same way was still applied, so a row the user had restored
//! disappeared. It needed no `SAVEPOINT` in the text either — a PL/pgSQL block
//! with an `EXCEPTION` clause opens an implicit subtransaction.
//!
//! The buffer is therefore a **stack of levels** rather than one map, keyed by
//! `GetCurrentTransactionNestLevel`, and this module registers a subtransaction
//! callback beside the transaction one. `ROLLBACK TO` drops every level above
//! the savepoint and the parent's entries are untouched; `RELEASE` merges the
//! level into its parent with the deeper verdict winning. Reads flatten the
//! stack the same way, so a transaction never sees what it rolled back.
//!
//! **`RELEASE` keeping its writes is as load-bearing as the rollback dropping
//! them**: a version that discarded on every subtransaction end would satisfy
//! the rollback cases and lose committed work. Both directions are in
//! `e2e/postgresql/sql/fdw_savepoint.sql`.
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
    /// One entry per transaction nesting level that has written, ascending by
    /// `depth` and at most one entry per depth.
    ///
    /// **A stack rather than one map, because a subtransaction must be able to
    /// be undone.** Tagging each ordinal with the subtransaction that wrote it
    /// does not work, and the case that proves it is a `DELETE` inside a
    /// savepoint of a row the outer transaction inserted: last-write-wins has
    /// already replaced the parent's verdict by the time the rollback arrives,
    /// so there is nothing left to restore. Keeping one map per level preserves
    /// the parent's entry underneath the child's, and dropping the child's map
    /// re-exposes it.
    levels: Vec<Level>,
}

/// One nesting level's writes. `depth` is `GetCurrentTransactionNestLevel()` as
/// it stood when this level first wrote: 1 is the top level, 2 is inside one
/// savepoint, and so on.
struct Level {
    depth: i32,
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
    ///
    /// Last-write-wins is exactly why this map is **per level**: within one
    /// level it is what SQL means, and across levels it would destroy the
    /// parent's verdict that a rollback has to restore.
    by_target: HashMap<(String, u64), HashMap<u64, bool>>,
}

impl Pending {
    fn new() -> Self {
        Pending { levels: Vec::new() }
    }

    /// The map to write into at `depth`, created if this level has not written.
    fn level_mut(&mut self, depth: i32) -> &mut HashMap<(String, u64), HashMap<u64, bool>> {
        match self.levels.binary_search_by_key(&depth, |l| l.depth) {
            Ok(i) => &mut self.levels[i].by_target,
            Err(i) => {
                self.levels.insert(
                    i,
                    Level {
                        depth,
                        by_target: HashMap::new(),
                    },
                );
                &mut self.levels[i].by_target
            }
        }
    }

    /// Every buffered write for one target, deeper levels winning.
    ///
    /// Ascending order is the whole of that rule: a later `extend` overwrites,
    /// so the innermost surviving level's verdict is the one that remains.
    fn flattened(&self, target: &(String, u64)) -> HashMap<u64, bool> {
        let mut out: HashMap<u64, bool> = HashMap::new();
        for level in &self.levels {
            if let Some(writes) = level.by_target.get(target) {
                out.extend(writes.iter().map(|(&o, &removed)| (o, removed)));
            }
        }
        out
    }

    /// Whether any level holds a write for this target.
    fn any_write_for(&self, target: &(String, u64)) -> bool {
        self.levels
            .iter()
            .any(|l| l.by_target.get(target).is_some_and(|w| !w.is_empty()))
    }

    /// Everything, flattened per target, leaving the buffer empty.
    fn drain_flattened(&mut self) -> Vec<((String, u64), HashMap<u64, bool>)> {
        let mut out: HashMap<(String, u64), HashMap<u64, bool>> = HashMap::new();
        for level in self.levels.drain(..) {
            for (target, writes) in level.by_target {
                out.entry(target).or_default().extend(writes);
            }
        }
        out.into_iter().collect()
    }

    /// A subtransaction aborted: discard everything it and anything inside it
    /// wrote. The parent's entries are untouched, which is what restores a row
    /// the child deleted.
    fn discard_from(&mut self, depth: i32) {
        self.levels.retain(|l| l.depth < depth);
    }

    /// A subtransaction committed: its writes now belong to its parent.
    fn merge_down(&mut self, depth: i32) {
        let split = self.levels.partition_point(|l| l.depth < depth);
        let moved: Vec<Level> = self.levels.split_off(split);
        if moved.is_empty() {
            return;
        }
        // `COMMIT_SUB` cannot fire at the top level, so the parent depth is at
        // least 1; the guard is for the impossible case rather than a real one.
        let parent = if depth > 1 { depth - 1 } else { 1 };
        let at = match self.levels.binary_search_by_key(&parent, |l| l.depth) {
            Ok(i) => i,
            Err(i) => {
                self.levels.insert(
                    i,
                    Level {
                        depth: parent,
                        by_target: HashMap::new(),
                    },
                );
                i
            }
        };
        for level in moved {
            for (target, writes) in level.by_target {
                self.levels[at]
                    .by_target
                    .entry(target)
                    .or_default()
                    .extend(writes);
            }
        }
    }

    fn clear(&mut self) {
        self.levels.clear();
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
    ensure_xact_callbacks();
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
        let writes = p.flattened(&(endpoint.to_string(), key));
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
    PENDING.with(|p| p.borrow().any_write_for(&(endpoint.to_string(), key)))
}

/// Buffer one `( key, ordinal )` against an endpoint, for flush at pre-commit.
///
/// Shared with the index access method rather than duplicated there. Both
/// write postings into yesno and both need the same two properties: a
/// `ROLLBACK` must leave nothing behind, and a multi-row statement must cost one
/// server commit rather than one per row. A second buffer would have its own
/// transaction callback and the two could flush in either order.
pub fn buffer_ordinal(endpoint: String, key: u64, ordinal: u64, remove: bool) {
    ensure_xact_callbacks();
    // The level is read here rather than tracked through `SUBXACT_EVENT_START_SUB`,
    // which matters because these callbacks register lazily on the first write:
    // a transaction whose first write happens *inside* a savepoint never saw
    // that savepoint start, and a design needing the start event would already
    // have lost its place.
    let depth = unsafe { pg_sys::GetCurrentTransactionNestLevel() };
    PENDING.with(|p| {
        let mut p = p.borrow_mut();
        let slot = p.level_mut(depth).entry((endpoint, key)).or_default();
        slot.insert(ordinal, remove);
    });
}

/// Register the transaction and subtransaction callbacks once per backend.
///
/// **Both, or the buffer is wrong rather than merely incomplete.** The
/// top-level callback alone was the state until 2026-09-19: PostgreSQL delivers
/// a subtransaction abort *only* through `RegisterSubXactCallback`, so nothing
/// discarded the writes a rolled-back savepoint made and `PRE_COMMIT` sent them
/// — and a `DELETE` rolled back the same way was still applied, losing a row
/// the user had restored. `e2e/postgresql/sql/fdw_savepoint.sql` is the
/// falsifier and covers both directions.
fn ensure_xact_callbacks() {
    REGISTERED.with(|r| {
        let mut r = r.borrow_mut();
        if *r {
            return;
        }
        // SAFETY: registering a callback is valid at any point in a backend's
        // life, and this runs exactly once.
        unsafe {
            pg_sys::RegisterXactCallback(Some(xact_callback), core::ptr::null_mut());
            pg_sys::RegisterSubXactCallback(Some(subxact_callback), core::ptr::null_mut());
        }
        *r = true;
    });
}

/// Discard a rolled-back subtransaction's writes; hand a committed one's to its
/// parent.
///
/// # Why the nesting level rather than the subtransaction id
///
/// PostgreSQL runs this while the ending subtransaction is still current, so
/// `GetCurrentTransactionNestLevel` names the level that is ending and every
/// buffered level at or below it belongs to it. Matching on the id would need
/// `SUBXACT_EVENT_START_SUB` to have been seen, and these callbacks register on
/// the first *write* — a transaction whose first write happens inside a
/// savepoint never saw that savepoint begin.
///
/// `PINNED` and `STATEMENT` are deliberately left alone. A ticket pinned inside
/// an aborted subtransaction stays pinned until the transaction ends, which
/// over-retains a version and cannot serve wrong data; the same treatment would
/// be tidier and buys no correctness.
///
/// # Safety
///
/// Called by PostgreSQL's transaction machinery.
unsafe extern "C-unwind" fn subxact_callback(
    event: pg_sys::SubXactEvent::Type,
    _my_subid: pg_sys::SubTransactionId,
    _parent_subid: pg_sys::SubTransactionId,
    _arg: *mut core::ffi::c_void,
) {
    // SAFETY: valid inside a transaction, which is the only time PostgreSQL
    // delivers a subtransaction event.
    let depth = unsafe { pg_sys::GetCurrentTransactionNestLevel() };
    match event {
        pg_sys::SubXactEvent::SUBXACT_EVENT_ABORT_SUB => {
            PENDING.with(|p| p.borrow_mut().discard_from(depth));
        }
        pg_sys::SubXactEvent::SUBXACT_EVENT_COMMIT_SUB => {
            PENDING.with(|p| p.borrow_mut().merge_down(depth));
        }
        _ => {}
    }
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
            PENDING.with(|p| p.borrow_mut().clear());
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
        ensure_xact_callbacks();
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
    ensure_xact_callbacks();
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
        PENDING.with(|p| p.borrow_mut().drain_flattened());
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
        for (o, removed) in p.flattened(&(endpoint.to_string(), key)) {
            if removed {
                rem.insert(o);
            } else {
                ins.insert(o);
            }
        }
        (ins, rem)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const EP: &str = "http://127.0.0.1:1";

    fn target() -> (String, u64) {
        (EP.to_string(), 42)
    }

    fn write(p: &mut Pending, depth: i32, ordinal: u64, remove: bool) {
        p.level_mut(depth)
            .entry(target())
            .or_default()
            .insert(ordinal, remove);
    }

    /// What a flatten yields, as `( present, absent )`, which is the shape both
    /// the read overlay and the flush consume.
    fn settled(p: &Pending) -> (Vec<u64>, Vec<u64>) {
        let mut present: Vec<u64> = Vec::new();
        let mut absent: Vec<u64> = Vec::new();
        let flat = p.flattened(&target());
        let mut keys: Vec<u64> = flat.keys().copied().collect();
        keys.sort_unstable();
        for o in keys {
            if flat[&o] {
                absent.push(o);
            } else {
                present.push(o);
            }
        }
        (present, absent)
    }

    /// The case a per-entry subtransaction tag cannot express, and the reason
    /// this is a stack: the parent's insert has to survive the child's delete
    /// being discarded, which needs the parent's verdict to still exist.
    #[test]
    fn discarding_a_level_restores_the_parents_verdict() {
        let mut p = Pending::new();
        write(&mut p, 1, 10, false);
        write(&mut p, 2, 10, true);
        assert_eq!(
            settled(&p),
            (vec![], vec![10]),
            "the child's delete rules while it lives"
        );
        p.discard_from(2);
        assert_eq!(
            settled(&p),
            (vec![10], vec![]),
            "the parent's insert must come back"
        );
    }

    /// `ROLLBACK TO` a savepoint that is not the innermost. PostgreSQL aborts
    /// every level from the innermost down to the named one, so the handler is
    /// exercised once per level -- and must also be correct if it is reached
    /// only once for the outer level.
    #[test]
    fn discarding_an_outer_level_takes_every_level_inside_it() {
        let mut p = Pending::new();
        write(&mut p, 1, 1, false);
        write(&mut p, 2, 2, false);
        write(&mut p, 3, 3, false);
        p.discard_from(3);
        p.discard_from(2);
        assert_eq!(settled(&p), (vec![1], vec![]));

        let mut once = Pending::new();
        write(&mut once, 1, 1, false);
        write(&mut once, 2, 2, false);
        write(&mut once, 3, 3, false);
        once.discard_from(2);
        assert_eq!(
            settled(&once),
            (vec![1], vec![]),
            "one call must cover the levels inside"
        );
    }

    /// A level dropped by a rollback is re-established by the savepoint's new
    /// subtransaction, and must be usable again at the same depth.
    #[test]
    fn a_level_can_be_rebuilt_after_it_is_discarded() {
        let mut p = Pending::new();
        write(&mut p, 1, 100, false);
        write(&mut p, 2, 110, false);
        p.discard_from(2);
        write(&mut p, 2, 130, false);
        p.discard_from(2);
        write(&mut p, 2, 140, false);
        p.merge_down(2);
        assert_eq!(settled(&p), (vec![100, 140], vec![]));
    }

    /// `RELEASE` keeps the writes, and a fix that discarded on every
    /// subtransaction end would pass every rollback test and lose committed
    /// work here.
    #[test]
    fn merging_a_level_down_keeps_its_writes_and_lets_the_deeper_one_win() {
        let mut p = Pending::new();
        write(&mut p, 1, 5, false);
        write(&mut p, 2, 5, true);
        write(&mut p, 2, 6, false);
        p.merge_down(2);
        assert_eq!(p.levels.len(), 1, "the child's level is gone");
        assert_eq!(
            settled(&p),
            (vec![6], vec![5]),
            "the deeper verdict wins on 5"
        );
    }

    /// Releasing an outer savepoint releases the nested ones with it, so a
    /// single merge has to collect every level at or below its depth.
    #[test]
    fn merging_an_outer_level_collects_the_levels_inside_it() {
        let mut p = Pending::new();
        write(&mut p, 1, 200, false);
        write(&mut p, 2, 210, false);
        write(&mut p, 3, 220, false);
        p.merge_down(2);
        assert_eq!(p.levels.len(), 1);
        assert_eq!(settled(&p), (vec![200, 210, 220], vec![]));
    }

    /// Levels are sparse: a depth that never wrote has no entry, and the
    /// bookkeeping is by depth rather than by position.
    #[test]
    fn a_depth_that_never_wrote_has_no_level() {
        let mut p = Pending::new();
        write(&mut p, 1, 1, false);
        write(&mut p, 4, 4, false);
        assert_eq!(p.levels.len(), 2);
        p.merge_down(4);
        assert_eq!(
            p.levels.iter().map(|l| l.depth).collect::<Vec<_>>(),
            vec![1, 3]
        );
        assert_eq!(settled(&p), (vec![1, 4], vec![]));
    }

    /// The flush sees one map per target, with the same deeper-wins rule the
    /// reads use -- otherwise a transaction could commit something it never
    /// showed itself.
    #[test]
    fn draining_flattens_the_same_way_reading_does() {
        let mut p = Pending::new();
        write(&mut p, 1, 7, false);
        write(&mut p, 2, 7, true);
        write(&mut p, 2, 8, false);
        let read = p.flattened(&target());
        let drained = p.drain_flattened();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].0, target());
        assert_eq!(drained[0].1, read);
        assert!(p.levels.is_empty(), "draining empties the buffer");
    }

    #[test]
    fn a_buffer_with_no_writes_for_a_target_reports_none() {
        let mut p = Pending::new();
        assert!(!p.any_write_for(&target()));
        write(&mut p, 2, 1, false);
        assert!(p.any_write_for(&target()));
        p.discard_from(2);
        assert!(
            !p.any_write_for(&target()),
            "a discarded level leaves nothing behind"
        );
    }
}
