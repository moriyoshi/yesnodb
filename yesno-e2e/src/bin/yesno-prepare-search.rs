//! Build-only helper for populating the shared external-integration image.
//!
//! This is not a scenario entrypoint. The PostgreSQL and MySQL Bazel runners
//! deliberately link a featureless fixture host, while search preparation
//! needs the external-only Java and download module.

use std::process::ExitCode;

fn main() -> ExitCode {
    match yesno_e2e::search::prepare_artifacts() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("cannot prepare search artifacts: {error}");
            ExitCode::FAILURE
        }
    }
}
