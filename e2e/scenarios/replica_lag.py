# A follower's watermark and its lag, across a multi-shard commit and a leader
# that keeps writing.
#
# # Two things this pins that `replication.py` cannot
#
# 1. **The watermark is a prefix rule over versions, not a byte count.** A commit
#    spanning shards becomes visible on the follower only once *every*
#    participant's records have arrived, and a hole below stops everything above
#    it. At one shard per commit those two rules agree on every input, so only a
#    genuinely spanning batch, caught up one shard at a time, can tell them
#    apart.
# 2. **Lag is reported, not assumed.** `Ack` drives the leader's retention floor;
#    a follower the leader has run ahead of must come back with a non-zero
#    number, and zero is what a stub would also return.
#
# **There is no checkpoint here, and that is load-bearing.** A checkpoint can
# reclaim generations already represented by its base image, so a follower
# starting at version 0 never sees those versions and its watermark stays at 0
# for ever — correctly, since its base image already covers them. Every assertion
# about `repl_visible` would then be unfalsifiable.
# `replication.py` is the checkpointed half; this one keeps the whole log so the
# watermark is a live quantity. Skipping the bootstrap is also why this scenario
# is cheap: no shard image crosses the socket.
#
#   yesno-e2e --show-output replica_lag.py

SHARDS = 4

lead = db_open("leader", shards=SHARDS)

# One key per shard, asked of the database rather than assumed: `vshard_of` is a
# hash, so "keys 0..n spread over n shards" is a property of the hash and not of
# the loop.
found = {}
k = 0
while len(found) < SHARDS and k < 100000:
    s = db_shard_of(lead, k)
    if s not in found:
        found[s] = k
    k = k + 1
assert len(found) == SHARDS, f"only {len(found)} of {SHARDS} shards are reachable"
keys = [found[s] for s in range(SHARDS)]

# ---- version 1: one batch touching every shard ------------------------------

b = batch(lead)
for i in range(SHARDS):
    batch_insert_range(b, keys[i], i * 100000, i * 100000 + 4999)
c = batch_commit(b)
assert c["shards"] == SHARDS, (
    f"the batch reached {c['shards']} shards, so it is not a spanning commit and "
    "this scenario is about nothing"
)
assert c["version"] == 1

# Version 2, single-shard, *after* it. It is complete on its own and must still
# stay invisible while version 1 has a hole below it.
b2 = batch(lead)
batch_insert(b2, keys[0], 999999)
assert batch_commit(b2)["version"] == 2

svc = repl_serve(lead)
end = repl_status(svc)["end_lsn"]

rep = repl_follower("replica")
repl_seed(rep, svc)

# ---- the participants arrive one at a time ----------------------------------

for s in range(SHARDS):
    moved = repl_catch_up(rep, svc, s, 0, 0)
    assert moved["records"] > 0, f"shard {s} shipped nothing, so it is not a participant"
    if s < SHARDS - 1:
        assert repl_visible(rep) == 0, (
            f"version 1 is still missing {SHARDS - 1 - s} participant(s), so nothing "
            "may be visible — including version 2, which is complete but sits above "
            f"the hole; the follower says {repl_visible(rep)}"
        )

assert repl_visible(rep) == 2, (
    "with every participant in hand the prefix must resolve through the later "
    f"single-shard commit too, not stop at {repl_visible(rep)}"
)

for s in range(SHARDS):
    assert repl_ack(rep, svc, s) == 0, f"shard {s} is reported behind after a full catch-up"

# ---- the leader runs ahead ---------------------------------------------------

cursor0 = repl_cursor(rep, 0)
assert cursor0 == end[0], "the follower's cursor is not at the log's end"

for i in range(20):
    db_insert_range(lead, keys[0], 5000000 + i * 1000, 5000000 + i * 1000 + 500)

grown = repl_status(svc)["end_lsn"][0]
assert grown > cursor0, "the leader's log did not grow"
assert repl_ack(rep, svc, 0) == grown - cursor0, (
    "the leader must report exactly the un-applied bytes"
)

# Resuming from the cursor ships the tail, not the whole log again.
tail = repl_catch_up(rep, svc, 0, cursor0, 0)
assert tail["bytes"] == grown - cursor0, (
    f"a resume shipped {tail['bytes']} bytes where the tail is {grown - cursor0}"
)
assert tail["next_lsn"] == grown
assert repl_ack(rep, svc, 0) == 0

repl_stop(svc)

# ---- and the replica agrees, key for key -------------------------------------

r = db_open("replica")
a = db_snapshot(lead)
f = db_snapshot(r)
for key in keys:
    assert snap_cardinality(a, key) > 0
    assert snap_load(a, key) == snap_load(f, key), f"key {key} diverged"

print(f"  {SHARDS} shards, visible {repl_visible(rep)}, {len(keys)} keys set-equal")

snap_release(a)
snap_release(f)
db_close(r)
db_close(lead)
