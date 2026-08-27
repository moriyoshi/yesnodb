//! `yesno-archive` — continuously archive a running database to object storage.

use std::process::ExitCode;

use clap::Parser;
use yesno_server_utils::sidecar::{run_with_shutdown, ArchiveOptions};

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = terminate.recv() => {}
                }
            }
            Err(error) => {
                tracing::warn!(%error, "cannot install SIGTERM handler; waiting for SIGINT");
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    yesno_server::init_tracing("yesno_server=info");
    let options = ArchiveOptions::parse();
    match run_with_shutdown(options, shutdown_signal()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprint!("yesno-archive: {error}");
            let mut source = error.source();
            while let Some(cause) = source {
                eprint!("\n  caused by: {cause}");
                source = cause.source();
            }
            eprintln!();
            ExitCode::FAILURE
        }
    }
}
