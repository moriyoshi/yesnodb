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
STRIDE = 1280
KEY = 42
RESULT_KEY = 43

A = (3 << 1200) | (1 << 640) | 0xFEEDFACE
B = (1 << 1279) - 12345
C = 0
SERIES = [A, B, C, 7]

# --- write, checkpoint, close, reopen ----------------------------------------
s = bn_series_build(SERIES, WIDTH, STRIDE)
b = batch(d)
batch_store_set(b, KEY, s)
batch_commit(b)
db_checkpoint(d)
db_close(d)

d = db_open("main", shards=2)
snap = db_snapshot(d)
loaded = snap_load_set(snap, KEY)

# Every value survived the round trip exactly.
for k in range(len(SERIES)):
    assert bn_series_get(loaded, k, WIDTH, STRIDE) == SERIES[k], k
assert bn_series_count(loaded, WIDTH, STRIDE) == 4

# --- arithmetic on values that came off disk ---------------------------------
x = bn_series_get(loaded, 0, WIDTH, STRIDE)
y = bn_series_get(loaded, 1, WIDTH, STRIDE)

assert bn_add(x, y) == A + B
assert bn_mul(x, y) == A * B
q, r = bn_divmod(y, x)
assert (q, r) == divmod(B, A)
assert q * x + r == y

# The product is wider than the layout, so storing it needs a wider one. A
# value that does not fit is refused rather than truncated -- truncate() is how a
# caller opts into the cyclic reading, and it is spelled at the call site.
product = bn_mul(x, y)
assert bn_bit_len(product) > WIDTH

WIDE_W = 2 * WIDTH + 64
out = bn_series_build([product, bn_truncate(product, WIDTH)], WIDE_W, WIDE_W)

b = batch(d)
batch_store_set(b, RESULT_KEY, out)
batch_commit(b)
db_checkpoint(d)
snap_release(snap)
db_close(d)

# --- reopen once more and check the derived values survived ------------------
d = db_open("main", shards=2)
snap = db_snapshot(d)
back = snap_load_set(snap, RESULT_KEY)

assert bn_series_get(back, 0, WIDE_W, WIDE_W) == A * B
assert bn_series_get(back, 1, WIDE_W, WIDE_W) == (A * B) % (1 << WIDTH)

# The original key is still intact after a second checkpoint.
orig = snap_load_set(snap, KEY)
for k in range(len(SERIES)):
    assert bn_series_get(orig, k, WIDTH, STRIDE) == SERIES[k], k

snap_release(snap)
db_close(d)
