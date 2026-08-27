# System Boundaries, Services, and Integrations Synthesis

## Summary

`yesno` is a storage and query engine whose durable core remains deliberately small while service, database-extension, client, and search integrations live at explicit boundaries. Its product direction is an operational context-eligibility plane over candidate identifiers, not a general bitmap DBaaS, OLAP engine, vector database, or authorization graph. The post-M7 system includes persisted routing, authenticated Flight service roles, ABI-pinned PostgreSQL integration, and language clients, but those surfaces must not leak their runtime dependencies or ownership models into `yesno-core`.

## Included Documents

| Source topic | Contribution |
|---|---|
| [Milestones and System Boundaries](./milestones-and-system-boundaries.md) | Delivered milestones, deliberate exclusions, and ownership of human-facing scope. |
| [Database APIs and Satellite Crates](./database-apis-and-satellite-crates.md) | General database visibility, snapshot, pending-write, and transaction-boundary rules. |
| [Network Service, Replication, and Operations](./network-service-replication-and-operations.md) | Flight roles, authentication, leadership identity, tickets, replication, and operational limits. |
| [PostgreSQL Extension and Query Pushdown](./postgresql-extension-and-query-pushdown.md) | Bazel-pinned PostgreSQL ABI, planner/executor integration, transaction scopes, and pushdown boundaries. |
| [Client Libraries and Search Integrations](./client-libraries-and-search-integrations.md) | Multi-language client contracts, acknowledged commit versions, exact reads, Arrow ownership, search indexing, stable IDs, and packaging constraints. |
| [Context Eligibility Product Direction](./context-eligibility-product-direction.md) | Candidate filtering, governed sharing, identity namespaces, disclosure limits, versioned receipts, and the BYOC-first boundary. |

## Stable Knowledge

- `yesno-core` owns durable representation, set algebra, planning primitives, storage, checkpointing, and WAL semantics. It does not own servers, PostgreSQL ABI bindings, language runtimes, search engines, or deployment policy.
- The delivered system has progressed beyond the early milestone map. Current documentation must distinguish historical gates from the implemented Flight service, replication, PostgreSQL extension, and client surfaces instead of presenting those as speculative future work.
- Routing is durable data. The manifest persists the vshard map used by ingest, queries, checkpoint recovery, replication, and replacement; service startup must restore it rather than derive ownership from the current shard count.
- Flight roles separate ingest/query service behavior from replication behavior. Authentication, authorization, dataset UUID, leadership term, and versioned tickets are protocol fields, not incidental server state.
- Raw WAL replication and Flight query transport serve different contracts. WAL preserves committed history and recovery continuity; Flight carries typed query or ingest data and snapshot/version tickets. Conflating them weakens both protocols.
- Query tickets pin a specific database version or snapshot and are released on success, cancellation, error, disconnect, and expiry. A ticket that names only a shard-local state cannot provide a database-wide consistent read.
- `yesno-pg` is built through Bazel against a sha256-pinned PostgreSQL source and server ABI. Cargo remains authoritative for ordinary workspace dependencies, but cannot establish the ABI that PostgreSQL will `dlopen`.
- PostgreSQL pushdown must preserve PostgreSQL visibility and error semantics. `READ COMMITTED` and transaction snapshots have different lifetimes, pending writes need an overlay, and unsupported expressions remain in PostgreSQL rather than being approximated remotely.
- Client libraries translate language-native values and lifetimes at their boundary. Arrow buffers should remain zero-copy only while ownership and release are explicit; language objects, exceptions, futures, and cancellation do not belong in the core API.
- Search integrations need stable external document IDs and explicit freshness semantics. An index is a derived view, not the durable authority, and rebuild or replay must not create duplicate identities.
- The first product primitive should filter caller-supplied candidates and return a mask plus a decision receipt. Ranking, content retrieval, and authoritative records remain outside yesno; unrestricted enumeration and bitmap export are different capabilities with different disclosure and revocation properties.
- Governed sharing needs publisher-owned grants, a shared identity namespace, operation and output limits, expiry, revocation, and authorization rechecked at delivery. Independently updated sources require a version vector and ingestion watermarks rather than an invented global version.
- Flight can return a commit version and wait briefly before preparing an exact-version read. This makes explicit read-your-writes expressible, but it is not a minimum-version API, an idempotency key, a conditional write, or a session guarantee. The legacy 8-byte acknowledgement and the 16-byte row-count-plus-version form must remain distinguishable.
- The initial commercial envelope is vendor-supported BYOC over rebuildable derived membership data. Multitenant DBaaS remains deferred until workload limits, isolation, recovery, and release evidence exist.
- Licensing, MSRV, wheel/JAR compatibility, and PostgreSQL major-version support are product boundaries. A test harness or client dependency must not silently raise the core crate's compatibility promise.

## Operational Guidance

- Put a new concern in the lowest layer that owns its invariant, then expose a narrow boundary upward. Keep runtime-, database-, and language-specific state out of `yesno-core`.
- Version every persisted or remote identifier whose interpretation can change: manifests, routing maps, Flight tickets, replication handshakes, and client payload envelopes.
- At startup and failover, validate dataset UUID, leadership term, persisted routing, checkpoint cut, and retained WAL continuity before serving reads or writes.
- Map cancellation and cleanup explicitly across boundaries. Releasing a Flight ticket, PostgreSQL snapshot pin, Arrow export, or client future must eventually release the core resource it protects.
- Keep README deployment and user-facing availability claims in `README.md`; agent documents should describe implementation constraints without duplicating operational instructions.
- Treat search indexes and client caches as rebuildable derivatives. Recovery authority remains the checkpoint, manifest, and committed WAL history.
- Treat identity, sharing, disclosure, and receipts as one service contract before adding core API. Candidate-only filtering reduces disclosure but does not by itself prevent singleton probing, differenced counts, or repeated-query inference.
- Enumerate every client decoder before changing wire metadata. Ack layouts cross Rust, Python, Java, Go, and C++ boundaries even when field-name searches find only part of the surface.

## Files

- `yesno-core/src/` owns the reusable engine and its stable Rust-facing contracts.
- `yesno-server/` owns daemon lifecycle, authentication, leadership, replication transport, retention, and follower installation.
- `yesno-flight/` and `yesno-wire/` own guarded query and write services, versioned tickets, expressions, and wire codecs.
- `yesno-pg/src/`, `e2e/postgresql/`, `MODULE.bazel`, and PostgreSQL Bazel rules own the extension and pinned server ABI.
- `yesno-flight-python/` and `yesno-flight-java/` own their runtime adapters, packaging metadata, and native test suites.
- `yesno-tantivy/` owns stable document-ID mapping, replay, and derived-index lifecycle.
- `.agents/docs/LTM/context-eligibility-product-direction.md` owns the product direction until its contracts are promoted into canonical project documents.
- `README.md` owns human-facing scope and deployment guidance; `.agents/docs/OVERVIEW.md` and `.agents/docs/ARCHITECTURE.md` own agent orientation and system constraints.

## Tests

- Service tests cover authentication, role separation, ticket versioning, cancellation cleanup, persisted routing, stale leadership terms, and checkpoint-plus-WAL recovery.
- PostgreSQL regression tests use the Bazel-built extension and real server sessions for snapshot visibility, pending writes, rollback, pushdown fallback, and prepared transactions.
- Client tests cover value conversion, Arrow ownership, cancellation, error translation, packaging, and compatibility with the supported runtime versions.
- Search tests verify stable IDs, idempotent replay, deletion, rebuild, and explicitly defined freshness behavior.
- Cross-boundary end-to-end scenarios should reopen durable state and prove that service, extension, and client results agree with the core oracle.
- Version-aware client tests cover both acknowledgement widths, version zero, invalid widths, monotonic commit identities, and waiting-enabled versus immediate exact reads. Context-eligibility APIs need a named workload and an independent disclosure and authorization oracle before implementation.

## Pitfalls

- Moving server convenience types into `yesno-core` turns an integration choice into a semver and dependency commitment.
- Recomputing routing at a service boundary can make every component internally consistent while disagreeing about which shard owns the data.
- Treating raw WAL as a query protocol exposes recovery internals; treating Flight batches as WAL loses ordering and crash semantics.
- Building `yesno-pg` with Cargo cannot prove compatibility with the PostgreSQL process that loads it.
- Pushing down an expression that cannot preserve PostgreSQL visibility, collation, null, or error semantics is a correctness bug, not an optimization miss.
- Letting a derived search index or client cache become authoritative makes recovery dependent on an integration that was intended to be rebuildable.
- Do not call exact-version waiting a session guarantee or silently reinterpret it as minimum-version reading after reclamation.
- Do not permit cross-tenant algebra merely because ordinals have the same numeric value; the parties must share an explicit identity namespace.
- Do not promise revocation for exported bitmap copies, or treat candidate filtering as a complete privacy control.
