# A bit matrix through the durable path: build, checkpoint, reopen, read,
# multiply, store, reopen, verify.
#
# What this covers that no Rust test does is the *sequence*. `yesno-core`'s
# proptests already compare `mul` against a naive Vec<Vec<bool>> oracle over
# boundary-biased shapes, randomized, which is strictly better than anything
# written here. What they cannot reach is a matrix that has been through a
# checkpoint and a reopen: a read on a live `Db` is answered from the memtable
# and never touches the store, and that blind spot hid three bugs.
#
# So the assertions here are about *survival and identity*, and the expected
# values are written out as literals rather than recomputed. A Python matmul
# compared against another Python walk is what got `and_shape.py` moved out of
# `scenarios/`.

d = db_open("main", shards=2)

# --- a 4x4 matrix, stated as literal rows -----------------------------------
# A =  1 0 0 1      B = the identity with rows 0 and 1 swapped
#      0 1 1 0
#      0 0 1 0
#      1 0 0 0
A_ROWS = [[0, 3], [1, 2], [2], [0]]
B_ROWS = [[1], [0], [2], [3]]

N = 4


def build(rows):
    # Dense 4x4 layout: element (r, c) is ordinal r * 4 + c.
    m = mx_zeros(N, N)
    for r in range(N):
        for c in rows[r]:
            m = mx_put(m, r, c, 1)
    return m


a = build(A_ROWS)
b = build(B_ROWS)

assert mx_shape(a) == [4, 4]
assert mx_ones(a) == 6
assert mx_row_bits(a, 0) == [0, 3]
assert mx_get(a, 0, 3) is True
assert mx_get(a, 0, 1) is False

# B is a permutation, so A*B just permutes A's columns: 0<->1.
# Row 0 of A is {0,3}; column 0 of A maps to column 1, so row 0 becomes {1,3}.
EXPECT_MUL = [[1, 3], [0, 2], [2], [1]]
ab = mx_mul(a, b, "bool")
for r in range(N):
    assert mx_row_bits(ab, r) == EXPECT_MUL[r], f"row {r} of A*B"

# --- store the product, checkpoint, reopen, read it back --------------------
prod_set = mx_to_set(ab, 0)
bt = batch(d)
batch_store_set(bt, 7, prod_set)
batch_commit(bt)
db_checkpoint(d)

# The reopen is the point. Without it the read is answered from the memtable.
d = db_reopen(d)

s = db_snapshot(d)
back_set = snap_load_set(s, 7)
back = mx_read(back_set, 0, N, N)
for r in range(N):
    assert mx_row_bits(back, r) == EXPECT_MUL[r], f"row {r} did not survive reopen"
assert mx_ones(back) == 6

# --- GEMM accumulates, and `mul` is the zero-addend case --------------------
zero = mx_zeros(N, N)
assert mx_row_bits(mx_gemm(a, b, zero, "bool"), 0) == EXPECT_MUL[0]

# A*B + A, over the boolean semiring, is the union of the two.
fused = mx_gemm(a, b, a, "bool")
split = mx_add(mx_mul(a, b, "bool"), a, "bool")
for r in range(N):
    assert mx_row_bits(fused, r) == mx_row_bits(split, r), f"gemm != mul+add at row {r}"
    assert mx_row_bits(fused, r) == sorted(set(EXPECT_MUL[r]) | set(A_ROWS[r]))

# Over GF(2) the same expression cancels where both terms have a bit.
fused2 = mx_gemm(a, b, a, "gf2")
for r in range(N):
    assert mx_row_bits(fused2, r) == sorted(set(EXPECT_MUL[r]) ^ set(A_ROWS[r]))

# --- the identity, transpose, and the reductions ---------------------------
ident = mx_identity(N)
assert mx_row_bits(mx_mul(a, ident, "bool"), 0) == A_ROWS[0]
assert mx_row_bits(mx_mul(ident, a, "bool"), 3) == A_ROWS[3]

at = mx_transpose(a)
# A's column 0 is set in rows 0 and 3, so row 0 of the transpose is {0, 3}.
assert mx_row_bits(at, 0) == [0, 3]
assert mx_ones(at) == mx_ones(a)
assert mx_row_bits(mx_transpose(at), 2) == A_ROWS[2]

assert mx_row_weights(a) == [2, 2, 1, 1]
# Ties go to the smallest row, so rows 0 and 1 both weigh 2 and row 0 wins.
assert mx_argmax_weight(a) == [0, 2]

# --- a strided layout, and one that straddles a chunk ----------------------
# 100x100 dense is 10 000 bits, so matrix 6 spans 60 000..70 000 and crosses the
# 65 536-bit chunk boundary. Reading it must give back exactly what was stored.
# A snapshot holds the directory's exclusive lock, so it must be released
# before the reopen — the harness refuses rather than deadlocking.
snap_release(s)
db_insert_range(d, 9, 60000, 60099)
db_checkpoint(d)
d = db_reopen(d)
s2 = db_snapshot(d)
wide = snap_load_set(s2, 9)
straddler = mx_read_at(wide, 6, 100, 100, 100, 10000, 0)
# Ordinals 60 000..60 099 are exactly row 0 of matrix 6.
assert mx_row_bits(straddler, 0) == list(range(100))
assert mx_ones(straddler) == 100, "a matrix crossing a chunk lost bits"

# The same bits read column-first are the transpose of reading them row-first.
cm = mx_read_at(wide, 6, 100, 100, 100, 10000, 1)
assert mx_ones(cm) == 100
for r in range(100):
    assert mx_row_bits(cm, r) == [0], f"col-major row {r}"

# --- the guard against a vacuous run ---------------------------------------
assert mx_ones(a) > 0 and mx_ones(b) > 0, "the operands must not be empty"
assert EXPECT_MUL != A_ROWS, "the product must differ from its operand"
assert mx_rank(ident) == N
