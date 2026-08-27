# Expression Planning, Statistics, and Segmentation

## Summary

The expression planner performs sound algebraic rewrites and then selects physical stream structures from opened operands. Its history is dominated by a recurring distinction: metadata-driven decisions can be nearly free, while statistics that inspect all prefixes must earn their cost against the execution they replace.

## Key Facts

- Logical rewriting and physical lowering are separate. `plan()` rewrites `Expr`; `open_planned()` selects stream operators from live statistics.
- `CostGuided` and `Conservative` are pluggable `PlanStrategy` implementations.
- Statistics are rationed by a per-query allowance. Refusal falls back to sound, coarser metadata.
- Exact prefix disjointness may use prefix arrays; approximate sketches never license a correctness-changing rewrite.
- Strict prefix-span separation lowers OR directly to `Concat`.
- `segmented_or` operates at chunk boundaries and uses `Restrict` plus `Concat`.
- The planner walk is linear after removing repeated bounds walks, deep no-op clones, and a hidden `prefix_span -> bounds` recursion.
- Database `Expr::Source` leaves provide exact prefix occupancy from their immutable key plans. Source segmentation is gated at 128 chunks per operand after a measured break-even near 88.
- Chunk-derived statistics cost is linear through `STATS_MAX_CHUNKS = 4096` and then declines to a constant estimate in one step; the cap bounds the cost but leaves a sharp threshold.
- The current cost model does not price materialization and can rate expressions with a 214x execution difference equally.

## Details

### Static rewrites and dynamic facts

Sound rules include identities, range algebra, bounded complement transformations, De Morgan factoring, and disjointness-based simplifications. A rewrite is accepted only under its rule's soundness conditions and, where applicable, a strictly lower heuristic cost.

`ChunkStream::stats` reports remaining chunks, prefix span, and backing class. It describes the remainder, survives boxing, and charges missing information pessimistically. `Snapshot::key_expr` is the production `Backing::Paged` leaf: its `KeySource` answers planner metadata from an immutable index plan and opens a `KeyStream` that decodes only demanded payloads.

### Statistics hierarchy

- `bounds()` and prefix spans are O(1) metadata on leaves and always available.
- Exact shared-prefix scans are sound and may prove disjointness, but cost O(chunks).
- `PrefixSketch` is exact only while unsaturated; a saturated sketch returns unknown for correctness decisions.
- `ChunkProfile` classifies spans as `Empty`, `Full`, or `Present` without reading payload bytes.
- `PrefixOccupancy` answers positional questions that KMV-style sketches cannot.

The statistics budget selects a sound abstraction from a precision chain. Budget-dependent costs are heuristics over mutable planning state, not mathematical functions of an expression, and must not appear in a termination measure.

### Segmentation and concatenation

`Concat` requires strict prefix order, not merely ordinal disjointness. Two disjoint sets inside the same 16-bit chunk cannot be concatenated as separate stream chunks.

`segmented_or` cuts on prefix boundaries where contributor sets change. Occupancy refines large span gaps, but not fine alternation; at bucket resolution, treating alternating operands as overlapping is the correct cost decision.

`ChunkSource::occupancy` lets a database source summarize the exact visible prefixes already present in its plan. A prefix span is safe to widen for `bounds`, where wider merely loses an optimization, but it is not a valid occupancy summary: claiming every prefix between two endpoints is present can select a worse split. `KeySource` therefore constructs `PrefixOccupancy` from the actual prefix list.

The source gate is measured for an eight-way union of nearly disjoint paged operands. Segmentation was 2.0x to 3.0x slower at 11 to 44 chunks per operand, crossed plain evaluation around 88, and was 1.11x, 1.17x, 1.26x, and 1.32x faster at 110, 165, 220, and 330 chunks. Setup was exactly 128 additional allocations at every measured size, so `SEGMENT_MIN_CHUNKS = 128` sits above the crossover with margin. The gate belongs in `occupancy_of`, not `cheap_to_reopen`: sources are cheap to reopen at every size, while segmentation pays only above a size threshold.

The early decline in `compute_segments` mirrors the source bounds before opening operands for spans. Below the threshold it saves exactly 11 allocations and changes nothing above it. Resident `Expr::Set` leaves are deliberately unaffected because the threshold was measured for paged sources only.

A verified consumer filter operand remains below the gate in its measured regimes. One aligned 65,536-document block is one chunk; a scattered filter occupies `ceil( N / 65536 )` chunks and a clustered filter remains one. This establishes that the gate protects that caller, not that the caller derives the value 128. The threshold still rests on the synthetic crossover above.

The explored n-pass split/merge design is superseded by aligned, demand-driven views. Its durable constraints remain:

- committed cuts and emitted fused segments are different state;
- fusion must never remove a committed cut from the termination measure;
- arbitrary ordinal boundaries require one scratch container to reassemble pieces that share a chunk;
- a shared-cursor intersection must retain early-out behavior.

### Planner complexity

Three real costs were removed in sequence:

1. guards recursively recomputed bounds;
2. the no-rewrite arm deep-cloned whole subtrees;
3. `covers` called `prefix_span`, which recursively called `bounds` again.

Only adjacent-size ratios distinguished constant-factor improvements from the quadratic term. After threading bounds and moving rebuilt nodes, the planner still approached 4x per doubling; after replacing the hidden recursion, ratios became about 2x and the walk became linear.

Chunk count is a separate axis from expression-node count. With two overlapping leaves, planning measures about 24 ns per chunk through 4,096 chunks, peaks at 99,325 ns, and falls to about 95 ns at 4,097 because the exact statistics path refuses. It remains flat through 524,288 chunks, the largest measured. The cap therefore reduces the former 42 ms absolute worst case to about 99 us but preserves a 1,045x one-chunk discontinuity and a worst measured plan-to-execution ratio of 0.699. The node-axis walk remains owned by the `plan_scaling` benchmark.

### Cost model limits

`cardinality_cost` models visited chunks and some metadata decisions. It does not distinguish a form that counts arithmetically from one that materializes operands. A one-chunk complement example prices both `And( Not(p), Not(q) )` and `Not( Or(p, q) )` at 1 while release execution differs by 214x.

`disjoint-or-is-overcharged` showed the inverse problem: charging a prefix-disjoint union as a merge caused recursive cardinality decomposition and a 2.97x regression. The cost model now shares the lowering's strict-span test.

## Files

- `yesno-core/src/stream/plan.rs` - strategies, rewrite rules, costs, and statistic allowance.
- `yesno-core/src/stream/sketch.rs` - sketches, profiles, and occupancy.
- `yesno-core/src/stream/dynamic.rs` - physical lowering and segmentation.
- `docs/formal-model.md` §6.3 and §14 - segmentation, and the restriction algebra underneath it. ( §14 is the surviving half of `docs/split-merge-algebra.md`, whose §§3-10 were superseded by `aligned-chunk-views` and which was deleted on 2026-08-27; the retired design's conclusions live in `TODO.md`'s `split-as-a-rewrite-not-a-lowering`. )
- `scripts/check-plan-measure.py` - source-pinned termination-measure audit.

## Test Coverage

- `planning_preserves_meaning` runs against every registered strategy and requires non-vacuous rewrite activity.
- `tests/expr_equivalence.rs` checks planned and unplanned results against eager semantics.
- Allocation tests guard n-ary lowering and decomposition choices.
- Source-segmentation coverage compares against eager evaluation and an independent `BTreeSet` oracle above the gate, while allocation bounds distinguish the segmented and declined paths.
- Planner benchmarks must call `Expr` APIs; direct `OrdSet` or `ChunkStreamExt` benchmarks bypass this code.

## Pitfalls

- A planner that rewrites nothing is perfectly sound, so soundness alone cannot show a rule is reachable.
- Do not treat a statistic used for planning as free because a related cached statistic is free.
- Do not key persistent caches by addresses; pointer identity is safe only within a call that pins the objects.
- Do not adopt stale prototype speedups without re-measuring against the current engine.
- Do not use a source's prefix span as occupancy or apply the source-derived threshold to resident set leaves.
- Do not quote the obsolete O(chunks) source-reopen cost; plans are shared and reopen is constant-time.

## The cost model has one dimension and needs two

`cardinality_cost` prices an expression in **chunks visited**. Measured against `cardinality_dyn` nanoseconds across ten shapes, with planning hoisted out of the timing loop, that unit buys between **3.0 ns and 47 384 ns** — a spread of roughly 16 000x across expressions the model exists to order.

```text
shape                          cost           ns      ns/cost
And( ¬p, ¬q ) 1-chunk             1        47384      47384.4
¬( p ∪ q )    1-chunk             1          421        421.0
And( ¬p, ¬q ) 32-chunk           32       425413      13294.2
¬( p ∪ q )    32-chunk           32          857         26.8
And( p, q )  plain             2000        75025         37.5
Or( p, q )   plain             8000        24053          3.0
And( dense, dense )               1          342        341.5
And( ¬dense, p )                  1          613        612.6
AndNot( p, q )                 2000        73819         36.9
Xor( p, q )                    8000        71713          9.0
```

`p` and `q` are 2 000 chunks at strides 3 and 5; `dense` is 60 000 contiguous ordinals; `¬` is clipped to the stated window.

**Two independent inversion families.** `And` over complements is the recorded one, up to 18x. The second is `And( p, q )` priced four times cheaper than `Or( p, q )` while running three times slower, with no complement involved — not visible from the single De Morgan pair the backlog entry was built on.

**One root cause.** `And::cardinality_dyn` materializes its operands and intersects; `Or` and `Xor` count by merge, and `Not` counts by subtraction — `( hi - lo ) - |input ∩ range|` — building nothing. So the model charges for chunks *iterated* and never for chunks *built*, and `And` is systematically undercharged rather than undercharged in one special case.

**A single scalar constant cannot repair this**, and fitting one to the De Morgan pair would leave the second family wrong. That is the shape of the previous cost-gate regression, which was calibrated on a two-operand measurement and sent an 8-way `Or` to 27 538 allocations.

The missing dimension is materialized **cardinality**, not chunk count, and the quantity already exists wherever a complement is counted. Any change here moves every planner decision, so the gate is this corpus plus `tests/allocation.rs` plus the `union_gate` bench, measured before and after.

**Method note.** The first run put `open_planned()` inside the timing loop and showed per-chunk cost *falling* with size — 47 us for one chunk against 13 us per chunk for thirty-two. Per-chunk cost that improves with size is the signature of a fixed per-call overhead dominating, not of the work under test. Hoisting planning out changed the numbers and did not change the conclusion, which is the only reason the conclusion is trustworthy.

