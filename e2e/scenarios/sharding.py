# Key locality (I7) and shard fan-out.
#
# The invariant is that **all chunks of one key live in one shard** — sharding
# is by `key`, never by `prefix48`. Everything above it depends on that: a
# range scan of one key is a single shard's B+tree walk, and a single-key write
# is a single-shard commit needing no `CommitIntent` at all.
#
# The end-to-end statement is that a key's assignment is a pure function of the
# key, stable across reopens, and that a key spanning many chunks does not
# scatter.

d = db_open("main", shards=4)

keys = list(range(64))
for k in keys:
    # Three chunks per key: chunk 0, chunk 1 and chunk 2 of the ordinal space.
    db_insert_many(d, k, [k, k + 65536, k + 131072])
db_checkpoint(d)

# --- every key reads back exactly
s = db_snapshot(d)
for k in keys:
    assert sorted(snap_load(s, k)) == [k, k + 65536, k + 131072], f"key {k} is wrong"

# --- the fan-out is real
#
# Without this, every assertion here would hold just as well with one shard,
# and the scenario would be testing nothing about sharding.
placement = [db_shard_of(d, k) for k in keys]
assert len(set(placement)) == 4, "64 keys must reach all four shards"

# --- assignment is a pure function of the key, and survives a reopen
snap_release(s)
d = db_reopen(d)
assert [db_shard_of(d, k) for k in keys] == placement, "shard assignment moved"

s = db_snapshot(d)
for k in keys:
    assert snap_cardinality(s, k) == 3, f"key {k} lost chunks across the reopen"

# --- a key that spans many chunks stays whole
#
# 400 chunks under one key. If any chunk were placed by prefix rather than by
# key, this key would be split across shards and the load below would come back
# short.
snap_release(s)
wide = 999
db_insert_many(d, wide, [i * 65536 + 7 for i in range(400)])
db_checkpoint(d)

s = db_snapshot(d)
assert snap_cardinality(s, wide) == 400
assert sorted(snap_load(s, wide)) == sorted([i * 65536 + 7 for i in range(400)])
assert snap_min(s, wide) == 7
assert snap_max(s, wide) == 399 * 65536 + 7

# The wide key occupies one shard, and the other keys are undisturbed.
snap_release(s)
d = db_reopen(d)
s = db_snapshot(d)
assert snap_cardinality(s, wide) == 400, "the wide key did not survive a reopen"
for k in keys:
    assert snap_cardinality(s, k) == 3

# --- distinct keys with identical ordinal sets stay distinct
snap_release(s)
db_insert_many(d, 5000, [1, 2, 3])
db_insert_many(d, 5001, [1, 2, 3])
db_remove(d, 5000, 2)

s = db_snapshot(d)
assert sorted(snap_load(s, 5000)) == [1, 3]
assert sorted(snap_load(s, 5001)) == [1, 2, 3], "keys must not share chunk storage"
