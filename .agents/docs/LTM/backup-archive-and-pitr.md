# Backup, Archive, and Point-in-Time Recovery

## Summary

Backups and archives are separate recovery products built over server-owned snapshots and the replication history. Hot base backup publishes one self-contained recovery point, while the archive maintains a fenced immutable history from which `yesno-restore` can select an exact logical commit version, a wall-clock instant, or the durable tip.

## Key Facts

- A hot base requires one common `checkpoint_cv` across every copied shard image; independently valid images from different checkpoints are not one database snapshot.
- Snapshot data is staged and synchronized before one final directory rename. Existing targets are refused and failed partial directories are removed.
- The archive writes immutable individual WAL-frame objects, then durable remote state, then its local cache, and only then acknowledges retention to the leader.
- WAL generation seal events are observations, not transfer sources; replication supplies the bytes because a sealed pathname may be reclaimed before a subscriber opens it.
- Archive schema v2 uses a conditional writer lease, compare-and-swap state publication, immutable history descriptors, base anchors, and per-shard SHA-256 chains.
- Every reconnect verifies retained leader history from the current base root. A missing retained root forces a new base instead of inventing continuity.
- An ordinary checkpoint activates a fresh archive but does not create repeated full bases once a recovery root exists. Retention loss and leadership-term changes are distinct rebase triggers.
- Every commit marker carries the wall-clock time its version was assigned, non-decreasing in commit version ( **I9** ), which is what makes a wall-clock recovery target name a prefix.
- Descriptor commit times are selection hints outside the SHA-256 commitments; the cut is resolved from verified frames.
- Archive object reclamation is reachability-based over a declared retention *window*, off by default. Object-store age rules remain no substitute: upload age is not commit age.
- A retained base that omits a shard prevents WAL reclamation for that shard and term; a cursor from another base must not be substituted.
- A recovery window must report only what a restore will actually stage. `stage_wal` walks to the durable `state.wal_cursors` tip; a window derived from *listed* WAL objects advertises versions past it, because an object is published before the cursor that covers it.
- The sidecar publishes a WAL object, then advances the cursor, then publishes state. Stopping it inside that gap leaves the object listable and the cursor behind it — visible to anything that lists, invisible to anything that trusts the cursor.

## Details

### Hot base backup

`yesno-basebackup` obtains a server-owned snapshot lease, copies every recoverable database file, and resumes each shard from the image's own `wal_replay_lsn`. It verifies the snapshot-reported replay position against the copied superblock, checks manifest and server shard counts, and refuses a recovered version below the common checkpoint.

The cross-shard watermark check is load-bearing. Checkpoint publication flips shards sequentially, so a transfer can observe individually sound images carrying different `checkpoint_cv` values. Recovery uses one global floor; taking the maximum from a mixed set would skip WAL still required by an older image. The whole transfer is retried when the image watermarks differ.

The target is assembled in a uniquely named sibling partial directory. Files and the directory are synchronized before one atomic rename publishes the backup. A pre-existing target is rejected before connecting, and transfer failure releases the lease and removes the partial directory.

### Archive publication and retention

`yesno-archive` consumes lifecycle events, snapshots, and replication through the shared authenticated control endpoint. A local sidecar can use a Unix socket; a remote sidecar uses mutual TLS. Snapshot mode may stream bytes over Protobuf or use the dual-opt-in direct-path escape hatch when server and client share the mount.

The publication order is:

1. upload immutable WAL frame or every base object;
2. publish the manifest last for a base;
3. compare-and-swap remote `state.pb`;
4. persist the local state cache;
5. acknowledge the replication position.

The acknowledgement consequently means the bytes are recoverable off-host and the leader may reclaim older WAL. A retention gap cancels live workers and establishes a new base before streaming resumes.

Object keys include database UUID and leadership term. The term distinguishes histories after promotion, but same-term split brain still requires external fencing. Attempt and snapshot names include process, time, and monotonic components so cancellation cannot make a retry collide with an orphan still owned by another process.

### Writer fencing and history proof

Archive schema v2 gives each writer an identity and a conditional `writer.pb` lease. Remote state advances by compare-and-swap, so an expired owner that resumes is fenced even after another writer takes over. Local archives use a kernel file lock because the local object-store backend has no conditional update.

Immutable WAL objects are normalized to individual frames. Transport batch boundaries depend on timing, so hashing whole batches would produce different object keys and chain tips for the same database history. On reconnect, the archiver rereads retained WAL from the current base and compares it with immutable objects before appending. This costs bandwidth but proves that the contacted endpoint shares the archived prefix.

Version 1 archives cannot be upgraded safely in place because they contain no authenticated history boundary. Start a new prefix and retain old completed bases under their original policy.

### Restore and base cadence

`yesno-restore` selects the newest base at or below the target -- an exact logical commit version, a wall-clock instant, or the durable archive tip when none is given.

A wall-clock target resolves to the highest committed version whose commit time satisfies the bound, inclusive by default and exclusive on request. It is answerable because every commit marker carries the time its version was assigned, stamped under the version oracle's lock and clamped monotone, so time order equals version order. Object upload time is still never a proxy: an upload may be retried long after its commit.

`--inspect` exposes the archive window and is also the E2E source of target timestamps. Fully stamped is computed per commit version, not per frame object, because only resolving markers carry stamps. The window's end time is the maximum nonzero time among objects at its maximum version; data frames precede the marker at the same version and must not suppress it.

An unstamped version in the targeted range is refused, not rounded, and not read as the epoch. History written before commit-time stamping is recoverable by version only.

Commit times also appear in base manifests and WAL descriptors, but as **selection hints only** -- they narrow which objects to fetch and are deliberately outside the history hashes, because extending those would change every existing fingerprint and break chain continuity at the next reconnect. The authority is the frames.

`--target-action` chooses what happens to the recovered directory: publish it, pause unpublished for inspection, or raise the term first so the copy is a new timeline. Promotion is the option for a restored database that will be written to: a restored copy otherwise shares the original's term *and* UUID, which is exactly the pair that cannot be fenced apart.

Restore verifies base sizes and CRCs, immutable WAL descriptors, hash-chain continuity, gaps, and forks. It asks the core recovery planner for the globally complete multi-shard prefix, truncates later frames, opens the staged directory through ordinary replica recovery, and publishes with one rename.

A completed checkpoint has one narrow cadence role: it activates a fresh archive. Once a base exists, later maintenance checkpoints advance the durable event cursor without replacing the recovery root. A term change or retention gap requests a new root. An empty manifest with a nonzero generation records an interrupted rebase and must resume that rebase after restart.

### Two readers of one archive, and the window that promised too much

A retained window reported as restorable restored **empty** — intermittently, about one run in five, and only inside the full scenario suite.

The measurement that settled it printed the base cursor, the durable tip and the descriptor chain at restore time:

```text
passing   base=[(0, 864336)]  state=[(0, 864456)]  descriptors=[(864336,864408),(864408,864456)]
failing   base=[(0, 864336)]  state=[(0, 864336)]  descriptors=[(864336,864408),(864408,864456)]
```

**The descriptors are identical**: the archive holds both WAL objects on the run that fails. Only the durable cursor differs. Where it equals the base cursor, `stage_wal`'s tip check matches on its first iteration and stages nothing, after which time resolution returns `checkpoint_version` with no time — a restore that reports success and contains only the base.

The cause is that two readers disagreed about where the archive ends. The window computation derived `end_version` from the objects it could *list*; `stage_wal` walks only to the durable tip, deliberately, because the tip is what the archive has committed to. The repair belongs in the report, not the walk: an object past the durable tip is not counted toward `end_version` for the manifest that tip describes. Letting the restore run past the tip would trade a visible wrong answer for an invisible one.

The same change repairs a scenario's synchronization without touching it: a sequence that waits on the window now waits until the cursor has advanced, so a sidecar stop can no longer land inside the gap.

The historical `pitr-retention-restores-an-empty-window-intermittently` slug names this closed defect. `beyond_durable_tip` and `a_window_does_not_count_wal_beyond_the_durable_tip` pin the repaired boundary; it is not an active flaky-gate item.

**Five hypotheses blamed reclamation before the measurement exonerated it** — the GC does the identical thing on the run that fails. A deterministic probe against the retention planner, showing it keeps increments above a retained base, was worth more than any amount of re-reading.

### A spurious wakeup is not an error

`request_base` coalesces a pending base request but calls `Notify::notify_one()` unconditionally, and `Notify` stores a permit when nothing is awaiting. Two requests arriving before the run loop's arm executes therefore leave **one request and two wakeups**; the second found the request already taken and aborted the sidecar. A wakeup is a hint, not a fact — the condition must be re-checked after every one. Notifying conditionally in the producer is not the fix: the request lock is released before the consumer takes it, so any "is a wakeup pending" test there is itself racy.

### Garbage-collection boundary

Leader-local WAL GC, server-local snapshot cleanup, and archive-object reclamation all exist. Bases, WAL frames and descriptors stay immutable; moving `state.pb` changes only the active root; reclamation is the only thing that deletes.

The policy is one duration: every wall-clock instant inside the window stays restorable. That is expressible only because commit times are persisted, and it is the unit an operator reasons in. Retention is computed as a *suffix* of the bases ordered by ( term, generation ), rooted at the newest base at or below the horizon, and never below the active base, an unplaceable base, an interrupted-rebase marker, or a `min_bases` floor. From each retained base, WAL at or above its cursor is reachable.

Reclamation is off unless a window is configured, runs under the writer lease renewed immediately before the pass, re-reads `state.pb` and abandons the pass if it moved, and deletes every reference before the thing it names — so an interrupted pass leaves orphans the next pass finishes, never a dangling manifest. Unrecognized keys are kept and counted rather than guessed at.

WAL floors are computed per `( term, shard )` only when every retained base of that term names the shard. If one retained base is silent, no cursor from a different base may be invented for it; the safe result is retaining all WAL for that shard and term. Current shard topology is stable, but keeping this condition prevents a future format extension from deleting an interval needed by a retained recovery point.

## Files

- `yesno-server-utils/` - base-backup, archive, restore, object-store, and writer-lease logic.
- `yesno-server/src/control.rs` - lifecycle subscription and snapshot-lease RPCs.
- `yesno-server/src/replication/` - WAL streaming and retention acknowledgements.
- `yesno-core/src/wal/` - framed WAL history consumed by replication and recovery.
- `e2e/scenarios/{basebackup,archive,pitr,pitr_retention}.py` - portable operational recovery sequences.
- `e2e/filesystems/` - deployed native-snapshot and S3 recovery scenarios.
- `docs/operations.md` - operator-facing backup, restore, archive, and disaster-recovery contract.

## Test Coverage

- The four-shard base-backup scenario compares a stopped, independently opened backup with a Python-set leader oracle.
- The archive scenario proves checkpoint activation, manifest-last publication, post-base WAL, reconnect history verification, writer exclusion, retention-gap rebase, and exact-version restore.
- The PITR scenario proves wall-clock resolution against a Python-set oracle across three archived commit groups, the inclusive/exclusive boundary, the pause and promote actions, and refusal of a target below every base. Its target times come from the shipped inspect report, never from the harness clock: those are different clocks, and comparing them would test their agreement rather than the resolution rule.
- `pitr.py` performs five distinct restore assertions and measures about 150 s; `pitr_retention.py` is separate, one-shard, and about 20 s so reclamation does not make the former critical path longer.
- Archive settling treats cursor waits as pacing only and re-reads the recovery window as the deciding assertion; the suite exposed a catch-up race that standalone runs could not.
- The retention scenario requires `unrecognized == 0` against objects emitted by the shipped sidecar; synthetic classifier keys cannot prove every production key shape is understood.
- Unit tests repartition identical WAL frames into different replication batches and require identical immutable keys and chain tips.
- Writer-lease tests cover live-owner exclusion, expiry takeover, and stale-owner state fencing.
- The opt-in filesystem scenarios run the shipped daemon, archiver, Winterbaume S3, restore utility, and restored daemon over real ZFS and Btrfs snapshots.
- A predicate test pins that a window does not count WAL past the durable tip, per shard, with the superseded-base and unknown-shard cases.
- A unit test pins the `Notify` permit semantics the sidecar's spurious-wakeup tolerance depends on, so a tokio change is caught rather than trusted.

## Pitfalls

- Do not use the maximum checkpoint version from mixed shard images.
- Do not transfer WAL by opening a pathname named in a seal event.
- Do not acknowledge retention before remote objects and state are durable.
- Do not hash transport batches; normalize history to frames.
- Do not create a full archive base for every maintenance checkpoint.
- Do not treat replication as backup or object-store lifecycle rules as PITR-aware GC.
- Do not delete a base whose commit time is unknown; "cannot place" is not "outside the window".
- Do not delete an archive object whose key shape is unrecognized.
- Do not derive one retained base's shard WAL floor from another base that is silent for that shard.
- Do not read an absent commit time as the epoch, or round a wall-clock target to a neighbouring commit.
- Do not add commit times to the archive history hashes; they are hints, and hashing them breaks every existing chain.
- Do not publish a restored copy that will be written to without raising its term.
- Do not overwrite version 1 archive state with version 2 metadata.
- Do not report a recovery window from listed objects alone; bound it by the durable cursor a restore will walk to.
- Do not fix that by letting a restore walk past the durable tip — the tip is a durability boundary, not a hint.
- Do not treat a `Notify` wakeup as proof its condition holds.
