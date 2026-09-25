# Arbitrary-precision arithmetic against Python's own `int`.
#
# Python's `int` is arbitrary-precision, so it *is* this type — a free, exact,
# independently written oracle that needs no reference implementation in the
# scenario. That is the same reason the set scenarios live here with Python's
# `set` as their oracle.
#
# These verbs are all prefixed because the names a caller reaches for first --
# pow, divmod, int, abs, min, max, len -- are Python builtins, and monty resolves
# builtins without ever asking the host. A verb named `pow` would silently
# compute Python's own `pow(a, e, m)`, which is exactly the oracle below, so the
# scenario would pass while testing nothing.
#
# Not the kernel oracle. yesno-core's tests/bignum_oracle.rs compares against
# num-bigint over boundary-biased randomized input, which is strictly stronger.
# What is here is the bridge and the boundary.

# Values chosen to cross the interesting widths: below a limb, exactly a limb,
# several limbs, and past the Karatsuba crossover ( 20 limbs = 1280 bits ).
SMALL = 12345
ONE_LIMB = (1 << 64) - 1
TWO_LIMB = (1 << 128) - 3
WIDE = (7 << 1600) | (1 << 800) | 0xDEADBEEF
ALL_ONES = (1 << 1536) - 1

VALUES = [0, 1, SMALL, ONE_LIMB, TWO_LIMB, WIDE, ALL_ONES]

# --- the bridge carries every magnitude, in both directions ------------------
# Expected bit lengths are written out rather than computed. monty has no
# int.bit_length(), and a Python loop that recomputes it would be reimplementing
# the thing under test -- which is what got and_shape.py moved out of scenarios/.
BIT_LENGTHS = [0, 1, 14, 64, 128, 1603, 1536]
LIMB_LENGTHS = [0, 1, 1, 1, 2, 26, 24]

for i in range(len(VALUES)):
    v = VALUES[i]
    assert bn_add(v, 0) == v, v
    assert bn_bit_len(v) == BIT_LENGTHS[i], i
    assert bn_limb_len(v) == LIMB_LENGTHS[i], i
    # A limb is 64 bits, so the two must agree by construction.
    assert bn_limb_len(v) == (bn_bit_len(v) + 63) // 64, i

# Hex is exact and has no leading zeros; the low limb keeps its full 16 digits.
assert bn_hex(0) == "0"
assert bn_hex(255) == "ff"
assert bn_hex(1 << 64) == "10000000000000000"
assert bn_hex(ONE_LIMB) == "ffffffffffffffff"

# --- arithmetic agrees with Python ------------------------------------------
for a in VALUES:
    for b in VALUES:
        assert bn_add(a, b) == a + b
        assert bn_mul(a, b) == a * b

        if a >= b:
            assert bn_sub(a, b) == a - b
        else:
            # None, not a wrap and not a clamp: the answer is not in the
            # domain of an unsigned type.
            assert bn_sub(a, b) is None

        if b == 0:
            assert bn_divmod(a, b) is None
        else:
            q, r = bn_divmod(a, b)
            assert (q, r) == divmod(a, b)
            # Both halves. A quotient digit one too small still satisfies the
            # first identity, because the excess lands in the remainder.
            assert q * b + r == a
            assert r < b

# --- shifts and truncation ---------------------------------------------------
for a in [SMALL, TWO_LIMB, WIDE, ALL_ONES]:
    for n in [0, 1, 63, 64, 65, 127, 128, 1000]:
        assert bn_shl(a, n) == a << n
        assert bn_shr(a, n) == a >> n
        # truncate is exactly `a mod 2**n`, which is what makes it the only
        # write-side overflow rule that agrees with a narrow read.
        assert bn_truncate(a, n) == a % (1 << n)

# --- modular exponentiation --------------------------------------------------
PRIMES = [97, 65537, 2147483647, 1000000007, (1 << 89) - 1]
for p in PRIMES:
    # Fermat: a^(p-1) == 1 mod p for a not divisible by p.
    for a in [2, 3, 7]:
        if a % p != 0:
            assert bn_pow_mod(a, p - 1, p) == 1, (a, p)
    assert bn_pow_mod(WIDE, 37, p) == pow(WIDE, 37, p)

# An even modulus, which Montgomery could never serve and Barrett must.
assert bn_pow_mod(WIDE, 20, 1 << 64) == pow(WIDE, 20, 1 << 64)
# m == 1 makes every residue zero, including the zero exponent -- the classic
# wrong answer here is 1.
assert bn_pow_mod(WIDE, 0, 1) == 0
assert bn_pow_mod(WIDE, 5, 1) == 0
# A zero modulus has no arithmetic at all.
assert bn_pow_mod(2, 3, 0) is None

# --- the OrdSet boundary, without a database ---------------------------------
# A set IS an integer: ordinal j carries the 2**j term. So this boundary is a
# pair of maps that must compose to the identity, not a layout to interpret.
WIDTH = 1700
for v in [WIDE, 0, SMALL, ALL_ONES % (1 << WIDTH)]:
    assert bn_of_set(bn_to_set(v), WIDTH) == v

# Reading at a narrower width is exactly `x mod 2**width`. It falls out of the
# least-significant-bit-first ordering, and with every value starting at ordinal
# zero it is the only thing the width argument does.
s = bn_to_set(WIDE)
for narrow in [1, 63, 64, 65, 300]:
    assert bn_of_set(s, narrow) == WIDE % (1 << narrow), narrow

# The empty set is zero. Absence and zero are the same thing in a set, which is
# why reading is total and has no "not addressable" answer to give.
assert bn_of_set(bn_to_set(0), WIDTH) == 0

# A value spanning several chunks. A chunk is 65536 bits and a limb is 64, so a
# chunk boundary is always a limb boundary -- but a reader that stopped at the
# first chunk would pass every assertion above and fail this one.
SPAN = 200_000
spanning = (1 << SPAN) | (1 << 65_536) | (1 << 65_535) | 1
t = bn_to_set(spanning)
assert bn_of_set(t, SPAN + 1) == spanning
assert bn_of_set(t, 65_536) == (1 << 65_535) | 1
