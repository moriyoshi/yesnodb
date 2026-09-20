//! Oracle coverage for pointwise Boolean folds over mapped packed views.
//!
//! The fused evaluator reduces a two-value truth table. These tests instead
//! substitute each constituent into the wire expression and reduce ordinary
//! `BTreeSet`s, so they do not share that derivation.

use std::collections::BTreeSet;

use yesno_core::Db;
use yesno_flight::expr;
use yesno_flight::{FoldOp, SetExpr, VecSetExpr, ViewSpec};

#[derive(Clone, Copy, Debug)]
enum Body {
    Union,
    HoleDifference,
    InvariantDifference,
    Static,
    RepeatedHole,
}

impl Body {
    fn expression(self) -> SetExpr {
        match self {
            Self::Union => SetExpr::Or(vec![SetExpr::Hole, SetExpr::Key(7)]),
            Self::HoleDifference => {
                SetExpr::AndNot(Box::new(SetExpr::Hole), Box::new(SetExpr::Key(7)))
            }
            Self::InvariantDifference => {
                SetExpr::AndNot(Box::new(SetExpr::Key(7)), Box::new(SetExpr::Hole))
            }
            Self::Static => SetExpr::Key(7),
            Self::RepeatedHole => SetExpr::AndNot(
                Box::new(SetExpr::Or(vec![SetExpr::Hole, SetExpr::Key(7)])),
                Box::new(SetExpr::And(vec![SetExpr::Hole, SetExpr::Key(8)])),
            ),
        }
    }

    fn evaluate(self, hole: &BTreeSet<u64>, a: &BTreeSet<u64>, b: &BTreeSet<u64>) -> BTreeSet<u64> {
        match self {
            Self::Union => hole.union(a).copied().collect(),
            Self::HoleDifference => hole.difference(a).copied().collect(),
            Self::InvariantDifference => a.difference(hole).copied().collect(),
            Self::Static => a.clone(),
            Self::RepeatedHole => {
                let left: BTreeSet<_> = hole.union(a).copied().collect();
                let right: BTreeSet<_> = hole.intersection(b).copied().collect();
                left.difference(&right).copied().collect()
            }
        }
    }
}

fn reduce(mut parts: Vec<BTreeSet<u64>>, op: FoldOp) -> BTreeSet<u64> {
    let mut out = parts.remove(0);
    for part in parts {
        out = match op {
            FoldOp::Or => out.union(&part).copied().collect(),
            FoldOp::And => out.intersection(&part).copied().collect(),
            FoldOp::Xor => out.symmetric_difference(&part).copied().collect(),
        };
    }
    out
}

fn query(key: u64, view: ViewSpec, body: Body, op: FoldOp) -> SetExpr {
    SetExpr::Fold(
        Box::new(VecSetExpr::Map(
            Box::new(VecSetExpr::View(Box::new(SetExpr::Key(key)), view)),
            Box::new(body.expression()),
        )),
        op,
    )
}

#[test]
fn pointwise_mapped_folds_agree_with_an_independent_set_oracle() {
    let parts = [
        BTreeSet::from([0, 2, 5, 9, 20]),
        BTreeSet::from([1, 2, 6, 9, 21]),
        BTreeSet::from([2, 3, 7, 9, 22]),
        BTreeSet::from([0, 4, 8, 9, 23]),
    ];
    let a = BTreeSet::from([0, 1, 2, 3, 8, 13, 21]);
    let b = BTreeSet::from([2, 4, 6, 9, 13, 22]);

    let db = Db::new();
    db.insert_many(7, &a.iter().copied().collect::<Vec<_>>())
        .unwrap();
    db.insert_many(8, &b.iter().copied().collect::<Vec<_>>())
        .unwrap();

    let mut frames = Vec::new();
    for sets in [3usize, 4] {
        let interleaved: Vec<_> = parts[..sets]
            .iter()
            .enumerate()
            .flat_map(|(owner, values)| values.iter().map(move |x| x * sets as u64 + owner as u64))
            .collect();
        let blocked: Vec<_> = parts[..sets]
            .iter()
            .enumerate()
            .flat_map(|(owner, values)| values.iter().map(move |x| owner as u64 * 100 + x))
            .collect();
        let interleaved_key = 20 + sets as u64;
        let blocked_key = 30 + sets as u64;
        db.insert_many(interleaved_key, &interleaved).unwrap();
        db.insert_many(blocked_key, &blocked).unwrap();
        frames.push((sets, interleaved_key, ViewSpec::interleaved(sets as u32)));
        frames.push((sets, blocked_key, ViewSpec::blocked(sets as u32, 100)));
    }

    let snapshot = db.snapshot().unwrap();
    let bodies = [
        Body::Union,
        Body::HoleDifference,
        Body::InvariantDifference,
        Body::Static,
        Body::RepeatedHole,
    ];
    let ops = [FoldOp::Or, FoldOp::And, FoldOp::Xor];

    for (sets, key, view) in frames {
        for body in bodies {
            let mapped: Vec<_> = parts[..sets]
                .iter()
                .map(|part| body.evaluate(part, &a, &b))
                .collect();
            for op in ops {
                let want = reduce(mapped.clone(), op);
                let got: BTreeSet<_> = expr::lower(&query(key, view, body, op), &snapshot)
                    .unwrap()
                    .collect_set()
                    .unwrap()
                    .iter()
                    .collect();
                assert_eq!(
                    got, want,
                    "{body:?} with {op:?} over {view:?} at arity {sets}"
                );
            }
        }
    }
}
