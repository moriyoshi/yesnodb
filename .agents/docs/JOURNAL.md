# yesno Development Journal

Append-only working log. New entries go at the **end** of the file, under a `## YYYY-MM-DD — <short title>` heading. Do not edit or delete existing sections; the `reconcile-journal-ltm` skill is the only sanctioned remover, and only for entries already consolidated into `.agents/docs/LTM/`.

What belongs here:

- design decisions and the alternatives you rejected, with the reason
- bug classes found, and which test layer did or did not catch them
- benchmark results, before and after
- peer code review findings and their resolution
- anything a future agent would otherwise have to rediscover

What does not belong here: routine "ran the gate, it passed" notes, and open follow-ups ( those go to `.agents/docs/TODO.md` ).

Entries accumulate until `good-sleep` distills them into topic documents under `.agents/docs/LTM/`, at which point a `## LTM Consolidation Record` section records the mapping.
---

## LTM Consolidation Record

**Audited entry by entry on 2026-10-12**, against the long-term memory documents rather than
against the previous record. Every substantive entry this file carried has been verified present
in `.agents/docs/LTM/` -- by probing distinctive facts in the target document, not by trusting a
mapping row -- and has been removed. What remains is this record.

The audit covered the 87 entries spanning 2026-10-03 to 2026-10-12. Entries before that range had
already been consolidated and removed by an earlier pass; their mapping is preserved below and is
now the only record of where that material lives.

**Two things were rescued rather than deleted.** The deep-sleep synthesis entry of 2026-10-03
carried a follow-up that had never reached `TODO.md` -- `ARCHITECTURE.md` still describes a removed
in-process plugin facility and a lint gap the workspace-wide Clippy gate closed -- which is now
filed as `architecture-describes-a-removed-plugin-facility`. That entry's own synthesis mapping is
folded into the synthesis table below, and the standalone list is corrected: it previously
described the state *before* those two syntheses existed.

### Journal Section Coverage

| Journal section or range | Durable home |
|---|---|
| `2026-08-24 — Agentic harness adopted from winterbaume` through `2026-08-27 — Work summary, addendum: what changed after the summary above` | [Milestones and System Boundaries](LTM/milestones-and-system-boundaries.md), [Container Representations and Roaring Compatibility](LTM/container-representations-and-roaring-compatibility.md), [Set Algebra Kernels and Cardinality](LTM/set-algebra-kernels-and-cardinality.md), [Chunk Stream Contracts and Lazy Operators](LTM/chunk-stream-contracts-and-lazy-operators.md), [Expression Planning, Statistics, and Segmentation](LTM/expression-planning-statistics-and-segmentation.md), [Storage Format, Index, and Zero Copy](LTM/storage-format-index-and-zero-copy.md), [Allocation, Reclamation, and Fsck](LTM/allocation-reclamation-and-fsck.md), [WAL, MVCC, Durability, and Concurrency](LTM/wal-mvcc-durability-and-concurrency.md), [Database APIs and Satellite Crates](LTM/database-apis-and-satellite-crates.md), [Testing and End-to-End Harness](LTM/testing-and-e2e-harness.md), [Quality Gates and Project Tooling](LTM/quality-gates-and-project-tooling.md), [Formal Model and Proof Obligations](LTM/formal-model-and-proof-obligations.md), [Compression Models and Space Economics](LTM/compression-models-and-space-economics.md), [Measurement and Investigation Methodology](LTM/measurement-and-investigation-methodology.md), [TODO](TODO.md) |
| `2026-08-27 — Deep-sleep synthesis of the LTM source topics` through `2026-08-28 — stats.rs removed, folded into LTM` | [Milestones and System Boundaries](LTM/milestones-and-system-boundaries.md), [Quality Gates and Project Tooling](LTM/quality-gates-and-project-tooling.md), [Formal Model and Proof Obligations](LTM/formal-model-and-proof-obligations.md), [Compression Models and Space Economics](LTM/compression-models-and-space-economics.md), [Measurement and Investigation Methodology](LTM/measurement-and-investigation-methodology.md) |
| `2026-08-28 — The bitmap kernels vectorized, and what 35x was measuring` through `2026-08-28 — The module surface: 14 public modules to 10, and the compiler drew the line` | [Set Algebra Kernels and Cardinality](LTM/set-algebra-kernels-and-cardinality.md), [Container Representations and Roaring Compatibility](LTM/container-representations-and-roaring-compatibility.md), [Testing and End-to-End Harness](LTM/testing-and-e2e-harness.md), [Measurement and Investigation Methodology](LTM/measurement-and-investigation-methodology.md), [Quality Gates and Project Tooling](LTM/quality-gates-and-project-tooling.md), [Milestones and System Boundaries](LTM/milestones-and-system-boundaries.md) |
| `2026-08-28 — vshard-map-is-a-modulo was a data-loss bug, and the item said it was harmless` through `2026-08-29 — Client-side fencing, and the one check that is a layer job` | [Storage Format, Index, and Zero Copy](LTM/storage-format-index-and-zero-copy.md), [WAL, MVCC, Durability, and Concurrency](LTM/wal-mvcc-durability-and-concurrency.md), [Network Service, Replication, and Operations](LTM/network-service-replication-and-operations.md), [Packed Lenses: Matrices, Integers, and Views](LTM/packed-lenses-matrix-bignum-and-views.md), [Database APIs and Satellite Crates](LTM/database-apis-and-satellite-crates.md), [Formal Model and Proof Obligations](LTM/formal-model-and-proof-obligations.md), [Testing and End-to-End Harness](LTM/testing-and-e2e-harness.md), [Quality Gates and Project Tooling](LTM/quality-gates-and-project-tooling.md), [Measurement and Investigation Methodology](LTM/measurement-and-investigation-methodology.md) |
| `pack/: one strided-packing facility under matrix/ and bignum/` through `2026-08-30 — Relicensed Apache-2.0 -> MIT OR Apache-2.0` | [Packed Lenses: Matrices, Integers, and Views](LTM/packed-lenses-matrix-bignum-and-views.md), [PostgreSQL Extension and Query Pushdown](LTM/postgresql-extension-and-query-pushdown.md), [Network Service, Replication, and Operations](LTM/network-service-replication-and-operations.md), [Database APIs and Satellite Crates](LTM/database-apis-and-satellite-crates.md), [Milestones and System Boundaries](LTM/milestones-and-system-boundaries.md), [Testing and End-to-End Harness](LTM/testing-and-e2e-harness.md), [Quality Gates and Project Tooling](LTM/quality-gates-and-project-tooling.md) |
| `2026-08-30 — An oracle for the index AM, and a hazard that turned out to be false` through `2026-08-30 — yesnodb is a namespace package` | [PostgreSQL Extension and Query Pushdown](LTM/postgresql-extension-and-query-pushdown.md), [Testing and End-to-End Harness](LTM/testing-and-e2e-harness.md), [Database APIs and Satellite Crates](LTM/database-apis-and-satellite-crates.md), [Formal Model and Proof Obligations](LTM/formal-model-and-proof-obligations.md), [Measurement and Investigation Methodology](LTM/measurement-and-investigation-methodology.md), [Packed Lenses: Matrices, Integers, and Views](LTM/packed-lenses-matrix-bignum-and-views.md), [Client Libraries and Search Integrations](LTM/client-libraries-and-search-integrations.md) |
| `2026-08-30 - Deep-sleep synthesis refresh` through `2026-08-30 - The on-disk format is now a standing document` | [Quality Gates and Project Tooling](LTM/quality-gates-and-project-tooling.md), [Database APIs and Satellite Crates](LTM/database-apis-and-satellite-crates.md), [Storage Format, Index, and Zero Copy](LTM/storage-format-index-and-zero-copy.md) |
| `2026-08-30 - Test plan: WAL generations` through `2026-08-30 - Archive capture retries cannot collide with orphan snapshots` | [WAL, MVCC, Durability, and Concurrency](LTM/wal-mvcc-durability-and-concurrency.md), [Network Service, Replication, and Operations](LTM/network-service-replication-and-operations.md), [Backup, Archive, and Point-in-Time Recovery](LTM/backup-archive-and-pitr.md), [Client Libraries and Search Integrations](LTM/client-libraries-and-search-integrations.md) |
| `2026-08-30 - Test plan: server utility crate boundary` through `2026-08-30 - External database E2E uses one generic fixture surface` | [Backup, Archive, and Point-in-Time Recovery](LTM/backup-archive-and-pitr.md), [Snapshot Leases, Providers, and Privilege Separation](LTM/snapshot-leases-providers-and-privilege-separation.md), [Kubernetes Operator, Failover, and Certificates](LTM/kubernetes-operator-and-failover.md), [PostgreSQL Extension and Query Pushdown](LTM/postgresql-extension-and-query-pushdown.md), [Network Service, Replication, and Operations](LTM/network-service-replication-and-operations.md), [Testing and End-to-End Harness](LTM/testing-and-e2e-harness.md), [Client Libraries and Search Integrations](LTM/client-libraries-and-search-integrations.md) |
| `2026-08-31 - MySQL E2E covers embedded and Flight backends` through `2026-08-31 - Quality Gate: deployed archive on native snapshots and Winterbaume` | [Database APIs and Satellite Crates](LTM/database-apis-and-satellite-crates.md), [Backup, Archive, and Point-in-Time Recovery](LTM/backup-archive-and-pitr.md), [Kubernetes Operator, Failover, and Certificates](LTM/kubernetes-operator-and-failover.md), [Network Service, Replication, and Operations](LTM/network-service-replication-and-operations.md), [Snapshot Leases, Providers, and Privilege Separation](LTM/snapshot-leases-providers-and-privilege-separation.md), [Testing and End-to-End Harness](LTM/testing-and-e2e-harness.md) |
| `2026-08-31 - Test plan: ordinal-set literals in the query language` through `2026-08-31 - Test plan: ordinal-set literal client propagation` | [Client Libraries and Search Integrations](LTM/client-libraries-and-search-integrations.md), [PostgreSQL Extension and Query Pushdown](LTM/postgresql-extension-and-query-pushdown.md) |
| `2026-08-31 - Quality Gate: Amazon EBS snapshot backend` through `2026-08-31 - Test plan: ECS/Fargate EBS materialization` | [Snapshot Leases, Providers, and Privilege Separation](LTM/snapshot-leases-providers-and-privilege-separation.md), [Testing and End-to-End Harness](LTM/testing-and-e2e-harness.md), [Quality Gates and Project Tooling](LTM/quality-gates-and-project-tooling.md), [TODO](TODO.md) |
| `2026-08-31 - Ordinal-set literals and Flight surface result` | [Client Libraries and Search Integrations](LTM/client-libraries-and-search-integrations.md) |
| `2026-08-31 - Correction: archiver-owned deferred EBS materialization` through `2026-08-31 - Local LVM file-bearing snapshot leases` | [Snapshot Leases, Providers, and Privilege Separation](LTM/snapshot-leases-providers-and-privilege-separation.md), [Backup, Archive, and Point-in-Time Recovery](LTM/backup-archive-and-pitr.md), [Testing and End-to-End Harness](LTM/testing-and-e2e-harness.md), [Quality Gates and Project Tooling](LTM/quality-gates-and-project-tooling.md), [TODO](TODO.md) |
| `2026-09-01 - Privileged LVM snapshot agent and release-barrier RPCs` through `2026-09-01 - The AWS acceptance harness had leaked into the operator guide` | [Snapshot Leases, Providers, and Privilege Separation](LTM/snapshot-leases-providers-and-privilege-separation.md), [Testing and End-to-End Harness](LTM/testing-and-e2e-harness.md), [Quality Gates and Project Tooling](LTM/quality-gates-and-project-tooling.md), [TODO](TODO.md) |
| `2026-09-01 — The filesystem snapshot backends were never run unprivileged, and two of the three could not have been` through `2026-09-01 — Deferring the btrfs-uapi decision in favour of owning the seam` | [Snapshot Leases, Providers, and Privilege Separation](LTM/snapshot-leases-providers-and-privilege-separation.md), [Network Service, Replication, and Operations](LTM/network-service-replication-and-operations.md), [Testing and End-to-End Harness](LTM/testing-and-e2e-harness.md), [TODO](TODO.md) |
| `2026-09-01 — Three E2E images became one, and the identity split that made it possible` through `2026-09-02 — Work summary: the E2E image consolidation, and what the gates cost` | [Quality Gates and Project Tooling](LTM/quality-gates-and-project-tooling.md), [Testing and End-to-End Harness](LTM/testing-and-e2e-harness.md), [Snapshot Leases, Providers, and Privilege Separation](LTM/snapshot-leases-providers-and-privilege-separation.md) |
| `2026-09-02 — The AWS gate's orchestration moved out of shell and into a scenario` through `2026-09-05 — Work summary: the AWS gate's deferred arms, and seventeen live runs` | [Testing and End-to-End Harness](LTM/testing-and-e2e-harness.md), [Snapshot Leases, Providers, and Privilege Separation](LTM/snapshot-leases-providers-and-privilege-separation.md), [Kubernetes Operator, Failover, and Certificates](LTM/kubernetes-operator-and-failover.md), [Quality Gates and Project Tooling](LTM/quality-gates-and-project-tooling.md), [Measurement and Investigation Methodology](LTM/measurement-and-investigation-methodology.md), [TODO](TODO.md) |
| `2026-09-05 — The EBS snapshot backend in yesno-operator` through `2026-09-05 — Work summary: the operator arm of the AWS gate, and four live runs` | [Kubernetes Operator, Failover, and Certificates](LTM/kubernetes-operator-and-failover.md), [Network Service, Replication, and Operations](LTM/network-service-replication-and-operations.md), [Snapshot Leases, Providers, and Privilege Separation](LTM/snapshot-leases-providers-and-privilege-separation.md), [Testing and End-to-End Harness](LTM/testing-and-e2e-harness.md), [Measurement and Investigation Methodology](LTM/measurement-and-investigation-methodology.md), [TODO](TODO.md) |
| `2026-09-06 — Sparse base snapshots: not shipping what is not there` through `2026-09-06 — Work summary: sparse base snapshots, and a measurement that needed a room of its own` | [Network Service, Replication, and Operations](LTM/network-service-replication-and-operations.md), [Testing and End-to-End Harness](LTM/testing-and-e2e-harness.md), [Measurement and Investigation Methodology](LTM/measurement-and-investigation-methodology.md) |
| `2026-09-06 — The lint gate in CLAUDE.md lints two crates out of fourteen` through `2026-09-06 — Correction: the clippy finding was a rediscovery, and that is the finding` | [Quality Gates and Project Tooling](LTM/quality-gates-and-project-tooling.md), [TODO](TODO.md) |
| `2026-09-06 — One image for local Docker, ECS, and Kubernetes, cross-built rather than emulated` | [Quality Gates and Project Tooling](LTM/quality-gates-and-project-tooling.md), [TODO](TODO.md) |
| `2026-09-06 - WAL commit-time stamping, and why it went in a body` | [WAL, MVCC, Durability, and Concurrency](LTM/wal-mvcc-durability-and-concurrency.md), [Backup, Archive, and Point-in-Time Recovery](LTM/backup-archive-and-pitr.md) |
| `2026-09-06 — yesno-snapshot-agent folded into the unified image, and LVM without emulation` | [Quality Gates and Project Tooling](LTM/quality-gates-and-project-tooling.md), [Snapshot Leases, Providers, and Privilege Separation](LTM/snapshot-leases-providers-and-privilege-separation.md), [TODO](TODO.md) |
| `2026-09-06 - Wall-clock recovery targets, and the ergonomics around them` | [Backup, Archive, and Point-in-Time Recovery](LTM/backup-archive-and-pitr.md) |
| `2026-09-06 — CD: the gate and the publish in one pipeline` through `2026-09-06 — Work summary: one published image, and four ways a multi-arch build lies quietly` | [Quality Gates and Project Tooling](LTM/quality-gates-and-project-tooling.md), [Testing and End-to-End Harness](LTM/testing-and-e2e-harness.md), [Measurement and Investigation Methodology](LTM/measurement-and-investigation-methodology.md), [TODO](TODO.md) |
| `2026-09-06 - Archive retention as a duration, and reachability-based reclamation` through `2026-09-06 — Work summary: point-in-time recovery, in three gated phases` | [Backup, Archive, and Point-in-Time Recovery](LTM/backup-archive-and-pitr.md), [Testing and End-to-End Harness](LTM/testing-and-e2e-harness.md), [Measurement and Investigation Methodology](LTM/measurement-and-investigation-methodology.md), [TODO](TODO.md) |
| `2026-09-06 — Deep-sleep synthesis refresh for recovery and operational evidence` | [Long-Term Memory Index](LTM/INDEX.md) and the synthesis documents it names |
| `2026-09-06 — is-range-empty-short-circuits: range_summary stops at the first set value` through `2026-09-06 — A TODO sweep, and three entries that were wrong about their own subject` | [Set Algebra Kernels and Cardinality](LTM/set-algebra-kernels-and-cardinality.md), [Quality Gates and Project Tooling](LTM/quality-gates-and-project-tooling.md), [Formal Model and Proof Obligations](LTM/formal-model-and-proof-obligations.md), [Storage Format, Index, and Zero Copy](LTM/storage-format-index-and-zero-copy.md), and [Measurement and Investigation Methodology](LTM/measurement-and-investigation-methodology.md) |
| `2026-09-06 — The deep gate's three measurement steps had stopped running, and --bin was being added one call site at a time` through `2026-09-06 — CI was a strict subset of the gate, and nothing said so` | [Quality Gates and Project Tooling](LTM/quality-gates-and-project-tooling.md), [Storage Format, Index, and Zero Copy](LTM/storage-format-index-and-zero-copy.md), [Network Service, Replication, and Operations](LTM/network-service-replication-and-operations.md), [Testing and the End-to-End Harness](LTM/testing-and-e2e-harness.md), and [Measurement and Investigation Methodology](LTM/measurement-and-investigation-methodology.md) |
| `2026-09-06 — Benchmarking after the kernel change, and two bench headers describing a crate that no longer exists` through `2026-09-07 — Mask-and-compact, and a threshold that its own success invalidated` | [Set Algebra Kernels and Cardinality](LTM/set-algebra-kernels-and-cardinality.md) and [Measurement and Investigation Methodology](LTM/measurement-and-investigation-methodology.md) |
| `2026-09-07 — Sweep: verifying the commands and examples the backlog tells you to trust` through `2026-09-07 — segmentation-setup-is-unbounded closed: the loss it names no longer exists` | [Testing and the End-to-End Harness](LTM/testing-and-e2e-harness.md), [Quality Gates and Project Tooling](LTM/quality-gates-and-project-tooling.md), [Expression Planning, Statistics, and Segmentation](LTM/expression-planning-statistics-and-segmentation.md), and [Measurement and Investigation Methodology](LTM/measurement-and-investigation-methodology.md) |
| `2026-09-07 — Sweep: closing items that are records, and one verified before closing` through `2026-09-07 — Re-measuring a verdict's inputs, and a unit bug in my own parser` | [Measurement and Investigation Methodology](LTM/measurement-and-investigation-methodology.md) and [Set Algebra Kernels and Cardinality](LTM/set-algebra-kernels-and-cardinality.md) |
| `2026-09-07 — A new finding: endpoint queries materialize the whole posting list` through `2026-09-08 — Auditing the SAFETY-comment discipline, and three checkers that disagreed` | [Storage Format, Index, and Zero Copy](LTM/storage-format-index-and-zero-copy.md), [Client Libraries and Search Integrations](LTM/client-libraries-and-search-integrations.md), [Formal Model and Proof Obligations](LTM/formal-model-and-proof-obligations.md), [Quality Gates and Project Tooling](LTM/quality-gates-and-project-tooling.md), and [Measurement and Investigation Methodology](LTM/measurement-and-investigation-methodology.md) |
| `2026-09-08 — Closed TODO items folded in from TODO.md` through `2026-09-08 — Closed items removed from TODO.md, and the citations repointed` | The source topic documents listed in [Long-Term Memory Index](LTM/INDEX.md) |
| `2026-09-08 — I filed four items as hardware-blocked; the harness already had the hardware` through `2026-09-08 — A container-runtime propagation gate, and two requirements it surfaced` | [Snapshot Leases, Providers, and Privilege Separation](LTM/snapshot-leases-providers-and-privilege-separation.md), [Testing and the End-to-End Harness](LTM/testing-and-e2e-harness.md), and [Measurement and Investigation Methodology](LTM/measurement-and-investigation-methodology.md) |
| `2026-09-08 — The operator gate runs here, and it fails reproducibly` through `2026-09-09 — Operator promotion: the fix, and why the trigger is a state and not a timer` | [Kubernetes Operator, Failover, and Certificates](LTM/kubernetes-operator-and-failover.md) and [Measurement and Investigation Methodology](LTM/measurement-and-investigation-methodology.md) |
| `2026-09-09 — One metrics listener for the life of the process` through `2026-09-09 — The always-identity-argument sweep, and three stale hits` | [Network Service, Replication, and Operations](LTM/network-service-replication-and-operations.md), [Storage Format, Index, and Zero Copy](LTM/storage-format-index-and-zero-copy.md), and [Measurement and Investigation Methodology](LTM/measurement-and-investigation-methodology.md) |
| `2026-09-09 — Deep gate on the day's changes, and a durability failure that is not a flake` through `2026-09-10 — pitr_retention root cause: a window that promised more than a restore would stage` | [Backup, Archive, and Point-in-Time Recovery](LTM/backup-archive-and-pitr.md), [Testing and the End-to-End Harness](LTM/testing-and-e2e-harness.md), and [Measurement and Investigation Methodology](LTM/measurement-and-investigation-methodology.md) |
| `2026-09-12 — fallible-posting-source: a design item that had already been decided` | [Database APIs and Satellite Crates](LTM/database-apis-and-satellite-crates.md) and [Testing and the End-to-End Harness](LTM/testing-and-e2e-harness.md) |
| `2026-09-12 — The space/retention observability decision, and three items it unblocked` | [Allocation, Reclamation, and Fsck](LTM/allocation-reclamation-and-fsck.md), [Database APIs and Satellite Crates](LTM/database-apis-and-satellite-crates.md), and [Network Service, Replication, and Operations](LTM/network-service-replication-and-operations.md) |
| `2026-09-12 — The cost model measured, and deliberately not changed` | [Expression Planning, Statistics, and Segmentation](LTM/expression-planning-statistics-and-segmentation.md) and [Measurement and Investigation Methodology](LTM/measurement-and-investigation-methodology.md) |
| `2026-09-12 — Assessed as an OLTP store: what the consistency model actually is` | [WAL, MVCC, Durability, and Concurrency](LTM/wal-mvcc-durability-and-concurrency.md) and [Context Eligibility Product Direction](LTM/context-eligibility-product-direction.md) |
| `2026-09-12 — Product direction: an operational context-eligibility plane` | [Context Eligibility Product Direction](LTM/context-eligibility-product-direction.md) |
| `2026-09-12 — Read-your-writes becomes expressible, and three constructions that did not work` | [WAL, MVCC, Durability, and Concurrency](LTM/wal-mvcc-durability-and-concurrency.md), [Client Libraries and Search Integrations](LTM/client-libraries-and-search-integrations.md), [Database APIs and Satellite Crates](LTM/database-apis-and-satellite-crates.md), [Quality Gates and Project Tooling](LTM/quality-gates-and-project-tooling.md), [Testing and the End-to-End Harness](LTM/testing-and-e2e-harness.md), and [Measurement and Investigation Methodology](LTM/measurement-and-investigation-methodology.md) |
| `2026-09-12 — Consolidated: what this sweep closed, and where each closure's reasoning now lives` | [Long-Term Memory Index](LTM/INDEX.md), [Measurement and Investigation Methodology](LTM/measurement-and-investigation-methodology.md), and the source topic documents named by the section |
| `2026-09-13 — CI had run fifteen times and never passed, and the lint gate could not see why` | [Quality Gates and Project Tooling](LTM/quality-gates-and-project-tooling.md) |
| `2026-09-13 — Deep-sleep refresh for visibility, CI evidence, and context eligibility` | [Testing, Gates, and Measurement](LTM/testing-gates-and-measurement-synthesis.md), [System Boundaries, Services, and Integrations](LTM/system-boundaries-and-integrations-synthesis.md), [Durability, Reclamation, Replication, and Snapshot Concurrency](LTM/durability-reclamation-and-concurrency-synthesis.md), [Quality Gates and Project Tooling](LTM/quality-gates-and-project-tooling.md), and [TODO](TODO.md) |
| `2026-09-13 -- emoji removed from the whole tree, and the rule that keeps them out` | [Quality Gates and Project Tooling](LTM/quality-gates-and-project-tooling.md) |
| `2026-09-13 -- a consumer's bit-sliced-lens proposal audited, and Snapshot::key_stream built from its §7` | [Packed Lenses: Matrices, Integers, and Views](LTM/packed-lenses-matrix-bignum-and-views.md), [Database APIs and Satellite Crates](LTM/database-apis-and-satellite-crates.md), [Chunk Stream Contracts and Lazy Operators](LTM/chunk-stream-contracts-and-lazy-operators.md), [Expression Planning, Statistics, and Segmentation](LTM/expression-planning-statistics-and-segmentation.md), [WAL, MVCC, Durability, and Concurrency](LTM/wal-mvcc-durability-and-concurrency.md), [Testing and the End-to-End Harness](LTM/testing-and-e2e-harness.md), [Measurement and Investigation Methodology](LTM/measurement-and-investigation-methodology.md), and [TODO](TODO.md) |
| `2026-09-14 -- leaf-search-truncated-suffixes-above-eight-bytes` | [Storage Format, Index, and Zero-Copy Reads](LTM/storage-format-index-and-zero-copy.md), [Testing and the End-to-End Harness](LTM/testing-and-e2e-harness.md), and [Measurement and Investigation Methodology](LTM/measurement-and-investigation-methodology.md) |
| `2026-09-14 -- dangling-backlog-citations: slugs cited from source that record nothing` | [WAL, MVCC, Durability, and Concurrency](LTM/wal-mvcc-durability-and-concurrency.md), [Quality Gates and Project Tooling](LTM/quality-gates-and-project-tooling.md), [Measurement and Investigation Methodology](LTM/measurement-and-investigation-methodology.md), and [TODO](TODO.md) |
| `2026-09-14 -- unwired sweep re-run, and what a one-day-old dead symbol shows` | [Quality Gates and Project Tooling](LTM/quality-gates-and-project-tooling.md), [Chunk Stream Contracts and Lazy Operators](LTM/chunk-stream-contracts-and-lazy-operators.md), and [Measurement and Investigation Methodology](LTM/measurement-and-investigation-methodology.md) |
| `2026-09-14 -- dangling-backlog-citations closed: baseline 16 -> 0` | [Quality Gates and Project Tooling](LTM/quality-gates-and-project-tooling.md), [Measurement and Investigation Methodology](LTM/measurement-and-investigation-methodology.md), and [TODO](TODO.md) |
| `2026-09-14 -- chunks_remaining was a duplicate, not an orphan` | [Chunk Stream Contracts and Lazy Operators](LTM/chunk-stream-contracts-and-lazy-operators.md), [Testing and the End-to-End Harness](LTM/testing-and-e2e-harness.md), and [Measurement and Investigation Methodology](LTM/measurement-and-investigation-methodology.md) |
| `2026-09-14 -- TODO sweep: what it actually produced` | [Quality Gates and Project Tooling](LTM/quality-gates-and-project-tooling.md), [Measurement and Investigation Methodology](LTM/measurement-and-investigation-methodology.md), and [TODO](TODO.md) |
| `2026-09-14 -- re-sweep: three of my own eight entries were defective` | [Backup, Archive, and Point-in-Time Recovery](LTM/backup-archive-and-pitr.md), [Network Service, Replication, and Operations](LTM/network-service-replication-and-operations.md), [Quality Gates and Project Tooling](LTM/quality-gates-and-project-tooling.md), [Measurement and Investigation Methodology](LTM/measurement-and-investigation-methodology.md), and [TODO](TODO.md) |
| `2026-09-14 -- eleven broken doc links, and a lint that was already running` | [Quality Gates and Project Tooling](LTM/quality-gates-and-project-tooling.md) |
| `2026-09-14 -- SEGMENT_MIN_CHUNKS has a verified caller, and it is below the gate` | [Expression Planning, Statistics, and Segmentation](LTM/expression-planning-statistics-and-segmentation.md) and [Measurement and Investigation Methodology](LTM/measurement-and-investigation-methodology.md) |
| `2026-09-14 -- planner-cost-is-o-chunks re-measured: the cap bounded the shape, it did not change it` | [Expression Planning, Statistics, and Segmentation](LTM/expression-planning-statistics-and-segmentation.md), [Measurement and Investigation Methodology](LTM/measurement-and-investigation-methodology.md), and [TODO](TODO.md) |
| `2026-09-14 -- internal-key-compression closed, and its own falsifier could never have fired` | [Storage Format, Index, and Zero-Copy Reads](LTM/storage-format-index-and-zero-copy.md) and [Measurement and Investigation Methodology](LTM/measurement-and-investigation-methodology.md) |
| `2026-09-14 -- the sanitizers do not cover the mmap unsafe, and never did` | [Storage Format, Index, and Zero-Copy Reads](LTM/storage-format-index-and-zero-copy.md), [Allocation, Reclamation, and Fsck](LTM/allocation-reclamation-and-fsck.md), [Quality Gates and Project Tooling](LTM/quality-gates-and-project-tooling.md), [Measurement and Investigation Methodology](LTM/measurement-and-investigation-methodology.md), and [TODO](TODO.md) |
| `2026-09-14 -- the read-path checksum cache landed, and the check needed an error channel first` | [Storage Format, Index, and Zero-Copy Reads](LTM/storage-format-index-and-zero-copy.md), [Allocation, Reclamation, and Fsck](LTM/allocation-reclamation-and-fsck.md), [Testing and the End-to-End Harness](LTM/testing-and-e2e-harness.md), and [Measurement and Investigation Methodology](LTM/measurement-and-investigation-methodology.md) |
| `2026-09-14 -- running the actual gate found two failures my hand-assembled one could not` | [Quality Gates and Project Tooling](LTM/quality-gates-and-project-tooling.md), [Testing and the End-to-End Harness](LTM/testing-and-e2e-harness.md), and [Measurement and Investigation Methodology](LTM/measurement-and-investigation-methodology.md) |
| `2026-09-14 -- deep-gate validation of the checksum cache, and a third false label` | [Quality Gates and Project Tooling](LTM/quality-gates-and-project-tooling.md), [Storage Format, Index, and Zero-Copy Reads](LTM/storage-format-index-and-zero-copy.md), [Testing and the End-to-End Harness](LTM/testing-and-e2e-harness.md), [Measurement and Investigation Methodology](LTM/measurement-and-investigation-methodology.md), and [TODO](TODO.md) |
| `2026-09-14 -- WriteBatch::commit was 22x slower on document-major input` | [Database APIs and Satellite Crates](LTM/database-apis-and-satellite-crates.md), [WAL, MVCC, Durability, and Concurrency](LTM/wal-mvcc-durability-and-concurrency.md), [Testing and the End-to-End Harness](LTM/testing-and-e2e-harness.md), and [Measurement and Investigation Methodology](LTM/measurement-and-investigation-methodology.md) |
| `2026-09-14 -- concurrent read scaling is bounded by shard count, not cores` | [WAL, MVCC, Durability, and Concurrency](LTM/wal-mvcc-durability-and-concurrency.md), [Chunk Stream Contracts and Lazy Operators](LTM/chunk-stream-contracts-and-lazy-operators.md), [Measurement and Investigation Methodology](LTM/measurement-and-investigation-methodology.md), and [TODO](TODO.md) |
| `2026-09-14 -- the checkpoint no longer holds the store lock across its fsyncs` | [WAL, MVCC, Durability, and Concurrency](LTM/wal-mvcc-durability-and-concurrency.md), [Testing and the End-to-End Harness](LTM/testing-and-e2e-harness.md), [Measurement and Investigation Methodology](LTM/measurement-and-investigation-methodology.md), and [TODO](TODO.md) |
| `2026-09-14 -- gate-pg run against the committed tree, and it was overdue` | [Quality Gates and Project Tooling](LTM/quality-gates-and-project-tooling.md), [Checkpoint and Commit Lock Contention](LTM/checkpoint-and-commit-lock-contention.md), [Measurement and Investigation Methodology](LTM/measurement-and-investigation-methodology.md) |
| `2026-09-15 -- distill-2026-09-13-synthesis-findings closed` | Already promoted into `ARCHITECTURE.md`, `OVERVIEW.md` and `QUALITY_GATE.md`; recorded in [Quality Gates and Project Tooling](LTM/quality-gates-and-project-tooling.md) and [TODO](TODO.md) |
| `2026-09-15 -- CPU contention ruled out by a counter, not a probe` through `2026-09-15 -- it is a lock after all, and shard-invariance never said otherwise` | [Checkpoint and Commit Lock Contention](LTM/checkpoint-and-commit-lock-contention.md), [Measurement and Investigation Methodology](LTM/measurement-and-investigation-methodology.md), [Allocation, Reclamation, and Fsck](LTM/allocation-reclamation-and-fsck.md), [Storage Format, Index, and Zero-Copy Reads](LTM/storage-format-index-and-zero-copy.md) |
| `2026-09-15 -- slab-free-leaves-a-stale-bump-pointer: two size classes allocating into one slab` through `2026-09-15 -- the fix had two call sites and one guarded test, and the suite could not tell` | [Allocation, Reclamation, and Fsck](LTM/allocation-reclamation-and-fsck.md), [Testing and the End-to-End Harness](LTM/testing-and-e2e-harness.md), [Measurement and Investigation Methodology](LTM/measurement-and-investigation-methodology.md) |
| `2026-09-15 -- an audit for sabotage residue, run with a broken pathspec` and `2026-09-15 -- the audit's baseline is itself unverified` | [Measurement and Investigation Methodology](LTM/measurement-and-investigation-methodology.md), [Quality Gates and Project Tooling](LTM/quality-gates-and-project-tooling.md) |
| `2026-09-15 -- packed-page-sharing-may-contend-across-keys: measured, and refuted as a runtime cost` and `2026-09-15 -- bounding the verified-region cache` | [Allocation, Reclamation, and Fsck](LTM/allocation-reclamation-and-fsck.md), [Storage Format, Index, and Zero-Copy Reads](LTM/storage-format-index-and-zero-copy.md), [Measurement and Investigation Methodology](LTM/measurement-and-investigation-methodology.md) |
| `2026-09-15 -- the checkpoint's exclusive hold is input materialization, not tree building` through `2026-09-16 -- reusing untouched leaves without decoding them` | [Checkpoint and Commit Lock Contention](LTM/checkpoint-and-commit-lock-contention.md), [Storage Format, Index, and Zero-Copy Reads](LTM/storage-format-index-and-zero-copy.md), [Measurement and Investigation Methodology](LTM/measurement-and-investigation-methodology.md) |
| `2026-09-16 -- commits own the tail; the checkpoint work is aimed at the worst case` through `2026-09-16 -- the consumer's write amplification is fixed, and the near-miss is the finding` | [Checkpoint and Commit Lock Contention](LTM/checkpoint-and-commit-lock-contention.md), [WAL, MVCC, Durability, and Concurrency](LTM/wal-mvcc-durability-and-concurrency.md), [Measurement and Investigation Methodology](LTM/measurement-and-investigation-methodology.md) |
| `2026-09-16 -- the same sweep, run on this tree's own recent edits, found a live one here too`, `2026-09-16 -- reading the code rather than the entries`, `2026-09-16 -- the orphaned-comment entry prescribed the wrong remedy`, `2026-09-17 -- five orphaned doc comments, four different repairs` | [Quality Gates and Project Tooling](LTM/quality-gates-and-project-tooling.md), [Measurement and Investigation Methodology](LTM/measurement-and-investigation-methodology.md), [TODO](TODO.md) |
| `2026-09-16 -- the memtable write lock is held across disk reads`, `2026-09-16 -- hoisting the commit path's disk reads out of the memtable lock`, `2026-09-16 -- the delete path had the same defect, and my probe misreported it twice` | [WAL, MVCC, Durability, and Concurrency](LTM/wal-mvcc-durability-and-concurrency.md), [Checkpoint and Commit Lock Contention](LTM/checkpoint-and-commit-lock-contention.md), [Measurement and Investigation Methodology](LTM/measurement-and-investigation-methodology.md) |
| `2026-09-16 -- mechanize the arithmetic, read the judgement` through `2026-09-16 -- the scope of an audit is part of its result` ( the seven audit-method entries ) | [Measurement and Investigation Methodology](LTM/measurement-and-investigation-methodology.md), [Quality Gates and Project Tooling](LTM/quality-gates-and-project-tooling.md), [Testing and the End-to-End Harness](LTM/testing-and-e2e-harness.md) |
| `2026-09-17 -- the slab-reuse fixture, and a calibration that did not transfer` | [Testing and the End-to-End Harness](LTM/testing-and-e2e-harness.md), [Allocation, Reclamation, and Fsck](LTM/allocation-reclamation-and-fsck.md) |
| `2026-09-17 -- revisiting per-shard commit parallelism` and `2026-09-17 -- the apply half, and a contaminated measurement that nearly buried it` | [Checkpoint and Commit Lock Contention](LTM/checkpoint-and-commit-lock-contention.md), [Measurement and Investigation Methodology](LTM/measurement-and-investigation-methodology.md) |
| `2026-09-17 -- bring your own executor: the dispatch escape hatch`, `2026-09-17 -- a batch write for the C ABI`, `2026-09-17 -- the dispatcher reaches C, and a sabotage that failed for the wrong reason` | [Database APIs and Satellite Crates](LTM/database-apis-and-satellite-crates.md), [Testing and the End-to-End Harness](LTM/testing-and-e2e-harness.md), [Quality Gates and Project Tooling](LTM/quality-gates-and-project-tooling.md), [PostgreSQL Extension and Query Pushdown](LTM/postgresql-extension-and-query-pushdown.md) |
| `2026-09-17 -- the doc-warning baseline reaches zero, and CI had drifted again`, `2026-09-17 -- unwired sweep re-run`, `2026-09-17 -- a 105-line integration test that has never run` | [Quality Gates and Project Tooling](LTM/quality-gates-and-project-tooling.md), [Measurement and Investigation Methodology](LTM/measurement-and-investigation-methodology.md), [TODO](TODO.md) |
| `2026-09-17 -- the x86_64 SSSE3 array arm, landed with no speedup figure` through `2026-09-18 -- three ways not to speed up ops::bitmap's NEON kernel` | [SIMD Arch Arms and Kernel Selection](LTM/simd-arch-arms-and-kernel-selection.md), [Set Algebra Kernels and Cardinality](LTM/set-algebra-kernels-and-cardinality.md), [Measurement and Investigation Methodology](LTM/measurement-and-investigation-methodology.md), [Storage Format, Index, and Zero-Copy Reads](LTM/storage-format-index-and-zero-copy.md), [Quality Gates and Project Tooling](LTM/quality-gates-and-project-tooling.md) |
| `2026-09-19 — a consumer's `view_count` declined, because the premise that made it immune to measurement is false` | [packed-lenses-matrix-bignum-and-views.md](LTM/packed-lenses-matrix-bignum-and-views.md) |
| `2026-09-19 — `sets` was the third amplification vector, and the fix belongs in two places for two different reasons` | [packed-lenses-matrix-bignum-and-views.md](LTM/packed-lenses-matrix-bignum-and-views.md) |
| `2026-09-19 — the set-expression language becomes multi-sorted, and the three view leaves become compositions` | [packed-lenses-matrix-bignum-and-views.md](LTM/packed-lenses-matrix-bignum-and-views.md) |
| `2026-09-19 — `map`, the hole, and the sorts that made a facet query expressible` | [packed-lenses-matrix-bignum-and-views.md](LTM/packed-lenses-matrix-bignum-and-views.md) |
| `2026-09-20 — Test plan: fail-closed and prefix-bounded key streams` | [chunk-stream-contracts-and-lazy-operators.md](LTM/chunk-stream-contracts-and-lazy-operators.md) |
| `2026-09-20 — expression-level view maps pay for extraction before they pay for their terminal` | [packed-lenses-matrix-bignum-and-views.md](LTM/packed-lenses-matrix-bignum-and-views.md) |
| `2026-09-20 — stage 2 fuses view-map terminals before materialization` | [packed-lenses-matrix-bignum-and-views.md](LTM/packed-lenses-matrix-bignum-and-views.md) |
| `2026-09-20 — fail-closed and prefix-bounded key streams` | [chunk-stream-contracts-and-lazy-operators.md](LTM/chunk-stream-contracts-and-lazy-operators.md) |
| `2026-09-20 — Test plan: pointwise Boolean view maps` | [packed-lenses-matrix-bignum-and-views.md](LTM/packed-lenses-matrix-bignum-and-views.md) |
| `2026-09-20 — stage 3 fuses pointwise Boolean cardinality and rank maps` | [packed-lenses-matrix-bignum-and-views.md](LTM/packed-lenses-matrix-bignum-and-views.md) |
| `2026-09-20 — Test plan: general pointwise mapped folds` | [packed-lenses-matrix-bignum-and-views.md](LTM/packed-lenses-matrix-bignum-and-views.md) |
| `2026-09-20 — stage 4 fuses general pointwise mapped folds` | [packed-lenses-matrix-bignum-and-views.md](LTM/packed-lenses-matrix-bignum-and-views.md) |
| `2026-09-21 — Test plan: mapped-select folds` | [packed-lenses-matrix-bignum-and-views.md](LTM/packed-lenses-matrix-bignum-and-views.md) |
| `2026-09-21 — Test plan: composed cardinality-map normalization` | [packed-lenses-matrix-bignum-and-views.md](LTM/packed-lenses-matrix-bignum-and-views.md) |
| `2026-09-21 — stages 5 and 6 close mapped selection and composed cardinality gaps` | [packed-lenses-matrix-bignum-and-views.md](LTM/packed-lenses-matrix-bignum-and-views.md) |
| `2026-09-21 — SIMD exploration found a packed-view arm, not a general word-loop arm` | [simd-arch-arms-and-kernel-selection.md](LTM/simd-arch-arms-and-kernel-selection.md) |
| `2026-09-21 — native intersection counts dominate the next expression opportunity` | [packed-lenses-matrix-bignum-and-views.md](LTM/packed-lenses-matrix-bignum-and-views.md) |
| `2026-09-21 — Test plan: scalar blocked bitmap intersection counts` | [packed-lenses-matrix-bignum-and-views.md](LTM/packed-lenses-matrix-bignum-and-views.md) |
| `2026-09-21 — Stage 8a lands scalar blocked bitmap intersection counts` | [packed-lenses-matrix-bignum-and-views.md](LTM/packed-lenses-matrix-bignum-and-views.md) |
| `2026-09-21 — Test plan: complete native intersection-count traversal` | [packed-lenses-matrix-bignum-and-views.md](LTM/packed-lenses-matrix-bignum-and-views.md) |
| `2026-09-21 — Stage 8 completes native view intersection counts` | [packed-lenses-matrix-bignum-and-views.md](LTM/packed-lenses-matrix-bignum-and-views.md) |
| `2026-09-21 — Quality Gate: Stage 8 native view intersection counts` | [packed-lenses-matrix-bignum-and-views.md](LTM/packed-lenses-matrix-bignum-and-views.md) |
| `2026-09-21 — Blocked bitmap batches cross the SIMD boundary once` | [simd-arch-arms-and-kernel-selection.md](LTM/simd-arch-arms-and-kernel-selection.md) |
| `2026-09-21 — JIT fusion measured against the production expression path` | [simd-arch-arms-and-kernel-selection.md](LTM/simd-arch-arms-and-kernel-selection.md) |
| `2026-09-21 — Cranelift gap traced to non-canonical pairwise IR` | [simd-arch-arms-and-kernel-selection.md](LTM/simd-arch-arms-and-kernel-selection.md) |
| `2026-09-21 — Cranelift reaches AArch64 steady-state parity` | [simd-arch-arms-and-kernel-selection.md](LTM/simd-arch-arms-and-kernel-selection.md) |
| `2026-09-21 — Fused bitmap-DAG JIT reaches the Flight expression path` | [simd-arch-arms-and-kernel-selection.md](LTM/simd-arch-arms-and-kernel-selection.md) |
| `2026-09-21 — Rust 1.95 becomes the shared minimum` | [simd-arch-arms-and-kernel-selection.md](LTM/simd-arch-arms-and-kernel-selection.md) |
| `2026-09-21 — Rust 1.95 gate results` | [simd-arch-arms-and-kernel-selection.md](LTM/simd-arch-arms-and-kernel-selection.md) |
| `2026-09-21 — The fused bitmap-DAG JIT moves behind a core feature` | [simd-arch-arms-and-kernel-selection.md](LTM/simd-arch-arms-and-kernel-selection.md) |
| `2026-09-21 — Flight and Bazel JIT are opt-in` | [simd-arch-arms-and-kernel-selection.md](LTM/simd-arch-arms-and-kernel-selection.md) |
| `2026-09-22 — Quality Gate: remove the temporary `yesno-jit` crate` | [simd-arch-arms-and-kernel-selection.md](LTM/simd-arch-arms-and-kernel-selection.md) |
| `2026-09-22 — Nested fused-DAG AOT/JIT kernel comparison` | [simd-arch-arms-and-kernel-selection.md](LTM/simd-arch-arms-and-kernel-selection.md) |
| `2026-09-22 — The fused bitmap-DAG JIT is ported to x86_64, code generation only` | [simd-arch-arms-and-kernel-selection.md](LTM/simd-arch-arms-and-kernel-selection.md) |
| `2026-09-22: Test plan for final-prefix view count and JIT module lifetime` | [simd-arch-arms-and-kernel-selection.md](LTM/simd-arch-arms-and-kernel-selection.md) |
| `2026-09-22 — The x86 JIT measurement, and the gate it retired` | [simd-arch-arms-and-kernel-selection.md](LTM/simd-arch-arms-and-kernel-selection.md) |
| `2026-09-22: Final-prefix count and JIT executable-memory lifetime fixed` | [simd-arch-arms-and-kernel-selection.md](LTM/simd-arch-arms-and-kernel-selection.md) |
| `2026-09-22: Test plan for automatic JIT admission` | [simd-arch-arms-and-kernel-selection.md](LTM/simd-arch-arms-and-kernel-selection.md) |
| `2026-09-22: Automatic JIT admission excludes union-draining losses` | [simd-arch-arms-and-kernel-selection.md](LTM/simd-arch-arms-and-kernel-selection.md) |
| `2026-09-22: Admission regression test-layer follow-up` | [simd-arch-arms-and-kernel-selection.md](LTM/simd-arch-arms-and-kernel-selection.md) |
| `2026-09-22: Test plan for simple binary automatic-JIT admission` | [simd-arch-arms-and-kernel-selection.md](LTM/simd-arch-arms-and-kernel-selection.md) |
| `2026-09-22: Binary automatic-JIT admission follows measured shape evidence` | [simd-arch-arms-and-kernel-selection.md](LTM/simd-arch-arms-and-kernel-selection.md) |
| `2026-09-22: Test plan for seek-driven explicit fused-DAG traversal` | [simd-arch-arms-and-kernel-selection.md](LTM/simd-arch-arms-and-kernel-selection.md) |
| `2026-09-22 — x86_64 automatic admission is enabled` | [simd-arch-arms-and-kernel-selection.md](LTM/simd-arch-arms-and-kernel-selection.md) |
| `2026-09-22: Explicit JIT seeks candidate prefixes` | [simd-arch-arms-and-kernel-selection.md](LTM/simd-arch-arms-and-kernel-selection.md) |
| `2026-09-22 — Codex handoff: native x86_64 validation, and the explicit JIT's selective losses` | [simd-arch-arms-and-kernel-selection.md](LTM/simd-arch-arms-and-kernel-selection.md) |
| `2026-09-22 — Test plan: bitmap-native interleaved view terminals` | [packed-lenses-matrix-bignum-and-views.md](LTM/packed-lenses-matrix-bignum-and-views.md) |
| `2026-09-22 — Stage 7a: bitmap-native interleaved cardinalities without SIMD` | [packed-lenses-matrix-bignum-and-views.md](LTM/packed-lenses-matrix-bignum-and-views.md) |
| `2026-09-22 Stage 7b test plan: bitmap-native interleaved folds` | [packed-lenses-matrix-bignum-and-views.md](LTM/packed-lenses-matrix-bignum-and-views.md) |
| `2026-09-22 Stage 7b: bitmap-native interleaved folds` | [packed-lenses-matrix-bignum-and-views.md](LTM/packed-lenses-matrix-bignum-and-views.md) |
| `2026-09-22 — Test plan: SIMD interleaved view terminals` | [packed-lenses-matrix-bignum-and-views.md](LTM/packed-lenses-matrix-bignum-and-views.md) |
| `2026-09-22 Stage 7c: NEON interleaved bitmap count and fold terminals` | [packed-lenses-matrix-bignum-and-views.md](LTM/packed-lenses-matrix-bignum-and-views.md) |
| `2026-09-23 Stage 7d: x86_64 interleaved bitmap count and fold terminals` | [packed-lenses-matrix-bignum-and-views.md](LTM/packed-lenses-matrix-bignum-and-views.md) |
| `2026-09-23 Stage 7e: persisted map and fold SIMD measurement` | [packed-lenses-matrix-bignum-and-views.md](LTM/packed-lenses-matrix-bignum-and-views.md) |
| `2026-09-23 Stage 7f: sparse persisted terminals expose the eager boundary` | [packed-lenses-matrix-bignum-and-views.md](LTM/packed-lenses-matrix-bignum-and-views.md) |
| `2026-09-23 — Quality Gate: Stage 7d-7f view terminals` | [packed-lenses-matrix-bignum-and-views.md](LTM/packed-lenses-matrix-bignum-and-views.md) |
| `2026-09-23 — Test plan: extremely sparse persisted view terminals` | [packed-lenses-matrix-bignum-and-views.md](LTM/packed-lenses-matrix-bignum-and-views.md) |
| `2026-09-23 GPU offload for long dense sets: measured, and the answer is conditional` | [gpu-offload-on-unified-memory.md](LTM/gpu-offload-on-unified-memory.md) |
| `2026-09-23 The Intel Mac's two GPUs, and what the contrast establishes` | [gpu-offload-on-unified-memory.md](LTM/gpu-offload-on-unified-memory.md) |
| `2026-09-23 — Stage 7f: extremely sparse persisted view terminals` | [packed-lenses-matrix-bignum-and-views.md](LTM/packed-lenses-matrix-bignum-and-views.md) |
| `2026-09-23 — Stage 7g measurement: bounded direct rank maps` | [packed-lenses-matrix-bignum-and-views.md](LTM/packed-lenses-matrix-bignum-and-views.md) |
| `2026-09-23 — Test plan: streamed direct view ranks` | [packed-lenses-matrix-bignum-and-views.md](LTM/packed-lenses-matrix-bignum-and-views.md) |
| `2026-09-23 -- The hotspot observer, and what a counter already settled` | [query-hotspot-observation.md](LTM/query-hotspot-observation.md) |
| `2026-09-23 — Stage 7g: bounded direct identity ranks` | [packed-lenses-matrix-bignum-and-views.md](LTM/packed-lenses-matrix-bignum-and-views.md) |
| `2026-09-23 -- The capture point, and the identity bug that would have faked an answer` | [query-hotspot-observation.md](LTM/query-hotspot-observation.md) |
| `2026-09-23 -- Ungating the capture point, and what the idle path actually costs` | [query-hotspot-observation.md](LTM/query-hotspot-observation.md) |
| `2026-09-23 -- Publishing through tracing, and bounding what a capture commits to` | [query-hotspot-observation.md](LTM/query-hotspot-observation.md) |
| `2026-09-23 -- Moving the capture point into yesno-core` | [query-hotspot-observation.md](LTM/query-hotspot-observation.md) |
| `2026-09-23 -- `hotspot` must not be gated on a dependency's feature` | [query-hotspot-observation.md](LTM/query-hotspot-observation.md) |
| `2026-09-23 -- Flight write transactions, and four things I got wrong on the way` | [flight-write-transactions.md](LTM/flight-write-transactions.md) |
| `2026-09-23 -- Starting the GPU offload satellite, on branch `gpu-offload`` | [gpu-offload-on-unified-memory.md](LTM/gpu-offload-on-unified-memory.md) |
| `2026-09-23 -- Wiring the accelerator into `count_blocked`` | [gpu-offload-on-unified-memory.md](LTM/gpu-offload-on-unified-memory.md) |
| `2026-09-23 -- CUDA versus OpenCL on GB10: no difference, and what that retires` | [gpu-offload-on-unified-memory.md](LTM/gpu-offload-on-unified-memory.md) |
| `2026-09-23 -- An OpenCL device backend for `yesno-gpu`` | [gpu-offload-on-unified-memory.md](LTM/gpu-offload-on-unified-memory.md) |
| `2026-09-23 -- A gate piped into `tail` reports its own success` | [quality-gates-and-project-tooling.md](LTM/quality-gates-and-project-tooling.md) |
| `2026-09-23 -- The offload path loses end to end, and the reason is structural` | [gpu-offload-on-unified-memory.md](LTM/gpu-offload-on-unified-memory.md) |
| `2026-09-23 -- Reconciling 6.71x with 0.87x: they measured different things` | [gpu-offload-on-unified-memory.md](LTM/gpu-offload-on-unified-memory.md) |
| `2026-09-23 -- The u32-to-u64 accumulate is already vectorized` | [gpu-offload-on-unified-memory.md](LTM/gpu-offload-on-unified-memory.md) |
| `2026-09-23 -- The deferred hook makes offload win, and a 4 MiB allocation nearly hid it` | [gpu-offload-on-unified-memory.md](LTM/gpu-offload-on-unified-memory.md) |
| `2026-09-23 -- `yesno-gpu` renamed to `yesno-opencl`` | [gpu-offload-on-unified-memory.md](LTM/gpu-offload-on-unified-memory.md) |
| `2026-09-23 -- An external audit found three holes in the write transaction, and the server never ran it` | [flight-write-transactions.md](LTM/flight-write-transactions.md) |
| `2026-09-24 -- `patch_chunk`, and the trap it was built around` | [chunk-local-patch-writes.md](LTM/chunk-local-patch-writes.md) |
| `2026-09-24 -- The patch operation cost 39% on the path it does not touch` | [chunk-local-patch-writes.md](LTM/chunk-local-patch-writes.md) |
| `2026-09-24 -- `vec_int_batch` fuses one key and N filters, not N siblings` | [packed-lenses-matrix-bignum-and-views.md](LTM/packed-lenses-matrix-bignum-and-views.md) |
| `2026-09-24 -- A consumer's space probe found a comment describing a mechanism we never built` | [allocation-reclamation-and-fsck.md](LTM/allocation-reclamation-and-fsck.md) |
| `2026-09-25 -- Stage A of arbitrary-precision integers: the sort exists, the transport does not` | [packed-lenses-matrix-bignum-and-views.md](LTM/packed-lenses-matrix-bignum-and-views.md) |
| `2026-09-25 -- One set is one integer: `IntLayout` and `IntSink` removed` | [packed-lenses-matrix-bignum-and-views.md](LTM/packed-lenses-matrix-bignum-and-views.md) |
| `2026-09-25 -- Signed arithmetic, saturation, and stage B of the expression language` | [packed-lenses-matrix-bignum-and-views.md](LTM/packed-lenses-matrix-bignum-and-views.md) |
| `2026-09-25 -- The multi-sorted language gets a transport, and it had never had one` | [packed-lenses-matrix-bignum-and-views.md](LTM/packed-lenses-matrix-bignum-and-views.md) |
| `2026-09-25 -- Client parity, a vector sort, folding, and four checks that live outside the loop` | [packed-lenses-matrix-bignum-and-views.md](LTM/packed-lenses-matrix-bignum-and-views.md) |
| `2026-09-25 -- Stage C: modular exponentiation, and the first bound that is not about size` | [packed-lenses-matrix-bignum-and-views.md](LTM/packed-lenses-matrix-bignum-and-views.md) |
| `2026-09-26 -- Fused big-integer arithmetic was the wrong lever, and two stale rationales` | [packed-lenses-matrix-bignum-and-views.md](LTM/packed-lenses-matrix-bignum-and-views.md) |
| `2026-09-26 -- The narrow-width kernels, finished properly` | [packed-lenses-matrix-bignum-and-views.md](LTM/packed-lenses-matrix-bignum-and-views.md) |
| `2026-09-26 -- PowMod was already in the clients, and I said twice that it was not` | [packed-lenses-matrix-bignum-and-views.md](LTM/packed-lenses-matrix-bignum-and-views.md) |
| `2026-09-26 -- A SIMD kernel for zip/scale, and the 875x it led to instead` | [packed-lenses-matrix-bignum-and-views.md](LTM/packed-lenses-matrix-bignum-and-views.md) |
| `2026-09-26 -- SIMD on the array scatter: 1.05x, and 4x from the setup` | [packed-lenses-matrix-bignum-and-views.md](LTM/packed-lenses-matrix-bignum-and-views.md) |
| `2026-09-27 -- view_select: the specialisation was fine, the premise was mine` | [packed-lenses-matrix-bignum-and-views.md](LTM/packed-lenses-matrix-bignum-and-views.md) |
| `2026-09-27 -- The wide interleaved fold, and a caution of mine that was wrong` | [packed-lenses-matrix-bignum-and-views.md](LTM/packed-lenses-matrix-bignum-and-views.md) |
| `2026-09-27 -- sets of 16 and 32: the third structure, and the simplest` | [packed-lenses-matrix-bignum-and-views.md](LTM/packed-lenses-matrix-bignum-and-views.md) |
| `2026-09-27 -- The top of the arity range, where a bound stood in for a condition` | [packed-lenses-matrix-bignum-and-views.md](LTM/packed-lenses-matrix-bignum-and-views.md) |
| `2026-09-27 -- Reclassifying unbounded growth under delete churn as a defect` | [allocation-reclamation-and-fsck.md](LTM/allocation-reclamation-and-fsck.md) |
| `2026-09-27 -- Hole punching, and the plateau that bounded the severity` | [allocation-reclamation-and-fsck.md](LTM/allocation-reclamation-and-fsck.md) |
| `2026-09-27 -- COMPACT_LIVE_FRACTION is not the blocker, and the value is unchanged` | [allocation-reclamation-and-fsck.md](LTM/allocation-reclamation-and-fsck.md) |
| `2026-09-27 -- Asked to do a format change that had shipped two weeks earlier` | [allocation-reclamation-and-fsck.md](LTM/allocation-reclamation-and-fsck.md) |
| `2026-09-28 -- Triaging the dangling citations: 22 became 2` | [quality-gates-and-project-tooling.md](LTM/quality-gates-and-project-tooling.md) |
| `2026-09-28 -- Why evacuation never fires: the trigger, not the threshold` | [allocation-reclamation-and-fsck.md](LTM/allocation-reclamation-and-fsck.md) |
| `2026-09-28 -- The shard mutex: narrowed, and about 40% of the contention was it` | [chunk-stream-contracts-and-lazy-operators.md](LTM/chunk-stream-contracts-and-lazy-operators.md) |
| `2026-09-28 -- The 40% was my fixture's, not the change's: two frames, one lock` | [chunk-stream-contracts-and-lazy-operators.md](LTM/chunk-stream-contracts-and-lazy-operators.md) |
| `2026-09-28 -- Assessing the hosted-plugin ABI: the engine is ready, the boundary is not` | [hosted-plugin-abi-assessment.md](LTM/hosted-plugin-abi-assessment.md) |
| `2026-09-28 -- Sent the ABI assessment to the wrong haiiie session, for the same reason as the 40%` | [hosted-plugin-abi-assessment.md](LTM/hosted-plugin-abi-assessment.md) |
| `2026-09-28 -- Evacuation was unreachable, and the verdict against it was missing half the trade` | [allocation-reclamation-and-fsck.md](LTM/allocation-reclamation-and-fsck.md) |
| `2026-09-28 -- Designing the plugin facility, and retracting my own lease recommendation` | [hosted-plugin-abi-design.md](LTM/hosted-plugin-abi-design.md) |
| `2026-09-28 -- I contradicted a sentence I had already quoted: the lease pins the flock` | [hosted-plugin-abi-design.md](LTM/hosted-plugin-abi-design.md) |
| `2026-09-28 -- The escalation I invented made things worse in both directions` | [hosted-plugin-abi-design.md](LTM/hosted-plugin-abi-design.md) |
| `2026-09-28 -- The lane lifetime I picked would have retained the whole scan` | [hosted-plugin-abi-design.md](LTM/hosted-plugin-abi-design.md) |
| `2026-09-28 -- Shared-snapshot concurrency, and a test that could not have failed` | [hosted-plugin-abi-design.md](LTM/hosted-plugin-abi-design.md) |
| `2026-09-28 -- Plugin facility, first increment: the core primitive and the C table` | [hosted-plugin-abi-design.md](LTM/hosted-plugin-abi-design.md) |
| `2026-09-28 -- Exploring dynamic loading: two panic defects, and a linking rule that guards punching` | [hosted-plugin-abi-design.md](LTM/hosted-plugin-abi-design.md) |
| `2026-09-28 -- The loader, a C fixture plugin, and the server wiring` | [hosted-plugin-abi-design.md](LTM/hosted-plugin-abi-design.md) |
| `2026-09-28 -- Wiring the leader, and two bugs a weaker test would have shipped` | [hosted-plugin-abi-design.md](LTM/hosted-plugin-abi-design.md) |
| `2026-09-28 -- The out-of-process plugin is mostly already built, and SHM is the wrong tool` | [out-of-process-plugin-via-foreign-reader.md](LTM/out-of-process-plugin-via-foreign-reader.md) |
| `2026-09-28 -- Conceiving the out-of-process payload, after a wrong turn worth naming` | [plugin-channel-protocol-and-security.md](LTM/plugin-channel-protocol-and-security.md) |
| `2026-09-28 -- The IPC channel server, and what it costs against the in-process table` | [plugin-channel-protocol-and-security.md](LTM/plugin-channel-protocol-and-security.md) |
| `2026-09-28 -- Batching answers the primitive question, and two harness bugs answered it wrongly first` | [plugin-channel-protocol-and-security.md](LTM/plugin-channel-protocol-and-security.md) |
| `2026-09-28 -- Sealing the arena, and what portability actually requires` | [plugin-channel-protocol-and-security.md](LTM/plugin-channel-protocol-and-security.md) |
| `2026-09-28 -- The plugin protocol was in the wrong crate, on a rationale that did not apply` | [plugin-channel-protocol-and-security.md](LTM/plugin-channel-protocol-and-security.md) |
| `2026-09-28 -- Wiring the channel into yesnod` | [plugin-channel-protocol-and-security.md](LTM/plugin-channel-protocol-and-security.md) |
| `2026-09-28 -- A policy number written into a structural cap, found by a consumer` | [plugin-channel-protocol-and-security.md](LTM/plugin-channel-protocol-and-security.md) |
| `2026-09-28 -- The five SetSnapshot reads, and where paging is honest versus quadratic` | [plugin-channel-protocol-and-security.md](LTM/plugin-channel-protocol-and-security.md) |
| `2026-09-28 -- A protocol property is not a server property` | [plugin-channel-protocol-and-security.md](LTM/plugin-channel-protocol-and-security.md) |
| `2026-09-28 -- Inline advertised a width it could not encode, and my test could not see it` | [plugin-channel-protocol-and-security.md](LTM/plugin-channel-protocol-and-security.md) |
| `2026-09-28 -- Closing the call-site gap: everything worked and nothing called it` | [plugin-channel-protocol-and-security.md](LTM/plugin-channel-protocol-and-security.md) |
| `2026-09-28 -- The first daemon smoke test, and the bug it found immediately` | [plugin-channel-protocol-and-security.md](LTM/plugin-channel-protocol-and-security.md) |
| `2026-09-29 -- a run reached both plugin transports in its stored form, not its documented one` | [plugin-channel-protocol-and-security.md](LTM/plugin-channel-protocol-and-security.md) |
| `2026-09-29 -- the lean-core dependency guard was red for 25 commits, because only CI could see it` | [quality-gates-and-project-tooling.md](LTM/quality-gates-and-project-tooling.md) |
| `2026-09-29 -- a WAL rotation race, and why it is probably not the archive ENOENT it was found chasing` | [wal-mvcc-durability-and-concurrency.md](LTM/wal-mvcc-durability-and-concurrency.md) |
| `2026-09-29 -- foreign-reader liveness now asks the kernel, not the PID namespace` | [out-of-process-plugin-via-foreign-reader.md](LTM/out-of-process-plugin-via-foreign-reader.md) |
| `2026-09-29 -- slab reuse really did overwrite a held container, and the cost of stopping it was mis-estimated` | [allocation-reclamation-and-fsck.md](LTM/allocation-reclamation-and-fsck.md) |
| `2026-09-29 -- the gate/CI comparison is automated, and it found the two files already agree` | [quality-gates-and-project-tooling.md](LTM/quality-gates-and-project-tooling.md) |
| `2026-09-29 -- the plugin channel is configurable without a config file` | [operator-hosted-plugin-container-plan.md](LTM/operator-hosted-plugin-container-plan.md) |
| `2026-09-29 -- the in-process cdylib plugin ABI is gone` | [removed-cdylib-plugin-abi.md](LTM/removed-cdylib-plugin-abi.md) |
| `2026-09-29 -- a security review, and the one claim in the cdylib removal that was wrong` | [plugin-channel-protocol-and-security.md](LTM/plugin-channel-protocol-and-security.md) |
| `2026-09-29 -- the rest of the security review, and a deadlock I wrote while fixing it` | [plugin-channel-protocol-and-security.md](LTM/plugin-channel-protocol-and-security.md) |
| `2026-09-29 -- the re-review, and a test of mine that could not fail` | [plugin-channel-protocol-and-security.md](LTM/plugin-channel-protocol-and-security.md) |
| `2026-09-29 -- correction: the channel protocol *is* exercised over a real socket` | [plugin-channel-protocol-and-security.md](LTM/plugin-channel-protocol-and-security.md) |
| `2026-09-29 -- a peer in its own process, which is the only way the liveness claim can be tested` | [plugin-channel-protocol-and-security.md](LTM/plugin-channel-protocol-and-security.md) |
| `2026-09-30 -- the operator can run a plugin peer beside yesnod` | [operator-hosted-plugin-container-plan.md](LTM/operator-hosted-plugin-container-plan.md) |
| `2026-09-30 -- the operator guide is a directory now` | [quality-gates-and-project-tooling.md](LTM/quality-gates-and-project-tooling.md) |
| `2026-09-30 -- Phase 2: the peer's readiness is not the daemon's` | [operator-hosted-plugin-container-plan.md](LTM/operator-hosted-plugin-container-plan.md) |
| `2026-09-30 -- Phase 3: the arena is charged to both containers, not one` | [arena-cgroup-charging.md](LTM/arena-cgroup-charging.md) |
| `2026-09-30 -- Phase 4: the example peer was a worked example of the mistake it warns about` | [operator-hosted-plugin-container-plan.md](LTM/operator-hosted-plugin-container-plan.md) |
| `2026-09-30 -- the-operator-gate-was-red-for-a-week-and-nobody-ran-it` | [quality-gates-and-project-tooling.md](LTM/quality-gates-and-project-tooling.md) |
| `2026-09-30 -- a snapshot is a point in time, and a sidecar starts before the data` | [operator-hosted-plugin-container-plan.md](LTM/operator-hosted-plugin-container-plan.md) |
| `2026-09-30 -- the e2e image recompiled MySQL because one layer held the whole tree` | [quality-gates-and-project-tooling.md](LTM/quality-gates-and-project-tooling.md) |
| `2026-09-30 -- the e2e image cache, measured: 1753 s of artifact builds became 526 s` | [quality-gates-and-project-tooling.md](LTM/quality-gates-and-project-tooling.md) |
| `2026-09-30 -- one encoder, two destinations: integrating the haiiie direct-arena patch` | [plugin-shape-performance.md](LTM/plugin-shape-performance.md) |
| `2026-09-30 -- compact WAL chunk images: the record type to do it with already existed` | [wal-mvcc-durability-and-concurrency.md](LTM/wal-mvcc-durability-and-concurrency.md) |
| `2026-09-30 -- three set representations on the Flight wire, and where zero-copy stops` | [database-apis-and-satellite-crates.md](LTM/database-apis-and-satellite-crates.md) |
| `Zero-copy: asked for, and the honest boundary` | [database-apis-and-satellite-crates.md](LTM/database-apis-and-satellite-crates.md) |
| `What is not done` | [database-apis-and-satellite-crates.md](LTM/database-apis-and-satellite-crates.md) |
| `2026-09-30 -- dense spans, Phase 1: the read path, and four latent assumptions` | [contiguous-dense-span-extents-plan.md](LTM/contiguous-dense-span-extents-plan.md) |
| `2026-09-30 -- dense contiguity, landed: the fix was a smaller slot, not a bigger page` | [contiguous-dense-span-extents-plan.md](LTM/contiguous-dense-span-extents-plan.md) |
| `2026-10-01 -- the bitvector wire: Binary not Boolean, and lent not gathered` | [contiguous-dense-span-extents-plan.md](LTM/contiguous-dense-span-extents-plan.md) |
| `2026-10-01 -- the test scratch leak, which filled the disk and failed a gate` | [quality-gates-and-project-tooling.md](LTM/quality-gates-and-project-tooling.md) |
| `2026-10-02 -- three bugs in one borrow path, and only one of them found by me` | [database-apis-and-satellite-crates.md](LTM/database-apis-and-satellite-crates.md) |
| `2026-10-02 -- a windowed read that first read everything` | [database-apis-and-satellite-crates.md](LTM/database-apis-and-satellite-crates.md) |
| `2026-10-02 -- a blocker that was not one, and the other two arms` | [database-apis-and-satellite-crates.md](LTM/database-apis-and-satellite-crates.md) |
| `2026-10-02 -- the quadratic was not the fold, it was the unbounded seek` | [chunk-stream-contracts-and-lazy-operators.md](LTM/chunk-stream-contracts-and-lazy-operators.md) |
| the write wire and its bound; the peer socket can write; the advertised write cap; the differential review of two channel clients; the C ABI's placement; splitting the peer half; filter pushdown and `yesno-eval`; the `pushdown` option; the channel ticket's lifetime; one version per transaction | [Plugin Channel Protocol and Security](LTM/plugin-channel-protocol-and-security.md) |
| `Transport::Channel` for PostgreSQL and the bug the gate caught; transport fidelity in one fixture; `yesno_pg.channel_socket` and its two bugs; the FDW's absent snapshot stability; two limits hiding in a closed entry; paying back the round trip; counting the requests | [PostgreSQL Extension and Query Pushdown](LTM/postgresql-extension-and-query-pushdown.md) |
| the MySQL channel backend and leg; `Backend` widened and the retraction; the four-entry MySQL range-estimate arc | [Client Libraries and Search Integrations](LTM/client-libraries-and-search-integrations.md) |
| the verification cache as a threshold; the checkpoint's exclusive region; the seek loop's ratio; the read/write mix; the published chunk reader; `RwLock` on the segment caches; striping the caches; the shared published view | [Checkpoint and Commit Lock Contention](LTM/checkpoint-and-commit-lock-contention.md) |
| exact top-k offloading; Scaled8 top-k opportunities; the Scaled8 arm's four optimizations; `sdot` and the MSRV bump; row blocking; the missing 56% | [SIMD Arch Arms and Kernel Selection](LTM/simd-arch-arms-and-kernel-selection.md) |
| the unwired sweep's dead-symbol hit; lifting the view intersection strategy; the adversarial review of that diff; the numeric fold's planning cost; cutting the descents; `reaim`'s other two callers; the planner measurement | [Expression Planning, Statistics and Segmentation](LTM/expression-planning-statistics-and-segmentation.md) |
| the numeric fold streams; the machine-integer evaluator and its two mistakes | [Packed Lenses: Matrix, Bignum and Views](LTM/packed-lenses-matrix-bignum-and-views.md) |
| the span API that already existed; the ladder recomputed; nine tautologies deleted; measuring the ladder shift | [Storage Format, Index and Zero-Copy](LTM/storage-format-index-and-zero-copy.md) |
| failing to reproduce the mis-pointed extent, four ways; eliminating reclaim-and-reuse; the inverted path; the entry that was stale with an unopened index | [Allocation, Reclamation and Fsck](LTM/allocation-reclamation-and-fsck.md) |
| real SimHash correlation and the constraint worth more | [Compression Models and Space Economics](LTM/compression-models-and-space-economics.md) |
| seeding a follower from the archive; static archive credentials; follower readiness; the archiver's bare `ENOENT` | [Backup, Archive and PITR](LTM/backup-archive-and-pitr.md) |
| ECS checks the HTTP liveness probe | [Kubernetes Operator and Failover](LTM/kubernetes-operator-and-failover.md) |
| write transaction ownership; the ownership scenario and its reachable limit; the fencing half | [Flight Write Transactions](LTM/flight-write-transactions.md) |
| fixing the checkpointer flake and the control not written down; the transaction verbs | [Testing and E2E Harness](LTM/testing-and-e2e-harness.md) |
| the consumer's own measurement; a host-wide idle gate and a co-tenant; four right numbers and four wrong frames; the quiet-host rule's absence then its wrongness; a gate failure attributed without re-rolling; the error that made its own diagnosis impossible; the load average as the wrong instrument | [Measurement and Investigation Methodology](LTM/measurement-and-investigation-methodology.md) |
| the user-documentation drift audit; four assessments that produced no code; CI red for ten days; two TODO sweeps and the backlog's prose drift; out of disk for nine days; CI green and a refusal that outlived its cause; the Rust-floor checker; the floor checker green at 1.95; verifying to-dos instead of dispatching; re-deriving the unsafe counts; the recount already automated | [Quality Gates and Project Tooling](LTM/quality-gates-and-project-tooling.md) |

### Synthesis Documents

| Synthesis document | Source topic documents |
|---|---|
| [Representation, Format, and Space](LTM/representation-format-and-space-synthesis.md) | `container-representations-and-roaring-compatibility.md`; `storage-format-index-and-zero-copy.md`; `compression-models-and-space-economics.md` |
| [Set Evaluation, Planning, and Packed Lenses](LTM/set-evaluation-and-planning-synthesis.md) | `set-algebra-kernels-and-cardinality.md`; `chunk-stream-contracts-and-lazy-operators.md`; `expression-planning-statistics-and-segmentation.md`; `packed-lenses-matrix-bignum-and-views.md` |
| [Durability, Reclamation, Replication, and Snapshot Concurrency](LTM/durability-reclamation-and-concurrency-synthesis.md) | `storage-format-index-and-zero-copy.md`; `allocation-reclamation-and-fsck.md`; `wal-mvcc-durability-and-concurrency.md`; `network-service-replication-and-operations.md`; `postgresql-extension-and-query-pushdown.md` |
| [Backup, Snapshot, and Cloud Operations](LTM/backup-snapshot-and-cloud-operations-synthesis.md) | `backup-archive-and-pitr.md`; `snapshot-leases-providers-and-privilege-separation.md`; `kubernetes-operator-and-failover.md`; `network-service-replication-and-operations.md` |
| [Testing, Gates, and Measurement](LTM/testing-gates-and-measurement-synthesis.md) | `testing-and-e2e-harness.md`; `quality-gates-and-project-tooling.md`; `measurement-and-investigation-methodology.md` |
| [System Boundaries, Services, and Integrations](LTM/system-boundaries-and-integrations-synthesis.md) | `milestones-and-system-boundaries.md`; `database-apis-and-satellite-crates.md`; `network-service-replication-and-operations.md`; `postgresql-extension-and-query-pushdown.md`; `client-libraries-and-search-integrations.md`; `context-eligibility-product-direction.md` |
| [Plugin Hosting and Isolation](LTM/plugin-hosting-and-isolation-synthesis.md) | `plugin-channel-protocol-and-security.md`; `out-of-process-plugin-via-foreign-reader.md`; `operator-hosted-plugin-container-plan.md`; `arena-cgroup-charging.md`; `plugin-shape-performance.md`; `removed-cdylib-plugin-abi.md` |
| [Query Acceleration Admission](LTM/query-acceleration-admission-synthesis.md) | `gpu-offload-on-unified-memory.md`; `query-hotspot-observation.md`; `simd-arch-arms-and-kernel-selection.md`; `expression-planning-statistics-and-segmentation.md` |

### Standalone Source Topic Documents

These source topics are deliberately not folded into any synthesis:

- [checkpoint-and-commit-lock-contention](LTM/checkpoint-and-commit-lock-contention.md)
- [chunk-local-patch-writes](LTM/chunk-local-patch-writes.md)
- [contiguous-dense-span-extents-plan](LTM/contiguous-dense-span-extents-plan.md)
- [flight-write-transactions](LTM/flight-write-transactions.md)
- [formal-model-and-proof-obligations](LTM/formal-model-and-proof-obligations.md)
- [hosted-plugin-abi-assessment](LTM/hosted-plugin-abi-assessment.md)
- [hosted-plugin-abi-design](LTM/hosted-plugin-abi-design.md)
- [redis-backed-cache-performance-20261002](LTM/redis-backed-cache-performance-20261002.md)

Open follow-ups remain in [`.agents/docs/TODO.md`](TODO.md). See
[`.agents/docs/LTM/INDEX.md`](LTM/INDEX.md) for the complete topic index and the current synthesis
map.

---

## Deep Sleep Consolidation, 2026-10-12

**No new synthesis documents.** All 16 source documents that grew in the preceding `good-sleep`
pass ( 834 lines ) were already sources of one of the eight existing syntheses, or deliberately
standalone. There was no new cluster, and inventing one to balance file counts is the forced
consolidation the skill warns against. What existed instead was **drift**: five syntheses
summarised sources that had materially changed underneath them.

Refreshed, in descending order of drift:

| Synthesis | Sources that grew | Lines |
|---|---|---:|
| [Testing, Gates, and Measurement](LTM/testing-gates-and-measurement-synthesis.md) | methodology, quality gates, e2e harness | 193 |
| [System Boundaries, Services, and Integrations](LTM/system-boundaries-and-integrations-synthesis.md) | PostgreSQL extension, client libraries | 140 |
| [Set Evaluation, Planning, and Packed Lenses](LTM/set-evaluation-and-planning-synthesis.md) | expression planning, packed lenses | 132 |
| [Query Acceleration Admission](LTM/query-acceleration-admission-synthesis.md) | SIMD arms, expression planning | 131 |
| [Plugin Hosting and Isolation](LTM/plugin-hosting-and-isolation-synthesis.md) | plugin channel protocol | 80 |

**Refreshed as facts and guidance, not prose.** These syntheses are 55 to 87 lines against sources
of several hundred each, and they work because `Stable Knowledge` carries one line per durable
fact with the detail left in the source. Restating today's material at source length would destroy
the property that makes a synthesis worth reading, so each gained lines rather than sections.

**Left standalone, deliberately.** `checkpoint-and-commit-lock-contention` ( +65 ) and
`flight-write-transactions` ( +60 ) both grew and both were previously judged standalone.
Contention now overlaps the durability synthesis and write transactions overlap system boundaries;
folding either in would bury a self-contained narrative, so the overlap is named here rather than
acted on.

**No source document was modified or deleted, and `LTM/INDEX.md` needed no edit**: no synthesis
gained a *source*, only facts, and the index's 47 rows still match the 47 documents on disk. A
cosmetic edit to show activity would have been the wrong move.

### Candidates for `distill-memories`

Flagged, not written -- promotion into the canonical documents is that skill's job, and keeping
them separate is what stops a consolidation pass from quietly rewriting them.

- **`ARCHITECTURE.md`**: that leaf planning and the rewrite planner are distinct subsystems, with
  Theorem 7's termination measure binding only the latter's rewrite arms -- the conflation deferred
  a fix for days. And that `Cursor` carries no lower bound anywhere, which is what makes a
  leaf-reach test a soundness condition rather than an optimization.
- **`QUALITY_GATE.md`**: that `.agents/docs` is gate input, so a documentation-only change is not
  exempt, and that a checker anchored on a sentence makes nearby prose load-bearing.
- Already filed as a backlog item rather than a candidate:
  `architecture-describes-a-removed-plugin-facility`.

---

## 2026-10-13 — The Java client gets a live daemon, and the image stops redownloading 1.4 GB

### What the Java client's own gate could not see

`yesno-flight-java/gate.sh` compiles the client, runs its unit tests and runs javadoc. Its only server is a fake in its own test sources, and that fake minted its ticket with `ByteBuffer.allocate( QueryTicket.HEADER_LENGTH )` -- the client's own constant. A fake built from the subject's constants agrees with the subject by construction, which is how the module stayed **green and broken for a week** through the Flight ticket header widening from 40 to 48 bytes while the Go and Python integration suites failed every test.

What landed at the time was a 48-byte hex vector mirrored across the Rust, Go, Python and Java ticket tests. That pins a layout a vector can express. It cannot pin a changed refusal, a changed status code, or a schema whose metadata moved, and it only pins the layout because someone remembers to update all four copies.

`e2e/java/client.py` is the missing half. It starts a real `yesnod` with `srv_start`, hands `flight_location` to a new `java_client()` verb, and checks every observation against Python's own sets before re-reading the same server through the harness's **Rust** Flight client -- two independent implementations agreeing about one server's state. The Java side is a `main` that *prints* and asserts nothing, so the oracle is Python's arithmetic rather than the client agreeing with itself. The one judgement it makes locally is the name of a refusal's status code, which a scenario cannot observe from outside the client.

### The blocker was imaginary, and the real constraint was elsewhere

I had been saying the host has a JRE and no `javac`, so this could not be built here. True of the *host*, and irrelevant: `e2e/Dockerfile` already pulls `eclipse-temurin:21-jdk-jammy`, sets `JAVA_HOME` and `YESNO_E2E_JAVA_HOME`, copies the whole repo to `/workspace`, and carries a built `yesnod`. A JDK, the client source and a daemon to talk to were in one place the entire time.

The real constraint was Gradle. The image's dependency cache is a BuildKit `--mount=type=cache`, so it exists while the image is being built and is gone by the time a gate runs inside it -- a scenario that invoked Gradle would reach Maven Central from inside a gate. So the check is a plain `main` in a `liveCheck` source set, a `liveCheckInstall` Sync task stages its classes and whole runtime classpath at image build time, and the verb runs `java -cp` against an ordinary directory in the image. No Gradle, no JUnit, no network. The `search_*` family does invoke Gradle at scenario time and pays that cost; this one does not.

### Two findings from the first live run, both kept

**Arrow Java folds `FAILED_PRECONDITION` into `INVALID_ARGUMENT`.** I predicted `UNKNOWN` -- reasoning that `FlightStatusCode` has no `FAILED_PRECONDITION` and `StatusUtils` would fall through -- and the measurement said otherwise. The consequence is sharper than the one I guessed: **aborting an already-committed transaction and fetching a malformed ticket are the same status code to a Java caller.** One means the request was nonsense and the other means the request was well formed and the world had moved on, and nothing but the message text separates them. The scenario pins the collapse, with a second assertion that fails if the two ever diverge so the comment cannot go stale silently. This is exactly the class of thing a wire vector cannot express.

**A client cannot observe the teardown of streams it just closed.** The driver read `stats().liveReaders()` before disconnecting and saw **3**, with nothing leaked: the server unwinds those tasks after answering the request. The observation was removed rather than re-baselined to 3, which would have been equally arbitrary and equally racy. `srv_stop`'s `readers_left`, taken after the daemon has drained, is where that question has a settled answer, and the scenario already asserted it.

Of 23 observations, 21 matched on the first live run.

### The image was re-downloading 1.4 GB on every rebuild

Found by rebuilding twice in one day. `search.rs`'s `download_archive` cached the pinned OpenSearch and Elasticsearch tarballs under `crate::scratch_dir( workspace )` -- `.agents-workspace/tmp/search-e2e-cache` -- and `.agents-workspace` is in `.dockerignore`. So the cache was written into the layer that `COPY . /workspace` invalidates, and every source edit re-fetched both archives. **Measured: the second rebuild was served at 350 kB/s, a 37-minute projection, against 11 MB/s an hour earlier.** A build whose duration is set by someone else's throttle is not a slow build, it is an unpredictable one.

Fixed with a `search-archives` stage above the source copy, which is the shape the Dockerfile already uses for every other pinned fixture. Two `ENV` lines set `YESNO_E2E_OPENSEARCH_ARCHIVE` and `YESNO_E2E_ELASTICSEARCH_ARCHIVE`, overrides `search.rs` already honoured, so build time and run time both read the staged copies.

**A stage and not a cache mount, and the reason generalizes.** These archives are consumed at **run** time too -- `search_engine_start` unpacks one per scenario -- and a build-time mount does not exist by then. The Dockerfile already records that same mistake twice about the Bazel caches. A stage's output is copied into the image, which is what run time needs, while its layer is keyed only on its own instructions.

The pinned list moved to `e2e/search-archives.tsv` so the stage and `search.rs` read one table rather than two; a second list would drift in the direction that costs the most, with the stage fetching a stale URL and the harness rejecting the result half an hour into a build. `search.rs` includes it with `include_str!` and parses it strictly, because a lenient parser's failure reads as "no pinned archive for this architecture" and sends the next reader looking at a platform limitation instead of a malformed table. The digest is still verified by the harness on every use including the overridden path, so the stage is a courier and never the thing that vouches for the bytes.

Measured after: the search artifact step went from **203 s to 88 s**, and no longer downloads anything.

### Verified

`./scripts/gate-java.sh` passes ( `ok client.py`, 11 verb calls ). `./scripts/gate.sh` passes, with `yesno-flight-java/gate.sh` reported skipped for want of `javac` on this host, as it always is. `gate-pg.sh` was not run and is not implicated: neither `yesno-core` nor `yesno-flight` changed.

Both new tests are sabotage-checked. Dropping a field from a table row fails `the_whole_pinned_table_parses`; swapping an engine's two architecture rows fails `every_supported_engine_architecture_is_checksum_pinned`, which is why that test asserts the architecture token appears in the file name rather than only that the digest is well formed.

---

## 2026-10-13 — The capability bitmask had drifted in three of four clients, and now it cannot

Filed as a Java defect while writing the live-daemon gate: `ServerStats.java` names two of the server's three capability bits. The survey the entry asked for said it was not Java's defect. `FEATURE_CONTAINER_PUT` ( bit 2 ) landed in `yesno-flight` on 2026-10-03, and the Python, Go **and** Java clients were all never told. For ten days three of four implementations could not name a capability the server advertises on every `stats` call.

### Why that is worse than a missing constant

`ServerStats.features` is not decoration. A client is required to consult it before sending a new `do_put` command, because a server predating a bit reports zero and treats an unrecognised command as insert -- so an unchecked mixed batch has its removals applied as **insertions**, with no error anywhere. The entire mechanism rests on client and server agreeing which bit means what, and nothing was checking that they did.

This is the second instance of one shape. The Flight ticket header widened from 40 to 48 bytes on 2026-09-30, three clients were not widened with it, and the Go and Python integration suites failed every test for a week while Java stayed green because its only server was a fake built from its own constants. The repair then was a 48-byte hex vector mirrored across four test files. That works, and it has exactly the weakness that produced this entry: it holds only while someone remembers all four copies.

### What closes it

`scripts/check-feature-bits.py` parses the bits out of all four sources and fails when the sets or the positions disagree. Three details are the whole value:

* It normalizes case and separators, because Go spells them `FeatureMixedPut` and the others `FEATURE_MIXED_PUT`. That is a real convention difference, not drift, and a checker that called it drift would be turned off.
* It **refuses to pass on a source it found no bits in.** A regex checker's characteristic failure is matching nothing and reporting agreement with an empty set; that is the same "tools that cannot see their subject" failure this tree keeps finding, applied to the tool.
* `yesno-flight/src/lib.rs` is the authority rather than a fifth hand-written list, because that is where the server decides what it implements and what `FEATURES` puts on the wire.

Sabotage-checked both directions: a bit moved to the wrong position, and a bit a client invented. Wired into `scripts/gate.sh` and `ci.yml`, so `check-gate-parity.py` sees it in both -- 21 shared scripts now, up from 20.

### The constant question, answered the other way

The backlog entry asked whether to add a constant for a path no client can take, and leaned towards no: "a constant for a path the client cannot take is API surface with no caller." That reasoning was wrong, and the reason it was wrong is worth keeping. **The bitmask is server capability discovery.** What the server accepts is a fact about the server, and a caller may legitimately want it whatever this client happens to be able to send -- to choose a different tool for a bulk load, to report what a deployment supports, to fail early. The three new constants each say in their own doc comment that they name what the server accepts and that the client cannot build such a batch yet, which makes the honest answer available without implying a path that is not there.

It also makes the checker possible. A deliberate omission would need an exception list, and an exception list is the thing that grows every time someone is in a hurry.

### The gap that remains is bigger than the one that was filed

Container payloads on the write wire measured **64.8x less wire** than one row per set bit, and 1058-1212 ms to 41.0-42.1 ms on a 64-chunk half-dense page. Only the Rust client can send them. Filed as `three-clients-cannot-send-container-batches`, with the note that Java may be the cheap one -- it already depends on `org.roaringbitmap:roaringbitmap` and the payloads are recorded as byte-identical to a `.roaring` file -- and with the prior question that actually decides it: whether any non-Rust caller has a bulk-load shape at all. The 64.8x is nothing to a client inserting a few thousand pairs.

### The gate's own guard fired, correctly

The first full run after adding the step reported `INCOMPLETE: ran 23 of 22 steps` and failed, with every individual check green. `EXPECT_STEPS` is a hardcoded count that exists to catch a gate that stopped early and reported a verdict on a partial run; a new step is indistinguishable from an extra one until someone says which it is. Diagnosed before raising, as the comment beside it demands, and in the easy direction: one new unconditional `check` added deliberately in this commit, so the delta is an addition by construction and counts in both the routine and the deep mode. Raised 22 -> 23 and 28 -> 29.

Worth recording because the guard is easy to read as noise and is not: a checker added without touching that number would have made the gate fail for a reason unrelated to the change, and a checker added while *lowering* it would hide a real early exit.

### The rule

Recorded in `QUALITY_GATE.md` § 2: a constant the wire defines is restated by four implementations, and a convention is not a mechanism. The ticket header, the capability bitmask and the mutation opcodes are all of this shape. **Look for the third instance.**

---

## 2026-10-13 — A Java client can build yesno container payloads with no new encoder

Container payloads on the write wire cost **64.8x less** than one row per set bit, and only the Rust client sends them. The entry I had just filed named Java as probably the cheapest of the three remaining clients, on the grounds that it already depends on `org.roaringbitmap` and that the payloads are recorded as byte-identical to a `.roaring` file -- and told the next reader to **check that claim before budgeting the work**, because "byte-identical to a `.roaring` file" is a statement about the container payload while the batch also carries key, prefix and cardinality columns.

Checked. It holds, and the encoder is free.

### The contract, read out of the decoder rather than the commit message

`codec::decode` wants: an array as `2 * card` little-endian `u16` values, strictly ascending, **no leading count**; a bitmap as exactly 8192 little-endian bytes whose popcount must equal the declared cardinality; a run as a little-endian `u16` interval count followed by that many `( start, len - 1 )` pairs, so `2 + 4 * nruns` bytes. That is the Roaring container layout, which is the whole reason this is possible.

### The public access path is not the obvious one

`RoaringArray.getContainerAtIndex` and `getKeyAtIndex` are `protected`, and `RoaringBitmap.highLowContainer` is package-private, so the route a reader would reach for first is closed. `RoaringBitmap.getContainerPointer()` is public, and `ContainerPointer` happens to expose exactly a yesno row: `key()` for the prefix, `getContainer()` for the payload, `getCardinality()` for the cardinality column, and `isBitmapContainer()` / `isRunContainer()` as the kind discriminator with array as neither. Established with `javap` against the jar already staged in the E2E image by the live-daemon gate, which is what made the check cheap.

### Measured against the shipped decoder, with an independent oracle

A Java `main` built one `RoaringBitmap` spanning three prefixes -- a 4-value array, 9000 strided bits, and two contiguous intervals after `runOptimize()` -- and printed each container's kind, cardinality and hex payload. A scratch crate with a path dependency on `yesno-core` fed each row to `codec::decode`. All three accepted, and the 17 004 decoded ordinals compared equal to a `BTreeSet<u64>` recomputed **in Rust from the same construction** rather than from the dump, so the comparison is against an oracle and not against the thing under test.

### Four traps, and the first one is why the check was worth running

**`serializedSizeInBytes()` is not the payload length.** For an array container it is `2 + 2 * card` -- two bytes more than the values -- so the natural `ByteBuffer.allocate( c.serializedSizeInBytes() )` followed by `buf.array()` appends two zero bytes. The first run produced exactly that: `bytes=10` for a 4-value array where yesno wants 8. The decoder refuses it ( `payload length 10 is not valid for "array" with cardinality 4` ), which is the right failure and is recorded as a negative control, but a reader who trusted the method name would have shipped it. The answer is the buffer's written `position()`.

The other three: `ByteBuffer` defaults to **big-endian** and both formats are little-endian; `runOptimize()` is required or there are no run containers to send; and a 32-bit `RoaringBitmap` reaches only `prefix48 < 2^16`, so the 64-bit domain needs `Roaring64NavigableMap` composed as `( high32 << 16 ) | key`.

### What it settles, and what it does not

It settles the stated risk -- the encoder is free for Java. It does not make the work worth doing: the 64.8x is on the write wire for dense data, and a client inserting a few thousand pairs pays nothing for the pair wire. The prior question stands, and it is a workload question rather than a code one: does any non-Rust caller have a bulk-load shape at all?

It also sharpens the entry in the other direction. **Python and Go have no per-container access to a roaring value**, so for them this is a payload encoder as well as a wire change. Pricing all three together, which the original entry implicitly did, would have been wrong by a wide margin.

Derivation recorded in `LTM/client-libraries-and-search-integrations.md`; the instrument was a scratch crate under `.agents-workspace/tmp/` and is discarded, per the research rule.

---

## 2026-10-14 — The unwired sweep becomes a script, after making its own recorded mistake a third time

`rerun-the-unwired-sweep` is a hand audit that has been run at least six times. It works: it found `Db::open_reader_with_events` within a day of the symbol landing, and it found `KeyStream::chunks_remaining`, whose body production was already writing out by hand three screens below it. The backlog entry records its own construction, both of its controls, two earlier ruler corrections, and ends with a process note saying the author rebuilt the ruler from scratch anyway and got it wrong twice before arriving back at the notes already written there.

I read that note, rebuilt the ruler from scratch, and got it wrong a third time.

### The error

The first run reported two dead symbols: `into_magnitude`, which is on the recorded list and has been read twice, and **`Tree::build_reusing`**, which is not dead at all. It is the **differential oracle** for `build_updating` -- `build_updating_agrees_with_build_reusing` calls it, and `tree.rs` says in so many words that it "remains only for `build_reusing`, which is the differential oracle".

The cause is worth more than the hit. My comment stripper dropped every line beginning with `#`. That is right for shell, Python, YAML and TOML. In Rust `#` opens an **attribute**, so it deleted every `#[cfg(test)]` in the tree. The production/test split collapsed into a single blob, and a symbol whose only caller is a test became a symbol with no caller at all. The first `#[cfg(test)]` in `tree.rs` is at line 132, so the mis-split swallowed the entire file.

**Third error of one family**: a ruler rebuilt by hand, reporting a wired tree as unwired. The previous two were counting bare filename occurrences, and excluding only Markdown when every surviving mention of `miri.sh` outside Markdown is a comment.

### The fix is the mechanism, not the correction

A recorded construction is a convention. Six hand runs and three ruler errors is the evidence that a convention is not a mechanism -- the rule I wrote into `QUALITY_GATE.md` § 2 yesterday, arriving a day later for a different reason.

`scripts/check-unwired.py` now carries both halves, both baselines, and the per-language comment marker with the reason written beside it. One tokenizing pass over the corpus replaced one regex scan per name: **38 s to 0.7 s** for the same answer, which is the difference between a gate step and an errand.

Both baselines shrink and never grow, like `check-r1.py`: it fails on a new orphan *and* on a baselined entry that has gained a caller, so the lists can only get shorter.

### The control the entry was missing

Four controls now, all verified against the real script:

1. a planted orphan **fails**;
2. a symbol called only from a `#[cfg(test)]` module **passes**;
3. a baselined dead symbol that gains a caller **fails**;
4. a baselined script that gains a caller **fails**.

The second is new, and it is the one that matters. The entry's recorded controls were a planted orphan and a known-wired symbol -- both of which my broken ruler passed, because it was wrong only about the *boundary between production and test*, not about presence. A control has to separate "no caller" from "no production caller" or it cannot see this class of error at all. That is the transferable part: **a control must discriminate the distinction the checker exists to make, not merely prove the checker reacts to something.**

### The sweep's actual result, which is quiet

Scripts half reproduces the recorded result exactly -- 39 considered, 3 never invoked, all three already explained, no action -- and the two scripts added the day before are correctly seen as wired. `pub fn` half: 86 files, 821 declared, **1 with no caller anywhere**, 115 test-only. The one is `into_magnitude`, already recorded. No new dead symbol has appeared.

### One rule I broke

Restoring a sabotaged file during the controls, I used `git checkout -- <path>` once. The standing rule in this repository is not to, because another agent may be working in the same checkout. No harm done -- the file was restored and `git status` confirms the tree is clean -- but the correct move was the backup copy I used for every other control, and I should not have reached for the other thing.

---

## 2026-10-14 — The cost model was being taught to predict a defect

`cost-model-cannot-see-materialization` records that `cardinality_cost` rates `And( ¬p, ¬q )` and `¬( p ∪ q )` equal while they differ 214x in execution, and concludes that "what is needed is a cost model that charges an operator for operands it must materialize, which is a real modelling change". A later pass measured a ten-shape corpus, found a second inversion family, and sharpened the prescription to "a second dimension ... keyed on the materialized cardinality".

I rebuilt the corpus to get before-numbers for that change. It refuted the prescription and relocated the defect.

### Materialized cardinality is not the key, and the corpus says so

`And( dense, dense )` materializes **131 072 ordinals in 17 ns**. `And( ¬p, ¬q )` materialized the same 131 070 in **46 981 ns**. Same quantity, 2 700x apart. Draining each operand on its own says why: a **stored** chunk costs 15-32 ns to produce *regardless of cardinality* -- `dense`, one chunk holding 65 536 ordinals, drains in 32 ns -- while a **complement** chunk cost 23 502 ns. The question was never how much gets built. It was what builds it.

### What built it slowly

`Not::produce` builds its window as a one-interval `Run` and calls `and_not( universe, input )`. `mixed::try_apply_array_run` has a fast arm for `AndNot` **only when the array is the left operand**, so `Run \ Array` falls through to bitmap expansion, and the bitmap result is re-encoded by `Container::optimize`.

Timed directly: **`optimize` on a 65 535-bit bitmap was 23 292 of the 23 424 ns**, and cloning its 8 KiB of words was 90 ns, so this was not memory traffic. `BitmapContainer::runs()` short-circuited all-**zero** words and then tested all 64 bits of every other word. A dense bitmap has no zero words, so it paid 65 536 bit tests at ~0.36 ns each.

**`run_count()` was already word-wise**, using `( x & !( x << 1 ) ).count_ones()`. The two functions compute the same structure. That asymmetry was the whole defect: one counted runs in 1 024 word operations while the other enumerated them in 65 536 bit tests.

### The fix, and what it is worth

`runs()` is now word-wise too: per word it pairs the run **starts** `x & !( x << 1 )` with the run **ends** `x & !( x >> 1 )` and advances with `trailing_zeros`, so the inner loop runs once per run rather than once per bit. Bit 63 is suppressed from `ends` when the next word's bit 0 is set, which is what keeps a 1 024-word span one maximal run instead of 1 024.

```text
  measurement                      before      after   factor
  optimize( bitmap, 65 535 bits )  23 292 ns  2 848 ns    8.2x
  and_not( Run, Array )            23 424     2 974       7.9x
  drain !p, 1-chunk window         23 528     3 063       7.7x
  And( !p, !q ) 1-chunk            46 981     6 127       7.7x
  And( !p, !q ) 32-chunk          423 925    56 244       7.5x
```

Every shape with no complement in it is unchanged within noise -- `And( p, q )` 27 781 -> 27 639, `Or` 24 173 -> 24 537, `Xor` 24 057 -> 23 993 -- which is the control that says this is the path it claims to be.

The gain is not confined to complements. `optimize` runs on every mixed-kernel result, so any dense bitmap that re-encodes as runs was paying this.

### The test is the old implementation

`word_wise_runs_agree_with_the_per_bit_oracle` keeps the per-bit loop `runs()` replaced and asserts the two agree, over all-ones, a single cleared bit at position 0 / 63 / a word boundary, runs ending exactly at bit 63, alternating bits, single bits at each end, and a deterministic pseudo-random spread. It also cross-checks `run_count()` against the length of the runs it would enumerate, which nothing did before, and asserts every emitted pair is non-touching so a correct *count* cannot stand in for maximality.

Four sabotages, each failing it and each restored byte-identical: dropping the next-word suppression, treating bit 0 as a new start when a run is open, forgetting to carry an unterminated start, and an off-by-one on the run end.

### What this leaves

The entry survives: the model still rates the De Morgan pair equal and they still differ. But **24.5x, not 187x** -- a seventh of the prize the modelling change was being justified against, and the second inversion family has shrunk on its own too ( `And( p, q )` against `Or( p, q )` is 1.15x in runtime now, not 3x; the 4x is a pricing difference, not a runtime one ). Re-derive before pricing that work.

What remains on the fast path is filed as `run-minus-array-expands-to-a-bitmap`: the missing `Run \ Array` arm is still 2 974 ns against 17 ns for the array-on-the-left pair, about 300x, with the generic kernel available as the differential oracle the crate's own policy promises.

### The order of the questions

Two sessions went into designing a cost model faithful to an execution cost that was a defect. A cost model's job is to predict what the engine does; before making it predict an expensive operation faithfully, check that the operation has to be expensive. The neighbouring entry already had the slogan -- *ask what is refusing before asking what is imprecise* -- and this is the same shape with "slow" in place of "refusing".

---

## 2026-10-14 — I pushed a red gate, and the reason is the lesson from the commit that broke it

`9c09ab8` added `scripts/check-unwired.py` and a gate step for it. I ran `scripts/gate.sh` before pushing and it passed. The next full run, on a clean tree, **failed** — on that step, with three false positives:

```text
  scripts/fmt-scoped.py is baselined as uninvoked but now has a caller
  scripts/gate-mount-propagation.sh is baselined as uninvoked but now has a caller
  scripts/miri.sh is baselined as uninvoked but now has a caller
```

### Why the verifying run could not see it

The checker's corpus is `git ls-files`. When I validated it, `check-unwired.py` was **untracked**, so it was not in its own corpus. The commit made it tracked, and its `BASELINE_SCRIPTS` dictionary — which names every script it baselines, in string literals no comment stripper removes — started counting as a caller of each one. The baseline is a statement *about* invocation; the checker read it as an invocation.

So I verified the tool in a state it would never be in again, and the first state it could never be verified in was the one it shipped in. **A checker whose corpus is the tracked tree has to be validated from inside the tracked tree.**

That is the same failure as the ruler bug in the same commit, one level up. There I wrote that a control must discriminate the distinction the checker exists to make. Here the missing control was the file's own tracked-ness, and I did not think to vary it because the tool worked.

### The second bug, which is plainer

```text
  ./scripts/gate.sh: line 547: pub: command not found
```

The step title was `step "... every core `pub fn` called"`. Backticks inside a **double-quoted** shell string are command substitution, so the gate ran `pub fn` as a command on every invocation. Harmless only because no `pub` exists on the path. The header printed as "every core  called", which is the visible tell I walked past in the passing run — the substitution had already eaten the words before I read the output.

### Fixes, and the controls that now hold them

The step title is single-quoted. The checker excludes its own file from the invocation corpus, with the reason written beside it.

Two controls, both run with the file tracked this time: removing the self-exclusion reproduces all three false positives exactly, and a baselined script that gains a **real** caller still fails. The second is the one that matters — excluding a file from the corpus could easily have blinded the shrink-only direction, and it does not.

### What to carry

Two things. **Read the header a passing step prints**, not just its verdict; "every core  called" was the bug announcing itself in the run I accepted. And when a tool's input is the repository state, the states to vary include *whether the tool itself is committed* — which is invisible from inside a working tree where it already is.

---

## 2026-10-14 — The archive ENOENT was a shared temporary, and the path it finally printed said so

`the-archive-sidecar-fails-with-enoent-under-load` had been open since 2026-09-28: `pitr_retention.py` and `archive.py` failing at `srv_archive_wait` with a bare `No such file or directory ( os error 2 )`, 0 reproductions in 26 isolated runs, two sessions of reading, and a recorded "strongest suspect" that turned out to be innocent.

It failed a third time, in a routine gate run, and this time it named its file:

```text
  cannot rename the state temporary
    '.../archive-work/.state.pb.partial': No such file or directory ( os error 2 )
```

That was the whole investigation. The 2026-09-29 note had asked only that the next occurrence capture the path, and `archive::ctx` was added for it; the named path identified the defect in about ten minutes of reading.

### The defect

`write_local_state` writes to a temporary and renames. Its temporary was derived from the **target's** file name -- `.{name}.partial` -- so it was one name per *path*, not one per *call*.

The sidecar spawns a `wal_loop` **per shard** plus `event_loop`, `term_loop`, `writer_lease_loop` and `retention_loop` into a single `JoinSet`, and four of those call `publish_state` on the same `runtime.local_state`. Two concurrent writers therefore created, wrote and `sync_all`'d the *same* temporary. Whichever renamed first took it away; the other's `rename` failed `ENOENT` **on a file it had itself just synced a microsecond earlier**. In the local-objects mode every e2e scenario uses there is nothing to serialize them: that arm is a plain `put`, with no writer-lease renewal and no compare-and-swap.

### Why it never reproduced

Every reproduction attempt re-ran the *scenario*, and the scenario's concurrency is incidental to the bug. The window is the few microseconds between one writer's `sync_all` and its `rename`; two publishers almost never land inside it, and 26 runs of a workload that happens to produce two publishers is a poor way to sample it.

Sixteen concurrent `write_local_state` calls against one path reproduce it **deterministically in 20 ms**, with the identical message. That is the regression test.

### The fix

A per-call temporary -- `.{name}.{pid}.{seq}.partial`, following `capture_target`'s naming for the same reason -- and removal of the temporary on any failure, since a per-call name cannot be reused and would otherwise accumulate in the work directory. The test fails under a sabotage restoring the shared name, with the original error.

### Two things to carry

**The strongest suspect was wrong, and it was a good suspect.** The entry named the direct-path snapshot flow and `remove_staged_base` -- "a lease release or checkpoint cleanup landing between the two is exactly a bare `ENOENT` on a path the sidecar never chose". It reads well and it is false: `remove_staged_base` removes `work_dir/base-...`, a subdirectory, never the work directory the state file sits in. The `NotFound` tolerance added for it on 2026-10-11 is correct on its own terms and fixed nothing here. A plausible mechanism that explains a symptom is not evidence for it, and the cost of treating it as evidence was two sessions.

**Diagnosability beat reproduction, and it was much cheaper.** 26 attempts to reproduce yielded nothing. One `map_err` adding the path turned the next occurrence into a ten-minute read. When an error can arise from several places and says which only by luck, spend the effort on making the next sighting informative rather than on making it happen again -- which is what the entry itself concluded on 2026-09-29, and it was right.


---

## 2026-10-14 — Four serial fetch loops on the restore path, and the one that was hiding

A consumer suggested that letting a follower retrieve the base archive *incrementally* would cut boot time. Reading the path to evaluate that found something simpler and larger: **every object fetch on the restore path was a plain `for` loop with an `.await` inside**, so a restore paid one full round trip per object, in series.

### What the audit found

Four loops, and the first pass over the code only saw two of them:

| loop | shape |
|---|---|
| `load_wal_descriptors` | one `get_bytes` per `.wal.pb` across the whole retained WAL history -- many small objects, pure latency |
| `list_bases` | one manifest fetch per retained base |
| `materialize_base` | one `shard-NNNN.yno` image per shard -- few, large |
| `assemble_wal` | per shard, each walking its own fingerprint chain |

The widest is `load_wal_descriptors`, and it is the one that was missed on the first read. Earlier in the same session I had characterized the base as "a few large objects, so per-object latency is negligible" -- true of `materialize_base`, and wrong about the path as a whole, because the *descriptors* are the many-small-objects case and they are fetched separately.

### The change

All four now run at `ARCHIVE_FETCH_CONCURRENCY`, with two deliberate choices.

**`buffered`, not `buffer_unordered`.** Results and errors arrive in the order the serial code produced them, so nothing downstream reasons about completion order and a failure still names the first offending object in listing order. The concurrency is identical; only the retirement order differs.

**Eight, deliberately modest.** The caller is usually a follower booting, and a cluster restart boots all of them at once. A large per-follower fan-out turns a recovery into a thundering herd against a single object-store prefix. Eight hides latency without being a load generator.

Within a shard the WAL walk stays sequential: the next object is selected by the *current* fingerprint, so there is nothing to overlap. The loop body became `assemble_shard_wal` unchanged; shards are independent because each writes its own `shard-NNNN.wal`.

### What was left alone, and why it is a decision rather than an omission

`bootstrap_shard`, the leader-driven path, is serial across shards too. It is **not** parallelised here. It opens with `check_leader`, which mutates `FollowerClient::adopted` and `term` and carries the comment "Before a byte is written, not after: this is the whole point." Concurrent shards either race that adoption check or require hoisting it out of the loop -- and hoisting turns a per-shard fence into a per-bootstrap one, so a leadership change mid-bootstrap is no longer caught between shards. The MANIFEST fence still sits behind it, but that is a semantic change to a fencing path in replication and belongs to whoever owns the failover contract, not to a performance pass.

### On the original claim

Worth recording because the question was asked and answered rather than acted on blind. "Incremental retrieval" has three readings and they do not fare alike. *Resumable* helps only the retry path -- though an interrupted seed does re-download everything today, since `restore_to` refuses an existing target and `create_staging` mints a fresh directory per attempt. *Lazy or demand-paged* fights three invariants: `inspect_base` verifies the whole base against its manifest, recovery must land exactly on the selected version, and readiness requires a full replication pass -- so it would defer precisely the bytes a read-serving follower exists to serve. *Delta* already exists under another name: a follower that keeps its MANIFEST never refetches the base and closes the gap by WAL.

None of the three touches `sync_tree`, which fsyncs the whole restored tree before publication, or the first full replication pass that gates `/readyz`. **Where boot time actually goes is still unmeasured**, and the parallelism above was taken on the separate ground that independent network transfers running in series is a defect whatever the profile says.

---

## 2026-10-14 — Where the restore path's concurrency stops, and the rule that put it there

Asked whether the tree uses tokio, after the four serial fetch loops were parallelised. It does, in a shape worth writing down, and the answer turned up the limit of the change made an hour earlier.

### The layering is enforced, not merely intended

`yesno-core` has **no tokio**, and the `lean-core` CI job fails if `cargo tree -p yesno-core -e normal` so much as mentions `tokio`, `tonic`, `prost` or `arrow-flight`. Async starts one layer out, in `yesno-server-utils`, `yesno-server` and `yesno-e2e`, and every binary entry point is `#[tokio::main]` or an explicit `Runtime::new()` -- multi-thread in both cases.

### The consequence for the change just made

`buffered( 8 )` is concurrency on one task, not parallelism across threads. For network fetches that is exactly right: the reactor drives the sockets and eight requests are genuinely in flight. But `WalWriter` lives in `yesno-core`, so `open`, `append_frames_at` and `sync` are **synchronous by construction** -- the lean-core rule makes them so -- and `assemble_shard_wal` alternates an awaited fetch with a blocking write. Eight of those futures now share one task, so while one shard writes, the other seven cannot issue their next fetch.

Never worse than the serial loop it replaced, which made the same blocking calls with nothing to lose. But the eight-way figure overstates what was bought, and the overstatement is the thing to record. Filed as `the-restore-path-makes-blocking-core-calls-inside-async`.

### Two things the audit corrected about itself

**`materialize_base` has the same shape and is already clean.** `download_base_file` writes through `tokio::fs`, and its one synchronous call -- `inspect_base` -- sits *after* the join rather than inside it. So the defect is one function, not a property of the file, and a sweep that stopped at "restore.rs has zero `spawn_blocking`" would have proposed four changes where one is warranted.

**The usual reason for `spawn_blocking` does not apply here.** It is normally about not starving a reactor that is also serving queries. A restore never shares a runtime with serving traffic: `yesno-server` runs the seed as a **separate helper process** ( `tokio::process::Command`, `seed-follower` ) before it starts serving, with the comment that the seed can outlast Kubernetes' liveness window, and `yesnoctl restore` is a one-shot CLI. So the cost is confined to the restore's own overlap. That is a smaller claim than the one worth making before checking, and it is why the entry asks for a measurement rather than a patch -- the fix is worth nothing if the loop is mostly network.

The same reasoning retires three other call sites before anyone spends time on them: `restore_to` runs `materialize_base`, `assemble_wal`, `plan_and_cut`, `Db::open_replica` and `sync_tree` strictly in sequence, so moving any of them to a blocking thread adds a hop and overlaps with nothing.

---

## 2026-10-14 — `Run \ Array` gets its arm, and a bimodal probe nearly sold me a regression

`mixed::try_apply_array_run` had a fast arm for `Array \ Run` and none for `Run \ Array`, so the latter expanded the run into 1 024 words, cleared a handful of bits, and asked `optimize` to find the intervals again. That is exactly what `Not::produce` computes for every complement chunk: a one-interval window minus the input.

### The numbers

A/B on one tree -- the arm present, then the same tree with its result discarded through a `black_box` filter -- six processes per arm, modes reported.

| | arm off | arm on | |
|---|---|---|---|
| `and_not( Run[0..65535], Array[0] )` | 2 978 ns | **30 ns** | 99x |
| `And( not p, not q )`, 1-chunk window | 6 140 ns | **223 ns** | 27.5x |
| `And( not p, not q )`, 32-chunk window | 56 300 ns | **3 120 ns** | 18x |
| `not ( p or q )`, 1 chunk | 251 ns | 249 ns | unchanged |
| `not ( p or q )`, 32 chunks | 697 ns | 694 ns | unchanged |

The last two rows are the control and had to come out flat: `Not::cardinality_dyn` counts the window width and subtracts the inner cardinality, so it reaches no set kernel at all.

### Where the cost actually was

Not the word loop. The probe prices `optimize` on the resulting 65 535-bit bitmap at **2 840 of the 2 978 ns** -- a popcount, a `run_count` and a `runs()` across 1 024 words, to recover the single interval the operands had already described. Building words to describe an interval and then searching the words for the interval is the whole defect; the direct arm is `O( nruns + |array| )` and never builds them.

This is **not** an argument for dropping `optimize` from the expansion path, which is what stops an 8 KiB bitmap being stored for a handful of intervals. The neighbouring bitmap x run arm's cheaper `ensure_demoted` is a different trade, not a correction to this one.

### The cap is load-bearing, which took a specific construction to show

Above `RUN_MAX_INTERVALS` the arm bails to the word expansion. The obvious test -- 4 096 points out of the full chunk -- does not prove the cap is needed, because at 4 096 intervals `optimize` demotes the run to a bitmap anyway and removing the cap changes nothing observable.

The window where it bites is narrower. `optimize` switches encoding only on a 12.5% saving, so it keeps a run until `2 + 4n >= 9363`, that is `n >= 2340`, while the writer refuses to emit more than 2 032. Between those two numbers nothing downstream corrects an over-long run. 2 200 points out of the full chunk lands there, and that is the case the test pins; raising the cap fails on it and on nothing else.

### Two corrections the work made to itself

**The random generator was testing an input no container holds.** It built intervals separated by a gap of one, which `validate` rejects as adjacent, and the first run failed on its own operand rather than on the kernel. The generator now leaves a gap of two and asserts both operands validate before comparing -- a disagreement on an impossible input says nothing.

**The probe is bimodal, and it briefly showed a regression in a shape the change cannot reach.** The first A-B-A-B read `not ( p or q )` at 248 ns with the arm off and 521 with it on, reproducibly across both phases. Measuring that shape first in its own process reproduced it. A null edit -- twelve dead `#[inline(never)]` functions added to the same module -- produced **516 and then 253 from the same binary**, which is what settled it: one process in roughly eight lands at twice the modal figure, and a median of seven samples *within* a process cannot see it because the process is the bistable unit. Both arms exhibit it.

Carry this: an A/B whose two sides differ by less than about 2.5x on this host is not measurable from one process per arm, however clean the interleaving looks. The 99x and 27.5x figures survive it comfortably; the 2x "regression" was the artifact, and I spent four experiments before testing the null edit that would have ruled it out first.

---

## 2026-10-14 — The flock holder was a forked child, and the failing build said so all along

`a-dropped-db-may-not-have-released-its-flock` had two gate sightings, 0 in 24 direct reproduction attempts, and a plan that amounted to waiting for a third. It is closed instead, with a reproduction that runs in under a second.

### The mechanism

`flock` attaches to the open file **description**, not to the descriptor. `fork` gives the child a descriptor onto the same description, and Rust's `O_CLOEXEC` closes it at `exec` -- not at `fork`. So between those two points a spawned child holds the database's directory lock, and a `try_lock` in the parent sees a genuine second holder and is right to report it. The holder is our own child, it is already leaving, and nothing the caller is told can be acted on.

### What pointed at it

Not more sampling. The entry recorded that both failures were in the `cargo test -p yesno-core --features jit` step and treated that as a fact about which binary to re-run. The useful question was what is *different* about that binary: it is the only one carrying `jit::tests::jit_cache_drop_reclaims_executable_mappings`, which re-runs the test binary as a child process while every other test in the binary runs beside it.

### Measured, 200 close-then-open sequences per arm

| arm | reopens refused | any open refused |
|---|---|---|
| no child processes ( control ) | 0 | 0 |
| one `/bin/true` every 10 ms | 3 ( 1.5% ) | 109 |
| four spawners, continuous | 5 ( 2.5% ) | 193 |
| `pre_exec` sleep of 20 ms | 200 ( 100% ) | 200 |

The control is what makes this evidence. The first-open column is larger than the reopen column throughout for a mechanical reason worth stating: once a child holds the lock, *both* opens of a cycle fail and the loop spins, so folding the two together would have reported 98% and overstated the thing by a factor of forty.

### It is a product defect, not a test artifact

The entry filed it that way round and was right. `yesno-server`'s snapshot providers spawn `mountpoint` and the LVM and EBS tools while the database is open, so an open racing one of those is refused naming a holder that is about to cease to exist -- an unattributable error of exactly the kind `follower.rs` and `plugin.rs` already have comments about.

### The fix, and why it does not overturn the decision it touches

`acquire_lock` now tries `LOCK_ATTEMPTS` ( 10 ) times `LOCK_RETRY_GAP` ( 5 ms ) apart. The in-function comment it replaces argued for non-blocking on the grounds that "blocking here would hang a caller with no way to know why", and that still holds: this is bounded, it still reports, and 45 ms is three orders of magnitude past a `fork`-to-`exec` while failing a genuinely contended open promptly -- which the follower, plugin and lifecycle paths all depend on, since each reads a persistent `AlreadyOpen` as "a peer still has the directory".

End to end: **42 707 spawns and 0 refusals**, against 2.5% of reopens before. Both halves are pinned by one test, because they pull opposite ways -- a holder that leaves must be waited out, a holder that stays must still be reported -- and it fails under both sabotages, one attempt and no sleep. The test stands a second `flock` on the same `LOCK` file in for the forked child: the same thing from the lock's point of view, and it needs no `unsafe` and no subprocess.

### Three things to carry

**Diagnosability was the right investment and was not sufficient.** The 2026-10-12 split of `WouldBlock` from `LockUnavailable` was correct, and this entry's plan rested on it: wait for a third sighting and let it classify itself. A third would have said `AlreadyOpen` -- truthfully, a real holder existed -- and still not named a forked child. The entry's other instruction, to run `fuser` against the directory on the next occurrence, would most likely have found nothing, because by the time anyone looked the child would have `exec`ed. Contrast the archive ENOENT, where one `map_err` carrying the path turned the next sighting into a ten-minute read: there the missing information was *in* the error, here it was in the process tree a millisecond earlier.

**"Which binary" and "what is in that binary" are different questions.** Four reproduction campaigns were run against the right binary and found nothing, because re-running it does not reproduce a race between two of its tests at the density needed. Reading what else that binary contains took one grep.

**A live `Snapshot` does hold the directory lock**, which is worth knowing independently: five descriptors remain open after `drop( db )` and a reopen is refused until the snapshot drops. That is by design -- `Snapshot` and `ReaderSlot` are the only things outside tests that clone `Arc< DbInner >` -- and it means the entry's leading suspect was genuinely reachable. It was simply not what happened, because the failing test takes no snapshot before its drop and no production path in `db/mod.rs` takes one at all.

One exposure remains and is stated rather than fixed: a child that forks and never `exec`s holds the lock indefinitely, and the retry only delays the report by 45 ms. That is correct -- there really is a holder -- but an operator reading `AlreadyOpen` should know a stray child can be the one holding it.

## 2026-10-14 — A restore is 92% holes, and the WAL walk was serial for a reason that did not apply

Two TODO entries were parked on the same measurement and said to take them together: `where-follower-boot-time-actually-goes-is-unmeasured` and `the-restore-path-makes-blocking-core-calls-inside-async`. Taken together, the measurement closed the second, inverted the first, and found a third thing neither had named.

### The construction

A scratch scenario under `.agents-workspace/tmp/` ( not admitted to `e2e/scenarios/`: its assertions are about timings, not about the crate ) launches a leader, starts the shipped sidecar against a `file://` archive, checkpoints to root a base, commits 200 transactions of 32 keys across 4 shards, waits until the restore window stops advancing, and then restores to the tip through `srv_restore`, which is `restore_to` in process. `restore.rs` was instrumented with stage timers, measured, and the instrumentation removed; the original was kept in the scratch directory and copied back, so what shipped was re-derived from the pristine file rather than un-edited.

### The split

Four shards, 6 401 keys, 8 000 WAL objects of retained history, release build, mode of five runs:

| stage | before | after |
|---|---|---|
| base selection | 105 ms | 110 ms |
| materialize base | 4 150 ms | 3 090 ms |
| assemble WAL | 280 ms | 250 ms |
| plan and cut | 1.1 ms | 1.1 ms |
| recovery replay | 7 ms | 6 ms |
| `sync_tree` | 5 ms | 5 ms |
| **total** | **4 500 ms** | **3 470 ms** |

**Materializing the base is 92% of a restore, and the base is almost entirely holes.** A shard image is grown to a whole 1 GiB mmap segment by `set_len`. Measured on the leader: 1 and 2 MiB allocated against an apparent 1 GiB each. Measured on the restored copy, before: **1 GiB allocated each, 4.00 GiB in total**, for a database holding a few hundred kilobytes of state. The restore was writing back every zero.

`LTM/network-service-replication-and-operations.md` already carries the rule as policy -- "Do not materialize sparse holes on the wire or in follower storage" -- and the replication bootstrap path implements it in both directions. The archive restore path violated it in both. Setting the length first and writing only non-empty 1 MiB windows takes the restored allocation to **5.0 MiB**, 818x less, and the stage to 3.09 s. The transfer is unchanged and that half is filed as `the-archived-base-object-still-carries-its-holes`: the archive object *is* the file, its `size` and `crc32c` are the file's, and shipping only the non-empty runs is a `SCHEMA_VERSION` change rather than a write-side one.

### The entry that asked the right question about the wrong line

`the-restore-path-makes-blocking-core-calls-inside-async` named one candidate and insisted on measuring it first. It was right to: over 8 000 objects, `append_frames_at` totalled **10.5 ms** and `sync` **24 ms** against a restore of 8 461 ms. **0.4%.** `spawn_blocking` around those writes was not worth the `'static` closure it would need, and the entry is closed as not-taken rather than as done.

What the same instrumentation showed was that the *fetches* those writes were stalling totalled 2 017 ms -- and that they were serial within a shard. The 2026-10-14 entry that parallelised the other four archive fetch loops had written of this one that "the next object is selected by the *current* fingerprint, so there is nothing to overlap there". **That is true of the descriptor chain and irrelevant to the bodies.** `load_wal_descriptors` fetches a descriptor for every object in the retained history before the walk begins, so the chain is resolved entirely from memory and the whole fetch order is known up front. Only the bodies cost round trips.

`chain_objects` now resolves each shard's chain as a pure function, and every shard's bodies go through **one** `buffered( ARCHIVE_FETCH_CONCURRENCY )` rather than one per shard. One shard, 2 000 objects, `file://`: **770 ms to 240 ms, 3.2x**. At four shards it is 280 to 250 ms, because four-way concurrency was already there and a local store has no latency left to hide -- which is the direction that matters, since a network store has more to gain, not less.

Resolving the chain first paid twice more. A forked or short history is reported **before a byte is downloaded**. And the per-step `filter` over the shard's whole listing was **quadratic in the retained history** -- 8.5 ms of pure comparison at 2 000 objects, 126 ms at 8 000 -- for a walk that visits each object once; it is an index lookup now, and a cycle in the listing is bounded and reported rather than walked for ever.

### Two notes on method

**The first run said the fix was a 2.7x regression.** Materializing the base went 7.0 s to 19.2 s. It was a debug build: the all-zero scan is a byte loop, `-O` vectorizes it and `cargo run` without `--release` does not. The `Run \ Array` probe earlier the same day taught variance within a process; this one is about the profile the measurement is taken in, and the same discipline caught it -- repeat, and do not conclude from a pair of runs. In release and warm, baseline materialize still ranged 4.09 to 5.89 s across five runs, so a single pair could have been made to show anything.

**An entry that says "measure first" can be right about the method and wrong about the target.** This one was. Its structural reasoning -- one task, a blocking call stalls the others -- was correct and checkable, and the measurement it demanded is what found the real defect sitting beside the one it named. The value was in the instruction, not in the hypothesis.

## 2026-10-14 — The other half of the holes: 4.00 GiB of archive objects became 5.01 MiB

Earlier today's entry fixed the restore side of the base-image holes and filed the rest as `the-archived-base-object-still-carries-its-holes`, on the grounds that the archive object *is* the file and narrowing it is a format decision. Taken now. Measured on the same construction -- four shards, 6 401 keys, 200 commits of retained history, a `file://` store, a release build, four runs per arm with the first discarded:

| | before | after |
|---|---|---|
| base objects in the store | 4 294 975 488 B ( 4.00 GiB ) | 5 251 072 B ( 5.01 MiB ) |
| materialize base | 3.11 s | 0.24 s |
| assemble WAL | 0.25 s | 0.25 s |

**818x fewer bytes stored and transferred, and 12.9x off the stage that was 92% of a restore** -- on a local store, where there is no network latency to hide and the "transfer" is a page-cache read. A real object store has more to gain, not less.

### The decision the entry was parked on, and why it went the other way

The entry said the fix needed "a representation, a `SCHEMA_VERSION` bump ( currently 2 ), and a read path that still accepts every existing archive". The representation is a list of extents on `pb::ArchiveFile` plus a `sparse` flag; the version bump turned out to be exactly the wrong move, and the reason is already written in this tree.

`BaseManifest.checkpoint_time` carries the precedent verbatim: it is "deliberately NOT part of `history_anchor` -- adding a field to that hash would change the fingerprint of every existing history and break chain continuity at the sidecar's next reconnect." The same is true of the schema number itself, more strongly: `history::base_anchor` hashes `SCHEMA_VERSION` into the anchor, and every `validate_*` compares it with `!=` rather than a range. **Bumping it to 3 would refuse every archive in existence and invalidate every fingerprint derived from one** -- the precise opposite of "a read path that still accepts every existing archive".

So the extents are additive and uncommitted, and `size` and `crc32c` keep their meaning: the **file's** length and checksum, holes included. That is what makes the change safe rather than merely compatible. The anchor still commits to content; a restore still verifies the file it reconstructed, not the bytes it received; and an extent list that disagrees with the file it was derived from fails the CRC instead of quietly restoring something else. The encoding is canonical -- runs ascend, carry bytes, stay inside the file, and are separated by a real hole -- so adjacent runs are a writer bug and are refused rather than accepted.

**The one direction that does not hold is the old one.** A release from before this change, pointed at a base stored without its holes, refuses it on object length: "base object X is N bytes, manifest says M". A loud refusal, not a wrong restore, and the same outcome a schema bump would have produced at a hundredth of the cost. Written down in `docs/operations/backup.md` beside the version-2 paragraph, because it is the kind of thing an operator finds out during a recovery otherwise.

### What is left of the stage is the checksum over bytes that were never sent

The manifest's `crc32c` is the whole file's, so a restore has to hash holes it did not receive. Measured, `crc32c` over zeros runs at 16.0 GiB/s on this host: 250 ms for 4 GiB. Measured end to end, the stage after the change is **0.24 s**. The materialize stage is now, to within noise, *exactly* the cost of hashing the absent zeros, and nothing else.

That makes the next move obvious and also makes it not worth taking. A GF(2) shift of the CRC polynomial -- the `crc32_combine` trick -- would turn the stage from O( what the database appears to hold ) into O( what it holds ). It saves a quarter of a second off a one-off operation and has to be proved correct. The loop over a zero buffer stays, with the reasoning next to it, so a later session with a genuinely enormous apparent size knows what the lever is.

### The harness was checking the thing that changed

`inspect_archive` in `yesno-e2e` asserted `size != file.size` on every base object. It would have failed every archive scenario the moment the first sparse object was written -- which is the gate working, but it is also the measurement: the harness now reports `base_apparent_bytes` and `base_object_bytes`, and `archive.py` asserts the objects are under a sixty-fourth of the apparent length. Measured there, a two-shard database at 2 147 491 840 B apparent against 3 153 920 B stored: **681x**, so the threshold has two orders of magnitude of headroom and still fails the day someone ships the holes again. Shipping them is *correct*, which is exactly why only a scenario notices.

### Not taken

`SEEK_DATA`, again, and for the third time with the same reasoning: it would let the upload skip *reading* the holes as well, and it needs a `libc` call and the unsafe rules that come with it to save a scan whose cost is memory bandwidth on a one-off operation. The leader still reads its own gigabyte per shard; the store no longer stores it. If a leader's CPU during a base upload is ever shown to matter, that is the note to revisit.

## 2026-10-14 — A plan for external slab placement, and the consumer shape that asked for it

**Status: PROPOSED, nothing started.** A storage-format change behind a feature bit, in four
phases. Phase 0 needs no `yesno-core` change at all and pays regardless of the open question;
phases 2 and 3 bet on a number nobody has measured yet. Read the phase boundaries as decision
points, not as a schedule.

### What this is for

A consumer runs yesno as a genuine key-value store carrying **60 MiB and more per key**. Three
consequences follow immediately, and the first one is a correction to a result from the same week.

- **Their images are dense, so the sparse-object work of 2026-10-14 buys them nothing.** That
  change was 818x against *holes*. A database whose keys are 60 MiB has no holes worth skipping;
  its base is real bytes and its restore is bandwidth-bound. See
  `backup-archive-and-pitr.md`. The restore split measured there -- materialize base at 92% --
  holds for them for a completely different reason, and is not improved by that fix.
- **They are near the format's ceiling rather than comfortably inside it.** `ChunkRef.cell` is 40
  bits ( 1 TiB ), but `PageId` is `cell / 64` as a `u32`, which caps index-node addressing at
  256 GiB, and the documented working cap is 128 GiB per shard. At 60 MiB per key that is roughly
  **2 100 keys per shard**. Shard count is the only lever today.
- **A key is a small number of byte ranges.** At bitmap density a 60 MiB key is about 7 700 chunks
  of 8 KiB, allocated through the size-class ladder out of 2 MiB slabs: **about 30 slabs**, and
  consecutive ones when the key was ingested in one pass. Its index is roughly 20 bytes per chunk,
  so **the index is about 0.25% of the key's bytes**. That ratio is what makes everything below
  possible; without it there would be no small eager core to materialize.

### What the format is today

Stated so the plan can be checked against it rather than against memory.

- One file per shard, `shard-NNNN.yno`, holding one flat byte address space. `ChunkRef` is exactly
  8 bytes and every bit is contested: `[0:40) cell`, `[40:56) card_m1`, `[56:58) kind`,
  `[58:59) inline`, `[59:60) enc` and `[60:64) reserved`. The last five bits are **refused when
  non-zero**, deliberately, so a reader cannot mistake a future format for a valid file.
- Mapped read-only in `SEGMENT_SIZE` ( 1 GiB ) segments, one `Arc<MmapSegment>` each. **I6**:
  segments are never recreated, never unmapped while a `Buffer` may point into them, and the file
  never shrinks. Truncation under a live mapping is `SIGBUS`, which is not catchable as a `Result`.
- **I2**: the mapping is `PROT_READ` and every write is a `pwrite` on a separate descriptor, because
  containers hand out slices aliasing the mapping and writing through one of those is undefined
  behaviour rather than a torn read.
- Space is returned by `fallocate( PUNCH_HOLE | KEEP_SIZE )` per slab, never by truncating. The
  mechanism for turning a present region back into a hole therefore already exists.
- Allocation is by size class into `SLAB_SIZE` ( 2 MiB ) slabs. Each slab reserves a `SLAB_META`
  region holding its class, generation, used count and occupancy bitmap under a CRC. That region is
  explicitly **a cache of derivable state** -- `fsck::rebuild` recomputes all of it from the index --
  which is what makes it safe to write in place.
- `SuperBlock` carries `feat_required` and `feat_compat`. `feat::SUPPORTED` is **0**, and `decode`
  refuses any file whose `feat_required` names a bit this build does not implement. The mechanism is
  live and tested and nothing uses it yet, so the first real feature bit costs no new machinery.

### The decision: where the indirection goes

Three places could carry "this payload is not at that offset in this file".

**In `ChunkRef`.** Five spare bits exist. Rejected. The index dominates total size in the sparse
regime, which is why those bits were left reserved rather than spent; and per-chunk relocation is
more flexibility than the workload needs while costing more -- moving a chunk rewrites an index
entry, which shadow-pages its leaf and every node above it.

**In the segment.** 1 GiB granularity. Rejected: it is the mmap unit, and a 1 GiB fetch is not a
lazy fetch of a 60 MiB key.

**In the slab.** Taken. The slab is already the allocator's unit, already the unit `punch` operates
on, and already the unit `slabmeta` describes. Relocating a slab touches one placement record and
**no index entry at all**, because the cell arithmetic -- `slot_of_cell`, `trailer_offset`, the
ladder, `verify_once`, the extent trailers -- is untouched. The 40-bit cell stops being a file
offset and becomes a **virtual** address; a placement table says what backs each 2 MiB of it.

That is the whole idea. Everything below is consequence.

### Shape: the shard file becomes a cache over backing objects

A slab is in one of three states:

- **local** -- its bytes are at `cell` in `shard-NNNN.yno`, which is exactly today's behaviour and
  stays the default;
- **external, present** -- its bytes are *also* at `cell` in the file, materialized from a backing
  object;
- **external, absent** -- the range in the file is a hole, and a placement record says which object
  and offset hold it.

**Materializing is a `pwrite` into a hole, which is the exact inverse of `punch`.** That is what
keeps I2 and I6 intact: the mapping stays `PROT_READ`, the file never shrinks, no segment is
remapped, and the length was already final because `set_len` establishes it before anything is
written. Eviction is `punch` plus clearing presence, and it must respect `any_pinned_in` -- which is
reclamation condition 3, already implemented and already load-bearing.

**Presence must be derivable, not new durable state.** An absent slab reads as zeros, and zeros are
what a corrupt slab also looks like to `verify_once`. Rather than add an authoritative presence
bitmap -- which would have to be crash-safe and would make a half-materialized image
indistinguishable from a damaged one -- derive presence from the slab's own `SLAB_META`: write the
body first, fsync, then the 8 KiB meta. A partial fill leaves a meta that fails its CRC or
disagrees with the placement record's generation, and is simply re-fetched. Fetches are idempotent,
so a crash costs a re-fetch and nothing else. **Do not invert that write order**, and do not let a
reader consult `verify_once` before consulting presence.

**The placement table is the only new durable state, and it is authoritative.** Nothing can derive
which object backs slab *k*. One record per slab -- a `u16` object id into a small object-name table
plus a `u48` offset is 8 bytes -- so a 128 GiB shard costs 512 KiB. It is rooted in the superblock
beside the index root, shadow-paged and published by the same checkpoint, under **I4**: the
placement a checkpoint publishes must be the placement its index entries resolve against.

**The feature bit.** Define `feat::EXTERNAL_SLABS`, add it to `feat::SUPPORTED`, and set it in
`feat_required` **only when a shard actually has a non-local slab**. A fully local shard writes the
file it has always written, byte for byte, and an older binary opens it unchanged. One that does not
is refused at `decode` with the existing message rather than misread. This is the same shape as the
archive extents of 2026-10-14: additive, and inert when there is nothing to say.

**`yesno-core` cannot fetch.** The lean-core budget is five direct dependencies and no tokio, so the
fetch is a trait the server supplies -- synchronous, blocking, one range at a time -- and the server
implements it against its runtime. **This is a latency-contract change, not plumbing**: a query
touching an absent slab blocks inside the store on network latency. It must be opt-in per database
and it must be single-flighted per slab, or a hot cold-key is fetched once per concurrent reader.

### Phases

#### Phase 0 -- slab-granular, content-addressed archive objects. No core change.

Generalize `pb::ArchiveFile` so a file's `extents` may each name their own `object_key` and
`crc32c`, rather than all coming from one object. Cut a base file on `SLAB_SIZE` boundaries and key
each object by the content hash of its bytes.

This is a straight generalization of the sparse-extent work already shipped, it touches only
`yesno-server-utils`, and it buys three things immediately:

- **Base generations become incremental.** Consecutive checkpoints of a 60 GiB database share
  almost every slab, so generation *N+1* uploads only what changed and its manifest reuses *N*'s
  object keys. `gc.rs` already reclaims by reachability over retained manifests, so "keep an object
  any retained manifest names" is the rule it already implements.
- **A restore fetches ranges in parallel** at `ARCHIVE_FETCH_CONCURRENCY` instead of one stream per
  file, which is where a remote store's per-stream bandwidth cap currently binds.
- **A restore becomes resumable**, because an object either arrived whole and verified or did not.

**This is the phase that answers the consumer's original question**, and it answers it in a form
nobody proposed. "Incremental base retrieval" was refuted on 2026-10-14 as *laziness* on a database
holding a few hundred kilobytes. At 60 MiB per key the right reading of "incremental" is
**deduplication between generations**, and that one is correct.

#### Phase 1 -- the placement table, with everything still local.

Add the table, the feature bit, and the virtualization of `cell`, with every slab written as
`local`. Nothing changes observably; the table is written, read back, checked by `fsck`, and proved
inert. Ship it on its own so that the format change and the behaviour change are separately
bisectable.

Needs: a round-trip test at a non-trivial slab count, an `fsck` mode that validates placement
without fetching, and a sabotage test that a placement disagreeing with a slab's own meta is
reported rather than followed.

#### Phase 2 -- the fetch seam.

The `SlabSource` trait, single-flight, presence derivation, and the body-then-meta fill order.
Still no policy that makes anything external: the seam is exercised by a test source that serves
from a second local file.

#### Phase 3 -- the two policies, which are separate decisions.

- **Lazy materialization on restore and bootstrap**: bring up a follower on superblock, index,
  placement and WAL -- about 0.25% of the base -- and fetch payload slabs on touch or in the
  background. This is the one that bets on the working set.
- **Tiering**: let a cold slab live *only* in the object store. This is a different and larger
  decision, because it means the database directory is no longer the database. `du` stops being the
  answer to "how much data is this", a filesystem snapshot stops being a complete backup, and the
  retention of the external store becomes a durability dependency. **Do not let phase 3 merge the
  two.** Lazy materialization is a cache with a complete origin; tiering moves the origin.

### What has to be decided before phase 1

- **The per-shard cap.** At 60 MiB per key a 128 GiB shard holds about 2 100 keys. If that is the
  real deployment shape, `feat::LARGE_SHARD` and the `u32` `PageId` need a plan of their own, and it
  should be made before a second format change rather than after.
- **Whether the placement table is per shard or per database.** Per shard keeps it beside the
  superblock it is published with; per database would let a slab move between shards, which nothing
  needs today and which would break the shard-id guard.

### What has to be measured before phase 2

**The fraction of keys a follower touches in a window.** Lazy materialization turns a multi-minute
boot into seconds if the working set is small, and adds a network stall to every first read while
moving exactly the same bytes if it is not. This number is a property of the consumer's workload and
cannot be derived here.

Secondarily, and obtainable here: **the restore split at their shape** -- dense, 60 MiB keys -- to
confirm `materialize_base` is dominant for them and to size it in bytes per second rather than in
percentages. Generating that corpus needs a native ingest fixture driving the server, because
`flight_put` goes through monty and a scattered half-billion-ordinal key is not expressible there.

### Do not

- Do not spend `ChunkRef`'s reserved bits on this. They are refused-when-set on purpose, and the
  slab is the right granularity.
- Do not reach for `userfaultfd` or FUSE. The page fault is the wrong hook at this granularity, and
  this tree has declined `libc`-level filesystem calls three times for smaller gains.
- Do not land the placement table without the `fsck` path and the feature bit. Eight pieces of
  machinery in this codebase were built to spec and wired to nothing.
- Do not make a slab external and a slab absent the same concept. The first is a placement, the
  second is a cache state, and conflating them is what makes a half-materialized image
  indistinguishable from a corrupt one.

### Adoption: the client stages the bytes, the server takes ownership of them

A companion to the above, wanted by the same consumer shape and for the same reason. Writing a
60 MiB key through the ordinary path pushes those bytes over Flight, into the memtable, through the
WAL and then into the slab store -- several passes over data the client already has in hand.
**Adoption** lets the client put the object into an object store itself, hand the server a *key*
rather than the bytes, and have the server take the object over.

#### What is adopted, and why it can be a client's bytes at all

A staged object is **Roaring-serialized** or **raw bits**, at the client's choice, and the choice
is about the data rather than about convenience -- see the subsection below. What it is *not* is an
image in yesno's internal layout. A client that would have to reproduce the size-class ladder and
the extent trailers could not, and should not be asked to: `SuperBlock::decode` refuses a geometry
this build does not implement, so a client built against a different ladder would stage objects
the server rejects.

Roaring is a legitimate input rather than a lucky one because `roaring_format` round-trips `OrdSet`
and `tests/differential.rs` asserts **byte-level identity** with the `roaring` crate. That property
was built for importing `.roaring` files at `O( container count )`, and adoption is the same
import arriving from an object store instead of a path.

**The bytes are untrusted, and for Roaring the contract that makes that survivable already
exists.** `container::codec::decode` is a fuzz target by contract: for *any* input it returns
`Err` or a container satisfying its invariants, and never panics. Adoption is the first production
path where that contract faces genuinely adversarial input rather than a corrupted file, so it is
the reason to keep it rather than a new obligation. Raw bits need no such contract -- any 8 192
bytes is a valid bitmap -- which moves their whole risk into the framing the request declares.
Either way the staged object is consumed a chunk at a time: the point is not to hold 60 MiB, let
alone a larger key, in the server's heap.

#### Two staging encodings, because one of them is wrong for each half of the workload

**Roaring for sparse or mixed, raw bits for dense.** They are not interchangeable and the server
should accept both rather than making every client pay for the one that suits the other. A client
holding a thousand scattered ordinals must not be made to stage a mask; a client holding a dense
60 MiB key already *has* the bits and should not be made to serialize a container directory around
them, only for the server to take it apart again.

**Reuse the vocabulary that already exists.** The Flight surface names exactly this choice for
results -- `SetWire::Ordinals`, `Containers`, `Bitvector`, with an unknown choice refused rather
than silently answered as ordinals. Ingest should use the same names and the same refusal, so a
client that has already chosen a wire form for reads does not learn a second taxonomy for writes.
A sorted `u64` array is the obvious third form and `from_sorted_u32` is already there; it is not
proposed here only because nobody has asked for it.

**Raw bits have to be framed, and the framing belongs in the request.** A Roaring object is
self-describing; a naked bit array is not, so the base ordinal, the length and the word order have
to come from somewhere. Put them in the `adopt` message, not in a header on the object. A header
would make the staged object a new file format to version, fuzz and document, and it would destroy
the one property that makes raw bits worth having -- that a client can stage a memory-mapped
bitset **verbatim**, with no serialization step at all.

The framing is therefore pinned by the protocol rather than by the bytes:

- `base` is the first ordinal and **must be chunk-aligned** ( a multiple of 65 536 ), so a staged
  window is a whole number of chunks and chunk *i* is at byte offset `i * 8192`.
- The object's length must be **exactly** `chunks * 8192`. Not at least -- exactly; a trailing
  partial chunk is a framing error, not a short last container.
- Words are **little-endian `u64`**, bit *i* of word *w* being ordinal `base + 64 * w + i`. This is
  the in-memory bitmap container's own layout, which is what makes the copy a `memcpy`, and it is
  the same layout `unstable_arrow::bitmap_words` already lends to consumers. A big-endian host
  refuses rather than byte-swaps, as the superblock's endian field already does.
- The window is bounded by configuration. `Bitvector` refuses a result window above 16 384 chunks
  ( 128 MiB ) rather than clipping it, precisely because an unbounded sparse key could otherwise
  ask for a two-pebibyte mask; ingest needs the same bound for the same reason. A 60 MiB key is
  about 7 680 chunks, comfortably inside it.

**Raw bits are not "already in our format", and assuming they are is the trap.** They are the
*uncompressed input to kind selection*. A window staged as bits may well contain chunks that
should be stored as arrays or runs, and storing every one of them as a bitmap would make an
adopted key several times larger than the same data written through the ordinary path. So the
server runs the crate's own selection -- `ARRAY_MAX`, `BITMAP_DEMOTE`, `RUN_MAX_INTERVALS`, the
thresholds whose rationale `lib.rs` says not to touch -- and the wire form does **not** dictate the
stored kind. An all-zero chunk is dropped entirely rather than stored empty.

**Validation for raw bits costs one pass and no decode.** Any 8 192 bytes is a valid bitmap
container, so there is nothing to reject on structure; what the server must compute is the
popcount per chunk, which is what fills `card_m1` and what feeds the selection. That popcount runs
in the same pass as the CRC, so the whole validation of a 60 MiB object is one vectorized read.
Roaring is the opposite: cheap to lay out, but every container must go through
`container::codec::decode`, and the budget there is that it never panics on any input.

**Only Roaring can be adopted without a transform.** Phase C below keeps the client's object as
the payload, which needs a directory the payload carries -- Roaring has one. Raw bits can be blobbed
too, and more easily, since chunk *i* is simply at `i * 8192` and no directory is needed at all,
but it is admissible **only when the selection pass says every chunk in the window would have been
a bitmap anyway**. Otherwise blobbing raw bits is just the bloat above with a longer lifetime.

#### The ordering decision, which is the whole of the design

The scenario as posed is **commit first, materialize later**: the server accepts the key, the
create-or-update travels through the WAL, and the staged object is dropped once that operation
reaches the slab store. That ordering buys the lowest possible client latency and costs a property
the database currently has unconditionally.

**Between the commit and the materialization, the WAL names data that is not in the database.**
Every consequence follows from that one sentence:

- A crash in the window means recovery must re-read the staged object, so it has to outlive the
  *request* by as long as the WAL retains that record -- which is a retention question, not a
  request-scoped lease.
- An **archive restore** replays WAL from a base root. If a retained root predates the
  materialization, restoring through it needs the staged object too. So the staged object becomes
  reachable state that `gc.rs` has to account for, on the same footing as a WAL object.
- A reader between the two points either cannot see an acknowledged commit, or must fall through to
  the staged object -- which needs the external-placement read path and makes the staging store a
  **serving** dependency.

**Recommended instead: materialize, then commit.** Copy or transform the staged object into the
server-managed location first, verifying as it goes, and only then append the WAL record -- which
names the managed object, never the staged one. The client's `adopt` call blocks for the copy;
in exchange the database is self-contained at every instant, the staging lease is request-scoped,
`gc.rs` is untouched, and restore semantics do not change at all. When staging and managed prefixes
share a bucket and no re-layout is needed, that copy is a server-side `CopyObject` and no bytes
cross the server.

The copy is **not** free when the layout must change -- a Roaring container order is not a slab
order -- so tier one costs the server one read of the object. That is still far better than the
ordinary path, which costs several. **If the lower-latency ordering is wanted anyway**, the price
is explicit and should be paid deliberately: the staged object joins the reachability set, its
lease becomes server-renewable rather than client-held, and the archive's retention has to keep it
until every retained base already contains the materialized slabs. Do not take that ordering by
default, and do not take it before the reachability work is done.

#### The lock

The lease this needs has been written in this tree once already. `ArchiveWriterLease` plus
`lease.rs` is a conditional, compare-and-swap, TTL'd ownership record over an object store, and
`archive.rs` already drives `PutMode` and `UpdateVersion` for exactly that. Reuse the shape: a
lease object beside the staged object, owner id, expiry, CAS on every transition. It answers both
directions -- the client proves nobody else is writing the object it is about to hand over, and the
server proves the object did not change between validation and adoption.

**Constrain the key, do not trust it.** The client names a path *within* a server-configured
staging prefix, never a bucket, a URL or an absolute key, and the server reads it with its own
credentials. Without that rule the server is a confused deputy that fetches whatever it is told.
The tree already has the instinct in two places -- `download_base_file` refuses a manifest file
name that is not flat, and the e2e harness refuses a scenario path containing a separator -- and
this is the same check. Size is declared in the lease and bounded by configuration, because an
unbounded staged object is an unbounded server-side read.

#### Phases, and the first one needs no format change

- **Phase A -- ingest from an object key.** `adopt( key, staged_name, encoding, lease, content_hash )`:
  the server reads the staged object, validates it a chunk at a time -- decode for Roaring,
  popcount and kind selection for raw bits -- performs an ordinary bulk insert, commits, then
  releases the lease and deletes the staged object. No new format, no
  external placement, no change to recovery -- the bytes land exactly where an ordinary write would
  have put them. This alone removes the Flight hop and the client-side serialization round trip,
  and it is the honest first step because it can ship before any of the slab-placement phases.
- **Phase B -- adopt into managed objects.** Once phase 1 and 2 of the slab plan exist, the
  transform writes slab-formatted objects into the managed prefix and the WAL record carries their
  placement, so a large key is never resident in the server at all.
- **Phase C -- adopt without a transform.** Keep the client's Roaring object as the payload and
  reference it as a blob, which is what `ChunkRef`'s `enc` bit and `feat::ALT_ARRAY_ENC` were
  reserved for and what `per-key-blob-for-cold-keys` sketches. This makes adoption a pure
  server-side copy and unifies three filed items -- at the cost that entry already records: a
  blobbed key cannot carry per-chunk `card_m1`, so its `cardinality()` stops being free and becomes
  a payload read. Decide that deliberately; it is a headline property of the index.

#### Hazards worth naming now

- **Idempotency.** A retried `adopt` must not insert twice. The natural key is the content hash
  **and the declared framing**, not the hash alone: raw bits take their base and length from the
  request, so the same bytes staged at a different base are a different write and must not collide
  with the first. This is the same gap `no-idempotency-key-on-the-write-path` already records;
  solve them together rather than inventing a second mechanism here.
- **Orphans.** A staged object whose `adopt` never arrives is reclaimed by lease TTL, not left
  forever. `reap_stale_roots` is the precedent for sweeping abandoned staging state.
- **Partial adoption.** A transform that fails halfway must leave neither a partial key nor an
  orphaned managed object. The existing shape applies: write the managed objects, then the WAL
  record, then release -- so a failure before the WAL record leaves only unreferenced objects,
  which the same reclamation sweep removes.
- **Do not let `adopt` bypass the visibility rules.** It is a write like any other: it takes a
  version, it is ordered by the commit clock, and I9's non-decreasing commit time applies to it.
  An out-of-band ingest that skipped the commit clock would break wall-clock recovery targets for
  the whole database, not just for the adopted key.

### Self-review the same day, against the code rather than the prose

Three of the conclusions above are wrong and are corrected here; two numbers were stated more
firmly than they were derived; three constraints were missing. **Where this section and the text
above disagree, this section is the later word.** The central decision survives -- the slab is the
right indirection point and `ChunkRef`'s reserved bits should stay reserved -- and phase 0 comes
out stronger than it was argued, for a reason the plan failed to credit.

#### Corrected: presence cannot be derived from slab metadata while `fsck::rebuild` exists

The claim was "**the placement table is the only new durable state**", resting on deriving presence
from each slab's own `SLAB_META` under a body-then-meta write order. That does not survive the path
that makes `slabmeta` safe to write in place in the first place. **`fsck::rebuild` recomputes slab
metadata from the index** -- that is its whole purpose -- so run against a half-materialized image
it writes a valid meta for a slab whose body is absent. Presence then reads true, the bytes read
zero, and every chunk CRC in that slab fails.

So the no-new-state conclusion is **not established**. Either `rebuild` becomes placement-aware and
refuses to synthesize a meta for a slab the placement table calls absent, or presence gets an
explicit durable home of its own. The second option was dismissed above on the grounds that it
"would have to be crash-safe and would make a half-materialized image indistinguishable from a
damaged one"; the first option has to solve exactly that problem anyway, so the comparison was not
the one I drew. **Decide this before phase 2, not during it.**

#### Corrected: a fill breaks I2's letter rather than preserving it

The claim was that materializing a slab "keeps I2 and I6 intact" because it is a `pwrite` into a
hole. I6 is fine -- nothing shrinks and no segment is remapped. **I2 is not.** Its text is
"mapped pages are read-only and immutable once published", and the discipline behind it is
write-then-publish: a checkpoint writes into free space and publishes an index pointing at it
afterwards, so no reader can reference bytes still being written. A fill inverts that order. The
index entry is **already published** and the bytes arrive later.

The rule has to be restated as **immutable once *present***, with the presence flip as the
publication point for the bytes. That covers readers arriving through a `ChunkRef`. It does not
cover paths that read **by offset rather than by reference**, and the plan named only one of them:

- `fsck`, which the plan does mention;
- **`verify_once`'s backward invalidation scan**, bounded by `MAX_VERIFIED_SPAN`, which the plan
  does not. It reaches below a write to find cached entries that overlap it, and that reach can
  cross into an absent slab.

The mirror-image guard already exists and should have been cited: `punch` is gated on
`any_pinned_in`, which is reclamation condition 3. A fill needs its counterpart. And because I2
lives in a `//!` block in `segment.rs`, amending it is **part of this work** rather than follow-up,
by the rule that a stale rationale comment is worse than none.

#### Corrected: a 32-bit CRC cannot be a content address

Phase 0 says to key each slab object "by the content hash of its bytes" while proposing to reuse a
manifest field named `crc32c`. Taken literally that is a silent-corruption bug. **Thirty-two bits
collide by birthday at about 65 536 objects**; a 60 GiB database is roughly 30 000 slabs and a
retention window holds several generations of them, so a collision is reachable in ordinary
operation -- and its consequence is serving the wrong slab, not an error. The address must be the
**SHA-256** `history.rs` already computes for every fingerprint in this format. A per-range
`crc32c` may stay beside it as the transport check it already is; it is not a name.

#### Corrected: `gc.rs` would leak every slab object, and that is the safe failure

`classify` requires `db/<uuid>/term/<term>/{base|wal}/...`. A content-addressed prefix outside the
term tree falls to `Kind::Unrecognized`, which is **kept and reported** rather than deleted, so
phase 0 as written reclaims nothing at all until `classify` learns the shape. Mandatory work, and
the failure direction is the right one.

**The dedup claim itself checks out.** `Kind::BaseFile` is deleted on `!keep_files.contains( key )`,
and `keep_files` is the union of every retained manifest's object keys -- so an object two
manifests name survives the pruning of either. No change needed there.

**One thing to record rather than assume**: a content address is deliberately **term-independent**,
while every key shape in this archive is term-scoped on purpose, so that a promoted timeline cannot
overwrite an older timeline's identically numbered objects. Identical bytes make sharing across
terms safe, but that is a reasoned exception to a stated invariant and has to be written down where
the invariant is.

#### Added: what dedup is actually keyed on, and the allocator policy that makes it work

Unstated above: dedup granularity is **byte-identity of a whole 2 MiB slab**, not "payloads
unchanged". A slab with a single newly allocated slot is a new object, because its occupancy bitmap
and trailer table moved.

What rescues the hit rate is a policy the plan did not credit. `Slab::new( class, generation )`
stamps the generation at **creation**, and `begin_generation()` only advances the allocator's
counter -- so an untouched slab's metadata does not churn from checkpoint to checkpoint. And
`alloc.rs` already keeps one checkpoint's allocations in a **contiguous slab run**, which there is a
test named for. Together those are what make slab-level dedup effective rather than theoretical,
and they are support for phase 0 rather than a caveat against it.

#### Derived: two numbers that were guesses

**The index ratio.** "Roughly 20 bytes per chunk, so about 0.25%" was not derived.
`CHUNKKEY_BYTES` is 14 and `ChunkRef` is 8, so an untruncated entry is 22 bytes -- but index leaves
store **truncated** big-endian suffixes, and across one key's consecutive chunks nearly the whole
`ChunkKey` is shared, so 10 to 12 bytes is likelier. The honest figure is a range,
**0.15% to 0.28%**, and every conclusion that rests on it holds across the whole range. The guess
landing inside the range was luck.

**Superblock headroom** was assumed. Checked: the fields end at offset 192 and the CRC sits at
`PAGE - 4`, so about 3.9 KiB of the page is free. A placement root fits comfortably.

#### Gaps now named

- **Slab granularity is right conditional on this workload**, and the plan presents it as right
  outright. A key with mixed container kinds spreads over several class-specific slab runs, and
  packed pages put small payloads in a shared class -- so a 2 MiB fetch can serve an 8 KiB need and
  drag in other keys' payloads, up to **256x read amplification**. Harmless for dense 60 MiB keys,
  bad for a sparse workload. This is a second and independent reason phase 3 must be opt-in per
  database; the plan argues that only from latency.
- **The fetch must not happen while holding a store lock**, which is the constraint that actually
  decides where the seam can sit. `verify_once` takes a per-shard mutex and the node reader takes a
  mutex plus an rwlock read; a blocking object-store call inside either stalls every reader of that
  shard. "A latency-contract change" understates it.
- **"The WAL record carries the placement" is unverified.** Nothing was checked about whether WAL
  record types are extensible or what replay would do with such a record. Read it as an assumption
  to test before phase B, not as a decision.
- Minor: phase A "removes the client-side serialization round trip" is true for raw bits and false
  for Roaring, which still has to be serialized by the client.

#### The content address: SHA-256, and why the choice is not a cryptographic one

Settled after asking whether BLAKE3 should be preferred. **It should not, for phase 0, and the
reason is not that SHA-256 is stronger.**

**Collision resistance does not separate them.** Both target **128-bit** collision resistance.
SHA-256 reaches it as the birthday bound on a 256-bit output; BLAKE3's security target is 128 bits
and **does not rise with output length**, so asking its extendable output for 512 bits buys nothing
against collisions. On the number they are identical and both are far past anything reachable.

What differs is confidence in that number, and there SHA-256 is the conservative pick: **64 rounds
with published collision work reaching roughly half of them, under continuous attack since 2001**,
against BLAKE3's **7 rounds** -- trimmed from BLAKE2s's 10 deliberately for throughput, which is the
design's principal criticism -- and five years of history on that particular parameterisation.
BLAKE3 inherits real pedigree through BLAKE2 and BLAKE, so this is a margin argument, not a doubt
about soundness. Any specific best-attack round count should be re-checked rather than quoted from
here.

**And the threat model makes the comparison moot.** Replacing an existing victim slab is a
**second preimage**, about 2^256 for both. The only 2^128 route is a chosen-prefix collision, which
needs the attacker to influence the victim's bytes as well as their own. Both resist. So the
content address is not where this design's security is decided.

**It is decided by scoping, which is a separate decision and has to be made.** A content-addressed
namespace shared across tenants means one tenant's upload can be served out of another tenant's
object, whichever hash is used -- that is what dedup *is*. Scope the address space **per database**
unless cross-tenant dedup is explicitly wanted, and note that choosing otherwise pre-decides part of
`tenant-and-identity-namespace-model`, which is still open.

So the tiebreakers are the ones that actually separate the two. `yesno-server-utils` already
depends on `sha2`, and every commitment in this archive format -- base anchors, per-shard WAL chain
fingerprints, the `FINGERPRINT_DOMAIN` prefix -- is already SHA-256. **`blake3` is absent from the
workspace entirely, not even transitively.** Reusing the hash the format already commits with beats
adding a second family for a speed difference that does not bind: a base capture to an object store
is network-bound, and SHA-256 with hardware extensions already outruns the upload. The three
recorded `SEEK_DATA` refusals are the same reasoning about the same one-off operation.

**Two warnings that follow.**

- **Do not truncate the address.** A 128-bit object name is 64-bit collision resistance, which *is*
  adversarially reachable. If key length becomes a problem, shorten the prefix structure around the
  address, never the address.
- **Build it the way `history.rs` already builds fingerprints.** SHA-256 is Merkle-Damgård and
  length-extendable; `field()` writes an eight-byte length before every variable-length value, which
  is exactly the mitigation. A content address over anything but a fixed-length byte range inherits
  that obligation.

#### If BLAKE3 is ever adopted, it is for the Merkle property and not for speed

BLAKE3's distinctive feature is that it is internally a Merkle tree, so it supports **verified
subrange reads**. That is precisely what a lazy fetch wants -- and the plan above designs it away by
cutting objects at slab granularity, so every object is 2 MiB and is verified whole.

The condition under which it stops being a preference and becomes the mechanism: **one object per
shard file, BLAKE3-addressed, with verified range reads**, instead of tens of thousands of slab
objects per generation. That trades dedup away for object count, and the count is not trivial --
60 GiB at 2 MiB is about **30 700 objects for the first generation**, and `gc.rs`'s `survey` lists
every key under the database prefix *and* decodes every manifest it finds, so the reclamation sweep
grows with it. If that sweep becomes the bottleneck, this is the alternative, and BLAKE3 is what
makes it possible. Do not reach for it before then.

## 2026-10-14 — Verdict on the slab plan: go on phase 0 and phase A, and the per-shard cap is promoted to a blocker

**Decision recorded, not new analysis.** The plan and its self-review are the two entries above;
this is the go / no-go over them, plus the one piece of it that was checked against the code
afterwards and came back more serious than the plan presented it.

### The verdict

**Go: phase 0** -- slab-granular, content-addressed archive objects. It touches only
`yesno-server-utils`, needs no `yesno-core` change, no feature bit, no format-version bump and no
invariant restated. It pays on its own ( incremental base generations, parallel range fetch,
resumable restore ) and, decisively, **it does not depend on the number nobody has measured**. The
dedup it buys is a property of how checkpoints write slabs, and the self-review confirmed the two
policies that make the hit rate real: `Slab::new( class, generation )` stamps the generation at
creation, and `alloc.rs` keeps one checkpoint's allocations in a contiguous slab run. If the rest
of the plan is never built, phase 0 is still correct work.

Three things are **mandatory inside phase 0**, not follow-ups: the address is SHA-256 and
untruncated; `gc.rs::classify` learns the new key shape before any such object exists, or the
archive leaks silently; the address space is scoped per database unless cross-tenant dedup is
explicitly wanted.

**Go: phase A** -- `adopt` from an object key, landing through an ordinary bulk insert. Also no
format change, also shippable alone, and it removes the Flight hop, which is the consumer's actual
complaint. Take **materialize-then-commit**. The commit-first ordering the scenario was posed with
is buildable, but it prices in reachability work in `gc.rs`, a server-renewable lease, and a
staging store that becomes a *serving* dependency; do not pay that before the cheap ordering has
been measured as insufficient. Idempotency is a precondition rather than a hazard, and it is the
same gap `no-idempotency-key-on-the-write-path` already records.

**No-go: phases 1, 2, 3, B and C.** Not rejected -- unready, and each for a stated reason. Phase 3
rests on the fraction of keys a follower touches in a window, which is a property of the consumer's
workload and cannot be derived here. Phase 2 carries an open correctness question the self-review
opened and did not close: presence cannot be derived from `slabmeta` while `fsck::rebuild` exists.
Phase B rests on "the WAL record carries the placement", which is an untested assumption about WAL
record extensibility.

### The per-shard cap, checked against the code

The plan listed this under "what has to be decided before phase 1". That placement was too mild.
Checked today:

- `PageId` is `u32` ( `index/tree.rs` ) and `PAGE_ID_SHIFT` is 64 ( `db/store.rs` ), so index-node
  addressing tops out at **256 GiB** of shard address space.
- **The cap binds on index-node placement, not on payload.** `ChunkRef.cell` is 40 bits, 1 TiB, and
  payload never goes through `PageId`. The sole conversion is `cell_to_page_id`, reached from
  exactly one caller, `ShardStore::append_node`.
- **There is exactly one enforcement site in the tree**: the `u32::try_from` in `cell_to_page_id`,
  which yields `CodecError::Invariant( "index page id exceeds 32 bits" )`. The failure direction is
  right -- an error, not a wrap, not corruption. What is wrong is *when* and *how predictably* it
  arrives. It surfaces out of a checkpoint, after the data has been accepted, and it fires when an
  index node happens to be allocated above the line rather than when the shard crosses a threshold.
  Because the index class's slabs are interleaved with payload slabs in allocation order, the
  crossing point is a function of allocation history, not of size. A database can sit above the
  line for some time and then fail to checkpoint.
- **128 GiB is a comment, not a check.** It appears in `extent.rs` and `db/store.rs` as the "cap"
  the 40-bit cell and the 32-bit page id are sized comfortably past. Nothing enforces it, and
  `docs/operations/index.md`'s operating envelope states **no capacity limit at all** -- so an
  operator has no way to learn the number before meeting it.
- `feat::LARGE_SHARD` is declared in `superblock.rs` and unimplemented; `feat::SUPPORTED` is 0.

At 60 MiB per key that is roughly **2 100 keys per shard** at the documented 128 GiB and about
**4 400** at the hard 256 GiB. Shard count is the only lever today.

### What follows, in order

1. **Make the cap explicit and early, and document it.** A refusal at open or at allocation with a
   number in it, plus a line in the operating envelope. This is cheap, independent of every phase
   above, and valuable whether or not external placement is ever built -- the present behaviour is
   an undocumented limit that announces itself as a failed checkpoint.
2. **Ask the consumer the shape question**: keys per shard and shards per database at their target
   size. The answer decides whether `LARGE_SHARD` is urgent or theoretical.
3. **Only then decide `LARGE_SHARD`.** If the deployment shape really is 60 MiB keys at volume, a
   plan for the 32-bit `PageId` belongs *before* a second format change lands on top of the current
   geometry, not after. This reorders the plan: the cap is now a blocker on phase 1 rather than a
   question to settle alongside it.

Item 1 does not wait on items 2 and 3, and should not be bundled with them.

## 2026-10-15 — The consumer's shape arrives: LARGE_SHARD is not needed, and phase 0 inherits a 16-44 MB manifest

The two numbers the verdict entry asked for came back. **Item 3 of that entry closes rather than
proceeds**, and item 1's cap work turns out to have been the right thing to build first for a
reason the entry did not anticipate: the cap is not binding, so what the consumer needed from it
was the *number*, not the refusal.

### What was asked and what came back

- **Total data per database: 500 GiB.**
- **Size distribution: 70% of values within 100 MiB, 20% within 250 MiB, 8% within 500 MiB.** The
  remaining 2% was not stated, and that gap is the only input that could still move the answer.

Note that this revises the working figure. The plan above was written against "60 MiB and more per
key"; the real distribution is heavier, with a mean around 115 MiB, so every per-key number in
those entries reads low by roughly 2x. The conclusions do not change sign.

### The derivation, so it can be re-derived rather than re-measured

Bands treated as uniform within each, the unstated 2% priced twice -- once with the tail running to
1 GiB, once to 4 GiB:

| tail to | mean key | sd | keys at 500 GiB |
| --- | --- | --- | --- |
| 1 GiB | 115 MiB | 139 MiB | ~4 443 |
| 4 GiB | 146 MiB | 355 MiB | ~3 508 |

**This database is three to four thousand keys.** Worth stating plainly, because it changes which
risk matters: at this object count the threat to a shard is routing variance, not volume.

Per-shard load is then the live share, plus three standard deviations of routing imbalance over
`keys / shards` draws, times 1.47 for space amplification, against the 256 GiB cap ( tail at
1 GiB ):

| shards | live/shard | +3 sigma | amplified | fraction of cap |
| --- | --- | --- | --- | --- |
| 4 | 125.0 GiB | 138.6 | 203.7 | 80% |
| 8 | 62.5 | 72.1 | 106.0 | 41% |
| 16 | 31.2 | 38.0 | 55.9 | 22% |
| 32 | 15.6 | 20.4 | 30.0 | 12% |

A 4 GiB tail costs about seven points of utilization at every shard count and changes no decision.

### The recommendation: 16 shards

Eight fits 500 GiB and is the configured default. **Sixteen is the advice**, because the shard
count is creation-only and 500 GiB is a target rather than a ceiling -- 16 absorbs roughly a
doubling before the cap is a conversation again. Four is refused: 80% of an unraisable cap on day
one, before the tail is known.

**`feat::LARGE_SHARD` is therefore not needed and should not be designed.** It stays declared and
unimplemented. This removes the blocker the verdict entry placed in front of phase 1 -- the
geometry question is settled by configuration rather than by a format change.

### Three caveats the numbers above rest on

- **The 1.47x amplification is measured, but not on this shape.** It is the consumer plateau
  recorded under `the-fresh-process-half-of-punching-is-unverified`: 26.82 MB steady against an
  18.24 MB fresh store. Small corpus, and *physical blocks* rather than address space. It is used
  here because it is the only empirical anchor in the tree. The mechanism does transfer: `alloc`
  never scavenges partially free slabs from older generations, so address space returns only when
  a slab empties completely and is punched.
- **That figure is conditional on compacting out of process.** The inherited-slab confinement
  refuses reuse in a process that has already opened the directory, so a server that compacts
  **in band sees none of the gain** and should budget worse than 1.47x. Same caveat as the one
  already recorded for `haiiie`.
- **The unstated 2% is the only live unknown.** It would have to reach tens of GiB per key to
  threaten 16 shards.

### Phase 0 inherits an object count and a manifest size nobody priced

500 GiB at `SLAB_SIZE` is **256 000 slabs per base generation**. Phase 0 cuts one content-addressed
object per slab, so a base manifest carries a quarter-million `FileExtent` records instead of a
handful. Sizing it two ways, at protobuf field overhead:

- extent carrying only the 32-byte SHA-256, the object key derived from it: ~64 B per extent,
  **~16 MB** per manifest;
- extent carrying the key as a hex path string as well: ~172 B, **~44 MB**.

Today's manifest is kilobytes. **This does not sink phase 0**, and the dedup argument is untouched
-- consecutive generations still share nearly every object. What changes is that the manifest stops
being a small message:

- store the address, derive the key, and never write both. The 2.7x between the two rows above is
  entirely a redundant string.
- `gc.rs::survey` **decodes every manifest it finds**, so a retention window holding several
  generations decodes that size per generation per pass. The reclamation pass becomes the place
  this cost lands, exactly as the BLAKE3 note predicted at a fifth of the scale.
- a restore downloads and parses it before fetching a byte.

**And this strengthens the alternative that note named.** One object per shard file with verified
subrange reads -- which is what BLAKE3's internal Merkle tree buys and SHA-256 does not -- trades
dedup for an object count four orders of magnitude lower. At 30 700 objects it was a contingency.
At 256 000 per generation it deserves to be priced against phase 0 before phase 0 starts, rather
than kept as the thing to reach for if the sweep becomes the bottleneck.

That comparison is the one piece of new work these numbers create. It does not block phase 0; it
decides what phase 0 should be.

## 2026-10-15 — Measured: the object count is not what phase 0 should fear, and one-object-per-shard loses at every churn rate

I said yesterday that at 256 000 objects per generation, the one-object-per-shard-file alternative
"deserves to be priced against phase 0 before phase 0 starts". **It was priced, and it loses.** The
correction is mine: I weighed phase 0's object count without weighing what its dedup saves, and the
two are not close.

Measured with a scratch crate under `.agents-workspace/tmp/manifest-scale`, prost 0.14 against the
real message shapes, release profile, Rust 1.98. 500 GiB over 16 shards is 256 000 slabs, 16 000
per shard file.

### The manifest, and the 3.1x that is pure redundancy

| extent encoding | manifest | per extent | decode |
| --- | --- | --- | --- |
| address only, object key derived from it | **13.04 MB** | 51 B | 20.1 ms |
| address plus the key spelled out beside it | **40.94 MB** | 160 B | 38.1 ms |

My estimates of 16 MB and 44 MB were high by about 20%; the shape of the conclusion stands and the
numbers are now measured rather than counted on fingers. **Store the address and derive the key.**
The 3.1x is a 64-character hex string written next to the 32 bytes it is made of, and it costs
28 MB per manifest and 18 ms per decode to carry it twice.

### The object count costs far less than it looks, because dedup is the point

**Pricing the listing as generations times slabs is the error that makes phase 0 look
prohibitive**, and it is the one I made. Generation N+1 adds only the slabs that changed, so after
`g` generations at churn `c` the archive holds `SLABS * ( 1 + ( g - 1 ) * c )` objects. What
`gc.rs::survey` holds in its `Vec<String>`, at 8 retained generations:

| churn | keys | String heap | list to `Vec` | keep-set |
| --- | --- | --- | --- | --- |
| 2% | 291 840 | 53.7 MB | 251 ms | 24 ms |
| 5% | 345 600 | 63.6 MB | 304 ms | 30 ms |
| 20% | 614 400 | 113.0 MB | 526 ms | 61 ms |
| 100% | 2 048 000 | 376.8 MB | 1.76 s | 288 ms |

At any plausible churn this is tens of megabytes of transient memory and a third of a second of CPU
on a pass whose default interval is an hour. **It is not the bottleneck and should not have been
treated as one.**

### What dedup is worth, which is the term I left out

Per base generation, against the alternative of one content-addressed object per whole shard file:

| churn | slab-granular | one object per shard | ratio |
| --- | --- | --- | --- |
| 2% | 10.0 GiB | 500.0 GiB | 50x |
| 5% | 25.0 GiB | 500.0 GiB | 20x |
| 20% | 100.0 GiB | 500.0 GiB | 5x |
| 100% | 500.0 GiB | 500.0 GiB | 1x |

**The alternative never wins.** Content addressing at whole-file granularity means any change to a
shard is a new object and a full re-upload, so it pays 500 GiB per generation at every churn rate
and converges to parity only where dedup has nothing left to find. Trading 475 GiB of upload per
generation for 64 MB of transient memory is not a trade.

**So the BLAKE3 note above is refuted at this scale rather than strengthened**, and the condition
it set out -- "if that sweep becomes the bottleneck" -- is measured and not met. Verified subrange
reads remain the right mechanism for a *lazy fetch* under phase 3; they are not a reason to coarsen
phase 0's granularity. SHA-256 stays the content address.

### What the pass does cost, which is network rather than CPU

Two terms the model above makes visible, neither of which changes the verdict:

- **`survey` downloads and decodes one manifest per retained base.** Eight generations at 13.04 MB
  is 104 MB per reclamation pass, hourly by default. Worth knowing before someone sets
  `--retention-interval-secs` low.
- **The listing is the round trips, not the bytes.** `list_relative` over the whole database prefix
  at 1 000 keys per page is ~346 requests at 5% churn. At a typical per-request latency that is
  seconds to tens of seconds of wall clock, and it dwarfs the 304 ms of local work. If a pass ever
  needs to be cheaper, that is the term to attack -- not the manifest, and not the object count.

### Limits of this measurement

Synthetic 32-byte addresses rather than real hashes, which is sound because size and parse cost do
not depend on the bytes. No network and no object store: every figure above is local encode, decode
and allocation. The round-trip estimate is arithmetic over a page size, not a measurement. Decode
times are one host, release profile, and are reported to show an order of magnitude rather than to
be compared against a budget.

## 2026-10-15 — Phase 0 lands: a base file is now content-addressed slabs, and three bugs were the same omission

The first phase of the external-slab plan, which the verdict entry marked go and the measurement
entry settled the shape of. **No `yesno-core` change, no feature bit, no schema-version bump.**

### What changed

`FileExtent` gained `content_hash` ( SHA-256, whole, never truncated ) and a per-object `crc32c`.
A base file is now one of two shapes and never both:

- `ArchiveFile.object_key` names one object and the extents are runs inside it -- every archive
  written before this, unchanged;
- `object_key` is empty and **every extent names its own object** at `db/<uuid>/slabs/<address>`.

`SCHEMA_VERSION` stays at 2 for the reason the sparse work established: added fields, not changed
meanings, so every archive already in a store still restores. The direction that does not hold is
an older release reading a base this one writes, and it fails to find an object rather than
reconstructing from the wrong bytes.

**The cut is `yesno_core::store::SLAB_SIZE`, imported rather than restated.** That alignment is the
whole reason dedup works: a checkpoint dirties the slabs it touched and leaves the rest
byte-identical, so generation `N+1` renames almost every object `N` already stored. Cut anywhere
else and an unrelated write shifts every later object. The cost is that hole detection coarsens
from `SPARSE_WINDOW` to the slab, because an object cannot be half present -- a one-megabyte hole
inside a two-megabyte slab is now stored. `archive.py`'s 64x assertion still passes, so the sparse
saving survives the coarsening.

Uploads run 16-wide and each checks `head` before `put`, so **a slab unchanged since the last
generation is not sent**. That is the 20x of the measurement entry, obtained without threading the
previous manifest anywhere.

### Two decisions inside the change

**`validate_extents` had to stop refusing adjacent extents.** Under one object per file, touching
runs meant the writer failed to merge them and the list was not canonical. Under one object per
slab, two consecutive slabs that both carry data are touching runs and are the ordinary case. The
rule is now per shape: adjacency is a defect in the first and expected in the second, and only
overlap is refused in both. This is the kind of invariant that looks like it transfers and does
not.

**The restore fetches with `buffered`, not `buffer_unordered`.** The file's CRC32C covers the holes
as well as the runs, so it has to accumulate from offset zero upward; out-of-order completion would
mean either holding the whole file or composing CRCs. Ordered concurrency costs nothing here and
keeps the verification sequential. Each object is checked by length, then by its own CRC, then the
reassembled file against the manifest's whole-file CRC -- the last being the one that proves the
*file*, since the first two only prove each object is what its own extent described.

### `gc.rs` was mandatory, and the self-review was right about why

`classify` requires `db/<uuid>/term/<term>/...`, so without a change every slab object would have
fallen to `Kind::Unrecognized` -- **kept and reported**, never deleted. The archive would have
grown without bound while every other signal looked healthy. `Kind::Slab` is now matched on a
64-character hex name under `db/<uuid>/slabs/` and reclaimed purely by reachability.

**It is deliberately not term-scoped**, which is a reasoned exception to a stated invariant and is
written down where the invariant is. Every other key here carries a term so a promoted timeline
cannot overwrite an older one's identically numbered object; two writers that produce a content
address produced the same bytes, so there is nothing to fence, and scoping it by term would defeat
the sharing between generations it exists for.

One thing the fix surfaced: `keep_slabs` has to be an owned `BTreeSet<String>` where `keep_files`
borrows, because a slab key is *derived* rather than spelled out in the manifest. And `plan` must
hex-encode the UUID **without** `uuid_hex`'s length check -- it is a planner, not a validator, and
refusing there would turn a bad identity into a failed reclamation pass rather than a rejected
manifest. A prefix matching no key keeps objects, which is the safe direction.

### Three bugs, one omission: the file that is entirely zero

It broke validation, then the e2e harness, then restore, and each time I fixed the symptom before
seeing the cause. **A file with no bytes at all has no extents, so it names neither an object nor
an address -- and needs neither.** The shape check read that as "neither", the harness joined an
empty key onto the object directory and measured the directory, and the restore asked the store for
an object at the empty path.

The fix that generalizes, rather than the three that did not: **the restore dispatches on
`object_key.is_empty()`, not on whether any extent is addressed.** A file whose bytes are elsewhere
and a file whose bytes are nowhere are the same reassembly with a different number of fetches, so
they take one path -- `set_len`, fetch nothing, hash the holes, verify. The empty case stopped
being special.

Worth recording as a pattern: all three failures were a *degenerate* input to a new encoding, and
the encoding's own tests did not have one until the harness supplied it. `archive.py` found two of
the three, which is the argument for running it before claiming the change works.

### `base_anchor` now commits to less, on purpose

It hashes `file.object_key`, which is empty for a slab-addressed file, so the anchor commits to
what each file *is* -- name, length, CRC32C -- and no longer to where its bytes were put. That is
the right division: the locations of a content-addressed file's objects are a function of its
content, so hashing them would commit to the same information twice. Per-extent addresses stay
outside the hash for the reason `checkpoint_time` gives. Noted in `history.rs` beside the function.

### Gates and what is not done

`scripts/gate.sh` passed. `gate-pg.sh` was run as well -- not because `yesno-server-utils` has a
Bazel target, it has none, but because this change touches `yesno-e2e`, whose image that gate
builds. `archive.py`, `archive_seed.py`, `pitr.py`, `pitr_retention.py` and `basebackup.py` all
pass.

Not in this increment, and both are the same follow-up: the `head` round trip per slab is how an
unchanged object is skipped, and the previous generation's manifest would answer that for free --
256 000 head requests per capture at the consumer's scale is the term to remove next. The manifest
itself is still written whole per generation.

## 2026-10-15 — The skip set: a capture now costs the churn, with no round trip for what did not change

Phase 0's follow-up, and the term the previous entry named as the only thing the 20x bandwidth
saving still cost. Increment 1 skipped an unchanged slab by asking the store whether the object was
there -- correct, and 256 000 `head` requests per capture at the consumer's scale.

**The previous generation's manifest answers the same question for free.** `slabs_of_active_base`
reads `state.pb`, follows `latest_base_manifest`, and collects the content addresses it names. A
slab in that set is skipped outright: no upload, and no request of any kind. At 500 GiB and 5%
churn that is 256 000 requests becoming about 12 800, and those remaining ones still `head` before
putting, which is what makes a *resumed* capture cheap -- an interrupted run's objects are found
rather than sent twice.

### Why the set can be trusted, which is the whole of the correctness argument

Reclamation keeps every object named by a retained manifest, and **the active base is never
deletable** -- `the_active_base_is_never_deleted` is the test. The active base is exactly what
`state.pb` points at. So every object this set names is in the store.

The part worth stating plainly: **this adds no failure mode a restore does not already have.** If
the active base's objects could vanish, restoring from it was already broken, and the skip is
trusting precisely what the restore path trusts. That is the argument that makes skipping the
`head` safe rather than merely cheap.

### Every failure returns the empty set, and one of them was mine

No `state.pb`, no active base, a state that will not load, a manifest that will not decode, a
manifest written before slab objects existed -- each yields an empty set. An empty set costs a
`head` per slab and is **always correct**, so every one of those degrades to increment 1's
behaviour rather than skipping an upload that was needed. The asymmetry is the design: a set that
is too small wastes a round trip, a set that is too large loses data.

I wrote `self.load_state().await?` first, which propagated. `load_state` validates the schema
version, so a `state.pb` this build could not read would have **failed the entire base capture** --
and in the same function whose doc comment promised to degrade. The test caught it. It now
degrades, and the reasoning is recorded beside it: `put_base` publishes `state.pb` immediately
afterwards, so a bad state is still reported, by the code whose job that is rather than by an
optimisation that happened to read it first.

### The test is about the assumption, not the code

`a_slab_the_active_base_already_holds_is_never_sent` runs against an **empty** store with a
populated skip set. Increment 1's behaviour -- `head`, miss, upload -- would have written every
object; this writes nothing and still produces a complete manifest, because a skipped upload must
never mean a skipped extent. It passes only because the set is taken on trust, which is exactly
what makes it worth having as a test: it will fail the day someone makes the skip conditional on
something the store says.

`gate.sh` passed. `gate-pg.sh` was not run and does not apply: this touches `yesno-server-utils`
alone, which has no Bazel target, and unlike the previous increment it does not reach `yesno-e2e`.
`archive.py`, `archive_seed.py` and `pitr_retention.py` pass.

### What is left of phase 0

The manifest is still written whole per generation -- 13 MB at 500 GiB, measured -- and `survey`
downloads one per retained base on every reclamation pass. That is now the largest remaining
per-capture and per-pass cost in this path, and the listing round trips behind it are larger still.
Neither blocks anything; both are recorded under the measurement entry above.

## 2026-10-15 — A capture and a reclamation pass could not safely overlap, and phase 0 is what made that true

Found by asking what phase 0 changed about reclamation rather than about bandwidth. The answer is
a hazard that did not exist a day ago, and it is the kind that produces a published base nobody
can restore.

### The mechanism

`put_base` uploads every slab object and publishes the manifest **last**, so while a capture runs
its objects are named by no published manifest. `gc::plan` is reachability over the manifests the
survey found. A pass that overlaps a capture therefore plans to delete exactly the objects that
capture is about to reference -- and the plan is *right* about every manifest it can see.

**This was not reachable while a base file was one object per generation.** Each generation wrote
under its own `base/<gen>/` prefix, so a capture depended on nothing but its own uploads, and an
interrupted one left orphans the next pass was supposed to collect. Content-addressed slabs made a
capture depend on objects an **older generation owns**: an unchanged slab is proven present with a
`head` and then named without being re-sent. Drop the base that first stored it and the manifest
published seconds later is a dangling reference.

The two skip paths are not equally exposed, and the difference is worth stating because it is the
whole of the argument:

- **The increment-2 skip set is safe on its own terms.** It is built from the *active* base, and
  the active base is never deletable -- `the_active_base_is_never_deleted`.
- **The increment-1 `head` is not.** It asks the store whether an object exists, and the store
  answers yes for an object that only an about-to-be-dropped generation retains.

`collect`'s existing re-read of `state.pb` does not help: `state.pb` does not move until the
capture finishes, so the check sees nothing and the deletes proceed.

### The fix is a gate, not a marker

`CaptureGate` in `sidecar.rs`: one `tokio::sync::Mutex` shared by the capture path and
`retention_loop`, which are sibling tasks in the same `JoinSet` and were previously uncoordinated.
A capture holds it from the first upload until the manifest exists -- from that moment the base is
the newest, is therefore always retained, and its slabs are reachable again.

**The asymmetry is deliberate: a capture waits for a pass, a pass skips rather than waits.** The
capture is the work; reclamation is a periodic sweep that loses nothing by running one interval
later. The gate is taken *before* the lease renewal, because a pass that will not run needs no
fence and `writer_lease_loop` owns liveness, and it is held across the survey and the plan rather
than only across the deletes -- a pass that planned before the capture and deleted after it would
be the same bug.

**One in-process gate is the whole of what this needs**, because the writer lease already fences a
second writer. No marker object, no new durable state, no format change. A crash mid-capture is
unaffected: the objects it leaves are named by no manifest at all, which is the unreachability the
next pass exists for.

### The test, and the mutation that proves it is a test

`a_capture_in_progress_keeps_a_pass_off_an_orphan_slab` puts a slab object no manifest names into
a real `file://` archive, holds the gate the way `publish_captured_base` holds it, drives the real
`retention_loop`, and asserts the skip was reported, no pass completed, and **the object is still
there**. Then it releases the gate and waits for the same object to go. Without that second half
it would pass against a loop that never ticked.

Neutering the gate check makes it fail at `passes_total` 1 against 0, which is the confirmation
worth having: an orphan slab really is deletable, so the first half is not asserting a tautology.

### What else came out of it

Two things, both small and both in the same path.

- **`run_with_shutdown`'s doc comment was sitting on `retention_loop`.** The same mis-stitch class
  as `33e9670`, one line this time: "Run until `shutdown` resolves or an archive task fails."
  described a function three hundred lines further down. Returned to it.
- **`archive_reclamation_skipped_total` now has two causes and documented one.** The counter, its
  `pass_skipped` doc and `docs/operations/backup.md` all said "the writer lease was not renewed".
  The operator-facing text now names the second cause and says what a steady climb with no
  completed pass between the skips means: the reclamation interval is shorter than the capture,
  and the interval should be raised. That is a real reading on a 500 GiB database, where the first
  capture is long.

`gate.sh` passed. `gate-pg.sh` was not run and does not apply: `yesno-server-utils` has no Bazel
target and nothing here reaches `yesno-e2e`.

## 2026-10-15 — Measured: what a generation re-sends tracks the database, not the change

Phase 0's economics were argued from a churn rate and never measured against a running archive.
Measuring them produced a number that does not refute the plan and does narrow it sharply, and a
scenario that covers a dependency nothing else in the suite touches.

### The instrument

`inspect_archive` gained `slab_objects` and `slab_object_bytes` -- every object under
`db/<uuid>/slabs/`, whichever generation wrote it. The pair with `base_object_bytes` is what makes
sharing observable at all: the manifest says what a base **names**, these say what the store was
made to **hold**, and the difference between two readings is what a generation cost.

Construction: one shard, N single-ordinal keys through Flight, checkpoint, base generation 1; stop
the sidecar, write `CHURN` more keys, exceed the leader's 64 KiB hard WAL cap so the reconnect
must re-root, checkpoint, base generation 2. Probe at `.agents-workspace/tmp/dedup-probe/`.

### The numbers

All bytes; "pieces" are the 2 MiB content-addressed objects a base file is cut into.

| keys    | churn  | gen 1 pieces | gen 1 bytes | gen 2 names | gen 2 stored | shared        |
| ------- | ------ | ------------ | ----------- | ----------- | ------------ | ------------- |
| 50 000  | 2 000  | 3            | 4 202 496   | 4 202 496   | 4 194 304    | 8 192 ( 0.2% )|
| 300 000 | 2 000  | 5            | 8 396 800   | 8 396 800   | 6 291 456    | 2 105 344 ( 25% ) |
| 900 000 | 2 000  | 11           | 20 979 712  | 20 979 712  | 10 485 760   | 10 493 952 ( 50% ) |
| 900 000 | 20 000 | 11           | 20 979 712  | 20 979 712  | 10 485 760   | 10 493 952 ( 50% ) |

**The last two rows are the finding.** A tenfold increase in churn -- 2 000 keys to 20 000 -- moved
the stored bytes by **zero**. The dirty set is a property of the database's layout, not of how much
changed. Two further controls came out identical: churn as *fresh* keys rather than overwrites
( ruling out the freed-chunk and slab-metadata explanations ), and churn at the low end of the key
space rather than the high.

So in this shape a generation costs a roughly constant **fraction** of the database -- 67%, 60%,
45% re-sent as the database grows -- and content addressing buys about 2x rather than the 20x the
pricing entry derived from a 5% churn rate.

### Why this does not refute phase 0, and what it does do

**The regime that was priced is not the regime that was measured.** These databases are *index*:
single-ordinal keys, payload negligible, so a checkpoint's shadow-paged index rewrite touches a
large fraction of the file. The consumer's shape is the opposite and the plan said so on 2026-10-14
-- "the index is about 0.25% of the key's bytes" at 60 MiB per key -- so there the frontier a
checkpoint advances through is small and payload pieces are untouched. The 20x may well hold there.

What has changed is the standing of the claim. **It was general and is now shape-dependent, and the
shape it was argued for remains unmeasured.** Phase 0 is still correct work and still ships; the
bandwidth figure in the pricing entry should be read as conditional on a payload-dominated
database, which no test here constructs. Filed as `base-dedup-unmeasured-in-the-payload-regime`.

### The scenario, and what it is actually for

`e2e/scenarios/base_dedup.py`, 22 s standalone, and bought by the restore at the end rather than
by the ratio. The suite total moves more than that between runs on this machine -- 251 s before
the fixture, 206 s on the gate that includes it -- so the honest statement of the cost is the
scenario's own time, not a delta against a total that will not hold still.

**A generation whose manifest names objects an earlier capture wrote is a new kind of dependency**,
and nothing in the suite exercised it. The unit tests cover the uploader on synthetic files;
`pitr_retention.py` restores after a sweep without asking where the bytes came from. The way this
fails is a base that verifies, lists and plans correctly and then cannot be read back -- so the
load-bearing assertion is a restore of generation 2 that reads a key the churn never touched.

The ratio is printed and **deliberately not asserted**, with the measurement above recorded in the
file's header as the reason. Pinning a number from the one shape that cannot represent the
workload would be pinning the wrong one. Two things are asserted besides the restore: that
generation 1's stored bytes equal exactly what its manifest names -- a capture that left an
unnamed object would show up here as an orphan before reclamation had to find it -- and that
generation 2 stored strictly less than it named, which is the mechanism and fails only if sharing
stops entirely.

## 2026-10-15 — The payload regime, measured: a generation costs what changed plus one piece

Closes `base-dedup-unmeasured-in-the-payload-regime`, filed hours earlier in the entry above. The
answer is the one phase 0 was argued from, and it is sharper than the argument was.

### Getting at the regime at all

The filed item said this needed a widened `flight_put` or a route through `db_insert_range`,
because the Flight write wire carries one row per set bit and a payload-dominated body cannot go
through it. **Neither was needed.** `db_open` and `srv_config` both resolve their name through
`World::scenario_path`, so a body built through the library sits in exactly the directory the
daemon then opens. Build, checkpoint, close, launch.

**The first attempt measured nothing, for a reason worth keeping.** A body of twenty contiguous
`db_insert_range` keys spanning 335 million ordinals came to **6 MB** -- a contiguous range is a
run container, four bytes an interval, so the densest-looking construction available is also the
cheapest. Stride 16 is what produces payload: 4096 ordinals in every 65 536-wide chunk, which is
`ARRAY_MAX`, so 8192 bytes of payload against about 20 bytes of index.

### The numbers

Body of 10 keys, stride 16. `added` is what generation 2's manifest names beyond generation 1's;
`grew` is what the store actually had to hold for it.

| body    | gen 1 pieces | gen 1 bytes | churn   | added     | grew      | overhead  |
| ------- | ------------ | ----------- | ------- | --------- | --------- | --------- |
| 25 MB   | 14           | 25 174 064  | 12 000  | 2 097 104 | 4 194 304 | 2 097 200 |
| 46 MB   | 24           | 46 145 584  | 12 000  | 2 097 104 | 4 194 304 | 2 097 200 |
| 25 MB   | 14           | 25 174 064  | 120 000 | 4 194 256 | 6 291 456 | 2 097 200 |

**The overhead is identical to the byte across all three** -- 2 097 200, which is `SLAB_SIZE` plus
48. Double the database: unchanged. Multiply the churn by ten: unchanged, and `grew` rises by
exactly the data that was added. So in this regime a generation costs **what changed plus one
2 MiB piece**, and the saving improves with database size without bound.

Set against the previous entry's index-dominated measurements, where generation 2 named *nothing*
new -- `added` of zero -- and still stored 3 pieces at 300 000 keys and 5 at 900 000, the
distinction is clean and is about one thing: **the region a checkpoint rewrites tracks the index.**
Payload pieces are not touched, so they are named rather than sent; index pieces are rewritten
wholesale, and in a database of single-ordinal keys that is most of the file.

That is the mechanism the plan asserted on 2026-10-14 from the 0.25% index ratio at 60 MiB per key,
now measured rather than inferred. **The 20x bandwidth figure stands for the consumer's shape**,
and the qualification the previous entry attached to it is answered rather than left open.

### The fixture now builds the regime that matters

`base_dedup.py` was rewritten rather than duplicated -- a second archive scenario would have cost
another 27 s for a worse question. It builds the payload body, captures two generations, and
asserts the bound this entry measured: `grew - added <= 2 * SLAB_SIZE`, which has 2x headroom over
the 2 097 200 observed and fails if the per-generation rewrite starts growing with the database.
The index-regime figures stay here, with their construction, which is where a number that is not
asserted belongs.

The restore at the end is unchanged and is still the load-bearing part: generation 2's manifest
names pieces the *first* capture uploaded, and a key the churn never touched comes back only if
those are fetched. 27.9 s, both gates green.

## 2026-10-15 — A reclamation pass already holds the listing, so it can say what the archive has lost

Phase 0 made a base file a set of content-addressed pieces, and the three increments since then
have been about what that buys and what it costs. This one is about what it **breaks that nothing
was watching**: an object's age stopped being evidence that anything still needs it.

### The hazard, stated precisely

A slab is named by every later generation whose checkpoint did not rewrite it, and is deliberately
not re-uploaded when that happens -- that is the saving the last two entries measured. The
consequence nobody had written down is that **its last-modified time stops advancing while it stays
live**. Last-modified is exactly what an object-store lifecycle rule keys on. So a rule expiring
objects after 90 days now deletes pieces of the base being written today, and `backup.md`'s existing
warning -- which argued from WAL being anchored to an older base -- was arguing the weak version of
the case.

Before phase 0 that could not happen. A base owned its files under its own prefix, every one of them
uploaded when the base was captured, so the whole base aged together and an age rule that spared the
base spared its files.

### The check costs nothing, which is why it belongs in the pass

`gc::plan` already receives a complete listing of the database prefix and already computes what the
retained bases name -- `keep_files` and `keep_slabs` exist so it can decide what to delete. Asking
the opposite question is a set difference over data in hand: **no round trip, no second survey, no
new configuration**. It is now `GcPlan::missing`, reported in the pass summary line, published as
`yesnod_archive_missing_objects`, and logged at warning level with one example key beside the count.

The metric is the mirror of `yesnod_archive_unrecognized_keys` and the more urgent of the two. That
one says the archive is too big and will grow without bound; this one says the archive is
*incomplete*, does not heal, and that no later pass repairs it. Without the gauge the condition
surfaces at a restore, which is the one moment nobody wants to discover it.

Scope is deliberate: only what a base names directly, its file objects and their slabs. WAL
reachability is a chain walk rather than a set membership, and `restore` already refuses a chain
with a hole in it before it fetches anything.

### A bug fell out of writing the check

`plan` inserted `file.object_key` into `keep_files` unconditionally, and a slab-addressed file
carries the **empty string** there -- its bytes are under `slabs/`, named by content, so it has no
object of its own. That was harmless while the set was only ever consulted ( no listed key is the
empty string, so it matched nothing ). Searching the other way round it is not harmless: every
slab-addressed file in every retained base would have been reported as an object the store had lost,
which is a loud false alarm on exactly the deployments phase 0 was built for. Guarded, and the guard
is mutation-checked -- reverting it makes `a_slab_object_survives_while_any_retained_base_names_it`
fail with `missing: [""]`.

### What the end-to-end suite gained, and what it does not claim

`base_dedup.py` built two generations and restored the second; it never swept. It now runs one real
reclamation pass between the two, with a window of zero and `min_bases` of 1, so the **first
generation is pruned** -- and then restores. Measured: 14 pieces in generation 1, generation 2 names
13 of them, the sweep deletes 3 objects ( the old manifest and the 2 pieces only it named ), and the
restore reads back a payload key the churn never touched. Those bytes were uploaded by the
generation that no longer exists. That is the cross-generation dependency, exercised through the
shipped reclaimer rather than a unit test's synthetic manifests.

Be clear about what the new `missing == 0` assertion there does and does not do. The plan is
computed before the pass's own deletes, so a zero is a statement that the archive was whole when the
sweep surveyed it -- a precondition, not a verdict on the sweep. **The restore is still the thing
that would catch a wrong deletion**, and it is placed after the sweep for that reason.

Cost: the scenario runs in 4.1 s. Gates: `gate.sh` and `gate-pg.sh`, both, because the diff reaches
`yesno-e2e`, which Bazel builds.

## 2026-10-16 — The reclamation preview had been written and never wired to anything

`gc::plan_for` has carried the doc comment "Safe to run against a live archive, and what an
operator should look at before enabling reclamation" since it was written. It had **zero callers**
anywhere in the workspace. `yesnoctl` offers `checkpoint`, `basebackup`, `restore` and
`seed-follower`, and nothing else; `backup.md` tells an operator to weigh a retention window and
then hands them no way to see what one would do.

So `--retention-window 7d` was a flag whose effect is irreversible and whose consequences were
unpreviewable, and the dry run that would have fixed that existed, compiled, and was unreachable.
It is now `yesnoctl archive-plan`.

### Two questions, one pass, and the window is optional

The obvious use is the preview: what would a pass with this policy delete. The one I did not expect
to be the more valuable is what happens when `--retention-window` is **omitted**. The policy then
has a window no base can fall out of, the horizon lands at the epoch, every base is retained, and
nothing is proposed for deletion -- so the whole report is `missing`: what the bases name and the
store does not hold.

That matters because **reclamation is off by default**. Yesterday's
`yesnod_archive_missing_objects` is published by a reclamation pass, so exactly the deployments
most exposed to a lifecycle rule eating their content-addressed slabs -- the ones that never
enabled reclamation and assumed lifecycle rules would handle it -- were the ones with no way to
learn. Without a window the command is an integrity check, and it is the right thing to run after
any change to bucket lifecycle, replication or access policy.

It takes **no writer lease**. Taking one would fence the sidecar that owns the prefix out of its
own archive for the lease's term, and nothing here writes. A concurrent capture can make the
delete count optimistic, because objects uploaded after the listing are not in it; it cannot
produce a false `missing`, because a manifest is published after every object it names.

### What the test asserts that the numbers do not

`a_preview_reports_a_lost_object_and_removes_nothing` counts the files under the store before and
after. The counts are the point. An operator reaches for this command precisely when they are not
yet willing to let anything be deleted, so a test that checked only the reported numbers would miss
the single property they are relying on.

### A fixture moved, and the reason is worth recording

The test needs a real `ArchiveStore`, and `gc`'s own test manifests cannot be fed to one: they are
built for `plan`, which is pure, and they fail `validate_base_manifest` on a one-byte uuid and an
absent history anchor. `metrics` already had the smallest archive that passes. It is now
`src/fixture.rs`, `#[cfg(test)]`, for the reason `lib.rs::ctx` gives for living where it does --
neither module owns it. `metrics` asserts what a pass publishes, `gc` asserts what a pass decides,
and both need the same archive on disk.

## 2026-10-16 — One manifest this build could not read ended reclamation for the whole archive

Found while reading `survey` for the preview command, not by a failure. The loop was:

```rust
let manifest = pb::BaseManifest::decode(store.get_bytes(key).await?)?;
validate_base_manifest(&manifest)?;
```

Two `?`s on a per-object property, inside a loop over every manifest under the prefix. **One
manifest this build cannot read therefore ended the pass**, and the next pass, and every pass after
that -- no plan, no gauges, no deletions, the bucket growing without bound. The sidecar logged
`unsupported base-manifest schema 3, expected 2` once an interval, and that message does not say
which of an archive's thousands of objects it is about.

That is precisely the failure the module's own design argues against everywhere else. `classify`
returns `Unrecognized` rather than guessing, the doc says an unrecognized key is "a newer writer's,
or a hand-placed file, and either way guessing is how a reclaimer becomes a data-loss incident",
and such keys are counted and reported. The conservative treatment stopped at the *key* and never
reached the *contents*.

### Why it cannot simply skip the manifest, and what it does instead

A manifest is not an object to be kept -- it is the thing that justifies keeping other objects.
Every deletion is reachability from the manifests, and one that will not decode may name any slab
in the store. There is no conservative subset: the objects such a manifest protects are exactly the
ones nothing else vouches for.

So the pass **runs, reports, and deletes nothing**. Be honest about what that buys, because
reclamation is halted either way. What changes is that the gauges are published, the offending key
is named with its reason, and the halt is a distinct alertable state rather than something an
operator has to infer from a deletion count that has gone flat -- which is also what a healthy
archive with nothing to reclaim looks like. Hence
`yesnod_archive_unreadable_base_manifests` rather than folding it into `unrecognized`.

**A fetch failure is deliberately not treated this way.** `store.get_bytes` failing is transient
and the next pass should simply try again, so it still ends the survey. Only the bytes being wrong
is a property of the object.

### The test is a pair, because one half of it would pass vacuously

`a_manifest_this_build_cannot_read_halts_deletion_and_names_itself` plans twice over the same real
store. The first plan has a spent WAL object to remove and removes it; then a manifest with
`schema_version: 99` is written and the second plan deletes nothing. Asserting only the second
would hold just as well on an archive with nothing to reclaim.

### The same lesson as the ENOENT entry, one layer out

`lib.rs::ctx` exists because `std::io::Error` carries no path and the `?` that boxes it is where the
only identifying information is lost. This is the same shape with an object key instead of a path,
and it had the same consequence: a true message that could not be acted on. The unreadable list
carries `"{key}: {error}"`, so the report names the object and the reason together.

### Postscript: the gate caught what the local loop structurally cannot

The first gate run on this change failed, and not on a test: `cargo doc` refused
`public documentation for 'gc' links to private item 'planned'`. The local lint loop in CLAUDE.md
is clippy, `fmt --check` and `cargo test -p yesno-core`, and **none of the three can see an
intra-doc link**. Only `cargo doc` resolves them, and it runs in the gate rather than in the loop.

Worth knowing rather than worth changing: a private helper is the natural thing for a public doc
comment to want to point at, so this is reachable any time a `pub` item's documentation explains
itself by naming the function that implements the rule. The fix is to state the rule in the doc
comment instead of linking to where it lives, which is better writing anyway -- the field now
carries the reasoning rather than a pointer to it.

## 2026-10-16 — The same defect on the restore path, where it costs the recovery rather than the sweep

Having found it in `gc::survey` by reading, I looked for the shape elsewhere: a `?` on a per-object
property inside a loop over every manifest. `restore::list_bases` had it, through `try_collect`:

```rust
let manifest = pb::BaseManifest::decode(store.get_bytes(&key).await?)?;
validate_base_manifest(&manifest)?;
```

**This one is worse than the reclamation case.** There, one unreadable manifest halts the sweep and
the bucket grows. Here, one unreadable manifest anywhere under `db/<uuid>/term/` failed **every
restore and every `--inspect`** -- including a target that manifest has nothing to do with, and
including bases this build wrote and can read perfectly. It is a failure discovered during a
recovery, which is the worst moment there is to discover anything.

### The fix is easier here, and the reason is worth stating

Reclamation could not simply skip: a manifest there *justifies keeping* other objects, so one that
will not decode may name any slab and there is no safe subset. On the restore path a manifest is a
**candidate recovery root and nothing else**. One that will not decode is simply not a candidate,
and skipping it costs nothing it could have offered.

Skipping is safe only because it is reported. A quiet skip changes which base a restore roots at
without saying so, which is why the warning is emitted from `list_bases` itself -- the one place
that learns of them -- and why the "no base at or below target version V" error now says how many
were skipped. Without that, the message reads as "this archive never held one" while the base that
answers it sits there unread.

### Two asymmetries, both deliberate

**A fetch failure is still fatal**, here as in `gc`. A restore that quietly rooted at an older base
because one GET timed out would answer a different recovery point than it was asked for. Only the
bytes being wrong is a property of the object, and that will not resolve on a retry.

**The tip is still fatal.** `--target-version` omitted asks for the durable tip, which names one
specific manifest through `state.pb`. If that one cannot be read, rooting at an older base would
answer a different recovery point under the name the operator asked for -- worse than refusing. The
message now carries the key, which it did not.

### Mutation-checked

Both tests fail under a sabotage that restores the old behaviour ( `list_bases` returning `Err` on
the first unreadable manifest ), with the good base present and selectable. The second test is the
one that matters for the error text: it asserts both that the target is named and that the skipped
count is.

`cargo doc -p yesno-server-utils --no-deps` was run locally this time, before the gate, which is
the only change the previous entry's postscript asks for.

## 2026-10-16 — Finishing the sweep: the WAL descriptor loops, and where fatal is still right

Two entries today fixed the same shape in two places. Rather than stop at two, I swept every
`BaseManifest::decode` and `WalObject::decode` in the crate. The remaining ones split cleanly, and
the split is the content of this entry: **the question is never "is a decode failure fatal", it is
"does this object have an alternative".**

### Changed: both loops over the term's whole WAL history

`load_wal_descriptors` fetches one descriptor per WAL object in the **entire** leadership term,
then `chain_objects` selects the part a given restore replays. Most of that listing is history
below the base's cursor or above the target -- neither replayed nor reachable -- and one
undecodable object anywhere in it failed the restore.

Dropping it is safe for a reason specific to this path and worth stating: **`chain_objects`
refuses a chain with a hole in it.** A descriptor the restore actually needs therefore still stops
the restore, now with the object named by a warning; a descriptor it would have walked past no
longer costs it anything. The oracle was already there, failing closed; the `?` was pre-empting it.

`inspect`'s own loop is the same change for a weaker reason -- it is advisory output, and one
unreadable descriptor should not cost an operator the report of every window that is fine.

### Unchanged, and each for a stated reason

- **The durable tip** ( `load_base` with no target ) and **the sidecar's active base**
  ( `wal_loop` ). Both name *one specific manifest* through `state.pb`. There is no alternative to
  fall back to, and rooting at an older base would answer a different recovery point under the
  name the caller asked for. Both now put the object key in the message, which neither did.
- **A manifest a base capture just built** ( `put_base` ). Validating our own output before
  publishing it; a failure is a bug in this process, not damage in the store.
- **Escaping the database or term prefix.** A well-formed object in the wrong place is a different
  claim from bytes that will not parse, and it stays fatal on the replay path.
- **Every fetch failure, everywhere.** Transient, and the retry is the next pass or the next
  invocation.
- **WAL bodies in `assemble_wal`.** Those are the objects being replayed, checked against their
  descriptors. By then there is no "this one is not needed" left to say.

### The rule the three entries add up to

An object that *is* a candidate -- a recovery root, a descriptor in a listing -- can be skipped
when it cannot be read, provided the skip is reported and some later check still fails closed on
the ones that mattered. An object that *justifies* other objects, or that the caller named
specifically, cannot be: there is nothing for the operation to mean without it.

## 2026-10-16 -- The archives that needed the integrity gauge were the ones that never published it

Three days of work put an integrity check into the reclamation pass: `GcPlan::missing`, the set
difference between what the retained bases name and what the listing holds, published as
`yesnod_archive_missing_objects`. Yesterday's entry on `yesnoctl archive-plan` noticed the gap and
left it open:

> reclamation is off by default ... exactly the deployments most exposed to a lifecycle rule
> eating their content-addressed slabs -- the ones that never enabled reclamation and assumed
> lifecycle rules would handle it -- were the ones with no way to learn.

The command closed it for an operator who runs a command. It did not close it for a dashboard, and
a hazard whose whole character is that nobody notices until a restore is not one to leave to
someone remembering to ask.

### The pass was already the right shape; only its spawn condition was wrong

`run_with_shutdown` spawned `retention_loop` inside `if let Some(policy) = retention`. Everything
inside the loop that is worth having with reclamation off -- the listing, the manifest reads, the
reachability computation, the three gauges, the capture exclusion -- was gated on the one decision
that is about *deleting*. So the loop now runs unconditionally and takes `Option<RetentionPolicy>`,
and the two places the modes differ are the two places they should: a survey calls `gc::plan_for`
rather than `gc::collect`, and it does not renew the writer lease first, because nothing it does
needs the archive to still be ours.

The survey's policy is a new `RetentionPolicy::SURVEY`, which opens both axes as far as they go --
`window_micros: u64::MAX` and `min_bases: usize::MAX`. Either alone would retain every base; both
are set so that no later change to how the floor is computed can quietly make a survey drop one.
What the plan then reports is the strongest form of the check: not "can the bases this policy
would keep be restored" but "can every base in this archive be restored".

### The one thing a survey must throw away

A plan made with `SURVEY` still proposes deleting WAL below the oldest base's cursor, because that
is unreachable however many bases are kept. The loop clears `plan.delete` before counting the pass.
This is not cosmetic. `pass_completed` adds `plan.delete.len()` to `archive_objects_deleted_total`,
and a counter of deletions that moves while nothing is deleted is worse than no counter -- it is
the metric an operator would use to confirm that a survey-only sidecar is behaving. Clearing
`delete` to mean "this pass will remove nothing" is the same idiom `gc::planned` already uses for
an unreadable manifest.

`a_sidecar_with_reclamation_off_still_surveys` is built so neither half is vacuous: the fixture is
missing an object a live base names *and* holds a spent WAL object below the only cursor. Removing
the `plan.delete.clear()` line makes the deleted counter read 1 while the file is still on disk,
which is exactly the lie the clear exists to prevent.

### Why a survey takes the capture gate, and the preview does not

Yesterday I wrote that a concurrent capture "cannot produce a false `missing`, because a manifest
is published after every object it names". That is true of the publication order and not quite true
of the survey, and the loop is where the difference starts to matter.

A listing is served in key order, and `slabs/` sorts before `term/`. A capture running alongside an
ungated survey can therefore upload its slabs *after* that part of the listing was served and
publish its manifest *before* the manifest keys were read -- and the survey reports objects as
missing that are sitting in the store. The window is narrow and the next pass clears it. But
`archive_missing_objects` is documented as a condition that does not heal, which is the whole
argument for alerting on any non-zero value, so a transient false reading costs more here than the
listing it would save. The loop takes `CaptureGate::reclaiming()` for a survey exactly as it does
for a reclaiming pass. `yesnoctl archive-plan` keeps the race: it has no gate to take, it is run by
someone who is watching the output, and a preview that fenced the sidecar out of its own archive
would be a worse trade.

### What was deliberately not renamed

`archive_reclamation_passes_total` now counts surveys as well, and its help text says so. Renaming
a published family to `archive_passes_total` would be more accurate and would break every dashboard
and alert already written against it, for an accuracy nobody is confused by once the help string is
read. The family name stays; the help string carries the correction, and points at
`archive_objects_deleted_total` as the one that tells the two modes apart.

### A split doc comment, found on the way past

`download_slab_file` in `restore.rs` had been inserted into the middle of `object_layout`'s doc
comment, so the slab downloader's documentation opened with "Where a base object's bytes belong,
and how many of them there are" and `object_layout` was left carrying the sentence fragment "every
archive written before sparse objects existed reads." Both halves rejoined their own functions.
Nothing catches this: each comment is well-formed, `cargo doc` renders both without complaint, and
only reading them in order shows the seam.

## 2026-10-16 -- One lost object, and no way to say what it cost

`yesnod_archive_missing_objects` has been published for two days and the operator guide tells
people to page on any non-zero value. Reading the page it would produce, the number does not
answer the first question anyone asks next: **how bad is it.** The count is in objects, and the
unit the operator acts in is recovery points.

Those two units came apart the day base files became content-addressed pieces. A slab that three
generations left unchanged is named by all three and stored once, which is the whole point of
phase 0 -- and it means **one** missing object can cost every generation in the archive, while a
hundred missing objects can all belong to one old base nobody would ever restore. The counts are
identical in the first case and the second; the decisions are opposite.

Worse, it is not an attribution an operator can do by hand. A slab's key is its SHA-256, so the
key says nothing about who named it. Working out which generations are affected means fetching
every base manifest and expanding its extents -- which is exactly what the pass just did.

### `GcPlan::damaged_bases`

So the same walk that produces `missing` now also attributes it. A second pass over the retained
manifests, run only when `missing` is non-empty, collects the manifest key of every base naming at
least one absent object. The healthy case -- the one that runs every hour forever -- does no extra
work at all.

`one_lost_slab_costs_every_generation_that_named_it` builds the shape the feature exists for: three
generations, each with its own slab and one they share. Dropping the shared slab gives
`missing.len() == 1` and `damaged_bases.len() == 3`; dropping the oldest generation's own slab
gives `missing.len() == 1` and one damaged base. The two halves differ in no other number, which is
the argument for the field.

### The distinction the counts still cannot draw

Even `damaged_bases=2` does not say whether tonight's restore works. Holes in old history and a
hole in the base a restore picks by default are different emergencies. Both lists are sorted by
key, and for a base manifest key the term and generation are zero-padded to fixed width, so
lexicographic order *is* retention order and the last entry of each is the newest. When
`damaged_bases.last() == retained_bases.last()` the sidecar logs at error level, separately from
the warning that carries the counts. That comparison is the cheapest useful thing in this commit.

The gauge is `yesnod_archive_damaged_bases`, and the guide now says to compare it against
`yesnod_archive_retained_bases`: equal means no recovery point in the archive is usable.

### A correction carried into the code

Yesterday's entry recorded that a concurrent capture cannot produce a false `missing`. The
`gc::run` doc comment said the same thing, and it is wrong for the key-order reason set out in the
survey entry above: `slabs/` sorts before `term/`, so a listing can miss a capture's slabs and
still read the manifest that names them. The doc comment now states the race and why the command
accepts it while the sidecar's survey excludes it, and `backup.md` tells the reader to re-run the
command if it reports a loss during a base copy. A wrong reassurance in a doc comment is worse than
no comment, because the next person to touch the code will rely on it.

## 2026-10-16 -- Closing the loop: what a restore says when it meets the hole, and what a check exits with

Two ends of the integrity work were still loose, and both are about the moment the reporting leaves
this process.

### The restore said the least useful true thing

A restore reaching an object the archive no longer holds produced, verbatim:

```
Object at location /.../db/1111.../slabs/2222... not found: No such file or directory (os error 2)
```

Every word of that is true and it is close to the least helpful form the truth could take. A slab
is named by its SHA-256, so the key says nothing about **which file wanted it**, **where in that
file it belongs**, or **whether anything else is gone** -- and the first two are known only in the
frame that was fetching it. The third is known by `archive-plan` and nowhere else, which matters
more than it looks: a restore stops at the first absent object, so it reports one, and an operator
who fixes or works around that one simply meets the next. What they need is the set.

So both fetch paths now match `object_store::Error::NotFound` specifically and build a message
naming the base file, the byte range, the key, and the command that enumerates the rest. Other
fetch failures are left alone beyond the file name and range: they are transient and a pointer at
an integrity command would be a wrong diagnosis. `a_restore_that_meets_a_lost_object_says_what_wanted_it`
pins all four parts.

### A check that always exits zero is not a check

`backup.md` has been telling operators to run `yesnoctl archive-plan` after any change to bucket
lifecycle, replication or access policy. The obvious way to honour that is a cron or a CI step, and
until now the command printed its verdict and exited 0 no matter what it found.

It now exits **2** when the archive is not whole. Two is deliberate rather than one: a damaged
archive and an unreachable store are entirely different nights, and collapsing them into a single
non-zero code would make the check's only machine-readable output ambiguous in exactly the case it
exists for. One stays what it has always meant -- the command itself failed.

The verdict is `GcPlan::is_whole`, and what it leaves out is the interesting half.
`unrecognized` is excluded: an archive sharing a prefix with a newer writer has objects this build
does not classify, which deserves a look and must not turn a nightly check red, because a check
that goes red for a known-benign reason stops being run. `delete` is excluded because a pass having
work to do is the healthy case. `unreadable_bases` is included, even though it is not damage in the
same sense, because a pass that cannot read a manifest has not actually checked anything -- exiting
0 there would report "whole" on the strength of a survey that did not happen.

The plumbing is a small `Done { line, code }` in `yesnoctl`, so the four commands that have nothing
to say about exit codes say nothing: `Done::ran(line)`. Only the one command that answers a
question rather than performing an action sets a code. `code` is a `u8` converted at the process
boundary rather than an `ExitCode`, because `ExitCode` has no `PartialEq` -- a status nothing can
assert on is not a contract, and the mapping is the part worth pinning.

### A self-inflicted red gate, recorded because the cause is not obvious

The first gate run after this work failed in `Doc-tests yesno_e2e` with `can't find crate for
yesno_server_utils`. Nothing was wrong with the change. I had run a `cargo run -p yesno-server-utils`
smoke test **while the gate was running**, which rebuilt the crate under a new metadata hash and
left the doctest step linking against an rlib that no longer existed. The workspace has one shared
target directory on purpose, so a concurrent cargo invocation is not isolated from the gate -- it
waits on `.cargo-lock` for the build and then invalidates artifacts the gate has already resolved.
Run nothing else under cargo while a gate is running.

## 2026-10-16 -- A check whose two sides are both derived by code has to be tested against neither

`missing` compares what the retained manifests name against what the store holds. Both sides of
that comparison are produced by code in this repository: the uploader writes a slab to
`db/<uuid>/slabs/<sha256 of the contents>`, and the planner recomputes that key from the
manifest's extents. **Nothing forces the two derivations to agree**, and every test written so far
shares one of them.

The consequence is the kind of hole that outlives a release. If the derivations drift, `missing`
reports nothing for ever -- and every assertion in the suite still passes, because the only
end-to-end assertion about it is `base_dedup.py`'s `missing == 0` on a whole archive, which is
exactly what a check that can never fire also reports. The unit tests are no help either: they
build their archives from the same `slab_key` the planner calls.

So `archive_integrity.py` takes an object away from an archive the shipped sidecar actually wrote,
and requires the pass to notice. Two new harness verbs, both narrow:

- `srv_archive_lose_slab(dir)` deletes the lexicographically first slab object and returns its key.
  Deterministic, and it is the specific damage the whole feature exists for -- a lifecycle rule
  removing an object while the manifests that name it stay exactly as they were. Truncating a file
  or removing a manifest is a different failure.
- `srv_restore_error(dir, target, version)` returns the restore's error instead of raising it,
  following `srv_archive_second_error`. What is under test is what the operator is told.

The scenario asserts the archive is whole *before* taking anything, which turned out to be the
assertion that caught the mutation.

### What the sabotage showed

Flipping one bit of the hash in `plan`'s `keep_slabs` derivation does not make the holed archive
report two losses instead of one. It makes the **whole** archive report three, and the scenario
stops at the first check:

```
AssertionError: {'deleted': 3, 'retained_bases': 1, 'unrecognized': 0, 'missing': 3, 'damaged_bases': 0}
```

That is the drift failing loudly in the direction nobody would have predicted -- a false alarm on
a healthy archive rather than silence on a broken one -- and it is only visible because the
scenario establishes the healthy reading first. A test that went straight to the damaged case
would have seen `missing: 3` and passed its `>= 1`.

The same output shows an invariant worth naming: `missing` non-empty with `damaged_bases` zero is
unreachable in correct code, because every entry in `missing` came from some retained base's own
names. Here it appears because the attribution walk and the collection walk disagreed, which is
precisely the drift. It is left as an observation rather than an assertion in `src/` -- the
scenario covers the case, and a runtime check for a structurally impossible state is not worth the
line.

## 2026-10-16 -- A four-shard bootstrap of an empty database took 15 seconds, and none of it was the data

`follower-bootstrap-from-the-leader-is-serial-across-shards` has been open since 2026-10-14,
blocked on a judgement about the failover contract: parallelising the loop means hoisting
`check_leader` out of it, and that turns a per-shard fence into a per-bootstrap one. Before
spending that, I measured what the serial loop actually costs.

A four-shard leader, `repl_bootstrap` per shard, timed from the harness:

| database | per shard | four shards |
| --- | --- | --- |
| 24 ordinals | 3723, 3856, 4067, 3804 ms | 15 453 ms |
| 8 192 000 ordinals | 3756, 3675, 3677, 3645 ms | 14 755 ms |

**Raising the payload by a factor of three hundred and forty thousand changed nothing
measurable.** The transfer was never the cost, so parallelising transfers was never the fix. A
shard image is grown to a whole 1 GiB mapping segment at creation, and `stream_base_snapshot`
read every byte of it to find out which parts were holes.

### The comment that said not to do this also said when to

`stream_base_snapshot`'s own documentation had considered the fix and declined it:

> `SEEK_DATA` would let the leader skip *reading* the holes as well, and is not used: it needs a
> `libc` call and the unsafe rules that come with it, to save a scan whose cost is memory
> bandwidth on a one-off operation. **Revisit only if a leader's CPU during bootstrap is ever
> shown to matter.**

That is a well-formed decision -- it names its own falsifier -- and the falsifier had arrived. The
reasoning was sound and the premise was wrong in one specific way: "a one-off operation" is true,
and "whose cost is memory bandwidth" quietly assumed the cost scaled with the data. It scaled with
the *apparent* size, which for this file is a constant gigabyte whatever the database holds.

### What changed

`next_allocated` walks the file with `SEEK_DATA` / `SEEK_HOLE` and the loop reads only the runs it
reports, never past the end of the current run -- an image alternating small runs and holes would
otherwise read more than the old loop did. The all-zero test is kept rather than replaced: the two
filters compose, one skipping what was never written and the other what was written and is zero.
Any refusal from the kernel degrades to one run covering the whole remainder, which is exactly the
old behaviour, so a filesystem without hole support loses nothing. Only `ENXIO` from `SEEK_DATA`
means "nothing left".

| database | per shard, after | four shards, after |
| --- | --- | --- |
| 24 ordinals | 60, 60, 59, 58 ms | 239 ms |
| 8 192 000 ordinals | 57, 27, 22, 27 ms | 136 ms |

**15.5 seconds to 0.24.** The payload case is now faster than the empty one, which is the right
way round and was not true before.

### The unsafe, and what is tested

Two `libc::lseek` calls, which read and write no memory and move only the offset of a descriptor
this code opens for the probing and never reads from. The file is opened a second time
deliberately: `tokio::fs::File` tracks its own position and does its reads on the blocking pool,
so moving its descriptor's offset from underneath it would be interfering with it rather than
using it.

`every_written_byte_falls_inside_a_reported_run` asserts the property the whole thing rests on --
a run that is missed is a region the leader never reads and never ships, and the follower's image
is then quietly short in the middle. It deliberately does **not** assert that holes are excluded,
because a filesystem without hole support correctly reports one run covering everything, and that
assertion would be about the gate's filesystem rather than about this code. What exclusion buys is
measured above instead. Adding 1 to the reported start makes the test fail.

### Where the same cost still sits

The archive capture reads each shard image the same way for the same reason, and the comment in
`archive.rs` telling the reader not to reach for `SEEK_DATA` cited this decision -- so it has been
corrected rather than left pointing at a conclusion that no longer holds. That change is not this
one: the uploader also CRCs the whole file, holes included, so it would have to account for the
runs it skipped the way the restore side already does. Filed as
`archive-capture-reads-every-hole-it-does-not-upload`.

## 2026-10-16 -- The same gigabyte, read a second time by the capture

The entry above ends by filing `archive-capture-reads-every-hole-it-does-not-upload`. Measuring it
first, as before, because the point of the replication finding was that the shape of the cost was
not what the name suggested.

A four-shard leader holding **three ordinals**, timed from `srv_checkpoint` to the sidecar
publishing base generation 1:

```
capture ms 17341  files=9  object_bytes=12591104  apparent=4294975488
```

**17.3 seconds, 4.29 GB of apparent size, 12.6 MB of actual objects.** Worse than the bootstrap it
was split off from, for the same reason and in the same proportion.

### One helper, two callers, and where it lives

`next_allocated` moved out of `replication/mod.rs` into `yesno-server::sparse`, public. The
alternative was a second copy of two `libc` calls and their safety argument in
`yesno-server-utils`, which is one too many. `yesno-server-utils` already depends on this crate
and already takes the daemon's metric writers rather than restating them, so the direction is
established; nothing about "which parts of this file are there" is specific to replication.
`yesno-core` would be the other candidate and is the wrong one -- it owns the image *format*, and
this is about copying images.

### The cut does not move, only the reading

The uploader's loop is still whole `SLAB_OBJECT_BYTES` windows measured from offset zero, because
an extent boundary written by a capture is a window boundary a restore seeks to, and because the
dedup rests on that boundary matching the allocator's. Changing the cut to follow the filesystem's
runs would have been the natural-looking move and would have broken both. All the filesystem is
asked is whether a window is worth reading.

Two filters now compose where there was one: a window no run overlaps is never read, and a window
that is read is still dropped if its bytes are zero. They are not the same test -- a run of
*allocated* zeros passes the first and fails the second -- and keeping both is what makes the set
of uploaded objects identical to before. The measurement confirms it: `object_bytes=12591104`
before and after, to the byte.

A skipped window is still hashed, because the file's CRC covers its whole length.
`restore::crc_hole` already existed for the other end of exactly this and is now `pub(crate)`.

| | capture |
| --- | --- |
| before | 17 341 ms |
| after | 2 977 ms |

The existing tests turned out to be the right oracles and needed nothing added:
`a_sparse_file_uploads_only_its_slabs` already asserts the uploaded CRC equals the CRC of the
whole file read back, holes included, and its fixture builds genuine holes with `set_len` and a
seek. Removing the `crc_hole` call makes `an_entirely_empty_file_uploads_no_objects_and_still_has_a_length`
fail. One case was missing and is now covered: a file whose length is not a whole number of slabs,
with its only contents in the short final window -- which after this change is both the first
window read and a partial one.

### What is left, and a number that nearly misled me

`base_dedup.py` runs in 13.4 s, and I had it in mind as a 4-second scenario, so the first reading
looked like a regression. Rather than guess, I forced the old read-everything behaviour back in
place by pinning the run to the whole file and re-ran: **20.5 s**. The change made that scenario
35% faster too; the 4-second figure was simply wrong. Reaching for the measurement rather than the
explanation took two minutes and would have cost an afternoon the other way round.

What remains of a capture is mostly not I/O any more. With the `crc_hole` call removed entirely --
wrong, and for measurement only -- the same capture takes **899 ms**, so 2.1 of the remaining 3.0
seconds is hashing 4.28 GB of zeros at about 2 GB/s. `crc_hole`'s own comment predicted this and
named its falsifier the same way the other two did: "Revisit if a restore's apparent size ever
grows far past its real one." It is 4.29 GB against 12.6 MB. Filed as
`hashing-a-hole-costs-what-hashing-its-bytes-costs`, with the note that 2 GB/s may be the build
rather than the algorithm and is worth checking before anyone writes GF(2) matrix code.

## 2026-10-16 -- The numbers in the last two entries were debug builds, and the correction changes one conclusion

Before acting on `hashing-a-hole-costs-what-hashing-its-bytes-costs` I went to check its first
question -- whether 2 GB/s was the algorithm or the build -- and found the answer by reading
`crc_hole`'s own comment, which records **16.0 GiB/s on this host**. Eight times what I measured.
The difference is the profile: I ran every measurement in the last two entries with
`cargo run -p yesno-e2e`, and this repository's measurement fixtures use `--release`. TESTING.md
even says so, and says a host call costs 6.7 us in debug against 1 us in release.

So, re-measured in release, with the pre-change behaviour restored by pinning the run walk to the
whole file:

| | debug before | debug after | **release before** | **release after** |
| --- | --- | --- | --- | --- |
| four-shard bootstrap | 15 453 ms | 239 ms | **2 845 ms** | **239 ms** |
| four-shard capture | 17 341 ms | 2 977 ms | **5 079 ms** | **544 ms** |

Both changes stand, at 11.9x and 9.3x rather than 65x and 5.8x. The bootstrap's *after* figure is
the same in both profiles, which is the tell that it is no longer doing any work worth measuring
-- what is left is RPC and setup.

**The correction that matters is not the ratios, it is that debug flattered one conclusion and
would have misled the next.** `crc32c` was running at 2 GB/s because it was unoptimised, not
because it was taking a software path: the crate does use the AArch64 CRC32 instructions here
( `armsimd` is set for rustc >= 1.80, and the host advertises `crc` ). Had I not checked, the
filed item's first instruction -- check the build before writing GF(2) code -- would have sent the
next session to look for a missing target feature that was never missing.

### And then the item was worth doing anyway, for the other reason

In release, removing the `crc_hole` call entirely -- wrong, for measurement -- takes the capture
from 544 ms to 325. So 219 ms of it is hashing 4.28 GB of zeros, at about 19.6 GB/s, which agrees
with the recorded 16.0 GiB/s.

A fifth of a second is not what justifies changing it. What justifies it is that after this week
it is **the only term left that is proportional to what a database appears to hold** rather than
what it holds, and the apparent size is a whole mapping segment per shard. Measured at sixteen
shards, 17.2 GB apparent: **2 175 ms before, 1 194 ms after.** The saving is a second, and it
grows with the shard count while nothing else in a capture does any more.

### One line, and the oracle that makes it safe

`crc32c_combine( c, d, n )` is `shift( c, n ) ^ d`, the n-zero-bytes operator over GF(2). It is
already in this dependency, taken from zlib's reference implementation, so none of that arithmetic
is mine. CRC32C reports `R ^ !0` for its internal register and it is `R` the operator is linear
in, so the whole function is: take the inversion off, shift, put it back, with `d` zero because a
run of zeros contributes no message bits.

```rust
crc32c::crc32c_combine(checksum ^ u32::MAX, 0, length as usize) ^ u32::MAX
```

The old loop is kept, in the test, as the reference. `hashing_a_hole_agrees_with_hashing_its_zeros`
compares the two across twenty lengths chosen around where a shift-and-square walk goes wrong and
six checksums including 0 and `!0` -- the two an implementation that dropped an inversion could
still get right by accident. Dropping either `^ u32::MAX` makes it fail, along with the three
existing hole tests.

### A mistake worth recording about how I edited

The first attempt at this replaced a slice of the file between two anchors, and the closing anchor
-- a doc comment opening -- matched in the *test module* eight hundred lines below the intended
one. It deleted 1 612 lines, most of the restore path, and `cargo test` reported it as an
unbalanced delimiter in a test rather than as what it was.

The repair was to write `git show HEAD:...restore.rs` over that one file and re-apply the two
small edits by hand, which is a single-file write rather than `git checkout` or `git restore` --
those are forbidden here precisely because they reach the whole tree. The lesson is narrower than
"be careful": **a replacement bounded by two anchors is only as safe as the uniqueness of the
second one**, and an anchor that is a doc-comment opening is a poor choice because doc comments
repeat. Anchored single-string replacement with an asserted count of one, which is what the rest
of this session used, does not have this failure mode.

### Postscript, 2026-10-17 -- making a capture fast broke a scenario that was quietly timing it

The gate failed on `pitr_retention.py`, at `assert swept["deleted"] > 0`.

The scenario captures two base generations and then sweeps with a **one-second** retention
window, expecting generation 1 to go. A one-second window roots retention at the newest base
whose checkpoint is at least a second old -- so for generation 1 to be dropped, more than a
second of wall clock had to have passed since it was captured. That was reliably true only
because each capture was reading a gigabyte of holes per shard. With that gone the two
generations landed 482 ms apart, both were inside the window, and the pass correctly kept both.

Nothing was wrong with the reclamation. The scenario had an undeclared dependency on the
implementation being slow, and it had been a race since it was written -- one that a faster
machine would have found eventually. The sweep now uses a window of zero, which roots retention
at the newest base whatever the clock has done; `base_dedup.py` already roots the same way. The
alternative, waiting for generation 1 to age past a one-second window, means comparing the
harness clock against the server's commit clock, which `TESTING.md` says not to do.

Worth stating as a general thing, because this is the second time this week a performance change
has falsified something written down: **a test that passes because the code under it is slow is
a test with a hidden parameter**, and the parameter is not in the file. The two assertions either
side of this one -- a wide window keeps both roots, a narrow one keeps the newest -- are the
properties, and neither needed a clock.

## 2026-10-17 -- Putting the profile on the line that carries the number

Yesterday's correction was four measurements recorded as release numbers that were debug ones,
three to six times too slow. The entry explains what happened and the fix for those numbers. This
is the fix for the next four.

The runner printed `ok  base_dedup.py ( 13436 ms, 73 verb calls )`. Everything needed to
misread that is in the line, and everything needed to read it correctly is not. So the profile
now sits inside it: `ok  base_dedup.py ( 13436 ms debug, 73 verb calls )`, from
`cfg!( debug_assertions )`, on the failing line as well as the passing one.

**Not a banner at the top of the run, which was my first instinct and is the wrong shape.** What
travels out of a terminal into a journal entry is one line, cut from the middle of forty-four.
A banner is correct and absent exactly when it is needed. The rule generalizes: a label belongs
on the smallest thing someone will copy, not on the context they will leave behind.

Nothing parses this format -- `yesno-e2e/tests/scenarios.rs` prints its own line and deliberately
carries no duration, since a timing taken under `cargo test` is a timing of the gate's own
parallelism. `TESTING.md` now says to treat a `debug` figure as a ratio at best and never to
record one without saying so.

## 2026-10-17 -- The archive can say how long it has left before a lifecycle rule takes something live

Everything this week's integrity work added reports damage that has already
happened. `missing` is objects a retained base names and the store does not
hold; `damaged_bases` is what that cost in recovery points; the survey runs
with reclamation off so an archive nobody prunes still publishes both; the
exit code makes it a nightly check. All four fire **after** the object is
gone, and none of them is self-healing.

The hazard every one of them was built for is a bucket lifecycle rule keyed on
last-modified time, and content addressing is what makes that rule dangerous:
a slab a later base still contains is *named* rather than re-uploaded, so its
last-modified stops advancing while it stays live. That is written down in
`backup.md` and in the `missing` doc comment, and both of them then tell the
operator to watch for the aftermath.

### The reading that was already in hand

A pass takes one listing of the database prefix, and reachability from the
retained manifests already says which of those objects the archive needs. The
listing also carries each object's last-modified time, because an object-store
listing does. So the pass can report **the oldest object it is keeping**, and
that is exactly the number an expiry rule has to be set longer than.

It is free in the same sense `missing` is free: no extra round trip, one extra
field off a listing that was happening anyway. `GcPlan::oldest_live_object`,
the `oldest object still needed` line of `yesnoctl archive-plan`, the
`oldest_live=` field of the summary line the sidecar logs, and
`yesnod_archive_oldest_live_object_timestamp_seconds`.

### Four decisions in it, none of them obvious from the name

**It is not the oldest object in the bucket.** A healthy archive always holds
objects older than anything it needs -- superseded bases, spent WAL -- and
those are precisely what an expiry rule is welcome to take. A gauge read off
the listing alone would report one of them and tell an operator to lengthen a
rule that was doing its job. The test that pins this is
`the_oldest_object_kept_is_not_the_oldest_object_listed`: two objects older
than the answer, one about to be deleted and one unclassifiable, and deleting
the filter makes both it and the metrics test fail.

**Unrecognized keys are excluded although the pass does keep them.** It keeps
them out of caution rather than need. Somebody else's file lying in the prefix
since last year is not the archive's floor, and counting it would make the
number describe a thing no lifecycle rule on this data could damage.

**A timestamp, not an age.** An age gauge from a sidecar whose passes have
stopped keeps looking fresher than it is; a timestamp cannot. The alert is
`time()` minus the gauge against the operator's own retention setting, which
is a number this process has no way to know.

**Zero is withheld rather than published.** The gauge family is absent when
the pass kept nothing, for the same reason the other gauges are withheld until
a pass completes: a zero here reads as 1970, which is the most alarming value
the gauge can take, and it would page on an empty prefix.

### What it cannot see, and that is fine

Copying an archive resets every object's last-modified time, so a fresh
`rsync` or `aws s3 sync` reports a recent floor for slabs whose content has
not been rewritten in a year. That is not an error in the reading -- the
copy's objects really are young, and a rule on the destination really will
leave them alone for another window. It is worth stating in the doc comment
because the instinct is to read the number as the age of the *content*.

A pass halted by an unreadable manifest has an empty `delete`, so every
recognized object counts as kept and the floor it reports is as conservative
as the pass is. That is the right direction: a halted pass is not the moment
to claim a later floor than reachability can justify.

### The shape worth carrying

This is the third thing in a row that the same listing turned out to already
answer -- what is retained, what is absent, and now how long the absent one
has before it arrives. The general form: **a check that needs no new I/O is a
check whose cost argument is already settled**, so the only question left is
whether the answer is worth reporting. The reason to look for one is that the
expensive version of this reading -- a crawl that asks the store for every
object's age -- would have been declined on cost, and the question would have
stayed unanswered rather than being asked a cheaper way.

## 2026-10-18 -- "Is it there" was only half of the question the listing could answer

The integrity check asks whether every object a retained base names is in the
store. It has never asked whether what is there is the right length, and an
object that is present and short fails a restore exactly as completely as an
absent one -- later, in fact: the recovery point has been chosen and
everything before the short object fetched by the time the length disagrees.

The restore path has always known the rule. `download_slab_file` refuses a
slab whose bytes are not `extent.length`, and `download_base_file` refuses an
object that is not the length `object_layout` computes. Both messages were
already written. The survey had the other side of the comparison in hand and
was not making it: an object-store listing reports sizes, and a pass takes one
listing of the whole prefix.

So `GcPlan::wrong_length` is the same rule applied a day earlier, for nothing.
`yesnod_archive_wrong_length_objects`, a line in `yesnoctl archive-plan`, the
exit-code verdict, and the `damaged_bases` attribution, which now counts a
base that names a short object exactly as it counts one that names an absent
object -- the recovery point is gone either way.

### One rule, in one place

`restore::object_len` was extracted rather than reimplemented. Two spellings
of a length rule would be worse than either one: the survey would promise an
archive whole that the restore then refused, or the other way round, and the
disagreement would surface at the worst moment either way.

There really are two shapes to get right. A slab is `extent.length`. A file
stored whole is its own length when the object carries the holes, and its
extents' total when it does not, which is `sparse` deciding. Reading one
against the other would report every sparse archive as damaged or miss damage
in every pre-slab one. `a_sparse_object_is_measured_by_its_runs_and_not_by_
the_file` writes both numbers out as literals rather than deriving them from
the same helper the production path uses, because a shared derivation cannot
catch itself being wrong -- swapping `object_len` for `file.size` fails only
that test.

### The failure this actually catches

A PUT is atomic, so the store that wrote the archive does not serve half an
object it accepted. An interrupted `rsync` or `aws s3 sync` of the archive to
somewhere else does exactly that, and leaves an archive whose listing is
complete and whose keys are all where they should be.

Which is the same operation yesterday's entry had to carve an exception for:
copying an archive resets every object's last-modified time, so the floor
reported by `oldest_live_object` describes the copy. Both entries are about
the same event seen from two sides, and it is worth saying once: **a copied
archive is a different archive**, and the two readings that come free off a
listing are precisely the two a copy can falsify.

What this does not do is check the bytes. A length that agrees is not a
checksum, and the CRC the restore verifies is the authority there. It is not
free, so it is not here.

### The signature change, and what it bought in the fixtures

`gc::plan` took `&[String]` and now takes `&[Listed]` -- key, size and
last-modified, which is what a listing is. Eighteen call sites in the tests
moved with it, and the fixtures now have to state a size for every object they
list. That is stricter than it sounds in a useful way: a fixture that lists the
wrong number reports damage it did not mean to build, and the first run after
the change said so twice.

`oldest_live_object` also moved inside `plan`, which now has the listing it
needs. It had been filled in by the wrapper, so a caller of the pure function
got a half-populated plan back; `planned` still recomputes it after an
unreadable manifest clears the deletions, because clearing them widens what
the pass is keeping.

### Left for the next increment

The end-to-end verb surface reports `missing` and `damaged_bases` and does not
yet report this. Filed as `e2e-does-not-exercise-a-wrong-length-object`; the
premise is covered here by a test over a real `file://` store and the shipped
pass, and adding the verb means both gates, which is its own increment rather
than a tail on this one.

## 2026-10-18 -- Following the length check out to the end of the chain

The library half landed earlier today and left the end-to-end surface
reporting only presence. This finishes it, and the shape is the one
`archive_integrity.py` already argues for: both sides of the comparison are
derived by code, nothing forces the two derivations to agree, and an archive
the shipped uploader wrote is the only thing that can catch them drifting.

`srv_archive_shorten_slab` takes **one byte** off the lexicographically first
slab object. One byte rather than half of it, deliberately: a check comparing
against anything rounded -- a slab count, a count of whole windows -- would
pass a half-sized object about as often as it failed one, and would pass this
never. It is the same object `srv_archive_lose_slab` deletes, which is what
lets the scenario assert `lost == short` and makes the two reports comparable
rather than two unrelated runs. The scenario now walks whole, then short,
then absent, asserting `wrong_length` and `missing` move independently --
`"an absent object has no length"` is an assertion, not a comment.

### One thing the library change had left asymmetric

A restore meeting an absent object says what wanted it and points at
`yesnoctl archive-plan` for the rest of the damage. A restore meeting a short
one said two numbers and stopped. The operator is in the same position in
both cases -- a restore stops at the first object it cannot use, so the
message says nothing about how much else is wrong or which recovery points
survive -- so the pointer belongs in both, and `restore::wrong_length` now
carries it. The sentence still says which damage it met, because the next
move differs: a short object is usually an interrupted copy that can be
re-run, and an absent one has to be found somewhere else.

### A repeat of the editing mistake from 2026-10-16, caught the same way

Inserting the new restore test, I anchored on "the last doc-comment line
before the test function" and the walk stopped in the middle of an existing
doc comment, splicing the new test between "and the first two are" and
"known only here." It compiled and all thirty-one tests passed, because a
doc comment split across an inserted item is still two valid doc comments.

The October 16 entry's lesson was that a replacement bounded by two anchors
is only as safe as the uniqueness of the second. This is the adjacent one:
**a computed anchor is only as safe as the invariant it computes against**,
and "the doc comment ends where a line stops starting with `///`" is not an
invariant when the block you are walking back through is itself a run of
`///` lines. Reading the inserted region rather than trusting the green test
run is what caught it; the repair moved the test whole to after the function
that owned the comment.

## 2026-10-18 -- The third and fourth places that moved a gigabyte of holes, and why fixing one of them did nothing

Two sites were fixed on 2026-10-16: the replication snapshot sender and the
archive capture's uploader. Both read every byte of a 1 GiB-per-shard sparse
image to find out which parts were holes. The sweep stopped there, and it
should not have: the control plane's `FetchSnapshotFile` does the same thing,
and it is worse, because those bytes go on the wire. It serves
`yesnoctl basebackup` always, and the archive sidecar whenever the co-located
direct path is unavailable -- which is every remote sidecar.

Underneath it, the portable snapshot provider was copying each database file
with `tokio::io::copy`. That cost the apparent size **three** times: read
here, written there, and then kept, because what it produced was a fully
allocated file that every later reader of the snapshot paid for again.

### The measurement that makes the entry worth writing

Four shards, `basebackup.py`, release, three runs each:

| copy | wire | ms |
| --- | --- | --- |
| dense | dense | 11 979 / 13 581 / 13 946 |
| dense | **sparse** | 14 108 / 14 809 / 15 073 |
| **sparse** | dense | 3 294 |
| **sparse** | **sparse** | 796 / 804 / 805 |

**The second row is the finding.** Teaching the wire to skip holes, on its
own, changed nothing -- and not because the change was wrong. There were no
holes left to skip. The provider had filled them in one step earlier, so by
the time the file reached the streamer it really was four gigabytes of
allocated zeros and the streamer was right to send them all.

Measured one at a time, in the order they were written, the first change
looks like a mistake. It is not: it is half of one, and the half that cannot
be measured until the other half lands. Worth keeping as a general caution --
**an optimization that depends on an upstream property is unfalsifiable until
that property holds**, and the measurement will say "no effect" rather than
"not yet".

Together they are 13 500 ms to 800 ms, about seventeen times, on a database
holding sixty-four keys.

### The negotiation, because this one is a wire format

The replication change needed no negotiation: leader and follower ship
together. This one does not have that, since `yesnoctl` is a command an
operator runs and may be older than the daemon. So the hole-skipping is a
capability the **caller** declares -- `allow_sparse` on the request, beside
the `allow_direct_path` that was already there for the same kind of reason.
A client that does not set it is served every byte, exactly as before.

It also changes what a chunk means: offsets may now jump, so a receiver has
to place chunks rather than append them. Both receivers in this tree size the
file first and seek, and their continuity check became an ordering check --
chunks must not go backwards and must not overrun the file, which still
catches a server rewriting history mid-stream. The stream ends with an empty
marker chunk at the file's length, because the last byte of a sparse file can
be inside a hole that nothing sends, and a file that is nothing but holes
sends no data chunk at all.

### The guard that had to be rebuilt rather than kept

The old copy verified the byte count afterwards: `copied != source.size` was
how a file that had shrunk since its size was recorded got caught. A walk
that skips holes cannot use that test, and the failure is silent in the worst
direction -- a file that has lost its tail reports no further allocated runs,
which is indistinguishable from a file that ends in a hole, so the walk just
stops and publishes a snapshot quietly short of what its lease promises.
`read_exact` catches a file that shrank *inside* a reported run; a length
check after the loop catches the rest. Both are tested.

### Where the sweep stands now

Four sites found, four fixed: the replication sender, the archive uploader,
the control plane's file stream, and the portable provider's copy. The
remaining consumer of a whole image is the restore path, which already writes
holes as holes.

One thing measured and not improved: a capture-heavy scenario
( `base_dedup.py`, 1 600 ms ) is unchanged by the copy walk, because its cost
was never the reading. The all-zero filter already kept the *writes* sparse;
it was `tokio::io::copy` materializing the destination that cost, and that
scenario's captures do not go through it.
