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
//! A view has no core expression node, so unsupported shapes retain an eager
//! packed `OrdSet` boundary. Exact terminal fusions avoid unnecessary
//! constituent materialization. Indexing evaluates only the requested map
//! element, while cardinality and membership maps walk constituents without
//! retaining them. Direct intersection counts over a key use bounded or full
//! persisted streams and native container traversal, and sibling terminals can
//! share that traversal through [`vec_int_batch`]. When exact key statistics
//! show one value in each of at least 64 occupied chunks, direct interleaved
//! identity counts and folds also consume the persisted stream and construct
//! only their terminal result; denser inputs retain materialization and its
//! bitmap SIMD kernels. A direct interleaved identity-rank map instead opens
//! only the physical prefix that can contribute below its strict logical
//! bound, then counts rows without constructing the packed set. Pointwise
//! Boolean cardinality and rank maps use one
//! packed walk after decomposing the body at the absent and present values of
//! its hole, and folds of pointwise maps reduce the same two-value truth table
//! over the packed view. A fold of direct selections tracks every
//! constituent's nth ordinal in one physical walk. An identity cardinality map
//! is normalized through a composed set map before terminal selection. This
//! closes the measured repeated-extraction costs without adding an unmeasured
//! `Expr` variant or changing the planner's audited termination proof.

use std::sync::Arc;

use yesno_core::bignum::{Barrett, BigInt, BigUint};
use yesno_core::view::{
    stream_interleaved_view_fold, stream_view_cardinalities, stream_view_ranks,
    IntersectionCountStrategy, Reduce, View, ViewIntersectionCounter, ViewSink,
};
use yesno_core::{
    ChunkStream, ChunkStreamExt, Container, Expr, KeyStream, OrdSet, Prefix48, Snapshot,
};
pub use yesno_wire::{
    AnyExpr, BigBinOp, BigExpr, BigFoldOp, BigLit, BoolExpr, ExprError, FoldOp, IntExpr, SetExpr,
    Sort, VecBigExpr, VecIntExpr, VecSetExpr, ViewLayout, ViewSpec, MAGIC, MAX_DEPTH, MAX_NODES,
    MAX_RESULT_BITS, MAX_VALUE_BITS, MAX_VIEW_SETS, MAX_WORK, VERSION,
};

/// Exact cardinality for a wire expression.
///
/// A top-level `view( .. )[ i ]` uses the view's dedicated count and does not
/// build the selected set. Vector consumers use the terminal fusions described
/// by [`lower`]; unsupported shapes retain the eager fallback.
fn expr_cardinality(expr: Expr) -> yesno_core::Result<u64> {
    // Research capture, on target `yesno::hotspot`. A level check against a
    // global atomic unless a capture is running; see `yesno_core::hotspot`.
    yesno_core::hotspot::record_shape(&expr);
    #[cfg(feature = "jit")]
    {
        yesno_core::jit::cardinality(&expr)
    }
    #[cfg(not(feature = "jit"))]
    {
        expr.cardinality()
    }
}

pub fn cardinality(e: &SetExpr, snap: &Snapshot) -> yesno_core::Result<u64> {
    // Container identity lives on the wire expression, not the lowered one:
    // lowering replaces every key with a freshly allocated source. See
    // `yesno_core::hotspot`, "Identity, which the first version got wrong".
    // This is the whole of the wire-specific half -- naming the posting lists
    // -- and it is guarded so an idle server never builds the list.
    if yesno_core::hotspot::enabled() {
        let mut named = Vec::new();
        e.keys(&mut named);
        yesno_core::hotspot::record_containers(&named, snap);
    }
    match e {
        // The counting form of the same fusion `lower` performs: counting a
        // constituent never needs the constituent.
        SetExpr::At(v, i) => match v.as_ref() {
            VecSetExpr::View(input, view) => Ok(lower(input, snap)?
                .collect_set()?
                .view_cardinality(&core_view(*view), *i)),
            _ => expr_cardinality(lower(e, snap)?),
        },
        _ => expr_cardinality(lower(e, snap)?),
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
/// A view still has no core planner node. Exact terminal fusions below avoid
/// materializing every constituent, and the conservative singleton-chunk arm
/// consumes direct persisted identity counts and folds as streams. Unsupported
/// vector shapes and denser sources still use the explicit eager fallback.
pub fn lower(e: &SetExpr, snap: &Snapshot) -> yesno_core::Result<Expr> {
    lower_in(e, snap, None)
}

/// The element `_` stands for while a `map` body is evaluated.
///
/// `None` outside a body. The decoder already refuses a hole there, so a `None`
/// here means a locally built expression rather than one off the wire, and it is
/// reported rather than silently treated as empty.
type Hole<'a> = Option<&'a Arc<OrdSet>>;

/// Below 64 singleton chunks the measured saving is only a few microseconds.
///
/// This is deliberately the conservative edge of the Stage 7f measurement:
/// exactly one ordinal per occupied chunk gained 24-42%, while eight ordinals
/// per chunk gained only 5-8%. Dense bitmap inputs can never satisfy the
/// singleton condition and therefore retain their materialized SIMD terminal.
const SPARSE_VIEW_MIN_CHUNKS: u64 = 64;

fn extremely_sparse_view_admitted(
    spec: ViewSpec,
    chunks: Option<u64>,
    cardinality: (u64, Option<u64>),
) -> bool {
    let Some(chunks) = chunks else {
        return false;
    };
    spec.check().is_ok()
        && matches!(spec.layout, ViewLayout::Interleaved)
        && chunks >= SPARSE_VIEW_MIN_CHUNKS
        && cardinality == (chunks, Some(chunks))
}

fn key_view_stream(
    key: u64,
    spec: ViewSpec,
    snap: &Snapshot,
) -> yesno_core::Result<(KeyStream, bool)> {
    let stream = snap.key_stream(key)?;
    let admitted =
        extremely_sparse_view_admitted(spec, stream.stats().chunks, stream.cardinality_hint());
    Ok((stream, admitted))
}

const PREFIX_EXCLUSIVE_END: u64 = 1u64 << 48;

fn interleaved_rank_prefix_end(spec: ViewSpec, upper: u64) -> Option<u64> {
    spec.check().ok()?;
    if !matches!(spec.layout, ViewLayout::Interleaved) {
        return None;
    }
    let physical_end = u128::from(upper) * u128::from(spec.sets);
    let prefixes = physical_end
        .div_ceil(1u128 << 16)
        .min(u128::from(PREFIX_EXCLUSIVE_END));
    Some(prefixes as u64)
}

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
            VecSetExpr::View(input, spec) => {
                let view = core_view(*spec);
                if let SetExpr::Key(key) = input.as_ref() {
                    let (mut stream, admitted) = key_view_stream(*key, *spec, snap)?;
                    let out = if admitted {
                        stream_interleaved_view_fold(&mut stream, &view, core_reduce(*op))?
                    } else {
                        stream.collect_set()?.view_fold(&view, core_reduce(*op))
                    };
                    Expr::set(out)
                } else {
                    Expr::set(
                        lower_in(input, snap, hole)?
                            .collect_set()?
                            .view_fold(&view, core_reduce(*op)),
                    )
                }
            }
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

/// Evaluate sibling integer vectors, sharing one packed-key traversal when all
/// are direct intersection-cardinality maps over the same view.
///
/// The wire format deliberately has no multi-result opcode. Callers that own
/// several facet planes can opt into sharing through this in-process entrypoint;
/// unrelated shapes retain the exact scalar evaluator.
pub fn vec_int_batch(vectors: &[VecIntExpr], snap: &Snapshot) -> yesno_core::Result<Vec<Vec<u64>>> {
    let Some((first_key, first_spec, _)) = vectors.first().and_then(direct_key_intersection) else {
        return vectors.iter().map(|v| vec_int(v, snap)).collect();
    };
    let mut filters = Vec::with_capacity(vectors.len());
    for vector in vectors {
        let Some((key, spec, body)) = direct_key_intersection(vector) else {
            return vectors.iter().map(|v| vec_int(v, snap)).collect();
        };
        if key != first_key || spec != first_spec {
            return vectors.iter().map(|v| vec_int(v, snap)).collect();
        }
        let Some(filter) = intersection_filter(body, snap, None)? else {
            return vectors.iter().map(|v| vec_int(v, snap)).collect();
        };
        filters.push(filter);
    }
    let filter_refs: Vec<_> = filters.iter().collect();
    count_key_intersections(first_key, core_view(first_spec), &filter_refs, snap)
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
        IntExpr::Cardinality(a) => expr_cardinality(lower_in(a, snap, hole)?)?,
        IntExpr::Rank(a, x) => lower_in(a, snap, hole)?.collect_set()?.rank(*x),
        IntExpr::At(v, i) => eval_vec_int_at(v, *i, snap, hole)?,
    })
}

/// Evaluate an arbitrary-precision expression.
pub fn big(e: &BigExpr, snap: &Snapshot) -> yesno_core::Result<BigInt> {
    eval_big(e, snap, None)
}

/// Evaluate one arbitrary-precision integer per constituent.
///
/// The `Big` analogue of [`vec_int`]. Each element is evaluated with the hole
/// bound to that constituent, exactly as the integer vector does, so the two
/// sorts agree about what a `map` means.
pub fn vec_big(v: &VecBigExpr, snap: &Snapshot) -> yesno_core::Result<Vec<BigInt>> {
    eval_vec_big(v, snap, None)
}

fn eval_vec_big(
    v: &VecBigExpr,
    snap: &Snapshot,
    hole: Hole<'_>,
) -> yesno_core::Result<Vec<BigInt>> {
    match v {
        VecBigExpr::List(xs) => xs.iter().map(|x| eval_big(x, snap, hole)).collect(),
        VecBigExpr::Map(vector, body) => {
            let arity = vector.arity();
            let mut out = Vec::with_capacity(arity as usize);
            for i in 0..arity {
                let part = lower_vec_at(vector, i, snap, hole)?.collect_set()?;
                out.push(eval_big(body, snap, Some(&Arc::new(part)))?);
            }
            Ok(out)
        }
        // Position by position. The arities agree because the decoder refused
        // the payload otherwise, so this cannot be a partial answer.
        VecBigExpr::Zip(a, b, op) => {
            let left = eval_vec_big(a, snap, hole)?;
            let right = eval_vec_big(b, snap, hole)?;
            debug_assert_eq!(left.len(), right.len(), "arity is checked at decode");
            left.iter()
                .zip(right.iter())
                .map(|(x, y)| apply_bin(*op, x, y))
                .collect()
        }
        // The scalar is evaluated once, not once per element: it cannot depend
        // on the position, since nothing in the language can.
        VecBigExpr::Scale(v, x, op) => {
            let elements = eval_vec_big(v, snap, hole)?;
            let scalar = eval_big(x, snap, hole)?;
            elements
                .iter()
                .map(|e| apply_bin(*op, e, &scalar))
                .collect()
        }
    }
}

/// One element-wise application, with the scalar nodes' own semantics.
///
/// Routed through the same operations [`BigExpr`]'s binary nodes use, so a zip
/// and the equivalent written out element by element cannot disagree -- which
/// is the property a second implementation here would quietly break.
fn apply_bin(op: BigBinOp, a: &BigInt, b: &BigInt) -> yesno_core::Result<BigInt> {
    Ok(match op {
        BigBinOp::Add => a.add(b),
        BigBinOp::Sub => a.sub(b),
        BigBinOp::Mul => a.mul(b),
        BigBinOp::Div => a.divrem(b).ok_or(DIVIDE_BY_ZERO)?.0,
        BigBinOp::Rem => a.divrem(b).ok_or(DIVIDE_BY_ZERO)?.1,
    })
}

fn eval_big(e: &BigExpr, snap: &Snapshot, hole: Hole<'_>) -> yesno_core::Result<BigInt> {
    Ok(match e {
        BigExpr::Lit(v) => {
            BigInt::from_magnitude(v.is_negative(), BigUint::from_le_bytes(v.magnitude_le()))
        }
        // The inclusion of the counts into the integers. Never negative, which
        // is the whole content of the widening.
        BigExpr::Widen(a) => BigInt::from_uint(BigUint::from_u64(eval_int(a, snap, hole)?)),
        BigExpr::Read(a, width) => BigInt::from_uint(read_raw(a, u64::from(*width), snap, hole)?),
        BigExpr::ReadSigned(a, width) => {
            let width = u64::from(*width);
            let raw = read_raw(a, width, snap, hole)?;
            // Two's complement: the top addressable bit is the sign, and a
            // negative value's magnitude is `2^width - raw`. The subtraction
            // cannot fail -- `read_int` yields a value below `2^width` -- but it
            // is an `Option` because unsigned subtraction is partial, so the
            // impossible case is named rather than unwrapped.
            if raw.bit(width - 1) {
                let magnitude = BigUint::one().shl(width).sub(&raw).ok_or(
                    yesno_core::CodecError::Invariant(
                        "a read value exceeded its own declared width",
                    ),
                )?;
                BigInt::from_magnitude(true, magnitude)
            } else {
                BigInt::from_uint(raw)
            }
        }
        BigExpr::Neg(a) => eval_big(a, snap, hole)?.neg(),
        BigExpr::Add(a, b) => eval_big(a, snap, hole)?.add(&eval_big(b, snap, hole)?),
        BigExpr::Sub(a, b) => eval_big(a, snap, hole)?.sub(&eval_big(b, snap, hole)?),
        BigExpr::Mul(a, b) => eval_big(a, snap, hole)?.mul(&eval_big(b, snap, hole)?),
        // A zero divisor has no answer in the domain, which the decoder cannot
        // see: the divisor is a value, not a descriptor.
        BigExpr::Div(a, b) => {
            eval_big(a, snap, hole)?
                .divrem(&eval_big(b, snap, hole)?)
                .ok_or(DIVIDE_BY_ZERO)?
                .0
        }
        BigExpr::Rem(a, b) => {
            eval_big(a, snap, hole)?
                .divrem(&eval_big(b, snap, hole)?)
                .ok_or(DIVIDE_BY_ZERO)?
                .1
        }
        // The wrap and the ceiling. Two rules, spelled apart, because they
        // coincide only when the residue is already the ceiling.
        BigExpr::Truncate(a, bits) => eval_big(a, snap, hole)?.truncate(u64::from(*bits)),
        BigExpr::Saturate(a, bits) => eval_big(a, snap, hole)?.saturate(u64::from(*bits)),
        BigExpr::PowMod(base, exp, modulus) => {
            let modulus = eval_big(modulus, snap, hole)?;
            let exp = eval_big(exp, snap, hole)?;
            // A negative exponent is a modular inverse, and `bignum` has none:
            // an extended GCD needs signed intermediates the magnitude type
            // deliberately lacks. Named rather than answered wrongly.
            if exp.is_negative() {
                return Err(yesno_core::CodecError::Invariant(
                    "a negative exponent needs a modular inverse, which this engine has not",
                ));
            }
            let barrett = Barrett::new(modulus.magnitude())
                .ok_or(yesno_core::CodecError::Invariant("modulus is zero"))?;
            let base = eval_big(base, snap, hole)?;
            // A negative base enters its residue class first, so the answer is
            // always in `[ 0, modulus )` rather than carrying a sign out.
            let reduced = barrett.reduce(base.magnitude());
            let reduced = if base.is_negative() && !reduced.is_zero() {
                modulus
                    .magnitude()
                    .sub(&reduced)
                    .expect("a residue is below its modulus")
            } else {
                reduced
            };
            BigInt::from_uint(barrett.pow_mod(&reduced, exp.magnitude()))
        }
        BigExpr::Fold(v, op) => {
            let values = eval_vec_big(v, snap, hole)?;
            // A vector is never empty -- the decoder refuses one -- so the
            // fold needs no identity, which is what admits `min` and `max` in
            // a domain that has no least or greatest element.
            let mut it = values.into_iter();
            let first = it.next().ok_or(yesno_core::CodecError::Invariant(
                "a fold over an empty vector has no value",
            ))?;
            it.fold(first, |acc, x| match op {
                BigFoldOp::Add => acc.add(&x),
                BigFoldOp::Mul => acc.mul(&x),
                BigFoldOp::Min => {
                    if x < acc {
                        x
                    } else {
                        acc
                    }
                }
                BigFoldOp::Max => {
                    if x > acc {
                        x
                    } else {
                        acc
                    }
                }
            })
        }
    })
}

/// A zero divisor, named once so both arms report it identically.
const DIVIDE_BY_ZERO: yesno_core::CodecError =
    yesno_core::CodecError::Invariant("division by zero");

/// The stored bits of one integer, before any sign is read into them.
///
/// Unlike `cardinality`, this materializes: `read_int` gathers from a set
/// rather than walking a stream.
fn read_raw(
    a: &SetExpr,
    width_bits: u64,
    snap: &Snapshot,
    hole: Hole<'_>,
) -> yesno_core::Result<BigUint> {
    // The width is the bound on this: `read_int` allocates for the lesser of
    // the width and the set's own extent, and the decoder has already refused a
    // width above `MAX_VALUE_BITS`. A set with a distant maximum is otherwise
    // cheap to store and expensive to render, which is the whole reason the
    // wire requires a width rather than defaulting to the whole set.
    let set = lower_in(a, snap, hole)?.collect_set()?;
    Ok(set.read_int(width_bits))
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
fn eval_intersection_cardinalities(
    packed: &OrdSet,
    view: &View,
    body: &SetExpr,
    snap: &Snapshot,
    hole: Hole<'_>,
) -> yesno_core::Result<Option<Vec<u64>>> {
    if let Some(filter) = intersection_filter(body, snap, hole)? {
        return Ok(Some(packed.view_intersection_cardinalities(view, &filter)));
    }
    let mut invariants = Vec::new();
    let mut holes = 0;
    collect_intersection_body(body, &mut invariants, &mut holes);
    if holes == 1 && invariants.is_empty() {
        return Ok(Some(packed.view_cardinalities(view)));
    }
    Ok(None)
}

fn intersection_filter(
    body: &SetExpr,
    snap: &Snapshot,
    hole: Hole<'_>,
) -> yesno_core::Result<Option<OrdSet>> {
    let mut invariants = Vec::new();
    let mut holes = 0;
    collect_intersection_body(body, &mut invariants, &mut holes);
    if holes != 1 || invariants.is_empty() {
        return Ok(None);
    }

    let mut invariants = invariants.into_iter();
    let mut filter = lower_in(invariants.next().unwrap(), snap, hole)?;
    for invariant in invariants {
        filter = filter.and(lower_in(invariant, snap, hole)?);
    }
    Ok(Some(filter.collect_set()?))
}

fn direct_key_intersection(v: &VecIntExpr) -> Option<(u64, ViewSpec, &SetExpr)> {
    let VecIntExpr::Map(vector, terminal) = v else {
        return None;
    };
    let IntExpr::Cardinality(body) = terminal.as_ref() else {
        return None;
    };
    let VecSetExpr::View(input, spec) = vector.as_ref() else {
        return None;
    };
    let SetExpr::Key(key) = input.as_ref() else {
        return None;
    };
    Some((*key, *spec, body))
}

fn count_key_intersections(
    key: u64,
    view: View,
    filters: &[&OrdSet],
    snap: &Snapshot,
) -> yesno_core::Result<Vec<Vec<u64>>> {
    if filters.iter().all(|filter| filter.is_empty()) {
        return Ok(vec![vec![0; view.sets() as usize]; filters.len()]);
    }
    let (strategy, mut counter) = match view.layout() {
        yesno_core::view::ViewLayout::Interleaved => {
            let logical_end = snap
                .max(key)?
                .and_then(|physical| view.logical_of(physical))
                .and_then(|(_, logical)| logical.checked_add(1))
                .unwrap_or(0);
            let support_upper = filters
                .iter()
                .fold(0u128, |sum, filter| sum + u128::from(filter.len()));
            let strategy = if support_upper * 2 < u128::from(logical_end) {
                IntersectionCountStrategy::Selective
            } else {
                IntersectionCountStrategy::FullScan
            };
            (
                strategy,
                ViewIntersectionCounter::new(view, filters.iter().copied(), strategy)?,
            )
        }
        yesno_core::view::ViewLayout::Blocked { .. } => (
            IntersectionCountStrategy::FullScan,
            ViewIntersectionCounter::new(
                view,
                filters.iter().copied(),
                IntersectionCountStrategy::FullScan,
            )?,
        ),
    };

    if strategy == IntersectionCountStrategy::Selective {
        let windows = counter.prefix_windows().to_vec();
        for (lo, hi) in windows {
            let mut stream = snap.key_stream_prefix_range(key, lo, hi)?;
            while let Some((prefix, container)) = stream.next_chunk()? {
                counter.push(prefix, &container)?;
            }
        }
    } else {
        let mut stream = snap.key_stream(key)?;
        while let Some((prefix, container)) = stream.next_chunk()? {
            counter.push(prefix, &container)?;
        }
    }
    counter.finish()
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
    let base = expr_cardinality(restrict(when_absent.clone()))?;
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
    if let (SetExpr::Key(key), IntExpr::Rank(operand, upper)) = (input, body) {
        if matches!(operand.as_ref(), SetExpr::Hole) {
            if let Some(prefix_end) = interleaved_rank_prefix_end(spec, *upper) {
                let mut stream = snap.key_stream_prefix_range(*key, 0, prefix_end)?;
                return stream_view_ranks(&mut stream, &view, *upper).map(Some);
            }
        }
    }
    if let (SetExpr::Key(key), IntExpr::Cardinality(operand)) = (input, body) {
        if matches!(operand.as_ref(), SetExpr::Hole) {
            let (mut stream, admitted) = key_view_stream(*key, spec, snap)?;
            let out = if admitted {
                stream_view_cardinalities(&mut stream, &view)?
            } else {
                stream.collect_set()?.view_cardinalities(&view)
            };
            return Ok(Some(out));
        }
        if let Some(filter) = intersection_filter(operand, snap, hole)? {
            return count_key_intersections(*key, view, &[&filter], snap)
                .map(|mut results| results.pop());
        }
    }
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
                out.push(expr_cardinality(prepared.eval(snap, &part)?)?);
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

    #[test]
    fn extremely_sparse_view_admission_is_narrow_and_boundary_exact() {
        let interleaved = ViewSpec::interleaved(4);
        assert!(!extremely_sparse_view_admitted(
            interleaved,
            Some(SPARSE_VIEW_MIN_CHUNKS - 1),
            (SPARSE_VIEW_MIN_CHUNKS - 1, Some(SPARSE_VIEW_MIN_CHUNKS - 1)),
        ));
        assert!(extremely_sparse_view_admitted(
            interleaved,
            Some(SPARSE_VIEW_MIN_CHUNKS),
            (SPARSE_VIEW_MIN_CHUNKS, Some(SPARSE_VIEW_MIN_CHUNKS)),
        ));
        assert!(!extremely_sparse_view_admitted(
            interleaved,
            Some(SPARSE_VIEW_MIN_CHUNKS),
            (SPARSE_VIEW_MIN_CHUNKS, Some(SPARSE_VIEW_MIN_CHUNKS + 1)),
        ));
        assert!(!extremely_sparse_view_admitted(
            ViewSpec::blocked(4, 65_536),
            Some(SPARSE_VIEW_MIN_CHUNKS),
            (SPARSE_VIEW_MIN_CHUNKS, Some(SPARSE_VIEW_MIN_CHUNKS)),
        ));
        assert!(!extremely_sparse_view_admitted(
            interleaved,
            None,
            (SPARSE_VIEW_MIN_CHUNKS, Some(SPARSE_VIEW_MIN_CHUNKS)),
        ));
        assert!(!extremely_sparse_view_admitted(
            ViewSpec {
                sets: 0,
                layout: ViewLayout::Interleaved,
            },
            Some(SPARSE_VIEW_MIN_CHUNKS),
            (SPARSE_VIEW_MIN_CHUNKS, Some(SPARSE_VIEW_MIN_CHUNKS)),
        ));
    }

    #[test]
    fn interleaved_rank_prefix_end_is_ceil_divided_and_saturating() {
        let interleaved = ViewSpec::interleaved(4);
        assert_eq!(interleaved_rank_prefix_end(interleaved, 0), Some(0));
        assert_eq!(interleaved_rank_prefix_end(interleaved, 1), Some(1));
        assert_eq!(interleaved_rank_prefix_end(interleaved, 16_384), Some(1));
        assert_eq!(interleaved_rank_prefix_end(interleaved, 16_385), Some(2));
        assert_eq!(
            interleaved_rank_prefix_end(ViewSpec::interleaved(1), u64::MAX),
            Some(PREFIX_EXCLUSIVE_END)
        );
        assert_eq!(
            interleaved_rank_prefix_end(ViewSpec::interleaved(4_096), u64::MAX),
            Some(PREFIX_EXCLUSIVE_END)
        );
        assert_eq!(
            interleaved_rank_prefix_end(ViewSpec::blocked(4, 65_536), 1),
            None
        );
        assert_eq!(
            interleaved_rank_prefix_end(
                ViewSpec {
                    sets: 0,
                    layout: ViewLayout::Interleaved,
                },
                1,
            ),
            None
        );
    }

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

#[cfg(test)]
mod big_expr_tests {
    use super::*;
    use yesno_core::Db;
    use yesno_core::OrdSet;

    /// Store a series of integers through the lens, then name one of them in
    /// the expression language and read it back.
    ///
    /// This is the gap the sort exists to close: the engine could already do
    /// this reading, and nothing on the wire could ask for it.
    #[test]
    fn an_integer_stored_through_the_lens_is_readable_as_an_expression() {
        let values = [
            BigUint::from_u64(7),
            BigUint::from_limbs_le(vec![0xDEAD_BEEF_CAFE_BABE, 0x1234_5678]),
            BigUint::from_u64(0),
            BigUint::from_limbs_le(vec![u64::MAX, u64::MAX >> 1]),
        ];

        // One set is one integer, so each value is its own key.
        let db = Db::new();
        for (k, v) in values.iter().enumerate() {
            let ordinals: Vec<u64> = OrdSet::from_int(v).unwrap().iter().collect();
            db.insert_many(11 + k as u64, &ordinals).unwrap();
        }
        let snap = db.snapshot().unwrap();

        for (k, want) in values.iter().enumerate() {
            let e = BigExpr::Read(Box::new(SetExpr::Key(11 + k as u64)), 128);
            // Through the wire, not just the in-process value: this is the path
            // a client actually takes.
            let decoded = match AnyExpr::decode(&AnyExpr::Big(e).encode()).unwrap() {
                AnyExpr::Big(b) => b,
                other => panic!("decoded as {:?}", other.sort()),
            };
            let got = big(&decoded, &snap).unwrap();
            assert!(!got.is_negative(), "a read is never negative");
            assert_eq!(got.magnitude(), want, "integer {k}");
        }
    }

    /// Reading under a **narrower** width is `x mod 2^width`, which is the
    /// identity the least-significant-bit-first layout exists to buy. It holds
    /// through the expression language, not only in the lens.
    #[test]
    fn a_narrower_read_is_the_value_modulo_two_to_the_width() {
        let v = BigUint::from_limbs_le(vec![0xDEAD_BEEF_CAFE_BABE, 0x1234_5678]);
        let ordinals: Vec<u64> = OrdSet::from_int(&v).unwrap().iter().collect();

        let db = Db::new();
        db.insert_many(3, &ordinals).unwrap();
        let snap = db.snapshot().unwrap();

        let narrow = BigExpr::Read(Box::new(SetExpr::Key(3)), 64);
        let got = big(&narrow, &snap).unwrap();
        assert_eq!(got.magnitude(), &v.truncate(64));
    }

    #[test]
    fn a_negative_literal_survives_the_wire() {
        let db = Db::new();
        let snap = db.snapshot().unwrap();
        let e = BigExpr::Lit(BigLit::from_i64(-256));
        let decoded = match AnyExpr::decode(&AnyExpr::Big(e).encode()).unwrap() {
            AnyExpr::Big(b) => b,
            other => panic!("decoded as {:?}", other.sort()),
        };
        let got = big(&decoded, &snap).unwrap();
        assert!(got.is_negative());
        assert_eq!(got.magnitude(), &BigUint::from_u64(256));
    }

    /// The widening is the inclusion of the counts into the integers, so it
    /// must agree with the count it widens.
    #[test]
    fn a_widened_cardinality_equals_the_cardinality() {
        let db = Db::new();
        db.insert_many(5, &[1, 2, 3, 9, 40]).unwrap();
        let snap = db.snapshot().unwrap();

        let e = BigExpr::Widen(Box::new(IntExpr::Cardinality(Box::new(SetExpr::Key(5)))));
        let got = big(&e, &snap).unwrap();
        assert!(!got.is_negative());
        assert_eq!(got.magnitude(), &BigUint::from_u64(5));
    }

    /// There is no unaddressable case left: a set is an integer, so every read
    /// has an answer and a set with nothing in the width reads as zero.
    #[test]
    fn a_read_is_total() {
        let db = Db::new();
        db.insert_many(4, &[0, 1]).unwrap();
        let snap = db.snapshot().unwrap();

        // Reading is total now: an empty region is zero, not an error, because
        // absence and zero are the same thing in a set.
        let e = BigExpr::Read(Box::new(SetExpr::Key(4)), 1024);
        assert_eq!(
            big(&e, &snap).unwrap().magnitude(),
            &BigUint::from_u64(0b11)
        );
    }
}

#[cfg(test)]
mod signed_read_tests {
    use super::*;
    use yesno_core::Db;
    use yesno_core::OrdSet;

    /// The same stored bits, read twice, denoting two different numbers.
    ///
    /// This is exactly why the signed reading is its own node and not a flag:
    /// there is no bit pattern that announces which one was meant.
    #[test]
    fn a_signed_read_is_twos_complement_over_the_declared_width() {
        // 255 -> -1, 128 -> -128, 127 -> 127, 0 -> 0.
        let stored: [u64; 4] = [255, 128, 127, 0];
        let want_signed: [i64; 4] = [-1, -128, 127, 0];

        // One set is one integer, so each stored pattern is its own key.
        let db = Db::new();
        for (k, v) in stored.iter().enumerate() {
            let ordinals: Vec<u64> = OrdSet::from_int(&BigUint::from_u64(*v))
                .unwrap()
                .iter()
                .collect();
            db.insert_many(21 + k as u64, &ordinals).unwrap();
        }
        let snap = db.snapshot().unwrap();

        for (k, (raw, signed)) in stored.iter().zip(want_signed.iter()).enumerate() {
            let key = 21 + k as u64;
            let k = k as u64;

            let unsigned = big(&BigExpr::Read(Box::new(SetExpr::Key(key)), 8), &snap).unwrap();
            assert!(!unsigned.is_negative());
            assert_eq!(unsigned.magnitude(), &BigUint::from_u64(*raw), "raw {k}");

            let got = big(&BigExpr::ReadSigned(Box::new(SetExpr::Key(key)), 8), &snap).unwrap();
            assert_eq!(got.is_negative(), *signed < 0, "sign of {k}");
            assert_eq!(
                got.magnitude(),
                &BigUint::from_u64(signed.unsigned_abs()),
                "magnitude of {k}"
            );
        }
    }

    /// A signed read survives the wire, and the two readings stay distinct
    /// across it -- a client cannot lose which one it asked for.
    #[test]
    fn the_two_readings_stay_distinct_across_the_wire() {
        let ordinals: Vec<u64> = OrdSet::from_int(&BigUint::from_u64(u64::MAX))
            .unwrap()
            .iter()
            .collect();

        let db = Db::new();
        db.insert_many(22, &ordinals).unwrap();
        let snap = db.snapshot().unwrap();

        for (e, negative) in [
            (BigExpr::Read(Box::new(SetExpr::Key(22)), 64), false),
            (BigExpr::ReadSigned(Box::new(SetExpr::Key(22)), 64), true),
        ] {
            let decoded = match AnyExpr::decode(&AnyExpr::Big(e).encode()).unwrap() {
                AnyExpr::Big(b) => b,
                other => panic!("decoded as {:?}", other.sort()),
            };
            let got = big(&decoded, &snap).unwrap();
            assert_eq!(got.is_negative(), negative);
        }
    }

    /// A one-bit signed integer holds only 0 and -1. The width is the whole
    /// content of the sign, so the narrowest case is worth pinning.
    #[test]
    fn a_one_bit_signed_read_is_zero_or_minus_one() {
        // Key 23 is empty, so it reads as zero. Key 24 holds ordinal 0, which
        // at width 1 is the sign bit and nothing else.
        let db = Db::new();
        db.insert_many(24, &[0u64]).unwrap();
        let snap = db.snapshot().unwrap();

        let zero = big(&BigExpr::ReadSigned(Box::new(SetExpr::Key(23)), 1), &snap).unwrap();
        assert!(!zero.is_negative());
        assert_eq!(zero.magnitude(), &BigUint::zero());

        let minus_one = big(&BigExpr::ReadSigned(Box::new(SetExpr::Key(24)), 1), &snap).unwrap();
        assert!(minus_one.is_negative());
        assert_eq!(minus_one.magnitude(), &BigUint::one());
    }
}

#[cfg(test)]
mod big_arithmetic_tests {
    use super::*;
    use yesno_core::Db;

    fn lit(v: i64) -> Box<BigExpr> {
        Box::new(BigExpr::Lit(BigLit::from_i64(v)))
    }

    fn eval(e: BigExpr) -> BigInt {
        let db = Db::new();
        let snap = db.snapshot().unwrap();
        // Through the wire, so the tests cover the encoding too.
        let decoded = match AnyExpr::decode(&AnyExpr::Big(e).encode()).unwrap() {
            AnyExpr::Big(b) => b,
            other => panic!("decoded as {:?}", other.sort()),
        };
        big(&decoded, &snap).unwrap()
    }

    /// Rust's own operators are the reference, which is available here because
    /// the operands fit a machine word even though the sort does not require it.
    #[test]
    fn the_five_operations_agree_with_machine_arithmetic() {
        for a in [-100i64, -7, -1, 0, 1, 7, 100] {
            for b in [-9i64, -2, -1, 1, 2, 9] {
                assert_eq!(eval(BigExpr::Add(lit(a), lit(b))), BigInt::from_i64(a + b));
                assert_eq!(eval(BigExpr::Sub(lit(a), lit(b))), BigInt::from_i64(a - b));
                assert_eq!(eval(BigExpr::Mul(lit(a), lit(b))), BigInt::from_i64(a * b));
                assert_eq!(eval(BigExpr::Div(lit(a), lit(b))), BigInt::from_i64(a / b));
                assert_eq!(eval(BigExpr::Rem(lit(a), lit(b))), BigInt::from_i64(a % b));
            }
            assert_eq!(eval(BigExpr::Neg(lit(a))), BigInt::from_i64(-a));
        }
    }

    /// Beyond a machine word, which is the point of the sort.
    #[test]
    fn arithmetic_runs_past_sixty_four_bits() {
        let big_lit = |v: BigLit| Box::new(BigExpr::Lit(v));
        let a = BigLit::from_le_bytes(false, vec![0xFF; 16]).unwrap();
        let squared = eval(BigExpr::Mul(big_lit(a.clone()), big_lit(a)));
        // (2^128 - 1)^2 = 2^256 - 2^129 + 1, so the top bit is 255.
        assert_eq!(squared.magnitude().bit_len(), 256);
        assert!(!squared.is_negative());
    }

    #[test]
    fn a_zero_divisor_is_an_error_not_a_value() {
        let db = Db::new();
        let snap = db.snapshot().unwrap();
        for e in [BigExpr::Div(lit(7), lit(0)), BigExpr::Rem(lit(7), lit(0))] {
            assert!(big(&e, &snap).is_err());
        }
    }

    /// The wrap and the ceiling are different rules, and the language keeps
    /// them apart rather than offering one under two names.
    #[test]
    fn truncate_wraps_where_saturate_clamps() {
        // One field, two overflow rules. Machine `i8` is the reference for the
        // wrap; `clamp` for the ceiling.
        for v in [-300i64, -129, -1, 0, 1, 127, 128, 255, 300] {
            assert_eq!(
                eval(BigExpr::Truncate(lit(v), 8)),
                BigInt::from_i64(v as i8 as i64),
                "truncate {v}"
            );
            assert_eq!(
                eval(BigExpr::Saturate(lit(v), 8)),
                BigInt::from_i64(v.clamp(-128, 127)),
                "saturate {v}"
            );
        }
        // Different rules, so they part company as soon as anything overflows.
        assert_ne!(
            eval(BigExpr::Truncate(lit(128), 8)),
            eval(BigExpr::Saturate(lit(128), 8))
        );
    }

    /// **The amplification the width budget exists to refuse, and it is a
    /// `Read` that supplies it.**
    ///
    /// A multiplication over *literals* does not amplify: the wire has no
    /// sharing, so `Mul( a, a )` writes `a` twice and the payload grows with
    /// the width. A `Read` is the asymmetry -- six bytes that declare a width
    /// -- so one `Mul` of two maximal reads is a fifteen-byte payload
    /// describing a value no machine should try to build. Refused at decode,
    /// before anything evaluates.
    #[test]
    fn a_multiplication_of_wide_reads_is_refused_before_it_runs() {
        let read = || {
            Box::new(BigExpr::Read(
                Box::new(SetExpr::Key(1)),
                MAX_VALUE_BITS as u32,
            ))
        };
        let payload = AnyExpr::Big(BigExpr::Mul(read(), read())).encode();
        assert!(
            payload.len() < 64,
            "payload was {} bytes, so it is not an amplification",
            payload.len()
        );
        assert_eq!(AnyExpr::decode(&payload), Err(ExprError::ValueTooWide));
    }

    /// The same node one bit under the bound is accepted, so the refusal above
    /// is the bound doing its job rather than the shape being rejected.
    #[test]
    fn the_same_shape_just_inside_the_bound_is_accepted() {
        let read = || {
            Box::new(BigExpr::Read(
                Box::new(SetExpr::Key(1)),
                (MAX_VALUE_BITS / 2) as u32,
            ))
        };
        assert!(AnyExpr::decode(&AnyExpr::Big(BigExpr::Mul(read(), read())).encode()).is_ok());
    }

    /// And the bound is not so tight that ordinary arithmetic trips it.
    #[test]
    fn arithmetic_within_the_budget_is_accepted() {
        let wide = BigLit::from_le_bytes(false, vec![0xAB; 4096]).unwrap();
        let e = BigExpr::Mul(
            Box::new(BigExpr::Lit(wide.clone())),
            Box::new(BigExpr::Lit(wide)),
        );
        assert!(AnyExpr::decode(&AnyExpr::Big(e).encode()).is_ok());
    }
}

#[cfg(test)]
mod big_fold_tests {
    use super::*;
    use yesno_core::Db;

    fn lit(v: i64) -> BigExpr {
        BigExpr::Lit(BigLit::from_i64(v))
    }

    fn fold(values: &[i64], op: BigFoldOp) -> BigInt {
        let db = Db::new();
        let snap = db.snapshot().unwrap();
        let e = BigExpr::Fold(
            Box::new(VecBigExpr::List(values.iter().copied().map(lit).collect())),
            op,
        );
        // Through the wire, so the operator byte is covered too.
        let decoded = match AnyExpr::decode(&AnyExpr::Big(e).encode()).unwrap() {
            AnyExpr::Big(b) => b,
            other => panic!("decoded as {:?}", other.sort()),
        };
        big(&decoded, &snap).unwrap()
    }

    /// All four operators, against the obvious reduction over machine ints.
    #[test]
    fn the_four_operators_reduce_as_they_say() {
        let v = [3i64, -7, 2, 10];
        assert_eq!(fold(&v, BigFoldOp::Add), BigInt::from_i64(8));
        assert_eq!(fold(&v, BigFoldOp::Mul), BigInt::from_i64(-420));
        assert_eq!(fold(&v, BigFoldOp::Min), BigInt::from_i64(-7));
        assert_eq!(fold(&v, BigFoldOp::Max), BigInt::from_i64(10));
    }

    /// A one-element vector is its own fold under every operator, which is the
    /// case an identity would be needed for if the vector could be empty.
    #[test]
    fn a_single_element_folds_to_itself() {
        for op in [
            BigFoldOp::Add,
            BigFoldOp::Mul,
            BigFoldOp::Min,
            BigFoldOp::Max,
        ] {
            assert_eq!(fold(&[-5], op), BigInt::from_i64(-5));
        }
    }

    /// **Only `mul` grows with the arity, and the budget knows it.** A sum of
    /// `n` values costs a handful of bits; a product costs `n` times the
    /// element width, which is the same unbounded product `MAX_RESULT_BITS`
    /// exists for -- except that here it lands in one value, so `MAX_VALUE_BITS`
    /// has to hold it.
    #[test]
    fn a_product_fold_is_bounded_where_a_sum_fold_is_not_troubled() {
        let wide = || {
            VecBigExpr::Map(
                Box::new(VecSetExpr::View(
                    Box::new(SetExpr::Key(1)),
                    ViewSpec::interleaved(1024),
                )),
                Box::new(BigExpr::Read(Box::new(SetExpr::Hole), 4096)),
            )
        };
        // 1024 * 4096 = 4 Mibit, four times over MAX_VALUE_BITS.
        let product = AnyExpr::Big(BigExpr::Fold(Box::new(wide()), BigFoldOp::Mul));
        assert_eq!(
            AnyExpr::decode(&product.encode()),
            Err(ExprError::ValueTooWide)
        );
        // The same vector sums to 4096 + 11 bits, which is nowhere near it.
        let sum = AnyExpr::Big(BigExpr::Fold(Box::new(wide()), BigFoldOp::Add));
        assert!(AnyExpr::decode(&sum.encode()).is_ok());
    }

    /// The fold reads the same stored values the vector sort returns, so the
    /// two agree by construction rather than by coincidence.
    #[test]
    fn a_fold_agrees_with_reducing_the_vector_it_folds() {
        use yesno_core::bignum::BigUint;

        let values = [BigUint::from_u64(11), BigUint::from_u64(4), BigUint::zero()];
        let sets = 3u32;
        let mut packed: Vec<u64> = Vec::new();
        for (i, v) in values.iter().enumerate() {
            for x in 0..v.bit_len() {
                if v.bit(x) {
                    packed.push(x * u64::from(sets) + i as u64);
                }
            }
        }
        let db = Db::new();
        db.insert_many(77, &packed).unwrap();
        let snap = db.snapshot().unwrap();

        let vector = VecBigExpr::Map(
            Box::new(VecSetExpr::View(
                Box::new(SetExpr::Key(77)),
                ViewSpec::interleaved(sets),
            )),
            Box::new(BigExpr::Read(Box::new(SetExpr::Hole), 64)),
        );
        let elements = vec_big(&vector, &snap).unwrap();
        let summed = big(&BigExpr::Fold(Box::new(vector), BigFoldOp::Add), &snap).unwrap();

        let want = elements
            .iter()
            .cloned()
            .reduce(|a, b| a.add(&b))
            .expect("non-empty");
        assert_eq!(summed, want);
        assert_eq!(summed.magnitude(), &BigUint::from_u64(15));
    }
}

#[cfg(test)]
mod pow_mod_tests {
    use super::*;
    use yesno_core::Db;

    fn lit(v: i64) -> Box<BigExpr> {
        Box::new(BigExpr::Lit(BigLit::from_i64(v)))
    }

    fn eval(e: BigExpr) -> yesno_core::Result<BigInt> {
        let db = Db::new();
        let snap = db.snapshot().unwrap();
        // Through the wire, so the node's encoding is covered too.
        let decoded = match AnyExpr::decode(&AnyExpr::Big(e).encode()).unwrap() {
            AnyExpr::Big(b) => b,
            other => panic!("decoded as {:?}", other.sort()),
        };
        big(&decoded, &snap)
    }

    /// Against modular arithmetic done the slow way, which is available here
    /// because the operands fit a machine word even though the node does not
    /// require it.
    #[test]
    fn it_agrees_with_repeated_multiplication() {
        for (b, e, m) in [(2i64, 10i64, 1000i64), (7, 0, 13), (5, 117, 19), (3, 5, 7)] {
            let want = (0..e).fold(1i64, |acc, _| acc * b % m);
            assert_eq!(
                eval(BigExpr::PowMod(lit(b), lit(e), lit(m))).unwrap(),
                BigInt::from_i64(want),
                "{b}^{e} mod {m}"
            );
        }
    }

    /// `m == 1` makes every residue zero, **including** `base^0`. The classic
    /// wrong answer is `1`, and `bignum`'s own header names this case.
    #[test]
    fn a_modulus_of_one_reduces_everything_including_a_zero_exponent() {
        assert_eq!(
            eval(BigExpr::PowMod(lit(5), lit(0), lit(1))).unwrap(),
            BigInt::zero()
        );
        assert_eq!(
            eval(BigExpr::PowMod(lit(5), lit(3), lit(1))).unwrap(),
            BigInt::zero()
        );
    }

    /// A negative base enters its residue class; the answer never carries a
    /// sign out of `[ 0, m )`.
    #[test]
    fn a_negative_base_enters_its_residue_class() {
        // (-1)^3 mod 7 = -1 mod 7 = 6.
        let got = eval(BigExpr::PowMod(lit(-1), lit(3), lit(7))).unwrap();
        assert!(!got.is_negative());
        assert_eq!(got, BigInt::from_i64(6));
        // (-2)^2 mod 7 = 4.
        assert_eq!(
            eval(BigExpr::PowMod(lit(-2), lit(2), lit(7))).unwrap(),
            BigInt::from_i64(4)
        );
    }

    /// Both are facts about the operands rather than answers, so both are
    /// errors. A zero would be a well-formed wrong answer for either.
    #[test]
    fn a_zero_modulus_and_a_negative_exponent_are_errors() {
        assert!(eval(BigExpr::PowMod(lit(2), lit(3), lit(0))).is_err());
        assert!(eval(BigExpr::PowMod(lit(2), lit(-3), lit(7))).is_err());
    }

    /// **The amplification the work bound exists for, and the one the width
    /// bound structurally cannot see.** The result is only as wide as the
    /// modulus, so `width_bound` finds nothing wrong with a payload that names
    /// a computation which would not finish.
    #[test]
    fn a_costly_exponentiation_is_refused_where_its_width_is_unremarkable() {
        let wide = |bits: u32| Box::new(BigExpr::Read(Box::new(SetExpr::Key(1)), bits));
        let e = BigExpr::PowMod(lit(2), wide(1 << 20), wide(1 << 20));

        // The width bound is untroubled: a residue is as wide as its modulus.
        assert_eq!(e.width_bound(), 1 << 20);
        assert!(e.width_bound() <= MAX_VALUE_BITS);
        // The work bound is not.
        assert!(e.work_bound() > MAX_WORK);

        let payload = AnyExpr::Big(e).encode();
        assert!(payload.len() < 64, "payload was {} bytes", payload.len());
        assert_eq!(AnyExpr::decode(&payload), Err(ExprError::TooMuchWork));
    }

    /// And the sizes a caller plausibly means are admitted, so the bound is
    /// calibrated rather than merely restrictive.
    #[test]
    fn rsa_scale_exponentiation_is_admitted() {
        for bits in [2048u32, 4096] {
            let operand = || Box::new(BigExpr::Read(Box::new(SetExpr::Key(1)), bits));
            let e = BigExpr::PowMod(lit(2), operand(), operand());
            assert!(
                e.work_bound() <= MAX_WORK,
                "{bits}-bit modulus cost {} exceeds the budget",
                e.work_bound()
            );
            assert!(AnyExpr::decode(&AnyExpr::Big(e).encode()).is_ok());
        }
    }
}

#[cfg(test)]
mod vec_big_arithmetic_tests {
    use super::*;
    use yesno_core::Db;

    fn vec_of(values: &[i64]) -> VecBigExpr {
        VecBigExpr::List(
            values
                .iter()
                .map(|v| BigExpr::Lit(BigLit::from_i64(*v)))
                .collect(),
        )
    }

    fn eval(v: VecBigExpr) -> Vec<BigInt> {
        let db = Db::new();
        let snap = db.snapshot().unwrap();
        // Through the wire, so the operator byte and the arity check are
        // covered rather than bypassed.
        let decoded = match AnyExpr::decode(&AnyExpr::VecBig(v).encode()).unwrap() {
            AnyExpr::VecBig(x) => x,
            other => panic!("decoded as {:?}", other.sort()),
        };
        vec_big(&decoded, &snap).unwrap()
    }

    fn want(values: &[i64]) -> Vec<BigInt> {
        values.iter().map(|v| BigInt::from_i64(*v)).collect()
    }

    /// All five operators, position by position, against machine arithmetic.
    #[test]
    fn a_zip_applies_its_operator_position_by_position() {
        let a = [10i64, -20, 7];
        let b = [3i64, 4, -2];
        for (op, expected) in [
            (BigBinOp::Add, [13i64, -16, 5]),
            (BigBinOp::Sub, [7, -24, 9]),
            (BigBinOp::Mul, [30, -80, -14]),
            (BigBinOp::Div, [3, -5, -3]),
            (BigBinOp::Rem, [1, 0, 1]),
        ] {
            assert_eq!(
                eval(VecBigExpr::Zip(
                    Box::new(vec_of(&a)),
                    Box::new(vec_of(&b)),
                    op
                )),
                want(&expected),
                "{op:?}"
            );
        }
    }

    /// The scalar is on the **right**, which matters for the two operators
    /// that are not commutative and which the bytes do not announce.
    #[test]
    fn a_scale_puts_the_scalar_on_the_right() {
        let v = vec_of(&[10, -20, 7]);
        let three = Box::new(BigExpr::Lit(BigLit::from_i64(3)));
        assert_eq!(
            eval(VecBigExpr::Scale(
                Box::new(v.clone()),
                three.clone(),
                BigBinOp::Sub
            )),
            want(&[7, -23, 4]),
        );
        assert_eq!(
            eval(VecBigExpr::Scale(Box::new(v), three, BigBinOp::Div)),
            want(&[3, -6, 2]),
        );
    }

    /// **The arity check is static**, which is what makes the evaluator's
    /// `zip` total rather than silently truncating to the shorter side.
    #[test]
    fn a_zip_of_unequal_arities_is_refused_at_decode() {
        let e = AnyExpr::VecBig(VecBigExpr::Zip(
            Box::new(vec_of(&[1, 2, 3])),
            Box::new(vec_of(&[1, 2])),
            BigBinOp::Add,
        ));
        assert_eq!(
            AnyExpr::decode(&e.encode()),
            Err(ExprError::ArityMismatch {
                expected: 3,
                found: 2
            })
        );
    }

    /// A zip and the same arithmetic written out element by element are bounded
    /// identically, so neither spelling can smuggle work past the budget.
    #[test]
    fn a_zip_is_bounded_like_the_elements_it_stands_for() {
        let wide = |bits: u32| {
            VecBigExpr::Map(
                Box::new(VecSetExpr::View(
                    Box::new(SetExpr::Key(1)),
                    ViewSpec::interleaved(8),
                )),
                Box::new(BigExpr::Read(Box::new(SetExpr::Hole), bits)),
            )
        };
        // A product doubles the element width, exactly as the scalar `Mul`
        // node does.
        let zipped = VecBigExpr::Zip(Box::new(wide(1024)), Box::new(wide(1024)), BigBinOp::Mul);
        assert_eq!(zipped.element_bound(), 2048);
        assert_eq!(zipped.result_bound(), 8 * 2048);

        // And the result bound still refuses what it refused before.
        let huge = VecBigExpr::Zip(
            Box::new(wide(MAX_VALUE_BITS as u32 / 2)),
            Box::new(wide(MAX_VALUE_BITS as u32 / 2)),
            BigBinOp::Mul,
        );
        assert!(AnyExpr::decode(&AnyExpr::VecBig(huge).encode()).is_err());
    }

    #[test]
    fn a_zero_divisor_in_a_zip_is_an_error() {
        let db = Db::new();
        let snap = db.snapshot().unwrap();
        let e = VecBigExpr::Zip(
            Box::new(vec_of(&[1, 2])),
            Box::new(vec_of(&[1, 0])),
            BigBinOp::Div,
        );
        assert!(vec_big(&e, &snap).is_err());
    }
}
