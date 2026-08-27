#!/usr/bin/env python3
"""Flag characters in `docs/*.md` that pdflatex cannot typeset.

The documents under `docs/` are Pandoc-flavoured Markdown that must convert to
TeX. A bare Unicode symbol *outside* math mode is fatal there and invisible
everywhere else: it renders fine in every Markdown viewer, in the terminal, and
in review, and only fails at `pandoc --pdf-engine=pdflatex`. That is a bad
failure mode -- the defect is introduced by one author and discovered by
another, long afterwards.

A logical AND in prose (`bitmap ∧ bitmap`) is the canonical case. It is also a
*content* defect rather than a typesetting one: a mathematical operator belongs
in math mode, where it would have been safe automatically.

What is allowed, and why the allowlist is small:

* Anything inside `$...$` or `$$...$$`. Pandoc emits it as math and TeX handles
  the symbol.
* Latin-1 (`< U+0100`). `inputenc` covers it -- accented author names, `§`,
  `µ`, `×`, `±`.
* Typographic punctuation that Pandoc maps to TeX commands.
* Characters explicitly declared via `\newunicodechar` in the file's own
  `header-includes`. Declaring one is the supported way to add a symbol: it
  makes the mapping visible in the document that relies on it.

Verbatim spans are **not** exempt. `\texttt{}` does not rescue an unmapped
character, so a symbol inside backticks fails exactly like one in prose.

Usage:  python3 scripts/check-tex-safe.py [files...]     (default: docs/*.md)
Exit 1 on any finding.
"""

import glob
import re
import sys
import unicodedata

# Punctuation Pandoc knows how to write as TeX.
TYPOGRAPHY = set("\u2014\u2013\u2026\u2018\u2019\u201c\u201d\u2010\u2011\u00a0")


def strip_math(text: str) -> str:
    """Blank out math spans, preserving line count and column offsets."""
    def blank(m: re.Match) -> str:
        return "".join(c if c == "\n" else " " for c in m.group(0))

    text = re.sub(r"\$\$.*?\$\$", blank, text, flags=re.S)
    return re.sub(r"(?<!\$)\$[^$\n]+\$", blank, text)


def declared(text: str) -> set:
    """Characters the file maps itself via \\newunicodechar in header-includes."""
    return {m.group(1) for m in re.finditer(r"\\newunicodechar\{(.)\}", text)}


def check(path: str) -> list:
    raw = open(path, encoding="utf-8").read()
    allowed = TYPOGRAPHY | declared(raw)
    findings = []
    for lineno, line in enumerate(strip_math(raw).split("\n"), 1):
        for col, ch in enumerate(line, 1):
            if ord(ch) < 0x100 or ch in allowed:
                continue
            try:
                name = unicodedata.name(ch)
            except ValueError:
                name = "unnamed"
            findings.append((path, lineno, col, ch, name))
    return findings


def main() -> int:
    files = sys.argv[1:] or sorted(glob.glob("docs/*.md"))
    if not files:
        print("check-tex-safe: no files to check", file=sys.stderr)
        return 0

    findings = [f for path in files for f in check(path)]
    for path, lineno, col, ch, name in findings:
        print(
            f"{path}:{lineno}:{col}: U+{ord(ch):04X} {name} "
            f"is not typesettable outside math mode"
        )

    if findings:
        print(
            f"\n{len(findings)} finding(s). Fix by moving the symbol into math "
            f"( $\\wedge$, $\\subseteq$, $\\to$ ) -- which is usually what it "
            f"should have been -- or, for a marker with no math meaning, by "
            f"declaring it in the file's header-includes:\n"
            f"    \\usepackage{{newunicodechar}}\n"
            f"    \\newunicodechar{{X}}{{\\textbf{{[X]}}}}",
            file=sys.stderr,
        )
        return 1

    # Name what was excluded, not just what passed.
    #
    # `strip_math` blanks every math span before scanning, so a character
    # inside `$...$` is never examined -- 6.1% of `docs/` by volume as of
    # 2026-09-16. "N file(s) clean" invited the reader to conclude the files had
    # been checked in full, which is the same defect as a consumer's
    # "54 lines checked" against a 77-line file with comments stripped. The
    # exclusion is deliberate ( math mode has its own escaping rules ); leaving
    # it unsaid was not.
    print(
        f"check-tex-safe: {len(files)} file(s) clean "
        f"( math spans not examined )"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
