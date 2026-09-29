# Documentation

yesnodb is a persistent inverted index: each unsigned 64-bit key names a set of
unsigned 64-bit ordinals, and queries combine those sets with Boolean algebra.
It is pre-release software and should be evaluated against the limitations in
the project README before it holds important data.

## Start here

- [Getting started](getting-started.md) walks through embedded use and the
  `yesnod` and `yesno` command-line workflow.
- [Data modeling](data-modeling.md) explains how to choose keys and ordinals,
  represent common relationships, and plan updates.
- [Query language](query-language.md) is the reference for expressions accepted
  by `yesno query`, including complement and cardinality-only queries.

## Connect another system

- [Integrations](integrations.md) covers Apache Arrow, Arrow Flight, DataFusion,
  and the experimental PostgreSQL extension.

## Run and recover a service

- [Operations](operations/index.md) states the operating envelope and leads to
  one guide per task:
  [configuration and service lifecycle](operations/configuration.md),
  [container image and deployment](operations/deployment.md),
  [backup and archiving](operations/backup.md),
  [snapshot providers](operations/snapshots.md),
  [restore](operations/restore.md), and
  [replication and disaster recovery](operations/replication.md).
- [Troubleshooting](troubleshooting.md) starts from visible symptoms such as a
  refused connection, stale ticket, growing disk use, or replication lag.
- On Kubernetes, an operator manages leader and follower Deployments, retained
  storage, stable read-write and read-only endpoints, and fenced automatic
  promotion. The Kubernetes section of the
  [deployment guide](operations/deployment.md) says what it arranges; its own
  guide is the `yesno-operator` README, linked from the project README.

## Understand the design

- [On-disk format](storage-format.md) specifies the database directory, byte
  layouts, checksums, checkpoint publication, and WAL recovery rules.
- [Formal model](formal-model.md) gives the mathematical model, invariants, cost
  arguments, and related work behind the storage and query engine.

The guides deliberately overlap only at hand-off points. For example, the
getting-started guide shows one query, the query-language guide defines its
semantics, and the operations guide owns the deployment procedure.
