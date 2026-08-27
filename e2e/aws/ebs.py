# Runs only through scripts/gate-aws.sh. Terraform supplies a disposable EC2
# runner and source EBS volume. yesnod creates and destroys the snapshot and the
# temporary volume through the production AWS SDK path; the privileged snapshot
# agent beside it attaches, mounts, unmounts, and detaches the clone. This is
# the only gate that runs an EBS mount at all, so it also asserts which of the
# two processes holds the privilege to perform it.

assert aws_prepare() == "ebs"
assert aws_resource_counts() == {"snapshots": 0, "volumes": 0}

# The daemon must hold no capability whatsoever, and the agent must be the one
# that does. A daemon that had quietly kept CAP_SYS_ADMIN would still pass every
# behavioural assertion below, which is exactly how this went unnoticed before.
privileges = aws_privileges()
assert privileges["daemon_uid"] != 0, privileges
assert privileges["daemon_caps"] == 0, privileges
assert privileges["agent_uid"] == 0, privileges
assert privileges["agent_caps"] != 0, privileges
agent_pid = privileges["agent_pid"]

want_42 = {2, 6, 10}
want_7 = {4}
assert aws_put(42, sorted(want_42)) == len(want_42)
assert aws_put(7, sorted(want_7)) == len(want_7)
checkpoint = aws_checkpoint()
assert checkpoint > 0

lease = aws_snapshot_begin()
assert lease["source"] == "ebs", lease
assert lease["files"] >= 2, lease
assert lease["ttl"] > 0, lease
assert lease["direct"] is True, lease
# The lease is served out of a real mount below the configured snapshot
# directory, on an attachment name the agent chose from its own pool. The
# daemon never names a device, so a name from outside the pool would mean the
# agent had started taking the caller's word for it.
assert lease["mounted"] is True, lease
assert lease["pooled"] is True, lease
assert aws_resource_counts() == {"snapshots": 1, "volumes": 1}

# Releasing has to unmount and detach before the daemon may delete, and the
# daemon cannot do either half itself.
aws_snapshot_release()
assert aws_wait_resource_counts(0, 0, 300000) == {"snapshots": 0, "volumes": 0}

report = aws_basebackup("aws-ebs-backup")
assert report["shards"] == 1, report
assert report["checkpoint"] >= checkpoint, report
assert report["recovered"] >= report["checkpoint"], report
assert report["bytes"] > 0, report

restored = db_open("aws-ebs-backup", shards=1)
snapshot = db_snapshot(restored)
assert snap_load(snapshot, 42) == sorted(want_42)
assert snap_load(snapshot, 7) == sorted(want_7)
snap_release(snapshot)
db_close(restored)

# A process death cannot run lease cleanup. Startup reconciliation must remove
# exactly the resources tagged with this Terraform run ID, and that now spans
# both processes: the agent unmounts and detaches, then the daemon deletes.
orphan = aws_snapshot_begin()
assert orphan["source"] == "ebs", orphan
assert orphan["mounted"] is True, orphan
assert aws_resource_counts() == {"snapshots": 1, "volumes": 1}
aws_crash_server()
assert aws_resource_counts() == {"snapshots": 1, "volumes": 1}
aws_restart_server()
assert aws_wait_resource_counts(0, 0, 300000) == {"snapshots": 0, "volumes": 0}
assert aws_get(42) == sorted(want_42)
assert aws_get(7) == sorted(want_7)

# The agent outlives the daemon and reconnects rather than being restarted with
# it. Reconciliation above therefore ran through a re-established connection,
# which is the path a real deployment takes after every daemon restart.
after = aws_privileges()
assert after["agent_pid"] == agent_pid, (after, agent_pid)
assert after["daemon_pid"] != privileges["daemon_pid"], (after, privileges)
assert after["daemon_caps"] == 0, after

assert aws_cleanup() is True
