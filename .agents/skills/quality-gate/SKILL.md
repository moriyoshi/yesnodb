---
name: quality-gate
description: Check a change ( or a module ) against the quality gate in .agents/docs/QUALITY_GATE.md, then fix every failure. Runs build/clippy/fmt/tests, audits invariants, test-layer coverage, buffer discipline, codec fidelity, and unsafe usage, reports pass/fail per check, and works through the failures until it passes.
argument-hint: "[module-or-scope]"
user_invocable: true
allowed-tools: Bash, Read, Write, Edit, Grep, Glob
---

# Quality Gate Check and Fix

Evaluate work against the gate defined in `.agents/docs/QUALITY_GATE.md`, then fix every failure until it passes.

## Arguments

- `$0` (optional) — Scope to audit. Either a module path (`container`, `stream`, `ops`, `codec`, `roaring_format`, `buffer`, `set`) or omitted, in which case the scope is the current uncommitted change set ( `git status --short` and `git diff` ), falling back to the whole crate if the tree is clean.

---

## Procedure

Work through the sections below in order. For each check, emit a verdict line:

```
QG-{N}.{M}: {short description}
QG-{N}.{M}: {short description} — {reason}
QG-{N}.{M}: {short description} — {reason}
```

Use for pass, for a non-blocking observation, for a failure. Print a summary table at the end.

---

### Step 1: Read the gate document

Read `.agents/docs/QUALITY_GATE.md` in full, and the § Invariants section of `.agents/docs/ARCHITECTURE.md`. Do not work from memory of them.

### Step 2: Determine scope

```bash
git status --short
git diff --stat
```

List the files in scope. If the tree is clean and no argument was given, say so and audit the whole crate.

### Step 3: Baseline commands (QG §1)

```bash
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test -p yesno-core
```

**`--workspace` is load-bearing.** Without it clippy lints `default-members` — `yesno-core` and `yesno-arrow` alone — and reports success having never looked at the other six crates. See `gate-clippy-saw-two-crates` in `JOURNAL.md` ( closed ).

Report each. For any `#[allow(...)]` introduced in scope, check there is a comment explaining why the lint is wrong there.

The formatting baseline was closed on 2026-08-27, so `cargo fmt --check` passes tree-wide and a bulk reformat is a no-op. Any diff `cargo fmt` produces is yours. `yesno-core/fuzz` is outside `[workspace] members` and `cargo fmt --all` cannot reach it — check it separately if you edited it.

### Step 4: Invariant conformance (QG §2)

Read the changed code and check, explicitly, each of:

- **QG-2.1** No empty container can reach an `OrdSet`. Any new mutation or construction path drops a chunk that became empty.
- **QG-2.2** `len` is maintained incrementally. Grep the scope for cardinality computed by iteration ( `.iter().count()`, `.map(|c| c.len()).sum()` inside a hot path ) where a cached value exists.
- **QG-2.3** Run containers stay sorted, non-overlapping, and non-adjacent. Adjacent intervals are merged, never stored as two runs.
- **QG-2.4** Every `Prefix48` is `< 1 << 48`. Anything building a prefix by arithmetic rather than `split()` has a check.
- **QG-2.5** Container size-class rules hold: array `card <= ARRAY_MAX`, bitmap exactly `BITMAP_WORDS` words, run `nruns <= RUN_MAX_INTERVALS` on write.

### Step 5: Test-layer coverage (QG §3)

For each behavioural change in scope, name the layer that would catch its failure, using the QG §3 table. Then verify that coverage actually exists — open the test file and find it. A change with no covering test in the right layer is a , not a .

Specifically check:

- **QG-3.1** A new or modified `cardinality_dyn` override has a matching budget assertion in `tests/allocation.rs`.
- **QG-3.2** A change to byte layout has both directions covered in `tests/differential.rs` ( we parse theirs, they parse ours ).
- **QG-3.3** A new proptest strategy is boundary-biased ( clusters ordinals into few chunks, or produces runs ). A uniform `u64` strategy is a at best.
- **QG-3.4** `tests/*.proptest-regressions` has not lost entries. Check `git diff` for deletions.
- **QG-3.5** No oracle was weakened, no property loosened, no allocation budget raised. Any such diff is an automatic unless the run explicitly justifies it and records the reasoning.

### Step 6: Kernel specialization discipline (QG §4)

Skip with a note if no `ops/` arm was specialized. Otherwise:

- **QG-4.1** A benchmark motivating this specific arm exists and its numbers are recorded.
- **QG-4.2** The arm is differential-tested against `ops::generic`, not only against `BTreeSet`.
- **QG-4.3** The generic path is still reachable and correct for that arm.

### Step 7: Buffer and zero-copy discipline (QG §5)

```bash
grep -rn "arrow_buffer" yesno-core/src/ --include=*.rs | grep -v "^yesno-core/src/buffer.rs"
```

- **QG-5.1** The grep is empty — `arrow_buffer` is named only in `buffer.rs`.
- **QG-5.2** No `into_mutable()` call was introduced in `to_mut` / `words_mut` or their callers.
- **QG-5.3** `Container` is still `'static + Clone + Send + Sync`. If unsure, add a temporary `const _: fn() = || { fn a<T: Send + Sync + Clone + 'static>() {} a::<Container>(); };` to confirm, then remove it.

### Step 8: Codec and format fidelity (QG §6)

- **QG-6.1** `codec::decode` still returns `Err` rather than panicking for every input class. `assert!`, `unwrap`, `expect`, and slice indexing introduced into `decode` are findings.
- **QG-6.2** `deserialize_never_panics` still passes.
- **QG-6.3** The offset-header rule is only computed in `roaring_format::has_offsets`; grep for `NO_OFFSET_THRESHOLD` used elsewhere.
- **QG-6.4** Payload bytes remain spec-identical — `differential.rs` passes.

### Step 9: Unsafe code (QG §7)

```bash
grep -rn "unsafe" yesno-core/src/ --include=*.rs
```

For each `unsafe` block in scope: is there a `// SAFETY:` comment naming the invariant, a `JOURNAL.md` note, and a property test that would fail if the invariant were violated? Is `#![deny(unsafe_op_in_unsafe_fn)]` still in `lib.rs`?

### Step 10: Documentation conformance (QG §8)

- **QG-8.1** Module `//!` comments still describe actual behaviour. If the change altered something a `//!` block explains, the block was updated in the same change.
- **QG-8.2** New public items have doc comments; new constants explain their value, not just state it.
- **QG-8.3** Repo-authored docs use half-width parentheses and colons. Grep the changed docs for `（`, `）`, `：`.

### Step 11: Journal and backlog hygiene (QG §9)

- **QG-9.1** A `JOURNAL.md` entry exists for anything a future agent would want to know.
- **QG-9.2** No existing `JOURNAL.md` section was edited or deleted.
- **QG-9.3** Follow-ups are in `TODO.md`, not only as `// TODO` comments.

---

## Phase A Output: Initial Report

```markdown
# Quality Gate Report: {scope}

## Summary

| Section | Result |
|---------|--------|
| 1 Baseline commands | PASS / FAIL |
| 2 Invariants | ... |
| ... | ... |

## Verdict: PASS / FAIL

## Details

{verdict lines with reasons}
```

---

## Phase B: Remediation

### Step 12: Build a work list

Turn every into a concrete task. Order them: correctness and invariants first, then test gaps, then discipline and documentation. Present the list.

### Step 13: Work through it

Fix each item. After each fix, re-run the narrowest command that proves it ( a single test by name while iterating ). Do not batch-fix and re-run once at the end — a fix that breaks an earlier fix is much cheaper to find immediately.

Never close a test-gap item by changing the test's expectations. If a test looks wrong, stop and say so.

### Step 14: Re-run the whole gate

Re-run every check from Phase A, not only the ones you touched.

### Step 15: Final verification

```bash
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test -p yesno-core
```

All three clean, or the verdict is FAIL and you say so plainly.

---

## Phase C: Record to JOURNAL.md

Append:

```markdown
## {YYYY-MM-DD} — Quality Gate: {scope}

### Result: {PASS / FAIL (with N deferred)}

### Findings

{what the gate caught, and which check caught it}

### Remediation

{what changed}

### Deferred Items

{anything left, with the reason — also add these to TODO.md}
```

If the gate caught a failure class the test suite structurally could not have caught, that is the most valuable thing in the entry. Say what it was and consider whether a new check belongs in `QUALITY_GATE.md`.

---

## Recurring failure modes

Keep this list current as the gate finds things repeatedly:

- A "fast path" that quietly materializes — passes every correctness test, caught only by an allocation budget.
- A run container built with adjacent intervals — round-trips fine, differs from the `roaring` crate's bytes.
- A cached `len` updated on the common branch but not the early-return branch.
- A proptest strategy that looks random but never fills a container.
