# Query Acceleration Admission Synthesis

## Summary

SIMD, fused-DAG JIT, and GPU offload accelerate different shapes of set work. Admission must account for the entire public query path, including traversal, materialization, compilation, launch, copying, and reuse. A bounded hotspot trace and exact reuse-distance analysis provide the missing workload evidence; synthetic hit rates are conditional examples, not production measurements.

## Included Documents

| Document | Focus |
|---|---|
| [GPU Offload on Unified Memory](./gpu-offload-on-unified-memory.md) | Kernel and wired offload economics, deferred scan contract, and cache admission. |
| [Query Hotspot Observation](./query-hotspot-observation.md) | Stable identities, bounded query capture, and exact LRU reuse distances. |
| [SIMD Arch Arms and Kernel Selection](./simd-arch-arms-and-kernel-selection.md) | Native vector evidence, JIT compile and traversal costs, and automatic admission. |
| [Expression Planning, Statistics, and Segmentation](./expression-planning-statistics-and-segmentation.md) | Sound planning facts, statistics budgets, materialization costs, and source gates. |

## Stable Knowledge

- A fast inner kernel is not a fast terminal. On GB10, a batched GPU kernel reached 6.7-11.1x over twenty CPU cores on its probe, but the first wired 256-chunk, 64-filter OpenCL path took 10.01 ms against 8.71 ms on CPU. Per-chunk launches and host plumbing consumed the gain. Deferred enqueue and one fallible flush brought the warm public path to 3.35 ms against 8.60 ms ( 2.56x median, with a 1.61-4.71 ms warm range ); cold fill was 8.15 ms. This is one blocked-view workload on one GB10 host.
- `Accelerator::enqueue` either accepts ownership of a chunk or declines; `flush` returns every accepted count or an error. Queued slots stay pinned, eviction skips them, a full pinned cache declines, and dropping an unfinished counter releases pins. A failed flush must never look like an empty answer.
- GPU batch value comes from reusing a resident set against many filters, not from one Boolean operation. On GB10, one core measured 31 GB/s, twenty cores 102 GB/s, and the GPU 169 GB/s for dense AND-cardinality; an empty launch plus sync cost 5.10 us. A launch per 8 KiB container lost about 41x to one launch over a pointer table. CUDA and OpenCL measured within 0.99-1.03x on that device.
- JIT admission is likewise a whole-expression decision. An older 256-chunk-only rule admitted a selective AND that decoded 257 payloads where core decoded two. Automatic admission now requires at least four leaves before source scans; seek-driven explicit traversal and fallback preserve sparse behavior. Four-leaf dense fixtures retained roughly 0.64-0.71 of core's whole-expression time, while a simple aligned binary bitmap AND lost through explicit JIT. The 256-chunk condition alone is not a general safety bound.
- SIMD value depends on the instruction set and the public terminal. Native AArch64 and x86 measurements are distinct evidence; emulation can check correctness but cannot price throughput. A fallback that returns the right answer can conceal an unreachable fast arm, so direct differential tests and arm-off controls matter.
- The planner's exact facts can justify sound rewrites; approximate sketches cannot justify dropping rows. Its statistics and segmentation setup cost real work, with the paged-source segmentation gate at 128 chunks per operand after a crossover near 88. `cardinality_cost` prices visited chunks but omits materialized cardinality, so equally priced forms have differed by 214x in execution.
- The observer keys container recurrence by wire posting-list key plus container index and planned-shape recurrence by a leaf-collapsed postfix fingerprint. A `KeySource` pointer is not stable across queries; JIT shape reuse must retain OS thread identity because its 64-entry cache is thread-local. Captured container reuse is an upper bound because an index substitutes for the actual prefix.
- A contiguous `yesno::hotspot` TRACE window of at most 250,000 queries preserves reuse distances inside that window. Sampling would change the distances. The synthetic 16 MiB cache case achieved 78.2% hits and 25 fill bytes per operation with five-observation admission and a 20,000-access half-life, versus 72.0% and 2,295 bytes for fill-on-miss. Those figures do not establish a real traffic hit rate or justify changing `AUTO_MIN_CHUNKS`.

## Operational Guidance

First identify the production terminal and count the work it actually performs: decoded payloads, seeks, allocations, compilation, launches, and transfers. Compare complete calls against the shipped fallback on the same corpus, rotate arms when possible, and preserve warm, cold, and error cases separately. Keep planner soundness proofs apart from profitability evidence.

For reuse-based admission, capture a bounded contiguous real query window and compute stack distances by capacity and by worker thread. Use it to test GPU residency and JIT shape recurrence before changing a threshold. The GPU source note's opening says no crate code was written; its later production section supersedes that statement and documents the `yesno-opencl` satellite and deferred contract. The observer exists, but the real workload distribution is still unmeasured.

## Files

- `yesno-core/src/{jit,hotspot}.rs` - optional fused-DAG generation and bounded trace events.
- `yesno-core/src/stream/{plan,sketch,dynamic}.rs` - logical rewrites, statistics, and physical lowering.
- `yesno-core/src/ops/` and `yesno-core/src/view/` - scalar and architecture-specific set and view terminals.
- `yesno-flight/src/expr.rs` - stable wire-key extraction and hotspot capture on the query path.
- `yesno-opencl/src/lib.rs` - the satellite accelerator backend and deferred scan implementation.
- `.agents-workspace/tmp/hotspot-observer/` - scratch exact reuse-distance and replay instrument, if retained.

## Tests

- `cargo test -p yesno-core --features tracing` and `cargo test -p yesno-flight` cover bounded capture and its real query-path hook; the observer's linear differential and Fenwick-growth regression check stack distances.
- `cargo test -p yesno-core` covers planner equivalence, stream behavior, JIT fallback, and direct scalar-versus-vector properties where the architecture can execute them.
- `cargo bench -p yesno-core --bench setops` and the relevant view and DAG benches are measurement tools, not correctness gates. For GPU admission, compare the wired blocked-view terminal with CPU and a host-backend control on the same fixture.

## Pitfalls

- Do not infer a whole-query win from a kernel ratio, a cache simulation, or one synthetic shape.
- A false admission can be costly even when the fast path is correct. Count selective traversal and cold compile or fill work.
- A cache threshold and decay window form one policy; raising a threshold can cut hit rate when capacity binds.
- Do not combine shape events across worker threads or sample away intervening accesses when computing reuse distance.
- Do not port an AArch64 speedup to x86, or treat a QEMU timing as a native one.

