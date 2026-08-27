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
- E2E's Rust version is intentionally higher than core's MSRV.
- `range_summary` and `len_in_range` answer complete chunks from index cardinalities and underpin DataFusion selection planning.
- Packed matrix, integer, and view layouts are caller-owned lenses over ordinary sets, not new storage formats.

## Details

### Database surface

Writes are accumulated in a consuming `WriteBatch`, locked by ascending shard order, assigned a version after lock acquisition, logged, synced, and then made visible. Tombstones are values in the memtable's version chain rather than absence from the map.

`WriteBatch::merge_set` unions an `OrdSet` into a key using ordinary insert and range operations. It deliberately does not emit `PutChunk`, whose live apply replaces a chunk while its WAL replay unions one. Maximal runs become one `SetRange`; scattered values stream from the already sorted set without an intermediate copy.

Batch planning is insensitive to the caller's arrival order. Every recording method passes through `push_op`, which maintains a constant-time `keys_ascending` flag. An out-of-order batch builds contiguous `( key, original_index )` pairs and sorts them lexicographically with `sort_unstable`; the original index makes the result a stable sort by key, preserving operations such as insert followed by remove. At 2,097,152 document-major inserts, commit fell from 1,746 ms to 170 ms, about 10.3x, while the already key-major path remained in its baseline range. Sorting references directly was rejected because repeated pointer dereferences cost about 14% at 8.4 million operations.

Snapshots capture version, per-shard root, and registry ownership. `rank` counts values strictly less than its argument and `select` is zero-based. Database range mutation uses inclusive bounds, while expression ranges use half-open bounds; the E2E harness mirrors both instead of normalizing them.

`Snapshot::key_stream` constructs an immutable visible plan and decodes one container per `next_chunk`. `Snapshot::key_expr` exposes the same plan through the re-openable `ChunkSource` factory used by lazy expressions. Cardinality operations consume `card_m1` and cached memtable lengths without reading payloads. The memtable overlay is collected eagerly to avoid introducing a live memtable-to-store lock order; it remains bounded by the flush threshold.

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

### Host-independent C ABI

`yesno-c` is a separate Rust 1.89 workspace exposing only opaque `yesno_db` and materialized `yesno_cursor` handles. Every operation receives its database handle explicitly, so independent embedders do not share process-global state. A cursor owns an immutable ordered snapshot and deliberately spends O(cardinality) memory to avoid borrowed Rust lifetimes in foreign callers.

Unsafe code is confined to pointer ownership, slice construction, and bounded error-buffer copies. Every exported call catches unwinding. A strict C11 linked test covers two-database isolation, one-byte error buffers, all seek modes, stable EOF, cursor snapshot isolation, checkpoint, and reopen; direct properties add canary-guarded error buffers and `BTreeSet` seek partitions.

### MySQL storage-engine boundary

`yesno-mysql` implements one `BIGINT UNSIGNED NOT NULL PRIMARY KEY` set per `CONNECTION` key. It advertises `HA_NO_TRANSACTIONS`: writes commit immediately and MySQL rollback cannot undo them, while each scan remains stable through its owned cursor. `HTON_CAN_RECREATE` must remain absent because MySQL would bypass atomic truncate and lose the `CONNECTION` string.

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

## Pitfalls

- Do not allow satellite dependencies to flow back into `yesno-core`.
- Do not call an inexact OR lowering safe unless its rows are proved to be a superset.
- Do not treat the virtual-shard hash as a persisted shard-indirection layer.
- Do not bypass the persisted shard map at write-batch or snapshot routing sites.
- Do not add a `PutChunk` producer without first reconciling its replace-on-commit and union-on-replay semantics.
- Do not interpret `bitmap_words(None)` as proof that a container is not a bitmap.
- Shut down services that own a `Db` before testing reopen of the same directory.
- Do not put process-global database state or MySQL policy into the C ABI.
