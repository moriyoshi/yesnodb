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
- **Freeing a slab must retire it from every `active` bump pointer naming it.** Both `Free` transitions -- `free_now` and `adopt_live_at_open` -- call `retire_from_active`, or two size classes allocate into one slab and one chunk's payload lands where another's trailer belongs.
- A mis-addressed extent reports as "points at another chunk" while the extent is healthy: the defect is in the addressing. `CodecError::MisPointedExtent` therefore carries key, cell, slab class, trailer offset, found tag and expected tag, because garbage at the right offset and a plausible tag at the wrong offset need **opposite** fixes.
- The verified-region cache is bounded by two generations of `VERIFIED_GENERATION` = 16 384 entries. Eviction is always safe and invalidation must stay exact, which is why the bound may be approximate and `invalidate_verified` may not.
- Packed-page sharing amplifies verification **in principle only**: under I2 a checkpoint publishes fresh pages and never rewrites a live one, so measured evictions from key sharing are zero.

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

### A freed slab must stop being its old class's bump slab

`Allocator::active[class]` is the per-class bump slab, and for as long as the allocator existed **no code cleared it when a slab was freed**. A slab reaches `SlabState::Free` at two sites -- `free_now` ( runtime, via reclamation ) and `adopt_live_at_open` ( open-time rebuild ) -- and both set the state while leaving every `active` entry naming the slab intact. `new_slab_for` then scans for a `Free` slab and re-initializes it **with the requesting class's geometry**, so:

1. Slab N is the bump slab for class 9; `active[9] = Some( N )`.
2. Reclamation empties N, `used_count == 0` sets `state = Free`, and `active[9]` survives.
3. Class 10 calls `new_slab_for`, finds N, rewrites it as a class-10 slab, sets `active[10] = Some( N )`.
4. Class 9 allocates, `active[9]` still names N, and it bump-allocates into a class-10 slab using class-9 slot arithmetic.

Two slot sizes then index one occupancy bitmap: `first_free` hands class 9 a slot overlapping one class 10 has written, and `slot_offset( N, class, slot )` maps one slot index to two cells. One chunk's payload lands where another chunk's `ExtTrailer` belongs.

**The error message pointed away from the cause.** The observed failure is `owning_class` resolving the trailer position from the slab's *single recorded class*, which is wrong for any class-9 cell in it, so the reader computes a trailer offset inside someone else's payload, finds no matching `ckey_tag`, and reports "extent reference points at another chunk" while pointing at a healthy extent. **The corruption is in the addressing, not in the data** -- which is also why the "stored" checksums in the first report were consecutive `u16` values: array payload read at a trailer offset.

`retire_from_active( id )` clears every `active` entry naming a slab and is called at **both** `Free` transitions. Document-major churn, 24 rounds:

| keys | errors | leaked | dangling | read-back |
|---|---|---|---|---|
| 512, before | 72 | 342 | 1 | failed |
| 512, after | 0 | 0 | 0 | ok |
| 1024, before | 95 | 239 | 1 | failed |
| 1024, after | 0 | 0 | 0 | ok |

**Why every existing layer was blind.** The bug requires a slab to be emptied and then a *different* class to allocate. Every test in the tree grew a database monotonically -- slabs fill and never empty, and `new_slab_for` never reuses one. The differential and proptest layers are structurally incapable of seeing it: they compare *contents* against an oracle, and the contents were correct until two live extents overlapped.

**The measurement consequence is separate from the data-integrity one and is easy to miss.** The tag and CRC checks are skipped for unaligned cells and packed slabs, so a small corrupt index does not error -- it answers slightly wrong. A depressed recall figure produced that way is plausible, in range, and has no signature. Any measurement taken on a store that was never `verify()`d is not a measurement.

### Bounding the verified-region cache

`SegmentedMmap::verified` originally had no bound, no eviction and no TTL: entries were inserted on first verification and removed **only** by an overlapping write, so the set grew with the number of distinct regions ever read -- measured at 52 / 100 / 148 / 198 entries for 1000 / 2000 / 3000 / 4000 distinct keys, with no plateau, bounded only by the database's page count. A full scan populates one entry per page and holds it for the process lifetime.

**The design rests on an asymmetry: evicting is always safe, keeping is not.** Dropping an entry costs one recomputed CRC over at most `MAX_VERIFIED_SPAN` bytes and can never produce a wrong answer; retaining an entry whose bytes have changed is exactly the failure the cache exists to prevent. So the bound may evict as freely as it likes, while `invalidate_verified` must stay exact -- and it clears **both** generations, since an entry surviving in the older one is just as stale.

**Two generations rather than LRU or clear-on-full.** True LRU needs a per-entry clock the read path would maintain plus a way to find the minimum, which is a second structure. Clearing at the cap discards the hot set wholesale, so the next sweep re-verifies everything at once. Two generations need no clock: a hit in `old` is promoted to `young`, and when `young` fills, `old` is dropped and `young` replaces it. Anything used since the last rotation survives and at most half the entries are lost at a time. `VERIFIED_GENERATION` is 16 384, so at most 32 768 entries -- roughly 0.8 MB per segment -- sized for the **hot set** rather than the corpus, since a miss is a single CRC over at most 8256 bytes. It is a constant rather than a `DbOptions` knob, because a knob is public API under R1 / R6 / R7 and nothing has shown a workload needing a different number.

### Packed-page sharing: the mechanism is real, the runtime cost is not

One packed page carries chunks for several keys and the read-path cache keys on the **page** ( `verify_once( base, PAGE )` ), which predicts that a write touching the page invalidates verification for every key in it, with cost scaling as keys-per-page.

**Measured evictions were 0 in every configuration.** Keys-per-page of 64, 21.3, 9.1 and 2.0 -- payloads 8 B, 32 B, 128 B, 400 B, 1800 B, all Array containers -- one key rewritten through `insert_many` plus `checkpoint`, read sweep against a **reopened** database so nothing is served from the memtable. Re-verifications after the write were a constant **3** ( the new page plus two rewritten index nodes ), not proportional to sharing.

**The reason is I2 and it is structural.** A checkpoint builds pages fresh at newly allocated cells and publishes them immutably, so a live page is never written a second time. The test that pins the amplification reaches it by calling `write_at` on a live page **directly**, which the engine never does. Under sustained churn -- 400 keys rewritten every round for 12 rounds, where cells *are* recycled -- eviction ran at **one region per checkpoint**, independent of keys-per-page, beginning at round 4 ( consistent with `RECLAIM_CKPT_DELAY` ) and only on pages already dead. Packing is deterministic given a `ChunkKey`-sorted dirty set, and a page is sealed and written whole and once, so there is no partial-page write to contend at 4096-byte granularity either.

**Packing is an MVCC question, not only an allocation one.** A page is repacked *whole*, so rewriting key `A` produces a new page carrying a fresh copy of untouched key `B` -- one key's write moves another key's bytes -- and a snapshot taken before the rewrite must keep reading both through the previous page.

### `is_clean()` is the definition of consistent, and a field subset is not

`FsckReport` exposes eight fields and one predicate, and the predicate checks **five** of them: `leaked`, `dangling`, `dangling_nodes`, `packed_live_mismatch`, `errors`. The struct's doc comment used to define consistency as "empty `leaked` and `dangling`", naming two -- and a consumer's churn suite, whose entire purpose was driving free-then-reallocate, asserted on four fields individually and **never looked at `leaked`**, which is precisely where that workload produces defects. Nothing was leaking, so it passed.

The repair is the doc, not a field: a consumer reaches for the fields with obvious names, so the definition has to point at the total predicate. The fields answer three different questions, which is why choosing a subset is easy to get wrong -- **waste** and recoverable ( `leaked` ), **corruption** and never acceptable ( `dangling`, `dangling_nodes`, `packed_live_mismatch`, and `errors`, a region that could not be checked at all ), and **retention** and not a defect ( `pending`, whose own doc already said so and was correctly left alone by the same consumer ). Assert `is_clean()`; read the fields to *attribute* a failure, not to define one.

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
- `a_reused_slab_is_no_longer_the_bump_slab_of_its_old_class` and `a_slab_emptied_at_open_is_no_longer_the_bump_slab_of_its_old_class` -- **one per `retire_from_active` call site**, each sabotage-checked against its own site. The first version covered only `free_now`, and deleting the open-time call left the entire suite green at 1069 tests.
- Both assert through `owning_class` rather than comparing offsets, because `owning_class` is what the read path consults, so the test states the failure at the layer that causes it.
- `an_extent_belonging_to_another_key_is_refused_with_evidence` builds the trailer exactly as `checkpoint::run` does -- a fixture inventing its own layout would pass while the real one was broken -- and asserts every field of `MisPointedExtent`.
- `e2e/scenarios/slab_reuse.py` is the gate-level regression: it rewrites every key every round at a cardinality walking a ladder of size classes, and asserts its own ladder exceeds `PACK_MAX` before it starts.
- `one_packed_page_is_verified_once_for_every_key_it_holds` and `a_write_into_a_packed_page_invalidates_it_for_every_key_in_it` pin the sharing mechanism; `a_checkpoint_evicts_no_verified_region_however_many_keys_share_a_page` pins that the write path does not reach it. The third is not sabotage-verifiable on its own -- its guard is the pairing, and either alone is misleading.
- `a_snapshot_reads_packed_neighbours_at_its_own_version` covers MVCC across a shared packed page, rewriting only **half** the keys ( a round rewriting every key leaves no survivor sharing a page with a rewritten neighbour, which is the whole subject ) and asserting the two snapshots disagree somewhere.
- `the_verified_set_is_bounded_however_many_regions_are_read` plus a promotion test, each sabotage-checked against its own half -- remove the rotation, remove the promotion.

## Pitfalls

- Never free `old_nodes`; free `old_nodes - reused_nodes` after a reusing tree build.
- Do not adopt an incomplete liveness map.
- Do not conflate a reader version with the root generation it holds.
- A test that reads cardinality from index metadata cannot observe recycled payload or index bytes; use `load` and compare full contents.
- A sabotage that fails to compile proves nothing; verify that the changed program runs and the intended test fails.
- Do not make production reads and diagnostic reconstruction share a mode flag; their different error behavior belongs in distinct reader types.
- **A fix with two call sites needs two sabotage-checked tests.** One test covering one site is half unguarded and indistinguishable from a whole fix: the suite is green either way, and the count of passing tests goes *up* when the partial guard is added.
- **Monotonically growing fixtures cannot reach allocator reuse at all.** What reproduces it is churn that empties slabs and then allocates a different class -- document-major rewrites whose per-key density sweeps across size-class boundaries.
- A churn fixture's detection threshold is a property of **that** fixture, not of the bug. Volume was the variable in a consumer's workload ( 40 000 documents at 32 shards catching what 25 000 missed ); **rounds** are the variable in `slab_reuse.py`, where two keys suffice and four rounds do not at any width, because detection needs one full pass of the ladder plus `RECLAIM_CKPT_DELAY` checkpoints. Do not copy a calibration across fixtures.
- A cardinality ladder that stays under `PACK_MAX` ( 2028 bytes ) puts every chunk in a shared packed page -- one class for everything, no migration, and a fixture that claims to cross size classes and crosses none.
- A content-preserving write is the right way to test invalidation. Writing zeros corrupts the page, the re-read fails its checksum, and the final assertion then passes on the **error** rather than on the re-verification.
- A fixture holding the varying quantity at one is structurally incapable of seeing an effect proportional to it: with a single chunk per page, "invalidate the page" and "invalidate that key" are the same event, and the test passes against an implementation with no sharing at all.
- **"Is this mine?" is answerable by reverting**, and the answer is worth having before the search for a cause starts. Reverting a suspect recent change and observing byte-identical failures took ten minutes and redirected the whole investigation.
