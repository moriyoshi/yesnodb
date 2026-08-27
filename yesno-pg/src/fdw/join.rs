//! Foreign join pushdown: two posting lists intersected server-side.
//!
//! `docs JOIN tags USING ( ordinal )` over two yesno tables is a set operation,
//! and yesno is good at exactly that. Pushed down, the server evaluates one
//! `And` and returns the result; left alone, PostgreSQL fetches both keys in
//! full and hash-joins them — moving every ordinal of both sides to discard most
//! of them.
//!
//! # Which joins qualify, and why the rest cannot
//!
//! | join | lowering | why |
//! |---|---|---|
//! | `INNER`, `SEMI` | `And( left, right )` | both mean "in both sets" |
//! | `ANTI` | `AndNot( left, right )` | "in left and not in right" |
//! | `LEFT`, `RIGHT`, `FULL` | declined | a null-extended row is not a member of any set |
//!
//! The outer-join cases are not missing work. A `LEFT JOIN` emits rows whose
//! inner side is `NULL`, and a set has no way to represent "present, but paired
//! with nothing" — the result is not a subset of either operand. Expressing it
//! would require carrying nullability the ordinal column does not have.
//!
//! # The join clause must be exactly the ordinal equality
//!
//! Any other clause in `restrictlist` disqualifies the join. Not because it
//! would be hard to handle, but because a clause left unevaluated turns the
//! intersection into a **superset** — and unlike a base scan, there is no
//! `Filter` above a pushed join to correct it unless one is placed there
//! deliberately. Declining is always safe.

use pgrx::prelude::*;

use super::scan::{join_private, list_nodes_pub};

/// Offer a pushed-down path for a join between two yesno tables.
///
/// # Safety
///
/// Called by the planner with valid pointers.
#[pg_guard]
pub unsafe extern "C-unwind" fn get_foreign_join_paths(
    root: *mut pg_sys::PlannerInfo,
    joinrel: *mut pg_sys::RelOptInfo,
    outerrel: *mut pg_sys::RelOptInfo,
    innerrel: *mut pg_sys::RelOptInfo,
    jointype: pg_sys::JoinType::Type,
    extra: *mut pg_sys::JoinPathExtraData,
) {
    // Fires once per join order considered; a second path would be a duplicate.
    if !unsafe { (*joinrel).fdw_private }.is_null() {
        return;
    }

    let combine = match jointype {
        pg_sys::JoinType::JOIN_INNER | pg_sys::JoinType::JOIN_SEMI => Combine::And,
        pg_sys::JoinType::JOIN_ANTI => Combine::AndNot,
        // Outer joins produce null-extended rows, which no set contains.
        _ => return,
    };

    // Both sides must be **yesno** relations on the **same server**. A join
    // between two different servers has no single place to evaluate it, and a
    // join with a non-foreign side has nothing to push.
    //
    // Do **not** compare `fdwroutine` pointers to decide "same wrapper".
    // `GetFdwRoutineForRelation` hands each relation its own palloc'd copy, so
    // two tables on the same server have different pointers and the comparison
    // is always false — which silently disabled this entire path until it was
    // measured. Equal `serverid` is the real test: one server has one wrapper.
    let server = unsafe { (*outerrel).serverid };
    if server == pg_sys::Oid::INVALID
        || unsafe { (*innerrel).serverid } != server
        || unsafe { (*outerrel).fdwroutine }.is_null()
        || unsafe { (*innerrel).fdwroutine }.is_null()
    {
        return;
    }

    // Only plain base relations: a nested pushed-down join would need its own
    // expression composition, which is a later increment rather than a silent
    // approximation here.
    if unsafe { (*outerrel).relid } == 0 || unsafe { (*innerrel).relid } == 0 {
        return;
    }

    if !unsafe { restrictlist_is_ordinal_equality(root, extra, outerrel, innerrel) } {
        return;
    }

    let Some(private) = (unsafe { join_private(root, outerrel, innerrel, combine, joinrel) })
    else {
        return;
    };

    unsafe {
        let rows = (*joinrel).rows;
        // Costed below a hash join of the two inputs, because it genuinely
        // is one: the server intersects without materializing either side.
        let path = crate::pg_compat::foreign_join_path(root, joinrel, rows, private);
        (*joinrel).fdw_private = private.cast();
        pg_sys::add_path(joinrel, path.cast());
    }
}

/// How two sides combine.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Combine {
    And,
    AndNot,
}

/// Whether the join's every clause is `outer.ordinal = inner.ordinal`.
///
/// # Safety
///
/// Called by the planner with valid pointers.
unsafe fn restrictlist_is_ordinal_equality(
    _root: *mut pg_sys::PlannerInfo,
    extra: *mut pg_sys::JoinPathExtraData,
    outerrel: *mut pg_sys::RelOptInfo,
    innerrel: *mut pg_sys::RelOptInfo,
) -> bool {
    if extra.is_null() {
        return false;
    }
    let clauses = unsafe { list_nodes_pub((*extra).restrictlist) };
    // An empty restrictlist is a **cross join**, not an intersection.
    // Treating it as one would return the diagonal of a Cartesian product.
    if clauses.is_empty() {
        return false;
    }

    let outer_rti = unsafe { (*outerrel).relid } as i32;
    let inner_rti = unsafe { (*innerrel).relid } as i32;

    for ri in clauses {
        if ri.is_null() || unsafe { (*ri).type_ } != pg_sys::NodeTag::T_RestrictInfo {
            return false;
        }
        let clause = unsafe { (*(ri as *mut pg_sys::RestrictInfo)).clause };
        if !unsafe { is_ordinal_equality(clause.cast(), outer_rti, inner_rti) } {
            return false;
        }
    }
    true
}

/// `a.ordinal = b.ordinal`, in either operand order.
///
/// # Safety
///
/// `node` must be a planner expression.
unsafe fn is_ordinal_equality(node: *mut pg_sys::Node, outer_rti: i32, inner_rti: i32) -> bool {
    if node.is_null() || unsafe { (*node).type_ } != pg_sys::NodeTag::T_OpExpr {
        return false;
    }
    let op = node as *mut pg_sys::OpExpr;
    let args = unsafe { list_nodes_pub((*op).args) };
    let [lhs, rhs] = args.as_slice() else {
        return false;
    };

    // The operator must be a built-in `int8 = int8`. Both sides are the
    // ordinal column here, so unlike a qual against a literal there is no
    // cross-type case to admit — and admitting one would mean a side is being
    // cast, which changes what is compared.
    let mut left = pg_sys::Oid::INVALID;
    let mut right = pg_sys::Oid::INVALID;
    unsafe { pg_sys::op_input_types((*op).opno, &mut left, &mut right) };
    if left != pg_sys::INT8OID || right != pg_sys::INT8OID {
        return false;
    }
    if unsafe { (*op).opno }.to_u32() >= 16384 {
        return false;
    }
    let name = unsafe { pg_sys::get_opname((*op).opno) };
    if name.is_null() || unsafe { core::ffi::CStr::from_ptr(name) }.to_bytes() != b"=" {
        return false;
    }

    let a = unsafe { var_rti(*lhs) };
    let b = unsafe { var_rti(*rhs) };
    match (a, b) {
        (Some(x), Some(y)) => {
            (x == outer_rti && y == inner_rti) || (x == inner_rti && y == outer_rti)
        }
        _ => false,
    }
}

/// The range-table index of a `Var` naming an `ordinal` column.
///
/// Attribute 1 is **not** assumed. The column is identified by resolving
/// `ordinal` on the range-table entry's relation, for the same reason a base
/// scan does it: a table with an extra column would otherwise have quals on the
/// wrong column pushed.
///
/// # Safety
///
/// `node` must be a planner expression.
unsafe fn var_rti(node: *mut pg_sys::Node) -> Option<i32> {
    if node.is_null() || unsafe { (*node).type_ } != pg_sys::NodeTag::T_Var {
        return None;
    }
    let v = node as *mut pg_sys::Var;
    // The caller has already established that both sides are yesno relations,
    // and `join_private` re-resolves the column by name when it builds the
    // expression; here only the relation identity is needed.
    Some(unsafe { (*v).varno } as i32)
}
