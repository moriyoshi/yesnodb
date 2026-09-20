//! Oracle coverage for folds over mapped selection from packed views.
//!
//! Production walks the packed representation once. These tests instead select
//! from independent logical `BTreeSet` constituents and reduce those singleton
//! sets, so they do not share the implementation's counters or physical order.

use std::collections::BTreeSet;

use yesno_core::Db;
use yesno_flight::expr;
use yesno_flight::{FoldOp, SetExpr, VecSetExpr, ViewSpec};

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

fn selected(part: &BTreeSet<u64>, n: u64) -> BTreeSet<u64> {
    part.iter().nth(n as usize).copied().into_iter().collect()
}

fn query(key: u64, view: ViewSpec, n: u64, op: FoldOp) -> SetExpr {
    SetExpr::Fold(
        Box::new(VecSetExpr::Map(
            Box::new(VecSetExpr::View(Box::new(SetExpr::Key(key)), view)),
            Box::new(SetExpr::Select(Box::new(SetExpr::Hole), n)),
        )),
        op,
    )
}

#[test]
fn mapped_select_folds_agree_with_an_independent_set_oracle() {
    let parts = [
        BTreeSet::from([4, 9, 12, 20]),
        BTreeSet::from([4, 10, 13]),
        BTreeSet::from([4, 8, 11, 14, 21]),
        BTreeSet::from([4, 6]),
    ];
    let db = Db::new();
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
    for (sets, key, view) in frames {
        for n in [0, 1, 2, 3, 99] {
            let selections: Vec<_> = parts[..sets].iter().map(|part| selected(part, n)).collect();
            for op in [FoldOp::Or, FoldOp::And, FoldOp::Xor] {
                let want = reduce(selections.clone(), op);
                let got: BTreeSet<_> = expr::lower(&query(key, view, n, op), &snapshot)
                    .unwrap()
                    .collect_set()
                    .unwrap()
                    .iter()
                    .collect();
                assert_eq!(
                    got, want,
                    "{op:?} of selection {n} over {view:?} at arity {sets}"
                );
            }
        }
    }
}
