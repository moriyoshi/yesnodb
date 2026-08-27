# Runs only through scripts/gate-aws.sh, on the same EC2 runner as ebs.py and
# the two deferred scenarios, and against the same EKS cluster.
#
# The operator's EBS snapshot backend, against real Amazon EBS.
#
# `e2e/operator/operator.py` drives the same verbs in a disposable kind cluster
# and covers everything about the operator that storage does not decide:
# cert-manager topology, promotion, the fence, retained claims. It cannot cover
# this. kind provisions local-path volumes, so `SnapshotBackendReady` there can
# only ever reach `UnusableVolume` -- the discovery walks claim to volume and
# then correctly refuses, which is worth asserting and is not the same thing as
# the backend working.
#
# Here the same code path ends in a real `vol-`, in a generated
# configuration, in a daemon that started with it. Do not fold the two files
# together: the assertions differ because the environments differ, and a shared
# file would have to weaken both.

assert op_prepare("eks") is True

# The same invalid-spec refusal the kind arm makes, restated here for one
# reason: it is the cheapest possible proof that this cluster's API server has
# the CRD and that the controller installed above is reconciling. If it is
# broken, everything below fails minutes later and further from the cause.
op_invalid_create()
invalid = op_invalid_status()
assert invalid["phase"] == "Invalid", invalid
assert invalid["deployment_exists"] is False, invalid
assert invalid["pvc_exists"] is False, invalid
op_invalid_delete()

# Two instances, gp3 claims, and `spec.snapshot.backend: ebs` from the start --
# not patched on afterwards as the kind arm does. This is the ordinary way a
# user asks for it, and it is the ordering that exercises the window: an EBS
# StorageClass binds WaitForFirstConsumer, so at the moment these Pods are
# first scheduled their volumes do not exist and their configuration cannot
# name one.
op_create()

snapshots = op_snapshot_status()

# ---------------------------------------------------------------------------
# The assertion the kind arm cannot make.
# ---------------------------------------------------------------------------
assert snapshots["condition_status"] == "True", snapshots
assert snapshots["reason"] == "Configured", snapshots
assert snapshots["snapshot_configured"] is True, snapshots

# Both instances, and each with its own volume. `sorted` on both sides and
# compared whole: a controller that resolved one volume and wrote it into every
# instance's configuration would satisfy any per-item check and would be
# snapshotting one database twice.
configured = snapshots["configured_volumes"]
bound = snapshots["bound_volumes"]
assert len(configured) == 2, snapshots
assert len(bound) == 2, snapshots
assert configured[0] != configured[1], snapshots

# The cross-check, and the reason this arm exists. The left side is parsed
# out of the ConfigMap the daemon actually mounts; the right side is read from
# the PersistentVolumes the claims bound to. Kubernetes is the independent
# witness -- if the controller had invented, defaulted or transposed a volume
# id, these would differ.
assert configured == bound, snapshots
for volume in configured:
    assert volume.startswith("vol-"), snapshots

# A daemon is running that configuration, not merely a ConfigMap holding it.
# Learning the volume rewrites the ConfigMap and the Pod template in the same
# reconcile that patches the status, so `SnapshotBackendReady=True` is
# observable before the Recreate rollout carrying it has even started. Every
# instance's running Pod must carry the config identity its Deployment
# specifies -- which is the restart, completed.
assert snapshots["specified_instances"] == 2, snapshots
assert snapshots["settled_instances"] == 2, snapshots

# And the database came back from that restart serving.
assert snapshots["phase"] == "Ready", snapshots
assert snapshots["ready"] == "True", snapshots

# **One-sided, and stated so rather than dressed up.** Startup
# reconciliation asks EC2 for the snapshots this database left behind, and when
# it cannot -- no credentials, wrong role, a trust policy naming the wrong
# ServiceAccount -- it retries in the background and logs this. The Pod stays
# Ready either way, so nothing else in this file would notice. A non-zero count
# proves EC2 was not reached; zero does not prove it was.
#
# Do not upgrade this to a claim that IRSA works. What would prove it is a
# snapshot lease taken from inside the Pod, which needs an archiver in the
# cluster and is tracked as its own item.
assert snapshots["reconcile_errors"] == 0, snapshots

# Ingest and read back through the same mTLS client Pod the kind arm uses. A
# positive control for the whole stack: a cluster that reached Ready with an
# unusable data volume would fail here rather than pass quietly.
assert op_put([42, 42, 42, 7], [1, 5, 9, 5]) == 4
assert op_count(42) == 3
assert op_count(7) == 1
assert op_checkpoint() > 0

# The follower has the same data, which is what makes its own EBS volume -- the
# second entry in the cross-check above -- a database rather than an empty
# filesystem with a volume id.
assert op_wait_count(1, 42, 3) == 3
assert op_wait_count(1, 7, 1) == 1

# Deliberately no failover here. The kind arm fences and promotes, and doing
# it again on EKS would cost billable minutes to re-prove something storage has
# no part in.

op_delete()
deleted = op_deleted_status()
assert deleted["deployment_count"] == 0, deleted
assert deleted["service_count"] == 0, deleted
assert deleted["configmap_count"] == 0, deleted
# Retained, exactly as on kind, and here it means two EBS volumes survive
# their YesnoCluster. That is the documented contract -- data is never deleted
# with the custom resource -- and it is why the arm's runner script deletes the
# namespace afterwards and then waits for the volumes to actually go.
assert deleted["pvc_count"] == 2, deleted
assert deleted["retained_pvc_count"] == 2, deleted

assert op_cleanup() is True
