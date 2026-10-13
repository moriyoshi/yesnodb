//! `java_client`: the shipped Java Flight client, driven against a live `yesnod`.
//!
//! # Why this family exists
//!
//! The Python and Go clients each have an integration suite that starts a real
//! daemon and talks to it over Flight, and those two suites are what caught the
//! Flight ticket header widening from 40 to 48 bytes. The Java client had no
//! equivalent: its only server was a fake inside its own unit tests, and that
//! fake minted its ticket from `QueryTicket.HEADER_LENGTH` -- the client's own
//! constant -- so the module stayed **green and broken for a week** while the
//! other two jobs failed every integration test.
//!
//! What landed at the time was a 48-byte hex vector mirrored across the four
//! implementations. A literal vector catches a layout change in a field a vector
//! can express; it cannot catch a changed refusal, a changed status code, or a
//! schema whose metadata moved, and it only catches the layout half because
//! someone remembers to update all four copies.
//!
//! # Why it is a `java -cp` and not a Gradle invocation
//!
//! The search family runs `gradlew e2eTest` at scenario time, which needs
//! Gradle's dependency cache. In the E2E image that cache is a BuildKit
//! `--mount=type=cache`: it exists while the image is being built and is gone by
//! the time a gate runs inside it. So a Gradle invocation here would reach Maven
//! Central from inside a gate. `liveCheckInstall` stages the classes and the
//! whole runtime classpath into the image instead, and this verb runs a plain
//! `java -cp` against it -- no Gradle, no JUnit, no network.
//!
//! # What the host decides and what Python decides
//!
//! The host owns the JDK, the staged classpath and the subprocess. It parses
//! the driver's output into a dict and decides nothing about the values; the
//! scenario checks them against Python's own `set`. The one judgement the
//! driver makes locally is the *name* of a refusal, because a Flight status code
//! is not an observation a scenario can make from outside the client.

use std::path::{Path, PathBuf};
use std::process::Command;

use monty_types::{ExcType, MontyException, MontyObject};

use crate::convert::{int_obj, value_err, Args};
use crate::world::World;

/// The verbs this module dispatches. Merged into [`crate::world::all_names`].
pub const OWNS: &[&str] = &["java_client"];

/// The marker every observation line carries, so JVM noise is not parsed.
const MARKER: &str = "yesno-live";

/// The staged driver, relative to the workspace root.
const STAGED: &str = "yesno-flight-java/build/live-check";

/// The driver's entry point.
const MAIN_CLASS: &str = "dev.yesnodb.client.live.LiveDaemonCheck";

impl World {
    pub(crate) fn call_java_client(
        &mut self,
        verb: &str,
        a: &Args<'_>,
    ) -> Result<MontyObject, MontyException> {
        match verb {
            "java_client" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let location = a.str_at(0)?;
                run(location).map_err(|error| {
                    MontyException::new(
                        ExcType::RuntimeError,
                        Some(format!("{verb}(): the Java client check failed: {error}")),
                    )
                })
            }
            _ => Err(value_err(format!("{verb}() is not a Java client verb"))),
        }
    }
}

fn workspace() -> Result<PathBuf, String> {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or_else(|| "yesno-e2e manifest has no workspace parent".to_owned())
        .map(Path::to_path_buf)
}

fn run(location: &str) -> Result<MontyObject, String> {
    let workspace = workspace()?;
    let java_home = std::env::var_os("YESNO_E2E_JAVA_HOME")
        .or_else(|| std::env::var_os("JAVA_HOME"))
        .map(PathBuf::from)
        .ok_or("set YESNO_E2E_JAVA_HOME to a JDK 21 installation")?;
    let java = java_home.join("bin/java");
    if !java.is_file() {
        return Err(format!("{} is not a file", java.display()));
    }

    let staged = workspace.join(STAGED);
    let classes = staged.join("classes");
    if !classes.is_dir() {
        return Err(format!(
            "{} is missing; stage it with `./scripts/build-database-artifacts.sh java`",
            classes.display()
        ));
    }
    // A wildcard entry, which the JVM expands itself. Listing the jars here
    // would duplicate Gradle's resolution in a second place that could then
    // disagree with it.
    let classpath = format!("{}:{}/lib/*", classes.display(), staged.display());

    let output = Command::new(&java)
        .arg("--add-opens=java.base/java.nio=ALL-UNNAMED")
        .arg("-cp")
        .arg(&classpath)
        .arg(MAIN_CLASS)
        .arg(location)
        .output()
        .map_err(|error| format!("cannot run {}: {error}", java.display()))?;

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "the driver exited with {}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
            output.status
        ));
    }
    parse(&stdout)
}

/// Turn the driver's marked lines into a Python dict.
///
/// Unmarked lines are ignored on purpose: a JVM warning on standard output must
/// not fail a run, and a line that *is* marked and malformed must.
fn parse(stdout: &str) -> Result<MontyObject, String> {
    let mut pairs: Vec<(MontyObject, MontyObject)> = Vec::new();
    for line in stdout.lines() {
        let mut fields = line.split_whitespace();
        if fields.next() != Some(MARKER) {
            continue;
        }
        let kind = fields.next().ok_or_else(|| missing("a type", line))?;
        let name = fields.next().ok_or_else(|| missing("a name", line))?;
        // A list may legitimately be empty, so the value is the only field
        // allowed to be absent, and only for a list.
        let value = fields.next();
        if fields.next().is_some() {
            return Err(format!("the driver emitted a trailing field: {line:?}"));
        }
        let object = match (kind, value) {
            ("int", Some(v)) => int_obj(
                v.parse::<u64>()
                    .map_err(|error| format!("{v:?} is not an unsigned integer: {error}"))?,
            ),
            ("bool", Some("true")) => MontyObject::Bool(true),
            ("bool", Some("false")) => MontyObject::Bool(false),
            ("str", Some(v)) => MontyObject::String(v.to_owned()),
            ("list", None) => MontyObject::List(Vec::new()),
            ("list", Some(v)) => MontyObject::List(
                v.split(',')
                    .map(|part| {
                        part.parse::<u64>()
                            .map(int_obj)
                            .map_err(|error| format!("{part:?} is not an ordinal: {error}"))
                    })
                    .collect::<Result<Vec<_>, _>>()?,
            ),
            _ => return Err(format!("the driver emitted an unreadable line: {line:?}")),
        };
        pairs.push((MontyObject::String(name.to_owned()), object));
    }
    if pairs.is_empty() {
        return Err(format!(
            "the driver emitted no observations at all:\n{stdout}"
        ));
    }
    Ok(MontyObject::Dict(pairs.into()))
}

fn missing(what: &str, line: &str) -> String {
    format!("the driver emitted a line with no {what}: {line:?}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The parsed dict, flattened so a test can index it. `DictPairs` keeps
    /// insertion order and exposes only `IntoIterator`, so this is also the
    /// check that the order survived.
    fn flatten(object: &MontyObject) -> Vec<(String, MontyObject)> {
        let MontyObject::Dict(entries) = object else {
            panic!("not a dict: {object:?}")
        };
        entries
            .into_iter()
            .map(|(k, v)| match k {
                MontyObject::String(s) => (s.clone(), v.clone()),
                other => panic!("a non-string key: {other:?}"),
            })
            .collect()
    }

    #[test]
    fn unmarked_output_is_ignored_and_marked_output_is_typed() {
        let parsed = parse(
            "Picked up JAVA_TOOL_OPTIONS: -Dfoo\n\
             yesno-live int cardinality_a 5\n\
             yesno-live bool supports_mixed_put true\n\
             yesno-live str short_ticket_status INVALID_ARGUMENT\n\
             yesno-live list ordinals_a 1,3,5\n\
             yesno-live list ordinals_d_aborted\n",
        )
        .expect("the driver's own shapes must parse");
        let flat = flatten(&parsed);
        let names: Vec<&str> = flat.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            [
                "cardinality_a",
                "supports_mixed_put",
                "short_ticket_status",
                "ordinals_a",
                "ordinals_d_aborted"
            ]
        );
        assert_eq!(flat[0].1, MontyObject::Int(5));
        assert_eq!(flat[1].1, MontyObject::Bool(true));
        assert_eq!(
            flat[2].1,
            MontyObject::String("INVALID_ARGUMENT".to_owned())
        );
        assert_eq!(
            flat[3].1,
            MontyObject::List(vec![
                MontyObject::Int(1),
                MontyObject::Int(3),
                MontyObject::Int(5)
            ])
        );
        // The empty list is the case worth pinning: it is the only one where
        // the value field is absent, and it is exactly what a staged-but-
        // uncommitted transaction must read back as. Treating a missing value
        // as a malformed line would turn the scenario's strongest assertion
        // into a harness error.
        assert_eq!(flat[4].1, MontyObject::List(Vec::new()));
    }

    #[test]
    fn a_driver_that_printed_nothing_is_a_failure_and_not_an_empty_dict() {
        let error = parse("Error: a JVM crash dump\n").expect_err("silence must not pass");
        assert!(error.contains("no observations"), "{error}");
    }

    #[test]
    fn a_malformed_marked_line_fails_rather_than_being_skipped() {
        for line in [
            "yesno-live int cardinality_a five\n",
            "yesno-live bool supports_mixed_put yes\n",
            "yesno-live int cardinality_a 5 6\n",
            "yesno-live list ordinals_a 1,x\n",
            "yesno-live int\n",
        ] {
            parse(line).expect_err(line);
        }
    }
}
