# A leader, a standby, and a promotion — the operational sequence, scripted.
#
# # Why this is a scenario
#
# `yesno-server/tests/follower.rs` pins the loop's behaviour: it catches up, it
# resumes across a restart, it halts on the two conditions a retry cannot fix.
# What is a recompile there and a line here is the *sequence an operator
# performs*: write, checkpoint, write again, stand a standby up, watch it track,
# stop the leader, promote, and then serve queries from the node that used to be
# a replica.
#
# And the comparison is the payoff. What the promoted node answers is compared
# against a Python `set` this file built — not against what the old leader said,
# which would be comparing yesno against yesno.
#
# # The shape of the writes
#
#   key 1 — written **before** the checkpoint, so it reaches the standby through
#       the physical base image.
#   key 2 — written **after** it, so it exists only as log records and can only
#       arrive through catch-up. A bootstrap that copied the image and shipped
#       nothing would satisfy every other assertion here.
#   key 3 — written while the standby is already following, so it exercises
#       steady state rather than the seam.
#
#   yesno-e2e --show-output --arg n=20000 failover.py

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

# Checkpoint, so key 1 lives in the image rather than in the log.
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

# ---- a standby, built from the socket alone
scfg = srv_config("standby", shards=2)
srv_follows(scfg, leader)
standby = srv_launch(scfg)

# A standby serves no queries — it opens no database at all — and asking it
# for one must say so rather than handing back something that fails later.
refused = False
try:
    srv_flight(standby)
except ValueError as e:
    refused = True
    assert "standby" in str(e), str(e)
assert refused, "a standby answered a request for a query endpoint"

# The wait happens in the host. A poll loop written here spins hundreds of
# times a millisecond against a standby whose interval is a second, so it is a
# busy loop that gives up rather than a wait.
srv_follower_wait(standby, "records", 1, 30000)
st = srv_follower_state(standby)
assert st["connected"] is True, st
assert st["halted"] is False, st

# ---- steady state: write again, and watch it arrive
before = srv_follower_state(standby)["applied_bytes"]
want3 = set()
keys = []
ords = []
for i in range(200):
    o = i * 11
    keys.append(3)
    ords.append(o)
    want3.add(o)
flight_put(lf, keys, ords)

srv_follower_wait(standby, "applied_bytes", before + 1, 30000)
assert srv_follower_state(standby)["rebootstraps"] == 0, (
    "the standby rebuilt itself during ordinary operation"
)

# ---- what a client sees, before anything moves
#
# The term is on **every** response, not behind a special call, so a client
# that only ever reads still learns it — and it is only from values it has
# actually seen that a client can build the memory which protects it.
assert flight_term(lf) == 0, "a fresh leadership reports term 0"

# ---- failover
#
# Stop the leader first. In a real outage the operator has to establish that
# independently: nothing in yesno fences a split brain, and a promoted standby
# carries the same database identity as the node it replaced.
flight_stop(lf)
t = srv_stop(leader)
assert t["clean"] is True, t

# Before the promotion, both nodes are the same leadership. Identity cannot
# tell them apart — a standby *is* a copy of its leader, same `db_uuid`, same
# shard files — so the term is the only thing that can.
assert srv_term(standby) == 0, "a database that has never failed over is at term 0"

srv_promote(standby)

# And promotion raises it. This is the fence: a leader that was replaced and
# came back is still at the old term, and a standby that has seen the new one
# refuses it instead of following it back onto an abandoned timeline.
assert srv_term(standby) == 1, "promotion did not raise the leadership term"

# ---- and the promoted node answers for everything it was shipped
pf = srv_flight(standby)
assert flight_info(pf, 1)["total_records"] == len(want1), (
    "the image half did not survive promotion"
)
assert flight_info(pf, 2)["total_records"] == len(want2), (
    "the log half did not survive promotion"
)
assert flight_info(pf, 3)["total_records"] == len(want3), (
    "the steady-state writes did not survive promotion"
)
assert flight_get(pf, 3) == sorted(want3)

# A promoted node is a leader in full: it accepts writes.
assert flight_put(pf, [4], [42]) == 1
assert flight_info(pf, 4)["total_records"] == 1

# ---- and the client-facing fence
#
# The promoted node reports the new term, and a client that has moved on with it
# is served.
assert flight_term(pf) == 1
flight_expect_term(pf, 1)
assert flight_info(pf, 1)["total_records"] == len(want1)

# A client pinned to a term this server does not have is refused. That is the
# case that matters in a real outage: a superseded leader does not know it has
# been replaced, so the caller is the only party holding the newer number.
refused = False
try:
    flight_expect_term(pf, 2)
    flight_info(pf, 1)
except RuntimeError as e:
    refused = True
    assert "FailedPrecondition" in str(e), str(e)
    assert "older leadership" in str(e), str(e)
assert refused, "a server served a client that requires a newer leadership"

# And a client *behind* the server is fine. After a promotion every client is
# behind for a moment, and refusing them would turn a failover into an outage —
# the response header is what teaches them the newer number.
flight_expect_term(pf, 0)
assert flight_info(pf, 1)["total_records"] == len(want1)

flight_stop(pf)
t = srv_stop(standby)
assert t["clean"] is True, t

# ---- and the directory is an ordinary yesno database afterwards
#
# This works only because the promoted node released the lock, and it is what
# proves the data is in the *store* rather than in anything the server held.
db = db_open("standby", shards=2)
snap = db_snapshot(db)
assert snap_load(snap, 1) == sorted(want1)
assert snap_load(snap, 2) == sorted(want2)
assert snap_load(snap, 3) == sorted(want3)
snap_release(snap)
db_close(db)
