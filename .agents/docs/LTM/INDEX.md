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
| [Plugin Hosting and Isolation](./plugin-hosting-and-isolation-synthesis.md) | Current served-channel lifetime and security boundary, sidecar deployment, measured arena costs, and the retired in-process ABI. | `plugin-channel-protocol-and-security.md`; `out-of-process-plugin-via-foreign-reader.md`; `operator-hosted-plugin-container-plan.md`; `arena-cgroup-charging.md`; `plugin-shape-performance.md`; `removed-cdylib-plugin-abi.md` |
| [Query Acceleration Admission](./query-acceleration-admission-synthesis.md) | Whole-query admission for GPU, JIT and SIMD paths, with planner costs and real-traffic reuse measurement boundaries. | `gpu-offload-on-unified-memory.md`; `query-hotspot-observation.md`; `simd-arch-arms-and-kernel-selection.md`; `expression-planning-statistics-and-segmentation.md` |

## Source Topic Documents

| Document | Summary |
|----------|---------|
| [Milestones and System Boundaries](./milestones-and-system-boundaries.md) | Milestone gates, deliberate scope limits, and the architectural promises between them. |
| [Container Representations and Roaring Compatibility](./container-representations-and-roaring-compatibility.md) | Container invariants, canonical codecs, Roaring interoperability, and representation transitions. |
| [Set Algebra Kernels and Cardinality](./set-algebra-kernels-and-cardinality.md) | Eager kernels, range mutation, cardinality-only paths, and specialization discipline. |
| [GPU Offload on Unified Memory](./gpu-offload-on-unified-memory.md) | GB10 bandwidth and residency economics, the gap between kernel and wired scans, and the deferred OpenCL path's measured 2.56x warm end-to-end gain. |
| [Query Hotspot Observation](./query-hotspot-observation.md) | Exact reuse-distance measurement, bounded production tracing, and the identity and thread boundaries needed to judge JIT and GPU cache admission. |
| [SIMD Arch Arms and Kernel Selection](./simd-arch-arms-and-kernel-selection.md) | NEON and x86_64 arms, Cranelift fused-DAG lowering and production admission, why emulation cannot price a vector arm, and how to earn a whole-query number. |
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
| [Packed Lenses: Matrices, Integers, and Views](./packed-lenses-matrix-bignum-and-views.md) | Affine layouts, packed algebra, shared strided packing, scalar and SIMD interleaved terminals, view folds, the declined bit-sliced proposal, expansion, and Flight integration. |
| [Plugin Shape Performance](./plugin-shape-performance.md) | In-process table versus IPC copy and round trips, why batching wins, and the separate-process CPU and memory evidence for direct arena encoding. |
| [Cold Peer Restore and Page Warming](./cold-peer-page-warming-20261006.md) | Qwen session cold peer read, file-page footprint, data versus index faults, and measured targeted prewarm bounds. |
| [Plugin Channel Protocol and Security](./plugin-channel-protocol-and-security.md) | Arena and inline frames, snapshot paging, admission and access controls, safe rebootstrap, and the wire-contract failures caught by peer tests. |
| [Out-of-Process Plugin and Foreign-Reader Boundary](./out-of-process-plugin-via-foreign-reader.md) | Historical shared-directory exploration, the later served-channel correction, and the per-slot `flock` fix for foreign readers across PID namespaces. |
| [Making a Plugin Container Available Through the Operator](./operator-hosted-plugin-container-plan.md) | Plan for `spec.plugin` as a sidecar: why the UDS forces same-Pod and forbids a shared PID namespace, why the in-process `cdylib` is out of the picture, the config precedence work that makes it usable with a config Secret, and the two resource questions ( cgroup charging, unbounded peers ) that need measuring rather than asserting. |
| [Who Pays for a Touched Arena Page](./arena-cgroup-charging.md) | Measured: a shared `memfd`'s pages are charged in full to **both** cgroups that map them, so an operator must size the arena into both containers -- inverting the plan's first-touch assumption. Includes the phase-C anomaly left unexplained and the one-arm experiment that would settle it. |
| [Removed: the in-process cdylib Plugin ABI](./removed-cdylib-plugin-abi.md) | The published C header preserved in full, an inventory of what was deleted, and the three properties of in-process extension that retired it: a shared address space, an abort on an escaped panic, and leases invisible to the shutdown proof. |
| [A yesnod-hosted Plugin Facility: Design](./hosted-plugin-abi-design.md) | SUPERSEDED, the ABI was removed 2026-09-29. The proposed two-table ABI: why a lease must own a Snapshot, the settled rebootstrap semantics, borrowed lane payloads, role and generation lifecycle, host-driven listener, and the three changes yesno-core needs. |
| [A yesnod-hosted Plugin ABI](./hosted-plugin-abi-assessment.md) | SUPERSEDED, the ABI was removed 2026-09-29. What yesno-c and yesnod supply for an in-process plugin over the live Db, the eleven missing primitives, and the two design problems: leases invisible to the shutdown proof, and a rebootstrap hazard two documents describe differently. |
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
| [Chunk-Local Patch Writes](./chunk-local-patch-writes.md) | The `patch_chunk` operation and its WAL record, the live/replay agreement it is designed around, and the durable ingest measurement that justified it. |
| [Contiguous Dense Span Extents](./contiguous-dense-span-extents-plan.md) | DONE for the dense case: shrinking the bitmap slot to exactly its payload and moving the extent trailer to a table at the slab-body tail made consecutive payloads adjacent and page-aligned ( 0 of 63 adjacent pairs to 63 of 63 ), so a dense window is lent rather than gathered. Records why both deferred decisions turned out moot, and why the larger shared-region variant is closed -- first for the wrong reason, then correctly. |
| [Redis-backed Cache Performance](./redis-backed-cache-performance-20261002.md) | Reproducible 8 MiB and 512 KiB half-dense cache read comparison against Redis 7.4.10 over loopback, with durability-matched but API-asymmetric overwrite timings and the Flight socket tail. |

## Preserved Source

| Document | Summary |
|----------|---------|
| [Removed source: `stats.rs`](./removed-stats-instrument-source.md) | Complete source of the removed `( m, r )` histogram and compressed-binary-trie cost models, with restore instructions. Kept because the trie modelling was never committed. |
