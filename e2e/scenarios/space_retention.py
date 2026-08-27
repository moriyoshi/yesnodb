# What a long-lived reader costs, and that the cost is given back.
#
# This is the design's largest known open gap, so it is worth an end-to-end
# statement of exactly where things stand. A snapshot held across a churn
# workload retains superseded extents, and **nothing bounds that** — the
# design's 2x limit is meant to come from aborting the oldest reader, which
# requires every `Snapshot` read method to become fallible, which has not
# happened. The bound is therefore a design promise the mechanism supports and
# nothing enforces -- which is the distinction this scenario exists to keep.
#
# What *is* implemented is the mechanism underneath: retention while a reader
# lives, release once it is gone. Both directions are asserted here, because a
# test that only checked retention would also pass if the space were never
# released at all.

d = db_open("main", shards=1)

db_insert_range(d, 1, 0, 400000)
db_checkpoint(d)

reader = db_snapshot(d)
base = db_stats(d)["deferred_bytes"]

# Churn: rewrite the same key repeatedly. Each checkpoint supersedes the extents
# the reader still holds, so they cannot be reclaimed.
for i in range(6):
    lo = 500000 + i * 100000
    db_insert_range(d, 1, lo, lo + 50000)
    db_remove_range(d, 1, i * 1000, i * 1000 + 500)
    db_checkpoint(d)

held = db_stats(d)["deferred_bytes"]
assert held > base, "a live reader must retain superseded extents"
assert db_stats(d)["live_readers"] == 1

# The reader still reads what it was promised, which is the point of retaining.
assert snap_cardinality(reader, 1) == 400001
assert snap_contains(reader, 1, 0), "an ordinal removed after the snapshot"
assert snap_contains(reader, 1, 500000) is False, "an ordinal added after the snapshot"

# --- and the space comes back
snap_release(reader)
assert db_stats(d)["live_readers"] == 0

# Two checkpoints: reclamation additionally waits for the A/B superblock rule,
# so the extents freed by the checkpoint that superseded them are not eligible
# until a later one.
db_checkpoint(d)
db_checkpoint(d)
db_checkpoint(d)

after = db_stats(d)["deferred_bytes"]
assert after < held, "releasing the reader must return the retained space"

# `space_amplification` is the number an operator would watch. It is reported,
# not enforced — that is the gap named at the top of this file.
amp = db_stats(d)["space_amplification"]
assert amp > 0.0, "space amplification must be reported"

# The database is still correct after all of that.
#
# `consistent` is the strong form: no dangling reference, and no packed page
# whose live-byte count disagrees with the index. This workload churns *range*
# data, so its containers are runs — which is what makes it the scenario that
# found the packed-run accounting leak, where superseding a packed run returned
# zero bytes to its page and the page could never be reclaimed.
report = db_fsck(d)
assert report["consistent"], f"the store is inconsistent after reclamation: {report['errors']}"
assert report["chunks"] > 0, "an fsck that walked no chunks proves nothing"
# The strong form: nothing unaccounted for. Space still waiting out the
# reclamation conditions is `pending`, which is retention rather than a leak.
assert report["leaked"] == 0, f"unaccounted slots after churn: {report['leaked']}"
assert report["clean"], f"not clean after reclamation: {report['errors']}"
s = db_snapshot(d)
assert snap_contains(s, 1, 500000), "the churned-in data is present"
assert snap_contains(s, 1, 0) is False, "the churned-out data is gone"
