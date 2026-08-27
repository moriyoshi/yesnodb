# Durability: the two ways data reaches disk, and the one that is easy to skip.
#
# `Db` has no `Drop` impl, so nothing is flushed when a handle goes away. That
# makes a close already a crash: whatever was committed but not checkpointed
# exists only in the write-ahead log, and reopening is redo-only recovery.
#
# Both halves are asserted here on purpose. A durability test that checkpoints
# before every reopen never executes the replay path at all, and replay is the
# half where a bug loses acknowledged writes.

d = db_open("main", shards=2)

# --- Phase 1: checkpointed. Reaches the data file through the superblock flip.
checkpointed = [1, 5, 9, 65540, 131072, 4000000]
db_insert_many(d, 7, checkpointed)
db_checkpoint(d)

# --- Phase 2: committed, never checkpointed. Lives only in the log.
replayed = [2, 6, 70000]
db_insert_many(d, 8, replayed)
assert db_stats(d)["wal_bytes"] > 0, "a commit must have reached the log"
assert db_stats(d)["dirty_bytes"] > 0, "uncheckpointed data must still be dirty"

d = db_reopen(d)
s = db_snapshot(d)
assert sorted(snap_load(s, 7)) == sorted(checkpointed), "checkpointed data was lost"
assert sorted(snap_load(s, 8)) == sorted(replayed), "WAL replay lost a commit"

# --- Phase 3: a removal must replay too.
#
# Worth its own phase: a replay that reapplies inserts but drops tombstones
# resurrects deleted ordinals, and every assertion above would still pass.
snap_release(s)
db_remove(d, 7, 5)
db_remove_range(d, 7, 131072, 4000000)
survivors = [1, 9, 65540]

d = db_reopen(d)
s = db_snapshot(d)
assert sorted(snap_load(s, 7)) == survivors, "a removal did not survive replay"
assert snap_contains(s, 7, 5) is False, "a removed ordinal came back"

# --- Phase 4: checkpoint after replay, then reopen again.
#
# The checkpoint has to fold the replayed state into the data file. If it wrote
# only what changed since the reopen, phase 2's data would vanish here.
snap_release(s)
db_checkpoint(d)
d = db_reopen(d)
s = db_snapshot(d)
assert sorted(snap_load(s, 7)) == survivors, "checkpoint-after-replay lost data"
assert sorted(snap_load(s, 8)) == sorted(replayed), "checkpoint-after-replay lost key 8"

report = db_fsck(d)
assert report["consistent"], f"the store is corrupt after replay: {report['errors']}"
assert report["chunks"] > 0, "an fsck that walked no chunks proves nothing"
assert report["index_nodes"] > 0, "fsck must walk the tree's own pages too"

# The strong form, across a reopen. The deferred free list is in-memory, so an
# extent still awaiting the reclamation conditions at shutdown holds a slot the
# checkpoint persisted as used — and used to be orphaned by the reopen, once per
# restart. Open now recomputes occupancy from the committed index, so `leaked`
# is zero on the far side of a restart too.
assert report["leaked"] == 0, f"the reopen orphaned slots: {report['leaked']}"
assert report["clean"], f"not clean after replay and checkpoint: {report['errors']}"
