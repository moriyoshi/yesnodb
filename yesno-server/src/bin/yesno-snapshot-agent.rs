//! Privileged local snapshot executor for `yesnod`.

use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use yesno_server::config::{Config, EbsMaterialization, SnapshotBackend};

#[derive(Debug, Parser)]
#[command(
    name = "yesno-snapshot-agent",
    about = "Execute privileged local snapshot work received from yesnod",
    version
)]
struct Args {
    #[arg(long, value_name = "FILE", env = "YESNOD_CONFIG")]
    config: PathBuf,

    #[arg(
        long,
        value_name = "FILTER",
        env = "YESNOD_LOG",
        default_value = "info"
    )]
    log: String,
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let args = Args::parse();
    yesno_server::init_tracing(&args.log);
    let config = match Config::from_file(&args.config).and_then(|config| {
        // The agent opens no listeners. Permit listener choices that the
        // separately launched daemon may have explicitly authorized while
        // retaining all structural and snapshot validation.
        config.validate_with(true, true)?;
        Ok(config)
    }) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("yesno-snapshot-agent: {error}");
            return std::process::ExitCode::from(2);
        }
    };
    // Which backends need this process is a property of the backend, not of
    // the operator's intent, so let the executor answer rather than repeating
    // the rule here: LVM always, EBS only when it materializes locally.
    if !matches!(
        config.server.snapshot.backend,
        SnapshotBackend::Lvm | SnapshotBackend::Ebs
    ) {
        eprintln!("yesno-snapshot-agent: server.snapshot.backend must be 'lvm' or 'ebs'");
        return std::process::ExitCode::from(2);
    }
    if let Some(ebs) = config.server.snapshot.ebs.as_ref() {
        // Fail at startup rather than in the reconnect loop: a deferred
        // deployment that also enabled this unit would otherwise retry forever.
        if ebs.materialization != EbsMaterialization::Local {
            eprintln!(
                "yesno-snapshot-agent: deferred EBS materialization mounts in the archiver's worker and needs no local agent"
            );
            return std::process::ExitCode::from(2);
        }
    }
    let Some(control_socket) = config.control_unix_socket() else {
        eprintln!("yesno-snapshot-agent: server.control.unix_socket is required");
        return std::process::ExitCode::from(2);
    };

    loop {
        tokio::select! {
            result = yesno_server::snapshot::run_snapshot_agent(
                control_socket,
                config.data_dir(),
                &config.server.snapshot,
            ) => {
                tracing::warn!(error = ?result.unwrap_err(), "snapshot-agent connection ended; retrying");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            signal = tokio::signal::ctrl_c() => {
                if let Err(error) = signal {
                    tracing::error!(%error, "cannot install snapshot-agent shutdown signal");
                    return std::process::ExitCode::FAILURE;
                }
                return std::process::ExitCode::SUCCESS;
            }
        }
    }
}
