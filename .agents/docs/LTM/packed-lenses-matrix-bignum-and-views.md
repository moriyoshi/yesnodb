# Packed Lenses: Matrices, Integers, and Views

## Summary

`matrix/`, `bignum/`, and `view/` reinterpret one `OrdSet` through caller-owned affine layouts instead of introducing new storage formats. The shared principle is that the durable object remains an ordinal set; the lens supplies shape, packing, and algebra, while explicit sinks write the result back through ordinary set operations.

## Key Facts

- `BitMatrix` is a dense packed value over Boolean or GF(2) operations; it is not a sparse matrix store.
- `BigUint` is an unsigned little-endian limb value, while `IntLayout` owns stored width and stride.
- `ViewSpec` packs several constituent sets into one ordinal space using interleaved or blocked layouts.
- `matrix/` and `bignum/` share `pack/` because the useful abstraction is strided gathering and scattering across chunk boundaries, not duplicated loops.
- Layout descriptors are caller-owned metadata. None is persisted as a new database catalog or container encoding.
- A fold is not a restriction: `Any`, `All`, and `Parity` each commute with only their corresponding Boolean operator; `view_expand` is the homomorphic inverse-image direction.
- Packed view transforms cross Flight as dependency-free wire expressions. They are eager planner boundaries until a workload justifies new lazy expression nodes.
- A consumer's bit-sliced lens proposal was narrowed to `OrdSet::view_count` after measurement and then **declined in full on 2026-09-19**: the per-ordinal count is already reachable from the shipped public API by a ripple-carry over `view_select`, so nothing is being thrown away and the ask is a 4 - 8x ratio on one layout with no caller.
- Six further integer arms are deferred with recorded reasons, the load-bearing one being that Montgomery reduction must be a separate type rather than a flag, because entering an operand changes what the value means.
- Division is the better target than multiplication if that ladder resumes: sub-quadratic multiplication made division relatively worse, 2.3x at 64 limbs widening to 4.6x at 1024.

## Details

### Bit matrices

`MatrixLayout` maps `(row, column)` to an ordinal. Reads seek within containers instead of discarding prefixes. Bitmap lines become block transfers, run lines become range fills, and arrays use a cursor carried across monotonically increasing line windows. The mixed array-plus-bitmap case is the reason the cursor matters: removing an all-or-nothing array decline preserved the bitmap fast path and improved the measured case about 14x.

The generic product remains the oracle. Specialized arms are benchmark-gated:

- a one-word output-row arm pays for `N <= 64` and declines above it;
- sparse transpose scatters set bits, while dense transpose uses blocks;
- the Four Russians arm is restricted to sufficiently large, sufficiently dense operands because a false positive is much more expensive than a false negative;
- counted multiplication is materially more expensive than Boolean multiplication because it cannot skip the zero structure in the same way.

GF(2) support includes elimination, rank, inverse, solve, reusable `PA = LU`, matrix-vector products, and powers. `solve_gf2` delegates to factor-and-substitute because the former Gauss-Jordan path did unnecessary work even for one right-hand side. Reusing a factorization pays increasingly across several right-hand sides.

`BitMatrix` carries `ones: Option<u64>`. Chunk cardinalities make the exact count free for aligned database reads; mutating paths either update it or invalidate it. Equality deliberately ignores whether that cache is known.

### Arbitrary-precision integers

`BigUint` stores normalized little-endian `u64` limbs and carries no fixed width. `IntLayout` owns width and stride so arithmetic can grow naturally while storage remains bounded. Ordering must compare high limbs first; deriving order over the little-endian vector is silently wrong.

The production ladder is schoolbook multiplication, Knuth division, Barrett reduction, modular exponentiation, and Karatsuba above a measured threshold. Toom-3 and Burnikel-Ziegler were declined after measurement: their crossover lies beyond plausible indexed operands, so adding them would create public machinery without a workload.

Six arms beyond that ladder were deliberately deferred rather than left unconsidered, and each carries the reason it is not worth building yet, so none needs re-deriving. A squaring kernel is worth about a factor of two and owes its own measurement as a fourth arm. Montgomery reduction for odd moduli must be a **separate type and never a flag or a transparent switch**, because entering a value changes what the integer *means*, and a modular multiply that sometimes expects entered operands has no error path at all. A windowed modular exponentiation is a straightforward later refinement. Extended GCD and modular inversion need signed intermediates, and this representation deliberately has none. A Moller-Granlund reciprocal for the two-by-one division step is unattractive on the crate's minimum supported Rust: a 128-by-128 division lowers to a compiler support call rather than a hardware divide, because the language cannot express the precondition that the quotient fits in a machine word. Decimal formatting ships as repeated division by a power of ten, quadratic and documented as such, rather than divide-and-conquer.

If that ladder is ever resumed, division is the better target than multiplication. Division over multiplication at equal operand size measured 2.3x at 64 limbs and 4.6x at 1024, and the gap widens because Karatsuba pulled multiplication sub-quadratic while the schoolbook division stayed proportional to the product of both lengths. Extending the multiplication ladder therefore made division relatively worse.

The generic algorithms stay reachable as differential oracles. Rare Knuth and Barrett correction branches have constructed corpora and counters because uniform random inputs almost never exercise them. `num-bigint` is a dev-dependency oracle, not a runtime dependency.

### Shared packing

`pack/` owns the cross-chunk strided mechanics used by both matrix and integer sinks. The abstraction was justified by shared failure modes rather than line-count reduction: chunk straddles, one limb crossing a boundary, monotone placement, overflow at the ordinal ceiling, and the requirement to write only through canonical set operations.

`MatrixSink::layout()` and `IntSink::layout()` were unused public accessors and were **removed on 2026-09-06**, as their own diff. A caller already possesses the descriptor it passed to the sink, so handing it back was public API earning nothing. All three sinks now agree: `ViewSink` never had the accessor, and its comment records why. The class is what to carry forward — unused `pub` API compiles and `dead_code` deliberately does not fire on public items, so nothing mechanical sees this; it was found by auditing a new surface for callers by hand, which is the only instrument that catches it.

### Packed views

A view treats one set as `n` constituent sets over a shared logical ordinal space:

```text
Interleaved: physical(x, i) = x * n + i
Blocked:     physical(x, i) = i * stride + x
```

Selection extracts one constituent. `view_fold` reduces constituents using `Any`, `All`, or `Parity`; `view_expand` maps a coarse logical set back into all selected slots. Blocked layouts impose a real `x < stride` capacity constraint.

The algebra is asymmetric:

| Transform | Exact law |
|---|---|
| `Any` fold | union |
| `All` fold | intersection |
| `Parity` fold | symmetric difference |
| expansion | intersection, union, symmetric difference, and difference |

One-sided laws require strict counterexamples in tests. An inclusion-only assertion would also pass if an implementation accidentally claimed equality.

Flight descriptors carry validated `ViewSpec` values through versioned tickets. The server lowers transforms through audited eager `OrdSet` operations and re-enters the Boolean expression as a set leaf. Top-level view selection retains an exact non-materializing cardinality path. Lazy core expression nodes remain open only under the conditions recorded in `TODO.md`.

### Bit-sliced proposal and the surviving count

A bit-sliced value would read several ordinary sets as integer planes over each ordinal, the transpose of `bignum`'s significance-inner layout. **It was proposed by a consumer and declined in full; see the `view_count` discussion below for why.** The storage doctrine fits because the caller still owns the layout and each plane remains an ordinary equality-encoded key. The arithmetic does not license a lazy API shape: `matrix/`, `bignum/`, `view/`, and `pack/` are eager and contain no `Expr` or `ChunkStream` integration.

Subtraction at `L` planes must wrap modulo `2^L` and return the borrow-out set if the caller needs saturation. Saturation would disagree with a reader that can observe only the stored `L` bits, while one `Option` cannot identify which of many ordinals underflowed. A future `ge` can lower to existing Boolean expression operators without adding an `Expr` variant, but `top_k` still needs a separate comparison against materializing the tied set between planes.

The proposed carry-save motivation did not survive its benchmark as a reason to add a kernel:

```text
addends  levels    ripple   carry-save   measured   2L/5 model
     32       6   25.0 us      22.9 us      1.09x         2.4x
    128       8  114.1 us      90.8 us      1.26x         3.2x
    512      10  574.3 us     379.0 us      1.52x         4.0x
```

The frame is one 65,536-ordinal block in the consumer's `[u64; 1024]` plane buffers, not yesno containers. The corrected word-operation model still over-predicts time by two to three times. Its first, larger ratios used a word-major ripple baseline that blocked vectorization; a level-major baseline changed 128 addends from 348 us to 114 us. End to end, carry-save later measured 1.34x after a read bottleneck stopped masking it, still too small to carry a new public kernel and lens.

`OrdSet::view_count` was the last part standing, on a structural claim rather than a ratio: `view_fold` computes the per-ordinal count and `Reduce::keep` discards it to a Boolean, so the capability was said to be thrown away and immune to measurement. **Checked 2026-09-19, the premise is false and the part is declined.**

The count is already reachable from the shipped public API, by a ripple-carry add of each constituent's indicator into an accumulator of bit planes:

```rust
let mut planes: Vec<OrdSet> = Vec::new();
for i in 0..v.sets() {
    let mut carry = packed.view_select( v, i );
    for p in planes.iter_mut() {
        if carry.is_empty() { break; }
        let next = p.and( &carry );
        *p = p.xor( &carry );
        carry = next;
    }
    if !carry.is_empty() { planes.push( carry ); }
}
```

Verified at 8 constituents over a 200 000-ordinal span, both layouts: 154 285 logical ordinals' counts exact against a `BTreeSet`-style count oracle built from the constituents, and all three `Reduce` variants reproduced from the planes -- `Any` as their union, `Parity` as plane zero, `All` as the support constrained plane by plane to agree with `sets`.

What `view_fold` discards is therefore one **walk**, not the capability, which makes the ask a ratio: **4 - 8x on `Interleaved`** for a one-walk arm against the downstream construction, and **1x on `Blocked`**, where `fold_via_select` never forms a count at all and an in-crate implementation would be the loop above character for character. Three further findings survive the decision: the refusal of a `Reduce::Plane( j )` variant is correct because the enum is closed on **monoids** and `Plane( j )` for `j > 0` cannot be folded pairwise without carrying ( `Parity` escapes only because XOR is a monoid that happens to equal plane zero ); a `Vec<OrdSet>` return has a **data-dependent length**, since the natural construction yields one plane when no ordinal is held twice whatever `sets` is, so padding is an unaddressed specification decision; and the cohort-overlap use case actually wants a `count >= t` threshold, which the proposal itself observes dominates the planes and then declines to request. The decisive fact is that the proposer does not consume it -- unused public API is an R1 / R6 / R7 semver promise, and `stats.rs` is the recorded precedent.

## Files

- `yesno-core/src/matrix/` - packed matrix values, readers, algebra, and GF(2) operations.
- `yesno-core/src/bignum/` - unsigned arithmetic and ordinal-set integer lenses.
- `yesno-core/src/pack/` - shared strided packing primitives.
- `yesno-core/src/view/` - view layouts, selection, folds, and expansion.
- `yesno-core/src/view/fold.rs` - the count-producing walk currently reduced through `Reduce::keep`.
- `yesno-wire/` - dependency-free expression, view, and version-pinned request formats.
- `yesno-flight/` - wire lowering and eager view execution.
- `e2e/scenarios/{bitmatrix,bitmatrix_gf2,view,server_views}.py` - durable operational oracles.

## Test Coverage

- Matrix and integer kernels are checked against generic implementations and external dev-dependency oracles.
- Boundary cases cover matrices, integers, limbs, and view slots that straddle 16-bit chunks.
- E2E scenarios checkpoint, reopen, and compare against literal or Python-set expectations so packing is exercised through storage.
- View laws cover De Morgan duality, strict one-sided fold laws, the fold/expand adjunction, and expansion over every Boolean operator.
- Flight corruption tests cover every view tag and malformed descriptor; daemon scenarios cover both layouts before and after restart.

## Pitfalls

- Do not make storage acceptance depend on a value's highest set bit; it depends on the whole declared layout span.
- Do not derive `Ord` for little-endian limbs or `PartialEq` over an optional matrix count cache.
- Do not assume an aligned dense copy is the main matrix optimization; skipping irrelevant prefixes was the larger cost.
- Do not push folds through arbitrary Boolean expressions as if they were restrictions.
- Do not add an arithmetic rung because it exists in a mature library; measure it against reachable indexed operands.
- Do not add lazy view nodes without planner bounds, statistics, streaming cardinality, termination evidence, and an allocation-motivated workload.
- Do not use lane-level word-operation counts as time predictions or treat the withdrawn CSA and `Slice` parts as an implementation queue.
