# Operations

This is the operator guide for the pre-release `yesnod` service. yesnodb has not
been operated at production scale or under a long-lived real workload. Treat
the procedures below as a basis for evaluation and drills, not as a production
readiness claim.

## Operating envelope

- Use a 64-bit Linux or macOS host.
- Put the data directory on a local filesystem with coherent `pwrite` and
  `MAP_SHARED` behavior.
- Do not use NFS or another network filesystem. An mmap I/O failure can become
  `SIGBUS`, which a process cannot recover from as an ordinary error. There is a
  second, independent reason: a reader in another process is kept visible to the
  writer's space reclamation by an advisory lock on a file in the data directory,
  and advisory locking is not dependable over a network filesystem. If it
  silently does nothing, the writer can conclude that a running reader has gone
  and reuse the space it is still reading.
- Run one process per data directory. The database takes an exclusive file
  lock, so overlapping restarts fail deliberately.
- **A single shard holds at most 256 GiB.** Index nodes are named by a 32-bit
  identifier, so no byte beyond that point can be referenced. A shard that
  reaches it refuses to grow, naming both its size and the cap, and the
  database stays readable -- but the write that hit the limit fails, and no
  setting raises it. Capacity is planned with the shard count, which is
  creation-only ( see [Configuration](configuration.md) ). Choose it for
  the data the database will hold, not for the data it starts with: at 60 MiB
  per key a shard holds roughly 4 000 keys.
- Replication is asynchronous. It is not a backup and never provides zero RPO.
- There is no consensus-based leader election. Promotion is manual for a
  standalone server and automatic under the Kubernetes operator, which fences
  before it promotes by waiting for every Pod of the old primary to disappear.
  A monotonically increasing leadership term fences a superseded leader that
  survives regardless, and clients enforce it by refusing a term below the
  highest they have seen. The operator is a single external arbiter rather than
  a quorum, so its availability and correctness bound that guarantee.

## Guides

- [Configuration and service lifecycle](configuration.md) covers building,
  first start, the plugin channel, logging and tracing, the shared control
  endpoint, TLS and authentication, certificate rotation, health, metrics, and
  shutdown.
- [Container image and deployment](deployment.md) covers the published image,
  which tag to pin, and what Docker, Kubernetes, and ECS require.
- [Backup and archiving](backup.md) covers base backups, the continuous archive
  sidecar, WAL archiving, and retention and reclamation.
- [Snapshot providers](snapshots.md) covers how a consistent snapshot is
  captured and how to configure the portable, ZFS, Btrfs, LVM, and EBS backends.
- [Restore](restore.md) covers restoring an archive to a version or a wall-clock
  instant, choosing a timeline, and returning a directory to service.
- [Replication and disaster recovery](replication.md) covers standbys, measured
  RPO and RTO, promotion and fencing, and the drills to rehearse.
