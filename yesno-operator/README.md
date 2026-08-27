# yesno-operator

`yesno-operator` is the Kubernetes control plane for yesnodb. A `YesnoCluster`
reconciles one leader and zero or more read-serving followers, one retained
filesystem PVC per instance, stable read-write and read-only Services, health
probes, configuration, automatic promotion, and observable failover status.

This is intentionally smaller than CloudNativePG. It does not schedule backups
or archives, restore a cluster from an archive, perform rolling upgrades, or
provide synchronous replication. It configures the snapshot provider a backup
would lease, but takes no snapshot of its own. Replication is asynchronous, so acknowledged
writes can be lost when the leader fails even when promotion succeeds.

## Topology and automatic promotion

`spec.instances` is between 1 and 9. Instance zero starts as leader; the other
instances bootstrap from it, follow its WAL, and serve potentially stale reads.
`<name>-rw` selects the current leader and `<name>-ro` selects followers. Each
instance has its own `ReadWriteOnce` PVC and `Recreate` Deployment, so no two
yesnod processes mount the same database directory.

When a ready primary becomes unavailable for `spec.failoverDelaySecs`, the
controller chooses the reachable follower with the greatest visible version.
Failover is a persisted state machine visible in `.status.failover`:

1. Scale the former primary Deployment to zero.
2. Wait until no Pod for that instance remains.
3. Send the durable promotion command once and wait for the new leader to open.
4. Move the read-write Service and restart the former primary as a follower.

The stages make reconciliation restartable, and yesnod treats a queued duplicate
promotion as success after the first command has already made it leader. The
controller never force-deletes a Pod. If Kubernetes cannot confirm the old Pod
is gone, promotion remains blocked: availability is sacrificed rather than
knowingly creating two writers.

The fence has a clear boundary. An administrator must not force-delete the old
Pod or remove its Node while the process might still run; Kubernetes object
absence is not a power fence. Deployments needing automatic failover across
untrusted node failure modes must add infrastructure-level node or storage
fencing. Leadership terms let clients detect a superseded timeline, but cannot
make an isolated old process discover its replacement.

## Build and install

No images are published yet. From the repository root, build both images and
make them available to the cluster:

```console
docker build -f yesno-server/dist/Dockerfile -t yesnod:local .
docker build -f yesno-operator/dist/Dockerfile -t yesno-operator:local .
```

For a local cluster, load those images using that cluster's normal image-loading
command. For a remote cluster, push them and replace the two `:local` image
references in the manifests.

Install the CRD and controller, then create the evaluation cluster:

```console
kubectl apply -f yesno-operator/deploy/crd.yaml
kubectl apply -f yesno-operator/deploy/operator.yaml
kubectl apply -f yesno-operator/deploy/example.yaml
kubectl get yesnoclusters,pods,pvc
```

The example expects cert-manager and a same-namespace `Issuer` named
`yesno-ca`. It also expects a `yesno-root-ca` Secret whose `tls.crt` key is the
CA bundle for that issuer. The operator does not create or rotate a root CA.

## cert-manager configuration

`spec.config.certManager` is the generated secure topology mode. Exactly one of
`certManager`, `secretName`, and `allowInsecure` must be set. `issuerRef.kind`
defaults to `Issuer`, `issuerRef.group` defaults to `cert-manager.io`, and
`caSecretRef.key` defaults to `ca.crt`:

```yaml
spec:
  instances: 3
  config:
    certManager:
      issuerRef:
        name: yesno-ca
        kind: Issuer
      caSecretRef:
        name: yesno-root-ca
        key: tls.crt
```

The operator creates one cert-manager `Certificate` per instance and one
administrative client `Certificate`. Instance certificates contain the
per-instance Service plus the stable `-rw` and `-ro` Service DNS names and have
both server-auth and client-auth usages. Generated yesnod configuration requires
mTLS on Flight and on the shared control/replication listener. Instance
identities have only the replication role; the client identity is the
administrative Flight and control principal. Its Secret name is reported in
`status.clientSecret`.

The referenced CA Secret is intentionally independent from the generated leaf
Secrets. Do not point it at a leaf Certificate merely because that Secret
happens to contain `ca.crt`: not every issuer supplies that key, and it is not a
stable trust-distribution contract. Keeping trust ownership separate also lets
a bundle contain old and new roots during a CA transition.

cert-manager leaf Secrets and their `Certificate` objects are owned by the
`YesnoCluster` and are removed with it. The user-owned CA Secret and retained
data PVCs are not. A leaf or CA-bundle change changes the Pod-template identity
and triggers a `Recreate` restart because yesnod reads certificates at startup.
The refresh is detected by periodic reconciliation; certificate hot reload is
not implemented.

## User-supplied configuration

For a single-instance shared cluster, create a Secret whose `yesnod.toml` key contains a full
leader configuration and whose other keys contain any referenced certificate
files. The configuration must use `/var/lib/yesno` as `server.data_dir`, bind
Flight on `0.0.0.0:50051`, and bind health and metrics on `0.0.0.0:9750` so the
managed probes can reach it. Its `server.shutdown_grace_secs` must not exceed
`spec.shutdownGraceSecs`; Kubernetes allows that interval plus ten seconds
before killing the Pod. Reference the Secret instead of enabling insecure mode:

```yaml
spec:
  instances: 1
  config:
    secretName: search-config
```

Secret configuration remains restricted to `instances: 1` because one static
file cannot describe different leader and follower identities. Use cert-manager
mode for a generated secure multi-instance topology. `allowInsecure: true`
remains available only for isolated evaluation namespaces.

Secret content participates in the Pod-template identity, so changing config or
certificates performs a `Recreate` restart. Two yesnod processes never overlap
on one data directory.

## Storage and deletion

The operator never creates ephemeral database storage. Set exactly one of
`spec.storage.size` or `spec.storage.existingClaim`; an existing claim is valid
only with `instances: 1`. An operator-created PVC is
not owner-referenced and remains after its `YesnoCluster` is deleted. Managed
PVCs also require an explicit, non-empty `spec.storage.storageClassName`.

The selected StorageClass must provide a local filesystem. yesnodb memory-maps
its store; a network-backed filesystem can turn a transient I/O error into
SIGBUS. `ReadWriteOnce` alone does not prove that a volume is local, so the
operator cannot infer this property from the PVC API.

PVC capacity and StorageClass are creation-time choices in this first API. The
operator does not mutate an existing claim. Delete retained data only as an
explicit, separate Kubernetes operation after verifying that it is no longer
needed.

## EBS snapshot backend

`spec.snapshot` chooses the provider that services a base-snapshot lease. It
defaults to `disabled`, which is the portable server-staged copy rather than the
absence of snapshots. On EKS with the EBS CSI driver, `ebs` snapshots each
instance's own data volume instead:

```yaml
spec:
  serviceAccountName: yesno-snapshotter
  storage:
    size: 100Gi
    storageClassName: gp3
  snapshot:
    backend: ebs
    leaseTtlSecs: 300
    ebs:
      region: ap-northeast-1
      filesystem: ext4
      operationTimeoutSecs: 3600
      resourceTags:
        cost-center: search
```

There is no `volumeId` field. Each instance has its own PVC and therefore its
own EBS volume, and the controller reads that volume's id from the bound
PersistentVolume and writes it into that instance's configuration alone. This
is the reason the backend belongs to the operator at all: yesnod checks that
the volume named in its configuration is the one its data directory actually
sits on, and a volume id the daemon derived from that same directory would
agree with itself by construction. Kubernetes is the independent witness.

Materialization is likewise not selectable. Restoring a snapshot *locally*
means attaching a volume to the node and mounting it, which the managed Pod is
built to make impossible: it drops every capability, refuses privilege
escalation, and runs as an unprivileged user. Only deferred materialization is
generated, where yesnod returns a provisional lease naming a completed EBS
snapshot and the archiver stages its contents elsewhere.

### The window before the claim binds

An EBS StorageClass normally binds `WaitForFirstConsumer`, so the volume does
not exist until the Pod has been scheduled -- and that Pod needs a
configuration first. An instance therefore starts on the portable provider and
picks up the EBS backend on the reconcile after its claim binds. That changes
the Pod-template identity and performs one `Recreate` restart. The state is
visible throughout:

```console
kubectl get yesnocluster search -o jsonpath='{.status.conditions}'
```

`SnapshotBackendReady` is `True` only when every instance names its own EBS
volume. It reports `AwaitingVolumeBinding` during the window above, and
`UnusableVolume` when a claim has bound to something that is not an EBS volume
or when the controller's ClusterRole is missing the `persistentvolumes` read.
Neither state affects `Ready`: an instance in it is serving queries normally.

### Credentials

yesnod calls EC2 itself to create and delete snapshots, so it needs an AWS
identity. Name a ServiceAccount in `spec.serviceAccountName` and annotate it for
IRSA or EKS Pod Identity. The Pods still set `automountServiceAccountToken:
false` -- yesnod never talks to the Kubernetes API -- which does not interfere,
because the projected identity token is a volume the admission webhook adds by
itself.

The role needs four EC2 actions and no more: `ec2:CreateSnapshot`,
`ec2:CreateTags`, `ec2:DescribeSnapshots` and `ec2:DeleteSnapshot`. `Describe`
cannot be scoped to a resource, so it takes `Resource: "*"`; the other three
can be scoped to the snapshot ARN.

It needs neither `ec2:CreateVolume`, `ec2:AttachVolume`, `ec2:DetachVolume` nor
`ec2:DeleteVolume`, which belong to local materialization. It does not need
`ec2:DescribeVolumes` either: yesnod identifies its own volume from the NVMe
serial under `/sys/class/block` rather than by asking EC2, and in deferred mode
it refuses to snapshot at all when it cannot -- so a volume it can describe but
cannot see is not a case that gets a pass.

`spec.snapshot.backend: ebs` requires generated configuration. It is rejected
alongside `spec.config.secretName`, because a Secret the operator only reads has
nowhere for the controller to put the volume id it discovered.

### Live coverage

Two gates exercise this, and they cover different halves.

`scripts/gate-operator.sh` runs the full controller lifecycle in a disposable
kind cluster and patches the EBS backend on deliberately. kind provisions
local-path volumes, so the only correct outcome there is `UnusableVolume` and a
cluster that stays `Ready` — which is worth asserting, and is not the same as
the backend working.

`scripts/gate-aws.sh` runs the other half against a real EKS cluster and real
EBS volumes: `SnapshotBackendReady` reaches `Configured`, each instance names
its own `vol-`, and those ids are compared against the volume handles of the
PersistentVolumes the claims bound to. `YESNO_AWS_ONLY=operator` runs that arm
alone. It is opt-in and billable.

Neither gate yet takes a snapshot through a Pod-hosted daemon, so nothing
proves an IRSA identity reaches one. The AWS arm checks the daemon has not
logged a failure to reach EC2, which is one-sided: startup reconciliation
retries in the background and the Pod stays `Ready` either way.

## Live kind E2E

The opt-in live test builds both images from the checkout and exercises the full
controller lifecycle in a disposable kind cluster:

```console
./scripts/gate-operator.sh
```

It requires only a working Docker daemon and network access to the pinned
cert-manager v1.21.1 release manifest. The script builds an all-in-one E2E
driver image containing kind 0.33, kubectl 1.36.1, the Docker client, the
operator, and the ordinary `yesno-e2e` runner. It mounts the Docker socket; it
does not run Docker-in-Docker and does not require a privileged driver. The
ordinary `yesno-e2e` runner executes `e2e/operator/operator.py`; there is no
operator-specific binary. That scenario sequences the common `op_*` verbs and
asserts invalid-spec rejection, two-instance readiness and resource shape,
certificate issuance and mTLS-only client access, ingest plus checkpoint,
fencing and automatic promotion, pre-failure data survival, writes through the
promoted leader, follower rejoin, generated certificate/private-key cleanup,
and both retained PVCs. The host verbs attach the driver to kind's Docker
network, use an isolated internal kubeconfig and sha256-pinned kind node image,
and export diagnostics on failure.
The cluster, images, and scratch directory are removed by default; set
`YESNO_E2E_KEEP_KIND=1` to retain them for inspection. Set
`YESNO_E2E_CERT_MANAGER_MANIFEST` to a local manifest path on an offline test
host.

## Generate the CRD

The checked-in CRD is generated from the Rust API:

```console
cargo run -p yesno-operator -- crd
```
