# Structural integrity under churn, and what `fsck` actually verifies.
#
# `fsck` is the escape hatch for every other storage risk in the design, so it
# is worth being precise about the three things it separates:
#
#   dangling  a live chunk — or a live index node — sits in a slot the
#             allocator believes is free. That space can be handed out again.
#             Corruption.
#
#   pending   a superseded extent still holding its slot while it waits out the
#             three reclamation conditions. Retention, not waste; the space
#             comes back.
#
#   leaked    a slot the allocator marks used that *nothing* accounts for:
#             no chunk, no index node, no pending reclamation. This must be
#             zero, and asserting that it is only became possible once the
#             rebuild learned to mark the index's own nodes — before that,
#             every live node was counted as a leak and `clean` could never
#             be true for a database that had an index.

d = db_open("main", shards=2)

# A corpus that reaches every storage path: inline (<= 3 ordinals), packed
# small arrays, standalone extents, a dense bitmap, and a multi-chunk key.
db_insert_many(d, 1, [1, 2])
db_insert_many(d, 2, [5, 500, 5000, 50000])
db_insert_range(d, 3, 0, 60000)
db_insert_many(d, 4, [i * 97 for i in range(3000)])
db_insert_many(d, 5, [i * 65536 for i in range(200)])
db_checkpoint(d)

report = db_fsck(d)
assert report["consistent"], f"inconsistent after the first checkpoint: {report['errors']}"
assert report["chunks"] > 0, "an fsck that walked no chunks is not reading the index"
assert report["inline_chunks"] > 0, "the corpus must exercise the inline path"

assert report["index_nodes"] > 0, "fsck must walk the tree's own pages, not just its entries"
assert report["leaked"] == 0, f"unaccounted slots: {report['leaked']}"
assert report["clean"], f"a healthy database must be clean: {report['errors']}"

# --- churn, which is when allocator state and index diverge if anything is wrong
for round in range(6):
    b = batch(d)
    batch_delete_key(b, 4)
    batch_commit(b)
    db_insert_many(d, 4, [round + i * 89 for i in range(3000)])
    db_remove_range(d, 3, round * 1000, round * 1000 + 400)
    db_checkpoint(d)

report = db_fsck(d)
assert report["consistent"], f"inconsistent after churn: {report['errors']}"
# Churn is where retention shows up. It must be counted as `pending`, never as
# a leak — otherwise every database that has ever superseded anything is dirty.
assert report["leaked"] == 0, f"churn produced unaccounted slots: {report['leaked']}"
assert report["clean"], f"not clean after churn: {report['errors']}"

# --- and the data is still right
s = db_snapshot(d)
assert sorted(snap_load(s, 1)) == [1, 2]
assert sorted(snap_load(s, 2)) == [5, 500, 5000, 50000]
assert sorted(snap_load(s, 4)) == sorted([5 + i * 89 for i in range(3000)])
assert snap_cardinality(s, 5) == 200
assert snap_cardinality(s, 3) == 60001 - 6 * 401

# --- the reported statistics have to be self-consistent
st = db_stats(d)
assert st["used_extents"] <= st["allocated_bytes"], "more used than allocated"
assert st["allocated_bytes"] > 0, "a checkpointed database must have allocated slabs"
assert st["slabs"] > 0
assert st["index_nodes_written"] > 0, "a checkpoint must write index nodes"
assert st["wal_syncs"] > 0, "commits must have been fsynced"
assert st["live_readers"] == 1, "exactly this scenario's one snapshot"

# --- and everything survives one more round trip
snap_release(s)
d = db_reopen(d)
s = db_snapshot(d)
assert sorted(snap_load(s, 4)) == sorted([5 + i * 89 for i in range(3000)])
assert db_fsck(d)["consistent"]
