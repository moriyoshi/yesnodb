//! Lowering a DataFusion filter into set algebra.
//!
//! # The AND/OR asymmetry is the whole safety argument
//!
//! Under **AND** an unhandled conjunct may be dropped: the result is then a
//! *superset* of the truth, which `Inexact` legalizes because DataFusion keeps a
//! `FilterExec` above the scan to remove the extra rows.
//!
//! Under **OR** an unhandled disjunct may **not** be dropped. Dropping shrinks
//! the result, and no filter above the scan can re-add rows that were never
//! emitted. So a single unsupported branch poisons the entire disjunction to
//! `Unsupported`.
//!
//! Getting this backwards produces silently missing rows — no error, no warning,
//! just fewer answers than the query asked for. It is the single most dangerous
//! mistake available in this module, which is why the asymmetry is encoded in
//! one place and tested against a brute-force oracle.
//!
//! # Exactness and non-injective encoders
//!
//! If a [`TermEncoder`] hashes a term to a `u64`, two terms can collide and the
//! posting list is then a superset of the truth. That is legal **only** as
//! `Inexact`. Reporting `Exact` when the encoder is not injective tells
//! DataFusion it may drop the `FilterExec`, and the collision's rows are
//! returned as genuine matches.
//!
//! So `Exact` requires all of: an injective encoder, a fully-lowered filter tree
//! with nothing dropped, and an ordinal mapping known to be exact. The default
//! encoder hashes, and therefore defaults to `Inexact`.

use std::collections::BTreeSet;
use std::fmt::Debug;

use datafusion::common::ScalarValue;
use datafusion::logical_expr::{BinaryExpr, Expr, Operator, TableProviderFilterPushDown};

/// A filter lowered to set operations over posting lists.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SetExpr {
    /// The posting list for one encoded term.
    Key(u64),
    /// Every ordinal. The identity for AND and the complement base for NOT.
    All,
    /// No ordinals.
    Empty,
    And(Vec<SetExpr>),
    Or(Vec<SetExpr>),
    AndNot(Box<SetExpr>, Box<SetExpr>),
}

/// Result of lowering one filter.
#[derive(Clone, Debug)]
pub struct LoweredFilter {
    pub set: SetExpr,
    pub exactness: TableProviderFilterPushDown,
}

/// Maps a literal term to the key its posting list lives under.
pub trait TermEncoder: Send + Sync + Debug {
    fn encode(&self, v: &ScalarValue) -> Option<u64>;

    /// Whether distinct terms always map to distinct keys.
    ///
    /// Defaults to `false`, which is the safe answer: a non-injective encoder
    /// can only ever produce `Inexact` pushdown. Override only for a
    /// dictionary-backed encoder that genuinely cannot collide.
    fn is_injective(&self) -> bool {
        false
    }
}

/// The default: a 64-bit hash of the term's string form.
///
/// **Not injective.** Two terms can collide, so every filter lowered through it
/// is `Inexact` and DataFusion keeps its `FilterExec`. That costs one cheap
/// filter; claiming `Exact` here would cost correctness.
#[derive(Debug, Default, Clone, Copy)]
pub struct HashEncoder;

impl TermEncoder for HashEncoder {
    fn encode(&self, v: &ScalarValue) -> Option<u64> {
        let s = match v {
            ScalarValue::Utf8(Some(s)) | ScalarValue::LargeUtf8(Some(s)) => s.clone(),
            ScalarValue::Int64(Some(i)) => i.to_string(),
            ScalarValue::UInt64(Some(u)) => u.to_string(),
            _ => return None,
        };
        // splitmix64 over an FNV pass; only avalanche is required.
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in s.as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01B3);
        }
        h = (h ^ (h >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        h = (h ^ (h >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        Some(h ^ (h >> 31))
    }
}

/// Weakest of two exactness verdicts.
fn weaken(
    a: TableProviderFilterPushDown,
    b: TableProviderFilterPushDown,
) -> TableProviderFilterPushDown {
    use TableProviderFilterPushDown::*;
    match (a, b) {
        (Unsupported, _) | (_, Unsupported) => Unsupported,
        (Inexact, _) | (_, Inexact) => Inexact,
        _ => Exact,
    }
}

/// Lower one filter expression.
pub fn lower(e: &Expr, enc: &dyn TermEncoder, indexed: &BTreeSet<String>) -> LoweredFilter {
    use TableProviderFilterPushDown::*;

    let unsupported = || LoweredFilter {
        set: SetExpr::All,
        exactness: Unsupported,
    };
    // An injective encoder is a precondition for ever claiming Exact.
    let base = if enc.is_injective() { Exact } else { Inexact };

    match e {
        Expr::BinaryExpr(BinaryExpr { left, op, right }) => match op {
            Operator::Eq | Operator::NotEq => {
                let Some((col, lit)) = column_and_literal(left, right) else {
                    return unsupported();
                };
                if !indexed.contains(&col) {
                    return unsupported();
                }
                let Some(k) = enc.encode(&lit) else {
                    return unsupported();
                };
                let set = if *op == Operator::Eq {
                    SetExpr::Key(k)
                } else {
                    SetExpr::AndNot(Box::new(SetExpr::All), Box::new(SetExpr::Key(k)))
                };
                LoweredFilter {
                    set,
                    exactness: base,
                }
            }
            Operator::And => {
                let l = lower(left, enc, indexed);
                let r = lower(right, enc, indexed);
                // Dropping a conjunct yields a superset, which Inexact covers.
                match (l.exactness.clone(), r.exactness.clone()) {
                    (Unsupported, Unsupported) => unsupported(),
                    (Unsupported, _) => LoweredFilter {
                        set: r.set,
                        exactness: Inexact,
                    },
                    (_, Unsupported) => LoweredFilter {
                        set: l.set,
                        exactness: Inexact,
                    },
                    (le, re) => LoweredFilter {
                        set: SetExpr::And(vec![l.set, r.set]),
                        exactness: weaken(le, re),
                    },
                }
            }
            Operator::Or => {
                let l = lower(left, enc, indexed);
                let r = lower(right, enc, indexed);
                // A dropped disjunct SHRINKS the result, and nothing above the
                // scan can put those rows back. One unsupported branch poisons
                // the whole disjunction.
                if l.exactness == Unsupported || r.exactness == Unsupported {
                    return unsupported();
                }
                let ex = weaken(l.exactness.clone(), r.exactness.clone());
                LoweredFilter {
                    set: SetExpr::Or(vec![l.set, r.set]),
                    exactness: ex,
                }
            }
            _ => unsupported(),
        },
        Expr::InList(list) => {
            let Expr::Column(c) = list.expr.as_ref() else {
                return unsupported();
            };
            if !indexed.contains(&c.name) {
                return unsupported();
            }
            let mut keys = Vec::with_capacity(list.list.len());
            for item in &list.list {
                let Expr::Literal(v, _) = item else {
                    return unsupported();
                };
                // An IN list is a disjunction, so a single unencodable member
                // makes the whole list unsupported — same rule as OR.
                let Some(k) = enc.encode(v) else {
                    return unsupported();
                };
                keys.push(SetExpr::Key(k));
            }
            if keys.is_empty() {
                return LoweredFilter {
                    set: SetExpr::Empty,
                    exactness: base,
                };
            }
            let inner = SetExpr::Or(keys);
            let set = if list.negated {
                SetExpr::AndNot(Box::new(SetExpr::All), Box::new(inner))
            } else {
                inner
            };
            LoweredFilter {
                set,
                exactness: base,
            }
        }
        Expr::Not(inner) => {
            let l = lower(inner, enc, indexed);
            if l.exactness == Unsupported {
                return unsupported();
            }
            LoweredFilter {
                set: SetExpr::AndNot(Box::new(SetExpr::All), Box::new(l.set)),
                exactness: l.exactness,
            }
        }
        // IS NULL / IS NOT NULL have no sentinel key in this design.
        _ => unsupported(),
    }
}

/// Pull a `(column, literal)` pair out of a binary comparison, either order.
fn column_and_literal(left: &Expr, right: &Expr) -> Option<(String, ScalarValue)> {
    match (left, right) {
        (Expr::Column(c), Expr::Literal(v, _)) => Some((c.name.clone(), v.clone())),
        (Expr::Literal(v, _), Expr::Column(c)) => Some((c.name.clone(), v.clone())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use datafusion::logical_expr::{col, lit};
    use TableProviderFilterPushDown::*;

    fn indexed() -> BTreeSet<String> {
        ["term".to_string()].into_iter().collect()
    }

    /// An encoder that cannot collide, so `Exact` is legal.
    #[derive(Debug)]
    struct Injective;
    impl TermEncoder for Injective {
        fn encode(&self, v: &ScalarValue) -> Option<u64> {
            match v {
                ScalarValue::UInt64(Some(u)) => Some(*u),
                ScalarValue::Utf8(Some(s)) => s.parse().ok(),
                _ => None,
            }
        }
        fn is_injective(&self) -> bool {
            true
        }
    }

    #[test]
    fn equality_on_an_indexed_column_lowers_to_a_key() {
        let e = col("term").eq(lit("rust"));
        let l = lower(&e, &HashEncoder, &indexed());
        assert!(matches!(l.set, SetExpr::Key(_)));
        assert_eq!(
            l.exactness, Inexact,
            "a hashing encoder can only be Inexact"
        );
    }

    #[test]
    fn an_injective_encoder_permits_exact() {
        let e = col("term").eq(lit(7u64));
        let l = lower(&e, &Injective, &indexed());
        assert_eq!(l.set, SetExpr::Key(7));
        assert_eq!(l.exactness, Exact);
    }

    #[test]
    fn a_non_indexed_column_is_unsupported() {
        let e = col("other").eq(lit("rust"));
        assert_eq!(lower(&e, &HashEncoder, &indexed()).exactness, Unsupported);
    }

    #[test]
    fn a_literal_on_either_side_works() {
        let a = lower(&col("term").eq(lit("x")), &Injective, &indexed());
        let b = lower(
            &Expr::BinaryExpr(BinaryExpr::new(
                Box::new(lit("x")),
                Operator::Eq,
                Box::new(col("term")),
            )),
            &Injective,
            &indexed(),
        );
        // "x" does not parse as u64, so both are unsupported — but symmetrically.
        assert_eq!(a.exactness, b.exactness);
    }

    /// Under AND, dropping an unhandled conjunct is safe and yields Inexact.
    #[test]
    fn and_may_drop_an_unsupported_conjunct() {
        let e = col("term").eq(lit("rust")).and(col("other").eq(lit(1i64)));
        let l = lower(&e, &HashEncoder, &indexed());
        assert_eq!(
            l.exactness, Inexact,
            "dropping a conjunct yields a superset"
        );
        assert!(
            matches!(l.set, SetExpr::Key(_)),
            "the supported half must survive alone, got {:?}",
            l.set
        );
    }

    /// Under OR, dropping a disjunct loses rows. It must poison the whole thing.
    #[test]
    fn or_must_not_drop_an_unsupported_disjunct() {
        let e = col("term").eq(lit("rust")).or(col("other").eq(lit(1i64)));
        let l = lower(&e, &HashEncoder, &indexed());
        assert_eq!(
            l.exactness, Unsupported,
            "a dropped disjunct would silently lose rows"
        );
    }

    #[test]
    fn and_of_two_unsupported_conjuncts_is_unsupported() {
        let e = col("a").eq(lit(1i64)).and(col("b").eq(lit(2i64)));
        assert_eq!(lower(&e, &HashEncoder, &indexed()).exactness, Unsupported);
    }

    #[test]
    fn nested_or_inside_and_still_poisons_only_its_own_branch() {
        // term = x AND (term = y OR other = z)
        // The OR is unsupported, so it is dropped from the AND -> Inexact.
        let e = col("term")
            .eq(lit("x"))
            .and(col("term").eq(lit("y")).or(col("other").eq(lit(1i64))));
        let l = lower(&e, &HashEncoder, &indexed());
        assert_eq!(l.exactness, Inexact);
        assert!(
            matches!(l.set, SetExpr::Key(_)),
            "only the usable conjunct survives"
        );
    }

    #[test]
    fn not_equal_becomes_a_complement() {
        let e = col("term").not_eq(lit(7u64));
        let l = lower(&e, &Injective, &indexed());
        match l.set {
            SetExpr::AndNot(all, k) => {
                assert_eq!(*all, SetExpr::All);
                assert_eq!(*k, SetExpr::Key(7));
            }
            other => panic!("expected a complement, got {other:?}"),
        }
    }

    #[test]
    fn in_list_becomes_a_disjunction() {
        let e = col("term").in_list(vec![lit(1u64), lit(2u64), lit(3u64)], false);
        let l = lower(&e, &Injective, &indexed());
        match l.set {
            SetExpr::Or(ks) => assert_eq!(ks.len(), 3),
            other => panic!("expected an OR, got {other:?}"),
        }
        assert_eq!(l.exactness, Exact);
    }

    #[test]
    fn a_negated_in_list_becomes_a_complemented_disjunction() {
        let e = col("term").in_list(vec![lit(1u64), lit(2u64)], true);
        let l = lower(&e, &Injective, &indexed());
        assert!(matches!(l.set, SetExpr::AndNot(_, _)));
    }

    /// An IN list is a disjunction, so it inherits OR's rule.
    #[test]
    fn an_in_list_with_an_unencodable_member_is_unsupported() {
        let e = col("term").in_list(vec![lit(1u64), lit("not-a-number")], false);
        assert_eq!(
            lower(&e, &Injective, &indexed()).exactness,
            Unsupported,
            "one unencodable member must not be silently dropped from a disjunction"
        );
    }

    #[test]
    fn not_of_an_unsupported_expression_is_unsupported() {
        let e = Expr::Not(Box::new(col("other").eq(lit(1i64))));
        assert_eq!(lower(&e, &HashEncoder, &indexed()).exactness, Unsupported);
    }

    /// Brute-force oracle over the asymmetry.
    ///
    /// For every AND/OR combination of a supported and an unsupported leaf, the
    /// verdict must never claim more than it can deliver: an OR touching an
    /// unsupported branch is Unsupported, and an AND is at worst Inexact.
    #[test]
    fn the_asymmetry_holds_across_every_combination() {
        let supported = col("term").eq(lit("a"));
        let unsupported_leaf = col("other").eq(lit(1i64));

        let cases: Vec<(Expr, TableProviderFilterPushDown)> = vec![
            (supported.clone().and(supported.clone()), Inexact),
            (supported.clone().and(unsupported_leaf.clone()), Inexact),
            (unsupported_leaf.clone().and(supported.clone()), Inexact),
            (
                unsupported_leaf.clone().and(unsupported_leaf.clone()),
                Unsupported,
            ),
            (supported.clone().or(supported.clone()), Inexact),
            (supported.clone().or(unsupported_leaf.clone()), Unsupported),
            (unsupported_leaf.clone().or(supported.clone()), Unsupported),
            (
                unsupported_leaf.clone().or(unsupported_leaf.clone()),
                Unsupported,
            ),
        ];

        for (e, want) in cases {
            let got = lower(&e, &HashEncoder, &indexed()).exactness;
            assert_eq!(got, want, "wrong verdict for {e}");
        }
    }

    #[test]
    fn exactness_never_strengthens_through_composition() {
        // Combining anything with an Inexact branch must stay Inexact or worse.
        let inexact = col("term").eq(lit("a")); // HashEncoder -> Inexact
        let e = inexact.clone().and(inexact.clone());
        assert_ne!(
            lower(&e, &HashEncoder, &indexed()).exactness,
            Exact,
            "composition must not manufacture exactness"
        );
    }
}
