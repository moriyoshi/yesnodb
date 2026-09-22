#!/usr/bin/env bash
#
# Every check, in one command.
#
#   ./scripts/gate.sh            # the routine gate: clippy, tests, fmt of your diff
#   ./scripts/gate.sh --deep     # adds Valgrind and the sanitizers
#
# **MIRI was removed from `--deep` on 2026-08-29** and is no longer part of
# any gate. `scripts/miri.sh` is still on disk and still runnable by hand; its
# header records what it covers and why it stopped being affordable. Do not
# re-add it here without reading that header first.
#
# ---------------------------------------------------------------------------
# Why this exists
# ---------------------------------------------------------------------------
#
# The expensive checks in this repo have a poor record of being run. The fuzz
# targets, the aged-state measurement and Valgrind are each documented, each
# catch things nothing else does, and each only help when somebody remembers.
# Several of the bugs found in this codebase were sitting in front of a tool
# that could see them for whole milestones.
#
# Making them one command does not fix that on its own. `.github/workflows/ci.yml`
# now runs the routine steps on every push and the deep tools weekly, so the
# automation gap this header used to describe is closed — but note that workflow
# has still never executed anywhere. Until it has, this script is the gate that
# is actually known to work.
#
# ---------------------------------------------------------------------------
# What is deliberately *not* here
# ---------------------------------------------------------------------------
#
# ( Nothing, for formatting. The whole-tree `cargo fmt --check` used to be
# excluded here because it failed on a 65-hunk pre-existing baseline, which is
# noise that trains people to ignore the gate. The baseline was formatted away
# on 2026-08-27, so the check is now whole-tree and clean -- which is the point
# of having done it. See the `fmt-baseline` item. )
#
set -uo pipefail

# Every step below uses repo-root-relative paths ( `scripts/check-r1.py`,
# `yesno-core/fuzz`, ... ), so establish that root once rather than requiring the
# caller to be standing in it.
#
# The fuzz-crate step used to derive its own path from `$0` instead. That
# looked like robustness and was not: it made exactly one step invocable from
# elsewhere while every other step still assumed the repo root, so running the
# script from another directory produced a *step-specific* failure
# ( `cd: .../scratchpad/../yesno-core/fuzz: No such file or directory` ) that
# reads like a real formatting problem. Found by running a snapshot copy of this
# script out of a scratch directory, 2026-08-27.
cd "$(dirname "$(readlink -f "$0")")/.." || exit 1

deep=0
[[ "${1:-}" == "--deep" ]] && deep=1

fail=0
steps_run=0
# How many `step` calls each mode must reach. **Update these when adding or
# removing a step** — a mismatch is reported, not silently tolerated.
# **Raised 12 -> 14 and 18 -> 20 on 2026-09-14**, for two steps added the same
# day: "cited backlog slugs are recorded" and "doc links resolve". Diagnosed
# before raising, exactly as the paragraph above demands -- the failing run
# printed the final rustfmt steps and the `skipped (use --deep)` banner, which
# are the last things before `verdict`, so it reached the end and the mismatch
# was additions rather than an early exit. Both new steps are unconditional and
# so count in both modes.
EXPECT_STEPS=16
# Went 15 -> 14 on 2026-08-29 when the MIRI step was removed, and back to 15 on
# 2026-08-30 with the two-node failover drill. The number is a coincidence,
# not a restoration -- the MIRI step is gone and is not coming back here.
# **Raised 15 -> 16 on 2026-09-18** for "tests behind off-by-default features".
# Diagnosed before raising, as the paragraph above demands: the run reached the
# `skipped ( use --deep )` banner, so it ran to the end and the mismatch was the
# addition rather than an early exit.
#
# 9 -> 12 and 15 -> 18 on 2026-09-06, having been **wrong and failing every
# run**: three steps were added by two people without touching these, and
# `verdict` reported "ran 12 of 9" and exited non-zero whatever the checks
# said. Exactly the failure this counter exists to catch, arriving from the
# other direction -- it cannot tell "the run stopped early" from "somebody
# added a step", and both read as a red gate. Do not raise these to silence
# a mismatch without first checking which of the two it is.
#
# Left at 12 / 18 on 2026-09-06 when the two unified-image checks were added
# ( the binary set, and the entrypoint's shell syntax ), and that was **not** a
# no-op: the counts were 10 / 16 before them and the declared numbers were
# already 12 / 18, so adding two steps made the existing numbers correct rather
# than stale. At that moment the two errors were merely cancelling — the
# numbers were right for the wrong reason, which is not the same as being right.
#
# **Reconciled 2026-09-06, and re-reconciled 2026-09-14 at 14 / 20.** The two
# lists were enumerated against the source rather than inferred: routine mode is
# every `step` call above the `verdict` inside `if [[ $deep -eq 0 ]]`, deep
# mode adds the 6 below it, and **every one is unconditional** — no `step` sits
# inside an `if`, so there is no mode in which a declared number is
# reachable-but-not-reached. The cancellation is therefore spent: the numbers
# stand on the actual lists, and the earlier over-count no longer
# exists to be "corrected independently".
#
# The counts are deliberately **not** written into this prose any more. They were
# ( "the 12 `step` calls" ), and the 2026-09-14 additions made that sentence
# false in the same edit that made the constants right — a stale number inside
# the paragraph warning about stale numbers. The constants below are the one
# place they appear; the deep delta of 6 is the invariant worth stating here. The warning that survives is the
# original one, unchanged: `verdict` cannot tell "the run stopped early" from
# "somebody added a step", and both read as a red gate. Do not raise these to
# silence a mismatch without first checking which of the two it is.
#
# Count them the way `verdict` does, which is the only count that matters:
# routine mode is every `step` call reached before the `verdict` inside
# `if [[ $deep -eq 0 ]]`, and deep mode is all of them. Counting `^step` with
# grep gets a different answer, because some calls are indented and the whole
# file is not one mode.
EXPECT_STEPS_DEEP=22
step() {
    steps_run=$((steps_run + 1))
    printf '\n\033[1m== %s\033[0m\n' "$1"
}

# Verify the run reached the end, not merely that nothing it ran failed.
#
# **This exists because the gate once exited 0 having run less than half of
# itself.** A `--deep` run passed MIRI and then died on `ntf: command not found`
# — a fragment of `printf`, i.e. bash resuming mid-word, which is what happens
# when a script is modified while running. `set -uo pipefail` does not abort on
# that ( deliberately: the gate reports *every* failing check rather than
# stopping at the first ), so `$fail` was still 0, the tail never executed, and
# the run reported success without Valgrind, the sanitizers, or the aged-state
# measurement having happened at all.
#
# A gate that can pass without running is worse than no gate. Counting what
# actually ran is the cheapest check that catches it however it is caused.
verdict() {
    local want=$1
    if [[ $steps_run -ne $want ]]; then
        printf '\n\033[31m   INCOMPLETE: ran %d of %d steps\033[0m\n' "$steps_run" "$want"
        printf '   The run stopped early. Its verdict below covers only what ran.\n'
        fail=1
    fi
    [[ $fail -eq 0 ]] && printf '\n\033[32mgate passed\033[0m\n' || printf '\n\033[31mgate failed\033[0m\n'
    exit $fail
}
check() {
    if "$@"; then
        printf '   ok\n'
    else
        printf '   FAILED\n'
        fail=1
    fi
}
# A measurement step: its *numbers* are reported rather than asserted, but the
# fact that it *ran* is asserted.
#
# Do not write these as `cmd 2>/dev/null | tail -n`. That was the shape here
# until 2026-08-26, and it is structurally incapable of reporting its own
# absence: a renamed or deleted fixture left the step printing nothing and the
# gate green. A measurement that silently stops running is worse than no
# measurement — the gate keeps looking healthy exactly where a number used to be.
#
# The cause is **solely that nothing consulted the status**, and it is worth
# being exact about that, because the plausible diagnosis is wrong. `pipefail`
# on line 33 was already doing its job: measured, that pipeline returns 1 with
# `pipefail` and 0 without it. What is missing is `set -e`, which this script
# deliberately does not use — it collects failures in `fail` and reports them
# together rather than stopping at the first. So an unchecked command is simply
# discarded, however correct its exit status. Adding `pipefail` would have fixed
# nothing; it was already there.
#
# Empty output counts as failure for the same reason.
report() {
    local lines=$1; shift
    local out status
    out=$("$@" 2>/dev/null); status=$?
    if (( status != 0 )) || [[ -z ${out//[[:space:]]/} ]]; then
        printf '   FAILED (the measurement did not run; its numbers are unasserted, its existence is not)\n'
        fail=1
        return
    fi
    printf '%s\n' "$out" | tail -n "$lines"
}

step "clippy (-D warnings, all targets, all features, whole workspace)"
# `--workspace`, and the flag is the whole point of this line.
#
# Without it cargo lints `default-members`, which is `yesno-core` and
# `yesno-arrow` alone — so this step reported "clippy passed" while
# yesno-datafusion, yesno-flight, yesno-server,
# yesno-server-utils and yesno-e2e
# were never looked at. CI's `gate` job has always used `--workspace`, so the
# only thing the gap produced was a green local gate followed by a red CI, and
# it did: a `.into()` in yesno-e2e and a `first.map(Ok).into_iter()` in
# yesno-flight both sat here for days, each visible only to the job nobody runs
# first.
#
# This is the fourth instance of the same class recorded in JOURNAL — a check
# whose success was not evidence that it checked what it claimed. `gate.sh` now
# matches CI exactly, which is the property that makes running it locally mean
# anything. Do not drop the flag to make a satellite's lint failure go away;
# fix the lint, or the gate goes back to being decorative.
check cargo clippy --workspace --all-targets --all-features -- -D warnings

step "tests (workspace)"
check cargo test --workspace

step "tests behind off-by-default features"
# `cargo test --workspace` above is **default features**, and that is not a
# complete run. `yesno-tantivy/tests/flight.rs` is `#![cfg( feature = "flight" )]`
# and `flight` is off by default, so the whole file compiled to nothing in both
# this gate and CI -- one integration test that had **never executed anywhere**
# until it was run by hand on 2026-09-17, when it passed in 0.32 s.
#
# It read as covered because clippy runs `--all-targets --all-features`, so the
# file is type-checked and lint-clean on every run. Compiled is not executed,
# and no line of either summary distinguishes them.
#
# **Targeted rather than `--all-features`, and that is measured, not stylistic**:
#
#     cargo test --workspace                  1556 passed, 70 binaries
#     cargo test --workspace --all-features   1557 passed, 70 binaries
#
# The whole delta is this one test ( measured 2026-09-17, before the `jit`
# feature existed; the line after it covers what that one added ).
# `--all-features` would additionally compile
# `yesno-core/tracing` into the other 69 binaries -- changing the code under
# test everywhere to gain nothing, since that feature gates instrumentation and
# contributes no tests at all.
check cargo test -p yesno-tantivy --features flight
# The same class, found again on 2026-09-22 while enabling the fused bitmap-DAG
# generator on x86_64: `yesno-core/src/jit.rs` is `#[cfg( feature = "jit" )]`
# and `jit` is off by default, so its five tests -- including the property test
# that is the only check on two `unsafe` boundaries -- had never executed in
# this gate or in CI. Clippy's `--all-features` type-checks them, which is
# exactly what made the gap invisible the first time.
check cargo test -p yesno-core --features jit

step "ARCHITECTURE.md layout matches the tree"
# Cheap, and it catches a class review cannot: a diagram that is 90% right reads
# as right. Checks both directions — a file added without a diagram entry is how
# the drift found on 2026-08-25 actually happened, and a one-directional check
# would have passed throughout.
check python3 scripts/check-layout.py

step "the AWS gate's runner scripts parse as POSIX sh"
# The one class of failure in the AWS gate that costs a provisioned EKS cluster
# to discover and has nothing to do with what the gate tests. Each remote step
# is a Python string handed to Systems Manager and run by /bin/sh on the
# instance; nothing between here and there parses it, so a missing `fi` inside a
# string literal survives compilation, `terraform apply`, and fifteen minutes of
# cluster creation. Syntax only — a script that runs the wrong command parses.
check python3 scripts/check-runner-scripts.py

step "the unified image's binary set matches the workspace"
# The binary list is written three times — the crates' `[[bin]]` targets,
# `dist/Dockerfile`'s copy loop, and `dist/entrypoint.sh`'s dispatch arm — and
# nothing but this connects them. A binary added to the first two and not the
# third produces an image that carries it and cannot run it: the name falls
# through to the default arm and silently becomes an argument to `yesnod`, which
# rejects it. That is a confusing failure a long way from its cause, and it
# surfaces in a cluster rather than here.
check python3 scripts/check-image-binaries.py

step "the image entrypoint parses as POSIX sh"
# It is the image's ENTRYPOINT and it is not covered by any Rust test. A syntax
# error is a container that exits immediately with nothing useful on stderr.
check sh -n dist/entrypoint.sh

step "policy R1: no Arrow types in yesno-core's public API"
# The baseline started at six on 2026-08-25 and reached **empty** on 2026-08-28,
# so R1 is now true rather than aspirational. The mechanism stays: it fails on
# new violations and on baseline entries fixed but not removed, so the list can
# only shrink. Do not add an entry to make a change pass.
check python3 scripts/check-r1.py

step "docs/ is self-contained"
# `docs/` holds standing human-facing documents, so a source path written into
# prose goes stale silently — nothing recompiles a sentence. This is the
# opposite of the rule for `.agents/docs/`, whose module diagram check-layout.py
# verifies against the tree on purpose. The baseline started at one reference,
# shrank to empty the same day, and must stay there. Added after §13.9 was left
# asserting that a deleted file exists.
check python3 scripts/check-docs-selfcontained.py

step "cited backlog slugs are recorded"
# The mirror image of the step above. `docs/` must not name source; source *may*
# name a backlog entry, and nothing checked that the entry exists. A sweep on
# 2026-09-14 found 25 such slugs recorded nowhere under `.agents/docs/`, two of
# them the only surviving record of a real design gap. The decay is mechanical:
# `reconcile-journal-ltm` removes a JOURNAL entry once it is consolidated, the
# slug dies with it, and every citation in `src/` survives untouched — reading,
# as it decays, exactly as though the reasoning were still written down.
check python3 scripts/check-todo-refs.py

step "backlog counts agree with the tree"
# **Mechanize the arithmetic; read the judgement.** Three detectors were written
# on 2026-09-16 and two were retired as strictly worse than reading -- one for
# doc comments orphaned by an insertion ( 16 flagged, 8 false ), one for backlog
# entries naming vanished identifiers ( 14 flagged, 14 false, and none of the
# real cases ). This one is kept, and not because it was written more carefully:
# a count has exactly one correct value, re-deriving it is cheaper than reading
# the sentence that states it, and a disagreement is a fact rather than a
# judgement. "Does this claim still follow" has none of those properties.
#
# The subject is `miri-cannot-reach-the-mmap-unsafe-sites`, which has quoted a
# wrong `unsafe` count **twice** -- two sites when restored, corrected to 25 the
# same day, found to be 37 two days later. Its conclusion was never wrong and
# its file list was right throughout; only the arithmetic decayed, and it decays
# whenever a SIMD arm is filled in. Correcting it was demonstrably not enough:
# it had already been corrected once, by someone paying attention, and rotted
# again inside two days.
check python3 scripts/check-unsafe-count.py

step "doc links resolve"
# The compiled-language twin of the step above, and the cheapest check in this
# gate: rustdoc already resolves every intra-doc link and nothing was reading
# its output. A sweep on 2026-09-14 found **11** broken or ambiguous links
# workspace-wide -- `SKETCH_MAX_PREFIXES` ( a constant that has never existed ),
# `Expr::not`, `ChunkRef::cell`, `OrdSet::view_select`, `WalBatch` and others --
# pointing at symbols that were renamed, made private, or never named that.
#
# **Widened to `-D warnings` on 2026-09-17**, because the 38 decisions this
# comment used to defer have been made. The set was `private_intra_doc_links`
# ( a public doc linking to a private item ) plus 6 `redundant_explicit_links`;
# the judgment went the same way at every private-link site but one -- **demote
# the link to plain backticks rather than widen the API**, because making a
# `pub(crate)` kernel `pub` to satisfy rustdoc buys a semver promise for
# something nothing outside the crate calls. The exception was repointed
# instead: `yesno-flight`'s `visibility_wait` is a private field with a public
# builder, so the link now names the builder and is more useful than before.
#
# Two things to know if this ever reddens in bulk again. **`cargo doc` fails
# fast per crate**: the first measurement said 2 workspace-wide because
# `could not document yesno-flight` aborted the run before `yesno-e2e` was
# reached. Iterate to a fixed point; do not trust one pass's count. And **a
# find-and-replace on link text is wrong here** -- `[\`apply_words\`]` occurs
# three times in `ops/bitmap.rs` and only one is an error, the others being
# docs on private items where the link resolves fine. Fix the sites rustdoc
# names, by file and line, and re-run.
check env RUSTDOCFLAGS="-D warnings" \
    cargo doc --workspace --no-deps

step "docs/ converts to TeX"
# The only failure mode in this gate with no author-side signal at all: a bare
# math symbol in prose renders correctly in every Markdown viewer, in the
# terminal, in review and in git diff, and fails only under
# `pandoc --pdf-engine=pdflatex`, which nobody runs. So the author gets nothing
# and someone else finds it later.
#
# Verified against the real toolchain rather than against the checker's own
# model: on 2026-08-26 pandoc built `docs/` clean while the checker passed, and
# an injected bare U+2227 made pandoc fail while the checker flagged it at the
# exact line and column. A math-mode `$\wedge$` passes both.
#
# Covers `docs/` only. `.agents/docs/` is never converted and uses its own
# emphasis markers freely.
check python3 scripts/check-tex-safe.py

step "Theorem 7's termination measure"
# `docs/formal-model.md` Theorem 7 proves the planner terminates under a weighted
# measure. The proof obligation is a loop over `pass_b`'s rewrite outcomes, so it
# is written as one rather than as prose.
#
# This is here because THREE published versions of that theorem were wrong,
# each repaired against only the counterexample that killed its predecessor. The
# checker pins a hash of `pass_b`, so editing the planner's rewrite rules makes
# it go red until the transcription is re-audited. That is the intended
# behaviour, not a nuisance: a rule added without re-auditing is exactly how the
# theorem silently stops being true.
check python3 scripts/check-plan-measure.py

step "the article's derived figures still follow from the constants"
# Several numbered statements in `docs/formal-model.md` are arithmetic over named
# constants — Prop 5's amortised bound, Prop 5''s switch cap and two-cycle proof,
# Cor 14.1 and Prop 14.2's amplification figures, §8.3's A/B delay, §13.1's index
# floor. If a constant moves, those numbers go quietly false: structurally right,
# numerically wrong.
#
# Unlike the hash pin above, this one *recomputes*, and it binds each figure to
# **named sites matched inside their own section**, because a value like `1.53`
# renders in five places and a whole-article search goes green once any one of
# them is corrected.
#
# `--self-test` runs first and is not optional. It asserts that each source
# input reddens EXACTLY its dependent site list — not merely one site, which is
# how two earlier coverage regressions hid — and includes the false-green controls
# a weaker design cannot see: staling one site while the same role phrase is
# added elsewhere, and relocating a site's token. It corrupts an in-memory copy
# and never writes the article.
check python3 scripts/check-model-constants.py --self-test
check python3 scripts/check-model-constants.py

step "rustfmt"
# Whole-tree, because the baseline is gone ( `fmt-baseline`, closed 2026-08-27 ).
#
# `yesno-core/fuzz` is **outside `[workspace] members`**, so `cargo fmt --all`
# cannot see it. It had 3 unformatted hunks of its own that were invisible to
# every check for exactly that reason. Checked separately here; a residue that
# no gate can reach is how a baseline grows back.
check cargo fmt --all -- --check
# `$1`, not `$0`: with `bash -c SCRIPT -- ARG`, the `--` becomes `$0` and ARG
# becomes `$1`. Using `$0` here made this `dirname --`, which is "missing
# operand", so the step failed unconditionally — with the gate reporting a
# formatting failure while `yesno-core/fuzz` was in fact clean.
check bash -c 'cd yesno-core/fuzz && cargo fmt --all -- --check'

if [[ $deep -eq 0 ]]; then
    printf '\n\033[1m== skipped (use --deep)\033[0m\n'
    printf '   Valgrind, sanitizers, fuzz targets, aged-state measurement\n'
    verdict "$EXPECT_STEPS"
fi

# The MIRI step stood here until 2026-08-29. It is gone, not moved: no gate
# runs `scripts/miri.sh` any more. The `unsafe` sites it never covered are the
# ones below, which is why removing it costs less coverage than it appears to.
step "Valgrind (heap errors; NOT the mmap sites -- see below)"
# **This step was labelled "the mmap sites, and the crate's only two unsafe
# blocks" until 2026-09-14, and both halves were false.** There are 25 `unsafe`
# blocks across six files, not two -- the NEON arms landed after that label was
# written. And Valgrind **cannot see** the mmap sites: measured that day with a
# heap control in the same binary, an overread 1000 bytes past a 16-byte slice
# that stays inside a 4096-byte file mapping is silent under both Valgrind and
# AddressSanitizer, while a heap overread is caught by both. A mapping is not an
# allocation either tool tracks.
#
# The step is kept because it does cover the heap-resident paths, which is real
# and has found real defects. What it does not do is what its name claimed.
# The mmap sites are covered by `SegmentedMmap`'s containment check, the safe
# slice index that forms the pointer, the I2/I6 invariants, and -- since
# 2026-09-14 -- the read-path checksum cache.
check ./scripts/valgrind.sh

step "AddressSanitizer"
# Covers the concurrency suite, which Valgrind skips because it serialises
# threads onto one core.
#
# `--lib --tests`, i.e. **everything except doctests**, and this is a scope
# fix rather than a weakening — the doctests do not *fail* here, they cannot
# *link*. rustdoc compiles each example with these RUSTFLAGS but links it against
# dependency rlibs ( `num-bigint`, `num-traits` ) built without the sanitizer
# runtime, so the link dies on `undefined reference to __asan_report_load8`
# before any code runs. No amount of correct code makes that succeed.
#
# Nothing is lost: `cargo test --workspace` in the routine gate runs every
# doctest unsanitized on every invocation, and a documentation example is not
# where a memory-safety bug lives. What ASan is here for — the mmap sites, the
# crash matrix, the concurrency suite — is `--tests`, and all of it still runs.
#
# Found 2026-08-30 by running `--deep` to completion for the first time in a
# while: 813 unit tests and every integration suite passed under ASan, and the
# step still reported FAILED on six doctests in `bignum/`, `matrix/`, `pack/`
# and `view/`. Do not "fix" this by deleting those doctests.
check env RUSTFLAGS="-Zsanitizer=address" \
    cargo +nightly test -p yesno-core --lib --tests \
    --target aarch64-unknown-linux-gnu

step "ThreadSanitizer (concurrency suite)"
# Needs `core` rebuilt with the same flag, hence `-Zbuild-std` and `rust-src`.
# It is the only tool here that looks for data races: Valgrind serialises the
# threads away and ASan does not check for races.
check env RUSTFLAGS="-Zsanitizer=thread" \
    cargo +nightly test -p yesno-core -Zbuild-std \
    --target aarch64-unknown-linux-gnu --test concurrency

# The measurement fixtures moved out of `yesno-core/examples/` on 2026-08-26 and
# are `e2e/scenarios/*.py` now. Their assertions already ran under
# `cargo test --workspace` above, at a gate-sized corpus; what these two lines
# add is the *corpus the numbers were recorded at*, which is too slow to be a
# routine gate and is the only size the recorded figures can be compared to.
# `--show-output` is what makes a passing fixture print its table at all.
step "aged-state space measurement (numbers reported, the run asserted)"
report 14 cargo run --release -p yesno-e2e -- --show-output \
    --arg keys=200 --arg per_key=2000 --arg rounds=60 \
    e2e/scenarios/aged_state.py
report 14 cargo run --release -p yesno-e2e -- --show-output \
    --arg keys=400 --arg per_key=400 --arg rounds=40 --arg spread=100000 \
    e2e/scenarios/aged_state.py

step "WAL bytes per ordinal (numbers reported, the run asserted)"
# The ceilings are asserted in tests/durability.rs; this shows the shape curve.
report 5 cargo run --release -p yesno-e2e -- --show-output \
    --arg n=100000 e2e/scenarios/wal_size.py

step "two-node failover drill (real processes, real ports, mutual TLS)"
# The only thing that exercises the **shipped artifacts** rather than the code:
# the `yesnod`, `yesno`, and `yesnoctl` binaries, `dist/`'s example configs, and the
# certificate script, all fitting together. Everything above tests the library;
# this tests what an operator actually touches.
#
# It belongs in `--deep` and not the routine gate because it starts two
# processes and binds four loopback ports, which is a different failure surface
# from everything else here — a busy port fails it for a reason unrelated to the
# change under test. `e2e/scenarios/failover.py` and `yesno-server/tests/` cover the
# same code in one process on every run.
#
# It has already earned this: on 2026-08-29 it caught an example config that
# `--check-config` refuses, a principal table that silently closed the client
# port, and a SIGPIPE interaction that killed the script. None of those are
# reachable from a library test.
check ./yesno-server/dist/two-node.sh

printf '\n\033[1m== not run even by --deep\033[0m\n'
printf '   fuzz targets: cd yesno-core && cargo +nightly fuzz run decode_container\n'
printf '   they need a time budget, so they take an argument rather than a default\n'

verdict "$EXPECT_STEPS_DEEP"
