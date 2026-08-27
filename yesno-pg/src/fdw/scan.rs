//! The scan callbacks: planning a `Foreign Scan` over one yesno key.
//!
//! # Planning, pushdown, execution
//!
//! `GetForeignRelSize` asks the server for the key's exact cardinality;
//! `GetForeignPlan` lowers what it can of the quals and removes only the clauses
//! it lowered **exactly**; the scan callbacks stream ordinals back.
//!
//! The row estimate deliberately uses the **bare key**, not the lowered
//! quals. `GetForeignRelSize` runs before `GetForeignPlan`, so no pushdown
//! exists yet, and PostgreSQL applies its own selectivity on top of whatever is
//! reported — counting the filtered set here would apply the filter twice.
//!
//! A scan that cannot reach the server must error, never return zero rows: an
//! empty result is indistinguishable from a correct scan over an empty key, and
//! only one of the two is a fact about the data.

use pgrx::prelude::*;

use yesno_wire::SetExpr;

use crate::fdw::qual::{lower_all, Clause};
use crate::fdw::relation_options;
use crate::fdw::walk::walk;
use crate::options::{ServerOptions, TableOptions, Transport as TransportKind};
use crate::transport::flight::FlightTransport;
use crate::transport::{OrdinalBatch, Transport};

/// What a `ForeignScan` should produce.
///
/// Carried through `fdw_private` as an integer rather than inferred from the
/// plan shape. `GetForeignPlan` fires for base relations and upper relations
/// alike and the two are easy to confuse; naming the intent explicitly is what
/// stops an aggregate path from silently streaming rows.
pub const PUSHDOWN_ROWS: i32 = 0;
pub const PUSHDOWN_COUNT_STAR: i32 = 1;
/// A pushed-down join. The scan emits ordinals, like `PUSHDOWN_ROWS`, but from
/// a relation with no range-table entry of its own.
pub const PUSHDOWN_JOIN: i32 = 2;

/// Row count used when the server cannot be reached during planning.
///
/// A fallback, not an estimate anyone tuned. The real number comes from
/// `get_flight_info`'s `total_records`, which yesnod answers **exactly** by
/// summing container popcounts from the B+tree leaves without decoding a
/// payload extent — so a yesno foreign table hands the planner a true row count
/// rather than a guess, which is unusual among foreign data wrappers.
///
/// Do not tune this. If it is ever the number in play, the interesting fact
/// is that the server was unreachable, and that is what the warning says.
const UNREACHABLE_ROWS_FALLBACK: f64 = 1000.0;

/// Read and parse a foreign table's server and table options.
///
/// # Safety
///
/// `relid` must name a foreign table.
unsafe fn options_for(relid: pg_sys::Oid) -> Result<(ServerOptions, TableOptions), String> {
    let (server_raw, table_raw) = unsafe {
        let table = pg_sys::GetForeignTable(relid);
        if table.is_null() {
            return Err("relation is not a foreign table".into());
        }
        let server = pg_sys::GetForeignServer((*table).serverid);
        if server.is_null() {
            return Err("foreign table has no server".into());
        }
        (
            relation_options((*server).options),
            relation_options((*table).options),
        )
    };
    let server = ServerOptions::parse(&server_raw).map_err(|e| e.to_string())?;
    let table = TableOptions::parse(&table_raw).map_err(|e| e.to_string())?;
    Ok((server, table))
}

/// Build a transport from parsed server options.
fn connect(server: &ServerOptions) -> Result<FlightTransport, String> {
    match &server.transport {
        TransportKind::Flight { endpoint } => {
            FlightTransport::new(endpoint).map_err(|e| e.to_string())
        }
        // Not "unimplemented" in the sense of work that only needs writing.
        // `Db::open` takes an exclusive `flock` and PostgreSQL forks a backend
        // per connection, so this needs a multi-process reader in `yesno-core`
        // first. See `crate::transport`.
        TransportKind::Local { .. } => Err(
            "the \"data_dir\" transport is not available: it needs a multi-process \
             read-only reader in yesno-core, which does not exist yet. Use \
             \"endpoint\" to reach a running yesnod."
                .into(),
        ),
    }
}

/// [`connect`], for the `IMPORT FOREIGN SCHEMA` path, which has a server but no
/// table to derive one from.
pub fn connect_pub(server: &ServerOptions) -> Result<FlightTransport, String> {
    connect(server)
}

/// Estimate the size of the foreign relation.
///
/// This is where a yesno foreign table differs from most: the row count is
/// **exact and free**. `get_flight_info` returns `total_records` without moving
/// a single ordinal, so the planner is given truth rather than an estimate.
///
/// # Safety
///
/// Called by the planner with valid `PlannerInfo` and `RelOptInfo` pointers.
#[pg_guard]
pub unsafe extern "C-unwind" fn get_foreign_rel_size(
    _root: *mut pg_sys::PlannerInfo,
    baserel: *mut pg_sys::RelOptInfo,
    foreigntableid: pg_sys::Oid,
) {
    let rows = (|| -> Result<f64, String> {
        let (server, table) = unsafe { options_for(foreigntableid)? };
        let mut transport = connect(&server)?;
        // The bare key, not the lowered quals. `GetForeignRelSize` runs
        // before `GetForeignPlan`, so no pushdown exists yet; the estimate is
        // the key's full cardinality and PostgreSQL applies its own selectivity
        // on top. Counting the filtered set here would double-count it.
        let n = transport
            .cardinality(&FlightTransport::key_cmd(table.key))
            .map_err(|e| e.to_string())?;
        Ok(n as f64)
    })();

    let rows = match rows {
        Ok(n) => n,
        Err(why) => {
            // A warning, not an error. Planning must not fail because the
            // server is momentarily unreachable — `EXPLAIN` is often exactly
            // what someone runs while diagnosing that.
            pgrx::warning!("yesno_fdw: using a fallback row estimate: {why}");
            UNREACHABLE_ROWS_FALLBACK
        }
    };
    unsafe {
        (*baserel).rows = rows;
    }
}

/// Offer the planner exactly one way to read the relation.
///
/// **No `pathkeys`.** The pathkeys argument stays null on purpose: yesno
/// emits ordinals in `u64` order, the column is `int8`,
/// and the two orders disagree above `2^63`. Declaring sorted output would let
/// PostgreSQL skip a sort it genuinely needs and return rows in an order the
/// plan asserted but the data does not have. See [`crate::ordinal`].
///
/// # Safety
///
/// Called by the planner with valid pointers.
#[pg_guard]
pub unsafe extern "C-unwind" fn get_foreign_paths(
    root: *mut pg_sys::PlannerInfo,
    baserel: *mut pg_sys::RelOptInfo,
    _foreigntableid: pg_sys::Oid,
) {
    unsafe {
        let rows = (*baserel).rows;
        let path = crate::pg_compat::foreign_scan_path(root, baserel, rows);
        pg_sys::add_path(baserel, path.cast());
    }
}

/// The `ordinal` column of a yesno foreign table, if it has a usable one.
///
/// Resolved **by name and type**, never by assuming it is attribute 1.
/// Nothing stops a user writing `CREATE FOREIGN TABLE t ( a int, ordinal
/// bigint )`, and a walker told to look at attribute 1 would then recognise
/// quals on `a` as quals on the ordinal and push them down — which is a wrong
/// answer, not a missed optimisation.
///
/// Returning `None` disables pushdown entirely, which is always safe.
///
/// # Safety
///
/// `relid` must name a relation.
unsafe fn ordinal_attno(relid: pg_sys::Oid) -> Option<i16> {
    let attno = unsafe { pg_sys::get_attnum(relid, c"ordinal".as_ptr()) };
    if attno <= 0 {
        return None;
    }
    if unsafe { pg_sys::get_atttype(relid, attno) } != pg_sys::INT8OID {
        return None;
    }
    Some(attno)
}

/// Build the `ForeignScan` plan node, pushing down what can be lowered.
///
/// **A clause is removed from `scan_clauses` only when its lowering was
/// exact.** Everything else stays in the plan for PostgreSQL to evaluate. An
/// inexact lowering is still sent — it narrows the scan — but removing its
/// clause would return the rows it was meant to exclude. See
/// [`crate::fdw::qual`] for why exactness is tracked at all.
///
/// # Safety
///
/// Called by the planner with valid pointers.
#[pg_guard]
pub unsafe extern "C-unwind" fn get_foreign_plan(
    _root: *mut pg_sys::PlannerInfo,
    baserel: *mut pg_sys::RelOptInfo,
    foreigntableid: pg_sys::Oid,
    _best_path: *mut pg_sys::ForeignPath,
    tlist: *mut pg_sys::List,
    scan_clauses: *mut pg_sys::List,
    outer_plan: *mut pg_sys::Plan,
) -> *mut pg_sys::ForeignScan {
    unsafe {
        // An **upper** relation ( the aggregate path ) has `relid == 0` and no
        // scan clauses of its own; its plan is built from the path's
        // `fdw_private`, and `scanrelid` must be 0 with an explicit
        // `fdw_scan_tlist` describing what the scan emits. Treating it like a
        // base relation produces a plan that references a range-table entry
        // that is not there.
        if (*baserel).relid == 0 {
            let private = (*_best_path).fdw_private;

            // An aggregate rel and a join rel both arrive here with
            // `relid == 0`, and they need different target lists. The aggregate
            // emits the `Aggref`'s single `int8`, so its own `tlist` serves. A
            // join emits *ordinals*, and the parent's `tlist` may reference
            // either side's column — so the scan tlist is built from the Vars
            // actually referenced, and every one of them is filled with the
            // same value, which the join condition guarantees they share.
            let scan_tlist = if plan_mode(private) == PUSHDOWN_JOIN {
                join_scan_tlist(baserel, tlist)
            } else {
                tlist
            };

            return pg_sys::make_foreignscan(
                tlist,
                core::ptr::null_mut(), // no local quals: they were required exact
                0,                     // scanrelid: none
                core::ptr::null_mut(), // fdw_exprs
                private,
                scan_tlist,
                core::ptr::null_mut(), // fdw_recheck_quals
                outer_plan,
            );
        }

        // `false`: pseudoconstant clauses are handled by a gating Result node,
        // not by the scan, so they must not be duplicated into the qual list.
        let all_quals = pg_sys::extract_actual_clauses(scan_clauses, false);
        let qual_nodes = list_nodes(all_quals);

        let plan = plan_pushdown(foreigntableid, (*baserel).relid as i32, &qual_nodes);

        let (kept, private) = match plan {
            None => (all_quals, core::ptr::null_mut()),
            Some((expr, keep)) => {
                // Rebuild the qual list from the clauses that must stay.
                let mut kept: *mut pg_sys::List = core::ptr::null_mut();
                for (node, must_keep) in qual_nodes.iter().zip(keep.iter()) {
                    if *must_keep {
                        kept = pg_sys::lappend(kept, (*node).cast());
                    }
                }
                (
                    kept,
                    encode_private_with(&expr, PUSHDOWN_ROWS, foreigntableid),
                )
            }
        };

        pg_sys::make_foreignscan(
            tlist,
            kept,
            (*baserel).relid,
            core::ptr::null_mut(), // fdw_exprs
            private,
            core::ptr::null_mut(), // fdw_scan_tlist
            core::ptr::null_mut(), // fdw_recheck_quals
            outer_plan,
        )
    }
}

/// Lower the scan's quals, or `None` when this relation cannot push anything.
///
/// # Safety
///
/// `relid` must name a foreign table and the nodes must be planner expressions.
unsafe fn plan_pushdown(
    relid: pg_sys::Oid,
    varno: i32,
    quals: &[*mut pg_sys::Node],
) -> Option<(SetExpr, Vec<bool>)> {
    let (_, table) = unsafe { options_for(relid) }.ok()?;
    let attno = unsafe { ordinal_attno(relid) }?;
    let clauses: Vec<Clause> = quals
        .iter()
        .map(|n| unsafe { walk(*n, varno, attno) })
        .collect();
    let p = lower_all(&clauses, table.key);
    Some((p.expr, p.keep))
}

/// Carry the lowered expression from planning to execution.
///
/// Hex rather than raw bytes. `fdw_private` must survive `copyObject` and
/// `nodeToString`, so its members have to be real `Node`s — and a `String` node
/// holds a NUL-terminated C string, which arbitrary encoded bytes are not.
/// Hex doubles a payload that is tens of bytes, and makes the plan readable in
/// `EXPLAIN VERBOSE` besides.
unsafe fn encode_private_with(expr: &SetExpr, mode: i32, relid: pg_sys::Oid) -> *mut pg_sys::List {
    let hex: String = expr.encode().iter().map(|b| format!("{b:02x}")).collect();
    let c = std::ffi::CString::new(hex).expect("hex contains no NUL");
    // The relation OID travels here, and it is **required** rather than a
    // convenience. An upper ( aggregate ) `ForeignScan` has `scanrelid == 0`, so
    // the executor opens no relation and `ss_currentRelation` is **null** — the
    // usual way of finding the server's endpoint segfaults. Carrying the OID is
    // what lets the aggregate path reach its options at all.
    //
    // As a decimal string, not `makeInteger`: an `Oid` is a `u32` and
    // `makeInteger` takes a C `int`, so OIDs above 2^31 would wrap.
    let oid = std::ffi::CString::new(relid.to_u32().to_string()).expect("digits contain no NUL");
    unsafe {
        let s = pg_sys::makeString(pg_sys::pstrdup(c.as_ptr()));
        let mut l = pg_sys::lappend(core::ptr::null_mut(), s.cast());
        l = pg_sys::lappend(l, pg_sys::makeInteger(mode).cast());
        l = pg_sys::lappend(l, pg_sys::makeString(pg_sys::pstrdup(oid.as_ptr())).cast());
        l
    }
}

/// Recover the expression stored by [`encode_private`].
///
/// # Safety
///
/// `private` must be a `List *` produced by `encode_private`, or null.
unsafe fn decode_private(private: *mut pg_sys::List) -> Option<(Vec<u8>, i32, pg_sys::Oid)> {
    let nodes = unsafe { list_nodes(private) };
    let first = *nodes.first()?;
    if first.is_null() || unsafe { (*first).type_ } != pg_sys::NodeTag::T_String {
        return None;
    }
    let s = unsafe { core::ffi::CStr::from_ptr((*(first as *mut pg_sys::String)).sval) };
    let s = s.to_str().ok()?;
    if s.len() % 2 != 0 {
        return None;
    }
    let bytes: Vec<u8> = (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect::<Option<_>>()?;

    // A missing mode reads as `PUSHDOWN_ROWS`, the conservative answer: worst
    // case the scan streams rows an aggregate would have counted, which is slow
    // rather than wrong. Defaulting the other way would return one bogus row.
    let mode = match nodes.get(1) {
        Some(n) if !n.is_null() && unsafe { (**n).type_ } == pg_sys::NodeTag::T_Integer => unsafe {
            (*(*n as *mut pg_sys::Integer)).ival
        },
        _ => PUSHDOWN_ROWS,
    };
    let relid = match nodes.get(2) {
        Some(n) if !n.is_null() && unsafe { (**n).type_ } == pg_sys::NodeTag::T_String => {
            let s = unsafe { core::ffi::CStr::from_ptr((*(*n as *mut pg_sys::String)).sval) };
            s.to_str()
                .ok()?
                .parse::<u32>()
                .ok()
                .map(pg_sys::Oid::from)?
        }
        _ => return None,
    };
    Some((bytes, mode, relid))
}

fn describe_view(view: &yesno_wire::ViewSpec) -> String {
    match view.layout {
        yesno_wire::ViewLayout::Interleaved => format!("interleaved({})", view.sets),
        yesno_wire::ViewLayout::Blocked { stride } => {
            format!("blocked({}, {stride})", view.sets)
        }
    }
}

/// A one-line rendering of a pushed-down expression, for `EXPLAIN`.
///
/// Kept compact and stable: regression fixtures diff this text, so it is part
/// of the tested surface rather than debug output. Do not add a field that
/// varies between runs — an address, a timing, a snapshot version — or every
/// fixture churns.
fn describe(e: &SetExpr) -> String {
    match e {
        SetExpr::Empty => "empty".into(),
        SetExpr::Key(k) => format!("key {k}"),
        SetExpr::Range(lo, hi) => format!("[{lo}, {hi})"),
        SetExpr::Literal(ordinals) => {
            let inner: Vec<String> = ordinals.iter().map(u64::to_string).collect();
            format!("{{{}}}", inner.join(", "))
        }
        SetExpr::And(xs) => {
            let inner: Vec<String> = xs.iter().map(describe).collect();
            format!("({})", inner.join(" AND "))
        }
        SetExpr::Or(xs) => {
            let inner: Vec<String> = xs.iter().map(describe).collect();
            format!("({})", inner.join(" OR "))
        }
        SetExpr::AndNot(a, b) => format!("({} MINUS {})", describe(a), describe(b)),
        // The FDW never produces these, but EXPLAIN must still render an
        // expression that arrived from another producer, so the shapes are
        // spelled the way the query language spells them.
        SetExpr::At(v, i) => format!("{}[{i}]", describe_vec(v)),
        SetExpr::Fold(v, op) => {
            let name = match op {
                yesno_wire::FoldOp::Or => "or",
                yesno_wire::FoldOp::And => "and",
                yesno_wire::FoldOp::Xor => "xor",
            };
            format!("fold({}, {name})", describe_vec(v))
        }
        SetExpr::Pack(v, view) => {
            format!("pack({}, {})", describe_vec(v), describe_view(view))
        }
        SetExpr::Expand(input, view) => {
            format!("expand({}, {})", describe(input), describe_view(view))
        }
        SetExpr::Hole => "_".into(),
        SetExpr::Select(a, n) => format!("select({}, {n})", describe(a)),
        SetExpr::MapBool(v, body) => {
            let yesno_wire::BoolExpr::Contains(a, x) = body.as_ref();
            format!("map({}, contains({}, {x}))", describe_vec(v), describe(a))
        }
    }
}

/// Render a vector-sorted expression for EXPLAIN.
fn describe_vec(v: &yesno_wire::VecSetExpr) -> String {
    match v {
        yesno_wire::VecSetExpr::List(xs) => {
            let inner: Vec<String> = xs.iter().map(describe).collect();
            format!("[{}]", inner.join(", "))
        }
        yesno_wire::VecSetExpr::View(input, view) => {
            format!("view({}, {})", describe(input), describe_view(view))
        }
        yesno_wire::VecSetExpr::Map(v, body) => {
            format!("map({}, {})", describe_vec(v), describe(body))
        }
    }
}

/// Re-label an already-lowered expression as a count.
///
/// This is what makes `count(*)` over a **pushed-down join** free. The join
/// path has already built `And( left, right )`; counting it needs the same
/// expression with a different mode, not a second lowering — and re-deriving it
/// would mean re-walking quals the join already consumed.
///
/// # Safety
///
/// `private` must be a `fdw_private` list built by this module.
pub unsafe fn recount_private(private: *mut pg_sys::List) -> Option<*mut pg_sys::List> {
    let (bytes, _, relid) = unsafe { decode_private(private) }?;
    let expr = SetExpr::decode(&bytes).ok()?;
    Some(unsafe { encode_private_with(&expr, PUSHDOWN_COUNT_STAR, relid) })
}

/// The pushdown mode recorded in a plan's private list.
///
/// # Safety
///
/// `private` must be a `fdw_private` list or null.
unsafe fn plan_mode(private: *mut pg_sys::List) -> i32 {
    unsafe { decode_private(private) }
        .map(|(_, mode, _)| mode)
        .unwrap_or(PUSHDOWN_ROWS)
}

/// The target list a joined `ForeignScan` emits.
///
/// Every `Var` the parent references becomes a column, and all of them carry the
/// same ordinal — which is exactly what `a.ordinal = b.ordinal` asserts.
///
/// # Safety
///
/// Called by the planner with valid pointers.
unsafe fn join_scan_tlist(
    joinrel: *mut pg_sys::RelOptInfo,
    tlist: *mut pg_sys::List,
) -> *mut pg_sys::List {
    unsafe {
        // `reltarget` is a `PathTarget`, which is **not** an expression node.
        // Handing it to `pull_var_clause` walks garbage and fails with
        // "unrecognized node type" at plan time — its `exprs` list is the
        // expression tree.
        let target = (*joinrel).reltarget;
        let mut vars = if target.is_null() {
            core::ptr::null_mut()
        } else {
            pg_sys::pull_var_clause((*target).exprs.cast(), 0)
        };
        if vars.is_null() {
            vars = pg_sys::pull_var_clause(tlist.cast(), 0);
        }
        let nodes = list_nodes(vars);
        let mut out: *mut pg_sys::List = core::ptr::null_mut();
        for (i, v) in nodes.iter().enumerate() {
            let te =
                pg_sys::makeTargetEntry((*v).cast(), (i + 1) as i16, core::ptr::null_mut(), false);
            out = pg_sys::lappend(out, te.cast());
        }
        out
    }
}

/// [`options_for`], for the sibling modules.
///
/// # Safety
///
/// `relid` must name a foreign table.
pub unsafe fn options_for_pub(relid: pg_sys::Oid) -> Result<(ServerOptions, TableOptions), String> {
    unsafe { options_for(relid) }
}

/// [`ordinal_attno`], for the sibling modules.
///
/// # Safety
///
/// `relid` must name a relation.
pub unsafe fn ordinal_attno_pub(relid: pg_sys::Oid) -> Option<i16> {
    unsafe { ordinal_attno(relid) }
}

/// [`list_nodes`], for the sibling modules.
///
/// # Safety
///
/// `list` must be a `List *` of `Node *`.
pub unsafe fn list_nodes_pub(list: *mut pg_sys::List) -> Vec<*mut pg_sys::Node> {
    unsafe { list_nodes(list) }
}

/// Lower a relation's own quals and encode them, for a caller that has a
/// `RelOptInfo` rather than a `scan_clauses` list.
///
/// Used by the aggregate path: an upper relation has no `scan_clauses` of its
/// own, so the filter it must count under comes from the relation beneath it.
///
/// # Safety
///
/// Called by the planner with valid pointers.
pub unsafe fn encode_private_parts(
    relid: pg_sys::Oid,
    varno: i32,
    rel: *mut pg_sys::RelOptInfo,
    mode: i32,
) -> Option<*mut pg_sys::List> {
    let quals = unsafe { pg_sys::extract_actual_clauses((*rel).baserestrictinfo, false) };
    let nodes = unsafe { list_nodes(quals) };
    let (expr, keep) = unsafe { plan_pushdown(relid, varno, &nodes) }?;
    // **Every qual must have lowered exactly.** A count is a single number
    // with nothing above it to re-filter — unlike a row scan, where an inexact
    // pushdown is corrected by the `Filter` that stays in the plan. Counting a
    // superset would return a number larger than the answer, with nothing able
    // to notice.
    if keep.iter().any(|k| *k) {
        return None;
    }
    Some(unsafe { encode_private_with(&expr, mode, relid) })
}

/// Build the `fdw_private` for a pushed-down join of two yesno relations.
///
/// Each side contributes its own key **and its own lowered quals**, so
/// `a JOIN b USING ( ordinal ) WHERE a.ordinal > 100` intersects two already
/// filtered sets rather than filtering after the fact.
///
/// # Safety
///
/// Called by the planner with valid pointers.
pub unsafe fn join_private(
    root: *mut pg_sys::PlannerInfo,
    outerrel: *mut pg_sys::RelOptInfo,
    innerrel: *mut pg_sys::RelOptInfo,
    combine: super::join::Combine,
    joinrel: *mut pg_sys::RelOptInfo,
) -> Option<*mut pg_sys::List> {
    let left = unsafe { side_expr(root, outerrel) }?;
    let right = unsafe { side_expr(root, innerrel) }?;

    let expr = match combine {
        super::join::Combine::And => SetExpr::And(vec![left, right]),
        super::join::Combine::AndNot => SetExpr::AndNot(Box::new(left), Box::new(right)),
    };

    // The relation OID recorded is the **outer** side's. A joined
    // `ForeignScan` has `scanrelid == 0` and opens no relation, so the executor
    // still needs somewhere to read the *server* options from — and both sides
    // were required to share a server before this was called.
    let outer_relid = unsafe { rte_relid(root, (*outerrel).relid) }?;
    let _ = joinrel;
    Some(unsafe { encode_private_with(&expr, PUSHDOWN_JOIN, outer_relid) })
}

/// One side of a join, as a set expression: its key, narrowed by its own quals.
///
/// # Safety
///
/// Called by the planner with valid pointers.
unsafe fn side_expr(
    root: *mut pg_sys::PlannerInfo,
    rel: *mut pg_sys::RelOptInfo,
) -> Option<SetExpr> {
    let rti = unsafe { (*rel).relid };
    let relid = unsafe { rte_relid(root, rti) }?;
    let quals = unsafe { pg_sys::extract_actual_clauses((*rel).baserestrictinfo, false) };
    let nodes = unsafe { list_nodes(quals) };
    let (expr, keep) = unsafe { plan_pushdown(relid, rti as i32, &nodes) }?;
    // Every qual on this side must have lowered exactly. A pushed join has no
    // `Filter` above it holding the leftovers — the clause list was consumed by
    // the join — so an inexact side would silently widen the intersection.
    if keep.iter().any(|k| *k) {
        return None;
    }
    Some(expr)
}

/// The table OID behind a range-table index.
///
/// # Safety
///
/// Called by the planner with a valid `PlannerInfo`.
unsafe fn rte_relid(root: *mut pg_sys::PlannerInfo, rti: pg_sys::Index) -> Option<pg_sys::Oid> {
    if rti == 0 {
        return None;
    }
    let rte = unsafe { *(*root).simple_rte_array.add(rti as usize) };
    if rte.is_null() {
        return None;
    }
    Some(unsafe { (*rte).relid })
}

/// The elements of a `List *` of `Node *`.
///
/// Walked by index rather than through `pgrx::PgList`, which is behind the
/// `cshim` feature this crate disables.
unsafe fn list_nodes(list: *mut pg_sys::List) -> Vec<*mut pg_sys::Node> {
    let mut out = Vec::new();
    if list.is_null() {
        return out;
    }
    let len = unsafe { (*list).length } as usize;
    let cells = unsafe { (*list).elements };
    for i in 0..len {
        out.push(unsafe { (*cells.add(i)).ptr_value } as *mut pg_sys::Node);
    }
    out
}

/// Per-scan execution state.
///
/// Holds the transport ( which owns a tokio runtime ) and the batch currently
/// being handed out one ordinal at a time.
struct ScanState {
    transport: FlightTransport,
    /// The Flight descriptor payload: a bare key, or the pushed-down expression.
    cmd: Vec<u8>,
    batch: OrdinalBatch,
    /// Index into `batch` of the next ordinal to emit.
    at: usize,
    exhausted: bool,
    /// `PUSHDOWN_ROWS` or `PUSHDOWN_COUNT_STAR`.
    mode: i32,
    /// This transaction's buffered writes, applied on top of the server's rows.
    ///
    /// Without this a transaction cannot see its own writes — `BEGIN;
    /// INSERT …; DELETE …;` deletes nothing, because the `DELETE`'s scan does
    /// not find the row the `INSERT` buffered. Removals filter each batch;
    /// insertions are held back until the server's stream is exhausted, and any
    /// the server turns out to hold already are dropped from the set as they go
    /// past, so nothing is emitted twice.
    add: std::collections::BTreeSet<u64>,
    remove: std::collections::BTreeSet<u64>,
}

/// Drop a `ScanState` when the executor's memory context is reset.
///
/// **This is not belt-and-braces.** A `ScanState` owns a tokio runtime on the
/// *Rust* heap, which PostgreSQL knows nothing about, and PostgreSQL unwinds
/// errors with `longjmp` — so an error anywhere in the query skips
/// `EndForeignScan` entirely and the runtime, its threads and its sockets leak
/// for the life of the backend. Registering the drop against `es_query_cxt`,
/// which is reset on both success and abort, is what makes the lifetime match
/// the query rather than the happy path.
///
/// # Safety
///
/// `arg` must be a `*mut ScanState` produced by `Box::into_raw`.
unsafe extern "C-unwind" fn drop_scan_state(arg: *mut core::ffi::c_void) {
    if !arg.is_null() {
        drop(unsafe { Box::from_raw(arg as *mut ScanState) });
    }
}

/// # Safety
///
/// Called by the executor with a valid `ForeignScanState`.
#[pg_guard]
pub unsafe extern "C-unwind" fn begin_foreign_scan(
    node: *mut pg_sys::ForeignScanState,
    eflags: core::ffi::c_int,
) {
    // `EXPLAIN` without `ANALYZE` reaches here with `EXEC_FLAG_EXPLAIN_ONLY`
    // set and never iterates. Opening a connection then would make a plan
    // un-inspectable whenever the server is down — which is exactly when
    // someone runs `EXPLAIN`.
    if eflags & pg_sys::EXEC_FLAG_EXPLAIN_ONLY as core::ffi::c_int != 0 {
        return;
    }

    // The relation OID comes from `fdw_private` first and only falls back to
    // the scan relation. An aggregate `ForeignScan` has `scanrelid == 0`, so
    // `ss_currentRelation` is **null** and dereferencing it crashes the backend
    // — which is exactly what happened before this was written that way.
    let fdw_private = unsafe { (*(*node).ss.ps.plan.cast::<pg_sys::ForeignScan>()).fdw_private };
    let decoded = unsafe { decode_private(fdw_private) };

    let relid = match &decoded {
        Some((_, _, relid)) => *relid,
        None => {
            let rel = unsafe { (*node).ss.ss_currentRelation };
            if rel.is_null() {
                error!("yesno_fdw: scan has neither a relation nor a plan payload");
            }
            unsafe { (*rel).rd_id }
        }
    };
    let (server, table) = match unsafe { options_for(relid) } {
        Ok(v) => v,
        Err(e) => error!("yesno_fdw: {e}"),
    };
    let mut transport = match connect(&server) {
        Ok(t) => t,
        Err(e) => error!("yesno_fdw: {e}"),
    };

    // The plan carries the lowered quals when there were any to lower; falling
    // back to the bare key is what an unfiltered scan looks like.
    let (cmd, mode) = match decoded {
        Some((cmd, mode, _)) => (cmd, mode),
        None => (FlightTransport::key_cmd(table.key), PUSHDOWN_ROWS),
    };

    let (add, remove) = pending_overlay(&server, &cmd, mode);
    let mut batch = Vec::new();
    if mode == PUSHDOWN_COUNT_STAR && add.is_empty() && remove.is_empty() {
        // One `get_flight_info` and **no** `do_get`. That is the entire point
        // of the phase: the count comes from container popcounts in the index,
        // so no ordinal is read, transferred, or decoded.
        match transport.cardinality(&cmd) {
            Ok(n) => batch.push(n as i64),
            Err(e) => error!("yesno_fdw: {e}"),
        }
    } else if mode == PUSHDOWN_COUNT_STAR {
        // The fast count is a count **on the server**, which has not seen this
        // transaction's buffered writes — so with any pending write it is the
        // wrong answer, not merely a stale one. Counting the overlaid stream
        // costs a scan and is correct; the fast path above still covers every
        // read-only statement, which is nearly all of them.
        if let Err(e) = transport.open_scan(&cmd) {
            error!("yesno_fdw: {e}");
        }
        let mut n: i64 = 0;
        let mut pending = add.clone();
        loop {
            match transport.next_batch() {
                Err(e) => error!("yesno_fdw: {e}"),
                Ok(None) => break,
                Ok(Some(b)) => {
                    for v in b {
                        let o = crate::ordinal::i64_to_ordinal(v);
                        pending.remove(&o);
                        if !remove.contains(&o) {
                            n += 1;
                        }
                    }
                }
            }
        }
        transport.close_scan();
        batch.push(n + pending.len() as i64);
    } else if let Err(e) = transport.open_scan(&cmd) {
        error!("yesno_fdw: {e}");
    }
    let state = Box::new(ScanState {
        transport,
        cmd,
        batch,
        at: 0,
        // A count has its single row already; there is no stream to drain.
        exhausted: mode == PUSHDOWN_COUNT_STAR,
        mode,
        add,
        remove,
    });
    let raw = Box::into_raw(state);
    unsafe {
        (*node).fdw_state = raw as *mut core::ffi::c_void;

        // Tie the Rust allocation's lifetime to the query's memory context, so
        // an error that skips `EndForeignScan` still frees it.
        let cxt = (*(*node).ss.ps.state).es_query_cxt;
        let cb = pg_sys::MemoryContextAlloc(cxt, size_of::<pg_sys::MemoryContextCallback>())
            as *mut pg_sys::MemoryContextCallback;
        (*cb).func = Some(drop_scan_state);
        (*cb).arg = raw as *mut core::ffi::c_void;
        (*cb).next = core::ptr::null_mut();
        pg_sys::MemoryContextRegisterResetCallback(cxt, cb);
    }
}

/// # Safety
///
/// Called by the executor with a valid `ForeignScanState`.
#[pg_guard]
pub unsafe extern "C-unwind" fn iterate_foreign_scan(
    node: *mut pg_sys::ForeignScanState,
) -> *mut pg_sys::TupleTableSlot {
    let slot = unsafe { (*node).ss.ss_ScanTupleSlot };
    // `ExecClearTuple` is `static inline` in `executor/tuptable.h`, so
    // bindgen never sees it and pgrx only supplies it through the `cshim`
    // feature this crate disables. The inline is one line — dispatch to the
    // slot's own clear op — so it is written out rather than dragging in a C
    // compilation step for it.
    unsafe {
        if let Some(clear) = (*(*slot).tts_ops).clear {
            clear(slot);
        }
    }

    let state = unsafe { (*node).fdw_state as *mut ScanState };
    if state.is_null() {
        return slot;
    }
    let state = unsafe { &mut *state };

    // Refill until a batch has something, or the stream ends. A `while`
    // rather than an `if`: an empty batch is legal on the wire and treating one
    // as end-of-stream would truncate the scan silently.
    while state.at >= state.batch.len() {
        if state.exhausted {
            // The server's rows are done; anything this transaction inserted
            // that the server did not already hold is emitted now. Held to the
            // end so that an ordinal the server *does* hold is emitted once, by
            // the server, rather than twice.
            if let Some(&o) = state.add.iter().next() {
                state.add.remove(&o);
                unsafe { store_ordinal(slot, o) };
                return slot;
            }
            return slot;
        }
        match state.transport.next_batch() {
            Err(e) => error!("yesno_fdw: {e}"),
            Ok(None) => {
                state.exhausted = true;
                continue;
            }
            Ok(Some(b)) => {
                // Removals filter the batch; anything the server already holds
                // is dropped from the pending insertions so it is not repeated.
                if !state.remove.is_empty() || !state.add.is_empty() {
                    let mut kept = Vec::with_capacity(b.len());
                    for v in b {
                        let o = crate::ordinal::i64_to_ordinal(v);
                        state.add.remove(&o);
                        if !state.remove.contains(&o) {
                            kept.push(v);
                        }
                    }
                    state.batch = kept;
                } else {
                    state.batch = b;
                }
                state.at = 0;
            }
        }
    }

    let value = state.batch[state.at];
    state.at += 1;

    unsafe {
        // A virtual tuple: the value is already a `Datum`-sized integer, so
        // there is nothing to deform and no heap tuple to build.
        //
        // **Every** column gets the same value, not just column 0. A base
        // scan emits one column, but a pushed-down join emits one per `Var` the
        // parent referenced — `SELECT s.ordinal, t.ordinal` is two — and they
        // are all the same ordinal, which is precisely what
        // `s.ordinal = t.ordinal` asserts. Filling only the first would leave
        // the rest holding whatever the slot last contained.
        let natts = (*(*slot).tts_tupleDescriptor).natts as usize;
        for i in 0..natts {
            *(*slot).tts_values.add(i) = pg_sys::Datum::from(value);
            *(*slot).tts_isnull.add(i) = false;
        }
        pg_sys::ExecStoreVirtualTuple(slot)
    }
}

/// # Safety
///
/// Called by the executor with a valid `ForeignScanState`.
#[pg_guard]
pub unsafe extern "C-unwind" fn rescan_foreign_scan(node: *mut pg_sys::ForeignScanState) {
    let state = unsafe { (*node).fdw_state as *mut ScanState };
    if state.is_null() {
        return;
    }
    let state = unsafe { &mut *state };
    // Re-open rather than rewind: the transport streams, and a nested-loop join
    // rescanning the inner side needs the whole set again from the start.
    if state.mode == PUSHDOWN_COUNT_STAR {
        // Rewind rather than re-ask. The count was taken at plan time from a
        // snapshot; re-fetching on rescan could return a *different* number
        // mid-query if a concurrent write landed, which no plan expects.
        state.at = 0;
        return;
    }
    let cmd = state.cmd.clone();
    if let Err(e) = state.transport.open_scan(&cmd) {
        error!("yesno_fdw: {e}");
    }
    state.batch.clear();
    state.at = 0;
    state.exhausted = false;
}

/// # Safety
///
/// Called by the executor with a valid `ForeignScanState`.
#[pg_guard]
pub unsafe extern "C-unwind" fn end_foreign_scan(node: *mut pg_sys::ForeignScanState) {
    let state = unsafe { (*node).fdw_state as *mut ScanState };
    if !state.is_null() {
        // Not `Box::from_raw` here. The memory-context callback registered in
        // `begin_foreign_scan` owns the drop, and doing it in both places is a
        // double free on the normal path. Closing the stream releases the socket
        // promptly; the runtime goes with the context.
        unsafe { (*state).transport.close_scan() };
    }
}

/// Add yesno-specific rows to `EXPLAIN` output.
///
/// This is what makes the regression fixtures able to assert that a pushdown
/// *happened* rather than only that the answer was right — the two are not the
/// same claim, and only the plan can distinguish them.
///
/// # Safety
///
/// Called by the executor with a valid `ForeignScanState` and `ExplainState`.
#[pg_guard]
pub unsafe extern "C-unwind" fn explain_foreign_scan(
    node: *mut pg_sys::ForeignScanState,
    es: *mut pg_sys::ExplainState,
) {
    // Read from the catalog rather than from `fdw_state`, which is null under
    // `EXPLAIN` without `ANALYZE` — that path never runs `BeginForeignScan`'s
    // setup. An `EXPLAIN` that showed nothing for the plain case would be
    // useless exactly where it is most used.
    let fdw_private = unsafe { (*(*node).ss.ps.plan.cast::<pg_sys::ForeignScan>()).fdw_private };
    // Showing the *pushed-down expression* rather than only the key is what
    // lets a regression fixture assert that a qual actually moved. A plan that
    // merely says "Foreign Scan" cannot distinguish a pushed filter from one
    // PostgreSQL is still evaluating itself.
    let decoded = unsafe { decode_private(fdw_private) };
    let text = match decoded
        .as_ref()
        .map(|(bytes, mode, _)| (SetExpr::decode(bytes), *mode))
    {
        Some((Ok(e), mode)) => {
            let d = describe(&e);
            if mode == PUSHDOWN_COUNT_STAR {
                format!("count of {d}")
            } else {
                d
            }
        }
        // Worded differently from the pushdown case on purpose. "key 42" and
        // "key 42, unfiltered" look alike but mean opposite things — one is a
        // pushed expression that happens to be the bare key, the other is no
        // pushdown at all — and a fixture that cannot tell them apart cannot
        // detect a pushdown silently regressing.
        // Reached only for a base relation without a payload; an aggregate
        // scan always has one. `ss_currentRelation` is null for the latter, so
        // it is consulted only here.
        _ => {
            let rel = unsafe { (*node).ss.ss_currentRelation };
            if rel.is_null() {
                "no plan payload".to_string()
            } else {
                match unsafe { options_for((*rel).rd_id) } {
                    Ok((_, table)) => format!("key {}, unfiltered", table.key),
                    Err(e) => format!("options unavailable: {e}"),
                }
            }
        }
    };
    let text = std::ffi::CString::new(text).unwrap_or_default();
    unsafe {
        pg_sys::ExplainPropertyText(c"yesno".as_ptr(), text.as_ptr(), es);
    }
}

#[cfg(test)]
mod tests {
    /// There is deliberately no unit test of a callback here. Every one of
    /// them takes planner or executor state that cannot be constructed outside
    /// a backend, so a test that built a plausible-looking `RelOptInfo` would be
    /// testing the fixture. The scan path is covered by `test/sql/fdw_plan.sql`,
    /// which runs against a real cluster and asserts the *plan*.
    /// The fallback is only ever reached when the server is unreachable, so a
    /// change to it would be a change to behaviour nobody is measuring. Pinning
    /// it keeps that edit deliberate.
    ///
    /// It is **not** a row estimate in the ordinary sense: the real count
    /// comes from `get_flight_info` and is exact. Do not "improve" this
    /// number — if it is in play, the fact worth acting on is the warning.
    #[test]
    fn the_unreachable_fallback_is_a_fixed_sentinel() {
        assert_eq!(super::UNREACHABLE_ROWS_FALLBACK, 1000.0);
    }
}

/// Fill every column of `slot` with one ordinal and store it.
///
/// # Safety
/// `slot` must be a virtual slot belonging to this scan.
unsafe fn store_ordinal(slot: *mut pg_sys::TupleTableSlot, ordinal: u64) {
    let value = crate::ordinal::ordinal_to_i64(ordinal);
    unsafe {
        let natts = (*(*slot).tts_tupleDescriptor).natts as usize;
        for i in 0..natts {
            *(*slot).tts_values.add(i) = pg_sys::Datum::from(value);
            *(*slot).tts_isnull.add(i) = false;
        }
        pg_sys::ExecStoreVirtualTuple(slot);
    }
}

/// The buffered writes this scan must apply on top of the server's rows.
///
/// **A pushed-down expression has to be re-evaluated locally** for the
/// pending insertions, because the server never saw them. That is only
/// answerable when the expression names **one** key: `Key( k )` means "is this
/// ordinal in key k's set", which is known here only for the key the pending
/// ordinal was written to.
///
/// So a pushed-down **join** with pending writes raises an error rather than
/// answering. Returning the unoverlaid rows would silently omit the
/// transaction's own writes, and guessing `Key( other )` either way would
/// invent membership. Refusing names the situation and stays correct; the
/// workaround is to commit before joining.
fn pending_overlay(
    server: &ServerOptions,
    cmd: &[u8],
    mode: i32,
) -> (
    std::collections::BTreeSet<u64>,
    std::collections::BTreeSet<u64>,
) {
    use std::collections::BTreeSet;
    let empty = (BTreeSet::new(), BTreeSet::new());

    let _ = mode;
    let TransportKind::Flight { endpoint } = &server.transport else {
        return empty;
    };

    // A bare 8-byte key, or an encoded expression.
    let (keys, expr) = match yesno_wire::SetExpr::decode(cmd) {
        Ok(e) => {
            // `SetExpr::keys` already exists in the wire crate; a second
            // walker here would be a second thing to keep in step.
            let mut ks = Vec::new();
            e.keys(&mut ks);
            (ks, Some(e))
        }
        Err(_) if cmd.len() == 8 => {
            let mut b = [0u8; 8];
            b.copy_from_slice(cmd);
            (vec![u64::from_le_bytes(b)], None)
        }
        Err(_) => return empty,
    };

    let touched: Vec<u64> = keys
        .iter()
        .copied()
        .filter(|k| super::modify::has_pending(endpoint, *k))
        .collect();
    if touched.is_empty() {
        return empty;
    }
    if keys.len() > 1 {
        error!(
            "yesno_fdw: this transaction has uncommitted writes to a key used by a \
             pushed-down join, and a join cannot be evaluated against them locally. \
             COMMIT before running the join, or write after it."
        );
    }

    let key = touched[0];
    let (ins, rem) = super::modify::pending_sets(endpoint, key);
    let ins = match &expr {
        // The pushed-down predicate applies to the pending rows too. Without
        // this a `DELETE … WHERE ordinal > 5` would see an inserted `3`.
        Some(e) => ins
            .into_iter()
            .filter(|o| super::qual::expr_admits(e, key, *o))
            .collect(),
        None => ins,
    };
    (ins, rem)
}
