# A second base generation is built partly out of the first one's objects, and
# restores from them.
#
# Phase 0 made a base file a set of content-addressed 2 MiB pieces, so an
# unchanged piece is named by the next generation rather than uploaded again.
# Nothing else in the suite exercises that across a generation boundary: the
# unit tests cover the uploader on synthetic files, and `pitr_retention.py`
# restores after a sweep without ever asking where the bytes came from.
#
# The load-bearing assertion is the restore. A generation whose manifest names
# objects an *earlier* capture wrote is a new kind of dependency, and the way
# it fails is a base that verifies, lists and plans fine and then cannot be
# read back.
#
# **The sharing ratio is printed and deliberately not asserted**, because it
# is a property of the workload rather than of the uploader. Measured here on
# 2026-10-15: at 300 000 single-ordinal keys a second generation re-sends 3 of
# 5 pieces, and raising the churn tenfold -- 2 000 keys to 20 000 -- changes
# that by nothing at all. What a generation re-sends tracks the size of the
# database, not the size of the change, for a database whose bytes are index.
# Pinning a number from this shape would pin the wrong one.
#
#   yesno-e2e --show-output --arg n=900000 base_dedup.py

N = yn_arg("n", 300000)
CHURN = yn_arg("churn", 2000)

cfg = srv_config("leader", shards=1, interval_secs=3600)
srv_control_admin(cfg, "all")
srv_archive_access(cfg)
srv_control_admin(cfg, "all")
srv_archive_retention(cfg, 65536)
leader = srv_launch(cfg)
flight = srv_flight(leader)

body = list(range(1000, 1000 + N))
assert flight_put(flight, body, list(range(5000000, 5000000 + N))) == N

archive = srv_archive_start(leader, "objects", "archive-work")
assert srv_checkpoint(leader) > 0
first = srv_archive_wait(archive, "base_generation", 1, 180000)
# Nothing but this base has been stored, so the two readings must agree. They
# diverge if a capture left an object no manifest names -- which is the orphan
# a reclamation pass would later have to find.
assert first["slab_objects"] > 1, first
assert first["slab_object_bytes"] == first["base_object_bytes"], first

# Force a second generation the way production gets one: the sidecar is down,
# so writing past the leader's hard WAL cap makes the reconnect re-root.
srv_archive_stop(archive)
churned = list(range(1000, 1000 + CHURN))
assert flight_put(flight, churned, list(range(9000000, 9000000 + CHURN))) == CHURN
version = srv_checkpoint(leader)
assert version > 0

archive = srv_archive_start(leader, "objects", "archive-work")
second = srv_archive_wait(archive, "base_generation", 2, 180000)
srv_archive_stop(archive)

# What the second generation had to store, against what it names.
grew = second["slab_object_bytes"] - first["slab_object_bytes"]
print("gen1 objects=" + str(first["slab_objects"]) + " bytes=" + str(first["slab_object_bytes"]))
print("gen2 names=" + str(second["base_object_bytes"]) + " grew=" + str(grew))
assert grew < second["base_object_bytes"], (first, second)

flight_stop(flight)
assert srv_stop(leader)["clean"] is True

# The restore is the assertion. A key the churn never touched comes back only
# if the piece holding it was fetched from the object the *first* capture
# wrote, which is the whole of what content addressing changed.
report = srv_restore("objects", "restore-dedup", version)
assert report["recovered"] >= version, report
restored = db_open("restore-dedup", shards=1)
snapshot = db_snapshot(restored)
# `flight_put` inserts, so the churned key carries both ordinals rather than
# the later one alone.
assert snap_load(snapshot, 1000) == [5000000, 9000000], "the churned key did not survive"
untouched = 1000 + N - 1
assert snap_load(snapshot, untouched) == [5000000 + N - 1], "an untouched key did not survive"
snap_release(snapshot)
db_close(restored)
