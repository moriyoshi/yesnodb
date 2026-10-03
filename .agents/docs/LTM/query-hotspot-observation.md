# Query Hotspot Observation

## Summary

The hotspot observer measures recurrence of posting-list containers and planned expression shapes so GPU residency and JIT admission can be judged against actual traffic. A standalone research crate computes exact reuse distances; a bounded `yesno::hotspot` tracing capture supplies a replayable stream from the running query path. Synthetic results establish that reuse can be substantial, but they do not establish a production hit rate.

## Key Facts

- The research crate is `.agents-workspace/tmp/hotspot-observer/`. It uses a Fenwick tree of last-access slots to compute exact LRU stack distance in `O( log T )` per touch. One histogram gives the hit-rate curve for every cache capacity.
- Container identity is the posting-list key plus a container index. The key comes from the wire expression before lowering; a `KeySource` pointer changes on each query and would falsely report zero recurrence. The index is a proxy for the actual prefix, so captured container reuse is an upper bound.
- Shape identity is a postfix fingerprint of the planned `Expr`, with leaf identities collapsed. It approximates the private `DagJit` instruction key. Shape events are tagged with the OS thread because the 64-entry JIT cache is thread-local.
- The final capture uses `tracing` target `yesno::hotspot` at TRACE. There is no bespoke trace file or `YESNO_HOTSPOT_TRACE` switch. A capture records a contiguous window of at most 250,000 queries, emits `hotspot.done`, and disarms itself.
- The real workload distribution remains unmeasured. `jit-auto-min-chunks-rests-on-a-withdrawn-number` in `TODO.md` tracks the capture needed before changing `AUTO_MIN_CHUNKS`.

## Details

### Why reuse distance is the primitive

A hit rate assumes a cache size and policy. The LRU stack-distance histogram belongs to the access stream itself; summing distances below a capacity yields that capacity's hit rate. The observer feeds the same counter with container and shape keys. Its first Fenwick implementation grew by appending zero nodes, but a newly created node covering earlier accesses did not inherit their sum. A 4,000-access differential against a linear oracle caught the failure at `t = 2047`; growth now rebuilds from raw slots in `O( n )` per doubling.

The generator varies query templates (`--shapes`), the touched prefix window (`--window`), and popularity drift (`--drift`). A single template would force perfect shape reuse, and a query touching every chunk would make container recurrence identical to set recurrence. A decay parameter on a stationary stream would have no meaningful job.

### Synthetic result and its limit

The recorded corpus has 64 sets of 128 chunks ( 8,192 containers, 64 MiB ), 20,000 queries, arity up to eight, a 16-chunk window, and 200 templates collapsing to 113 planned shapes. A 64-entry shape cache hits 73.0% at zero skew and 94.8% at Zipf 1.2. These values describe the synthetic template library, not a deployment.

At a 16 MiB container budget and Zipf 1.2, admission after five observations with a 20,000-access half-life achieved 78.2% hits and 25 fill bytes per operation. Fill on miss achieved 72.0% hits and 2,295 fill bytes per operation: a 92x difference in fill traffic. The threshold and half-life are coupled; admission after ten with the same decay fell to 64.2% hits. At a 1 MiB capacity, a higher threshold reduced hits, so admission is not a free win when the working set exceeds the cache.

### Capture path and cost

`yesno-flight` extracts posting-list keys from `SetExpr` before lowering, then calls the semver-exempt `yesno-core::hotspot` API. Core emits once per key per capture for chunk dimensions and once per query for the key list and shape. Sending `( key, chunk_count )` instead of expanding up to 4,096 hashes per leaf reduced the trace payload by roughly three orders of magnitude. A caller checks `hotspot::enabled()` before allocating the key list.

The capture originally used files, a custom environment switch, and per-thread file naming. Review moved it to the existing `tracing` channel. With the target filtered out, `tracing` rejects the event at its level guard before formatting; the measured idle cost was within a roughly 30 ns code-layout band. Enabled capture measured 2,107 ns against 817 ns for a cheap key query ( 2.58x ), and 7,265 ns against 5,553 ns for a two-key intersection ( 1.31x ). It should be enabled for a bounded diagnostic window.

Sampling events would invalidate stack distance: dropping an intervening access changes the measured distance, not merely its precision. A contiguous 250,000-query window preserves exact distances shorter than the window. The capture budget is tested under eight concurrent threads; a spent capture returns to the idle cost.

The move into core exposed a build-configuration trap. Gating the whole `hotspot` module on core's optional `tracing` feature compiled under Cargo's unified features but failed Bazel, which builds core without propagating Flight's dependency feature. The module is now unconditional and its emission is feature-gated internally; without tracing, `enabled()` is constant false. Core keeps the wire-shaped key extraction out of its dependencies.

## Files

- `yesno-core/src/hotspot.rs`: bounded capture, shape fingerprinting, and tracing events.
- `yesno-flight/src/expr.rs`: extracts stable posting-list keys and calls the capture API on the query path.
- `.agents-workspace/tmp/hotspot-observer/`: standalone exact reuse-distance counter, simulator, and trace replay tool.
- `.agents/docs/LTM/gpu-offload-on-unified-memory.md`: GPU admission question that shares this measurement.

## Test Coverage

The observer has a linear differential oracle over 4,000 accesses and a named Fenwick-growth regression. Core tests cover the budget under concurrency and one dimension event per posting list. A live Flight integration test proves the query path emits replayable events; removing its hook makes that test fail. The parser is tested against real `tracing_subscriber::fmt` output and distinguishes `key` from `keys`.

Run `cargo test -p yesno-core --features tracing` and `cargo test -p yesno-flight`; run the observer crate's tests from `.agents-workspace/tmp/hotspot-observer/` if that scratch directory is present.

## Pitfalls

- A `KeySource` address is not a stable identity across queries or snapshots. Using it yields a convincing but false flat distribution.
- The container trace substitutes ordinal container indices for physical prefixes, counts only all-bitmap leaves, and caps a leaf at 4,096 containers. Derived hit rates are upper bounds.
- Merging shape events across threads overstates reuse in a thread-local JIT cache.
- Do not sample the trace or route it through a span sampler. Stop after a contiguous budget instead.
- `release_max_level_*` would compile TRACE capture out of release binaries, defeating the purpose of a production capture point.
