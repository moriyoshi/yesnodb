//! Continuous archive orchestration shared by the CLI and E2E harness.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::archive::pb::{self as archive_pb, BaseManifest, SnapshotSource, WalCursor};
use crate::transport::{self, ClientTls};

use crate::archive::{
    cursor, cursor_fingerprint, inspect_base, new_state, set_cursor_history,
    validate_base_manifest, ArchiveError, ArchiveStore, SCHEMA_VERSION,
};
use clap::{Parser, ValueEnum};
use prost::Message;
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex, Notify};
use tonic::transport::Channel;
use tonic::Code;
use yesno_server::control::pb::control_plane_client::ControlPlaneClient;
use yesno_server::control::pb::{
    event_envelope, snapshot_file_chunk, subscribe_item, BaseSnapshotLease, BaseSnapshotSource,
    BeginBaseSnapshotRequest, EventPhase, FetchSnapshotFileRequest, KeepBaseSnapshotAliveRequest,
    ReleaseBaseSnapshotRequest, SubscribeRequest,
};
use yesno_server::replication::pb::replication_client::ReplicationClient;
use yesno_server::replication::pb::{
    AckRequest, StatusRequest, SubscribeRequest as WalSubscribeRequest,
};

use crate::basebackup::{replication_base_backup, BaseBackupOptions};
use crate::deferred::{DeferredMaterializer, EcsConfig, EksConfig};
use crate::lease::{new_writer_id, WriterLease};

type SharedState = Arc<Mutex<archive_pb::ArchiveState>>;
#[derive(Clone, Copy)]
struct BaseRequest {
    sequence: u64,
    reset_cursors: bool,
}

static CAPTURE_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum SnapshotModeArg {
    Auto,
    Network,
    Server,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum DeferredMaterializerArg {
    Ecs,
    Eks,
}

#[derive(Parser, Debug)]
#[command(
    name = "yesno-archive",
    about = "Archive yesno WAL and base images to object storage",
    version
)]
pub struct ArchiveOptions {
    /// Shared control/replication gRPC endpoint.
    #[arg(
        long,
        default_value = "http://127.0.0.1:50052",
        env = "YESNO_ARCHIVE_ENDPOINT"
    )]
    endpoint: String,

    /// Object-store URL: file:///path, s3://bucket/prefix, or compatible S3.
    #[arg(long, env = "YESNO_ARCHIVE_STORE")]
    store: String,

    /// Local protobuf cursor cache and network-backup staging directory.
    #[arg(long, default_value = ".yesno-archive", env = "YESNO_ARCHIVE_WORK_DIR")]
    work_dir: PathBuf,

    /// Base acquisition: prefer a server snapshot, require one, or use the
    /// portable replication stream. `auto` falls back to `network` when the
    /// server cannot provide a snapshot lease.
    #[arg(long, value_enum, default_value = "auto")]
    snapshot_mode: SnapshotModeArg,

    /// Ask an explicitly configured server for snapshot-local paths instead of
    /// streaming bytes. Both processes must see the same read-only mount.
    #[arg(long)]
    direct_snapshot_path: bool,

    /// Materialize provisional EBS leases with an ECS task or EKS Job.
    #[arg(long, value_enum, env = "YESNO_ARCHIVE_DEFERRED_MATERIALIZER")]
    deferred_materializer: Option<DeferredMaterializerArg>,

    /// EBS filesystem mount path inside the launched materializer.
    #[arg(long, env = "YESNO_ARCHIVE_MATERIALIZER_SOURCE_PATH")]
    materializer_source_path: Option<PathBuf>,

    /// Shared staging path mounted at the same path in this archiver and the launched workload.
    #[arg(long, env = "YESNO_ARCHIVE_MATERIALIZER_STAGING_PATH")]
    materializer_staging_path: Option<PathBuf>,

    /// Maximum seconds to wait for a launched materializer.
    #[arg(long, default_value = "3600")]
    materializer_timeout_secs: u64,

    #[arg(long, env = "YESNO_ARCHIVE_ECS_CLUSTER")]
    ecs_cluster: Option<String>,
    #[arg(long, env = "YESNO_ARCHIVE_ECS_TASK_DEFINITION")]
    ecs_task_definition: Option<String>,
    #[arg(long, env = "YESNO_ARCHIVE_ECS_CONTAINER_NAME")]
    ecs_container_name: Option<String>,
    #[arg(long, env = "YESNO_ARCHIVE_ECS_VOLUME_NAME")]
    ecs_volume_name: Option<String>,
    #[arg(long, env = "YESNO_ARCHIVE_ECS_INFRASTRUCTURE_ROLE_ARN")]
    ecs_infrastructure_role_arn: Option<String>,
    #[arg(
        long = "ecs-subnet",
        env = "YESNO_ARCHIVE_ECS_SUBNETS",
        value_delimiter = ','
    )]
    ecs_subnets: Vec<String>,
    #[arg(
        long = "ecs-security-group",
        env = "YESNO_ARCHIVE_ECS_SECURITY_GROUPS",
        value_delimiter = ','
    )]
    ecs_security_groups: Vec<String>,
    #[arg(long)]
    ecs_assign_public_ip: bool,

    #[arg(long, env = "YESNO_ARCHIVE_EKS_NAMESPACE")]
    eks_namespace: Option<String>,
    #[arg(long, env = "YESNO_ARCHIVE_EKS_IMAGE")]
    eks_image: Option<String>,
    #[arg(long, env = "YESNO_ARCHIVE_EKS_SERVICE_ACCOUNT")]
    eks_service_account: Option<String>,
    #[arg(long, env = "YESNO_ARCHIVE_EKS_SNAPSHOT_CLASS")]
    eks_snapshot_class: Option<String>,
    #[arg(long, env = "YESNO_ARCHIVE_EKS_STORAGE_CLASS")]
    eks_storage_class: Option<String>,
    #[arg(
        long,
        default_value = "ebs.csi.aws.com",
        env = "YESNO_ARCHIVE_EKS_CSI_DRIVER"
    )]
    eks_csi_driver: String,
    #[arg(long, env = "YESNO_ARCHIVE_EKS_STAGING_CLAIM")]
    eks_staging_claim: Option<String>,
    /// Label selector for EBS-capable EKS worker nodes, as key=value.
    #[arg(long = "eks-node-selector", value_delimiter = ',')]
    eks_node_selector: Vec<String>,

    /// PEM of the CA that signed the shared endpoint certificate.
    #[arg(long, value_name = "FILE", env = "YESNO_ARCHIVE_CA")]
    ca: Option<PathBuf>,
    /// Client certificate. Its authenticated principal needs control-read and replication.
    #[arg(
        long,
        value_name = "FILE",
        requires = "key",
        env = "YESNO_ARCHIVE_CERT"
    )]
    cert: Option<PathBuf>,
    /// Private key for `--cert`.
    #[arg(
        long,
        value_name = "FILE",
        requires = "cert",
        env = "YESNO_ARCHIVE_KEY"
    )]
    key: Option<PathBuf>,
    /// TLS name to verify when it differs from the endpoint host.
    #[arg(long, value_name = "NAME", env = "YESNO_ARCHIVE_SERVER_NAME")]
    server_name: Option<String>,

    /// Complete network-base attempts before retrying the event.
    #[arg(long, default_value = "3")]
    max_base_attempts: u32,

    /// Maximum replication batch bytes. Zero asks the server for its default.
    #[arg(long, default_value_t = 0)]
    max_batch_bytes: u32,

    // Conditional object-store writer lease lifetime.
    #[arg(long, default_value = "30")]
    writer_lease_ttl_secs: u64,

    /// Keep every wall-clock instant within this window restorable, and reclaim
    /// archive objects outside it. Accepts 7d, 72h, 90m or 3600s.
    ///
    /// Omitting it disables reclamation entirely, which is the default: an
    /// archive that has never been told what to keep is not one to start
    /// deleting from.
    #[arg(long, value_name = "DURATION")]
    retention_window: Option<String>,

    /// Newest base generations to keep regardless of the window, so a database
    /// quiet for longer than the window still has a recovery root.
    #[arg(long, default_value_t = 2)]
    retention_min_bases: usize,

    /// Seconds between reclamation passes. Ignored without --retention-window.
    #[arg(long, default_value_t = 3600)]
    retention_interval_secs: u64,

    /// Serve `/metrics` and `/healthz` on this address.
    ///
    /// Reclamation decides what an archive destroys, and the unrecognized-key
    /// count it reports is an alerting condition rather than a log line — so a
    /// deployment that reclaims should scrape this.
    ///
    /// Off by default, unlike the daemon's own listener, which defaults to a
    /// loopback address. The daemon is one process per data directory; sidecars
    /// are routinely run several to a host, one per archived database, and a
    /// default port would make the second one fail to start on a bind the
    /// operator never asked for. Loopback is still the right *value* when it is
    /// asked for: this listener is unauthenticated.
    #[arg(long, value_name = "ADDR", env = "YESNO_ARCHIVE_METRICS_ADDR")]
    metrics_addr: Option<std::net::SocketAddr>,
}

/// Parse `7d` / `72h` / `90m` / `3600s` into microseconds.
///
/// A bare number is refused. "Retention 7" is seven of something, and every
/// wrong guess about which deletes recoverable history.
pub(crate) fn parse_duration_micros(text: &str) -> Result<u64, ArchiveError> {
    let text = text.trim();
    let (digits, unit) = text.split_at(text.len().saturating_sub(1));
    let scale = match unit {
        "s" => 1_000_000u64,
        "m" => 60 * 1_000_000,
        "h" => 3_600 * 1_000_000,
        "d" => 86_400 * 1_000_000,
        _ => {
            return Err(
                format!("'{text}' needs a unit: s, m, h or d, for example 7d or 72h").into(),
            )
        }
    };
    let value: u64 = digits
        .parse()
        .map_err(|_| format!("'{text}' is not a whole number of {unit}"))?;
    if value == 0 {
        return Err("a retention window of zero would keep nothing restorable".into());
    }
    value
        .checked_mul(scale)
        .ok_or_else(|| format!("retention window '{text}' overflows").into())
}

#[derive(Clone)]
struct Runtime {
    channel: Channel,
    store: ArchiveStore,
    state: SharedState,
    local_state: PathBuf,
    work_dir: PathBuf,
    snapshot_mode: SnapshotModeArg,
    direct_snapshot_path: bool,
    deferred_materializer: Option<DeferredMaterializer>,
    max_base_attempts: u32,
    max_batch_bytes: u32,
    term: u32,
    rebootstrap: Arc<Notify>,
    base_request: Arc<Mutex<Option<BaseRequest>>>,
}

impl ArchiveOptions {
    /// Configure the hermetic plaintext/network-base shape used by the E2E
    /// harness. Production callers normally obtain the same fields from Clap.
    pub fn network(endpoint: String, store: String, work_dir: PathBuf) -> Self {
        Self {
            endpoint,
            store,
            work_dir,
            snapshot_mode: SnapshotModeArg::Network,
            direct_snapshot_path: false,
            deferred_materializer: None,
            materializer_source_path: None,
            materializer_staging_path: None,
            materializer_timeout_secs: 3_600,
            ecs_cluster: None,
            ecs_task_definition: None,
            ecs_container_name: None,
            ecs_volume_name: None,
            ecs_infrastructure_role_arn: None,
            ecs_subnets: Vec::new(),
            ecs_security_groups: Vec::new(),
            ecs_assign_public_ip: false,
            eks_namespace: None,
            eks_image: None,
            eks_service_account: None,
            eks_snapshot_class: None,
            eks_storage_class: None,
            eks_csi_driver: "ebs.csi.aws.com".to_owned(),
            eks_staging_claim: None,
            eks_node_selector: Vec::new(),
            ca: None,
            cert: None,
            key: None,
            server_name: None,
            max_base_attempts: 3,
            max_batch_bytes: 0,
            writer_lease_ttl_secs: 30,
            // Reclamation off by default here as on the command line. A test
            // fixture that silently deleted archive objects would make every
            // other archive assertion conditional on timing.
            retention_window: None,
            retention_min_bases: 2,
            retention_interval_secs: 3_600,
            metrics_addr: None,
        }
    }

    /// Serve the sidecar's Prometheus surface, for the E2E harness.
    pub fn with_metrics(mut self, addr: std::net::SocketAddr) -> Self {
        self.metrics_addr = Some(addr);
        self
    }

    /// Enable reclamation, for the E2E harness.
    pub fn with_retention(mut self, window: String, min_bases: usize, interval_secs: u64) -> Self {
        self.retention_window = Some(window);
        self.retention_min_bases = min_bases;
        self.retention_interval_secs = interval_secs;
        self
    }

    /// The configured policy, or `None` when reclamation is off.
    fn retention(&self) -> Result<Option<crate::gc::RetentionPolicy>, ArchiveError> {
        let Some(text) = self.retention_window.as_deref() else {
            return Ok(None);
        };
        Ok(Some(crate::gc::RetentionPolicy {
            window_micros: parse_duration_micros(text)?,
            min_bases: self.retention_min_bases.max(1),
        }))
    }

    fn validate(&self) -> Result<(), ArchiveError> {
        if self.writer_lease_ttl_secs < 3 {
            return Err("--writer-lease-ttl-secs must be at least 3".into());
        }
        if self.direct_snapshot_path && self.snapshot_mode == SnapshotModeArg::Network {
            return Err(
                "--direct-snapshot-path cannot be used with --snapshot-mode network".into(),
            );
        }
        self.deferred_config()?;
        // Parsed at validation, not at the first pass an hour later. An
        // unparseable window means reclamation silently never runs, and the
        // operator finds out when the bucket fills.
        self.retention()?;
        if self.retention_interval_secs == 0 {
            return Err("--retention-interval-secs must be at least 1".into());
        }
        Ok(())
    }

    fn deferred_config(&self) -> Result<Option<DeferredMaterializer>, ArchiveError> {
        let Some(kind) = self.deferred_materializer else {
            return Ok(None);
        };
        if self.materializer_timeout_secs == 0 {
            return Err("--materializer-timeout-secs must be positive".into());
        }
        let source_path = required_path(
            self.materializer_source_path.as_ref(),
            "--materializer-source-path",
        )?;
        let staging_path = required_path(
            self.materializer_staging_path.as_ref(),
            "--materializer-staging-path",
        )?;
        if source_path == staging_path {
            return Err("materializer source and staging paths must differ".into());
        }
        let timeout = Duration::from_secs(self.materializer_timeout_secs);
        Ok(Some(match kind {
            DeferredMaterializerArg::Ecs => {
                if self.ecs_subnets.is_empty() || self.ecs_subnets.len() > 16 {
                    return Err("ECS materialization needs 1 to 16 --ecs-subnet values".into());
                }
                DeferredMaterializer::Ecs(EcsConfig {
                    cluster: required_string(&self.ecs_cluster, "--ecs-cluster")?,
                    task_definition: required_string(
                        &self.ecs_task_definition,
                        "--ecs-task-definition",
                    )?,
                    container_name: required_string(
                        &self.ecs_container_name,
                        "--ecs-container-name",
                    )?,
                    volume_name: required_string(&self.ecs_volume_name, "--ecs-volume-name")?,
                    infrastructure_role_arn: required_string(
                        &self.ecs_infrastructure_role_arn,
                        "--ecs-infrastructure-role-arn",
                    )?,
                    subnets: self.ecs_subnets.clone(),
                    security_groups: self.ecs_security_groups.clone(),
                    assign_public_ip: self.ecs_assign_public_ip,
                    source_path,
                    staging_path,
                    timeout,
                })
            }
            DeferredMaterializerArg::Eks => DeferredMaterializer::Eks(EksConfig {
                namespace: required_string(&self.eks_namespace, "--eks-namespace")?,
                image: required_string(&self.eks_image, "--eks-image")?,
                service_account: self.eks_service_account.clone(),
                snapshot_class: required_string(&self.eks_snapshot_class, "--eks-snapshot-class")?,
                storage_class: required_string(&self.eks_storage_class, "--eks-storage-class")?,
                csi_driver: self.eks_csi_driver.clone(),
                staging_claim: required_string(&self.eks_staging_claim, "--eks-staging-claim")?,
                node_selector: key_values(&self.eks_node_selector, "--eks-node-selector")?,
                source_path,
                staging_path,
                timeout,
            }),
        }))
    }
}

fn required_string(value: &Option<String>, flag: &str) -> Result<String, ArchiveError> {
    value
        .as_ref()
        .filter(|value| !value.trim().is_empty())
        .cloned()
        .ok_or_else(|| format!("deferred materialization needs {flag}").into())
}

fn required_path(value: Option<&PathBuf>, flag: &str) -> Result<PathBuf, ArchiveError> {
    value
        .filter(|path| path.is_absolute())
        .cloned()
        .ok_or_else(|| format!("deferred materialization needs absolute {flag}").into())
}

fn key_values(
    values: &[String],
    flag: &str,
) -> Result<std::collections::BTreeMap<String, String>, ArchiveError> {
    let mut parsed = std::collections::BTreeMap::new();
    for value in values {
        let (key, value) = value
            .split_once('=')
            .filter(|(key, value)| !key.is_empty() && !value.is_empty())
            .ok_or_else(|| format!("{flag} must use key=value"))?;
        if parsed.insert(key.to_owned(), value.to_owned()).is_some() {
            return Err(format!("{flag} repeats key '{key}'").into());
        }
    }
    Ok(parsed)
}

async fn connect(cli: &ArchiveOptions) -> Result<Channel, ArchiveError> {
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
    .map_err(|error| {
        format!(
            "cannot connect to control endpoint '{}': {error}",
            cli.endpoint
        )
        .into()
    })
}

async fn ack(channel: &Channel, shard: u32, lsn: u64) -> Result<(), ArchiveError> {
    let mut client = ReplicationClient::new(channel.clone());
    client
        .ack(tokio_stream::iter([AckRequest {
            shard,
            applied_lsn: lsn,
        }]))
        .await?;
    Ok(())
}

async fn capture_base(
    runtime: &Runtime,
    sequence: u64,
    establish_cursors: bool,
) -> Result<(), ArchiveError> {
    let archive_generation = runtime.state.lock().await.base_generation + 1;
    match runtime.snapshot_mode {
        SnapshotModeArg::Network => {
            capture_network_base(runtime, sequence, archive_generation, establish_cursors).await
        }
        SnapshotModeArg::Auto | SnapshotModeArg::Server => {
            let lease = ControlPlaneClient::new(runtime.channel.clone())
                .begin_base_snapshot(BeginBaseSnapshotRequest {})
                .await;
            match lease {
                Ok(lease) => {
                    capture_server_base(
                        runtime,
                        sequence,
                        archive_generation,
                        establish_cursors,
                        lease.into_inner(),
                    )
                    .await
                }
                Err(status)
                    if runtime.snapshot_mode == SnapshotModeArg::Auto
                        && matches!(
                            status.code(),
                            Code::FailedPrecondition | Code::Unimplemented
                        ) =>
                {
                    capture_network_base(runtime, sequence, archive_generation, establish_cursors)
                        .await
                }
                Err(status) => Err(status.into()),
            }
        }
    }
}

fn capture_target(runtime: &Runtime, archive_generation: u64) -> Result<PathBuf, ArchiveError> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let id = CAPTURE_ID.fetch_add(1, Ordering::Relaxed);
    let target = runtime.work_dir.join(format!(
        "base-{archive_generation:020}-{}-{nonce:032x}-{id:016x}",
        std::process::id(),
    ));
    if target.exists() {
        return Err(format!(
            "stale base directory '{}' exists; remove it after inspection",
            target.display()
        )
        .into());
    }
    Ok(target)
}

async fn capture_network_base(
    runtime: &Runtime,
    sequence: u64,
    archive_generation: u64,
    establish_cursors: bool,
) -> Result<(), ArchiveError> {
    tokio::fs::create_dir_all(&runtime.work_dir).await?;
    let target = capture_target(runtime, archive_generation)?;
    let mut client = ReplicationClient::new(runtime.channel.clone());
    let report = replication_base_backup(
        &mut client,
        &target,
        BaseBackupOptions {
            max_attempts: runtime.max_base_attempts,
            max_batch_bytes: runtime.max_batch_bytes,
        },
    )
    .await?;
    let result = publish_captured_base(
        runtime,
        &target,
        SnapshotSource::Network,
        report.recovered_version,
        sequence,
        archive_generation,
        establish_cursors,
    )
    .await;
    let cleanup = remove_staged_base(&target).await;
    result.and(cleanup)
}

async fn capture_server_base(
    runtime: &Runtime,
    sequence: u64,
    archive_generation: u64,
    establish_cursors: bool,
    lease: BaseSnapshotLease,
) -> Result<(), ArchiveError> {
    let lease_id = lease.lease_id.clone();
    let ttl_secs = lease.lease_ttl_secs;
    let capture = capture_server_snapshot(
        runtime,
        sequence,
        archive_generation,
        establish_cursors,
        &lease,
    );
    tokio::pin!(capture);
    let keepalive = keep_snapshot_alive(runtime.channel.clone(), lease_id.clone(), ttl_secs);
    tokio::pin!(keepalive);
    let result = tokio::select! {
        result = &mut capture => result,
        result = &mut keepalive => match result {
            Ok(()) => Err("snapshot keepalive stopped unexpectedly".into()),
            Err(error) => Err(error),
        },
    };
    let release = ControlPlaneClient::new(runtime.channel.clone())
        .release_base_snapshot(ReleaseBaseSnapshotRequest { lease_id })
        .await
        .map(|_| ())
        .map_err(ArchiveError::from);
    result.and(release)
}

async fn capture_server_snapshot(
    runtime: &Runtime,
    sequence: u64,
    archive_generation: u64,
    establish_cursors: bool,
    lease: &BaseSnapshotLease,
) -> Result<(), ArchiveError> {
    if lease.lease_id.is_empty()
        || (lease.files.is_empty() && lease.deferred_ebs.is_none())
        || (!lease.files.is_empty() && lease.deferred_ebs.is_some())
        || lease.lease_ttl_secs == 0
    {
        return Err("server returned an incomplete snapshot lease".into());
    }
    let source = match BaseSnapshotSource::try_from(lease.source) {
        Ok(BaseSnapshotSource::Zfs) => SnapshotSource::Zfs,
        Ok(BaseSnapshotSource::Btrfs) => SnapshotSource::Btrfs,
        Ok(BaseSnapshotSource::Lvm) => SnapshotSource::Lvm,
        Ok(BaseSnapshotSource::Ebs) => SnapshotSource::Ebs,
        Ok(BaseSnapshotSource::Portable) => SnapshotSource::Network,
        _ => return Err("server returned an unknown snapshot source".into()),
    };
    tokio::fs::create_dir_all(&runtime.work_dir).await?;
    let target = capture_target(runtime, archive_generation)?;
    let capture = async {
        let (path, cleanup_path) = materialize_snapshot(runtime, lease, &target).await?;
        let result = publish_captured_base(
            runtime,
            &path,
            source,
            0,
            sequence,
            archive_generation,
            establish_cursors,
        )
        .await;
        let cleanup = match cleanup_path {
            Some(path) => remove_staged_base(&path).await,
            None => Ok(()),
        };
        result.and(cleanup)
    };
    let result = capture.await;
    if target.exists() {
        let cleanup = remove_staged_base(&target).await;
        if result.is_ok() {
            cleanup?;
        }
    }
    result
}

async fn keep_snapshot_alive(
    channel: Channel,
    lease_id: Vec<u8>,
    ttl_secs: u64,
) -> Result<(), ArchiveError> {
    let interval_ms = ttl_secs.saturating_mul(1000).saturating_div(3).max(100);
    loop {
        tokio::time::sleep(Duration::from_millis(interval_ms)).await;
        ControlPlaneClient::new(channel.clone())
            .keep_base_snapshot_alive(KeepBaseSnapshotAliveRequest {
                lease_id: lease_id.clone(),
            })
            .await?;
    }
}

async fn materialize_snapshot(
    runtime: &Runtime,
    lease: &BaseSnapshotLease,
    target: &Path,
) -> Result<(PathBuf, Option<PathBuf>), ArchiveError> {
    if let Some(snapshot) = &lease.deferred_ebs {
        let materializer = runtime.deferred_materializer.as_ref().ok_or(
            "server returned a deferred EBS lease but yesno-archive has no deferred materializer",
        )?;
        let path = materializer.materialize(snapshot, &lease.lease_id).await?;
        return Ok((path.clone(), Some(path)));
    }
    let mut direct_root: Option<PathBuf> = None;
    let mut streamed = false;
    for expected in &lease.files {
        let relative = Path::new(&expected.name);
        if relative.components().count() != 1
            || relative
                .file_name()
                .is_none_or(|name| name != expected.name.as_str())
        {
            return Err(
                format!("snapshot file name '{}' is not a flat name", expected.name).into(),
            );
        }
        let mut stream = ControlPlaneClient::new(runtime.channel.clone())
            .fetch_snapshot_file(FetchSnapshotFileRequest {
                lease_id: lease.lease_id.clone(),
                name: expected.name.clone(),
                allow_direct_path: runtime.direct_snapshot_path,
            })
            .await?
            .into_inner();
        let mut first = stream
            .message()
            .await?
            .ok_or("snapshot file stream returned no chunks")?;
        match first.payload.take() {
            Some(snapshot_file_chunk::Payload::DirectPath(value)) => {
                if streamed || !first.last || first.offset != 0 || first.total_size != expected.size
                {
                    return Err("malformed direct snapshot-file response".into());
                }
                if stream.message().await?.is_some() {
                    return Err("direct snapshot-file response has trailing chunks".into());
                }
                let path = PathBuf::from(value);
                if path
                    .file_name()
                    .is_none_or(|name| name != expected.name.as_str())
                {
                    return Err("direct snapshot path does not name the requested file".into());
                }
                let size = tokio::fs::metadata(&path).await?.len();
                if size != expected.size {
                    return Err(format!(
                        "direct snapshot file '{}' is {size} bytes, lease says {}",
                        path.display(),
                        expected.size
                    )
                    .into());
                }
                let root = path.parent().ok_or("direct snapshot path has no parent")?;
                if direct_root.as_deref().is_some_and(|known| known != root) {
                    return Err("direct snapshot files do not share one directory".into());
                }
                direct_root = Some(root.to_path_buf());
            }
            Some(snapshot_file_chunk::Payload::Data(data)) => {
                if direct_root.is_some() {
                    return Err("snapshot response mixed direct paths and streamed files".into());
                }
                streamed = true;
                tokio::fs::create_dir_all(target).await?;
                let path = target.join(&expected.name);
                let mut file = tokio::fs::File::create(&path).await?;
                let mut offset = 0u64;
                let mut chunk = first;
                let mut data = data;
                loop {
                    if chunk.offset != offset || chunk.total_size != expected.size {
                        return Err(format!(
                            "snapshot file '{}' has a discontinuous or inconsistent chunk",
                            expected.name
                        )
                        .into());
                    }
                    file.write_all(&data).await?;
                    offset = offset.saturating_add(data.len() as u64);
                    if chunk.last {
                        if offset != expected.size {
                            return Err(format!(
                                "snapshot file '{}' ended at {offset}, expected {}",
                                expected.name, expected.size
                            )
                            .into());
                        }
                        if stream.message().await?.is_some() {
                            return Err("snapshot file has chunks after its last marker".into());
                        }
                        break;
                    }
                    chunk = stream
                        .message()
                        .await?
                        .ok_or("snapshot file stream ended before its last marker")?;
                    data = match chunk.payload.take() {
                        Some(snapshot_file_chunk::Payload::Data(data)) => data,
                        _ => return Err("snapshot byte stream changed access mode".into()),
                    };
                }
                file.sync_all().await?;
            }
            None => return Err("snapshot file chunk has no payload".into()),
        }
    }
    if let Some(root) = direct_root {
        Ok((root, None))
    } else if streamed {
        Ok((target.to_path_buf(), Some(target.to_path_buf())))
    } else {
        Err("snapshot lease contains no files".into())
    }
}

async fn publish_captured_base(
    runtime: &Runtime,
    path: &Path,
    source: SnapshotSource,
    recovered_version: u64,
    sequence: u64,
    archive_generation: u64,
    establish_cursors: bool,
) -> Result<(), ArchiveError> {
    let inspection = inspect_base(path)?;
    if inspection.term != runtime.term {
        return Err(format!(
            "leadership term changed from {} to {} during base capture; restart to fence the new timeline",
            runtime.term, inspection.term
        )
        .into());
    }
    {
        let state = runtime.state.lock().await;
        if state.database_uuid != inspection.database_uuid {
            return Err("base image UUID differs from the archive's database identity".into());
        }
    }
    if !establish_cursors {
        return Err("a published base must establish a new WAL history root".into());
    }
    let manifest = BaseManifest {
        schema_version: SCHEMA_VERSION,
        database_uuid: inspection.database_uuid.to_vec(),
        term: inspection.term,
        event_sequence: sequence,
        checkpoint_version: inspection.checkpoint_version,
        recovered_version: if recovered_version == 0 {
            inspection.checkpoint_version
        } else {
            recovered_version
        },
        source: source as i32,
        files: Vec::new(),
        wal_cursors: inspection.wal_cursors.clone(),
        archive_generation,
        history_anchor: Vec::new(),
        checkpoint_time: inspection.commit_clock,
    };
    let (manifest_key, stored_manifest) = runtime.store.put_base(path, manifest).await?;
    let cursors = {
        let mut state = runtime.state.lock().await;
        for new_cursor in &stored_manifest.wal_cursors {
            let current = cursor(&state, new_cursor.shard).unwrap_or(0);
            if new_cursor.archived_lsn < current {
                return Err(format!(
                    "new base moved shard {} backward from {current} to {}",
                    new_cursor.shard, new_cursor.archived_lsn
                )
                .into());
            }
        }
        state.wal_cursors = stored_manifest.wal_cursors;
        state.event_sequence = state.event_sequence.max(sequence);
        state.base_generation = archive_generation;
        state.latest_base_manifest = manifest_key;
        runtime
            .store
            .publish_state(&runtime.local_state, &state)
            .await?;
        state.wal_cursors.clone()
    };
    for cursor in cursors {
        ack(&runtime.channel, cursor.shard, cursor.archived_lsn).await?;
    }
    Ok(())
}

async fn remove_staged_base(path: &Path) -> Result<(), ArchiveError> {
    // This path was constructed by this process for this capture and did not
    // exist beforehand. It contains no user-authored files.
    tokio::fs::remove_dir_all(path).await?;
    Ok(())
}

async fn wal_loop(runtime: Runtime, shard: u32) -> Result<(), ArchiveError> {
    loop {
        let (durable_start, database_uuid, base_key) = {
            let state = runtime.state.lock().await;
            (
                cursor(&state, shard)
                    .ok_or_else(|| format!("archive state has no cursor for shard {shard}"))?,
                state.database_uuid.clone(),
                state.latest_base_manifest.clone(),
            )
        };
        let base = BaseManifest::decode(runtime.store.get_bytes(&base_key).await?)?;
        validate_base_manifest(&base)?;
        if base.database_uuid != database_uuid || base.term != runtime.term {
            return Err("current archive base does not match the active database timeline".into());
        }
        let root = base
            .wal_cursors
            .iter()
            .find(|cursor| cursor.shard == shard)
            .ok_or_else(|| format!("archive base has no cursor for shard {shard}"))?;
        let mut verified_lsn = root.archived_lsn;
        let mut verified_fingerprint = root.history_fingerprint.clone();

        // ACK the durable tip, but deliberately subscribe from the base root.
        // Re-reading the leader's retained bytes and comparing immutable WAL
        // objects proves that a newly contacted same-term endpoint shares the
        // archived prefix before it may append to that prefix.
        ack(&runtime.channel, shard, durable_start).await?;
        let mut last_ack = Instant::now();

        let mut client = ReplicationClient::new(runtime.channel.clone());
        let response = client
            .subscribe(WalSubscribeRequest {
                shard,
                after_lsn: verified_lsn,
                max_batch_bytes: runtime.max_batch_bytes,
            })
            .await;
        let mut stream = match response {
            Ok(response) => response.into_inner(),
            Err(status) if transient(status.code()) => {
                tracing::warn!(shard, error = %status, "WAL subscribe failed; retrying");
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
            Err(status) if status.code() == Code::FailedPrecondition => {
                tracing::warn!(shard, error = %status, "archived WAL fell behind retention; requesting a new base");
                request_base(&runtime, None, true).await;
                return std::future::pending::<Result<(), ArchiveError>>().await;
            }
            Err(status) => return Err(status.into()),
        };

        loop {
            let batch = match stream.message().await {
                Ok(Some(batch)) => batch,
                Ok(None) => break,
                Err(status) if transient(status.code()) => {
                    tracing::warn!(shard, error = %status, "WAL stream interrupted; retrying");
                    break;
                }
                Err(status) if status.code() == Code::FailedPrecondition => {
                    tracing::warn!(shard, error = %status, "archived WAL fell behind retention; requesting a new base");
                    request_base(&runtime, None, true).await;
                    return std::future::pending::<Result<(), ArchiveError>>().await;
                }
                Err(status) => return Err(status.into()),
            };
            if batch.is_heartbeat {
                let state = runtime.state.lock().await;
                let durable = cursor(&state, shard)
                    .ok_or_else(|| format!("archive state has no cursor for shard {shard}"))?;
                let durable_fingerprint = cursor_fingerprint(&state, shard)
                    .ok_or_else(|| format!("archive state has no history for shard {shard}"))?;
                if verified_lsn != durable || verified_fingerprint != durable_fingerprint {
                    return Err(format!(
                        "replication endpoint history for shard {shard} ends at {verified_lsn}, \
                         before or apart from archived cursor {durable}"
                    )
                    .into());
                }
                if last_ack.elapsed() >= Duration::from_secs(1) {
                    ack(&runtime.channel, shard, durable).await?;
                    last_ack = Instant::now();
                }
                continue;
            }
            if batch.shard != shard || batch.first_lsn != verified_lsn {
                return Err(format!(
                    "WAL discontinuity for shard {shard}: expected {verified_lsn}, got shard {} at {}",
                    batch.shard, batch.first_lsn
                )
                .into());
            }
            let expected_end = batch
                .first_lsn
                .checked_add(batch.records.len() as u64)
                .ok_or("WAL batch end overflowed u64")?;
            if batch.last_lsn != expected_end {
                return Err(format!(
                    "WAL batch for shard {shard} reports end {}, byte length implies {expected_end}",
                    batch.last_lsn
                )
                .into());
            }
            let checksum = crc32c::crc32c(&batch.records);
            if checksum != batch.crc32c {
                return Err(format!(
                    "WAL batch checksum mismatch for shard {shard} at {}",
                    batch.first_lsn
                )
                .into());
            }

            let upload = runtime
                .store
                .put_wal(
                    &database_uuid,
                    runtime.term,
                    shard,
                    batch.first_lsn..batch.last_lsn,
                    &verified_fingerprint,
                    batch.records,
                )
                .await?;
            let durable_cursor = {
                let mut state = runtime.state.lock().await;
                let latest = cursor(&state, shard).unwrap_or(0);
                let latest_fingerprint = cursor_fingerprint(&state, shard).unwrap_or_default();
                if upload.last_lsn < latest {
                    latest
                } else if upload.last_lsn == latest {
                    if upload.history_fingerprint != latest_fingerprint {
                        return Err(format!(
                            "replication endpoint diverges from archived history for shard {shard} at LSN {latest}"
                        )
                        .into());
                    }
                    latest
                } else {
                    let joins_tip = (upload.first_lsn == latest
                        && upload.previous_fingerprint == latest_fingerprint)
                        || upload.frames.iter().any(|frame| {
                            frame.last_lsn == latest
                                && frame.history_fingerprint == latest_fingerprint
                        });
                    if !joins_tip {
                        return Err(format!(
                            "replication frames diverge from archived history for shard {shard} at LSN {latest}"
                        )
                        .into());
                    }
                    set_cursor_history(
                        &mut state,
                        shard,
                        upload.last_lsn,
                        upload.history_fingerprint.clone(),
                    );
                    runtime
                        .store
                        .publish_state(&runtime.local_state, &state)
                        .await?;
                    upload.last_lsn
                }
            };
            verified_lsn = upload.last_lsn;
            verified_fingerprint = upload.history_fingerprint;
            ack(&runtime.channel, shard, durable_cursor).await?;
            last_ack = Instant::now();
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn term_loop(runtime: Runtime) -> Result<(), ArchiveError> {
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let mut client = ReplicationClient::new(runtime.channel.clone());
        match client.status(StatusRequest {}).await {
            Ok(response) => {
                let status = response.into_inner();
                if status.db_uuid != runtime.state.lock().await.database_uuid {
                    return Err("replication endpoint changed database UUID while archiving".into());
                }
                if status.term != runtime.term {
                    return Err(format!(
                        "leadership term changed from {} to {}; restart to bootstrap the new timeline",
                        runtime.term, status.term
                    )
                    .into());
                }
            }
            Err(status) if transient(status.code()) => {
                tracing::warn!(error = %status, "term check failed; retrying");
            }
            Err(status) => return Err(status.into()),
        }
    }
}

fn transient(code: Code) -> bool {
    matches!(
        code,
        Code::Cancelled | Code::DeadlineExceeded | Code::Unavailable | Code::Unknown
    )
}
async fn request_base(runtime: &Runtime, sequence: Option<u64>, reset_cursors: bool) {
    let sequence = match sequence {
        Some(sequence) => sequence,
        None => runtime.state.lock().await.event_sequence,
    };
    let mut request = runtime.base_request.lock().await;
    match request.as_mut() {
        Some(current) => {
            current.sequence = current.sequence.max(sequence);
            current.reset_cursors |= reset_cursors;
        }
        None => {
            *request = Some(BaseRequest {
                sequence,
                reset_cursors,
            });
        }
    }
    runtime.rebootstrap.notify_one();
}

async fn advance_event(runtime: &Runtime, sequence: u64) -> Result<(), ArchiveError> {
    let mut state = runtime.state.lock().await;
    if sequence > state.event_sequence {
        state.event_sequence = sequence;
        runtime
            .store
            .publish_state(&runtime.local_state, &state)
            .await?;
    }
    Ok(())
}

async fn event_loop(runtime: Runtime) -> Result<(), ArchiveError> {
    loop {
        let after_sequence = runtime.state.lock().await.event_sequence;
        let mut client = ControlPlaneClient::new(runtime.channel.clone());
        let response = client.subscribe(SubscribeRequest { after_sequence }).await;
        let mut stream = match response {
            Ok(response) => response.into_inner(),
            Err(status) if transient(status.code()) => {
                tracing::warn!(error = %status, "control subscription failed; retrying");
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
            Err(status) => return Err(status.into()),
        };

        loop {
            let item = match stream.message().await {
                Ok(Some(item)) => item.item,
                Ok(None) => break,
                Err(status) if transient(status.code()) => {
                    tracing::warn!(error = %status, "control stream interrupted; retrying");
                    break;
                }
                Err(status) => return Err(status.into()),
            };
            match item {
                Some(subscribe_item::Item::Event(event)) => {
                    let sequence = event.sequence;
                    let completed_checkpoint = matches!(
                        event.payload,
                        Some(event_envelope::Payload::Checkpoint(checkpoint))
                            if checkpoint.phase == EventPhase::Completed as i32
                    );
                    let needs_initial_base =
                        runtime.state.lock().await.latest_base_manifest.is_empty();
                    if completed_checkpoint && needs_initial_base {
                        // Do not durably advance past the trigger before the
                        // base it requested is published. A crash in between
                        // must replay this event and try the capture again.
                        request_base(&runtime, Some(sequence), false).await;
                        return std::future::pending::<Result<(), ArchiveError>>().await;
                    }
                    advance_event(&runtime, sequence).await?;
                }
                Some(subscribe_item::Item::ResyncRequired(resync)) => {
                    let snapshot = resync
                        .snapshot
                        .ok_or("control resync response contains no state snapshot")?;
                    if runtime.state.lock().await.latest_base_manifest.is_empty() {
                        // A fresh archive cannot know whether the compacted
                        // range contained its activation checkpoint. A current
                        // base is the conservative resynchronization.
                        request_base(&runtime, Some(snapshot.through_sequence), false).await;
                        return std::future::pending::<Result<(), ArchiveError>>().await;
                    }
                    // Existing recovery coverage is already rooted in a
                    // durable base. A projection resync advances observation
                    // state without silently replacing that recovery root.
                    advance_event(&runtime, snapshot.through_sequence).await?;
                }
                Some(subscribe_item::Item::Snapshot(snapshot)) => {
                    advance_event(&runtime, snapshot.through_sequence).await?;
                }
                Some(subscribe_item::Item::Heartbeat(_))
                | Some(subscribe_item::Item::Started(_))
                | None => {}
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn writer_lease_loop(lease: Arc<WriterLease>, ttl_secs: u64) -> Result<(), ArchiveError> {
    let interval = Duration::from_millis(ttl_secs.saturating_mul(1000).saturating_div(3));
    loop {
        tokio::time::sleep(interval).await;
        lease.renew(ttl_secs).await?;
    }
}

async fn refresh_base(
    runtime: &Runtime,
    request: BaseRequest,
    shard_count: u32,
) -> Result<(), ArchiveError> {
    if request.reset_cursors {
        {
            let mut state = runtime.state.lock().await;
            state.latest_base_manifest.clear();
            for cursor in &mut state.wal_cursors {
                cursor.archived_lsn = 0;
                cursor.history_fingerprint.clear();
            }
            runtime
                .store
                .publish_state(&runtime.local_state, &state)
                .await?;
        }
        for shard in 0..shard_count {
            ack(&runtime.channel, shard, 0).await?;
        }
    }
    capture_base(runtime, request.sequence, true).await
}

/// Run until `shutdown` resolves or an archive task fails.
/// Reclaim unreachable archive objects on an interval.
///
/// Renews the writer lease immediately before each pass rather than relying
/// on `writer_lease_loop`'s schedule. A pass that deletes under an expired lease
/// is deleting from an archive another writer may already own.
///
/// A failed pass is logged and retried on the next tick rather than failing the
/// sidecar: reclamation is capacity management, and taking archiving down
/// because a delete failed would trade a disk-space problem for a durability
/// one.
///
/// Because a failure is only logged, every outcome is *also* recorded in
/// [`crate::metrics::ArchiveMetrics`]. A pass that keeps failing is otherwise
/// indistinguishable, from outside, from one that keeps finding nothing to do.
///
/// `pub(crate)` only so the metrics tests can drive the real loop rather than a
/// re-implementation of it: the one-line call that reports a pass is exactly
/// the wiring worth a test.
pub(crate) async fn retention_loop(
    store: ArchiveStore,
    lease: Arc<WriterLease>,
    lease_ttl: u64,
    policy: crate::gc::RetentionPolicy,
    interval_secs: u64,
    metrics: Arc<crate::metrics::ArchiveMetrics>,
) -> Result<(), ArchiveError> {
    let interval = Duration::from_secs(interval_secs.max(1));
    loop {
        tokio::time::sleep(interval).await;
        if let Err(error) = lease.renew(lease_ttl).await {
            tracing::warn!(%error, "archive reclamation skipped: writer lease not renewed");
            metrics.pass_skipped();
            continue;
        }
        match crate::gc::collect(&store, policy).await {
            Ok(plan) => {
                tracing::info!("{}", plan.summary_line());
                metrics.pass_completed(&plan);
            }
            Err(error) => {
                tracing::warn!(%error, "archive reclamation pass failed");
                metrics.pass_failed();
            }
        }
    }
}

pub async fn run_with_shutdown<F>(cli: ArchiveOptions, shutdown: F) -> Result<(), ArchiveError>
where
    F: Future<Output = ()> + Send,
{
    cli.validate()?;
    let snapshot_mode = cli.snapshot_mode;
    let direct_snapshot_path = cli.direct_snapshot_path;
    let deferred_materializer = cli.deferred_config()?;
    let lease_ttl = cli.writer_lease_ttl_secs;
    let retention = cli.retention()?;
    let retention_interval = cli.retention_interval_secs;
    tokio::fs::create_dir_all(&cli.work_dir).await?;
    let channel = connect(&cli).await?;
    let store = ArchiveStore::connect(&cli.store)?;
    let writer_lease = Arc::new(store.acquire_writer(new_writer_id()?, lease_ttl).await?);
    let mut replication = ReplicationClient::new(channel.clone());
    let status = replication.status(StatusRequest {}).await?.into_inner();
    if status.db_uuid.len() != 16 || status.shard_count == 0 {
        return Err("replication status has no valid database identity/topology".into());
    }

    let state = match store.load_state().await? {
        Some(state) => {
            if state.database_uuid != status.db_uuid {
                return Err("object archive belongs to a different database UUID".into());
            }
            if state.wal_cursors.len() != status.shard_count as usize
                || (0..status.shard_count).any(|shard| cursor(&state, shard).is_none())
            {
                return Err("archive-state shard cursors do not match leader topology".into());
            }
            state
        }
        None => new_state(
            status.db_uuid.clone(),
            status.term,
            (0..status.shard_count)
                .map(|shard| WalCursor {
                    shard,
                    archived_lsn: 0,
                    history_fingerprint: Vec::new(),
                })
                .collect(),
        ),
    };

    let mut state = state;
    let term_advanced = status.term > state.term;
    if status.term < state.term {
        return Err(format!(
            "server leadership term {} is below archived term {}; refusing an obsolete timeline",
            status.term, state.term
        )
        .into());
    }
    if status.term > state.term {
        state.term = status.term;
        state.event_sequence = 0;
        state.latest_base_manifest.clear();
        for cursor in &mut state.wal_cursors {
            cursor.archived_lsn = 0;
            cursor.history_fingerprint.clear();
        }
    }
    let resume_rebase = rebase_required_on_start(&state, term_advanced);

    let local_state = cli.work_dir.join("state.pb");
    store.claim_state(&local_state, &mut state).await?;
    let runtime = Runtime {
        channel: channel.clone(),
        store,
        state: Arc::new(Mutex::new(state)),
        local_state,
        work_dir: cli.work_dir,
        snapshot_mode,
        direct_snapshot_path,
        deferred_materializer,
        max_base_attempts: cli.max_base_attempts.max(1),
        max_batch_bytes: cli.max_batch_bytes,
        term: status.term,
        rebootstrap: Arc::new(Notify::new()),
        base_request: Arc::new(Mutex::new(None)),
    };
    if resume_rebase {
        // A new leadership term is a distinct WAL history. Re-root it
        // immediately instead of waiting for an unrelated maintenance
        // checkpoint. A published generation without a current manifest is
        // the durable marker of a rebase interrupted before capture, so a
        // second crash cannot turn this into fresh-archive activation.
        request_base(&runtime, None, true).await;
    }

    // Establish the retention floor before a first base copy. This cannot
    // recover generations already reclaimed, but it prevents the checkpoint
    // racing the snapshot from reclaiming anything the snapshot may need.
    let initial_cursors = runtime.state.lock().await.wal_cursors.clone();
    for wal_cursor in initial_cursors {
        ack(&channel, wal_cursor.shard, wal_cursor.archived_lsn).await?;
    }
    // Outside the restart loop below: a rebootstrap respawns every task, and a
    // counter that reset with them would understate what this process has done.
    let metrics = Arc::new(crate::metrics::ArchiveMetrics::new());
    let serving = match cli.metrics_addr {
        Some(addr) => Some(crate::metrics::serve(addr, metrics.clone()).await?),
        None => None,
    };
    tokio::pin!(shutdown);
    let outcome = 'run: loop {
        let mut tasks = tokio::task::JoinSet::new();
        if !runtime.state.lock().await.latest_base_manifest.is_empty() {
            for shard in 0..status.shard_count {
                tasks.spawn(wal_loop(runtime.clone(), shard));
            }
        }
        tasks.spawn(event_loop(runtime.clone()));
        tasks.spawn(term_loop(runtime.clone()));
        tasks.spawn(writer_lease_loop(writer_lease.clone(), lease_ttl));
        if let Some(policy) = retention {
            tasks.spawn(retention_loop(
                runtime.store.clone(),
                writer_lease.clone(),
                lease_ttl,
                policy,
                retention_interval,
                metrics.clone(),
            ));
        }

        tokio::select! {
            _ = &mut shutdown => {
                tasks.abort_all();
                while tasks.join_next().await.is_some() {}
                break 'run Ok(());
            },
            _ = runtime.rebootstrap.notified() => {
                tasks.abort_all();
                while tasks.join_next().await.is_some() {}
                // **A wakeup is a hint, not a fact.** `Notify` stores a
                // permit when nothing is awaiting, and `request_base`
                // coalesces the *request* while notifying unconditionally --
                // so two requests arriving before this arm runs leave one
                // merged request and one spare permit. The permit then fires
                // on the next iteration with `base_request` already taken.
                //
                // That was treated as fatal, and it aborted the sidecar with
                // "base refresh was notified without a request" -- observed
                // intermittently from `archive.py` at `srv_archive_stop`. The
                // condition must be re-checked after every wakeup, which is the
                // ordinary discipline for a condvar-style primitive.
                //
                // Do not instead notify conditionally in `request_base`:
                // the request lock is released before the consumer takes it, so
                // any "is a wakeup already pending" test there is itself racy.
                let request = runtime.base_request.lock().await.take();
                if let Some(request) = request {
                    if let Err(error) = refresh_base(&runtime, request, status.shard_count).await {
                        break 'run Err(error);
                    }
                }
            },
            result = tasks.join_next() => {
                tasks.abort_all();
                while tasks.join_next().await.is_some() {}
                break 'run match result {
                    Some(Ok(result)) => result,
                    Some(Err(error)) => Err(format!("archive task failed: {error}").into()),
                    None => Err("all archive tasks exited unexpectedly".into()),
                };
            },
        }
    };

    if let Some(serving) = serving {
        serving.stop().await;
    }
    let lease = Arc::try_unwrap(writer_lease)
        .map_err(|_| "archive writer lease remained referenced after task shutdown")?;
    let release = lease.release().await;
    outcome.and(release)
}

fn rebase_required_on_start(state: &archive_pb::ArchiveState, term_advanced: bool) -> bool {
    term_advanced || (state.latest_base_manifest.is_empty() && state.base_generation > 0)
}

#[cfg(test)]
mod tests {
    /// The premise the rebootstrap arm's spurious-wakeup tolerance rests on.
    ///
    /// `Notify::notify_one` stores a **permit** when nothing is awaiting, so a
    /// later `notified()` returns immediately. `request_base` coalesces two
    /// requests into one while notifying twice, which leaves exactly this: one
    /// request, two wakeups. The second found `base_request` empty and was
    /// treated as fatal — "base refresh was notified without a request",
    /// observed intermittently from `archive.py`.
    ///
    /// If this test ever fails, tokio changed a documented guarantee and the
    /// tolerance in the run loop should be re-derived rather than trusted.
    #[tokio::test]
    async fn a_notify_permit_outlives_the_absence_of_a_waiter() {
        let n = std::sync::Arc::new(tokio::sync::Notify::new());

        // No waiter: the permit is stored rather than dropped.
        n.notify_one();
        tokio::time::timeout(std::time::Duration::from_millis(50), n.notified())
            .await
            .expect("a stored permit must satisfy the next notified()");

        // And it is not replayed a second time.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), n.notified())
                .await
                .is_err(),
            "one notify_one must satisfy exactly one notified()"
        );
    }

    use super::*;

    #[test]
    fn only_an_established_or_new_term_archive_rebases_without_a_checkpoint() {
        let mut state = archive_pb::ArchiveState::default();
        assert!(!rebase_required_on_start(&state, false));
        assert!(rebase_required_on_start(&state, true));

        state.base_generation = 1;
        assert!(rebase_required_on_start(&state, false));

        state.latest_base_manifest = "base/manifest.pb".to_owned();
        assert!(!rebase_required_on_start(&state, false));
    }
}
