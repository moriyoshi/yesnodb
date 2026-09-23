# A Flight ticket is a promise about *when*, and this is the sequence that
# holds the server to it.
#
# # Why this is a scenario and not only a Rust test
#
# `yesno-flight/tests/ticket_consistency.rs` pins the mechanism. What it cannot
# vary without a recompile is the **operational sequence** — how long a ticket is
# held, what happens to the database while it is held, and what an operator sees
# when the answer is refused. That is this file's job, and Python's own `set` is
# the oracle for every answer.
#
# The verbs are deliberately split. `flight_get` mints a ticket and fetches
# with it inside one host call, so nothing can happen in between and it can never
# observe whether the version is honoured at all. `flight_ticket` + `flight_fetch`
# put the seam where the interesting thing happens.
#
#   yesno-e2e --show-output --arg n=50000 ticket_version.py

N = yn_arg("n", 4000)

cfg = srv_config("leader", shards=2, interval_secs=60)
srv_control_admin(cfg, "all")
srv_replication(cfg)
srv_control_admin(cfg, "all")
node = srv_launch(cfg)
f = srv_flight(node)

# ---- the set the count will be computed over
want = set()
keys = []
ords = []
for i in range(N):
    o = i * 3
    keys.append(1)
    ords.append(o)
    want.add(o)
flight_put(f, keys, ords)
assert srv_checkpoint(node) > 0

t = flight_ticket(f, 1)
assert t["total_records"] == len(want), t
assert t["version"] > 0, "a ticket without a version cannot pin anything"
assert t["key"] == 1
assert len(t["raw"]) == 40, "the ticket is a fixed 40-byte structure"

# ---- a write lands while the ticket is in the caller's hand
#
# Ordinals disjoint from `want`, so a fetch from the wrong instant is
# detectable ordinal-for-ordinal and not only by a count. A test that checked
# the count alone would pass against a server that returned a *different* set of
# the same size.
later = set()
keys = []
ords = []
for i in range(N // 5):
    o = i * 3 + 1
    keys.append(1)
    ords.append(o)
    later.add(o)
flight_put(f, keys, ords)

got = flight_fetch(f, t["raw"])
assert len(got) == t["total_records"], (
    "the fetch delivered " + str(len(got)) + " rows against a promise of "
    + str(t["total_records"])
)
assert set(got) == want, "the rows are not the ones the count was computed over"
assert set(got).isdisjoint(later), "the fetch leaked rows from after the ticket"

# The write is genuinely there — this must not pass by the write having failed.
assert flight_info(f, 1)["total_records"] == len(want) + len(later)

# ---- two readers, one instant
#
# The fan-out the version field exists for. Both fetches use the *same* ticket
# across another write, so a server answering from "now" would give them
# different sets.
mid = set()
keys = []
ords = []
for i in range(200):
    o = i * 3 + 2
    keys.append(1)
    ords.append(o)
    mid.add(o)

a = flight_fetch(f, t["raw"])
flight_put(f, keys, ords)
b = flight_fetch(f, t["raw"])
assert a == b, "two readers holding one ticket saw two different databases"
assert set(a) == want

# ---- a live lease keeps the ticket honourable across a checkpoint
#
# This is what the lease is *for*. `GetFlightInfo` parks a clone of the
# snapshot, which is a registered reader, so the version cannot be reclaimed
# under a caller still holding the ticket it was handed.
#
# **This assertion is new on 2026-09-23 and it used to assert the opposite.**
# Leasing was dead in `yesnod`: a fresh Flight service was built per RPC, so
# `DoGet` never found the lease `GetFlightInfo` had parked and fell back to
# re-opening by version, which the checkpoint below had collapsed. The refusal
# this file asserted was therefore real but reached for the wrong reason, and
# it was the only evidence anywhere that the lease mechanism did nothing.
assert srv_checkpoint(node) > 0
assert set(flight_fetch(f, t["raw"])) == want, (
    "a checkpoint must not collapse a version a live lease is holding"
)

# ---- a ticket the server can no longer honour is refused, not approximated
#
# `FailedPrecondition`, and the code is the whole point: it means "do not
# retry until you have fixed something", and the something is asking for a new
# ticket. `Unavailable` or an internal error would put a client in a loop
# against a version the floor only recedes further from.
#
# Reached by restarting rather than by waiting: leases live in the service, so
# a restart drops every one of them, and the checkpoint above already collapsed
# the version this ticket names. Waiting out the lease would mean sleeping for
# its full term in a gate.
assert srv_stop(node)["clean"] is True
restarted = srv_config("leader", shards=2, interval_secs=60)
srv_control_admin(restarted, "all")
srv_replication(restarted)
node = srv_launch(restarted)
f = srv_flight(node)

# The checkpoint above could not collapse this version -- the lease was holding
# it, which is the previous assertion's whole point -- so it survived into the
# restarted database. Writing and checkpointing again, with nothing pinning it
# now, is what moves the floor past it.
post = set()
keys = []
ords = []
for i in range(N // 5):
    o = i * 3 + 200000000
    keys.append(1)
    ords.append(o)
    post.add(o)
flight_put(f, keys, ords)
assert srv_checkpoint(node) > 0

refused = False
try:
    flight_fetch(f, t["raw"])
except RuntimeError as e:
    refused = True
    assert "FailedPrecondition" in str(e), str(e)
    assert "GetFlightInfo" in str(e), (
        "the refusal has to say what to do about it: " + str(e)
    )
assert refused, "a collapsed version was answered instead of refused"

# ---- and the server is fine. A stale ticket is the client's problem.
fresh = flight_ticket(f, 1)
assert set(flight_fetch(f, fresh["raw"])) == want | later | mid | post
assert fresh["version"] > t["version"]

flight_stop(f)
assert srv_stop(node)["clean"] is True
