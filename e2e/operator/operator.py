# Drive the live Kubernetes operator through the ordinary yesno-e2e runner.
# This file is outside e2e/scenarios so the routine Cargo suite does not
# require Docker or kind; scripts/gate-operator.sh selects it explicitly.

assert op_prepare("kind") is True

# A plugin peer, in its own container beside yesnod. Opted into before op_create
# because the peer is part of the generated manifest rather than a patch; the
# image is built and loaded when the cluster is created, so this costs nothing
# for the scenarios that do not use it.
op_enable_plugin()

# An absent security choice is invalid and must not create storage or a
# workload. The scenario owns the assertions; the host only reports observed
# Kubernetes state.
op_invalid_create()
invalid = op_invalid_status()
assert invalid["phase"] == "Invalid", invalid
assert invalid["deployment_exists"] is False, invalid
assert invalid["pvc_exists"] is False, invalid
op_invalid_delete()

op_create()
status = op_status()
assert status == {
    "phase": "Ready",
    "ready_instances": 2,
    "primary_instance": 0,
    "primary_term": 0,
    "promotion_count": 0,
    "endpoint": "search-rw.yesno-e2e.svc:50051",
    "data_claim": "search-0-data",
    "client_secret": "search-client-tls",
    "certificate_count": 3,
    "issued_secret_count": 3,
    "deployment_count": 2,
    "all_recreate": True,
    "all_replicas_one": True,
    "leader_pods": 1,
    "follower_pods": 1,
    "pvc_count": 2,
    "retention": "retained-after-cluster-deletion",
    "storage_class": "standard",
    "volume_mode": "Filesystem",
}, status

# Switch the running cluster to the EBS snapshot backend.
#
# kind has no EBS, and this asserts the *refusal*, which is the interesting
# half. Everything up to the volume is real: the CRD accepts the spec, the
# controller's ClusterRole permits the cluster-scoped PersistentVolume read,
# and discovery walks each instance's claim to its volume. kind's `standard`
# class provisions a local-path volume, so the only correct answer is to say so
# and keep the portable provider -- a controller that invented a volume id from
# a hostPath volume would fail here, and so would one that took the whole
# reconcile down because a backup setting could not be resolved.
snapshots = op_enable_ebs_snapshots()
assert snapshots["reason"] == "UnusableVolume", snapshots
assert snapshots["condition_status"] == "False", snapshots
assert snapshots["snapshot_configured"] is False, snapshots
# The database is unaffected. Backup configuration that cannot be satisfied
# must never read as an unavailable cluster.
assert snapshots["phase"] == "Ready", snapshots
assert snapshots["ready"] == "True", snapshots

# Use a second key as a positive control: key 42's expected three values cannot
# pass merely because every operation is a no-op.
assert op_put([42, 42, 42, 7], [1, 5, 9, 5]) == 4
assert op_count(42) == 3
assert op_count(7) == 1
assert op_checkpoint() > 0

# The whole path, end to end: a CRD field became a sidecar, which reached a Unix
# socket on a shared volume, received the arena as a file descriptor, opened a
# snapshot and read a key this scenario wrote through Flight. Nothing below the
# operator is mocked, and no part of it is asserted from the operator's own view
# of the world -- the number comes from the peer.
# Waits for the peer's next round, because its snapshot is its own point in time
# and the write above landed after the sidecar started. On timeout the verb returns
# what it last saw, so a wrong number shows up here rather than as a timeout.
report = op_plugin_report(3)
assert "plugin" in report["containers"], report
assert "yesnod" in report["containers"], report
# Key 42 holds three ordinals, written above and checkpointed. The peer counts
# what it finds in the arena, so a wrong lane decode or a wrong prefix shows up
# here as a wrong number rather than as a passing test.
assert report["cardinality"] == 3, report

# Replication is asynchronous. Establish the failover scenario's safety
# precondition explicitly instead of assuming the checkpoint made the follower
# current (checkpoint durability and replication progress are independent).
assert op_wait_count(1, 42, 3) == 3
assert op_wait_count(1, 7, 1) == 1

# Force the current primary Deployment to zero. The controller must first
# confirm that its Pod is absent, promote the caught-up follower exactly once,
# and then rejoin the old primary as a follower.
old_primary = op_fail_primary()
promoted = op_wait_promotion(old_primary)
assert promoted == {
    "phase": "Ready",
    "ready_instances": 2,
    "old_primary": 0,
    "primary_instance": 1,
    "primary_term": 1,
    "promotion_count": 1,
    "leader_pods": 1,
    "follower_pods": 1,
}, promoted

# The promoted copy must contain acknowledged pre-failure data and accept new
# writes through the same stable primary endpoint.
assert op_count(42) == 3
assert op_count(7) == 1
assert op_put([42, 99], [13, 21]) == 2
assert op_count(42) == 4
assert op_count(99) == 1

op_delete()
deleted = op_deleted_status()
assert deleted == {
    "deployment_count": 0,
    "service_count": 0,
    "configmap_count": 0,
    "certificate_count": 0,
    "issued_secret_count": 0,
    "pvc_count": 2,
    "retained_pvc_count": 2,
}, deleted

assert op_cleanup() is True
