//! Oracle coverage for direct intersection counts over blocked bitmap views.
//!
//! The first sixteen rows form a bitmap container and the seventeenth forms a
//! partial array tail. The same wire expression is checked before checkpoint
//! and after reopen so mutable and store-backed bitmap payloads share semantics.

use std::collections::BTreeSet;

use yesno_core::{ContainerKind, Db, OrdSet};
use yesno_flight::expr;
use yesno_flight::{IntExpr, SetExpr, VecIntExpr, VecSetExpr, ViewSpec};

const SETS: u32 = 17;
const STRIDE: u64 = 4_096;

fn expression() -> VecIntExpr {
    VecIntExpr::Map(
        Box::new(VecSetExpr::View(
            Box::new(SetExpr::Key(9)),
            ViewSpec::blocked(SETS, STRIDE),
        )),
        Box::new(IntExpr::Cardinality(Box::new(SetExpr::And(vec![
            SetExpr::Hole,
            SetExpr::Key(7),
            SetExpr::Key(8),
        ])))),
    )
}

fn evaluate(db: &Db) -> Vec<u64> {
    expr::vec_int(&expression(), &db.snapshot().unwrap()).unwrap()
}

#[test]
fn blocked_bitmap_intersection_counts_match_an_independent_oracle_before_and_after_reopen() {
    let parts: Vec<BTreeSet<u64>> = (0..SETS as u64)
        .map(|owner| (0..STRIDE).filter(|x| (x + owner * 11) % 3 != 0).collect())
        .collect();
    let a: BTreeSet<u64> = (0..STRIDE).filter(|x| x % 5 <= 1).collect();
    let b: BTreeSet<u64> = (0..STRIDE).filter(|x| (x + 3) % 7 <= 2).collect();
    let packed = OrdSet::from_iter_unsorted(
        parts
            .iter()
            .enumerate()
            .flat_map(|(owner, row)| row.iter().map(move |x| owner as u64 * STRIDE + x)),
    );
    assert!(packed
        .chunks()
        .any(|(_, container)| container.kind() == ContainerKind::Bitmap));

    let want: Vec<u64> = parts
        .iter()
        .map(|row| row.intersection(&a).filter(|x| b.contains(x)).count() as u64)
        .collect();

    let directory = tempfile::tempdir().unwrap();
    let db = Db::open(directory.path()).unwrap();
    let mut batch = db.batch();
    batch.store_set(7, &OrdSet::from_iter_unsorted(a));
    batch.store_set(8, &OrdSet::from_iter_unsorted(b));
    batch.store_set(9, &packed);
    batch.commit().unwrap();

    assert_eq!(evaluate(&db), want, "resident");
    db.checkpoint().unwrap();
    drop(db);

    let db = Db::open(directory.path()).unwrap();
    assert_eq!(evaluate(&db), want, "reopened");
}
