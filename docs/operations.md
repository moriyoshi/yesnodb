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
  `SIGBUS`, which a process cannot recover from as an ordinary error.
- Run one process per data directory. The database takes an exclusive file
  lock, so overlapping restarts fail deliberately.
- Replication is asynchronous. It is not a backup and never provides zero RPO.
- There is no consensus-based leader election. Promotion is manual for a
  standalone server and automatic under the Kubernetes operator, which fences
  before it promotes by waiting for every Pod of the old primary to disappear.
  A monotonically increasing leadership term fences a superseded leader that
  survives regardless, and clients enforce it by refusing a term below the
  highest they have seen. The operator is a single external arbiter rather than
  a quorum, so its availability and correctness bound that guarantee.

## Build and first start

Build optimized binaries from the repository root:

```console
cargo build --release -p yesno-server -p yesno-server-utils
```

The resulting programs are `yesnod`, the `yesno` data CLI, and the `yesnoctl`
administrative CLI. A minimal local start is:

```console
./target/release/yesnod --data-dir ./yesnodb-data
```

The default Flight and metrics listeners bind loopback. A non-loopback Flight
bind without TLS or principals is refused unless the operator passes
`--insecure` explicitly.

Validate a configuration without opening the database or taking its lock:

```console
./target/release/yesnod --config /etc/yesno/yesnod.toml --check-config
```

Configuration precedence is built-in defaults, configuration file,
`YESNOD_*` environment variables, then command-line flags.

## Logging and tracing

`yesnod` writes structured tracing output to standard error. The default filter
is `info`; set it with `--log FILTER` or `YESNOD_LOG`. Filters use the usual
target-and-level syntax, for example:

```console
YESNOD_LOG='yesno_core=debug,yesno_flight=debug,yesno_server=info' \
  ./target/release/yesnod --config /etc/yesno/yesnod.toml
```

Storage spans cover database open and recovery, checkpoint, verification,
commit, and live-replica WAL apply. Flight spans cover catalogue and query
preparation, streamed reads, bulk-ingest commits, and administrative actions.
The daemon emits a record when each span closes; `time.busy` and `time.idle`
therefore expose elapsed work around mmap faults, WAL synchronization,
checkpoint serialization, and other blocking boundaries. Routine per-commit
and per-WAL-batch details are at `debug`; lifecycle transitions, long-running
operations, aggregate row or byte counts, and failures are visible at `info` or
higher.

Trace fields intentionally exclude keys, ordinals, query expressions, and
Flight payloads. They include operation type, shard, version or LSN, row or byte
counts, and outcomes. Keep this rule when adding instrumentation: logs are an
operational surface, not a copy of user data.

OpenTelemetry export is disabled by default. Enable OTLP/gRPC trace export and
choose the SDK sampler in the server configuration:

```toml
[server.telemetry]
enabled = true
endpoint = "http://collector:4317"
service_name = "yesnod"
sampler = "parent_based_trace_id_ratio"
sample_ratio = 0.1
```

The sampler accepts `always_on`, `always_off`, `trace_id_ratio`,
`parent_based_always_on`, `parent_based_always_off`, and
`parent_based_trace_id_ratio`. A ratio is inclusive from 0 to 1 and is used
only by the two ratio policies; setting a non-default ratio on another policy
is a configuration error. Parent-based policies retain an upstream sampling
decision and use the configured inner sampler for a new root trace.

If `endpoint` is omitted, the exporter honors
`OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`, then `OTEL_EXPORTER_OTLP_ENDPOINT`, and
finally the OpenTelemetry SDK's local-collector default. The endpoint and
standard OTLP headers may therefore be supplied by the deployment environment
without putting collector credentials in the rendered configuration. The
`--log` filter controls standard-error formatting only; exported traces are
controlled by the SDK sampler. On graceful process exit, the daemon shuts down
the tracer provider and flushes its batch processor before returning.

## Shared control and replication endpoint

Set `server.control.listen`, `server.control.unix_socket`, or both, together
with `server.control.journal_dir` to enable the shared endpoint. Leaving all
three empty disables it; configuring a listener without the journal (or the
journal without a listener) is an error. Both listeners carry the same gRPC
lifecycle, event, WAL-shipping, and shard-image services. TLS applies to TCP;
filesystem ownership and permissions admit peers to the Unix socket, with
`server.control.unix_socket_mode` setting that mode explicitly when the default
umask-derived one is not what the deployment needs.
Keep the journal on a different failure domain from the database directory when
the deployment permits it, so a data-volume failure does not also erase the
event stream that reports it.

The gRPC services and durable journal carry typed Protocol Buffers, not JSON.
Consumers can request the current state projection, subscribe after a durable
sequence number, and request promotion, demotion, or graceful shutdown. A
command is journaled before it is delivered to the lifecycle loop. Invalid
same-role commands are rejected.

The journal appends CRC32C-framed envelopes and synchronizes each fact before
broadcasting it. It repairs only a torn final frame; corruption inside the valid
prefix stops startup. History is bounded at roughly 64 MiB by an atomically
published Protobuf projection checkpoint. A subscriber whose cursor predates
that checkpoint receives `ResyncRequired` with the current projection instead
of an incomplete replay. An unclosed prior process and database are recorded as
interruption facts on the next start.

## Minimal leader configuration

```toml
[server]
role = "leader"
data_dir = "/var/lib/yesno"

[server.flight]
listen = "127.0.0.1:50051"

[server.metrics]
listen = "127.0.0.1:9750"

[server.control]
listen = "127.0.0.1:50052"
journal_dir = "/var/lib/yesno-control"

[db]
shards = 8
max_readers = 4096
on_space_amp = "abort_oldest_reader"

[db.checkpoint]
dirty_bytes = "256MiB"
max_dirty_bytes = "512MiB"
wal_bytes = "1GiB"
max_wal_bytes = "4GiB"
interval_secs = 60

[auth]
anonymous = "none"

[[auth.rule]]
channel = "host"
principal = "all"
address = "127.0.0.0/8"
capability = "control-read"
action = "allow"

[[auth.rule]]
channel = "host"
principal = "all"
address = "127.0.0.0/8"
capability = "control-admin"
action = "allow"
```

The shard count is creation-only. Once a directory has a manifest, its
persisted routing map wins over configuration.

With no configured principals, a loopback-only server is deliberately open to
local clients; `anonymous = "none"` starts to deny unauthenticated calls once
at least one principal enables authentication. A non-loopback open bind is
refused unless `--insecure` explicitly acknowledges it.

## TLS and authentication

A publicly reachable service should configure a server certificate, its private
key, and at least one principal:

```toml
[server.flight]
listen = "0.0.0.0:50051"

[server.flight.tls]
cert = "/etc/yesno/tls/server.pem"
key = "/etc/yesno/tls/server.key"
client_ca = "/etc/yesno/tls/clients-ca.pem"
require_client_auth = false

[auth]
anonymous = "none"

[[auth.principal]]
name = "query"
role = "reader"
token_sha256 = "lowercase-sha256-of-token"

[[auth.principal]]
name = "ingest"
role = "writer"
cert_sha256 = "lowercase-sha256-of-client-certificate-der"
```

Roles are `reader`, `writer`, `admin`, and `replica`; they govern Flight. The
shared control endpoint has an independent, ordered `[[auth.rule]]` table. Its
columns are `channel`, principal, source address, and capability. Channel
values follow `pg_hba.conf`: `local`, `host`, `hostssl`, and `hostnossl`;
`host` matches either TCP variant. A local peer's kernel identity is exposed as
`uid:<number>`, so a local row can name that principal or use `all`. The address
column is ignored for local rows. Capabilities are `control-read`,
`control-admin`, and `replication`; the first match decides and no match denies.

Store bearer-token digests in configuration, never plaintext tokens. Clients
read the plaintext token from a file:

```console
yesno --token-file /run/secrets/yesno-token status
```

### Rotating certificates

`SIGHUP` re-reads the certificate, private key and client CA named by
`[server.flight.tls]` and `[server.control.tls]` and installs them for
connections accepted afterwards. The process is not restarted and the database
is not reopened, so a rotation costs no replay and no downtime. Connections
already established keep the material they negotiated with; a rotation drops no
traffic. Rotate by replacing the contents of the configured files and signalling:

```console
install -m0644 new-server.pem /etc/yesno/tls/server.pem
install -m0600 new-server.key /etc/yesno/tls/server.key
systemctl reload yesnod          # the shipped unit maps this to SIGHUP
```

The *paths* are read once, at start. Changing which file a listener reads is
still a restart; changing what is inside that file is not.

A reload is all or nothing. The server reads every configured file, parses the
certificate chain, parses the key, proves that the key belongs to the
certificate, and builds the client-authentication verifier — all before it
installs anything. If any of that fails, and a truncated copy, a key left over
from the previous certificate, or a client CA file with no usable certificate in
it all do fail, the reload is refused, the listener keeps the material it already
had, and the reason is logged at `error` level naming the file. A rotation with a
mistake in it is a log line, not an outage.

A refused reload is not something the signal can report back, so
`systemctl reload` succeeding means the signal was delivered, not that the
certificate changed. Confirm the rotation rather than assuming it:

```console
journalctl -u yesnod --since -1min | grep 'TLS '
openssl s_client -connect leader.internal:50051 -servername leader.internal \
  </dev/null 2>/dev/null | openssl x509 -noout -dates -fingerprint -sha256
```

Every configured listener reloads on one signal and each decides independently:
a Flight listener that accepts its new material is not held back by a control
listener that refuses its own.

Client-side material rotates without a signal. A follower reads the certificate,
key and CA under `[follower.tls]` each time it dials its leader, so replacing
those files takes effect at its next reconnection; command-line clients read
theirs once per invocation.

`SIGHUP` reloads TLS material and nothing else. Listen addresses, principals,
authorization rules, shard count and checkpoint policy are still read once, at
start, and changing any of them needs a graceful restart.

## Health and metrics

The metrics listener serves:

- `/healthz`: the process is alive;
- `/readyz`: the node is ready to answer its configured workload;
- `/metrics`: Prometheus text.

A follower rebuilding its local image remains healthy but is not ready.

The archive sidecar has its own listener, off unless started with
`--metrics-addr`. It serves `/metrics` and `/healthz` and reports what each
reclamation pass decided; see Retention and reclamation.
Scrape no faster than every 15 seconds because gathering store gauges takes a
lock once per shard.

Use `yesno status` for identity, leadership term, actions, and core space
counters. The Flight `stats` action returns typed Protocol Buffers; `yesno stats`
decodes it to stable key-value fields for scripts.

## Graceful shutdown

Send `SIGTERM` or `SIGINT`. The server stops accepting work, drains readers up
to the configured grace period, takes a final checkpoint, and releases the
database lock.

Do not use overlapping rolling replacement against one data directory. Stop the
old process before starting the new one. In an orchestrator, use a recreate
strategy for a single-node volume.

## Container image

One image serves local Docker, ECS, and Kubernetes. It is published as a
multi-architecture manifest list covering `linux/amd64` and `linux/arm64`, so a
single reference resolves correctly on x86 and Graviton nodes alike and no
deployment needs an architecture-specific tag.

The image carries every shipped binary — the `yesnod` daemon, the `yesno` data
CLI, `yesnoctl`, `yesno-archive`, the snapshot materializer, the privileged
snapshot agent, and the Kubernetes operator — along with the LVM userspace the
agent shells out to. It runs as uid and gid 10001, owns nothing outside
`/var/lib/yesno`, and contains no build toolchain.

Carrying the snapshot agent's binary is not the same as granting it privilege.
The image's user is unprivileged and stays that way; the agent needs uid 0 and
`CAP_SYS_ADMIN`, and those are granted by the deployment to that one container.
Run it as a separate, explicitly privileged container that happens to share the
image — never by raising the daemon's own user, which would hand the daemon the
privileges the split exists to deny it.

### Selecting a binary

The entrypoint dispatches on the first argument. A first argument naming one of
the shipped binaries runs that binary; anything else is the daemon's own
argument list:

```console
docker run --rm yesnodb:local --data-dir /var/lib/yesno   # runs yesnod
docker run --rm yesnodb:local yesnoctl status             # runs yesnoctl
docker run --rm yesnodb:local yesno-snapshot-agent ...    # runs the agent
```

This is what lets one image satisfy three schedulers that disagree about which
half of the argument list they set. Kubernetes supplies arguments and no
command, so the image has to start the daemon by itself. ECS overrides the
command — which replaces the image's default arguments but leaves the entrypoint
in place — and the override for the deferred-materialization worker begins with
a binary name. Plain `docker run` appends to the entrypoint and expects the
daemon.

The two readings never collide, because every daemon option is a `--flag` and
the daemon takes no bare positional argument. A test in the workspace fails if
that ever stops being true.

The daemon must receive `SIGTERM` to drain readers and take its final
checkpoint. The dispatcher replaces itself with the selected binary rather than
supervising it, so the container's process 1 is the binary and the signal
reaches it directly. A measured clean stop is well under a second; a stop that
takes the full kill timeout means something is wrapping the entrypoint.

### Published tags

Releases are published continuously from the project's own pipeline, which runs
the full test gate first and publishes nothing if it fails. Four kinds of tag
are produced, and which one to pin is an operational decision:

| Tag | Moves | Use it when |
|---|---|---|
| `1.2.3` | never | You want a specific release and no surprises. |
| `1.2`, `1` | on each patch or minor release | You accept compatible updates on a redeploy. |
| `latest` | on each release | Evaluation. Not for production. |
| `edge` | on every merge to the main branch | You are tracking development deliberately. |
| `sha-<commit>` | never | You need to name an exact build, typically to roll back. |

`latest` follows releases only; it never points at the tip of development.
`edge` does, and is the one that changes without a release being made.
Prereleases publish under their own version alone and move none of the aliases.

Because a deployment is a full stop and restart — the database takes an
exclusive lock, so the old process must exit before the new one starts — a
moving tag means the version you get is decided at the moment of an unrelated
restart. Pin an immutable tag anywhere that matters.

### Building it

The repository ships an image build script that runs two stages. The first
cross-compiles the release binaries for one architecture; the second assembles
them into the image and compiles nothing. Both stages always run as native code
and cross-compile rather than emulate, so building either architecture needs no
QEMU and no `binfmt_misc` registration on the host — only a working Docker
daemon with Buildx.

Run the script with no arguments for a single image tagged `yesnodb:local`, on
the host's own architecture, loaded into the local daemon. Publishing the
two-architecture manifest list is a separate, explicit flag; the script prints
the reference and revision it is about to publish before it does.

### Local Docker

The default command expects `/etc/yesno/yesnod.toml`; for a minimal evaluation,
override it:

```console
docker volume create yesnodb-data
docker run --rm --name yesnod \
  -p 50051:50051 -p 9750:9750 \
  -v yesnodb-data:/var/lib/yesno \
  yesnodb:local \
  --data-dir /var/lib/yesno \
  --flight-listen 0.0.0.0:50051 \
  --insecure
```

The `--insecure` flag is appropriate only for an isolated evaluation network.
Mount a validated TLS and authentication configuration for any shared network.
Use a local persistent volume, never a network-backed volume. The named volume
also avoids host-directory ownership mismatches with the image's non-root user.

The image declares a healthcheck that asks the running daemon for its status.
It is honoured by Docker and by Compose, and by nothing else — see below.

### Kubernetes

An operator ships with the project and automates everything this section
describes by hand: it manages the leader and follower Deployments, retained
per-instance storage, probes, stable read-write and read-only discovery, and
fenced automatic promotion. Its own guide is the `yesno-operator` README, linked
from the project README. Read the rest of this section if you are deploying
`yesnod` directly, and to understand what the operator arranges on your behalf.

Deploy with the `Recreate` strategy, never `RollingUpdate`. The database takes
an exclusive lock on its directory, so a new pod fails to open it while the old
pod still holds it — and a rolling update is defined by that overlap. This is
the design, one process per database, not a limitation to schedule around.

The volume must be a local filesystem. The store is memory-mapped, and mmap
reports I/O failure as `SIGBUS`, which cannot be caught and turned into an
error — so a network-backed volume converts a transient fault into a crash.

Set the security context to the image's own identity, which admission control
can verify without reading the image:

```yaml
securityContext:
  runAsNonRoot: true
  runAsUser: 10001
  runAsGroup: 10001
  readOnlyRootFilesystem: true
  allowPrivilegeEscalation: false
  capabilities:
    drop: ["ALL"]
```

`readOnlyRootFilesystem` is safe because the daemon writes only under its data
directory and whatever socket directory the configuration names; mount both as
volumes.

Kubernetes does not read the image's healthcheck. Declare probes explicitly.
The readiness endpoint on the metrics listener is the one that distinguishes
"up" from "serving" — a standby rebuilding its copy is up and is not ready — so
readiness must use it rather than a liveness-style port check, or traffic will
be sent to a replica that cannot answer.

### ECS

Fargate resolves the manifest list against the task's `cpu_architecture`, so the
same image reference works for both `X86_64` and `ARM64` task definitions.

ECS does not read the image's healthcheck either. Declare `healthCheck` in the
container definition.

The deferred snapshot materialization path runs this same image as its worker: a
task whose container override supplies the materializer's whole argument list,
beginning with the binary name. Point that task definition at this image and
leave its entry point unset — the dispatcher reads the override and selects the
right binary. Do not set an `entryPoint` in that task definition; it would
displace the dispatcher and the override's first argument would be misread as an
argument rather than a binary name.

The same caution about storage applies: the task's volume must present a local
filesystem, which is what the managed EBS volume attachment provides.

## Backup

Replication does not protect against an accidental delete, bad migration, or
application bug. Take independent backups.

The [on-disk format reference](storage-format.md) describes which files carry
recoverable state and how their A/B slots, checksums, and WAL replay positions
are interpreted.

The backup and archive clients are built from their independently deployable
utility package:

```console
cargo build --release -p yesno-server-utils
```

Use `yesnoctl basebackup` against the authenticated shared control endpoint:

```console
yesnoctl basebackup \
  --endpoint https://leader.internal:50052 \
  --ca /etc/yesno/replication-ca.pem \
  --cert /etc/yesno/backup-client.pem \
  --key /etc/yesno/backup-client.key \
  --server-name leader.internal \
  --target /backups/yesno-2026-08-30
```

The target must not exist. `yesnoctl basebackup` uses only the control-plane
base-snapshot protocol: it acquires an immutable lease, streams every named
file while renewing that lease, and releases it when transfer finishes. The
client never reads the server data path and never asks the replication service
for shard images or WAL. It validates and replays the staged copy with ordinary
replica recovery, synchronizes it, and publishes the target with one directory
rename.

The completion line records the database UUID, leadership term, shard count,
base checkpoint, recovered version, byte size, and attempt count. Store the
completed directory outside the leader and standby failure domain.

The manifest is mandatory. It carries the database identity and routing map.
Restoring shard files without it can create a new identity and route keys to the
wrong shards.

If the shared control endpoint is unavailable, take a cold copy instead: stop
`yesnod` cleanly, copy `MANIFEST` and every `shard-NNNN.yno`, then restart.
The final shutdown checkpoint makes WAL unnecessary for that cold procedure.

A base backup is one recoverable point. For a sidecar on the database host,
grant only its local connection channel the two archive capabilities:

```toml
[server.control]
unix_socket = "/run/yesno/control.sock"
journal_dir = "/var/lib/yesno-control"

[[auth.rule]]
channel = "local"
principal = "all"
capability = "control-read"
action = "allow"

[[auth.rule]]
channel = "local"
principal = "all"
capability = "replication"
action = "allow"
```

Restrict the socket's parent directory and socket mode to the daemon and
sidecar accounts. The ordered rules still limit what an admitted local process
can call. Then run the sidecar through that socket:

```console
AWS_REGION=ap-northeast-1 \
yesno-archive \
  --endpoint unix:///run/yesno/control.sock \
  --store s3://yesno-backups/production \
  --work-dir /var/lib/yesno-archive
```

A fresh archive publishes no base merely because the sidecar started. It waits
for a completed checkpoint event, including one replayed from the durable
control journal, and uses that event as the first base-generation trigger. Run
`yesnoctl checkpoint` after first deployment if an immediate baseline is
required. Once that recovery root exists, later ordinary checkpoint events only
advance the durable event cursor; they do not replace the base. If the requested
event range has already been compacted, a fresh archive conservatively takes a
base, while an established archive keeps its existing recovery root and advances
to the journal's state snapshot.

An archive process on another host instead uses the TCP listener, a dedicated
mutual-TLS identity, and `hostssl` rows for the same two capabilities.

`file:///srv/yesno-archive` selects a local filesystem store. Standard AWS
environment variables configure S3 credentials and region; `AWS_ENDPOINT`
selects an S3-compatible service. The sidecar writes Protobuf `state.pb` and
base manifests, never JSON. Data objects are below a database UUID and
leadership-term prefix, so a promoted timeline cannot overwrite an older
timeline's identically numbered WAL.

Writer fencing and history commitments are archive schema version 2. A version
1 prefix is rejected because it has no trustworthy fingerprint from which to
continue. Start version 2 in a new object prefix and retain the old completed
bases according to the previous recovery policy; do not fabricate a version 2
`state.pb` over version 1 objects.

The default `auto` archive mode asks `yesnod` for a control-plane snapshot
lease. With the default server configuration, `yesnod` makes a portable staged
copy; ZFS or Btrfs replaces that copy with a filesystem-native snapshot behind
the same protocol, and LVM supplies a locally mounted classic snapshot LV.
`--snapshot-mode network` explicitly selects the archive sidecar's replication
fallback.

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
at all, because the archiver's worker does the mounting. Configure one backend
on the server:

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

The default `backend = "disabled"` disables filesystem-native snapshots, not
the lease API: `yesnod` creates a private portable staging directory, copies a
bounded prefix of every database file under the checkpoint barrier, and serves
that immutable directory through the same Protobuf RPCs. It is slower and uses
temporary local space, but remains consistent and needs no ZFS, Btrfs, or LVM
tooling.

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

While it creates one,
WAL commits can proceed but checkpoints wait behind a short backup barrier; a
write that reaches mandatory checkpoint backpressure can therefore wait before
returning. The filesystem operation can also add brief I/O latency. Upload runs
after that barrier is released and reads only the immutable leased snapshot.

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

### Deferred EBS materialization on ECS or EKS

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

WAL bytes come from the replication stream, not from WAL-generation lifecycle
events: a sealed generation may already have been reclaimed when an event is
observed. For each batch the sidecar writes the WAL object, atomically advances
remote and local Protobuf state, and only then ACKs the LSN. That ACK is also
the leader's retention floor. If forced retention moves past an offline
sidecar, it automatically takes a new base before resuming. Ordinary checkpoint
events do not take a new base, because doing so would discard older recovery
coverage. A leadership-term advance clears the old timeline and takes a new
base when the sidecar restarts. Configure the object store to expire abandoned
multipart uploads, because process or host failure can prevent an in-progress
upload from being explicitly aborted.
An abrupt sidecar exit does not orphan a snapshot indefinitely: it renews the
server lease while uploading, and `yesnod` destroys an unrenewed lease after
`lease_ttl_secs`. A server shutdown also releases all leases. If the server
itself crashes, its next successful database open reconciles snapshots left in
that database's namespace.

Run one sidecar writer for an object-store prefix. It owns a renewable
`writer.pb` lease; remote acquisition, renewal, and `state.pb` publication use
conditional object updates, while `file://` uses a kernel file lock. A stale
writer cannot publish after a replacement fences it. The backing object store
must honor conditional create and update operations. The default lease is 30
seconds and can be changed with `--writer-lease-ttl-secs`.

Every WAL object has an immutable Protobuf descriptor and a SHA-256 history
link rooted in its base manifest. On subscription and reconnect, the sidecar
rereads WAL from that base and compares it with the archived chain before
appending. A same-term divergent endpoint is rejected; if retention has removed
the verification root, the sidecar takes a new base. This adds reconnect
bandwidth. It does not replace external leader fencing, which is still required
to prevent split brain rather than merely detect it at the archive boundary.

Restore an archive into a directory that does not exist:

```console
yesnoctl restore \
  --store s3://yesno-backups/production \
  --target /var/lib/yesno-restored \
  --target-version 1842
```

The target is an exact committed logical version, not an LSN. The restore
selects a base at or below it, validates object sizes and CRCs, verifies the
unbroken per-shard history, rejects gaps and forks, truncates to a globally
complete multi-shard transaction prefix, and opens the staging directory with
ordinary replica recovery before publishing it with one rename. Omit
`--target-version` to restore the durable archive tip. A requested version that
is not present as a complete commit fails rather than rounding silently.

### Point-in-time recovery

A target may instead be a wall clock:

```console
yesnoctl restore \
  --store s3://yesno-backups/production \
  --target /var/lib/yesno-restored \
  --target-time 2026-09-06T14:02:00Z
```

`--target-time` and `--target-version` are mutually exclusive. The instant must
be RFC 3339 and must carry an offset; a bare local time is refused rather than
assumed to be UTC, because guessing wrong shifts the recovery point by hours
without saying so.

Every commit records the wall-clock time its version was assigned, taken at the
moment the version is assigned and constrained to be non-decreasing in version
order. A backward system-clock step is absorbed rather than allowed to reorder
two commits, so recorded times can run slightly ahead of true time for the
duration of such a step. Object upload timestamps are still never used as a
proxy: an upload may be retried or delayed long after the commit it carries.

The target resolves to the highest committed version at or before the instant,
and the restore then proceeds exactly as for a version target. By default a
commit stamped exactly at the target is included; `--target-exclusive` drops it,
which is what to use when the instant you have is the start of the transaction
you are recovering away from.

A wall-clock target over history written before commit-time stamping existed
fails and names the first version it cannot place. It does not round, and it
does not treat an unrecorded time as the epoch. Recover that history by version
instead.

Ask which targets an archive can answer before choosing one:

```console
yesnoctl restore --store s3://yesno-backups/production --inspect
```

This reads manifests and object descriptors only. It downloads no data, writes
nothing, creates no directory, needs no `--target`, and is safe to run against a
live archive. Each line reports one recovery window -- its leadership term, base
generation, the version and time range it can restore to, whether that whole
window answers wall-clock targets, and whether it is the archive's active root.

### After the restore, choose a timeline

`--target-action` decides what happens to the recovered directory:

| value | effect |
|---|---|
| `publish` | rename it onto the target. The default. |
| `pause` | verify and recover it, then stop, leaving it unpublished under a temporary sibling name that the completion line reports. For confirming a target produced what you expected before committing to it. |
| `promote` | raise the leadership term before publishing, so the copy is a new timeline. |

Use `promote` whenever the restored database will be written to, or archived, or
replicated from. A restored copy otherwise carries the original's term, and it
shares the original's database UUID -- so nothing downstream can tell the two
histories apart, which is exactly the case a UUID cannot fence. A restore taken
only to read a past state does not need it.

A paused directory is left in place deliberately and is not cleaned up. Remove
it yourself once you are finished with it.

### Retention and reclamation

Bases, WAL objects and their descriptors are immutable and are never rewritten.
Nothing is deleted unless the sidecar is told what to keep:

```console
yesno-archive \
  --endpoint unix:///run/yesno/control.sock \
  --store s3://yesno-backups/production \
  --work-dir /var/lib/yesno-archive \
  --retention-window 7d
```

The window is a promise about recovery points, not about object age: **every
wall-clock instant within it stays restorable.** A pass keeps the newest base at
or below the start of the window -- the root a restore to that instant would
select -- every base after it, and every WAL object those roots still reach.
Accepted units are `s`, `m`, `h` and `d`; a bare number is refused, because every
wrong guess about the unit deletes recoverable history.

Omitting `--retention-window` disables reclamation entirely, which is the
default. `--retention-min-bases` (default 2) keeps that many newest bases
regardless, so a database quiet for longer than its window still has a root.
`--retention-interval-secs` (default 3600) sets the pass interval.

A pass never deletes the active base, a base whose commit time is unknown
(published before commit times were recorded, so it cannot be placed in the
window at all), the marker of an interrupted rebase, or any object whose key it
does not recognize. Unrecognized objects are counted in the pass summary; a
non-zero count means the archive holds objects this version does not understand
and reclamation is incomplete until that is explained.

Every reference is removed before the thing it names, so an interrupted pass
leaves unreferenced objects that the next pass finishes off, never a manifest
pointing at files that are gone.

**Object-store lifecycle rules are not a substitute.** A rule that expires
objects by age does not know that a WAL object from last week is anchored to a
base from last month, and will delete the base while keeping WAL that can no
longer be replayed. Use lifecycle rules only to expire incomplete multipart
uploads.

Reclamation runs under the same writer lease as publication, renewed immediately
before each pass, and is abandoned if the archive state moves while it is
planning. Run it from the same single sidecar that owns the prefix.

#### Reclamation metrics

The pass summary is otherwise only a log line, so a deployment that reclaims
should scrape the sidecar. `--metrics-addr` (or `YESNO_ARCHIVE_METRICS_ADDR`)
serves `/metrics` and `/healthz` on that address:

```console
yesno-archive \
  --endpoint unix:///run/yesno/control.sock \
  --store s3://yesno-backups/production \
  --work-dir /var/lib/yesno-archive \
  --retention-window 7d \
  --metrics-addr 127.0.0.1:9760
```

The listener is off by default, unlike the daemon's, because several sidecars
are routinely run on one host, one per archived database, and a default port
would make the second one fail to start on a bind nobody asked for. Bind it to
loopback: like the daemon's, it is unauthenticated. There is no `/readyz`;
nothing sends traffic to a sidecar, so there is no readiness question to answer.

The counters accumulate over the sidecar's lifetime and reset when it restarts:

- `yesnod_archive_reclamation_passes_total`: passes that completed and produced
  a plan.
- `yesnod_archive_reclamation_failures_total`: passes that ran and failed. A
  failed pass is retried on the next interval and never stops archiving, so
  this counter is the signal that reclamation is not keeping up.
- `yesnod_archive_reclamation_skipped_total`: passes skipped because the writer
  lease was not renewed. Nothing was attempted, which is why it is not counted
  as a failure.
- `yesnod_archive_objects_deleted_total`: archive objects deleted.

The gauges report the last completed pass. They are absent until one completes,
so a sidecar that has never surveyed the archive publishes no reading rather
than a reassuring zero:

- `yesnod_archive_unrecognized_keys`: objects the last pass could not classify,
  and so neither retained deliberately nor deleted. Alert on any non-zero
  value. It means the archive holds objects this version does not understand,
  that nothing will ever reclaim, and that the store grows without bound while
  every other signal looks healthy. The usual causes are a newer writer's
  objects under the same prefix and hand-placed files. Identify them before
  assuming reclamation is complete, and do not delete them by hand until you
  know what wrote them.
- `yesnod_archive_retained_bases`: base images the last pass kept. Read it
  against `--retention-min-bases` and the window.
- `yesnod_archive_reclamation_last_success_timestamp_seconds`: Unix time of the
  last completed pass, and therefore how old the two gauges above are.

A failed pass deliberately leaves the gauges at their last completed reading. A
pass that fails learns nothing, and zeroing the unrecognized count because the
pass failed would replace an absence with a false claim. Alert on staleness
instead: a freshness stamp more than one interval behind, or a rising failure
count, means the gauges no longer describe the archive.

## Restore

The following steps apply after either `yesnoctl basebackup` produced the
directory or `yesnoctl restore` reconstructed it from an archive.

1. Stop `yesnod` and confirm no process has the target directory open.
2. Put one completed backup directory at the configured local data path. Do not
   merge its files with another backup or an existing database.
3. Keep the restored files owned by the service account.
4. Start `yesnod` against that directory.
5. Run `yesno status` and sample known keys.
6. Run `yesnoctl checkpoint` once and take a fresh backup after validation.

Do not merge files from different backups or database identities.

## Replication

A leader exposes lifecycle control, events, and physical replication on one
shared gRPC listener. Bootstrap transfers complete database images, so grant
the `replication` capability only to explicit principals and source networks:

```toml
[server.control]
listen = "10.0.0.10:50052"
journal_dir = "/var/lib/yesno-control"

[server.control.tls]
cert = "/etc/yesno/tls/server.pem"
key = "/etc/yesno/tls/server.key"
client_ca = "/etc/yesno/tls/replicas-ca.pem"
require_client_auth = true

[[auth.principal]]
name = "standby-b"
role = "replica"
cert_sha256 = "lowercase-sha256-of-standby-certificate-der"

[[auth.rule]]
principal = "standby-b"
address = "10.0.0.0/24"
capability = "replication"
action = "allow"
```

A standby configuration names the leader's shared control endpoint:

```toml
[server]
role = "follower"
data_dir = "/var/lib/yesno"

[follower]
leader = "https://leader.internal:50052"
poll_interval_secs = 1
max_backoff_secs = 30
serve_reads = true

[follower.tls]
ca = "/etc/yesno/tls/replicas-ca.pem"
cert = "/etc/yesno/tls/standby-b.pem"
key = "/etc/yesno/tls/standby-b.key"
domain = "leader.internal"

[server.flight]
listen = "127.0.0.1:50051"

[server.metrics]
listen = "127.0.0.1:9750"
```

Read-serving followers are always potentially stale. Do not use them for
read-after-write.

### Recovery objectives

A deployment has one leader and any number of standbys, with asynchronous
replication and no consensus or automatic election. A commit is durable once
the leader has synchronized its WAL, before a standby necessarily has it. RPO
is therefore greater than zero by construction.

Measure that exposure under the intended workload. Compare the leader's
`yesnod_visible_version` with each standby's
`yesnod_follower_visible_version`, and time how long an acknowledged probe
commit takes to appear there. `yesnod_follower_connected`,
`yesnod_follower_halted`, and `yesnod_follower_catchup_failures_total` expose
replication health, but the configured poll interval alone is not an RPO
measurement. Publish the observed window. RTO includes detecting and
externally fencing the failed leader, selecting a standby, promoting it, and
repointing clients. Promotion itself is only one part of that interval.

## Disaster recovery: promotion and fencing

Before promotion, fence the old leader by a mechanism outside yesnodb: stop its
service, revoke its virtual address, or otherwise prove it cannot accept
writes. yesnodb detects superseded terms at followers and clients; it does not
prevent two reachable leaders.

Poll every standby and record its `visible_version` before changing any roles:

```console
for host in standby-b standby-c standby-d; do
  printf '%s ' "$host"
  yesno --endpoint "https://$host:50051" status | grep visible
done
```

Choose the standby with the highest complete `visible_version`, not the most
bytes applied or the highest single-shard LSN. The visible version is the
cross-shard watermark, so it names the newest transactionally complete state.
Promote that standby with:

```console
systemctl kill -s SIGUSR1 yesnod
```

Promotion performs a final catch-up attempt, raises and persists the leadership
term, recovers a consistent cross-shard prefix, and opens the local database as
leader. Recovery truncates each shard at its first unresolved record rather
than retaining half of a multi-shard commit. Repoint clients and pin the new
term on requests:

```console
yesno --endpoint https://new-leader:50051 status
yesno --endpoint https://new-leader:50051 --min-term NEW_TERM stats
```

Repoint the remaining standbys to the new leader and distinguish these outcomes:

- A standby that resumes following needs no repair.
- A halt reason saying the endpoint offers an older leadership term means the
  standby is pointed at a superseded leader. Check the address before doing
  anything destructive.
- `out_of_range` with the follower ahead of the leader means the standby holds
  commits missing from the promoted node. This is a reportable data-loss event.
  Preserve that standby as evidence and record the lost version range before
  rebuilding it.
- `failed_precondition` indicating that retained WAL no longer reaches the
  follower means it is behind and must bootstrap again. Do not confuse this
  ordinary retention gap with an ahead-of-leader data-loss report.
- A warning that a base image is unusable, followed by that shard being
  bootstrapped again, needs no action. A standby whose copy of a shard cannot
  be read replaces it rather than stopping, so an interrupted bootstrap or a
  damaged image costs one re-copy of that shard and no intervention. Repeated
  occurrences for the same shard are worth investigating as storage faults.

Never rejoin the old leader incrementally. Stop it, preserve any evidence needed
for incident review, empty its old data directory, and bootstrap it as a new
standby. Its database identity matches the new leader but its timeline does not.

After repointing, run `yesno status` and `yesno stats` against the new leader,
compare a sample of known keys with the latest backup, and run `yesnoctl checkpoint`
against its control endpoint. The reported watermark must be at
least the recorded `visible_version` of the promoted standby.

## Drill and acceptance criteria

Before relying on replication, rehearse:

- leader bootstrap and standby catch-up;
- stale reads on a read-serving follower;
- old-leader fencing;
- manual promotion;
- client `--min-term` refusal against the old leader;
- backup restore into an empty directory;
- a standby rebuild after falling behind retained WAL.

Record measured replication lag, backup duration, restore duration, and
promotion time under the intended workload. Those measurements, not the
configured poll interval, define the deployment's actual RPO and RTO.
