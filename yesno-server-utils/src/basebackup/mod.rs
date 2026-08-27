//! Build a restorable database directory through the shared control plane.
//!
//! `yesnoctl basebackup` acquires one immutable server-owned lease, streams its
//! named files as Protobuf chunks, renews and releases the lease, then opens the
//! staged copy through ordinary replica recovery before publishing it by rename.
//! The client never discovers the live database path and never uses replication
//! RPCs.
//!
//! This module also retains the replication assembler used only by the archive
//! sidecar's explicit `network` fallback. That path must retry unless every
//! independently fetched shard image names the same checkpoint watermark; a
//! mixed watermark would let global recovery skip WAL still needed by an older
//! image.
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::io::AsyncWriteExt;
use tokio::time::Duration;
use tonic::transport::Channel;
use yesno_core::{Db, DbOptions};
use yesno_server::control::pb::control_plane_client::ControlPlaneClient;
use yesno_server::control::pb::{
    snapshot_file_chunk, BaseSnapshotLease, BeginBaseSnapshotRequest, FetchSnapshotFileRequest,
    KeepBaseSnapshotAliveRequest, ReleaseBaseSnapshotRequest,
};

use yesno_server::replication::follower::FollowerError;
use yesno_server::replication::pb;
use yesno_server::replication::pb::replication_client::ReplicationClient;
use yesno_server::replication::FollowerClient;

mod cli;
pub use cli::{completion_line, run, BasebackupError, BasebackupOptions};

static STAGING_ID: AtomicU64 = AtomicU64::new(0);

/// Controls one complete backup operation.
#[derive(Debug, Clone, Copy)]
pub struct BaseBackupOptions {
    /// Whole attempts before giving up. Zero is treated as one.
    pub max_attempts: u32,
    /// Maximum bytes in a shipped WAL batch. Zero asks the leader for its default.
    pub max_batch_bytes: u32,
}

impl Default for BaseBackupOptions {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            max_batch_bytes: 0,
        }
    }
}

/// Facts about the directory that was durably published.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseBackupReport {
    /// Final published directory.
    pub target: PathBuf,
    /// Database identity copied from the leader's manifest.
    pub db_uuid: [u8; 16],
    /// Highest leadership term observed during transfer.
    pub term: u32,
    /// Number of physical shards.
    pub shards: u32,
    /// Common checkpoint watermark carried by every shard image.
    pub checkpoint_version: u64,
    /// Highest complete commit retained by staged recovery.
    pub recovered_version: u64,
    /// Total size of regular files in the completed directory.
    pub bytes: u64,
    /// Attempt on which the backup completed.
    pub attempts: u32,
}

/// A failure inside one disposable staging directory.
#[derive(Debug)]
pub enum BaseBackupAttemptError {
    Follower(FollowerError),
    Core(yesno_core::CodecError),
    Io(std::io::Error),
    Topology(String),
    InconsistentCheckpoint { versions: Vec<u64> },
}

impl std::fmt::Display for BaseBackupAttemptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Follower(error) => write!(f, "{error}"),
            Self::Core(error) => write!(f, "staged database did not recover: {error}"),
            Self::Io(error) => write!(f, "staging I/O failed: {error}"),
            Self::Topology(message) => write!(f, "leader topology is unusable: {message}"),
            Self::InconsistentCheckpoint { versions } => write!(
                f,
                "shard images came from different checkpoint watermarks {versions:?}"
            ),
        }
    }
}

impl std::error::Error for BaseBackupAttemptError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Follower(error) => Some(error),
            Self::Core(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::Topology(_) | Self::InconsistentCheckpoint { .. } => None,
        }
    }
}

impl From<FollowerError> for BaseBackupAttemptError {
    fn from(error: FollowerError) -> Self {
        Self::Follower(error)
    }
}

impl From<yesno_core::CodecError> for BaseBackupAttemptError {
    fn from(error: yesno_core::CodecError) -> Self {
        Self::Core(error)
    }
}

impl From<std::io::Error> for BaseBackupAttemptError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// Why no backup directory could be published.
#[derive(Debug)]
pub enum BaseBackupError {
    InvalidTarget(PathBuf),
    TargetExists(PathBuf),
    Io(std::io::Error),
    AttemptsExhausted {
        attempts: u32,
        last: BaseBackupAttemptError,
    },
}

impl std::fmt::Display for BaseBackupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidTarget(path) => write!(
                f,
                "backup target '{}' has no directory name",
                path.display()
            ),
            Self::TargetExists(path) => write!(
                f,
                "backup target '{}' already exists; choose a new path",
                path.display()
            ),
            Self::Io(error) => write!(f, "cannot prepare backup target: {error}"),
            Self::AttemptsExhausted { attempts, last } => {
                write!(f, "base backup failed after {attempts} attempt(s): {last}")
            }
        }
    }
}

impl std::error::Error for BaseBackupError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::AttemptsExhausted { last, .. } => Some(last),
            Self::InvalidTarget(_) | Self::TargetExists(_) => None,
        }
    }
}

impl From<std::io::Error> for BaseBackupError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

struct StagingDir {
    path: PathBuf,
    published: bool,
}

impl Drop for StagingDir {
    fn drop(&mut self) {
        if !self.published {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

fn target_parts(target: &Path) -> Result<(&Path, &std::ffi::OsStr), BaseBackupError> {
    let Some(name) = target.file_name() else {
        return Err(BaseBackupError::InvalidTarget(target.to_path_buf()));
    };
    let parent = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    Ok((parent, name))
}

fn create_staging(target: &Path) -> Result<StagingDir, BaseBackupError> {
    let (parent, name) = target_parts(target)?;
    std::fs::create_dir_all(parent)?;
    for _ in 0..100 {
        let id = STAGING_ID.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(
            ".{}.yesno-basebackup.{}.{}.partial",
            name.to_string_lossy(),
            std::process::id(),
            id
        ));
        match std::fs::create_dir(&path) {
            Ok(()) => {
                return Ok(StagingDir {
                    path,
                    published: false,
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not allocate a unique base-backup staging directory",
    )
    .into())
}

fn image_state(path: &Path) -> Result<(u64, u64), BaseBackupAttemptError> {
    use std::io::Read as _;

    let mut file = std::fs::File::open(path)?;
    let mut head = vec![0u8; 2 * yesno_core::store::PAGE];
    file.read_exact(&mut head)?;
    let page = yesno_core::store::PAGE;
    let superblock = yesno_core::store::superblock::pick(&head[..page], &head[page..])?
        .ok_or_else(|| {
            BaseBackupAttemptError::Topology(format!(
                "'{}' has no readable superblock",
                path.display()
            ))
        })?;
    Ok((superblock.checkpoint_cv, superblock.wal_replay_lsn))
}

fn sync_tree(dir: &Path) -> Result<u64, std::io::Error> {
    let mut bytes = 0u64;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            let file = std::fs::File::open(entry.path())?;
            bytes = bytes.saturating_add(file.metadata()?.len());
            file.sync_all()?;
        }
    }
    std::fs::File::open(dir)?.sync_all()?;
    Ok(bytes)
}

async fn one_attempt(
    client: &mut ReplicationClient<Channel>,
    dir: &Path,
    max_batch_bytes: u32,
) -> Result<BaseBackupReport, BaseBackupAttemptError> {
    let mut follower = FollowerClient::new(dir, 0, []);
    let db_uuid = follower.fetch_manifest(client).await?;
    let status = client
        .status(pb::StatusRequest {})
        .await
        .map_err(FollowerError::from)?
        .into_inner();
    if status.shard_count == 0 {
        return Err(BaseBackupAttemptError::Topology(
            "the leader reported zero shards".to_owned(),
        ));
    }
    if status.end_lsn.len() != status.shard_count as usize {
        return Err(BaseBackupAttemptError::Topology(format!(
            "the leader reported {} shards but {} WAL ends",
            status.shard_count,
            status.end_lsn.len()
        )));
    }

    let mut checkpoint_versions = Vec::with_capacity(status.shard_count as usize);
    let mut replay_offsets = Vec::with_capacity(status.shard_count as usize);
    for shard in 0..status.shard_count {
        let offered = follower.bootstrap_shard(client, shard).await?;
        let image = dir.join(format!("shard-{shard:04}.yno"));
        let (checkpoint, recorded) = image_state(&image)?;
        if offered != recorded {
            return Err(BaseBackupAttemptError::Topology(format!(
                "shard {shard} snapshot reported replay LSN {offered}, but its image records {recorded}"
            )));
        }
        checkpoint_versions.push(checkpoint);
        replay_offsets.push(offered);
    }

    let checkpoint_version = checkpoint_versions[0];
    if checkpoint_versions
        .iter()
        .any(|version| *version != checkpoint_version)
    {
        return Err(BaseBackupAttemptError::InconsistentCheckpoint {
            versions: checkpoint_versions,
        });
    }

    for (shard, replay_lsn) in replay_offsets.into_iter().enumerate() {
        follower
            .catch_up_shard(client, shard as u32, replay_lsn, max_batch_bytes)
            .await?;
    }

    // This also trims an incomplete multi-shard transaction at the shipped tail.
    let db = Db::open_replica(dir, DbOptions::default())?;
    let recovered_version = db.visible();
    let shards = db.shard_count() as u32;
    if shards != status.shard_count {
        return Err(BaseBackupAttemptError::Topology(format!(
            "the MANIFEST names {shards} shards but Status reported {}",
            status.shard_count
        )));
    }
    if recovered_version < checkpoint_version {
        return Err(BaseBackupAttemptError::Topology(format!(
            "recovery moved backward from checkpoint {checkpoint_version} to {recovered_version}"
        )));
    }
    drop(db);

    let bytes = sync_tree(dir)?;
    Ok(BaseBackupReport {
        target: PathBuf::new(),
        db_uuid,
        term: follower.term(),
        shards,
        checkpoint_version,
        recovered_version,
        bytes,
        attempts: 0,
    })
}

/// Fetch a hot base backup and publish it at `target` only after recovery.
///
/// `target` must not exist. All work happens in a sibling staging directory;
/// failed attempts are removed, and a successful directory becomes visible in
/// one rename.
pub(crate) async fn replication_base_backup(
    client: &mut ReplicationClient<Channel>,
    target: impl AsRef<Path>,
    options: BaseBackupOptions,
) -> Result<BaseBackupReport, BaseBackupError> {
    let target = target.as_ref();
    if target.exists() {
        return Err(BaseBackupError::TargetExists(target.to_path_buf()));
    }
    let (parent, _) = target_parts(target)?;
    std::fs::create_dir_all(parent)?;

    let max_attempts = options.max_attempts.max(1);
    let mut last = None;
    for attempt in 1..=max_attempts {
        let mut staging = create_staging(target)?;
        match one_attempt(client, &staging.path, options.max_batch_bytes).await {
            Ok(mut report) => {
                if target.exists() {
                    return Err(BaseBackupError::TargetExists(target.to_path_buf()));
                }
                std::fs::rename(&staging.path, target)?;
                std::fs::File::open(parent)?.sync_all()?;
                staging.published = true;
                report.target = target.to_path_buf();
                report.attempts = attempt;
                return Ok(report);
            }
            Err(error) => last = Some(error),
        }
    }

    Err(BaseBackupError::AttemptsExhausted {
        attempts: max_attempts,
        last: last.expect("at least one base-backup attempt ran"),
    })
}

/// One control-plane base-backup failure.
pub type ControlBaseBackupError = Box<dyn std::error::Error + Send + Sync>;

fn validate_snapshot_lease(lease: &BaseSnapshotLease) -> Result<(), ControlBaseBackupError> {
    if lease.lease_id.is_empty() || lease.files.is_empty() || lease.lease_ttl_secs == 0 {
        return Err("server returned an incomplete snapshot lease".into());
    }
    for file in &lease.files {
        let name = Path::new(&file.name);
        if name.components().count() != 1
            || name
                .file_name()
                .is_none_or(|value| value != file.name.as_str())
        {
            return Err(format!("snapshot file name '{}' is not a flat name", file.name).into());
        }
    }
    Ok(())
}

async fn stream_snapshot_file(
    channel: Channel,
    lease: &BaseSnapshotLease,
    file: &yesno_server::control::pb::SnapshotFile,
    target: &Path,
) -> Result<(), ControlBaseBackupError> {
    let mut stream = ControlPlaneClient::new(channel)
        .fetch_snapshot_file(FetchSnapshotFileRequest {
            lease_id: lease.lease_id.clone(),
            name: file.name.clone(),
            allow_direct_path: false,
        })
        .await?
        .into_inner();
    let path = target.join(&file.name);
    let mut output = tokio::fs::File::create(path).await?;
    let mut offset = 0u64;
    loop {
        let mut chunk = stream
            .message()
            .await?
            .ok_or("snapshot file stream ended before its last marker")?;
        if chunk.offset != offset || chunk.total_size != file.size {
            return Err(format!(
                "snapshot file '{}' has a discontinuous or inconsistent chunk",
                file.name
            )
            .into());
        }
        let data = match chunk.payload.take() {
            Some(snapshot_file_chunk::Payload::Data(data)) => data,
            Some(snapshot_file_chunk::Payload::DirectPath(_)) => {
                return Err("basebackup received an unsolicited direct snapshot path".into());
            }
            None => return Err("snapshot file chunk has no payload".into()),
        };
        output.write_all(&data).await?;
        offset = offset.saturating_add(data.len() as u64);
        if chunk.last {
            if offset != file.size {
                return Err(format!(
                    "snapshot file '{}' ended at {offset}, expected {}",
                    file.name, file.size
                )
                .into());
            }
            if stream.message().await?.is_some() {
                return Err("snapshot file has chunks after its last marker".into());
            }
            break;
        }
    }
    output.sync_all().await?;
    Ok(())
}

async fn stream_snapshot(
    channel: Channel,
    lease: &BaseSnapshotLease,
    target: &Path,
) -> Result<(), ControlBaseBackupError> {
    for file in &lease.files {
        stream_snapshot_file(channel.clone(), lease, file, target).await?;
    }
    tokio::fs::File::open(target).await?.sync_all().await?;
    Ok(())
}

async fn keep_snapshot_alive(
    channel: Channel,
    lease_id: Vec<u8>,
    ttl_secs: u64,
) -> Result<(), ControlBaseBackupError> {
    let interval = Duration::from_millis(ttl_secs.saturating_mul(1000).saturating_div(3).max(100));
    loop {
        tokio::time::sleep(interval).await;
        ControlPlaneClient::new(channel.clone())
            .keep_base_snapshot_alive(KeepBaseSnapshotAliveRequest {
                lease_id: lease_id.clone(),
            })
            .await?;
    }
}

async fn download_snapshot(
    channel: Channel,
    lease: &BaseSnapshotLease,
    target: &Path,
) -> Result<(), ControlBaseBackupError> {
    let transfer = stream_snapshot(channel.clone(), lease, target);
    tokio::pin!(transfer);
    let keepalive = keep_snapshot_alive(channel, lease.lease_id.clone(), lease.lease_ttl_secs);
    tokio::pin!(keepalive);
    tokio::select! {
        result = &mut transfer => result,
        result = &mut keepalive => match result {
            Ok(()) => Err("snapshot keepalive stopped unexpectedly".into()),
            Err(error) => Err(error),
        },
    }
}

/// Fetch one immutable snapshot entirely through the control-plane protocol,
/// recover it in a sibling staging directory, and atomically publish `target`.
pub async fn base_backup(
    channel: Channel,
    target: impl AsRef<Path>,
) -> Result<BaseBackupReport, ControlBaseBackupError> {
    let target = target.as_ref();
    if target.exists() {
        return Err(BaseBackupError::TargetExists(target.to_path_buf()).into());
    }
    let (parent, _) = target_parts(target)?;
    std::fs::create_dir_all(parent)?;
    let mut staging = create_staging(target)?;

    let lease = ControlPlaneClient::new(channel.clone())
        .begin_base_snapshot(BeginBaseSnapshotRequest {})
        .await?
        .into_inner();
    let transfer = async {
        validate_snapshot_lease(&lease)?;
        download_snapshot(channel.clone(), &lease, &staging.path).await
    }
    .await;
    let release = ControlPlaneClient::new(channel)
        .release_base_snapshot(ReleaseBaseSnapshotRequest {
            lease_id: lease.lease_id,
        })
        .await;
    if let Err(error) = transfer {
        let _ = release;
        return Err(error);
    }
    release?;

    let inspection = crate::archive::inspect_base(&staging.path)?;
    let db = Db::open_replica(&staging.path, DbOptions::default())?;
    let recovered_version = db.visible();
    let shards = db.shard_count() as u32;
    drop(db);
    if shards != inspection.shards {
        return Err(format!(
            "recovered MANIFEST names {shards} shards, snapshot contains {}",
            inspection.shards
        )
        .into());
    }
    if recovered_version < inspection.checkpoint_version {
        return Err(format!(
            "recovery moved backward from checkpoint {} to {recovered_version}",
            inspection.checkpoint_version
        )
        .into());
    }
    let bytes = sync_tree(&staging.path)?;
    if target.exists() {
        return Err(BaseBackupError::TargetExists(target.to_path_buf()).into());
    }
    std::fs::rename(&staging.path, target)?;
    std::fs::File::open(parent)?.sync_all()?;
    staging.published = true;
    Ok(BaseBackupReport {
        target: target.to_path_buf(),
        db_uuid: inspection.database_uuid,
        term: inspection.term,
        shards,
        checkpoint_version: inspection.checkpoint_version,
        recovered_version,
        bytes,
        attempts: 1,
    })
}
