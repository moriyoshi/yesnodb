# Take a hot backup through the daemon's shared control listener, reject an
# overwrite, and open the published directory as an independent database.
#
# Keys span four physical shards. Every key has checkpointed data and a later
# WAL-only point, so restoring only either half cannot satisfy the oracle.

SHARDS = 4
KEYS = 64

cfg = srv_config("leader", shards=SHARDS, interval_secs=3600)
srv_control_admin(cfg, "all")
srv_replication(cfg)
srv_control_admin(cfg, "all")
leader = srv_launch(cfg)
flight = srv_flight(leader)

wants = []
keys = []
ords = []
for key in range(KEYS):
    want = set()
    for offset in range(32):
        ordinal = key * 1000 + offset
        keys.append(key)
        ords.append(ordinal)
        want.add(ordinal)
    wants.append(want)
assert flight_put(flight, keys, ords) == KEYS * 32

checkpoint = srv_checkpoint(leader)
assert checkpoint > 0

keys = []
ords = []
for key in range(KEYS):
    ordinal = key * 1000 + 777
    keys.append(key)
    ords.append(ordinal)
    wants[key].add(ordinal)
assert flight_put(flight, keys, ords) == KEYS

report = srv_basebackup(leader, "backup")
assert report["shards"] == SHARDS, report
assert report["checkpoint"] == checkpoint, report
assert report["recovered"] > checkpoint, report
assert report["bytes"] > 0, report

# The published directory must be mostly holes, and this is the only thing in
# the suite that says so.
#
# A shard image is grown to a whole 1 GiB mapping segment at creation, so these
# four shards are 4 GiB long whatever they hold -- and both halves of the
# transfer were taught to skip the empty part: the snapshot provider copies
# only the allocated runs, and the control-plane stream sends only the chunks
# that carry something, which the client places at the offsets they carry. Both
# halves have unit tests, and **neither composes**. `allow_sparse` is a
# capability the *caller* declares, so a receiver that stopped setting it, or a
# provider that went back to a whole-file copy, keeps every unit test green and
# costs the transfer its apparent size again. Measured when this was written:
# 4.00 GiB long, 156 KiB allocated, and about seventeen times quicker than the
# same backup taken densely.
#
# A sixteenth is the bound, which a dense regime misses by sixteen times and
# the measured figure clears by seventeen hundred. It is deliberately nowhere near
# either: what is under test is that holes are skipped at all, not how much a
# scenario happens to write, and a tight bound would turn adding a key here
# into a failure about sparseness. `allocated > 0` keeps it from passing by
# measuring nothing.
#
# What this cannot see is worth stating. The copy skips a hole twice over --
# the extent probe never reads it, and an all-zero chunk is never written --
# and only the second of those shows up here. Defeating the probe alone leaves
# the output just as sparse and costs five times the wall clock, measured, with
# nothing in the suite asserting on wall clock. Two mechanisms producing one
# observable is the same trap as an optimization whose effect waits on an
# upstream property: the reading cannot tell them apart.
print("report=" + str(report))
assert report["allocated"] > 0, report
assert report["allocated"] * 16 < report["bytes"], (
    report,
    "the backup occupies its apparent size: a hole-skipping path went dense",
)

# The report's own two numbers, against a walk of the **published** directory
# that the report did not perform. The report measures the staging directory
# as it fsyncs it; this reads the target after the rename, recursively, and
# must agree. A figure taken before the flush, or a walk that missed a file,
# shows up here and nowhere else.
walked = disk_usage("backup")
assert walked["bytes"] == report["bytes"], (walked, report)
assert walked["allocated"] == report["allocated"], (walked, report)

# The same published directory is now an existing target. Refusal happens
# before transfer, and the successful backup remains the oracle below.
refused = False
try:
    srv_basebackup(leader, "backup")
except RuntimeError as error:
    refused = True
    assert "already exists" in str(error), str(error)
assert refused, "base backup overwrote an existing target"

flight_stop(flight)
assert srv_stop(leader)["clean"] is True

# `shards=1` is deliberately wrong: the copied MANIFEST owns the topology and
# must reopen the directory with all four physical shards.
restored = db_open("backup", shards=1)
snapshot = db_snapshot(restored)
for key in range(KEYS):
    assert snap_load(snapshot, key) == sorted(wants[key]), (
        "restored key %d diverged" % key
    )
snap_release(snapshot)
db_close(restored)
