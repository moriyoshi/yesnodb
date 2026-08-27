# Set algebra end to end, with Python's own `set` as the oracle.
#
# This is the scenario that most justifies scripting the harness rather than
# writing it in Rust: the expected answer is written in the language of sets
# (`sa & (sb | sc)`) instead of being recomputed by hand, so the assertion and
# the implementation cannot share a mistake.
#
# The operands are chosen to span container kinds. Multiples of 3 over 200k
# ordinals fill several array containers; the dense range packs bitmap and run
# containers; the sparse key lands one ordinal in each of many chunks, which is
# the case the leapfrog seek exists for.

d = db_open("main", shards=2)

a_vals = [i * 3 for i in range(20000)]
b_vals = [i * 5 for i in range(20000)]
c_vals = [i * 65537 for i in range(300)]

db_insert_many(d, 1, a_vals)
db_insert_many(d, 2, b_vals)
db_insert_many(d, 3, c_vals)
db_checkpoint(d)

sa = set(a_vals)
sb = set(b_vals)
sc = set(c_vals)

s = db_snapshot(d)
qa = q_key(s, 1)
qb = q_key(s, 2)
qc = q_key(s, 3)

# --- the four binary kernels against the oracle
assert sorted(q_collect(q_and(qa, qb))) == sorted(sa & sb)
assert sorted(q_collect(q_or(qa, qb))) == sorted(sa | sb)
assert sorted(q_collect(q_xor(qa, qb))) == sorted(sa ^ sb)
assert sorted(q_collect(q_andnot(qa, qb))) == sorted(sa - sb)

# ANDNOT is the one that is not commutative, so it needs both directions.
assert sorted(q_collect(q_andnot(qb, qa))) == sorted(sb - sa)

# --- a nested expression: a AND (b OR c)
nested = q_and(qa, q_or(qb, qc))
assert sorted(q_collect(nested)) == sorted(sa & (sb | sc))

# --- the cardinality identities
#
# `cardinality()` is a separate, non-materializing implementation of the same
# question. Only comparing it against the materialized answer stops it decaying
# into `len(collect())`, which is precisely how it would silently regress.
assert q_cardinality(q_and(qa, qb)) == len(sa & sb)
assert q_cardinality(q_or(qa, qb)) == len(sa | sb)
assert q_cardinality(q_xor(qa, qb)) == len(sa ^ sb)
assert q_cardinality(q_andnot(qa, qb)) == len(sa - sb)
assert q_cardinality(nested) == len(q_collect(nested))

# The sparse-against-dense intersection: one ordinal per chunk against a dense
# operand is the shape the design is built around.
assert q_cardinality(q_and(qc, qa)) == len(sc & sa)

# --- the guard against a vacuous run
#
# Every assertion above would also hold if all three keys were empty. These
# make that impossible.
assert len(sa & sb) > 0, "the operands must actually overlap"
assert len(sa - sb) > 0, "ANDNOT must have something to remove"
assert len(sa ^ sb) > 0, "XOR must be non-trivial"
assert len(sc & sa) > 0, "the sparse key must intersect the dense one"

# --- the empty and range literals
assert q_collect(q_empty()) == []
assert q_cardinality(q_and(qa, q_empty())) == 0

# `q_range` is half-open `[lo, hi)` — see ranges.py, which pins that against
# `db_insert_range`'s inclusive `[lo, hi]`.
assert q_cardinality(q_range(0, 10)) == 10
assert sorted(q_collect(q_and(qa, q_range(0, 10)))) == sorted(sa & set(range(0, 10)))
