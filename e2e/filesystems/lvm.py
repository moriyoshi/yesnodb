# Exercise the deployed yesnod + yesno-archive workflow with a real local LVM
# snapshot provider in the QEMU/KVM guest. The provider returns the same
# file-bearing Protobuf lease as ZFS and Btrfs.

assert fs_prepare("lvm") == "lvm"
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
assert lease["source"] == "lvm", lease
assert lease["files"] >= 2, lease
assert lease["ttl"] > 0, lease
assert lease["direct"] is True, lease
assert fs_snapshot_count() == 1
fs_snapshot_release()
assert fs_snapshot_wait(0) == 0

report = fs_basebackup("lvm-backup")
print("backup " + str(report))
assert report["shards"] == 1, report
assert report["checkpoint"] >= checkpoint, report
assert report["recovered"] >= report["checkpoint"], report
assert report["bytes"] > 0, report

# And the holes survived the snapshot, for a different reason than on the two
# copy-on-write filesystems.
#
# This provider snapshots a logical volume, which is a *block* device: the
# filesystem mounted off the snapshot is the same ext4 image with the same
# extent map, so a hole is a hole because nothing was ever allocated to it, not
# because a snapshot chose to share it. That makes it the arm most likely to
# behave and still the one worth asserting, because nothing else in this file
# would notice a backup through it going dense -- the restored database would
# be correct and cost a gibibyte per shard.
#
# Measured 2026-10-18: 1 073 750 552 long, 40 960 allocated, a factor of
# 26 000 -- the same figure the btrfs arm reports to the byte.
assert report["allocated"] > 0, report
assert report["allocated"] * 16 < report["bytes"], (
    report,
    "a backup through the LVM snapshot provider occupies its apparent size",
)

restored = db_open("lvm-backup", shards=1)
snapshot = db_snapshot(restored)
assert snap_load(snapshot, 42) == sorted(want_42)
assert snap_load(snapshot, 7) == sorted(want_7)
snap_release(snapshot)
db_close(restored)

orphan = fs_snapshot_begin()
assert orphan["source"] == "lvm", orphan
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
