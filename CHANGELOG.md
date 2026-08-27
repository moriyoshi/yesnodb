# Changelog

All notable user-facing changes will be recorded here. This project is
pre-release and has no tagged release yet.

The format follows Keep a Changelog, and published versions will follow
Semantic Versioning.

## Unreleased

### Added

- Persistent Roaring-style 64-bit posting lists with WAL recovery, MVCC
  snapshots, checkpointing, and sharding.
- Lazy Boolean set algebra with non-materializing cardinality queries.
- Arrow, DataFusion, Flight, replication, and experimental PostgreSQL
  integrations.
- `yesnod`, the `yesno` data CLI, and the `yesnoctl` administrative CLI,
  including TLS, role-based authorization, metrics, live read replicas, and
  manual promotion.
- User guides for getting started, integrations, and operations.
- `yesno query` with AND, OR, XOR, AND NOT, complement, and ranges.

### Known limitations

- No production-scale or long-lived workload history.
- No published crates, binary packages, or container image.
- Manual failover, asynchronous replication, and no automatic split-brain
  prevention.
- Point-in-time recovery requires a configured archive sidecar; a local data
  directory alone cannot be recovered to a past instant.
- PostgreSQL integration is experimental and not packaged for installation.
