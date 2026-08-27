# Network Service, Replication, and Operations

## Summary

`yesnod` combines a Flight query service, a raw-WAL replication service, checkpointing, metrics, authorization, TLS, promotion, and live read replicas. The operational design separates data identity, leadership identity, cursor position, and reader lifetime because conflating any two produced silent gaps or unsafe failover behavior.

## Key Facts

- Replication ships physical shard images plus raw WAL frames; query results use Arrow Flight.
- Base-image bootstrap streams bounded chunks, omits all-zero runs, and recreates them as sparse holes on the follower.
- A follower publishes a base image by renaming a complete sibling file; existence must never stand in for completeness while copying.
- WAL LSNs are global offsets in a shard's history, not offsets in the current post-checkpoint file.
- A follower verifies the leader database identity before writing and adopts the first identity only when unseeded.
- Checkpoints retain WAL according to authenticated per-follower acknowledgements, bounded by a hard `max_wal_bytes` escape hatch.
- A live standby keeps a read-only `Db` open while new WAL is applied and swaps it only at a whole-database phase boundary.
- Leadership terms fence superseded leaders in WAL records and client requests; database UUID alone cannot distinguish two leaders of one database.
- Authorization is enforced after protocol parsing where the operation is known. Transport interceptors cannot authorize Flight methods whose role depends on descriptors or actions.
- `SIGUSR1` promotion changes daemon state; the term is inherited from upstream and must never be synthesized locally.

## Details

### Cursor and image correctness

The base snapshot reports the superblock's `wal_replay_lsn`, not the current WAL file length. Records appended after the last checkpoint lie between that replay point and the current end and must be shipped after the image.

Checkpoints seal the active WAL as an immutable generation without reusing LSNs. Global LSNs translate to file offsets within the generation that contains them. `truncate_to` removes an invalid recovery suffix across generations; retention removes durable prefixes by deleting whole sealed files. The first complete frame makes a non-empty generation self-describing, while an empty active file begins at the newest sealed end or, when none remains, at the superblock replay LSN.

A stale cursor that no longer names a record boundary earns `FailedPrecondition` and a re-bootstrap instruction. A half-written record at a valid boundary remains a heartbeat condition so a busy leader heals without unnecessary image transfer.

Shard images are sparse files whose apparent size is rounded to the 1 GiB segment size even when their physical allocation is only kilobytes. Reading and sending the apparent file byte-for-byte costs that size in leader RSS, network traffic, and follower disk; a joining replica could therefore OOM the leader. The leader now scans bounded chunks and sends only nonzero runs together with the apparent length. The follower sets the length first and writes the ordered runs, recreating holes without `SEEK_DATA` or new unsafe filesystem calls. Allocated zero blocks may become holes, which preserves content.

Skipping runs removes the old contiguous-offset guarantee. The replacement is a declared image length, per-chunk ordering and bounds, and the leader's exact sent-byte count checked by the follower. The count is load-bearing: a sabotage that drops bytes can preserve the superblock and current logical data while leaving a file that fails only on later growth.

### Identity, retention, and fencing

Follower identity is derived from the authenticated principal placed in request extensions, not from a self-reported acknowledgement field. The retention floor is tracked per `(follower, shard)` through a grace window; anonymous deployments conservatively collapse to one identity. New acknowledgements replace old ones because an LSN can fall in local-file coordinates after a cut even though its global history position does not.

The hard WAL bound overrides retention. This mirrors snapshot space policy: a stalled consumer may force expensive recovery, but it cannot halt ingestion by filling the disk.

The historical `follower-retention-floor-is-not-enforced` slug is a closed, false claim. The leader acknowledgement handler calls `observe_from`, `Db::checkpoint` consumes the floor through `take` and sets `reclaim_through`, and `max_wal_bytes` bounds it with a `forced_past_retention` event.

Database UUID prevents cross-database mixing. A leadership term prevents an older leader of the same database from continuing to commit. The term is stored in already-CRC-covered slack and carried by records and client expectations. Client-side fencing belongs in a request-aware layer, unlike role authorization, because validating an expected term is independent of the particular Flight operation.

### Daemon and standby lifecycle

`yesno-server` owns the database, service listeners, checkpoint driver, and metrics server. `interval_secs = 0` means disabled; it must not become a zero-duration ticker that checkpoints continuously. Service stop waits for completion so the database lock and sockets are actually released before restart tests continue.

The follower loop has explicit states for bootstrap, catch-up, serving reads, and promotion. It refuses two tempting repairs: silently switching to a different leader identity, and inventing a leadership term locally. A read-serving standby publishes a new `Db` only when every shard has crossed the same replication phase, preventing per-shard visibility flapping.

Bootstrap writes each shard beside its final name, empties the shard log before the rename, and then renames the complete image into place. The rename is the single commit point: either the old image and its log remain, or the new image and an empty log do. An unreadable image is re-bootstrapped rather than retried forever; this is conservative repair and is logged because genuine corruption and interrupted work share the same safe response.

TLS supports bearer principals and mTLS identities. An empty principal configuration means open access, matching development behavior. Replication defaults are stricter than query defaults because a writable follower endpoint changes durable state.

Packaging lives under `dist/`, while disaster recovery procedure and real RPO live in `docs/operations.md`. Replication is not a backup: it eagerly copies logical mistakes and depends on retained WAL or a new base image.

### Durable control and lifecycle state

Core lifecycle observations are optional and panic-isolated. `yesno-server` translates them into versioned Protobuf envelopes, assigns a monotonic sequence before publication, and persists CRC32C-framed events before broadcasting. Recovery truncates only an incomplete tail, rejects interior corruption or sequence gaps, and synthesizes interruption facts for processes or database lifecycles that did not close.

Bounded compaction writes the complete state projection as an atomic Protobuf checkpoint. A subscriber older than retained history receives an explicit resync marker and projected snapshot. Control commands reserve delivery capacity before recording a durable request, then record completion or failure around the actual role transition.

### Authorization channels and listener ownership

Control and replication share one authenticated TCP endpoint and may also share a Unix-domain listener. Role transitions swap only the dynamic leader replication service; the endpoint, authorization policy, and principal resolver stay alive. The standalone replication crate was removed because transport, role, retention, and snapshot ownership are one server boundary.

Authorization is ordered, first-match, and default-deny over connection channel, capability, principal, and source network. `local` uses kernel peer credentials exposed as `uid:<number>`; `hostssl`, `hostnossl`, and `host` distinguish TCP transport. Replication over TCP still requires TLS and a named authenticated principal unless the explicit insecure escape hatch is selected. Socket creation refuses live listeners and non-socket paths, replaces only stale refused sockets, and unlinks only the inode it created.

### Protobuf statistics and observability

The Flight `stats` action returns `yesno.flight.v1.ServerStats`, not JSON. Five stable `uint64` field numbers are pinned by canonical bytes and decoded by Rust, Python, Java, Go, CLI, and E2E consumers while tolerating unknown fields.

Storage and Flight operations emit structural `tracing` spans without keys, ordinals, expressions, prefixes, or row payloads. Work moved to `spawn_blocking` carries the current span explicitly. Optional OpenTelemetry export uses batched OTLP/gRPC, configured sampling, service resource attributes, and explicit provider shutdown before the Tokio runtime is dropped so queued spans flush.

## Files

- `yesno-server/src/replication/` - leader service, follower client, cursor tracking, and retention acknowledgements.
- `yesno-server/tests/{follower,replication,replication_catch_up,replication_steady_state,replication_read_amplification}.rs` - bootstrap correctness, sparsity, catch-up, and bounded-read behavior.
- `yesno-server/` - daemon lifecycle, roles, checkpoint driver, shared listener, metrics, TLS, and promotion.
- `yesno-flight/` - guarded query and write service.
- `yesno-core/src/wal/` - global LSN framing, generation rollover and reclamation, and recovery suffix cuts.
- `yesno-core/src/db/` - checkpoint policy, leadership term, and read-only follower mode.
- `e2e/scenarios/{replication,replica_lag,server_lifecycle,server_auth,failover,live_replica}.py` - operational sequences.
- `docs/operations.md` - promotion, backup, and recovery runbook.

## Test Coverage

- Rust replication tests cover bootstrap, catch-up, checkpoint-spanning cursors, wrong leaders, multi-shard atomicity, retained floors, and hard bounds.
- Sparse-bootstrap regressions require equal apparent contents with a fraction of the physical allocation, reject lost-byte sabotages, and leave no partial image.
- The read-amplification assertion is isolated in its own test target because `/proc/self/io` is process-wide.
- E2E scenarios compare leader and follower sets through ordinary snapshot verbs rather than moving WAL in Python.
- Two-certificate tests prove per-follower identity is wired through the TLS guard into retention accounting.
- Daemon scenarios exercise start, metrics, checkpoint, stop, lock release, restart, authentication, and promotion.
- Sabotage checks separately remove identity insertion, identity consumption, term checks, retention publication, and service waiting.

## Pitfalls

- A successful heartbeat can conceal a cursor that no longer names a frame boundary.
- Do not infer replay position from file length after checkpoints begin cutting prefixes.
- Do not let followers self-report the identity used to move another follower's retention floor.
- A remedy named in an error is unverified until the shipped client has followed it successfully.
- Do not swap a live replica one shard at a time; publish at a database-wide phase boundary.
- Do not materialize sparse holes on the wire or in follower storage.
- Do not write a base image under its final name or use file existence as evidence of completeness.
- Do not drop the sent-byte check merely because current data still decodes after a short transfer.
- Replication, live read service, backup, and promotion are separate operational guarantees.
