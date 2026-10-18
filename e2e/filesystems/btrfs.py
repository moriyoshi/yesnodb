# Exercise the deployed yesnod + yesno-archive workflow with a real Btrfs
# snapshot provider in the QEMU/KVM guest. The assertions intentionally match
# zfs.py: both providers sit behind the same Protobuf lease contract.

assert fs_prepare("btrfs") == "btrfs"
assert fs_archive_start() == "winterbaume"

started = fs_archive_wait("event_sequence", 1, 30000)
assert started["base_generation"] == 0, started
assert started["base_ready"] is False, started

want_42 = {2, 6, 10}
want_7 = {4}
assert fs_put(42, sorted(want_42)) == len(want_42)
assert fs_put(7, sorted(want_7)) == len(want_7)

checkpoint = fs_checkpoint()
assert checkpoint > 0
base = fs_archive_wait("base_generation", 1, 180000)
assert base["base_generation"] == 1, base
assert base["base_ready"] is True, base
assert base["term"] == 0, base

want_42.add(14)
assert fs_put(42, [14]) == 1
advanced = fs_archive_wait("cursor_total", base["cursor_total"] + 1, 60000)

assert fs_archive_stop() is True
assert fs_archive_start() == "winterbaume"
want_7.add(18)
assert fs_put(7, [18]) == 1
resumed = fs_archive_wait("cursor_total", advanced["cursor_total"] + 1, 60000)
assert resumed["base_generation"] == 1, resumed
assert resumed["base_ready"] is True, resumed
assert fs_archive_stop() is True

lease = fs_snapshot_begin()
assert lease["source"] == "btrfs", lease
assert lease["files"] >= 2, lease
assert lease["ttl"] > 0, lease
assert lease["direct"] is True, lease
assert fs_snapshot_count() == 1
fs_snapshot_release()
assert fs_snapshot_wait(0) == 0

report = fs_basebackup("btrfs-backup")
print("backup " + str(report))
assert report["shards"] == 1, report
assert report["checkpoint"] >= checkpoint, report
assert report["recovered"] >= report["checkpoint"], report
assert report["bytes"] > 0, report

# And the holes survived the snapshot.
#
# This provider does not copy the image. It takes a subvolume snapshot and
# leases it, so what the backup stream probes with `SEEK_DATA` is the image as
# btrfs presents it through that snapshot -- and whether a copy-on-write
# snapshot still reports its holes is a property of btrfs, not of this tree.
# The portable provider's own hole-skipping says nothing about it. A provider
# that presented a fully allocated image would cost every backup through it the
# apparent size, a gibibyte per shard, while every other assertion in this file
# passed.
#
# Measured 2026-10-18: 1 073 750 552 long, 40 960 allocated, a factor of
# 26 000. The LVM arm reports the same figure to the byte and the ZFS arm about
# seven times more, which is a destination granularity difference and not a
# difference in what was transferred.
assert report["allocated"] > 0, report
assert report["allocated"] * 16 < report["bytes"], (
    report,
    "a backup through the btrfs snapshot provider occupies its apparent size",
)

restored = db_open("btrfs-backup", shards=1)
snapshot = db_snapshot(restored)
assert snap_load(snapshot, 42) == sorted(want_42)
assert snap_load(snapshot, 7) == sorted(want_7)
snap_release(snapshot)
db_close(restored)

orphan = fs_snapshot_begin()
assert orphan["source"] == "btrfs", orphan
assert fs_snapshot_count() == 1
fs_crash_server()
assert fs_snapshot_count() == 1
fs_restart_server()
assert fs_snapshot_wait(0) == 0
assert fs_checkpoint() >= report["recovered"]

archive_restore = fs_archive_restore()
assert archive_restore["base_generation"] == 1, archive_restore
assert archive_restore["checkpoint"] >= checkpoint, archive_restore
assert archive_restore["recovered"] > archive_restore["checkpoint"], archive_restore
assert archive_restore["shards"] == 1, archive_restore
assert archive_restore["bytes"] > 0, archive_restore
assert fs_get(42) == sorted(want_42)
assert fs_get(7) == sorted(want_7)

assert fs_cleanup() is True
