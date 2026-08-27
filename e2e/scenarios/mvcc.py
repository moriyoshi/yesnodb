# Snapshot isolation, end to end and across a checkpoint.
#
# The zero-copy design makes this a soundness property rather than a
# convenience one: containers alias the mmap, so a snapshot that saw a page get
# rewritten underneath it would be reading freed memory, not merely stale data.
# The three-condition reclamation rule exists for exactly this, and the visible
# half of it is what this scenario pins.

d = db_open("main", shards=2)

db_insert_many(d, 1, [1, 2, 3])
db_insert_range(d, 2, 0, 5000)
db_checkpoint(d)

early = db_snapshot(d)
v_early = snap_version(early)
assert snap_cardinality(early, 2) == 5001

# --- writes after the snapshot are invisible to it
db_insert_many(d, 1, [4, 5, 6])
db_remove_range(d, 2, 0, 999)

late = db_snapshot(d)
assert snap_version(late) > v_early, "a later snapshot must carry a later version"

assert sorted(snap_load(early, 1)) == [1, 2, 3], "an old snapshot saw a later write"
assert sorted(snap_load(late, 1)) == [1, 2, 3, 4, 5, 6]
assert snap_cardinality(early, 2) == 5001, "an old snapshot saw a later removal"
assert snap_cardinality(late, 2) == 5001 - 1000
assert snap_contains(early, 2, 0), "the removed ordinal must persist for the old reader"
assert snap_contains(late, 2, 0) is False

# --- a checkpoint under a live reader must not disturb it
#
# This is the dangerous one. Checkpointing rewrites the index and allocates
# extents; if it reclaimed what `early` still points at, the reads below would
# be reading recycled space.
db_checkpoint(d)
assert sorted(snap_load(early, 1)) == [1, 2, 3], "checkpoint disturbed a live reader"
assert snap_cardinality(early, 2) == 5001
assert snap_contains(early, 2, 500)

# The reader count is visible, and drops as snapshots are released.
assert db_stats(d)["live_readers"] >= 2

# --- delete_key is also versioned
b = batch(d)
batch_delete_key(b, 1)
batch_commit(b)

newest = db_snapshot(d)
assert snap_is_empty(newest, 1), "the key was deleted"
assert sorted(snap_load(early, 1)) == [1, 2, 3], "a delete must not reach back in time"
assert sorted(snap_load(late, 1)) == [1, 2, 3, 4, 5, 6]

# --- releasing readers lowers the count, and the newest state is durable
snap_release(early)
snap_release(late)
snap_release(newest)
assert db_stats(d)["live_readers"] == 0, "every reader was released"

db_checkpoint(d)
d = db_reopen(d)
s = db_snapshot(d)
assert snap_is_empty(s, 1), "the delete must survive the reopen"
assert snap_cardinality(s, 2) == 5001 - 1000
assert db_fsck(d)["consistent"]
