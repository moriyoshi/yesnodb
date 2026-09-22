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

Audited on 2026-09-18 against the actual contents of `.agents/docs/LTM/` and `.agents/docs/TODO.md`, entry by entry rather than by trusting an earlier record. Every journal section listed below has its durable decisions, constraints, evidence, and open follow-ups in the linked topic documents or in the backlog; the consolidated source entries and the superseded record blocks were then removed by `reconcile-journal-ltm`. This is the single canonical record -- earlier record sections have been merged into it rather than left to accumulate.

The 2026-09-18 pass added two topic documents, for investigations that spanned too many entries to sit inside an existing topic:

| Document | What it holds |
|---|---|
| [Checkpoint and Commit Lock Contention](LTM/checkpoint-and-commit-lock-contention.md) | The whole stall investigation and the work that came out of it: the page-fault and CPU hypotheses and the counters that killed them, the shard-invariance reasoning error, the exclusive-hold anatomy, `build_updating` and `leaf_spans`, the commit-path hoists and the staleness guard, commits-own-the-tail, the conserved-total knob family, and the parallelism arithmetic. |
| [SIMD Arch Arms and Kernel Selection](LTM/simd-arch-arms-and-kernel-selection.md) | The x86 ports and what they cost to get right: emulation as a correctness-only instrument, `pcmpestrm` over the rotate ladder, AVX2 accepted for bitmap and rejected for array, the three NEON dead ends and the instruction-set rule behind them, SVE preconditions, and how a crate-level number is earned. |

**To-do extraction added nothing on that pass.** All 24 slugs those entries name were already filed in `.agents/docs/TODO.md` with the correct open or closed state, including `macos-build-is-not-gated`, `bitmap-and-reads-both-payloads-in-full`, `nothing-compares-gate-sh-to-ci-yml`, `workspace-tests-run-default-features-only`, `codec-error-unknownkind-has-no-producer`, `parallel-per-shard-commit-apply`, the SVE re-open precondition, and the `O( dirty )` path-copy occupancy tradeoff.

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

### Synthesis Documents

| Synthesis document | Source topic documents |
|---|---|
| [Representation, Format, and Space](LTM/representation-format-and-space-synthesis.md) | `container-representations-and-roaring-compatibility.md`; `storage-format-index-and-zero-copy.md`; `compression-models-and-space-economics.md` |
| [Set Evaluation, Planning, and Packed Lenses](LTM/set-evaluation-and-planning-synthesis.md) | `set-algebra-kernels-and-cardinality.md`; `chunk-stream-contracts-and-lazy-operators.md`; `expression-planning-statistics-and-segmentation.md`; `packed-lenses-matrix-bignum-and-views.md` |
| [Durability, Reclamation, Replication, and Snapshot Concurrency](LTM/durability-reclamation-and-concurrency-synthesis.md) | `storage-format-index-and-zero-copy.md`; `allocation-reclamation-and-fsck.md`; `wal-mvcc-durability-and-concurrency.md`; `network-service-replication-and-operations.md`; `postgresql-extension-and-query-pushdown.md` |
| [Backup, Snapshot, and Cloud Operations](LTM/backup-snapshot-and-cloud-operations-synthesis.md) | `backup-archive-and-pitr.md`; `snapshot-leases-providers-and-privilege-separation.md`; `kubernetes-operator-and-failover.md`; `network-service-replication-and-operations.md` |
| [Testing, Gates, and Measurement](LTM/testing-gates-and-measurement-synthesis.md) | `testing-and-e2e-harness.md`; `quality-gates-and-project-tooling.md`; `measurement-and-investigation-methodology.md` |
| [System Boundaries, Services, and Integrations](LTM/system-boundaries-and-integrations-synthesis.md) | `milestones-and-system-boundaries.md`; `database-apis-and-satellite-crates.md`; `network-service-replication-and-operations.md`; `postgresql-extension-and-query-pushdown.md`; `client-libraries-and-search-integrations.md` |

### Standalone Source Topic Documents

These source topics are intentionally not folded into a synthesis document:

- [Formal Model and Proof Obligations](LTM/formal-model-and-proof-obligations.md)
- [Context Eligibility Product Direction](LTM/context-eligibility-product-direction.md)
- [Checkpoint and Commit Lock Contention](LTM/checkpoint-and-commit-lock-contention.md) ( created 2026-09-18 )
- [SIMD Arch Arms and Kernel Selection](LTM/simd-arch-arms-and-kernel-selection.md) ( created 2026-09-18 )

Open follow-ups remain in [`.agents/docs/TODO.md`](TODO.md). See [`.agents/docs/LTM/INDEX.md`](LTM/INDEX.md) for the maintained memory index and preserved research-source inventory.

---

## 2026-09-19 — a consumer's `view_count` declined, because the premise that made it immune to measurement is false

The `haiiie` bit-sliced prescription's last surviving part, re-specified as `OrdSet::view_count( &View ) -> Vec<OrdSet>` and argued on the one ground no benchmark could touch: that `view_fold` computes a per-ordinal count and `Reduce::keep` discards it, so a **capability** is being thrown away rather than a kernel being slow.

Checked rather than accepted, with a throwaway crate under `.agents-workspace/tmp/view-count-probe`. **The count is already reachable from the shipped public API** -- a ripple-carry add of each constituent's `view_select` indicator into an accumulator of planes, using only `and` / `xor` / `is_empty`. Verified at 8 constituents over a 200 000-ordinal span, both layouts: 154 285 logical ordinals' counts exact against a count oracle built from the constituents, and `Any` / `All` / `Parity` all reproduced from the planes.

So what is discarded is one **walk**, not the capability, and the ask is a ratio after all: 4 - 8x on `Interleaved`, and **1x on `Blocked`**, where `fold_via_select` never forms a count and an in-crate arm would be the downstream loop character for character. Ratios are what retired the other three parts, where a model predicting 3.2 - 4.0x measured 1.26 - 1.52x.

Declined. The decisive fact is that the proposer states it has no caller, which is `stats.rs` again. Three findings kept: the proposal's own refusal of a `Reduce::Plane( j )` variant is correct and for the right reason ( the enum is closed on monoids, and `Plane( j )` for `j > 0` cannot fold pairwise without carrying ); `Vec<OrdSet>` has a data-dependent length the specification does not address; and the cohort-overlap use case wants a `count >= t` threshold, which the proposal itself says dominates the planes and then declines to request.

Full reasoning, the construction, and the reopening conditions are in [Packed Lenses](LTM/packed-lenses-matrix-bignum-and-views.md) and in the closing addendum to `bit-sliced-lens-proposed-by-a-consumer` in [`TODO.md`](TODO.md). No change to `yesno-core`.

---

## 2026-09-19 — `sets` was the third amplification vector, and the fix belongs in two places for two different reasons

Found while planning the typed expression language, not while looking for it. `yesno-wire`'s module header says "This is a parser of untrusted bytes" and caps `MAX_DEPTH` and `MAX_NODES` against a small payload buying large work. **`ViewSpec.sets` is a `u32` that was capped at nothing**, and it does not appear in either bound: a descriptor is a fixed 13 bytes whether it declares 8 constituents or four billion, while the evaluator loops over every one.

* `view::fold::fold_via_select` ran `for i in 0..v.sets()`, a `view_select` each.
* `view::fold::expand_generic` ran that loop **per input ordinal** — cardinality x sets.

Measured, not argued: a `ViewExpand` of **four ordinals** under `blocked( u32::MAX, 2^56 )` took **231.37 s**. The same request with one ordinal took 57.88 s. Both are ~30-byte payloads reachable from any client on the network.

**The fix is deliberately not the same in the two crates, and the plan said to mirror it.** In `yesno-wire` it is a cap -- `MAX_VIEW_SETS = 4096`, enforced inside `ViewSpec::check()`, which the decoder already calls, so an oversized descriptor is refused before anything evaluates it. In `yesno-core` a cap would have been **wrong**: `View::check()`'s doc states it deliberately does not bound `sets`, because "a view can be legally declared whose upper constituents are unaddressable" and that is reported per ordinal rather than per descriptor, mirroring `Packing::check`. That rationale is about **addressability** and it is correct. The defect is about **work**. So core got bounds on the loops instead, and the documented stance survives.

Two bounds, because they answer different questions and neither subsumes the other:

* `View::addressable_sets( x )` — how many leading constituents can address logical ordinal `x` at all. `ordinal_of` is monotone in the constituent under both layouts, so the addressable ones are a prefix. Used by `expand_generic`.
* `occupied_sets( set, view )` — how many can hold any of this set's ordinals. The tighter bound when a `Blocked` stride is small enough that every constituent is addressable and almost all are empty. Used by `fold_via_select`.

**`All` is the arm truncation could have broken.** `Any` and `Parity` take an empty constituent as their identity, so stopping early drops nothing; an intersection does not, so a truncated loop must answer empty rather than return the union of what it visited.

**The test I wrote first could not fail, and the sabotage said so.** Reverting `expand_generic` to `0..v.sets()` left it **passing, in 57.88 s** — because the bounded and unbounded loops return byte-identical results. `ordinal_of` rejects the extra constituents one at a time, so no output, cardinality or allocation count distinguishes them. **Time is the only observable, which is exactly what makes the defect a defect**, so the test now carries four input ordinals and a 10 s deadline. Re-run against the same sabotage it fails in 231.37 s with "the loop is running to `sets` again". The margin is ~20x below the broken cost even in release and about six orders of magnitude above the correct one.

The lesson generalises past this fix: *"the two implementations return the same answer"* is the reason a value assertion cannot see the bug, not a reason the bug is minor. When the only difference is cost, assert cost — and prefer a wall-clock deadline with a stated margin over a silent pass.

---

## 2026-09-19 — the set-expression language becomes multi-sorted, and the three view leaves become compositions

Step 2 of the typed-expression-language plan. `yesno-wire` gains a second sort, `VecSetExpr` -- a fixed-arity vector of sets -- and the three special-case view nodes are **removed** rather than kept, because nothing has shipped:

```text
ViewSelect { key, spec, i }   ->  view( key( k ), shape )[ i ]
ViewFold   { key, spec, r }   ->  fold( view( key( k ), shape ), and | or | xor )
ViewExpand { input, spec }    ->  expand( input, shape )
```

**The gain is that these stop being leaves.** `ViewSelect` and `ViewFold` took a bare `u64`, so only a *stored key* could be viewed or folded. `fold( view( and( key( a ), key( b ) ), shape ), or )` was unreachable at any cost and now round-trips.

**Sorts are checked while decoding, not by a separate pass**, because the decoder already knows which sort each position requires. The two tag ranges deliberately share **one** tag space so a node in the wrong position reports `SortMismatch { expected, tag }` naming both sides, rather than being reinterpreted as whatever that byte means where it landed. In Rust the sorts are separate types, so an ill-sorted tree does not compile and the decoder checks only what arrives as bytes; the Python, Go and Java clients mirror that with a separate base class, sealed interface, and sealed interface respectively.

**Two checks are static that used to be dynamic or absent.** A vector's arity is always known -- from the descriptor for `view`, from the literal length for `[ .. ]` -- so an out-of-range index is refused at decode ( `IndexOutOfRange` ), and `pack`'s arity must equal the descriptor's `sets` ( `ArityMismatch` ). Both are decidable from the text alone, so the CLI parser refuses them too rather than sending them.

**`fold`'s operator is named for the Boolean operation being folded** -- `or` / `and` / `xor`, not `any` / `all` / `parity`. `∩` across the list and `∧` down each fibre are the same operation, so the operator's own name makes the pairwise implementation evident. The set is closed at three by Proposition 27 and the enum records why, including why `iff` and `andnot` are the two that cannot be added.

**The fusion obligation is the part that is easy to get wrong.** A naive lowering of `view( e, s )` builds a `Vec[Set]` by calling `view_select` `n` times, so spelling the old nodes as compositions would have been a **performance regression against the nodes they replaced** -- `OrdSet::view_fold` has a single-walk `Interleaved` arm that materializing `n` constituents throws away. `yesno-flight/src/expr.rs` therefore pattern-matches `fold( view( .. ) )`, `view( .. )[ i ]` and their counting form, and `lower_vec` carries a comment saying it must not be reached from those paths.

**`SetExpr::keys` now recurses instead of reading a field**, since the view nodes no longer name a key. A caller using it to find what a query touches gets *more* keys than before, never fewer.

**A cross-implementation wire vector is now pinned in all four languages** -- the same 34-byte hex for `view( key( 9 ), interleaved( 3 ) )[ 1 ]` in the Rust, Python, Go and Java tests. Five implementations of one format drift silently otherwise: each round-trips against itself while disagreeing with the others, and only a shared constant catches that. The constant was taken from the Rust encoder after a hand-computed one proved a byte short in the stride field.

**`yesno-pg` is outside the cargo workspace and `cargo build --workspace` did not catch it.** It matched the three removed variants in `qual.rs` twice and rendered them for EXPLAIN in `scan.rs`. That is the "neither gate subsumes the other" rule arriving in practice rather than in the abstract.

---

## 2026-09-19 — `map`, the hole, and the sorts that made a facet query expressible

Step 3 and 4 of the typed-expression plan, completing it. The language now has five sorts -- `Set`, `VecSet`, `Int`, `VecInt`, `Bool` -- and the query the whole redesign existed for works end to end:

```text
map( view( key( 9 ), interleaved( 3 ) ), cardinality( and( _, key( 7 ) ) ) )  ->  [ 3, 2, 3 ]
```

Per-cohort counts under a filter, checked against a `BTreeSet` oracle. That is the **row** marginal, and no fold and no per-ordinal count can produce it -- which is the thing a consumer's prescription got wrong earlier the same day, and the reason the sorts exist at all.

**`Vec[Bool]` is deliberately not a sort.** A truth value per constituent *is* a subset of the constituent indices, so `map( v, contains( _, x ) )` yields a `Set`. Giving it a separate sort would describe one object twice. That keeps the lattice closed at five and every index statically checkable.

**The hole is scoped at decode, not at evaluation.** `_` outside a map body is refused, and a `map` inside a body is refused rather than allowed to shadow -- refusing is one check in five implementations where de Bruijn indices would be a binder discipline in five. A `map` in the *vector* position is **sequential rather than nested** and must still decode; that distinction is easy to get backwards and is asserted in both directions in every implementation.

**A bug the tests found, and the fix that generalises it.** Tag 22 in a set position reported `UnknownTag` while being a `SortMismatch` everywhere else, because each of five decoders enumerated the other sorts' tags by hand. One `sort_of_tag` table now backs every fallback, pinned by a test asserting the tags are contiguous from zero and that each has exactly one sort.

**A stub I nearly shipped.** Go's first `validateIntVector` ignored its own argument and validated a dummy expression instead. It compiled, and every test would have passed while the integer-vector sort went entirely unvalidated. Caught on re-reading; the walker now exposes an entry point per sort and shares one node budget, so a mixed-sort tree cannot exceed the cap by splitting across sorts.

**Java has no compiler on this host** -- JREs only, so `./gradlew` fails on a missing `JAVA_COMPILER`, and an earlier "EXIT=0" for it was meaningless because the shell captured `head`'s status rather than Gradle's. `yesno-e2e:local` carries a JDK at `/opt/java/openjdk`, so the client was compiled `-Xlint:all -Werror` **and run** in that image. Its output includes the 34-byte cross-implementation hex, now identical in Rust, Python, Go and Java -- the only thing that catches five codecs drifting while each round-trips happily against itself.

**`gate-pg.sh` earned its place twice over.** It failed with three non-exhaustive matches in `yesno-pg` that **no cargo command can see**, because that crate is outside the workspace and built only by Bazel. One of the four sites fixed was inside `#[cfg(test)]` and so was not even reached by the cdylib build that reported the others. The re-run then died fetching Bazel's pinned LLVM toolchain -- `Unknown host: release-assets.githubusercontent.com` -- and while that lasted the exhaustiveness was confirmed instead by lifting the three non-test functions **verbatim** into a scratch crate over `yesno-wire` and compiling them against the real enums. A third run, once the network returned, **passed**: PostgreSQL 17 and 18 fixtures and `yesno-pg` rustfmt. `gate-search.sh` ( 3 scenarios ), the Go client gate and the Python client gate are green too, so every gate the `QUALITY_GATE.md` table requires for a `yesno-wire` change has run.

**Two process lessons, both of which cost a run.** A Docker gate snapshots the tree at build time, so editing while one runs produces a result describing a checkout that never existed -- one `gate-pg` run had to be discarded for exactly that. And `cargo test --workspace` exceeds a single 600 s foreground call, so it has to be split by package when background execution is unavailable.

---
## 2026-09-20 — Test plan: fail-closed and prefix-bounded key streams

### Failure classes

| Class | Concrete failure | Layer |
|---|---|---|
| Error becomes omission | A B+tree cursor error is skipped while building a posting plan, so an empty or truncated stream is returned as success | `tests/durability.rs`, corrupting a real persisted index node before opening the stream |
| Wrong contents or visibility | Prefix bounds are applied after MVCC resolution, mask the `2^48` endpoint, lose a tombstone, or include a chunk outside the requested interval | `tests/durability.rs`, comparing old and new persisted snapshots with mixed container shapes against a `BTreeSet` oracle |
| Stream-contract divergence | Counting, seeking, reader eviction, or a stream outliving its `Snapshot` behaves differently for the bounded constructor | Existing `KeyStream` conformance assertions instantiated through the new public constructor |
| Silent planning decay | A narrow constructor builds the whole key's metadata plan and filters it afterwards | `tests/allocation.rs`, comparing requested allocation bytes for a narrow plan with a fresh full-key plan |
| Deferred payload error is swallowed | A plan opens successfully but a corrupt standalone extent is treated as an absent chunk during iteration | `tests/durability.rs`, corrupting a real persisted payload and reading it through `next_chunk` |

### Planned tests

- `tests/durability.rs::a_prefix_bounded_key_stream_matches_persisted_mvcc_oracle` -- mixed array, bitmap, and run chunks; persisted terminal prefix; whole-chunk deletion, gap insertion, partial replacement, old/new snapshots, maximum key, empty/full/invalid bounds, counting, and seek below the lower bound.
- `tests/durability.rs::a_corrupt_index_node_payload_fails_the_checksum_scan` -- require both full and bounded plan construction to return the real index-reader error.
- `tests/durability.rs::a_corrupt_standalone_payload_is_refused_by_the_read_path` -- require payload failure to surface when either stream advances.
- `tests/allocation.rs::a_prefix_bounded_key_stream_does_not_plan_the_whole_key` -- assert that planning memory follows the requested chunk interval rather than total key width.

### Generators

The persisted oracle fixture deliberately creates all three container kinds and uses a fixed LCG to choose repeatable prefix intervals around disk-only, overlay-only, contested, tombstoned, absent, and terminal chunks. Uniform `u64` ordinals would produce only one-element array chunks and would leave both container diversity and boundary prefixes untested.

### Deliberately not covered

This pass does not optimize `KeyStream::seek`. Its target search is logarithmic, but retiring skipped disk steps is linear because `disk_remaining` is maintained by repeated `advance()`. The prescription's cached-plan timings are consistent with that observation but do not isolate it, and a cumulative-count representation adds plan metadata and construction work. That optimization needs its own measurement before it changes the plan representation.

---

## 2026-09-20 — expression-level view maps pay for extraction before they pay for their terminal

Measured step 1 of the view-expression performance plan through the real
`yesno_flight::expr` evaluator, using a disposable standalone crate under
`.agents-workspace/tmp/view-expr-baseline-20260920`. No instrument entered
production source.

The main fixture had 131 072 logical ordinals, 55% density in each constituent,
and identical logical sets packed as interleaved or chunk-aligned blocked views.
The database was measured from its resident memtable, then checkpointed, closed,
reopened, verified, and measured from a new snapshot. The matrix also carried
10/50/90% filters, a three-level map body, array and run fixtures, all three fold
operators, a membership map, and `At` over a mapped vector. Release build,
repository Arrow 59.2.0 lock, rustc 1.97.1, aarch64 Cortex-X925 / Cortex-A725,
commit `85a41c2bf34f7134df8eaf30a26265ceee6d6eba`. Five timing batches per row,
the whole process repeated twice, operands black-boxed, result checksums equal
between resident and reopened snapshots, and the allocation counter calibrated
with a one-allocation 128 KiB vector.

The headline cardinality map:

```text
sets    interleaved    blocked aligned    allocations ( interleaved / blocked )
   4        4.72 ms            1.39 us                         230 / 34
   8       14.07 ms            2.48 us                         449 / 57
  16       47.12 ms            4.90 us                        884 / 100
```

The interleaved heap peaks were 1.25, 1.32, and 1.45 MiB. At eight
constituents every terminal sat on the same ~14 ms / 1.32 MiB floor:
unfiltered cardinality, cardinality under a 10/50/90% filter, a three-level
body, OR/AND/XOR fold of the mapped vector, a membership map, and selecting one
mapped element. Allocations distinguished their extra work ( 449 through 1 082 )
while time did not. That is the answer to the causal question: `lower_vec`
extracts all constituents before the terminal runs, and extraction dominates
everything above it. The index-of-one case is the clearest witness because it
builds seven results the caller cannot observe.

Representation changed the magnitude, not the conclusion. Eight-way
interleaved versus blocked cardinality maps were 39.5 us / 2.53 us for arrays,
8.32 ms / 2.56 us for runs, and 14.07 ms / 2.48 us for bitmaps. Reopening left
interleaved bitmap time near 14 ms while the blocked row rose to 5.80 us and 106
allocations, showing that stored decoding becomes visible only once repeated
extraction stops dominating. The page cache was warm, so this is not a cold I/O
number. Exact decoded chunks were deliberately left unreported: the public API
has no counter, and adding a production observation hook for this one-off study
would violate the repository's research-code rule.

The backlog's evidence gate is therefore satisfied, but the result points to a
narrower first implementation than a core lazy `Expr` node: fuse batched
cardinality and membership, demand-drive `At( Map(..), i )`, and fuse mapped
sets into their fold. A core node remains open until those paths are measured
and until statistics, seek behavior, streaming cardinality, planner bounds, and
termination are specified together.

---

## 2026-09-20 — stage 2 fuses view-map terminals before materialization

Stage 2 replaced the measured extraction floor with terminal-specific paths.
`OrdSet::view_cardinalities` now counts every interleaved constituent in one
packed scan, while blocked views retain their range-count path. The Flight
evaluator demand-drives `At` through sequential maps, batches direct and
intersection-filtered cardinality maps, evaluates membership maps as scalar
Boolean expressions, and distributes OR, AND, and XOR folds over supported
intersection-shaped map bodies. Invariant operands are prepared once as
reopenable lazy expressions instead of being collected once per constituent.
Unsupported arbitrary map bodies keep the existing eager fallback.

The same disposable fixture used for the step-1 baseline measured the resident
interleaved bitmap cardinality map at 0.646, 1.285, and 2.576 ms for 4, 8, and
16 constituents, down from 4.72, 14.07, and 47.12 ms. Allocations fell from
230/449/884 to 13/16/19, and peak requested heap fell from 1.25/1.32/1.45 MiB
to 2.1/3.7/7.0 KiB. At eight constituents, a 50% filtered cardinality map took
2.491 ms, OR/AND/XOR mapped folds took 2.101/1.691/1.914 ms, membership took
1.46 us, and `At` over the filtered map took 1.701 ms. Resident and reopened
checksums remained equal. The construction and wider representation matrix are
recorded in `LTM/packed-lenses-matrix-bignum-and-views.md`.

Regression coverage includes semantic tests for batched view counts, filtered
facets, all mapped fold operators, invariant membership, rank, and demand-driven
indexing. A Flight allocation regression compares 4 and 64 constituents for
the fused terminal classes; its budgets were introduced with the implementation
and no existing allocation allowance was raised. No wire format, persisted
format, codec, container invariant, or unsafe code changed.

### Quality Gate — stage 2 view-expression terminal fusion

- `cargo clippy --workspace --all-targets --all-features -- -D warnings`: pass.
- `cargo +stable clippy --workspace --all-targets --all-features -- -D warnings`: pass.
- `cargo fmt --check`: pass.
- `cargo test -p yesno-core`: pass.
- `cargo test -p yesno-flight`: pass.
- `./scripts/gate.sh`: pass on the clean rerun, including the full scenario corpus. The first run had one load-sensitive failure in the unrelated `yesno-server` test `an_idle_database_still_checkpoints`; that exact test passed in isolation before the required full rerun passed.
- `./scripts/gate-pg.sh`: pass for the default PostgreSQL major and PostgreSQL 18, including unit and hermetic regression fixtures.
- `./scripts/gate-search.sh`: pass; application, OpenSearch, and Elasticsearch scenarios all passed.
- Invariant audit: the generic expression fallback remains the semantic oracle, `roaring` remains dev-only, Arrow types did not enter `yesno-core`, and the `arrow-buffer` containment boundary is unchanged.

---

## 2026-09-20 — fail-closed and prefix-bounded key streams

`KeyStream` plan construction now propagates B+tree cursor errors instead of
turning an unreadable index entry into an omitted chunk. A real persisted index
corruption reproduced the old false-success path before the one-line `?` fix and
now fails both full and bounded construction. Standalone payload errors remain
deferred until the affected chunk is advanced, and tests pin that distinction.

`Snapshot::key_stream_prefix_range` plans only the requested half-open chunk
prefix interval. Both the persisted B+tree walk and the MVCC memtable walk are
range-bounded; `2^48` is the exclusive endpoint, invalid bounds are refused, and
empty ranges preserve snapshot liveness checks. The persisted oracle covers old
and new snapshots, tombstones, overlays, all container kinds, the terminal
prefix, maximum keys, counting, seeking, and stream lifetime after the snapshot
handle is dropped.

The allocation regression compares requested bytes for an eight-chunk plan with
a 4 096-chunk plan. Deliberately replacing the bounded walk with a full-key walk
made it fail at 665 312 bounded bytes versus 666 416 full bytes. Deliberately
removing the disk bound made the persisted oracle return five chunks where one
was requested. Both sabotages were reverted before the passing gate. Existing
plan seek retirement remains deliberately deferred under
`keystream-seek-retires-skipped-steps-linearly` until an isolated cached-source
benchmark justifies changing plan metadata.

### Quality Gate — fail-closed and prefix-bounded key streams

- `cargo clippy --workspace --all-targets --all-features -- -D warnings`: pass.
- `cargo +stable clippy --workspace --all-targets --all-features -- -D warnings`: pass.
- `cargo fmt --check`: pass.
- `cargo test -p yesno-core`: pass, including 915 unit tests and all integration and doc-test layers.
- `./scripts/gate.sh`: pass on the final rerun, including the full scenario corpus. An earlier run reached its final formatting check and found a concurrent untracked Flight test before that separate work was formatted.
- `./scripts/gate-pg.sh`: pass for the default PostgreSQL major and PostgreSQL 18, including unit and hermetic regression fixtures.
- `./scripts/gate-mysql.sh`: pass, including the C ABI, native Flight client, pinned server build, and hermetic regression fixture.
- `./scripts/gate-search.sh`: pass; application, OpenSearch, and Elasticsearch scenarios all passed.
- `./yesno-c/gate.sh`: pass.
- Invariant audit: no persisted or wire format changed, no unsafe code was added, `roaring` remains dev-only, and the `arrow-buffer` containment boundary is unchanged.

---
## 2026-09-20 — Test plan: pointwise Boolean view maps

Stage-3 measurement varied the remaining eager shapes directly. On the same
resident interleaved bitmap fixture, union, both difference directions, a
repeated-hole Boolean body, and rank over union took about 4.75 / 14.1 / 46.7 ms
at 4 / 8 / 16 constituents, with 272-1,409 allocations and about 1.2 MiB peak
requested heap. The common cause is again constituent extraction.

### Failure classes

| Class | Concrete failure | Layer |
|---|---|---|
| Wrong contents | The false/true hole decomposition reverses a difference arm, mishandles a repeated hole, or applies the invariant base count per constituent incorrectly | Flight expression test against an independent `BTreeSet` oracle |
| Rank boundary | Rank includes `x`, omits zero, or clips only one decomposition arm | Flight expression test comparing every returned rank with the oracle's strict `< x` count |
| Silent decay | A pointwise Boolean cardinality or rank map falls back to `view_select` once per constituent | Flight allocation regression comparing 4 with 64 constituents |
| Untested by construction | Only monotone `and( _, q )` bodies run, leaving the false/true branches identical | Deterministic cases spanning union, both difference directions, repeated holes, static bodies, and rank limits |

### Planned tests

- `expr::tests::pointwise_boolean_maps_agree_with_an_independent_set_oracle` — compare cardinality and rank vectors for interleaved and blocked views against `BTreeSet` substitution.
- `tests/expression_allocation.rs::view_terminals_do_not_allocate_once_per_constituent` — extend the existing 4-versus-64 budget with union cardinality, repeated-hole cardinality, and rank.

### Generators

No new randomized generator is needed. The Boolean truth-table cases are
deterministic so both values of the hole and both sides of difference are
guaranteed to occur; the packed fixture contains distinct constituent sets and
the invariant sets overlap only partially.

### Deliberately not covered

Non-pointwise transforms inside a map body, such as `select( _, n )`, retain the
eager fallback. Their result cannot be derived from per-ordinal false/true
substitution. Blocked views retain their near-free selection fallback; the
allocation contract targets the measured interleaved extraction regression.
---

## 2026-09-20 — stage 3 fuses pointwise Boolean cardinality and rank maps

The remaining measured map bodies did not justify a core lazy view node. A
pointwise Boolean body is completely determined at each ordinal by its value
with the constituent hole absent and present. Calling those expressions `f0`
and `f1` gives the exact identity:

```text
|f(H)| = |f0| + |H intersect (f1 minus f0)| - |H intersect (f0 minus f1)|
```

Flight now prepares the invariant expression tree once, derives those two
expressions lazily, and uses one interleaved packed walk to count the positive
and negative filters for every constituent. Rank restricts every term to its
strict half-open prefix and stops the walk at the bound. The existing direct
and intersection-specialized paths remain first. Blocked layouts and
non-pointwise bodies decline to the established eager fallback.

On the extended 131 072-ordinal dense fixture, union cardinality moved from the
old 4.75 / 14.1 / 46.7 ms extraction floor to 2.128 / 3.027 / 4.470 ms at
4 / 8 / 16 constituents. Repeated-hole cardinality measured
2.228 / 3.113 / 4.525 ms, and midpoint rank measured
1.062 / 1.498 / 2.213 ms. Allocation counts became nearly flat with arity:
72 / 75 / 78 for union, 160 / 163 / 166 for the repeated-hole body, and
103 / 106 / 109 for rank. The 16-way peak requested heap was 19.6, 37.5, and
19.9 KiB respectively instead of about 1.2 MiB. Resident and checkpoint/reopen
checksums agreed, and the allocator positive control observed one 128 KiB
allocation.

The new integration oracle checks union, both difference directions, a static
body, a repeated hole, and strict rank boundaries against independent
`BTreeSet` substitution under both layouts. The allocation regression compares
4 with 64 constituents for union cardinality, repeated-hole cardinality, and
rank. Its distinguishing power was checked deliberately: reversing the
positive and negative terms made the oracle report union `[4, 5, 4]` against
`[10, 9, 10]`; bypassing the fused interleaved path made allocation counts
grow 209 -> 3,225, 302 -> 4,386, and 221 -> 3,161. Both mutations were restored
and the focused tests passed.

No `yesno-core` source, planner, persisted format, wire format, or unsafe code
changed. The lazy-view backlog remains partial only for genuinely non-pointwise
map bodies without a measured caller.
### Quality Gate — stage 3 pointwise Boolean view maps

- `cargo clippy --workspace --all-targets --all-features -- -D warnings`: pass.
- `cargo +stable clippy --workspace --all-targets --all-features -- -D warnings`: pass.
- `cargo fmt --check`: pass.
- `cargo test -p yesno-core`: pass, including 915 unit tests and every integration and doc-test layer.
- `cargo test -p yesno-flight`: pass, including the new independent oracle and allocation regression.
- `./scripts/gate.sh`: pass, including workspace tests, off-by-default features, the complete scenario corpus, layout and documentation checks, planner termination audit, derived-figure audit, and formatting.
- `./scripts/gate-pg.sh`: pass for the default PostgreSQL major and PostgreSQL 18, including unit and hermetic regression fixtures.
- `./scripts/gate-search.sh`: pass; application, OpenSearch, and Elasticsearch scenarios all passed.
- Correctness-layer audit: pointwise Flight lowering is checked against an independent `BTreeSet`; blocked views exercise the existing fallback; no oracle was weakened and no proptest seed was removed.
- Allocation-layer audit: the 4-versus-64 regression has a fixed 16-allocation output allowance and fails by thousands when the fused path is bypassed.
- Invariant audit: no container kernel, prefix bound, planner rewrite, persisted format, wire format, or `cardinality_dyn` implementation changed. No unsafe block or runtime dependency was added; `roaring` remains dev-only and Arrow types remain outside `yesno-core`.

---

## 2026-09-20 — Test plan: general pointwise mapped folds

The remaining measured `fold( map( view, f( _ ) ), op )` path still extracts
and materializes every constituent unless `f` is an intersection with invariant
sets. On the 131,072-ordinal interleaved bitmap fixture, union, right-difference,
and repeated-hole bodies took 4.78-4.83 ms at four constituents,
14.33-14.44 ms at eight, and 47.12-47.59 ms at sixteen across OR, AND, and XOR.
Allocations grew from 354-469 through 698-929 to 1,375-1,852, with
1.25-1.45 MiB peak requested heap. The construction verified the database after
checkpoint/reopen and obtained identical resident and reopened checksums.

### Failure classes

| Class | Concrete failure | Layer |
|---|---|---|
| Wrong fold truth table | All-absent, all-present, or mixed constituent states use the wrong `f0` / `f1` branch | Flight expression test against an independent `BTreeSet` oracle |
| Parity error | Even and odd view arities apply the invariant `f0` term with the wrong parity | Flight expression test covering both arities and XOR |
| Body restriction | Union, either difference direction, a static body, or a repeated hole silently falls back or returns wrong contents | Deterministic independent-oracle cases for all three folds |
| Silent decay | General pointwise mapped folds resume extracting one set per constituent | Flight allocation regression comparing 4 with 64 constituents |

### Planned tests

- `tests/pointwise_view_folds.rs::pointwise_mapped_folds_agree_with_an_independent_set_oracle` — compare OR, AND, and XOR folds under interleaved and blocked layouts, at odd and even arities, with direct `BTreeSet` substitution and reduction.
- `tests/expression_allocation.rs::pointwise_mapped_folds_have_bounded_allocation_growth` — compare 4 and 64 constituents for union, invariant-minus-hole, and repeated-hole bodies across the three fold operators.

### Sabotage plan

- Swap the all-present and mixed terms in one truth-table identity and confirm the independent oracle fails on contents.
- Bypass the general fusion while keeping the old intersection specialization, then confirm the new allocation regression fails by arity.

### Deliberately not covered

Non-pointwise map bodies such as `select( _, n )` retain the materializing
fallback. A per-ordinal two-value truth table cannot represent an operation that
depends on the constituent's global order statistics.

---

## 2026-09-20 — stage 4 fuses general pointwise mapped folds

Stage 4 extends mapped-view fold fusion from the intersection-shaped special
case to every direct pointwise map body. The evaluator prepares the map body
twice, with the view hole bound to the empty set and to the universe, then
combines those two invariant sets with one packed fold over the mapped inputs.
The exact formulas are:

- OR: `( f0 \ All ) ∪ ( f1 ∩ Any )`
- AND: `( f0 ∩ f1 ) ∪ ( f0 \ Any ) ∪ ( f1 ∩ All )`
- even XOR: `Parity ∩ ( f0 △ f1 )`
- odd XOR: `f0 △ ( Parity ∩ ( f0 △ f1 ) )`

The earlier intersection specialization remains first because it needs only
one packed accumulator. Bodies that use order-sensitive operations such as
`select` still take the eager fallback; no new expression node or container
kernel was needed.

An independent `BTreeSet` oracle now covers union, both difference directions,
static bodies, and repeated-hole bodies for OR, AND, and XOR at odd and even
arities and with interleaved and blocked layouts. Allocation tests cover
representative union, difference, and repeated-hole bodies for all three folds.
Temporarily disabling the general fusion made the allocation regression fail at
4,195 allocations for 64 inputs versus 268 for four inputs, and temporarily
substituting an intersection-shaped OR formula made the semantic oracle fail,
demonstrating that both test layers catch their intended regressions.

On the resident 65,536-key corpus, 16-way general mapped folds fell from about
47.1-47.6 ms before the change to about 6.08-6.10 ms for OR and AND and 3.04 ms
for XOR. Sixteen-way allocation counts fell from 1,375-1,852 to 131-253 for OR
and AND and 114-162 for XOR. Resident and reopened checksums agreed. OR and AND
retain about 1.2 MiB peak live allocation because they hold both `Any` and
`All`; XOR is about 0.65 MiB.

### Quality Gate — stage 4 pointwise mapped folds

- `cargo clippy --workspace --all-targets --all-features -- -D warnings`: pass after replacing a verbose test function-pointer type with a local alias.
- `cargo +stable clippy --workspace --all-targets --all-features -- -D warnings`: pass.
- `cargo fmt --check`: pass.
- `cargo test -p yesno-core`: pass, including all 915 unit tests and the integration layers.
- Focused `yesno-flight` pointwise-fold oracle and allocation tests: pass.
- `./scripts/gate.sh`: pass, including the complete end-to-end scenario corpus and documentation/layout checks.
- `./scripts/gate-pg.sh`: pass for the default PostgreSQL target and PostgreSQL 18, including unit and regression suites.
- `./scripts/gate-search.sh`: pass for the application, OpenSearch, and Elasticsearch scenarios.
- Invariant audit: no container-kernel, codec, wire-format, dependency, or unsafe-code changes; non-pointwise expressions retain the existing eager semantics.

---
## 2026-09-21 — Test plan: mapped-select folds

The only measured mapped-view fold still taking the eager fallback is
`fold( map( view, select( _, n ) ), op )`. On the 131,072-ordinal, 55%-dense
interleaved fixture, a midpoint selection over 4 / 8 / 16 constituents took
about 4.79 / 14.31 / 47.40 ms for OR, AND, and XOR, with 308-347 /
611-715 / 1,214-1,374 allocations and 1.25 / 1.32 / 1.45 MiB peak requested
heap. A disposable one-pass prototype produced identical resident and reopened
checksums in about 0.30 / 0.61 / 1.21 ms, with 6-25 allocations and 1-4 KiB
resident peak heap.

### Failure classes

| Class | Concrete failure | Layer |
|---|---|---|
| Wrong selected ordinal | The scan treats `n` as one-based, advances the wrong owner's counter, or misses a chunk boundary | Flight expression test against an independent `BTreeSet` oracle |
| Wrong fold semantics | Duplicate selected ordinals are deduplicated for XOR, retained for AND when one constituent is empty, or lost from OR | Independent oracle covering all three fold operators |
| Layout divergence | The physical traversal assumes interleaved order and returns wrong values for blocked views | Independent oracle covering interleaved and blocked descriptors |
| Silent decay | The terminal resumes materializing one set per constituent | Flight allocation regression comparing 4 with 64 constituents |

### Planned tests

- `tests/mapped_select_folds.rs::mapped_select_folds_agree_with_an_independent_set_oracle` — derive each constituent's nth ordinal from independent `BTreeSet` values, then reduce OR, AND, and XOR across common, distinct, and missing selections under both layouts.
- `tests/expression_allocation.rs::mapped_select_folds_have_bounded_allocation_growth` — compare 4 and 64 interleaved constituents for all three fold operators with a fixed output-size allowance.

### Sabotage plan

- Change the selection comparison from `count == n` to `count + 1 == n` and confirm the independent oracle fails.
- Bypass the mapped-select fusion and confirm the 4-versus-64 allocation regression fails by arity.

### Deliberately not covered

This stage recognises the direct `select( _, n )` body whose workload was
measured. Selection over an arbitrary transformed hole retains the eager
fallback because its ordering can depend on invariant sets and has no measured
caller. No core lazy-view expression node is introduced.

---

## 2026-09-21 — Test plan: composed cardinality-map normalization

The external expression-math prescription identifies an equivalent spelling
that misses the existing view terminal:
`map( map( V, f( _ ) ), cardinality( _ ) )`. On its 4,096-feature sparse
fixture, normalizing that spelling to
`map( V, cardinality( f( _ ) ) )` changed the time for two count vectors from
57.28-59.26 to 8.34-8.53 microseconds at 16 documents, 567.15-573.57 to
25.60-28.32 microseconds at 64, and 7,535.73-7,553.95 to 75.47-79.04
microseconds at 256.

### Failure classes

| Class | Concrete failure | Layer |
|---|---|---|
| Wrong composition | Direct and nested spellings return different count vectors | Flight expression test against an independent `BTreeSet` oracle |
| Hole capture | Normalization substitutes through another binding or rewrites a non-identity outer body | Binding-scope case whose outer cardinality operand is not a bare hole |
| Body loss | Static or repeated-hole inner bodies are simplified incorrectly | Independent oracle covering both shapes and empty constituents |
| Silent decay | The nested spelling resumes materializing every mapped constituent | Flight allocation regression pairing direct and nested forms at 4 and 64 constituents |

### Planned tests

- `tests/nested_view_maps.rs::nested_cardinality_maps_match_direct_maps_and_an_independent_oracle` — compare direct and nested spellings for union, both difference directions, static, and repeated-hole bodies under both layouts, including an empty constituent.
- `tests/nested_view_maps.rs::normalization_does_not_capture_a_non_identity_outer_body` — evaluate a nested map whose outer cardinality applies an additional intersection; a rewrite that treats that operand as a bare hole must fail.
- Extend `tests/expression_allocation.rs` to compare nested and direct intersection-cardinality maps at 4 and 64 constituents with a fixed allocation allowance.

### Sabotage plan

- Disable normalization and confirm the nested allocation regression fails while the semantic oracle stays green.
- Broaden the match from `cardinality( Hole )` to an arbitrary cardinality operand and confirm the binding-scope oracle fails.

### Deliberately not covered

This stage moves only the identity cardinality terminal across one set-map
binding and then redispatches. Rank, contains, selection, arbitrary
substitution, and cross-request source sharing retain their current execution
and require separate semantic and resource evidence.

---

## 2026-09-21 — stages 5 and 6 close mapped selection and composed cardinality gaps

Stage 5 recognises `fold( map( view, select( _, n ) ), op )` and finds every
constituent's nth logical ordinal in one physical packed-set walk. OR, AND, and
XOR then reduce at most one ordinal per constituent without constructing the
constituent sets. The implementation uses `View::logical_of` for both layouts,
so Flight does not duplicate the checked view-addressing arithmetic.

On the resident 131,072-ordinal, 55%-dense fixture, 4 / 8 / 16-way midpoint
selection folds fell from about 4.79 / 14.31 / 47.40 ms to
0.371 / 0.741 / 1.49 ms. At 16 constituents allocations fell from 1,214-1,374
to 21-39 and peak requested heap from about 1.45 MiB to 7.1 KiB. Reopened
checksums matched. The independent oracle spans common, distinct, and missing
selections, in-range and past-end indices, both layouts, odd and even arities,
and every fold operator. An off-by-one mutation returned empty instead of
`{4}`; bypassing fusion made the allocation test fail at 275 -> 3,490.

Stage 6 implements the expression-math prescription's first step: an outer
identity `cardinality( _ )` map moves through one inner set-map binding and the
result is redispatched to the existing terminal chooser. The match is
deliberately limited to a bare hole. It introduces no wire node, arithmetic
operator, core expression node, or count kernel.

The prescription's 4,096-feature sparse fixture measured the nested spelling at
57.28-59.26 / 567.15-573.57 / 7,535.73-7,553.95 microseconds for
16 / 64 / 256 documents and normalize-then-current at
8.34-8.53 / 25.60-28.32 / 75.47-79.04 microseconds. Locally, the allocation
test measured direct 4/64 forms at 27/27 allocations and nested forms before
normalization at 256/3,604. Direct, nested, and independent `BTreeSet` answers
agree for both layouts and the body shapes already supported by the pointwise
terminal. Broadening the guard to arbitrary cardinality operands made the
binding-scope test return `[8, 8, 5]` rather than `[5, 4, 3]`; the exact
guard is restored.

### Quality gate

- `cargo clippy --workspace --all-targets --all-features -- -D warnings`
  passed with the default toolchain and with stable.
- `cargo fmt --check` and the focused semantic and allocation tests passed.
- `./scripts/gate.sh` passed the complete workspace, property, differential,
  durability, Flight, server, scenario, documentation, and structural suite.
- `./scripts/gate-pg.sh` passed the default PostgreSQL major and PostgreSQL 18,
  including unit tests and hermetic regression fixtures.
- `./scripts/gate-search.sh` passed the application, OpenSearch, and
  Elasticsearch scenarios.
- No core container, codec, persistence, wire, dependency, or unsafe code was
  changed. No oracle or allocation budget was weakened.

---

## 2026-09-21 — SIMD exploration found a packed-view arm, not a general word-loop arm

A disposable crate under `.agents-workspace/tmp` measured explicit NEON on the
native Cortex-X925 performance core. Seven alternating repetitions black-boxed
the operands, checked every candidate against current public operations, and
kept the production tree untouched.

Plain matrix XOR and OR have no case: release assembly already contains paired
128-bit loads, vector `eor` / `orr`, and paired stores, and explicit NEON was
0.99-1.03x from 64 through 16,384 words. Fused AND-plus-popcount does retain a
wide-row case. A real 64xK by Kx64 `counted_mul` comparison crossed current code
near K=1,024 bits, then reached 1.38x at 4,096 and 1.44x at 16,384. It regressed
small shapes as far as 0.43x, so width dispatch is part of the result.

The high-value case is an interleaved view whose physical chunks are bitmaps.
For four 55%-dense constituents over 262,144 logical ordinals, current
`view_cardinalities` took 1.361 ms, a byte LUT 59.46 us, and NEON 5.361 us.
Current-to-NEON is about 254x, while the SIMD-only increment is the 11.1x from
the LUT to NEON; the rest comes from replacing one iteration per set bit with a
fixed-width bitmap traversal. Any / All / Parity folds, including output-set
construction, improved 3.27x / 17-22x / 4.97x. The shared raw fold kernel was
5.30x faster than the LUT, but dense output construction hid most of that gain
for Any and Parity.

No implementation landed. A production arm still owes per-container dispatch,
array/run and generic fallbacks, unaligned-buffer handling, output chunk seams,
arity-specific 2/4/8 kernels, oracle and sabotage coverage, and a separately
measured x86 implementation. SVE remains inapplicable: this host's vector length
is 16 bytes and the intrinsics remain outside the stable MSRV.

---
## 2026-09-21 — native intersection counts dominate the next expression opportunity

The expression-math prescription's native-count reference was rerun against the
current `epic` Flight evaluator rather than its pinned baseline copy. The full
24-case matrix passed its independent set oracle across array, bitmap, and run
containers, both layouts, and query supports 32 / 512 / 4,096. A second
single-terminal control separated native traversal from the prescription's
two-plane source sharing. Both runs were pinned to Cortex-X925 CPU 5; the single
control used seven alternating repetitions of three evaluations.

For one direct intersection-count map over 4,096 sparse interleaved documents,
32 selected features took a median 314.36 us through current evaluation and
26.66 us through coalesced bounded streams plus native range visits, an 11.8x
gain. With all 4,096 features selected the medians were 450.12 and 212.23 us,
only 2.1x, confirming that a selective arm needs a full-scan dispatch sibling.

Blocked layouts expose the larger omission because their direct-intersection
recogniser currently declines and the fallback extracts every constituent. One
count map over 512 dense bitmap rows fell from 20.61 ms to 16.95 us, about
1,216x; 512 run rows fell from 5.02 ms to 11.75 us, about 428x. The native
control still allocated an unused second output vector and cloned the first, so
these numbers do not depend on multi-result sharing and modestly penalise the
candidate.

The implementation boundary is not another Boolean identity. Interleaved rows
need query-driven physical windows and `key_stream_prefix_range` for a direct
persisted key. Blocked chunks need representation-native array assignment,
bitmap AND-popcount per aligned row, and run/query rank differences. A checked
core chunk accumulator should own those container details while Flight owns
source recognition and stream dispatch. The current full walk remains the
oracle, and unaligned buffers, partial rows, mixed kinds, read failures,
snapshots, tombstones, address overflow, and selective/full-scan crossover all
remain required before landing. No production code changed during this study.

---

## 2026-09-21 — Test plan: scalar blocked bitmap intersection counts

Stage 8a improves the existing direct
`map( view, cardinality( and( _, filter ) ) )` terminal for blocked views before
adding SIMD. The current blocked path declines the direct recogniser, extracts
every constituent, and evaluates the same invariant intersection once per row.
The proposed core helper instead consumes each packed chunk once. Aligned bitmap
rows use scalar word-wise AND-popcount; every other shape retains an exact
ordinal fallback.

### Failure classes

| Class | Concrete failure | Layer |
|---|---|---|
| Wrong owner | Chunk-prefix or row arithmetic adds a count to the adjacent constituent | `proptest_oracle.rs` against per-row `BTreeSet` intersections |
| Wrong word mask | A row begins or ends on the wrong word, or the filter's last word leaks bits | The same boundary-biased property with a forced bitmap and a partial final chunk |
| Mixed-container divergence | A bitmap chunk is correct while the partial array/run tail is skipped or double-counted | Core property requiring at least one bitmap while preserving the generic tail |
| Persistence divergence | Store-backed bitmap words differ from the resident mutable representation | Flight expression test before checkpoint and after reopen against one independent oracle |
| Silent decay | Flight resumes constructing one `OrdSet` per constituent | Allocation regression with equal physical span at 4 and 64 constituents |

### Planned tests

- `tests/proptest_oracle.rs::blocked_view_intersection_cardinalities_match_btreeset_oracle` — build 17 dense 4,096-bit rows so the first chunk is necessarily a bitmap and the seventeenth row is a partial tail, then vary data and filter phases and compare every count with independent row sets.
- `yesno-flight/tests/blocked_intersection_counts.rs::blocked_bitmap_intersection_counts_match_an_independent_oracle_before_and_after_reopen` — exercise the exact wire expression over resident and persisted data, including multiple invariant intersection operands.
- `yesno-flight/tests/expression_allocation.rs::blocked_bitmap_intersection_counts_do_not_materialize_constituents` — hold the physical span constant while changing 4 rows of width 65,536 to 64 rows of width 4,096, so per-chunk costs stay fixed and only per-constituent materialization can make allocation grow.

### Sabotage plan

- Shift the bitmap row owner by one and confirm the core and Flight oracles fail.
- Bypass the blocked direct terminal and confirm the 4-versus-64 allocation regression fails.

### Deliberately not covered

This stage does not add interleaved query-driven streams, a specialised array or
run kernel, sibling terminal sharing, SIMD, or a new wire operation. It also does
not promise a dispatch crossover: aligned blocked bitmap rows are the measured
first target, and all other shapes retain the generic exact path.

---

## 2026-09-21 — Stage 8a lands scalar blocked bitmap intersection counts

The direct intersection-cardinality map now accepts blocked views. Flight lowers
the invariant filter once, while core counts every constituent without extracting
one set per row. When a blocked stride is word-aligned and exactly tiles a chunk,
core constructs one query word mask and performs scalar AND-popcount over each
bitmap row. Other containers, mixed tails, non-tiling rows, and interleaved views
retain the exact ordinal walk. No explicit SIMD, unsafe code, wire operation, or
core expression variant was added.

Pinned to Cortex-X925 CPU 5, the same 512-row dense bitmap production terminal
moved from a 20.61 ms median before the change to 13.47 us after it, about 1,530x.
The checked-in core benchmark measured 5.329 us for the grouped scalar arm and
16.126 ms for the public select-plus-`and_cardinality` oracle, about 3,027x. The
run fixture also moved from about 5.02 ms to 1.27 ms because the fallback now
walks the packed set once, but that is not a native run kernel.

The boundary-biased core property forces a bitmap chunk plus a partial non-bitmap
tail and compares ordinary, empty, and full filters with independent `BTreeSet`
intersections. Flight checks the real expression before checkpoint and after
reopen. The equal-four-chunk allocation test changes only the row count from 4
to 64; bypassing the direct terminal produced 72 -> 2,256 allocations and failed
its fixed allowance. Shifting bitmap ownership by one made both semantic oracles
fail, then restoring the correct owner made the focused and full local suites
pass.

Remaining Stage 8 work is deliberately unchanged: interleaved query-driven
bounded streams, selective/full-scan dispatch, native array and run traversal,
sibling sharing, and SIMD. The SIMD bitmap candidate now has a smaller insertion
point: replace the scalar row reduction while preserving this arm's checked
layout conditions and exact fallback.

---

## 2026-09-21 — Test plan: complete native intersection-count traversal

The remaining Stage 8 work shares several failure modes, so it is tested around
one checked multi-filter chunk accumulator rather than as unrelated Flight
shortcuts. Resident sets and persisted bounded streams must feed identical chunk
semantics. Full-scan and selective interleaved traversal must be separately
forceable in core tests before production dispatch chooses between them.

### Failure classes

| Class | Concrete failure | Structural catcher |
|---|---|---|
| Selective clipping | A logical row crossing a chunk boundary skips or repeats its boundary bits | Core property forces odd arities and row/chunk seams, then compares selective and full arms with independent per-row sets |
| Window coalescing | Adjacent query rows open overlapping streams and count a payload twice | Counter rejects non-ascending or duplicate prefixes; a direct-key persisted oracle uses coalesced windows before and after reopen |
| Native kind drift | Array membership, bitmap masks, or run rank differences disagree | Forced array, bitmap, run, and mixed-kind fixtures compare the native batch with public select-and-count |
| Query-plane aliasing | A value present in two filters updates only one vector | Two-filter oracle includes low-only, high-only, overlap, duplicates at expression construction, empty, and full filters |
| Dispatch inversion | Broad support remains on selective traversal or sparse support resumes a full ordinal scan | Forced strategies plus a work counter; the 32 / 512 / 4,096 support benchmark records resident and snapshot arms before fixing the coverage rule |
| Persistence error loss | Tombstones, eviction, or a payload read failure becomes a partial vector | Existing bounded-stream liveness tests remain authoritative; Flight propagates every stream error and adds resident/reopened result checks |
| Lost sharing | Two sibling terminals resolve and traverse the same source twice | Batch allocation/work regression compares one two-filter batch with two independent calls and checks both complete vectors |
| SIMD tail/alignment | A vector loop drops a tail word or assumes aligned storage | Scalar and SIMD reducers are directly compared across every tail length and store-backed unaligned bitmap coverage; disabling the vector arm preserves semantics |

### Sabotage

- Remove selected-row boundary masking and require the odd-arity core oracle to fail.
- Feed one coalesced prefix twice and require the accumulator's ordering check to fail.
- Replace run interval rank differences with the physical interval length and
  require the sparse-filter run oracle to fail.
- Disable batching and require the source-work regression to observe two
  traversals.
- Shift one SIMD query pointer by a word and require scalar equivalence to fail.

The existing eager evaluator remains the expression oracle. No test may loosen
an allocation allowance or substitute current output for the independent set
construction. Arithmetic scoring, a new wire opcode, cross-RPC sharing, and an
unbounded view arity remain outside this stage.

---

## 2026-09-21 — Stage 8 completes native view intersection counts

The remaining intersection-cardinality work now uses one checked core
`ViewIntersectionCounter` for resident chunks and persisted streams. Interleaved
views choose between a full scan and coalesced selected-prefix windows; blocked
views keep the Stage 8a bitmap path and add native run splitting. Array, bitmap,
and run containers each use representation-native counting while retaining the
generic ordinal walk as the exact oracle. Flight recognises direct key-backed
expressions, opens bounded streams for selective traversal, and exposes an
explicit batch API that shares one source traversal across compatible sibling
maps. It adds no wire opcode, unsafe code, or expression variant.

Pinned to Cortex-X925 CPU 5, the production expression benchmark changed from
314.36 us to 33.30 us for interleaved support 32 (about 9.4x), from 450.12 us to
391.58 us for full support 4,096 (about 1.15x), from 20.61 ms to 13.14 us for
blocked bitmap (about 1,568x), and from 5.02 ms to 15.66 us for blocked run
(about 320x). A runtime NEON reducer was also measured on the blocked bitmap
kernel: 6.478 us versus 5.605 us for the restored scalar reducer, 15.6% slower.
The vector arm and its unsafe code were removed rather than shipped.

The boundary-biased core property exercises odd arities, sparse and dense
dispatch, multiple filters, and chunk seams against independent `BTreeSet`
rows. Forced core tests cover both strategies and every native representation;
Flight covers the direct persisted expression before checkpoint and after
reopen; and the allocation regression proves one batch is cheaper than two
independent calls. Sabotaging the selective lower clip by one made the property
fail on its first generated case, then restoring the boundary made all focused
and full suites pass.

`scripts/gate.sh`, `scripts/gate-pg.sh`, `scripts/gate-mysql.sh`,
`scripts/gate-search.sh`, and `yesno-c/gate.sh` all pass. The PostgreSQL gate
covered its default major and PostgreSQL 18; the database and search gates ran
their hermetic regression fixtures.

---

## 2026-09-21 — Quality Gate: Stage 8 native view intersection counts

### Result: PASS

The stable workspace Clippy gate with every target and feature, workspace
format checks, all `yesno-core` tests, the complete cargo gate, both PostgreSQL
majors, MySQL, OpenSearch, Elasticsearch, and the standalone C ABI gate pass.
The new behavior is covered at the oracle, persisted-expression, allocation,
and benchmark layers. Public APIs and their module rationale are documented,
and no codec, serialized format, container mutation invariant, Arrow buffer
boundary, or wire protocol changed.

Clippy identified manual saturating arithmetic during implementation; it was
replaced with `saturating_mul` before the final runs. The measured SIMD attempt
regressed the target kernel and was removed completely, leaving no new unsafe
block or architecture-specific production path. No Stage 8 remediation or
deferred failure remains. The separately identified bitmap-native fold
candidate remains outside this stage.

---
## 2026-09-21 — Blocked bitmap batches cross the SIMD boundary once

The Stage 8 blocked-bitmap SIMD rejection was a rejection of its call shape,
not of the instructions. Its feature-gated kernel ran once per row, 512 times
per filter in the measured fixture, while the scalar Rust reducer was already
auto-vectorized by LLVM. A disposable follow-up moved the architecture boundary
around the whole bitmap container and paired two filters so each data vector
fed both AND-popcounts. Pinned to Cortex-X925 CPU 5, nine alternating
repetitions measured 10.04 us for the current per-row auto-vectorized shape,
7.42 us for per-row NEON, and 6.24 us for whole-container paired NEON.

The paired shape now ships in `ops::bitmap` for NEON and AVX2 and is used by
the blocked view counter for adjacent batch filters. An odd final filter keeps
the existing scalar row loop, so the one-filter endpoint does not cross the
feature boundary and does not repeat the prior regression. The production
Criterion endpoint measured 7.130 us for two filters against 11.297 us for two
one-filter calls, 1.58x. The one-filter regression guard measured 5.762 us
before and 5.649 us after.

This adds two `unsafe fn` and five `unsafe` blocks. Bound B8 requires a vector
block or wider row, complete query rows, equal output lengths, and enough data
for every output row. The parent dispatcher checks those conditions before the
feature-gated call. A direct property calls the SIMD functions themselves,
varies row width across block boundaries and tails, varies row count and word
contents, starts outputs nonzero, and compares with the retained scalar oracle.
The public blocked-view property also evaluates a two-filter bitmap batch
against independent `BTreeSet` intersections. The AVX2 test target compiles and
that direct property passes under `qemu-x86_64`; no emulated timing is quoted.
The mechanically checked unsafe total is now 60 blocks and 28 functions.

The same scratch crate tested JIT fusion with Cranelift 0.135.2 on the Boolean
DAG `(A & B) | (C & !D)` followed by popcount. Scalar JIT was 2.3-2.7x slower
than LLVM AOT. Explicit vector IR improved it, after replacing unsupported
`i64x2` popcount with byte popcount and widening reductions, but still ran
1.75-2.05x slower across 64, 1,024, and 16,384 words. Cold compilation was
1.050 ms and the warmed second compilation 85.8 us, so there is no break-even
count: generated code is slower before compilation is charged. Do not add a
JIT runtime for current view terminals. Re-open only for a measured hot DAG
where eliminating several full bitmap passes first beats an ahead-of-time
fused specialization.

The required local gate passes: workspace Clippy with all targets and features,
workspace format check, and the full `yesno-core` suite. The unsafe-count and
self-contained-doc checks also pass.

---
## 2026-09-21 — JIT fusion measured against the production expression path

The earlier Cranelift experiment compared generated code only with an already
fused LLVM loop and therefore could not answer whether fusion beats the current
dynamic expression evaluator. The scratch harness now builds four real dense
bitmap `OrdSet` leaves, plans `( A AND B ) OR ( C AND NOT D )`, and compares its
cardinality walk with eager composition, a reusable raw multipass, per-container
AOT fusion, and per-container JIT fusion. All complete answers agree before the
timing loop.

The original vector JIT cost 439.0 ns per 1,024-word bitmap. Four-way unrolling
alone barely helped; retaining `i32x4` accumulators and widening only after the
loop reduced it to 324.0-338.8 ns. On the final counter-free pinned timing run,
a prepared expression cost 661.0 ns and 12 allocations for one chunk versus
338.8 ns and zero for JIT. Across sixteen chunks it cost 8.978 us and 42
allocations versus 5.770 us and zero, a 1.56x win. Planning each execution was
much worse at 1.785 / 16.029 us and 47 / 78 allocations. Allocation counts were
collected separately from the timing loop.

JIT is not the main source of that win. A reusable non-JIT multipass took
336.6 ns for one chunk and 6.620 us for sixteen, while LLVM AOT fusion took
224.4 ns and 4.547 us per real-container walk. Cranelift's warmed compile was
160.5 us and the first process compile 1.053 ms. Its warmed break-even against
the prepared expression is about 498 one-chunk or 50 sixteen-chunk executions;
charging startup makes that about 3,268 and 328. The fixture is an optimistic
all-bitmap, all-prefix-present, resident case.

The next implementation gate is an allocation-free prepared bitmap DAG executor
with reusable bounded scratch and workload counters, followed by AOT templates
for common canonical shapes. A cardinality-only JIT remains conditional on a
hot repeated workload that still has material residual after those steps. No
runtime dependency or production code changed in this exploration.

---

## 2026-09-21 — Cranelift gap traced to non-canonical pairwise IR

Disassembly overturned the remaining JIT code-quality conclusion. LLVM's fused
AArch64 loop uses `cnt`, two `uaddlp` reductions, and `uadalp` into two independent
accumulators. The first Cranelift generator instead wrote ordinary `iadd` between
`uwiden_low` and `uwiden_high`. Cranelift consequently emitted `uxtl`, `uxtl2`,
and `add` separately at both levels, six instructions after every `cnt`. That
352-byte function took about 439 ns per 1,024-word bitmap.

Cranelift's AArch64 lowering recognizes the exact canonical form
`iadd_pairwise( uwiden_low( x ), uwiden_high( x ) )` as `uaddlp`. Using it at both
levels, hoisting four base addresses, using pairwise IR for the final horizontal
sum, and matching LLVM's two-vector iteration reduced the generated function to
152 bytes. In one pinned run, arbitrary-slice LLVM AOT took 215.4 ns, fixed-size
LLVM AOT 213.6 ns, and corrected Cranelift JIT 217.5 ns. Real-container traversal
was 223.4 ns AOT versus 232.4 ns JIT. At 16,384 flat words the result was
4.042 us versus 4.118 us. The former 1.5-2x steady-state gap is gone.

The crucial distinction is recognition level. LLVM auto-vectorizes the scalar
Rust loop and discovers the reduction. Cranelift requires the generator to emit
vector IR in the backend's canonical pattern; algebraically equivalent widening
is not combined automatically. Its remaining static differences are individual
`ldr q` loads rather than LLVM's paired `ldp`, and no public accumulating
pairwise-long operation corresponding to LLVM's `uadalp`. They leave a small
2-4% residual, not a material compiler deficit.

JIT feasibility is now governed by compilation amortization, cache and executable
memory lifecycle, and mixed-container fallback. The corrected warmed compile was
185.1 us, amortizing against the prepared expression after about 456 one-chunk or
44 sixteen-chunk executions in that run. AOT templates still win for common
known shapes by avoiding compilation, but arbitrary hot bitmap DAGs should not
be rejected on steady-state code quality. A future generator needs an
architecture-specific lowering regression because a semantically correct change
from pairwise to ordinary widening restores the performance failure without
changing any result.

---

## 2026-09-21 — Cranelift reaches AArch64 steady-state parity

The remaining long-loop difference was the accumulator dependency. LLVM carries
two independent vector accumulators and updates them with `uadalp`; the corrected
Cranelift loop combined two vector results before updating one `i32x4` value.
Cranelift has no public accumulating pairwise-long operation, but it can carry
two `i32x4` block parameters and update one per vector, combining them only in
the epilogue.

Nine alternating pinned repetitions measured 215.6 ns JIT versus 228.0 ns LLVM
AOT over 1,024 flat words, and 4.085 us versus 4.059 us over 16,384 words. Over
real `OrdSet` payloads the figures were 230.0 versus 234.1 ns for one chunk and
4.806 versus 4.690 us for sixteen chunks. The direct-arm spreads were below 0.6%.
The 160-byte generated function remains larger than fixed LLVM's 140 bytes, but
its throughput is within 0.6% on the long loop and within about 2.5% at the
container endpoint. This is steady-state parity for the measured AArch64 bitmap
cardinality DAG.

The warmed compile cost was 191.4 us, amortizing against the prepared expression
path after about 455 one-chunk or 44 sixteen-chunk executions. Production remains
conditional on canonical shape caching, executable-memory lifecycle, mixed
representation fallback, and an x86 measurement. The generator must preserve
both canonical `iadd_pairwise` lowering and independent accumulators; correctness
tests alone cannot protect either performance property.

---

## 2026-09-21 — Fused bitmap-DAG JIT reaches the Flight expression path

Cranelift 0.135 requires Rust 1.95, whereas `yesno-core` promises 1.89 and
five runtime dependencies. A `yesno-jit` workspace satellite now owns the
compiler and a bounded thread-local cache; Flight server expressions enter it
through cardinality terminals without changing `yesno-core`'s dependencies.
The PostgreSQL client-only transport does not link this satellite.

The generated AArch64 function accepts a leaf-pointer table, evaluates any
postfix `And` / `Or` / `Xor` / `AndNot` DAG within 32 leaves in a fused vector
loop, and popcounts the result using canonical pairwise widening and two
independent accumulators. A per-leaf cursor holds at most one `Container` from
resident sets or re-openable sources; missing prefixes use a static zero
bitmap. Arrays, runs, and unlendable mmap bitmaps use audited core kernels per
prefix. Compilation failures and unsupported shapes fall back to core.
Executable mappings are retained with the module for the compiled function's
lifetime; at most 64 successful or failed shapes are kept per thread.

Two `unsafe` blocks are introduced in the satellite. The generated function
pointer is transmuted only after defining the exact host C ABI and retained
with its `JITModule`; calls pass a pointer table whose entries each address
1,024 live `u64` words for the duration of that call. Both sites carry
`SAFETY` comments. A property test compares generated Boolean DAG counts
against the core expression oracle, and separate tests cover sparse prefixes,
mixed containers, lazy sources and propagated read errors.

The whole-expression benchmark constructs four 6,000-value bitmap leaves per
chunk, tests 1/16/64/256 chunks against *prepared* core evaluation, and
black-boxes the expression input per iteration. Two sequential benchmark runs
reported core/JIT ratios of 0.58x at one chunk, 1.06-1.07x at sixteen,
1.05-1.09x at sixty-four and 1.04-1.12x at 256. Automatic JIT therefore
admits only expressions with a leaf reporting at least 256 chunks, avoiding
the measured small-view regression; explicit `DagJit` remains available
for hot smaller expressions. The 256-chunk gain is modest and variable, so
this is no claim of end-to-end parity for all workloads or x86.

The source tests found a separate planner correctness bug: an opaque
`ChunkSource` without a prefix span returned `None` from `bounds()`, but
`pass_b` uses that value as *proven empty* and discarded the source. The
conservative whole-universe bound fixes it, and an independent eager-set
oracle now tests all four Boolean operators. Both `cargo test -p yesno-jit`
and the focused core source-equivalence test passed before the full gate.

---

## 2026-09-21 — Rust 1.95 becomes the shared minimum

The maintainer explicitly raised the minimum after noting that core already
contains SIMD acceleration. The distinction between fixed AOT kernels in core
and a runtime DAG JIT remains, but MSRV no longer justifies separating them.
The separate `yesno-jit` crate still preserves core's five-direct-dependency
budget and keeps executable-code ownership outside the storage engine.

The Cargo workspace, including core, DataFusion, Flight and the Monty harness,
now declares Rust 1.95. The standalone `yesno-c` and literal `yesno-wire`
manifests move with it. PostgreSQL remains at its pgrx-imposed Rust 1.96.
The CI core-MSRV job and source-build Docker defaults were updated together;
previous claims of a 1.89 core promise are superseded by this decision. The
preceding JIT implementation passed the full routine gate before this MSRV
change; the updated manifest and toolchain checks must be rerun against 1.95.

---

## 2026-09-21 — Rust 1.95 gate results

The exact minimum was installed and checked, not inferred from the local
newer compiler: `cargo +1.95 check --workspace --all-targets --all-features`,
`cargo +1.95 check --manifest-path yesno-c/Cargo.toml --all-targets`, and
`cargo +1.95 build -p yesno-server -p yesno-server-utils -p yesno-operator`
all passed. The CI MSRV job now runs the workspace and standalone C checks.
The earlier core-only 1.95 build also passed; its first attempt was invalidated
when the shared `target/debug` directory disappeared, then the identical
command passed on retry without source changes.

`./scripts/gate.sh`, `./yesno-c/gate.sh`, `./scripts/gate-pg.sh`,
`./scripts/gate-mysql.sh`, and `./scripts/gate-search.sh` all passed on the
same checkout. The PostgreSQL gate built and tested both PostgreSQL majors;
the search gate passed all three scenarios. These gates used their configured
builder toolchains; the explicit Cargo 1.95 runs are the evidence for the new
version floor. The cold shared image built the new JIT dependency through its
server binary and then reused that image for MySQL and search.

---

## 2026-09-21 — The fused bitmap-DAG JIT moves behind a core feature

The Rust 1.95 floor removed the version mismatch, and the maintainer approved
moving the fused generator into `yesno-core`. The implementation is now
`yesno-core::jit`, behind an opt-in `jit` feature. Flight's server feature
enables it and calls `yesno_core::jit::cardinality`; the PostgreSQL client-only
target does not. `yesno-jit` stays in the workspace as a compatibility
re-export and retains the whole-expression benchmark. The default core Cargo
graph remains at five direct dependencies and 36 total tree lines.

The move brings two existing unsafe boundaries into core. One calls generated
code with a table whose entries point to live bitmap words or the static zero
page for the entire call. The other casts Cranelift's finalized function
address to the exact host-C ABI signature while the owning module retains its
executable mapping. Their local `SAFETY` comments and the generated-DAG
property test moved with the implementation; the scalar expression evaluator
remains the oracle. On non-AArch64 hosts the tests now assert the explicit JIT
fallback rather than attempting code generation.

---

## 2026-09-21 — Flight and Bazel JIT are opt-in

The preceding entry's claim that Flight's server feature enables JIT is
superseded. The core implementation remains behind `yesno-core/jit`, but
Flight now has a separate `jit` feature; its default `server` feature calls
the scalar `Expr::cardinality`. Bazel defaults to JIT off and selects both
core and Flight JIT features together with `--//:jit=on`. The compatibility
`yesno-jit` Bazel target is incompatible when that flag is off.

`cargo tree -p yesno-flight -e normal` contains no Cranelift dependency,
while `cargo tree -p yesno-flight --features jit -e normal` does. Flight tests
passed with `--no-default-features --features server` and with
`--features jit`. The full `scripts/gate.sh` and `scripts/gate-pg.sh` passed.
In the refreshed image, Bazel built `//yesno-flight:yesno_flight` with JIT
off and built it together with `//yesno-jit:yesno_jit` with `--//:jit=on`.

---

## 2026-09-22 — Quality Gate: remove the temporary `yesno-jit` crate

### Result: PASS ( focused checks; full gate scripts omitted at maintainer request )

### Findings

`yesno-jit` had no in-repository consumer after its implementation and benchmark
moved to `yesno-core`. Its only API was a re-export of core's opt-in `jit`
feature, so keeping it added a workspace member and Bazel target but no
independent behavior. No set, container, codec, stream, or unsafe code changed;
the corresponding invariant and test-layer checks were not applicable.

### Remediation

Removed the compatibility crate, its Cargo workspace dependency and lockfile
entry, its Bazel target, and current architecture and overview references.
Historical journal records remain append-only. Layout, locked metadata,
formatting, workspace Clippy on default and stable toolchains, and core tests
with and without `jit` passed.

### Deferred Items

`scripts/gate.sh` and the external integration gate scripts were not run at
the maintainer's request for a focused check.

---

## 2026-09-22 — Nested fused-DAG AOT/JIT kernel comparison

The shipped nested-expression benchmark measures whole-query paths, not
generated instructions. A standalone scratch binary now measures six matching
bitmap DAGs as fused LLVM AOT and Cranelift JIT functions with the same
pointer-table C ABI and per-chunk call loop. All 24 result counts match the
shipped benchmark, and both arms agree with a scalar postfix oracle.

Across three complete, alternating runs, the 256-chunk AOT/JIT time ratios
for every shape stayed within 0.965-1.037 despite large shifts in absolute
timings. AOT still used 19% less time for one-chunk `mixed4` and about 13%
less for 16-chunk `mixed16`. Thus the large whole-query JIT wins against
`Expr::cardinality` come from fusion, not a faster generated bitmap loop.
The copied-generator limitation, construction and full ranges are recorded
in `LTM/simd-arch-arms-and-kernel-selection.md`. This result changes no
production kernel or automatic admission threshold.

## 2026-09-22 — The fused bitmap-DAG JIT is ported to x86_64, code generation only

Code generation is now allow-listed to AArch64 **and** x86_64. Nothing about
the emitted IR changed: the x64 backend carries dedicated lowering rules for
every operation the generator already produced, so the port is a gate change
plus the verification that the gate is honest.

An allow-list rather than a fallible probe, and that is not conservatism. A
Cranelift backend with no rule for an instruction reaches
`machinst/lower.rs:1011` and **panics** -- `compile`'s `Result` has nothing to
catch, so trying an unknown backend and falling back on error is not a design
that exists.

### What the x64 backend actually emits

`popcnt` over `i8x16` has three tiers in 0.135.2: `vpopcntb` under
AVX512VL + AVX512BITALG, Mula's `pshufb` nibble table under SSSE3, and a
shift-and-mask fallback on bare SSE2. The two widening steps matter more:
`iadd_pairwise( uwiden_low( v ), uwiden_high( v ) )` over the *same* `v` has
its own fused rules -- `pmaddubsw` for `i8x16 -> i16x8` and
`pxor` / `pmaddwd` / `paddd` for `i16x8 -> i32x4`. The AArch64 note that the
canonical pairwise form must not be rewritten into ordinary widening therefore
holds on x86 for an independent reason, and neither architecture's test suite
would notice if it were.

### Two tiers verified, one unreachable

`qemu-x86_64`'s default `qemu64` model reports `has_ssse3 = false` and
`-cpu Haswell` reports SSSE3, SSE4.1, SSE4.2 and AVX ( measured with a scratch
probe that prints `cranelift_native`'s derived ISA flags ), so running the test
binary under both covers the SSSE3 and SSE2 tiers. `-d in_asm` on the Haswell
run shows the executed code contains `vpshufb`, `vpmaddubsw` and `vpmaddwd` --
the claim is observed, not inferred from reading ISLE.

**The `vpopcntb` tier is unexercised anywhere available.** QEMU's TCG
implements no AVX512 at all ( `Icelake-Server` prints "TCG doesn't support
requested feature" for every AVX512 bit ), and the Intel i9-9880H that settled
`ops::bitmap` is Coffee Lake, which has no AVX512BITALG. Reaching it needs an
Ice Lake, Sapphire Rapids or Zen 4 host.

### Automatic admission stays AArch64-only

`generator_supported()` and `auto_admission_supported()` are separate
predicates on purpose. The 256-chunk threshold came from timing this generator
against *prepared* core evaluation on AArch64, and that comparison does not
transfer: core's own bitmap kernels are AVX2 on x86_64 -- 2.70x over scalar
`popcnt` for a 1 024-word popcount, measured on the i9-9880H on 2026-09-18 --
while Cranelift IR caps vectors at 128 bits, so on x86 the generated loop would
be competing at half the width of the path it replaces. That is a reason to
expect the AArch64 ratios not to hold, not merely an absence of data.

`benches/dag.rs` runs on x86_64 now and is the instrument that settles it. Its
header records the protocol the AArch64 run did not need: at least three
complete runs on real silicon, ratios read *across* runs rather than within
one, all six shapes ( a win at four leaves and a loss at sixteen is a different
decision from a uniform one ), and `first_us` kept separate because it is the
numerator of the break-even. Explicit `DagJit` is the supported x86_64 entry
until then.

### The `jit` tests had never executed anywhere

The same class as `yesno-tantivy/tests/flight.rs` on 2026-09-17, and found the
same way. `yesno-core/src/jit.rs` is `#[cfg( feature = "jit" )]`, `jit` is off
by default, and both `scripts/gate.sh` and CI run `cargo test --workspace` at
default features -- so five tests, including the property test that is the only
check on this module's two `unsafe` boundaries, were type-checked by clippy's
`--all-features` on every run and executed by neither. `cargo test -p
yesno-core --features jit` is now in the gate's "tests behind off-by-default
features" step and in the matching CI step. The `1556 -> 1557` delta recorded
in that block predates the `jit` feature and is annotated as such.

This is also the only place the x86_64 arm gets executed on real silicon at
all: the gate host is AArch64, so `cargo test --workspace` there compiles the
x86 lowering to nothing and goes green. CI's `ubuntu-latest` is the x86
machine, and until this change it ran the JIT tests as zero tests.

### A test that reaches the unlendable branch

`execute` lends words to the kernel only where every live leaf at that prefix
is a full `BITMAP_WORDS` bitmap, and otherwise calls `generic_prefix`. No test
reached that branch: every fixture drew 5 000 values per prefix, which is
always a bitmap. `array_and_run_prefixes_take_the_per_prefix_core_fallback`
builds one set whose three prefixes are a bitmap, an array and a run, and
asserts those three kinds by construction so that a later change to
`ARRAY_MAX` or to `optimize` cannot quietly turn it back into an all-bitmap
fixture. One expression now crosses the branch in both directions.

### Checks

`cargo clippy --workspace --all-targets --all-features -- -D warnings`,
`cargo fmt --check`, and `cargo test -p yesno-core --features jit` pass on
AArch64. The six `jit` tests pass for `x86_64-unknown-linux-musl` under
`qemu-x86_64` with `-cpu qemu64` and with `-cpu Haswell`. No timing was taken
under emulation and none is claimed.

---

## 2026-09-22: Test plan for final-prefix view count and JIT module lifetime

The selective view regression belongs at the public chunk-accumulator boundary,
compared with both the full-scan strategy and the independent expected count.
Its fixture must include `ORDINAL_MAX` in the final legal prefix; existing
interleaved properties stop far below it. First run the new case on the
unfixed source and require its selective assertion to fail.

The JIT lifetime regression must observe executable mappings, not heap
allocations or answer equality. A Linux-only subprocess will isolate repeated
compile/drop cycles from parallel tests and assert that mappings do not grow
with the cycle count. It must fail against the existing leaking module drop.
Keep a separate randomized JIT-vs-core property that exercises compiled calls
before owner drop and re-compilation after it; this is the safety invariant
for the new explicit `free_memory` call. Compilation failures must be owned by
the same cleanup guard, not handled only in the successful cache path.

## 2026-09-22 — The x86 JIT measurement, and the gate it retired

Supersedes the automatic-admission half of the preceding entry. That entry
kept automatic admission AArch64-only and argued the AArch64 ratios would not
transfer, because core's bitmap kernels are AVX2 on x86_64 while Cranelift IR
caps vectors at 128 bits, so the generated loop would compete at half the
width of the path it replaces. **The argument is sound and the measurement
says no.** x86 tracks AArch64 to within about 15% on every shape, because the
win is fusion and fusion does not care about vector width. The 2026-09-22
nested-kernel entry had already reached that conclusion for generated
instructions; it was not carried into the admission decision, and it should
have been.

Measured on the Intel i9-9880H over ssh, macOS 26.6.2, `+stable` 1.98.1 pinned
because the machine defaults to nightly and the core arm of this comparison is
whatever LLVM builds it. Three complete runs each side, medians:

```text
                    x86_64 i9-9880H                    aarch64
shape          @1     @16    @64    @256        @1     @16    @64    @256
mixed4       2.48x   6.01x  6.81x   6.65x     3.07x   5.10x  6.57x   7.86x
balanced8    4.44x  13.32x 16.02x  13.39x     4.23x  11.61x 16.50x  11.51x
deep8        1.88x   5.70x  6.99x   6.25x     2.30x   4.08x  5.98x   5.14x
or8          1.43x   1.43x  1.61x   1.64x     1.57x   1.65x  1.52x   1.51x
xor8         1.31x   1.63x  1.88x   1.82x     1.48x   1.86x  1.79x   1.66x
mixed16      7.90x  16.99x 22.01x  16.71x     4.12x  21.67x 18.74x  12.50x
```

72 cells per architecture. **No losing cell on either.** x86 spans 1.16x to
22.42x, AArch64 1.48x to 22.48x. Absolute timings moved up to 28% between x86
runs while the ratios held to a few percent, which is the reason the bench
header says to read ratios across complete runs rather than within one.

### The recorded AArch64 ratios were a different experiment

The 2026-09-21 figures -- 0.58x at one chunk, 1.04-1.12x at 256 -- are not
comparable to anything above, and the discrepancy is not an architecture
result. Two things changed underneath them. They timed ***prepared*** core
evaluation, whereas this bench times unplanned `Expr::cardinality`, which is
what Flight actually calls; and they ran on the corpus this bench's own
comment now describes as having "made the old four-leaf expression effectively
one leaf", so the AND-heavy shapes were not intersecting anything. Either
change alone moves the ratio.

**This is why the control was re-run rather than read out of the journal.**
The x86 table looked implausible against the recorded numbers -- a 22x against
a recorded 1.05x -- and the correct response to an implausible number is to
reproduce the baseline, not to explain the new number. Running the same bench
on AArch64 took four minutes and moved the finding from "x86 is anomalous" to
"the record is stale".

### Break-even

`first_us` is compile plus first call: 572-4574 us on x86, 135-1437 us on
AArch64, the spread tracking machine speed. Against the median per-call
saving:

```text
                    x86_64            aarch64
  mixed16  @256     0.1 calls         0.1 calls
  mixed4   @256     0.4 calls         0.5 calls
  or8      @256     2.8 calls         3.5 calls
  balanced8 @1     19.6 calls        29.7 calls
  or8       @1    150.7 calls       136.8 calls
  xor8      @1    200.7 calls       160.6 calls
```

At 256 chunks the first call pays for itself before it finishes. The pessimal
corner is a dense-result shape at one chunk, ~200 repetitions, which a cached
shape reaches easily but which is the corner `AUTO_MIN_CHUNKS` exists to keep
out of the automatic path anyway.

### `AUTO_MIN_CHUNKS = 256` now rests on a number that does not reproduce

Recorded separately because it is a different code path from the port and was
not changed here. The constant exists to avoid the measured "0.58x at one
chunk" regression. On this bench, one chunk measures **1.31x to 7.90x in the
JIT's favour** on x86 and 1.48x to 4.23x on AArch64. The old figure was
against prepared core; Flight builds an `Expr` per request and counts it once,
so unplanned is the comparison that matches the caller. Lowering the constant
is a real decision with a real risk -- the break-even table above shows the
one-chunk corner needing 20-200 repetitions, so a workload of unique shapes
would pay compile cost it never amortizes -- and it needs its own measurement
of shape reuse, not this one. Left at 256.

### Native x86 correctness, no longer resting on emulation

`cargo test -p yesno-core --features jit --lib jit::` passes 6/6 on
`x86_64-apple-darwin`. This is the first execution of the generated x86 code
on real silicon, and the first time the JIT's executable mappings have met
macOS rather than Linux. The qemu runs established the counts across two
lowering tiers; this establishes them where it matters.

### Three `concurrency.rs` tests fail on macOS, and not because of this work

`cargo test -p yesno-core --features jit` on the i9-9880H is red:
`commit_times_never_invert_under_contention` ( "only 0 commits were observed;
this test is not contending" ), `concurrent_commit_cost_is_measured_not_assumed`
( "1931 fsyncs for 2000 concurrent commits ( 0.97 each ): commits are not
sharing fsyncs" ) and `readers_are_correct_while_a_checkpoint_syncs` ( "the
writer must have checkpointed repeatedly, got 2" ). All three assert that
contention *occurred*, which is a property of the machine rather than of the
code, and macOS's `F_FULLFSYNC` makes group commit behave differently again.

**The first control was wrong and is recorded because the mistake is easy to
repeat.** It ran the three tests alone with `--test-threads=1`, they passed,
and that looked like an answer. It is not: it changed the feature *and* the
machine load at once, and every one of these assertions is a load property, so
an idle machine fixes them whatever the feature does. The control that
attributes anything is the full file at default parallelism with the feature
off -- the failing run's conditions minus exactly one variable. Run that way it
fails identically, 11 passed and the same 3 failed, `1916` fsyncs against
`1931`. The failures are pre-existing on macOS and independent of the JIT.

macOS remains ungated ( `macos-build-is-not-gated` in TODO.md ), so this is
not a regression anyone would have caught here either.

### Checks

Workspace clippy with `--all-targets --all-features -D warnings`,
`cargo fmt --check`, and `cargo test -p yesno-core --features jit` pass on the
AArch64 host. The six `jit` tests pass natively on `x86_64-apple-darwin` and
under `qemu-x86_64` at both reachable lowering tiers. Benchmark data, the
run script and the raw run files are under `.agents-workspace/tmp/`; the
findings are here because the instrument does not ship.

### Not changed here

`auto_admission_supported()` is still AArch64-only in the tree. The data above
supports adding x86_64 and the change is one line, but the measurement and the
behaviour change are separate steps and this entry records only the first.

---

## 2026-09-22: Final-prefix count and JIT executable-memory lifetime fixed

The selective counter's chunk end used wrapping `u64` addition at the final
legal prefix. A public counter regression with `ORDINAL_MAX` and an interleaved
single-owner view failed before the fix: debug panicked, while release returned
`[[0]]` instead of `[[1]]`. Saturating the chunk end to the reserved
`u64::MAX` exclusive sentinel makes Selective and FullScan both return
`[[1]]`; the selected logical-row endpoint already uses the same sentinel.

Cranelift's memory provider does not unmap executable allocations on ordinary
module drop. A Linux child-process regression measured +163,840 executable
mapping bytes after 40 fresh `DagJit` compile/drop cycles on the old code;
it passes after an owning `OwnedModule` guard explicitly invokes
`JITModule::free_memory`. The guard exists immediately after module creation,
so `declare_function`, `define_function`, and `finalize_definitions` errors
all run the same cleanup. The new unsafe call is valid because generated
function pointers stay private to `Compiled`, calls borrow the owning cache
mutably, and compilation failures expose no pointer. A randomized property
checks that dropping one compiled cache leaves another live kernel usable and
that a new cache can compile again after both predecessors are dropped.

Workspace clippy with all targets and features, `cargo fmt --check`,
`cargo test -p yesno-core`, and `cargo test -p yesno-core --features jit`
all pass. Automatic JIT admission was not changed; its selectivity issue
remains a separate open task.

## 2026-09-22: Test plan for automatic JIT admission

### Failure classes

| Class | Concrete failure | Layer |
|-------|------------------|-------|
| Excess payload work | A 256-chunk AND one-chunk source decodes the union instead of seeking to the match | Counting source at the public `jit::cardinality` boundary |
| Wrong path choice | Aligned 256-chunk array leaves enter the bitmap JIT and pay generic-prefix overhead | Direct admission regression in `jit.rs` |
| Semantic drift | Declining JIT changes the result or swallows a source error | Existing JIT/core equivalence and source-error tests |

### Planned tests

- A counted source fixture compares automatic JIT's payload calls with core
  for a selective AND; it must fail on the old 257-call path and pass at two.
- A direct admission test requires dense aligned bitmap inputs to remain
  eligible, while selective, sparse-span, array and unknown-encoding inputs
  are declined. It pins the shape distinction a timing assertion cannot.
- Existing randomized JIT-vs-core properties remain the answer oracle.

### Generators

Deterministic 256-prefix fixtures make both the threshold and encoding branch
reachable. Source counters observe `next_chunk` calls, not wall-clock noise.

### Deliberately not covered

This pass does not expand x86 automatic admission or optimize explicit
`DagJit` traversal. It makes the automatic policy conservative so it cannot
select the known losing shapes; widening it needs a seek-driven executor or
new measured evidence.

## 2026-09-22: Automatic JIT admission excludes union-draining losses

The old gate admitted whenever *one* leaf reported 256 chunks. A new counted
source regression reproduced the cost without a timer: for a 256-chunk bitmap
AND a one-chunk bitmap, core read one payload from each source, while automatic
JIT read 256 from the wide source and one from the narrow one. The new gate
declines that shape, and the public automatic path returns the same count with
two payload reads. The test failed on the old policy and passes on the new one.

Automatic admission now requires every leaf to report the same contiguous
prefix span of at least 256 chunks, with every chunk a bitmap. This is
deliberately conservative: an exact dense aligned shape still enters the JIT;
a one-chunk sibling, sparse prefix span, array container, or unknown source
encoding does not. The direct shape test failed on the old max-leaf rule and
passes now. Explicit `DagJit` retains its broader mixed and sparse behavior;
the executor itself is unchanged and can still drain a prefix union when
called explicitly.

`ChunkSource::all_bitmap_chunks` defaults to unknown. `KeySource` computes
the exact answer once from its already-built plan: memtable container variants
and disk `ChunkRef` tags, without decoding payloads. A disk-bitmap, disk-array
and mixed disk/memtable test passes; temporarily forcing the hint to
`Some(true)` made that test fail on the array branch, then the truthful
implementation was restored.

Workspace clippy with all targets and features, `cargo fmt --check`,
`cargo test -p yesno-core`, and `cargo test -p yesno-core --features jit`
pass. No timing improvement is claimed from this gate change. x86 automatic
admission and the unavailable AVX512 `vpopcntb` tier remain separate tasks.

## 2026-09-22: Admission regression test-layer follow-up

The public payload-work regression was moved from `jit.rs` into
`tests/allocation.rs`, where work budgets are asserted externally. The
in-module test now covers only the private `worth_jitting` shape decision.
After the move, deliberately forcing auto admission for an `And` made the
external test fail at 256 wide-side payload reads against its required one;
restoring the gate made it pass again. This verifies the final external test,

## 2026-09-22: Test plan for simple binary automatic-JIT admission

### Failure classes

| Class | Concrete failure | Layer |
|-------|------------------|-------|
| Wrong path choice | Two aligned 256-chunk bitmap leaves enter automatic JIT despite a measured binary-AND loss | Private `worth_jitting` admission test in `jit.rs` |
| Lost useful path | A measured four-leaf mixed bitmap DAG no longer enters automatic JIT | Same admission test with an eligible mixed DAG |
| Semantic drift | Fallback disagrees with JIT on the result | Existing JIT/core expression equivalence tests |

### Planned tests

- Update `jit::tests::automatic_admission_requires_aligned_contiguous_bitmap_leaves` so an aligned binary AND declines and a four-leaf `( A AND B ) OR ( C ANDNOT D )` remains eligible. This pins dispatch without a noisy timing threshold.

### Generators

No new generator. The deterministic 256-prefix bitmap fixture reaches the exact automatic threshold and avoids random-shape gaps.

### Deliberately not covered

This pass does not claim three-leaf performance or optimize explicit `DagJit` traversal. Those shapes remain available explicitly; automatic admission stays conservative until measured.
not just the earlier in-module prototype.

## 2026-09-22: Binary automatic-JIT admission follows measured shape evidence

The haiiie verification confirmed the three preceding fixes independently,
then supplied a truthful bitmap-hint control that isolated a remaining loss:
plain aligned 256-chunk binary AND took 86.053-86.324 us in core versus
105.282-106.680 us automatically JITted and 104.092-105.137 us explicitly
JITted, over five warm rotated 100-call batches. Answers agreed. The explicit
control makes metadata-hint overhead insufficient as a sole explanation.
These were resident-backed counted sources, not disk I/O or end-to-end scoring.
The raw probe and results remain under the haiiie checkout's
`.agents-workspace/tmp/jit-review-20260922/`.

A direct admission test required that binary shape to decline and the prior
measured four-leaf mixed bitmap DAG to remain eligible. It failed against the
old policy and passed after the fix. Automatic admission now counts leaves to
a cap of four before inspecting any container or source metadata; this also
keeps the simple binary fallback cheap. A metadata-trap source in the test
pins the early exit. Unknown bitmap encoding is still tested on a four-leaf
shape, so it is not masked by the new leaf-count prerequisite. Explicit
`DagJit` and its traversal remain unchanged.

The final local checks passed: workspace Clippy with all targets and features,
`cargo fmt --check`, the default and JIT-featured `yesno-core` suites,
`git diff --check`, and the architecture layout checker. A single post-change
release run of the existing four-leaf AArch64 fixture kept automatic/core
median ratios at 0.704, 0.689, and 0.647 for 256, 512, and 1,024 chunks per
leaf respectively. Construction and limitations are recorded in the SIMD/JIT
LTM note. No new binary timing is claimed; its automatic path is now the core
fallback after a cheap structural preflight.

## 2026-09-22: Test plan for seek-driven explicit fused-DAG traversal

### Failure classes

| Class | Concrete failure | Layer |
|-------|------------------|-------|
| Excess payload work | Explicit JIT decodes 256 wide-side chunks for a one-prefix AND | Public counted-source work budget in `tests/allocation.rs` |
| Branch-irrelevant work | A disjunct's AND branch loads a leaf while its sibling cannot contribute at that prefix | Same counted-source work budget on a nested DAG |
| Wrong cardinality | Candidate-prefix pruning skips a valid output or mishandles a lower-bound-only peek | Existing JIT/core property plus an explicit lower-bound source regression |
| Lost errors | A source error on a needed branch is silently treated as empty | Existing JIT source-error regression |

### Planned tests

- Extend the counted-source fixture to run explicit `DagJit` on 256-by-1 selective AND and require one payload read per side, matching core.
- Add a nested `Or( And( wide, narrow ), other )` case that requires branch-irrelevant wide prefixes to be skipped while retaining the other branch's answer.
- Use a source whose `peek_prefix` is only a lower bound to verify a candidate may advance when `next_chunk` yields a later prefix.
- Keep the randomized generated-DAG-versus-core property as the semantic oracle.

### Generators

Deterministic bitmap-rich prefixes exercise seek selectivity. The existing randomized JIT property varies Boolean shape and bit contents; the new lower-bound source fixture targets the contract gap it cannot generate from `SetStream`.

### Deliberately not covered

This pass does not widen automatic admission or claim a throughput gain for dense explicit JIT. The existing dense whole-expression benchmark will be rerun to check for regressions; x86 automatic admission remains separately benchmark-gated.

## 2026-09-22 — x86_64 automatic admission is enabled

The behaviour change the preceding entry deliberately withheld. That entry
said `auto_admission_supported()` was still AArch64-only and that the flip was
one line; this is that line, plus the prose it invalidated.

`auto_admission_supported()` now returns true on AArch64 and x86_64, which
makes it identical to `generator_supported()` today. **The two are kept
separate anyway**, and the reason is a hazard rather than a taste: they answer
different questions -- does this backend lower our IR, and has the ratio been
measured here -- and collapsing them would mean that allow-listing a third
architecture for code generation silently switches automatic admission on for
it with no benchmark behind it. Widening either one is now a documented
obligation to run `benches/dag.rs` first.

### The measurement was re-validated against the current tree before the flip

The benchmark ran against the tree as rsynced to the Mac, and `jit.rs` had
moved 309 lines underneath it in the meantime -- a concurrent session narrowed
automatic admission to four-plus leaves over equal contiguous all-bitmap spans,
and reworked the executable-memory lifecycle. Timings taken against superseded
code would not have supported anything.

They were not. `fn execute` is byte-identical between the measured copy and
the current one ( same md5 ), `try_cardinality` differs by one word in a
comment, and the generated IR is unchanged instruction for instruction --
4 `iadd_pairwise`, 2 `uwiden_low`, 2 `uwiden_high`, 1 `popcnt`, 1 `extractlane`
and the rest, identical multiset. Both timed paths are the ones now in the
tree. **The check cost one `diff` and one `md5sum` and is the only reason the
numbers still mean anything**; the rsynced copy on the remote host is what made
it possible, so do not clean it up before the follow-up questions are closed.

The narrowed predicate also turns out to be *upstream* of the measurement in a
convenient way: all six benchmark shapes at 256 chunks have four or more
leaves over equal contiguous all-bitmap spans, so the measured region is
exactly the admissible one. Below 256 chunks the bench reaches the JIT through
explicit `DagJit`, which bypasses `worth_jitting` entirely.

### The floors measured on one host are applied on both

The four-leaf minimum and the equal-contiguous-span requirement came from
AArch64 measurements -- an aligned two-leaf bitmap AND was 22-24% slower
through automatic JIT, and selective ANDs and array fallbacks regressed when
the executor drained the prefix union. Neither was re-measured on x86.

They are applied on both hosts regardless, and that is sound in one direction
only: a floor can only *narrow* admission, so applying an AArch64-derived floor
to x86 can cost a win but cannot introduce a regression. It also happens to
excuse the gap in the x86 run, which covered no two-leaf, selective or
array-bearing shape -- the floors exclude precisely those. Stated because the
reverse reasoning would be invalid: an AArch64-measured *win* must not be
extended to x86 the same way, which is what the preceding entry got right and
the entry before it got wrong.

### Withdrawn figures, removed rather than left standing

`worth_jitting`'s doc comment cited "1 chunk 0.58x, 16 chunks 1.06x, 64 chunks
1.04x, 256 chunks 1.12x" and `benches/dag.rs`'s header framed x86 as an open
question. Both are replaced with the current per-shape figures and, in the
bench's case, with the two protocol rules that were learned by getting them
wrong: read ratios across complete runs rather than within one ( x86 absolutes
moved 28% while ratios held to a few percent ), and make sure both arms are the
shipped ones over a corpus that actually overlaps. A stale number in a load-
bearing comment is worse than no number, and this one stood for a day as the
stated reason x86 was gated off.

`AUTO_MIN_CHUNKS` stays at 256. Its justification is withdrawn, but its
replacement is not "lower it": the one-chunk break-even is 20-200 repetitions
of the same shape, so the constant is now a bet about shape reuse and nothing
has measured shape reuse. Recorded as
`jit-auto-min-chunks-rests-on-a-withdrawn-number` in TODO.md, which replaces
the now-closed `jit-x86-measurement-and-automatic-admission`.

### Checks

`cargo clippy --workspace --all-targets --all-features -- -D warnings`,
`cargo fmt --check`, and `cargo test -p yesno-core --features jit` pass; the
`jit` suite is 9 tests, including the concurrent session's two
executable-mapping reclamation tests and the admission predicate test, which
calls `worth_jitting` directly and is therefore architecture-independent.
Layout, docs-self-contained, TODO-reference and TeX-safety checks pass.

---

## 2026-09-22: Explicit JIT seeks candidate prefixes

The external counted-source tests were red on the old union executor: both a
binary 256-by-1 bitmap AND and that AND beneath an OR read 256 wide-side
payloads, while core read one. The new explicit `DagJit` path computes
candidate prefixes from the Boolean postfix DAG, seeks lagging leaves, and
loads only active leaves. Both tests now pass with one wide-side read and the
same count as core. A legal lower-bound-peek source test buffers a chunk
returned later than its advertised bound; deliberately discarding that
buffer changed its result from oracle 4 to 2, and restoration made it pass.
The existing source-error test and a strengthened 128-case sparse-prefix
JIT/core property pass. The property generator forces one shared prefix so
the planner keeps the JIT path, then varies other prefixes independently.

The concurrent x86 session enabled automatic JIT on its measured dense path
while this work was in progress. To preserve that evidence, automatic calls
retain the dense union executor behind the four-leaf/equal-span/all-bitmap
gate; only explicit `DagJit` uses the new seek executor. Both traverse through
one audited bitmap-call helper, with the original pointer-lifetime SAFETY
invariant and the generated-DAG property covering it. The admitted dense
fixture now compares the public automatic count with core in a test.

Three final-form AArch64 selective timing runs show explicit seek still
1.572-1.816x core on a 256-by-1 bitmap AND despite eliminating excess payload
reads. Three dense mixed-DAG runs show automatic/core ratios 0.646-0.721
across 256-1,024 chunks per leaf. Construction, raw median ranges and
limitations are in the SIMD/JIT LTM note. These results do not justify
widening automatic admission to selective shapes or changing the 256-chunk
threshold. The x86 automatic dense walk is preserved but was not retimed here.

Workspace Clippy with all targets and features, `cargo fmt --check`,
`cargo test -p yesno-core`, and `cargo test -p yesno-core --features jit`
passed after the dual-path change. The later addition of a public automatic
dense/core assertion passed its focused test; final gate status is recorded
below after rerunning it.

Final local gate after the admitted-dense public assertion passed: workspace
all-target/all-feature Clippy with warnings denied, `cargo fmt --check`,
`cargo test -p yesno-core`, `cargo test -p yesno-core --features jit`,
`git diff --check`, layout, and docs-self-contained checks. The JIT suite
included 929 unit tests, 25 allocation tests and the lower-bound expression
test. No PostgreSQL or full Docker gate was run.

The first sparse-prefix property generator allowed fully disjoint leaves, so
planning sometimes reduced the expression to a non-JIT shape and its
`try_cardinality(...).unwrap()` assumption failed before testing traversal.
The minimized seed ( masks [4, 1, 2, 4], selector 28 ) is retained at
`yesno-core/proptest-regressions/jit.txt` per corpus policy. The final generator
forces a shared prefix zero to keep the compiled path reachable while varying
other prefixes independently; 128 cases and the saved seed pass. This was a
test-generator reachability correction, not a JIT count discrepancy.

## 2026-09-22 — Codex handoff: native x86_64 validation, and the explicit JIT's selective losses

Taken over from the Codex session that owns the seek path, which could not
reach the Intel i9-9880H ( ssh authentication refused from that session ) and
had stopped its QEMU run at the maintainer's request. Everything below is the
tree at `jit.rs` md5 `d2271bf67964`, verified byte-identical on both ends of
the rsync before anything was run.

### Native x86_64 is clean, and the one gap is structural

`cargo +stable test -p yesno-core --features jit --no-fail-fast` on the
i9-9880H: **19 binaries, 1 161 passed, 3 failed**, the three being the
pre-existing macOS `concurrency.rs` load assertions already attributed in an
earlier entry. All five test groups the handoff named passed natively -- the
counted selective AND, the nested OR, the loose-peek equivalence test, the
generated-DAG oracle, and `module_reclamation_preserves_live_kernels`.

**`jit_cache_drop_reclaims_executable_mappings` did not run, and cannot.** It
is `#[cfg(target_os = "linux")]` because it counts executable bytes out of
`/proc/self/maps`. That single gate explains three things at once: it is the
entire 929-versus-928 lib-test difference between the hosts; it is why the only
native x86_64 machine this project has cannot cover executable-mapping
reclamation, that machine being a Mac; and it is the test whose self-spawn
( `Command::new(current_exe())` behind a `YESNO_JIT_LIFETIME_CHILD` env guard )
produced the handoff's QEMU binfmt SIGSEGV -- re-entering the emulator, not a
JIT failure. Its only possible home is native x86_64 Linux, which is CI's
`ubuntu-latest`, and CI runs it only because the `cargo test -p yesno-core
--features jit` step was added earlier the same day. Before that it executed
nowhere on any architecture.

### An unpinned probe produced 15x bimodal garbage and inverted its own control

Recorded before the result it nearly destroyed. The first three runs of a new
selective probe showed the explicit JIT losing by 20-25x, which read as a major
finding. It was the harness. Unpinned on the 20-core AArch64 host the JIT arm
is **bimodal by 8-15x**, and the control row -- the dense four-leaf shape
`benches/dag.rs` already measures -- inverted from 7.9x to 0.9x across runs on
byte-identical code, md5 checked. `taskset -c 2` collapses the observed spread
from 1 500% to under 35% and the rows become mutually consistent.

The repository already knew this: the 2026-09-21 AArch64 parity entry says
"nine alternating **pinned** repetitions". Neither the probe nor
`benches/dag.rs` pins. **A control row that reproduces a known number is what
turned two hours of plausible nonsense into a harness bug**; without it the
selective figures would have been believed, because a selective AND losing to
a seek-driven walk is exactly what one expects to see.

`benches/dag.rs` itself is not affected: re-run pinned on AArch64, the 256-chunk
column reads 6.39 / 10.52 / 5.14 / 1.69 / 1.98 / 11.43x against 7.86 / 11.51 /
5.14 / 1.51 / 1.66 / 12.17x unpinned -- same ordering, same family, no cell near
1.00x. The bimodality needed the probe's much longer single-process run to
appear. Pin anyway.

### The explicit surface contains real, cross-architecture losses

Warmed explicit `DagJit` against core with normal planning, identical resident
`Arc<OrdSet>` leaves on both arms, one wide 256-chunk leaf against a narrow
leaf of `n` chunks spread evenly through that span. Three complete runs per
host; AArch64 pinned, x86 unpinned because macOS exposes no CPU-affinity API,
which makes the x86 column the weaker evidence of the two. Core/JIT ratios:

```text
                      n=1     n=4    n=16    n=64   n=256
  2-leaf AND  x86    0.45x   0.44x   0.44x   0.51x   0.49x
              arm    0.57x   0.65x   0.69x   0.72x   0.67x
  AND under   x86    2.15x   4.26x   5.93x   6.52x   6.83x
    an OR     arm    2.88x   3.53x   4.63x   6.40x   7.12x
  4-leaf with x86    0.04x   0.11x   0.46x   1.81x   6.48x
    AND-NOT   arm    0.04x   0.09x   0.34x   1.70x   6.33x
  dense4 ctl  x86                                    6.47x
    ( anchor )arm                                    6.29x
```

Every cell agrees in sign across the two architectures, and the control anchors
against the recorded `dag` figures, which is what makes the rest readable.

The two-leaf selective AND loses at every selectivity. Its absolute cost does
track selectivity ( 1 250 ns at `n=1` to 224 000 ns at `n=256` on AArch64 ), so
**the seek path works**; it just does not beat core's tuned binary cardinality
path, and it loses by more on x86 because that path is AVX2 there. This is the
measurement the four-leaf floor rests on, now confirmed on both hosts.

The four-leaf shape whose second branch is `and_not` over the wide leaf is flat
in `n` -- ~260-357 us regardless -- while core scales from 8.7 us to 2 153 us.
`and_not` needs the wide side at every prefix, so nothing is skippable and this
is correct rather than a defect. It crosses 1.00x between `n=16` and `n=64`,
about a sixth of the span, and below that the JIT is up to **25x slower**.

**None of this changes the automatic decision.** Every losing shape here is one
automatic admission declines: unequal spans in all of them, and two leaves in
the worst. The x86 automatic claim covers dense equal-span four-plus-leaf
shapes and is stated that way. But the module header previously said explicit
`DagJit` "retains the broader contract" without saying that the broader
contract contains 0.04x, which left the 6-16x wins reading as the whole story.
The header now carries this table.

### Threshold: the floor stays at 256, and the question is sharper than before

From the handoff, and it narrows `jit-auto-min-chunks-rests-on-a-withdrawn-number`
usefully: **Flight invokes automatic JIT once, in `GetFlightInfo`, and `DoGet`
does not reuse it**, while the 64-shape cache is thread-local and keyed by
planned postfix shape. The deciding quantity is therefore recurrence of
*eligible* shapes on the *same worker thread* before that thread's cache
saturates -- not wire-query repetition. A workload repeating one query across 64
workers amortizes nothing. `AUTO_MIN_CHUNKS` unchanged at 256.

### Checks

Local: clippy `--workspace --all-targets --all-features -D warnings`,
`cargo fmt --check`, `cargo test -p yesno-core --features jit` ( 19 binaries,
1 165 passed ), and all eight repo checks, all against a pinned and verified
md5. Native x86_64 as above. The probe is research and does not ship: it lives
at `.agents-workspace/tmp/selective-probe` with a path dependency on core, and
its construction is stated here because that is what survives its deletion.

---

## 2026-09-22 — Test plan: bitmap-native interleaved view terminals

### Failure classes

| Class | Concrete failure | Layer |
|-------|------------------|-------|
| Wrong owner count | A bit is charged to the next constituent at a word or chunk seam | `proptest_oracle.rs` against independently grouped `BTreeSet`s |
| Wrong fallback | A mixed array/run chunk is skipped when the bitmap arm declines | The same property, forced mixed encodings |
| Hidden per-bit work | A correct fast path still walks every dense bitmap bit | Whole-terminal `view/cardinalities` benchmark, with before/after numbers |

### Planned tests

- `tests/proptest_oracle.rs::interleaved_view_cardinalities_match_independent_rows` compares arities 2, 4 and 8 with independently grouped sets, including a full bitmap chunk and a non-bitmap tail across the 65,536 boundary.
- Existing view-fold `BTreeSet` tests remain the materializing-fold oracle; a later fold arm must extend them to force bitmap and fallback chunks before it can land.

### Generator

A dense periodic membership pattern forces bitmap encoding in the first physical chunk, while a short tail forces a non-bitmap fallback. The phase varies which owner receives a bit at the word and chunk seams. A sparse extra prefix checks that chunk bases, not only low 16-bit offsets, determine owners.

### Deliberately not covered

This first slice changes only `view_cardinalities`, not materializing folds, arbitrary arity, or JIT. An unaligned mmap-backed bitmap must take the existing generic path; it needs separate storage-backed coverage before the fast path is widened.

## 2026-09-22 — Stage 7a: bitmap-native interleaved cardinalities without SIMD

The first production slice keeps the public `OrdSet::view_cardinalities` terminal and its generic walk. For interleaved arities 2, 4 and 8, a borrowed aligned bitmap word is partitioned by fixed owner masks and counted with `count_ones`; array, run and unaligned shared bitmap chunks still map ordinals through `View::logical_of`. Other arities and blocked layouts are unchanged. There is no new unsafe block or JIT dispatch.

The checked-in `view/cardinalities` benchmark fixes one physical input across all three arities: 16 contiguous bitmap chunks, 55% membership from the deterministic LCG seed `0x2545_f491_4f6c_dd1d`, with all 16 container tags asserted as Bitmap. Each public call allocates and returns the whole count vector. On the AArch64 Cortex-X925 pinned to CPU 2, three complete Criterion runs ( 10 samples, 0.5 s warm-up, 1 s measurement ) gave these median ranges:

| Sets | Before | After | Approximate speedup |
|------|-------:|------:|--------------------:|
| 2 | 2.592-2.596 ms | 41.153-41.168 us | 63x |
| 4 | 2.592-2.595 ms | 87.834-87.960 us | 29.5x |
| 8 | 2.594-2.594 ms | 108.34-108.39 us | 23.9x |

Runs two and three before and all three after are retained under `.agents-workspace/tmp/stage7-cardinalities/`; the first before run was captured in the session output. These are whole-terminal resident-set timings, not Flight snapshot or cross-host measurements. The arity-8 loop is still scalar and is not the earlier 5.361 us NEON prototype; that prototype was four-way and count-only on a different fixture.

The `BTreeSet` property forces a Bitmap first chunk, Array tail, Run next chunk, and sparse final legal prefix, then checks each owner at arities 2/4/8. Shifting the owner mask by one made it fail immediately ( owner 1: 21,531 instead of 22,213 ); restoration passed. A separate public `codec::decode_buffer` regression creates a genuinely unaligned shared bitmap and verifies the generic fallback; disabling only that fallback made it fail `[0, 0]` versus `[21,845, 21,845]`, then restoration passed. This closes the unaligned gap named in the preceding test plan for this cardinality slice.

Materializing `view_fold` still enumerates set bits and builds the result set; its bitmap-native Any/All/Parity arm, an independent native x86 timing, and any NEON specialization remain Stage 7 work. The automatic DAG JIT admission rules are unaffected.

## 2026-09-22 Stage 7b test plan: bitmap-native interleaved folds

Before implementation: exercise the new fold against an independent `BTreeSet` union, intersection, and symmetric difference over 2/4/8 owners. One fixture must contain several aligned bitmap chunks, including n physical chunks sharing one logical output chunk, and another must mix bitmap, array, run, a large prefix gap, and an unaligned shared bitmap to prove the generic fallback. Compare all three reductions and check output invariants. A deliberately broken reducer or output word offset must make the property fail before acceptance. Measure the public `view_fold` terminal on the same pinned 16-bitmap fixture as Stage 7a; report whole-operation medians, not an isolated byte reducer.

## 2026-09-22 Stage 7b: bitmap-native interleaved folds

The public Any/All/Parity fold now uses a scalar bitmap-byte lookup at interleaved arities 2/4/8 when every physical chunk lends aligned words. Each input word yields 64/n logical bits; n consecutive input words pack into one output word and n physical chunk prefixes into one logical output prefix. The builder skips absent prefixes, caches each output bitmap cardinality exactly, and optimizes the resulting OrdSet. A preflight sends array, run, mixed, or unaligned shared bitmap inputs to the existing grouped ordinal walk. No unsafe block or JIT dispatch was added. BitStore word borrowing now declines shared little-endian bytes on a big-endian host, letting its existing decoder preserve bit order.

The checked-in `view/bitmap_folds` benchmark uses the same physical input as Stage 7a: 16 contiguous Bitmap containers over 1,048,576 physical positions, 55% deterministic LCG membership with seed `0x2545_f491_4f6c_dd1d`. Every call materializes and optimizes the result. The comparison temporarily bypassed only the new dispatch for the grouped-walk baseline, restored it for the optimized measurements, and pinned all runs to Cortex-X925 CPU 2. Criterion used 10 samples, 0.3 s warm-up, and 0.7 s measurement. The first number below is the baseline median, followed by the range of two optimized medians:

| Sets | Any | All | Parity |
|------|-----|-----|--------|
| 2 | 4.693 ms -> 82.123-82.265 us | 3.663 ms -> 82.346-82.399 us | 4.242 ms -> 82.411-82.713 us |
| 4 | 4.010 ms -> 76.691-76.715 us | 3.081 ms -> 76.867-76.885 us | 3.654 ms -> 76.770-76.771 us |
| 8 | 3.092 ms -> 164.17-164.46 us | 2.532 ms -> 75.469-75.538 us | 2.813 ms -> 74.335-74.383 us |

The speedup spans about 18.8-57.1x; the 8-way Any row was stable across both optimized runs. Logs are under `.agents-workspace/tmp/stage7-cardinalities/fold-*.log`. This is a resident-set, whole-terminal comparison on one AArch64 host; it is not an x86, persisted-stream, or SIMD result.

An independent BTreeSet property forces eight consecutive bitmap chunks plus a ninth at the last legal physical prefix, checking all three reductions for arities 2/4/8 and output invariants. A second property forces Bitmap, Array, Run, and Array chunks through a large prefix gap; a separate shared-buffer regression forces genuine unalignment. Deliberately inverting the Any byte-table condition failed the bitmap property at phase 0; the correct condition was restored and the focused properties passed. The earlier grouped-walk and select-oracle tests remain. SIMD still needs a measured end-to-end advantage over this new scalar floor on both native AArch64 and x86_64.

## 2026-09-22 — Test plan: SIMD interleaved view terminals

### Failure classes

| Class | Concrete failure | Layer |
|-------|------------------|-------|
| Wrong contents | Vector lane packing assigns a bit to the wrong logical ordinal or owner | `proptest_oracle.rs` against independent `BTreeSet` rows |
| Wrong cached cardinality | Vector output words are right but the bitmap len is stale | `assert_invariants` on every folded result |
| Path divergence | AArch64 vector and scalar reducers disagree for one arity, reduction, or boundary bit | Direct private-helper differential test plus public property |
| Fallback error | An unaligned shared bitmap is treated as aligned | Existing unaligned-buffer regression |

### Planned tests

- Extend the existing all-bitmap view property to force every 2/4/8 fold and count path; it already crosses chunk seams and the final legal prefix.
- Add a direct scalar-versus-NEON differential over boundary-biased bitmap words for every arity and Any/All/Parity, so SIMD remains covered even if runtime dispatch changes.
- Deliberately corrupt one vector reduction and verify the direct differential fails, then restore it.

### Generators

Reuse the nine-bitmap boundary-biased property and add all-zero, all-one, one-hot-at-seam, and dense-periodic word images to the direct differential. These force vector lane, byte, word, and chunk transitions.

### Deliberately not covered

Native x86 SIMD correctness and timings belong to pane %615. No SIMD arm will ship solely on an isolated reducer speedup; the public terminal must beat the scalar floor on the pinned corpus.

## 2026-09-22 Stage 7c: NEON interleaved bitmap count and fold terminals

The arity-2/4/8 bitmap paths now select NEON on little-endian AArch64 after runtime feature detection. `view_cardinalities` counts interleaved owner masks in 16-byte vectors, using bounded pairwise u16 accumulators ( B10 ). `view_fold` maps each complete 8 KiB physical bitmap to 1/2, 1/4, or 1/8 as many logical output words ( B9 ); arity 2/4 uses nibble tables, while arity 8 uses byte-wise compare or population count and lane packing. Both retain the Stage 7a/7b scalar arms on other hosts. The bitmap preflight still declines mixed containers and unaligned shared buffers. Unsafe loads and stores are bounded by the complete 1,024-word input slice and exact 1,024/n-word output slice, with no tail; the public property checks the resulting bitmaps and cached cardinalities.

The benchmark is the checked-in `view/bitmap_folds` and `view/cardinalities/interleaved_*` groups over the same 16 contiguous Bitmap containers used for Stage 7a/7b: 1,048,576 physical positions, deterministic 55% LCG membership seeded `0x2545_f491_4f6c_dd1d`, and arities 2/4/8. This times complete resident-set public terminals, including result construction for folds, not an isolated vector loop or a persisted-stream query. On a native Cortex-X925 pinned to CPU 2, Criterion used 10 samples, 0.3 s warm-up, and 0.7 s measurement. The ranges below are medians of two complete NEON runs; scalar medians were the two Stage 7a/7b runs on the same fixture.

| Terminal | Arity 2 scalar -> NEON | Arity 4 scalar -> NEON | Arity 8 scalar -> NEON |
|----------|------------------------|------------------------|------------------------|
| Cardinalities | 41.15 -> 8.98-9.00 us | 87.83-87.96 -> 17.77-17.78 us | 108.34-108.39 -> 44.11-44.14 us |
| Fold Any | 82.12-82.27 -> 30.05-30.15 us | 76.69-76.72 -> 25.18-25.19 us | 164.17-164.46 -> 101.04-101.24 us |
| Fold All | 82.35-82.40 -> 29.86-29.88 us | 76.87-76.89 -> 25.32 us | 75.47-75.54 -> 16.94-17.08 us |
| Fold Parity | 82.41-82.71 -> 29.64-29.90 us | 76.77 -> 25.21-25.23 us | 74.34-74.38 -> 17.45 us |

Thus all twelve measured public rows beat their scalar floor; the smallest fold gain is arity-8 Any at about 1.62x, and the other folds gain about 2.7-4.5x. Count gains are about 4.6x, 4.9x, and 2.5x. The arity-8 Any result's construction and optimization remain substantial; the raw reducer ratio must not stand in for that terminal result. Raw logs are in `.agents-workspace/tmp/stage7-cardinalities/fold-neon-all-{first,second}.log` and `count-neon-all-{first,second}.log`.

The direct differential compares every arity and fold reduction against the scalar table over zero, full, seam one-hot, periodic, and dense words. Its count companion compares every owner against scalar word masks on those patterns. The independent nine-bitmap `BTreeSet` property checks public counts and folds across contiguous physical chunks and the final legal prefix. The existing mixed-kind and unaligned-buffer tests cover the declined arms. Temporarily changing the last arity-8 output shift from 7 to 6 made the fold differential fail at `sets=8 Any`; temporarily attributing each count mask to the preceding owner made the count differential fail at `sets=4`. Both mutations were restored before the passing checks. Native x86 SIMD and end-to-end persisted-stream map/fold timing remain separate work; no x86 speedup is inferred from these AArch64 measurements.

## 2026-09-23 Stage 7d: x86_64 interleaved bitmap count and fold terminals

The Stage 7c NEON arms now have x86_64 counterparts for the same public
terminals: `view_cardinalities` and materializing `view_fold` at arities 2, 4
and 8 over Any, All and Parity. The scalar paths and every decline condition
are unchanged -- mixed kinds, unaligned shared buffers, other arities,
big-endian hosts, and CPUs without the required feature all still take the
Stage 7a/7b arms. No JIT code was touched.

### Two NEON instructions have no x86 equivalent, and that shaped the port

`vshlq_u8` shifts each byte lane by its own amount, which is how both NEON
nibble arms move folded bits into position. x86 has no per-byte variable shift
at any width. The per-nibble half becomes a **second pre-shifted lookup table**,
which costs nothing because the tables are compile-time constants; the
cross-byte half becomes `pmaddubsw` / `pmaddwd`, which multiply each lane by a
weight and add adjacent lanes in one instruction -- a shift and NEON's
following pairwise add, fused.

For arity 8 the port was abandoned deliberately in favour of `pmovmskb`, which
takes bit 7 of all sixteen bytes into a 16-bit integer: exactly one output
word's worth of bits per instruction, where the NEON arm needs a compare, a
positioning shift and a three-level pairwise tree. **Arity 8 is the cheapest
x86 fold and the most expensive NEON one.** That is
`LTM/simd-arch-arms-and-kernel-selection.md`'s thesis turning up again from the
other side: a technique's value is a property of the instruction set.

The count kernel is AVX2 and the fold kernels are 128-bit, which is not an
oversight. Every fold ends by packing bytes drawn from across the whole
vector, and AVX2's byte shuffles and `packus` act within 128-bit halves, so
each fold would need a cross-lane permute per iteration -- the trade
`ops::array` measured and rejected. The count kernel has no such step
( `psadbw` accumulates within its lane, and the horizontal sum happens once
per container ), which is the condition under which `ops::bitmap` measured the
wider register to pay.

`psadbw` also makes the count bound trivial where NEON's is tight. B10 needs an
argument about u16 lanes because `vpadalq_u8` accumulates in 16 bits; a 64-bit
`psadbw` lane cannot overflow from an 8 KiB input at all, which is recorded as
B10x.

### Measurements: twelve rows, no losses

Native Intel i9-9880H, macOS, `+stable` 1.98.1, the checked-in `view` benchmark
over the Stage 7a/7b/7c fixture: 16 contiguous Bitmap containers, 1 048 576
physical positions, 55% deterministic LCG membership seeded
`0x2545_f491_4f6c_dd1d`. Criterion 10 samples, 0.3 s warm-up, 0.7 s
measurement. Complete public terminals including result construction, not
isolated kernels.

**The floor is a separate build, not a patched-and-restored one.** Two complete
trees were synced; one was compiled with `--cfg yesno_scalar_floor`, which
guards only the new x86 dispatch block. Both bench binaries were then invoked
**directly**. Going through `cargo bench` would have rebuilt the floor tree
*without* the cfg and measured the vector arm against itself -- a null result
that looks like a clean experiment rather than like a mistake.

```text
  terminal                          scalar floor (us)     x86 arm (us)   ratio
  cardinalities  arity 2               56.89-57.41         6.45-6.49    8.83x
  cardinalities  arity 4               81.30-85.03        12.15-12.38    6.78x
  cardinalities  arity 8             136.99-140.62        16.19-16.36    8.53x
  fold  arity 2  Any                 408.05-428.86        33.12-35.90   12.13x
  fold  arity 2  All                 415.96-421.09        31.35-34.46   12.72x
  fold  arity 2  Parity              409.04-420.18        31.91-32.74   12.83x
  fold  arity 4  Any                 397.21-405.85        20.61-20.78   19.40x
  fold  arity 4  All                 418.08-432.84        20.66-20.87   20.49x
  fold  arity 4  Parity              406.76-428.22        20.60-22.68   19.29x
  fold  arity 8  Any                 478.84-487.62        96.15-97.99    4.98x
  fold  arity 8  All                 395.29-416.49        21.35-21.84   18.80x
  fold  arity 8  Parity              397.58-415.51        24.57-24.75   16.49x
```

All twelve public rows beat their scalar floor; the smallest gain is arity-8
Any at 4.98x. That row is the outlier on **both** architectures -- Stage 7c
measured 164 us against 75 us for arity-8 All and Parity on AArch64 -- because
the union of eight constituents is the densest output and its construction and
optimization dominate what the kernel saved. Two arches agreeing on which row
is the outlier is the useful part; the raw reducer ratio must not stand in for
that terminal.

Runs were rotated scalar, vector, vector, scalar so monotone drift cancels
across the pair. **macOS exposes no CPU-affinity API, so these are not pinned**,
which makes them weaker evidence than Stage 7c's pinned Cortex-X925 figures.
Two complete passes per arm; the reported ranges are both medians.

**No cross-architecture comparison is drawn and none should be read in.** The
x86 scalar fold floor is roughly five times the AArch64 scalar fold figure, on
different silicon four generations apart, and this benchmark cannot attribute
that. What the floor *can* be checked against is itself: the x86 scalar fold is
about 7x the x86 scalar count on the same fixture, which matches the work ratio
of 8 192 byte-table lookups against 2 048 word popcounts per chunk. The floor
is also confirmed to be the Stage 7b byte-table path rather than the Stage 7a
grouped walk -- `fold_words_simd` returning false falls through to the scalar
loop *inside* `fold_interleaved_bitmaps`, not out to `fold_via_select`.

### Mutation testing, including one that proved the test was vacuous

Three mutations were introduced, observed to fail the relevant differential,
and restored:

- Reversing the owner masks in the count kernel failed at `sets=2`,
  `left: [0, 16384]` against `right: [16384, 0]`.
- Corrupting one lane of the arity-4 output gather ( `12` to `8` ) failed at
  `sets=4 Any`.
- Dropping the complement in arity-8 Any failed at `sets=8 Any`.

A fourth mutation **passed, and that is the finding.** Setting the count
kernel's nibble-population-count entry for `0b1111` to a wrong value changed no
result. After the owner mask, every nibble is a submask of that owner's nibble
-- `{0,1,4,5}` at arity 2, `{0,1}` at arity 4, a single bit at arity 8 -- so
twelve of the sixteen table lanes are unreachable. The table is kept in its
general form because it is a compile-time constant and the general form is the
recognisable one, but the kernel now carries a note that an unchanged test
there means the lane cannot be selected, not that it is covered. **A mutation
that fails to fail is evidence about the test, not a licence to move on.**

### A test that could have gone silently green

The first version of both x86 differentials returned early when the CPU feature
was absent, which under `qemu-x86_64 -cpu qemu64` ( no SSSE3, no AVX2 ) made
them pass in 0.02 s having executed nothing -- indistinguishable in the log from
a real run, and precisely the degradation `ops::bitmap` already documents.
SSSE3 predates every x86_64 CPU this project targets, so the fold differential
now **asserts** the feature and fails loudly without it ( verified under
`-cpu qemu64` ). AVX2 is not universal, so the count differential still has to
skip, but it says so on stderr and the skip is documented at the guard.

### Checks

`cargo fmt --all`, `cargo fmt --check`, `cargo clippy --workspace
--all-targets --all-features -- -D warnings` ( zero diagnostics ) and
`cargo test -p yesno-core` pass on the AArch64 host, where the NEON arms remain
selected and all 42 view tests pass unchanged. The x86 arms were additionally
cross-checked with `cargo clippy --target x86_64-unknown-linux-musl
--all-targets -- -D warnings` and executed under `qemu-x86_64 -cpu Haswell`,
where the 42 view tests pass including both new differentials. Emulation was
used for correctness only; every timing above is native.

---

## 2026-09-23 Stage 7e: persisted map and fold SIMD measurement

The remaining Stage 7 question was whether Flight's eager packed-input boundary
would hide the resident SIMD gain. It does not. A one-off benchmark uses the
same deterministic 16-bitmap, 55%-dense corpus as Stages 7a-7d, checkpoints
it, drops and reopens the database, and asserts that all recovered payloads are
aligned bitmaps. It measures public Flight direct folds, mapped
cardinalities, and a non-identity mapped-union fold at arities 2/4/8.

The run was pinned to Cortex-X925 CPU 2. Each row is a median of seven batches
of 100 calls after ten warm calls. Two scalar and two agreeing vector runs were
rotated around direct rebuilds; the scalar build disabled only the AArch64
dispatch in `fold_words_simd` and `count_bitmap_words_simd`, reaching the
unchanged Stage 7a/7b fallbacks. The dispatch was restored before verification.

Reopened key materialization costs 5.53-5.78 us. End-to-end mapped counts retain
2.29-3.98x of speedup; direct persisted folds retain 1.55-3.59x; and all nine
mapped-union folds retain 1.80-3.21x. The smallest row remains eight-way Any,
whose dense output construction dominates on both architectures. No measured
persisted row loses.

This closes Stage 7 without a streaming fold accumulator. On this workload the
new parallel implementation would duplicate the audited fold for a prize of
about 6 us, while the vector work it wraps costs roughly 15-136 us. Exact
construction, per-row figures, the isolated outlier treatment, and raw-log
locations are recorded in
`LTM/packed-lenses-matrix-bignum-and-views.md` under “Persisted SIMD
terminals retain the kernel win”. The only production-source cleanup in this
follow-up changes two stale helper comments from NEON-only wording to
architecture-neutral vector wording.


---

## 2026-09-23 Stage 7f: sparse persisted terminals expose the eager boundary

The Stage 7e conclusion was specific to dense bitmap input. A follow-up one-off
benchmark under `.agents-workspace/tmp/persisted-view-simd/` compares current
public Flight evaluation with a prototype that consumes
`Snapshot::key_stream` and never constructs the packed input `OrdSet`. One arm
accumulates the four interleaved constituent cardinalities; the other groups
logical ordinals and constructs only the final Any, All, or Parity set. Every
fixture is checkpointed, dropped, reopened, and asserted to contain Array
containers. Prototype answers are checked against shipped
`view_cardinalities` and `view_fold` before timing.

Runs were pinned to Cortex-X925 CPU 2, with seven median batches after five
warm calls. The 16-chunk by 64-value, 256-chunk by 64-value, and 4,096-chunk by
one-value anchors reproduced in two complete runs; a wider crossover sweep was
run once. At one value per occupied chunk, current divided by streaming time
was 1.30-1.42x for collection and mapped counts and 1.23-1.42x for folds over
16, 64, 256, 1,024, and 4,096 chunks. At eight values per chunk, the gains were
only 1.05-1.08x. At 64 values they were 1.01-1.08x, and at 512 values Any and
Parity slightly favoured current code at about 0.99x. The raw CSV and ratio
table remain beside the scratch crate.

The user's hypothesis holds, but “sparse” must not become a span-only query
heuristic. Cardinality per occupied chunk predicts both iteration work and the
container representation; occupied chunk count determines whether the
absolute saving matters. The exact quantities are already available from
source statistics before payload decoding. Dense all-bitmap inputs must retain
the SIMD terminals measured in Stage 7e. If the sparse branch lands, its
streaming accumulator belongs in core so Flight does not become a second
implementation of view semantics, and the current eager evaluator remains its
oracle. Direct key intersection-cardinality maps already have this shape via
`ViewIntersectionCounter`; identity cardinality maps and direct materializing
folds are the remaining eager terminals.

---

## 2026-09-23 — Quality Gate: Stage 7d-7f view terminals

### Result: PASS ( local mandatory gate; external integration gates omitted )

### Findings

The SIMD implementation and its scalar fallbacks passed the required format,
workspace Clippy, stable-toolchain Clippy, and full `yesno-core` suite. The
architecture layout, R1 public-API containment, and self-contained standing-doc
checks also passed. Direct scalar differentials cover the AArch64 and x86
kernels, and the independent `BTreeSet` properties cover public bitmap count
and fold semantics. Mixed kinds, unaligned storage, unsupported arities, and
unsupported CPU features retain reachable scalar fallbacks.

The unsafe inventory check caught one stale documentation count: the addition
of JIT and view SIMD had moved the tree from the recorded 63 blocks and 28
unsafe functions to 90 and 37 across nine files. This was arithmetic drift, not
a newly unreviewed boundary; each view SIMD call is feature-gated, carries its
local safety argument, and is covered by a direct scalar differential.

### Remediation

The `miri-cannot-reach-the-mmap-unsafe-sites` backlog entry now records the
90-block, 37-function inventory and names the two view files. The checker passes
again. Two stale NEON-only helper comments were made architecture-neutral. The
persisted measurement record now separates dense bitmap inputs, where eager
materialization costs about 6 us and preserves SIMD gains, from sparse Array
inputs, where streaming can save 24-42%.

### Deferred Items

The full Cargo gate and external PostgreSQL, MySQL, search, and C gates were not
run in this follow-up, consistent with the maintainer's earlier request not to
run the full gate. The x86 implementation was already checked natively and
under qemu in Stage 7d. The sparse view planner remains a measured design item
in `TODO.md`; this follow-up records its admission variables but does not add a
second production implementation before that plan is agreed.

---

## 2026-09-23 — Test plan: extremely sparse persisted view terminals

### Failure classes

| Class | Concrete failure | Layer |
|---|---|---|
| Wrong contents | Stream grouping loses a logical ordinal at a chunk seam, misassigns an owner, or mishandles Any, All, or Parity | Extend the boundary-biased view property in `proptest_oracle.rs` against its independent row sets |
| Path divergence | A direct persisted-key query differs from the existing eager `OrdSet` view terminal before or after checkpoint/reopen | Flight integration test comparing public expressions with the eager core oracle |
| Silent decay | The admitted identity-cardinality path resumes constructing a packed `OrdSet` while returning the same counts | Flight thread-local allocated-byte comparison against draining the same resident `KeyStream` |
| Bad admission | A short, blocked, or more-than-one-value-per-chunk input enters the sparse arm and gives up a better existing kernel | Unit coverage of the pure admission predicate at every boundary |
| Invariant violation | A repeated/descending prefix or the reserved final ordinal reaches a streaming result | Core unit regressions asserting a typed error |

### Planned tests

- `tests/proptest_oracle.rs::interleaved_view_cardinalities_match_independent_rows` — run the new streamed cardinality and fold terminals over the same boundary-biased packed set and compare with the existing independent constituent rows.
- `yesno-flight/tests/sparse_view_terminals.rs` — build at least 64 singleton Array chunks, compare all public direct terminals with eager `OrdSet` answers, checkpoint/reopen, and compare again.
- `yesno-flight/tests/expression_allocation.rs::extremely_sparse_identity_counts_do_not_materialize_the_packed_input` — compare allocated bytes with a direct drain of the same resident key stream; an added packed-input build must exceed the bounded allowance.
- Module tests for the admission boundary and malformed stream ordering.

### Generators

No new generator. The existing interleaved-view strategy is boundary-biased across
chunk seams and the final legal prefix and already constructs the independent
row oracle. The Flight fixture deliberately constructs the production admission
shape: one ordinal in each of 64 or more occupied chunks.

### Deliberately not covered

This first arm does not admit Blocked views, arbitrary input expressions, fewer
than 64 occupied chunks, or more than one value per occupied chunk. Dense
bitmap inputs retain the materialized SIMD path. Widening the sparse region
remains measurement-driven rather than being inferred from correctness tests.

## 2026-09-23 GPU offload for long dense sets: measured, and the answer is conditional

Exploration only. **No crate code was written and none should be yet**; the
finding is recorded in `LTM/gpu-offload-on-unified-memory.md` and the probes
under `.agents-workspace/tmp/gpu-probe/` do not ship.

The development host turns out to be an NVIDIA **GB10** -- Grace-Blackwell,
compute capability 12.1, the 20 Arm cores this project has been measuring on
all along, and 121 GiB shared between CPU and GPU. `Addressing Mode: ATS` and
`GPU C2C Mode: Enabled`: one coherent memory, one NUMA node, no discrete device
memory at all. **The standard reason GPU offload fails for set algebra -- the
PCIe copy costing more than the CPU computation -- does not exist on this
machine**, so the question had to be measured rather than dismissed.

A CUDA kernel dereferences plain `malloc`'d memory here, verified against a CPU
reference count. yesno's bitmap payloads are ordinary `arrow-buffer`
allocations, so offload would need no copy, no pinning and no allocator change
-- which matters given ARCHITECTURE's containment policy, and is the only
reason the rest was worth measuring.

### One memory, one ceiling

AND-cardinality over dense bitmaps: one core saturates at 31 GB/s, all twenty
at 102 GB/s, the GPU at 169 GB/s. Every processor here shares one memory, and
the operation is about an eighth of an operation per byte, so **1.65x over the
whole CPU is the hard ceiling for anything that touches each byte once**. Empty
kernel launch plus sync is 5.10 us, which is the floor under every offload
decision.

yesno's layout survives contact: over 65 536 separate 8 KiB containers, one
launch per container costs 269 ms against 6.6 ms for a single batched launch
( 41x worse ), but the scattered payloads themselves cost only about 4% of
bandwidth. **The obstacle is launch granularity, not the data structure.**

1.65x does not buy a hard dependency on one vendor's hardware, absent from
every deployment target the project names, against a core that holds a
five-direct-dependency budget a JIT already had to be kept out of. And the same
factor is available by threading the CPU, which yesno does not do yet and which
costs no dependency and no portability.

### The batched regime is a different answer

Reuse lifts arithmetic intensity off the bandwidth floor. One resident 32 MiB
set against K filters, GPU against all twenty cores: 6.71x at K=1, 8.80x at
K=4, 9.22x at K=16, 11.08x at K=64, with GPU throughput rising from 17.6 to
192.5 Gop/s. Against the single thread yesno actually uses, 40-83x.
`OrdSet::view_intersection_cardinalities_batch` already has exactly that
signature and Flight's server calls it.

Conditions before any of it becomes code are in the LTM document: a satellite
crate on the `yesno-jit` precedent, batched call sites only, a measured
admission floor, and a threaded CPU as the baseline rather than today's single
thread. A materializing result is unmeasured and must not be extrapolated from
counts.

### The measurement that was wrong first

**The first batched kernel measured 1.06-1.23x and would have closed this as
"no opportunity".** It put filters on `grid.y` and chunks on `grid.x`; CUDA
schedules x-major, so blocks sharing a chunk are spread across the grid and
each chunk is re-read from DRAM once per filter. The kernel was measuring the
bandwidth wall it existed to escape.

The tell was on screen: throughput flat at ~20 Gop/s for every K, which is the
*single-operation* bandwidth figure. And the comparison was not symmetric --
the CPU arm nests `for chunk { for filter }`, so it was exploiting the reuse
the GPU was not, which made the ratio *fall* from 2.50 to 1.06 as K rose and
read exactly like "batching does not help the GPU". Restructuring to one block
per chunk, chunk held in registers, filters looped inside, moved the answer by
about 9x.

The rule, which generalizes past CUDA: **when an arm's throughput stays flat as
work per byte rises, it is not exploiting reuse -- check the schedule before
believing the conclusion, and check that both arms exploit it or neither does.**
This is the same failure the repository already records twice for hoistable
timing loops, in a new place.

---

## 2026-09-23 The Intel Mac's two GPUs, and what the contrast establishes

Follow-up to the GB10 exploration, on the project's x86 reference machine. It
has **two** GPUs, and measuring both turns that result from a fact about one
box into a principle. Exploration only; no crate code. Findings are folded into
`LTM/gpu-offload-on-unified-memory.md`.

OpenCL probe, 128 MiB per operand, every count verified against the CPU. The
i9-9880H reaches 19.6 GB/s on one thread and only **29.1 GB/s on sixteen** --
memory-bound at about 70% of dual-channel DDR4-2666, so threading buys 1.5x
here against 3.3x on GB10.

**Intel UHD 630, integrated:** upload 3.3 GB/s, resident kernel 16.8 GB/s.
It shares the CPU's memory, so it has no bandwidth to offer, and its resident
kernel is **slower than a single CPU core**. Break-even never. Shared memory
removes the transfer problem without supplying the thing that would make
offload worth doing -- which is the GB10 result stated in reverse, and worth
knowing before anyone reads "unified memory" as automatically favourable.

**Radeon Pro 5500M, discrete, 8 GiB:** resident kernel **108-137 GB/s**, about
4x all sixteen CPU threads. But upload runs at 5.3 GB/s and costs ten times the
kernel, so a one-shot offload of host-resident data **loses by 6x**. It pays
only if a set is pinned in VRAM and queried at least **7.5 times**, and the
8 GiB cap bounds how long "extremely long" may be.

The contrast is the durable part. Same operation, same fixture:

```text
  GB10 unified      no transfer      1.65x CPU   offload is a pointer
  Radeon discrete   6x the kernel    3.7x CPU    offload is a residency project
  UHD integrated    no transfer      0.86x CPU   no
```

**Memory architecture, not GPU speed, decides which problem you are solving.**
On GB10 there is nothing to manage and the only question is whether the
workload has reuse. On a discrete card the question is whether a set can live
on the device across many queries -- a lifecycle and eviction design, far
larger than a 3.7x ceiling justifies for a laptop that is not a deployment
target.

**One number in that probe is mine, not the hardware's, and is marked as such.**
The "pinned" upload path measured 2.0 GB/s, worse than pageable.
`CL_MEM_ALLOC_HOST_PTR` plus map plus `memcpy` adds a host copy instead of
removing one; that is a bad implementation, not evidence against pinned DMA.
The pageable 5.3 GB/s is also well under what PCIe 3.0 x16 should give, so the
7.5-operation break-even is an **upper bound** a proper zero-copy upload would
lower, plausibly to 3-4. The verdict is unchanged either way, because the
one-shot case loses by 6x regardless of how the upload is done.

---

## 2026-09-23 — Stage 7f: extremely sparse persisted view terminals

The conservative sparse plan is now production code without adding a core
`Expr` variant. Core owns two checked chunk-stream terminals: constituent
cardinalities share the resident bitmap/SIMD helper, and interleaved
Any/All/Parity folds share the resident grouped accumulator. Both reject empty,
repeated, descending, out-of-range, and final-reserved stream chunks rather
than trusting a caller-defined `ChunkStream`.

Flight admits the stream arm only for a valid interleaved direct-key view with
at least 64 occupied chunks and an exact cardinality equal to the occupied
chunk count. That proves one value per nonempty chunk. The declined arm
materializes the same already-open `KeyStream`; it does not plan the key a
second time. Blocked views, short inputs, all denser Array inputs, and bitmap
inputs retain the old materialized route and its native SIMD kernels.

The boundary-biased independent-row property now runs streamed cardinality and
all three folds against its `BTreeSet` oracle. A public Flight integration
test covers resident and checkpoint/reopened singleton Array chunks. The
allocated-byte regression compares the terminal with draining the same
resident key stream; deliberately restoring materialization changed 74,224
requested bytes to 123,152 and failed the 4,096-byte allowance. Deliberately
changing the 64-chunk boundary from inclusive to exclusive failed the admission
test, and deliberately dropping prefix zero failed the core property.

A pinned Cortex-X925 production rerun kept the 16-chunk case eager, where the
public path remained about 1.28x above the standalone stream reference. The
admitted 64, 256, 1,024, and 4,096 singleton fixtures were within about
1.06-1.08x of that reference across mapped counts and folds. Eight or more
values per chunk remained declined as designed.

The required local gate passed: workspace Clippy with all targets/features and
warnings denied, `cargo fmt --check`, the complete `yesno-core` suite, and
the complete server-enabled `yesno-flight` suite. Layout, R1, standing-doc
self-containment, unsafe-inventory, and diff checks also passed. The full Cargo
gate and external service gates were not run, consistent with the maintainer's
earlier instruction.

---

## 2026-09-23 — Stage 7g measurement: bounded direct rank maps

A direct identity rank map still materializes the whole packed key and then
runs the general pointwise truth-table evaluator below the logical bound.
For an interleaved view, that query is exactly constituent cardinality over the
physical prefix `[ 0, upper * sets )`. The new Stage 7f stream counter already
owns the per-container scalar and SIMD kernels; only partial-boundary clipping
and bounded key planning are missing.

The existing persisted scratch corpus was extended with an aligned midpoint
rank. Three complete runs were pinned to Cortex-X925 CPU 2. Current versus
one-pass bounded-stream ranges were 4.778-4.793 versus 1.631-1.801 us at 16
singleton chunks, 45.33-45.67 versus 17.30-20.40 us at 256, 173.00-173.80
versus 66.64-79.33 us at 1,024, and 674.43-682.61 versus 267.20-317.65 us at
4,096. Every row favoured streaming. Denser Array rows kept the same sign but
had wider system noise: the 512-value row ranged from 1.02x to 3.85x. That
spread cannot support a density threshold. The specialization needs none:
identity rank becomes the existing cardinality kernel over less input, while
blocked and non-identity rank maps remain on their current paths.

## 2026-09-23 — Test plan: streamed direct view ranks

### Failure classes

| Class | Concrete failure | Layer |
|---|---|---|
| Wrong contents | The partial final chunk counts an ordinal at or above the logical upper bound, or loses an owner below it | Extend the boundary-biased independent-row property in `proptest_oracle.rs` |
| Path divergence | A persisted direct identity-rank map differs from eager constituent rank before or after reopen | Flight integration test against eager `view_select( .. ).rank( upper )` |
| Silent decay | Rank resumes materializing the full packed key or plans chunks beyond the physical bound | Flight allocated-byte comparison against draining the same bounded `KeyStream` |
| Bound arithmetic | `upper * sets` wraps, exact chunk seams round up twice, or the legal exclusive prefix endpoint is lost | Unit coverage of the pure interleaved prefix-end helper |
| Invariant violation | A malformed streamed boundary chunk bypasses the shared stream checks | Reuse the checked core accumulator and its malformed-stream unit coverage |

### Planned tests

- Extend `interleaved_view_cardinalities_match_independent_rows` with ranks at zero, byte/word/chunk seams, a partial final chunk, and the logical ceiling.
- Extend `sparse_view_terminals.rs` with a non-chunk-aligned upper bound before and after checkpoint/reopen.
- Extend `expression_allocation.rs` with a bounded-stream byte budget that fails on either full-key planning or packed-set construction.
- Add exact arithmetic cases for zero, one-past a chunk seam, overflow-scale bounds, blocked layouts, and invalid descriptors.

### Generators

No new generator. The existing property deliberately produces bitmap, array,
run, chunk-seam, and final-prefix containers and already builds independent
constituent rows.

### Deliberately not covered

Blocked views, non-identity rank bodies, arbitrary input expressions, and
individual indexed ranks keep their existing evaluator. This stage adds no
wire node and no core expression variant.

---

## 2026-09-23 -- The hotspot observer, and what a counter already settled

Built the access-frequency observer that `LTM/gpu-offload-on-unified-memory.md`
and `TODO.md`'s `jit-auto-min-chunks-rests-on-a-withdrawn-number` both named as
the missing instrument. It lives at `.agents-workspace/tmp/hotspot-observer/`,
is a standalone crate with a path dependency on `yesno-core`, and does not ship
-- research, per the rule in CLAUDE.md. 30 tests, `cargo clippy --all-targets
-- -D warnings` clean.

**It answers two questions on one pass because they are the same question.** GPU
residency needs the recurrence of a `( set, prefix )` container before a VRAM
budget saturates; the JIT needs the recurrence of a planned shape before a
64-entry thread-local cache saturates. Both are "how often does key K recur
before a cache of size C evicts it", so the counters are generic over a hashed
key and two extractors feed them. The container key is exact -- `Arc::as_ptr`
plus `OrdSet::chunks` plus the container kind, all public. **The shape key is a
proxy and is labelled one in its own module header**: `DagJit` keys on
`Vec<Instruction>`, which is private, so the observer fingerprints the postfix
walk of the *planned* `Expr` with leaves collapsed to one marker. That is the
equivalence `Program::from_expr` induces, reconstructed rather than observed. It
measures the workload's shape recurrence and is not a test of the JIT.

### The primitive is a reuse-distance histogram, not a hit rate

A hit rate is a claim about a workload *and* a capacity *and* a policy. The LRU
stack-distance histogram is a property of the stream alone, and every capacity's
hit rate is one prefix sum of it -- so a single pass answers the whole budget
sweep for both caches. A flat histogram would have retired both questions at
once, which was the cheap outcome worth checking for first.

**The obvious implementation would not have run.** A linear recency scan is
`O( distinct keys )` per access, and 8192 containers -- a mere 64 MiB of VRAM --
against a few million accesses is hours. It uses a Fenwick tree over access
slots instead: each slot holds a 1 while it is some key's most recent access, so
the distance is the count of live slots since the previous one. `O( log T )`,
exact, no truncation in the algorithm at all.

**The first Fenwick was wrong, and only the differential caught it.** It grew by
appending zeros, on the argument that node `i` covers `( i - lowbit( i ), i ]`
and that range never moves. The range does not move; the *nodes* were never
there. While the array held 2048 slots, `add` stopped climbing at index 2048 as
out of bounds -- so when the array doubled, the new node 2048, covering all of
`( 0, 2048 ]`, held zero instead of everything written so far. It failed at
exactly `t = 2047` and nowhere else. Five hand-written unit tests passed; the
differential against a linear oracle over a 4000-access stream failed, because
it was the only test long enough to double the array. The fix keeps the raw
slots and rebuilds in `O( n )` per doubling, amortized `O( 1 )`. There is now a
named regression test for the boundary.

### The generator, and the two knobs that stop it rigging itself

A generator with one query template reports 100% shape reuse by construction,
and one where every query reads every chunk makes container-level reuse
*identical* to set-level reuse, so the 8 KiB granularity is decorative. Both
were true of the first version and both are now parameters: a template library
( `--shapes` ), and a prefix window ( `--window` ). Drift ( `--drift` ) rotates
the popularity ranking, which is what gives decay anything to do -- a decay
parameter tuned against a stationary workload is tuned against nothing.

### What the numbers say

Corpus 64 sets x 128 chunks = 8192 containers = 64 MiB; 20 000 queries, arity up
to 8, window 16 chunks, 200 templates collapsing to 113 distinct planned shapes.
Accelerator modelled at 6.7x per operation with CPU fallback on miss, fills
charged as bandwidth rather than latency.

**Shape recurrence is not flat, which keeps the JIT question alive.** At the
64-entry cache the hit rate is 73.0% at zero skew and 94.8% at Zipf 1.2, over
113 distinct shapes. That is the first number of any kind on shape reuse. It is
a property of the synthetic template library, so it is *not* evidence about a
deployment -- but the flat outcome that would have closed the question did not
occur, and 73% at **zero** skew is the interesting half: it comes from the
library being smaller than the cache, not from popularity.

**Admission control is worth far more in bandwidth than in hit rate**, at
16 MiB and skew 1.2, sweeping the threshold against the decay half-life:

```text
  half-life    admit>=1        admit>=3        admit>=5        admit>=10
  none      72.0% 2295 B   74.2%  693 B   75.1%  393 B   75.7%  183 B
  200 000   72.0% 2295 B   74.8%  490 B   75.9%  278 B   76.9%   91 B
   20 000   72.0% 2295 B   78.0%  161 B   78.2%   25 B   64.2%   13 B
    2 000   72.0% 2295 B   69.0%   16 B   50.5%   10 B   28.1%    7 B
```

( hit rate and fill bytes per operation. `admit>=1` is fill-on-miss and is
invariant to decay by construction, which is the simulator checking itself. )

**The best row is `half-life 20 000, admit>=5`: 78.2% hit rate, 2.99x, 25 bytes
of fill traffic per operation, 1% futile admissions** -- against fill-on-miss's
72.0%, 2.58x, 2295 B/op and 65% futile. **A 92x reduction in fill bandwidth
while the hit rate goes up.** The hit rate is the smaller half of that; the
bandwidth is the part that decides whether a link can carry the policy at all.

**Threshold and half-life are one parameter, not two.** At half-life 20 000,
`admit>=10` collapses to 64.2% -- below fill-on-miss -- because evidence decays
faster than ten observations accumulate. At half-life 2 000 everything
collapses. So the rule is not "admit after five"; it is "admit after about five
*within a window of roughly 15 000 accesses*", and quoting the threshold without
the window is quoting half a parameter. The projection in the LTM document had
only the threshold.

**And the projected speedup is corroborated by an independent route.** 74.2%
hit rate here yields 2.71x; the A100 projection at 74% gave 2.97x from published
specs and a bandwidth model. Two unrelated derivations landing within 10% is
worth more than either alone.

**A correction to the adaptive-admission section as written.** It said the
threshold is safe because a wrong admission merely wastes bandwidth. That is
true of admission and false of the *threshold*: at 1 MiB, where capacity is the
binding constraint, raising `admit` from 1 to 10 *halves* the hit rate
( 4.8% -> 2.0% at zero skew, 24.2% -> 16.5% at skew 1.2 ). The counter pays when
the cache is large enough to hold a working set and costs when it is not, so it
is not the free win the projection implied.

### What it still cannot do

Supply a hit rate. Every number above is conditional on a synthetic stream, and
the program prints that line itself rather than leaving it to a reader.
`--trace` replays a captured key stream through the identical counters, which is
the seam where these become results; nothing has been captured yet. The
remaining work is a capture point on the ordinary query path, not more
simulation.

---

## 2026-09-23 — Stage 7g: bounded direct identity ranks

The measured range plan landed without a core expression node. For a direct
persisted key under an interleaved identity rank map, Flight now ceil-divides
`upper * sets` into a half-open physical-prefix range and asks the snapshot for
only that range. Endpoint arithmetic is `u128` and caps at the legal exclusive
prefix `2^48`, so `u64::MAX` and the final legal physical prefix do not wrap.
Core's checked `stream_view_ranks` terminal reuses the cardinality reducer for
whole interleaved chunks and maps ordinals only in the possible partial final
chunk. Blocked layouts, transformed inputs, and non-identity bodies retain the
materializing oracle.

The allocation test caught a real integration mistake before completion. The
first dispatch insertion was nested inside the cardinality branch and was
therefore unreachable for rank; the public rank call allocated 125,052 bytes
against a 73,716-byte bounded-stream control. Moving the exact-shape dispatch
to the top level made the regression pass without raising its budget. Two
deliberate mutations also proved the semantic layers: treating every touched
chunk as whole changed a strict partial rank from `[0, 1]` to
`[21845, 21845]`, and replacing prefix ceil-division with floor-division made
the endpoint unit return zero prefixes for the first logical value.

Three post-change pinned Cortex-X925 runs put public singleton ranks at
1.660-1.682, 17.477-17.617, 66.945-67.943, and 271.186-273.510 microseconds for
16, 256, 1,024, and 4,096 occupied chunks. The direct bounded reference was
1.594-1.646, 17.369-17.590, 66.345-67.522, and 267.805-272.550 microseconds.
Dense array rows had the same near-parity result. The pre-change public path
was 4.778-4.793, 45.33-45.67, 173.00-173.80, and 674.43-682.61 microseconds.

The required workspace Clippy gate with all targets and features, formatting
check, full `yesno-core` suite, and server-enabled `yesno-flight` suite all
pass. Layout, R1 public-surface, self-contained-doc, unsafe-count, and diff
checks also pass. The property layer compares strict boundary ranks against
independent `BTreeSet` rows through `u64::MAX`; Flight covers resident and
checkpoint/reopen evaluation; the allocation layer rejects reconstruction of
the packed input.

---
## 2026-09-23 -- The capture point, and the identity bug that would have faked an answer

Built the capture point the previous entry named as the remaining work, on top
of the Codex session's committed tree ( `760f6e7` ). The hotspot observer can
now be fed a real query stream: live Flight server -> trace files -> hit-rate
curve, demonstrated end to end.

### Where it went, and the rule it is judged against

`yesno-flight`, behind a non-default `hotspot-trace` feature, in a **private**
module -- `mod hotspot`, never `pub mod`, so R1, R6 and R7 make no semver
promise. Nothing in `yesno-core` changed; the module reaches it only through
published API. `check-r1.py` and the other six invariant scripts pass.

CLAUDE.md says research does not ship, and this is still an instrument in a
shipped crate's source. That is a deliberate judgment: a capture point has to
be where the traffic is, and the alternatives were worse -- `CoreEvent` was
checked first and is storage-lifecycle only, with a synchronous sink that a
per-container event would be the wrong cardinality for. What keeps it honest is
that it is doubly inert ( compiled out without the feature; a single `OnceLock`
returning `None` without `YESNO_HOTSPOT_TRACE` ) and deletable in one commit.

### The bug worth recording: pointer identity is a silent false negative

The obvious container key is the leaf `OrdSet`'s address plus the chunk prefix,
which is what the offline observer uses over resident sets. **On the Flight
path it measures nothing.** `lower` turns every `SetExpr::Key( k )` into
`Snapshot::key_expr`, which allocates a *fresh* `Arc<KeySource>` per query. So
pointer identity makes every leaf unique on every query, and the trace reports
zero reuse regardless of how hot the workload is.

**The reason this is worse than an ordinary bug**: a flat recurrence
distribution is exactly the outcome that *retires* both open questions. The
instrument would not have crashed or looked broken -- it would have confidently
reported "do not build the GPU satellite, do not lower the JIT floor", which is
the cheap answer everyone was hoping for. It was caught only by running the
real server and noticing the container trace contained nothing but its own
header. There is now a named regression test,
`the_same_key_yields_the_same_containers_across_snapshots`.

The fix is that identity is the **posting-list key**, taken from the wire
expression before lowering, where `SetExpr::Key` still says which list it is.
`dyn ChunkSource` cannot be asked -- it exposes statistics but not the key and
there is no downcast -- so the container hook sits on `expr::cardinality` with
the wire expression while the shape hook sits on `expr_cardinality` with the
planned expression the JIT keys on. **Two hooks, each where its data is.** The
key is also the identity that is stable across *snapshots*, which the address
never was: a new snapshot would have made every container look cold.

### Two more things only the real path showed

**Shape files are per thread.** `DagJit`'s cache is thread-local and
`MAX_CACHED_SHAPES` is a per-thread bound, so a workload repeating one query
across 64 workers amortizes nothing and a merged file would report the exact
opposite. The TODO names this distinction as the sharpening that makes the
measurement worth taking, and it is now structural rather than a note.

**Filenames carry the pid.** Several processes share a trace prefix -- the test
suite does exactly that -- and `File::create` truncates, so the first capture
run silently lost most of its own output to the next test binary.

### Fidelity, stated rather than assumed

Three approximations, all documented in the module and all erring toward
*overstating* reuse, so any hit rate derived from a capture is an upper bound:
a leaf contributes `chunk_count` containers indexed `0..n` rather than its real
prefixes ( enumerating them needs a stream walk, which is I/O per query and
would change the behaviour being measured ); only leaves whose chunks are all
bitmap count, via `ChunkSource::all_bitmap_chunks`; and a leaf is capped at
4096 containers.

### Validation

Ten unit tests plus an integration test that drives a live server. Both hooks
were mutated to confirm the tests are load-bearing: removing the container hook
fails `a_live_flight_query_is_captured_to_a_replayable_trace` and nothing else;
reverting the per-thread file assignment fails `each_thread_gets_its_own_shape_
file` and nothing else. The integration test exists specifically because the
unit tests supply their own expression and therefore cannot see the hook being
deleted -- which is the failure that actually happened.

Clippy caught a genuinely vacuous assertion in the first draft: a
`.map( parse ).count()` that discarded the parse, so "every token is a decimal
`u64`" was checked by nothing. `cargo clippy --workspace --all-targets
--all-features -- -D warnings` clean, `cargo fmt --check` clean, the default
`yesno-flight` build unaffected, `yesno-core/src/jit.rs` byte-identical to what
the Codex session committed.

### What is still missing

A workload. The pipe is proven and the numbers it currently produces come from
the test suite, which is not traffic. Capturing a real stream is an operational
step, not a coding one.

---

## 2026-09-23 -- Ungating the capture point, and what the idle path actually costs

The hotspot capture point shipped behind a non-default `hotspot-trace` feature.
That gate is now **removed**: the module compiles into every server build and
`YESNO_HOTSPOT_TRACE` is the only switch.

**The reason is the point of the tool.** A capture that needs a custom build
will never meet production traffic, and production traffic is the only traffic
whose distribution anyone wants. Ungated, an operator sets one environment
variable on a binary that is already deployed. Gated, someone must first
convince a release process to ship a research build -- which is how instruments
end up never being run.

### The measurement that permits it

Two arms of one probe at `.agents-workspace/tmp/hotspot-cost/`: the hook
compiled out ( today's production build ) against the hook compiled in with the
environment variable unset. Pinned to one core, ten interleaved rotations, and
the control binary stayed **byte-identical** across every rebuild, so any
difference is the hook and nothing else.

```text
                      idle          capturing
  Key( 1 )           779 ns           2402 ns     3.08x
  K7 AND K8         5562 ns           9785 ns     1.76x
```

**Idle is below the measurement floor.** The instrumented build measured 31 ns
*faster* on the cheap query, in 100% of pairings -- which cannot be a causal
effect of adding work. Incidental codegen and layout differences are worth
about +-30 ns on an 800 ns call, and the hook's own work, one relaxed load and
a predictable branch, is far under that. `Key( 1 )` is also the cheapest query
the path can serve, with no gRPC or network in it, so a real Flight request
pays a smaller fraction still.

The "capturing" column is the control that proves the arms differ at all: the
same binary, same core, with the variable set. A 1.8x-3.1x regression is
unmistakable, so the ON binary demonstrably contains a live hook, and the idle
column is a real null rather than a hook that was optimized away.

### A defect the measurement caught, which is why it was worth taking

The first fast path used a two-state `AtomicBool` initialized to `false`. That
cannot distinguish "capture is off" from "not resolved yet", so **every idle
call fell through to the `#[cold]` resolver** -- an out-of-line call on what was
supposed to be the fast path. It measured **+18 ns per query, consistently, at
7% pairwise overlap**: precisely the overhead the flag had been added to
remove, and it would have been reported as the cost of the feature. The
tri-state `UNRESOLVED / OFF / ON` that replaced it measures at nothing. An
unmeasured fast path is not a fast path.

This also explains an earlier confusion in the same session. Before the flag
existed, two four-rotation runs disagreed about whether a ~15 ns delta was real
-- one showed overlap, one did not. Both were contaminated: the delta was real
( the `OnceLock` call was not being folded into the caller ), and four
rotations could not resolve it against machine drift. Ten interleaved
rotations, with min *and* median *and* a pairwise-win count, separated the two.

### What ungating costs elsewhere

Nothing in the gate, and it gains coverage: the ten unit tests and the live
server integration test now run in the **default** `cargo test -p yesno-flight`
( 61 tests ) instead of only under a feature nobody passes. The client-only
build is unaffected -- the module keeps a `#[cfg(feature = "server")]` gate,
which is a dependency requirement ( it needs `yesno-core` ) and not a switch.
`cargo clippy --workspace --all-targets --all-features -- -D warnings` clean,
`cargo fmt --check` clean, the seven invariant scripts pass, `check-r1` included
-- the module is private, so nothing became public API.

**The tension with CLAUDE.md is sharper now, and stated rather than buried.**
"Research does not ship" and this now ships unconditionally. It remains private,
inert, and deletable in one commit, and it should be deleted once the
distribution is known. Ungating was a direct instruction, taken after the cost
was measured rather than assumed.

**The operator warning belongs with the switch, not in a footnote**: capture
writes unbounded, unbuffered output and costs up to 3.1x. It is a sampled,
time-boxed diagnostic, not telemetry to leave on.

---

## 2026-09-23 -- Publishing through tracing, and bounding what a capture commits to

Two pieces of review feedback landed on the capture point, and both were right.
**The two entries above describe a design that no longer exists**: the bespoke
trace files, the `YESNO_HOTSPOT_TRACE` variable, the pid-stamped names, the
per-thread file table and the hand-rolled enabled-flag are all gone.

### One: it should publish through `tracing`, not invent a channel

This crate already spans every request, already emits structured events, and
the server already wires an `EnvFilter` plus an OTLP layer. Writing files next
to that was a second output channel where a configured one existed. It now
emits on target `yesno::hotspot` at TRACE, and filtering, routing and retention
belong to whoever runs the deployment.

Three things fell out of the change rather than being argued for:

* **The payload shrank by about three orders of magnitude.** Container keys are
  `hash( key, i )` for `i` in `0..chunk_count`, which is *entirely determined*
  by `( key, chunk_count )`. The old capture expanded up to 4096 hashes per
  leaf into the trace. It now emits the two integers and the observer expands
  them. **That flaw had nothing to do with transport** and would have survived
  indefinitely in a file nobody questioned.
* **The cross-process hash contract disappeared.** Container identity is the
  posting-list key, so no two programs have to agree about hashing any more.
* **The hand-rolled fast-path flag disappeared**, replaced by the mechanism it
  was imitating.

### Two: emitting once per query is an open-ended commitment

An enabled target that emits for as long as it is on promises unbounded volume
on a server that may serve thousands of queries a second, for a measurement
that stops improving long before anyone remembers to turn it off. Two fixes:

**A capture spends a budget and disarms itself** -- 250 000 queries, then one
`hotspot.done` event and silence. Demonstrated rather than asserted: 200 000-
iteration rounds against that budget measure min 794 ns and max 2094 ns, a
164% spread, because the first round captures and every later one runs
disarmed. **794 ns is the idle number**, so a spent capture costs exactly what
never enabling it costs.

**Bounding by stopping, not by sampling, is forced by the metric.** Stack
distance is defined by the accesses falling *between* two touches of a key, so
dropping one query in ten does not add ten percent of error -- it deletes the
quantity being measured and reports distances far shorter than the truth. A
contiguous window measures every distance below its own length exactly. This is
also why these events must not ride the OTLP span sampler.

**And the per-key work came off the per-query path.** Chunk count is a property
of the posting list, not of the query, and obtaining it costs a `key_expr` plan
build -- which was most of the cost of having the target on. It is now emitted
once per key per capture as `hotspot.key`, and `hotspot.query` carries only the
key list, which is a tree walk and a format. In a 2000-query capture that is
**6 dimension events instead of 2000**.

```text
                   idle      capturing ( before )   capturing ( after )
  Key( 1 )        817 ns        2402 ns  3.08x        2107 ns  2.58x
  K7 AND K8      5553 ns        9785 ns  1.76x        7265 ns  1.31x
```

The residual is the event construction and formatting itself, which is what
emitting per query costs and cannot be optimized away without sampling.

### The `Interest` question, answered from the source rather than memory

Asked whether configuring `Interest` correctly would reduce the overhead. The
generated guard in `tracing` 0.1 is:

```rust
let enabled = level_enabled!($lvl) && {
    let interest = __CALLSITE.interest();
    !interest.is_never() && __is_enabled(__CALLSITE.metadata(), interest)
};
// level_enabled! = $lvl <= STATIC_MAX_LEVEL && $lvl <= LevelFilter::current()
```

**For the idle path, yes, and it is already doing it** -- which is why the idle
cost measures at nothing. With the target filtered out, the subscriber's
`max_level_hint` puts `LevelFilter::current()` above TRACE, and the event dies
at an integer compare against a global atomic: it never reaches `interest()`,
never touches the callsite cache, never calls the subscriber. `Interest::never`
is the next rung down, for a callsite whose level is globally on.

**For the enabled path, no.** `Interest` is tri-state and cached *per
callsite*, not per event; it cannot express "one in N". A subscriber *can*
sample inside `enabled()` by returning `Interest::sometimes()`, and because the
expensive work here sits behind a `tracing::enabled!` guard that would
genuinely cut the cost -- but it is the wrong lever for this metric, for the
stack-distance reason above. Recorded because the mechanism is real and the
objection to using it is statistical, not technical.

Also worth recording: **do not reach for `release_max_level_*`**. It would
compile these callsites out of release builds, which is precisely the ungating
benefit thrown away.

### Validation

Thirteen unit tests and the live-server integration test, all in the default
build ( `cargo test -p yesno-flight`, 64 tests ). The budget is injected rather
than read from the process-wide static, because a test that spent the real one
would silently disarm every test after it; `spending_is_exact_under_concurrency`
drives eight threads through one counter. `a_posting_list_is_described_once_
however_often_it_is_queried` is the regression test for the overcommit fix.

The observer gained a parser for `tracing` output and lost its two bespoke
formats -- 41 tests, including one that feeds it real
`tracing_subscriber::fmt` output produced by a live query path, with
`with_target( false )` as the server configures it. Parsing is deliberately
loose about formatter, quoting and span context, and strict about one thing:
`key` is a prefix of `keys`, so a sloppy match would read every query event as
a dimension event. There is a test for exactly that.

One bug found by the JSON case: the field extractor treated `,` as a value
terminator, which is right for `{"key":7,"chunks":2}` and wrong for
`keys=7,8`. The cut now lives in the single-valued accessor, which knows its
field holds one number, rather than in the shared extractor, which cannot tell.

`clippy --workspace --all-targets --all-features -- -D warnings` clean,
`cargo fmt --check` clean, seven invariant scripts pass, `yesno-core`
untouched.

---
## 2026-09-23 -- Moving the capture point into yesno-core

The hotspot capture point now lives at `yesno-core/src/hotspot.rs` rather than
in `yesno-flight`. Moved on an explicit decision, not drift, and the entry
above describes its previous home.

**The reason it belongs here**: what it measures is a core concern --
posting-list and container recurrence, and the planned-shape recurrence that
decides `jit.rs`'s own `AUTO_MIN_CHUNKS`. None of that is about Arrow Flight.
The capture sat in the wire crate only because that is where the query path
happened to be hooked.

### The container half needed a redesign to make the move legal

`yesno-core` does not depend on `yesno-wire` and must not: that would invert
the layering to reach a type the core has no other use for. But container
identity is the **posting-list key**, and the only place that survives is the
*wire* expression -- which is exactly the finding that fixed the original
identity bug, where `lower` allocates a fresh `KeySource` per query and address
identity reported zero reuse for every workload.

So `record_containers` now takes `&[u64]` -- the keys already extracted --
instead of a `&SetExpr` to extract them from. The genuinely wire-shaped step,
`SetExpr::keys`, stays at the call site in `yesno-flight`; everything that is
not about the wire format moved. The core gains no dependency.

That introduced one hazard and one fix for it. Naively the call site would
build a `Vec` per query to hand to a function that discards it when no capture
is running -- reintroducing the exact per-query cost two rounds of measurement
had just removed. Hence `hotspot::enabled()`, exported so the caller can skip
building the list at all:

```rust
if yesno_core::hotspot::enabled() {
    let mut named = Vec::new();
    e.keys(&mut named);
    yesno_core::hotspot::record_containers(&named, snap);
}
```

**Measured cost-neutral.** Idle 798.8 ns cheap / 5630.4 ns dense against
817.2 / 5553.0 before the move; capturing 2118.9 / 7300.5 against 2106.9 /
7264.5. Every difference is inside the +-30 ns codegen band this session
already established as the floor, so the cross-crate call is being inlined and
nothing regressed.

### Semver, and the rule this sits against

The module is now **public API** -- it has cross-crate callers -- so it is
marked semver-exempt on the [`unstable_arrow`] precedent, which is this
crate's existing convention for a public module that is not promised.

CLAUDE.md says research does not ship and names `stats.rs` as the precedent: a
1 600-line instrument in this crate with **zero callers**, deleted once the
decision it gated was made. This is a deliberate exception and it differs from
that precedent in the way that matters -- *it has callers*, and the entire
point of it is to run inside a deployed server. It is inert unless a subscriber
asks for `yesno::hotspot`, and a capture is bounded, so even switched on it
stops by itself. What it must not become is permanent: when the distribution is
known, delete the module, its line in ARCHITECTURE's diagram, and the two call
sites.

### Two things the move improved, neither of them planned

**The `postfix` wildcard was dead and is now gone.** `Expr` is
`#[non_exhaustive]`, which binds downstream crates but not the defining one --
so inside `yesno-core` the compiler flagged `_ => out.push( u64::MAX )` as
unreachable. Removing it makes adding an `Expr` variant a **compile error**
here until someone assigns it a shape marker, which is strictly better than
the runtime fallback the module needed in `yesno-flight`, where a new variant
would silently have joined an existing class and merged two shapes the JIT
compiles apart.

**No new dev-dependency.** The tests used `tempfile`, which `yesno-core` does
not carry. Rather than add it -- `crate_universe` reads these manifests, so it
would cost a Bazel re-pin -- they now use the house `tmpdir` plus `CleanDir`
convention already used by `db/mod.rs` and `db/keystream.rs`.

`scripts/check-layout.py` required ARCHITECTURE's module diagram to gain the
file, which it has; the check verifies that in both directions.

### Validation

Thirteen unit tests moved with the module and pass under
`--features tracing`; the live-server integration test stays in `yesno-flight`,
where the path it exercises is. `clippy --workspace --all-targets
--all-features -- -D warnings` clean, `cargo fmt --check` clean, seven
invariant scripts pass.

---
## 2026-09-23 -- Flight write transactions, and four things I got wrong on the way

Answered the CDC handoff and its haiiie addendum, then built what they asked
for. Branch `write-transactions` off `main`. The contract analysis is in
`LTM/flight-write-transactions.md`; this entry is the working record.

**No write transaction existed.** `Ticket` is `{ version, key, prefix range,
expr }` and is entirely about reading; the action surface was single-shot;
`do_put` never saw a ticket; neither `yesno-flight` nor `yesno-wire` mentioned
a transaction. The atomicity unit was one Arrow record batch.

**But the gap was narrower than the handoff assumed.** `WriteBatch` already
took insert, remove, insert_range, remove_range and delete_key, mixed, across
arbitrary keys, in one commit -- `do_put` itself called `wb.insert` and
`wb.remove` on the same object. And order survives: commit sorts through a
`( key, arrival index )` pair, which the core documents as *"stability is
structural here rather than a property of the algorithm"*. Only the wire could
not say any of it.

### The wire break, taken deliberately

`do_put` treated an **absent or unrecognised** command as insert. That made
every future command a trap: `apply` sent to a server that did not know it
would have had its *removals applied as insertions*, silently. A command is
now required.

This was originally going to be a careful narrowing -- keep absent meaning
insert, reject only unknown spellings -- because the doc comment said the
default was load-bearing for callers that send no descriptor. Being told
nothing has been released publicly turned a mitigation into a fix: absent,
empty and unrecognised are all errors now, and the hazard class is gone rather
than reduced.

### Four things I got wrong

**One: I said two callers relied on the default. There were six.** I grepped
`yesno-server/src/bin` and `yesno-e2e/src`, declared "only two call sites",
and moved on. The flight suite then hung for twenty-three minutes on
`reactor.rs`, which builds a `do_put` by hand. A proper audit -- every
`FlightDataEncoderBuilder::new()` in the tree, checked for
`with_flight_descriptor` -- found six, plus one that correctly has none
because it encodes a `DoGet` *response*. Grep the construct, not the
directories you expect it in.

**Two: the order fixture did not discriminate.** The addendum warns that
*"replaying operations grouped by kind would fail this fixture while still
appearing atomic"*, so I wrote one. It passed against a server deliberately
mutated to group by kind. Every case I had written put removals *before*
insertions -- which is exactly what grouping produces anyway. The
discriminating case is a removal that *follows* an insertion: insert then
delete-key must leave the key empty; insert then remove must leave it absent.
With those added the mutation fails with the message that explains why.

A fixture that cannot fail is worth less than no fixture, because it is
evidence in the wrong direction.

**Three: a `#[cfg]` attribute got orphaned by a text insertion.** Inserting
helper types before `impl YesnoFlightService` put them between that block and
its `#[cfg( feature = "server" )]`, so the attribute silently moved to my
struct. Twenty-seven errors -- in the *Bazel* build, six minutes in, because
the cargo default build has `server` on and never sees it. Reproduced locally
in seconds with `cargo build -p yesno-flight --no-default-features`, which is
the configuration Bazel uses for the client-only `yesno-pg` path. That check
belongs in the routine loop for this crate, not only in the gate.

**Four: I piped a gate into `tail` earlier in the session and got `tail`'s
exit code.** Not repeated here -- this run redirected to a file and reported
`GATE EXIT CODE: 1` honestly, which is how the orphaned `cfg` was caught at
all.

### The drivers

Both were doing the thing the handoff described, and both are now one commit.

`yesno-pg`'s `flush()` grouped by `( endpoint, key )`, opened a **fresh
connection per key**, and called `put` twice -- so a transaction touching *n*
keys published `2n` versions. It now groups by endpoint and stages through one
write transaction. Atomicity is per endpoint and cannot be more: a table set
spanning two servers is two commits, and that is recorded rather than hidden.

`yesno-mysql`'s Flight backend issued a `Clear` per cleared key, then
`RemoveMany`, then `InsertMany` -- `c + 2` versions, with removals visible
without insertions. The *embedded* backend already applied the whole plan
atomically through one `yesno_batch`, so the two backends differed in
semantics and not merely in speed. They now agree. A key's `DeleteKey` is
staged ahead of its own writes, which is the ordering the fixture above
exists to protect.

That needed a transaction surface on the C++ client, which had none.

### Not done, and named

Ownership and leadership fencing. The Flight service authenticates nobody, so
a handle is bound to whoever holds its eight bytes. Acceptance item 5 of the
addendum is unmet and is recorded as unmet rather than tested vacuously.

### The documents had drifted further than the one file I first noticed

I first found `yesno-mysql/README.md` and
`LTM/database-apis-and-satellite-crates.md` describing a nontransactional
MySQL engine while `ha_yesno.cc` had stopped advertising `HA_NO_TRANSACTIONS`
and implemented savepoints. Grepping the claim rather than the file found
**eight** places saying it, spread across `README.md` ( twice ), `OVERVIEW.md`,
`ARCHITECTURE.md` ( twice ), `TESTING.md` and both of the above. One of them
asserted the *opposite* of what the fixture asserts: the LTM said a buffered
insert is "still present" after `ROLLBACK`, and `e2e/mysql/mysql.py` fails with
"ROLLBACK must discard a buffered INSERT". A document that contradicts a
running assertion is not merely stale.

All eight are corrected to say what is true -- writes buffered per connection,
applied as one version at commit, `ROLLBACK` discards, `SAVEPOINT` unwinds --
while keeping the limitation that *is* still real: there is deliberately no
two-phase `prepare`, so a crash between this engine's commit and MySQL's binlog
write leaves the two disagreeing. That is the same gap `fdw-two-phase-commit`
records on the PostgreSQL side, and saying so links them instead of stating it
twice.

The write-transaction surface itself is now documented in `docs/integrations.md`
( the required command, `apply`, and the stage-and-commit flow ) and in
`docs/data-modeling.md` ( which unit of a Flight write is atomic ). The
`docs/` self-containment checker still reports an empty baseline.

**The lesson is the grep, not the edits.** Finding one wrong document is
evidence about the claim, not about the file. Searching for the claim across
the tree cost one command and found seven more.

---
