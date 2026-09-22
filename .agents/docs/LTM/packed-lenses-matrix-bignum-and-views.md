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

#### Stage 8 candidate: query-driven native intersection counts ( 2026-09-21 )

The direct intersection-count terminal still has two algorithmic gaps after
normalization. Under `Interleaved` it materializes the packed input and visits
every stored physical ordinal, asking a monotone filter stream once per logical
ordinal whether that row is selected. Under `Blocked` the specialised terminal
declines entirely, so evaluation extracts every constituent and counts its
intersection separately. Neither cost is inherent to the requested count.

A disposable reference was switched from its pinned evaluator copy to the
current `epic` `yesno-flight::expr::vec_int`, then run on one Cortex-X925
performance core. Seven alternating repetitions performed three evaluations
each and checked complete vectors against independent `BTreeSet` intersections.
The single-terminal arm used one query plane; its native control deliberately
retained the reference's second empty output vector and cloned the wanted vector,
so it slightly penalises the proposed side:

```text
corpus and layout                         support       current       native snapshot    ratio
sparse24, 4096-way interleaved                 32      314.36 us          26.66 us       11.8x
sparse24, 4096-way interleaved               4096      450.12 us         212.23 us        2.1x
dense50, 512-way blocked bitmap                32       20.61 ms          16.95 us     1215.6x
runs512, 512-way blocked run                    32        5.02 ms          11.75 us      427.5x
```

The interleaved construction inverts the current walk. Logical feature `x`
occupies the contiguous physical row `[x*N, (x+1)*N)`. Sorted query rows become
coalesced prefix windows; a direct key input can feed those windows through
`Snapshot::key_stream_prefix_range`, and each array, bitmap, or run payload is
clipped to the selected row before assigning hits to owner counters. The
32-feature case therefore avoids both unrelated payloads and unrelated ordinals.
The full-support row is only 2.1x faster and the earlier 512-document control
showed a shared full scan beating selective resident traversal, so query support
and addressed-prefix coverage must choose between selective and full-scan arms.

Blocked data needs a different native traversal. Arrays assign their stored
positions once, aligned bitmap rows use AND-plus-popcount against the query mask,
and runs split only at document-row boundaries and count selected query values by
lower-bound differences. That preserves the representations rather than
enumerating bitmap bits or run ordinals and explains the three-order-of-magnitude
results. Unaligned rows and unaligned store-backed bitmap buffers still need an
exact generic fallback.

This should remain one existing cardinality-map terminal, not a new wire opcode
or core expression node. The reusable core boundary is a checked chunk-count
accumulator that resident `OrdSet` chunks and Flight's persisted streams can both
feed; Flight owns source recognition, bounded stream orchestration, and dispatch.
Start with the measured direct intersection shape and retain the current walk as
its oracle. General pointwise positive/negative filters may reuse the helper only
after separate evidence. Sharing two sibling count terminals is another API
decision and is not required for the single-terminal gains above.

#### Stage 8a landed: scalar blocked bitmap intersection counts ( 2026-09-21 )

The first blocked arm is deliberately scalar. Flight now recognises the same
direct intersection-count terminal for a blocked view, materialises its invariant
filter once, and asks core for the complete count vector. Core builds the query's
word mask once when the row stride is word-aligned and exactly tiles a 65,536-bit
chunk. Each bitmap payload is then partitioned into integral rows and counted by
scalar word-wise AND-popcount. `BitmapContainer::words` keeps store-backed
unaligned payloads correct by copying only when alignment requires it. Arrays,
runs, mixed tails, non-tiling rows, and interleaved views retain one exact ordinal
walk through `View::logical_of`; no constituent `OrdSet` is constructed.

On the same 512-row, 4,096-bit dense blocked fixture, the production Flight
terminal moved from a 20.61 ms median to 13.47 us, about 1,530x. The checked-in
core benchmark compares the grouped arm with public `view_select` plus
`and_cardinality`: 5.329 us against 16.126 ms, about 3,027x. The production path
slightly beats the disposable 16.95 us control because that control retained an
unused sibling vector and cloned its result. The generic run fallback also moved
from about 5.02 ms to 1.27 ms by replacing per-constituent extraction with one
packed walk, but it is not the proposed interval-rank kernel and must not be
reported as closing that work.

The core property uses 17 dense rows to force a bitmap first chunk and a partial
non-bitmap tail, varies data and query phases, and checks ordinary, empty, and
full filters against independent `BTreeSet` intersections. Flight repeats an
independent oracle before checkpoint and after reopen. The allocation regression
holds four physical chunks constant while changing 4 rows to 64; bypassing the
new path measured 72 -> 2,256 allocations and failed. Shifting the bitmap owner
by one made both semantic oracles fail, proving they reach the row assignment.

Stage 8 remains partial. Interleaved query-driven bounded streams, native array
assignment, native run interval ranks, selective/full-scan dispatch, sibling
sharing, and SIMD are separate measured decisions. A later bitmap SIMD arm can
replace only the row reduction inside this checked scalar structure and must
retain the same oracle and fallback boundaries.

#### Stage 8 completed: bounded native counts and sibling sharing ( 2026-09-21 )

`ViewIntersectionCounter` is now the single checked chunk accumulator for
resident and persisted intersection counts. It requires strictly ascending
prefixes, exposes coalesced prefix windows for selective interleaved traversal,
and returns filter-major vectors. Rejecting repeated prefixes prevents two
overlapping windows from silently counting one payload twice.

For an interleaved view, each requested logical ordinal becomes its checked
physical row. Sparse query support opens only the prefix windows containing
those rows and clips array, bitmap, and run containers at both row and chunk
boundaries. Array visits start and end at binary partitions, run visits seek to
the first overlapping interval, and bitmap visits mask the boundary words. A
support upper bound below half the stored logical extent selects this arm.
Broader support takes one full source scan and caches each filter's membership
once per logical row, rather than once per present constituent.

Blocked traversal keeps the Stage 8a scalar bitmap arm, assigns array values in
one pass, and adds the native run arm: a physical run is split only where it
crosses a constituent boundary, then `len_in_range` counts the filter over the
corresponding logical interval. Dense blocked filters do not build the
interleaved-only selected-row metadata.

The resident `OrdSet::view_intersection_cardinalities_batch` method and Flight's
`vec_int_batch` entrypoint share one source traversal across several filters.
The latter is deliberately an in-process API rather than a new wire opcode;
when siblings do not name the same direct key-backed view and intersection
shape, it falls back to the scalar evaluator for each expression.

On the same oracle-checked fixtures used to prescribe the work, pinned to
Cortex-X925 CPU 5, final one-terminal production medians were:

```text
corpus and layout                         support       before       final       ratio
sparse24, 4096-way interleaved                 32     314.36 us    33.30 us        9.4x
sparse24, 4096-way interleaved               4096     450.12 us   391.58 us        1.15x
dense50, 512-way blocked bitmap                32      20.61 ms    13.14 us     1568x
runs512, 512-way blocked run                    32       5.02 ms    15.66 us      320x
```

The sparse case retains expression-filter preparation that the native reference
did not time, explaining the remaining 33.30 versus 26.26 us difference. Full
support likewise includes rebuilding its literal filter on every evaluation;
the traversal itself no longer repeats membership per owner.

An explicit NEON AND-popcount row reducer was rejected after measurement. On
the checked-in 512 by 4,096 blocked bitmap benchmark it took 6.478 us versus
5.605 us after restoring scalar word popcount, a 15.6% regression. The measured
row contains only 64 words, so vector setup and byte-horizontal reduction cost
more than the compiler's scalar popcount sequence. No new unsafe block remains.
This result does not decide the separate Stage 7 bitmap-native folds, whose
larger algorithmic gain comes from ceasing to enumerate set bits.

The regression layers are independent: a boundary-biased `BTreeSet` property
spans odd interleaved arities and sparse/full dispatch; forced core tests cover
array, bitmap, and run containers and both strategies; Flight checks resident
and reopened bounded streams; and the allocation suite requires a sibling batch
to allocate less than two independent evaluations. Shifting selective clipping
by one ordinal changed owner zero from 100 to 0 in the property and failed on
its first generated case, after which the correct boundary was restored.

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

### Scratch JIT count terminal over an interleaved mapped view ( 2026-09-22 )

A standalone research binary at
`.agents-workspace/tmp/simd-jit-revisit-20260921/src/bin/view_fold.rs`
tests cardinality( fold( map( view( packed ), identity ), Reduce ) ).
The identity map is intentional: it isolates the first materialization
boundary, rather than claiming general map-body compilation. Four
interleaved constituents span 262,144 logical ordinals at 55% density,
giving 16 aligned bitmap containers of 1,024 words each. Every Any, All,
and Parity count is checked against shipped `view_fold( .. ).len()`.
The JIT borrows words through `unstable_arrow::bitmap_words`, compiles a
128-bit pairwise-popcount loop once per reduction, and counts directly
without constructing the folded OrdSet. A Rust/LLVM AOT arm implements
the same word formula. Both timed arms black-box input operands. Nine
alternating batches of 16 whole-corpus calls per arm yield medians; four
complete runs of the unchanged anti-hoisting binary gave:

| Reduce | shipped fold then count | AOT count terminal | vector JIT terminal |
|---|---:|---:|---:|
| Any | 2.690-3.016 ms | 4.96-4.99 us | 5.32-5.33 us |
| All | 2.072-2.109 ms | 4.93-4.95 us | 5.31-5.33 us |
| Parity | 2.423-2.435 ms | 4.94-4.98 us | 5.32-5.33 us |

The initial scalar Cranelift loop was about 11.9 us for every reduction;
the vector lowering cut it to about 5.3 us, within 7-8% of this AOT arm.
Compilation took about 120-421 us after process warm-up, with a 1.2 ms
first compiler initialization observed once. These are count-only terminal
comparisons: the shipped arm must construct a result set, whereas both
research arms avoid it because the requested terminal is cardinality.
The experiment does not time Flight lowering, a nontrivial map body,
mixed container kinds, unaligned mmap payloads, blocked layouts, or
materialized fold output. It therefore supports extending the prototype
to a representative non-identity map and exact fallback cases; it does
not justify a production JIT dispatch yet.

### Persisted SIMD terminals retain the kernel win ( 2026-09-23 )

The production Flight evaluator reaches the bitmap SIMD arms after an eager
boundary: a key expression is collected into one packed `OrdSet`, then direct
folds and the pointwise-map truth-table reduction call `view_fold`, while an
identity cardinality map calls `view_cardinalities`. A streaming fold
accumulator was still an open possibility because that first collection could
have hidden the resident-kernel gain. It does not on the measured dense shape.

A one-off crate under
`.agents-workspace/tmp/persisted-view-simd/` uses the checked-in Stage 7
corpus: 16 contiguous physical bitmap chunks over 1,048,576 positions, 55%
membership from LCG seed `0x2545_f491_4f6c_dd1d`. It checkpoints the key,
drops the database, reopens it, and asserts that all 16 recovered containers
lend aligned bitmap words. The public Flight evaluator answers direct folds,
`map( view( key ), cardinality( _ ) )`, and
`fold( map( view( key ), or( _, range ) ), op )` for arities 2/4/8. The
mapped-union body is representative rather than synthetic identity: OR and AND
use both Any and All folds; XOR uses Parity.

Runs were pinned to Cortex-X925 CPU 2. Each row is the median of seven batches
of 100 calls after ten warm calls. Two scalar and two agreeing NEON runs were
rotated around rebuilds; the scalar build disabled only the two AArch64 SIMD
dispatch branches and fell through to the shipped Stage 7a/7b scalar kernels.
Those branches were restored before verification. One extra NEON run was kept
because an isolated resident arity-8 Any row read 142 us while its persisted
row in the same run stayed at 106.8 us; the extra run reproduced 100.8 and
106.8 us respectively. Raw runs and the comparison are retained beside the
scratch crate.

Collecting the reopened dense bitmap key costs only 5.53-5.78 us. It is visible
as an approximately 6 us additive cost on direct persisted folds and is too
small to justify a second streaming fold implementation for this shape:

| Terminal | Arity | scalar persisted | NEON persisted | gain |
|---|---:|---:|---:|---:|
| mapped cardinalities | 2 | 46.85 us | 14.64-14.86 us | 3.18x |
| mapped cardinalities | 4 | 93.57-93.60 us | 23.45-23.52 us | 3.98x |
| mapped cardinalities | 8 | 114.07-114.13 us | 49.78-49.88 us | 2.29x |
| direct Any fold | 2 / 4 / 8 | 88.60 / 82.94 / 165.59-165.68 us | 36.22-36.35 / 31.42-31.56 / 106.77-106.79 us | 2.44 / 2.63 / 1.55x |
| direct All fold | 2 / 4 / 8 | 88.46 / 83.07-83.08 / 82.05-82.09 us | 36.22-36.26 / 31.55-31.56 / 22.74-23.03 us | 2.44 / 2.63 / 3.59x |
| direct Parity fold | 2 / 4 / 8 | 88.59-88.71 / 82.85-82.89 / 80.69-80.75 us | 36.33-36.63 / 31.36-31.43 / 23.48-23.52 us | 2.43 / 2.64 / 3.44x |

Every nested mapped-union row also wins: 1.80-2.46x at arity 2,
2.25-2.71x at arity 4, and 1.86-3.21x at arity 8. These are complete reopened
Flight evaluator calls, not a resident-kernel projection. Therefore the
persisted measurement closes the dense-bitmap SIMD slice without a new stream
API: the existing eager packed-set boundary preserves the SIMD benefit, and
the remaining materialization prize on this workload is about 6 us rather than
the 80-250 us vector work it would duplicate. It does not settle sparse array
inputs; the following measurement does.

### Sparse persisted terminals need an encoding-aware plan ( 2026-09-23 )

The same scratch crate compares the current public Flight evaluator with a
one-pass prototype over `Snapshot::key_stream`. The prototype never constructs
the packed input `OrdSet`: one arm accumulates constituent cardinalities while
the other groups adjacent interleaved ordinals and constructs only the final
Any, All, or Parity result. Every fixture is checkpointed, dropped, reopened,
and verified to contain only Array containers. Results agree with
`OrdSet::view_cardinalities` and `OrdSet::view_fold` before timing.

Runs were pinned to Cortex-X925 CPU 2. Each row is the median of seven batches
after five warm calls. The compact 16-chunk, wide 256-chunk, and one-value
4,096-chunk anchors were repeated in two complete runs; the wider crossover
sweep was one complete run. Ratios below are current eager time divided by
streaming time, so values above one favour streaming:

| occupied chunks | values per chunk | collect / scan | mapped count | Any fold | All fold | Parity fold |
|---:|---:|---:|---:|---:|---:|---:|
| 16 | 1 | 1.33x | 1.33x | 1.31x | 1.40x | 1.32x |
| 64 | 1 | 1.42x | 1.39x | 1.32x | 1.42x | 1.34x |
| 256 | 1 | 1.33x | 1.32x | 1.24x | 1.33x | 1.27x |
| 256 | 8 | 1.09x | 1.08x | 1.05x | 1.08x | 1.07x |
| 256 | 64 | 1.08x | 1.05x | 1.01x | 1.07x | 1.04x |
| 256 | 512 | 1.10x | 1.03x | 0.99x | 1.02x | 0.99x |
| 1,024 | 1 | 1.33x | 1.32x | 1.25x | 1.35x | 1.27x |
| 1,024 | 8 | 1.08x | 1.08x | 1.05x | 1.08x | 1.07x |
| 4,096 | 1 | 1.30x | 1.30x | 1.23x | 1.31x | 1.25x |

The hypothesis therefore holds conditionally. The predictor is not global
density or prefix span; it is cardinality per occupied chunk, which also
predicts the container encoding, plus enough chunks for the absolute saving to
matter. Exactly one value per occupied chunk saves 24-42% across the measured
span. Eight values per chunk saves only 5-8%, and by 512 the fold difference is
noise or a slight streaming loss. At 16 chunks the strongest ratio is still
only about one microsecond in absolute terms.

The planner need not guess these quantities. `ChunkSource` already exposes
exact occupied chunk count, cardinality, span, and an all-bitmap hint for keyed
sources; `KeyStream` exposes exact chunk and cardinality information before
payload decoding. A production route should preserve the bitmap-native SIMD arm for
dense inputs and choose a core-owned streaming accumulator only for a measured
sparse region. A conservative first admission is many occupied chunks with
about one value per chunk; admitting the eight-value rows buys too little to
justify a parallel semantic implementation without further evidence. Direct
key intersection-cardinality maps already stream through
`ViewIntersectionCounter`; the remaining eager cases are the identity
cardinality map and direct materializing folds.

The conservative arm landed without a core expression node. Core now owns
checked stream terminals for interleaved identity cardinalities and
Any/All/Parity folds. Flight admits only a valid interleaved descriptor with at
least 64 occupied chunks and exact source cardinality equal to chunk count,
which proves one value per occupied chunk. A declined direct-key query
materializes the already-open `KeyStream`, preserving both source-plan
economy and the dense bitmap SIMD path. The public allocation regression
detects rebuilding the packed input; forcing that regression through the eager
path increased requested bytes from 74,224 to 123,152 on 256 singleton chunks.

On the same pinned production harness, admitted public calls at 64-4,096
singleton chunks were within about 6-8% of the standalone stream reference.
The 16-chunk case remained eager and retained its approximately 28% gap, which
confirms the threshold is active rather than the measurement collapsing.

### Bounded rank is a range plan, not a sparsity decision

A direct identity rank over an interleaved persisted view has a stronger bound
than full cardinality or fold: only physical ordinals below `upper * sets` can
contribute. Flight now ceil-divides that endpoint into a bounded prefix range,
with `u128` arithmetic and a cap at the legal exclusive prefix `2^48`. Core
counts every complete streamed chunk through the existing per-owner
cardinality reducer and maps ordinals only in the one possible partial final
chunk. The plan is valid for array, bitmap, and run containers at any density;
it deliberately does not use the singleton-chunk admission rule above.

Pinned pre-change singleton measurements for 16, 256, 1,024, and 4,096
occupied chunks were 4.778-4.793, 45.33-45.67, 173.00-173.80, and
674.43-682.61 microseconds. A bounded streaming reference measured
1.631-1.801, 17.30-20.40, 66.64-79.33, and 267.20-317.65 microseconds. After
the production plan landed, the same public rank terminal stayed within about
1-6% of the reference, including denser array rows. The useful planner signal
is therefore the strict range endpoint, not estimated output sparsity.

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
