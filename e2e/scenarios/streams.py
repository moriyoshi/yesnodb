# The lazy surface: expression structure, the planner, and the `ChunkStream`
# cursor contract. Python's own `set` is the oracle.
#
# # Why this exists
#
# `expr_equivalence.rs` is the M1 gate and checks that lazy evaluation equals
# eager evaluation on random expression shapes. It cannot check the *contract
# underneath* — `peek_prefix` being a lower bound rather than a promise, `seek`
# being monotone positioning, `next_cardinality` agreeing with `next_chunk` —
# because those are properties of the cursor, and a Rust test that drives the
# cursor by hand is testing the same code it is written against.
#
# A scenario can drive it from outside, one call at a time, and say what it
# expects in the language of Python sets.
#
# **Nothing here reimplements an operator.** That distinction is why
# `and_shape.py` and `aligned_eval.py` were moved out of this directory on
# 2026-08-26: they wrote k-way intersection and an aligned evaluator in Python,
# so their assertions were about the Python. Every assertion below is about
# something in `yesno-core`.

CHUNK_CARD = yn_const("CHUNK_CARD")


def stride_set(base, step, count):
    sb = sb_new()
    sb_stride(sb, base, step, count)
    return sb_build(sb)


def drain(st):
    # Walk a stream to exhaustion, returning `( chunks, ordinals )`.
    #
    # This is not a reimplementation of anything: it *is* the `ChunkStream`
    # contract, spelled out, which is what the assertions below are about.
    chunks = 0
    ordinals = 0
    prev = None
    while True:
        got = st_next_chunk(st)
        if got is None:
            break
        p, c = got
        assert prev is None or p > prev, f"prefixes must strictly ascend, got {p} after {prev}"
        assert ct_len(c) > 0, f"a stream must never yield an empty chunk at prefix {p}"
        prev = p
        chunks = chunks + 1
        ordinals = ordinals + ct_len(c)
    return (chunks, ordinals)


# ---------------------------------------------------------------- structure

pa = {1, 2, 3, 70000}
pb = {3, 4, 70000, 200000}
sa = set_of(sorted(pa))
sbb = set_of(sorted(pb))
ea = q_set(sa)
eb = q_set(sbb)

assert q_kind(ea) == "set"
assert q_arity(ea) == 0
assert q_kind(q_empty()) == "empty"
assert q_kind(q_range(0, 10)) == "range"
assert q_bounds(q_range(5, 40)) == (5, 40), "q_range is half-open [lo, hi)"

# `q_leaf_set` must hand back the operand, not a copy of some other set.
assert set_to_list(q_leaf_set(ea)) == sorted(pa)

for verb_name, node, want in [
    ("and", q_and(ea, eb), "and"),
    ("or", q_or(ea, eb), "or"),
    ("xor", q_xor(ea, eb), "xor"),
    ("andnot", q_andnot(ea, eb), "andnot"),
]:
    assert q_kind(node) == want
    assert q_arity(node) == 2
    # The children come back in order, and they are the operands that went in.
    assert set_to_list(q_leaf_set(q_child(node, 0))) == sorted(pa), verb_name
    assert set_to_list(q_leaf_set(q_child(node, 1))) == sorted(pb), verb_name

# Indexing past the arity is a scenario bug and must say so rather than
# returning something that reads as a leaf.
try:
    q_child(ea, 0)
    refused = False
except ValueError as e:
    refused = True
    print(f"  {e}")
assert refused, "a leaf has no children"

# `q_not_in` is a variant, not sugar for `AndNot( Range, x )`.
comp = q_not_in(ea, 0, 100)
assert q_kind(comp) == "not"
assert q_arity(comp) == 1
assert q_bounds(comp) == (0, 100)
assert q_kind(q_child(comp, 0)) == "set"


# ---------------------------------------------------------------- semantics

def check(label, node, want):
    # Three terminals, one oracle. `q_cardinality` is the non-materializing
    # walk, `q_collect` materializes to a list and `q_collect_set` to a set —
    # they are parallel implementations, and only comparing them can see one
    # decaying into the other.
    assert q_cardinality(node) == len(want), f"{label}: cardinality"
    assert q_collect(node) == sorted(want), f"{label}: collect"
    assert set_to_list(q_collect_set(node)) == sorted(want), f"{label}: collect_set"
    assert set_len(q_collect_set(node)) == q_cardinality(node), f"{label}: the two terminals disagree"


check("a AND b", q_and(ea, eb), pa & pb)
check("a OR b", q_or(ea, eb), pa | pb)
check("a XOR b", q_xor(ea, eb), pa ^ pb)
check("a ANDNOT b", q_andnot(ea, eb), pa - pb)
check("empty", q_empty(), set())
check("range", q_range(3, 9), set(range(3, 9)))
check("range AND a", q_and(q_range(0, 100), ea), {v for v in pa if v < 100})
check("not_in", q_not_in(ea, 0, 10), {v for v in range(10) if v not in pa})
# The complement of the complement, within the same window.
check("not(not a)", q_not_in(q_not_in(ea, 0, 10), 0, 10), {v for v in pa if v < 10})


# ---------------------------------------------------------------- the planner

# Planning must preserve semantics, whatever it rewrites to. Checked on shapes
# that do fire a rule and shapes that cannot.
lo = q_set(stride_set(0, 1 << 16, 20))
hi = q_set(stride_set(500 << 16, 1 << 16, 20))
mid = q_set(stride_set(10 << 16, 1 << 16, 20))

for label, node in [
    ("disjoint AND", q_and(lo, hi)),
    ("disjoint ANDNOT", q_andnot(lo, hi)),
    ("disjoint XOR", q_xor(lo, hi)),
    ("overlapping AND", q_and(lo, mid)),
    ("overlapping OR", q_or(lo, mid)),
    ("nested", q_and(q_or(lo, mid), q_andnot(hi, mid))),
]:
    planned = q_plan(node)
    assert q_cardinality(planned) == q_cardinality(node), f"{label}: planning changed the answer"
    assert q_collect(planned) == q_collect(node), f"{label}: planning changed the contents"
    # Planning is idempotent: a second pass over an already-planned tree must
    # not keep rewriting.
    assert q_repr(q_plan(planned)) == q_repr(planned), f"{label}: plan() is not idempotent"


# ---------------------------------------------------- the ChunkStream contract

# `st_open` lowers **without** planning, so this is the raw operator tree. That
# is deliberate: it is what lets a scenario compare planned against unplanned.
e = q_and(ea, eb)
st = st_open(e)

# `peek_prefix` reports without consuming.
first = st_peek_prefix(st)
assert first is not None
assert st_peek_prefix(st) == first, "peek_prefix must not advance the cursor"
p, c = st_next_chunk(st)
assert p == first, "next_chunk must yield the peeked prefix"
assert st_peek_prefix(st) != first or st_peek_prefix(st) is None, "next_chunk must advance"
st_release(st)

# `peek_prefix` is a **lower bound, not a promise**. For XOR and ANDNOT a
# prefix can be present on both sides and cancel to an empty chunk, which the
# stream then skips — so the peeked prefix may be below the one delivered.
# Asserting equality here would be asserting a stronger contract than the trait
# makes, and would fail the moment a cancelling chunk appeared.
cancel = q_xor(ea, ea)
st = st_open(cancel)
peeked = st_peek_prefix(st)
got = st_next_chunk(st)
if got is None:
    print(f"  XOR of a set with itself: peeked {peeked}, delivered nothing")
else:
    assert got[0] >= peeked, "peek_prefix must be a lower bound on what is delivered"
st_release(st)
assert q_cardinality(cancel) == 0, "a XOR b with a == b is empty"

# `seek` is monotone positioning: it never rewinds and never yields below the
# target. Driven over a wide operand so there is somewhere to seek to.
wide = q_set(stride_set(0, 1 << 16, 40))
st = st_open(wide)
st_seek(st, 10)
p, c = st_next_chunk(st)
assert p >= 10, f"seek(10) then next_chunk gave prefix {p}"
assert p == 10, "seek must land on the first prefix at or above the target"
# Seeking backwards must not rewind.
st_seek(st, 0)
p2, c2 = st_next_chunk(st)
assert p2 > p, f"seeking backwards rewound the cursor: {p} then {p2}"
# Seeking past everything exhausts it.
st_seek(st, 1 << 20)
assert st_next_chunk(st) is None, "seeking past the end must exhaust the stream"
assert st_peek_prefix(st) is None
st_release(st)

# `next_cardinality` is a *parallel implementation* of `next_chunk`: it exists
# to advance without handing back a payload. Only comparing the two can see it
# decay back into `next_chunk().len()` — or disagree.
for label, node in [
    ("set", wide),
    ("and", q_and(ea, eb)),
    ("or", q_or(ea, eb)),
    ("andnot", q_andnot(ea, eb)),
    ("range", q_range(0, 200000)),
    ("not_in", q_not_in(ea, 0, 300000)),
]:
    x = st_open(node)
    y = st_open(node)
    steps = 0
    while True:
        a1 = st_next_chunk(x)
        b1 = st_next_cardinality(y)
        if a1 is None:
            assert b1 is None, f"{label}: the counting advance outlived the materializing one"
            break
        assert b1 is not None, f"{label}: the counting advance ended early"
        assert a1[0] == b1[0], f"{label}: prefixes disagree at step {steps}"
        assert ct_len(a1[1]) == b1[1], f"{label}: cardinalities disagree at prefix {a1[0]}"
        steps = steps + 1
    assert steps > 0, f"{label}: the walk yielded nothing, so it proved nothing"
    st_release(x)
    st_release(y)

# `st_cardinality` consumes the stream and must equal both the chunk sum and the
# planned expression's answer — which is also a statement that planning
# preserves semantics, since `st_open` does not plan and `q_cardinality` does.
for label, node, want in [
    ("and", q_and(ea, eb), pa & pb),
    ("or", q_or(ea, eb), pa | pb),
    ("xor", q_xor(ea, eb), pa ^ pb),
    ("andnot", q_andnot(ea, eb), pa - pb),
]:
    st = st_open(node)
    chunks, ordinals = drain(st)
    st_release(st)
    assert ordinals == len(want), f"{label}: draining gave {ordinals}, want {len(want)}"

    st = st_open(node)
    assert st_cardinality(st) == len(want), f"{label}: st_cardinality"
    st_release(st)
    assert q_cardinality(node) == len(want), f"{label}: q_cardinality"

# A released stream must raise rather than resurrect a drained cursor.
st = st_open(ea)
st_release(st)
try:
    st_peek_prefix(st)
    refused = False
except ValueError as e:
    refused = True
    print(f"  {e}")
assert refused, "a released stream must not be usable"

print("  cursor contract, planner and terminals all agree with Python's set")
