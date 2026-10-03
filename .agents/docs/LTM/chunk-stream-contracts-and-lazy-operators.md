# Chunk Stream Contracts and Lazy Operators

## Summary

`ChunkStream` is the lazy, prefix-ordered representation used to evaluate set expressions without materializing intermediates. Its wide interface exists so an operator can ask the narrowest question it needs: prefix, cardinality, chunk, seek, whole-stream count, or physical statistics.

## Key Facts

- Conforming streams emit strictly increasing prefixes, never repeat a prefix, and never emit an empty container.
- `peek_prefix` is a lower bound, not a promise that `next_chunk` will produce that prefix.
- Operators use one-slot lookahead so their own `peek_prefix` is exact even when children cancel.
- `seek` is what realizes galloping and leapfrog bounds; a scan-only stream cannot inherit those asymptotics.
- `next_cardinality` advances without requiring a `Container` on optimized paths.
- `cardinality_dyn` is a terminal override and must not fall back to `collect_set().len()`.
- `BoxedStream` must delegate every capability. A valid default can silently erase an optimization after type erasure.
- Unbounded complement is viable only lazily. The eager `OrdSet::not()` was removed because it necessarily walks about `2^48` chunks.
- `Snapshot::key_stream` is the production paged leaf. It resolves visible chunk references once, decodes payloads only as requested, and carries cardinality metadata without reading payload bytes.
- `Expr::Source` holds a re-openable `ChunkSource`, not a stateful stream. Database sources share one immutable plan so reopening is constant-time in chunk count.
- A failed source open becomes `ErrStream`, never `EmptyStream`; an evicted or corrupt snapshot must not turn into a silently short result.

## Details

### Production sequence and canonicity

For each set there is one conforming sequence of non-empty fibers in ascending prefix order. This makes observational equality of conforming streams coincide with equality of the represented sets. A malformed stream can still collect to the correct `OrdSet`, so denotational oracle tests alone cannot enforce conformance.

`tests/stream_conformance.rs` therefore observes the production sequence directly. It includes negative controls for repeated, descending, and empty-prefix output and non-vacuity guards for cancellation cases.

### One position, several questions

| Method | Required answer | Optimized behavior |
|---|---|---|
| `peek_prefix` | Lower bound on the next surviving prefix | May fill operator lookahead |
| `next_chunk` | Prefix and payload | May materialize or clone a shared container |
| `next_cardinality` | Prefix and cached length | Avoids returning a payload where possible |
| `seek` | Advance to a lower-bound prefix | Gallops on indexed leaves |
| `cardinality_dyn` | Total remaining cardinality | Uses identities or cached lengths |
| `stats` | Remaining physical shape and backing | Guides lowering without changing semantics |

The materializing defaults preserve compatibility, but every performance-sensitive implementation must decide which methods it can answer more cheaply.

### Complement

The universe excludes `u64::MAX`, so the full cardinality fits in `u64`. `Not` is a dedicated bounded-relative-complement stream rather than `AndNot(RangeStream, S)`: the latter drives from the range and takes `2^48` steps for an unbounded complement. Counting, membership, minimum, emptiness, and early chunks can be `O(input)`; draining or asking for the maximum may still be `O(range)`.

A timeout intended to detect an unbounded walk must enforce the deadline around all work, including warm-up, rather than measure elapsed time after a call that may never return.

### N-ary operators

`UnionAll` flattens three or more contiguous OR leaves and merges each prefix with one reusable scratch buffer. Its cardinality path distinguishes a sole contributor from overlapping contributors using peeks before fetching payloads.

N-ary intersection experiments showed that strategy matters: converge-then-fold can pull all operands after the result is already empty, while an eliminating fold stops immediately. No single experimental variant dominated every share-rate regime, so those prototypes remain research input rather than a shipped universal operator.

### Database-backed lazy leaves

`Snapshot::key_stream` plans one key by scanning its index range and merging the bounded memtable overlay over the visible on-disk references. The overlay is collected eagerly on purpose: streaming from the memtable and store at the same time would introduce the first lock ordering between them, while the disk side is the unbounded work the API exists to avoid.

Each plan step carries its prefix, `ChunkRef`, and cardinality. `next_chunk` takes and releases the store lock per chunk and decodes one payload; `next_cardinality`, `cardinality_dyn`, and the hint path consume cached lengths without reading payloads. The stream checks snapshot liveness on every chunk because a stream can outlive its originating `Snapshot`, and it owns a clone of the reader slot so dropping that snapshot value does not release the protected root.

`Snapshot::key_expr` wraps this behavior in `KeySource`, an implementation of the `ChunkSource` factory declared by `stream/`. The dependency remains one-way: `stream` defines the abstraction and `db` implements it. A factory is forced by expression semantics because `Expr` is clonable and planning may open an operand repeatedly, whereas a `ChunkStream` is a mutable cursor.

The source plan is immutable behind an `Arc`. Reopening the same source is therefore about 61 ns and flat in chunk count, rather than the obsolete 0.8 us, 3.8 us, and 35 us index rescans measured before the plan was shared. On a 600-chunk key intersected with a five-chunk key, the eager path used 1,994 allocations and the lazy expression 169. The allocation layer is load-bearing because both paths return the same set.

`KeyStream::chunks_remaining` is the single spelling of `steps.len().saturating_sub(idx)` and feeds `stats`. The saturation is defensive against a multi-site cursor invariant, not a branch known to be reachable: construction, advance, seek, and terminal counting all maintain `idx <= steps.len()`.

## Files

- `yesno-core/src/stream/mod.rs` - trait contract and terminal operations.
- `yesno-core/src/stream/leaf.rs` - set and range leaves.
- `yesno-core/src/stream/ops.rs` - binary operators and lookahead.
- `yesno-core/src/stream/nary.rs` - `UnionAll`.
- `yesno-core/src/stream/dynamic.rs` - expression lowering and `Concat` / `Restrict`.
- `yesno-core/src/db/keystream.rs` - immutable key plans, paged streams, and the database `ChunkSource` implementation.
- `yesno-core/tests/stream_conformance.rs` - production-sequence conformance.

## Test Coverage

- `tests/expr_equivalence.rs` compares lazy and eager semantics and cardinality.
- `tests/allocation.rs` guards non-materializing terminal paths.
- `a_key_stream_does_not_decode_the_key_it_streams` and `a_lazy_leaf_does_not_decode_what_the_operator_skips` pin the paged source's allocation behavior.
- `tests/stream_conformance.rs` checks ordering, uniqueness, non-empty fibers, and `next_cardinality` agreement.
- Spy streams assert which question an operator asks when allocation counting cannot observe reference-count traffic.

## Pitfalls

- Never use `peek_prefix` as proof that `next_chunk` returns a chunk.
- Never omit delegation from `BoxedStream` for a new capability.
- An allocation counter cannot detect every unnecessary payload fetch once containers are frozen and cloning is a refcount bump.
- Do not use an eager whole-universe complement API.
- Do not rebuild a `KeySource` plan on each open or replace a source-open error with an empty stream.
## Bounded seeks through disjoint concatenation

`concat_disjoint_or` once folded sorted disjoint ranges into a left-deep binary `Concat` chain. `Concat::seek` descended into both children even when a subtree's upper bound already proved it could not contain the target. In `And( Key, Or( 399 disjoint ranges ) )` over 511 selected bitmap chunks, repeated seeks alone took 0.922 ms, while all 511 bitmap/run cardinality kernels took 0.083 ms. The count walk took 1.862 ms, slower than a stored bitmap mask's 0.223 ms for the same answer.

Both concatenating call sites now use `ConcatAll`, and the uncalled public binary `Concat` was removed. Each part carries a sound upper prefix bound so `seek` skips parts whose bound excludes the target. The count walk fell to 0.198 ms; repeated seeks fell to 0.0232 ms. At 102 ranges, count fell from 0.1307 to 0.0426 ms. An overlapping 256-segment `Restrict` control also improved 0.321 to 0.0197 ms. The bound may be wider than a part's true extent but never narrower, or it would skip live chunks.

The diagnostic test measures work rather than answers: both operators emit identical chunks. Rebuilding a pairwise chain *out of bounded `ConcatAll` pairs* still passed the seek budget, revealing that flatness was not the essential property. Restoring the old unbounded `Concat` failed with 2,143 leaf seeks against a budget of 256. The assertion is therefore that a seek never descends into a part its bound excludes, alongside the ordinary stream conformance and expression-equivalence oracles.
## Fail-closed, prefix-bounded key streams

`KeyStream` plan construction propagates B+tree cursor errors instead of omitting an unreadable entry and returning a short success. A persisted node-corruption regression fails both full and bounded construction on the old path. Payload errors remain deferred until the affected chunk is advanced. `Snapshot::key_stream_prefix_range` bounds both the index and MVCC memtable walks to a half-open chunk-prefix interval, accepts `2^48` as the legal exclusive end, and still checks snapshot liveness for an empty range. The oracle covers tombstones, old and new snapshots, overlays, the terminal prefix, count, seek, and a stream surviving its snapshot handle. An allocation regression distinguishes an eight-chunk plan from a 4,096-chunk full-key plan; replacing either bound was sabotage-verified.

The per-shard store mutex was narrowed for snapshot index walks. A published root cannot reach `pending_nodes`, so a reader can clone the immutable segment handle and node size under the lock, then walk without holding the outer mutex. A consumer's eight-thread, four-shard stress case improved from 182.9-206.5 to 121.1-156.9 us per 23 opens, about 27%; its real workload spread over 32 shards improved only 80.1 to 77.1 us. The larger 40% share found in a separate fixture applied to its longer 60-chunk scans, not to the consumer's workload. Lock benefit depends on both hold duration and sharing concentration.
