//! Allocation regression tests for terminal operations over views.
//!
//! Correctness tests cannot distinguish a terminal fused with a view from one
//! that first materializes every constituent: both return the same value. The
//! distinguishing contract is that asking for one element, a vector of counts,
//! or a vector of membership bits must not allocate once per constituent.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use yesno_core::Db;
use yesno_flight::expr;
use yesno_flight::{BoolExpr, FoldOp, IntExpr, SetExpr, VecIntExpr, VecSetExpr, ViewSpec};

thread_local! {
    static ALLOCS: Cell<u64> = const { Cell::new(0) };
    static COUNTING: Cell<bool> = const { Cell::new(false) };
}

struct Counting;

fn bump() {
    let _ = COUNTING.try_with(|on| {
        if on.get() {
            let _ = ALLOCS.try_with(|count| count.set(count.get() + 1));
        }
    });
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        bump();
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        bump();
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn count_allocations<T>(f: impl FnOnce() -> T) -> (T, u64) {
    ALLOCS.with(|count| count.set(0));
    COUNTING.with(|on| on.set(true));
    let value = f();
    COUNTING.with(|on| on.set(false));
    (value, ALLOCS.with(Cell::get))
}

fn view(sets: u32) -> VecSetExpr {
    VecSetExpr::View(Box::new(SetExpr::Key(9)), ViewSpec::interleaved(sets))
}

fn mapped_at(sets: u32) -> SetExpr {
    SetExpr::At(
        Box::new(VecSetExpr::Map(
            Box::new(view(sets)),
            Box::new(SetExpr::Hole),
        )),
        0,
    )
}

fn cardinalities(sets: u32) -> VecIntExpr {
    VecIntExpr::Map(
        Box::new(view(sets)),
        Box::new(IntExpr::Cardinality(Box::new(SetExpr::Hole))),
    )
}

fn filtered_cardinalities(sets: u32) -> VecIntExpr {
    VecIntExpr::Map(
        Box::new(view(sets)),
        Box::new(IntExpr::Cardinality(Box::new(SetExpr::And(vec![
            SetExpr::Hole,
            SetExpr::Key(7),
        ])))),
    )
}

fn folded_filter(sets: u32) -> SetExpr {
    SetExpr::Fold(
        Box::new(VecSetExpr::Map(
            Box::new(view(sets)),
            Box::new(SetExpr::And(vec![SetExpr::Hole, SetExpr::Key(7)])),
        )),
        FoldOp::Or,
    )
}

fn membership(sets: u32) -> SetExpr {
    SetExpr::MapBool(
        Box::new(view(sets)),
        Box::new(BoolExpr::Contains(Box::new(SetExpr::Hole), 17)),
    )
}

#[test]
fn view_terminals_do_not_allocate_once_per_constituent() {
    let db = Db::new();
    db.insert_range(9, 0, 262_143).unwrap();
    db.insert_range(7, 0, 1_000).unwrap();
    let snapshot = db.snapshot().unwrap();

    let at_small = mapped_at(4);
    let at_large = mapped_at(64);
    let counts_small = cardinalities(4);
    let filtered_small = filtered_cardinalities(4);
    let filtered_large = filtered_cardinalities(64);
    let fold_small = folded_filter(4);
    let fold_large = folded_filter(64);
    let counts_large = cardinalities(64);
    let membership_small = membership(4);
    let membership_large = membership(64);

    // Warm every path before measuring lazy initialization.
    let _ = expr::lower(&at_small, &snapshot)
        .unwrap()
        .collect_set()
        .unwrap();
    let _ = expr::lower(&at_large, &snapshot)
        .unwrap()
        .collect_set()
        .unwrap();
    let _ = expr::vec_int(&counts_small, &snapshot).unwrap();
    let _ = expr::vec_int(&filtered_small, &snapshot).unwrap();
    let _ = expr::vec_int(&filtered_large, &snapshot).unwrap();
    let _ = expr::lower(&fold_small, &snapshot)
        .unwrap()
        .collect_set()
        .unwrap();
    let _ = expr::lower(&fold_large, &snapshot)
        .unwrap()
        .collect_set()
        .unwrap();
    let _ = expr::vec_int(&counts_large, &snapshot).unwrap();
    let _ = expr::lower(&membership_small, &snapshot)
        .unwrap()
        .collect_set()
        .unwrap();
    let _ = expr::lower(&membership_large, &snapshot)
        .unwrap()
        .collect_set()
        .unwrap();

    let (at_result, at_small_allocs) = count_allocations(|| {
        expr::lower(&at_small, &snapshot)
            .unwrap()
            .collect_set()
            .unwrap()
    });
    let (_, at_large_allocs) = count_allocations(|| {
        expr::lower(&at_large, &snapshot)
            .unwrap()
            .collect_set()
            .unwrap()
    });
    assert!(!at_result.is_empty());

    let (small_counts, counts_small_allocs) =
        count_allocations(|| expr::vec_int(&counts_small, &snapshot).unwrap());
    let (large_counts, counts_large_allocs) =
        count_allocations(|| expr::vec_int(&counts_large, &snapshot).unwrap());
    assert_eq!(small_counts.iter().sum::<u64>(), 262_144);
    assert_eq!(large_counts.iter().sum::<u64>(), 262_144);

    let (small_filtered, filtered_small_allocs) =
        count_allocations(|| expr::vec_int(&filtered_small, &snapshot).unwrap());
    let (large_filtered, filtered_large_allocs) =
        count_allocations(|| expr::vec_int(&filtered_large, &snapshot).unwrap());
    assert_eq!(small_filtered.iter().sum::<u64>(), 4_004);
    assert_eq!(large_filtered.iter().sum::<u64>(), 64_064);

    let (small_fold, fold_small_allocs) = count_allocations(|| {
        expr::lower(&fold_small, &snapshot)
            .unwrap()
            .collect_set()
            .unwrap()
    });
    let (large_fold, fold_large_allocs) = count_allocations(|| {
        expr::lower(&fold_large, &snapshot)
            .unwrap()
            .collect_set()
            .unwrap()
    });
    assert_eq!(small_fold.len(), 1_001);
    assert_eq!(large_fold.len(), 1_001);

    let (members, membership_small_allocs) = count_allocations(|| {
        expr::lower(&membership_small, &snapshot)
            .unwrap()
            .collect_set()
            .unwrap()
    });
    let (_, membership_large_allocs) = count_allocations(|| {
        expr::lower(&membership_large, &snapshot)
            .unwrap()
            .collect_set()
            .unwrap()
    });
    assert_eq!(members.len(), 4);

    // The input key spans the same four chunks in both frames. Increasing only
    // the view arity by 16x may grow the returned vector or set, but must not
    // materialize 60 additional constituents. The small allowance covers the
    // output collection's size-class changes.
    const ALLOWANCE: u64 = 16;
    let at_bounded = at_large_allocs <= at_small_allocs + ALLOWANCE;
    let counts_bounded = counts_large_allocs <= counts_small_allocs + ALLOWANCE;
    let filtered_bounded = filtered_large_allocs <= filtered_small_allocs + ALLOWANCE;
    let fold_bounded = fold_large_allocs <= fold_small_allocs + ALLOWANCE;
    let membership_bounded = membership_large_allocs <= membership_small_allocs + ALLOWANCE;
    assert!(
        at_bounded && counts_bounded && filtered_bounded && fold_bounded && membership_bounded,
        "allocations at 4 -> 64 constituents: At(Map(View, ..)) \
         {at_small_allocs} -> {at_large_allocs}, cardinality map \
         {counts_small_allocs} -> {counts_large_allocs}, filtered cardinality map \
         {filtered_small_allocs} -> {filtered_large_allocs}, mapped fold \
         {fold_small_allocs} -> {fold_large_allocs}, membership map \
         {membership_small_allocs} -> {membership_large_allocs}; each terminal \
         may grow by at most {ALLOWANCE}"
    );
}
