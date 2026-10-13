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
