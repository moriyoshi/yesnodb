# Packed Lenses: Matrices, Integers, and Views

## Summary

`matrix/`, `bignum/`, and `view/` reinterpret one `OrdSet` through caller-owned affine layouts instead of introducing new storage formats. The shared principle is that the durable object remains an ordinal set; the lens supplies shape, packing, and algebra, while explicit sinks write the result back through ordinary set operations.

## Key Facts

- `BitMatrix` is a dense packed value over Boolean or GF(2) operations; it is not a sparse matrix store.
- `BigUint` is an unsigned little-endian limb value. A set **is** an integer and width is a reader's argument; the `IntLayout` descriptor and the `IntSink` builder were removed on 2026-09-25 for having no caller.
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

`BigUint` stores normalized little-endian `u64` limbs and carries no fixed width. Ordering must compare high limbs first; deriving order over the little-endian vector is silently wrong.

**The stored form carries no descriptor either, since 2026-09-25.** An `IntLayout { width_bits, stride }` used to make one set a *series* of integers. Nothing packed a series: every constructed layout was dense, and the padded spellings had no caller outside tests asserting their own arithmetic. A set is now one integer, several integers are several keys, and width appears only where a reader chooses to read fewer bits than are there -- which is exactly `x mod 2^width`. Removing the stride also removed the straddling hazard rather than relocating it: with every value based at ordinal zero and a chunk being exactly 1 024 limbs, no limb crosses a chunk boundary.

The production ladder is schoolbook multiplication, Knuth division, Barrett reduction, modular exponentiation, and Karatsuba above a measured threshold. Toom-3 and Burnikel-Ziegler were declined after measurement: their crossover lies beyond plausible indexed operands, so adding them would create public machinery without a workload.

Six arms beyond that ladder were deliberately deferred rather than left unconsidered, and each carries the reason it is not worth building yet, so none needs re-deriving. A squaring kernel is worth about a factor of two and owes its own measurement as a fourth arm. Montgomery reduction for odd moduli must be a **separate type and never a flag or a transparent switch**, because entering a value changes what the integer *means*, and a modular multiply that sometimes expects entered operands has no error path at all. A windowed modular exponentiation is a straightforward later refinement. Extended GCD and modular inversion need signed intermediates, and this representation deliberately has none. A Moller-Granlund reciprocal for the two-by-one division step is unattractive on the crate's minimum supported Rust: a 128-by-128 division lowers to a compiler support call rather than a hardware divide, because the language cannot express the precondition that the quotient fits in a machine word. Decimal formatting is **deferred, not shipped** ( corrected 2026-09-25: this line previously said it ships ). `BigUint` renders as hexadecimal, and `bignum/query.rs` records why -- base 10 needs repeated division, so it belongs with the divider rather than with the predicates. If it lands it is repeated division by a power of ten, quadratic and documented as such, rather than divide-and-conquer.

If that ladder is ever resumed, division is the better target than multiplication. Division over multiplication at equal operand size measured 2.3x at 64 limbs and 4.6x at 1024, and the gap widens because Karatsuba pulled multiplication sub-quadratic while the schoolbook division stayed proportional to the product of both lengths. Extending the multiplication ladder therefore made division relatively worse.

The generic algorithms stay reachable as differential oracles. Rare Knuth and Barrett correction branches have constructed corpora and counters because uniform random inputs almost never exercise them. `num-bigint` is a dev-dependency oracle, not a runtime dependency.

### Shared packing

`pack/` owns the cross-chunk strided mechanics used by both matrix and integer sinks. The abstraction was justified by shared failure modes rather than line-count reduction: chunk straddles, one limb crossing a boundary, monotone placement, overflow at the ordinal ceiling, and the requirement to write only through canonical set operations.

`MatrixSink::layout()` and `IntSink::layout()` were unused public accessors and were **removed on 2026-09-06**, as their own diff ( `IntSink` itself followed on 2026-09-25 ). A caller already possesses the descriptor it passed to the sink, so handing it back was public API earning nothing. All three sinks now agree: `ViewSink` never had the accessor, and its comment records why. The class is what to carry forward — unused `pub` API compiles and `dead_code` deliberately does not fire on public items, so nothing mechanical sees this; it was found by auditing a new surface for callers by hand, which is the only instrument that catches it.

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

### Bitmap-native interleaved terminals: scalar and SIMD ( 2026-09-22 to 2026-09-23 )

For interleaved views of arity 2, 4, or 8, `view_cardinalities` counts fixed owner masks over borrowed bitmap words, and `view_fold` reduces Any, All, or Parity with a byte lookup and packs `n` physical chunks into one logical output chunk. The fold caches exact output cardinality and optimizes the result. Mixed Array/Run chunks, unaligned shared bitmap buffers, unsupported arities, big-endian hosts, and missing CPU features retain the generic or scalar path. The aligned bitmap preflight is essential: an unaligned buffer must be decoded by the generic path.

The checked-in view benchmarks use 16 contiguous Bitmap containers over 1,048,576 physical positions, with 55% deterministic LCG membership ( seed `0x2545_f491_4f6c_dd1d` ). They time complete public terminals, including count-vector allocation or fold-result construction. On a Cortex-X925 pinned to CPU 2, the scalar cardinality path reduced the grouped-walk median from about 2.59 ms to 41.15, 87.83-87.96, and 108.34-108.39 us for arities 2/4/8. Scalar folds improved about 18.8-57.1x over grouped walks. The later NEON arm beat the scalar floor in all twelve measured count and fold rows: counts reached 8.98-9.00, 17.77-17.78, and 44.11-44.14 us; fold gains ranged from 1.62x ( eight-way Any, where dense output construction dominates ) to about 4.5x. These are resident-set AArch64 measurements, separate from persisted Flight results below.

Native Intel i9-9880H measurements of the same fixture compared separate scalar and x86 SIMD builds, rotating scalar/vector/vector/scalar. Both binaries were invoked directly so `cargo bench` could not silently rebuild away the scalar-only cfg. All twelve public rows improved: count gains were 6.78-8.83x and fold gains 4.98-20.49x. Eight-way Any remained the slowest relative fold at 4.98x. macOS offered no CPU affinity, so those runs are weaker evidence than the pinned AArch64 figures and imply no cross-architecture speed ranking.

The x86 count arm uses AVX2 because its `psadbw` accumulation needs no cross-lane fold until the end; fold arms stay 128-bit because AVX2 lane shuffles would require extra permutes. Pre-shifted nibble tables and `pmaddubsw`/`pmaddwd` replace NEON's per-byte variable shifts; `pmovmskb` makes the eight-way x86 fold especially cheap. NEON accumulation uses bounded u16 pairwise lanes, while x86 `psadbw` accumulates in 64-bit lanes. SIMD loads and stores are bounded by complete 1,024-word input and exact 1,024/n-word output slices.

Independent `BTreeSet` properties force bitmap chunk seams and the last legal prefix, plus mixed-kind and genuinely unaligned fallback. Direct scalar-versus-vector differentials use zero, full, one-hot seam, periodic, and dense words for every arity and reduction. Deliberate owner-mask, gather, and reduction mutations failed those tests. A mutation of an unreachable nibble-table lane did not fail; masked owner nibbles cannot reach all table entries, so a green mutation there proves no coverage. The x86 SSSE3 differential asserts the feature rather than silently passing under an unsupported emulator; the optional AVX2 count differential reports a skip. Native timings and emulated correctness must be kept distinct.

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

### Modular exponentiation: what two deferred arms are actually worth ( measured 2026-09-25 )

`pow_mod` became a query operation on 2026-09-25 and `MAX_WORK` was calibrated
on its square-and-multiply cost, so the two deferred arms bearing on its inner
loop were measured rather than left as estimates. The probe was a standalone
crate under `.agents-workspace/tmp/` with a path dependency, run twice to check
stability; both runs agreed to within 1% on every row except where noted.

**Frame**: random odd moduli with the top bit set, a full-width random
exponent, `--release`, on this workstation. Every arm asserts equality with the
shipped loop before being timed, so a faster wrong answer cannot be reported.

```text
  m bits   baseline    w = 4     w = 5    known-reduced   window gain
    1024     1.11 ms   0.86 ms   0.85 ms      1.00 ms          1.30x
    2048     6.70      5.38      5.24         6.66             1.28x
    4096    40.94     33.40     32.20        40.79             1.27x
```

**Windowed exponentiation is worth about 1.27x, flat across sizes**, and `w = 5`
beats `w = 4` slightly at every width. That is a real gain for what the record
calls "a straightforward later refinement", and it is **larger than the
operation count predicts**: at 1024 bits the baseline is ~1536 `mul_mod` calls
against ~1279 for `w = 4`, which is 1.20x against 1.30x measured. Note the
direction -- the op-count model *under*-predicts here, where the carry-save
model over-predicted by two to three times. Neither is evidence the models work;
both are evidence they do not track time, which is the standing conclusion.

**A squaring kernel does not exist to be measured, and the ratio says so.**
`a.mul( &a )` costs the same as `a.mul( &b )` at every size ( 1.00-1.02x over
16 to 256 limbs ). That is the **premise**, not a refutation: the record's "worth
about a factor of two" is what a *dedicated* kernel exploiting `a_i a_j == a_j
a_i` would deliver, and this measurement only confirms the general multiply gets
no such benefit today. **Do not read the 1.00x as the squaring arm being
worthless** -- it has not been built, so it has not been measured.

**The optimization the numbers suggested is worth nothing, and that is the most
useful row.** `mul_mod` is `reduce( a ) * reduce( b )` then reduce; the operand
reductions short-circuit on an already-reduced value but return `x.clone()`, so
every call copies `k` limbs twice -- and in `pow_mod`'s loop every operand is
already a residue. An arm skipping both ( `b.reduce( &x.mul( y ) )` ) measures
**1.00-1.01x at 2048 and 4096 bits**. The copies are `O( k )` against an
`O( k^2 )` multiply, so they vanish exactly where they were expected to matter.
An earlier run showed 1.65x at 1024 bits from eight iterations; at sixty it is
1.11x and unstable, and the honest reading is measurement noise rather than a
small-modulus effect. **Recorded because it looked obviously right on
inspection and was not.**

**What this changes**: nothing yet, and deliberately. Windowing is the only arm
with a measured gain, `MAX_WORK`'s calibration would move by about 1.27x if it
landed, and none of it has a caller asking for faster modular exponentiation.
The numbers are here so the next session does not re-derive them.

### Narrow values: inline storage and the arms that make it pay ( 2026-09-25 )

**The question was whether a JIT could speed up 4 to 128-bit arithmetic. The
measurement said the target was somewhere else.** A `mul` cost 13.3 ns against
0.55 ns for the native product and was **flat from 4 to 64 bits** -- the cost
did not depend on the width, so it was not the arithmetic. The breakdown named
it: one heap allocation was ~6.5 ns, a `mul` was about two of them, `cmp`
( which allocates nothing ) was 0.9 ns, and the arithmetic itself was ~0.4 ns,
about 3% of the total. A JIT compiles the 3% and still allocates the result.

**`BigUint` now holds up to two limbs inline**, 128 bits, chosen because that is
every width that measured flat.

```text
  bits    before    after    operation           before    after
     4   13.8 ns   8.3 ns    clone (1 limb)     6.6 ns   0.8 ns*
     8   13.3      8.3       from_u64           6.5      0.5
    16   13.3      8.3       mul               13.2      7.0
    32   13.3      7.1       add               10.7      7.7
    64   13.2      7.1       cmp                0.9      1.4
   128   15.5     11.0       mul then add      22.6      8.3
                             mul, 16 limbs    202.7    192.3
```

**Inline storage alone was a regression, and that is the part worth carrying
forward.** With the representation changed and nothing else, `mul` went from
13.2 ns to **17.5 ns**: the kernels still built a limb vector and then copied it
inline, paying the allocation *and* the copy. Cheap storage does not remove an
allocation that something else performs. The saving needed dedicated narrow
arms -- `u64 x u64` and `u128 x u128` products, and `u128` add and subtract --
that never form the vector.

**Two further defects the numbers caught, both mine.** `from_limbs_le` takes its
vector by value; routing it through the copying constructor made every
heap-sized result pay a second allocation, which showed as a 128-bit multiply
regressing to 24.3 ns while every narrow width improved. And the first probe
reported 3.0 ns at four bits, which looked like a win and was
`0xDEAD..DEF0 & 0xF == 0` -- a multiply by zero hitting an early return.

**The prediction was wrong by about seven times, in the optimistic direction.**
Before building, this record would have said removing the allocation takes
`mul` to "roughly 1 ns". It reached 7.0 ns. Allocation was the *largest* cost,
not the whole one, and what remains is enum dispatch, two 32-byte value moves
and the narrow-arm checks. **A cost attributed entirely to the one thing that
was measured will over-promise by whatever was never measured.**

**\* The 0.8 ns clone is not a usable figure ( corrected 2026-09-26 ).** It was
measured on a local the optimizer could see through, and an inline clone with
no side effect is eliminable where a `Vec` clone is not -- so the "after" column
is partly measuring a clone that did not happen. Behind a reference, where it
cannot be folded, the same one-limb clone measures **7.0 ns**. The `mul` figures
are sound ( the result is consumed through `black_box` ), but treat the
allocation-only rows as an upper bound on the improvement rather than a
measurement of it. **A before/after pair is not comparable when the
optimization also makes the operation eliminable.**

**`cmp` regressed, 0.9 ns to 1.4 ns**, from the branch `limbs()` now carries.
Kept because `reduce`'s short circuit calls it about 4 600 times in a 2048-bit
modular exponentiation, which is 2.3 us against that operation's 6.7 ms.
Recorded rather than buried: it is a real loss and the argument for it is an
arithmetic one, not an absence.

**The representation is not observable**, which is the whole safety argument:
`PartialEq`, `Hash` and `Ord` all read the limb slice and never the variant, so
a value that spilled to the heap and one that never left the inline arm compare
and hash alike. A representation leaking into equality would give one number two
identities -- the defect the canonical form exists to prevent, one level down --
and `where_the_limbs_live_is_not_observable` is what holds it.

### Fused `Vec[Big]` arithmetic: why not the JIT ( measured 2026-09-26 )

The question was whether `jit.rs` could fuse element-wise big-integer work. The
measurement says the headroom is real and the JIT is the wrong mechanism, and
the argument is the JIT's **own admission rule**.

```text
     n    bits   BigInt ns   native ns    ratio   per element
    64      64         711          51    14.0x      11.1 ns
   512      64       4 932         243    20.3x       9.6
  4096      64      37 128       1 861    19.9x       9.1
  4096     128     135 461       1 858    72.9x      33.1
  4096    1024   1 063 242       1 857   572.5x     259.6
```

**At 64 bits about 95% of an element is value bookkeeping, not arithmetic** --
`BigInt::mul` is 10.7 ns against 0.42 ns native, and the vector machinery
( iterator, `collect` ) adds almost nothing on top of the scalar op. So fusion
has roughly a 20x ceiling on narrow vectors. **At 1024 bits there is no fusion
headroom at all**: 260 ns per element is a 16-by-16-limb schoolbook multiply
doing real work, and the native `u64` column is not a meaningful floor for it.

**The JIT cannot amortise here, by its own standard.** `worth_jitting` admits an
expression only at four or more leaves over many dense bitmap chunks -- it earns
compilation back across *data volume*, which for a bitmap DAG is unbounded. A
big-integer vector has a hard ceiling: `MAX_VIEW_SETS` caps arity at 4096, and a
4096-element zip at 64 bits is **37.5 us** end to end. Cranelift compiling even a
small function is of that order or more, so a JIT'd kernel would need the *same
shape* repeated several times merely to break even, against an operator set that
is 5 binary operations by a few width classes -- about fifteen shapes, all
statically enumerable. **A static specialization gets the same ceiling with no
compilation to amortise**, which is the whole reason the bitmap JIT's admission
test is written the way it is.

**The measurement found a different opportunity than the one it went looking
for.** `x.magnitude().clone()` costs **7.0 ns** where a directly held
`BigUint::clone` measures 0.66 ns -- the same type, the same one-limb inline
value, ten times apart, because the second is a local the optimizer folds away
and the first is behind a reference it does not. `BigInt::mul` is 10.7 ns
against `BigUint::mul`'s 7.1 ns for the same magnitudes, so the signed wrapper
costs about 3.6 ns that the narrow arm underneath it does not.

**If narrow big-integer work is ever worth speeding up, that gap is the cheap
half and it is not in the JIT**: give `BigInt` the narrow arms `BigUint` has
rather than reaching them through `from_magnitude`, and stop cloning magnitudes
behind references. No caller is asking for either yet, which is why this is a
record and not a change.

### Specialized narrow kernels at 4/8/16/32/64/128 bits ( measured 2026-09-26 )

**The six widths are two kernels, and the measurement is what proves it.** 4, 8,
16, 32 and 64 bits all occupy one `u64` limb, so they take the same path and
time identically -- 7.1 ns for `add` at every one of them, before and after this
work. 128 bits is the two-limb case. **Width does not separate these operations;
limb count does**, which is why the arms are spelled `to_u64` / `to_u128` and
not as six cases. Specializing below 64 bits would buy nothing: the hardware
adds 64-bit registers whatever the value's bit length.

Before and after, ns per operation, best of five passes:

```text
BigUint            4-32 bits        64 bits         128 bits
  add            7.2 -> 7.1      7.2 -> 7.1     41.8 -> 9.0
  sub            7.2 -> 6.5      7.2 -> 6.6     16.4 -> 17.0
  mul            7.0 -> 7.1      7.0 -> 7.1     12.4 -> 10.6
  divrem        23.1 -> 7.9     32.8 -> 14.0    85.7 -> 86.9

BigInt ( signed )
  sub           11.5 -> 10.0    11.4 -> 10.0    45.8 -> 11.3
  divrem        28.8 -> 10.4    38.9 -> 17.1    89.8 -> 92.3
```

**Three defects, each invisible to every test because none is a wrong answer.**

**`add`'s 128-bit arm used `checked_add`.** Two operands that each fill 128 bits
are exactly the pair that carries, so the arm declined precisely at the width it
was written for and fell through to the generic path -- 41.8 ns, worse than
`sub` and `mul` on the same operands. `overflowing_add` plus a three-limb
construction fixes it. The carry case is `2^128`, and the largest non-carrying
sum is `2^128 - 1`, which needs two halves of the range to reach: `max128 + x`
carries for every nonzero `x`, a boundary the first draft of the regression test
got wrong.

**`divrem` had no narrow arm at all**, so a one-limb division ran Algorithm D:
three allocations ( normalized dividend, normalized divisor, quotient vector )
to do what one machine divide does. The `divrem` row above is the largest win
here. **The 128-bit column is not a failure**: that row divides a 256-bit
dividend, which genuinely needs Knuth. A 128-bit dividend takes the arm.

**`BigInt::sub` was `self.add( &rhs.neg() )`, and `neg` clones the magnitude**
to flip one bool -- so every signed subtraction allocated a whole second operand
to discard. Replaced by `add_signed( &magnitude, negative )`, which takes the
sign as an argument so the negation never exists as a value. `neg` itself still
costs 6.7 ns and must: it returns an owned value.

**`#[inline]` on the wrappers is load-bearing, not decoration.** The workspace
sets no LTO, so a caller in another crate -- the expression evaluator is one --
cannot see through a non-inline function, and an arm the caller cannot see is an
arm that does not exist for it. Each public kernel is now a small `#[inline]`
narrow arm over an `#[inline(never)]` generic body, so what duplicates at a call
site is a register operation and a branch rather than Karatsuba.

### Where a narrow operation's time actually goes

This is the finding that outranks the table, and it was reached by decomposing
`add` at 64 bits rather than assuming:

```text
  read both operands     0.68 ns
  construct one value    0.39 ns
  add ( whole )          7.11 ns
  add, result consumed   2.06 ns    <- the same add, folded into a scalar
```

**About 5 ns of the 7.11 is returning a 32-byte `BigUint` by value and dropping
it, not arithmetic.** The kernel is ~2 ns. `BigUint` is 32 bytes and carries a
destructor because of its `Heap` variant, so every result is moved into the
caller's frame and drop-checked there.

**This is a much better-grounded version of the fused-arithmetic argument than
the 20x headroom figure recorded above.** It says exactly where a fusion win
comes from -- not materializing each intermediate -- and sizes it at about 3.5x
at narrow widths. It also says the mechanism is a static specialization that
keeps intermediates in registers, not codegen, which agrees with the
amortisation argument for the same conclusion by a different route.

**Do not quote the `clone` row from any of these runs.** It read 0.65, 1.35 and
0.77 ns across three runs of the same binary. It is at the noise floor and the
harness prints a spread warning for exactly this reason.

### SIMD for zip/scale on block views: the arithmetic is 1.4% ( measured 2026-09-26 )

The proposal was a special kernel for `zip` / `scale` over block-backed
`Vec[Big]`, using SIMD to compute without materializing intermediates. **The
SIMD kernel works and is not worth building**, and the reason is Amdahl rather
than anything about SIMD.

**A vertical NEON add does win.** Lane `j` holds constituent `j`, so the carry
chains are independent -- SIMD across the limbs of one value would be wrong,
because a carry chain is serial by construction. Two independent values added
together, ns per pair, against scalar doing both in one call over the same
buffer:

```text
   bits  limbs   scalar   neon    ratio
     64      1      3.2    1.2    2.65x
    128      2      4.5    2.2    2.08x
   1024     16     20.6   13.6    1.51x
   4096     64     77.4   89.3    0.87x
  16384    256    308.8  480.2    0.64x
  65536   1024   1218.4 2036.8    0.60x
```

**The crossover at ~64 limbs is the loop-carried carry dependency.** NEON has no
carry flag, so each lane costs an add, two compares, an or and a shift where
AArch64's `ADCS` costs one instruction -- but it does two lanes at once. At short
lengths the instruction count wins; at long lengths the serial dependency chain
does, and vector ops have the higher latency. **Two earlier framings of this
measurement were wrong and are worth not repeating**: comparing one NEON call
against two scalar calls makes the narrow rows look like a SIMD win that is
mostly function-call overhead, and a kernel that assembles lanes with
`vsetq_lane_u64` loses everywhere ( 0.82-1.00x ), so it measures lane assembly
rather than arithmetic.

**None of that matters, because the arithmetic is not the cost.** A real blocked
zip, chunk-aligned stride, half density:

```text
  sets   bits   arith %   view_select %   read_int %
    64     64      1.5%          34.5%        51.7%
    64   1024      1.2%           4.7%        79.4%
  4096     64      1.4%          39.3%        47.4%
  4096   1024      1.2%           5.8%        77.6%
```

So a perfect arithmetic kernel is worth **1.2-1.5%** of the operation and the
measured 2x is worth about 0.7%. **And the data the winning kernel needs does
not exist anyway**: it wants limb-interleaved operands, which no view layout
produces -- `Blocked` puts constituents far apart and `Interleaved` interleaves
*bits*, not limbs.

**There is a second, independent blocker, and it is the more interesting one.**
SIMD over words needs a `Bitmap` container, the only kind with a word array. A
blocked constituent's kind by width and density:

```text
   bits   density=1.0   0.5      0.1
     64        Run      Array    Array
   1024        Run      Array    Array
   4096        Run      Array    Array
  16384        Run      Bitmap   Array
  65536        Run      Bitmap   Bitmap
```

**A fully dense value is a `Run` container at every width**, because a
contiguous stretch of ones coalesces into one run -- so "all ones" never has
words either. Words appear only from about 16 384 bits at intermediate density,
which is exactly where materializing is already negligible. The bands where the
kernel is possible and the bands where it would pay do not overlap.

### What the exploration found instead: read_int walked runs bit by bit

`read_int` block-transfers a bitmap and, before this, walked an Array **or a
Run** ordinal by ordinal. A run is a list of intervals, and an interval of set
bits maps onto whole words of `u64::MAX` with a partial word at each end. Filling
instead of walking, ns per limb:

```text
   bits    Run before   Run after   speedup    Bitmap ( for scale )
     64         137.7        22.6       6.1x
   1024         108.0         1.7       62x
   4096         106.0         0.5      226x
  16384         105.4         0.2      509x                     1.0
  65536         105.1         0.1      875x                     0.1
```

**The densest values took the slowest path.** An all-ones integer is the
simplest bit pattern there is and it was the worst case, at a flat ~105 ns per
limb independent of width. The run path now matches the bitmap path.

**The lesson is the ordering.** The kernel was proposed for the arithmetic, and
one measurement of where the time went ( 1.4% arithmetic, 47-79% `read_int` )
redirected the work to a 62-875x fix requiring no `unsafe`, no SIMD and no new
public API. **Measure the split before choosing the mechanism** -- the same error
the JIT exploration made and the same correction.

`view_select` at 34-39% on narrow values is the remaining unexamined term, and
the module header claims a chunk-aligned blocked view should be "close to free"
there. That claim has not been checked against a measurement.

### A shared decode buffer for read_int is a regression, and run-to-bitmap promotion is blocked ( 2026-09-26 )

Both were asked for directly. Both were implemented far enough to measure and
then reverted, which is the useful part of the record.

**A shared word scratch made `read_int` slower at every width.** The facility was
built the way the house already does it -- thread-local, grown never shrunk, only
the used prefix cleared, following `ops::nary::Scratch` -- and routed through
`read_int`:

```text
   bits   kind      owned vec   shared scratch
  65536   Run             122              198
  65536   Bitmap          134              207
```

**Only the wide rows are quoted, and that is deliberate.** The half-density
**Array** rows in this harness span **894 to 1247 ns across process runs of the
same binary** -- a 40% spread, bimodal, evidently allocation-address luck. Three
samples either side read as a clean 17% regression and it was noise; a later
"fix" for it that read as restoring the number was also noise. The 65 536-bit
rows are stable to about +/- 2 ns and are what the conclusion rests on. **Take a
distribution across process runs before believing a row in this harness.**

**The narrow case is a different story, and the shared buffer was the wrong tool
for it rather than the wrong idea.** A value of `INLINE_LIMBS` or fewer is held in
registers, so nothing keeps the buffer it was assembled in -- the
`vec![0u64; words]` was a malloc and a free of scratch space that was then
discarded. The fix is a **stack array**, not a thread-local: no allocator, no
`RefCell`, and the one- or two-word move into inline storage happens either way,
so avoiding the allocation costs no copy at all. Measured over six process runs,
with the wide path untouched:

```text
   bits   kind          before        after
     64   Run            23-26        14-19
     64   Array ( .1 )      41           24
  65536   Run            122-3      122-124   unchanged
  65536   Bitmap         131-4      132-143   unchanged
```

**The mechanism for the wide case is that `read_int` returns an owned value.**
`BigUint::from_limbs_le` takes the vector **by value and moves it** when the
value stays on the heap, so `vec![0u64; n]` costs one allocation and no copy.
Filling a shared buffer instead forces `from_slice`, which copies out -- an
allocation *and* a memcpy, and the wider the value the worse the trade. A shared
buffer pays only where the buffer is **transient and discarded**; it cannot pay
where the buffer *becomes* the result.

**`from_limbs_le`'s own doc comment already recorded this**, from the other
direction and an earlier session: "routing it through the copying path made every
heap-sized product pay a second allocation, which showed up as a 128-bit multiply
regressing from 15.5 ns to 24.3 ns while the narrow widths improved." The answer
was written in the function being edited. **Read the rationale on the constructor
before changing who owns its buffer.**

The place a shared buffer would pay is [`BitStore::decode_words`], which takes a
whole `BITMAP_WORDS` vector whenever a shared buffer is unaligned or the host is
big-endian -- including for `min` and `max`, which read one word and discard
1 023. That path is rare in practice, because `arrow_buffer` allocations are
over-aligned and `try_words` therefore succeeds, so it was not pursued. The
better fix there is for `min` / `max` not to decode 1 024 words at all.

**Automatic run/array to bitmap promotion cannot be an in-place representation
change.** Three independent blockers, each sufficient:

* **The containers are immutable.** `U16Store::Shared` and `BitStore::Shared` are
  documented as "immutable, refcounted, possibly aliasing an mmap". That
  immutability is what makes `Container: 'static + Send + Sync`, which is what
  lets streams be boxed and sent across threads.
* **It would break the M0 gate.** `Roaring32::serialize` branches on `c.kind()`
  to set the run-flag bitset, so changing a container's kind changes the
  serialized bytes -- and `serialized_bytes_are_identical_to_the_roaring_crate`
  requires our kind choice to match `roaring`'s for the same data. Byte identity
  is also what makes `O( container count )` import of `.roaring` files
  legitimate, so it is not a test to relax.
* **Containers are shared across snapshots.** Mutating one on read would change
  what another live snapshot sees.

**The demand-shaped design that *is* safe is a side cache** of decoded words
keyed by container identity, valid for a snapshot's lifetime, changing no
representation and no serialized byte. **It would not help the operation that
prompted it**: in a `zip` or `scale` each constituent is read exactly once, so
there is no second read to serve from a cache. It would help repeated queries
over the same chunks, which is a different workload and has not been measured.

**And the demand is mostly gone anyway.** The reason a run container was worth
promoting was that reading one cost ~105 ns per limb; filling intervals instead
of walking ordinals brought that to 0.1-1.7 ns per limb, matching the bitmap
path. **A representation change to reach a speed the existing representation can
already reach is not worth the invariants it costs.**

One further thing measured and reverted: grouping the array arm's writes by limb,
on the reasoning that sorted values share a word and the loop paid a
read-modify-write per value. **No gain, some rows slightly worse** -- the cost is
`container.iter()`, not the store, and LLVM was already keeping the word live.
The remaining array cost is ~55-85 ns per limb at half density and is genuinely
proportional to set bits.

### The array scatter: 4x from scalar changes, and NEON measured at 1.05x ( 2026-09-26 )

Asked to try SIMD on the remaining slow path in the `read_int` harness. The
answer is that **NEON does not pay**, and that the same harness gave **4x from
two scalar changes** found while setting up to ask the question properly.

**First the harness had to be fixed.** Timing one set per configuration made the
half-density Array rows bimodal, 894 to 1247 ns for the same row of the same
binary -- allocation-address luck, fixed for a process's lifetime, which a
best-of-five *inside* the process cannot see through. Building nine
independently allocated sets that coexist, and taking the median, brought every
row to within 1% of its own min and max. **A before/after on an unstable row is
not a measurement**, and two conclusions drawn on that row earlier the same day
were noise.

**`Container::iter` cost 1.4x to 3.5x of the array read.** It wraps the slice
iterator in an enum, so the array arm paid a discriminant branch per value --
32 of them per limb at half density. Reading `ArrayContainer::as_slice`
directly, as `ops::nary` already did, removes it.

**Grouping the writes by limb then paid another ~2x**, because values are sorted
and a limb's arrive consecutively: a register accumulator and one store per limb
replaces a load-or-store per value. **This exact change was tried earlier the
same day and measured as no gain** -- correctly, at the time, because the enum
dispatch was still there and dominated it. Removing the larger term is what made
the smaller one visible.

```text
  bits   density   original   +as_slice   +grouping   total
    64      0.50         75          53          27    2.8x
  1024      0.50       1065         583         286    3.7x
  4096      0.50       4137        2204        1029    4.0x
  4096      0.10        868         264         243    3.6x
 16384      0.10       3345         947         861    3.9x
```

**Only then is the SIMD question worth asking**, and the answer is no. The
scalar kernel is now ~0.50 ns per value, about 1.5 cycles. A NEON version that
accumulates in a vector and folds only at a limb boundary -- the one arrangement
that does not immediately give the lane count back -- measured against it on one
65 536-bit chunk:

```text
  density   values   scalar ns   neon ns   ratio
     0.50    32768       12867     12210    1.05x
     0.25    16384        6397      6670    0.96x
     0.10     6554        2623      4255    0.62x
     0.05     3277        1372      4327    0.32x
```

**A wash at best, and it collapses as density falls.** Two structural reasons,
both inherent rather than fixable by a better kernel: the reduction target is a
**single 64-bit accumulator**, so lanes must be folded before every store; and
the limb-boundary test is **data-dependent**, so it cannot be hoisted out of the
loop. As density drops the values per limb drop with it -- about three at
density 0.05 -- and the vector fast path stops hitting, leaving the fold
overhead with nothing to amortise.

**This is the third SIMD-shaped proposal in this file to fail on the same
property**: a carry chain, a per-element value cost, and now a scatter
reduction. In each case the arithmetic was not where the time was, and in each
case the useful result came from measuring the split first. The pattern worth
carrying: **SIMD wants many independent lanes with no cross-lane reduction, and
big-integer work keeps supplying the opposite.**

### The wide interleaved fold: 31x to 127x, and it wanted no SIMD ( 2026-09-27 )

A consumer observed that a `sets` which is a multiple of 64 looks like the
**easy** case for word-parallelism rather than the hard one. It is, and the
existing arms had solved only the hard one.

`fold_table` covers `sets` of 2, 4 and 8, and the whole
`fold_interleaved_bitmaps` arm is gated on it -- so every wider arity fell to
`fold_interleaved`, which walks every set bit. Measured over 8 dense chunks,
262 144 set bits, ns per set bit:

```text
  sets      before      after
     2        0.38       0.38      table plus a vector arm
     4        0.19       0.19
     8        0.10       0.10
    16        1.99       1.99      still declines: not a multiple of 64
    64        1.76       0.06      31x
   256        1.77       0.04      46x
  1024        1.72       0.01      127x
```

**Wide is now cheaper per bit than narrow** ( 0.01-0.06 against 0.10-0.38 ),
which is the consumer's point measured: at `sets = 2` a logical ordinal's bits
sit inside one word and have to be shuffled out, while at `sets = 256` a logical
ordinal spans **four whole words**, so its population is a sum of `count_ones`
with no shuffle, no table, and no vector instruction. **The hard case was solved
and the easy one was not, because the easy one only appears at widths nothing
exercised.**

The output addressing is deliberately identical to the narrow arm's --
`prefix / sets` names the output chunk, `( prefix % sets ) * ( BITMAP_WORDS /
sets )` its first word -- because `logical = physical / sets` makes both arms the
same relabel seen from opposite directions.

**The caution filed with this item was wrong, and that is worth more than the
speedup.** It said part of the 2.1 ns per bit was the `Container::iter` enum
dispatch, measured at 1.4x-3.5x in `read_int`'s array arm the day before, and
that a scalar pass should be tried first. Tried first, as filed: specializing
the container kind and replacing `/ self.sets` with a shift for power-of-two
arities measured **no change at all** ( 1.74-1.78 against 1.76-1.77 ). The
dispatch tax is real for an **array** payload, where `iter` yields one value per
step; these payloads are **bitmaps**, where `iter` already scans words and the
per-bit cost is bit extraction. **A measured cost does not transfer to another
call site just because the same function appears in both.**

**Verification, and a coverage hole worth remembering.** No test in the tree
exercised a wide interleaved fold at all, so the 44 passing `view` tests were no
evidence. Four new tests compare against the definition -- count the slots of
each logical ordinal and reduce -- across arities, all three reduces, and six
shapes including chunk seams. Sabotaging the per-chunk bit offset, the word span
per logical, and the cardinality each reddened them. **Sabotaging
`output_prefix` to a constant `0` did not**, because at `sets = 256` it takes 256
input chunks to fill one output chunk and every fixture of two or three adjacent
chunks maps entirely into output chunk 0. A fixture with chunks at prefix 0 and
prefix `sets` closes it. **When an addressing term only matters at a scale the
fixtures do not reach, the fixtures agree with any value of it.**

`sets` of 16 and 32 remain on the per-bit walk: too wide for the byte table, not
a multiple of 64. Nothing has asked for them, and they would need a third
structure ( several logicals per word, but more than 8 constituents ), so they
are left alone deliberately.

### The third structure: sets of 16 and 32 ( 2026-09-27 )

Closing the hole the wide arm left. There are **three** structures, not two, and
they are decided by where a logical ordinal's `sets` bits sit relative to a word:

* **inside a byte** ( `sets` 2, 4, 8 ) -- `fold_table`, a 256-entry lookup per
  byte, plus the NEON and SSE arms. The bits of one logical ordinal have to be
  *gathered out of* a byte, which is why this case needed a shuffle.
* **inside a word, wider than a byte** ( `sets` 16, 32 ) -- a contiguous bit-field:
  `( word >> k * sets ) & mask`, counted. One shift, one mask, one `count_ones`.
* **spanning whole words** ( `sets` a power of two from 64 up ) -- a sum of
  `count_ones` over `sets / 64` words.

The middle case was the last to be written and is the simplest of the three. It
existed only because 16 and 32 are too wide for a byte table and too narrow for a
word, so neither of the arms that were written reached them.

Measured over 8 dense chunks, 262 144 set bits, `Any`:

```text
  sets      before      after   speedup
    16   524 333 ns   54 092 ns    9.7x
    32   482 395 ns   27 761 ns   17.4x
```

**Per set bit the curve is now monotone in `sets` and has no hole**: 0.36, 0.19,
0.10, 0.21, 0.11, 0.06, 0.03, 0.04, 0.01 at 2 / 4 / 8 / 16 / 32 / 64 / 128 / 256
/ 1024. Before this the middle two read 2.00 and 1.84 -- **twenty times their
neighbours on both sides**, which is a strange shape for a library to have and
was only visible once the wide arm made the right-hand side fast.

**`sets` dividing 64 is a stronger condition than the wide arm's and needs no
separate chunk-tiling check.** A divisor of 64 is a power of two, hence a divisor
of `CHUNK_CARD`, so no logical ordinal can straddle a chunk -- which is exactly
the condition the wide arm has to test for itself and got wrong on its first
attempt. Stated in the arm's doc so the asymmetry between the two guards is not
read as an oversight.

**The tests were written to the standard the wide arm had to be repaired to.**
The firing list is **every arity from 1 to 96** plus the wide ones, asserting
`fires == ( sets == 16 || sets == 32 )` -- drawn from the domain, not from the
guard, because the wide arm passed a guard-derived list while being wrong on
eleven widths. There is a fixture spanning two *output* chunks ( at `sets = 16` it
takes 16 input chunks to fill one, so adjacent chunks agree with
`output_prefix = 0` ), a mixed-container fixture, and the neighbouring arities 8
and 64 run through `view_fold` to check that adding an arm did not steal a case
from the table or the wide path. Five sabotages -- wrong field, wrong logical
index, dropped chunk offset, mask one bit narrow, wrong output chunk -- each
reddened the suite, `output_prefix` included this time.

### And the top end, where the bound was not the condition ( 2026-09-27 )

Closing the arity range completely. The wide arm was guarded on
`sets % 64 == 0 && sets <= BITMAP_WORDS`, and that second clause **was never the
real condition**. `MAX_VIEW_SETS` is 4 096, so 2 048 and 4 096 are legal views;
they tile a chunk perfectly ( 32 and 64 words per logical ordinal, both dividing
1 024 ); and they were walking every set bit.

```text
  sets      before      after   speedup
  2048   446 758 ns   2 371 ns    188x
  4096   446 577 ns   1 968 ns    227x
```

**The fix was to delete a condition, not add one.** The divisibility test the
`sets = 192` bug forced already subsumes an upper bound: once `sets / 64` exceeds
`BITMAP_WORDS` it cannot divide it, so a `sets` too wide to yield one logical
ordinal per chunk declines on its own. One condition now does the work of two and
is the one the arithmetic actually needs.

**Both of this arm's defects were in the guard rather than in the loop**, and in
opposite directions -- `sets = 192` was **admitted** and wrong, 2 048 and 4 096
were **refused** and correct. A guard assembled from plausible-looking clauses
rather than derived from what the addressing requires can fail either way, and a
test list drawn from that guard's shape finds neither.

The full per-set-bit curve, with every arity a view can name:

```text
  sets      2     4     8    16    32    64   256  1024  2048  4096
  ns/bit 0.36  0.19  0.10  0.21  0.11  0.06  0.04  0.01  0.01  0.01
```

**Monotone from 8 onward and with no hole anywhere.** At the start of the day
everything above 8 read 1.7-2.0.

### The three arms' boundaries are measured, not assumed ( 2026-09-27 )

Having written the sub-word arm, the obvious question is whether it should also
take 2, 4 and 8 and retire the byte table and its NEON / SSE code. Measured by
disabling `fold_interleaved_bitmaps` so the scalar bit-field arm takes those
widths, same fixture, `Any` in ns:

```text
  sets   scalar bit-field   table + SIMD   SIMD wins by
     2            424 436         98 348          4.3x
     4            212 087         51 316          4.1x
     8            106 813         26 327          4.1x
    16             54 851         55 291          0.99x
```

**The table and its vector arms earn their place by about 4x, and the crossover
sits exactly at 16** -- where the two formulations tie, and where the sub-word arm
in fact begins. The boundary was chosen from the structure ( a logical ordinal
fits in a byte, in a word, or spans words ) and the measurement puts it in the
same place, which is the outcome that makes the three-arm split a design rather
than three accidents.

The reason is that at `sets = 2` a word holds **32** logical ordinals, so the
bit-field formulation does 32 shift-mask-popcount triples per word where the table
does 8 lookups. The narrower the constituent, the more the per-field overhead
multiplies, and the more a table amortises.

**This also sharpens `simd-arms-without-a-crate-level-case`, in the direction
opposite to that entry's thesis.** That entry's measured figures are `ops::mixed`
at **1.00x** and `ops::run` at **1.10x** -- arms that are correct but not shown to
pay. `view::fold`'s vector arms are **not** in that category: 4.1x-4.3x against
the best scalar formulation available for the same widths, measured the same way
by alternating builds. **"SIMD in this crate is unproven" is not a crate-level
claim; it is true of two `ops` arms and false of the fold's.**
## Typed expression language and bounded integer transport

The view operations on the wire are compositions, not special-case leaves: `view( set, spec )` yields `Vec[Set]`, indexing selects a constituent, and `fold( view, and | or | xor )` reduces one. This permits a view over a computed set such as `and( key( a ), key( b ) )`. Flight recognises direct select and fold patterns before generic vector lowering, preserving the packed single-walk path. The facet query `map( view( key( 9 ), interleaved( 3 ) ), cardinality( and( _, key( 7 ) ) ) )` produces per-constituent counts; `Vec[Bool]` is represented as a set of constituent indices rather than a duplicate sort. A map hole is valid only in its body, and nested body maps are refused to keep binding unambiguous across the Rust, Python, Go, and Java codecs.

**Untrusted descriptors need work bounds at the wire boundary.** `ViewSpec.sets` once let about 30 input bytes request four billion constituents: expanding four ordinals took 231.37 s. `yesno-wire::MAX_VIEW_SETS = 4096` now refuses that descriptor at decode. Core deliberately keeps `View::check` free of this policy cap because a descriptor with unaddressable upper constituents is legal; its loops instead stop at `addressable_sets` or the tighter `occupied_sets` bound. For `All`, omitted empty constituents force an empty answer; `Any` and `Parity` may skip them as identities. A value assertion could not distinguish the old loop, so the regression uses a deadline and was checked against the 231.37 s sabotage.

The final wire language has `Set`, `VecSet`, `Int`, `VecInt`, `Bool`, `Big`, and `VecBig` sorts. One tag space and `sort_of_tag` classify every node, including a wrong-sort tag, in every decoder. Vector arity, index bounds, and pack/view agreement are checked while decoding. Pinned byte vectors across the four client implementations catch drift that self-roundtrips cannot. `yesno-pg` is built by Bazel, so wire enum changes require `gate-pg.sh` as well as the Cargo gate.

The `Big` sort reads one set as one integer. `BigExpr::Read` and `ReadSigned` carry a bare width; `IntLayout`, `IntSink`, and wire `IntSpec` were removed. Unsigned and two's-complement signed reads are separate nodes because they assign different values to the same bits. The literal format is canonical sign and little-endian magnitude: no negative zero and no trailing zero byte. `BigInt` uses sign and magnitude internally, division truncates toward zero, and Euclidean remainder is derived separately. Exact arithmetic makes `add( a, b ).saturate( w )` sufficient; no parallel `add_sat` family is needed. Signed truncate and saturate target the same `W`-bit field.

`QueryRequest.expression` and `Ticket.expr` now carry `AnyExpr`, because a `SetExpr` ticket made `VecInt` and `Big` evaluable in process but unreachable over Flight. The result schema is sort-specific: `VecInt` has one `UInt64` row per constituent and `Big` has a single row with `negative: Boolean` and canonical `magnitude: Binary`. `schema_for` must govern `GetFlightInfo`, batch construction, **and** `FlightDataEncoderBuilder::with_schema`; an earlier implementation reached only the first two and a live socket test caught the wrong stream schema. `VecBig` is one sort for signed and unsigned readings, with sign determined by each expression node.

Six independent bounds protect the language's distinct amplification axes. Depth and node count bound tree structure; `MAX_VIEW_SETS = 4096` bounds view fan-out; `MAX_VALUE_BITS = 1 << 20` bounds each node's integer width; `MAX_RESULT_BITS = 1 << 24` bounds vector output size; and `MAX_WORK = 1 << 28` bounds arithmetic effort. The last bound is necessary because `PowMod` may return a modulus-width answer after expensive exponentiation. It also covers full-width multiplication elsewhere in the tree and is an admission upper bound, not a planner cost estimate. `PowMod` rejects zero modulus and negative exponent, reduces a negative base into `[ 0, m )`, and returns zero for modulus one even when the exponent is zero. These rules are tested on both sides of their limits; `rsa_scale_exponentiation_is_admitted` prevents an overzealous budget from refusing ordinary work.

The transport lesson is broader than integer arithmetic: a local evaluator test can pass for a query no client can send, and a correct batch can arrive with the wrong declared schema. Keep a live `GetFlightInfo` to `DoGet` test for every new result sort, alongside the independent numeric oracle and cross-language byte vector.
