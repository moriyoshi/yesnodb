# A series of big integers through the durable path: build, store, checkpoint,
# close, REOPEN, read back through a snapshot, do arithmetic, store the result,
# reopen again, verify.
#
# What this covers that no Rust test does is the *sequence*. yesno-core's
# tests/bignum_oracle.rs already compares every kernel against num-bigint over
# boundary-biased randomized input, which is strictly stronger than anything
# written here. What it cannot reach is a value that has been through a
# checkpoint and a reopen: a read on a live Db is answered from the memtable and
# never touches the store, and that blind spot hid three bugs in the matrix
# module.
#
# So the assertions are about survival and identity, and Python's own int is the
# oracle for the arithmetic in between.

d = db_open("main", shards=2)

WIDTH = 1280       # 20 limbs: exactly the Karatsuba crossover
KEY = 42           # each value is its own key: one set is one integer
RESULT_KEY = 60

A = (3 << 1200) | (1 << 640) | 0xFEEDFACE
B = (1 << 1279) - 12345
C = 0
SERIES = [A, B, C, 7]

# --- write, checkpoint, close, reopen ----------------------------------------
b = batch(d)
for k, v in enumerate(SERIES):
    batch_store_set(b, KEY + k, bn_to_set(v))
batch_commit(b)
db_checkpoint(d)
db_close(d)

d = db_open("main", shards=2)
snap = db_snapshot(d)
# Every value survived the round trip exactly.
for k, v in enumerate(SERIES):
    assert bn_of_set(snap_load_set(snap, KEY + k), WIDTH) == v, k

# --- arithmetic on values that came off disk ---------------------------------
x = bn_of_set(snap_load_set(snap, KEY + 0), WIDTH)
y = bn_of_set(snap_load_set(snap, KEY + 1), WIDTH)

assert bn_add(x, y) == A + B
assert bn_mul(x, y) == A * B
q, r = bn_divmod(y, x)
assert (q, r) == divmod(B, A)
assert q * x + r == y

# The product is wider than either operand. Nothing has to be declared wide
# enough for it: a set is as wide as its highest member, so storing it is the
# same call. Truncation is how a caller opts into the cyclic reading, and it is
# still spelled at the call site.
product = bn_mul(x, y)
assert bn_bit_len(product) > WIDTH

WIDE_W = 2 * WIDTH + 64
b = batch(d)
batch_store_set(b, RESULT_KEY, bn_to_set(product))
batch_store_set(b, RESULT_KEY + 1, bn_to_set(bn_truncate(product, WIDTH)))
batch_commit(b)
db_checkpoint(d)
snap_release(snap)
db_close(d)

# --- reopen once more and check the derived values survived ------------------
d = db_open("main", shards=2)
snap = db_snapshot(d)
assert bn_of_set(snap_load_set(snap, RESULT_KEY), WIDE_W) == A * B
assert bn_of_set(snap_load_set(snap, RESULT_KEY + 1), WIDE_W) == (A * B) % (1 << WIDTH)

# The original keys are still intact after a second checkpoint.
for k, v in enumerate(SERIES):
    assert bn_of_set(snap_load_set(snap, KEY + k), WIDTH) == v, k

snap_release(snap)
db_close(d)

# --- signed values across the durable path -----------------------------------
# What no Rust test reaches: a two's-complement pattern that has been through a
# checkpoint and a reopen. A read on a live Db is answered from the memtable and
# never touches the store, which is the blind spot this file exists for.
WIDTH_S = 64
SIGNED = [-1, -(1 << 63), (1 << 63) - 1, 0, -12345, 6789]
SIGNED_KEY = 80

d = db_open("main", shards=2)
b = batch(d)
for k, v in enumerate(SIGNED):
    # Store the pattern a signed field of WIDTH_S bits would hold.
    pattern = v if v >= 0 else (1 << WIDTH_S) + v
    batch_store_set(b, SIGNED_KEY + k, bn_to_set(pattern))
batch_commit(b)
db_checkpoint(d)
db_close(d)

d = db_open("main", shards=2)
snap = db_snapshot(d)
for k, v in enumerate(SIGNED):
    loaded = snap_load_set(snap, SIGNED_KEY + k)
    # Read signed, and the value comes back exactly as it went in.
    assert bi_of_set(loaded, WIDTH_S) == v, k
    # The same bits read unsigned are a different number, which is why the two
    # readings are separate verbs rather than a flag.
    unsigned = bn_of_set(loaded, WIDTH_S)
    assert unsigned == (v if v >= 0 else (1 << WIDTH_S) + v), k

# Arithmetic on signed values that came off disk, with Python as the oracle.
x = bi_of_set(snap_load_set(snap, SIGNED_KEY + 0), WIDTH_S)
y = bi_of_set(snap_load_set(snap, SIGNED_KEY + 4), WIDTH_S)
assert x == -1
assert y == -12345
assert bi_mul(x, y) == 12345
assert bi_add(x, y) == -12346
assert bi_sub(y, x) == -12344

# The product is wider than the field it came from. Storing it needs no wider
# declaration -- a set is as wide as its highest member -- but reading it back
# at the ORIGINAL width would wrap, which is the caller's choice to make
# explicitly rather than a surprise.
product = bi_mul(bi_of_set(snap_load_set(snap, SIGNED_KEY + 1), WIDTH_S), bi_neg(2))
assert product == (1 << 64)
assert bi_truncate(product, WIDTH_S) == 0
assert bi_saturate(product, WIDTH_S) == (1 << 63) - 1

snap_release(snap)
db_close(d)
