---
name: write-tests
description: "Plan and write tests for a module or behaviour in yesno-core, choosing the layer that could structurally catch the failure — oracle property, roaring differential, expression equivalence, or allocation budget — rather than defaulting to a hand-written unit test."
argument-hint: "<module-or-behaviour>"
user_invocable: true
allowed-tools: Bash, Read, Write, Edit, Grep, Glob
---

# Write Tests

Plan and implement tests for a module or behaviour in `yesno-core`. The point of this skill is layer selection: this crate has four test layers that each catch a failure class the others structurally cannot, and picking the wrong one produces a green suite that proves nothing.

## Arguments

- `$0` — A module path (`container::run`, `ops::card`, `stream::ops`, `roaring_format`) or a behaviour description ("bitmap demotion hysteresis", "seek-driven AND").

---

## Step 0: Orient

Read, in this order:

1. `.agents/docs/ARCHITECTURE.md` § Invariants and the section covering the target module
2. `.agents/docs/QUALITY_GATE.md` §3 ( test layer selection )
3. The module's own `//!` comment — it usually states the property that matters
4. The existing tests for the area

```bash
grep -rn "{module}" yesno-core/tests/ yesno-core/src/ --include=*.rs | head -40
```

## Step 1: Identify the Failure Classes

Before writing anything, list what could actually go wrong. For this crate the recurring classes are:

| Class | What it looks like | Layer that catches it |
|-------|--------------------|-----------------------|
| **Wrong contents** | The set holds the wrong ordinals | `proptest_oracle.rs` vs `BTreeSet` |
| **Wrong bytes** | Contents correct, serialization differs from the spec | `differential.rs`, byte-level, both directions |
| **Silent decay** | A non-materializing path quietly starts materializing | `allocation.rs` budget |
| **Path divergence** | Two implementations of the same query drift apart ( lazy vs eager, `cardinality()` vs `collect_set().len()` ) | `expr_equivalence.rs` |
| **Invariant violation** | Empty container in a set, adjacent runs, stale cached `len`, prefix ≥ 2^48 | `assert_invariants` in `proptest_oracle.rs` |
| **Panic on hostile input** | `decode` panics instead of returning `CodecError` | `deserialize_never_panics` property |
| **Untested by construction** | The generator never produces the shape the code handles | Generator design, not a new test |

The last row is the one people miss. A test that never builds a bitmap container cannot fail on a bitmap bug, no matter how many cases it runs.

## Step 2: Choose the Layer

Map each failure class to its layer using the table above and QG §3. Prefer *extending an existing property* over adding a new standalone test: a new arm on `set_ops_match_oracle` is worth more than a new `#[test]` with three hand-picked inputs.

Add a plain `#[test]` only for:

- A specific regression with a known reproducer ( put the reproducer in the test name )
- A boundary the generators cannot reliably hit ( `u64::MAX`, `(1 << 48) - 1`, exactly `ARRAY_MAX`, exactly `BITMAP_DEMOTE` )
- A compile-time or type-level property ( `Send + Sync` bounds )

## Step 3: Design the Generator

If the work needs a new proptest strategy, design it to be **boundary-biased**. Uniform random `u64`s put one ordinal in each chunk: arrays never fill, bitmaps never appear, runs are never produced.

The existing strategies show the three shapes worth having:

- `clustered_ordinals()` — a handful of chunk prefixes ( including `0` and `(1 << 48) - 1` ), each densely populated, so containers actually fill and promote.
- `runny_ordinals()` — long contiguous stretches, so run containers get built and `optimize()` has something to do.
- `any_ordinals()` — the mix.

When adding one, state in a comment what shape it exists to produce and what would go untested without it. Then verify it actually produces that shape:

```bash
cargo test -p yesno-core {new_property} -- --nocapture
```

Temporarily assert the shape ( e.g. that some container has `kind() == ContainerKind::Bitmap` ) to prove the generator reaches it, then remove the scaffold.

## Step 4: Write the Test Plan to JOURNAL.md

Append a plan before implementing:

```markdown
## {YYYY-MM-DD} — Test plan: {module or behaviour}

### Failure classes

| Class | Concrete failure | Layer |
|-------|------------------|-------|
| ... | ... | ... |

### Planned tests

- `tests/{file}.rs::{name}` — {what it asserts, and what it would catch}

### Generators

{new or modified strategies, and the shape each exists to produce}

### Deliberately not covered

{what this pass leaves alone, and why}
```

The "deliberately not covered" section matters. Silent gaps read as coverage.

## Step 5: Implement

### Property tests ( `proptest_oracle.rs`, `expr_equivalence.rs` )

- Assert against the oracle, never against a hand-computed expectation.
- Call `assert_invariants(&set)` after any mutation sequence.
- Keep the property statement in the test name: `rank_select_are_mutually_inverse`, not `test_rank_2`.
- Set `ProptestConfig` case counts deliberately; the default is fine for cheap properties and too slow for ones that build large bitmaps.

### Differential tests ( `differential.rs` )

- Both directions, always: we parse what `roaring` writes, and `roaring` parses what we write.
- For byte-level assertions, compare full `Vec<u8>` and print the first differing offset on failure — a bare `assert_eq!` on two 8 KiB vectors is unreadable.
- Cover the run-encoded case explicitly. The offset-header rule differs between cookies, and small run-encoded bitmaps are exactly where it goes wrong.
- Use the deterministic `lcg` helper rather than adding an RNG dev-dependency.

### Allocation tests ( `allocation.rs` )

- Wrap the measured region in `count_allocs`, and keep the region tight — setup allocations inside the closure make the budget meaningless.
- Budgets are upper bounds with a little headroom, not exact counts. Comment what the bound represents ( "one Vec per chunk would be N; this asserts we do none of that" ).
- Counters are thread-local by design so the suite does not need `--test-threads=1`. Do not make them global.

### Unit tests in `src/`

- Only for module-internal helpers not reachable from the public API.
- Anything reachable from `OrdSet`, `Container`, `Expr`, or `roaring_format` belongs in `tests/`, exercised the way a caller would reach it.

## Step 6: Verify the Test Can Fail

A test that has never failed is a hypothesis, not a test. Before finishing, break the code deliberately and confirm the new test catches it:

- For a property: invert a comparison, or skip the last element of a merge.
- For a differential test: change one byte of the payload writer.
- For an allocation budget: insert a `Vec::with_capacity(1)` in the measured path.
- For an invariant check: construct the violating state directly.

Undo the break. Note in your report what you broke and that the test caught it. If it did not catch it, the test is measuring the wrong thing — redesign it.

## Step 7: Run the Gate

```bash
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test -p yesno-core
```

`--workspace`, not a bare `cargo clippy`: without it only `yesno-core` and `yesno-arrow` are linted. And use `cargo fmt`, never bare `rustfmt <file>` — rustfmt follows every `mod x;`, so naming one file rewrites seven.

If a new property found a real bug, the bug fix and the test land together, and the `JOURNAL.md` entry records which layer caught it — that data is what keeps the layer table in QG §3 honest.

## Step 8: Report

- Tests added, and the failure class each covers
- Generators added or changed, and the shape each produces
- The deliberate break used to verify each test can fail
- Any bug the new tests found
- Anything left uncovered, added to `.agents/docs/TODO.md`

## Notes

- New proptest failures write seeds into `tests/*.proptest-regressions`. Keep them — they are a checked-in regression corpus.
- `--maxfail` is a pytest flag. For `cargo test`, use `--no-fail-fast` or a name filter.
- Never adjust an oracle, a property, or a budget to make a test pass. If the test is genuinely wrong, say so explicitly and record the reasoning in `JOURNAL.md`.
