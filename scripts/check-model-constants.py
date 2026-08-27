#!/usr/bin/env python3
"""Recompute `docs/formal-model.md`'s derived numbers from the source constants.

Several numbered statements in the article are *arithmetic over named constants*:
Proposition 5's amortised bound, Proposition 5''s switch cap and two-cycle proof,
Corollary 14.1 and Proposition 14.2's amplification figures, §8.3's A/B delay and
the index-entry floor §13.1 rests on. If a constant moves, those numbers become
quietly false — the propositions stay structurally right and numerically wrong,
which is the worst way for a document to fail.

This is the same drift class `check-plan-measure.py` guards for Theorem 7, but
the remedy is stronger. That script pins a hash and can only say "something
changed"; this one **reads the constants and recomputes**, so it names the figure
that is now wrong and what it should be.

# Every occurrence is bound to a named site

Two earlier versions of this file got the evidence wrong, in the same direction
twice. The first asked whether a value appeared *somewhere* in a 27 000-word
article — but `1.53` renders at five places, so correcting one would go green with
four stale. The second counted total occurrences — but **a count is cardinality,
not membership**: staling one intended site while an unrelated paragraph happens
to contain the same token leaves the total unchanged, and the check passes over a
stale document.

So each figure carries an explicit list of **named sites**, each a role-scoped
regex that must match exactly once. A value appearing in five places is five
checks. Staling one site, relocating a token, or adding a compensating occurrence
elsewhere all fail, because membership is what is asserted.

What this still does NOT do: it does not check that the constants are *right*.
`CLAUDE.md` is explicit that several look like tuning knobs and are not, and that
changing one requires a rationale in `JOURNAL.md`. This only enforces that the
article and the source agree about what they are.

**This checker never writes to the artifact it validates.** `main` takes the
article text as an argument, so `--self-test` corrupts an in-memory copy. An
earlier version wrote a deliberately wrong value into `docs/formal-model.md` and
restored it in a `finally` — which in a shared checkout is a lost-update race
against any concurrent edit, and leaves the article corrupted if the process dies.
It was wired into the gate, so every run took that risk.

Run: python3 scripts/check-model-constants.py [--self-test]
"""

import contextlib
import io
import math
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
ARTICLE = ROOT / "docs/formal-model.md"

WIDTH_OF = {"u8": 1, "u16": 2, "u32": 4, "u64": 8, "u128": 16}
WORDS = {1: "one", 2: "two", 3: "three", 4: "four", 5: "five"}

LIB = "yesno-core/src/lib.rs"
EXT = "yesno-core/src/store/extent.rs"
ALLOC = "yesno-core/src/store/alloc.rs"
NODE = "yesno-core/src/index/node.rs"


def source_value(kind, relpath, name, cast=int):
    """Every source input flows through here, so `--self-test` can perturb any of them."""
    text = (ROOT / relpath).read_text()
    if kind == "const":
        m = re.search(rf"pub const {name}\s*:\s*[A-Za-z0-9_]+\s*=\s*([0-9_.]+)", text)
    elif kind == "array_first":
        m = re.search(rf"pub const {name}\s*:\s*\[[^\]]+\]\s*=\s*\[\s*([0-9]+)", text)
    elif kind == "newtype_width":
        m = re.search(rf"pub struct {name}\(([a-z0-9]+)\)", text)
        if not m:
            raise SystemExit(f"could not find `pub struct {name}(..)` in {relpath}")
        return WIDTH_OF[m.group(1)]
    elif kind == "addend":
        m = re.search(rf"fn {name}\b[^{{]*{{[^}}]*?\+\s*([0-9]+)\s*}}", text, re.S)
    elif kind == "serialized_stride":
        # `let off = vals_off + i * 8;` — the width actually written to disk.
        m = re.search(r"let off = vals_off \+ i \* ([0-9]+);", text)
    else:
        raise SystemExit(f"unknown source kind {kind!r}")
    if not m:
        raise SystemExit(f"could not read {kind} {name} from {relpath}")
    return cast(m.group(1).replace("_", ""))


def figures():
    """Every audited figure, as (label, value, {site: pattern}, note).

    `{v}` in a pattern is replaced by the value's numeral rendering and `{w}` by
    its English word, both regex-escaped. Each site must match **exactly once**.
    """
    array_max = source_value("const", LIB, "ARRAY_MAX")
    demote = source_value("const", LIB, "BITMAP_DEMOTE")
    gain_num = source_value("const", LIB, "OPT_GAIN_NUM")
    gain_den = source_value("const", LIB, "OPT_GAIN_DEN")
    bitmap = source_value("const", LIB, "BITMAP_BYTES")
    C = source_value("const", ALLOC, "COMPACT_LIVE_FRACTION", float)
    delay = source_value("const", ALLOC, "RECLAIM_CKPT_DELAY")
    inline_max = source_value("const", EXT, "INLINE_MAX")
    narrowest = source_value("array_first", NODE, "KSUF_WIDTHS")
    # Two independent facts, deliberately read separately. `ChunkRef(u64)` is
    # the in-memory word; the on-disk value width is what `leaf_entry_size` adds
    # and what the writer strides by. A refactor could move one without the other,
    # so the article's on-disk claims depend on the *serialized* width and the
    # equality of the two is asserted as its own invariant.
    refwidth = source_value("newtype_width", EXT, "ChunkRef")
    addend = source_value("addend", NODE, "leaf_entry_size")
    stride = source_value("serialized_stride", NODE, "leaf value stride")
    if not (addend == stride == refwidth):
        raise SystemExit(
            f"index value width disagrees across the three places that define it: "
            f"leaf_entry_size adds {addend}, the writer strides {stride}, "
            f"ChunkRef is {refwidth} bytes")

    span = (array_max + 1) - (demote - 1)
    switches = round(math.log(bitmap / 2) / math.log(gain_den / gain_num))
    lo, hi = 1.5, 20.0
    for _ in range(200):
        mid = (lo + hi) / 2
        lo, hi = (mid, hi) if mid - 1 - 2 * math.log(mid) < 0 else (lo, mid)

    return [
        ("Prop 5 mutation span", span,
         {"§4.1 proposition": r"at least \$\d+-\d+ = {v}\$ mutations"},
         f"({array_max}+1)-({demote}-1)"),

        ("Prop 5 amortised bytes", math.ceil(bitmap / span),
         {"§1 constants table": r"\$\d+/\(g\+2\) < {v}\$ bytes per mutation",
          "§4.1 proposition": r"< {v}\$ bytes per mutation, amortised",
          "§10 complexity table": r"\| \$< {v}\$ bytes per mutation \|"},
         f"{bitmap}/{span} = {bitmap / span:.3f}"),

        ("Prop 5 hysteresis gap", array_max - demote,
         {"§4.1 asymmetry": r"gap is asymmetric — \${v}\$ below"},
         "ARRAY_MAX - BITMAP_DEMOTE"),

        ("Prop 5' switch cap", switches,
         {"§1 constants table": r"bounded at \$\\approx {v}\$ switches",
          "§4.1 proposition": r"\\approx {v}\$ switches can occur"},
         f"log_({gain_den}/{gain_num})({bitmap}/2)"),

        ("optimize gain ratio", f"{gain_num}/{gain_den}",
         {"§1 constants table": r"\| \${v}\$ \| a threshold chosen",
          "§4.1 contraction": r"multiplies the size by at most \${v}\$",
          "§12.6 hysteron": r"together with the \${v}\$ rule",
          "§12.6 comparison": r"\${v}\$ rule compares encoded sizes",
          "§12.6 MDL": r"§4\.1's \${v}\$ rule is a \*relative-improvement\*"},
         f"OPT_GAIN {gain_num}/{gain_den}"),

        ("optimize saving threshold", f"{100 * (1 - gain_num / gain_den):g}",
         {"§4.1 prose": r"\\ge {v}\\%\$ saving"}, f"1 - {gain_num}/{gain_den}"),

        ("optimize inequality", f"{gain_den}b_new<={gain_num}b_old",
         {"§4.1 display": rf"{gain_den}\\,b_{{\\mathrm{{new}}}} \\;\\le\\; "
                          rf"{gain_num}\\,b_{{\\mathrm{{old}}}}"},
         f"{gain_den} b_new <= {gain_num} b_old"),

        ("Prop 5' two-cycle", f"{gain_den ** 2}<={gain_num ** 2}",
         {"§4.1 proof": rf"\${gain_den ** 2} b_1 \\le {gain_num ** 2} b_1\$"},
         f"{gain_den}^2 vs {gain_num}^2"),

        ("Cor 14.1 SA", f"{math.log(1 / C) / (1 - C):.2f}",
         {"§9 simulation lead-in": r"and \$\\mathrm\{SA\} = {v}\$:",
          "§9 stationary row": r"\| \*\*{v}\*\* \|",
          "§9 degenerate comparison": r"above the stationary\n\${v}\$",
          "§9 configured value": r"giving \$\\mathrm\{SA\} = {v}\$ for",
          "§10 complexity table": r"\(1-C\) = {v}\$ at"},
         f"ln(1/{C})/(1-{C})"),

        ("Prop 14.2 WA", f"{1 / (1 - C):.2f}",
         {"§9 configured value": r"\$\\mathrm\{WA\} = {v}\$"}, f"1/(1-{C})"),

        ("degenerate bound 1/C", f"{1 / C:.2f}",
         {"§9 finite bound": r"upper bound is exactly \${v}\$"}, f"1/{C}"),

        ("Prop 15 optimum", f"{2 / (lo + hi):.3f}",
         {"§1 constants table": r"the computed optimum \${v}\$",
          "§9 statement": r"the product optimum is \$C \\approx {v}\$",
          "§9 derivation": r"i\.e\. \$C \\approx {v}\$",
          "§9 sensitivity row": r"— Proposition 15 \| {v} \|"},
         "root of x-1 = 2 ln x"),

        ("§8.3 A/B delay", delay,
         {"§8.3 obligation B": r"double-buffer delay of {w} checkpoints",
          "§12.5 LMDB": r"`RECLAIM_CKPT_DELAY = {v}` is that rule"},
         "RECLAIM_CKPT_DELAY"),

        ("§13.1 index floor", narrowest + addend,
         {"§13.1 floor": r"\${v}\$ bytes per occupied chunk is a floor"},
         f"KSUF_WIDTHS[0]={narrowest} + leaf_entry_size addend {addend}"),

        ("index entry value width", stride,
         {"§8.4 entry formula": r"an entry costs \$s \+ {v}\$",
          "§8.4 entry cost": r"\${v}\$ being the `ChunkRef`",
          "§13.1 entry formula": r"\\texttt\{ksuf\\_len\} \+ {v}\$ bytes",
          "§13.1 split": r"\${v}\$ are the `ChunkRef`"},
         f"leaf_entry_size addend = writer stride = {stride}"),

        ("§13.1 key suffix", narrowest,
         {"§13.1 narrowest width": r"narrowest legal suffix width is \${v}\$",
          "§13.1 split": r"\${v}\$ are the truncated key suffix"},
         f"KSUF_WIDTHS[0]={narrowest}"),

        ("§13.1 INLINE_MAX", inline_max,
         {"§13.1 inline regime": r"\\texttt\{INLINE\\_MAX\} = {v}\$"}, "INLINE_MAX"),
    ]


def sections(art):
    """Map a section number ( "4.1", "9", "13.1" ) to its own text.

A site label is only a *location constraint* if matching is confined to
    that location. An earlier version used the label as a comment and searched
    the whole article, so appending a site's complete role phrase to unrelated
    prose kept the checker green over a staled site. A section runs from its
    heading to the next heading of equal or higher level.
    """
    heads = []
    for m in re.finditer(r"^(#{1,3}) (\d+(?:\.\d+)*)\.?\s", art, re.M):
        heads.append((len(m.group(1)), m.group(2), m.start()))
    out = {}
    for i, (level, num, start) in enumerate(heads):
        end = len(art)
        for level2, _, start2 in heads[i + 1:]:
            if level2 <= level:
                end = start2
                break
        out[num] = art[start:end]
    return out


def render(pattern, value):
    v = re.escape(str(value))
    w = re.escape(WORDS.get(value, str(value))) if isinstance(value, int) else v
    return pattern.replace("{v}", v).replace("{w}", w)


RED = []


def main(art=None, collect=False):
    if art is None:
        art = ARTICLE.read_text()
    if collect:
        RED.clear()
    secs = sections(art)
    rows, bad = [], 0
    figs = figures()
    for label, value, sites, note in figs:
        for site, pattern in sites.items():
            num = site.split()[0].lstrip("§")
            hay = secs.get(num)
            n = -1 if hay is None else len(re.findall(render(pattern, value), hay))
            rows.append((label, site, str(value), n, note))
            if n != 1:
                bad += 1
                if collect:
                    RED.append(f"{label} :: {site}")
    w1 = max(len(r[0]) for r in rows)
    w2 = max(len(r[1]) for r in rows)
    for label, site, value, n, note in rows:
        if n == 1:
            print(f"  ok   {label:<{w1}}  {site:<{w2}}  {value}")
        else:
            what = "no such section" if n < 0 else f"matched {n}"
            print(f"  FAIL {label:<{w1}}  {site:<{w2}}  expected {value!r} exactly "
                  f"once in its own section, {what}   [{note}]")
    print()
    if bad:
        print(f"{bad} audited site(s) in docs/formal-model.md disagree with the source "
              f"constants. Update EVERY site — or, if a constant changed deliberately, "
              f"record the reasoning in JOURNAL.md as CLAUDE.md requires.")
        return 1
    print(f"all {len(rows)} audited sites across {len(figs)} figures agree with the "
          f"source constants.")
    return 0


# --------------------------------------------------------------- negative control

# Each source input, a value it is not, and **exactly which sites must redden**.
#
# An earlier version asserted only `fails >= 1`, which proves an input reaches
# *some* check and not that it reaches every dependent site. That is the coverage
# regression two previous reviews found: nine of `OPT_GAIN`'s ten sites could
# vanish and the control would stay green on the tenth.
PERTURBATIONS = [
    ("ARRAY_MAX", 4095, ['Prop 5 hysteresis gap :: §4.1 asymmetry',
                         'Prop 5 mutation span :: §4.1 proposition']),
    ("BITMAP_DEMOTE", 3500, ['Prop 5 amortised bytes :: §1 constants table',
                             'Prop 5 amortised bytes :: §10 complexity table',
                             'Prop 5 amortised bytes :: §4.1 proposition',
                             'Prop 5 hysteresis gap :: §4.1 asymmetry',
                             'Prop 5 mutation span :: §4.1 proposition']),
    ("BITMAP_BYTES", 4096, ['Prop 5 amortised bytes :: §1 constants table',
                            'Prop 5 amortised bytes :: §10 complexity table',
                            'Prop 5 amortised bytes :: §4.1 proposition',
                            "Prop 5' switch cap :: §1 constants table",
                            "Prop 5' switch cap :: §4.1 proposition"]),
    ("OPT_GAIN_NUM", 6, ["Prop 5' switch cap :: §1 constants table",
                         "Prop 5' switch cap :: §4.1 proposition",
                         "Prop 5' two-cycle :: §4.1 proof",
                         'optimize gain ratio :: §1 constants table',
                         'optimize gain ratio :: §12.6 MDL',
                         'optimize gain ratio :: §12.6 comparison',
                         'optimize gain ratio :: §12.6 hysteron',
                         'optimize gain ratio :: §4.1 contraction',
                         'optimize inequality :: §4.1 display',
                         'optimize saving threshold :: §4.1 prose']),
    ("OPT_GAIN_DEN", 9, ["Prop 5' switch cap :: §1 constants table",
                         "Prop 5' switch cap :: §4.1 proposition",
                         "Prop 5' two-cycle :: §4.1 proof",
                         'optimize gain ratio :: §1 constants table',
                         'optimize gain ratio :: §12.6 MDL',
                         'optimize gain ratio :: §12.6 comparison',
                         'optimize gain ratio :: §12.6 hysteron',
                         'optimize gain ratio :: §4.1 contraction',
                         'optimize inequality :: §4.1 display',
                         'optimize saving threshold :: §4.1 prose']),
    ("COMPACT_LIVE_FRACTION", 0.35, ['Cor 14.1 SA :: §10 complexity table',
                                     'Cor 14.1 SA :: §9 configured value',
                                     'Cor 14.1 SA :: §9 degenerate comparison',
                                     'Cor 14.1 SA :: §9 simulation lead-in',
                                     'Cor 14.1 SA :: §9 stationary row',
                                     'Prop 14.2 WA :: §9 configured value',
                                     'degenerate bound 1/C :: §9 finite bound']),
    ("RECLAIM_CKPT_DELAY", 3, ['§8.3 A/B delay :: §12.5 LMDB',
                               '§8.3 A/B delay :: §8.3 obligation B']),
    ("INLINE_MAX", 4, ['§13.1 INLINE_MAX :: §13.1 inline regime']),
    ("KSUF_WIDTHS", 4, ['§13.1 index floor :: §13.1 floor',
                        '§13.1 key suffix :: §13.1 narrowest width',
                        '§13.1 key suffix :: §13.1 split']),
    # These two define the same width in different places, so disagreeing with
    # the others must trip the cross-source invariant rather than redden prose.
    ("ChunkRef", "u32", "invariant"),
    ("leaf_entry_size", 9, "invariant"),
]


def run_quiet(art):
    """(exit code, red site IDs). `"invariant"` if a cross-source check tripped."""
    buf = io.StringIO()
    try:
        with contextlib.redirect_stdout(buf):
            rc = main(art, collect=True)
    except SystemExit:
        return 1, "invariant"
    return rc, sorted(RED)


def self_test():
    global source_value
    real = source_value
    art = ARTICLE.read_text()
    ok = True

    print("A. every source input must redden EXACTLY its dependent sites:\n")
    for name, value, expected in PERTURBATIONS:
        def fake(kind, relpath, n, cast=int, _n=name, _v=value):
            if n != _n:
                return real(kind, relpath, n, cast)
            return WIDTH_OF[_v] if kind == "newtype_width" else cast(_v)
        source_value = fake
        try:
            rc, got = run_quiet(art)
        finally:
            source_value = real
        good = rc == 1 and got == expected
        ok &= good
        if got == "invariant":
            detail = "tripped the cross-source width invariant"
        else:
            detail = f"{len(got)} site(s) red"
            if got != expected:
                detail += (f"\n         extra:   {sorted(set(got) - set(expected))}"
                           f"\n         missing: {sorted(set(expected) - set(got))}")
        print(f"  {'ok  ' if good else 'FAIL'} {name} := {value}: exit {rc}, {detail}")

    rc, _ = run_quiet(art)
    ok &= rc == 0
    print(f"\n  {'ok  ' if rc == 0 else 'FAIL'} unperturbed: exit {rc} (expected 0)")

    # B. Site identity, not cardinality. The corrupted rendering is *derived* from
    #    the constant, not hard-coded, so a legitimate change to C does not turn
    #    this control into a false alarm requiring a manual edit here.
    C = real("const", ALLOC, "COMPACT_LIVE_FRACTION", float)
    sa = f"{math.log(1 / C) / (1 - C):.2f}"
    site = f"giving $\\mathrm{{SA}} = {sa}$ for"
    assert site in art, "control cannot find the site it means to corrupt"

    print("\nB. site identity — a total occurrence count cannot tell these apart:\n")
    for label, mutated, want in [
        ("one site staled", art.replace(site, f"giving $\\mathrm{{SA}} = 9.99$ for", 1), 1),
        # The bare token is the WEAK version of this control: the role regex
        # rejects it anyway, so it never exercised the false green. The real
        # control appends the site's COMPLETE matching phrase somewhere else,
        # which a whole-article search accepts and a section-scoped one does not.
        ("staled + a bare token elsewhere",
         art.replace(site, "giving $\\mathrm{SA} = 9.99$ for", 1)
         + f"\n\nUnrelated prose mentioning {sa} in passing.\n", 1),
        ("staled + the COMPLETE role phrase elsewhere",
         art.replace(site, "giving $\\mathrm{SA} = 9.99$ for", 1)
         + f"\n\n# 99. Decoy\n\nUnrelated prose {site} nothing.\n", 1),
        ("the site's token relocated",
         art.replace(site, "giving it for", 1)
         + f"\n\nElsewhere: $\\mathrm{{SA}} = {sa}$ for nothing.\n", 1),
    ]:
        rc, _ = run_quiet(mutated)
        ok &= rc == want
        print(f"  {'ok  ' if rc == want else 'FAIL'} {label}: exit {rc} (expected {want})")

    print()
    print("the checker has teeth." if ok else "SELF-TEST FAILED — the checker cannot fire.")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(self_test() if "--self-test" in sys.argv else main())
