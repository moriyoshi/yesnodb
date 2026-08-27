# The Arrow surface, over data that came out of a real database.
#
# # The question this asks that a Rust test cannot
#
# `is_zero_copy` is true only for a **bitmap** container, and whether a chunk is
# a bitmap is decided by how many ordinals were written into it and in what
# pattern. So "can this posting list lend its bits to Arrow for free" is a
# property of the *write history*, not of the API.
#
# And it is decided **twice**: once by size-class promotion while the chunk is
# in the memtable, and again by the store codec when the chunk is read back after
# a checkpoint and reopen. Those are two implementations of one decision. A test
# that builds an `OrdSet` in memory and asserts on it never crosses the second.
#
# So the shape here is: write three keys of deliberately different density,
# assert what each one is, then **checkpoint, close, reopen** and assert the
# answers did not move.
#
#   yesno-e2e --show-output --arg n=200 arrow_surface.py

CHUNK = 65536
N = yn_arg("n", 10)          # chunks per key

db = db_open("arrow", shards=2)

# key 1 — two ordinals per chunk. Far below the array/bitmap threshold, so it
#         stays a sorted array and the mask path has to build the bits.
sparse = set()
for c in range(N):
    for o in [c * CHUNK, c * CHUNK + 7]:
        sparse.add(o)

# key 2 — every ordinal in each chunk. A solid run, which is neither an array
#         nor a bitmap, and is the case that makes "dense implies zero copy"
#         false.
solid = set()
for c in range(N):
    for o in range(c * CHUNK, c * CHUNK + CHUNK):
        solid.add(o)

# key 3 — every third ordinal. Dense enough to be a bitmap and scattered enough
#         not to collapse into runs.
speckled = set()
for c in range(N):
    for o in range(c * CHUNK, c * CHUNK + CHUNK, 3):
        speckled.add(o)

db_insert_many(db, 1, sorted(sparse))
db_insert_many(db, 2, sorted(solid))
db_insert_many(db, 3, sorted(speckled))


def kinds_of(snap, key):
    return set(c["kind"] for c in ar_kinds(snap, key))


def zero_copy_of(snap, key):
    return set(c["zero_copy"] for c in ar_kinds(snap, key))


def check(snap, when):
    # ---- contents, through every path, against Python's own set
    for key, want in [(1, sparse), (2, solid), (3, speckled)]:
        assert snap_load(snap, key) == sorted(want), (when, key)
        assert ar_mask_ordinals(snap, key) == sorted(want), (
            "the mask path selected a different set " + when
        )
        assert ar_batch_ordinals(snap, key) == sorted(want), (
            "the batch path returned a different set " + when
        )
        got = ar_containers(snap, key)
        assert got["ordinals"] == sorted(want), (
            "the container wire format did not round trip " + when
        )
        assert got["chunks"] == N, (when, key, got["chunks"])

    # ---- and the shapes, which are the point
    #
    # Asserted as exact sets, not with `in`. `"bitmap" in kinds` would pass on
    # a key that was half bitmap and half something else, which is precisely the
    # ambiguity that makes a shape assertion worthless.
    assert kinds_of(snap, 1) == {"array"}, (when, kinds_of(snap, 1))
    assert kinds_of(snap, 2) == {"run"}, (when, kinds_of(snap, 2))
    assert kinds_of(snap, 3) == {"bitmap"}, (when, kinds_of(snap, 3))

    # Only the bitmap lends its bits. The run is *denser* than the bitmap key
    # and still cannot — which is why a planner has to ask `is_zero_copy` rather
    # than reason from cardinality.
    assert zero_copy_of(snap, 1) == {False}, when
    assert zero_copy_of(snap, 2) == {False}, when
    assert zero_copy_of(snap, 3) == {True}, when


# ---- while it is all still in the memtable
s = db_snapshot(db)
check(s, "in memory")

# The mask covers a whole chunk regardless of how much of it is set: that is
# what makes it usable as a row filter without an offset calculation.
masks = ar_masks(s, 3)
assert len(masks) == N
assert set(m["bits"] for m in masks) == {CHUNK}
assert sum(m["selected"] for m in masks) == len(speckled)
assert masks[0]["base"] == 0
assert masks[1]["base"] == CHUNK

# A large key arrives as several batches rather than one allocation.
rows = ar_batches(s, 2)
assert len(rows) > 1, "a whole-chunk key must batch, not arrive in one piece"
assert sum(rows) == len(solid)
assert max(rows) <= 8192, rows

# `dense` emits a mask for every chunk in the span, including ones the set
# never touches — the shape a scan over a contiguous ordinal space wants. Here
# the span is fully populated, so the two agree; a gap is what separates them.
assert len(ar_masks(s, 1, "dense")) == N
snap_release(s)

# ---- a gap, so `dense` and `sparse` genuinely differ
db_insert_many(db, 4, [0, 1, 500 * CHUNK])
s = db_snapshot(db)
assert len(ar_masks(s, 4, "sparse")) == 2, "two chunks are touched"
assert len(ar_masks(s, 4, "dense")) == 501, "every chunk in the span gets a mask"
assert ar_mask_ordinals(s, 4, "dense") == [0, 1, 500 * CHUNK], (
    "the empty chunks contributed ordinals"
)
snap_release(s)

# ---- now through the store
#
# Checkpoint, close, reopen. Every read below is answered by the store's codec
# rather than by the memtable, and the shape decision is made a second time.
assert db_checkpoint(db) > 0
db_close(db)

db = db_open("arrow", shards=2)
s = db_snapshot(db)
check(s, "after a reopen")
snap_release(s)
db_close(db)
