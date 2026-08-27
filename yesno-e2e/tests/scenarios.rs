//! Runs every scenario in `e2e/scenarios/` under `cargo test`.
//!
//! One `#[test]` rather than one per file, because libtest cannot generate
//! cases at runtime without a custom harness — and a custom harness would be a
//! second way to run scenarios that could drift from the binary. Every failure
//! is collected and reported together, so a broken change shows all the
//! scenarios it broke instead of only the alphabetically first.

use std::time::Duration;

use yesno_e2e::{run_file, run_source, scenario_files, E2eError};

fn scenario_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../e2e/scenarios")
}

fn operator_scenario_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../e2e/operator")
}

fn filesystem_scenario_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../e2e/filesystems")
}

fn aws_scenario_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../e2e/aws")
}

fn search_scenario_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../e2e/search")
}

fn mysql_scenario_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../e2e/mysql")
}

fn postgres_scenario_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../e2e/postgresql")
}

#[test]
fn every_scenario_passes() {
    let dir = scenario_dir();
    let files = scenario_files(&dir).expect("scenarios/ must be readable");
    assert!(
        !files.is_empty(),
        "no scenarios found in {} — the suite would pass by finding nothing",
        dir.display()
    );

    let mut failures = String::new();
    let mut passed = 0;
    for path in &files {
        let out = run_file(path, Duration::from_secs(120));
        match out.error {
            None => {
                passed += 1;
                // A scenario is required to reach the database, but the count
                // is worth surfacing: a scenario that quietly shrank to two
                // calls is still passing and no longer testing much.
                println!("ok   {} ({} verb calls)", out.name, out.calls);
            }
            Some(err) => {
                failures.push_str(&format!("\n--- {} ---\n", out.name));
                if !out.output.is_empty() {
                    failures.push_str(&format!("stdout:\n{}\n", out.output));
                }
                failures.push_str(&format!("{err}\n"));
            }
        }
    }

    assert!(
        failures.is_empty(),
        "{} of {} scenarios failed:\n{failures}",
        files.len() - passed,
        files.len()
    );
}

/// The runner must be able to fail.
///
/// Without this, a regression that made `run_file` return success
/// unconditionally would turn the whole suite green and look like a very good
/// day. The unit tests in `lib.rs` cover the individual guards; this is the
/// one assertion that belongs next to the suite it protects.
#[test]
fn the_runner_can_still_fail() {
    let out = run_source(
        "deliberately_wrong.py",
        "d = db_open()\ndb_insert(d, 1, 1)\ns = db_snapshot(d)\nassert snap_cardinality(s, 1) == 99\n",
        Duration::from_secs(30),
    );
    match out.error {
        Some(E2eError::Python(m)) => {
            assert!(m.contains("AssertionError"), "{m}");
            assert!(
                m.contains("assert 1 == 99"),
                "monty must introspect the operands: {m}"
            );
        }
        other => panic!("a false assertion must fail the scenario, got {other:?}"),
    }
}

/// Every opt-in scenario at least compiles.
///
/// # Why this is worth a test
///
/// `every_scenario_passes` runs `e2e/scenarios/` and nothing else, because the
/// other directories need a Kubernetes cluster, a KVM guest, a search engine or
/// an AWS account. Those files are therefore delivered unparsed: the first
/// thing that reads `e2e/aws/operator.py` is a container on an EC2 instance,
/// twenty billable minutes into a run that has already provisioned an EKS
/// cluster. A misplaced bracket costs that whole run and reports itself as a
/// scenario failure on the far side of a Systems Manager round trip.
///
/// Compiling is all this claims. A scenario that asserts the wrong thing
/// compiles; only running it against real hardware can say otherwise. What
/// this removes is the class of failure that has nothing to do with the
/// subject and costs the most to discover.
#[test]
fn every_opt_in_scenario_compiles() {
    let mut failures = String::new();
    let mut checked = 0;
    for dir in [
        operator_scenario_dir(),
        filesystem_scenario_dir(),
        aws_scenario_dir(),
        search_scenario_dir(),
        mysql_scenario_dir(),
    ] {
        let files = scenario_files(&dir).expect("an opt-in scenario directory must be readable");
        assert!(
            !files.is_empty(),
            "no scenarios found in {} — this test would pass by finding nothing",
            dir.display()
        );
        for path in &files {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let source = std::fs::read_to_string(path).expect("a scenario must be readable");
            checked += 1;
            if let Err(error) = yesno_e2e::compiles(&name, &source) {
                failures.push_str(&format!("\n--- {} ---\n{error}\n", path.display()));
            }
        }
    }
    assert!(failures.is_empty(), "{checked} checked:{failures}");
}

/// Every advertised verb must be called by some scenario.
///
/// # Why this is a test and not a nice-to-have
///
/// Verbs are added for a reason and the reason can go away. On 2026-08-26 two
/// files were moved out of `scenarios/` because they turned out to be Python
/// reimplementations of algorithms that do not exist in `yesno-core` — and they
/// were the **only** callers of thirty-four verbs. Nothing failed. The surface
/// still compiled, its unit tests still passed, and a third of it had silently
/// stopped being exercised by the suite.
///
/// That is this project's standing failure mode ( "tools that cannot see their
/// subject" ) applied to the harness itself, and it is trivially catchable
/// mechanically, which is what this does.
///
/// It checks that a verb is *mentioned*, not that it is meaningfully
/// exercised. A scenario calling `set_rank` and ignoring the answer would pass
/// here. Only sabotaging the code under test can tell those apart, which is how
/// each scenario was checked; this catches the surface that no longer has a
/// caller at all.
#[test]
fn every_verb_has_a_caller_in_some_scenario() {
    let dir = scenario_dir();
    let mut files = scenario_files(&dir).expect("scenarios/ must be readable");
    files.extend(scenario_files(&mysql_scenario_dir()).expect("MySQL scenarios must be readable"));
    files.extend(
        scenario_files(&postgres_scenario_dir()).expect("PostgreSQL scenarios must be readable"),
    );
    files.extend(
        scenario_files(&operator_scenario_dir()).expect("operator scenarios must be readable"),
    );
    files.extend(
        scenario_files(&filesystem_scenario_dir()).expect("filesystem scenarios must be readable"),
    );
    files.extend(scenario_files(&aws_scenario_dir()).expect("AWS scenarios must be readable"));
    files
        .extend(scenario_files(&search_scenario_dir()).expect("search scenarios must be readable"));
    let mut corpus = String::new();
    for path in &files {
        corpus.push_str(&std::fs::read_to_string(path).expect("a scenario must be readable"));
        corpus.push('\n');
    }

    // `name(` rather than a bare substring: `set_or` is a prefix of nothing
    // here, but `batch` is a prefix of `batch_commit`, and matching bare names
    // would let `batch_commit(...)` vouch for `batch` having a caller.
    let uncalled: Vec<&str> = yesno_e2e::world::all_names()
        .iter()
        .copied()
        .filter(|name| !corpus.contains(&format!("{name}(")))
        .collect();

    assert!(
        uncalled.is_empty(),
        "{} verb(s) are advertised by the harness and called by no scenario: {}\n\
         Either add coverage, or drop the verb — an unexercised verb reads as \
         coverage and provides none.",
        uncalled.len(),
        uncalled.join(", ")
    );
}
