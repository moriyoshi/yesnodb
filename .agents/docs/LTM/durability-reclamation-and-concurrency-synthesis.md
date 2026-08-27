# Durability, Reclamation, Replication, and Snapshot Concurrency Synthesis

## Summary

Durability is a system-wide ordering contract, not merely a WAL format. Per-shard LSN histories remain stable across generation files, while commit versions define the cross-shard visible prefix; commit completion and visibility are distinct, checkpoints publish that prefix, retention protects authenticated followers, and front ends such as Flight and PostgreSQL pin snapshots for their own transaction scopes. Reclamation is safe only when roots, aliases, mapped bytes, readers, replicas, and crash recovery all agree that an extent or WAL prefix is unreachable.

## Included Documents

| Source topic | Contribution |
|---|---|
| [Storage Format, Index, and Zero Copy](./storage-format-index-and-zero-copy.md) | Roots, aliases, mmap ownership, checkpoint publication, and portable reopen behavior. |
| [Allocation, Reclamation, and Fsck](./allocation-reclamation-and-fsck.md) | Extent lifetimes, reader pins, hole reuse, and crash-safe reclamation. |
| [WAL, MVCC, Durability, and Concurrency](./wal-mvcc-durability-and-concurrency.md) | Global LSN semantics, prefix visibility, explicit read-your-writes, recovery, follower retention, and leadership identity. |
| [Network Service, Replication, and Operations](./network-service-replication-and-operations.md) | Service roles, authenticated replication, follower replacement, retention pressure, and operational recovery. |
| [PostgreSQL Extension and Query Pushdown](./postgresql-extension-and-query-pushdown.md) | PostgreSQL snapshot scopes, pending-write overlays, 2PC boundaries, and extension lifecycle constraints. |

## Stable Knowledge

- An LSN names a global byte position within one shard history, stable across that shard's immutable generations. Commit version, not one cross-shard LSN, coordinates atomic visibility. Deleting whole old generations advances the retained base without renumbering surviving history.
- A multi-shard commit becomes visible as one prefix decision. Recovery must either expose the complete committed unit or omit it; a torn subset cannot be repaired by treating each shard independently.
- A successful commit can return a version above the current visible watermark when an earlier commit is still pending. Read-your-writes is therefore explicit: retain the returned version, call `wait_visible`, then open that exact version with `snapshot_at`. An ordinary `snapshot()` remains non-linearizable.
- `wait_visible` observes the watermark rather than the local allocator. Replicas adopt versions without allocating them, so rejecting a requested version against `peek_next` would break the operation on the surface where it is most useful. A version that never existed times out.
- Checkpoints use alternating manifests and an atomic publication step. Recovery selects the newest valid manifest, validates every referenced artifact, and replays only the committed WAL suffix beyond the checkpoint replay position.
- Root publication and reclamation are coupled. An extent remains live while any published root, alias, mapped view, reader ticket, checkpoint, or recovery path can reach it. `ExtentGuard`-style ownership keeps allocation rollback automatic until publication transfers responsibility.
- Manifest identity includes the persisted vshard map. Reopen, recovery, and replacement must restore that map rather than recomputing routing from the current shard count.
- Followers authenticate before they influence retention. Each admitted follower contributes an acknowledged LSN; retained WAL must cover the minimum protected acknowledgement, subject to a bounded grace period and a hard maximum that forces an explicit unhealthy or replacement state instead of unbounded disk growth.
- Dataset UUID and leadership term solve different problems. The UUID prevents joining unrelated histories; the monotonic term prevents stale leaders or stale streams from continuing after failover.
- A live follower replacement is a database-wide phase transition: copy a consistent checkpoint, stream the protected suffix, verify identity and cut, then atomically install the replacement. Per-shard swaps would expose mixed database generations.
- Cross-process reader registries must treat uncertain liveness conservatively. PID reuse and stale registry slots cannot justify reclaiming bytes; ambiguity delays reuse until a stronger generation or lease check resolves it.
- PostgreSQL `REPEATABLE READ` and stronger transactions pin one snapshot for the transaction. `READ COMMITTED` pins a fresh snapshot per statement. Pending writes are overlaid on that pinned base so read-your-writes does not require publishing an uncommitted global root.
- PostgreSQL prepared transactions require an explicit prepare/commit/abort protocol outside the ordinary committed mutation stream. A prepared change cannot become visible merely because its WAL bytes exist.

- Resolving `ShardCommit` and `Abort` markers may carry an 8-byte UNIX-epoch-microsecond stamp under flag bit `0x01`. Invariant I9 requires stamps to be non-decreasing by commit version and identical across every shard participant; the version oracle assigns both under one mutex and the superblock persists the clock high-water mark.
- A generation's physical seal end can exceed its replay-safe reclamation cutoff when an appended commit is durable but still above the visible watermark. Checkpoint code preserves the generation containing the first record that recovery may still need.
- Follower bootstrap streams bounded chunks, omits all-zero runs, declares the apparent image length, validates ordering, bounds, and the leader's exact sent-byte count, then renames a complete sibling image into place. Sparse transfer and atomic publication are separate correctness properties.

## Operational Guidance

- When changing WAL layout, state separately how global LSNs map to generation offsets, how whole-generation reclamation is represented, and how recovery proves continuity across boundaries.
- Treat manifest publication, routing-map persistence, and checkpoint validation as one protocol. A manifest that names valid files but reconstructs different routing is not a valid checkpoint.
- Before reclaiming an extent or WAL prefix, enumerate all reachability classes: live roots, aliases, snapshot tickets, mapped buffers, checkpoints, followers, prepared transactions, and crash recovery.
- Make retention failures explicit and bounded. Operators need to distinguish a temporarily lagging follower from one that must be replaced before the hard WAL cap is reached.
- Pin PostgreSQL snapshots at the transaction or statement boundary selected by its isolation level, and release them on every success, error, cancellation, and backend-exit path.
- Test follower installation and PostgreSQL commit integration at process boundaries; in-process state alone cannot prove crash or ownership behavior.

- When adding wall-clock semantics, preserve I9 at version assignment, recovery, checkpoint persistence, and multi-shard validation. Missing stamps remain absent and unknown marker flags are errors.
- When changing base-image transfer, preserve declared length, ordered non-overlapping chunks, exact sent-byte accounting, sparse reconstruction, log clearing, and sibling-file rename as one protocol.
- Preserve exact-version semantics across service boundaries. A minimum-version request is a different operation because an exact version may already have been reclaimed; never silently fall forward. Keep zero timeout as a truthful non-blocking poll and handle `Duration::MAX` without computing `Instant + Duration`.

## Files

- `yesno-core/src/db/` owns roots, manifests, persisted routing, checkpoint policy, and database modes.
- `yesno-core/src/store/` owns mapped extents, allocation, reclamation, and persistent layout.
- `yesno-core/src/wal/` owns global history encoding, replay, generation rollover, recovery suffix cuts, and retained-base metadata.
- `yesno-core/src/mvcc.rs` owns prefix-closed visibility and the bounded `wait_visible` poll.
- `yesno-server/src/replication/` owns leader and follower streams, cursor acknowledgements, retention, and replacement.
- `yesno-server/` owns authentication, daemon lifecycle, checkpoint driving, metrics, and promotion.
- `yesno-flight/` owns guarded query and write services plus snapshot tickets.
- `yesno-pg/src/` owns PostgreSQL isolation mapping, pending overlays, transaction callbacks, and prepared-transaction integration.
- `docs/operations.md` owns the operator-facing promotion, backup, restore, and recovery procedure.

## Tests

- Recovery tests must inject truncation and corruption at every publication boundary and prove that only a complete committed prefix is visible.
- Allocation tests must keep old roots and mapped views alive while holes are reused, then verify that reuse begins only after every pin is released.
- Replication tests need authenticated follower lag, retained-base advancement, hard-cap behavior, term changes, checkpoint-plus-suffix replacement, and whole-database installation.
- PostgreSQL regression fixtures need `READ COMMITTED`, `REPEATABLE READ`, rollback, read-your-writes, backend cleanup, and prepared transaction coverage using real separate sessions where visibility matters.
- End-to-end scenarios should cover ingest, checkpoint, close, reopen, query, and recovery against the same persisted routing map.
- Commit-time tests pin monotonicity, equal multi-shard stamps, restart persistence, older unstamped logs, unknown-flag refusal, and recovery rejection of disagreement.
- Sparse-bootstrap tests compare apparent contents and physical allocation, bound leader reads, inject dropped bytes, interrupt publication, and require incomplete sibling files never to become the final image.
- Visibility tests must stage an earlier pending commit so a later commit returns above the watermark, then prove the explicit wait-plus-exact-snapshot path. Separate tests pin zero-timeout polling and `Duration::MAX` overflow safety.

## Pitfalls

- Equating an LSN with the current WAL file offset breaks as soon as a retained prefix is removed.
- Recomputing `vshard % shard_count` on reopen silently changes ownership after topology changes.
- A UUID without a leadership term admits stale leaders; a term without a UUID can join an unrelated dataset.
- Allowing unauthenticated or abandoned followers to pin WAL creates a remote disk-exhaustion path.
- Reclaiming after the newest root drops an extent is unsafe while an older snapshot, alias, mapping, checkpoint, or prepared transaction can still reach it.
- Mapping every PostgreSQL statement to one long-lived backend snapshot violates `READ COMMITTED`; refreshing every statement inside `REPEATABLE READ` violates PostgreSQL's transaction semantics.
- Do not interpret an absent commit stamp as the epoch or assign shard stamps independently.
- Do not equate a WAL generation's physical end with the replay-safe reclamation cutoff.
- Do not use final-path existence as evidence that a follower base image is complete.
- Do not treat an acknowledged commit version as already visible, or overload an exact-version read to mean "at least this version".
