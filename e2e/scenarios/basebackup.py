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
