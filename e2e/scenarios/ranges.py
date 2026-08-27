# Ranges, chunk boundaries, and the full u64 address space.
#
# Two things are pinned here that nothing else pins end to end.
#
# 1. **The two range conventions disagree, and that is the library's.**
#    `db_insert_range(lo, hi)` is inclusive `[lo, hi]`; `q_range(lo, hi)` is
#    half-open `[lo, hi)`, because it mirrors `Expr::Range`. The harness
#    reflects both faithfully rather than smoothing them together, so this file
#    is where the discrepancy is written down and will be caught if either side
#    changes.
#
# 2. **Ordinals above `i64::MAX` survive the round trip.** That is half the
#    address space, it does not fit a Python `int` in monty's fast arm, and it
#    is exactly the half a 48-bit-prefix design gets wrong if a shift is signed.

d = db_open("main")

# --- inclusive on both ends
n = db_insert_range(d, 1, 100, 200)
assert n == 101, "db_insert_range is inclusive, so [100, 200] is 101 ordinals"

s = db_snapshot(d)
assert snap_contains(s, 1, 100), "the low bound is included"
assert snap_contains(s, 1, 200), "the high bound is included"
assert snap_contains(s, 1, 99) is False
assert snap_contains(s, 1, 201) is False
assert snap_min(s, 1) == 100
assert snap_max(s, 1) == 200
assert snap_cardinality(s, 1) == 101

# A single-ordinal range is a range.
assert db_insert_range(d, 2, 42, 42) == 1

# --- the half-open query literal, side by side with the inclusive writer
snap_release(s)
assert q_cardinality(q_range(100, 201)) == 101, "q_range is half-open [lo, hi)"
assert q_cardinality(q_range(5, 5)) == 0, "an empty half-open range yields nothing"
assert q_cardinality(q_range(9, 5)) == 0, "an inverted range yields nothing"

# The two conventions must describe the same set when translated.
s = db_snapshot(d)
q_written = q_key(s, 1)
q_literal = q_range(100, 201)
assert q_cardinality(q_xor(q_written, q_literal)) == 0, "the two ranges must agree"

# --- crossing chunk boundaries
#
# A chunk is 65 536 ordinals. A range spanning three of them exercises the
# partial-head, whole-body, partial-tail split rather than one container.
snap_release(s)
db_insert_range(d, 3, 65000, 200000)
s = db_snapshot(d)
assert snap_cardinality(s, 3) == 200000 - 65000 + 1
assert snap_contains(s, 3, 65535), "the last ordinal of chunk 0"
assert snap_contains(s, 3, 65536), "the first ordinal of chunk 1"
assert snap_contains(s, 3, 131072), "the first ordinal of chunk 2"

# --- removal from the middle, leaving both halves
snap_release(s)
db_remove_range(d, 3, 70000, 150000)
s = db_snapshot(d)
assert snap_cardinality(s, 3) == (200000 - 65000 + 1) - (150000 - 70000 + 1)
assert snap_contains(s, 3, 69999)
assert snap_contains(s, 3, 70000) is False
assert snap_contains(s, 3, 150000) is False
assert snap_contains(s, 3, 150001)

# --- the top of the ordinal range
#
# Invariant I8 reserves u64::MAX, so the largest storable ordinal is 2^64-2.
# That is what makes every cardinality fit a u64 and makes an unbounded
# complement expressible.
U64_MAX = 18446744073709551615
ORDINAL_MAX = U64_MAX - 1
I64_MAX = 9223372036854775807

snap_release(s)
big = [I64_MAX, I64_MAX + 1, ORDINAL_MAX - 1, ORDINAL_MAX]
db_insert_many(d, 9, big)
db_checkpoint(d)
d = db_reopen(d)

s = db_snapshot(d)
assert sorted(snap_load(s, 9)) == sorted(big), "a u64 ordinal did not survive"
assert snap_max(s, 9) == ORDINAL_MAX, "the maximum ordinal must be representable"
assert snap_min(s, 9) == I64_MAX
assert snap_contains(s, 9, ORDINAL_MAX)
assert snap_cardinality(s, 9) == 4

# u64::MAX is not an ordinal. Every mutating entry point must refuse it, and
# must refuse it without partially applying the batch.
for call in ("insert", "insert_many", "insert_range"):
    refused = False
    try:
        if call == "insert":
            db_insert(d, 11, U64_MAX)
        elif call == "insert_many":
            db_insert_many(d, 11, [1, U64_MAX])
        else:
            db_insert_range(d, 11, ORDINAL_MAX, U64_MAX)
    except RuntimeError:
        refused = True
    assert refused, "db_" + call + " accepted u64::MAX as an ordinal"

snap_release(s)
s = db_snapshot(d)
assert snap_cardinality(s, 11) == 0, "a rejected write must not partially apply"

# A key above i64::MAX, not just an ordinal — keys are hashed into shards, and
# a signed shift there would fold the top half onto the bottom.
db_insert(d, U64_MAX, 7)
db_insert(d, I64_MAX, 8)
snap_release(s)
s = db_snapshot(d)
assert snap_load(s, U64_MAX) == [7]
assert snap_load(s, I64_MAX) == [8]
assert snap_is_empty(s, U64_MAX - 1), "distinct large keys must not collide"

# --- rank and select over a range
snap_release(s)
db_insert_range(d, 4, 1000, 1099)
s = db_snapshot(d)
# `rank` is *strictly less than*, and `select` is zero-based. Those two
# conventions have to agree or the pair is not invertible, which is the
# property worth asserting — the magic numbers alone would pass with both
# sides off by one in the same direction.
assert snap_rank(s, 4, 1000) == 0, "rank counts ordinals strictly below"
assert snap_rank(s, 4, 1050) == 50
assert snap_rank(s, 4, 5000) == 100, "rank past the end is the cardinality"
assert snap_select(s, 4, 0) == 1000, "select is zero-based"
assert snap_select(s, 4, 99) == 1099
assert snap_select(s, 4, 100) is None, "selecting past the end yields nothing"

for n in range(100):
    assert snap_rank(s, 4, snap_select(s, 4, n)) == n, "rank and select must invert"
