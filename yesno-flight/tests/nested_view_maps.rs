//! Oracle coverage for composed set maps consumed by integer terminals.
//!
//! The evaluator normalizes an identity cardinality terminal through one map
//! binding. These tests evaluate the direct and composed spellings against
//! independently substituted `BTreeSet` constituents.

use std::collections::BTreeSet;

use yesno_core::Db;
use yesno_flight::expr;
use yesno_flight::{IntExpr, SetExpr, VecIntExpr, VecSetExpr, ViewSpec};

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

fn view(key: u64, spec: ViewSpec) -> VecSetExpr {
    VecSetExpr::View(Box::new(SetExpr::Key(key)), spec)
}

fn direct(key: u64, spec: ViewSpec, body: SetExpr) -> VecIntExpr {
    VecIntExpr::Map(
        Box::new(view(key, spec)),
        Box::new(IntExpr::Cardinality(Box::new(body))),
    )
}

fn nested(key: u64, spec: ViewSpec, body: SetExpr) -> VecIntExpr {
    VecIntExpr::Map(
        Box::new(VecSetExpr::Map(Box::new(view(key, spec)), Box::new(body))),
        Box::new(IntExpr::Cardinality(Box::new(SetExpr::Hole))),
    )
}

#[test]
fn nested_cardinality_maps_match_direct_maps_and_an_independent_oracle() {
    let parts = [
        BTreeSet::from([0, 2, 5, 9, 20]),
        BTreeSet::new(),
        BTreeSet::from([2, 3, 7, 9, 22]),
    ];
    let a = BTreeSet::from([0, 1, 2, 3, 8, 13, 21]);
    let b = BTreeSet::from([2, 4, 6, 9, 13, 22]);
    let interleaved: Vec<_> = parts
        .iter()
        .enumerate()
        .flat_map(|(owner, values)| values.iter().map(move |x| x * 3 + owner as u64))
        .collect();
    let blocked: Vec<_> = parts
        .iter()
        .enumerate()
        .flat_map(|(owner, values)| values.iter().map(move |x| owner as u64 * 100 + x))
        .collect();

    let db = Db::new();
    db.insert_many(7, &a.iter().copied().collect::<Vec<_>>())
        .unwrap();
    db.insert_many(8, &b.iter().copied().collect::<Vec<_>>())
        .unwrap();
    db.insert_many(9, &interleaved).unwrap();
    db.insert_many(10, &blocked).unwrap();
    let snapshot = db.snapshot().unwrap();

    for (spec, key) in [
        (ViewSpec::interleaved(3), 9),
        (ViewSpec::blocked(3, 100), 10),
    ] {
        for body in [
            Body::Union,
            Body::HoleDifference,
            Body::InvariantDifference,
            Body::Static,
            Body::RepeatedHole,
        ] {
            let want: Vec<_> = parts
                .iter()
                .map(|part| body.evaluate(part, &a, &b).len() as u64)
                .collect();
            let direct_result =
                expr::vec_int(&direct(key, spec, body.expression()), &snapshot).unwrap();
            let nested_result =
                expr::vec_int(&nested(key, spec, body.expression()), &snapshot).unwrap();
            assert_eq!(direct_result, want, "direct {body:?} under {spec:?}");
            assert_eq!(nested_result, want, "nested {body:?} under {spec:?}");
        }
    }
}

#[test]
fn normalization_does_not_capture_a_non_identity_outer_body() {
    let parts = [
        BTreeSet::from([0, 2, 5, 9]),
        BTreeSet::from([1, 2, 6, 9]),
        BTreeSet::new(),
    ];
    let a = BTreeSet::from([0, 1, 3, 8, 13]);
    let b = BTreeSet::from([1, 2, 3, 5, 8]);
    let packed: Vec<_> = parts
        .iter()
        .enumerate()
        .flat_map(|(owner, values)| values.iter().map(move |x| x * 3 + owner as u64))
        .collect();

    let db = Db::new();
    db.insert_many(7, &a.iter().copied().collect::<Vec<_>>())
        .unwrap();
    db.insert_many(8, &b.iter().copied().collect::<Vec<_>>())
        .unwrap();
    db.insert_many(9, &packed).unwrap();
    let snapshot = db.snapshot().unwrap();

    let expression = VecIntExpr::Map(
        Box::new(VecSetExpr::Map(
            Box::new(view(9, ViewSpec::interleaved(3))),
            Box::new(SetExpr::Or(vec![SetExpr::Hole, SetExpr::Key(7)])),
        )),
        Box::new(IntExpr::Cardinality(Box::new(SetExpr::And(vec![
            SetExpr::Hole,
            SetExpr::Key(8),
        ])))),
    );
    let want: Vec<_> = parts
        .iter()
        .map(|part| {
            let inner: BTreeSet<_> = part.union(&a).copied().collect();
            inner.intersection(&b).count() as u64
        })
        .collect();
    assert_eq!(expr::vec_int(&expression, &snapshot).unwrap(), want);
}
