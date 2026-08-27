# Allocation, Reclamation, and Fsck

## Summary

Space recovery depends on durable slab occupancy, correct supersession accounting, and three independent obligations: no pinned root can reach an extent, recovery cannot select a root that reaches it, and no materialized zero-copy alias remains. `fsck` is the reconstruction path that ties index reachability back to allocator state.

## Key Facts

- Slab occupancy is durable metadata, but the committed index is the authoritative liveness map at open.
- Deferred frees are in memory. Open-time allocator rebuilding prevents pending extents from becoming permanent orphan slots after restart.
- Packed-page live bytes must use the run count when sizing run payloads.
- Index nodes occupy allocator slots and must be marked by `fsck` alongside payload extents.
- Used-but-unreferenced deferred extents are `pending`, not allocator leaks.
- Reclamation is gated by checkpoint reachability, A/B superblock delay, and live buffer pins.
- Version watermarks are not a sound proxy for which root a concurrent snapshot captured.
- The soft space-amplification threshold observes only: it emits an edge-triggered event and a gauge, and aborting the oldest reader remains the sole intervention.
- Ratios crossing the event boundary are integers in parts per thousand, because the event type derives equality and floats break deduplication.
- Idle reclamation requires a real superblock flip. When deferred extents are queued, the idle path performs the bounded flips needed to advance recovery safety; a truly quiescent database remains silent.
- Online reads verify each stored node, packed page, or standalone payload once per physical offset; `fsck` uses raw node access so corruption remains attributable rather than stopping reconstruction at the first error.

## Details

### Durable occupancy and open-time rebuild

Persisting slab bitmaps made occupancy durable but did not make free space reusable by itself. Reuse arrived only when the allocator retained active slabs across generations and could recycle empty ones.

The deferred-free queue is not durable. At open there is one committed root, no readers, and no live deferred queue, so rebuilding occupancy from index reachability is sound. Adoption refuses any report with dangling references or read errors and skips opaque slabs.

### Reclamation obligations

The stable formulation is in terms of obligations rather than the implementation's current predicates:

- Reachability: no live snapshot owns a root from which the extent is reachable.
- Recovery visibility: neither A/B superblock choice can lead recovery to the extent.
- Alias safety: no materialized `Buffer` still pins the extent.

Reader checkpoint sequences are captured with roots under the same shard lock. This closes the concurrency window where a snapshot could publish a newer version while capturing an older root, allowing a recycled index node to appear as a coherent newer tree.

The historical `safe_version > obsolete_at` gate was removed because checkpoint sequence directly represents the relevant root generation.

Idle reclamation cannot be write-free: the older durable superblock still reaches the superseded extent until it is overwritten. The idle checkpoint path therefore flips only while deferred work is queued. Advancing `checkpoint_seq` carries the queue past `RECLAIM_CKPT_DELAY`, after which the gate stops firing. This bounds cleanup after activity ends to at most two 4 KiB superblock writes plus `fdatasync` per shard.

### The space-amplification policy observes; it does not intervene twice

Space amplification has a hard bound enforced by aborting the oldest reader, and a softer threshold that was designed alongside it. That softer threshold was deliberately resolved as **observe only**: crossing it emits an event and moves a gauge, and evicts nothing and stalls nobody. Aborting the oldest reader stays the single intervention, because a second way to end a query is a second thing an operator must reason about when a query dies, and the observable half delivers the diagnosis without that cost.

Five decisions inside it are worth keeping, each with its reason.

Threshold and amplification are carried as **integers in parts per thousand rather than floats**. The core event type derives equality so subscribers can compare and deduplicate events, and a float makes that impossible or subtly wrong. Two thousand is the hard bound; the default soft threshold is one thousand two hundred and fifty.

The events are **edge-triggered, not level-triggered**. The evaluation point is every checkpoint, so a level-triggered event would fire once per checkpoint for as long as a single long-running reporting query holds the floor down.

Observation is a **separate operation from enforcement, and runs before it**. Running it afterwards would measure the state the eviction had just produced, which is precisely not the state worth reporting.

Snapshot ages are **wall-clock, not monotonic instants**, and that is the opposite of the usual rule. Nothing inside the engine compares them; they exist to be read on a dashboard in wall-clock terms. A clock step therefore misreports an age and cannot corrupt anything, whereas the same trade would be wrong for any internal deadline.

**Evicted readers are skipped when naming the oldest one.** An evicted reader no longer pins retention, so naming it points an operator at a query that is not the problem. That rule was documented and untested until a sabotage exposed it, which is the general hazard: a skip condition has no failing case unless a test constructs one.

There is deliberately **no hard age limit** to match the hard space bound, for the same reason the soft threshold observes rather than evicts.

### Online verification and diagnostic reconstruction

The read-path checksum cache and `fsck` answer different questions. Production reads use `SegmentedMmap::verify_once` and refuse corrupt bytes before decoding them. The cache is keyed by physical file offset because allocator IDs are reusable, and `write_at` invalidates overlapping successful verdicts after a write.

`fsck::rebuild` cannot use that refusing reader for its index walk. If the reader stops on the first corrupt node, the report loses the exact dangling, leaked, pending, and error attribution that makes it useful. `RawNodes` is a stateless diagnostic wrapper that exposes bytes to reconstruction without adding a mutable diagnostic-mode flag to `ShardStore`. The choice of reader, rather than a global mode, decides whether the operation refuses or inventories damage.

A failed online verification is never cached as success. An unrelated write does not clear the whole cache, while an overlapping write invalidates only the affected region. These properties prevent the cache from becoming either stale or a per-write global tax.

### Compaction and evacuation

Evacuation was implemented, measured, and left disabled by default. The original product-optimal threshold comes from a uniform death model, but real aged-state experiments showed that generation recycling already kept amplification near 1.0 and evacuation did not repay its copying cost.

The continuum model remains useful for reasoning about a policy if it is enabled, but it does not price index-node COW writes and does not describe default operation while `EVACUATE_PER_CHECKPOINT = 0`.

### Fsck reconstruction

`fsck::rebuild` walks chunk references and index node IDs, derives used slots and packed-page live totals, and distinguishes:

- `dangling`: a live reference points to a slot the allocator considers free;
- `leaked`: an allocated slot is neither reachable nor legitimately pending;
- `pending`: a superseded extent is retained by reclamation conditions;
- `errors`: a region could not be checked completely.

An errored or dangling rebuild must never be adopted as a partial repair.

## Files

- `yesno-core/src/store/alloc.rs` - slabs, deferred frees, recycling, and evacuation.
- `yesno-core/src/store/fsck.rs` - reachability reconstruction.
- `yesno-core/src/checkpoint.rs` - supersession and packed-live accounting.
- `yesno-core/src/db/store.rs` - open-time adoption, raw diagnostic nodes, and format-aware online verification.
- `yesno-core/src/store/segment.rs` - physical-region checksum caching and write invalidation.
- `yesno-core/src/db/mod.rs` - snapshot registry, checkpoint policy, and space-amplification enforcement.

## Test Coverage

- `tests/invariants.rs` covers storage invariants I2-I7.
- `tests/zero_copy_mvcc.rs` covers retained roots, aliases, and reclamation races.
- E2E scenarios `space_retention.py`, `ckpt_under_reader.py`, and `aged_state.py` cover operational behavior.
- `Db::verify()` exercises the integrated `fsck` path rather than only the pure rebuild helper.
- Corruption tests require `Snapshot::load` to return an error while `Db::verify` continues far enough to attribute the damaged regions.
- Idle-reclamation tests assert both that queued extents drain and that the database stops writing after the queue becomes reclaimable.

## Pitfalls

- Never free `old_nodes`; free `old_nodes - reused_nodes` after a reusing tree build.
- Do not adopt an incomplete liveness map.
- Do not conflate a reader version with the root generation it holds.
- A test that reads cardinality from index metadata cannot observe recycled payload or index bytes; use `load` and compare full contents.
- A sabotage that fails to compile proves nothing; verify that the changed program runs and the intended test fails.
- Do not make production reads and diagnostic reconstruction share a mode flag; their different error behavior belongs in distinct reader types.
