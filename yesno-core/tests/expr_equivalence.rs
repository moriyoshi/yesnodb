//! The M1 gate.
//!
//! Two properties, and the second is the one with teeth:
//!
//! 1. A lazy expression yields exactly what the eager `OrdSet` path yields.
//! 2. `cardinality()` equals `collect_set().len()`.
//!
//! Property 2 matters because the non-materializing cardinality walk is a
//! *parallel implementation* of the materializing one. Nothing but a
//! differential test stops the two from drifting apart, and a drift would be
//! silent — wrong counts, no error.

use std::collections::BTreeSet;
use std::sync::Arc;

use proptest::prelude::*;
use yesno_core::stream::plan::{Conservative, CostGuided, PlanStrategy};
use yesno_core::stream::{ChunkStream, ChunkStreamExt};
use yesno_core::{Expr, OrdSet};

/// A random expression over a fixed pool of leaf sets, evaluated two ways.
#[derive(Clone, Debug)]
enum Shape {
    Leaf(usize),
    And(Box<Shape>, Box<Shape>),
    Or(Box<Shape>, Box<Shape>),
    Xor(Box<Shape>, Box<Shape>),
    AndNot(Box<Shape>, Box<Shape>),
    /// Complement of the inner shape within `[lo, hi)`.
    Not(Box<Shape>, u64, u64),
    /// A literal range leaf, so the planner's range-absorption rules are
    /// reachable from the random trees rather than only from hand-written cases.
    Range(u64, u64),
}

/// Ranges for a NOT node, anchored on chunk boundaries and at the ceiling.
///
/// Kept **narrow** (< 2048 wide) on purpose. Width is not what this file is
/// testing — `proptest_oracle.rs` complements over multi-chunk ranges against a
/// brute-force oracle. What this file tests is NOT *composing*, nested inside
/// arbitrary AND / OR / XOR, and there the cost that matters is the `BTreeSet`
/// oracle, which is linear in the range width at every node of the tree.
fn not_range() -> impl Strategy<Value = (u64, u64)> {
    let anchor = prop::sample::select(vec![
        0u64,
        1,
        65_535,
        65_536,
        65_537,
        1 << 20,
        u64::MAX - 65_536,
        u64::MAX - 1,
        u64::MAX,
    ]);
    (anchor, 0u64..2048).prop_map(|(a, w)| (a, a.saturating_add(w)))
}

fn shape_strategy() -> impl Strategy<Value = Shape> {
    let leaf = (0usize..5).prop_map(Shape::Leaf);
    leaf.prop_recursive(4, 24, 2, |inner| {
        prop_oneof![
            (inner.clone(), inner.clone()).prop_map(|(a, b)| Shape::And(Box::new(a), Box::new(b))),
            (inner.clone(), inner.clone()).prop_map(|(a, b)| Shape::Or(Box::new(a), Box::new(b))),
            (inner.clone(), inner.clone()).prop_map(|(a, b)| Shape::Xor(Box::new(a), Box::new(b))),
            (inner.clone(), inner.clone())
                .prop_map(|(a, b)| Shape::AndNot(Box::new(a), Box::new(b))),
            (inner, not_range()).prop_map(|(a, (lo, hi))| Shape::Not(Box::new(a), lo, hi)),
            not_range().prop_map(|(lo, hi)| Shape::Range(lo, hi)),
        ]
    })
}

fn to_expr(s: &Shape, pool: &[Arc<OrdSet>]) -> Expr {
    match s {
        Shape::Leaf(i) => Expr::set(pool[*i % pool.len()].clone()),
        Shape::And(a, b) => to_expr(a, pool).and(to_expr(b, pool)),
        Shape::Or(a, b) => to_expr(a, pool).or(to_expr(b, pool)),
        Shape::Xor(a, b) => to_expr(a, pool).xor(to_expr(b, pool)),
        Shape::AndNot(a, b) => to_expr(a, pool).and_not(to_expr(b, pool)),
        Shape::Not(a, lo, hi) => to_expr(a, pool).not_in(*lo, *hi),
        Shape::Range(lo, hi) => Expr::Range(*lo, *hi),
    }
}

/// The same shape evaluated eagerly, as the independent oracle.
fn eval_eager(s: &Shape, pool: &[Arc<OrdSet>]) -> OrdSet {
    match s {
        Shape::Leaf(i) => (*pool[*i % pool.len()]).clone(),
        Shape::And(a, b) => eval_eager(a, pool).and(&eval_eager(b, pool)),
        Shape::Or(a, b) => eval_eager(a, pool).or(&eval_eager(b, pool)),
        Shape::Xor(a, b) => eval_eager(a, pool).xor(&eval_eager(b, pool)),
        Shape::AndNot(a, b) => eval_eager(a, pool).and_not(&eval_eager(b, pool)),
        Shape::Not(a, lo, hi) => eval_eager(a, pool).not_in_range(*lo, *hi),
        Shape::Range(lo, hi) => OrdSet::from_iter_unsorted(*lo..*hi),
    }
}

/// And once more against `BTreeSet`, so a shared bug in both paths still fails.
fn eval_oracle(s: &Shape, pool: &[BTreeSet<u64>]) -> BTreeSet<u64> {
    match s {
        Shape::Leaf(i) => pool[*i % pool.len()].clone(),
        Shape::And(a, b) => eval_oracle(a, pool)
            .intersection(&eval_oracle(b, pool))
            .copied()
            .collect(),
        Shape::Or(a, b) => eval_oracle(a, pool)
            .union(&eval_oracle(b, pool))
            .copied()
            .collect(),
        Shape::Xor(a, b) => eval_oracle(a, pool)
            .symmetric_difference(&eval_oracle(b, pool))
            .copied()
            .collect(),
        Shape::AndNot(a, b) => eval_oracle(a, pool)
            .difference(&eval_oracle(b, pool))
            .copied()
            .collect(),
        Shape::Not(a, lo, hi) => {
            let inner = eval_oracle(a, pool);
            (*lo..*hi).filter(|v| !inner.contains(v)).collect()
        }
        Shape::Range(lo, hi) => (*lo..*hi).collect(),
    }
}

fn pools() -> (Vec<Arc<OrdSet>>, Vec<BTreeSet<u64>>) {
    // Deliberately overlapping and multi-chunk, with one runny leaf so run
    // containers participate in the expression tree.
    let raw: Vec<Vec<u64>> = vec![
        (0..3000u64).map(|i| i * 3).collect(),
        (0..3000u64).map(|i| i * 5).collect(),
        (0..2000u64).map(|i| i * 65_536 / 7).collect(),
        (0..8000u64).collect(), // contiguous -> run container after optimize
        // The top of the address space.
        //
        // Every other leaf here lives below 2^25, so without this one no
        // expression ever exercises prefix arithmetic or a seek near the
        // ceiling — the same generator gap that let a range-walk overflow
        // survive four milestones in `Memtable`. The three clusters sit at
        // `u64::MAX`, at `1 << 63` and at `i64::MAX`, because a signed shift
        // anywhere on the path folds the upper half of the space onto the
        // lower and shows up on only one side of that line.
        {
            let mut v: Vec<u64> = (0..64u64).map(|i| yesno_core::ORDINAL_MAX - i).collect();
            v.extend((0..64u64).map(|i| (1u64 << 63) + i));
            v.extend((0..64u64).map(|i| (i64::MAX as u64) - i));
            v.sort_unstable();
            v.dedup();
            v
        },
    ];
    let sets = raw
        .iter()
        .map(|v| {
            let mut s = OrdSet::from_iter_unsorted(v.iter().copied());
            s.optimize();
            Arc::new(s)
        })
        .collect();
    let oracles = raw.iter().map(|v| v.iter().copied().collect()).collect();
    (sets, oracles)
}

/// The leaf pool must actually reach the top of the address space, and every
/// leaf must be selectable.
///
/// `to_expr` indexes with `pool[i % pool.len()]`, so a pool that grows without
/// the shape strategy's range growing to match leaves the tail unreachable —
/// and a leaf nothing ever selects is indistinguishable from one that is not
/// there. Both halves are asserted because this file spent four milestones with
/// a pool that topped out below 2^25, passing every property while exercising
/// nothing near the boundary.
#[test]
fn the_leaf_pool_reaches_the_ceiling_and_is_fully_selectable() {
    let (sets, oracles) = pools();
    assert_eq!(sets.len(), oracles.len());
    assert_eq!(
        sets.len(),
        5,
        "the shape strategy draws leaf indices from 0..5; a different pool size \
         silently makes leaves unreachable or aliases them"
    );
    assert!(
        sets.iter()
            .any(|s| s.max() == Some(yesno_core::ORDINAL_MAX)),
        "no leaf reaches the last ordinal"
    );
    assert!(
        sets.iter().any(|s| s.contains(1u64 << 63)),
        "no leaf sits at the signed-shift boundary"
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// The cardinality identity must agree with the merge it replaces.
    ///
    /// `Expr::cardinality` may answer `Or` / `Xor` via
    /// `|A| + |B| - |A ∩ B|` instead of merging, whenever the cost model says
    /// that is cheaper. Both compute the same number by entirely different
    /// routes, so this is a differential between the two strategies — and it
    /// needs no oracle, because the merge path is what every other test here
    /// already checks against `BTreeSet`.
    ///
    /// The generated trees in `planning_preserves_meaning` reach the identity
    /// **twice in 83 counts**: their `Range` leaves are under 2048 ordinals, so
    /// a range's `yield_chunks` is 1 and the gate declines. Multi-chunk ranges
    /// are what make it fire, and nothing else in the suite produces them.
    #[test]
    fn the_cardinality_identity_agrees_with_the_merge(
        vals in prop::collection::vec(0u64..300, 1..40),
        lo_chunk in 0u64..300,
        width in 1u64..40,
        off in 0u64..3,
    ) {
        let set = Arc::new(OrdSet::from_iter_unsorted(
            vals.iter().map(|c| (c << 16) | off),
        ));
        let lo = lo_chunk << 16;
        let hi = lo + (width << 16);
        for e in [
            Expr::set(set.clone()).or(Expr::Range(lo, hi)),
            Expr::Range(lo, hi).or(Expr::set(set.clone())),
            Expr::set(set.clone()).xor(Expr::Range(lo, hi)),
            Expr::Range(lo, hi).xor(Expr::set(set.clone())),
        ] {
            let identity = e.cardinality().unwrap();
            let merged = e.open_planned().cardinality_dyn().unwrap();
            prop_assert_eq!(identity, merged, "identity != merge for {:?}", e);
        }
    }

    /// Planning must not change what an expression means.
    ///
    /// The planner rewrites trees structurally, and a wrong rule returns a
    /// *plausible* set rather than an error — so this compares the planned
    /// lowering against the unplanned one and against the `BTreeSet` oracle.
    /// Adding a rewrite rule without this passing is how a query engine starts
    /// returning quietly wrong answers.
    #[test]
    fn planning_preserves_meaning(shape in shape_strategy()) {
        let (sets, oracles) = pools();
        let e = to_expr(&shape, &sets);

        let unplanned = e.open_planned().collect_set().unwrap();
        let oracle = eval_oracle(&shape, &oracles);

        // Every strategy, not just the default. A backend's whole obligation is
        // that it denotes the same set; a new one is not finished until it is
        // listed here.
        let strategies: [&dyn PlanStrategy; 2] = [&CostGuided, &Conservative];
        for st in strategies {
            let planned = e.plan_with(st).open_planned().collect_set().unwrap();
            prop_assert_eq!(
                planned.iter().collect::<Vec<_>>(),
                unplanned.iter().collect::<Vec<_>>(),
                "strategy {} changed the result",
                st.name()
            );
            prop_assert_eq!(
                planned.iter().collect::<Vec<_>>(),
                oracle.iter().copied().collect::<Vec<_>>(),
                "strategy {} != BTreeSet oracle",
                st.name()
            );
        }
        // Cardinality is a parallel implementation and must agree too.
        prop_assert_eq!(e.cardinality().unwrap(), unplanned.len());
    }

    #[test]
    fn lazy_expression_equals_eager_and_oracle(shape in shape_strategy()) {

        let (sets, oracles) = pools();

        let lazy = to_expr(&shape, &sets).collect_set().unwrap();
        let eager = eval_eager(&shape, &sets);
        let oracle = eval_oracle(&shape, &oracles);

        prop_assert_eq!(
            lazy.iter().collect::<Vec<_>>(),
            eager.iter().collect::<Vec<_>>(),
            "lazy != eager"
        );
        prop_assert_eq!(
            lazy.iter().collect::<Vec<_>>(),
            oracle.iter().copied().collect::<Vec<_>>(),
            "lazy != BTreeSet oracle"
        );
    }

    /// The test that catches a broken cardinality override.
    #[test]
    fn cardinality_equals_collect_len(shape in shape_strategy()) {
        let (sets, _) = pools();
        let e = to_expr(&shape, &sets);
        let counted = e.cardinality().unwrap();
        let materialized = e.collect_set().unwrap().len();
        prop_assert_eq!(counted, materialized);
    }

    #[test]
    fn is_empty_agrees_with_cardinality(shape in shape_strategy()) {
        let (sets, _) = pools();
        let e = to_expr(&shape, &sets);
        let empty = e.open().is_empty().unwrap();
        prop_assert_eq!(empty, e.cardinality().unwrap() == 0);
    }

    #[test]
    fn min_max_agree_with_materialized(shape in shape_strategy()) {
        let (sets, _) = pools();
        let e = to_expr(&shape, &sets);
        let m = e.collect_set().unwrap();
        prop_assert_eq!(e.open().min().unwrap(), m.min());
        prop_assert_eq!(e.open().max().unwrap(), m.max());
    }
}

#[test]
fn boxed_and_static_paths_agree() {
    let (sets, _) = pools();
    let a = sets[0].clone();
    let b = sets[1].clone();
    let c = sets[2].clone();

    // Statically composed.
    let stat = a
        .stream()
        .and(b.stream().or(c.stream()))
        .cardinality()
        .unwrap();
    // Type-erased through Box<dyn ChunkStream>.
    let dynamic = Expr::set(a)
        .and(Expr::set(b).or(Expr::set(c)))
        .cardinality()
        .unwrap();

    assert_eq!(stat, dynamic);
}

/// The generators must actually reach the nodes the tests exist to cover, and
/// the planner must actually fire on the trees they produce.
///
/// A generator that emits a `Not` in 3 draws of 258 reads as coverage and
/// provides none — this file has that exact history. The planner half is the
/// same hazard one level up: `planning_preserves_meaning` passes trivially if
/// `plan()` is a no-op on every tree it is handed.
#[test]
fn generators_reach_not_nodes_and_the_planner_rewrites_them() {
    use proptest::strategy::{Strategy, ValueTree};
    use proptest::test_runner::TestRunner;

    fn count_not(s: &Shape) -> usize {
        match s {
            Shape::Leaf(_) | Shape::Range(_, _) => 0,
            Shape::Not(a, _, _) => 1 + count_not(a),
            Shape::And(a, b) | Shape::Or(a, b) | Shape::Xor(a, b) | Shape::AndNot(a, b) => {
                count_not(a) + count_not(b)
            }
        }
    }

    let (sets, _) = pools();
    let mut runner = TestRunner::deterministic();
    let (mut with_not, mut nots, mut rewritten) = (0, 0, 0);
    for _ in 0..256 {
        let shape = shape_strategy().new_tree(&mut runner).unwrap().current();
        nots += count_not(&shape);
        if count_not(&shape) > 0 {
            with_not += 1;
        }
        let e = to_expr(&shape, &sets);
        if format!("{:?}", e.plan()) != format!("{e:?}") {
            rewritten += 1;
        }
    }
    println!("{nots} NOT nodes; {with_not}/256 shapes contain one; {rewritten}/256 replanned");
    assert!(
        with_not > 25,
        "only {with_not}/256 shapes contain a NOT node"
    );
    assert!(
        rewritten > 25,
        "the planner rewrote only {rewritten}/256 trees, so `planning_preserves_meaning` \
         is largely comparing an expression against itself"
    );
}

/// The unbounded complement must stay `O(chunks of the input)`.
///
/// Every operation here spans the whole `[0, ORDINAL_MAX]` universe — nearly
/// `2^48` chunks. Each must answer from the input instead of walking that, and a
/// correctness assertion cannot tell the difference because both return the same
/// value; only the elapsed time does. This is the test that caught two separate
/// regressions to a `2^48` walk: `AndNot`-driven cardinality, and `Expr::not_in`
/// desugaring to `AndNot(Range, x)` after the operator itself was fixed.
///
/// The work runs on a worker thread with a deadline, rather than being timed
/// after the fact. A regression here does not take five seconds — it takes about
/// a century — so an `assert!(elapsed < 5s)` placed after the call would never be
/// reached, and the failure would present as the whole suite hanging. This turns
/// that into an ordinary test failure.
///
/// **The deadline is scaled under Valgrind, and that is not a weakening.**
/// The bound discriminates between roughly a second and roughly a century, so
/// every value in between preserves all of its power. What it must not do is
/// measure *wall clock* in an environment 20-50x slower than native: on
/// 2026-09-06 this test failed the `--deep` gate under Valgrind on a machine at
/// load 26, with `ERROR SUMMARY: 0 errors` — the suite passed in 94 s when run
/// alone and the same binary blew a 30 s thread deadline under contention. That
/// is a false failure, and a gate that cries wolf is a gate people stop reading.
/// Do not "fix" a future occurrence by raising `NATIVE_DEADLINE`: 30 s native
/// is already three orders of magnitude above the real cost, so a native
/// timeout is a genuine regression, not a slow machine.
///
/// There is deliberately no eager `OrdSet::not()` to test here. It aborts for
/// every input; see `OrdSet::not_in_range`.
#[test]
fn the_unbounded_complement_is_answered_from_the_input_not_the_universe() {
    use std::sync::mpsc;
    use std::time::Duration;
    use yesno_core::ORDINAL_MAX;

    let vals: Vec<u64> = (0..5000u64).map(|i| i * 7_000_003).collect();
    let s = Arc::new(OrdSet::from_sorted_slice(&vals));
    let expect = u64::MAX - s.len();

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        assert_eq!(s.stream().not().cardinality().unwrap(), expect);
        assert_eq!((!Expr::set(s.clone())).cardinality().unwrap(), expect);

        // Usable, not merely countable: first chunk, a seek-driven probe, and
        // the minimum must all land without enumerating the universe.
        let mut st = s.stream().not();
        assert_eq!(st.next_chunk().unwrap().map(|(p, _)| p), Some(0));
        assert!(
            s.stream().not().contains(1).unwrap(),
            "1 is not in the input"
        );
        assert!(!s.stream().not().contains(0).unwrap(), "0 is in the input");
        assert_eq!(s.stream().not().min().unwrap(), Some(1));

        // Complement of the empty set is the whole universe, and it fits a u64 —
        // which is the entire point of reserving `u64::MAX` (invariant I8).
        let empty = Arc::new(OrdSet::new());
        assert_eq!(empty.stream().not().cardinality().unwrap(), u64::MAX);
        assert_eq!(u64::MAX, ORDINAL_MAX + 1);
        let _ = tx.send(());
    });

    // Valgrind announces itself by preloading its own shims, which is the only
    // detection that needs no build-time cooperation.
    const NATIVE_DEADLINE: u64 = 30;
    let slow = std::env::var("LD_PRELOAD")
        .map(|v| v.contains("vgpreload"))
        .unwrap_or(false);
    let deadline = Duration::from_secs(if slow {
        NATIVE_DEADLINE * 20
    } else {
        NATIVE_DEADLINE
    });

    assert!(
        rx.recv_timeout(deadline).is_ok(),
        "the unbounded complement is walking the universe instead of the input \
         ( deadline {deadline:?} )"
    );
}
