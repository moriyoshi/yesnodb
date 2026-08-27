//! `yesnoctl` — administrative commands for a running or archived yesno database.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use tonic::metadata::AsciiMetadataValue;
use tonic::transport::Channel;
use tonic::Request;
use yesno_server::control::pb;
use yesno_server_utils::basebackup::{
    completion_line as basebackup_completion, run as run_basebackup, BasebackupOptions,
};
use yesno_server_utils::restore::{
    completion_line as restore_completion, run as run_restore, RestoreOptions,
};
use yesno_server_utils::transport::{self, ClientTls};

type Fail = Box<dyn std::error::Error + Send + Sync>;

#[derive(Parser, Debug)]
#[command(
    name = "yesnoctl",
    about = "Administrative utilities for yesnod",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Force the active leader to checkpoint and print its watermark.
    Checkpoint(CheckpointOptions),
    /// Take a hot, restorable copy from a running server.
    Basebackup(BasebackupOptions),
    /// Restore a protobuf archive to a commit version or a wall-clock instant.
    Restore(RestoreOptions),
}

#[derive(Args, Debug)]
struct CheckpointOptions {
    /// The leader's shared control-plane endpoint.
    #[arg(
        long,
        default_value = "http://127.0.0.1:50052",
        env = "YESNOCTL_ENDPOINT"
    )]
    endpoint: String,

    /// PEM of the CA that signed the control-plane server's certificate.
    #[arg(long, value_name = "FILE", env = "YESNOCTL_CA")]
    ca: Option<PathBuf>,
    /// Client certificate for control-plane mutual TLS.
    #[arg(long, value_name = "FILE", requires = "key", env = "YESNOCTL_CERT")]
    cert: Option<PathBuf>,
    /// Private key for `--cert`.
    #[arg(long, value_name = "FILE", requires = "cert", env = "YESNOCTL_KEY")]
    key: Option<PathBuf>,
    /// Name to verify when the certificate name differs from the endpoint host.
    #[arg(long, value_name = "NAME", env = "YESNOCTL_SERVER_NAME")]
    server_name: Option<String>,
    /// File holding the bearer token. The token is never accepted in argv.
    #[arg(long, value_name = "FILE", env = "YESNOCTL_TOKEN_FILE")]
    token_file: Option<PathBuf>,
}

async fn connect(options: &CheckpointOptions) -> Result<Channel, Fail> {
    transport::connect(
        &options.endpoint,
        ClientTls {
            ca: options.ca.as_deref(),
            cert: options.cert.as_deref(),
            key: options.key.as_deref(),
            server_name: options.server_name.as_deref(),
        },
    )
    .await
}

async fn checkpoint(options: CheckpointOptions) -> Result<u64, Fail> {
    let channel = connect(&options).await?;
    let mut client = pb::control_plane_client::ControlPlaneClient::new(channel);
    let mut request = Request::new(pb::CheckpointRequest {});
    if let Some(path) = &options.token_file {
        let token = std::fs::read_to_string(path)?;
        let value: AsciiMetadataValue = format!("Bearer {}", token.trim()).parse()?;
        request.metadata_mut().insert("authorization", value);
    }
    Ok(client.checkpoint(request).await?.into_inner().watermark)
}

async fn run(cli: Cli) -> Result<String, Fail> {
    match cli.command {
        Command::Checkpoint(options) => Ok(format!(
            "checkpoint at version {}",
            checkpoint(options).await?
        )),
        Command::Basebackup(options) => Ok(basebackup_completion(&run_basebackup(options).await?)),
        // `--inspect` prints its windows itself and produces no report; the
        // command still succeeded, so it gets a summary line rather than a
        // placeholder that reads like a failure.
        Command::Restore(options) => Ok(match run_restore(options).await? {
            Some(report) => restore_completion(&report),
            None => "inspect complete: no directory was created".to_string(),
        }),
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(Cli::parse()).await {
        Ok(line) => {
            println!("{line}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprint!("yesnoctl: {error}");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn administrative_commands_are_nested() {
        assert!(Cli::try_parse_from(["yesnoctl", "checkpoint"]).is_ok());
        assert!(Cli::try_parse_from(["yesnoctl", "basebackup", "-D", "backup"]).is_ok());
        assert!(Cli::try_parse_from([
            "yesnoctl",
            "restore",
            "--store",
            "file:///archive",
            "-D",
            "restored",
        ])
        .is_ok());
    }
}
