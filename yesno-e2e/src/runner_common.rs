use std::time::{Duration, Instant};

use monty::{MontyRun, RunProgress};
use monty_types::{
    CompileOptions, ExtFunctionResult, MontyObject, NameLookupResult, PrintWriter, ResourceLimits,
    ResourceTracker,
};

pub use world::World;

/// Why a scenario did not pass.
#[derive(Debug)]
pub enum E2eError {
    /// The script raised, including a failed `assert`.
    Python(String),
    /// The script tried to reach the host operating system.
    Sandbox(String),
    /// The harness could not set the scenario up at all.
    Harness(String),
    /// The script never asserted anything, or never called a verb.
    Vacuous(String),
}

impl std::fmt::Display for E2eError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            E2eError::Python(m) => write!(f, "{m}"),
            E2eError::Sandbox(m) => write!(f, "sandbox violation: {m}"),
            E2eError::Harness(m) => write!(f, "harness error: {m}"),
            E2eError::Vacuous(m) => write!(f, "scenario proves nothing: {m}"),
        }
    }
}

impl std::error::Error for E2eError {}

/// The result of running one scenario.
#[derive(Debug)]
pub struct Outcome {
    pub name: String,
    /// `None` on success.
    pub error: Option<E2eError>,
    /// Everything the scenario printed, shown only when it fails.
    pub output: String,
    /// Harness verbs invoked.
    pub calls: u64,
    pub elapsed: Duration,
}

impl Outcome {
    #[must_use]
    pub fn passed(&self) -> bool {
        self.error.is_none()
    }
}

/// How long a single scenario may run before it is killed.
///
/// A scenario that hangs is a failure like any other; without a limit it would
/// hang the whole suite instead of reporting which file did it.
pub const DEFAULT_TIME_LIMIT: Duration = Duration::from_secs(120);

/// Whole-number knobs a scenario reads with `yn_arg( name, default )`.
///
/// This is what replaces an example's command line. `aged_state.rs` took
/// `--big` and `--sparse`; the scenario takes `--arg keys=1500`. Empty
/// under `cargo test`, so the gate always runs the small corpus and the large
/// one is a deliberate act.
pub type ScenarioArgs = Vec<(String, u64)>;

/// Run one scenario from source, at its default corpus.
pub fn run_source(name: &str, source: &str, limit: Duration) -> Outcome {
    run_source_with(name, source, limit, &[])
}

/// Run one scenario from source, with `yn_arg` knobs.
pub fn run_source_with(
    name: &str,
    source: &str,
    limit: Duration,
    args: &[(String, u64)],
) -> Outcome {
    let started = Instant::now();
    let mut output = String::new();
    let mut calls = 0;

    let error = execute(name, source, limit, args, &mut output, &mut calls).err();

    Outcome {
        name: name.to_owned(),
        error,
        output,
        calls,
        elapsed: started.elapsed(),
    }
}

/// Compile one scenario without running any of it.
///
/// This exists for the scenarios the routine suite never executes -- the
/// AWS, operator, filesystem and search arms all need hardware or an account
/// the gate does not have. Nothing else would notice a syntax error in one of
/// them until the moment it is delivered, which for the AWS gate is twenty
/// billable minutes and a provisioned cluster after the run began.
///
/// It proves the file compiles and nothing more. A scenario that asserts the
/// wrong thing compiles perfectly.
pub fn compiles(name: &str, source: &str) -> Result<(), E2eError> {
    if !has_assert_statement(source) {
        return Err(E2eError::Vacuous(
            "no `assert` statement in the file — a scenario that checks nothing always passes"
                .to_owned(),
        ));
    }
    MontyRun::new(
        source.to_owned(),
        name,
        Vec::new(),
        CompileOptions::default(),
    )
    .map(|_| ())
    .map_err(|e| E2eError::Python(format_exception(&e)))
}

/// Run one scenario from a file, at its default corpus.
pub fn run_file(path: &std::path::Path, limit: Duration) -> Outcome {
    run_file_with(path, limit, &[])
}

/// Run one scenario from a file, with `yn_arg` knobs.
pub fn run_file_with(path: &std::path::Path, limit: Duration, args: &[(String, u64)]) -> Outcome {
    let name = path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    match std::fs::read_to_string(path) {
        Ok(src) => run_source_with(&name, &src, limit, args),
        Err(e) => Outcome {
            name,
            error: Some(E2eError::Harness(format!(
                "cannot read {}: {e}",
                path.display()
            ))),
            output: String::new(),
            calls: 0,
            elapsed: Duration::ZERO,
        },
    }
}

/// Every `.py` file in `dir`, sorted, so a run is reproducible and a scenario
/// that vanishes from the directory shows up as a missing name rather than a
/// silently smaller suite.
pub fn scenario_files(dir: &std::path::Path) -> std::io::Result<Vec<std::path::PathBuf>> {
    let mut out: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(std::result::Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "py"))
        .collect();
    out.sort();
    Ok(out)
}

fn execute(
    name: &str,
    source: &str,
    limit: Duration,
    args: &[(String, u64)],
    output: &mut String,
    calls: &mut u64,
) -> Result<(), E2eError> {
    if !has_assert_statement(source) {
        return Err(E2eError::Vacuous(
            "no `assert` statement in the file — a scenario that checks nothing always passes"
                .to_owned(),
        ));
    }

    let mut world = World::temporary_labelled(name, args.to_vec())
        .map_err(|e| E2eError::Harness(format!("cannot create a temporary root: {e}")))?;

    let run = MontyRun::new(
        source.to_owned(),
        name,
        Vec::new(),
        CompileOptions::default(),
    )
    .map_err(|e| E2eError::Python(format_exception(&e)))?;

    let tracker = ResourceTracker::new(ResourceLimits::default().max_duration(limit));
    let mut progress = run
        .start(Vec::new(), tracker, writer(output))
        .map_err(|e| E2eError::Python(format_exception(&e)))?;

    loop {
        progress = match progress {
            RunProgress::Complete(_) => break,

            // An unresolved global. If it names a verb, hand back a callable;
            // otherwise let the VM raise `NameError`, which is what makes a
            // mistyped verb a failure rather than a silent no-op.
            RunProgress::NameLookup(nl) => {
                let result = if World::is_verb(&nl.name) {
                    NameLookupResult::Value(MontyObject::Function {
                        name: nl.name.clone(),
                        docstring: None,
                    })
                } else {
                    NameLookupResult::Undefined
                };
                nl.resume(result, writer(output))
                    .map_err(|e| E2eError::Python(format_exception(&e)))?
            }

            RunProgress::FunctionCall(fc) => {
                let result = if World::is_verb(&fc.function_name) {
                    *calls += 1;
                    match world.call(&fc.function_name, &fc.args, &fc.kwargs) {
                        Ok(v) => ExtFunctionResult::Return(v),
                        Err(e) => ExtFunctionResult::Error(e),
                    }
                } else {
                    ExtFunctionResult::NotFound(fc.function_name.clone())
                };
                fc.resume(result, writer(output))
                    .map_err(|e| E2eError::Python(format_exception(&e)))?
            }

            // The harness grants no filesystem, environment or network
            // authority, so any OS call is a scenario reaching outside its
            // sandbox. Reporting it beats resuming with a plausible answer.
            RunProgress::OsCall(os) => {
                return Err(E2eError::Sandbox(format!(
                    "the scenario attempted an operating-system call ({:?}); scenarios may only \
                     reach the database through harness verbs",
                    os.function_call
                )))
            }

            // Unreachable: no verb resumes with a future, so nothing can be
            // awaited. Reported rather than ignored in case that changes.
            RunProgress::ResolveFutures(_) => {
                return Err(E2eError::Harness(
                    "the scenario awaited a future, but no harness verb is asynchronous".to_owned(),
                ))
            }
        };
    }

    if *calls == 0 {
        return Err(E2eError::Vacuous(
            "the scenario ran to completion without calling a single harness verb, so it never \
             reached the database"
                .to_owned(),
        ));
    }
    debug_assert_eq!(*calls, world.calls(), "verb accounting disagrees");
    Ok(())
}

fn writer(output: &mut String) -> PrintWriter<'_> {
    // Bounded, so a scenario printing in a loop fails on its own output rather
    // than by exhausting memory.
    PrintWriter::CollectString(output, Some(1 << 20))
}

/// A CPython-shaped traceback, so a failing assertion points at a line.
fn format_exception(e: &monty_types::MontyException) -> String {
    let mut s = String::new();
    if !e.traceback().is_empty() {
        s.push_str("Traceback (most recent call last):\n");
        for frame in e.traceback() {
            s.push_str(&format!("{frame}\n"));
        }
    }
    s.push_str(&e.summary());
    s
}

/// Whether any line's first token is `assert`.
///
/// Deliberately syntactic and deliberately crude: it exists to reject an empty
/// or placeholder scenario file, not to judge whether the assertions are any
/// good. Matching the bare substring would count the word inside a comment or
/// a docstring, which is precisely the file this guard is for.
fn has_assert_statement(source: &str) -> bool {
    source.lines().any(|line| {
        let t = line.trim_start();
        t == "assert" || t.starts_with("assert ") || t.starts_with("assert(")
    })
}

