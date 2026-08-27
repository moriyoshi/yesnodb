# The eager surface: `OrdSet` construction, the chunk layout underneath it, and
# the container kernels. Python's own `set` is the oracle throughout.
#
# # Why this exists
#
# The `sb_*` / `set_*` / `ct_*` / `ops_*` verbs were added on 2026-08-26 so the
# measurement fixtures could build operands without a database. Two of those
# fixtures then turned out not to be tests at all — they reimplemented an
# algorithm in Python and asserted things about *that* — and moving them out
# left most of this surface with no caller in the suite.
#
# A verb nothing calls is the failure mode this project keeps hitting, one
# step removed: it compiles, it looks like coverage, and nothing would notice it
# breaking. So the surface is covered here directly, and the difference from the
# fixtures that were moved is the whole point — **nothing below reimplements
# yesno**. Every assertion compares a yesno answer against Python's `set`, which
# is an oracle rather than a second implementation of the thing under test.

CHUNK_CARD = yn_const("CHUNK_CARD")
ARRAY_MAX = yn_const("ARRAY_MAX")


def build(vals):
    sb = sb_new()
    sb_values(sb, vals)
    return sb_build(sb)


# ---------------------------------------------------------------- construction

# The two constructors must agree, including on duplicates and on order: a
# builder accumulates and `from_iter_unsorted` sorts and dedups, so handing it
# the same values backwards and twice must not change the set.
py = {5, 1, 9, 1, 65540, 5}
a = set_of([5, 1, 9, 1, 65540, 5])
b = build([65540, 5, 9, 5, 1, 1])
assert set_to_list(a) == sorted(py)
assert set_to_list(b) == sorted(py)
assert set_len(a) == len(py)

# `sb_stride` is an arithmetic progression, and the arithmetic must be exact —
# every fixture operand is built from it.
sb = sb_new()
sb_stride(sb, 7, 13, 5)
assert set_to_list(sb_build(sb)) == [7, 20, 33, 46, 59]

empty = set_of([])
assert set_is_empty(empty)
assert set_len(empty) == 0
assert set_min(empty) is None
assert set_max(empty) is None
assert set_chunk_count(empty) == 0


# ---------------------------------------------------------------- scalars

assert not set_is_empty(a)
assert set_min(a) == min(py)
assert set_max(a) == max(py)
for v in [0, 1, 2, 5, 9, 65539, 65540, 65541]:
    assert set_contains(a, v) == (v in py), f"contains({v})"

# `rank` is strictly-less-than and `select` is zero-based, so the two invert.
# Mirrored from `OrdSet` without adjustment, which is why it is worth pinning:
# an off-by-one in either would still round-trip against itself.
srt = sorted(py)
for i in range(len(srt)):
    assert set_select(a, i) == srt[i], f"select({i})"
    assert set_rank(a, srt[i]) == i, f"rank({srt[i]})"
assert set_select(a, len(srt)) is None, "select past the end must be None"
assert set_rank(a, max(py) + 1) == len(srt), "rank above the maximum counts everything"
assert set_rank(a, 0) == 0


# ---------------------------------------------------------------- set algebra

pa = {1, 2, 3, 4, 5, 70000, 70001}
pb = {4, 5, 6, 70001, 200000}
sa = set_of(sorted(pa))
sbb = set_of(sorted(pb))

assert set_to_list(set_and(sa, sbb)) == sorted(pa & pb)
assert set_to_list(set_or(sa, sbb)) == sorted(pa | pb)
assert set_to_list(set_xor(sa, sbb)) == sorted(pa ^ pb)
assert set_to_list(set_andnot(sa, sbb)) == sorted(pa - pb)
assert set_to_list(set_andnot(sbb, sa)) == sorted(pb - pa)
assert set_to_list(set_union_all([sa, sbb, empty])) == sorted(pa | pb)

# Operands are not consumed: yesno's set algebra is by-reference, and a verb
# that moved its arguments would make every fixture that reuses an operand
# quietly wrong.
assert set_to_list(sa) == sorted(pa)
assert set_to_list(sbb) == sorted(pb)

# The identities, which cost nothing to check and catch a kernel that is right
# on the cases a hand-picked corpus happens to include.
assert set_len(set_xor(sa, sa)) == 0
assert set_len(set_andnot(sa, sa)) == 0
assert set_to_list(set_and(sa, sa)) == sorted(pa)
assert set_to_list(set_or(sa, empty)) == sorted(pa)
assert set_len(set_and(sa, empty)) == 0
# |a| + |b| = |a & b| + |a | b|, over three container kinds at once.
assert set_len(sa) + set_len(sbb) == set_len(set_and(sa, sbb)) + set_len(set_or(sa, sbb))


# ---------------------------------------------------------------- chunk layout

# A corpus deliberately spanning every container kind, so the walk below is not
# a test of arrays three times over.
sb = sb_new()
sb_values(sb, [1, 5, 9, 65535])              # prefix 0: sparse   -> array
sb_stride(sb, (1 << 16), 1, 2001)            # prefix 1: a run    -> run
sb_stride(sb, (2 << 16), 2, 5000)            # prefix 2: dense    -> bitmap
sb_stride(sb, (3 << 16), 1, CHUNK_CARD)      # prefix 3: 1-filled -> full
big = sb_build(sb)

n_chunks = set_chunk_count(big)
assert n_chunks == 4, f"expected one chunk per prefix, got {n_chunks}"

total = 0
prev = None
prefixes = []
kinds = {}
for i in range(n_chunks):
    p, c = set_chunk_at(big, i)
    prefixes.append(p)
    assert set_prefix_at(big, i) == p, "prefix_at and chunk_at disagree"
    assert prev is None or p > prev, "prefixes must strictly ascend"
    prev = p
    # Invariant: a stored chunk is never empty. An empty one would be invisible
    # to cardinality and fatal to every seek-driven operator.
    assert ct_len(c) > 0, f"chunk {i} is empty"
    assert ct_kind(c) in ("array", "bitmap", "run"), ct_kind(c)
    kinds[p] = (ct_kind(c), ct_len(c), ct_is_full(c))
    total = total + ct_len(c)

# Cardinality is the sum of the chunks. This is the identity the whole
# non-materializing cardinality path rests on.
assert total == set_len(big), f"chunks sum to {total}, set says {set_len(big)}"
assert set_chunk_at(big, n_chunks) is None, "past the end must be None"
assert set_prefix_at(big, n_chunks) is None

print(f"  container mix: {kinds}")
assert kinds[0][1] == 4
assert kinds[3][1] == CHUNK_CARD, "a 1-filled chunk holds every ordinal"
assert kinds[3][2], "a 1-filled chunk must report itself full"
assert not kinds[0][2], "a 4-element chunk is not full"
# The size classes really are distinct, or the walk above proved less than it
# looks. Asserted as a *set* of kinds, not per prefix: which encoding a given
# shape lands in is `optimize()`'s decision and may change.
assert len({kinds[p][0] for p in kinds}) >= 2, f"the corpus reached only one kind: {kinds}"


# ---------------------------------------------------------- partition_point_in

# The definition: the number of prefixes in `[lo, hi)` strictly below `target`,
# as an offset relative to `lo`. Checked over every sub-range against a linear
# count, which is the definition rather than a second binary search.
for target in [0, 1, 2, 3, 4, 99]:
    for lo in range(n_chunks + 1):
        for hi in range(lo, n_chunks + 1):
            want = len([p for p in prefixes[lo:hi] if p < target])
            got = set_partition_point_in(big, lo, hi, target)
            assert got == want, f"partition_point_in({lo}, {hi}, {target}) = {got}, want {want}"


# ---------------------------------------------------------------- the kernels

def card(c):
    # `None` is how an empty result is reported, and telling it from a
    # zero-length container is the distinction every early-out branches on.
    return 0 if c is None else ct_len(c)


ka = {1, 2, 3, 4, 5, 6}
kb = {4, 5, 6, 7, 8}
ca = set_chunk_at(set_of(sorted(ka)), 0)[1]
cb = set_chunk_at(set_of(sorted(kb)), 0)[1]

assert card(ops_and(ca, cb)) == len(ka & kb)
assert card(ops_or(ca, cb)) == len(ka | kb)
assert card(ops_xor(ca, cb)) == len(ka ^ kb)
assert card(ops_andnot(ca, cb)) == len(ka - kb)
assert card(ops_andnot(cb, ca)) == len(kb - ka)

# An empty result is `None`, never a zero-length container.
odd = set_chunk_at(set_of([1, 3, 5]), 0)[1]
even = set_chunk_at(set_of([2, 4, 6]), 0)[1]
assert ops_and(odd, even) is None, "a disjoint intersection must be None"
assert ops_andnot(odd, odd) is None
assert ops_xor(odd, odd) is None
assert ops_or(odd, even) is not None

# `ct_full` is the all-ones container, and complementing through it must give
# exactly the ordinals that were absent.
f = ct_full()
assert ct_is_full(f)
assert ct_len(f) == CHUNK_CARD
assert card(ops_andnot(f, ca)) == CHUNK_CARD - len(ka)
assert card(ops_and(f, ca)) == len(ka)
assert card(ops_or(f, ca)) == CHUNK_CARD
assert ops_andnot(f, f) is None


# ------------------------------------------------------- storing a built set

# `batch_store_set` replaces a key's whole contents with a set the scenario
# built.
d = db_open("stored")

# `db_is_durable` does **not** mean "the committed data is on disk", which is
# what the name reads as. `Db::is_durable` is
# `shards.first().store.is_some()` — whether this database is backed by a store
# at all — so it is a property of how it was *opened* and is constant across
# commits and checkpoints. Pinned here in that form deliberately: asserting it
# flips at a checkpoint looks like a durability test, passes for the wrong
# reason, and would be testing nothing. See `is-durable-reads-as-a-claim` in
# JOURNAL.md ( closed; folded there 2026-09-08 ). Actual durability is what `wal_size.py` and `durability.py` check,
# by reopening and reading the data back.
fresh = db_is_durable(d)
assert fresh, "a file-backed database reports a store before anything is written"

wb = batch(d)
batch_store_set(wb, 7, big)
committed = batch_commit(wb)
assert committed["changed"] > 0, "storing a set changed nothing"
assert db_is_durable(d) == fresh, "is_durable is not supposed to move on commit"
db_checkpoint(d)
assert db_is_durable(d) == fresh, "is_durable is not supposed to move on checkpoint"

# Reopened before reading. `checkpoint()` does not clear the memtable, so a
# read on the same instance is answered from memory and never reaches the store
# — the blind spot that hid three separate bugs in this codebase.
d = db_reopen(d)
s = db_snapshot(d)
assert snap_cardinality(s, 7) == set_len(big)
back = snap_load_set(s, 7)
assert set_len(set_xor(back, big)) == 0, "the stored set did not survive the round trip"
# Storing *replaces*, so a second store of a smaller set must not union.
snap_release(s)
wb = batch(d)
batch_store_set(wb, 7, sa)
batch_commit(wb)
s = db_snapshot(d)
assert snap_cardinality(s, 7) == len(pa), "store_set must replace, not merge"
snap_release(s)
db_close(d)


# ------------------------------------------------------- handles are typed

# A handle is an index into one table, and passing it to a verb expecting
# another kind must raise. This was not true until 2026-08-26: every table
# started at 0, so `q_cardinality( a_set_handle )` silently answered about a
# different object.
q = q_set(sa)
try:
    set_len(q)
    typed = False
except ValueError as e:
    typed = True
    print(f"  {e}")
assert typed, "a query handle must not resolve as a set"

try:
    ct_len(sa)
    typed = False
except ValueError:
    typed = True
assert typed, "a set handle must not resolve as a container"

# And a builder is consumed by `sb_build`, so using it again is an error rather
# than a second identical set.
sb2 = sb_new()
sb_values(sb2, [1, 2])
sb_build(sb2)
try:
    sb_build(sb2)
    consumed = False
except ValueError:
    consumed = True
assert consumed, "a builder must not build twice"
