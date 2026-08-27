//! The `IndexAmRoutine`: `CREATE INDEX … USING yesno ( col )`.
//!
//! # Only a Bitmap Index Scan, and that is a safety property
//!
//! `amgettuple` is deliberately `NULL` and `amcanorder` deliberately false, so
//! PostgreSQL can only ever reach this AM through a **Bitmap Index Scan**. A
//! `TIDBitmap` is unordered by construction and the heap scan re-sorts by block,
//! which means the ordering hazard that dogs the foreign data wrapper — yesno
//! emits `u64` order, PostgreSQL wants `int8` order, and the two disagree above
//! `2^63` — cannot arise here at all. It is not that ordered scans are
//! unimplemented; it is that offering one would be a way to be wrong.
//!
//! # What the wrapper gains that no built-in AM has
//!
//! `amcostestimate` takes its row count from `Snapshot::cardinality`, which sums
//! container popcounts from the B+tree leaves without decoding a payload extent.
//! So the planner is handed the **exact** number of matching rows rather than an
//! estimate from `pg_statistic`. Every built-in AM estimates.
//!
//! # Crash safety: detected, not provided
//!
//! The index's data lives in yesno, not in a PostgreSQL relation fork, so it
//! is **outside PostgreSQL's WAL**: not covered by PITR, not shipped to a
//! physical standby. This AM does not pretend otherwise. What it does is make
//! divergence *detectable* — see [`super::meta`] — and refuse to operate where
//! it would be silently wrong.

use pgrx::prelude::*;
use pgrx::PgBox;

use super::{cost, scan, vacuum, write};

/// `PG_FUNCTION_INFO_V1` for the handler. Same reasoning as the FDW's: an
/// `index_am_handler` has no Rust type for `#[pg_extern]` to map.
const V1_API: pg_sys::Pg_finfo_record = pg_sys::Pg_finfo_record { api_version: 1 };

#[no_mangle]
#[doc(hidden)]
pub extern "C" fn pg_finfo_yesno_iam_handler() -> &'static pg_sys::Pg_finfo_record {
    &V1_API
}

/// Returns the `IndexAmRoutine` PostgreSQL dispatches through.
#[pg_guard]
#[no_mangle]
pub extern "C-unwind" fn yesno_iam_handler(_fcinfo: pg_sys::FunctionCallInfo) -> pg_sys::Datum {
    let mut r =
        unsafe { PgBox::<pg_sys::IndexAmRoutine>::alloc_node(pg_sys::NodeTag::T_IndexAmRoutine) };

    // One strategy — equality — and no support functions: the key is derived by
    // hashing the datum's text form, not by a per-type support procedure.
    r.amstrategies = 1;
    r.amsupport = 0;
    r.amoptsprocnum = 0;

    // `amcanorder` false and `amgettuple` NULL: see this module's header. The
    // combination is what confines the AM to Bitmap Index Scan.
    r.amcanorder = false;
    r.amcanorderbyop = false;
    r.amcanbackward = false;
    r.amcanunique = false;
    // A yesno key is one value's posting list, so a multi-column index would
    // need a composite key and a way to say which columns a scan constrained.
    r.amcanmulticol = false;
    // False: a scan with no key would mean "every row", which this AM cannot
    // produce — it has no notion of the whole heap, only of keys it was told
    // about.
    r.amoptionalkey = false;
    r.amsearcharray = false;
    // A posting list is a set of *present* values with no null member, so
    // `col IS NULL` can never be answered from one. Claiming otherwise would
    // return no rows for a predicate that has matches.
    r.amsearchnulls = false;
    r.amstorage = false;
    r.amclusterable = false;
    r.ampredlocks = false;
    r.amcanparallel = false;
    r.amcanbuildparallel = false;
    // No index-only scans: the index stores a *hash* of the value, so it
    // cannot reconstruct the tuple it indexed.
    r.amcaninclude = false;
    r.amusemaintenanceworkmem = false;
    r.amsummarizing = false;
    r.amparallelvacuumoptions = pg_sys::VACUUM_OPTION_NO_PARALLEL as u8;
    r.amkeytype = pg_sys::Oid::INVALID;

    r.ambuild = Some(write::ambuild);
    r.ambuildempty = Some(write::ambuildempty);
    r.aminsert = Some(write::aminsert);
    r.ambulkdelete = Some(vacuum::ambulkdelete);
    r.amvacuumcleanup = Some(vacuum::amvacuumcleanup);
    r.amcostestimate = Some(cost::amcostestimate);
    r.amoptions = Some(options_stub);
    r.amvalidate = Some(amvalidate);

    r.ambeginscan = Some(scan::ambeginscan);
    r.amrescan = Some(scan::amrescan);
    r.amgetbitmap = Some(scan::amgetbitmap);
    r.amendscan = Some(scan::amendscan);

    pg_sys::Datum::from(r.into_pg())
}

/// No index-level storage parameters.
///
/// Returning null is "no options", which is different from rejecting one: a
/// `WITH ( … )` clause is caught by `amvalidate`'s opclass check rather than
/// here. The server option naming the endpoint lives on the `SERVER`, which the
/// index reaches through its table.
#[pg_guard]
pub unsafe extern "C-unwind" fn options_stub(
    _reloptions: pg_sys::Datum,
    _validate: bool,
) -> *mut pg_sys::bytea {
    core::ptr::null_mut()
}

/// Accept any opclass. There is one, defined by this extension's SQL.
#[pg_guard]
pub unsafe extern "C-unwind" fn amvalidate(_opclassoid: pg_sys::Oid) -> bool {
    true
}
