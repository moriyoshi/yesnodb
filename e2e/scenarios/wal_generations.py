# WAL generations across the full operational sequence: retain, seal, append,
# resume across the boundary, reopen, and finally reclaim.
#
# This is deliberately one shard. Sharding is orthogonal here, while one WAL
# makes every physical-layout assertion name exactly the history whose contents
# are compared below. Python lists are the contents oracle; `db_wal_layout`
# reports only generation counts and byte sizes and does not replay anything.

lead = db_open("leader", shards=1)

FIRST = list(range(0, 1001))
db_insert_range(lead, 1, FIRST[0], FIRST[-1])

svc = repl_serve(lead)
rep = repl_follower("replica")
repl_seed(rep, svc)

# Catch up only through the first write and publish that cursor as the floor.
# The leader then writes beyond it, making the floor land strictly inside the
# generation the checkpoint is about to seal.
caught = repl_catch_up(rep, svc, 0, 0, 0)
assert caught["records"] > 0, "the initial catch-up shipped no WAL records"
assert repl_ack(rep, svc, 0) == 0, "the follower was not caught up before the hold"

SECOND = [200000 + i * 13 for i in range(800)]
db_insert_many(lead, 2, SECOND)
before = db_wal_layout(lead)[0]
assert before["sealed_generations"] == 0, "a fresh WAL unexpectedly began sealed"
assert before["active_bytes"] > 0, "the active WAL contains no bytes to seal"

db_checkpoint(lead)
sealed = db_wal_layout(lead)[0]
assert sealed["sealed_generations"] == 1, (
    "the checkpoint did not seal an immutable WAL generation while a follower "
    "still needed its suffix"
)
assert sealed["sealed_bytes"] > 0, "the sealed generation is empty"
assert sealed["active_bytes"] == 0, "the new active generation did not start empty"

# This tail is in the new active generation. Resuming from `caught` must read
# the remainder of the sealed file and then continue into this active file.
THIRD = [400000 + i * 17 for i in range(600)]
db_insert_many(lead, 3, THIRD)
split = db_wal_layout(lead)[0]
assert split["sealed_generations"] == 1
assert split["active_bytes"] > 0, "the post-checkpoint write missed the active generation"

end = repl_status(svc)["end_lsn"][0]
moved = repl_catch_up(rep, svc, 0, caught["next_lsn"], 0)
assert moved["records"] > 0, "the cross-generation resume shipped no records"
assert moved["next_lsn"] == end, "the cross-generation resume stopped before the logical end"

# Do not ack this cursor. The floor consumed by the first checkpoint must not
# become a permanent pin merely because the follower object remains alive.
repl_stop(svc)

# Both recovery directions matter. The leader reopens a retained sealed file
# followed by an active tail; the follower reopens the concatenated frames it
# received through the shipping path.
lead = db_reopen(lead)
replica = db_open("replica", shards=1)
ls = db_snapshot(lead)
rs = db_snapshot(replica)
for key in (1, 2, 3):
    assert snap_load(ls, key) == snap_load(rs, key), f"key {key} diverged across generations"
assert snap_load(ls, 1) == FIRST
assert snap_load(ls, 2) == sorted(SECOND)
assert snap_load(ls, 3) == sorted(THIRD)
snap_release(ls)
snap_release(rs)
db_close(replica)

# A subsequent checkpoint with no fresh ack may reclaim every now-redundant
# sealed generation. A write is load-bearing: an entirely idle shard has no
# new checkpoint work and correctly remains physically quiet.
FOURTH = [800000 + i for i in range(128)]
db_insert_many(lead, 4, FOURTH)
db_checkpoint(lead)
reclaimed = db_wal_layout(lead)[0]
assert reclaimed["sealed_generations"] == 0, "an obsolete sealed generation was not reclaimed"
assert reclaimed["retained_bytes"] == 0, "a no-floor checkpoint left redundant WAL bytes"

lead = db_reopen(lead)
final = db_snapshot(lead)
assert snap_load(final, 1) == FIRST
assert snap_load(final, 2) == sorted(SECOND)
assert snap_load(final, 3) == sorted(THIRD)
assert snap_load(final, 4) == FOURTH

print("  sealed generation retained, crossed, reopened, and reclaimed")

snap_release(final)
db_close(lead)
