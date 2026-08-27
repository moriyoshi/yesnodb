//! One-shot worker used by ECS/Fargate EBS snapshot materialization.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

#[path = "../snapshot/stage.rs"]
mod stage;

#[derive(Debug, Parser)]
#[command(
    name = "yesno-snapshot-stage",
    about = "Stage an ECS-mounted yesno EBS snapshot into shared storage",
    version
)]
struct Options {
    #[arg(long)]
    source: PathBuf,
    #[arg(long)]
    target: PathBuf,
}

fn main() -> ExitCode {
    let options = Options::parse();
    match stage::copy_database_snapshot(&options.source, &options.target) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("yesno-snapshot-stage: {error}");
            ExitCode::FAILURE
        }
    }
}
