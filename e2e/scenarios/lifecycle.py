# Lifecycle: what a handle means, and what the exclusive lock actually
# guarantees.
#
# The lock is the operational question an embedded database gets asked most
# often, and it is the one this project enforced last: for every milestone up
# to M7 two `Db` handles could open one directory, which is two writers over
# one mmap. This scenario is the end-to-end statement of the rule.

d = db_open("main", shards=4)
assert db_stats(d)["shards"] == 4, "shards= must reach DbOptions, not be ignored"

db_insert(d, 10, 100)
db_checkpoint(d)
epoch_before = db_epoch(d)

# A second open of the same directory must be refused while the first is live.
refused = False
try:
    db_open("main")
except RuntimeError:
    refused = True
assert refused, "a second open of a live database must be refused by the file lock"

# A different directory is unaffected — the lock is per-database, not global.
other = db_open("other")
db_insert(other, 10, 999)
db_close(other)

# Reopening mints a new handle and kills the old one. A stale handle must
# raise: silently reusing the closed `Db` would hide a use-after-close.
d2 = db_reopen(d)
assert d2 != d
stale = False
try:
    db_epoch(d)
except ValueError:
    stale = True
assert stale, "the pre-reopen handle must be dead, not merely discouraged"

# The fencing epoch is read-incremented-written in LOCK on every open, which is
# how a fenced-off previous process is recognised.
assert db_epoch(d2) > epoch_before, "each open must advance the fencing epoch"

s = db_snapshot(d2)
assert snap_contains(s, 10, 100)

# A snapshot pins the store, and therefore the lock. Closing underneath one
# would leave the *next* open failing with a lock error far from the cause.
held = False
try:
    db_close(d2)
except ValueError:
    held = True
assert held, "closing under a live snapshot must be refused"

snap_release(s)
db_close(d2)

# Released, the directory is free again — and the data is still there.
d3 = db_open("main")
s3 = db_snapshot(d3)
assert snap_cardinality(s3, 10) == 1
assert snap_load(s3, 10) == [100]

# The two databases stayed separate throughout.
assert snap_contains(s3, 10, 999) is False, "'other' must not leak into 'main'"
