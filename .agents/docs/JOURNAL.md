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
