# WAL, MVCC, Durability, and Concurrency

## Summary

Durability is built from global byte-offset LSNs within each shard history, per-shard WALs, prefix-closed commit visibility, redo-only recovery, and A/B superblocks. Concurrency correctness depends on ordering version assignment, WAL durability, shard locks, snapshot registration, and checkpoint publication so every acknowledged commit remains recoverable and every snapshot remains stable.

## Key Facts

- An LSN is a global byte offset in one shard history. Within a generation, its file offset is `lsn - generation_base_lsn`.
- `append` and `sync` are separate so group commit can release the log lock during fsync.
- A multi-shard commit becomes visible only after every participant is durable.
- Every assigned commit version must resolve as durable or aborted; an abandoned pending slot stalls the visible prefix forever.
- Abort records are fsynced before the in-memory slot is resolved.
- Recovery replays only the consecutive resolved run above the checkpoint watermark.
- Records above the recovered global version form a suffix and are truncated.
- Checkpoint publication is one superblock flip after data and metadata are durable.
- A process-wide lock file prevents two writers from opening the same database directory.
- Checkpoints seal immutable WAL generations; reclaiming old generations never restarts LSNs at zero.
- The physical seal end and replay-safe cutoff may differ when a durable appended commit remains above the visible watermark.
- Reader roots are protected across processes by a registry shared with PostgreSQL backends.
- Resolving commit markers carry an optional UNIX-epoch-microsecond time in their body; invariant I9 makes it non-decreasing by version and identical across one multi-shard commit.
- The superblock persists the commit-clock high-water mark so a restart after checkpoint cannot move timestamps backwards.
- A commit can return before the consecutive visible prefix reaches its version. Read-your-writes is explicit through `wait_visible`, not an implicit property of `commit`.
- `PutChunk` replaces a live memtable chunk but replays its `ChunkImage` as a union. Its only safe producer emits `DeleteKey` first; `merge_set` uses ordinary insert operations instead.
- Document-major batches are stably grouped by key before planning while preserving same-key arrival order; an O(1) flag leaves already key-major batches unsorted.
- Checkpoint fsyncs run without the shard-store lock. A checkpoint mutex excludes a second checkpoint, and a `Durable` token makes adoption before sync unrepresentable.
- Concurrent reads serialize per shard under `Mutex<ShardStore>`, so scaling follows shard count rather than core count. Any `RwLock` change must measure checkpoint progress under sustained reads.

## Details

### Prefix-closed visibility

The visible watermark is the greatest version for which every earlier version is resolved. This supplies atomic visibility across shards and gives recovery the same rule as the live process. A later durable commit cannot pass an unresolved hole.

Commit work after version assignment is fallible. The commit path therefore wraps WAL append and fsync work so an error writes best-effort abort records to every participant and resolves the slot as aborted. Without that path, later acknowledged commits can disappear after recovery.

### WAL ownership and group commit

The original WAL module had record framing, scanning, and recovery planning but no writer owned a file. The writer now owns immutable sealed generations plus one append-only active file, together with sync and durable-position tracking.

Group commit uses a target LSN. A leader fsyncs through a captured length and publishes `synced`; waiters return when `synced >= target`. The leader duplicates the descriptor and drops the log lock before fsync so later appenders can join the covered range.

Generation rollover and recovery truncation must coordinate with sync state. A group-commit leader duplicates the current active descriptor for each sync; a descriptor captured before rollover must not be reused for later active bytes. An epoch makes stale completions discardable.

### Write batches must agree with replay

`Op::PutChunk` has deliberately asymmetric implementations. Live commit calls `Memtable::put_chunk` and replaces the chunk; WAL encoding emits a bare `ChunkImage`; replay applies that record with per-ordinal inserts and therefore unions. The paths agree only because `WriteBatch::store_set`, the sole producer, emits `DeleteKey` first. A new producer that omits the delete can commit one state and recover another.

`WriteBatch::merge_set` therefore lowers through the ordinary insert path, where live apply and replay share the same union semantics. `store_set_replays_to_what_it_committed` first populates ordinals that the replacement omits, because replacing or unioning into an empty key is indistinguishable.

`plan_ops` groups consecutive same-key operations, so document-major input formerly created one group and one `BTreeMap` allocation per operation. Every recorder now routes through `push_op`, which tracks whether keys remain ascending. Only an out-of-order batch sorts compact `( key, original_index )` pairs; the index preserves same-key order while allowing `sort_unstable`. At 2,097,152 inserts, document-major commit fell from 1,746 ms to 170 ms while key-major commit stayed in its measured baseline range.

### Commit-time stamping

Wall-clock recovery is encoded without widening the 40-byte WAL header. `ShardCommit` and `Abort` use flag bit `0x01` and an 8-byte timestamp in their formerly empty body. Older logs retain an empty body, and older readers already ignore the body of the known record type, so rolling compatibility works in both directions. A new record type would make older scanners reject the stream, while a wider header would break every archived frame and tax every data record.

Measured on one shard, 10,000 single-ordinal commits grew from 1,120,048 B to 1,200,048 B ( 7.14% ); widening the header would cost 14.29%. A bulk load of 1,000 `SetRange` records grew from 72,088 B to 72,096 B ( 0.011% ), versus 11.12% for a wider header. The per-commit body avoids repeating one timestamp in every record of a batch.

`VersionOracle::begin` assigns the version and stamp under the same mutex as `max( now, last + 1 )`. Reading the clock outside that lock permits two committers to enter in the opposite order and invert time. A backward clock step bunches later commits one microsecond apart; a forward jump is accepted. Every shard participant receives the same stamp, and recovery rejects disagreement as corruption rather than selecting or averaging one.

Checkpointing stores the clock high-water mark in previously unused superblock space. Open resumes from the maximum of that value and stamps replayed from WAL; zero means the image predates the field. Unknown flag bits are rejected, because silently ignoring a later writer's flag would interpret its body with an older layout. Unstamped versions remain absent from `RecoveryPlan::times`, never epoch zero.

### Checkpoint and recovery

Checkpoint ordering is:

1. sample the visible watermark;
2. write new payloads and index nodes unreachable from the live root;
3. persist slab metadata and the inactive superblock;
4. fsync;
5. flip the active superblock slot.

After superblock publication, the active WAL is sealed at the checkpoint's captured end. Retention then deletes only complete sealed generations ending at or below its floor. A crash after the rename but before the new active file is created is repaired at open from the sealed generation's end.

The physical WAL end captured under the shard write lock is a safe rollover boundary, but it is not always a replay-safe reclamation cutoff. A commit can append and release that lock while group sync remains in flight, leaving its version above the sampled visible watermark. `WalWriter` tracks the first record LSN of retained commit versions; the checkpoint writes the first LSN above its watermark into the superblock and preserves the generation containing it.

Recovery scans framed records, builds commit resolution, chooses the consecutive visible prefix, and applies redo. It reuses the ordinary database write semantics instead of maintaining an independent apply interpretation.

The checkpoint's store-lock scope is split into three phases. `prepare_superblock` gathers the tree, allocator, and slab-metadata changes under `Mutex<ShardStore>`; `run` releases that lock for the three durability syncs; `adopt_superblock` reacquires it and accepts only the `Durable` token returned by `run`. A separate per-shard `ckpt` mutex spans all three phases so two checkpoints cannot interleave in the newly unlocked window.

The token is a proof obligation the test suite cannot replace. Adopting before a sync changes only an in-memory field and passes every ordinary test, but a later sync failure would leave a live superblock whose bytes are not durable. Making early adoption a type error is stronger than a sabotage whose failure requires a second fault to co-occur.

On a light writer, three A/B runs measured worst reader latency at 5.91, 8.28, and 7.24 ms with the lock held, against 0.68, 0.49, and 0.69 ms with it released. The change removes the roughly 17 ms fsync floor from the exclusive region. It does not remove the tree rebuild and carry-forward work, whose cost separates only at millions of dirty ordinals and remains under the lock.

### Concurrency and snapshots

The concurrency suite covers distinct-key writers, read-modify-write on one chunk, reverse-order multi-shard batches, readers sampling atomic batches, writers racing checkpoints, and held snapshots.

Snapshot slots are refcounted so clones do not release registration early. Root and checkpoint sequence are captured together under one shard lock. Slot teardown clears root metadata before making the slot reusable.

`VersionOracle::wait_visible( version, timeout )` polls the visible watermark with exponential backoff from 50 us to a 2 ms cap. It deliberately does not reject a version from `peek_next`: a replica advances visibility through `adopt_visible` and does not allocate local versions, so such a guard would disable the wait on the surface where it is most useful. A version that never existed consequently times out, which is the only truthful answer available from the watermark alone.

The poll reads the watermark before testing the deadline on every iteration. This makes a zero timeout a truthful non-blocking poll without a race-only special branch. Deadline handling compares elapsed time rather than computing `Instant::now() + timeout`, because `Duration::MAX` is a legal spelling of an unbounded wait and adding it to an `Instant` panics.

Per-chunk database reads still acquire `Mutex<ShardStore>`. On a 20-core machine, aggregate scaling was 0.22x at one shard, 0.75x at four, and 3.51x at twenty; one shard degrades because contention costs more than simple serialization. The 278 ns one-shard critical section could sustain 3.60 million serialized reads per second, while twenty contenders achieved only 0.80 million. Narrowing an already short section is therefore not the primary remedy because it raises acquisition frequency without removing the queue.

The candidate `RwLock<ShardStore>` conversion has ten read-only and three mutable lock sites, but every establishing measurement had no writer. Ordinary `WriteBatch::commit` does not take the store lock; checkpoint does, and a starved checkpoint fails to bound WAL growth and deferred reclamation. A mixed reader/checkpoint workload must show writer progress before the conversion is justified.

The three inner `SegmentedMmap` locks for segments, pins, and verified checksums are redundant only while the outer store lock is exclusive. With a shared outer lock they become load-bearing for concurrent `BTreeMap` mutation. Removing them first and converting the outer lock later would make two locally plausible changes compose into a race.

### Global LSNs and replication retention

Global positions flow through append, group commit, recovery, and replication. Translation to file offsets occurs per generation. `truncate_to` removes a recovery suffix across generation boundaries; checkpoint rollover seals the active file; retention advances the retained base by deleting whole sealed files.

A sealed generation's 20-digit filename names its base and its first complete frame carries the same LSN. An empty active generation begins at the newest sealed end, or uses the superblock fallback when no sealed file remains. Reading only the fixed header is insufficient because the LSN probe also verifies the full-frame checksum.

Replication catalog readers sample the sealed-generation list on both sides of opening the active file and retry if rollover changed it. This prevents combining an old active descriptor with a new generation catalog. Every group-sync leader likewise duplicates the active descriptor for its own pass rather than retaining one descriptor across rollovers.

Replication acknowledgements feed a checkpoint retention floor. Per-follower identities retain the slowest required history for a bounded grace period, while `max_wal_bytes` remains the hard ingestion-safety bound. Retention is an operational cost policy; prefix-closed visibility and crash recovery remain correctness policies.

Cross-process readers publish pinned checkpoint roots. PID-only liveness can mistake a recycled PID for the dead reader and retain space unnecessarily, but it cannot make a live reader look dead. A future refinement needs a process-start identity rather than a timer.

## Files

- `yesno-core/src/wal/` - records, writer, group commit, and recovery planning.
- `yesno-core/src/mvcc.rs` - version slots and prefix visibility.
- `yesno-core/src/wal/record.rs` - commit-marker flag and body encoding.
- `yesno-core/src/store/superblock.rs` - durable root publication and persisted commit-clock high-water mark.
- `yesno-core/src/db/mod.rs` - commit grouping, checkpoint preparation and adoption, replay, snapshots, and directory lock.
- `yesno-core/src/db/readers.rs` - cross-process reader registry.
- `yesno-core/tests/{durability,crash_matrix,concurrency,zero_copy_mvcc}.rs` - integrated gates.

## Test Coverage

- `a_commit_survives_a_reopen_with_no_checkpoint` proves the WAL, not an automatic checkpoint, provides durability.
- Abort tests separately pin in-process watermark progress and recovery agreement.
- `tests/crash_matrix.rs` exhausts log truncation and corruption offsets and superblock tear cases.
- The historical `crash-matrix-has-no-partial-multi-shard-commit` gap is closed by `a_multi_shard_batch_is_all_or_nothing_at_every_step` and `truncating_one_shard_of_a_multi_shard_batch_blocks_the_whole_batch`.
- `store_set_replays_to_what_it_committed` distinguishes replace-on-commit from union-on-replay using a non-empty prior key.
- `interleaving_order_does_not_change_what_a_batch_commits` forces the batch sort and pins same-key ordering.
- `readers_are_correct_while_a_checkpoint_syncs` verifies full sets while readers overlap repeated unlocked checkpoint syncs.
- Concurrency tests include sample-count and progress guards so a rarely scheduled reader cannot pass vacuously.
- Invariant and concurrency tests pin monotone stamps, equal multi-shard stamps, restart persistence, unknown-flag refusal, and disagreement rejection.
- `a_zero_timeout_is_a_truthful_poll_in_both_directions` pins the watermark-before-deadline ordering, and `an_unbounded_timeout_does_not_overflow_the_clock` pins the legal worst-case duration.

## Pitfalls

- Never sync while holding every shard log lock.
- Never resolve an abort slot before its abort record is durable.
- Do not read the clock outside the version-oracle lock or stamp each shard independently.
- Do not interpret a missing stamp as the epoch.
- Do not add commit time as a new record type or widen the header without treating it as a hard format break.
- A durability test that checkpoints before reopen cannot prove the WAL path.
- Do not change generation rollover or recovery truncation without coordinating `synced` and in-flight leaders.
- Do not infer a global cursor from one WAL generation's file length.
- Do not use the physical seal end as the replay cutoff without accounting for durable commits above the visible watermark.
- Do not pair a generation catalog sampled before rollover with an active descriptor opened after it.
- Do not declare readers dead from a heartbeat timeout; a slow live reader must remain protected.
- Do not reject a requested version by comparing it with the local allocator's next version; replicas adopt versions without allocating them.
- Do not build a deadline as `Instant + Duration` when callers may pass `Duration::MAX`.
- A process lock is part of the correctness model, not an operational convenience.
- Do not add a `PutChunk` producer that can omit the preceding key deletion.
- Do not remove inner mapping locks on the assumption that they stay redundant after the outer store lock becomes shared.
- Do not report the checkpoint stall as fixed: only the fsync floor left the store-lock region.

## The consistency model, measured

Established 2026-09-12 by two tests written for the purpose, after a read of the commit path. Both are demonstrations, not derivations.

**Leader reads are not linearizable, and read-your-writes is explicit rather than automatic.** `snapshot()` reads the visible watermark; the watermark advances over a *consecutive prefix* of resolved versions; and `commit` releases its shard guards before the durability wait, then returns as soon as its own version resolves. It never waits for the watermark to reach it. So a commit that resolves while an earlier version is still pending returns a version the database does not yet show.

```text
attempt 9: commit returned v20 while visible was 18 (earlier pending commit held v19)
```

Observed on 8 of 8 runs, first hit between attempts 2 and 14, with two committers on different shards so their fsyncs are independent. At that instant `snapshot_at( 20 )` returns `VersionNotVisible`. The window is bounded by the earlier commit's fsync, so this is transient staleness rather than loss; it is still a linearizability violation, reachable in milliseconds on an idle machine.

The library exposes the missing synchronization without changing the consistency model: `Db::wait_visible` waits for the returned version and reports `VisibilityTimeout` if the visible prefix does not reach it. A caller that needs read-your-writes must retain the commit version, wait, and then request that exact snapshot. This does not make an ordinary `snapshot()` linearizable and does not prove that a never-assigned version will appear.

Single-writer workloads never see it: versions resolve in order and the watermark keeps up. It takes two concurrent committers.

**Retries are safe in isolation and unsafe under interleaving.** Every mutation is a set operation, so replaying one lands the same state — which is exactly what makes retry *look* safe. There is no idempotency key, request id, or dedup on the write path, so a retry is indistinguishable from a fresh intent because that is all it is.

The damage is not merely that a retried insert restores a deliberately removed ordinal. It is that the return value points the wrong way: an uncontested retry answers `false` ( "already present" ), which is how a client learns its first attempt landed, while a retry that *undid someone's delete* answers `true` ( "newly added" ), the same answer a genuinely necessary retry gives. The one signal available says "good thing you retried" at the moment the retry did damage. The symmetric case holds for a retried delete removing a later re-insert.

**What is genuinely strong.** Multi-shard commits are atomic in visibility on both leader and replica: `CommitEntry::is_resolved` requires every named participant, and `CommitIntent` is written to *every* participant's WAL so each shard's stream is independently interpretable. A partially arrived multi-shard commit is invisible, never half-visible. Follower visibility is `fetch_max`, so it is monotonic per follower and never regresses.

**The Flight surface makes the explicit path available but does not turn it into a session guarantee.** `do_put` acknowledges rows plus the commit version, and `GetFlightInfo` can wait a bounded interval for that exact version before calling `snapshot_at`. The default wait is one second because the condition it closes is one pending fsync; a larger gap is replication lag or a version this database never assigned. Exact still means exact after reclamation, and a future minimum-version request must be a distinct wire form. Streaming ingest remains one commit per record batch and reports the last version, so an ambiguous timeout mid-stream can still leave an arbitrary prefix applied.

