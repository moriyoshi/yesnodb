//! `yesno-e2e` — run scenario scripts against a real database.
//!
//! ```text
//! yesno-e2e                       # every scenario in e2e/scenarios
//! yesno-e2e path/to/one.py ...    # just these
//! yesno-e2e --list                # the verbs a scenario may call
//! yesno-e2e --timeout 30 ...      # seconds per scenario
//! ```
//!
//! The same runner backs `tests/scenarios.rs`, so a scenario behaves
//! identically under `cargo test` and by hand. This binary exists for the
//! by-hand half: `cargo test` gives no way to run one scenario file and see
//! its output, which is exactly what writing a new scenario needs.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use yesno_e2e::{run_file_with, scenario_files, world, ScenarioArgs, DEFAULT_TIME_LIMIT};

fn main() -> ExitCode {
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut limit = DEFAULT_TIME_LIMIT;
    let mut knobs: ScenarioArgs = Vec::new();
    let mut show_output = false;
    let mut args = std::env::args().skip(1);

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                println!("{}", HELP);
                return ExitCode::SUCCESS;
            }
            "--list" => {
                for name in world::all_names() {
                    println!("{name}");
                }
                return ExitCode::SUCCESS;
            }
            // A failing scenario always shows what it printed. A passing one
            // does not, which is right for a suite and wrong for the
            // measurement fixtures — their printed table *is* the result, and
            // a fixture whose output nobody can see is one nobody will run.
            "--show-output" => show_output = true,
            "--timeout" => {
                let Some(v) = args.next().and_then(|v| v.parse::<u64>().ok()) else {
                    eprintln!("--timeout needs a whole number of seconds");
                    return ExitCode::FAILURE;
                };
                limit = Duration::from_secs(v);
            }
            // `--arg name=value`, read back by `yn_arg( name, default )`.
            // Deliberately whole numbers only: every knob a fixture takes is a
            // corpus size or an iteration count, and accepting arbitrary
            // strings would invite a scenario to branch on a mode name that the
            // harness cannot validate.
            "--arg" => {
                let Some(kv) = args.next() else {
                    eprintln!("--arg needs name=value");
                    return ExitCode::FAILURE;
                };
                let Some((k, v)) = kv.split_once('=') else {
                    eprintln!("--arg {kv}: expected name=value");
                    return ExitCode::FAILURE;
                };
                let Ok(v) = v.parse::<u64>() else {
                    eprintln!("--arg {kv}: '{v}' is not a whole number");
                    return ExitCode::FAILURE;
                };
                knobs.push((k.to_owned(), v));
            }
            other if other.starts_with('-') => {
                eprintln!("unknown option {other}\n\n{HELP}");
                return ExitCode::FAILURE;
            }
            other => paths.push(PathBuf::from(other)),
        }
    }

    if paths.is_empty() {
        let dir = default_scenario_dir();
        match scenario_files(&dir) {
            Ok(found) if found.is_empty() => {
                eprintln!("no .py scenarios in {}", dir.display());
                return ExitCode::FAILURE;
            }
            Ok(found) => paths = found,
            Err(e) => {
                eprintln!("cannot read {}: {e}", dir.display());
                return ExitCode::FAILURE;
            }
        }
    }

    let mut failed = 0usize;
    for path in &paths {
        let out = run_file_with(path, limit, &knobs);
        let ms = out.elapsed.as_millis();
        if let Some(err) = &out.error {
            failed += 1;
            println!("FAIL {} ({ms} ms, {} verb calls)", out.name, out.calls);
            if !out.output.is_empty() {
                for line in out.output.lines() {
                    println!("     | {line}");
                }
            }
            for line in err.to_string().lines() {
                println!("     {line}");
            }
        } else {
            println!("ok   {} ({ms} ms, {} verb calls)", out.name, out.calls);
            if show_output && !out.output.is_empty() {
                for line in out.output.lines() {
                    println!("     | {line}");
                }
            }
        }
    }

    println!(
        "\n{} scenario(s): {} passed, {failed} failed",
        paths.len(),
        paths.len() - failed
    );
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// The workspace-level `e2e/scenarios/` directory, resolved from this crate's
/// manifest so the binary works from any working directory.
fn default_scenario_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../e2e/scenarios")
}

const HELP: &str = "\
yesno-e2e — run Python scenario scripts against a real yesno database

USAGE:
    yesno-e2e [OPTIONS] [SCENARIO.py ...]

With no paths, runs every .py file in the workspace's e2e/scenarios/ directory.

OPTIONS:
    --list             print the verbs a scenario may call
    --show-output      print what a passing scenario printed. The measurement
                       fixtures report their table this way; a failing scenario
                       shows its output regardless.
    --timeout SECS     per-scenario time limit (default 120)
    --arg NAME=N       a whole-number knob, read by yn_arg(NAME, default).
                       Scenarios default to a gate-sized corpus; this is how
                       a measurement fixture is run at full scale, e.g.
                         yesno-e2e --arg keys=1500 --arg rounds=150 aged_state.py
    -h, --help         this message";
