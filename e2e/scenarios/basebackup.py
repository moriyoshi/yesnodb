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

read_before = snapshot_bytes_read()
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
# What this assertion alone cannot see is worth stating, because it is half the
# saving -- and it is not the same half at the two ends of the transfer. There
# are two extent probes on this path and only one of them is invisible here.
#
# Blinding the **stream's** probe does show up: the control-plane stream has no
# all-zero filter and sends every chunk it reads, so the receiver writes the
# zeros and the published directory comes back 4.00 GiB allocated, which the
# assertion below catches. Blinding the **provider's** does not: the copy into
# the lease directory still drops every all-zero chunk, so what it stages is as
# sparse as ever while having read the whole image to produce it. Measured on
# 2026-10-18 with only the provider's probe defeated: 12.0 MiB allocated
# against 4.00 GiB long -- twenty-one times inside the bound below -- and 15.3
# seconds instead of about one. Wall clock is not something this suite asserts
# on. The bytes read are, just below.
print("report=" + str(report))
assert report["allocated"] > 0, report
assert report["allocated"] * 16 < report["bytes"], (
    report,
    "the backup occupies its apparent size: a hole-skipping path went dense",
)

# What the leader asked its disk for. Both reading paths are counted: the
# portable provider copies each image into the lease directory, and the
# control-plane stream then reads that copy. Skipping holes, the total is about
# twice what the backup publishes -- 282 KiB against 156 KiB, measured. With
# the provider's probe defeated it is 4.01 GiB, because the image is read in
# full and the staged copy that comes out of it is still sparse for the stream
# to read.
#
# The counter is this process's and the harness runs its servers inside it, so
# the difference across the call is the reading and the absolute value is
# whatever ran before.
read_delta = snapshot_bytes_read() - read_before
print("read_delta=" + str(read_delta))
assert read_delta > 0, "the backup read nothing at all"
assert read_delta * 16 < report["bytes"], (
    read_delta,
    report,
    "the backup read its apparent size: the extent probe went blind",
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
