# Container image and deployment

This guide covers the published container image and what Docker, Kubernetes,
and ECS each need from a deployment. Configure the daemon itself with the
[configuration guide](configuration.md).

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

## Selecting a binary

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

## Published tags

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

## Building it

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

## Local Docker

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

The image declares a healthcheck against the daemon's `/healthz` liveness
endpoint. It is honoured by Docker and by Compose, and by nothing else — see
below.

## Kubernetes

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

## ECS

Fargate resolves the manifest list against the task's `cpu_architecture`, so the
same image reference works for both `X86_64` and `ARM64` task definitions.

ECS does not read the image's healthcheck either. Declare `healthCheck` in the
container definition.

Use the daemon's HTTP liveness endpoint for the ECS container check. This stays
successful while a follower is seeding from an archive; ECS will not restart a
healthy process just because it is not ready to receive traffic yet:

```json
{
  "healthCheck": {
    "command": [
      "CMD",
      "/usr/local/bin/yesno",
      "--endpoint",
      "http://127.0.0.1:9750",
      "healthz"
    ],
    "interval": 10,
    "timeout": 5,
    "retries": 3,
    "startPeriod": 60
  }
}
```

The `yesno healthz` and `yesno readyz` commands call `/healthz` and `/readyz`
on the plain-HTTP metrics listener. Use `/readyz` for a load balancer's target
health check when the metrics listener is reachable from that load balancer;
keep the ECS container health check on `/healthz` as a process-liveness check.

The [deferred snapshot materialization](snapshots.md) path runs this same image
as its worker: a task whose container override supplies the materializer's whole
argument list, beginning with the binary name. Point that task definition at this image and
leave its entry point unset — the dispatcher reads the override and selects the
right binary. Do not set an `entryPoint` in that task definition; it would
displace the dispatcher and the override's first argument would be misread as an
argument rather than a binary name.

The same caution about storage applies: the task's volume must present a local
filesystem, which is what the managed EBS volume attachment provides.
