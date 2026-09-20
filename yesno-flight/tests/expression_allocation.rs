//! Allocation regression tests for terminal operations over views.
//!
//! Correctness tests cannot distinguish a terminal fused with a view from one
//! that first materializes every constituent: both return the same value. The
//! distinguishing contract is that asking for one element, a vector of counts,
//! or a vector of membership bits must not allocate once per constituent.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use yesno_core::{Db, OrdSet};
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

fn union_cardinalities(sets: u32) -> VecIntExpr {
    VecIntExpr::Map(
        Box::new(view(sets)),
        Box::new(IntExpr::Cardinality(Box::new(SetExpr::Or(vec![
            SetExpr::Hole,
            SetExpr::Key(7),
        ])))),
    )
}

fn repeated_hole_cardinalities(sets: u32) -> VecIntExpr {
    VecIntExpr::Map(
        Box::new(view(sets)),
        Box::new(IntExpr::Cardinality(Box::new(SetExpr::AndNot(
            Box::new(SetExpr::Or(vec![SetExpr::Hole, SetExpr::Key(7)])),
            Box::new(SetExpr::And(vec![SetExpr::Hole, SetExpr::Key(8)])),
        )))),
    )
}

fn union_ranks(sets: u32) -> VecIntExpr {
    VecIntExpr::Map(
        Box::new(view(sets)),
        Box::new(IntExpr::Rank(
            Box::new(SetExpr::Or(vec![SetExpr::Hole, SetExpr::Key(7)])),
            750,
        )),
    )
}

#[test]
fn pointwise_boolean_terminals_have_bounded_allocation_growth() {
    let db = Db::new();
    db.insert_range(9, 0, 262_143).unwrap();
    db.insert_range(7, 0, 1_000).unwrap();
    db.insert_range(8, 500, 1_500).unwrap();
    let snapshot = db.snapshot().unwrap();

    let union_small = union_cardinalities(4);
    let union_large = union_cardinalities(64);
    let repeated_small = repeated_hole_cardinalities(4);
    let repeated_large = repeated_hole_cardinalities(64);
    let rank_small = union_ranks(4);
    let rank_large = union_ranks(64);

    for expression in [
        &union_small,
        &union_large,
        &repeated_small,
        &repeated_large,
        &rank_small,
        &rank_large,
    ] {
        let _ = expr::vec_int(expression, &snapshot).unwrap();
    }

    let (union_small_result, union_small_allocs) =
        count_allocations(|| expr::vec_int(&union_small, &snapshot).unwrap());
    let (union_large_result, union_large_allocs) =
        count_allocations(|| expr::vec_int(&union_large, &snapshot).unwrap());
    let (repeated_small_result, repeated_small_allocs) =
        count_allocations(|| expr::vec_int(&repeated_small, &snapshot).unwrap());
    let (repeated_large_result, repeated_large_allocs) =
        count_allocations(|| expr::vec_int(&repeated_large, &snapshot).unwrap());
    let (rank_small_result, rank_small_allocs) =
        count_allocations(|| expr::vec_int(&rank_small, &snapshot).unwrap());
    let (rank_large_result, rank_large_allocs) =
        count_allocations(|| expr::vec_int(&rank_large, &snapshot).unwrap());

    assert_eq!(union_small_result.len(), 4);
    assert_eq!(union_large_result.len(), 64);
    assert_eq!(repeated_small_result.len(), 4);
    assert_eq!(repeated_large_result.len(), 64);
    assert_eq!(rank_small_result.len(), 4);
    assert_eq!(rank_large_result.len(), 64);

    const ALLOWANCE: u64 = 16;
    let union_bounded = union_large_allocs <= union_small_allocs + ALLOWANCE;
    let repeated_bounded = repeated_large_allocs <= repeated_small_allocs + ALLOWANCE;
    let rank_bounded = rank_large_allocs <= rank_small_allocs + ALLOWANCE;
    assert!(
        union_bounded && repeated_bounded && rank_bounded,
        "allocations at 4 -> 64 constituents: union cardinality map \
         {union_small_allocs} -> {union_large_allocs}, repeated-hole cardinality map \
         {repeated_small_allocs} -> {repeated_large_allocs}, union rank map \
         {rank_small_allocs} -> {rank_large_allocs}; each terminal may grow by at most \
         {ALLOWANCE}"
    );
}

fn mapped_fold(sets: u32, body: SetExpr, op: FoldOp) -> SetExpr {
    SetExpr::Fold(
        Box::new(VecSetExpr::Map(Box::new(view(sets)), Box::new(body))),
        op,
    )
}

fn union_body() -> SetExpr {
    SetExpr::Or(vec![SetExpr::Hole, SetExpr::Key(7)])
}

fn invariant_difference_body() -> SetExpr {
    SetExpr::AndNot(Box::new(SetExpr::Key(7)), Box::new(SetExpr::Hole))
}

fn repeated_hole_body() -> SetExpr {
    SetExpr::AndNot(
        Box::new(SetExpr::Or(vec![SetExpr::Hole, SetExpr::Key(7)])),
        Box::new(SetExpr::And(vec![SetExpr::Hole, SetExpr::Key(8)])),
    )
}

type BodyFactory = fn() -> SetExpr;

#[test]
fn pointwise_mapped_folds_have_bounded_allocation_growth() {
    let db = Db::new();
    db.insert_range(9, 0, 262_143).unwrap();
    db.insert_range(7, 0, 1_000).unwrap();
    db.insert_range(8, 500, 1_500).unwrap();
    let snapshot = db.snapshot().unwrap();

    let bodies: [(&str, BodyFactory); 3] = [
        ("union", union_body),
        ("invariant difference", invariant_difference_body),
        ("repeated hole", repeated_hole_body),
    ];
    let ops = [FoldOp::Or, FoldOp::And, FoldOp::Xor];

    for (name, body) in bodies {
        for op in ops {
            let small = mapped_fold(4, body(), op);
            let large = mapped_fold(64, body(), op);

            let _ = expr::lower(&small, &snapshot)
                .unwrap()
                .collect_set()
                .unwrap();
            let _ = expr::lower(&large, &snapshot)
                .unwrap()
                .collect_set()
                .unwrap();

            let (_, small_allocs) = count_allocations(|| {
                expr::lower(&small, &snapshot)
                    .unwrap()
                    .collect_set()
                    .unwrap()
            });
            let (_, large_allocs) = count_allocations(|| {
                expr::lower(&large, &snapshot)
                    .unwrap()
                    .collect_set()
                    .unwrap()
            });

            const ALLOWANCE: u64 = 16;
            assert!(
                large_allocs <= small_allocs + ALLOWANCE,
                "{name} with {op:?} allocated {small_allocs} -> {large_allocs} at 4 -> 64 \
                 constituents; growth may be at most {ALLOWANCE}"
            );
        }
    }
}

fn mapped_select_fold(sets: u32, op: FoldOp) -> SetExpr {
    mapped_fold(sets, SetExpr::Select(Box::new(SetExpr::Hole), 1_000), op)
}

#[test]
fn mapped_select_folds_have_bounded_allocation_growth() {
    let db = Db::new();
    db.insert_range(9, 0, 262_143).unwrap();
    let snapshot = db.snapshot().unwrap();

    for op in [FoldOp::Or, FoldOp::And, FoldOp::Xor] {
        let small = mapped_select_fold(4, op);
        let large = mapped_select_fold(64, op);

        let _ = expr::lower(&small, &snapshot)
            .unwrap()
            .collect_set()
            .unwrap();
        let _ = expr::lower(&large, &snapshot)
            .unwrap()
            .collect_set()
            .unwrap();

        let (small_result, small_allocs) = count_allocations(|| {
            expr::lower(&small, &snapshot)
                .unwrap()
                .collect_set()
                .unwrap()
        });
        let (large_result, large_allocs) = count_allocations(|| {
            expr::lower(&large, &snapshot)
                .unwrap()
                .collect_set()
                .unwrap()
        });

        assert_eq!(small_result, large_result);
        const ALLOWANCE: u64 = 16;
        assert!(
            large_allocs <= small_allocs + ALLOWANCE,
            "mapped selection with {op:?} allocated {small_allocs} -> {large_allocs} at 4 -> 64 \
             constituents; growth may be at most {ALLOWANCE}"
        );
    }
}

fn nested_filtered_cardinalities(sets: u32) -> VecIntExpr {
    VecIntExpr::Map(
        Box::new(VecSetExpr::Map(
            Box::new(view(sets)),
            Box::new(SetExpr::And(vec![SetExpr::Hole, SetExpr::Key(7)])),
        )),
        Box::new(IntExpr::Cardinality(Box::new(SetExpr::Hole))),
    )
}

#[test]
fn nested_cardinality_maps_share_the_direct_terminal_allocation_shape() {
    let db = Db::new();
    db.insert_range(9, 0, 262_143).unwrap();
    db.insert_range(7, 0, 1_000).unwrap();
    let snapshot = db.snapshot().unwrap();

    let direct_small = filtered_cardinalities(4);
    let direct_large = filtered_cardinalities(64);
    let nested_small = nested_filtered_cardinalities(4);
    let nested_large = nested_filtered_cardinalities(64);
    for expression in [&direct_small, &direct_large, &nested_small, &nested_large] {
        let _ = expr::vec_int(expression, &snapshot).unwrap();
    }

    let (direct_small_result, direct_small_allocs) =
        count_allocations(|| expr::vec_int(&direct_small, &snapshot).unwrap());
    let (direct_large_result, direct_large_allocs) =
        count_allocations(|| expr::vec_int(&direct_large, &snapshot).unwrap());
    let (nested_small_result, nested_small_allocs) =
        count_allocations(|| expr::vec_int(&nested_small, &snapshot).unwrap());
    let (nested_large_result, nested_large_allocs) =
        count_allocations(|| expr::vec_int(&nested_large, &snapshot).unwrap());

    assert_eq!(nested_small_result, direct_small_result);
    assert_eq!(nested_large_result, direct_large_result);
    const ALLOWANCE: u64 = 16;
    assert!(
        nested_small_allocs <= direct_small_allocs + ALLOWANCE
            && nested_large_allocs <= direct_large_allocs + ALLOWANCE
            && nested_large_allocs <= nested_small_allocs + ALLOWANCE,
        "allocations for direct 4/64 and nested 4/64 cardinality maps were \
         {direct_small_allocs}/{direct_large_allocs} and \
         {nested_small_allocs}/{nested_large_allocs}; normalization may add at most \
         {ALLOWANCE} allocations and growth must stay bounded"
    );
}

fn blocked_filtered_cardinalities(key: u64, sets: u32, stride: u64) -> VecIntExpr {
    VecIntExpr::Map(
        Box::new(VecSetExpr::View(
            Box::new(SetExpr::Key(key)),
            ViewSpec::blocked(sets, stride),
        )),
        Box::new(IntExpr::Cardinality(Box::new(SetExpr::And(vec![
            SetExpr::Hole,
            SetExpr::Key(20),
        ])))),
    )
}

fn dense_blocked(sets: u32, stride: u64) -> OrdSet {
    OrdSet::from_iter_unsorted((0..sets as u64).flat_map(|owner| {
        (0..stride)
            .filter(move |x| (x + owner) % 2 == 0)
            .map(move |x| owner * stride + x)
    }))
}

#[test]
fn blocked_bitmap_intersection_counts_do_not_materialize_constituents() {
    let db = Db::new();
    let mut batch = db.batch();
    batch.store_set(20, &OrdSet::from_iter_unsorted((0..4_096).step_by(3)));
    // Both frames occupy exactly four physical chunks. Only their row shape
    // differs, so allocation growth here detects per-constituent intermediates
    // rather than charging the larger case for more input payloads.
    batch.store_set(21, &dense_blocked(4, 65_536));
    batch.store_set(22, &dense_blocked(64, 4_096));
    batch.commit().unwrap();
    let snapshot = db.snapshot().unwrap();
    let small = blocked_filtered_cardinalities(21, 4, 65_536);
    let large = blocked_filtered_cardinalities(22, 64, 4_096);

    let _ = expr::vec_int(&small, &snapshot).unwrap();
    let _ = expr::vec_int(&large, &snapshot).unwrap();
    let (small_result, small_allocs) =
        count_allocations(|| expr::vec_int(&small, &snapshot).unwrap());
    let (large_result, large_allocs) =
        count_allocations(|| expr::vec_int(&large, &snapshot).unwrap());

    assert_eq!(small_result.len(), 4);
    assert_eq!(large_result.len(), 64);
    assert!(small_result.iter().all(|count| *count > 0));
    assert!(large_result.iter().all(|count| *count > 0));
    const ALLOWANCE: u64 = 16;
    assert!(
        large_allocs <= small_allocs + ALLOWANCE,
        "blocked intersection counts allocated {small_allocs} -> {large_allocs} at 4 -> 64 \
         constituents over the same four chunks; growth may be at most {ALLOWANCE}"
    );
}

fn filtered_cardinalities_for(sets: u32, filter_key: u64) -> VecIntExpr {
    VecIntExpr::Map(
        Box::new(view(sets)),
        Box::new(IntExpr::Cardinality(Box::new(SetExpr::And(vec![
            SetExpr::Hole,
            SetExpr::Key(filter_key),
        ])))),
    )
}

#[test]
fn sibling_intersection_counts_share_source_planning_and_traversal() {
    let db = Db::new();
    db.insert_range(9, 0, 262_143).unwrap();
    db.insert_range(7, 0, 1_000).unwrap();
    db.insert_range(8, 900, 2_000).unwrap();
    let snapshot = db.snapshot().unwrap();
    let expressions = [
        filtered_cardinalities_for(64, 7),
        filtered_cardinalities_for(64, 8),
    ];

    let _ = expr::vec_int_batch(&expressions, &snapshot).unwrap();
    for expression in &expressions {
        let _ = expr::vec_int(expression, &snapshot).unwrap();
    }
    let (batch_result, batch_allocs) =
        count_allocations(|| expr::vec_int_batch(&expressions, &snapshot).unwrap());
    let (independent_result, independent_allocs) = count_allocations(|| {
        expressions
            .iter()
            .map(|expression| expr::vec_int(expression, &snapshot).unwrap())
            .collect::<Vec<_>>()
    });

    assert_eq!(batch_result, independent_result);
    assert!(
        batch_allocs < independent_allocs,
        "batched siblings allocated {batch_allocs} times versus {independent_allocs} for two \
         independent traversals; the shared entrypoint must remove source work"
    );
}
