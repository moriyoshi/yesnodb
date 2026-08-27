# Serve a database over Arrow Flight and drive it the way a client library
# would: ask what is there, fetch it, ingest over the wire, stop, and check it
# survived a reopen.
#
# # Why this is a scenario and not another Rust test
#
# `yesno-flight/tests/roundtrip.rs` already pins the protocol's shape — the
# schema, the ticket, the cut surfaces. What it cannot express cheaply is the
# *sequence*, because each variation of it is a recompile. This file is the
# sequence and nothing else.
#
# And the comparison is the payoff. `flight_get` returns a plain Python list,
# so the expectation is `sorted(...)` of a set this file built. That is an
# oracle. A Rust test comparing the socket's answer against `Snapshot::load` is
# comparing yesno against yesno.
#
# # What the shape of the writes is for
#
#   key 1 — written before serving, so it exercises the read path against
#       state the service did not create.
#   key 2 — written *through* Flight with `do_put`, so it exercises the write
#       path, and is then read back with the ordinary `snap_*` verbs rather
#       than with Flight. Reading it back the way it went in would prove only
#       that Flight agrees with itself.
#   key 3 — never written, because "a key with nothing in it" is a different
#       answer from an error and the difference is easy to lose.
#
#   yesno-e2e --show-output --arg n=50000 flight.py

N = yn_arg("n", 4000)

db = db_open("served", shards=2)

# ---- state that exists before the service does
#
# A stride and one `db_insert_set`, not a loop of `db_insert`. Each
# `db_insert` is a commit and therefore an **fsync**: written as a loop this
# setup alone took 15 seconds, against 0.2 for the same data in one call. The
# Python-side set is free by comparison — it costs no host calls at all.
sb = sb_new()
sb_stride(sb, 0, 3, N)
db_insert_set(db, 1, sb_build(sb))
want1 = set()
for i in range(N):
    want1.add(i * 3)

f = flight_serve(db)

# ---- the headline: an exact count, with no ordinal materialized
info = flight_info(f, 1)
assert info["total_records"] == len(want1), (
    "get_flight_info must answer exactly, from the index: got %d, want %d"
    % (info["total_records"], len(want1))
)
assert info["key"] == 1
assert info["ordered"] is True
# The ticket is minted at the database's current visible version.
assert info["version"] == db_stats(db)["visible"], (
    "the ticket's snapshot version is not the database's visible version"
)

# A key that was never written is a legitimate, empty answer — not an error.
assert flight_info(f, 3)["total_records"] == 0
assert flight_get(f, 3) == []

# ---- the round trip
got1 = flight_get(f, 1)
assert got1 == sorted(want1), "the ordinals off the wire are not the ones stored"
assert len(got1) == info["total_records"], (
    "total_records promised %d rows and do_get delivered %d"
    % (info["total_records"], len(got1))
)

# ---- ingest over the wire, and read it back by another road
keys = []
ords = []
want2 = set()
for i in range(N // 2):
    o = i * 7 + 1
    keys.append(2)
    ords.append(o)
    want2.add(o)
# A second key in the same batch: do_put takes pairs, so one batch may span
# keys, and the batch is committed atomically.
extra = 4
want4 = set()
for i in range(64):
    o = i * 1000
    keys.append(extra)
    ords.append(o)
    want4.add(o)

assert flight_put(f, keys, ords) == len(keys)

snap = db_snapshot(db)
assert snap_load(snap, 2) == sorted(want2), "do_put did not land key 2"
assert snap_load(snap, extra) == sorted(want4), "do_put did not land key 4"
assert snap_cardinality(snap, 2) == len(want2)
snap_release(snap)

# And Flight agrees with the engine about what it just ingested.
assert flight_info(f, 2)["total_records"] == len(want2)

# ---- the Flight action surface
actions = flight_actions(f)
assert "compact" not in actions, "checkpoint leaked into the Flight surface"
assert "stats" in actions, actions
stats = flight_action(f, "stats")
assert stats["shards"] == 2, stats

# This fixture hosts the bare Flight service rather than `yesnod`, so its
# durability boundary is driven through the embedded database API. The daemon
# scenarios exercise the typed control-plane checkpoint RPC.
watermark = db_checkpoint(db)
assert watermark > 0, "checkpoint returned no watermark"

# ---- teardown, and the lock
#
# The service holds a `Db` clone, and a `Db` clone holds the directory's
# exclusive lock. Closing while it serves would fail the *reopen* below with a
# lock error pointing nowhere near the cause, so the harness refuses it here
# instead.
flight_stop(f)
db_close(db)

# ---- everything that went over the wire is still there
db = db_open("served", shards=2)
snap = db_snapshot(db)
assert snap_load(snap, 1) == sorted(want1), "key 1 did not survive the reopen"
assert snap_load(snap, 2) == sorted(want2), "the do_put ingest did not survive the reopen"
assert snap_load(snap, extra) == sorted(want4)
snap_release(snap)
db_close(db)
