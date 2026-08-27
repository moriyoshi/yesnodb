# Do the disjointness rewrites pay for themselves?
#
# Was `yesno-core/examples/rule_economics.rs` until 2026-08-26.
#
# # Why it matters beyond the rules themselves
#
# `docs/formal-model.md` §12.3 builds its central argument on this measurement:
# a leapfrogging intersection is an *adaptive* algorithm, already within a
# constant factor of the shortest proof that two sets are disjoint, so a planner
# that recomputes that proof is competing against a constant-factor optimal
# opponent while paying `O(chunks)` for statistics. The claim that makes it
# structural rather than incidental is **"at any operand size"** — the unplanned
# cost must stay flat as the operands grow. That is the column to read.
#
# # Why the timing runs in the host and not in this file
#
# The quantities here are tens of nanoseconds. One host call costs about a
# microsecond, so a repetition loop written in Python would be timing monty by a
# factor of twenty. `q_time( expr, iters, terminal )` runs the loop on the Rust
# side and reports nanoseconds per iteration; it is the only reason this fixture
# survives migration at all.
#
# # The disjointness used here
#
# Spans that do not overlap, so `bounds()` alone proves it in `O(1)` — no
# sketch, no profile. That is the *cheapest* case for the planner, which makes
# it the fairest to the rule: any more expensive form of disjointness can only
# make the rewrite look worse.
#
# The example's own operand sizes were 10 / 1 000 / 100 000 chunks:
#
#   cargo run --release -p yesno-e2e -- --show-output \
#     --arg chunks=100000 --arg iters=2000 e2e/scenarios/rule_economics.py

MAX_CHUNKS = yn_arg("chunks", 1000)
# Repetitions per `q_time` call. Small by default so the gate stays quick;
# the example used thousands, which is what `--arg iters=` restores.
ITERS = yn_arg("iters", 300)


def chunks(lo, hi):
    # One ordinal at the base of each prefix in `[lo, hi)`, so chunk count is
    # exactly `hi - lo` and the operand's span is what decides disjointness.
    sb = sb_new()
    sb_stride(sb, lo << 16, 1 << 16, hi - lo)
    return sb_build(sb)


def sizes(candidates):
    return [n for n in candidates if n <= MAX_CHUNKS]


def iters_for(n):
    # Fewer repetitions on the big operands, as the example did — the point is
    # nanoseconds per operation, not a fixed wall-clock budget.
    return ITERS // 10 if n >= 100000 else ITERS


# The example carried a third column here: a hand-written leapfrog that read
# `len()` off a borrowed container, timed against the two above to show how much
# of the planner's win an adaptive executor could recover ( 83-86% ). It is gone,
# and the reason is the one that moved `and_shape.py` out of this directory on
# 2026-08-26 — it was a Python reimplementation of an executor that does not
# exist in `yesno-core`, so once its timing column stopped being comparable all
# that remained was "my Python agrees with yesno", which is an assertion about
# the wrong subject. The capability that motivated it landed as
# `ChunkStream::next_cardinality`, which `streams.py` exercises directly.

# ---------------------------------------------------------------- the rules

def measure(label, e, it):
    planned = q_plan(e)
    rewritten = q_repr(planned) != q_repr(e)

    # Agreement first. A rewrite that is faster and wrong is not a rewrite.
    assert q_cardinality(planned) == q_cardinality(e), f"{label}: planning changed the answer"

    plan_ns = q_time(e, it, "plan")["ns"]
    p = q_time(planned, it, "cardinality")["ns"]
    u = q_time(e, it, "cardinality")["ns"]
    # What the rule is worth: what it saves at execution, against what it costs
    # to find. Positive net means the rewrite pays for itself on one execution.
    net = (u - p) - plan_ns
    tag = "REWRITTEN" if rewritten else "unchanged"
    print(f"  {label:<34} plan {plan_ns:>10.0f}  exec-P {p:>10.0f}  exec-U {u:>10.0f}  P/U {p / u:>5.2f}x  net {net:>+10.0f}  {tag}")
    return {"label": label, "plan": plan_ns, "p": p, "u": u, "net": net, "rewritten": rewritten, "planned": planned}


print("ns per operation; P = planned, U = unplanned; net = (U - P) - plan\n")
unplanned_and = []
for n in sizes([10, 1000, 100000]):
    a = q_set(chunks(0, n))
    b = q_set(chunks(10 * n, 11 * n))
    # Control: overlapping operands, where no disjointness rule can fire. If the
    # unplanned column moves here too, the effect above is not the rewrite.
    c = q_set(chunks(n // 2, 3 * n // 2))
    it = iters_for(n)
    print(f"{n} chunks per operand:")
    r_and = measure("And(disjoint) -> Empty", q_and(a, b), it)
    r_andnot = measure("AndNot(disjoint) -> lhs", q_andnot(a, b), it)
    r_xor = measure("Xor(disjoint) -> Or", q_xor(a, b), it)
    r_ctl = measure("And(overlapping) [control]", q_and(a, c), it)
    print("")

    # Structural claims, not string comparisons. `q_repr` tells you *that*
    # something changed; these say *what* the planner produced, which is what
    # the rules are named after.
    assert q_kind(r_and["planned"]) == "empty", "a span-disjoint And must plan to Empty"
    assert q_kind(r_andnot["planned"]) == "set", "a span-disjoint AndNot must plan to its left operand"
    assert q_kind(r_xor["planned"]) != "xor", "a span-disjoint Xor must not stay an Xor"
    assert not r_ctl["rewritten"], "the overlapping control must not be rewritten"

    unplanned_and.append((n, r_and["u"]))

# The load-bearing claim. The operands grow by four orders of magnitude and the
# *unplanned* leapfrog must stay flat — that is what makes it a constant-factor
# optimal opponent rather than a baseline the planner can outgrow.
first_n, first_ns = unplanned_and[0]
last_n, last_ns = unplanned_and[-1]
growth = last_ns / first_ns
print(f"unplanned And(disjoint): {first_ns:.0f} ns at {first_n} chunks, {last_ns:.0f} ns at {last_n} chunks = {growth:.2f}x")
if last_n >= 1000 * first_n:
    assert growth < 10.0, (
        f"operands grew {last_n // first_n}x and the unplanned intersection cost grew {growth:.1f}x; "
        "the leapfrog is no longer adaptive"
    )
