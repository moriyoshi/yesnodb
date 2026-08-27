# Documents for both humans and coding agents

* [README.md](./README.md) ... the human-facing description of the crate: what it is, how to run the server, and an honest list of what is not built. It owns that description — do not restate deployment guidance in `.agents/docs/` and let the two drift.
* [docs/operations.md](./docs/operations.md) ... the operator guide: configuration, backup, restore, replication, promotion, measured RPO/RTO, and disaster recovery.

# Documents for coding agents

* [./.agents/docs/OVERVIEW.md](./.agents/docs/OVERVIEW.md) ... project overview: what `yesno` is, what it deliberately is not, and the milestone gates.
* [./.agents/docs/ARCHITECTURE.md](./.agents/docs/ARCHITECTURE.md) ... module map, data model, invariants, and the design policies that constrain changes.
* [./.agents/docs/QUALITY_GATE.md](./.agents/docs/QUALITY_GATE.md) ... the checklist a change must pass before it is reported as done, plus implementation conventions.
* [./.agents/docs/TESTING.md](./.agents/docs/TESTING.md) ... the end-to-end harness in depth: the verb surface, the handle model, what may and may not go in `scenarios/`, and how to time a fixture without timing `monty`.
* [./.agents/docs/JOURNAL.md](./.agents/docs/JOURNAL.md) ... findings, insights, and peer code review history. Append-only.
* [./.agents/docs/LTM/INDEX.md](./.agents/docs/LTM/INDEX.md) ... long-term memory index for durable project knowledge under `./.agents/docs/LTM/`.
* [./.agents/docs/TODO.md](./.agents/docs/TODO.md) ... open to-do items extracted from JOURNAL.md during `good-sleep` consolidation. Check and update this file when picking up or finishing work.

# Rules and protocols

## General

* Before changing anything in `yesno-core/src/`, read `./.agents/docs/ARCHITECTURE.md`. This crate encodes a lot of deliberate decisions ( size classes, hysteresis thresholds, the generic-kernel-as-oracle policy, the `arrow-buffer` containment policy ). Most of them are recorded in module-level `//!` comments, and those comments are load-bearing documentation, not decoration.
* Never "fix" a constant in `yesno-core/src/lib.rs` ( `ARRAY_MAX`, `BITMAP_DEMOTE`, `RUN_MAX_INTERVALS`, `OPT_GAIN_NUM` / `OPT_GAIN_DEN`, `GALLOP_RATIO` ) without reading the rationale next to it and recording the reasoning for the change in `JOURNAL.md`. Several of them look like tuning knobs and are not.
* The `roaring` crate is a **dev-dependency oracle**, not a runtime dependency. Do not reach for it in `src/`.

## File Management

* When you'd make summary documents for your work, be sure to write them under `./.agents/docs`, not under `/tmp`.
* Temporary files ( scratch scripts, corpora, profiling output, generated `.roaring` fixtures ) should be created under `./.agents-workspace/tmp`, not under `/tmp`.
* Do not leave built binaries, flamegraphs, or `perf.data` inside the version-controlled tree. Put them under `./.agents-workspace/tmp`.
* Never delete user files without permission. Only safe to delete: files YOU created in THIS session that are in `./.agents-workspace/tmp/`. Always ask first if unsure. Assume all pre-existing files belong to the user.

## Research and Measurement Code

* **Do not add code to `yesno-core/src/` to study a question the crate does not itself answer at runtime.** Instruments, cost models, corpus histograms, encoding simulations and benchmark scaffolding are *research*, and research does not ship. `src/` is production; a `pub mod` there is public API, which R1 / R6 / R7 turn into a semver promise you then have to keep for something nothing calls.
* **Where it goes instead**, in order of preference:
  * A **one-off question** → a standalone crate under `./.agents-workspace/tmp/` with a path dependency on `yesno-core`. This costs about ten lines of `Cargo.toml`, builds in seconds, and reaches the whole public API.
  * A measurement that **a gate should keep running** → `e2e/scenarios/`, which is where the migrated fixtures already live.
  * A **kernel timing** → `yesno-core/benches/`.
* **The finding is the deliverable, not the instrument.** Record the numbers, the construction that produced them, and the reasoning under `./.agents/docs/LTM/` — that is what a later session needs, and it survives the code being deleted. A number recorded without its construction cannot be re-derived, only re-measured.
* Do not keep an instrument in `src/` on the grounds that a future question might want it. If that question arrives, rebuilding from a recorded derivation is cheap; carrying unused public API until then is not.
* **The precedent**: `yesno-core/src/stats.rs` was a 1 600-line `( m, r )` histogram and encoding cost model with **zero callers anywhere in the workspace** — it existed to gate one decision, that decision was made, and it was deleted on 2026-08-28. Its full source is preserved at [`./.agents/docs/LTM/removed-stats-instrument-source.md`](./.agents/docs/LTM/removed-stats-instrument-source.md) and its findings in `LTM/compression-models-and-space-economics.md`. Deleting it also forced edits to `lib.rs` and to ARCHITECTURE's module diagram, because `scripts/check-layout.py` verifies that diagram against the tree in both directions — so the cost of removal is never just the file.

## Self-Contained `docs/`

* **Documents under `docs/` must never depend on source code outside that directory.** No `yesno-*/src/...` paths, no `yesno-*/{benches,tests,examples}/...`, no `scripts/...`. `docs/` holds standing, human-facing documents; the tree moves underneath them and a path written into prose goes stale silently, with nothing to catch it.
* **This is the opposite of the rule for `.agents/docs/`**, which is *supposed* to name source — `ARCHITECTURE.md` carries a module diagram that `scripts/check-layout.py` verifies against the tree in both directions. Agent docs track the code; `docs/` must stand on its own.
* State the *result* rather than the location. "An instrument evaluates the bound per chunk" survives a refactor; "the instrument exists ( `yesno-core/src/stats.rs` )" became false the day that file was deleted.
* Enforced by `scripts/check-docs-selfcontained.py`, which runs in the gate. Its baseline is **empty** and `docs/` is clean. The baseline mechanism is kept so the list can only ever shrink: it fails on new references, and on baseline entries that have been fixed but not removed. Do not add entries to make a change pass.
* The checker matches **file paths**, not Rust symbol paths like `stream::plan::pass_b`. A symbol reference is a weaker form of the same coupling and is left to review.

## Building

* Plain `cargo` is the supported entry point for **the cargo workspace**. There is no agent cargo wrapper — the workspace is one small crate with four dependencies, so a cold build is cheap and per-session target directories would cost more than they save.
* **`yesno-pg` is the one exception, and it is built by Bazel.** It produces a `cdylib` that PostgreSQL `dlopen`s, and such a library is only meaningful against one specific server ABI — same major, same `BLCKSZ`, same configure flags. Cargo cannot pin that: `cargo pgrx init` obtains headers and `pg_config` by building PostgreSQL into `~/.pgrx`, so the build depends on machine state that is neither pinned nor reviewable. Bazel builds PostgreSQL from a sha256-pinned source tarball instead. Run `./scripts/gate-pg.sh`; Docker is the only host dependency. Its artifacts share the single all-in-one `yesno-e2e:local` image with the MySQL, OpenSearch, Elasticsearch, operator, and filesystem gates, so an unchanged checkout reuses them in later sessions.
  * A scoped exception, not a migration. `scripts/gate.sh` is unchanged and remains the authority on everything else, and **neither gate subsumes the other** — a change to `yesno-core` or `yesno-flight` must run *both*, because Bazel builds those crates too and a change that satisfies cargo can still break the Bazel build through a stale lockfile resolution.
  * `Cargo.toml` and `Cargo.lock` stay the single source of truth for dependency versions; `crate_universe` reads them. Never hand-write a crate version into `MODULE.bazel`. Re-pin with `CARGO_BAZEL_REPIN=1 bazel mod deps`.
  * Bazel's convenience symlinks are redirected to `.agents-workspace/tmp/bazel-*` by `.bazelrc`, which is what keeps the rule about built artifacts in the tree true. Do not remove that `--symlink_prefix`.
  * `cargo pgrx test` and `#[pg_test]` are not used and must not be reintroduced: both want a cluster under `~/.pgrx`. SQL behaviour is covered by `pg_regress`-style fixtures in `e2e/postgresql/{sql,expected}`, run against a cluster `initdb`'d inside the test. Expected files are reviewed, byte-exact oracles; never replace one with new output merely to make a red test green without first explaining and reading the diff.
* If two agents are working in the same checkout concurrently and you hit contention on `target/.cargo-lock`, wait rather than minting a private `CARGO_TARGET_DIR`; a second target directory triples disk use and buys nothing at this workspace size.
* Format with a plain `cargo fmt --all` before running the gate. The unformatted baseline was closed on 2026-08-27 ( a one-shot format of the workspace plus `yesno-core/fuzz` ), so `cargo fmt` no longer rewrites anything you did not touch and `scripts/fmt-scoped.py` is no longer needed for routine work.
* `yesno-core/fuzz` is **outside `[workspace] members`**, so `cargo fmt --all` does not reach it. Format it separately ( `cd yesno-core/fuzz && cargo fmt --all` ) if you edit it; the gate checks it separately for the same reason.
* **Do not use bare `rustfmt <file>`.** rustfmt follows every `mod x;` declaration, so `rustfmt yesno-core/src/lib.rs` rewrites **seven** files. That no longer produces a diff on a clean tree, but it is still the wrong tool — use `cargo fmt`.
* **Do not run a bare `cargo update`.** `get-size2` is pinned to 0.10.1 in `Cargo.lock`. 0.10.2 moved to `compact_str` 0.10 while `ruff_python_ast` — which `monty` parses with — still uses 0.9, so the `GetSize` impl for `CompactString` stops applying and `yesno-e2e` fails to build with a confusing trait error inside a dependency. If you must update, re-pin with `cargo update -p get-size2 --precise 0.10.1`.
* `yesno-e2e` declares its own `rust-version = "1.95"` because `monty` requires it. That is deliberate and must stay per-package: `yesno-core`'s MSRV of 1.89 is a promise to its users, and a test harness must not be able to move it. Do not "fix" the mismatch by raising `[workspace.package] rust-version`.

## Testing

* The test suite is layered, and each layer exists to catch something the others structurally cannot. Know which one your change threatens:
  * `tests/proptest_oracle.rs` — randomized properties against a `BTreeSet<u64>` oracle. Generators are **boundary-biased on purpose**; uniform random `u64`s would never fill an array container or produce a run container.
  * `tests/differential.rs` — the M0 gate. Semantic agreement with the `roaring` crate, and **byte-level identity** of serialized output. Byte identity is what makes `O(container count)` import of `.roaring` files legitimate.
  * `tests/expr_equivalence.rs` — the M1 gate. Lazy stream evaluation must equal eager `OrdSet` evaluation, and `cardinality()` must equal `collect_set().len()`.
  * `tests/allocation.rs` — allocation regression tests. The non-materializing cardinality walk is a *parallel implementation* of the materializing one; only an allocation count stops it decaying into `collect_set().len()`.
  * `e2e/scenarios/*.py` — end-to-end operational sequences, scripted in Python and run by `monty`. Open, ingest, checkpoint, close, reopen, query. Adding a scenario is adding a `.py` file; run one with `cargo run -p yesno-e2e -- <file>` and list the verbs with `--list`. Python's own `set` is the oracle, which is why these live here rather than as Rust tests. Never add a verb whose name collides with a Python builtin ( `open`, `min`, `max`, `len` ): monty resolves builtins without asking the host, so the call would silently do something else. A unit test enforces this.

    This directory also holds the **measurement fixtures** migrated from `yesno-core/examples/`. They run at a small corpus by default so the gate stays quick; the example's own corpus is `--arg name=value`, and a passing scenario's printed table needs `--show-output` ( a failing one always shows it ). Each fixture's header states its full-scale command line. When a fixture reports **nanoseconds**, the repetition loop must run in the host via `q_time( expr, iters, terminal )` — a loop written in Python costs ~1 us per host call and would be timing `monty`. Do not add a wall-clock column for a walk written in Python; report its work counters instead. **The live examples are `aged_state.py` and `wal_size.py`**, which the deep gate runs.

**This sentence used to name `and_shape.py` and `aligned_eval.py` as the model, and that was exactly backwards** ( corrected 2026-09-07 ). Those two were migrated on 2026-08-26 and **moved straight back out** — their subjects, a k-way intersection and an aligned-grid evaluator, exist in Python and **nowhere in `src/`**, so their assertions were about the scenario file rather than about the crate. They now live under `.agents-workspace/tmp/prototypes/`, and the paths this sentence gave had not existed for eleven days. The admission test is in `ARCHITECTURE.md` and is the thing to carry away: *does a change to `yesno-core` fail this fixture in the way the assertion is phrased?* A scenario that reimplements an operator and checks its own answer against yesno is testing the reimplementation.
* Make sure that regression tests are ready for your fix. A bug that was reachable from `OrdSet` or from a `ChunkStream` should end up covered by the oracle or property layer, not only by a hand-written unit test.
* Never weaken an oracle, loosen a property, or raise an allocation budget to make a failing test pass. If the test is genuinely wrong, say so explicitly and record the reasoning in `JOURNAL.md`.
* `--maxfail` is a pytest flag, not a Rust libtest flag. Do not pass it to `cargo test`. Use `--no-fail-fast`, `--test-threads=N`, or a name filter instead.
* Proptest failures persist to `tests/*.proptest-regressions`. That file is a checked-in regression corpus — commit new seeds, never delete them to make the suite green.
* Benchmarks are `cargo bench --bench setops`, measured against the `roaring` crate as the absolute reference rather than against our own past numbers. Benchmarks are not a gate; a benchmark regression is a finding for `JOURNAL.md`, and only an allocation-test failure blocks a change.

## Local Lint Gate

Before reporting any Rust code change as done — and this applies to subagents as well — you must run:

```
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --check
cargo test -p yesno-core
```

* **`--workspace` is load-bearing, not decoration.** A bare `cargo clippy --all-targets --all-features` lints `[workspace] default-members` — `yesno-core` and `yesno-arrow` alone — so it reports success having never looked at `yesno-datafusion`, `yesno-replication`, `yesno-flight`, `yesno-server`, `yesno-operator` or `yesno-e2e`. Do not drop the flag to make a satellite crate's lint go away. `scripts/gate.sh` and CI both use `--workspace`; deferring to `./scripts/gate.sh` is equally correct and always current.
  * **A tool and its instructions are two implementations of the same check, and both have to be repaired.** `gate.sh` was fixed on 2026-08-30 and this block was not, so six days of hand-run "lint-clean" reports covered two crates while three findings sat in the others — one of them a test that had never run. See `gate-clippy-saw-two-crates` in `.agents/docs/JOURNAL.md`, 2026-09-06.
* If clippy fails, fix the violations and re-run until it passes. Do not declare the change complete with outstanding clippy errors.
* `cargo fmt --check` now passes on the whole tree. The 65-hunk baseline ( 89 when first measured on 2026-08-25 ) was formatted away on 2026-08-27, together with the 3 hunks in `yesno-core/fuzz/` that `cargo fmt --all` cannot see. `scripts/gate.sh` checks both. Keep it that way: a bulk reformat is now a *no-op*, so any diff `cargo fmt` produces is yours.
* The workspace is small enough that the workspace-wide gate is the routine gate. There is no cheaper per-crate variant worth reaching for.
* When delegating Rust changes to a subagent, instruct it to run this same gate before returning, and have it include the result in its final report.

## Unsafe Code

* The crate sets `#![deny(unsafe_op_in_unsafe_fn)]`. Keep it.
* Prefer `bytemuck` over hand-written transmutes for `u16` / `u64` reinterpretation. It is already a dependency precisely so that the common cases need no `unsafe`.
* Do not introduce a new `unsafe` block to win a benchmark without ( a ) a `// SAFETY:` comment stating the invariant it relies on, ( b ) a note in `JOURNAL.md`, and ( c ) a property test that would fail if the invariant were violated.
* `container::codec::decode` is a fuzz target by contract: for **any** input it must return `Err` or a container satisfying its invariants, and must never panic. Changes to it need the `deserialize_never_panics` property to still hold.

## Python

* If there's a `pyproject.toml` file, try to run the tests with `uv run pytest ...` and arbitrary scripts with `uv run python ...`.
  * If there's no `pyproject.toml`, never run a bare `pip install` out of a venv. Always use `uv pip ...` in combination with `uv venv`.

## Shell Pitfalls ( prezto defaults )

The user's shell uses prezto, which sets aliases and options that break non-interactive scripts:

* `cp src dst` prompts interactively when `dst` exists ( prezto aliases `cp` to `cp -i` ). Always `rm -f dst` before `cp`.
* `cat > file <<'EOF'` and `echo > file` fail with `file exists` when the target exists ( prezto enables `NO_CLOBBER` ). Workaround: `rm -f file` before writing, or use `tee` / `/bin/cat`.
* `rm file` prompts for confirmation on some files ( prezto aliases `rm` to `rm -i` ). Always use `rm -f` for non-interactive deletion.

## Git Workflow

* Neither do `git checkout` nor `git restore`. Another coding agent may be concurrently working on the same directory.
* Never make discretionary commits.

## Documentation

* Try to write your work summary to one of the existing documents.
* Avoid editing any existing sections of JOURNAL.md. You should rather just append texts to it. ( The sole exception is the `reconcile-journal-ltm` skill, which may remove entries that have already been consolidated into `.agents/docs/LTM/` per the canonical `## LTM Consolidation Record` table. )
* Module-level `//!` comments in `yesno-core/src/` carry the *why*. When you change behaviour that a `//!` block explains, update the block in the same change. A stale rationale comment is worse than none.
* For repo-authored documentation only ( e.g. `AGENTS.md`, `README.md`, `.agents/docs/**` ), never use full-width parentheses ( `（` `）` ). Instead, use half-width parentheses ( `(` `)` ) with a half-width space being put before/after an open/close parenthesis when it's preceded/followed by a non-white-space character.
* For repo-authored documentation only, never use full-width colons ( `：` ). Instead, use a half-width colon followed by a half-width space.
* Never use emoji anywhere in the repository -- not in Markdown under `docs/` or `.agents/docs/`, not in `//` / `//!` / `#` comments, not in commit messages, and not in strings a program prints. This includes the pictographic markers this tree used to carry ( a cross mark for a prohibition, a warning sign for a caveat, a check mark for a settled point, a pushpin, a light bulb, an hourglass ). They were removed on 2026-09-13; do not reintroduce them.
  * Say it in words instead. A prohibition opens with "Do not"; a caveat opens with "Note" or states the hazard directly; a settled point says when it was settled. Markdown emphasis ( `**bold**` ) carries the weight the glyph was carrying, and it survives a terminal, a diff and a `grep` that the glyph does not.
  * Mathematical and typographic characters are not emoji and are unaffected -- arrows ( `->`, U+2192 ), set operators ( U+2229, U+222A ), floor brackets and the symmetric-difference triangle stay where they read as notation. The test is whether the character is being *read* as part of the sentence or *looked at* as an icon.
