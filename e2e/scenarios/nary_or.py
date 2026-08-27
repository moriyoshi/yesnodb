# k-way union: the n-ary accumulator against a pairwise fold.
#
# Was `yesno-core/examples/nary_or.rs` until 2026-08-26. Two shapes, because
# they put the cost in different places:
#
#  * **dense / overlapping** — both paths must read every input ordinal, so the
#    ceiling is low however the union is organized.
#  * **sparse / disjoint** — the posting-list shape. The fold's accumulator
#    grows toward the whole union and is copied once per input, which is the
#    quadratic the n-ary accumulator exists to remove.
#
# The example asserted only that the two agree on cardinality. This asserts they
# agree on *contents*, which is the stronger claim and no more expensive: a
# `union_all` that dropped an ordinal and gained another would pass on length.
#
#   yesno-e2e --show-output --arg kmax=1024 --arg dense_n=50000 nary_or.py

KMAX = yn_arg("kmax", 64)
DENSE_N = yn_arg("dense_n", 5000)
SPARSE_N = yn_arg("sparse_n", 500)


def ks():
    # 2, 8, 32, ... up to KMAX. Powers of four, as the example used.
    out = []
    k = 2
    while k <= KMAX:
        out.append(k)
        k = k * 4
    return out


def stride_set(base, step, count):
    sb = sb_new()
    sb_stride(sb, base, step, count)
    return sb_build(sb)


def run(label, build):
    print(label)
    for k in ks():
        sets = [build(i) for i in range(k)]

        # The fold, timed as the scenario performs it. One host call per
        # `set_or`, so the interpreter is in this number — but each union of
        # thousands of ordinals is far above the ~1 us boundary cost, which is
        # why this shape survives migration and a nanosecond one would not.
        t0 = clock_ns()
        acc = stride_set(0, 1, 0)
        for s in sets:
            acc = set_or(acc, s)
        t1 = clock_ns()

        all_at_once = set_union_all(sets)
        t2 = clock_ns()

        fold_ms = (t1 - t0) / 1000000.0
        nary_ms = (t2 - t1) / 1000000.0
        n = set_len(all_at_once)
        assert n == set_len(acc), f"{label} k={k}: union_all and the fold disagree on cardinality"
        # Contents, not just the count.
        assert set_len(set_xor(all_at_once, acc)) == 0, f"{label} k={k}: union_all and the fold disagree on contents"
        # And a union really is a superset of every operand.
        for s in sets:
            assert set_len(set_andnot(s, all_at_once)) == 0, f"{label} k={k}: an operand is not contained in the union"

        ratio = fold_ms / nary_ms if nary_ms > 0 else 0.0
        print(f"  k={k:<5} fold {fold_ms:>8.2f} ms   union_all {nary_ms:>8.2f} ms   {ratio:>5.1f}x   ({n} ordinals)")
    print("")


# Heavy overlap over a shared space.
run(
    f"dense/overlapping - each {DENSE_N} ordinals over a shared space:",
    lambda i: stride_set(i * 37, 79, DENSE_N),
)
# Sparse and mostly disjoint - the posting-list shape.
run(
    f"sparse/disjoint - each {SPARSE_N} ordinals in its own region:",
    lambda i: stride_set(i * 10000000, 131, SPARSE_N),
)

# The identity that makes the n-ary path legitimate at all: unioning k sets is
# the same as unioning them one at a time, in any order. Checked here against a
# *reversed* fold, which the example did not do — an accumulator that depended
# on operand order would pass every test above.
sets = [stride_set(i * 10000000, 131, SPARSE_N) for i in range(8)]
forward = stride_set(0, 1, 0)
for s in sets:
    forward = set_or(forward, s)
backward = stride_set(0, 1, 0)
for s in reversed(sets):
    backward = set_or(backward, s)
assert set_len(set_xor(forward, backward)) == 0, "union is not order-independent"
assert set_len(set_xor(forward, set_union_all(sets))) == 0, "union_all disagrees with both folds"
