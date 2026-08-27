//! Executing a wire [`SetExpr`] against a snapshot.
//!
//! The **format** lives in `yesno-wire`, a dependency-free crate both this
//! server and `yesno-pg` compile, so there is exactly one definition of the
//! encoding. What lives here is the half that needs `yesno-core`: turning a
//! decoded expression into a `yesno_core::Expr` the engine can evaluate.
//!
//! That split is the point. A client must be able to *build* an expression
//! without linking a storage engine, and the server must be able to *run* one
//! without the client's dependencies. Bytes are the only thing that crosses.
//!
//! View transforms are eager boundaries in this first network integration:
//! selection, folding, and expansion use the existing audited `OrdSet` paths,
//! then re-enter the lazy expression tree as a set leaf. Boolean operators around
//! them remain lazy. Ordinal literals similarly become one `OrdSet` leaf. This
//! avoids adding unmeasured `Expr` variants and changing the planner's audited
//! termination proof merely to expose either feature.

use std::sync::Arc;

use yesno_core::view::{Reduce, View, ViewSink};
use yesno_core::{Expr, OrdSet, Snapshot};
pub use yesno_wire::{
    AnyExpr, BoolExpr, ExprError, FoldOp, IntExpr, SetExpr, Sort, VecIntExpr, VecSetExpr,
    ViewLayout, ViewSpec, MAGIC, MAX_DEPTH, MAX_NODES, MAX_VIEW_SETS, VERSION,
};

/// Exact cardinality for a wire expression.
///
/// A top-level `view( .. )[ i ]` uses the view's dedicated count and does not
/// build the selected set. Other view transforms are eager boundaries because
/// the core expression planner has no view operator yet.
pub fn cardinality(e: &SetExpr, snap: &Snapshot) -> yesno_core::Result<u64> {
    match e {
        // The counting form of the same fusion `lower` performs: counting a
        // constituent never needs the constituent.
        SetExpr::At(v, i) => match v.as_ref() {
            VecSetExpr::View(input, view) => Ok(lower(input, snap)?
                .collect_set()?
                .view_cardinality(&core_view(*view), *i)),
            _ => lower(e, snap)?.cardinality(),
        },
        _ => lower(e, snap)?.cardinality(),
    }
}

/// Lower a wire expression to the executable form.
///
/// **Key leaves are lazy.** A bare key becomes `Snapshot::key_expr`, which
/// resolves the key's chunks to index references and decodes a payload only when
/// an operator actually asks for that chunk -- so an `And` that skips most of a
/// key never decodes the part it skipped. This used to be `snap.load( key )`,
/// which built every container of every operand before evaluation started:
/// correct, and about three allocations per chunk paid whether or not the query
/// needed them.
///
/// It is also what finally puts `Backing::Paged` in front of the planner. Every
/// leaf used to report `Memory`, so the backing-aware branch of the cost model
/// never fired outside its own unit test.
///
/// View transforms remain explicit eager boundaries: the `view` lens operates on
/// a materialized `OrdSet` and has no streaming form, so those arms still load.
pub fn lower(e: &SetExpr, snap: &Snapshot) -> yesno_core::Result<Expr> {
    lower_in(e, snap, None)
}

/// The element `_` stands for while a `map` body is evaluated.
///
/// `None` outside a body. The decoder already refuses a hole there, so a `None`
/// here means a locally built expression rather than one off the wire, and it is
/// reported rather than silently treated as empty.
type Hole<'a> = Option<&'a Arc<OrdSet>>;

fn lower_in(e: &SetExpr, snap: &Snapshot, hole: Hole<'_>) -> yesno_core::Result<Expr> {
    Ok(match e {
        SetExpr::Empty => Expr::Empty,
        SetExpr::Key(k) => snap.key_expr(*k),
        SetExpr::Range(lo, hi) => Expr::Range(*lo, *hi),
        SetExpr::Literal(ordinals) => {
            for &ordinal in ordinals {
                yesno_core::check_ordinal(ordinal)?;
            }
            Expr::set(Arc::new(OrdSet::from_iter_unsorted(
                ordinals.iter().copied(),
            )))
        }
        SetExpr::And(xs) => fold(xs, snap, hole, Expr::and)?,
        SetExpr::Or(xs) => fold(xs, snap, hole, Expr::or)?,
        SetExpr::AndNot(a, b) => lower_in(a, snap, hole)?.and_not(lower_in(b, snap, hole)?),
        // The three view-consuming nodes **fuse** with a `view` operand rather
        // than materializing it. See `lower_vec` for why that is not an
        // optimization but a correctness-of-cost obligation.
        SetExpr::At(v, i) => match v.as_ref() {
            VecSetExpr::View(input, view) => Expr::set(
                lower_in(input, snap, hole)?
                    .collect_set()?
                    .view_select(&core_view(*view), *i),
            ),
            _ => {
                // Nothing to fuse: take the element that was asked for.
                let mut parts = lower_vec(v, snap, hole)?;
                Expr::set(parts.swap_remove(*i as usize))
            }
        },
        SetExpr::Fold(v, op) => match v.as_ref() {
            VecSetExpr::View(input, view) => Expr::set(
                lower_in(input, snap, hole)?
                    .collect_set()?
                    .view_fold(&core_view(*view), core_reduce(*op)),
            ),
            // A literal vector has no packed form to walk, so the fold is the
            // ordinary pairwise one over its elements -- which is exactly what
            // `fold_via_select` would do anyway.
            VecSetExpr::List(xs) => {
                let join: fn(Expr, Expr) -> Expr = match op {
                    FoldOp::Or => Expr::or,
                    FoldOp::And => Expr::and,
                    FoldOp::Xor => Expr::xor,
                };
                fold(xs, snap, hole, join)?
            }
            VecSetExpr::Map(..) => {
                let parts = lower_vec(v, snap, hole)?;
                let join: fn(Expr, Expr) -> Expr = match op {
                    FoldOp::Or => Expr::or,
                    FoldOp::And => Expr::and,
                    FoldOp::Xor => Expr::xor,
                };
                parts
                    .into_iter()
                    .map(|p| Expr::set(Arc::new(p)))
                    .reduce(join)
                    .unwrap_or(Expr::Empty)
            }
        },
        SetExpr::Pack(v, view) => {
            let parts = lower_vec(v, snap, hole)?;
            let core = core_view(*view);
            let mut sink = ViewSink::new(core);
            for (i, part) in parts.iter().enumerate() {
                sink.place(i as u32, part)?;
            }
            Expr::set(sink.build())
        }
        SetExpr::Expand(input, view) => Expr::set(
            lower_in(input, snap, hole)?
                .collect_set()?
                .view_expand(&core_view(*view)),
        ),
        SetExpr::Hole => {
            let s = hole.ok_or(yesno_core::CodecError::Invariant("`_` outside a map body"))?;
            Expr::Set(Arc::clone(s))
        }
        SetExpr::Select(a, n) => {
            // Partial by nature, so a singleton or nothing -- never a sentinel.
            let s = lower_in(a, snap, hole)?.collect_set()?;
            match s.select(*n) {
                Some(o) => Expr::set(Arc::new(OrdSet::from_iter_unsorted([o]))),
                None => Expr::Empty,
            }
        }
        // `Vec[Bool]` over the constituents **is** a set of constituent
        // indices, which is why this yields a set rather than a vector sort.
        SetExpr::MapBool(v, body) => {
            let parts = lower_vec(v, snap, hole)?;
            let mut out = Vec::new();
            for (i, part) in parts.into_iter().enumerate() {
                if eval_bool(body, snap, Some(&Arc::new(part)))? {
                    out.push(i as u64);
                }
            }
            Expr::set(Arc::new(OrdSet::from_iter_unsorted(out)))
        }
    })
}

/// Evaluate a vector of integers -- one per constituent.
///
/// This is the query shape the sorted language exists for:
/// `map( view( k, shape ), cardinality( and( _, q ) ) )` is a facet histogram.
pub fn vec_int(v: &VecIntExpr, snap: &Snapshot) -> yesno_core::Result<Vec<u64>> {
    eval_vec_int(v, snap, None)
}

fn eval_vec_int(v: &VecIntExpr, snap: &Snapshot, hole: Hole<'_>) -> yesno_core::Result<Vec<u64>> {
    Ok(match v {
        VecIntExpr::List(xs) => xs
            .iter()
            .map(|x| eval_int(x, snap, hole))
            .collect::<yesno_core::Result<Vec<_>>>()?,
        VecIntExpr::Map(vs, body) => {
            let parts = lower_vec(vs, snap, hole)?;
            let mut out = Vec::with_capacity(parts.len());
            for part in parts {
                out.push(eval_int(body, snap, Some(&Arc::new(part)))?);
            }
            out
        }
    })
}

fn eval_int(e: &IntExpr, snap: &Snapshot, hole: Hole<'_>) -> yesno_core::Result<u64> {
    Ok(match e {
        IntExpr::Lit(v) => *v,
        // Counting never materializes the operand: this is the whole reason
        // `Expr::cardinality` exists, and a facet query is `sets` of these.
        IntExpr::Cardinality(a) => lower_in(a, snap, hole)?.cardinality()?,
        IntExpr::Rank(a, x) => lower_in(a, snap, hole)?.collect_set()?.rank(*x),
        IntExpr::At(v, i) => {
            let xs = eval_vec_int(v, snap, hole)?;
            // Decoding refused an out-of-range index, so this cannot miss for
            // anything that arrived over the wire.
            *xs.get(*i as usize)
                .ok_or(yesno_core::CodecError::Invariant(
                    "index is at or above the vector's arity",
                ))?
        }
    })
}

fn eval_bool(e: &BoolExpr, snap: &Snapshot, hole: Hole<'_>) -> yesno_core::Result<bool> {
    Ok(match e {
        BoolExpr::Contains(a, x) => lower_in(a, snap, hole)?.collect_set()?.contains(*x),
    })
}

/// Materialize a vector's elements.
///
/// **Only called where an operator genuinely needs every constituent** — today
/// that is `pack`, and indexing into a literal list. The `view` cases of `at`
/// and `fold` deliberately do **not** come through here: `view( e, s )` under
/// `Interleaved` has a single-walk arm in `OrdSet::view_fold`, and building the
/// `sets` constituents first to combine them afterwards would throw it away,
/// making the composed spelling slower than the single node it replaced.
fn lower_vec(v: &VecSetExpr, snap: &Snapshot, hole: Hole<'_>) -> yesno_core::Result<Vec<OrdSet>> {
    Ok(match v {
        VecSetExpr::List(xs) => {
            let mut out = Vec::with_capacity(xs.len());
            for x in xs {
                out.push(lower_in(x, snap, hole)?.collect_set()?);
            }
            out
        }
        VecSetExpr::View(input, view) => {
            let core = core_view(*view);
            let packed = lower_in(input, snap, hole)?.collect_set()?;
            (0..core.sets())
                .map(|i| packed.view_select(&core, i))
                .collect()
        }
        // A map preserves shape, so the result has the operand's arity. The
        // body is evaluated once per element with `_` bound to it.
        VecSetExpr::Map(vs, body) => {
            let parts = lower_vec(vs, snap, hole)?;
            let mut out = Vec::with_capacity(parts.len());
            for part in parts {
                out.push(lower_in(body, snap, Some(&Arc::new(part)))?.collect_set()?);
            }
            out
        }
    })
}

fn core_view(spec: ViewSpec) -> View {
    match spec.layout {
        ViewLayout::Interleaved => View::interleaved(spec.sets),
        ViewLayout::Blocked { stride } => View::blocked(spec.sets, stride),
    }
}

/// The wire's fold operator, as the core's reduction.
///
/// The names differ on purpose. The wire spells the operator the way a caller
/// writes it -- `or` / `and` / `xor`, the Boolean operations being folded --
/// while `Reduce` names the quantifier each one computes. Proposition 28's
/// exactness table is the correspondence: `or` is `∪`/`∃`, `and` is `∩`/`∀`,
/// `xor` is `△`/`⊕`.
fn core_reduce(op: FoldOp) -> Reduce {
    match op {
        FoldOp::Or => Reduce::Any,
        FoldOp::And => Reduce::All,
        FoldOp::Xor => Reduce::Parity,
    }
}
fn fold(
    xs: &[SetExpr],
    snap: &Snapshot,
    hole: Hole<'_>,
    join: fn(Expr, Expr) -> Expr,
) -> yesno_core::Result<Expr> {
    // `decode` rejects an empty junction, so `xs` is non-empty for anything that
    // arrived over the wire. A locally built one could still be empty;
    // `Expr::Empty` is the conservative answer for both AND and OR because it
    // can only ever return fewer rows, never invent one.
    let mut it = xs.iter();
    let Some(first) = it.next() else {
        return Ok(Expr::Empty);
    };
    let mut acc = lower_in(first, snap, hole)?;
    for x in it {
        acc = join(acc, lower_in(x, snap, hole)?);
    }
    Ok(acc)
}

/// A bare `OrdSet` for a key, for callers that already know the key.
pub fn set_for_key(snap: &Snapshot, key: u64) -> yesno_core::Result<Arc<OrdSet>> {
    Ok(Arc::new(snap.load(key)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use yesno_core::Db;

    /// **The facet query**, which is the reason the language became sorted.
    ///
    /// For each cohort, how many of its members satisfy `q`. The oracle is
    /// plain `BTreeSet` arithmetic over the constituents, because the point is
    /// that this is the *row* marginal -- per constituent -- and no fold or
    /// per-ordinal count can produce it.
    #[test]
    fn a_facet_query_counts_each_constituent_under_a_filter() {
        let db = Db::new();
        // Three cohorts interleaved under one key, plus a filter set.
        let parts: Vec<Vec<u64>> = vec![vec![0, 1, 2, 3, 4], vec![2, 3, 9], vec![0, 4, 9, 11]];
        let packed: Vec<u64> = parts
            .iter()
            .enumerate()
            .flat_map(|(i, xs)| xs.iter().map(move |x| x * 3 + i as u64))
            .collect();
        db.insert_many(9, &packed).unwrap();
        let q: Vec<u64> = vec![0, 2, 4, 9];
        db.insert_many(7, &q).unwrap();
        let snap = db.snapshot().unwrap();

        let facet = VecIntExpr::Map(
            Box::new(VecSetExpr::View(
                Box::new(SetExpr::Key(9)),
                ViewSpec::interleaved(3),
            )),
            Box::new(IntExpr::Cardinality(Box::new(SetExpr::And(vec![
                SetExpr::Hole,
                SetExpr::Key(7),
            ])))),
        );

        let filter: BTreeSet<u64> = q.iter().copied().collect();
        let want: Vec<u64> = parts
            .iter()
            .map(|xs| {
                xs.iter()
                    .collect::<BTreeSet<_>>()
                    .iter()
                    .filter(|x| filter.contains(**x))
                    .count() as u64
            })
            .collect();
        assert_eq!(want, vec![3, 2, 3], "the oracle itself must be non-trivial");
        assert_eq!(vec_int(&facet, &snap).unwrap(), want);

        // Unfiltered, the same shape is each cohort's own cardinality -- which
        // is the singular `view_cardinality` vectorized.
        let sizes = VecIntExpr::Map(
            Box::new(VecSetExpr::View(
                Box::new(SetExpr::Key(9)),
                ViewSpec::interleaved(3),
            )),
            Box::new(IntExpr::Cardinality(Box::new(SetExpr::Hole))),
        );
        let want_sizes: Vec<u64> = parts.iter().map(|xs| xs.len() as u64).collect();
        assert_eq!(vec_int(&sizes, &snap).unwrap(), want_sizes);

        // And the marginal identity: the row sums total the packed cardinality.
        assert_eq!(
            vec_int(&sizes, &snap).unwrap().iter().sum::<u64>(),
            packed.len() as u64
        );
    }

    /// `map` with a set-valued body, a `Bool` body, and the scalar queries.
    #[test]
    fn map_bodies_of_every_sort_agree_with_an_oracle() {
        let db = Db::new();
        let parts: Vec<Vec<u64>> = vec![vec![0, 1, 2], vec![1, 5], vec![7]];
        let packed: Vec<u64> = parts
            .iter()
            .enumerate()
            .flat_map(|(i, xs)| xs.iter().map(move |x| x * 3 + i as u64))
            .collect();
        db.insert_many(9, &packed).unwrap();
        db.insert_many(7, &[1, 2, 7]).unwrap();
        let snap = db.snapshot().unwrap();
        let view = || {
            Box::new(VecSetExpr::View(
                Box::new(SetExpr::Key(9)),
                ViewSpec::interleaved(3),
            ))
        };

        // Set body: restrict every constituent, then fold. Equals the union of
        // the restricted cohorts.
        let restricted = SetExpr::Fold(
            Box::new(VecSetExpr::Map(
                view(),
                Box::new(SetExpr::And(vec![SetExpr::Hole, SetExpr::Key(7)])),
            )),
            FoldOp::Or,
        );
        let want: BTreeSet<u64> = parts
            .iter()
            .flatten()
            .copied()
            .filter(|x| [1u64, 2, 7].contains(x))
            .collect();
        let got: BTreeSet<u64> = lower(&restricted, &snap)
            .unwrap()
            .collect_set()
            .unwrap()
            .iter()
            .collect();
        assert_eq!(got, want);
        assert!(!want.is_empty(), "the fixture must not be vacuous");

        // Bool body: which constituents hold ordinal 1. That is a set of
        // *constituent indices* -- the column of the matrix.
        let holds = SetExpr::MapBool(
            view(),
            Box::new(BoolExpr::Contains(Box::new(SetExpr::Hole), 1)),
        );
        let got: Vec<u64> = lower(&holds, &snap)
            .unwrap()
            .collect_set()
            .unwrap()
            .iter()
            .collect();
        assert_eq!(got, vec![0, 1], "cohorts 0 and 1 hold logical ordinal 1");

        // `select` is partial, so a singleton or nothing -- never a sentinel.
        let first = SetExpr::Select(Box::new(SetExpr::Key(7)), 0);
        let got: Vec<u64> = lower(&first, &snap)
            .unwrap()
            .collect_set()
            .unwrap()
            .iter()
            .collect();
        assert_eq!(got, vec![1]);
        let past = SetExpr::Select(Box::new(SetExpr::Key(7)), 99);
        assert!(lower(&past, &snap)
            .unwrap()
            .collect_set()
            .unwrap()
            .is_empty());
    }

    /// Lowering must agree with evaluating the same expression by hand.
    ///
    /// The oracle is `BTreeSet`, not another yesno path. An expression
    /// lowered into a *different* yesno expression would agree with itself, so
    /// only an outside answer can catch it.
    #[test]
    fn lowering_agrees_with_a_set_oracle() {
        let db = Db::new();
        db.insert_many(1, &[1, 2, 3, 10, 11, 12]).unwrap();
        db.insert_many(2, &[2, 3, 4, 11, 12, 13]).unwrap();
        let snap = db.snapshot().unwrap();

        let a: BTreeSet<u64> = [1, 2, 3, 10, 11, 12].into_iter().collect();
        let b: BTreeSet<u64> = [2, 3, 4, 11, 12, 13].into_iter().collect();

        let cases: Vec<(SetExpr, BTreeSet<u64>)> = vec![
            (SetExpr::Key(1), a.clone()),
            (SetExpr::Empty, BTreeSet::new()),
            (
                SetExpr::Literal(vec![0, 2, 65_536, u64::MAX - 1]),
                BTreeSet::from([0, 2, 65_536, u64::MAX - 1]),
            ),
            (
                SetExpr::And(vec![SetExpr::Key(1), SetExpr::Key(2)]),
                a.intersection(&b).copied().collect(),
            ),
            (
                SetExpr::And(vec![SetExpr::Key(1), SetExpr::Literal(vec![2, 3, 65_536])]),
                BTreeSet::from([2, 3]),
            ),
            (
                SetExpr::Or(vec![SetExpr::Key(1), SetExpr::Key(2)]),
                a.union(&b).copied().collect(),
            ),
            (
                SetExpr::AndNot(Box::new(SetExpr::Key(1)), Box::new(SetExpr::Key(2))),
                a.difference(&b).copied().collect(),
            ),
            (
                // The shape qual pushdown actually produces: a key restricted to
                // a half-open range.
                SetExpr::And(vec![SetExpr::Key(1), SetExpr::Range(2, 11)]),
                a.iter().copied().filter(|v| (2..11).contains(v)).collect(),
            ),
            (
                // Two disjoint ranges — what a `BETWEEN` straddling zero lowers
                // to once the sign mapping is applied.
                SetExpr::And(vec![
                    SetExpr::Key(1),
                    SetExpr::Or(vec![SetExpr::Range(1, 3), SetExpr::Range(11, 13)]),
                ]),
                a.iter()
                    .copied()
                    .filter(|v| (1..3).contains(v) || (11..13).contains(v))
                    .collect(),
            ),
        ];

        for (e, want) in cases {
            // Exercise the real request boundary before lowering. A locally
            // constructed enum would leave a wire tag that drops or reorders
            // literal members invisible to this oracle.
            let e = SetExpr::decode(&e.encode()).unwrap();
            let got: BTreeSet<u64> = lower(&e, &snap)
                .unwrap()
                .collect_set()
                .unwrap()
                .iter()
                .collect();
            assert_eq!(got, want, "{e:?}");

            // The cardinality path is a *parallel implementation* — it walks
            // without materializing — so agreeing on the set does not imply
            // agreeing on the count.
            let n = cardinality(&e, &snap).unwrap();
            assert_eq!(n, want.len() as u64, "cardinality of {e:?}");
        }
    }

    #[test]
    fn view_lowering_agrees_with_an_independent_set_oracle() {
        let db = Db::new();
        db.insert_many(1, &[1, 2, 3, 10, 11, 12]).unwrap();
        let parts = [
            BTreeSet::from([1, 2, 5]),
            BTreeSet::from([2, 3, 9]),
            BTreeSet::from([2, 4, 9]),
        ];

        let interleaved: Vec<u64> = parts
            .iter()
            .enumerate()
            .flat_map(|(set, xs)| xs.iter().map(move |x| x * 3 + set as u64))
            .collect();
        db.insert_many(9, &interleaved).unwrap();

        let blocked: Vec<u64> = parts
            .iter()
            .enumerate()
            .flat_map(|(set, xs)| xs.iter().map(move |x| set as u64 * 100 + x))
            .collect();
        db.insert_many(10, &blocked).unwrap();
        let snap = db.snapshot().unwrap();

        let any = parts
            .iter()
            .fold(BTreeSet::new(), |a, x| a.union(x).copied().collect());
        let all = parts[0]
            .intersection(&parts[1])
            .copied()
            .collect::<BTreeSet<_>>()
            .intersection(&parts[2])
            .copied()
            .collect();
        let parity = parts.iter().fold(BTreeSet::new(), |a, x| {
            a.symmetric_difference(x).copied().collect()
        });
        let expanded: BTreeSet<u64> = [1u64, 2, 3, 10, 11, 12]
            .into_iter()
            .flat_map(|x| [x * 2, x * 2 + 1])
            .collect();

        let view_of =
            |k: u64, spec: ViewSpec| Box::new(VecSetExpr::View(Box::new(SetExpr::Key(k)), spec));
        let cases = [
            (
                SetExpr::At(view_of(9, ViewSpec::interleaved(3)), 1),
                parts[1].clone(),
            ),
            (
                SetExpr::At(view_of(10, ViewSpec::blocked(3, 100)), 2),
                parts[2].clone(),
            ),
            (
                SetExpr::Fold(view_of(9, ViewSpec::interleaved(3)), FoldOp::Or),
                any,
            ),
            (
                SetExpr::Fold(view_of(10, ViewSpec::blocked(3, 100)), FoldOp::And),
                all,
            ),
            (
                SetExpr::Fold(view_of(9, ViewSpec::interleaved(3)), FoldOp::Xor),
                parity,
            ),
            (
                SetExpr::Expand(Box::new(SetExpr::Key(1)), ViewSpec::interleaved(2)),
                expanded,
            ),
        ];

        for (e, want) in cases {
            let got: BTreeSet<u64> = lower(&e, &snap)
                .unwrap()
                .collect_set()
                .unwrap()
                .iter()
                .collect();
            assert_eq!(got, want, "{e:?}");
            assert_eq!(
                cardinality(&e, &snap).unwrap(),
                want.len() as u64,
                "cardinality of {e:?}"
            );
        }
    }

    /// An expression naming a key that was never written must be empty rather
    /// than an error: a term nobody indexed is a legitimate query with no rows.
    #[test]
    fn an_absent_key_lowers_to_an_empty_set() {
        let db = Db::new();
        let snap = db.snapshot().unwrap();
        let e = SetExpr::And(vec![SetExpr::Key(404), SetExpr::Range(0, 100)]);
        assert_eq!(cardinality(&e, &snap).unwrap(), 0);
    }
}
