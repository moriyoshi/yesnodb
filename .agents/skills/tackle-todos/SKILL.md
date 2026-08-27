---
name: tackle-todos
description: "Read TODO.md and scan source code for TODO/FIXME comments, build a consolidated list, then work through as many items as possible, dispatching parallel agents for independent units."
user_invocable: true
allowed-tools: Bash, Read, Write, Edit, Grep, Glob, Agent
---

# Tackle TODOs: Consolidate and Resolve Open Items

This skill scans the project for outstanding work items — both from `.agents/docs/TODO.md` and from `// TODO` / `// FIXME` comments in source — builds a consolidated, deduplicated, prioritized list, and then works through as many items as possible.

**Use this skill when:** you want to make a focused sweep of outstanding TODOs and fix them in bulk.

## Arguments

- `[filter]` (optional): A module or keyword to restrict which TODOs to tackle (e.g., `codec`, `stream`, `bench`). If omitted, all TODOs are considered.

## Step 0: Collect TODOs from TODO.md

Read `.agents/docs/TODO.md` and extract every unchecked `- [ ]` item. Record its slug, description, and source.

## Step 1: Scan Source Code for TODO/FIXME Comments

Use Grep to search `yesno-core/` for `TODO` and `FIXME` in `*.rs` files, including `tests/` and `benches/`.

Group and deduplicate the results. Comments that are informational-only ( noting a known limitation that cannot be fixed without a design change ) should be flagged but deprioritized rather than dispatched.

## Step 2: Build a Consolidated TODO List

Merge the two sources into a single list. Deduplicate items that appear in both `TODO.md` and as code comments.

For each item, assign a category:

| Category | Description | Priority |
|----------|-------------|----------|
| **correctness** | A wrong result, a violated invariant, or a missing bounds check | High |
| **invariant-gap** | An invariant that holds by convention but is not enforced or tested | High |
| **test-gap** | A behaviour reachable from the public API with no coverage in the layer that could catch its failure ( QG §3 ) | High |
| **performance** | A known slow path with a benchmark to prove it | Medium — must follow QG §4 |
| **format** | Codec or serialization fidelity, foreign-file compatibility | Medium |
| **ergonomics** | API shape, naming, doc comments | Low |
| **design** | Requires a real design decision ( new abstraction, scope change, dependency ) | Deferred — flag for the user |

Write the consolidated list to `.agents-workspace/tmp/consolidated-todos.md` for reference.

## Step 2b: Verify stale items before dispatch

Before working any `TODO.md` entry that is more than a few days old, verify it is still applicable: grep for the symptom ( the missing check, the materializing call, the absent test ). Close stale entries instead of dispatching work on them.

## Step 3: Filter (if argument provided)

If the user passed a `[filter]` argument, restrict the work list to items matching that filter ( module path, symbol, or keyword ).

## Step 4: Plan Work Units

Group the consolidated TODOs into independent work units. Each work unit should:

- Be self-contained ( one module, or one cross-cutting concern )
- Not conflict with other units — **no two units may edit the same file**

This workspace is one crate, so file-level conflicts are the binding constraint, not crate-level ones. In practice that means at most a handful of genuinely parallel units. Sequential is often correct here; do not manufacture parallelism.

Present the plan to the user and get confirmation before dispatching.

## Step 5: Dispatch

For each approved work unit that is genuinely independent, launch an Agent ( `subagent_type: general-purpose` ) with a prompt containing:

1. The specific TODO(s) to address
2. The files it owns, and an explicit instruction not to touch any other file
3. The expected behaviour, with the relevant `ARCHITECTURE.md` invariant quoted
4. Which test layer must cover the change ( QG §3 ), and the instruction to run the full gate before returning:
   `cargo fmt --check && cargo clippy --workspace --all-targets --all-features -- -D warnings && cargo test -p yesno-core`
   ( `--workspace` is load-bearing: without it clippy lints `default-members` only and never sees the six satellite crates )
5. The instruction to include the gate result in its final report

Launch independent units in a single batch so they run concurrently.

Never use `isolation: worktree` for parallel agents here — the workspace is small, the merge cost outweighs the isolation benefit, and it has repeatedly caused trouble. Batch file-disjoint tasks instead.

## Step 6: Collect Results and Update TODO.md

After all units complete:

1. Review each result — did clippy, fmt, and the test suite actually pass? Do not take "done" at face value; re-run the gate yourself once at the end.
2. For resolved items, remove them from `.agents/docs/TODO.md` and delete the corresponding `// TODO` comments from source.
3. For unresolved items, add a note about what was attempted and why it stalled.
4. Append a summary to `.agents/docs/JOURNAL.md` documenting what was tackled and the outcomes.

## Notes

- **Do not attempt design-category items** without user approval. Those need an architectural decision.
- Respect `AGENTS.md`: no `git checkout`, no `git restore`, no discretionary commits. Agents edit files; they do not commit.
- `--maxfail` is a pytest flag, not a Rust libtest flag. Use `--no-fail-fast` or a name filter with `cargo test`.
- Never resolve a `test-gap` item by weakening the oracle or the property that exposed it.
