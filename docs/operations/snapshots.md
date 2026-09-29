# Snapshot providers

A snapshot provider is how `yesnod` captures a consistent, immutable copy of the
database for a base backup or an archive. This guide covers the lease lifetime
every provider shares, the privilege model, and how to configure each backend.
The clients that consume these snapshots are in the
[backup guide](backup.md).

## Snapshot lifetime

Every server snapshot follows the same operator-visible lifetime even though
the provider-specific capture differs:

```text
  checkpoint event
         |
         v
  yesno-archive requests lease
         |
         v
  yesnod briefly excludes checkpoints and creates an immutable capture
         |
         +---- portable / ZFS / Btrfs ------------------> file-bearing lease
         |                       unprivileged daemon
         |
         +---- LVM --> privileged local agent -----------> file-bearing lease
         |
         +---- local EBS --> privileged local agent -----> file-bearing lease
         |
         +---- deferred EBS --------------------------> provisional lease
                                                               |
                                                               v
                                                archiver launches worker
         +-----------------------------------------------------+
         |
         v
  archiver renews lease while validating and publishing the base
         |
         v
  release or expiry --> yesnod removes provider resources
```

The short exclusion above is not an upload lock. WAL commits continue, while a
checkpoint waits. LVM mounting, transfer, EBS restoration, staged-file
validation, recovery, and object-store publication happen after the exclusion
has ended.

## Privilege model

Snapshot ownership belongs to `yesnod`, which owns the live database lifecycle,
lease state, and checkpoint barrier. ZFS, Btrfs, and EBS currently execute in
the daemon; LVM delegates privileged commands to a second local agent.

What decides whether a backend needs that second process is whether its
privilege can be delegated to an ordinary account. ZFS and Btrfs can delegate
snapshot creation and destruction, so no second process is needed. LVM cannot:
`lvcreate -s` and the clone mount are unconditionally privileged. Run `yesnod`
under an unprivileged account with an empty capability bounding set for every
backend in that group. Running it as root to make a snapshot backend work gives
the network-facing daemon exactly the privilege the agent split exists to
withhold.

Local EBS materialization cannot delegate either, for the same reason as LVM:
it calls `mount(2)`. It therefore uses the same agent. Attachment moves with
the mount rather than staying in the daemon, because a process that can attach
a volume of its choosing to this instance can decide what a later mount exposes,
which would make the split decorative. Deferred materialization needs no agent
at all, because the archiver's worker does the mounting.

## Portable default

The default `backend = "disabled"` disables filesystem-native snapshots, not
the lease API: `yesnod` creates a private portable staging directory, copies a
bounded prefix of every database file under the checkpoint barrier, and serves
that immutable directory through the same Protobuf RPCs. It is slower and uses
temporary local space, but remains consistent and needs no ZFS, Btrfs, or LVM
tooling.

## ZFS and Btrfs

Configure one backend on the server:

```toml
[server.snapshot]
backend = "zfs"
zfs_dataset = "tank/yesno"
lease_ttl_secs = 300
allow_direct_path = false

# Or:
# backend = "btrfs"
# btrfs_snapshot_dir = "/var/lib/yesno-snapshots"
```

For ZFS, `server.data_dir` must be the selected dataset's mount root and `.zfs`
snapshot access must be enabled. The Btrfs snapshot directory must be on the
same Btrfs filesystem and outside the database subvolume. `yesnod` needs
permission to create and destroy read-only snapshots, and both filesystems can
grant exactly that to the unprivileged account it runs as.

For ZFS, delegate the three verbs on the one dataset and leave pool
delegation at its default:

```sh
zfs allow yesno snapshot,destroy,mount tank/yesno
chown -R yesno:yesno /var/lib/yesno
```

A delegated account can create the snapshot, traverse the automounted
`.zfs/snapshot` directory to stage the file list, and destroy the snapshot
afterwards, all with no capabilities at all. Note that a delegated `destroy`
is broader than "destroy snapshots"; scope the grant to the dataset the
database owns and nothing above it.

For Btrfs, the daemon must own its data subvolume and the snapshot directory,
and the filesystem must be mounted with `user_subvol_rm_allowed`:

```sh
mount -o user_subvol_rm_allowed /dev/disk/by-id/... /mnt/yesno
chown yesno:yesno /mnt/yesno /mnt/yesno/data /mnt/yesno/snapshots
```

That mount option is not optional. Ownership alone lets the account create a
snapshot but never delete one, so both lease release and startup
reconciliation would fail and snapshots would accumulate until the filesystem
filled. Deleting a *read-only* subvolume is refused for an unprivileged owner
even with the option set, so the daemon clears the read-only property and
retries when the kernel rejects the first delete; that happens only after the
lease is released, and a root daemon never reaches it.

While `yesnod` creates a snapshot, WAL commits can proceed but checkpoints wait
behind a short backup barrier; a
write that reaches mandatory checkpoint backpressure can therefore wait before
returning. The filesystem operation can also add brief I/O latency. Upload runs
after that barrier is released and reads only the immutable leased snapshot.

## LVM and the snapshot agent

A database on a local linear LVM logical volume can return a normal
file-bearing lease without copying the database:

```toml
[server.snapshot]
backend = "lvm"
lease_ttl_secs = 300
allow_direct_path = false

[server.snapshot.lvm]
source_mount = "/var/lib/yesno"
volume_group = "data-vg"
logical_volume = "yesno"
mount_dir = "/var/lib/yesno-lvm-snapshots"
filesystem = "ext4"                 # `ext4` or `xfs`
snapshot_size_gib = 8
operation_timeout_secs = 300
```

`source_mount` is the mounted root of the configured origin LV;
`server.data_dir` may be that root or a directory below it. `mount_dir` must
be outside the origin mount, including after symlink resolution. The origin
must use classic linear LVM segments; thin pools, RAID, encrypted mappings, and
multi-device layouts are rejected rather than guessed. LVM also requires an
absolute `server.control.unix_socket`; the privileged agent and ordinary local
clients all connect through that same socket.

Run `yesnod` without storage capabilities. Run `yesno-snapshot-agent` as
uid/gid 0 with `CAP_SYS_ADMIN`, and install `lvm2`, `findmnt`, `mount`,
`mountpoint`, `umount`, and `sync` only in that service or image. The capability
is needed for device-mapper and mount operations. Before HTTP/2, the agent
sends an explicit `SCM_CREDENTIALS` tuple containing its real pid and root
uid/gid; the server requires it to match the connection's `SO_PEERCRED`. The
server does not accept a token, authorization rule, or claimed Protobuf
identity in its place.

```text
              same /run/yesno/control.sock
                         |
            +------------+-------------+
            |                          |
     yesno-archive                 snapshot agent
     ordinary local auth          uid=0 gid=0
                                  CAP_SYS_ADMIN
                                  SCM_CREDENTIALS(real-pid,0,0)
                                           |
 yesnod (no storage capabilities)           +--> lvs/lvcreate/mount/umount
     owns lease + backup barrier <----------+    fixed config only
```

For systemd, enable the snapshot-agent unit alongside `yesnod`; its capability
bounding and ambient sets should contain only `CAP_SYS_ADMIN`, and
`PrivateDevices` must remain disabled for that unit. Keep `yesnod` under its
normal unprivileged account and private-device policy. Ensure the kernel's
`dm_snapshot` target is loaded before starting the agent; do not grant the
agent `CAP_SYS_MODULE` to load it itself. The agent reconnects if the daemon
restarts.

Give the two units a shared group for the control socket, and set the socket's
mode explicitly. Connecting to a Unix socket needs write permission on it, and
an agent restricted to `CAP_SYS_ADMIN` holds no `CAP_DAC_OVERRIDE`, so being
uid 0 does not excuse it from that check. Left to the daemon's umask the socket
lands on 0755 under the usual 022, the agent is refused with `EACCES` and
retries once a second forever, and the first base backup fails with a snapshot
agent timeout rather than a permission error:

```toml
[server.control]
unix_socket = "/run/yesno/control.sock"
unix_socket_mode = "0660"
```

with `SupplementaryGroups=` the daemon's group on the agent unit. The mode is
read as octal whether or not it carries a `0o` prefix, and the owner must keep
read and write. Leaving `unix_socket_mode` unset keeps the historical
umask-derived behaviour, which is umask dependent in both directions: 022 gives
0755 and refuses the group, while 002 gives 0775 and admits it. Granting the
agent `CAP_DAC_OVERRIDE` instead of setting the mode also works, and is a wider
grant for no gain.

In a containerized deployment, use a distinct root agent sidecar. Share the
control-socket directory and read-only configuration with `yesnod`; expose the
configured block devices and source mount only to the agent. The snapshot mount
root must use bidirectional mount propagation from the agent, and the daemon or
archive container must receive those submounts with host-to-container
propagation when direct paths are enabled.

Propagation out of the agent is a property of the shared *mount*, not of the
agent's namespace, and the two are not interchangeable. Marking the agent's own
mount namespace shared is not sufficient: that creates a fresh peer group, so
its mounts stay inside it and never reach the host or any sibling. The snapshot
mount root has to be bind-mounted into the agent as `rshared`, which is what
puts the agent's copy in the same peer group as the host's. On the receiving
side, host-to-container propagation is enough — the daemon never needs to
propagate anything outward. A mount that does not arrive does not present as a
missing mount: the daemon reaches the agent's pre-mount directory instead,
which is `0700` and root-owned, so the lease fails with a permission error. Grant `SYS_ADMIN` only to the agent.
A default seccomp or AppArmor profile may deny `mount` or device-mapper ioctls;
use a narrow profile allowing those calls, or an unconfined profile only for
the agent. Do not solve this by making the database container privileged.

The server holds the backup barrier while the agent flushes the origin and
creates the fixed-size COW snapshot. The agent's successful capture completion
is the event that lets `yesnod` release the barrier. Outside that barrier the
agent mounts the clone read-write
to replay the `ext4` or `xfs` journal, then remounts it read-only and returns
the ordinary file manifest. Streaming, `yesnoctl basebackup`, and the
dual-opt-in direct path therefore behave exactly as for ZFS and Btrfs. Release,
expiry, and startup reconciliation unmount the clone before removing the LV;
reconciliation claims only the database UUID namespace on the configured
origin.

Reserve at least `snapshot_size_gib` of free extents for every concurrent
lease. This is COW capacity, not the origin's full size: if origin writes during
a lease exhaust it, LVM invalidates the snapshot and readers fail. Size it for
the maximum write churn over the lease TTL, keep leases short, and alert on
snapshot data usage. LVM is local and always file-bearing; it does not use the
deferred ECS or EKS materialization path.

## Local EBS materialization

An EC2 host with the database on one directly mounted EBS volume can instead
materialize leases from Amazon EBS:

```toml
[server.snapshot]
backend = "ebs"
lease_ttl_secs = 300
allow_direct_path = false

[server.snapshot.ebs]
materialization = "local"
region = "ap-northeast-1"
volume_id = "vol-0123456789abcdef0"
instance_id = "i-0123456789abcdef0"
availability_zone = "ap-northeast-1a"
source_mount = "/var/lib/yesno"
mount_dir = "/var/lib/yesno-ebs-snapshots"
filesystem = "ext4"                 # `ext4` or `xfs`
# partition = 1                      # omit for a whole-volume filesystem
# device_names = ["/dev/sdf", "/dev/sdg"]
# operation_timeout_secs = 3600
# resource_tags = { "backup-policy" = "daily" }
```

`resource_tags` are copied to every temporary EBS snapshot and restored volume.
The keys `yesno:database` and `yesno:lease` are reserved for ownership and
reconciliation. Deployment tags are useful both for cost allocation and for an
IAM policy that restricts cleanup to resources created by one workload.

The EBS backend uses the AWS SDK for Rust with the instance role or configured
AWS credentials. Local materialization needs `ec2:CreateSnapshot`,
`ec2:DescribeSnapshots`, `ec2:DeleteSnapshot`, `ec2:CreateVolume`,
`ec2:DescribeVolumes`, `ec2:AttachVolume`, `ec2:DetachVolume`,
`ec2:DeleteVolume`, and the `ec2:CreateTags` performed by tagged create
operations, split across the two processes described below. The host also needs
`findmnt`, `mount`, `mountpoint`, `umount`, and `sync`; only the clone `mount`,
its read-only remount, and the matching `umount` are privileged, and they belong
to the agent. Deferred materialization reduces the server policy to snapshot
create, describe, delete, and create-time tagging, needs no agent, and needs no
mount privilege anywhere on the database host, because the archiver's worker
mounts instead; volume restoration belongs to the archiver's ECS identity or the
EBS CSI driver.

Local materialization runs `yesno-snapshot-agent` beside `yesnod`, configured
and secured exactly as for LVM, including the shared control socket and its
group. The daemon creates the point-in-time snapshot and the temporary volume
and deletes both afterwards; the agent attaches the volume, mounts the clone
read-only, and on release unmounts and detaches it. The daemon deletes only
after the agent reports the detach complete.

The division is not arbitrary. `AttachVolume` changes which block devices exist
on the instance, so a daemon that could call it would decide what a later mount
exposes and the split would protect nothing. The agent therefore locates the
volume itself, by the `yesno:database` and `yesno:lease` tags that
`CreateVolume` applies at creation time, and chooses the attachment name from
its own configured `device_names`. Nothing the daemon sends selects a device, a
volume, or a path — only which of its own leases to act on. Restrict the
daemon's IAM policy to `ec2:CreateSnapshot`, `ec2:DescribeSnapshots`,
`ec2:DeleteSnapshot`, `ec2:CreateVolume`, `ec2:DescribeVolumes`,
`ec2:DeleteVolume`, and create-time `ec2:CreateTags`, and the agent's to
`ec2:DescribeVolumes`, `ec2:AttachVolume`, and `ec2:DetachVolume`. On an
instance role both processes share one identity and the split is structural
rather than enforced by IAM; separate roles make it enforced as well.

`source_mount` is the mount root of `volume_id`, not merely the database
directory. At capture time the server verifies that the mount's direct block
device is that EBS volume; LVM, dm-crypt, RAID, multi-device filesystems, and
network filesystems are deliberately refused. Both whole-volume and one-partition
`ext4` or `xfs` layouts are supported. For every lease, the server flushes the
source filesystem, requests a database-UUID-tagged point-in-time snapshot, then
releases the checkpoint barrier. It waits for completion outside the barrier,
creates a temporary `gp3` volume in the configured Availability Zone, attaches
it to the configured instance, performs filesystem journal recovery on the
clone, remounts it read-only, and only then publishes the lease. Normal database
crash recovery handles a WAL record cut by the EBS point in time.

Releasing or expiring the lease unmounts and detaches the clone, deletes the
temporary volume, and deletes the EBS snapshot. Startup reconciliation finds
abandoned resources by the exact database UUID tag and removes them before a new
lease is allowed. Each concurrent lease consumes one configured device name,
one instance attachment slot, one snapshot, and one temporary volume; both EBS
storage charges and snapshot initialization latency therefore apply. On Nitro,
the server identifies the local NVMe device by its EBS volume serial rather than
assuming the requested `/dev/sdX` name. `device_names` is a pool; mappings
already present on the instance, including the source volume's mapping, are
skipped.

## Deferred EBS materialization on ECS or EKS

When `yesnod` cannot attach a restored volume itself, select deferred
materialization:

```toml
[server.snapshot]
backend = "ebs"
lease_ttl_secs = 3600

[server.snapshot.ebs]
materialization = "deferred"
region = "ap-northeast-1"
volume_id = "vol-0123456789abcdef0"
source_mount = "/var/lib/yesno"
filesystem = "ext4"
operation_timeout_secs = 3600
```

`yesnod` flushes and creates the tagged EBS snapshot under the checkpoint
barrier, waits for completion after releasing the barrier, and returns a
provisional lease containing the snapshot ID, region, filesystem, database
subpath, and volume size. It does not call ECS or Kubernetes. The archiver
keeps the lease alive, launches a materializer, waits for success, publishes
the staged files while still holding the one archive-writer lease, then
releases the server lease. Release authorizes `yesnod` to delete the snapshot.

The deferred sequence has two separate control authorities: `yesnod` owns the
snapshot lifetime, while the parent archiver owns workload launch and archive
publication. The materializer owns neither:

```text
  yesno-archive          yesnod             ECS or EKS          object store
       |                    |                    |                    |
       | begin snapshot     |                    |                    |
       |------------------->| capture EBS        |                    |
       |                    | snapshot           |                    |
       | provisional lease  |                    |                    |
       |<-------------------|                    |                    |
       | keepalive          |                    |                    |
       |------------------->|                    |                    |
       | launch(snapshot ID, read-only source)   |                    |
       |---------------------------------------->|                    |
       |                    |       restore, mount, stage, sync       |
       |                    |                    |                    |
       | successful exit + shared staging       |                    |
       |<----------------------------------------|                    |
       | worker restore resources enter cleanup |                    |
       | validate and recover staged files      |                    |
       | publish base under archive writer lease|                    |
       |------------------------------------------------------------>|
       | remove shared staging                  |                    |
       | release snapshot    |                   |                    |
       |------------------->| delete EBS snapshot                    |
```

On worker failure or timeout, the archiver stops or deletes the workload,
cleans the transient restore resources, and releases the snapshot lease without
publishing a base. A handled copy failure removes its partial directory. An
abruptly killed task or Pod can leave a hidden `.LEASE.partial-*` directory on
shared staging; it is never accepted as the completed `LEASE` directory, but it
should be removed after confirming that no workload or snapshot lease with that
token remains. If the archiver disappears, lease renewal stops and server-side
expiry eventually authorizes snapshot deletion.

On Kubernetes, this configuration is generated rather than written. The
yesnodb operator resolves each instance's `volume_id` from the volume its
PersistentVolumeClaim bound to, writes it into that instance's configuration
alone, and reports whether every instance has one. Two of the fields above are
consequently not choices there: `materialization` is always `deferred`, because
the managed Pod drops every capability and so cannot mount a restored volume,
and `source_mount` is always the claim's mount point. An instance whose claim
has not bound yet keeps the portable provider until it does, and restarts once
when it changes over.

`yesnoctl basebackup` does not launch deferred materializers and therefore
rejects a provisional EBS lease. Use `yesno-archive` for deferred continuous
base publication. For an ad hoc `yesnoctl basebackup`, configure local EBS
materialization or another file-bearing backend. Note that this is the one
place where the two recommendations pull apart: preferring deferred
materialization to keep the daemon unprivileged also gives up ad hoc
`yesnoctl basebackup` against that database, so choose a file-bearing backend
rather than switching to local EBS if you need both.

The materializer image must contain `yesno-snapshot-stage`. Its source mount is
read-only. Its staging mount must be shared with the parent archiver at the
same absolute path, normally EFS on ECS or an RWX claim backed by EFS on EKS.
The helper copies only database files into a sibling partial directory,
synchronizes it, and atomically renames it. It never writes archive objects or
archive state.

For ECS/Fargate, define a Fargate task with a `configuredAtLaunch` volume named
`snapshot`, mount it at `/snapshot`, and mount shared staging at `/staging`:

```text
yesno-archive \
  --endpoint http://127.0.0.1:50052 \
  --store s3://yesno-archive/prod \
  --snapshot-mode server \
  --deferred-materializer ecs \
  --materializer-source-path /snapshot \
  --materializer-staging-path /staging \
  --ecs-cluster yesno-archive \
  --ecs-task-definition yesno-snapshot-materializer \
  --ecs-container-name materializer \
  --ecs-volume-name snapshot \
  --ecs-infrastructure-role-arn arn:aws:iam::123456789012:role/ecsInfrastructureRole \
  --ecs-subnet subnet-0123456789abcdef0 \
  --ecs-security-group sg-0123456789abcdef0
```

The archiver calls `RunTask` with Fargate launch type, the provisional snapshot
ID, and `deleteOnTermination = true`. Its IAM identity needs `ecs:RunTask`,
`ecs:DescribeTasks`, `ecs:StopTask`, and `iam:PassRole` for the task execution,
task, and EBS infrastructure roles. The infrastructure role needs the ECS
managed-EBS permissions. `yesnod` does not need ECS permissions.

```text
                    AWS SDK RunTask
  +---------------+ -----------------> +--------------------------+
  | yesno-archive |                    | Fargate materializer task|
  |               | <--- exit status - |                          |
  | sole archive  |                    | /snapshot: restored EBS  |
  | publisher     |                    |            read-only     |
  +-------+-------+                    | /staging:  shared EFS    |
          |                            +------------+-------------+
          |                                         |
          | reads staged database                   | atomic stage
          +------------------------+----------------+
                                   v
                              shared EFS
                                   |
          publish only after       |
          validation succeeds      v
                              object store

  yesnod creates/deletes the provisional EBS snapshot; it is not in the
  RunTask path.
```

For EKS, install the EBS CSI driver and CSI snapshot controller and provide a
`VolumeSnapshotClass` and CSI-backed `StorageClass`.

If the database's EBS volume is encrypted, that `StorageClass` must set
`encrypted: "true"` in its parameters. The CSI driver passes the encryption
flag to `CreateVolume` explicitly and defaults it to false, and EC2 rejects the
combination:

```text
InvalidParameterCombination: EncryptedVolume parameter [false] is
inconsistent with snapshot encryption state [true]
```

Nothing between the driver and the archiver reports this. The claim stays
pending, the Job's pod is never scheduled, and the archiver reports only that
the Job did not finish, so the visible failure is three layers from the missing
field. Set `kmsKeyId` as well only to restore under a different key than the
source volume's.

The archiver creates a
pre-provisioned `VolumeSnapshotContent` whose `snapshotHandle` is the server's
EBS snapshot ID and whose deletion policy is `Retain`, a bound
`VolumeSnapshot`, a PVC restored from it, and a Job mounting that PVC read-only:

```text
yesno-archive \
  --endpoint http://yesnod:50052 \
  --store s3://yesno-archive/prod \
  --snapshot-mode server \
  --deferred-materializer eks \
  --materializer-source-path /snapshot \
  --materializer-staging-path /staging \
  --eks-namespace yesno \
  --eks-image 123456789012.dkr.ecr.ap-northeast-1.amazonaws.com/yesno:release \
  --eks-service-account yesno-archive \
  --eks-snapshot-class ebs-snapshots \
  --eks-storage-class gp3 \
  --eks-staging-claim yesno-archive-staging \
  --eks-node-selector eks.amazonaws.com/compute-type=ec2
```

`Retain` is mandatory because the server, not the CSI snapshot controller,
owns deletion of the provisional snapshot. The archiver service account needs
create, get, and delete access to Jobs, PVCs, and VolumeSnapshots in its
namespace, plus cluster-scoped VolumeSnapshotContents. EBS cannot be mounted
to an EKS Fargate pod, so the Job must select an EBS-capable EC2 worker. The
parent archiver may run elsewhere if it can call the Kubernetes API and sees
shared staging. Cleanup deletes the Job, restored PVC, VolumeSnapshot, and
retained VolumeSnapshotContent; `yesnod` deletes the EBS snapshot after lease
release.

```text
  +---------------+   Kubernetes API    +-------------------------------+
  | yesno-archive | -------------------> | retained VolumeSnapshotContent|
  |               |                      |           |                   |
  | sole archive  |                      |      VolumeSnapshot           |
  | publisher     |                      |           |                   |
  +-------+-------+                      |      restored RWO PVC          |
          |                              +-----------+-------------------+
          |                                          |
          | launches and watches                     | EBS CSI mount
          v                                          v
  +----------------------+                 +-------------------------+
  | Kubernetes Job      |                 | EBS-capable EC2 worker  |
  | /snapshot: RWO PVC  | <---------------| (not EKS Fargate)       |
  | /staging: shared RWX|                 +-------------------------+
  +----------+-----------+
             |
             | atomic stage
             v
       shared RWX storage ----------------> parent archiver --> object store

  Resource cleanup removes Job, PVC, VolumeSnapshot, and retained content;
  yesnod deletes the underlying EBS snapshot after lease release.
```

## Direct paths and reconciliation

Normally snapshot files cross the shared endpoint as Protobuf byte chunks. A
co-located sidecar with the same snapshot mount can avoid that copy by setting
`allow_direct_path = true` on `yesnod` and passing `--direct-snapshot-path` to
`yesno-archive`. Both opt-ins are required. Do not enable it when the sidecar
cannot resolve exactly the same paths; direct paths are still validated against
the lease's flat file list and sizes. Portable snapshots never expose direct
paths, even if the client asks.

Provider objects are named in a server-owned namespace containing the database
UUID. After the database opens, `yesnod` removes abandoned objects in that
exact namespace before allowing a new snapshot to depend on reconciliation.
Failed startup reconciliation retries with a capped delay. A failed live lease
deletion remains inaccessible but registered and is retried rather than being
forgotten. Snapshot objects created by versions that predate the UUID namespace
are deliberately not claimed automatically, because an old shared Btrfs
snapshot directory cannot identify which database owned them; remove those
legacy objects once during an upgrade after verifying they are not in use.
