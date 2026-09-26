# Signed arbitrary-precision arithmetic, with Python's own int as the oracle.
#
# Python's int is signed AND unbounded, so it is exactly this type -- which is
# the whole reason these assertions live here rather than in a Rust test. A
# scenario can write `assert bi_mul(a, b) == a * b` with no reference
# implementation of its own.
#
# Two conventions need spelling out rather than comparing, and they are the
# reason this file exists:
#
#   * Division TRUNCATES toward zero here. Python's own divmod is FLOORED, so
#     comparing against it directly would assert the wrong thing for exactly
#     the operand pairs where the conventions differ -- negative dividends.
#   * The two's-complement wrap and the clamp target the SAME field. They are
#     two overflow rules for one range, not two ranges.

A = (3 << 1200) | (1 << 640) | 0xFEEDFACE
B = (1 << 900) - 12345
SMALL = [-100, -7, -1, 0, 1, 7, 100]
DIVISORS = [-9, -2, -1, 1, 2, 9]

# --- the ring operations, straight against Python ----------------------------
for a in SMALL + [A, -A, B, -B]:
    for b in SMALL + [A, -A]:
        assert bi_add(a, b) == a + b, (a, b)
        assert bi_sub(a, b) == a - b, (a, b)
        assert bi_mul(a, b) == a * b, (a, b)

assert bi_neg(A) == -A
assert bi_neg(-A) == A
assert bi_neg(0) == 0
assert bi_abs(-A) == A
assert bi_abs(A) == A

# Ordering reverses below zero, which is the one thing a sign-and-magnitude
# representation most easily gets wrong.
def sign_of(a, b):
    if a < b:
        return -1
    if a > b:
        return 1
    return 0


for a in SMALL:
    for b in SMALL:
        assert bi_cmp(a, b) == sign_of(a, b), (a, b)
assert bi_cmp(-A, -B) == -1  # -A is much further below zero than -B
assert bi_cmp(-1, 0) == -1

# --- division truncates toward zero ------------------------------------------
# Spelled out rather than compared to divmod(), because Python floors.
def trunc_divmod(a, b):
    q = abs(a) // abs(b)
    if (a < 0) != (b < 0):
        q = -q
    return q, a - q * b


for a in SMALL + [A, -A]:
    for b in DIVISORS:
        assert bi_divmod(a, b) == trunc_divmod(a, b), (a, b)
        q, r = bi_divmod(a, b)
        assert q * b + r == a, (a, b)
        # The remainder carries the sign of the DIVIDEND, which is what
        # "truncating" means and what distinguishes it from Python's rule.
        assert r == 0 or (r < 0) == (a < 0), (a, b)
        assert abs(r) < abs(b), (a, b)

# Where the conventions differ, they really differ: -7 / 2 is -3 here and -4 in
# Python. Asserting that keeps this file honest about why it does its own sums.
assert bi_divmod(-7, 2) == (-3, -1)
assert divmod(-7, 2) == (-4, 1)

# A zero divisor has no answer in the domain, so it is absent rather than an
# error -- the same shape as the unsigned divmod.
assert bi_divmod(1, 0) is None
assert bi_div_euclid(1, 0) is None

# --- the Euclidean remainder is never negative -------------------------------
# And this one IS expressible in Python directly: a % abs(b) is exactly it.
for a in SMALL + [A, -A]:
    for b in DIVISORS:
        q, r = bi_div_euclid(a, b)
        assert r == a % abs(b), (a, b)
        assert r >= 0, (a, b)
        assert r < abs(b), (a, b)
        assert q * b + r == a, (a, b)

# --- one field, two overflow rules -------------------------------------------
# bi_truncate wraps; bi_saturate clamps. Both land inside [-2^(w-1), 2^(w-1)),
# and they agree exactly where nothing overflowed.
for w in [1, 2, 8, 16, 64, 200]:
    lo = -(1 << (w - 1))
    hi = (1 << (w - 1)) - 1
    for v in [-300, -129, -128, -1, 0, 1, 127, 128, 255, 300, A, -A]:
        wrapped = bi_truncate(v, w)
        clamped = bi_saturate(v, w)
        assert wrapped == ((v - lo) % (1 << w)) + lo, (v, w)
        assert clamped == max(lo, min(v, hi)), (v, w)
        assert lo <= wrapped <= hi, (v, w)
        assert lo <= clamped <= hi, (v, w)
        if lo <= v <= hi:
            assert wrapped == clamped == v, (v, w)

# Two's complement is asymmetric, so the clamp is too: one further below zero.
assert bi_saturate(128, 8) == 127
assert bi_saturate(-129, 8) == -128
assert bi_truncate(128, 8) == -128
assert bi_truncate(255, 8) == -1

# A zero-width field holds only zero, and both rules agree on that.
assert bi_truncate(5, 0) == 0
assert bi_saturate(5, 0) == 0

# --- the unsigned clamp, for comparison --------------------------------------
# bn_saturate targets [0, 2^w), so it is a different ceiling from bi_saturate's.
for w in [1, 8, 64]:
    for v in [0, 1, 255, 300, A]:
        assert bn_saturate(v, w) == min(v, (1 << w) - 1), (v, w)
assert bn_saturate(300, 8) == 255
assert bi_saturate(300, 8) == 127

# --- the same stored bits, read two ways -------------------------------------
# This is why the signed read is its own verb: nothing in the bits says which
# reading was meant.
for pattern, width, signed in [
    (255, 8, -1),
    (128, 8, -128),
    (127, 8, 127),
    (0, 8, 0),
    ((1 << 63), 64, -(1 << 63)),
]:
    s = bn_to_set(pattern)
    assert bn_of_set(s, width) == pattern, (pattern, width)
    assert bi_of_set(s, width) == signed, (pattern, width)

# A value clamped to a width survives a round trip through a set read back at
# that width. This is the property saturation was once refused on -- "the
# reader cannot saturate" -- and it holds because the clamp lands inside the
# field the reader reads.
for w in [8, 64, 200]:
    lo = -(1 << (w - 1))
    for v in [-300, -1, 0, 1, 300, A, -A]:
        clamped = bi_saturate(v, w)
        pattern = clamped if clamped >= 0 else (1 << w) + clamped
        assert bi_of_set(bn_to_set(pattern), w) == clamped, (v, w)
