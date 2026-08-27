# yesno Project Overview

`yesno` is a Rust library for Roaring-style sparse ordinal sets. An ordinal is a `u64`. The low 16 bits select a slot within a *chunk*; the high 48 bits are the chunk's `Prefix48`. A set is a sorted sequence of `(Prefix48, Container)` pairs, where a container is one of three representations — array, bitmap, or run — chosen by size class.

`yesno-core` is the engine and holds the whole data model. Satellite crates and
language clients surround it. The core boundary is enforced rather than
intended: CI's `lean-core` job fails if
`cargo tree -p yesno-core -e normal` so much as mentions
`tokio`, `tonic`, `prost` or `arrow-flight`, and caps the direct dependencies at
five. Every "just add serde derives to `DbOptions`" shortcut breaks that cap,
which is why the server mirrors the struct instead of deriving on it.

| | |
|---|---|
| `yesno-core` | containers, kernels, page store, WAL, MVCC, the sharded `Db` |
| `yesno-wire` | the on-wire encoding shared by `yesno-flight` and `yesno-pg` |
| `yesno-arrow` | zero-copy Arrow surface |
| `yesno-datafusion` | predicate lowering |
| `yesno-flight` | Arrow Flight for query results and `DoPut` ingest |
| `yesno-flight-c++` | the synchronous native Arrow C++ Flight client used by remote hosts |
| `yesno-tantivy` | generation-bound Tantivy filters from embedded or Flight results |
| `yesno-server` | `yesnod` + `yesno`: daemon, data CLI, TLS, auth, lifecycle, failover, and gRPC WAL replication |
| `yesno-server-utils` | `yesnoctl` + `yesno-archive`: checkpoint, hot backup, restore, and continuous object archive |
| `yesno-e2e` | the scenario harness; Python driven by `monty` |
| `yesno-c` | the host-independent C ABI. A separate cargo workspace at the core Rust 1.89 floor |
| `yesno-mysql` | the embedded or remote MySQL 8.4 storage engine. Built and tested with pinned MySQL and Arrow source by Bazel |
| `yesno-pg` | the PostgreSQL extension. Outside the cargo workspace, built by Bazel |
| `yesno-flight-python/` | the pure-Python `yesnodb` Arrow Flight client |
| `yesno-flight-java/` | the `dev.yesnodb.client` Java client and 32-bit `RoaringBitmap` adapters |
| `yesno-search-java/` | bounded Java resolution plus OpenSearch and Elasticsearch REST query adapters |
| `yesno-opensearch-plugin/` | OpenSearch 3.8 coordinator rewrite to its native bitmap-valued `terms` query |
| `yesno-elasticsearch-plugin/` | Elasticsearch 9.5 coordinator rewrite and portable Roaring doc-values query |
| `yesno-flight-go/` | the native Arrow Go Flight client with streaming reads, TLS/auth, and leadership fencing |

## Scope

`yesno` targets two things that a general-purpose bitmap crate does not necessarily give you together:

1. **Zero-copy, buffer-backed containers.** Container payloads live in `arrow_buffer::Buffer`, so a container decoded from a page store is a *slice* of that buffer, not a copy. Containers are `'static + Clone + Send + Sync`, with copy-on-write on mutation.
2. **Lazy, chunk-aligned set algebra.** `ChunkStream` composes operators so `a AND (b OR c)` never materializes an intermediate set, and cardinality-only queries never materialize a result container at all.

The intended consumer is a query engine: something that builds a plan, pushes a boolean expression down to a set index, and asks for either a cardinality or a stream of matching ordinals. `Expr` is the runtime-constructed form that such a planner lowers into ( the shape a DataFusion pushdown would take ).

## What It Deliberately Is Not

- **Not a fork of `roaring`.** The `roaring` crate is a dev-dependency used as a differential oracle. It is not a runtime dependency and must not appear in `src/`.
- **Not a 32-bit bitmap.** The native domain is `u64` ordinals. The 32-bit Roaring format is supported as an interchange format ( `roaring_format::Roaring32` ), not as the data model.
- **Not a superset of every Roaring dialect.** For 64-bit serialization we implement CRoaring's `Roaring64Map` layout. Java's `Roaring64NavigableMap` is explicitly unsupported.
- **Not tuned by guesswork.** Specialized set-operation kernels are added only when a benchmark demands one, and every specialized arm stays differential-tested against the generic kernel.

## Format Compatibility as a Design Lever

Container payloads are byte-identical to the portable Roaring serialization format. This is not compatibility theatre — it buys two concrete things:

- A **byte-level** differential test against the `roaring` crate, which catches codec bugs that semantic oracle testing structurally cannot.
- `O(container count)` import and export of `.roaring` files, rather than `O(cardinality)`.

Any change that would break byte identity therefore breaks a test *and* a performance property, and needs an explicit decision recorded in `JOURNAL.md`.

## Milestone Gates

The test suite names its gates, and the names are used throughout the codebase:

- **M0** — containers, generic kernel, `OrdSet`, Roaring codec. Set algebra agrees with `RoaringBitmap` over the 32-bit subrange and with `BTreeSet` everywhere, and serialized bytes are identical to the `roaring` crate's.
- **M1** — lazy set algebra. A lazy `Expr` yields exactly what the eager `OrdSet` path yields, `cardinality()` equals `collect_set().len()`, and allocation budgets hold.
- **M2** — page store and copy-on-write index. Layout checks, lifetime tests, `fsck`.
- **M3** — WAL, checkpoint, recovery, MVCC. A crash matrix that truncates and corrupts at **every byte offset** rather than sampling.
- **M4** — the sharded database. Reopen, atomic multi-shard batch, snapshot, concurrency, durability.
- **M5** — the Arrow surface. Pointer identity on the bitmap arm, and allocation tests to prove the zero copy is real.
- **M6** — DataFusion lowering, with exact and inexact verdict semantics.
- **M7** — replication and Flight: physical bootstrap plus WAL catch-up, and Flight round trips.

**A milestone is not complete because its components exist.** This project has
repeatedly found machinery that was implemented, unit-tested, and reached from
nothing in production — `Follower` with no caller while the M7 gate hand-rolled
its own, `enforce_space_amp` called only from a test, a `db_uuid` written
everywhere and compared nowhere. The gate has to exercise the *production* path
that claims the property. `LTM/milestones-and-system-boundaries.md` carries the
full table and the boundary decisions behind it.

Alongside them, `tests/allocation.rs` asserts allocation budgets as *tests, not benchmarks*, on the reasoning that a benchmark regression gets triaged next quarter while a failing test gets fixed today.

## Current Shape

- `yesno-core` is roughly 45k lines of source plus 13k of tests and benches.
- **Kernels are specialized, and measured against the `roaring` crate rather than against our own past numbers.** All nine kind-pairs are specialized in both `ops::apply` and `ops::card`; the AArch64 NEON array intersect is in. The x86_64 arm is deliberately unwritten because there is no machine here to measure it on, and an unmeasured kernel would put a number in a table nobody has seen.
- Streaming operators keep a one-slot lookahead so `peek_prefix` is exact, and each overrides `cardinality_dyn` with a non-materializing walk.
- Serialization covers the 32-bit portable format and CRoaring's 64-bit map layout.
- **It runs on a network.** `yesnod` serves Arrow Flight and ships its WAL over gRPC, with TLS, a role-based permission table, a leader/follower deployment, `SIGUSR1` promotion, and a leadership term that fences a superseded leader out. A standby can serve reads while it follows.
- **The PostgreSQL integration is complete through its original phases 0-9.** `yesno-pg` provides a Flight-backed foreign data wrapper with qual, aggregate and join pushdown plus transactional writes; an index access method over ordinary heaps; and a single-column `bigint` table access method. Its separate Bazel gate runs the complete SQL and isolation corpus against pinned PostgreSQL 17 and 18. Isolation within yesno follows PostgreSQL's transaction and statement lifetimes; cross-engine commit atomicity and xid-clock agreement remain explicit follow-on tradeoffs in `TODO.md`.
- **There is a host-independent C boundary, native C++ and Go Flight clients, and an experimental MySQL edge.** `yesno-c` owns explicit opaque database and materialized snapshot-cursor handles, catches Rust panics at every exported boundary, and supports multiple independent databases. `yesno-flight-c++` and `yesno-flight-go` use their ecosystems' native Arrow Flight APIs without a Rust bridge. `yesno-mysql` selects either backend at startup and maps a `CONNECTION` key to a one-column unsigned ordinal table. It is nontransactional by design: writes commit immediately, while each scan retains one stable yesno snapshot. Its Bazel gate builds Arrow, MySQL 8.4.0, and the plugin from pinned source, then runs the checked mysqltest fixture in a throwaway server.
- **Backup and recovery are shipped subsystems.** `yesnod` owns renewable provider snapshots across portable, ZFS, Btrfs, LVM, and EBS backends; `yesno-snapshot-agent` isolates privileged LVM and local-EBS work; `yesno-archive` publishes fenced immutable history; and `yesno-restore` selects a commit-version, wall-clock, or durable-tip prefix. The Kubernetes operator resolves EBS identity from each bound PersistentVolume and rolls the resulting per-instance configuration.
- **Still not production-ready, and the README keeps saying so.** There is no leader election or hostile-node split-brain prevention, only fencing and detection at observable edges. Replication is asynchronous, so acknowledged writes can be lost during failover, and the system has not been run at scale or for long. Point-in-time recovery requires a configured archive sidecar; a local data directory alone cannot be wound back. Archive reclamation is opt-in and duration-based, while live conditional-object-store reclamation coverage and operator-visible reclamation metrics remain open.

## Documentation Map

- `.agents/docs/ARCHITECTURE.md`: module map, data model, invariants, and design policies.
- `.agents/docs/QUALITY_GATE.md`: the checklist a change must pass, and the implementation conventions behind it.
- `.agents/docs/TESTING.md`: the end-to-end harness in depth — the verb surface, the handle model, and what may not go in a scenario.
- `docs/operations.md`: the operator guide, including backup, restore, promotion, measured RPO/RTO, and disaster recovery.
- `.agents/docs/TODO.md`: active backlog extracted from journal and LTM maintenance.
- `.agents/docs/JOURNAL.md`: append-only working log.
- `.agents/docs/LTM/INDEX.md`: durable long-term memory index.

## Operating Model

Work starts from the architecture document and the module-level `//!` comments, which carry the *why* behind size classes, hysteresis thresholds, and the buffer-containment policy. New behaviour lands with coverage in the layer that could structurally catch its failure — oracle, differential, equivalence, or allocation — rather than only a hand-written unit test. Findings and review outcomes are appended to `JOURNAL.md` and periodically consolidated into `LTM/` by the `good-sleep` and `deep-sleep` skills.
