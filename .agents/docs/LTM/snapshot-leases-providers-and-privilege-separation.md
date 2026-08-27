# Snapshot Leases, Providers, and Privilege Separation

## Summary

`yesnod` owns snapshot namespaces, lease identity, and the checkpoint consistency barrier, while provider-specific capture and cleanup supply immutable file-bearing views. Privileged effects are delegated only when the provider cannot safely grant them to the daemon's ordinary account, and the privileged side derives every resource from its own configuration and server-generated namespace rather than trusting caller-selected paths or devices.

## Key Facts

- `Db::begin_backup` holds a short checkpoint-only barrier through provider capture; reads and ordinary WAL commits may continue.
- Snapshot leases are opaque, renewable, and bounded. Release, expiry, shutdown, and startup reconciliation own cleanup.
- Portable copies, ZFS, Btrfs, LVM, and local EBS all publish the same file-bearing lease contract.
- Direct paths require both server policy and an explicit client request. Portable staging never exposes one.
- ZFS snapshot and destroy privileges can be delegated with `zfs allow` on the measured platform.
- An unprivileged Btrfs owner can create a read-only snapshot but cannot delete it; cleanup clears the read-only property after a failed delete and retries once.
- LVM and local EBS attach/mount operations are privileged and run through `yesno-snapshot-agent`, not the network-facing daemon.
- Attach and mount form one privilege boundary: the side choosing what is attached controls what a later mount exposes.
- A gate cannot verify least privilege while running the subject as root. Authority assertions inspect effective runtime identity and capabilities.
- Local EBS and both deferred materializers have passed against real AWS; emulator coverage remains complementary, not equivalent.
- EKS Job deletion uses background propagation so worker Pods release PVC protection and CSI volumes can be reclaimed.

## Details

### Lease and consistency contract

The control service creates, renews, streams, and releases snapshot leases. The server retains ownership of the database UUID namespace, operation IDs, lease names, checkpoint barrier, and provider registry. A client receives flat recoverable filenames and immutable contents, never authority to select an arbitrary local path.

A filesystem snapshot taken between two shard superblock flips is not one database checkpoint. `Db::begin_backup` therefore holds a checkpoint-only barrier until provider capture has fixed the snapshot or staged file sizes. The lease is released from the barrier before transfer or object upload. A portable WAL prefix may end during a commit; ordinary recovery trims it to the highest globally complete version.

File streaming uses contiguous offsets, one invariant total size, and an explicit last marker. Direct-path delivery is a co-location optimization and is dual opt-in. The client validates flat names, exact sizes, one shared root, and one uniform access mode before publication.

### Cleanup and reconciliation

Provider objects are named below a database-UUID namespace. Release and expiry make the lease inaccessible before cleanup. Failed cleanup remains registered, retries with bounded exponential delay, and is discoverable by a later process. Database installation starts namespace reconciliation; snapshot creation waits for its initial pass before taking the checkpoint barrier.

Reconciliation scans only the configured provider namespace and verifies provider ownership: ZFS snapshots of one dataset, Btrfs or portable names below one root, LVM snapshots whose origin matches the configured LV, and EBS resources with exact database and lease tags. Legacy pre-namespace objects are not adopted when ownership cannot be proved.

### Provider matrix

| Provider | Capture and immutable view | Privilege boundary |
|---|---|---|
| Portable | Bounded staged copy under observed file sizes | Ordinary daemon account |
| ZFS | Delegated read-only dataset snapshot and automounted view | Ordinary account with `zfs allow snapshot,destroy,mount` |
| Btrfs | Read-only subvolume snapshot | Ordinary owner; failed deletion clears `ro` and retries |
| LVM | Classic COW LV, journal recovery, read-only remount | Privileged snapshot agent |
| EBS local | EC2 snapshot and clone plus attach, journal recovery, read-only remount | Daemon owns EC2 creation/deletion; agent owns attach, mount, unmount, detach |
| EBS deferred | Provisional snapshot consumed by ECS or EKS materializer | Remote archiver-owned worker, not a host daemon capability |

LVM supports one linear ext4 or XFS origin and a fixed-size classic snapshot. Operators must reserve volume-group capacity for every concurrent lease; an exhausted classic snapshot becomes invalid.

Local EBS supports a direct whole-volume or single-partition ext4/XFS source. Nitro discovery matches normalized EBS serials rather than requested device names; Xen names are a bounded fallback. LVM, dm-crypt, RAID, multi-device, and network filesystems are refused rather than inferred.

The live EC2 path proved the whole local lifecycle: unprivileged daemon, privileged agent, tagged provider snapshot and clone, Nitro device discovery, attach, mount, lease release, and crash reconciliation. Device canonicalization is opportunistic because a deferred container can read NVMe serials in sysfs without having a corresponding `/dev` node; equality is decided from normalized device names, so removing the false `ENOENT` does not make a mismatch acceptable. AWS SDK failures must report service code and message because `SdkError`'s display alone reduces every service rejection to `service error`.

The runner's mount topology is part of the privilege contract. Binding `/mnt` over already-mounted children hides them because `mount --bind` is not recursive. Binding first is necessary but insufficient on a systemd host whose root is shared: the bind first becomes a peer of `/`, so it must be made recursively private before becoming recursively shared. A container reproduction must make its root shared explicitly or it cannot reproduce this production behavior.

### Snapshot agent boundary

The independently deployed `yesno-snapshot-agent` connects through the Unix control socket. Linux `SO_PEERCRED` and explicit `SCM_CREDENTIALS` must agree on the agent's real PID, UID, and GID; the earlier pid-zero design is invalid because Linux rejects it with `ESRCH`. Root identity authenticates the agent RPC, while `CAP_SYS_ADMIN` authorizes its storage operations. The device-mapper snapshot target must be preloaded; the agent does not receive `CAP_SYS_MODULE`.

Work items contain only an operation, database namespace, and server-generated lease name. VG, origin LV, filesystem, mount roots, AWS attachment pool, and resource lookup come from agent configuration. For local EBS, the daemon creates the tagged volume, then the agent locates it by database and lease tags, selects an attachment name, attaches and mounts it, and later unmounts and detaches it before the daemon deletes it.

The daemon must not delete around a failed delegated cleanup. If the agent cannot detach a local EBS clone, the volume remains for reconciliation. This is safer than deleting ownership metadata while a device may still be attached.

The release image includes `yesno-snapshot-agent` and `lvm2` while remaining uid 10001 by default. Packaging a privileged binary grants no authority; the deployment raises identity and capabilities only for the agent container. `btrfs` and ZFS userspace remain separate deployment dependencies.

### Delegation measurements and gate design

Measured on Linux 6.8.0, ZFS 2.2.2, and btrfs-progs 6.6.3:

| Backend | Ordinary-account result |
|---|---|
| ZFS create, traverse, and destroy | succeeds with delegated permissions |
| Btrfs read-write snapshot delete | succeeds with ownership and `user_subvol_rm_allowed` |
| Btrfs read-only snapshot delete | fails with `EROFS` until `ro` is cleared |
| LVM capture and mount | requires privilege |
| Local EBS attach and mount | requires privilege |

The filesystem KVM gate now runs `yesnod` as an unprivileged account with an empty capability bounding set and `NoNewPrivileges=true`. This exposed both Btrfs cleanup and Unix-socket mode/group defects that root-only tests hid. The socket mode is an explicit octal `server.control.unix_socket_mode` option when deployment policy needs one; the default remains umask-derived.

Mount propagation is temporal and directional. A receiver namespace created after a mount inherits a copy regardless of propagation, so a valid test starts the receiver first. Systemd `PrivateMounts=yes` plus `MountFlags=shared` creates a new peer group and does not propagate outward. The sending half needs an `rshared` bind of the snapshot root under a real container runtime; the receiving half is covered by `lvm_propagation.py`.

The live AWS gate also proves the deferred split. ECS launches an unprivileged Fargate worker with no host device or propagation access. EKS restores through a retained VolumeSnapshotContent, a bound VolumeSnapshot and PVC, and a Job using namespace-scoped credentials. The failure injections differ deliberately: an absent source path fails ECS, while EKS needs an unprovisionable StorageClass because its source-path option becomes the volume mount path and Kubernetes creates it.

### Btrfs seam direction

The pending decision is an owned provider seam, not immediate adoption of `btrfs-uapi`. The seam covers read-only snapshot creation, deletion, clearing the read-only flag after any delete failure, and a plain directory walk for reconciliation. A future typed-ioctl implementation may sit behind it.

`btrfs-uapi` 0.13 meets Rust 1.89 and exposes safe typed operations, but it is Linux-only, new, and adds bindgen/libclang to the build contract on a privileged path. An in-house ioctl implementation would add unsafe code and therefore needs a `SAFETY` invariant, a journal record, and a regression that can violate the invariant. Any replacement must be tested under the delegated account, because root does not exhibit the read-only delete refusal.

## Files

- `yesno-server/src/snapshot.rs` - lease manager, provider selection, and reconciliation.
- `yesno-server/src/snapshot/{lvm,ebs,agent}.rs` - privileged provider and broker boundaries.
- `yesno-server/src/bin/yesno-snapshot-agent.rs` - privileged worker process.
- `yesno-server/src/control.rs` - lease and agent RPC authorization.
- `yesno-server-utils/` - snapshot clients, base backup, archive, and deferred materializers.
- `e2e/filesystems/` - real ZFS, Btrfs, LVM, and propagation scenarios.
- `e2e/aws/` - Terraform-owned live EBS acceptance scenario.
- `scripts/gate-filesystems.sh` - KVM native-filesystem gate.

## Test Coverage

- Fake-provider tests cover independent leases, renewal, expiry, exact release, traversal refusal, streaming offsets, and direct-path negotiation.
- Portable-provider tests inject deletion failure and prove crash/restart reconciliation while preserving another namespace.
- Winterbaume tests exercise production EC2 and ECS SDK serialization, ownership tags, waiters, cleanup order, and request shape.
- The KVM gate exercises ZFS, Btrfs, and LVM with an unprivileged daemon and a narrowly privileged agent.
- The Terraform AWS harness has passed local EBS plus deferred ECS together, and deferred EKS separately, against real AWS. Each scenario observes a nonzero owned resource before requiring zero after cleanup.

## Pitfalls

- Do not hold the checkpoint barrier while transferring or waiting for materialization.
- Do not expose direct paths without dual opt-in or from portable staging.
- Do not infer snapshot ownership from a broad directory, volume group, or tag alone.
- Do not grant the daemon `CAP_SYS_ADMIN` to avoid designing the provider boundary.
- Do not delegate mount while leaving caller-controlled attach on the unprivileged side.
- Do not test privilege absence with a root daemon or by rereading only harness intent.
- Do not delete an EBS volume before delegated detach succeeds.
- Do not assume a shared namespace is in the host's shared peer group.
- Do not parse `SdkError` through display alone; preserve AWS error metadata and waiter source chains.
- Do not delete an EKS Job with orphan propagation when its Pod mounts a cleanup-sensitive PVC.
