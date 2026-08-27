#!/usr/bin/env python3
"""Check Theorem 7's termination measure against the rewrite arms in plan.rs.

Theorem 7 of `docs/formal-model.md` claims the planner terminates under the
measure

    mu(e) = ||e||

a single weighted node count. The proof obligation is a loop over the rewrite
arms, so this is that loop: EVERY outcome must strictly decrease the weight.

mu deliberately does **not** mention `cardinality_cost`. An earlier version
used ( ||e||, C ) lexicographically, which failed for two reasons found in
review: `cardinality_cost` is not a function of the expression at all -- it
consults `shared_prefixes`/`disjoint`, which spend a mutable thread-local
statistics budget, so its value depends on traversal history -- and it is live
source that changed three times while the proof quoted it. A one-component
measure is independent of `cheaper`, of the budget, of saturating arithmetic, and
of every future cost-model correction.

# What this script does and does not establish

**Completeness of the transcription is checked by drift detection, not by
parsing.** The ARMS table below is hand-transcribed from `pass_b`. This script
cannot read Rust; what it can do is refuse to stay green when the source it was
audited against changes. It therefore:

  1. locates `fn pass_b` and its closing brace, and hashes that exact region;
  2. fails if the hash differs from the pinned one -- meaning the transcription
     must be re-audited by a human before the green light is meaningful again;
  3. counts syntactic `Some((` sites and pins that too;
  4. only then checks the weights against every transcribed outcome.

The hash covers **all of `pass_b`**, including its traversal prologue and its
comments, not only the rewrite arms. That is deliberately conservative: it will
go red for edits that cannot affect the measure. A false red costs a re-audit; a
false green costs a wrong theorem.

It does **not** prove the table is complete today. That is a manual audit,
recorded by the pinned hash. A green run means "the transcribed outcomes satisfy
the measure, and the source has not moved since they were transcribed".

# Counting unit

`pass_b` contains 31 occurrences of the literal `Some((`. Exactly one of those
is not a rewrite outcome: line 747 builds an inline `Option<bounds>` as the
second field of an outcome. That leaves **30 syntactic outcome sites**.

One of those 30 -- the `range_difference` call at line 719 -- returns a value
whose shape is not fixed: it can fuse to one range, split into two, or vanish to
`Empty`. Expanding it gives **32 outcome schemas**, which is the unit the measure
must be checked against and the number of rows below.

This is a *third* unit, distinct from the "twenty rewrite arms" in `pass_b`'s
own doc comment. That comment counts top-level match arms for a bounds-statability
argument. Three different questions about the same function legitimately count it
three different ways; the failure mode is using one number to answer another's
question.

Run: python3 scripts/check-plan-measure.py
"""

import hashlib
import pathlib
import sys

SRC = pathlib.Path(__file__).resolve().parent.parent / "yesno-core/src/stream/plan.rs"

# Pinned against the audited revision. Update ONLY together with a re-audit of
# the ARMS table below.
PINNED_SHA256 = "035ac44c62e181cea267628a513e0499245832ab8e58429ba050b012344c63c3"
PINNED_SOME_LITERALS = 31  # 30 outcome sites + 1 inline Option<bounds> (line 747)

# Node weights. Every rewrite outcome must strictly decrease the weighted sum;
# there is no neutrality constraint, because no rule is neutral. ( An earlier
# weighting used w(or) = 1, which made the Or-factoring De Morgan arm neutral and
# forced a second lexicographic component. w(or) = 2 removes both. )
W = {
    "empty": 1, "range": 1, "set": 1,
    "not": 3,
    "or": 2,
    "and": 4,
    "diff": 6,   # \
    "xor": 6,    # symmetric difference
}


def weigh(t):
    """Weight of a term as (constant, {metavariable: count})."""
    if isinstance(t, str):
        if t in W:
            return (W[t], {})
        return (0, {t: 1})  # a metavariable stands for an arbitrary subterm
    const = W[t[0]]
    acc = {}
    for kid in t[1:]:
        c, d = weigh(kid)
        const += c
        for v, n in d.items():
            acc[v] = acc.get(v, 0) + n
    return (const, acc)


def extract_pass_b(text):
    """The source of `fn pass_b`, from its signature to its column-0 brace."""
    lines = text.splitlines()
    start = next(i for i, l in enumerate(lines) if l.startswith("fn pass_b("))
    end = next(i for i in range(start + 1, len(lines)) if lines[i] == "}")
    return "\n".join(lines[start:end + 1]), start + 1, end + 1


def check(name, lhs, rhs, kind):
    lc, ld = weigh(lhs)
    rc, rd = weigh(rhs)
    names = set(ld) | set(rd)
    if kind == "decrease":
        ok = all(ld.get(v, 0) >= rd.get(v, 0) for v in names) and lc > rc
        verdict = "decreases" if ok else "*** FAILS ***"
    else:
        ok = all(ld.get(v, 0) == rd.get(v, 0) for v in names) and lc == rc
        verdict = "neutral (C-gated)" if ok else "*** FAILS ***"
    print(f"  {'ok  ' if ok else 'FAIL'} {name:<52} {lc:>3} -> {rc:<3}  {verdict}")
    return ok


# Every outcome schema of pass_b, in source order.
ARMS = [
    ("a AND b => 0            (empty bounds)", ("and", "a", "b"), "empty", "decrease"),
    ("a OR b => a             (b empty)", ("or", "a", "b"), "a", "decrease"),
    ("a OR b => b             (a empty)", ("or", "a", "b"), "b", "decrease"),
    ("a XOR b => a            (b empty)", ("xor", "a", "b"), "a", "decrease"),
    ("a XOR b => b            (a empty)", ("xor", "a", "b"), "b", "decrease"),
    ("a \\ b => 0              (a empty)", ("diff", "a", "b"), "empty", "decrease"),
    ("a \\ b => a              (b empty)", ("diff", "a", "b"), "a", "decrease"),
    ("a AND b => 0            (disjoint)", ("and", "a", "b"), "empty", "decrease"),
    ("a \\ b => a              (disjoint)", ("diff", "a", "b"), "a", "decrease"),
    ("a XOR b => a OR b       (disjoint)", ("xor", "a", "b"), ("or", "a", "b"), "decrease"),
    ("R1 \\ R2 => R            (range diff, fused)", ("diff", "range", "range"), "range", "decrease"),
    ("R1 \\ R2 => R OR R       (range diff, split)", ("diff", "range", "range"), ("or", "range", "range"), "decrease"),
    ("R1 \\ R2 => 0            (range diff, empty)", ("diff", "range", "range"), "empty", "decrease"),
    ("R \\ e => not_R e        (definition)", ("diff", "range", "e"), ("not", "e"), "decrease"),
    ("e \\ R => 0              (e contained in R)", ("diff", "e", "range"), "empty", "decrease"),
    ("e \\ b => 0              (b covers e)", ("diff", "e", "b"), "empty", "decrease"),
    ("R1 AND R2 => R          (both ranges)", ("and", "range", "range"), "range", "decrease"),
    ("R1 AND R2 => 0          (both ranges, disjoint)", ("and", "range", "range"), "empty", "decrease"),
    ("R AND b => b            (b contained)", ("and", "range", "b"), "b", "decrease"),
    ("a AND R => a            (a contained)", ("and", "a", "range"), "a", "decrease"),
    ("a AND b => b            (a covers b)", ("and", "a", "b"), "b", "decrease"),
    ("a AND b => a            (b covers a)", ("and", "a", "b"), "a", "decrease"),
    ("not p AND not q => not(p OR q)   [De Morgan, AND-factoring]",
     ("and", ("not", "p"), ("not", "q")), ("not", ("or", "p", "q")), "decrease"),
    ("R1 OR R2 => R           (fuse)", ("or", "range", "range"), "range", "decrease"),
    ("R OR b => R             (b contained)", ("or", "range", "b"), "range", "decrease"),
    ("a OR R => R             (a contained)", ("or", "a", "range"), "range", "decrease"),
    ("a OR b => a             (a covers b)", ("or", "a", "b"), "a", "decrease"),
    ("a OR b => b             (b covers a)", ("or", "a", "b"), "b", "decrease"),
    ("not p OR not q => not(p AND q)   [De Morgan, OR-factoring]",
     ("or", ("not", "p"), ("not", "q")), ("not", ("and", "p", "q")), "decrease"),
    ("R XOR b => not_R b      (b contained)", ("xor", "range", "b"), ("not", "b"), "decrease"),
    ("a XOR R => not_R a      (a contained)", ("xor", "a", "range"), ("not", "a"), "decrease"),
    ("not not x => x AND R    (double negation)",
     ("not", ("not", "x")), ("and", "x", "range"), "decrease"),
]


def main():
    text = SRC.read_text()
    body, lo, hi = extract_pass_b(text)
    digest = hashlib.sha256(body.encode()).hexdigest()
    some_sites = body.count("Some((")

    print(f"source: {SRC.name} lines {lo}-{hi} (fn pass_b)")
    ok_src = True

    if digest != PINNED_SHA256:
        print(f"  DRIFT  pass_b has changed since the ARMS table was audited.")
        print(f"         pinned {PINNED_SHA256}")
        print(f"         actual {digest}")
        print("         Re-audit the ARMS table against the source, then re-pin.")
        ok_src = False
    else:
        print(f"  ok    source unchanged since audit ({digest[:16]}...)")

    for label, actual, pinned in (
        ("`Some((` literals", some_sites, PINNED_SOME_LITERALS),
    ):
        if actual != pinned:
            print(f"  FAIL  {label}: {actual}, audited against {pinned}")
            ok_src = False
        else:
            print(f"  ok    {label}: {actual}")

    print()
    print(f"weights: {W}")
    print()
    results = [check(n, a, b, k) for n, a, b, k in ARMS]
    print()
    print(f"{len(ARMS)} outcome schemas ( from {some_sites - 1} outcome sites; the "
          f"range-difference site has three shapes ): all must strictly decrease.")

    if all(results) and ok_src:
        print("ALL TRANSCRIBED OUTCOME SCHEMAS OK.")
        print("( Completeness of the transcription is a manual audit, pinned by the "
              "source hash above. )")
        return 0
    print("CHECK FAILED -- see the FAIL/DRIFT lines above.")
    return 1


if __name__ == "__main__":
    sys.exit(main())
