# Run the daemon an operator actually runs: start it, talk to it over Flight,
# stop it, start it again, and check it gave the database back.
#
# # Why this is a scenario
#
# `yesno-server/tests/` pins the fixed shapes — one start, one stop, one leaked
# handle. What is a recompile there and a line here is *varying the sequence*:
# restart, ingest between restarts, stop with work outstanding, run the
# checkpoint driver at one second rather than at a minute.
#
# # The assertion that matters
#
# `yesno-core` has no `Db::close()`. Teardown is `Drop` on the last
# `Arc<DbInner>`, and the exclusive directory lock lives inside it — so a handle
# leaked anywhere leaves the directory locked with nothing saying so, and the
# failure surfaces at the *next* start as `AlreadyOpen`, pointing nowhere near
# its cause. `srv_stop` reports what teardown observed, and the restart below is
# the independent check that it was telling the truth.
#
#   yesno-e2e --show-output --arg n=20000 server_lifecycle.py

N = yn_arg("n", 3000)

# `interval_secs=1` so the background checkpoint driver actually runs during
# this scenario. The engine never fires that trigger on its own — nothing in
# `yesno-core` ticks — so this is the daemon's driver or nothing.
srv = srv_start("node", shards=2, interval_secs=1)
f = srv_flight(srv)

# ---- ingest over the wire, into a database only the daemon has open
keys = []
ords = []
want = set()
for i in range(N):
    o = i * 5
    keys.append(11)
    ords.append(o)
    want.add(o)
assert flight_put(f, keys, ords) == N

info = flight_info(f, 11)
assert info["total_records"] == N, (
    "the daemon's exact count disagrees with what was ingested: %d vs %d"
    % (info["total_records"], N)
)
assert flight_get(f, 11) == sorted(want)

# ---- the metrics surface, in the exposition format a scraper consumes
m = srv_metrics(srv)
assert "yesnod_shards 2" in m, m
assert "yesnod_build_info" in m, m
assert "# TYPE yesnod_wal_syncs_total counter" in m, m
# Every HELP must have a TYPE. A renderer that emits one without the other is a
# silently failed scrape rather than a visible error.
assert m.count("# HELP ") == m.count("# TYPE "), (
    "unbalanced HELP and TYPE lines: %d vs %d" % (m.count("# HELP "), m.count("# TYPE "))
)

# ---- stop, and insist the teardown was clean
flight_stop(f)
t = srv_stop(srv)
assert t["clean"] is True, "the daemon did not shut down cleanly: %s" % t
assert t["readers_left"] == 0, t
assert t["lock_released"] is True, t
assert t["checkpoint"] is not None, "no final checkpoint ran"

# ---- the independent check: the lock really was released
#
# A `srv_stop` that reported success while holding the directory would be caught
# here and nowhere else — this is the same proof the daemon's own test makes,
# and it is worth making twice because the consequence is silent.
srv = srv_start("node", shards=2, interval_secs=1)
f = srv_flight(srv)
assert flight_info(f, 11)["total_records"] == N, (
    "the ingest did not survive a restart"
)
assert flight_get(f, 11) == sorted(want)

# ---- a second write, so the restart is not merely read-only
more = set()
keys = []
ords = []
for i in range(500):
    o = i * 5 + 2
    keys.append(11)
    ords.append(o)
    more.add(o)
flight_put(f, keys, ords)
want = want | more
assert flight_info(f, 11)["total_records"] == len(want)

flight_stop(f)
t = srv_stop(srv)
assert t["clean"] is True, t

# ---- and the directory is an ordinary yesno database when nobody serves it
#
# This only works because the daemon released the lock. It is also what
# proves the data is in the *store* rather than in some server-side cache.
db = db_open("node", shards=2)
snap = db_snapshot(db)
assert snap_load(snap, 11) == sorted(want), "the store disagrees with the wire"
assert snap_cardinality(snap, 11) == len(want)
snap_release(snap)
db_close(db)
