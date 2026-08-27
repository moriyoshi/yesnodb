//! CLI-facing hot base-backup orchestration shared with the E2E harness.

use std::path::PathBuf;

use clap::Args;
use tonic::transport::Channel;

use crate::transport::{self, ClientTls};

use super::{base_backup, BaseBackupReport};

#[derive(Args, Debug)]
pub struct BasebackupOptions {
    /// The leader's shared control-plane endpoint.
    #[arg(
        long,
        default_value = "http://127.0.0.1:50052",
        env = "YESNO_BASEBACKUP_ENDPOINT"
    )]
    endpoint: String,

    /// New directory to publish. It must not already exist.
    #[arg(short = 'D', long, value_name = "DIR")]
    target: PathBuf,

    /// PEM of the CA that signed the control-plane server's certificate.
    #[arg(long, value_name = "FILE", env = "YESNO_BASEBACKUP_CA")]
    ca: Option<PathBuf>,
    /// Client certificate for control-plane mutual TLS.
    #[arg(
        long,
        value_name = "FILE",
        requires = "key",
        env = "YESNO_BASEBACKUP_CERT"
    )]
    cert: Option<PathBuf>,
    /// Private key for `--cert`.
    #[arg(
        long,
        value_name = "FILE",
        requires = "cert",
        env = "YESNO_BASEBACKUP_KEY"
    )]
    key: Option<PathBuf>,
    /// Name to verify when the certificate name differs from the endpoint host.
    #[arg(long, value_name = "NAME", env = "YESNO_BASEBACKUP_SERVER_NAME")]
    server_name: Option<String>,
}

/// One base-backup client failure.
pub type BasebackupError = Box<dyn std::error::Error + Send + Sync>;

impl BasebackupOptions {
    /// Configure a plaintext loopback backup, as used by the E2E harness.
    pub fn plaintext(endpoint: String, target: PathBuf) -> Self {
        Self {
            endpoint,
            target,
            ca: None,
            cert: None,
            key: None,
            server_name: None,
        }
    }
}

async fn connect(cli: &BasebackupOptions) -> Result<Channel, BasebackupError> {
    transport::connect(
        &cli.endpoint,
        ClientTls {
            ca: cli.ca.as_deref(),
            cert: cli.cert.as_deref(),
            key: cli.key.as_deref(),
            server_name: cli.server_name.as_deref(),
        },
    )
    .await
}

fn uuid_hex(uuid: &[u8; 16]) -> String {
    uuid.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Take and durably publish one server-coordinated hot base backup.
pub async fn run(cli: BasebackupOptions) -> Result<BaseBackupReport, BasebackupError> {
    if cli.target.exists() {
        return Err(format!(
            "backup target '{}' already exists; choose a new path",
            cli.target.display()
        )
        .into());
    }
    let channel = connect(&cli).await?;
    base_backup(channel, &cli.target).await
}

/// Render the stable human-facing completion line.
pub fn completion_line(report: &BaseBackupReport) -> String {
    format!(
        "base backup complete: target={} uuid={} term={} shards={} checkpoint={} recovered={} bytes={} attempts={}",
        report.target.display(),
        uuid_hex(&report.db_uuid),
        report.term,
        report.shards,
        report.checkpoint_version,
        report.recovered_version,
        report.bytes,
        report.attempts
    )
}
