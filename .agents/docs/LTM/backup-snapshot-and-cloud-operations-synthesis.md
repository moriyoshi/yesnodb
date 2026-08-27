# Backup, Snapshot, and Cloud Operations Synthesis

## Summary

Recovery is a chain of separately owned guarantees: the server grants a consistent snapshot lease, the backup or archive client publishes recoverable bytes, restore verifies and stages a selected history prefix, and the deployment supplies the authority needed to materialize provider snapshots. Local filesystems, EBS, Kubernetes, and object stores differ in mechanism, but they share strict rules for identity, fencing, publication, and cleanup. A green integration arm proves only the authorities and lifecycle it actually exercised.

## Included Documents

| Document | Focus |
|----------|-------|
| [Backup, Archive, and Point-in-Time Recovery](./backup-archive-and-pitr.md) | Consistent bases, immutable WAL history, writer fencing, restore selection, and reachability-based retention. |
| [Snapshot Leases, Providers, and Privilege Separation](./snapshot-leases-providers-and-privilege-separation.md) | Lease lifecycle, provider ownership, privileged-agent boundaries, and local or deferred materialization. |
| [Kubernetes Operator, Failover, and Certificates](./kubernetes-operator-and-failover.md) | Persistent-volume discovery, rollout state, EBS configuration, failover, and Kubernetes cleanup. |
| [Network Service, Replication, and Operations](./network-service-replication-and-operations.md) | Snapshot and control RPCs, WAL retention acknowledgements, sparse bootstrap, identity, and leadership fencing. |

## Stable Knowledge

- A base backup is valid only when every shard image carries one common `checkpoint_cv`. Individually valid images from different checkpoints are not a database snapshot.
- Snapshot leases become inaccessible before cleanup begins. Failed cleanup remains registered and retryable, and reconciliation may inspect only resources whose provider-specific ownership is proven inside the configured database namespace.
- Archive publication orders immutable objects before manifest, conditional `state.pb`, local cache, and replication acknowledgement. The acknowledgement means remote recovery is durable enough for the leader to release WAL.
- Archive schema v2 fences writers with a conditional lease and compare-and-swap state. Local archives use a kernel file lock; that is not evidence that conditional object-store behavior works.
- Wall-clock restore depends on invariant I9: resolving commit markers carry non-decreasing times assigned with commit versions, and all shards in one commit share the stamp. Descriptor times are search hints; verified WAL frames choose the cut.
- Archive reclamation is duration-based reachability, disabled by default. Unknown keys and bases with unknown commit times are retained. A WAL floor for `( term, shard )` is valid only if every retained base of that term names the shard.
- Portable, ZFS, Btrfs, LVM, and EBS providers implement one lease contract but have different authority. LVM and local EBS attach or mount through `yesno-snapshot-agent`; the network daemon remains unprivileged.
- Local EBS materialization attaches a tagged clone to the daemon host. Deferred ECS and EKS materializers are archive-owned remote workers and receive no host attachment capability.
- The Kubernetes operator is the independent witness between a claim, its bound PersistentVolume, and the EBS CSI volume handle. It accepts backend policy and tags, never a caller-supplied volume ID.
- `WaitForFirstConsumer` makes EBS configuration a two-stage rollout: start portable, discover the bound volume, render the EBS configuration, and wait for Pods carrying the new configuration identity. `SnapshotBackendReady` is distinct from serving `Ready`.
- EKS restoration owns a retained VolumeSnapshotContent, VolumeSnapshot, PVC, and Job. Job deletion uses background propagation so the Pod exits and releases PVC protection before CSI cleanup.
- A restored copy intended for writes must raise its leadership term. Database UUID identifies a history, while the term fences competing writers within that history.
- Real-AWS local EBS plus ECS, deferred EKS, and operator arms have each passed, but only in partial combinations. Their shared stack and cleanup remain unproven until one four-arm run passes.

## Operational Guidance

- Begin by naming the recovery product and authority boundary: hot base, continuous archive, local provider lease, or deferred cloud materialization. Do not substitute replication for backup.
- For a base, hold the server lease through transfer, verify the common checkpoint watermark and shard identities, stage into a unique sibling, synchronize it, and publish with one rename.
- For an archive, preserve immutable frame identity, publish manifests last, compare-and-swap state, persist the local cursor, and only then acknowledge retention.
- For wall-clock restore, obtain target times from the archive inspection surface, reject unstamped history, verify the selected frames, and choose `publish`, `pause`, or `promote` according to whether the result will be written.
- Keep attach and mount on the same trusted side. Validate EBS identity from tags and normalized Nitro serials rather than trusting requested device paths.
- In Kubernetes, wait for both backend status and running Pods with the current rendered configuration. Treat an early condition update as progress, not completion.
- Reconcile deletion from exact ownership evidence. Preserve resources after delegated detach failure, and collect diagnostics before Jobs, claims, namespaces, or Terraform state are removed.
- Report emulator, native-filesystem, and live-cloud results separately. They exercise different authority and consistency mechanisms.

## Files

- `yesno-server/src/snapshot.rs` owns lease state, provider selection, and reconciliation.
- `yesno-server/src/snapshot/{lvm,ebs,agent}.rs` and `yesno-server/src/bin/yesno-snapshot-agent.rs` own privileged local materialization.
- `yesno-server/src/control.rs` exposes authenticated lifecycle and snapshot control.
- `yesno-server-utils/` owns base backup, archive, restore, object-store publication, and deferred materializers.
- `yesno-server/src/replication/` owns WAL streaming and retention acknowledgement.
- `yesno-operator/` owns claim discovery, rendered configuration, rollout identity, and status.
- `e2e/scenarios/{basebackup,archive,pitr,pitr_retention}.py`, `e2e/filesystems/`, `e2e/operator/`, and `e2e/aws/` own progressively stronger operational evidence.
- `docs/operations.md` owns the operator-facing recovery, promotion, and disaster-recovery procedure.

## Tests

- Run the portable recovery scenarios through `cargo run -p yesno-e2e -- <scenario>`.
- Run `./scripts/gate-filesystems.sh` for real ZFS, Btrfs, LVM, snapshot-agent, and deployed archive behavior.
- Run `./scripts/gate-operator.sh` for the ordinary Kubernetes lifecycle and failover surface.
- Run `./scripts/gate-aws.sh` only with the explicit billable-gate opt-in for local EBS, deferred ECS, deferred EKS, and operator behavior.
- Recovery properties must cover mixed shard watermarks, frame-chain gaps or forks, unstamped target refusal, conditional writer fencing, interrupted reclamation, and writable-restore promotion.
- Cloud cleanup assertions must observe at least one owned resource before requiring zero, or the result is vacuous.

## Pitfalls

- Do not take the maximum checkpoint version from mixed shard images.
- Do not acknowledge a WAL position before the corresponding remote state is durable.
- Do not use object age as commit age or infer one retained base's shard cursor from another base.
- Do not treat a packaged privileged binary as granted authority; deployment identity and capabilities are the authority.
- Do not derive the EBS volume ID from the mount whose identity the operator is supposed to verify.
- Do not treat `SnapshotBackendReady` as proof that the matching configuration has reached running Pods.
- Do not orphan an EKS materializer Pod that still protects a PVC.
- Do not interpret separate green cloud arms as proof that one full run composes.
- Do not destroy cloud state before collecting the identifiers and errors needed to diagnose failure.
- Do not act on a recorded cloud-cost note without re-inventorying first: such a note is a claim about a live external system at a moment that has passed, and one was found stale in both directions at once -- the expensive resource it named was already gone, and the volumes actually blocking every later run came from a run it did not list. One tag query over the run-id tag key returns every tagged resource grouped by run.
