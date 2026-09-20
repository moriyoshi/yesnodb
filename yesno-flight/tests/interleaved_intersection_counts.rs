//! Oracle coverage for bounded and full-scan interleaved intersection counts.
//!
//! An odd view arity makes selected logical rows cross chunk boundaries. Sparse
//! filters take the bounded path, a dense filter takes the full scan, and two
//! sibling filters share one source traversal through `vec_int_batch`. Each is
//! checked before checkpoint and after reopen.

use std::collections::BTreeSet;

use yesno_core::{Db, OrdSet};
use yesno_flight::expr;
use yesno_flight::{IntExpr, SetExpr, VecIntExpr, VecSetExpr, ViewSpec};

const SETS: u32 = 513;
const WIDTH: u64 = 520;
const PACKED_KEY: u64 = 9;

fn expression(filter_key: u64) -> VecIntExpr {
    VecIntExpr::Map(
        Box::new(VecSetExpr::View(
            Box::new(SetExpr::Key(PACKED_KEY)),
            ViewSpec::interleaved(SETS),
        )),
        Box::new(IntExpr::Cardinality(Box::new(SetExpr::And(vec![
            SetExpr::Hole,
            SetExpr::Key(filter_key),
        ])))),
    )
}

fn oracle(parts: &[BTreeSet<u64>], filter: &BTreeSet<u64>) -> Vec<u64> {
    parts
        .iter()
        .map(|part| part.intersection(filter).count() as u64)
        .collect()
}

fn check(
    db: &Db,
    sparse_a: &BTreeSet<u64>,
    sparse_b: &BTreeSet<u64>,
    dense: &BTreeSet<u64>,
    parts: &[BTreeSet<u64>],
    phase: &str,
) {
    let snapshot = db.snapshot().unwrap();
    let expressions = [expression(7), expression(8)];
    let want_a = oracle(parts, sparse_a);
    let want_b = oracle(parts, sparse_b);
    assert_eq!(
        expr::vec_int(&expressions[0], &snapshot).unwrap(),
        want_a,
        "{phase} sparse a"
    );
    assert_eq!(
        expr::vec_int(&expressions[1], &snapshot).unwrap(),
        want_b,
        "{phase} sparse b"
    );
    assert_eq!(
        expr::vec_int_batch(&expressions, &snapshot).unwrap(),
        vec![want_a, want_b],
        "{phase} batch"
    );
    assert_eq!(
        expr::vec_int(&expression(10), &snapshot).unwrap(),
        oracle(parts, dense),
        "{phase} dense full scan"
    );
}

#[test]
fn interleaved_counts_clip_boundary_rows_and_share_sibling_scans_after_reopen() {
    let parts: Vec<BTreeSet<u64>> = (0..SETS as u64)
        .map(|owner| {
            (0..WIDTH)
                .filter(|x| (x * 7 + owner * 11) % 13 < 5)
                .collect()
        })
        .collect();
    let sparse_a = BTreeSet::from([0, 1, 127, 128, 255, 256, 383, 384, 519]);
    let sparse_b = BTreeSet::from([2, 126, 127, 129, 254, 257, 511, 519]);
    let dense: BTreeSet<u64> = (0..WIDTH).collect();
    let packed = OrdSet::from_iter_unsorted(parts.iter().enumerate().flat_map(|(owner, part)| {
        part.iter()
            .map(move |logical| logical * SETS as u64 + owner as u64)
    }));

    let directory = tempfile::tempdir().unwrap();
    let db = Db::open(directory.path()).unwrap();
    let mut batch = db.batch();
    batch.store_set(7, &OrdSet::from_iter_unsorted(sparse_a.iter().copied()));
    batch.store_set(8, &OrdSet::from_iter_unsorted(sparse_b.iter().copied()));
    batch.store_set(10, &OrdSet::from_iter_unsorted(dense.iter().copied()));
    batch.store_set(PACKED_KEY, &packed);
    batch.commit().unwrap();

    check(&db, &sparse_a, &sparse_b, &dense, &parts, "resident");
    db.checkpoint().unwrap();
    drop(db);

    let db = Db::open(directory.path()).unwrap();
    check(&db, &sparse_a, &sparse_b, &dense, &parts, "reopened");
}
