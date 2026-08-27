//! Executing a wire [`SetExpr`] against a snapshot.
//!
//! The **format** lives in `yesno-wire`, a dependency-free crate both this
//! server and `yesno-pg` compile, so there is exactly one definition of the
//! encoding. What lives here is the half that needs `yesno-core`: turning a
//! decoded expression into a `yesno_core::Expr` the engine can evaluate.
//!
//! That split is the point. A client must be able to *build* an expression
//! without linking a storage engine, and the server must be able to *run* one
//! without the client's dependencies. Bytes are the only thing that crosses.
//!
//! View transforms are eager boundaries in this first network integration:
//! selection, folding, and expansion use the existing audited `OrdSet` paths,
//! then re-enter the lazy expression tree as a set leaf. Boolean operators around
//! them remain lazy. Ordinal literals similarly become one `OrdSet` leaf. This
//! avoids adding unmeasured `Expr` variants and changing the planner's audited
//! termination proof merely to expose either feature.

use std::sync::Arc;

use yesno_core::view::{Reduce, View};
use yesno_core::{Expr, OrdSet, Snapshot};
pub use yesno_wire::{
    ExprError, SetExpr, ViewLayout, ViewReduce, ViewSpec, MAGIC, MAX_DEPTH, MAX_NODES, VERSION,
};

/// Exact cardinality for a wire expression.
///
/// A top-level view selection uses the view's dedicated count and does not
/// build the selected set. Other view transforms are eager boundaries because
/// the core expression planner has no view operator yet.
pub fn cardinality(e: &SetExpr, snap: &Snapshot) -> yesno_core::Result<u64> {
    match e {
        SetExpr::ViewSelect { key, view, set } => {
            Ok(snap.load(*key)?.view_cardinality(&core_view(*view), *set))
        }
        _ => lower(e, snap)?.cardinality(),
    }
}

/// Lower a wire expression to the executable form.
///
/// **Key leaves are lazy.** A bare key becomes `Snapshot::key_expr`, which
/// resolves the key's chunks to index references and decodes a payload only when
/// an operator actually asks for that chunk -- so an `And` that skips most of a
/// key never decodes the part it skipped. This used to be `snap.load( key )`,
/// which built every container of every operand before evaluation started:
/// correct, and about three allocations per chunk paid whether or not the query
/// needed them.
///
/// It is also what finally puts `Backing::Paged` in front of the planner. Every
/// leaf used to report `Memory`, so the backing-aware branch of the cost model
/// never fired outside its own unit test.
///
/// View transforms remain explicit eager boundaries: the `view` lens operates on
/// a materialized `OrdSet` and has no streaming form, so those arms still load.
pub fn lower(e: &SetExpr, snap: &Snapshot) -> yesno_core::Result<Expr> {
    Ok(match e {
        SetExpr::Empty => Expr::Empty,
        SetExpr::Key(k) => snap.key_expr(*k),
        SetExpr::Range(lo, hi) => Expr::Range(*lo, *hi),
        SetExpr::Literal(ordinals) => {
            for &ordinal in ordinals {
                yesno_core::check_ordinal(ordinal)?;
            }
            Expr::set(Arc::new(OrdSet::from_iter_unsorted(
                ordinals.iter().copied(),
            )))
        }
        SetExpr::And(xs) => fold(xs, snap, Expr::and)?,
        SetExpr::Or(xs) => fold(xs, snap, Expr::or)?,
        SetExpr::AndNot(a, b) => lower(a, snap)?.and_not(lower(b, snap)?),
        SetExpr::ViewSelect { key, view, set } => {
            Expr::set(snap.load(*key)?.view_select(&core_view(*view), *set))
        }
        SetExpr::ViewFold { key, view, reduce } => Expr::set(
            snap.load(*key)?
                .view_fold(&core_view(*view), core_reduce(*reduce)),
        ),
        SetExpr::ViewExpand { input, view } => Expr::set(
            lower(input, snap)?
                .collect_set()?
                .view_expand(&core_view(*view)),
        ),
    })
}

fn core_view(spec: ViewSpec) -> View {
    match spec.layout {
        ViewLayout::Interleaved => View::interleaved(spec.sets),
        ViewLayout::Blocked { stride } => View::blocked(spec.sets, stride),
    }
}

fn core_reduce(reduce: ViewReduce) -> Reduce {
    match reduce {
        ViewReduce::Any => Reduce::Any,
        ViewReduce::All => Reduce::All,
        ViewReduce::Parity => Reduce::Parity,
    }
}
fn fold(xs: &[SetExpr], snap: &Snapshot, join: fn(Expr, Expr) -> Expr) -> yesno_core::Result<Expr> {
    // `decode` rejects an empty junction, so `xs` is non-empty for anything that
    // arrived over the wire. A locally built one could still be empty;
    // `Expr::Empty` is the conservative answer for both AND and OR because it
    // can only ever return fewer rows, never invent one.
    let mut it = xs.iter();
    let Some(first) = it.next() else {
        return Ok(Expr::Empty);
    };
    let mut acc = lower(first, snap)?;
    for x in it {
        acc = join(acc, lower(x, snap)?);
    }
    Ok(acc)
}

/// A bare `OrdSet` for a key, for callers that already know the key.
pub fn set_for_key(snap: &Snapshot, key: u64) -> yesno_core::Result<Arc<OrdSet>> {
    Ok(Arc::new(snap.load(key)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use yesno_core::Db;

    /// Lowering must agree with evaluating the same expression by hand.
    ///
    /// The oracle is `BTreeSet`, not another yesno path. An expression
    /// lowered into a *different* yesno expression would agree with itself, so
    /// only an outside answer can catch it.
    #[test]
    fn lowering_agrees_with_a_set_oracle() {
        let db = Db::new();
        db.insert_many(1, &[1, 2, 3, 10, 11, 12]).unwrap();
        db.insert_many(2, &[2, 3, 4, 11, 12, 13]).unwrap();
        let snap = db.snapshot().unwrap();

        let a: BTreeSet<u64> = [1, 2, 3, 10, 11, 12].into_iter().collect();
        let b: BTreeSet<u64> = [2, 3, 4, 11, 12, 13].into_iter().collect();

        let cases: Vec<(SetExpr, BTreeSet<u64>)> = vec![
            (SetExpr::Key(1), a.clone()),
            (SetExpr::Empty, BTreeSet::new()),
            (
                SetExpr::Literal(vec![0, 2, 65_536, u64::MAX - 1]),
                BTreeSet::from([0, 2, 65_536, u64::MAX - 1]),
            ),
            (
                SetExpr::And(vec![SetExpr::Key(1), SetExpr::Key(2)]),
                a.intersection(&b).copied().collect(),
            ),
            (
                SetExpr::And(vec![SetExpr::Key(1), SetExpr::Literal(vec![2, 3, 65_536])]),
                BTreeSet::from([2, 3]),
            ),
            (
                SetExpr::Or(vec![SetExpr::Key(1), SetExpr::Key(2)]),
                a.union(&b).copied().collect(),
            ),
            (
                SetExpr::AndNot(Box::new(SetExpr::Key(1)), Box::new(SetExpr::Key(2))),
                a.difference(&b).copied().collect(),
            ),
            (
                // The shape qual pushdown actually produces: a key restricted to
                // a half-open range.
                SetExpr::And(vec![SetExpr::Key(1), SetExpr::Range(2, 11)]),
                a.iter().copied().filter(|v| (2..11).contains(v)).collect(),
            ),
            (
                // Two disjoint ranges — what a `BETWEEN` straddling zero lowers
                // to once the sign mapping is applied.
                SetExpr::And(vec![
                    SetExpr::Key(1),
                    SetExpr::Or(vec![SetExpr::Range(1, 3), SetExpr::Range(11, 13)]),
                ]),
                a.iter()
                    .copied()
                    .filter(|v| (1..3).contains(v) || (11..13).contains(v))
                    .collect(),
            ),
        ];

        for (e, want) in cases {
            // Exercise the real request boundary before lowering. A locally
            // constructed enum would leave a wire tag that drops or reorders
            // literal members invisible to this oracle.
            let e = SetExpr::decode(&e.encode()).unwrap();
            let got: BTreeSet<u64> = lower(&e, &snap)
                .unwrap()
                .collect_set()
                .unwrap()
                .iter()
                .collect();
            assert_eq!(got, want, "{e:?}");

            // The cardinality path is a *parallel implementation* — it walks
            // without materializing — so agreeing on the set does not imply
            // agreeing on the count.
            let n = cardinality(&e, &snap).unwrap();
            assert_eq!(n, want.len() as u64, "cardinality of {e:?}");
        }
    }

    #[test]
    fn view_lowering_agrees_with_an_independent_set_oracle() {
        let db = Db::new();
        db.insert_many(1, &[1, 2, 3, 10, 11, 12]).unwrap();
        let parts = [
            BTreeSet::from([1, 2, 5]),
            BTreeSet::from([2, 3, 9]),
            BTreeSet::from([2, 4, 9]),
        ];

        let interleaved: Vec<u64> = parts
            .iter()
            .enumerate()
            .flat_map(|(set, xs)| xs.iter().map(move |x| x * 3 + set as u64))
            .collect();
        db.insert_many(9, &interleaved).unwrap();

        let blocked: Vec<u64> = parts
            .iter()
            .enumerate()
            .flat_map(|(set, xs)| xs.iter().map(move |x| set as u64 * 100 + x))
            .collect();
        db.insert_many(10, &blocked).unwrap();
        let snap = db.snapshot().unwrap();

        let any = parts
            .iter()
            .fold(BTreeSet::new(), |a, x| a.union(x).copied().collect());
        let all = parts[0]
            .intersection(&parts[1])
            .copied()
            .collect::<BTreeSet<_>>()
            .intersection(&parts[2])
            .copied()
            .collect();
        let parity = parts.iter().fold(BTreeSet::new(), |a, x| {
            a.symmetric_difference(x).copied().collect()
        });
        let expanded: BTreeSet<u64> = [1u64, 2, 3, 10, 11, 12]
            .into_iter()
            .flat_map(|x| [x * 2, x * 2 + 1])
            .collect();

        let cases = [
            (
                SetExpr::ViewSelect {
                    key: 9,
                    view: ViewSpec::interleaved(3),
                    set: 1,
                },
                parts[1].clone(),
            ),
            (
                SetExpr::ViewSelect {
                    key: 10,
                    view: ViewSpec::blocked(3, 100),
                    set: 2,
                },
                parts[2].clone(),
            ),
            (
                SetExpr::ViewFold {
                    key: 9,
                    view: ViewSpec::interleaved(3),
                    reduce: ViewReduce::Any,
                },
                any,
            ),
            (
                SetExpr::ViewFold {
                    key: 10,
                    view: ViewSpec::blocked(3, 100),
                    reduce: ViewReduce::All,
                },
                all,
            ),
            (
                SetExpr::ViewFold {
                    key: 9,
                    view: ViewSpec::interleaved(3),
                    reduce: ViewReduce::Parity,
                },
                parity,
            ),
            (
                SetExpr::ViewExpand {
                    input: Box::new(SetExpr::Key(1)),
                    view: ViewSpec::interleaved(2),
                },
                expanded,
            ),
        ];

        for (e, want) in cases {
            let got: BTreeSet<u64> = lower(&e, &snap)
                .unwrap()
                .collect_set()
                .unwrap()
                .iter()
                .collect();
            assert_eq!(got, want, "{e:?}");
            assert_eq!(
                cardinality(&e, &snap).unwrap(),
                want.len() as u64,
                "cardinality of {e:?}"
            );
        }
    }

    /// An expression naming a key that was never written must be empty rather
    /// than an error: a term nobody indexed is a legitimate query with no rows.
    #[test]
    fn an_absent_key_lowers_to_an_empty_set() {
        let db = Db::new();
        let snap = db.snapshot().unwrap();
        let e = SetExpr::And(vec![SetExpr::Key(404), SetExpr::Range(0, 100)]);
        assert_eq!(cardinality(&e, &snap).unwrap(), 0);
    }
}
