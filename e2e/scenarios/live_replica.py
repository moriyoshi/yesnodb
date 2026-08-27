# A standby that answers queries **while it is still following** — the read
# fan-out a replicated deployment exists for.
#
# # What makes this different from `failover.py`
#
# There, the standby is cold: it holds no database, serves nothing, and its data
# only becomes readable once it is promoted. Here it holds the database open and
# applies shipped frames into it, so the same node is a follower and a reader at
# the same time.
#
# The distinguishing assertion is therefore **not** that the data arrives — a
# cold standby manages that too — but that it is readable *from the standby, over
# Flight, without the standby ever closing*. A check that opened the directory
# afterwards would pass on either.
#
# # The shape of the writes
#
#   key 1 — before the checkpoint, so it arrives through the base image.
#   key 2 — after it, so it can only arrive through catch-up.
#   key 3 — while the replica is already open and serving, which is the case
#       that has no cold-standby equivalent at all.
#
#   yesno-e2e --show-output --arg n=20000 live_replica.py

N = yn_arg("n", 2000)

# ---- a leader that ships its log
lcfg = srv_config("leader", shards=2, interval_secs=60)
srv_control_admin(lcfg, "all")
srv_replication(lcfg)
srv_control_admin(lcfg, "all")
leader = srv_launch(lcfg)
lf = srv_flight(leader)

want1 = set()
keys = []
ords = []
for i in range(N):
    o = i * 3
    keys.append(1)
    ords.append(o)
    want1.add(o)
flight_put(lf, keys, ords)
assert srv_checkpoint(leader) > 0

want2 = set()
keys = []
ords = []
for i in range(N // 4):
    o = i * 7 + 1
    keys.append(2)
    ords.append(o)
    want2.add(o)
flight_put(lf, keys, ords)

# ---- a standby that serves reads
scfg = srv_config("replica", shards=2)
srv_follows(scfg, leader)
srv_serve_reads(scfg)
replica = srv_launch(scfg)

srv_follower_wait(replica, "records", 1, 30000)
srv_follower_wait(replica, "passes", 1, 30000)

st = srv_follower_state(replica)
assert st["connected"] is True, st
assert st["halted"] is False, st

# ---- read from the replica, over its own socket, while it follows
rf = srv_flight(replica)
assert flight_info(rf, 1)["total_records"] == len(want1), (
    "the replica did not serve the half that arrived in the base image"
)
assert flight_info(rf, 2)["total_records"] == len(want2), (
    "the replica did not serve the half that arrived through catch-up"
)
assert flight_get(rf, 2) == sorted(want2), (
    "the ordinals the replica served are not the ones the leader stored"
)

# ---- a write made now reaches it without any restart
want3 = set()
keys = []
ords = []
for i in range(300):
    o = i * 11
    keys.append(3)
    ords.append(o)
    want3.add(o)
flight_put(lf, keys, ords)

for _ in range(200):
    if flight_info(rf, 3)["total_records"] == len(want3):
        break
    srv_follower_wait(replica, "passes", srv_follower_state(replica)["passes"] + 1, 10000)
assert flight_info(rf, 3)["total_records"] == len(want3), (
    "a write made while the replica was serving never reached it"
)
assert flight_get(rf, 3) == sorted(want3)

# ---- it is a *replica*, and says so
#
# `FailedPrecondition`, not an internal error. A client told "internal error"
# retries and pages somebody; what it needs to do is send the write to the
# leader, and the code has to say which.
refused = False
try:
    flight_put(rf, [9], [1])
except RuntimeError as e:
    refused = True
    assert "FailedPrecondition" in str(e), str(e)
    assert "read-only replica" in str(e), str(e)
assert refused, "a replica accepted a write"

# The leader is unaffected by the attempt.
assert flight_info(lf, 9)["total_records"] == 0

# ---- and the replica has not diverged
assert flight_get(rf, 1) == flight_get(lf, 1), "key 1 diverged from the leader"

flight_stop(rf)
flight_stop(lf)
assert srv_stop(replica)["clean"] is True
assert srv_stop(leader)["clean"] is True

# ---- the replica's directory is an ordinary database once nobody holds it
#
# This works only because the replica released the lock, and it is what proves
# the data is in the *store* rather than in anything the server was holding.
db = db_open("replica", shards=2)
snap = db_snapshot(db)
assert snap_load(snap, 1) == sorted(want1)
assert snap_load(snap, 2) == sorted(want2)
assert snap_load(snap, 3) == sorted(want3)
snap_release(snap)
db_close(db)
