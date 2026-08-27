# The documented container topology, expressed with mount namespaces.
#
# `operations.md` requires that the agent-created snapshot mount reach the
# database through host-to-container propagation. A container is a mount
# namespace, so this runs yesnod under `PrivateMounts=yes` — Docker's
# `bind-propagation=rslave`, Kubernetes' `mountPropagation: HostToContainer` —
# with the privileged agent in the host namespace, and requires the lease to
# still be serviceable.
#
# The load-bearing assertion is `isolated`. A daemon sharing the host namespace
# sees every snapshot mount trivially, so without it every other assertion here
# would pass on a deployment that proves nothing about propagation.
#
# Sabotage-checked by adding `MountFlags=private` to the daemon unit, which
# turns its namespace from slave to private. The scenario then fails at
# `fs_snapshot_begin()` with `Permission denied`, not at a mount count: without
# the mount the daemon reaches the agent's pre-mount directory, which is 0700
# root-owned, and cannot even traverse it. Read that error as "the mount did
# not propagate", not as an ownership problem.

assert fs_prepare("lvm", isolate_mounts=True) == "lvm"

before = fs_mount_isolation()
assert before["isolated"] is True, before
assert before["daemon_ns"] != before["host_ns"], before
# The agent must stay in the host namespace: its mounts have to be visible
# there before they can propagate anywhere else.
assert before["agent_ns"] == before["host_ns"], before
assert before["daemon_mounts"] == 0, before
assert before["host_mounts"] == 0, before

want = {1, 5, 9, 4096}
assert fs_put(11, sorted(want)) == len(want)
assert fs_checkpoint() > 0

# The agent mounts the clone in the host namespace after the daemon's namespace
# already exists, which is the only ordering under which propagation is what
# decides visibility.
lease = fs_snapshot_begin()
assert lease["source"] == "lvm", lease
assert lease["direct"] is True, lease
# Staging the file list means the daemon read the mount from inside its own
# namespace. Without propagation it would have found an empty directory.
assert lease["files"] >= 2, lease

during = fs_mount_isolation()
assert during["host_mounts"] == 1, during
assert during["daemon_mounts"] == 1, during
assert during["daemon_ns"] == before["daemon_ns"], during

fs_snapshot_release()
assert fs_snapshot_wait(0) == 0
after = fs_mount_isolation()
assert after["host_mounts"] == 0, after
assert after["daemon_mounts"] == 0, after

assert fs_cleanup() is True
