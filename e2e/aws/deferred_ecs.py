# Runs only through scripts/gate-aws.sh, on the same EC2 runner as ebs.py and
# from the same `terraform apply`.
#
# This is the *other* arm of the EBS backend. In ebs.py yesnod restores its own
# snapshot and a privileged agent mounts it here; in this file yesnod restores
# nothing at all. It hands the archiver a provisional snapshot descriptor, and
# the archiver launches an ECS Fargate task that mounts the restored volume,
# copies the bounded database file set onto shared EFS storage, and exits. The
# parent archiver stays the only object-store publisher.
#
# Nothing below is a local decision: the Fargate task, the restored volume, the
# staging filesystem, and the failure cleanup are all real, and the only thing
# this scenario constructs is the *order*.

config = aws_deferred_config()
source_path = config["YESNO_ARCHIVE_MATERIALIZER_SOURCE_PATH"]
staging_path = config["YESNO_ARCHIVE_MATERIALIZER_STAGING_PATH"]
assert config["YESNO_ARCHIVE_DEFERRED_MATERIALIZER"] == "ecs", config
assert source_path.startswith("/"), config
assert staging_path.startswith("/"), config
# `yesno-archive` refuses these two equal, because the worker would then stage
# a directory into the filesystem it is reading. Terraform keeps them apart;
# this is the check that it still does.
assert source_path != staging_path, config

assert aws_prepare_deferred() == "deferred"
assert aws_resource_counts() == {"snapshots": 0, "volumes": 0}
assert aws_materializer_volumes(0, 0, 0) == 0
assert aws_staged_entries() == 0

# The claim this arm makes is an absence: deferred materialization mounts
# nothing on this host, so there is no privileged process on it at all. ebs.py
# asserts the opposite half -- an agent that is root and a daemon that is not
# -- and between them the two say the privilege is where the mount is.
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
# A worker that dies must not leave a chargeable volume, a half-staged
# directory, or a snapshot the server still believes is leased. Pointing the
# archiver at a source path the task will not have is what makes the worker
# fail: the same image, the same task definition, the same launch, and a
# `yesno-snapshot-stage` that exits non-zero because it has nothing to read.
# ---------------------------------------------------------------------------
aws_archive_start("failed-objects", "failed-work", "/nonexistent-materializer-source")
assert aws_checkpoint() > 0

failed = aws_archive_wait_exit(900000)
assert failed["code"] == 1, failed
# The archiver must report the worker's own failure. Anything else here -- a
# timeout, a keepalive that stopped, a lease error -- would mean the launch
# never got as far as running the container.
assert "ECS materializer container exited with" in failed["log"], failed

# The server owns the snapshot's lifetime even when the materializer fails, so
# releasing the lease has to delete it. ECS owns the restored volume and
# deletes it when the task stops. Neither is synchronous with the archiver's
# exit, which is why both are waits.
assert aws_wait_resource_counts(0, 0, 300000) == {"snapshots": 0, "volumes": 0}
assert aws_materializer_volumes(0, 0, 600000) == 0
assert aws_staged_entries() == 0

# ---------------------------------------------------------------------------
# The success path. Same server, same volume, a source path that exists.
# ---------------------------------------------------------------------------
aws_archive_start("objects", "archive-work", source_path)
assert aws_checkpoint() > 0

# The restored volume, seen while it exists. Without this the zero-checks
# around it would pass just as happily against a filter that matched nothing,
# and "no volume was left behind" would be a statement about the filter rather
# than about ECS.
assert aws_materializer_volumes(1, 4, 900000) >= 1

# Only reachable through a Fargate task that restored the snapshot, staged the
# file set, and exited zero: the archiver validates the container's exit code
# before it publishes anything, and every object named by the manifest is
# checked for existence and size as this returns.
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

# Natural worker exit leaves nothing behind either: the volume goes with the
# task, and the archiver removes the staged copy once it has published it.
assert aws_materializer_volumes(0, 0, 600000) == 0
assert aws_staged_entries() == 0
assert aws_wait_resource_counts(0, 0, 300000) == {"snapshots": 0, "volumes": 0}

# The database is still the database. A staging path that had silently copied
# the wrong thing would have published a base already; this only proves the
# server the archive was taken from was never disturbed by any of it.
assert aws_get(42) == sorted(want_42)
assert aws_get(7) == sorted(want_7)

assert aws_cleanup() is True
