# GF(2) inversion and rank over a bit matrix, through the durable path.
#
# Every expectation here is a literal. The temptation is to write a Gauss-Jordan
# in Python and compare — that would be testing the Python, which is exactly why
# `and_shape.py` is not in this directory. Small matrices whose inverse can be
# written down by hand are a stronger oracle and a shorter file.

d = db_open("main", shards=1)

N = 3

# L is unit lower triangular, so it is invertible and its inverse is easy to
# state:  L = 1 0 0     L^-1 = 1 0 0
#             1 1 0            1 1 0
#             1 1 1            0 1 1
# ( over GF(2): row 2 of L^-1 is row2 + row1 because 1+1 = 0 )
L_ROWS = [[0], [0, 1], [0, 1, 2]]
L_INV_ROWS = [[0], [0, 1], [1, 2]]


def build(rows, n):
    m = mx_zeros(n, n)
    for r in range(n):
        for c in rows[r]:
            m = mx_put(m, r, c, 1)
    return m


el = build(L_ROWS, N)
assert mx_shape(el) == [3, 3]
assert mx_rank(el) == N, "unit lower triangular must be full rank"

inv = mx_invert(el)
assert inv is not None, "L is invertible"
for r in range(N):
    assert mx_row_bits(inv, r) == L_INV_ROWS[r], f"row {r} of L^-1"

# The defining property, through the module's own product.
prod = mx_mul(el, inv, "gf2")
ident = mx_identity(N)
for r in range(N):
    assert mx_row_bits(prod, r) == mx_row_bits(ident, r), f"L * L^-1 row {r}"
assert mx_row_bits(mx_mul(inv, el, "gf2"), 2) == [2], "L^-1 * L row 2"

# Inversion is an involution.
back = mx_invert(inv)
for r in range(N):
    assert mx_row_bits(back, r) == L_ROWS[r], f"(L^-1)^-1 row {r}"

# --- singular, both by construction ----------------------------------------
# A zero row.
zero_row = build([[0], [], [0, 1, 2]], N)
assert mx_invert(zero_row) is None, "a zero row must be singular"
assert mx_rank(zero_row) == 2

# Two identical rows.
dup = build([[0, 1], [0, 1], [2]], N)
assert mx_invert(dup) is None, "a duplicated row must be singular"
assert mx_rank(dup) == 2

# The zero matrix.
assert mx_invert(mx_zeros(N, N)) is None
assert mx_rank(mx_zeros(N, N)) == 0

# Both directions matter. A function that always answered `None` would pass
# every assertion above; only the invertible cases rule that out.
assert mx_invert(ident) is not None
assert mx_invert(el) is not None

# --- transpose and reductions ----------------------------------------------
lt = mx_transpose(el)
# L's column 0 is set in all three rows, so row 0 of the transpose is full.
assert mx_row_bits(lt, 0) == [0, 1, 2]
assert mx_row_bits(lt, 2) == [2]
assert mx_ones(lt) == mx_ones(el)

assert mx_row_weights(el) == [1, 2, 3]
assert mx_argmax_weight(el) == [2, 3]
assert mx_get(el, 2, 0) is True
assert mx_get(el, 0, 2) is False

# --- and it survives a round trip through the store ------------------------
s_inv = mx_to_set(inv, 0)
bt = batch(d)
batch_store_set(bt, 3, s_inv)
batch_commit(bt)
db_checkpoint(d)
d = db_reopen(d)

snap = db_snapshot(d)
reloaded = mx_read(snap_load_set(snap, 3), 0, N, N)
for r in range(N):
    assert mx_row_bits(reloaded, r) == L_INV_ROWS[r], f"row {r} did not survive"

# The reloaded inverse still inverts the original.
assert mx_row_bits(mx_mul(el, reloaded, "gf2"), 1) == [1]

# --- GEMM over GF(2), where addition cancels -------------------------------
# L*L^-1 + I = I + I = 0 over GF(2).
cancelled = mx_gemm(el, reloaded, ident, "gf2")
assert mx_ones(cancelled) == 0, "I + I must cancel over GF(2)"

# The same expression over the boolean semiring is NOT the identity plus the
# identity, because L^-1 is only an inverse over GF(2) — the whole point of that
# field is 1 + 1 = 0, and boolean OR has no cancellation. The boolean product is
#   row 0: {0}            row 1: {0} | {0,1}        row 2: {0} | {0,1} | {1,2}
# so it is lower triangular and full, six ones, and OR-ing I changes nothing.
bool_prod = mx_mul(el, reloaded, "bool")
assert mx_row_bits(bool_prod, 0) == [0]
assert mx_row_bits(bool_prod, 1) == [0, 1]
assert mx_row_bits(bool_prod, 2) == [0, 1, 2]
assert mx_ones(mx_gemm(el, reloaded, ident, "bool")) == 6
assert mx_ones(bool_prod) == 6, "an inverse over GF(2) is not one over booleans"

# The strided reader reaches the same matrix through an explicit layout.
same = mx_read_at(snap_load_set(snap, 3), 0, N, N, N, N * N, 0)
for r in range(N):
    assert mx_row_bits(same, r) == L_INV_ROWS[r], f"strided read row {r}"

# --- the guard against a vacuous run ---------------------------------------
assert mx_ones(el) == 6, "the operand must not have been silently emptied"
assert L_INV_ROWS != L_ROWS, "the inverse must differ from the matrix"
assert mx_add(el, el, "gf2") is not None
assert mx_ones(mx_add(el, el, "gf2")) == 0, "x + x must be zero over GF(2)"
