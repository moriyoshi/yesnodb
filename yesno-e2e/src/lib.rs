//! An end-to-end test harness for yesno, scripted in Python by
//! [monty](https://github.com/pydantic/monty).
//!
//! # Why a scripting layer at all
//!
//! yesno already has seven test layers, and every one of them is a Rust
//! integration test compiled against the crate. That is the right shape for
//! testing containers, kernels and the store — and the wrong shape for the
//! last question, which is whether the *operational* sequences hold up: open,
//! ingest, checkpoint, close, reopen, query, and do the answers still match
//! what was put in. Those scenarios are dominated by their setup and their
//! oracle, not by anything the type system checks, and expressing them in Rust
//! means a recompile for every new one.
//!
//! Here a scenario is a `.py` file under `e2e/scenarios/`. Adding one is adding a
//! file — no Rust changes, no new `#[test]`. The scripts get the whole of
//! monty's Python subset, which matters more than it sounds: **Python's own
//! `set` and `sorted` are the oracle**, so a scenario states what it expects in
//! the language of sets rather than reimplementing set algebra in assertions.
//!
//! # What the sandbox buys
//!
//! monty is not an embedded CPython; it is a bytecode VM with no ambient
//! authority. Filesystem, environment and network are absent unless the host
//! supplies them, and this harness supplies none of them — a scenario reaching
//! for `open()` is stopped with [`E2eError::Sandbox`] rather than writing
//! somewhere unexpected. Everything a scenario can touch arrives through the
//! verbs in [`world`], so the surface under test is exactly the surface the
//! harness chose to expose.
//!
//! # Two guards against a scenario that cannot fail
//!
//! This codebase has repeatedly produced tests that passed because they could
//! not see their subject, so the runner refuses two shapes outright:
//!
//! * a scenario whose source contains no `assert` statement, and
//! * a scenario that completes without calling a single harness verb.
//!
//! Neither guard can tell a weak assertion from a strong one — only sabotaging
//! the code under test can do that, which is how each scenario in
//! `e2e/scenarios/` was checked. What they do catch is the empty file and the
//! scenario that quietly stopped reaching the database.
//!
//! # Scope: not only operational sequences
//!
//! The harness began as a way to script open / ingest / checkpoint / reopen. It
//! now also carries the verb surface the **measurement fixtures** need — direct
//! `OrdSet` construction, the container kernels, raw `ChunkStream` cursors, the
//! planner, and the space diagnostics. Those fixtures were
//! `yesno-core/examples/*.rs` until 2026-08-26, and they moved here because they
//! had the same problem from the other direction: dominated by their setup and
//! their oracle, recompiled for every variation, and **run by nothing**.
//! `cargo clippy --all-targets` type-checks an example; no gate executes one. A
//! fixture that is never executed is the failure mode this project keeps
//! hitting, one step removed. `examples/readme.rs` stayed behind, because its
//! whole value is being *Rust that compiles*.
//!
//! One thing does **not** migrate faithfully, and pretending otherwise would
//! be worse than saying so: a repetition loop written in Python times monty.
//! Where a fixture reports nanoseconds — `rule_economics` prices a planner pass
//! at 88 ns — the loop has to run on this side of the boundary, which is what
//! `q_time` is for. A hand-written k-way walk expressed in Python can be
//! checked for **agreement** and counted, and its wall-clock cannot be compared
//! against the engine's.

/// Where this workspace puts temporary files.
///
/// **The one place in Rust that names it.** It was written out longhand in
/// three modules, and in seven shell scripts beside them, each of which had to
/// stay in step with the others and with CLAUDE.md's rule about where scratch
/// files go.
///
/// `YESNO_SCRATCH_DIR` overrides it, and `scripts/scratch.sh` sets that same
/// variable for every shell entry point -- so a gate script and the harness it
/// runs agree by construction rather than by both spelling the path correctly.
///
/// `.bazelrc` still spells the default out, and has to: Bazel's rc files
/// take a literal `--symlink_prefix` and expand no variables.
pub fn scratch_dir(workspace: &std::path::Path) -> std::path::PathBuf {
    match std::env::var_os("YESNO_SCRATCH_DIR") {
        Some(value) if !value.is_empty() => std::path::PathBuf::from(value),
        _ => workspace.join(".agents-workspace/tmp"),
    }
}

pub mod arrow;
pub mod aws;
pub mod bignum;
pub mod cloud;
pub mod convert;
pub mod datafusion;
pub mod eager;
pub mod filesystems;
pub mod fixture;
pub mod flight;
pub mod lazy;
pub mod matrix;
pub mod operator;
mod repl;
pub mod search;
pub mod server;
pub mod view;
pub mod world;

include!("runner_common.rs");
#[cfg(test)]
mod tests {
    use super::*;

    const LIMIT: Duration = Duration::from_secs(30);

    #[test]
    fn a_passing_scenario_passes() {
        let out = run_source(
            "ok.py",
            "d = db_open()\ndb_insert(d, 1, 5)\ns = db_snapshot(d)\nassert snap_contains(s, 1, 5)\n",
            LIMIT,
        );
        assert!(out.passed(), "{:?}", out.error);
        assert!(out.calls >= 4);
    }

    #[test]
    fn a_failing_assert_fails_with_a_line_number() {
        let out = run_source(
            "bad.py",
            "d = db_open()\ns = db_snapshot(d)\nassert snap_contains(s, 1, 5)\n",
            LIMIT,
        );
        let msg = out.error.expect("a false assertion must fail").to_string();
        assert!(msg.contains("AssertionError"), "{msg}");
        assert!(
            msg.contains("line 3"),
            "traceback must locate the assert: {msg}"
        );
    }

    /// The guard that matters most: a typo'd verb must not be a silent no-op.
    #[test]
    fn a_mistyped_verb_is_a_name_error() {
        let out = run_source("typo.py", "assert True\ndb_insrt(0, 1, 2)\n", LIMIT);
        let msg = out.error.expect("an unknown verb must fail").to_string();
        assert!(msg.contains("NameError"), "{msg}");
    }

    #[test]
    fn a_scenario_without_assertions_is_refused() {
        let out = run_source("empty.py", "d = db_open()\ndb_insert(d, 1, 2)\n", LIMIT);
        assert!(
            matches!(out.error, Some(E2eError::Vacuous(_))),
            "{:?}",
            out.error
        );
    }

    /// `assert` inside a comment must not satisfy the guard.
    #[test]
    fn a_commented_assert_does_not_count() {
        let out = run_source("cmt.py", "# assert something\nd = db_open()\n", LIMIT);
        assert!(
            matches!(out.error, Some(E2eError::Vacuous(_))),
            "{:?}",
            out.error
        );
    }

    #[test]
    fn a_scenario_that_never_touches_the_database_is_refused() {
        let out = run_source("pure.py", "assert 1 + 1 == 2\n", LIMIT);
        match out.error {
            Some(E2eError::Vacuous(m)) => assert!(m.contains("never reached the database"), "{m}"),
            other => panic!("expected a vacuity error, got {other:?}"),
        }
    }

    /// The sandbox has no filesystem, and must say so rather than succeeding.
    #[test]
    fn reaching_for_the_filesystem_is_a_sandbox_violation() {
        let out = run_source(
            "fs.py",
            "d = db_open()\nassert d == 0\nf = open('/etc/passwd')\n",
            LIMIT,
        );
        assert!(
            matches!(out.error, Some(E2eError::Sandbox(_))),
            "{:?}",
            out.error
        );
    }

    #[test]
    fn print_output_is_captured_not_leaked() {
        let out = run_source(
            "p.py",
            "d = db_open()\nprint('hello from a scenario')\nassert True\n",
            LIMIT,
        );
        assert!(out.passed(), "{:?}", out.error);
        assert!(
            out.output.contains("hello from a scenario"),
            "{:?}",
            out.output
        );
    }
}
