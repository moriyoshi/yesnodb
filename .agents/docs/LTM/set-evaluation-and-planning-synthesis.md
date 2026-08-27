# Set Evaluation, Planning, and Packed Lenses Synthesis

## Summary

Set evaluation spans representation-specific eager kernels, conforming lazy streams, a terminating expression planner, and caller-owned packed lenses that reinterpret an `OrdSet` without changing storage. Performance comes from asking the narrowest question available while keeping semantic laws, termination proofs, and profitability heuristics separate.

## Included Documents

| Document | Focus |
|---|---|
| [Set Algebra Kernels and Cardinality](./set-algebra-kernels-and-cardinality.md) | Eager kernels, SIMD, counts, predicates, skew, and representation controls. |
| [Chunk Stream Contracts and Lazy Operators](./chunk-stream-contracts-and-lazy-operators.md) | Stream conformance, lookahead, seek, cardinality capabilities, complement, and n-ary execution. |
| [Expression Planning, Statistics, and Segmentation](./expression-planning-statistics-and-segmentation.md) | Rewrites, costs, statistics, segmentation, termination, and lowering. |
| [Packed Lenses: Matrices, Integers, and Views](./packed-lenses-matrix-bignum-and-views.md) | Matrix, integer, and view layouts, derived algebra, shared packing, and Flight lowering. |

## Stable Knowledge

- The generic value-merge kernel is the semantic oracle. Materialization, cardinality, `is_disjoint`, and `contains_all` are parallel dispatch surfaces and need independent specialization.
- Array, bitmap, and balanced run counting have measured AArch64 NEON arms. Honest array controls vary operand values; repeating one pair understated the scalar merge by 4.4x through branch-history learning.
- Run x run gallops at skewed interval counts and uses a separate block kernel when balanced. Benchmarks must optimize the reference into runs and exercise both operand orders.
- A conforming `ChunkStream` emits strictly increasing, unique, non-empty prefixes. `peek_prefix` is a lower bound unless lookahead establishes exactness; boxing must preserve seek and cardinality capabilities.
- Unbounded complement is feasible only as a lazy bounded-relative-complement stream. Eager universe materialization is structurally impossible.
- Planner soundness comes from algebra and one-sided facts, termination from the source-pinned weighted measure, and profitability from empirical costs. These are not interchangeable proofs.
- `Concat` requires strict prefix order. Arbitrary ordinal cuts can leave two pieces in one chunk and require scratch-container reassembly.
- Matrix, integer, and view layouts are caller-owned affine lenses over ordinary sets. They add no storage catalog or fourth container kind.
- `BigUint` is normalized little-endian limbs while `IntLayout` owns width. Karatsuba is measured; Toom-3 and Burnikel-Ziegler were declined until real operands exceed roughly 256 and 512 limbs.
- View folds are not restrictions. `Any`, `All`, and `Parity` are exact for union, intersection, and symmetric difference respectively; inverse-image expansion preserves the whole Boolean signature.
- Flight view transforms deliberately materialize at an eager boundary and re-enter the Boolean tree as set leaves. Lazy view expression nodes require a workload plus planner bounds, statistics, streaming cardinality, and a renewed termination audit.

## Operational Guidance

For kernel changes, audit every representation pair plus the count and predicate twins. Prove correctness against the generic path, then prove reachability and cost with a representation-specific benchmark whose construction cannot be memorized or normalized into another kind.

For planner changes, write the semantic law first, update the termination transcription, and measure allocations and representative expression shapes separately. Approximate statistics may guide cost but may not license row-dropping rewrites.

For packed lenses, keep layout descriptors outside durable storage, cover chunk and limb straddles, and preserve generic operations as oracles. Do not move a fold through a Boolean expression using the restriction law.

## Files

- `yesno-core/src/ops/` - eager materializing, counting, predicate, and n-ary kernels.
- `yesno-core/src/stream/` - lazy contracts, physical operators, planner, and statistics.
- `yesno-core/src/{matrix,bignum,pack,view}/` - packed lens implementations and shared strided access.
- `yesno-wire/` and `yesno-flight/` - validated view expressions and eager network lowering.
- `scripts/check-plan-measure.py` - source-pinned termination audit.

## Tests

- `cargo test -p yesno-core --test proptest_oracle` anchors eager algebra to the set oracle.
- `cargo test -p yesno-core --test expr_equivalence` checks planned and unplanned lazy results.
- `cargo test -p yesno-core --test stream_conformance` checks production order and canonicity.
- `cargo test -p yesno-core --test allocation` protects non-materializing terminals and plan choices.
- `e2e/scenarios/{bitmatrix,bitmatrix_gf2,bignum,bignum_series,view,server_views}.py` exercises lenses through checkpoint and reopen.

## Pitfalls

- Correctness tests cannot reveal a missing fast arm when the fallback is semantically correct.
- Never treat `peek_prefix` as an exact next prefix without the operator contract that makes it so.
- Never implement cardinality by collecting a set.
- Do not infer promotion from a pairwise crossover without charging conversion and reuse.
- Do not generalize restriction homomorphism to view folds.
- Do not add an arithmetic or planner arm merely because a mature library contains one; demand and measurement are part of the admission proof.
