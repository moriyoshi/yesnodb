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

archive = srv_archive_start(leader, "objects", "archive-work")
# Startup and listener events prove that the control subscription is running,
# but only a completed checkpoint event may activate the first base.
assert srv_archive_wait_unactivated(archive, 5000) > 0
assert srv_checkpoint(leader) > 0
initial = srv_archive_wait(archive, "base_generation", 1, 30000)
assert initial["base_files"] > 0, initial
assert initial["term"] == 0, initial

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
