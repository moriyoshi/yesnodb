//! The M6 gate: lowering checked against a brute-force oracle.
//!
//! `pushdown.rs` already tests the *verdict* — which combinations come back
//! `Exact`, `Inexact` or `Unsupported`, exhaustively for the AND/OR asymmetry.
//! What nothing tested is the **rows**: that the set expression a filter lowers
//! to actually contains the rows the original predicate selects.
//!
//! Those are different failures. A verdict test catches "we claimed Exact when
//! we should have said Inexact". This catches "we said Inexact, which licenses
//! a superset, and then returned a *subset*" — which is silently wrong answers,
//! and is exactly what the design warns about:
//!
//! > Under `AND` you may drop an unhandled conjunct — the result is a superset,
//! > which `Inexact` legalizes. Under `OR` you may **not** drop a disjunct;
//! > dropping shrinks the result and silently loses rows.
//!
//! The contract asserted here, per verdict:
//!
//! - `Exact`      — lowered rows **==** true rows.
//! - `Inexact`    — lowered rows **⊇** true rows. A `FilterExec` removes the rest.
//! - `Unsupported`— no claim; the pushdown is discarded, so nothing to check.

use std::collections::{BTreeMap, BTreeSet};

use datafusion::common::ScalarValue;
use datafusion::logical_expr::{col, lit, Expr, TableProviderFilterPushDown};
use yesno_datafusion::pushdown::{lower, HashEncoder, SetExpr, TermEncoder};

const TERMS: [&str; 4] = ["a", "b", "c", "d"];
const ROWS: u64 = 40;

/// Row `i` carries term `TERMS[i % 4]`. Deliberately tiny and total, so the
/// oracle can be evaluated by looking at every row.
fn term_of(row: u64) -> &'static str {
    TERMS[(row % TERMS.len() as u64) as usize]
}

/// The **unindexed** column, and it must genuinely select rows.
///
/// The first version of this oracle modelled `other` as satisfied by nothing.
/// That made the whole OR-asymmetry check vacuous: dropping a disjunct that
/// selects no rows loses no rows, so the test passed even with the asymmetry
/// deliberately broken. `other == 1` now holds for one row in seven, which is
/// what gives a dropped disjunct something to lose.
fn other_of(row: u64) -> i64 {
    (row % 7) as i64
}

/// Posting lists, keyed exactly as the encoder keys them.
fn posting_lists() -> BTreeMap<u64, BTreeSet<u64>> {
    let mut m: BTreeMap<u64, BTreeSet<u64>> = BTreeMap::new();
    for row in 0..ROWS {
        let key = HashEncoder
            .encode(&ScalarValue::Utf8(Some(term_of(row).to_string())))
            .expect("a string term must encode");
        m.entry(key).or_default().insert(row);
    }
    m
}

/// Evaluate a lowered set expression over the posting lists.
fn eval(set: &SetExpr, lists: &BTreeMap<u64, BTreeSet<u64>>) -> BTreeSet<u64> {
    match set {
        SetExpr::All => (0..ROWS).collect(),
        SetExpr::Empty => BTreeSet::new(),
        SetExpr::Key(k) => lists.get(k).cloned().unwrap_or_default(),
        SetExpr::And(xs) => {
            let mut it = xs.iter().map(|x| eval(x, lists));
            match it.next() {
                None => (0..ROWS).collect(), // empty AND is the identity
                Some(first) => it.fold(first, |a, b| a.intersection(&b).copied().collect()),
            }
        }
        SetExpr::Or(xs) => xs.iter().fold(BTreeSet::new(), |a, x| {
            a.union(&eval(x, lists)).copied().collect()
        }),
        SetExpr::AndNot(a, b) => {
            let (a, b) = (eval(a, lists), eval(b, lists));
            a.difference(&b).copied().collect()
        }
        // `SetExpr` is `#[non_exhaustive]`. A new variant must **fail** here
        // rather than fall through to something plausible: treating an unknown
        // operator as `Empty` or `All` would let this gate keep passing while
        // silently no longer covering the lowering.
        other => panic!(
            "the lowering oracle does not know how to evaluate {other:?} - \
             teach it this variant rather than widening the catch-all"
        ),
    }
}

/// The predicates under test, as (expression, row-by-row truth).
///
/// The truth side is written independently of the lowering — it is a plain
/// closure over one row — which is what makes it an oracle rather than a
/// restatement.
type Case = (Expr, Box<dyn Fn(u64) -> bool>);

fn cases() -> Vec<Case> {
    let mut v: Vec<Case> = Vec::new();

    // Every single-term equality and its negation.
    for t in TERMS {
        v.push((col("term").eq(lit(t)), Box::new(move |r| term_of(r) == t)));
        v.push((
            col("term").not_eq(lit(t)),
            Box::new(move |r| term_of(r) != t),
        ));
    }

    // Every pair, under both connectives, in both orders.
    for a in TERMS {
        for b in TERMS {
            v.push((
                col("term").eq(lit(a)).and(col("term").eq(lit(b))),
                Box::new(move |r| term_of(r) == a && term_of(r) == b),
            ));
            v.push((
                col("term").eq(lit(a)).or(col("term").eq(lit(b))),
                Box::new(move |r| term_of(r) == a || term_of(r) == b),
            ));
        }
    }

    // Mixed with an unindexed column, which is where the asymmetry bites: the
    // AND form may drop it and stay a superset, the OR form may not.
    let unindexed = col("other").eq(lit(1i64));
    for t in TERMS {
        v.push((
            col("term").eq(lit(t)).and(unindexed.clone()),
            Box::new(move |r| term_of(r) == t && other_of(r) == 1),
        ));
        v.push((
            // The case the asymmetry exists for: dropping the `other` disjunct
            // would lose every row that satisfies it and not the term.
            col("term").eq(lit(t)).or(unindexed.clone()),
            Box::new(move |r| term_of(r) == t || other_of(r) == 1),
        ));
    }

    // Three-way nesting, so a poisoned branch has somewhere to propagate to.
    v.push((
        col("term")
            .eq(lit("a"))
            .or(col("term").eq(lit("b")))
            .and(col("term").not_eq(lit("c"))),
        Box::new(|r| (term_of(r) == "a" || term_of(r) == "b") && term_of(r) != "c"),
    ));
    v.push((
        col("term")
            .eq(lit("a"))
            .and(col("term").eq(lit("b")))
            .or(col("term").eq(lit("c"))),
        Box::new(|r| (term_of(r) == "a" && term_of(r) == "b") || term_of(r) == "c"),
    ));

    v
}

#[test]
fn a_lowered_filter_never_loses_a_row_it_claimed_to_keep() {
    let lists = posting_lists();
    let indexed: BTreeSet<String> = ["term".to_string()].into_iter().collect();

    let mut checked = 0;
    let mut supersets = 0;
    for (expr, truth) in cases() {
        let l = lower(&expr, &HashEncoder, &indexed);
        let got = eval(&l.set, &lists);
        let want: BTreeSet<u64> = (0..ROWS).filter(|r| truth(*r)).collect();

        match l.exactness {
            // No claim is made, so nothing to verify.
            TableProviderFilterPushDown::Unsupported => continue,

            TableProviderFilterPushDown::Inexact => {
                let missing: Vec<u64> = want.difference(&got).copied().collect();
                assert!(
                    missing.is_empty(),
                    "Inexact lowering of `{expr}` LOST rows {missing:?} - \
                     Inexact licenses a superset, never a subset"
                );
                if got.len() > want.len() {
                    supersets += 1;
                }
            }

            TableProviderFilterPushDown::Exact => {
                assert_eq!(
                    got, want,
                    "Exact lowering of `{expr}` does not match the true rows"
                );
            }
        }
        checked += 1;
    }

    assert!(
        checked > 20,
        "only {checked} cases were verified; too few to mean much"
    );
    assert!(
        supersets > 0,
        "no case produced a strict superset, so the Inexact branch was never \
         really exercised and this test proves less than it appears to"
    );
}
