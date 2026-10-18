# Exercise the deployed yesnod + yesno-archive workflow with a real ZFS
# snapshot provider in the QEMU/KVM guest. Winterbaume supplies stateful S3
# outside the guest; the sidecar reaches it through QEMU user networking.

assert fs_prepare("zfs") == "zfs"
assert fs_archive_start() == "winterbaume"

# Starting a fresh sidecar must not create a base by itself. A control-plane
# event proves that the Unix-socket subscription and local-channel authz work.
started = fs_archive_wait("event_sequence", 1, 30000)
assert started["base_generation"] == 0, started
assert started["base_ready"] is False, started

want_42 = {1, 5, 9}
want_7 = {3}
assert fs_put(42, sorted(want_42)) == len(want_42)
assert fs_put(7, sorted(want_7)) == len(want_7)

# This is the first base trigger. The sidecar asks yesnod for a server-owned
# snapshot and consumes the explicit direct-path escape hatch in the guest.
checkpoint = fs_checkpoint()
assert checkpoint > 0
base = fs_archive_wait("base_generation", 1, 180000)
assert base["base_generation"] == 1, base
assert base["base_ready"] is True, base
assert base["term"] == 0, base

# These commits are deliberately later than the base. They must reach S3 as
# generational WAL objects before the state cursor is allowed to advance.
want_42.add(13)
assert fs_put(42, [13]) == 1
advanced = fs_archive_wait("cursor_total", base["cursor_total"] + 1, 60000)

# systemd stops with SIGTERM. An immediate restart proves graceful shutdown
# released the conditional S3 writer lease instead of waiting for its TTL.
assert fs_archive_stop() is True
assert fs_archive_start() == "winterbaume"
want_7.add(17)
assert fs_put(7, [17]) == 1
resumed = fs_archive_wait("cursor_total", advanced["cursor_total"] + 1, 60000)
assert resumed["base_generation"] == 1, resumed
assert resumed["base_ready"] is True, resumed
assert fs_archive_stop() is True

# Keep the lower-level lease and network backup checks in the same guest: the
# provider remains interchangeable behind one Protobuf lease contract.
lease = fs_snapshot_begin()
assert lease["source"] == "zfs", lease
assert lease["files"] >= 2, lease
assert lease["ttl"] > 0, lease
assert lease["direct"] is True, lease
assert fs_snapshot_count() == 1
fs_snapshot_release()
assert fs_snapshot_wait(0) == 0

report = fs_basebackup("zfs-backup")
print("backup " + str(report))
assert report["shards"] == 1, report
assert report["checkpoint"] >= checkpoint, report
assert report["recovered"] >= report["checkpoint"], report
assert report["bytes"] > 0, report

# And the holes survived the snapshot.
#
# This provider does not copy the image. It takes a dataset snapshot and leases
# it, so what the backup stream probes with `SEEK_DATA` is the image as ZFS
# presents it through that snapshot -- and whether a copy-on-write snapshot
# still reports its holes is a property of ZFS, not of this tree. The portable
# provider's own hole-skipping says nothing about it. A provider that presented
# a fully allocated image would cost every backup through it the apparent size,
# a gibibyte per shard, while every other assertion in this file passed.
#
# Measured 2026-10-18: 1 073 750 552 long, 282 624 allocated, a factor of
# 3 800. That is about seven times what the btrfs and LVM arms allocate for the
# same content, and the difference is this destination's record granularity
# rather than anything the transfer did -- which is why the bound is a
# sixteenth and not a tighter number.
assert report["allocated"] > 0, report
assert report["allocated"] * 16 < report["bytes"], (
    report,
    "a backup through the ZFS snapshot provider occupies its apparent size",
)

restored = db_open("zfs-backup", shards=1)
snapshot = db_snapshot(restored)
assert snap_load(snapshot, 42) == sorted(want_42)
assert snap_load(snapshot, 7) == sorted(want_7)
snap_release(snapshot)
db_close(restored)

# A SIGKILL cannot run SnapshotManager::shutdown. Startup reconciliation must
# remove the orphan before a new lease can be served.
orphan = fs_snapshot_begin()
assert orphan["source"] == "zfs", orphan
assert fs_snapshot_count() == 1
fs_crash_server()
assert fs_snapshot_count() == 1
fs_restart_server()
assert fs_snapshot_wait(0) == 0
assert fs_checkpoint() >= report["recovered"]

# Restore from Winterbaume into a distinct ZFS child dataset, start yesnod on
# the published directory, and validate the base plus both WAL eras.
archive_restore = fs_archive_restore()
assert archive_restore["base_generation"] == 1, archive_restore
assert archive_restore["checkpoint"] >= checkpoint, archive_restore
assert archive_restore["recovered"] > archive_restore["checkpoint"], archive_restore
assert archive_restore["shards"] == 1, archive_restore
assert archive_restore["bytes"] > 0, archive_restore
assert fs_get(42) == sorted(want_42)
assert fs_get(7) == sorted(want_7)

assert fs_cleanup() is True
