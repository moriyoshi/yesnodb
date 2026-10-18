# Continuously archive a base and generational WAL objects, stop the sidecar,
# force the leader's hard retention cap past its durable cursor, then restart
# and require publication of a replacement base generation.

cfg = srv_config("leader", shards=2, interval_secs=3600)
srv_control_admin(cfg, "all")
srv_archive_access(cfg)
srv_control_admin(cfg, "all")
srv_archive_retention(cfg, 65536)
leader = srv_launch(cfg)
flight = srv_flight(leader)

assert flight_put(flight, [1, 1, 1], [10, 20, 50]) == 3

read_before = archive_bytes_read()
archive = srv_archive_start(leader, "objects", "archive-work")
# Startup and listener events prove that the control subscription is running,
# but only a completed checkpoint event may activate the first base.
assert srv_archive_wait_unactivated(archive, 5000) > 0
assert srv_checkpoint(leader) > 0
initial = srv_archive_wait(archive, "base_generation", 1, 30000)
assert initial["base_files"] > 0, initial
assert initial["term"] == 0, initial

# The base is almost entirely holes and the archive no longer ships them. A
# shard image is grown to a whole memory-mapping segment, so its apparent
# length is gigabytes whatever the database holds; the objects are what the
# store is billed for and what a restore transfers. Ratios this large are the
# point of the extent encoding, and only a scenario notices it regressing,
# because shipping the holes is still correct.
assert initial["base_object_bytes"] > 0, initial
assert initial["base_object_bytes"] * 64 < initial["base_apparent_bytes"], initial

# And the capture did not read the holes to find out they were holes, which is
# the half of the saving the assertion above is blind to.
#
# The uploader skips a hole twice: the extent probe decides which 2 MiB slab
# windows are read at all, and an all-zero test decides which of the windows
# that were read become objects. The archive records only what survives the
# second test, so blinding the probe leaves what the archive holds byte for
# byte identical and moves nothing but the wall clock. That was measured here,
# not argued: with the probe returning the whole file, this capture reported
# `base_object_bytes` 6 299 648 and `slab_objects` 4 -- the same two figures as
# an honest run, to the byte -- and took 8.9 seconds instead of 1.8. The
# assertion above passed. That is a stronger version of what the same sabotage
# showed on 2026-10-18 for the base backup and the replication bootstrap, where
# the allocation at least shifted, and it is why reading the whole image cost
# 17.3 seconds for a four-shard capture before this module learned `SEEK_DATA`.
#
# Measured when this was written: 6 299 648 bytes read of an apparent
# 2 147 491 840, a factor of three hundred and forty. A sixteenth is the bound,
# which a blind probe misses by sixteen times and this clears by twenty-one, for
# the reason the other three sites give -- what is under test is that holes are
# skipped at all, not how much this scenario happens to write.
#
# The counter is process-wide and the harness runs its sidecars inside this
# process, so the difference across the capture is the reading.
read_delta = archive_bytes_read() - read_before
print("capture read " + str(read_delta) + " of " + str(initial["base_apparent_bytes"]))
assert read_delta > 0, "the capture read nothing at all"
assert read_delta * 16 < initial["base_apparent_bytes"], (
    read_delta,
    initial,
    "the capture read its apparent size: the extent probe went blind",
)
# An object's bytes were read to produce it, so this cannot be less. It can be
# more, and legitimately: a window that is allocated but all zeros is read and
# stored as nothing, and a slab the active base already holds is read, hashed
# and then not uploaded. Here the two came out equal, which says every window
# this capture read carried something.
assert read_delta >= initial["base_object_bytes"], (read_delta, initial)

# A later ordinary checkpoint is observation, not a new recovery root. Wait
# until both checkpoint phases have crossed the durable event cursor so this
# assertion is about processed behavior rather than timing.
assert srv_checkpoint(leader) > 0
observed = srv_archive_wait(
    archive, "event_sequence", initial["event_sequence"] + 2, 30000
)
assert observed["base_generation"] == initial["base_generation"], (initial, observed)

second_error = srv_archive_second_error(leader, "objects", "archive-work-2")
assert "live writer" in second_error, second_error

before_cursor = initial["cursor_total"]
keys = []
ords = []
for key in range(100, 132):
    keys.append(key)
    ords.append(key * 10)
assert flight_put(flight, keys, ords) == len(keys)

advanced = srv_archive_wait(archive, "cursor_total", before_cursor + 1, 30000)
assert advanced["wal_objects"] > 0, advanced
assert advanced["wal_bytes"] > 0, advanced
assert advanced["term"] == initial["term"], (initial, advanced)

# Keep a later committed transaction in the same archived WAL history. A
# restore through version 2 must cut this record instead of merely selecting a
# chain that happens to end at the target.
assert flight_put(flight, [150], [999]) == 1
later = srv_archive_wait(archive, "cursor_total", advanced["cursor_total"] + 1, 30000)
assert later["cursor_total"] > advanced["cursor_total"], (advanced, later)
srv_archive_stop(archive)

# A clean reconnect must replay and verify the existing immutable history from
# its base root before it can append another transaction. Transport batches may
# group those same WAL frames differently on the second subscription.
archive = srv_archive_start(leader, "objects", "archive-work")
assert flight_put(flight, [151], [1001]) == 1
reconnected = srv_archive_wait(archive, "cursor_total", later["cursor_total"] + 1, 30000)
assert reconnected["base_generation"] == initial["base_generation"], reconnected
srv_archive_stop(archive)

# More than 64 KiB per shard, followed by a checkpoint, makes the old durable
# cursor unavailable. The hard cap deliberately overrides the departed
# subscriber's ACK floor; restart must rebootstrap, never bridge the gap.
keys = []
ords = []
for i in range(12000):
    keys.append(200 + i)
    ords.append(1000000 + i * 3)
assert flight_put(flight, keys, ords) == len(keys)
assert srv_checkpoint(leader) > 0

archive = srv_archive_start(leader, "objects", "archive-work")
rebased = srv_archive_wait(
    archive, "base_generation", initial["base_generation"] + 1, 30000
)
assert rebased["base_files"] > 0, rebased
assert rebased["term"] == initial["term"], rebased
srv_archive_stop(archive)

restored_report = srv_restore("objects", "restore-v2", 2)
assert restored_report["recovered"] == 2, restored_report
assert restored_report["checkpoint"] <= 2, restored_report
# The restored copy is content-identical and differently allocated, which the
# restore module states in prose and nothing else states in numbers. Measured
# when this was written: 2.00 GiB long, 6.02 MiB allocated.
#
# What it pins is narrower than it looks, and worth naming. Almost all of that
# difference is the **trailing** hole -- a shard image carries its data near
# the front and the rest of the mapping segment is empty -- so what holds it
# open is the length being declared by `set_len` and the extents then being
# placed at their offsets. Materializing the image first makes this fail;
# defeating the all-zero window filter does not, because the slab objects
# carry only their own runs and there is nothing zero left to filter. See
# `basebackup.py` for the same oracle on the other copy.
print("restored_report=" + str(restored_report))
assert restored_report["allocated"] > 0, restored_report
assert restored_report["allocated"] * 16 < restored_report["bytes"], restored_report
restored = db_open("restore-v2", shards=1)
snapshot = db_snapshot(restored)
assert snap_load(snapshot, 1) == [10, 20, 50]
for key in range(100, 132):
    assert snap_load(snapshot, key) == [key * 10], key
assert snap_load(snapshot, 150) == []
assert snap_load(snapshot, 151) == []
assert snap_load(snapshot, 200) == []
snap_release(snapshot)
db_close(restored)

flight_stop(flight)
assert srv_stop(leader)["clean"] is True
