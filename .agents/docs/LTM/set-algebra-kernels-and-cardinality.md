# Set Algebra Kernels and Cardinality

## Summary

Set algebra is implemented by representation-specific kernels backed by a generic value-merge oracle. Cardinality and relation predicates are parallel non-materializing implementations and must receive every specialization independently; a fast materializing arm does not automatically improve its counting twin.

## Key Facts

- All nine ordered kind pairs have specialized materializing paths.
- `ops::card` is a second dispatch table and must be audited whenever a kernel is specialized.
- Run kernels operate on intervals, bitmap kernels on words, and array kernels use merge or gallop.
- `GALLOP_RATIO = 32` is an empirical crossover between different step costs, not a solution where the asymptotic formulas cross.
- `is_disjoint(a, b)` must not cost more than `and_cardinality(a, b) == 0`.
- `contains_all(a, b)` can often delegate to `and_cardinality(a, b) == b.len()` when the smaller side is bounded.
- Temporary array-to-bitmap promotion does not pay for a single pair; it is potentially an n-ary reuse decision.
- AArch64 array, bitmap, and balanced run counting kernels have measured NEON arms; every arm keeps its scalar oracle and runtime feature dispatch.
- Run x run galloping pays on skewed interval counts, while balanced high-`nruns` counting uses a separate block kernel.

## Details

### Specialized kernels

The original generic kernel merged individual values. That discarded the purpose of compressed run containers and produced gaps from 30x to more than 400x. Interval and word-specialized paths reduced representative cases as follows:

| Pair and operation | Before | After | Result |
|---|---:|---:|---|
| run x run AND | 996 us | 355 ns | About 6x faster than `roaring` |
| bitmap x run AND | 716 us | 2.31 us | Near parity |
| array x bitmap AND | 314 us | 12.99 us | Within about 1.2x |
| array x run AND | 564 us | 57.4 us initially | Later reduced with sorted merges |

Run XOR and ANDNOT use a boundary sweep so asymmetric leading and trailing regions are not encoded as special cases. Emitters coalesce adjacent runs because non-adjacency is a container invariant, not merely a compression preference.

### Counting is a separate implementation

`ops::card` initially specialized bitmap x bitmap only. Adding run x run, bitmap x run, array x run, array x bitmap, and array x array arms removed repeated per-value dispatch and avoided materializing result containers.

The array x array cardinality arm shares the materializing arm's merge and gallop choice. This keeps the algorithmic decision in one module and prevents two implementations from drifting while still returning the same number.

### Relation predicates

`is_disjoint` and `contains_all` once had only bitmap x bitmap specializations. True-result workloads exposed the missing arms because false answers short-circuit early. Representative improvements included run x run `is_disjoint` from 124.2 us to 58 ns and `contains_all` from 86.6 us to 57 ns.

Ordered-pair regression cases cover coincidence, straddling, strict containment, adjacency, and disjointness. Random generators alone do not promise to reach interval-alignment cases.

### Array versus bitmap time crossover

Bitmap intersection is a fixed 1024-word loop. Array intersection is linear in cardinality and branch-sensitive. Measured crossovers after the direct array arm were about 118 elements resident and 80 elements streamed, far below `ARRAY_MAX`.

That does not justify per-pair promotion. Allocating and zeroing the bitmap costs more than the saved intersection for every legal cardinality on a single use. Promotion becomes plausible only when the same operand participates in several intersections.

### SIMD, skew, and representation-aware controls

The scalar array merge does not auto-vectorize because both cursors advance data-dependently. On AArch64 an 8x8 all-pairs NEON block made array intersection roughly 1.8x to 6.0x faster from cardinality 256 to 4096. The honest production comparison uses distinct pairs: reusing one pair lets the branch predictor memorize the scalar path and understated its cost by 4.4x at cardinality 1024.

Bitmap word kernels use independent accumulators to exploit instruction throughput. Resident `and_cardinality` improved 1.77x, but the like-for-like distinct-pair gain was about 1.37x because the optimized loop moved more of the bill to fetching 16 KiB of operands. `is_disjoint` and `contains_all` use blocked short-circuiting loops; an iterator reduction had cost more than the stronger count it was meant to avoid.

Run x run first removed repeated accessor and bounds-check work, then added galloping when interval counts differ by `GALLOP_RATIO`. The skew fixture runs both operand orders because the materializing closure once made a commutative operation 1.8x asymmetric. Balanced high-run counting uses an 8x8 NEON block; materializing output and skewed probes remain scalar because their ordering or asymptotic work differs.

High-run containers are not universal. Standing E2E shape coverage shows them in clustered burst regimes, not uniform scattered, append-shaped, or aged point-edit data. The representation decision itself has three gates: interval capacity, raw bytes, and the 12.5% optimize gain.

A proposed compressed-binary-trie fourth container kind was declined. Its optimistic model saved at most 6.6% selectively on the measured corpus, lost across the bitmap-dominated middle density, and would add seven kind-pairs plus export normalization. Increasing chunk width grew the trie margin only because its array baseline degraded; total bytes per ordinal stayed essentially flat.

## Files

- `yesno-core/src/ops/generic.rs` - semantic oracle.
- `yesno-core/src/ops/{array,bitmap,run,mixed}.rs` - specialized materializing kernels.
- `yesno-core/src/ops/card.rs` - non-materializing cardinality and relation dispatch.
- `yesno-core/src/ops/nary.rs` - eager n-ary operations.
- `yesno-core/benches/setops.rs` - kind-pair, predicate, and crossover measurements.
- `e2e/scenarios/container_shape.py` - standing `(cardinality, run count)` corpus gate.

## Test Coverage

- `every_specialized_kernel_matches_the_generic_oracle` covers all kind pairs and both operand orders.
- `cardinality_identities_agree_for_every_kind_pair` anchors counting to materialization, which is itself anchored to the generic oracle.
- Ordered relation cases explicitly cover interval geometry the random generator may miss.
- `tests/allocation.rs` ensures cardinality identities do not decay into result construction.

## Pitfalls

- Do not let a feature-gated dispatcher be the only route a test takes into a SIMD kernel. On a host reporting the feature absent every such assertion degrades to `scalar == scalar`, and six kernels were once completely breakable with the suite green. Add a direct-call wrapper — `#[cfg(target_arch = ...)]`, assert the feature, `// SAFETY:` naming the bound — **alongside** the dispatcher assertions, and prove it with a two-condition sabotage table: each kernel broken in turn, run once on the real host and once with every dispatch gate forced to `false`.
- Do not read `assert_eq!( reached, N )` as evidence a vector arm executed; counters like that measure operand pairs clearing a *length threshold*, which is selection, not execution.
- A correctness property cannot reveal a missing specialization when both paths return the same value.
- Do not benchmark only array x array and bitmap x bitmap when deciding whether mixed kernels matter.
- Do not infer a promotion policy from the kernel crossover without charging conversion and reuse.
- Do not quote a kernel number from a fixture that repeats one operand pair.
- Run x run benchmarks must run both operand orders and must ensure the reference is actually run-optimized.
- After adding a materializing arm, inspect `ops::card`, `is_disjoint`, and `contains_all` separately.

