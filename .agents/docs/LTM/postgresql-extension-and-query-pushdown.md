# PostgreSQL Extension and Query Pushdown

## Summary

`yesno-pg` is a Bazel-built PostgreSQL extension that exposes Flight-backed foreign scans, transactional writes, an index access method, and a table access method. Its correctness boundary is not merely SQL result equality: it must preserve PostgreSQL callback lifetimes, planner identity, isolation semantics, and one pinned yesno version at the appropriate transaction or statement scope.

## Key Facts

- PostgreSQL ABI compatibility is pinned by Bazel against PostgreSQL 17.11 and 18.6; Cargo does not build the extension.
- `Cargo.toml` and `Cargo.lock` remain dependency sources of truth; the two crate-universe hubs must resolve identical Rust type identities.
- The FDW supports reads, qual/count/join/aggregate pushdown, transactional buffered writes, key enumeration, and `IMPORT FOREIGN SCHEMA`.
- The index and table access methods use different TID packings deliberately; neither should be normalized into the other.
- Writes are visible to the writing transaction through a streaming pending overlay with last-write-wins ordering per ordinal.
- REPEATABLE READ and SERIALIZABLE pin tickets for the transaction. READ COMMITTED pins one version per executor statement and refreshes between statements.
- Cross-process reader slots conservatively protect reclamation. PID reuse can retain space but cannot expose wrong data.
- PostgreSQL two-phase commit remains a separate durable prepare/resolve protocol problem.

## Details

### Hermetic build and ABI

The extension is a `cdylib` loaded into one concrete PostgreSQL server ABI. `./scripts/gate-pg.sh` uses a sha256-pinned PostgreSQL build and runs unit plus `pg_regress`-style fixtures on both supported majors. Bazel resolver manifests are compared so a feature or version drift cannot give nominally identical Rust types different identities at the cdylib boundary.

The PostgreSQL build does not use `cargo pgrx init`, `cargo pgrx test`, or a cluster under `~/.pgrx`. Test clusters and convenience symlinks stay under `.agents-workspace/tmp/`.

### FDW and pushdown

Planning retains opaque Flight commands and versioned tickets. Execution decodes batches through the shared `YesnoClient<Channel>` while the extension owns its current-thread runtime, lazy connection, ordinal-to-`bigint` mapping, and PostgreSQL error boundary.

Pushdown correctness is checked by differential plans. `OFFSET 0` disables the target optimization while preserving SQL semantics and supplies an independent result oracle. Join and aggregate pushdown tests assert both result equality and sensitivity to disabling the optimized arm.

The FDW write buffer must preserve statement order. Separate insert and remove vectors lose whether `INSERT` or `DELETE` came last; the buffer is keyed by ordinal with last write winning. Pending changes overlay remote rows without materializing both sides and decline forms whose ordering cannot be preserved.

`IMPORT FOREIGN SCHEMA` depends on populated-key enumeration, not on scanning the ordinal universe. Fixtures must assert generated schema and live query behavior, because plan-only coverage cannot prove the imported relations work.

### Access methods and snapshot pinning

The index AM maps yesno keys to PostgreSQL relation identities and maintains planner-visible costs. The table AM stores stable application ordinals rather than heap tuples. PostgreSQL callback state must live in the memory context whose lifetime matches the callback API; Rust stack or temporary allocations are not sufficient.

Within one PostgreSQL transaction, pending writes are overlaid on pinned remote state so read-your-own-writes and snapshot stability compose. Isolation scopes are:

| PostgreSQL level | Ticket scope |
|---|---|
| REPEATABLE READ / SERIALIZABLE | transaction, endpoint, and key |
| READ COMMITTED | outer executor statement, endpoint, and key |

`ExecutorEnd` clears statement tickets, while a transaction-abort callback is the backstop because PostgreSQL `ERROR` can bypass normal executor cleanup. The two-session harness uses marker-synchronized commands and advisory locks rather than timing guesses.

The remaining `tam-mvcc` item is specifically agreement with PostgreSQL's xid clock, not absence of within-yesno isolation. Adding stored xids would also add freezing, `relfrozenxid`, and severe write amplification.

### Reader registry and atomicity boundary

Cross-process reader slots let PostgreSQL backends pin yesno checkpoint roots. Liveness currently uses PID alone. A recycled PID can keep a dead slot looking live, retaining extents conservatively; it cannot make a live reader look dead. A sound refinement needs a process-start identifier, not a heartbeat timer.

Buffered writes flush during PostgreSQL pre-commit, so rollback and statement batching work. A crash after the yesno commit but before PostgreSQL's commit record can leave the systems disagreeing. Closing that window requires durable prepared operations outside the consecutive-prefix visibility stream, idempotent commit/discard, and restart reconciliation before assigning the real yesno version.

### Hermetic mutable process harness

Bazel remains the authority for immutable PostgreSQL inputs: server prefix, extension artifacts, selected major, and declared fixtures. The mutable process lifecycle lives in `yesno-e2e` and consumes only Bazel runfiles. It initializes and stops the private cluster, starts the in-process Flight fixture on an ephemeral port, runs Monty SQL, byte-compares expected output, and owns persistent two-session isolation schedules.

PostgreSQL and MySQL use the ordinary Monty binary and one featureless generic fixture host. Backend identity, arguments, readiness commands, protocols, and expected-output selection remain Python scenario data; Rust exposes declared resources, paths, managed processes, PTY sessions, transcripts, and cleanup. There is no PostgreSQL-specific runner, Cargo feature, shell lifecycle, or source-directory discovery.

Interactive `psql -a` sessions use a PTY because tuple output block-buffers behind a pipe and can appear after a synchronization marker. Marker comments and bounded waits establish ordering without sleeps or host `stdbuf`. The accept target is the only path that writes expected output back to the workspace.

## Files

- `yesno-pg/` - extension sources, Bazel targets, and SQL declarations.
- `e2e/postgresql/{sql,expected,isolation}/` - PostgreSQL 17 and 18 behavior fixtures.
- `scripts/gate-pg.sh` - hermetic two-major gate.
- `yesno-flight/` - shared client used by the extension.
- `yesno-core/src/db/readers.rs` - cross-process reader registry.
- `.agents/docs/ARCHITECTURE.md` - PostgreSQL ABI, callback, planner, storage, TID, and snapshot invariants.

## Test Coverage

- `scripts/gate-pg.sh` builds and runs the complete extension suite on PostgreSQL 17 and 18.
- FDW, index-AM, and table-AM fixtures assert plans and answers; independent oracles cover omission-prone execution paths.
- Two-session specifications prove transaction snapshot stability, READ COMMITTED refresh, same-statement pinning, and ERROR cleanup.
- Write oracles cover both insert/delete orders, own writes, pending count pushdown, and streaming overlay behavior.
- Resolver-manifest parity, shell syntax, formatting, and the Cargo workspace gate remain separate authorities.

## Pitfalls

- Never accept regenerated expected output without reading the diff; acceptance is not an independent oracle.
- A single-session `pg_regress` script cannot prove interleaved snapshot behavior.
- Do not reserve an ordinary commit version for an unresolved prepared transaction; it would block every later visible commit.
- Do not replace PID reuse protection with a heartbeat that can declare a merely slow live reader dead.
- Neither the Cargo gate nor the PostgreSQL gate subsumes the other.
- Do not hand-write dependency versions into `MODULE.bazel`; repin from Cargo metadata.
