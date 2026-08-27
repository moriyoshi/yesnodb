//! Lowering a SQL qual to a pushed-down set expression.
//!
//! # The AND/OR asymmetry is the whole safety argument
//!
//! Under **AND**, a branch that cannot be lowered may be dropped: the pushed
//! expression is then a *superset* of the truth, and the clause stays in
//! PostgreSQL's own qual list to remove the extra rows.
//!
//! Under **OR**, a branch that cannot be lowered may **not** be dropped.
//! Dropping a disjunct *shrinks* the result, and no filter above the scan can
//! re-add rows that were never emitted. One unsupported branch therefore
//! poisons the entire disjunction.
//!
//! Getting this backwards produces **silently missing rows** — no error, no
//! warning, just fewer answers than the query asked for. It is the single most
//! dangerous mistake available in this file, which is why the asymmetry lives in
//! one place and is checked against a brute-force oracle rather than by review.
//!
//! # NOT needs an *exact* child, and that is a third rule
//!
//! Not a special case of either rule above. If a child lowers only
//! approximately — to some `A' ⊇ A` — then its complement satisfies
//! `¬A' ⊆ ¬A`, so an inexact child under `NOT` yields a **subset**, which is the
//! OR failure mode wearing different clothes. `NOT` therefore requires its child
//! to be exact or it does not lower at all.
//!
//! # Exactness decides what may be removed from the plan
//!
//! Lowering produces both an expression and whether it is *exact*. Only an
//! exactly-lowered clause may be removed from `scan_clauses`; an inexact one is
//! still worth pushing ( it narrows the scan ) but must stay in the plan, or the
//! rows it was meant to exclude are returned.
//!
//! This module is deliberately free of `pg_sys`. The walker that turns a
//! `RestrictInfo` into a [`Clause`] is mechanical; the algebra below is where
//! the danger is, and keeping it pure is what makes the oracle test possible.

use yesno_wire::SetExpr;

use crate::ordinal::{i64_point_to_ordinals, i64_range_to_ordinals, OrdinalRanges};

/// A comparison against a `bigint` constant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Lt,
    Le,
    Gt,
    Ge,
}

/// A qual, as the `pg_sys` walker understands it.
///
/// [`Clause::Unsupported`] is a first-class variant rather than an `Option`
/// because it has to survive *inside* a tree — an `OR` containing one behaves
/// completely differently from an `AND` containing one, and that distinction is
/// the point of this module.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Clause {
    /// `ordinal <op> value`
    Cmp {
        op: CmpOp,
        value: i64,
    },
    /// `ordinal IN ( … )`
    In(Vec<i64>),
    And(Vec<Clause>),
    Or(Vec<Clause>),
    Not(Box<Clause>),
    /// Anything the walker did not recognise.
    Unsupported,
}

/// A lowered qual and whether it is faithful.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lowered {
    pub expr: SetExpr,
    /// `true` when the expression matches the clause exactly. Only then may the
    /// clause be removed from the plan.
    pub exact: bool,
}

/// What to push down for a whole scan, and which clauses must stay behind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    /// The expression to send. Always rooted at `Key`, so an empty qual list
    /// still reads the posting list.
    pub expr: SetExpr,
    /// Parallel to the input clauses: `true` means PostgreSQL must keep
    /// evaluating this clause itself.
    ///
    /// The caller removes a clause from `scan_clauses` **only** where this is
    /// `false`. Removing one whose lowering was absent or inexact is what turns
    /// a superset into wrong output.
    pub keep: Vec<bool>,
}

/// Lower every qual of a scan over `key`.
///
/// The clauses arrive already conjunctive — PostgreSQL hands `baserestrictinfo`
/// as an implicit AND — so each is lowered independently and the results are
/// ANDed. That is why the top level needs no special handling: dropping a whole
/// top-level clause is the AND rule, and it is safe precisely because the clause
/// stays in the plan.
pub fn lower_all(clauses: &[Clause], key: u64) -> Plan {
    let mut parts = vec![SetExpr::Key(key)];
    let mut keep = Vec::with_capacity(clauses.len());
    for c in clauses {
        match lower_one(c, key) {
            Some(l) => {
                parts.push(l.expr);
                keep.push(!l.exact);
            }
            None => keep.push(true),
        }
    }
    let expr = if parts.len() == 1 {
        parts.into_iter().next().expect("len 1")
    } else {
        SetExpr::And(parts)
    };
    Plan { expr, keep }
}

/// Lower one qual, **without** the enclosing `Key`.
///
/// Returns `None` when nothing useful could be extracted.
pub fn lower_one(clause: &Clause, key: u64) -> Option<Lowered> {
    match clause {
        Clause::Unsupported => None,

        Clause::Cmp { op, value } => ranges_to_expr(cmp_ranges(*op, *value)),

        Clause::In(values) => {
            // An `IN` list is a union of points. Every element must lower or
            // the whole list must be abandoned: `IN` is a disjunction, so
            // dropping one element loses its rows. Points always lower, so this
            // is a property of the code rather than a branch — but the union is
            // built as an `Or`, and `Or`'s rule is what governs it.
            let mut parts = Vec::with_capacity(values.len());
            for v in values {
                let r = i64_point_to_ordinals(*v);
                for &(lo, hi) in r.as_slice() {
                    parts.push(SetExpr::Range(lo, hi));
                }
            }
            match parts.len() {
                // Every value was `u64::MAX`, which is not an ordinal, so the
                // predicate is unsatisfiable — exactly, not approximately.
                0 => Some(Lowered {
                    expr: SetExpr::Empty,
                    exact: true,
                }),
                1 => Some(Lowered {
                    expr: parts.pop().expect("len 1"),
                    exact: true,
                }),
                _ => Some(Lowered {
                    expr: SetExpr::Or(parts),
                    exact: true,
                }),
            }
        }

        // ── AND: a branch may be dropped, at the cost of exactness ──────────
        Clause::And(children) => {
            let mut parts = Vec::new();
            let mut exact = true;
            for c in children {
                match lower_one(c, key) {
                    Some(l) => {
                        exact &= l.exact;
                        parts.push(l.expr);
                    }
                    // Dropping a conjunct widens the result. Legal, because the
                    // clause stays in PostgreSQL's qual list — which is what
                    // `exact = false` tells the caller to arrange.
                    None => exact = false,
                }
            }
            match parts.len() {
                0 => None,
                1 => Some(Lowered {
                    expr: parts.pop().expect("len 1"),
                    exact,
                }),
                _ => Some(Lowered {
                    expr: SetExpr::And(parts),
                    exact,
                }),
            }
        }

        // ── OR: a branch may NOT be dropped ─────────────────────────────────
        Clause::Or(children) => {
            if children.is_empty() {
                return None;
            }
            let mut parts = Vec::with_capacity(children.len());
            let mut exact = true;
            for c in children {
                // The `?` is the safety property. One unlowerable disjunct
                // abandons the whole disjunction, because emitting the rest
                // would return a **subset** and no filter above the scan can
                // recover the missing rows.
                let l = lower_one(c, key)?;
                exact &= l.exact;
                parts.push(l.expr);
            }
            // An inexact disjunct is just as fatal as a missing one. Each
            // lowered branch is a superset of its clause, so their union is a
            // superset of the disjunction — which is *safe* for OR, unlike for
            // NOT. Exactness is therefore propagated, not required.
            match parts.len() {
                1 => Some(Lowered {
                    expr: parts.pop().expect("len 1"),
                    exact,
                }),
                _ => Some(Lowered {
                    expr: SetExpr::Or(parts),
                    exact,
                }),
            }
        }

        // ── NOT: the child must be exact ────────────────────────────────────
        Clause::Not(inner) => {
            let l = lower_one(inner, key)?;
            // Not negotiable. `¬A' ⊆ ¬A` whenever `A' ⊇ A`, so complementing
            // an approximation loses rows.
            if !l.exact {
                return None;
            }
            // Complement *within this key*, which is the universe the scan is
            // over. `AndNot( Key, A )` is exactly "in the posting list and not
            // in A".
            Some(Lowered {
                expr: SetExpr::AndNot(Box::new(SetExpr::Key(key)), Box::new(l.expr)),
                exact: true,
            })
        }
    }
}

/// The ordinal ranges a single comparison selects.
fn cmp_ranges(op: CmpOp, value: i64) -> OrdinalRanges {
    match op {
        CmpOp::Eq => i64_point_to_ordinals(value),
        // Every inequality becomes an *inclusive* `int8` interval bounded by
        // `i64::MIN` / `i64::MAX`, then goes through the same mapping as a
        // `BETWEEN`. That is what makes the sign boundary handled in one place:
        // `ordinal > 0` is `[1, i64::MAX]`, which maps to a single `u64` range,
        // while `ordinal < 0` is `[i64::MIN, -1]`, which maps to the *upper*
        // half of the `u64` space. Deriving either by hand here would duplicate
        // the trap.
        CmpOp::Lt => match value.checked_sub(1) {
            Some(hi) => i64_range_to_ordinals(i64::MIN, hi),
            // `ordinal < i64::MIN` is unsatisfiable.
            None => OrdinalRanges::EMPTY,
        },
        CmpOp::Le => i64_range_to_ordinals(i64::MIN, value),
        CmpOp::Gt => match value.checked_add(1) {
            Some(lo) => i64_range_to_ordinals(lo, i64::MAX),
            None => OrdinalRanges::EMPTY,
        },
        CmpOp::Ge => i64_range_to_ordinals(value, i64::MAX),
    }
}

fn ranges_to_expr(r: OrdinalRanges) -> Option<Lowered> {
    let parts: Vec<SetExpr> = r
        .as_slice()
        .iter()
        .map(|&(lo, hi)| SetExpr::Range(lo, hi))
        .collect();
    Some(match parts.len() {
        0 => Lowered {
            expr: SetExpr::Empty,
            exact: true,
        },
        1 => Lowered {
            expr: parts.into_iter().next().expect("len 1"),
            exact: true,
        },
        _ => Lowered {
            expr: SetExpr::Or(parts),
            exact: true,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// Evaluate a clause directly, as PostgreSQL would.
    ///
    /// `Unsupported` is given **real semantics** here — "the value is odd" —
    /// and that choice is what makes the oracle able to see its subject.
    ///
    /// An earlier version returned `None` for it and propagated that outward, so
    /// any tree containing an unsupported branch had no expected answer at all;
    /// `want` came out empty and `want ⊆ got` held vacuously. The oracle was
    /// therefore blind on **exactly** the trees the module exists to get right,
    /// and a sabotaged `OR` that dropped its unlowerable disjunct passed it.
    /// Verified: with this predicate in place that sabotage fails the oracle.
    ///
    /// Do not restore an `Option` return. `Unsupported` means "the walker did
    /// not recognise this SQL", not "this SQL has no truth value" — every real
    /// qual denotes something.
    fn eval_clause(c: &Clause, v: i64) -> bool {
        match c {
            Clause::Unsupported => v % 2 != 0,
            Clause::Cmp { op, value } => match op {
                CmpOp::Eq => v == *value,
                CmpOp::Lt => v < *value,
                CmpOp::Le => v <= *value,
                CmpOp::Gt => v > *value,
                CmpOp::Ge => v >= *value,
            },
            Clause::In(vs) => vs.contains(&v),
            Clause::And(cs) => cs.iter().all(|c| eval_clause(c, v)),
            Clause::Or(cs) => cs.iter().any(|c| eval_clause(c, v)),
            Clause::Not(c) => !eval_clause(c, v),
        }
    }

    /// Evaluate a lowered expression over a candidate universe.
    fn eval_expr(e: &SetExpr, key: u64, universe: &BTreeSet<u64>) -> BTreeSet<u64> {
        match e {
            SetExpr::Empty => BTreeSet::new(),
            // Every candidate is "in the key" for this test: the universe *is*
            // the posting list.
            SetExpr::Key(k) => {
                assert_eq!(*k, key);
                universe.clone()
            }
            SetExpr::Range(lo, hi) => universe
                .iter()
                .copied()
                .filter(|o| o >= lo && o < hi)
                .collect(),
            SetExpr::Literal(ordinals) => ordinals.iter().copied().collect(),
            SetExpr::And(xs) => xs
                .iter()
                .map(|x| eval_expr(x, key, universe))
                .reduce(|a, b| a.intersection(&b).copied().collect())
                .unwrap_or_default(),
            SetExpr::Or(xs) => xs
                .iter()
                .map(|x| eval_expr(x, key, universe))
                .reduce(|a, b| a.union(&b).copied().collect())
                .unwrap_or_default(),
            SetExpr::ViewSelect { .. } | SetExpr::ViewFold { .. } | SetExpr::ViewExpand { .. } => {
                panic!("the PostgreSQL qual lowerer does not produce view expressions")
            }
            SetExpr::AndNot(a, b) => {
                let l = eval_expr(a, key, universe);
                let r = eval_expr(b, key, universe);
                l.difference(&r).copied().collect()
            }
        }
    }

    /// Candidate ordinals, biased to the boundaries that matter: zero, the sign
    /// flip at `2^63`, and the reserved top of the domain.
    fn universe() -> BTreeSet<u64> {
        [
            0u64,
            1,
            2,
            3,
            5,
            (1 << 63) - 1,
            1 << 63,
            (1 << 63) + 1,
            u64::MAX - 2,
            u64::MAX - 1,
        ]
        .into_iter()
        .collect()
    }

    fn cmp(op: CmpOp, value: i64) -> Clause {
        Clause::Cmp { op, value }
    }

    /// Every clause shape, checked against direct evaluation.
    ///
    /// The assertion is **directional**, not equality, and that is the entire
    /// test. A lowering is allowed to be a *superset* of the truth as long as it
    /// reports `exact = false`; it is never allowed to be a subset. Asserting
    /// equality unconditionally would reject legitimate inexact lowerings, and
    /// asserting nothing would miss the failure that matters.
    fn check(c: &Clause, key: u64) {
        let u = universe();
        let Some(lowered) = lower_one(c, key) else {
            return; // declining to lower is always safe
        };
        // The scan is always rooted at the key, so the oracle evaluates the
        // same shape `lower_all` would send.
        let rooted = SetExpr::And(vec![SetExpr::Key(key), lowered.expr.clone()]);
        let got = eval_expr(&rooted, key, &u);

        let mut want = BTreeSet::new();
        for &o in &u {
            if eval_clause(c, crate::ordinal::ordinal_to_i64(o)) {
                want.insert(o);
            }
        }

        if lowered.exact {
            assert_eq!(got, want, "exact lowering must match exactly: {c:?}");
        } else {
            assert!(
                want.is_subset(&got),
                "lowering lost rows for {c:?}\n  want ⊆ got\n  want={want:?}\n  got={got:?}"
            );
        }
    }

    #[test]
    fn every_comparison_lowers_faithfully() {
        let key = 42;
        for op in [CmpOp::Eq, CmpOp::Lt, CmpOp::Le, CmpOp::Gt, CmpOp::Ge] {
            for v in [
                i64::MIN,
                i64::MIN + 1,
                -2,
                -1,
                0,
                1,
                2,
                i64::MAX - 1,
                i64::MAX,
            ] {
                check(&cmp(op, v), key);
            }
        }
    }

    #[test]
    fn in_lists_lower_faithfully() {
        let key = 42;
        for vs in [
            vec![],
            vec![0],
            vec![-1],
            vec![0, 1, 2],
            vec![i64::MIN, 0, i64::MAX],
            vec![-1, -1, -1],
        ] {
            check(&Clause::In(vs), key);
        }
    }

    /// The failure this module exists to prevent. An `OR` with an
    /// unsupported branch must decline entirely — lowering only the supported
    /// half would silently drop the other half's rows.
    #[test]
    fn an_or_with_an_unsupported_branch_does_not_lower() {
        let c = Clause::Or(vec![cmp(CmpOp::Eq, 1), Clause::Unsupported]);
        assert_eq!(lower_one(&c, 42), None, "a poisoned OR must not lower");

        // And the contrast that makes it meaningful: the *same* shape under
        // AND does lower, inexactly.
        let c = Clause::And(vec![cmp(CmpOp::Eq, 1), Clause::Unsupported]);
        let l = lower_one(&c, 42).expect("AND may drop a branch");
        assert!(!l.exact, "dropping a conjunct must report inexact");
    }

    /// The second failure mode: complementing an approximation loses rows.
    #[test]
    fn a_not_over_an_inexact_child_does_not_lower() {
        let inexact = Clause::And(vec![cmp(CmpOp::Eq, 1), Clause::Unsupported]);
        assert!(
            lower_one(&inexact, 42).is_some_and(|l| !l.exact),
            "precondition: the child lowers inexactly"
        );
        assert_eq!(
            lower_one(&Clause::Not(Box::new(inexact)), 42),
            None,
            "NOT over an inexact child must decline"
        );

        // An exact child is fine.
        let exact = Clause::Not(Box::new(cmp(CmpOp::Eq, 1)));
        assert!(lower_one(&exact, 42).is_some_and(|l| l.exact));
    }

    /// The brute-force oracle: every small tree over a boundary-biased universe.
    ///
    /// This is the test that catches a dropped disjunct, and it is written to
    /// fail loudly when one is dropped rather than to confirm the happy path.
    #[test]
    fn every_small_tree_is_a_superset_and_exact_ones_are_equal() {
        let key = 42;
        let leaves = [
            cmp(CmpOp::Eq, 0),
            cmp(CmpOp::Lt, 0),
            cmp(CmpOp::Ge, 1),
            cmp(CmpOp::Le, -1),
            Clause::In(vec![0, 2, -1]),
            Clause::Unsupported,
        ];

        for a in &leaves {
            check(a, key);
            check(&Clause::Not(Box::new(a.clone())), key);
            for b in &leaves {
                check(&Clause::And(vec![a.clone(), b.clone()]), key);
                check(&Clause::Or(vec![a.clone(), b.clone()]), key);
                check(
                    &Clause::Not(Box::new(Clause::Or(vec![a.clone(), b.clone()]))),
                    key,
                );
                check(
                    &Clause::Not(Box::new(Clause::And(vec![a.clone(), b.clone()]))),
                    key,
                );
                for c in &leaves {
                    check(
                        &Clause::And(vec![a.clone(), Clause::Or(vec![b.clone(), c.clone()])]),
                        key,
                    );
                    check(
                        &Clause::Or(vec![a.clone(), Clause::And(vec![b.clone(), c.clone()])]),
                        key,
                    );
                }
            }
        }
    }
}

/// Whether `ordinal` satisfies `expr`, given that it is a member of `key`.
///
/// Only meaningful when `expr` names **one** key. `Key( k )` is answered as
/// "is this ordinal in key k's set", which is knowable here only for the key the
/// pending ordinal was written to. For a multi-key expression — a pushed-down
/// join — the caller must not use this; see [`scan::pending_overlay`].
pub fn expr_admits(expr: &SetExpr, key: u64, ordinal: u64) -> bool {
    match expr {
        SetExpr::Empty => false,
        SetExpr::Key(k) => *k == key,
        SetExpr::Range(lo, hi) => ordinal >= *lo && ordinal < *hi,
        SetExpr::Literal(ordinals) => ordinals.binary_search(&ordinal).is_ok(),
        SetExpr::And(xs) => xs.iter().all(|x| expr_admits(x, key, ordinal)),
        SetExpr::Or(xs) => xs.iter().any(|x| expr_admits(x, key, ordinal)),
        SetExpr::AndNot(a, b) => expr_admits(a, key, ordinal) && !expr_admits(b, key, ordinal),
        // The FDW does not lower SQL quals to view transforms. If one arrives
        // from another producer, a pending physical ordinal cannot be tested
        // against its logical result without evaluating the packed key.
        SetExpr::ViewSelect { .. } | SetExpr::ViewFold { .. } | SetExpr::ViewExpand { .. } => false,
    }
}
