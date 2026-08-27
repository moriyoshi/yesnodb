# Views: several sets packed into one ordinal space, across a real database.
#
# What this file is for is the *operational* sequence, not the algebra. The
# crate's own tests diff every specialised arm against its generic path and
# assert the algebraic laws; none of them ever puts a packed view on disk.
# Here it is packed, stored, checkpointed, REOPENED, and only then taken apart —
# a read on a live database is answered from the memtable and never reaches the
# store, which is the blind spot that hid three bugs in the matrix module.
#
# Python's own `set` is the oracle throughout, which is the whole reason these
# live here rather than as Rust tests.

# Four constituents with deliberately different shapes: empty, singleton,
# scattered, and a dense run.
parts = [
    set(),
    {7},
    {i * 13 for i in range(300)},
    set(range(500, 2000)),
]
handles = [set_of(sorted(p)) for p in parts]

# ── Both layouts hold the same contents ──────────────────────────────────────
# They are transposes of each other, so this is a real cross-check: select is a
# strided filter under one and a range window under the other.
for v in [vw_interleaved(len(parts)), vw_blocked(len(parts), 4096)]:
    assert vw_sets(v) == len(parts)
    packed = vw_pack(v, handles)

    # The packing is an injection, so no slot is shared.
    assert set_len(packed) == sum(len(p) for p in parts)

    for i, want in enumerate(parts):
        got = vw_select(v, packed, i)
        assert set_to_list(got) == sorted(want)
        assert vw_cardinality(v, packed, i) == len(want)
        for x in [0, 7, 13, 500, 1999, 2000]:
            assert vw_contains(v, packed, i, x) == (x in want)

    # ── Folding across constituents ─────────────────────────────────────────
    union = parts[0] | parts[1] | parts[2] | parts[3]
    inter = parts[0] & parts[1] & parts[2] & parts[3]
    parity = set()
    for p in parts:
        parity ^= p
    assert set_to_list(vw_fold(v, packed, "any")) == sorted(union)
    assert set_to_list(vw_fold(v, packed, "all")) == sorted(inter)
    assert set_to_list(vw_fold(v, packed, "parity")) == sorted(parity)

    # Expanding then folding is the identity: every constituent gets the bit.
    coarse = set_of([1, 2, 900])
    assert set_to_list(vw_fold(v, vw_expand(v, coarse), "any")) == [1, 2, 900]
    assert set_to_list(vw_fold(v, vw_expand(v, coarse), "all")) == [1, 2, 900]

# ── The operational sequence: does a view survive the store? ─────────────────
# This is what no Rust test does.
v = vw_interleaved(len(parts))
packed = vw_pack(v, handles)

db = db_open()
db_insert_set(db, 42, packed)
db_checkpoint(db)
db = db_reopen(db)

snap = db_snapshot(db)
loaded = snap_load_set(snap, 42)

# Byte-for-byte the same set came back, and the view still reads it.
assert set_len(loaded) == set_len(packed)
for i, want in enumerate(parts):
    assert set_to_list(vw_select(v, loaded, i)) == sorted(want)
    assert vw_cardinality(v, loaded, i) == len(want)

union = parts[0] | parts[1] | parts[2] | parts[3]
assert set_to_list(vw_fold(v, loaded, "any")) == sorted(union)

snap_release(snap)
db_close(db)

# ── A blocked constituent's stride is a real capacity bound ──────────────────
# The first thing a caller trips over, so it is pinned here rather than left to
# be discovered.
narrow = vw_blocked(2, 100)
try:
    vw_pack(narrow, [set_of([5, 100]), set_of([])])
    raise AssertionError("an ordinal at the stride must be refused")
except ValueError:
    pass

# An unknown reduction is an error, not a silently different fold.
ok = vw_pack(vw_interleaved(1), [set_of([1])])
try:
    vw_fold(vw_interleaved(1), ok, "sum")
    raise AssertionError("an unknown reduction must be refused")
except ValueError:
    pass

print("view: both layouts, the fold, and a reopen all agree with Python's set")
