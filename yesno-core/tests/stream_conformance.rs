//! Proposition 6' of `docs/formal-model.md`: the conformance obligation.
//!
//! A stream is *conforming* when it produces `(p, S_p)` for
//! `p in supp(S)` in **strictly ascending** order with **no empty fibers**.
//! Proposition 6' says the conforming sequence for a set is unique, which is
//! what makes "operators are homomorphisms lifted along Theorem 2" a definition
//! rather than a coincidence — and what lets every later argument treat two
//! streams with the same denotation as interchangeable.
//!
//! # Why this file exists
//!
//! Nothing else checks it. `expr_equivalence` compares *denotations*, and a
//! stream that emits an empty fiber, or repeats a prefix, or walks backwards,
//! still collects to the right set — `OrdSet::from_chunks` and the operators
//! above it absorb the malformation silently. So the whole test suite passes
//! while the object every proposition quantifies over has been left behind.
//!
//! The obligation is unenforceable by the type system ( a `Container` cannot
//! refuse to be empty, and a `Prefix48` is a `u64` ), which is why the article
//! states it as an invariant and why it needs a behavioural layer.
//!
//! **The negative control is the point.** A conformance checker that never
//! fires is indistinguishable from one that cannot. `the_checker_rejects_*`
//! below feed it three hand-built malformed streams — one repeating a prefix,
//! one descending, one emitting an empty fiber — and require rejection. Without
//! those, this file would be another test that passes for the wrong reason.
//!
//! # What is and is not covered
//!
//! Both production paths ( `next_chunk` and `next_cardinality` ), both lowerings
//! ( `open`, which plans first, and `open_planned`, which does not ), all eight
//! `Expr` variants, and the three combinators `Expr` cannot express — `Restrict`,
//! `Concat`, `UnionAll` — which §5.5 of the article counts inside the closure and
//! which an `Expr`-only corpus would silently omit.
//!
//! This file is **not** a correctness oracle. It checks the *shape* of a
//! production sequence, not that the sequence denotes the right set. The oracles
//! for that are `expr_equivalence` ( lazy against eager ) and `proptest_oracle`
//! ( against `BTreeSet` ). See `planning_preserves_the_denotation` for why the
//! comparison in this file cannot substitute for either.

use std::collections::BTreeSet;
use std::sync::Arc;

use yesno_core::stream::nary::UnionAll;
use yesno_core::stream::{ChunkStream, ChunkStreamExt, Concat, Restrict};
use yesno_core::{Container, Db, DbOptions, Expr, OrdSet, Prefix48, Result};

/// What a conforming stream may not do.
#[derive(Debug, PartialEq, Eq)]
enum Violation {
    /// Two productions at the same prefix, or a prefix below its predecessor.
    NotAscending { prev: Prefix48, next: Prefix48 },
    /// A fiber that is not in the support at all.
    EmptyFiber { prefix: Prefix48 },
}

/// Drains `s` and returns the first violation of Proposition 6', if any.
///
/// Checks `next_chunk`. `next_cardinality` carries the same contract — the
/// trait says "strictly ascending prefixes, and never a zero count" — and is
/// checked separately by [`violations_of_counts`], because the two are
/// different code paths in every operator and an override can conform on one
/// and not the other.
fn violation_of(s: &mut dyn ChunkStream) -> Result<Option<Violation>> {
    let mut prev: Option<Prefix48> = None;
    while let Some((p, c)) = s.next_chunk()? {
        if let Some(q) = prev {
            if p <= q {
                return Ok(Some(Violation::NotAscending { prev: q, next: p }));
            }
        }
        if c.is_empty() {
            return Ok(Some(Violation::EmptyFiber { prefix: p }));
        }
        prev = Some(p);
    }
    Ok(None)
}

/// The same obligation on the counting path.
fn violations_of_counts(s: &mut dyn ChunkStream) -> Result<Option<Violation>> {
    let mut prev: Option<Prefix48> = None;
    while let Some((p, n)) = s.next_cardinality()? {
        if let Some(q) = prev {
            if p <= q {
                return Ok(Some(Violation::NotAscending { prev: q, next: p }));
            }
        }
        if n == 0 {
            return Ok(Some(Violation::EmptyFiber { prefix: p }));
        }
        prev = Some(p);
    }
    Ok(None)
}

// ---------------------------------------------------------------- negatives

/// A stream that replays a fixed script, however malformed.
struct Scripted {
    items: Vec<(Prefix48, Container)>,
    at: usize,
}

impl ChunkStream for Scripted {
    fn next_chunk(&mut self) -> Result<Option<(Prefix48, Container)>> {
        let out = self.items.get(self.at).cloned();
        if out.is_some() {
            self.at += 1;
        }
        Ok(out)
    }
    fn seek(&mut self, prefix: Prefix48) -> Result<()> {
        while self.at < self.items.len() && self.items[self.at].0 < prefix {
            self.at += 1;
        }
        Ok(())
    }
    fn peek_prefix(&mut self) -> Result<Option<Prefix48>> {
        Ok(self.items.get(self.at).map(|(p, _)| *p))
    }
}

fn one(v: u16) -> Container {
    Container::from_sorted(&[v])
}

fn scripted(items: Vec<(Prefix48, Container)>) -> Scripted {
    Scripted { items, at: 0 }
}

#[test]
fn the_checker_rejects_a_repeated_prefix() {
    let mut s = scripted(vec![(0, one(1)), (0, one(2))]);
    assert_eq!(
        violation_of(&mut s).unwrap(),
        Some(Violation::NotAscending { prev: 0, next: 0 }),
        "a repeated prefix must be rejected"
    );
}

#[test]
fn the_checker_rejects_a_descending_prefix() {
    let mut s = scripted(vec![(7, one(1)), (3, one(2))]);
    assert_eq!(
        violation_of(&mut s).unwrap(),
        Some(Violation::NotAscending { prev: 7, next: 3 }),
        "a backwards step must be rejected"
    );
}

#[test]
fn the_checker_rejects_an_empty_fiber() {
    let mut s = scripted(vec![(0, one(1)), (5, Container::new_array())]);
    assert_eq!(
        violation_of(&mut s).unwrap(),
        Some(Violation::EmptyFiber { prefix: 5 }),
        "an empty fiber must be rejected"
    );
    // ...and the counting path must reject it too, since a zero count is the
    // same malformation seen through `next_cardinality`.
    let mut s = scripted(vec![(0, one(1)), (5, Container::new_array())]);
    assert_eq!(
        violations_of_counts(&mut s).unwrap(),
        Some(Violation::EmptyFiber { prefix: 5 })
    );
}

#[test]
fn the_checker_accepts_a_well_formed_script() {
    let mut s = scripted(vec![(0, one(1)), (5, one(2)), (9, one(3))]);
    assert_eq!(violation_of(&mut s).unwrap(), None);
}

// ---------------------------------------------------------------- positives

/// Operands chosen so that every operator has something to cancel.
///
/// `b` and `c` share prefixes with `a` but not ordinals, which is what makes
/// `Xor` and `AndNot` produce candidate prefixes that cancel to empty — the
/// case §5 names as the one where conformance is not automatic.
fn operands() -> (Arc<OrdSet>, Arc<OrdSet>, Arc<OrdSet>) {
    let a: Vec<u64> = (0..400u64).map(|i| (i << 16) | 1).collect();
    let b: Vec<u64> = (0..400u64).map(|i| (i << 16) | 1).collect(); // identical to a
    let c: Vec<u64> = (0..400u64).map(|i| (i << 16) | 2).collect(); // same prefixes, other low
    (
        Arc::new(OrdSet::from_sorted_slice(&a)),
        Arc::new(OrdSet::from_sorted_slice(&b)),
        Arc::new(OrdSet::from_sorted_slice(&c)),
    )
}

fn expressions() -> Vec<(&'static str, Expr)> {
    let (a, b, c) = operands();
    let (ea, eb, ec) = (
        Expr::set(a.clone()),
        Expr::set(b.clone()),
        Expr::set(c.clone()),
    );
    let r = || Expr::Range(0, 400 << 16);
    vec![
        // Xor of identical operands: every fiber cancels to empty.
        ("xor_identical", ea.clone().xor(eb.clone())),
        // AndNot of identical operands: likewise.
        ("andnot_identical", ea.clone().and_not(eb.clone())),
        // Same prefixes, disjoint ordinals: And cancels every fiber.
        ("and_same_prefix_disjoint_low", ea.clone().and(ec.clone())),
        // Xor over shared prefixes: survives, but only after cancellation.
        ("xor_shared_prefix", ea.clone().xor(ec.clone())),
        ("or", ea.clone().or(ec.clone())),
        ("not_in_range", ea.clone().not_in(0, 400 << 16)),
        ("andnot_range", r().and_not(ea.clone())),
        ("nested", ea.clone().xor(eb.clone()).or(ec.clone().and(r()))),
        (
            "deep",
            ea.clone()
                .and_not(eb.clone())
                .xor(ec.clone().and_not(ea.clone()))
                .or(ea.clone().and(ec.clone())),
        ),
    ]
}

#[test]
fn every_operator_produces_a_conforming_stream() {
    for (name, e) in expressions() {
        let mut s = e.open();
        assert_eq!(
            violation_of(&mut *s).unwrap(),
            None,
            "{name}: planned stream violated Proposition 6'"
        );
        let mut s = e.open_planned();
        assert_eq!(
            violation_of(&mut *s).unwrap(),
            None,
            "{name}: unplanned stream violated Proposition 6'"
        );
    }
}

#[test]
fn the_counting_path_conforms_too() {
    for (name, e) in expressions() {
        // Both lowerings, for the same reason `every_operator_produces_a_
        // conforming_stream` checks both: `open()` plans first, and planning may
        // fold the operator under test away entirely, leaving the counting
        // override of that operator unexercised.
        let mut s = e.open();
        assert_eq!(
            violations_of_counts(&mut *s).unwrap(),
            None,
            "{name}: next_cardinality violated Proposition 6' ( planned )"
        );
        let mut s = e.open_planned();
        assert_eq!(
            violations_of_counts(&mut *s).unwrap(),
            None,
            "{name}: next_cardinality violated Proposition 6' ( unplanned )"
        );
    }
}

/// Planning preserves the denotation.
///
/// This is **not** an independent correctness oracle, and the name it used to
/// carry ( "still denote the right set" ) claimed that it was. Both sides come
/// from the same `Expr`, so agreement means planning changed nothing — it says
/// nothing about whether the shared answer is correct. The oracle for that is
/// `tests/expr_equivalence.rs`, which compares lazy evaluation against eager
/// `OrdSet` evaluation, and `tests/proptest_oracle.rs` against `BTreeSet`.
///
/// It is kept because it is still the guard that stops this file passing on a
/// stream that emits nothing at all, and because a planning bug that silently
/// changed a denotation would show up here first.
#[test]
fn planning_preserves_the_denotation() {
    for (name, e) in expressions() {
        let got: BTreeSet<u64> = e.open().collect_set().unwrap().iter().collect();
        let want: BTreeSet<u64> = e.open_planned().collect_set().unwrap().iter().collect();
        assert_eq!(got, want, "{name}: planning changed the denotation");
    }
}

/// The combinators `Expr` cannot reach.
///
/// §5.5 lists the closure as $\cap, \cup, \triangle, \setminus$, complement,
/// **restriction to a prefix window, ordered concatenation, and an $n$-ary
/// union**. `Expr` has eight variants and none of the last three: `Restrict`,
/// `Concat` and `UnionAll` are `ChunkStream` combinators built directly, and
/// `expressions()` above therefore says nothing about them. Without this test
/// `every_operator_produces_a_conforming_stream` names more than it checks.
///
/// `Concat` is the interesting one: its operands must already be ordered and
/// disjoint, so it is the one combinator whose conformance is a *precondition*
/// on the caller rather than a property it establishes. A `Concat` over
/// overlapping operands would emit a descending prefix, which is exactly
/// `NotAscending`.
#[test]
fn the_combinators_outside_expr_conform_too() {
    let lo_set = Arc::new(OrdSet::from_sorted_slice(
        &(0..200u64).map(|i| (i << 16) | 1).collect::<Vec<_>>(),
    ));
    let hi_set = Arc::new(OrdSet::from_sorted_slice(
        &(200..400u64).map(|i| (i << 16) | 1).collect::<Vec<_>>(),
    ));
    let open = |s: &Arc<OrdSet>| Expr::set(s.clone()).open_planned();

    let (a, _, c) = operands();

    let mut r = Restrict::new(open(&lo_set), 50, 150);
    assert_eq!(violation_of(&mut r).unwrap(), None, "Restrict");

    let mut k = Concat::new(open(&lo_set), open(&hi_set));
    assert_eq!(violation_of(&mut k).unwrap(), None, "Concat");

    let mut u = UnionAll::new(vec![open(&a), open(&c), open(&hi_set)]);
    assert_eq!(violation_of(&mut u).unwrap(), None, "UnionAll");

    // The counting path is a separate override in each of the three, so it gets
    // the same obligation. `UnionAll` is the one that matters most: it merges k
    // operands at a shared prefix, and a zero count there is the same
    // malformation `EmptyFiber` names on the chunk path.
    let mut r = Restrict::new(open(&lo_set), 50, 150);
    assert_eq!(
        violations_of_counts(&mut r).unwrap(),
        None,
        "Restrict counts"
    );

    let mut k = Concat::new(open(&lo_set), open(&hi_set));
    assert_eq!(violations_of_counts(&mut k).unwrap(), None, "Concat counts");

    let mut u = UnionAll::new(vec![open(&a), open(&c), open(&hi_set)]);
    assert_eq!(
        violations_of_counts(&mut u).unwrap(),
        None,
        "UnionAll counts"
    );

    // Not vacuous: each must actually produce something, on *both* paths. A
    // stream that yields nothing conforms trivially, and the counting path can
    // fall silent independently of the chunk path, being a separate override.
    let fresh = |which: usize| -> Box<dyn ChunkStream> {
        match which {
            0 => Box::new(Restrict::new(open(&lo_set), 50, 150)),
            1 => Box::new(Concat::new(open(&lo_set), open(&hi_set))),
            _ => Box::new(UnionAll::new(vec![open(&a), open(&c)])),
        }
    };
    for (which, name) in ["Restrict", "Concat", "UnionAll"].iter().enumerate() {
        let mut s = fresh(which);
        let mut chunks = 0usize;
        while s.next_chunk().unwrap().is_some() {
            chunks += 1;
        }
        let mut s = fresh(which);
        let mut counts = 0usize;
        while s.next_cardinality().unwrap().is_some() {
            counts += 1;
        }
        assert!(
            chunks > 0 && counts > 0,
            "{name}: yielded {chunks} chunks and {counts} counts; \
             a zero on either path makes its conformance check vacuous"
        );
    }
}

/// Guards against the positive cases being vacuous.
///
/// A stream that yields nothing conforms trivially, so a conformance test over
/// expressions that all collapse to the empty set would pass without exercising
/// the filtering path at all. This pins that the suite contains both kinds:
/// cases that cancel *every* fiber and cases that survive cancellation.
#[test]
fn the_positive_cases_are_not_vacuous() {
    let mut cancel_all = 0usize;
    let mut survives = 0usize;
    for (name, e) in expressions() {
        let mut s = e.open();
        let mut n = 0usize;
        while s.next_chunk().unwrap().is_some() {
            n += 1;
        }
        if n == 0 {
            cancel_all += 1;
        } else {
            survives += 1;
        }
        // Every case must at least reach the operator: an expression the planner
        // folded to a leaf would test nothing about operator conformance.
        assert!(
            n > 0 || matches!(e, Expr::Xor(..) | Expr::AndNot(..) | Expr::And(..)),
            "{name}: yielded nothing and is not a cancelling shape"
        );
    }
    assert!(cancel_all > 0, "no case exercises total cancellation");
    assert!(survives > 0, "no case exercises surviving fibers");
}

/// The teeth of this file, made explicit.
///
/// `every_operator_produces_a_conforming_stream` only watches the operators if
/// the operators are actually *reached*. An expression the planner folds to
/// `Empty` yields nothing and conforms vacuously, and the suite would pass
/// while `Xor` had stopped filtering.
///
/// So: take the **unplanned** lowering of `a XOR a`, which is a real `Xor` node
/// over two `SetStream`s by construction. Its operands share 400 prefixes, and
/// every one of them cancels. Yielding zero chunks is therefore only possible if
/// the operator visited 400 candidate fibers and filtered every one. If it ever
/// emitted them instead, `violation_of` reports `EmptyFiber` — which is exactly
/// the regression this file exists to catch.
#[test]
fn the_cancelling_cases_reach_the_operator() {
    let (a, b, _) = operands();
    assert_eq!(a.chunk_count(), 400, "fixture should span 400 chunks");
    assert_eq!(b.chunk_count(), 400);

    let e = Expr::set(a).xor(Expr::set(b));
    let mut s = e.open_planned(); // unplanned: a real Xor over two SetStreams
    let mut yielded = 0usize;
    while s.next_chunk().unwrap().is_some() {
        yielded += 1;
    }
    assert_eq!(
        yielded, 0,
        "400 shared prefixes must all cancel; a non-zero count means the \
         operand fixture stopped overlapping and this file lost its teeth"
    );
    // And the same stream must be conforming, which is the property under test.
    let mut s = e.open_planned();
    assert_eq!(violation_of(&mut *s).unwrap(), None);
}

/// The database-backed bounded leaf has the same conformance obligation as
/// every in-memory leaf and operator above.
///
/// Both disk and memtable entries are present, with an overlay tombstone in the
/// requested interval. That is the merge where an empty or repeated fiber can
/// otherwise escape while the denotation remains correct after collection.
#[test]
fn a_prefix_bounded_key_stream_conforms() {
    let dir = std::env::temp_dir().join(format!(
        "yesno-stream-conformance-prefix-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    struct Clean(std::path::PathBuf);
    impl Drop for Clean {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _clean = Clean(dir.clone());
    let options = DbOptions {
        shards: 1,
        ..Default::default()
    };

    {
        let db = Db::open_with(&dir, options.clone()).unwrap();
        let values: Vec<u64> = (0..24u64)
            .flat_map(|prefix| [1u64, 7, 11].map(move |low| (prefix << 16) | low))
            .collect();
        db.insert_many(9, &values).unwrap();
        db.checkpoint().unwrap();
    }

    let db = Db::open_with(&dir, options).unwrap();
    let mut batch = db.batch();
    batch.remove_range(9, 8 << 16, (9 << 16) - 1);
    batch.insert(9, (10 << 16) | 13);
    batch.insert(9, (12 << 16) | 17);
    batch.commit().unwrap();
    let snap = db.snapshot().unwrap();

    let mut chunks = snap.key_stream_prefix_range(9, 4, 16).unwrap();
    assert_eq!(
        violation_of(&mut chunks).unwrap(),
        None,
        "bounded next_chunk path"
    );

    let mut counts = snap.key_stream_prefix_range(9, 4, 16).unwrap();
    assert_eq!(
        violations_of_counts(&mut counts).unwrap(),
        None,
        "bounded next_cardinality path"
    );

    // Neither conformance check may pass because the constructor returned an
    // empty stream or ignored its bounds.
    let mut chunks = snap.key_stream_prefix_range(9, 4, 16).unwrap();
    let mut seen = Vec::new();
    while let Some((prefix, _)) = chunks.next_chunk().unwrap() {
        seen.push(prefix);
    }
    assert!(
        !seen.is_empty() && seen.iter().all(|prefix| (4..16).contains(prefix)),
        "bounded fixture produced the wrong prefix support: {seen:?}"
    );
}
