//! Turning PostgreSQL's expression tree into a [`Clause`].
//!
//! Deliberately mechanical. Every decision that could lose rows lives in
//! [`crate::fdw::qual`], which is pure and oracle-tested; this file only
//! *recognises* shapes and reports [`Clause::Unsupported`] for everything else.
//!
//! That split is why `Unsupported` is a value rather than an error. A shape
//! this walker does not know must still reach the algebra, because whether it
//! can be dropped depends entirely on whether it sits under an `AND` or an `OR`
//! — and only the algebra knows that.
//!
//! # Recognising an operator
//!
//! Operator OIDs are **not** hardcoded. Only `Int8LessOperator` is exported
//! by the bindings, and inventing the other four from memory is exactly the kind
//! of guess that produces a wrong answer rather than an error. Instead an
//! operator qualifies when all four hold:
//!
//! - it is **built in** ( `opno < FirstNormalObjectId` ), so a user-defined
//!   operator that merely spells itself `=` cannot be mistaken for equality;
//! - both input types are integers, and the **ordinal side is declared `int8`**,
//!   so the column is not silently being cast;
//! - the result type is `bool`;
//! - the name is one of the five comparisons.
//!
//! Dropping any one of those admits an operator whose meaning is not the one
//! assumed. The `Int8LessOperator` constant is used as a self-check that the
//! recognition agrees with the catalog.
//!
//! **Cross-type operators are the common case, not the exception.** A SQL
//! integer literal is `int4` unless it needs more, so `ordinal = 21` against an
//! `int8` column resolves to `int84eq`, never `int8eq`. An implementation that
//! insisted both input types be `int8` would compile, pass its unit tests, and
//! push down essentially nothing — which is what happened here until the
//! end-to-end fixture showed `Filter:` surviving on every query.

use pgrx::prelude::*;

use super::qual::{Clause, CmpOp};

/// OIDs below this are catalog built-ins. PostgreSQL's own `is_builtin`.
const FIRST_NORMAL_OBJECT_ID: u32 = 16384;

/// Walk a qual expression for the scan of one relation.
///
/// `varno` and `attno` identify the single `ordinal` column; a `Var` naming
/// anything else makes the comparison unsupported rather than mis-attributed.
///
/// # Safety
///
/// `node` must be a valid `Expr *` from the planner.
pub unsafe fn walk(node: *mut pg_sys::Node, varno: i32, attno: i16) -> Clause {
    if node.is_null() {
        return Clause::Unsupported;
    }
    let tag = unsafe { (*node).type_ };
    match tag {
        pg_sys::NodeTag::T_BoolExpr => unsafe { walk_bool(node.cast(), varno, attno) },
        pg_sys::NodeTag::T_OpExpr => unsafe { walk_op(node.cast(), varno, attno) },
        pg_sys::NodeTag::T_ScalarArrayOpExpr => unsafe {
            walk_scalar_array(node.cast(), varno, attno)
        },
        // A `RelabelType` is a binary-compatible cast the planner inserts; the
        // value underneath is unchanged, so seeing through it costs nothing and
        // recovers quals that would otherwise be unsupported.
        pg_sys::NodeTag::T_RelabelType => {
            let inner = unsafe { (*(node as *mut pg_sys::RelabelType)).arg };
            unsafe { walk(inner.cast(), varno, attno) }
        }
        _ => Clause::Unsupported,
    }
}

unsafe fn walk_bool(b: *mut pg_sys::BoolExpr, varno: i32, attno: i16) -> Clause {
    let args = unsafe { list_nodes((*b).args) };
    let children: Vec<Clause> = args
        .into_iter()
        .map(|a| unsafe { walk(a, varno, attno) })
        .collect();
    match unsafe { (*b).boolop } {
        pg_sys::BoolExprType::AND_EXPR => Clause::And(children),
        pg_sys::BoolExprType::OR_EXPR => Clause::Or(children),
        pg_sys::BoolExprType::NOT_EXPR => match children.into_iter().next() {
            Some(c) => Clause::Not(Box::new(c)),
            None => Clause::Unsupported,
        },
        _ => Clause::Unsupported,
    }
}

unsafe fn walk_op(op: *mut pg_sys::OpExpr, varno: i32, attno: i16) -> Clause {
    let args = unsafe { list_nodes((*op).args) };
    let [lhs, rhs] = args.as_slice() else {
        return Clause::Unsupported;
    };
    let Some((cmp, left_ty, right_ty)) = (unsafe { int_cmp_op((*op).opno) }) else {
        return Clause::Unsupported;
    };

    // The operands can arrive either way round. `7 = ordinal` is the same
    // predicate as `ordinal = 7`, but `7 < ordinal` is `ordinal > 7` — the
    // comparison must be **mirrored**, not merely accepted. Getting this wrong
    // silently inverts a range.
    //
    // The type check is per side: whichever side holds the column must be
    // declared `int8` by the operator, or the column is being cast and the
    // comparison is not the one it appears to be.
    let (value, cmp) = if unsafe { is_ordinal_var(*lhs, varno, attno) } {
        if left_ty != pg_sys::INT8OID {
            return Clause::Unsupported;
        }
        match unsafe { const_int(*rhs, right_ty) } {
            Some(v) => (v, cmp),
            None => return Clause::Unsupported,
        }
    } else if unsafe { is_ordinal_var(*rhs, varno, attno) } {
        if right_ty != pg_sys::INT8OID {
            return Clause::Unsupported;
        }
        match unsafe { const_int(*lhs, left_ty) } {
            Some(v) => (v, mirror(cmp)),
            None => return Clause::Unsupported,
        }
    } else {
        return Clause::Unsupported;
    };

    Clause::Cmp { op: cmp, value }
}

/// `ordinal = ANY ( array )`, which is how PostgreSQL represents `IN`.
unsafe fn walk_scalar_array(sa: *mut pg_sys::ScalarArrayOpExpr, varno: i32, attno: i16) -> Clause {
    // `useOr = false` is `= ALL (…)`, a conjunction, not `IN`. Treating it as
    // one would turn "equals every element" into "equals any element".
    if !unsafe { (*sa).useOr } {
        return Clause::Unsupported;
    }
    let Some((cmp, left_ty, right_ty)) = (unsafe { int_cmp_op((*sa).opno) }) else {
        return Clause::Unsupported;
    };
    if cmp != CmpOp::Eq || left_ty != pg_sys::INT8OID {
        return Clause::Unsupported;
    }
    let args = unsafe { list_nodes((*sa).args) };
    let [lhs, rhs] = args.as_slice() else {
        return Clause::Unsupported;
    };
    if !unsafe { is_ordinal_var(*lhs, varno, attno) } {
        return Clause::Unsupported;
    }
    match unsafe { const_int_array(*rhs, right_ty) } {
        Some(vs) => Clause::In(vs),
        None => Clause::Unsupported,
    }
}

/// Whether `node` is the scan's `ordinal` column.
unsafe fn is_ordinal_var(node: *mut pg_sys::Node, varno: i32, attno: i16) -> bool {
    if node.is_null() || unsafe { (*node).type_ } != pg_sys::NodeTag::T_Var {
        return false;
    }
    let v = node as *mut pg_sys::Var;
    unsafe { (*v).varno as i32 == varno && (*v).varattno == attno }
}

/// The width of an integer type, and whether it is one at all.
///
/// Widening `int2` and `int4` to `i64` is **exact**, which is what makes
/// accepting cross-type comparisons safe: no value changes meaning on the way in.
fn int_width(ty: pg_sys::Oid) -> Option<i32> {
    match ty {
        pg_sys::INT2OID => Some(16),
        pg_sys::INT4OID => Some(32),
        pg_sys::INT8OID => Some(64),
        _ => None,
    }
}

/// A non-null integer constant of the operator's declared type, widened to `i64`.
unsafe fn const_int(node: *mut pg_sys::Node, want: pg_sys::Oid) -> Option<i64> {
    if node.is_null() || unsafe { (*node).type_ } != pg_sys::NodeTag::T_Const {
        return None;
    }
    let c = node as *mut pg_sys::Const;
    // A NULL constant is not a value. `ordinal = NULL` is never true, but it
    // is not `ordinal = 0` either, and mapping it to one would invent a match.
    if unsafe { (*c).constisnull } || unsafe { (*c).consttype } != want {
        return None;
    }
    let raw = unsafe { (*c).constvalue.value() } as i64;
    // Sign-extend from the declared width. Reading an `int4` Datum as `i64`
    // without this yields 4294967275 for -21 on a little-endian machine.
    Some(match int_width(want)? {
        16 => raw as i16 as i64,
        32 => raw as i32 as i64,
        _ => raw,
    })
}

/// A non-null integer-array constant, as `IN` lists arrive.
///
/// `elem` is the operator's declared **right** input type, which for
/// `ordinal IN ( 1, 2 )` is `int4` rather than `int8`. Deconstructing with the
/// wrong element width reads adjacent memory as values.
unsafe fn const_int_array(node: *mut pg_sys::Node, elem: pg_sys::Oid) -> Option<Vec<i64>> {
    if node.is_null() || unsafe { (*node).type_ } != pg_sys::NodeTag::T_Const {
        return None;
    }
    let width = int_width(elem)?;
    let (len, align) = match width {
        16 => (2i32, b's'),
        32 => (4, b'i'),
        _ => (8, b'd'),
    };
    let c = node as *mut pg_sys::Const;
    let want_array = match elem {
        pg_sys::INT2OID => pg_sys::INT2ARRAYOID,
        pg_sys::INT4OID => pg_sys::INT4ARRAYOID,
        _ => pg_sys::INT8ARRAYOID,
    };
    if unsafe { (*c).constisnull } || unsafe { (*c).consttype } != want_array {
        return None;
    }
    unsafe {
        let arr = (*c).constvalue.cast_mut_ptr::<pg_sys::varlena>();
        let arr = pg_sys::pg_detoast_datum(arr) as *mut pg_sys::ArrayType;
        let mut values: *mut pg_sys::Datum = core::ptr::null_mut();
        let mut nulls: *mut bool = core::ptr::null_mut();
        let mut n: core::ffi::c_int = 0;
        // Every integer type here is pass-by-value on a 64-bit build.
        pg_sys::deconstruct_array(
            arr,
            elem,
            len,
            true,
            align as core::ffi::c_char,
            &mut values,
            &mut nulls,
            &mut n,
        );

        let mut out = Vec::with_capacity(n as usize);
        for i in 0..n as usize {
            // A NULL element makes the whole list unusable rather than
            // skippable: `ordinal IN (1, NULL)` is not `ordinal IN (1)` — it is
            // `true` for 1 and `NULL` ( not false ) for everything else, so
            // dropping the NULL would change which rows a `NOT IN` returns.
            if !nulls.is_null() && *nulls.add(i) {
                return None;
            }
            let raw = (*values.add(i)).value() as i64;
            out.push(match width {
                16 => raw as i16 as i64,
                32 => raw as i32 as i64,
                _ => raw,
            });
        }
        Some(out)
    }
}

/// Recognise a built-in integer comparison, with its declared input types.
///
/// See this module's header for why every condition is required, and why both
/// sides need only be *integers* rather than both `int8`.
unsafe fn int_cmp_op(opno: pg_sys::Oid) -> Option<(CmpOp, pg_sys::Oid, pg_sys::Oid)> {
    if opno.to_u32() >= FIRST_NORMAL_OBJECT_ID {
        return None;
    }
    let mut left = pg_sys::Oid::INVALID;
    let mut right = pg_sys::Oid::INVALID;
    unsafe { pg_sys::op_input_types(opno, &mut left, &mut right) };
    int_width(left)?;
    int_width(right)?;
    if unsafe { pg_sys::get_op_rettype(opno) } != pg_sys::BOOLOID {
        return None;
    }
    let name = unsafe { pg_sys::get_opname(opno) };
    if name.is_null() {
        return None;
    }
    let name = unsafe { core::ffi::CStr::from_ptr(name) };
    let cmp = match name.to_bytes() {
        b"=" => CmpOp::Eq,
        b"<" => CmpOp::Lt,
        b"<=" => CmpOp::Le,
        b">" => CmpOp::Gt,
        b">=" => CmpOp::Ge,
        // `<>` is deliberately absent. It lowers to `AndNot( Key, point )`,
        // which is correct only if the column cannot be NULL — true here, since
        // a posting list has no nulls — but it is a separate lowering and
        // belongs with a test rather than smuggled in beside the orderings.
        _ => return None,
    };
    Some((cmp, left, right))
}

/// `a OP b` becomes `b MIRROR(OP) a`.
fn mirror(op: CmpOp) -> CmpOp {
    match op {
        CmpOp::Eq => CmpOp::Eq,
        CmpOp::Lt => CmpOp::Gt,
        CmpOp::Le => CmpOp::Ge,
        CmpOp::Gt => CmpOp::Lt,
        CmpOp::Ge => CmpOp::Le,
    }
}

/// The elements of a `List *` of `Node *`.
///
/// Walked by index rather than through `pgrx::PgList`, which lives behind the
/// `cshim` feature this crate disables. Since PostgreSQL 13 a `List` is
/// array-backed, so this is the same access the macro would perform.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Mirroring is the one piece of this file that is pure, and it is also
    /// the one that silently inverts a range when wrong — `7 < ordinal` read as
    /// `ordinal < 7` returns a disjoint set with no error.
    #[test]
    fn mirroring_a_comparison_twice_is_the_identity() {
        for op in [CmpOp::Eq, CmpOp::Lt, CmpOp::Le, CmpOp::Gt, CmpOp::Ge] {
            assert_eq!(mirror(mirror(op)), op, "{op:?}");
        }
    }

    #[test]
    fn mirroring_reverses_the_inequality() {
        assert_eq!(mirror(CmpOp::Lt), CmpOp::Gt);
        assert_eq!(mirror(CmpOp::Le), CmpOp::Ge);
        assert_eq!(mirror(CmpOp::Gt), CmpOp::Lt);
        assert_eq!(mirror(CmpOp::Ge), CmpOp::Le);
        assert_eq!(mirror(CmpOp::Eq), CmpOp::Eq, "equality is symmetric");
    }

    /// The one operator OID PostgreSQL exports, used to check that this file's
    /// notion of "built in" agrees with the catalog's.
    #[test]
    fn the_builtin_threshold_admits_the_known_int8_operator() {
        assert!(
            pg_sys::Int8LessOperator < FIRST_NORMAL_OBJECT_ID,
            "int8 '<' must be recognised as built in"
        );
    }

    // **Nothing here may call a `pg_sys` function, and the reason is
    // link-time rather than stylistic.** `//yesno-pg:unit` is a standalone
    // executable, while PostgreSQL's server symbols — `CurrentMemoryContext`,
    // `get_opname`, `pg_detoast_datum` — only resolve when the `cdylib` is
    // `dlopen`ed by a backend. They stay undefined in a shared library and are
    // an error in a binary.
    //
    // The trap is that this is invisible until something *reaches* them: the
    // linker only complains about symbols in retained sections, so a test that
    // merely calls `walk( null, .. )` pulls the whole chain in and the build
    // fails with a wall of "undefined reference" that names pgrx internals
    // rather than the test that caused it. ( Observed exactly that, 2026-08-29. )
    //
    // Do not add a test that invokes a walker entry point. Recognition is
    // covered by `test/sql/fdw_qual.sql`, which asserts the *plan* against a
    // real planner — whether the Filter disappeared — which is the stronger
    // claim anyway. Constants like `pg_sys::Int8LessOperator` are fine: a
    // constant needs no symbol.
}
