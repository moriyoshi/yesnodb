# Milestones and System Boundaries

## Summary

`yesno` progressed through milestones M0-M7 from an in-memory compressed set to a persistent, sharded database with Arrow, DataFusion, replication, and Flight surfaces. The milestone gates matter because each one names a property that later layers depend on; existence of a module or passing unit tests was repeatedly shown to be weaker than satisfying the gate.

## Key Facts

- M0 established containers, the generic oracle kernel, `OrdSet`, and byte-identical portable Roaring serialization.
- M1 established lazy `ChunkStream` algebra, equivalence with eager evaluation, and non-materializing cardinality paths.
- M2 established the extent and packed-page formats, mmap segment layer, COW B+tree, allocator, and `fsck` escape hatch.
- M3 established WAL framing, prefix-closed visibility, redo-only recovery, superblock flipping, and checkpointing.
- M4 wired the sharded database end to end, including persistent reopen and multi-shard atomic batches.
- M5 and M6 added Arrow and DataFusion without allowing their dependency trees into `yesno-core`.
- M7 deliberately split raw WAL replication over tonic from columnar query results over Arrow Flight.
- Post-M7 integration adds the shipped `yesnod`, a Bazel-built PostgreSQL extension, packed data lenses, and Rust, Python, Java, and Tantivy clients without moving those dependencies into core.
- The repository is licensed `MIT OR Apache-2.0`; workspace metadata plus literal declarations in `yesno-wire` and `yesno-pg` must move together.
- The crate's current ordinal universe is `0..=u64::MAX - 1`; `u64::MAX` is reserved by invariant I8.

## Details

### Milestone gates

| Gate | Deliverable | Structural evidence |
|---|---|---|
| M0 | Containers, generic kernel, `OrdSet`, Roaring codec | Properties against `BTreeSet`; serialized bytes identical to `roaring` |
| M1 | Lazy set algebra and cardinality | Lazy equals eager; count equals collected length; allocation budgets |
| M2 | Page store and COW index | Layout checks, lifetime tests, `fsck`, mmap coverage outside MIRI |
| M3 | WAL, checkpoint, recovery, MVCC | Crash matrix and prefix-closed recovery |
| M4 | Sharded database | Reopen, atomic batch, snapshot, concurrency, and durability tests |
| M5 | Arrow surface | Bitmap-arm pointer identity and allocation tests |
| M6 | DataFusion lowering | Row-level oracle with exact and inexact verdict semantics |
| M7 | Replication and Flight | Physical bootstrap plus WAL catch-up; Flight round trips |

The project repeatedly found machinery that had been implemented and unit-tested but never reached from production. A milestone is therefore not complete merely because its components exist. Its end-to-end gate must exercise the production path that claims the property.

### Boundary decisions

- `yesno-core` remains synchronous and dependency-lean. Tokio, tonic, prost, Arrow Flight, DataFusion, and monty live in satellite crates.
- The `roaring` crate is a dev-dependency oracle, not a runtime implementation dependency.
- Replication ships raw WAL frames because crash recovery is the canonical decoder. Query results use Flight because those payloads are genuinely columnar.
- Flight SQL and `do_exchange` are deliberately absent. SQL composition belongs in DataFusion rather than in the storage engine.
- `yesno-e2e` has its own Rust 1.95 requirement because monty needs it; `yesno-core` keeps its Rust 1.89 MSRV promise.
- MIRI cannot execute the mmap boundary. Pure casts are covered by MIRI, while mmap lifetime and race properties are covered by Valgrind, ASan, TSan, and targeted tests.

### Post-milestone integration boundaries

The milestone sequence closed the storage engine, not every deployment surface. `yesnod` adds roles, TLS, authorization, live followers, promotion, metrics, and packaging. `yesno-pg` is a PostgreSQL-ABI-bound cdylib built hermetically with Bazel. Python and Java are installable Flight clients, and Tantivy is an application-ID filter adapter. All consume stable core or wire boundaries rather than becoming new core dependencies.

Packed matrices, integers, and views are lenses over `OrdSet`; they add no fourth container kind or storage catalog. Their layout descriptors remain caller-owned. This distinction lets algebra grow without turning each interpretation into a durable format promise.

The dual license was selected while the project had one author and no vendored third-party source. MIT improves GPLv2 compatibility; Apache-2.0 retains the express patent grant expected by enterprise and ASF-adjacent consumers. Future license edits must update workspace metadata, `yesno-wire`, and the out-of-workspace `yesno-pg` declaration.

## Files

- `yesno-core/src/lib.rs` - public constants, invariants, and core module surface.
- `yesno-core/src/db/` - M4 database integration.
- `yesno-arrow/`, `yesno-datafusion/` - M5 and M6 integrations.
- `yesno-server/src/replication/`, `yesno-flight/` - the two M7 wire surfaces.
- `.agents/docs/OVERVIEW.md` - current milestone and scope summary.
- `.agents/docs/ARCHITECTURE.md` - current system map and invariant descriptions.

## Test Coverage

Run the routine workspace gate with:

```text
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --check
cargo test -p yesno-core
```

The broader milestone evidence also includes satellite-crate tests, E2E scenarios, the crash matrix, and `scripts/gate.sh --deep`.

## Pitfalls

- Do not infer milestone completion from module existence or isolated unit tests.
- Do not raise the workspace MSRV to match the E2E harness.
- Do not add async or transport dependencies to `yesno-core`.
- Do not treat post-M7 integrations as permission to move their runtimes or language dependencies into core.
- Do not change only the inherited workspace license field; two crates carry literal declarations for build-boundary reasons.
- Do not report MIRI as covering mmap-backed unsafe code.
