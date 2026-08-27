# Runs only through scripts/gate-aws.sh, on the same EC2 runner as ebs.py and
# deferred_ecs.py, and from the same `terraform apply`.
#
# The second deferred materializer. deferred_ecs.py proves that a provisional
# lease can be handed to a workload the archiver launches; this file proves the
# *other* launcher, and the two share only the lease. ECS restores the snapshot
# with a managed volume and an infrastructure role. Kubernetes restores it with
# a retained VolumeSnapshotContent, a VolumeSnapshot bound to it, an EBS-CSI
# PersistentVolumeClaim, and a Job on an EC2 node -- four objects, three
# controllers, and a different failure surface at every step.
#
# Everything the archiver needs inside the cluster was created before this
# scenario started, by the runner, against the Kubernetes API. Its credential
# here is a namespace-scoped ServiceAccount token that may create, read and
# delete exactly the four kinds above.

config = aws_deferred_config()
source_path = config["YESNO_ARCHIVE_MATERIALIZER_SOURCE_PATH"]
staging_path = config["YESNO_ARCHIVE_MATERIALIZER_STAGING_PATH"]
assert config["YESNO_ARCHIVE_DEFERRED_MATERIALIZER"] == "eks", config
assert source_path.startswith("/"), config
assert staging_path.startswith("/"), config
# `yesno-archive` refuses these two equal, because the worker would then stage
# a directory into the filesystem it is reading. Terraform keeps them apart;
# this is the check that it still does.
assert source_path != staging_path, config
# The Job runs the same image this scenario is running in. One image, three
# roles -- harness, ECS worker, Job worker -- which is why it has no ENTRYPOINT.
assert config["YESNO_ARCHIVE_EKS_IMAGE"] != "", config
assert config["KUBECONFIG"].startswith("/"), config

assert aws_prepare_deferred() == "deferred"
assert aws_resource_counts() == {"snapshots": 0, "volumes": 0}
assert aws_materializer_volumes(0, 0, 0) == 0
assert aws_staged_entries() == 0

# The same absence deferred_ecs.py asserts, and for the same reason: whichever
# materializer is configured, deferred materialization mounts nothing on this
# host and needs no privileged process on it.
privileges = aws_privileges()
assert privileges["daemon_uid"] != 0, privileges
assert privileges["daemon_caps"] == 0, privileges
assert privileges["agent_pid"] is None, privileges

want_42 = {2, 6, 10}
want_7 = {4}
assert aws_put(42, sorted(want_42)) == len(want_42)
assert aws_put(7, sorted(want_7)) == len(want_7)

# ---------------------------------------------------------------------------
# The failure path first, while nothing has been published yet.
#
# The Job is created with `backoffLimit: 0`, so a worker that exits non-zero
# fails the Job rather than retrying it. Pointing the archiver at a source path
# a storage class that does not exist is what stops it: the claim is never
# provisioned, the pod is never scheduled, and the archiver gives up on its own
# deadline and cleans up regardless.
#
# Not by pointing the source path at nothing, which is how the ECS arm does
# it. That works there because the task definition pins the container path
# independently; here the archiver *builds* the pod, so the same value becomes
# the volume's `mountPath`, Kubernetes creates it, and the worker finds a
# perfectly good source and exits zero. A live run on 2026-09-04 proved the
# whole materialization path -- volume restored from an encrypted snapshot,
# attached, pod scheduled, `exitCode: 0` -- and failed only because this half
# had injected nothing at all.
#
# This is a weaker statement than the ECS arm's, and deliberately so: it
# tests a materializer that never completes, not one whose container fails.
# Do not "unify" the two halves by giving this one the ECS technique back.
# ---------------------------------------------------------------------------
aws_archive_start(
    "failed-objects", "failed-work", source_path,
    "YESNO_ARCHIVE_EKS_STORAGE_CLASS", "yesno-no-such-storage-class",
)
assert aws_checkpoint() > 0

failed = aws_archive_wait_exit(900000)
assert failed["code"] == 1, failed
# The archiver must report giving up on a Kubernetes object it named, rather
# than dying for some other reason. Its own deadline has to be the one that
# fires: `MATERIALIZER_TIMEOUT_SECS` is set below this wait so that the message
# comes from the archiver and not from the harness's impatience.
assert "timed out waiting for" in failed["log"], failed

# The server owns the snapshot's lifetime even when the materializer fails, so
# releasing the lease has to delete it. The archiver deletes its own Job,
# claim, VolumeSnapshot and VolumeSnapshotContent on both paths, and deleting
# the claim is what deletes the volume the CSI driver provisioned from it.
assert aws_wait_resource_counts(0, 0, 300000) == {"snapshots": 0, "volumes": 0}
assert aws_materializer_volumes(0, 0, 600000) == 0
assert aws_staged_entries() == 0

# ---------------------------------------------------------------------------
# The success path. Same server, same volume, a source path that exists.
# ---------------------------------------------------------------------------
aws_archive_start("objects", "archive-work", source_path)
assert aws_checkpoint() > 0

# The provisioned volume, seen while it exists. Without this the zero-checks
# around it would pass just as happily against a filter that matched nothing --
# and this filter is a tag the EBS CSI driver writes, not one we set, so a
# driver configured without `--extra-create-metadata` would silently make every
# other volume assertion here vacuous.
assert aws_materializer_volumes(1, 4, 900000) >= 1

# Only reachable through a Job that mounted a volume restored from the
# snapshot, staged the file set, and succeeded: the archiver waits on the Job's
# status before it publishes anything, and every object named by the manifest
# is checked for existence and size as this returns.
base = aws_archive_wait_base(1200000)
assert base["base_generation"] >= 1, base
assert base["base_files"] >= 2, base

# Taken here, while the archiver is still running, and as a wait. Publishing
# the base and removing the staged copy are two steps in that order, so the base
# appearing in the object store does not yet mean staging is empty -- and
# stopping the archiver aborts whatever it still had in flight, which would
# strand the copy for good. A live run lost this race on 2026-09-02 having won
# it the run before.
assert aws_wait_staged_entries(0, 300000) == 0

aws_archive_stop()

# Natural completion leaves nothing behind either. The retained
# VolumeSnapshotContent is deleted by the archiver and deliberately does *not*
# take the EBS snapshot with it -- that belongs to the server, which deletes it
# on lease release, and both counts below have to reach zero for that division
# to hold.
assert aws_materializer_volumes(0, 0, 600000) == 0
assert aws_staged_entries() == 0
assert aws_wait_resource_counts(0, 0, 300000) == {"snapshots": 0, "volumes": 0}

# The database is still the database. Nothing in the Kubernetes path touches
# the running server, and a staging path that had copied the wrong thing would
# have published a base already.
assert aws_get(42) == sorted(want_42)
assert aws_get(7) == sorted(want_7)

assert aws_cleanup() is True
