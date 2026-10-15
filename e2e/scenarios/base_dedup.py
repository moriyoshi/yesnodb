# A second base generation is built out of the first one's objects, costs what
# changed plus a bounded constant, and restores from the pieces it did not
# write.
#
# Phase 0 made a base file a set of content-addressed 2 MiB pieces, so an
# unchanged piece is named by the next generation rather than uploaded again.
# Nothing else in the suite exercises that across a generation boundary: the
# unit tests cover the uploader on synthetic files, and `pitr_retention.py`
# restores after a sweep without ever asking where the bytes came from.
#
# **The body is payload, and that is the whole reason this file is shaped the
# way it is.** Measured on 2026-10-15, what a generation re-sends is the region
# a checkpoint rewrites, which tracks the *index*. A database of single-ordinal
# keys is almost all index, and there a generation re-sends 45-67% of the whole
# database no matter how little changed -- a tenfold churn increase moved the
# stored bytes by zero. A database whose bytes are payload re-sends the new
# data plus **one** 2 MiB piece, flat, whichever of the two axes is moved. The
# consumer's shape is the second one, so that is what is built here.
#
# Ranges are the wrong tool for it: a contiguous range is a run container and
# nearly free. Stride 16 puts 4096 ordinals in every 65536-wide chunk, which is
# `ARRAY_MAX` -- 8192 bytes of payload against about 20 bytes of index.
#
#   yesno-e2e --show-output --arg chunks=500 base_dedup.py

KEYS = yn_arg("keys", 10)
CHUNKS = yn_arg("chunks", 250)
CHURN = yn_arg("churn", 12000)

PER_KEY = CHUNKS * 4096
SPAN = CHUNKS * 65536
SLAB = 2097152

# Built through the library and handed to the daemon afterwards, by opening the
# same directory. The Flight write wire carries one row per set bit, so a
# payload-dominated body cannot be put there in reasonable time.
d = db_open("payload", shards=1)
total = 0
for i in range(KEYS):
    sb = sb_new()
    sb_stride(sb, i * SPAN, 16, PER_KEY)
    total = total + db_insert_set(d, 10 + i, sb_build(sb))
assert total == KEYS * PER_KEY, total
assert db_checkpoint(d) > 0
db_close(d)

cfg = srv_config("payload", shards=1, interval_secs=3600)
srv_control_admin(cfg, "all")
srv_archive_access(cfg)
srv_control_admin(cfg, "all")
srv_archive_retention(cfg, 65536)
leader = srv_launch(cfg)
flight = srv_flight(leader)

archive = srv_archive_start(leader, "objects", "archive-work")
assert srv_checkpoint(leader) > 0
first = srv_archive_wait(archive, "base_generation", 1, 300000)
# Nothing but this base has been stored, so the two readings must agree. They
# diverge if a capture left an object no manifest names -- the orphan a
# reclamation pass would otherwise have to find later.
assert first["slab_objects"] > 1, first
assert first["slab_object_bytes"] == first["base_object_bytes"], first

# Force a second generation the way production gets one: the sidecar is down,
# so writing past the leader's hard WAL cap makes the reconnect re-root.
srv_archive_stop(archive)
assert flight_put(flight, list(range(900000, 900000 + CHURN)),
                  list(range(700000000, 700000000 + CHURN))) == CHURN
version = srv_checkpoint(leader)
assert version > 0

archive = srv_archive_start(leader, "objects", "archive-work")
second = srv_archive_wait(archive, "base_generation", 2, 300000)
srv_archive_stop(archive)

# What the second generation had to store, against what it added. The
# difference is the rewrite a checkpoint cannot avoid, and the assertion is
# that it stays a bounded constant rather than growing with the database.
# Measured at 2 097 200 bytes -- one piece and change -- at 25 MB and at 46 MB
# of body, and at both 12 000 and 120 000 churned keys.
grew = second["slab_object_bytes"] - first["slab_object_bytes"]
added = second["base_object_bytes"] - first["base_object_bytes"]
print("gen1 objects=" + str(first["slab_objects"]) + " bytes=" + str(first["slab_object_bytes"]))
print("gen2 names=" + str(second["base_object_bytes"]) + " added=" + str(added) + " grew=" + str(grew))
assert grew > 0, (first, second)
assert grew - added <= 2 * SLAB, (first, second)

flight_stop(flight)
assert srv_stop(leader)["clean"] is True

# The restore is the load-bearing assertion. A generation whose manifest names
# objects an *earlier* capture wrote is a new kind of dependency, and the way
# it fails is a base that verifies, lists and plans fine and then cannot be
# read back. A key the churn never touched comes back only from a piece the
# first capture uploaded.
report = srv_restore("objects", "restore-dedup", version)
assert report["recovered"] >= version, report
restored = db_open("restore-dedup", shards=1)
snapshot = db_snapshot(restored)
assert snap_cardinality(snapshot, 10) == PER_KEY, "an untouched payload key did not survive"
assert snap_cardinality(snapshot, 10 + KEYS - 1) == PER_KEY, "the last payload key did not survive"
assert snap_cardinality(snapshot, 900000) == 1, "the churned key did not survive"
snap_release(snapshot)
db_close(restored)
