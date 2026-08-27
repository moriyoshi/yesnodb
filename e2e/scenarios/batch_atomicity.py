# Batches: all-or-nothing across shards, and across a crash.
#
# A batch that touches several keys is a multi-shard commit, which is the one
# place the design needs two-phase machinery (`CommitIntent` naming the
# participants, then a `ShardCommit` in each). The end-to-end statement of that
# is simply: a committed batch is entirely present after a reopen, and a
# rolled-back one is entirely absent.
#
# Shards are chosen by `splitmix64(key)`, so spreading over many keys is what
# makes this a multi-shard commit rather than a single-shard one. The scenario
# asserts that it really got more than one shard, or it would be testing the
# easy case.

d = db_open("main", shards=4)

keys = [1, 2, 3, 4, 5, 6, 7, 8]
touched = set()
for k in keys:
    touched.add(db_shard_of(d, k))
assert len(touched) > 1, "these keys must span several shards, or this proves nothing"

# --- a committed batch is visible atomically
b = batch(d)
for k in keys:
    batch_insert(b, k, k * 10)
    batch_insert_range(b, k, 1000 + k, 1010 + k)
r = batch_commit(b)

assert r["changed"] == len(keys) * 12, "one ordinal plus an 11-wide range per key"
assert r["shards"] > 1, "the commit must have spanned several shards"
assert r["version"] > 0

s = db_snapshot(d)
for k in keys:
    assert snap_contains(s, k, k * 10)
    assert snap_cardinality(s, k) == 12

# --- a rolled-back batch leaves nothing
snap_release(s)
b2 = batch(d)
batch_insert(b2, 99, 1)
batch_insert_range(b2, 99, 100, 200)
batch_rollback(b2)

s = db_snapshot(d)
assert snap_is_empty(s, 99), "a rolled-back batch must leave nothing behind"

# A rolled-back handle is dead, not reusable.
dead = False
try:
    batch_insert(b2, 99, 2)
except ValueError:
    dead = True
assert dead, "a rolled-back batch handle must be refused"

# --- removals and deletes inside a batch
snap_release(s)
b3 = batch(d)
batch_remove(b3, 1, 10)
batch_remove_range(b3, 2, 1002, 1006)
batch_delete_key(b3, 3)
batch_commit(b3)

s = db_snapshot(d)
assert snap_contains(s, 1, 10) is False
assert snap_cardinality(s, 1) == 11
assert snap_cardinality(s, 2) == 12 - 5
assert snap_is_empty(s, 3), "batch_delete_key must remove the whole key"
assert snap_cardinality(s, 4) == 12, "an untouched key must be untouched"

# --- and all of it survives a crash
#
# No checkpoint here on purpose: this is the write-ahead log replaying a
# multi-shard commit, which is the path `CommitIntent` exists for.
snap_release(s)
d = db_reopen(d)
s = db_snapshot(d)
assert snap_cardinality(s, 1) == 11
assert snap_cardinality(s, 2) == 7
assert snap_is_empty(s, 3)
assert snap_is_empty(s, 99), "a rolled-back batch must not reappear from the log"
for k in [4, 5, 6, 7, 8]:
    assert snap_cardinality(s, k) == 12, f"key {k} did not survive replay"
