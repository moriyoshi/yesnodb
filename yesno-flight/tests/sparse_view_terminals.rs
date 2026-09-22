//! Oracle coverage for the extremely sparse direct-key view plan.
//!
//! One physical ordinal in each occupied chunk is the narrow production
//! admission shape. The public Flight terminals are checked against eager core
//! view operations both in the memtable and after checkpoint/reopen.

use yesno_core::view::{Reduce, View};
use yesno_core::{ContainerKind, Db, OrdSet, Snapshot};
use yesno_flight::expr;
use yesno_flight::{FoldOp, IntExpr, SetExpr, VecIntExpr, VecSetExpr, ViewSpec};

const KEY: u64 = 91;
const SETS: u32 = 4;
const CHUNKS: u64 = 96;

fn physical_values() -> Vec<u64> {
    (0..CHUNKS)
        .map(|prefix| {
            // Cycle owners and vary the low bits without ever creating a
            // second value in one physical chunk.
            let low = (prefix * 997 + prefix % u64::from(SETS)) % 65_535;
            (prefix << 16) | low
        })
        .collect()
}

fn cardinalities() -> VecIntExpr {
    VecIntExpr::Map(
        Box::new(VecSetExpr::View(
            Box::new(SetExpr::Key(KEY)),
            ViewSpec::interleaved(SETS),
        )),
        Box::new(IntExpr::Cardinality(Box::new(SetExpr::Hole))),
    )
}

fn ranks(upper: u64) -> VecIntExpr {
    VecIntExpr::Map(
        Box::new(VecSetExpr::View(
            Box::new(SetExpr::Key(KEY)),
            ViewSpec::interleaved(SETS),
        )),
        Box::new(IntExpr::Rank(Box::new(SetExpr::Hole), upper)),
    )
}

fn fold(op: FoldOp) -> SetExpr {
    SetExpr::Fold(
        Box::new(VecSetExpr::View(
            Box::new(SetExpr::Key(KEY)),
            ViewSpec::interleaved(SETS),
        )),
        op,
    )
}

fn assert_terminals(snapshot: &Snapshot, eager: &OrdSet, phase: &str) {
    let view = View::interleaved(SETS);
    assert_eq!(
        expr::vec_int(&cardinalities(), snapshot).unwrap(),
        eager.view_cardinalities(&view),
        "{phase} cardinalities",
    );
    let boundary_ordinal = physical_values()[CHUNKS as usize / 2];
    let upper = boundary_ordinal / u64::from(SETS) + 1;
    let want: Vec<u64> = (0..SETS)
        .map(|owner| eager.view_select(&view, owner).rank(upper))
        .collect();
    assert_eq!(
        expr::vec_int(&ranks(upper), snapshot).unwrap(),
        want,
        "{phase} ranks below {upper}",
    );
    for (op, reduce) in [
        (FoldOp::Or, Reduce::Any),
        (FoldOp::And, Reduce::All),
        (FoldOp::Xor, Reduce::Parity),
    ] {
        let got = expr::lower(&fold(op), snapshot)
            .unwrap()
            .collect_set()
            .unwrap();
        assert_eq!(got, eager.view_fold(&view, reduce), "{phase} {op:?}");
    }
}

#[test]
fn extremely_sparse_direct_view_terminals_agree_before_and_after_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let db = Db::open(directory.path()).unwrap();
    db.insert_many(KEY, &physical_values()).unwrap();

    let resident = db.snapshot().unwrap();
    let eager = resident.load(KEY).unwrap();
    assert_eq!(eager.chunk_count(), CHUNKS as usize);
    assert!(eager.chunks().all(|(_, container)| {
        container.kind() == ContainerKind::Array && container.len() == 1
    }));
    assert_terminals(&resident, &eager, "resident");
    drop(resident);

    db.checkpoint().unwrap();
    drop(db);

    let db = Db::open(directory.path()).unwrap();
    let reopened = db.snapshot().unwrap();
    let eager = reopened.load(KEY).unwrap();
    assert!(eager.chunks().all(|(_, container)| {
        container.kind() == ContainerKind::Array && container.len() == 1
    }));
    assert_terminals(&reopened, &eager, "reopened");
}
