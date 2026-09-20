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
//! A view's packed input is still an eager boundary: the lens operates on an
//! audited `OrdSet`, not a core expression node. Its consumers are fused where
//! their terminal makes that exact. Indexing evaluates only the requested map
//! element, cardinality and membership maps walk constituents without retaining
//! them, pointwise Boolean cardinality and rank maps use one packed walk after
//! decomposing the body at the absent and present values of its hole, and folds
//! of pointwise maps reduce the same two-value truth table over the packed view.
//! A fold of direct selections tracks every constituent's nth ordinal in one
//! physical walk. An identity cardinality map is normalized through a composed
//! set map before terminal selection. Other non-pointwise shapes keep the eager
//! fallback. This closes the measured repeated-extraction costs without adding
//! an unmeasured `Expr` variant or changing the planner's audited termination
//! proof.

use std::sync::Arc;

use yesno_core::view::{Reduce, View, ViewSink};
use yesno_core::{ChunkStream, Container, Expr, OrdSet, Prefix48, Snapshot};
pub use yesno_wire::{
    AnyExpr, BoolExpr, ExprError, FoldOp, IntExpr, SetExpr, Sort, VecIntExpr, VecSetExpr,
    ViewLayout, ViewSpec, MAGIC, MAX_DEPTH, MAX_NODES, MAX_VIEW_SETS, VERSION,
};

/// Exact cardinality for a wire expression.
///
/// A top-level `view( .. )[ i ]` uses the view's dedicated count and does not
/// build the selected set. Vector consumers use the terminal fusions described
/// by [`lower`]; unsupported shapes retain the eager fallback.
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
/// A view's packed input remains an eager boundary because the core planner has
/// no view expression node. Exact terminal fusions below avoid materializing
/// every constituent; unsupported vector shapes still use the explicit eager
/// fallback.
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
        // Indexing is demand-driven through every map layer. Materializing the
        // other elements would do work whose result cannot be observed.
        SetExpr::At(v, i) => lower_vec_at(v, *i, snap, hole)?,
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
                if let Some(fused) = fold_mapped_view(v, *op, snap, hole)? {
                    return Ok(fused);
                }
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
            if let VecSetExpr::View(input, view) = v.as_ref() {
                return Ok(Expr::set(eval_view_bool_map(
                    input, *view, body, snap, hole,
                )?));
            }
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
            if let Some(normalized) = normalize_cardinality_map(vs, body) {
                return eval_vec_int(&normalized, snap, hole);
            }
            if let VecSetExpr::View(input, view) = vs.as_ref() {
                if let Some(out) = eval_view_int_map(input, *view, body, snap, hole)? {
                    return Ok(out);
                }
            }
            let parts = lower_vec(vs, snap, hole)?;
            let mut out = Vec::with_capacity(parts.len());
            for part in parts {
                out.push(eval_int(body, snap, Some(&Arc::new(part)))?);
            }
            out
        }
    })
}

/// Move an identity cardinality terminal through one set-map binding.
///
/// Only a bare `cardinality( _ )` matches. An arbitrary cardinality operand
/// would introduce another use of the outer hole, so substituting it into the
/// inner body here would capture the wrong map binding.
fn normalize_cardinality_map(input: &VecSetExpr, body: &IntExpr) -> Option<VecIntExpr> {
    let IntExpr::Cardinality(operand) = body else {
        return None;
    };
    if !matches!(operand.as_ref(), SetExpr::Hole) {
        return None;
    }
    let VecSetExpr::Map(inner, mapped_body) = input else {
        return None;
    };
    Some(VecIntExpr::Map(
        inner.clone(),
        Box::new(IntExpr::Cardinality(mapped_body.clone())),
    ))
}

fn eval_int(e: &IntExpr, snap: &Snapshot, hole: Hole<'_>) -> yesno_core::Result<u64> {
    Ok(match e {
        IntExpr::Lit(v) => *v,
        // Counting never materializes the operand: this is the whole reason
        // `Expr::cardinality` exists, and a facet query is `sets` of these.
        IntExpr::Cardinality(a) => lower_in(a, snap, hole)?.cardinality()?,
        IntExpr::Rank(a, x) => lower_in(a, snap, hole)?.collect_set()?.rank(*x),
        IntExpr::At(v, i) => eval_vec_int_at(v, *i, snap, hole)?,
    })
}

fn eval_bool(e: &BoolExpr, snap: &Snapshot, hole: Hole<'_>) -> yesno_core::Result<bool> {
    Ok(match e {
        BoolExpr::Contains(a, x) => expr_contains(lower_in(a, snap, hole)?, *x)?,
    })
}
fn index_error() -> yesno_core::CodecError {
    yesno_core::CodecError::Invariant("index is at or above the vector's arity")
}

/// Lower only one element, preserving map semantics without evaluating siblings.
fn lower_vec_at(
    v: &VecSetExpr,
    i: u32,
    snap: &Snapshot,
    hole: Hole<'_>,
) -> yesno_core::Result<Expr> {
    if i >= v.arity() {
        return Err(index_error());
    }
    match v {
        VecSetExpr::List(xs) => lower_in(&xs[i as usize], snap, hole),
        VecSetExpr::View(input, view) => Ok(Expr::set(
            lower_in(input, snap, hole)?
                .collect_set()?
                .view_select(&core_view(*view), i),
        )),
        VecSetExpr::Map(vs, body) => {
            let part = Arc::new(lower_vec_at(vs, i, snap, hole)?.collect_set()?);
            lower_in(body, snap, Some(&part))
        }
    }
}

/// Evaluate only one integer element, just as lower_vec_at does for sets.
fn eval_vec_int_at(
    v: &VecIntExpr,
    i: u32,
    snap: &Snapshot,
    hole: Hole<'_>,
) -> yesno_core::Result<u64> {
    if i >= v.arity() {
        return Err(index_error());
    }
    match v {
        VecIntExpr::List(xs) => eval_int(&xs[i as usize], snap, hole),
        VecIntExpr::Map(vs, body) => {
            let part = Arc::new(lower_vec_at(vs, i, snap, hole)?.collect_set()?);
            eval_int(body, snap, Some(&part))
        }
    }
}

/// A map body lowered once except where it depends on the current element.
///
/// Static expressions are cloneable, reopenable plans. They are deliberately
/// not collected here: a large invariant filter must remain lazy so the core
/// planner can order it against each constituent.
enum PreparedSet<'a> {
    Static(Expr),
    Hole,
    And(Vec<PreparedSet<'a>>),
    Or(Vec<PreparedSet<'a>>),
    AndNot(Box<PreparedSet<'a>>, Box<PreparedSet<'a>>),
    Dynamic(&'a SetExpr),
}

impl<'a> PreparedSet<'a> {
    fn new(e: &'a SetExpr, snap: &Snapshot, hole: Hole<'_>) -> yesno_core::Result<Self> {
        if !set_contains_hole(e) {
            return Ok(Self::Static(lower_in(e, snap, hole)?));
        }
        Ok(match e {
            SetExpr::Hole => Self::Hole,
            SetExpr::And(xs) => Self::And(
                xs.iter()
                    .map(|x| Self::new(x, snap, hole))
                    .collect::<yesno_core::Result<_>>()?,
            ),
            SetExpr::Or(xs) => Self::Or(
                xs.iter()
                    .map(|x| Self::new(x, snap, hole))
                    .collect::<yesno_core::Result<_>>()?,
            ),
            SetExpr::AndNot(a, b) => Self::AndNot(
                Box::new(Self::new(a, snap, hole)?),
                Box::new(Self::new(b, snap, hole)?),
            ),
            // Select, pack, and other non-Boolean transforms can contain the
            // hole, but have no exact substitution rule here.
            other => Self::Dynamic(other),
        })
    }

    fn eval(&self, snap: &Snapshot, part: &Arc<OrdSet>) -> yesno_core::Result<Expr> {
        match self {
            Self::Static(e) => Ok(e.clone()),
            Self::Hole => Ok(Expr::set(Arc::clone(part))),
            Self::And(xs) => eval_prepared(xs, snap, part, Expr::and),
            Self::Or(xs) => eval_prepared(xs, snap, part, Expr::or),
            Self::AndNot(a, b) => Ok(a.eval(snap, part)?.and_not(b.eval(snap, part)?)),
            Self::Dynamic(e) => lower_in(e, snap, Some(part)),
        }
    }

    /// Substitute one lazy expression for the hole when the body is pointwise.
    ///
    /// Non-pointwise transforms decline so their existing materializing
    /// semantics remain the fallback.
    fn pointwise(&self, hole: Expr) -> Option<Expr> {
        match self {
            Self::Static(e) => Some(e.clone()),
            Self::Hole => Some(hole),
            Self::And(xs) => pointwise_fold(xs, hole, Expr::and),
            Self::Or(xs) => pointwise_fold(xs, hole, Expr::or),
            Self::AndNot(a, b) => Some(a.pointwise(hole.clone())?.and_not(b.pointwise(hole)?)),
            Self::Dynamic(_) => None,
        }
    }
}

fn eval_prepared(
    xs: &[PreparedSet<'_>],
    snap: &Snapshot,
    part: &Arc<OrdSet>,
    join: fn(Expr, Expr) -> Expr,
) -> yesno_core::Result<Expr> {
    let mut it = xs.iter();
    let Some(first) = it.next() else {
        return Ok(Expr::Empty);
    };
    let mut out = first.eval(snap, part)?;
    for x in it {
        out = join(out, x.eval(snap, part)?);
    }
    Ok(out)
}

fn pointwise_fold(
    xs: &[PreparedSet<'_>],
    hole: Expr,
    join: fn(Expr, Expr) -> Expr,
) -> Option<Expr> {
    let mut it = xs.iter();
    let Some(first) = it.next() else {
        return Some(Expr::Empty);
    };
    let mut out = first.pointwise(hole.clone())?;
    for x in it {
        out = join(out, x.pointwise(hole.clone())?);
    }
    Some(out)
}

/// Count an intersection-shaped body without extracting any constituent.
///
/// Interleaved physical order is logical-ordinal-major, so one monotone cursor
/// over the invariant expression answers each logical ordinal once and the
/// packed walk assigns matching rows to their constituent counters.
fn eval_intersection_cardinalities(
    packed: &OrdSet,
    view: &View,
    body: &SetExpr,
    snap: &Snapshot,
    hole: Hole<'_>,
) -> yesno_core::Result<Option<Vec<u64>>> {
    if !matches!(view.layout(), yesno_core::view::ViewLayout::Interleaved) {
        return Ok(None);
    }

    let mut invariants = Vec::new();
    let mut holes = 0;
    collect_intersection_body(body, &mut invariants, &mut holes);
    if holes != 1 {
        return Ok(None);
    }
    if invariants.is_empty() {
        return Ok(Some(packed.view_cardinalities(view)));
    }

    let mut invariants = invariants.into_iter();
    let mut filter = lower_in(invariants.next().unwrap(), snap, hole)?;
    for invariant in invariants {
        filter = filter.and(lower_in(invariant, snap, hole)?);
    }
    let mut stream = filter.open();
    let mut current: Option<(Prefix48, Container)> = None;
    let mut exhausted = false;
    let mut last_x = None;
    let mut selected = false;
    let mut counts = vec![0; view.sets() as usize];

    for physical in packed.iter() {
        let Some((owner, x)) = view.logical_of(physical) else {
            continue;
        };
        if last_x != Some(x) {
            selected = monotone_stream_contains(stream.as_mut(), &mut current, &mut exhausted, x)?;
            last_x = Some(x);
        }
        if selected {
            counts[owner as usize] += 1;
        }
    }
    Ok(Some(counts))
}

fn monotone_stream_contains(
    stream: &mut dyn ChunkStream,
    current: &mut Option<(Prefix48, Container)>,
    exhausted: &mut bool,
    ordinal: u64,
) -> yesno_core::Result<bool> {
    let (target, low) = yesno_core::split(ordinal);
    if !*exhausted && current.as_ref().is_none_or(|(prefix, _)| *prefix < target) {
        stream.seek(target)?;
        *current = stream.next_chunk()?;
        *exhausted = current.is_none();
    }
    Ok(current
        .as_ref()
        .is_some_and(|(prefix, container)| *prefix == target && container.contains(low)))
}

fn expr_contains(expr: Expr, ordinal: u64) -> yesno_core::Result<bool> {
    let mut stream = expr.open();
    let mut current = None;
    let mut exhausted = false;
    monotone_stream_contains(stream.as_mut(), &mut current, &mut exhausted, ordinal)
}

/// Count a pointwise Boolean map body without extracting its constituents.
///
/// For one logical ordinal the body is a Boolean function of the hole. Let
/// `f0` and `f1` be that function with the hole absent and present. Then
///
/// `|f(H)| = |f0| + |H ∩ (f1 ∖ f0)| - |H ∩ (f0 ∖ f1)|`.
///
/// The invariant base is counted once and one packed walk counts both
/// intersections for every constituent. Rank uses the same identity after
/// restricting all three terms to `[0, upper)`.
fn eval_pointwise_cardinalities(
    packed: &OrdSet,
    view: &View,
    prepared: &PreparedSet<'_>,
    upper: Option<u64>,
) -> yesno_core::Result<Option<Vec<u64>>> {
    if !matches!(view.layout(), yesno_core::view::ViewLayout::Interleaved) {
        return Ok(None);
    }
    if upper == Some(0) {
        return Ok(Some(vec![0; view.sets() as usize]));
    }

    let Some(when_absent) = prepared.pointwise(Expr::Empty) else {
        return Ok(None);
    };
    let Some(when_present) = prepared.pointwise(Expr::Range(0, u64::MAX)) else {
        return Ok(None);
    };
    let restrict = |e: Expr| match upper {
        Some(hi) => e.and(Expr::Range(0, hi)),
        None => e,
    };
    let base = restrict(when_absent.clone()).cardinality()?;
    let positive = restrict(when_present.clone().and_not(when_absent.clone()));
    let negative = restrict(when_absent.and_not(when_present));

    let mut positive_stream = positive.open();
    let mut positive_current: Option<(Prefix48, Container)> = None;
    let mut positive_exhausted = false;
    let mut negative_stream = negative.open();
    let mut negative_current: Option<(Prefix48, Container)> = None;
    let mut negative_exhausted = false;
    let mut positive_counts = vec![0u64; view.sets() as usize];
    let mut negative_counts = vec![0u64; view.sets() as usize];
    let mut last_x = None;
    let mut positive_selected = false;
    let mut negative_selected = false;

    for physical in packed.iter() {
        let Some((owner, x)) = view.logical_of(physical) else {
            continue;
        };
        if upper.is_some_and(|hi| x >= hi) {
            break;
        }
        if last_x != Some(x) {
            positive_selected = monotone_stream_contains(
                positive_stream.as_mut(),
                &mut positive_current,
                &mut positive_exhausted,
                x,
            )?;
            negative_selected = monotone_stream_contains(
                negative_stream.as_mut(),
                &mut negative_current,
                &mut negative_exhausted,
                x,
            )?;
            last_x = Some(x);
        }
        if positive_selected {
            positive_counts[owner as usize] += 1;
        }
        if negative_selected {
            negative_counts[owner as usize] += 1;
        }
    }

    let mut out = vec![base; view.sets() as usize];
    for ((value, add), subtract) in out.iter_mut().zip(positive_counts).zip(negative_counts) {
        *value = value
            .checked_add(add)
            .and_then(|n| n.checked_sub(subtract))
            .ok_or(yesno_core::CodecError::Invariant(
                "pointwise cardinality identity overflowed",
            ))?;
    }
    Ok(Some(out))
}

fn eval_view_int_map(
    input: &SetExpr,
    spec: ViewSpec,
    body: &IntExpr,
    snap: &Snapshot,
    hole: Hole<'_>,
) -> yesno_core::Result<Option<Vec<u64>>> {
    let view = core_view(spec);
    let packed = lower_in(input, snap, hole)?.collect_set()?;

    match body {
        IntExpr::Cardinality(a) if matches!(a.as_ref(), SetExpr::Hole) => {
            Ok(Some(packed.view_cardinalities(&view)))
        }
        IntExpr::Cardinality(a) => {
            if let Some(out) = eval_intersection_cardinalities(&packed, &view, a, snap, hole)? {
                return Ok(Some(out));
            }
            let prepared = PreparedSet::new(a, snap, hole)?;
            if let Some(out) = eval_pointwise_cardinalities(&packed, &view, &prepared, None)? {
                return Ok(Some(out));
            }
            let mut out = Vec::with_capacity(view.sets() as usize);
            for i in 0..view.sets() {
                let part = Arc::new(packed.view_select(&view, i));
                out.push(prepared.eval(snap, &part)?.cardinality()?);
            }
            Ok(Some(out))
        }
        IntExpr::Rank(a, x) => {
            let prepared = PreparedSet::new(a, snap, hole)?;
            if let Some(out) = eval_pointwise_cardinalities(&packed, &view, &prepared, Some(*x))? {
                return Ok(Some(out));
            }
            let mut out = Vec::with_capacity(view.sets() as usize);
            for i in 0..view.sets() {
                let part = Arc::new(packed.view_select(&view, i));
                out.push(prepared.eval(snap, &part)?.collect_set()?.rank(*x));
            }
            Ok(Some(out))
        }
        IntExpr::Lit(_) | IntExpr::At(..) => Ok(None),
    }
}

/// A membership predicate prepared at its one queried ordinal.
///
/// Boolean set operators become Boolean scalar operators. Invariant branches
/// are answered once, while a hole is a direct packed-view membership probe.
enum PreparedContains<'a> {
    Const(bool),
    Hole,
    And(Vec<PreparedContains<'a>>),
    Or(Vec<PreparedContains<'a>>),
    AndNot(Box<PreparedContains<'a>>, Box<PreparedContains<'a>>),
    Dynamic(&'a SetExpr),
}

impl<'a> PreparedContains<'a> {
    fn new(e: &'a SetExpr, x: u64, snap: &Snapshot, hole: Hole<'_>) -> yesno_core::Result<Self> {
        if !set_contains_hole(e) {
            return Ok(Self::Const(expr_contains(lower_in(e, snap, hole)?, x)?));
        }
        Ok(match e {
            SetExpr::Hole => Self::Hole,
            SetExpr::And(xs) => Self::And(
                xs.iter()
                    .map(|a| Self::new(a, x, snap, hole))
                    .collect::<yesno_core::Result<_>>()?,
            ),
            SetExpr::Or(xs) => Self::Or(
                xs.iter()
                    .map(|a| Self::new(a, x, snap, hole))
                    .collect::<yesno_core::Result<_>>()?,
            ),
            SetExpr::AndNot(a, b) => Self::AndNot(
                Box::new(Self::new(a, x, snap, hole)?),
                Box::new(Self::new(b, x, snap, hole)?),
            ),
            other => Self::Dynamic(other),
        })
    }

    fn eval(
        &self,
        packed: &OrdSet,
        view: &View,
        i: u32,
        x: u64,
        snap: &Snapshot,
    ) -> yesno_core::Result<bool> {
        match self {
            Self::Const(v) => Ok(*v),
            Self::Hole => Ok(packed.view_contains(view, i, x)),
            Self::And(xs) => {
                if xs.is_empty() {
                    return Ok(false);
                }
                for a in xs {
                    if !a.eval(packed, view, i, x, snap)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            Self::Or(xs) => {
                for a in xs {
                    if a.eval(packed, view, i, x, snap)? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            Self::AndNot(a, b) => {
                Ok(a.eval(packed, view, i, x, snap)? && !b.eval(packed, view, i, x, snap)?)
            }
            Self::Dynamic(e) => {
                let part = Arc::new(packed.view_select(view, i));
                expr_contains(lower_in(e, snap, Some(&part))?, x)
            }
        }
    }
}

fn eval_view_bool_map(
    input: &SetExpr,
    spec: ViewSpec,
    body: &BoolExpr,
    snap: &Snapshot,
    hole: Hole<'_>,
) -> yesno_core::Result<Arc<OrdSet>> {
    let view = core_view(spec);
    let packed = lower_in(input, snap, hole)?.collect_set()?;
    let BoolExpr::Contains(a, x) = body;
    let prepared = PreparedContains::new(a, *x, snap, hole)?;
    let mut out = Vec::new();
    for i in 0..view.sets() {
        if prepared.eval(&packed, &view, i, *x, snap)? {
            out.push(i as u64);
        }
    }
    Ok(Arc::new(OrdSet::from_iter_unsorted(out)))
}

/// Fuse a fold over a mapped packed view.
///
/// Direct selection uses one physical walk to find every constituent's nth
/// logical ordinal. The intersection-only arm then stays ahead of the general
/// pointwise arm because it needs only one packed fold. Otherwise let `f0` and
/// `f1` be the map body with its hole absent and present. At one ordinal the
/// mapped values are copies of only those two bits, so their OR, AND, or parity
/// is determined by the packed view's any/all/parity folds and the view arity.
/// Other non-pointwise bodies decline to the eager oracle.
fn fold_mapped_view(
    v: &VecSetExpr,
    op: FoldOp,
    snap: &Snapshot,
    hole: Hole<'_>,
) -> yesno_core::Result<Option<Expr>> {
    if let Some((input, spec, n)) = mapped_view_selection(v) {
        let packed = lower_in(input, snap, hole)?.collect_set()?;
        let out = fold_view_selections(&packed, &core_view(spec), n, op);
        return Ok(Some(Expr::set(Arc::new(out))));
    }

    let mut invariants = Vec::new();
    if let Some((input, spec)) = mapped_view_intersections(v, &mut invariants) {
        let packed = lower_in(input, snap, hole)?.collect_set()?;
        let mut out = Expr::set(packed.view_fold(&core_view(spec), core_reduce(op)));
        for invariant in invariants {
            out = out.and(lower_in(invariant, snap, hole)?);
        }
        return Ok(Some(out));
    }

    let VecSetExpr::Map(inner, body) = v else {
        return Ok(None);
    };
    let VecSetExpr::View(input, spec) = inner.as_ref() else {
        return Ok(None);
    };
    let prepared = PreparedSet::new(body, snap, hole)?;
    let Some(when_absent) = prepared.pointwise(Expr::Empty) else {
        return Ok(None);
    };
    let Some(when_present) = prepared.pointwise(Expr::Range(0, u64::MAX)) else {
        return Ok(None);
    };

    let packed = lower_in(input, snap, hole)?.collect_set()?;
    let view = core_view(*spec);
    let out = match op {
        FoldOp::Or => {
            let any = Expr::set(packed.view_fold(&view, Reduce::Any));
            let all = Expr::set(packed.view_fold(&view, Reduce::All));
            when_absent.and_not(all).or(when_present.and(any))
        }
        FoldOp::And => {
            let any = Expr::set(packed.view_fold(&view, Reduce::Any));
            let all = Expr::set(packed.view_fold(&view, Reduce::All));
            when_absent
                .clone()
                .and(when_present.clone())
                .or(when_absent.and_not(any))
                .or(when_present.and(all))
        }
        FoldOp::Xor => {
            let parity = Expr::set(packed.view_fold(&view, Reduce::Parity));
            let toggled = parity.and(when_absent.clone().xor(when_present));
            if view.sets().is_multiple_of(2) {
                toggled
            } else {
                when_absent.xor(toggled)
            }
        }
    };
    Ok(Some(out))
}

fn mapped_view_selection(v: &VecSetExpr) -> Option<(&SetExpr, ViewSpec, u64)> {
    let VecSetExpr::Map(inner, body) = v else {
        return None;
    };
    let VecSetExpr::View(input, spec) = inner.as_ref() else {
        return None;
    };
    let SetExpr::Select(selected, n) = body.as_ref() else {
        return None;
    };
    matches!(selected.as_ref(), SetExpr::Hole).then_some((input.as_ref(), *spec, *n))
}

/// Select one ordinal per constituent and reduce the singleton results.
///
/// The state is bounded by the wire's view-arity limit, while the data walk is
/// bounded by the packed input. Both layouts use `logical_of`, preserving the
/// generic view mapping as the oracle rather than duplicating its arithmetic.
fn fold_view_selections(packed: &OrdSet, view: &View, n: u64, op: FoldOp) -> OrdSet {
    if view.check().is_err() {
        return OrdSet::new();
    }

    let mut state = vec![(0u64, None); view.sets() as usize];
    let mut remaining = view.sets();
    for physical in packed.iter() {
        let Some((owner, logical)) = view.logical_of(physical) else {
            continue;
        };
        let (count, selected) = &mut state[owner as usize];
        if selected.is_some() {
            continue;
        }
        if *count == n {
            *selected = Some(logical);
            remaining -= 1;
            if remaining == 0 {
                break;
            }
        } else {
            *count += 1;
        }
    }

    match op {
        FoldOp::Or => {
            OrdSet::from_iter_unsorted(state.iter().filter_map(|(_, selected)| *selected))
        }
        FoldOp::And => {
            let Some((_, Some(first))) = state.first() else {
                return OrdSet::new();
            };
            if state.iter().all(|(_, selected)| selected == &Some(*first)) {
                OrdSet::from_iter_unsorted([*first])
            } else {
                OrdSet::new()
            }
        }
        FoldOp::Xor => {
            let mut values: Vec<_> = state.iter().filter_map(|(_, selected)| *selected).collect();
            values.sort_unstable();
            let mut odd = Vec::with_capacity(values.len());
            let mut i = 0;
            while i < values.len() {
                let value = values[i];
                let mut end = i + 1;
                while end < values.len() && values[end] == value {
                    end += 1;
                }
                if (end - i) % 2 == 1 {
                    odd.push(value);
                }
                i = end;
            }
            OrdSet::from_iter_unsorted(odd)
        }
    }
}

fn mapped_view_intersections<'a>(
    v: &'a VecSetExpr,
    invariants: &mut Vec<&'a SetExpr>,
) -> Option<(&'a SetExpr, ViewSpec)> {
    match v {
        VecSetExpr::View(input, spec) => Some((input, *spec)),
        VecSetExpr::Map(inner, body) => {
            let keep = invariants.len();
            let mut holes = 0;
            collect_intersection_body(body, invariants, &mut holes);
            if holes != 1 {
                invariants.truncate(keep);
                return None;
            }
            mapped_view_intersections(inner, invariants)
        }
        VecSetExpr::List(_) => None,
    }
}

fn collect_intersection_body<'a>(
    e: &'a SetExpr,
    invariants: &mut Vec<&'a SetExpr>,
    holes: &mut u32,
) {
    match e {
        SetExpr::Hole => *holes += 1,
        SetExpr::And(xs) => {
            for x in xs {
                collect_intersection_body(x, invariants, holes);
            }
        }
        other if !set_contains_hole(other) => invariants.push(other),
        _ => {
            // More than one marks this body as unsupported without another flag.
            *holes += 2;
        }
    }
}

fn set_contains_hole(e: &SetExpr) -> bool {
    match e {
        SetExpr::Empty | SetExpr::Key(_) | SetExpr::Range(..) | SetExpr::Literal(_) => false,
        SetExpr::Hole => true,
        SetExpr::And(xs) | SetExpr::Or(xs) => xs.iter().any(set_contains_hole),
        SetExpr::AndNot(a, b) => set_contains_hole(a) || set_contains_hole(b),
        SetExpr::At(v, _) | SetExpr::Fold(v, _) | SetExpr::Pack(v, _) => vec_set_contains_hole(v),
        SetExpr::Expand(a, _) | SetExpr::Select(a, _) => set_contains_hole(a),
        SetExpr::MapBool(v, body) => {
            vec_set_contains_hole(v)
                || match body.as_ref() {
                    BoolExpr::Contains(a, _) => set_contains_hole(a),
                }
        }
    }
}

fn vec_set_contains_hole(v: &VecSetExpr) -> bool {
    match v {
        VecSetExpr::List(xs) => xs.iter().any(set_contains_hole),
        VecSetExpr::View(a, _) => set_contains_hole(a),
        VecSetExpr::Map(v, body) => vec_set_contains_hole(v) || set_contains_hole(body),
    }
}

/// Materialize a vector's elements.
///
/// **Only called where an operator genuinely needs every materialized
/// constituent or no exact terminal fusion applies.** `at` never comes through
/// here, and cardinality, membership, and pointwise-map folds have direct view
/// paths. The fallback remains important for transformed order-sensitive map
/// bodies.
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

        for (op, want) in [
            (FoldOp::Or, vec![1, 2, 7]),
            (FoldOp::And, vec![]),
            (FoldOp::Xor, vec![2, 7]),
        ] {
            let folded = SetExpr::Fold(
                Box::new(VecSetExpr::Map(
                    view(),
                    Box::new(SetExpr::And(vec![SetExpr::Hole, SetExpr::Key(7)])),
                )),
                op,
            );
            let got: Vec<u64> = lower(&folded, &snap)
                .unwrap()
                .collect_set()
                .unwrap()
                .iter()
                .collect();
            assert_eq!(got, want, "{op:?}");
        }

        // Indexing traverses both sequential map layers but no sibling. The
        // outer union is deliberately not a fold-fusion shape.
        let indexed = SetExpr::At(
            Box::new(VecSetExpr::Map(
                Box::new(VecSetExpr::Map(
                    view(),
                    Box::new(SetExpr::And(vec![SetExpr::Hole, SetExpr::Key(7)])),
                )),
                Box::new(SetExpr::Or(vec![SetExpr::Hole, SetExpr::Literal(vec![99])])),
            )),
            1,
        );
        let got: Vec<u64> = lower(&indexed, &snap)
            .unwrap()
            .collect_set()
            .unwrap()
            .iter()
            .collect();
        assert_eq!(got, vec![1, 99]);

        let ranks = VecIntExpr::Map(
            view(),
            Box::new(IntExpr::Rank(
                Box::new(SetExpr::And(vec![SetExpr::Hole, SetExpr::Key(7)])),
                7,
            )),
        );
        assert_eq!(vec_int(&ranks, &snap).unwrap(), vec![2, 1, 0]);
        assert_eq!(
            eval_int(&IntExpr::At(Box::new(ranks), 1), &snap, None).unwrap(),
            1
        );
        // Bool body: which filtered constituents hold ordinal 1. The invariant
        // key is prepared once and the hole remains a direct view probe. The
        // result is a set of constituent indices, the column of the matrix.
        let holds = SetExpr::MapBool(
            view(),
            Box::new(BoolExpr::Contains(
                Box::new(SetExpr::And(vec![SetExpr::Hole, SetExpr::Key(7)])),
                1,
            )),
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
