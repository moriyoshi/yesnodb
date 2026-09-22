# Database APIs and Satellite Crates

## Summary

The workspace keeps storage semantics in `yesno-core` and places columnar, query-planning, transport, and scripting integrations in satellite crates. These boundaries keep the core dependency tree lean while allowing each integration to expose the strongest property the storage engine can support.

## Key Facts

- `Db` hashes into 256 virtual shards and routes them through a persisted `vshard -> shard` map in the database manifest.
- Snapshots are `Clone + Send + Sync + 'static` and keep their reader slot alive through shared ownership.
- `Snapshot::key_stream` and `key_expr` expose paged keys without materializing every container; the stream owns the reader slot and checks liveness per chunk.
- Arrow bitmap masks share a store-backed buffer when possible, while a mutable memtable bitmap is re-encoded. `bitmap_words` lends aligned bitmap words and returns `None` for either a non-bitmap or a misaligned shared bitmap.
- DataFusion lowering distinguishes `Exact`, `Inexact`, and `Unsupported`; an inexact predicate must be a superset, never a subset.
- Replication uses tonic and raw WAL frames. Query delivery uses Arrow Flight.
- A live `Db` holds an exclusive `<db>/LOCK`; a Flight server therefore prevents a second process from opening the directory.
- Since 2026-09-21 the E2E harness and core share the workspace's Rust 1.95 floor.
- `range_summary` and `len_in_range` answer complete chunks from index cardinalities and underpin DataFusion selection planning.
- Packed matrix, integer, and view layouts are caller-owned lenses over ordinary sets, not new storage formats.
- Parallelism inside a commit is opt-in and host-supplied: `DbOptions.dispatch` carries a `Dispatcher`, whose default `Sequential` spawns nothing. `yesno-core` owns no thread pool.
- Collaborators are `DbOptions` fields rather than `open_with_*` constructors, because constructors do not compose across two collaborators.
- A dispatcher that skips a task is expressible and silent, so the commit counts completions and refuses. One that returns early is not expressible, because the task is borrowed rather than `'static`.
- `DbOptions` is no longer `Copy`: `Copy` cannot be hand-implemented over an `Arc` ( E0204, and `unsafe impl` is E0199 ).

## Details

### Database surface

Writes are accumulated in a consuming `WriteBatch`, locked by ascending shard order, assigned a version after lock acquisition, logged, synced, and then made visible. Tombstones are values in the memtable's version chain rather than absence from the map.

`WriteBatch::merge_set` unions an `OrdSet` into a key using ordinary insert and range operations. It deliberately does not emit `PutChunk`, whose live apply replaces a chunk while its WAL replay unions one. Maximal runs become one `SetRange`; scattered values stream from the already sorted set without an intermediate copy.

Batch planning is insensitive to the caller's arrival order. Every recording method passes through `push_op`, which maintains a constant-time `keys_ascending` flag. An out-of-order batch builds contiguous `( key, original_index )` pairs and sorts them lexicographically with `sort_unstable`; the original index makes the result a stable sort by key, preserving operations such as insert followed by remove. At 2,097,152 document-major inserts, commit fell from 1,746 ms to 170 ms, about 10.3x, while the already key-major path remained in its baseline range. Sorting references directly was rejected because repeated pointer dereferences cost about 14% at 8.4 million operations.

Snapshots capture version, per-shard root, and registry ownership. `rank` counts values strictly less than its argument and `select` is zero-based. Database range mutation uses inclusive bounds, while expression ranges use half-open bounds; the E2E harness mirrors both instead of normalizing them.

`Snapshot::key_stream` constructs an immutable visible plan and decodes one container per `next_chunk`. `Snapshot::key_expr` exposes the same plan through the re-openable `ChunkSource` factory used by lazy expressions. Cardinality operations consume `card_m1` and cached memtable lengths without reading payloads. The memtable overlay is collected eagerly to avoid introducing a live memtable-to-store lock order; it remains bounded by the flush threshold.

### What the batch API's range folds are actually worth

`merge_set` and `WriteBatch::remove_range` both fold contiguous ordinals into one operation, and consumers reason about them from operation counts, which cannot tell the two cases apart.

**Density has a closed form.** For a *randomly* scattered set of density `d` over a contiguous id range, expected runs are `N * d * ( 1 - d )` against `N * d` members, so the fold is `1 / ( 1 - d )` -- 1.05x at `d = 0.05`, 2x at `d = 0.5`, 10x at `d = 0.9`. Reported by a consumer measuring their own posting sets at `d = 0.5` and sparse ( 2.00x and 1.02x, invariant to batch size and width ) and re-derived here by simulation over two orders of magnitude of `N`. **The API repays density, and the caller who most wants ingest speed usually has sparse sets**, so a restructure motivated by "there is a range operation" can be declined on arithmetic.

**Contiguity is a different lever and a much larger one.** `1 / ( 1 - d )` is bounded by the gaps being random; ordinals that are **contiguous by construction** collapse to a single operation whatever their width. The same consumer's whole-row clear -- `row_bits` adjacent ordinals through `remove_range` -- went from `row_bits` operations to one, about 2x on their delete path end to end, against the 2x that density bought them at `d = 0.5` over an entire ingest. Same API family, same cost model, three orders of magnitude apart in fold. **Ask whether the ordinals are adjacent by design, not whether there are many of them.**

### Arrow

`yesno-arrow` exposes bitmap containers as selection masks with two ownership cases. A store-backed `BitStore::Shared` can share the underlying bytes when alignment permits; a memtable-resident `BitStore::Mut` re-encodes the 8 KiB bitmap into Arrow's byte representation. Structural pointer-identity tests are required for the shared-object claim because allocation budgets alone can absorb one accidental copy.

`unstable_arrow::bitmap_words` lets a consumer borrow `&[u64]` directly and avoid reconstructing words from an Arrow mask. Its `Option` is not a kind test: `None` also means a shared bitmap whose offset fails `u64` alignment. One consumer used it to reduce posting-list reads from 2.02 ms to 0.48 ms and a whole scan from 3.51 ms to 1.66 ms; sparse or misaligned inputs take the position-scatter fallback.

Validity buffers are excluded by construction for the posting-list surface. Batches coalesce adjacent chunks, and array/run paths may materialize where the bitmap arm stays zero-copy.

### DataFusion

Filter lowering is asymmetric:

- exact AND terms can be retained;
- dropping an unsupported AND term yields a safe superset;
- dropping an unsupported OR term can lose rows and is unsafe.

Verdict tests are not enough. The row oracle evaluates a small table independently and checks equality for `Exact` and superset inclusion for `Inexact`.

`SnapshotSource` is the production `PostingSource` bridge; `MapSource` remains a fixture implementation. Range summaries provide `Empty`, `Full`, and `Partial` without constructing a set, and populated-key enumeration is proportional to distinct keys rather than the ordinal universe.

### Replication and Flight

The leader reads shard WAL files rather than borrowing a live `Db`, so shipping does not perturb the writer. The follower validates batches with the core tracker before writing frames and opens its local `Db` to replay through normal crash recovery.

Flight tickets include snapshot version, key, prefix bounds, and expression hash. `FlightInfo.total_records` is exact because cardinality is available from index metadata. `do_get` uses `spawn_blocking` and a bounded channel because mmap faults must not park async reactor threads.

Versioned tickets are honored by `snapshot_at`; preparing a ticket and fetching it after a write still reads the ticket minted version. `QueryRequest` adds strict caller-selected version preparation without changing the legacy current-version descriptor.

A ticket's version is additionally **leased** between `GetFlightInfo` and `DoGet`, because honouring a version is not the same as keeping it readable: a checkpoint in that gap moves the reclamation floor past it and the fetch is refused. The service parks a **clone of the snapshot it answered from** in a per-version map, and that needs no new retention mechanism at all -- a snapshot clone refcounts its reader-registry slot, so the parked clone *is* the floor holding the version down. A lease is therefore a registered reader with a deadline.

Four properties of that design are load-bearing. Expired leases are swept when a new one is registered, so the only thing that creates leases is the only thing that retires them and no background task exists. An expired lease is **released, never an error**, and the fetch still falls back to opening by version: a ticket may legitimately outlive its lease or have been minted by another process, and refusing those would be a regression. A lease reports through the live-reader count and the soft space-amplification threshold rather than through a quota or a metric of its own, because it *is* a reader -- the observe-only space policy applied. And the default is a bounded interval, with a zero duration disabling leasing entirely.

Disabling leasing is what keeps the refusal path tested. The stale-ticket test runs with a zero lease, which is not a weakened assertion: a ticket older than its lease, or one minted elsewhere, still has to be refused with a code that means "get a new ticket", and a zero duration is the cheapest way to reach that path deterministically.

The shared Flight client is consumed by PostgreSQL, Python, Java, and Tantivy integrations. Client-only builds exclude the service and storage crates, preserving one dependency direction.

### Bring your own executor: the `dispatch` seam

`yesno-core` runs inside somebody else's process, and the hosts that benefit from parallelism already run a runtime sized for their machine ( `yesno-server` and `yesno-flight` both have a tokio one ). A pool owned by this crate would spawn threads that host never asked for and cannot size, and per-commit thread spawning cannot pay at the measured timescales anyway. `crate::dispatch` is the seam instead: a one-method `Dispatch` trait, a `Sequential` implementation that is the default and spawns nothing, and a `Dispatcher` handle.

**Collaborators are `DbOptions` fields, not constructors.** `open_with_events` was one `open_with_*` per collaborator, which does not compose -- two collaborators need a third constructor naming both. `Events` and `Dispatcher` are now fields with `Default`s reproducing the previous behaviour exactly, the `open_with_*` variants are two-line wrappers that set the field, and **a third collaborator will never add a constructor**. The handle also avoids `Option<Arc<dyn Dispatch>>`, where `None`-means-sequential is a second spelling of what `Sequential` already says, plus an inline sequential arm at every use site.

**`DbOptions` loses `Copy`, and that cost was measured rather than argued.** `Copy` cannot be hand-implemented over an `Arc`: `impl Copy` is **E0204** and `unsafe impl Copy` is **E0199**, because bitwise-copying an `Arc` gives two owners and one refcount. Removing the derive and counting gave **11 `.clone()` sites, all in tests** and none in another crate.

**Half the contract is enforced by the compiler and half is not.** An implementation must call `f( i )` once for every `i` in `0..n` and not return until all finish. *Returning early is not expressible*: `f` is borrowed rather than `'static`, so it cannot be moved into a detached thread, and only a scoped or blocking executor compiles -- a first attempt to smuggle the task across threads as a raw pointer failed to compile, which is the contract arriving as a compiler error. *Skipping a task is expressible*, memory-safe and silent: it drops a shard's resolved chunks, and for a delete that means the wrong tombstone set and a persisted value showing through. So the commit **counts completions and refuses**, which is also what catches a panic swallowed at the C boundary.

Both halves of a commit now run through the dispatcher -- the prefetch first, being the larger and safer half at 254-293 us entirely outside every lock, then the apply, whose only escaping state was the `changed` accumulator and which measured ~3.5x to ~5x at 8 shards. Task `i` owns shard `participants[i]` entirely and version assignment happens once above the loop, so I5 is unaffected by the order tasks run in.

### Host-independent C ABI

`yesno-c` is a separate Rust 1.95 workspace exposing only opaque `yesno_db` and materialized `yesno_cursor` handles. Every operation receives its database handle explicitly, so independent embedders do not share process-global state. A cursor owns an immutable ordered snapshot and deliberately spends O(cardinality) memory to avoid borrowed Rust lifetimes in foreign callers.

Unsafe code is confined to pointer ownership, slice construction, and bounded error-buffer copies. Every exported call catches unwinding. A strict C11 linked test covers two-database isolation, one-byte error buffers, all seek modes, stable EOF, cursor snapshot isolation, checkpoint, and reopen; direct properties add canary-guarded error buffers and `BTreeSet` seek partitions.

**Writes are batched and dispatch is the host's** ( 2026-09-17 ). An opaque batch handle groups inserts, removals and whole-key deletes into one commit; the commit call consumes the handle on every path, success or failure, because the underlying Rust batch takes `self`, and the reported count is what **moved**, not what was recorded. An open-time options handle carries a shard count and a dispatch callback flattened from the core trait -- `( user_data, n, task, task_ctx )` -- so an embedder supplies its own executor and yesno creates no threads. The per-task trampoline catches unwinding, since the caller is C; a swallowed panic degrades to a task that did nothing, which the commit's completion count independently refuses.

The batch is what makes the dispatcher worth having: a single-ordinal insert is one shard and therefore one task, so options without a batch would have been public surface that could not pay. Null dispatch restores the sequential default rather than erroring, and options are read at open and not retained -- but `user_data` must outlive the database handle.

### MySQL storage-engine boundary

`yesno-mysql` implements one `BIGINT UNSIGNED NOT NULL PRIMARY KEY` set per `CONNECTION` key. Writes are **buffered per connection and applied when MySQL commits**, so `ROLLBACK` discards them and `SAVEPOINT` unwinds; each scan remains stable through its owned cursor. `HTON_CAN_RECREATE` must remain absent because MySQL would bypass atomic truncate and lose the `CONNECTION` string.

**MySQL's transaction boundary is real, and the previous three paragraphs of this section described the opposite.** This document said `ha_yesno` advertised `HA_NO_TRANSACTIONS`, registered no commit or rollback hooks, and had a fixture asserting a rolled-back row was **still present**. All three became false when the engine gained transactions, and the documentation did not move with the code -- found 2026-09-23 while answering a CDC handoff that asked which of the two to believe.

What is true now: `table_flags()` does not include `HA_NO_TRANSACTIONS`; the handlerton registers `commit`, `rollback`, `savepoint_set`, `savepoint_rollback`, `savepoint_release` and `close_connection`; and the fixture runs `START TRANSACTION; INSERT ... ( 13 ); ROLLBACK` and asserts the row is **gone**, with a mirror case so the assertion cannot pass against an engine that drops every write. `yesno-pg` is therefore a **parallel** rather than a contrast: both buffer per transaction to make `ROLLBACK` real, and both own the subtransaction problem that follows. See [PostgreSQL Extension and Query Pushdown](./postgresql-extension-and-query-pushdown.md).

**There is deliberately no two-phase `prepare`.** A crash between this engine's commit and MySQL's binlog write leaves the two disagreeing, which is the same gap `fdw-two-phase-commit` records on the PostgreSQL side and needs the same durable prepare/resolve protocol yesnodb does not expose. The limitation is narrower than the one this section used to claim, and it is still a limitation.

**Both backends now publish a transaction as one version.** The embedded backend always did, through a single C-ABI `yesno_batch`. The Flight backend issued a `Clear` per cleared key, then one `RemoveMany`, then one `InsertMany` -- so truncating *c* keys published `c + 2` versions and a reader could see removals without insertions. As of 2026-09-23 it stages the whole plan through one Flight write transaction, with each key's `DeleteKey` ahead of that key's own writes. The two backends now agree on semantics and not merely on results.

**MySQL keeps the sequential default, and the reason is the handler contract rather than performance.** `Backend::Insert` is one key and one ordinal because MySQL asks whether row N collided before it offers row N+1, and the verdict has to account for rows already buffered by this same statement. `write_row` therefore *reads* -- `view_contains` over the committed set overlaid with the pending buffer -- and returns `HA_ERR_FOUND_DUPP_KEY` before recording anything. ( This is the second thing that changed when the engine gained transactions: the verdict used to come from the **return value of writing the row**, `changed == false`, which a buffered write cannot supply because its commit is deferred. ) The pinned MySQL 8.4 fixture checks error 1062 by name. Batching the engine's writes would trade away per-row duplicate detection, a **declared behaviour of the table**, for parallelism that a single ordinal does not have anyway: one ordinal is one shard is one task.

`yesno-pg` is a different case and not a gap either: it has no `Db` at all. The Flight client is built without the `server` feature so the extension shares protocol handling *without linking `yesno-core`* into PostgreSQL, and `Transport::Local` is unimplementable because `Db::open` takes a non-blocking exclusive `flock` while PostgreSQL forks a backend per connection. There is no `DbOptions` to put a `Dispatcher` in; wiring one would mean reversing a recorded decision. **The escape hatch exists for the embedder that batches, and neither of these two is that embedder.**

The backend interface has embedded and Flight implementations. Embedded mode wraps `yesno-c`; remote mode uses the native C++ Flight client and materializes the result into the same before/row/after cursor states. Point mutations are atomic server actions rather than racy contains-then-insert sequences. The pinned MySQL 8.4 fixture runs the same SQL corpus through both backends and requires duplicate-key error 1062 by restoring primary-key index zero in `info(HA_STATUS_ERRKEY)`.

## Files

- `yesno-core/src/db/` - database and snapshot APIs.
- `yesno-core/src/db/keystream.rs` - paged key plans, streams, and expression sources.
- `yesno-arrow/` - Arrow masks and batches.
- `yesno-datafusion/` - predicate lowering and table integration.
- `yesno-server/src/replication/` - tonic leader and follower client.
- `yesno-flight/` - Flight service and tickets.
- `yesno-e2e/` - operational scripting harness.
- `yesno-c/` - host-independent opaque-handle C ABI and strict C11 fixture.
- `yesno-mysql/` - MySQL 8.4 storage engine with embedded and Flight backends.
- `yesno-flight-c++/` - native client used by the remote MySQL backend.

## Test Coverage

- `yesno-arrow/tests/allocation.rs` checks the bitmap zero-copy path.
- `yesno-datafusion/tests/lowering_oracle.rs` checks row semantics.
- `yesno-server/tests/replication_catch_up.rs` verifies physical bootstrap, catch-up, watermark, and lag.
- `yesno-flight/tests/roundtrip.rs` covers metadata, `do_get`, `do_put`, shutdown, and reopen.
- Core allocation tests distinguish paged key streaming from eager `Snapshot::load`, and batch-order agreement tests force descending keys plus same-ordinal insert/remove ordering.
- C ABI properties cover bounded error buffers and all cursor partition points.
- The pinned MySQL fixture runs one exact SQL contract through embedded and remote backends.
- `a_dispatcher_that_skips_work_is_refused` drives an executor that runs all but the last task and asserts the commit errors, sabotage-checked.
- `a_concurrent_dispatcher_commits_what_the_sequential_one_does` compares two databases key by key across inserts, rewrites, checkpoints and deletes, with a control asserting the corpus is non-empty so it cannot pass as two empty databases agreeing.
- The C fixture asserts `dispatch_calls > 0`; without it every other assertion in that section passes identically against a dispatcher that is never invoked.

## Pitfalls

- Do not allow satellite dependencies to flow back into `yesno-core`.
- Do not call an inexact OR lowering safe unless its rows are proved to be a superset.
- Do not treat the virtual-shard hash as a persisted shard-indirection layer.
- Do not bypass the persisted shard map at write-batch or snapshot routing sites.
- Do not add a `PutChunk` producer without first reconciling its replace-on-commit and union-on-replay semantics.
- Do not interpret `bitmap_words(None)` as proof that a container is not a bitmap.
- Shut down services that own a `Db` before testing reopen of the same directory.
- Do not put process-global database state or MySQL policy into the C ABI.
- Do not add a C ABI surface for a shape that cannot pay: one ordinal is one shard is one task, so options without a batch write would have been public surface incapable of using the thing it exposes.
- Do not wire a dispatcher into `yesno-mysql` to gain parallelism; its per-row duplicate-key verdict is a declared table behaviour and a deferred commit cannot produce it.
- `user_data` passed through the C dispatch callback must outlive the database handle; options themselves are read at open and not retained.
