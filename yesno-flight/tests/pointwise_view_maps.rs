//! Oracle coverage for pointwise Boolean terminals over packed views.
//!
//! These cases exercise the false/true decomposition used by interleaved views
//! and the materializing fallback used by blocked views against the same
//! independent `BTreeSet` answers.

use std::collections::BTreeSet;

use yesno_core::Db;
use yesno_flight::expr;
use yesno_flight::{IntExpr, SetExpr, VecIntExpr, VecSetExpr, ViewSpec};

fn union(a: &BTreeSet<u64>, b: &BTreeSet<u64>) -> BTreeSet<u64> {
    a.union(b).copied().collect()
}

fn intersection(a: &BTreeSet<u64>, b: &BTreeSet<u64>) -> BTreeSet<u64> {
    a.intersection(b).copied().collect()
}

fn difference(a: &BTreeSet<u64>, b: &BTreeSet<u64>) -> BTreeSet<u64> {
    a.difference(b).copied().collect()
}

fn mapped(body: SetExpr, key: u64, view: ViewSpec, rank: Option<u64>) -> VecIntExpr {
    let body = match rank {
        Some(x) => IntExpr::Rank(Box::new(body), x),
        None => IntExpr::Cardinality(Box::new(body)),
    };
    VecIntExpr::Map(
        Box::new(VecSetExpr::View(Box::new(SetExpr::Key(key)), view)),
        Box::new(body),
    )
}

#[test]
fn pointwise_boolean_maps_agree_with_an_independent_set_oracle() {
    let parts = [
        BTreeSet::from([0, 2, 5, 9, 20]),
        BTreeSet::from([1, 2, 6, 9, 21]),
        BTreeSet::from([2, 3, 7, 9, 22]),
    ];
    let a = BTreeSet::from([0, 1, 2, 3, 8, 13, 21]);
    let b = BTreeSet::from([2, 4, 6, 9, 13, 22]);

    let interleaved: Vec<u64> = parts
        .iter()
        .enumerate()
        .flat_map(|(owner, values)| values.iter().map(move |x| x * 3 + owner as u64))
        .collect();
    let blocked: Vec<u64> = parts
        .iter()
        .enumerate()
        .flat_map(|(owner, values)| values.iter().map(move |x| owner as u64 * 100 + x))
        .collect();

    let db = Db::new();
    db.insert_many(7, a.iter().copied().collect::<Vec<_>>().as_slice())
        .unwrap();
    db.insert_many(8, b.iter().copied().collect::<Vec<_>>().as_slice())
        .unwrap();
    db.insert_many(9, &interleaved).unwrap();
    db.insert_many(10, &blocked).unwrap();
    let snapshot = db.snapshot().unwrap();

    let cases: [(&str, SetExpr, [BTreeSet<u64>; 3]); 5] = [
        (
            "union",
            SetExpr::Or(vec![SetExpr::Hole, SetExpr::Key(7)]),
            std::array::from_fn(|i| union(&parts[i], &a)),
        ),
        (
            "hole difference",
            SetExpr::AndNot(Box::new(SetExpr::Hole), Box::new(SetExpr::Key(7))),
            std::array::from_fn(|i| difference(&parts[i], &a)),
        ),
        (
            "invariant difference",
            SetExpr::AndNot(Box::new(SetExpr::Key(7)), Box::new(SetExpr::Hole)),
            std::array::from_fn(|i| difference(&a, &parts[i])),
        ),
        (
            "static body",
            SetExpr::Key(7),
            std::array::from_fn(|_| a.clone()),
        ),
        (
            "repeated hole",
            SetExpr::AndNot(
                Box::new(SetExpr::Or(vec![SetExpr::Hole, SetExpr::Key(7)])),
                Box::new(SetExpr::And(vec![SetExpr::Hole, SetExpr::Key(8)])),
            ),
            std::array::from_fn(|i| {
                difference(&union(&parts[i], &a), &intersection(&parts[i], &b))
            }),
        ),
    ];

    for (layout, key) in [
        (ViewSpec::interleaved(3), 9),
        (ViewSpec::blocked(3, 100), 10),
    ] {
        for (name, body, answers) in &cases {
            let cardinalities = mapped(body.clone(), key, layout, None);
            let want: Vec<u64> = answers.iter().map(|answer| answer.len() as u64).collect();
            assert_eq!(
                expr::vec_int(&cardinalities, &snapshot).unwrap(),
                want,
                "{name} cardinalities in {layout:?}"
            );

            for upper in [0, 1, 2, 9, 10, 23, u64::MAX] {
                let ranks = mapped(body.clone(), key, layout, Some(upper));
                let want: Vec<u64> = answers
                    .iter()
                    .map(|answer| answer.range(..upper).count() as u64)
                    .collect();
                assert_eq!(
                    expr::vec_int(&ranks, &snapshot).unwrap(),
                    want,
                    "{name} ranks below {upper} in {layout:?}"
                );
            }
        }
    }
}
