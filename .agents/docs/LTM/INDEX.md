# Long-Term Memory Index

Durable, topic-organised project knowledge. Unlike `.agents/docs/JOURNAL.md` ( append-only, chronological ), these documents are meant to be **edited and refined** over time.

Documents arrive here two ways:

- `good-sleep` distills chronological `JOURNAL.md` entries into topic documents ( the source table below ).
- `deep-sleep` merges overlapping topic documents into broader synthesis documents ( the synthesis table below ).

`distill-memories` then promotes anything durable enough into `.agents/docs/OVERVIEW.md`, `.agents/docs/ARCHITECTURE.md`, or `.agents/docs/QUALITY_GATE.md`.

## Synthesis Documents

| Document | Summary | Consolidates |
|----------|---------|--------------|
| [Representation, Format, and Space](./representation-format-and-space-synthesis.md) | Container selection, format fidelity, persisted routing, zero-copy ownership, range summaries, and total space economics. | `container-representations-and-roaring-compatibility.md`; `storage-format-index-and-zero-copy.md`; `compression-models-and-space-economics.md` |
| [Set Evaluation, Planning, and Packed Lenses](./set-evaluation-and-planning-synthesis.md) | Eager kernels, lazy streams, logical planning, packed matrices and integers, view folds, and physical lowering. | `set-algebra-kernels-and-cardinality.md`; `chunk-stream-contracts-and-lazy-operators.md`; `expression-planning-statistics-and-segmentation.md`; `packed-lenses-matrix-bignum-and-views.md` |
| [Durability, Reclamation, Replication, and Snapshot Concurrency](./durability-reclamation-and-concurrency-synthesis.md) | Per-shard WAL histories, prefix visibility, explicit read-your-writes, commit-time invariants, checkpoint publication, sparse follower bootstrap, reclamation, retention, replacement, and PostgreSQL snapshot scopes. | `storage-format-index-and-zero-copy.md`; `allocation-reclamation-and-fsck.md`; `wal-mvcc-durability-and-concurrency.md`; `network-service-replication-and-operations.md`; `postgresql-extension-and-query-pushdown.md` |
| [Backup, Snapshot, and Cloud Operations](./backup-snapshot-and-cloud-operations-synthesis.md) | Consistent bases, immutable archives, wall-clock restore, provider leases, privilege boundaries, EBS materialization, and operator rollout. | `backup-archive-and-pitr.md`; `snapshot-leases-providers-and-privilege-separation.md`; `kubernetes-operator-and-failover.md`; `network-service-replication-and-operations.md` |
| [Testing, Gates, and Measurement](./testing-gates-and-measurement-synthesis.md) | Independent oracles, authority-sensitive cloud gates, evidence-preserving diagnostics, process-isolated measurements, remote workflow composition, and release verification. | `testing-and-e2e-harness.md`; `quality-gates-and-project-tooling.md`; `measurement-and-investigation-methodology.md` |
| [System Boundaries, Services, and Integrations](./system-boundaries-and-integrations-synthesis.md) | Core dependency boundaries, persisted service protocols, PostgreSQL ABI integration, version-aware clients, derived search indexes, and the context-eligibility product boundary. | `milestones-and-system-boundaries.md`; `database-apis-and-satellite-crates.md`; `network-service-replication-and-operations.md`; `postgresql-extension-and-query-pushdown.md`; `client-libraries-and-search-integrations.md`; `context-eligibility-product-direction.md` |

## Source Topic Documents

| Document | Summary |
|----------|---------|
| [Milestones and System Boundaries](./milestones-and-system-boundaries.md) | Milestone gates, deliberate scope limits, and the architectural promises between them. |
| [Container Representations and Roaring Compatibility](./container-representations-and-roaring-compatibility.md) | Container invariants, canonical codecs, Roaring interoperability, and representation transitions. |
| [Set Algebra Kernels and Cardinality](./set-algebra-kernels-and-cardinality.md) | Eager kernels, range mutation, cardinality-only paths, and specialization discipline. |
| [SIMD Arch Arms and Kernel Selection](./simd-arch-arms-and-kernel-selection.md) | NEON and x86_64 arms per kernel module, why emulation cannot price a vector arm, which kernels win on which instruction set, and how to earn a crate-level number. |
| [Chunk Stream Contracts and Lazy Operators](./chunk-stream-contracts-and-lazy-operators.md) | Stream laws, lazy operators, database-backed paged sources, error propagation, seek behavior, complement semantics, and n-ary execution. |
| [Expression Planning, Statistics, and Segmentation](./expression-planning-statistics-and-segmentation.md) | Rewrite rules, cost models, statistics, lazy-source occupancy, measured segmentation gates, and adaptive evaluation directions. |
| [Storage Format, Index, and Zero Copy](./storage-format-index-and-zero-copy.md) | Extents, segment and index layout, wide suffix search, mmap borrowing, online checksums, trailers, alignment, and compatibility. |
| [Allocation, Reclamation, and Fsck](./allocation-reclamation-and-fsck.md) | Free-space management, checkpoint reclamation, online verification versus diagnostic reconstruction, packing, integrity checks, and space bounds. |
| [WAL, MVCC, Durability, and Concurrency](./wal-mvcc-durability-and-concurrency.md) | WAL framing, transactions, apply/replay agreement, snapshot isolation, checkpoint lock splitting, crash recovery, and concurrency invariants. |
| [Checkpoint and Commit Lock Contention](./checkpoint-and-commit-lock-contention.md) | Why a concurrent writer costs a reader latency: the discriminating counters, the exclusive-hold anatomy, the input fuse and leaf spans, the commit-path hoists, and the tail-shape knobs. |
| [Flight Write Transactions and Mixed Mutation](./flight-write-transactions.md) | The write contract before and after: why no write ticket existed, why the handle is separate from the read ticket, the mixed-operation wire schema, ordering and idempotency guarantees, and what ownership/fencing still is not. |
| [Database APIs and Satellite Crates](./database-apis-and-satellite-crates.md) | Public database behavior, paged key streaming, batch planning, Arrow lending, DataFusion, C ABI, MySQL, replication, and Flight boundaries. |
| [Network Service, Replication, and Operations](./network-service-replication-and-operations.md) | Daemon lifecycle, durable control events, authorization channels, observability, replication, retention, TLS, and fencing. |
| [PostgreSQL Extension and Query Pushdown](./postgresql-extension-and-query-pushdown.md) | Hermetic PostgreSQL ABI builds, FDW and access methods, pushdown oracles, pending writes, reader pins, and snapshot isolation. |
| [Packed Lenses: Matrices, Integers, and Views](./packed-lenses-matrix-bignum-and-views.md) | Affine layouts, packed algebra, shared strided packing, view folds, the declined bit-sliced proposal, expansion, and Flight integration. |
| [Client Libraries and Search Integrations](./client-libraries-and-search-integrations.md) | Shared Flight requests plus Rust, Python, Java, Go, C++, PostgreSQL, MySQL, Tantivy, and search-engine boundaries. |
| [Backup, Archive, and Point-in-Time Recovery](./backup-archive-and-pitr.md) | Hot backup, archive publication, writer fencing, immutable history, version and wall-clock restore, and retention boundaries. |
| [Snapshot Leases, Providers, and Privilege Separation](./snapshot-leases-providers-and-privilege-separation.md) | Portable, ZFS, Btrfs, LVM, and EBS lease semantics, cleanup, privileged-agent ownership, and authority-sensitive gates. |
| [Kubernetes Operator, Failover, and Certificates](./kubernetes-operator-and-failover.md) | Retained-storage topology, fail-closed promotion, cert-manager mTLS, fencing limits, and single-harness operator E2E. |
| [Testing and End-to-End Harness](./testing-and-e2e-harness.md) | Test-layer responsibilities, sabotage calibration, forced-path agreement, Monty scenarios, generators, fixtures, and operational coverage. |
| [Quality Gates and Project Tooling](./quality-gates-and-project-tooling.md) | Local and deep gates, citation and rustdoc checks, checker sensitivity, CI/release evidence, formatting, and repository hygiene. |
| [Formal Model and Proof Obligations](./formal-model-and-proof-obligations.md) | Set semantics, refinement arguments, restriction algebra, and reviewable proof obligations. |
| [Compression Models and Space Economics](./compression-models-and-space-economics.md) | Entropy bounds, run-aware models, corpus histograms, and total storage economics. |
| [Measurement and Investigation Methodology](./measurement-and-investigation-methodology.md) | Experimental controls, evaluation frames, repeated sweeps, benchmark provenance, instrument canaries, and durable evidence. |
| [Context Eligibility Product Direction](./context-eligibility-product-direction.md) | Candidate filtering, governed shares, identity namespaces, disclosure controls, versioned receipts, and the BYOC-first product boundary. |

## Preserved Source

| Document | Summary |
|----------|---------|
| [Removed source: `stats.rs`](./removed-stats-instrument-source.md) | Complete source of the removed `( m, r )` histogram and compressed-binary-trie cost models, with restore instructions. Kept because the trie modelling was never committed. |
