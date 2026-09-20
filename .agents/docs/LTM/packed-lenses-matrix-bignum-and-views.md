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

#### Expression-level map and fold baseline ( 2026-09-20 )

The typed Flight expression evaluator has a measured bottleneck before any
Boolean terminal. `lower_vec` materializes every view constituent, then map
bodies and folds consume those sets. On an interleaved view, each
`view_select` walks the whole packed set; with fixed logical density the packed
cardinality itself grows with `sets`, so mapping over all constituents grows
close to quadratically.

Measured through the public `yesno_flight::expr` entry points at commit
`85a41c2bf34f7134df8eaf30a26265ceee6d6eba`, in release mode on the 20-core
Cortex-X925 / Cortex-A725 aarch64 host with rustc 1.97.1 and the repository's
pinned Arrow 59.2.0 graph. The fixture held 131 072 logical ordinals and 55%
density per constituent. Each row is the median of five batches; the complete
process was run twice and the medians below reproduced within 1%.

| constituents | interleaved | blocked aligned | ratio | interleaved allocations | blocked allocations |
|---:|---:|---:|---:|---:|---:|
| 4 | 4.72 ms | 1.39 us | 3 390x | 230 | 34 |
| 8 | 14.07 ms | 2.48 us | 5 675x | 449 | 57 |
| 16 | 47.12 ms | 4.90 us | 9 620x | 884 | 100 |

This is `map( view( P ), cardinality( _ ) )`. Incremental peak heap was
1.25, 1.32, and 1.45 MiB on the interleaved rows against 2.1, 3.8, and 7.3 KiB
on blocked. The 4 -> 8 -> 16 interleaved times scale 1 -> 2.98 -> 9.98 while
blocked scales 1 -> 1.78 -> 3.51. The shape, allocations, and heap growth agree
on the mechanism: repeated extraction dominates.

At eight dense interleaved constituents, changing the consumer did not change
the time or the 1.32 MiB peak:

| terminal | median | allocations |
|---|---:|---:|
| unfiltered cardinality map | 14.07 ms | 449 |
| cardinality map under a 50% filter | 14.06 ms | 561 |
| OR fold of that map | 14.07 ms | 698 |
| AND fold of that map | 14.10 ms | 794 |
| XOR fold of that map | 14.05 ms | 705 |
| Boolean membership map | 14.06 ms | 485 |
| one indexed element of the mapped vector | 14.04 ms | 608 |
| cardinality under a three-level body | 14.08 ms | 945 |

Filter selectivity at 10%, 50%, and 90% measured 14.09, 14.06, and 14.05 ms.
That flat line is the discriminating observation: neither result density,
terminal work, nor expression depth controls this frame. Even an index of one
mapped element first materializes all eight constituents.

The result holds across representations, but cardinality sets the absolute
cost. For eight constituents, unfiltered interleaved versus blocked cardinality
maps measured 39.5 us versus 2.53 us for 16 array containers, 8.32 ms versus
2.56 us for 8/16 run containers, and 14.07 ms versus 2.48 us for 16 bitmap
containers. After checkpoint, close, reopen, `verify`, and snapshot, dense
interleaved time stayed near 14 ms. The blocked eight-way row rose from 2.48 us
to 5.80 us and from 57 to 106 allocations, so stored decoding is visible only
after extraction stops dominating. The OS page cache remained warm; these are
reopened mmap results, not cold-storage latency.

Construction: dense membership was `mix64( x xor ( set << 40 ) ) % 100 < 55`;
the array fixture selected `( x + 37*set ) % 509 == 0`; the run fixture selected
`[0, 32768)` and `[65536, 98304)` in every constituent so the packed form stayed
run-encoded. Filters used independent `mix64` seeds. Both layouts held identical
logical constituents, blocked stride was 131 072, every result was checksumed
between resident and reopened snapshots, and an allocator positive control
observed exactly one 128 KiB vector allocation. The disposable harness lived at
`.agents-workspace/tmp/view-expr-baseline-20260920`; it used adaptive repetitions
targeting 25 ms per batch and black-boxed operands. Exact decoded-chunk counts
remain unmeasured because the public engine surface exposes no counter; adding a
production hook for a one-off question would violate the research-code policy.

This supplies the allocation-motivated workload the lazy-view backlog required,
but it does not by itself license a new core `Expr` node. The first target is a
fused evaluator path for batched cardinality, membership, indexed map, and
map-to-fold consumers. A lazy core node still owes stream statistics,
cardinality, seek behavior, planner bounds, and termination evidence.

#### Stage 2 terminal fusion ( 2026-09-20 )

The narrow evaluator paths were sufficient to remove repeated extraction from
all common measured terminals without adding a lazy view node to `Expr`. The
packed input remains one eager `OrdSet`; the consumer controls what happens
next:

- `At` descends through sequential maps and selects only the requested element.
- An unfiltered cardinality map uses `view_cardinalities`. Interleaved layouts
  assign every physical ordinal to its owner in one walk; blocked layouts retain
  scalar range counts, which sum whole containers without reading payloads.
- An intersection-shaped filtered cardinality map lowers invariant operands
  once as reopenable expressions. A monotone filter stream answers each logical
  ordinal once while the interleaved packed set assigns matches to row counters.
  The filter is not eagerly materialized.
- A membership map scalarizes `And`, `Or`, and `AndNot` at the requested ordinal.
  The hole is one direct `view_contains` probe per row and invariant branches
  are answered once.
- An intersection-shaped mapped fold uses distributivity for all three exact
  reductions: union, intersection, and symmetric difference. It folds the view
  once, then applies each invariant once.

Unsupported transforms retain `lower_vec`, and rank still selects one
constituent at a time while reusing lowered invariant work. That fallback is an
explicit boundary: the implementation does not claim that arbitrary map bodies
are vectorized.

The same release harness, fixture, host, repetitions, and resident snapshot as
the baseline measured:

| constituents | unfiltered before | unfiltered after | speedup | filtered 50% after | allocations unfiltered / filtered | peak heap unfiltered / filtered |
|---:|---:|---:|---:|---:|---:|---:|
| 4 | 4.72 ms | 0.646 ms | 7.3x | 1.753 ms | 13 / 20 | 2.1 / 2.1 KiB |
| 8 | 14.07 ms | 1.285 ms | 11.0x | 2.491 ms | 16 / 23 | 3.7 / 3.7 KiB |
| 16 | 47.12 ms | 2.576 ms | 18.3x | 3.655 ms | 19 / 26 | 7.0 / 7.0 KiB |

At eight dense interleaved constituents the terminal comparison is:

| terminal | before | after | speedup | allocations after |
|---|---:|---:|---:|---:|
| unfiltered cardinality map | 14.07 ms | 1.285 ms | 11.0x | 16 |
| cardinality under a 50% filter | 14.06 ms | 2.491 ms | 5.6x | 23 |
| cardinality under a three-level body | 14.08 ms | 2.490 ms | 5.7x | 71 |
| OR fold of the filtered map | 14.07 ms | 2.101 ms | 6.7x | 105 |
| AND fold of the filtered map | 14.10 ms | 1.691 ms | 8.3x | 87 |
| XOR fold of the filtered map | 14.05 ms | 1.914 ms | 7.3x | 89 |
| Boolean membership map | 14.06 ms | 1.46 us | 9 650x | 28 |
| one indexed filtered-map element | 14.04 ms | 1.701 ms | 8.3x | 93 |

The 10%, 50%, and 90% filtered cardinality rows are now 2.084, 2.491,
and 2.218 ms rather than the old flat 14 ms floor. The result still scales with
the packed input that must be visited, but it no longer multiplies that visit by
the number of rows. Array and run fixtures improved consistently: the eight-way
filtered interleaved rows moved from 42.5 us to 7.74 us and from 8.37 ms to
1.51 ms. Checkpoint, close, reopen, verify, and a fresh snapshot produced the
same checksums and near-identical interleaved times. Reopened allocation counts
include the stored key source plan but remain independent of constituent count.

A dedicated allocation regression compares 4 and 64 constituents for direct
and filtered cardinality maps, mapped folds, membership maps, and indexed maps.
Every path may grow only by the output collection's 16-allocation allowance;
the pre-change implementation failed at 192 -> 2 200, 175 -> 2 003, and
197 -> 2 213 allocations for indexed map, direct cardinality, and membership.
#### Stage 3 pointwise Boolean terminal fusion ( 2026-09-20 )

The remaining measured cardinality and rank maps were pointwise Boolean
functions of one constituent hole. They do not require a lazy view node. For a
body `f`, evaluate its invariant expression tree twice, with the hole empty
(`f0`) and with the hole equal to the ordinal universe (`f1`), then use:

```text
|f(H)| = |f0| + |H intersect (f1 minus f0)| - |H intersect (f0 minus f1)|
```

Invariant leaves are lowered once. One interleaved packed walk advances
monotone streams for the positive and negative filters and updates every
constituent's two counters. Rank applies the same identity below its strict
upper bound and stops the packed walk when logical order reaches that bound.
Union, both difference directions, repeated uses of the hole, and static bodies
therefore share one exact path. Non-pointwise transforms decline the
substitution and retain the eager fallback; blocked views retain their
near-free contiguous-selection fallback.

Before this stage, the extended baseline measured union, both differences, a
repeated-hole body, and rank over union at about 4.75 / 14.1 / 46.7 ms for
4 / 8 / 16 constituents. They allocated 272-371 / 527-718 / 1,034-1,409 times
and peaked near 1.2 MiB because every constituent was extracted. After the
pointwise path, representative resident medians on the same fixture were:

| terminal | 4 constituents | 8 constituents | 16 constituents | allocations at 4 / 8 / 16 | peak heap at 16 |
|---|---:|---:|---:|---:|---:|
| union cardinality | 2.128 ms | 3.027 ms | 4.470 ms | 72 / 75 / 78 | 19.6 KiB |
| repeated-hole cardinality | 2.228 ms | 3.113 ms | 4.525 ms | 160 / 163 / 166 | 37.5 KiB |
| union rank below the midpoint | 1.062 ms | 1.498 ms | 2.213 ms | 103 / 106 / 109 | 19.9 KiB |

The two difference directions landed in the same 2.13-4.58 ms envelope.
Checkpoint, close, reopen, verify, and a fresh snapshot produced identical
checksums and comparable times. The allocator positive control still observed
one 128 KiB vector allocation. A 4-versus-64 allocation regression now covers
union cardinality, repeated-hole cardinality, and rank; bypassing the fused
path made it fail at 209 -> 3,225, 302 -> 4,386, and 221 -> 3,161 allocations.
An independent `BTreeSet` oracle covers both layouts and strict rank
boundaries; deliberately reversing the positive and negative terms made its
first union case fail.

This closes every pointwise Boolean cardinality and rank workload measured so
far without changing `yesno-core::Expr` or its planner proof. The backlog stays
partial only for genuinely non-pointwise map bodies. A future core view node
still needs an allocation-motivated caller plus planner bounds, statistics,
streaming cardinality, seek behavior, and a termination measure.

#### Stage 4 general pointwise mapped folds ( 2026-09-20 )

A fold of `map( view, f( _ ) )` needs no constituent extraction when `f` is
pointwise. At one ordinal let `f0` and `f1` be the body's results with its hole
absent and present, and let `Any`, `All`, and `Parity` be the corresponding
packed-view folds. The exact set identities are:

```text
OR  = ( f0 minus All ) union ( f1 intersect Any )
AND = ( f0 intersect f1 ) union ( f0 minus Any ) union ( f1 intersect All )
XOR = Parity intersect ( f0 xor f1 )                       when arity is even
XOR = f0 xor ( Parity intersect ( f0 xor f1 ) )            when arity is odd
```

The OR and AND forms distinguish the three possible row states: every hole is
absent, every hole is present, or the holes are mixed. XOR follows by writing
each mapped bit as `f0 xor ( hole intersect ( f0 xor f1 ) )`; XOR of the `f0`
copies survives exactly at odd arity. Flight derives `f0` and `f1` from the
already prepared expression tree, runs the required packed folds, and leaves
the remaining set algebra lazy. The older intersection-only specialization
stays first because distributivity answers that case with one packed fold rather
than the general OR/AND path's two.

The same verified release fixture measured union, invariant-minus-hole, and
repeated-hole bodies across all three reductions. Before this stage they shared
the extraction floor: 4.78-4.83 ms at 4 constituents, 14.33-14.44 ms at 8,
and 47.12-47.59 ms at 16, with 354-1,852 allocations and 1.25-1.45 MiB peak
requested heap. After fusion, representative resident medians were:

| reduction | 4 constituents | 8 constituents | 16 constituents | allocations at 16 |
|---|---:|---:|---:|---:|
| OR, across the three bodies | 2.368-2.379 ms | 3.778-3.785 ms | 6.080-6.099 ms | 131-193 |
| AND, across the three bodies | 2.371-2.402 ms | 3.776-3.783 ms | 6.081-6.085 ms | 151-253 |
| XOR, across the three bodies | 1.166-1.179 ms | 1.860-1.870 ms | 3.037-3.040 ms | 114-162 |

Resident and checkpoint/reopen checksums agreed, and the database was verified
before reopened measurement. OR and AND still peak near 1.2 MiB because their
general identities retain both `Any` and `All` results; XOR needs only parity
and peaked near 0.65 MiB at 16 constituents. That residual heap is independent
of constituent extraction and should not motivate a core node without a caller
that demonstrates it matters.

An independent `BTreeSet` oracle substitutes each constituent directly and
reduces it without sharing the truth-table derivation. It covers union, both
difference directions, a static body, and a repeated hole; OR, AND, and XOR;
odd and even arities; and both layouts. Deliberately swapping the OR identity's
mixed-state terms made the first union case fail. The allocation regression
compares 4 with 64 constituents for nine body/operator combinations; bypassing
the general fusion made its first case grow from 268 to 4,195 allocations.

The only known eager mapped-fold remainder is now genuinely non-pointwise, such
as `select( _, n )`. Its result depends on global order statistics and cannot be
recovered from the hole's per-ordinal false/true values. Keep that fallback and
the lazy-core-node planner obligations until a measured caller justifies more.

#### Stage 5 direct mapped-select folds ( 2026-09-21 )

The non-pointwise remainder became measured: a midpoint
`fold( map( view, select( _, n ) ), op )` on the same 131,072-ordinal,
55%-dense interleaved fixture shared the old extraction floor. OR, AND, and XOR
all took about 4.79 / 14.31 / 47.40 ms at 4 / 8 / 16 constituents, with
308-347 / 611-715 / 1,214-1,374 allocations and 1.25 / 1.32 / 1.45 MiB peak
requested heap. A disposable implementation established the attainable shape
before production changed: one physical walk, one counter and selected ordinal
per constituent, then a reduction of at most one ordinal per constituent.

Flight now recognises the exact direct body `select( _, n )`. It maps every
physical ordinal through the view's existing `logical_of` oracle, advances only
that owner's counter, and stops early once every constituent has selected a
value. OR deduplicates the selected values, AND retains a singleton only when
every constituent selected the same value, and XOR retains values with odd
multiplicity. This works for both layouts without copying view-addressing
arithmetic into Flight.

The resident production medians became about 0.371 / 0.741 / 1.49 ms at
4 / 8 / 16 constituents across the three reductions, with 15-39 allocations
and 2.1-7.1 KiB peak requested heap. Reopened medians were comparable and every
checksum matched the resident result. At 16 constituents this is about 32x
faster than the eager path and removes roughly 1.44 MiB of transient heap.

An independent `BTreeSet` oracle covers common, distinct, and missing selected
values, selection indices at and beyond each constituent's length, both
layouts, odd and even arities, and all three reductions. Changing the
zero-based comparison made its first OR case return empty instead of `{4}`.
The 4-versus-64 allocation regression failed at 275 -> 3,490 allocations when
the fusion was bypassed.

No measured mapped-fold workload now requires constituent extraction. The
backlog remains partial only for selection over an arbitrary transformed hole,
whose ordering can depend on invariant sets and has no measured caller. That
narrow remainder does not justify a core view expression node or any change to
the planner proof.

#### Stage 6 composed cardinality-map normalization ( 2026-09-21 )

An external expression-math study found that an equivalent expression spelling
could still force the eager vector fallback:
`map( map( V, f( _ ) ), cardinality( _ ) )`. The direct spelling
`map( V, cardinality( f( _ ) ) )` already reaches the stage 2-3 terminal
fusions. On a 4,096-feature interleaved fixture with exactly 24 features per
document, evaluating two count vectors through the nested spelling took
57.28-59.26 microseconds at 16 documents, 567.15-573.57 at 64, and
7,535.73-7,553.95 at 256. Normalizing first and calling the unchanged evaluator
took 8.34-8.53, 25.60-28.32, and 75.47-79.04 microseconds respectively.

Flight now moves only an identity `cardinality( _ )` terminal through one
set-map binding and redispatches the resulting existing expression. It does not
substitute an arbitrary cardinality operand, extend the wire syntax, or add a
kernel. Repeated composition removes one map layer per recursive dispatch and
therefore terminates structurally.

Independent `BTreeSet` coverage compares direct and nested spellings for union,
both difference directions, static and repeated-hole bodies, both layouts, and
an empty constituent. A separate binding-scope case keeps an additional outer
intersection; deliberately broadening the match returned `[8, 8, 5]` instead
of `[5, 4, 3]`. The allocation regression pairs direct and nested forms at
4 and 64 constituents. Before normalization the direct forms allocated 27 / 27
times while the nested forms allocated 256 / 3,604; after normalization the
fixed allowance passes.

This closes a composition reachability gap, not an execution-kernel gap. The
prescription's native-container and query-support-driven count traversal remains
separate future work with much broader core, persistence, dispatch, and
mixed-container acceptance obligations.
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
