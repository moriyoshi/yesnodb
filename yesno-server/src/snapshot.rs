//! Server-owned immutable base-snapshot leases.
//!
//! Snapshot creation is part of the database lifecycle, not a backup client or
//! object-store concern. The default portable provider copies a bounded set of
//! database-file prefixes while the core backup barrier excludes checkpoints;
//! ZFS, Btrfs, LVM, and Amazon EBS are providers behind the same lease protocol.
//! LVM and EBS split their short verification, flush, and point-in-time capture
//! from mount materialization. The latter runs after the core backup barrier
//! is released. With deferred EBS materialization, `yesnod` returns a
//! provisional snapshot descriptor and the archiver launches and supervises
//! the ECS task or EKS Job that stages its contents. `yesnod` owns the live
//! data path, barrier timing, lease expiry, and cleanup decisions; a separately
//! authenticated local agent owns LVM privilege and executes those decisions. A
//! server-local path is exposed only by filesystem providers when both operators
//! opt into the co-located archive-sidecar escape hatch.
//!
//! ZFS and Btrfs need no agent because the filesystem delegates the privilege
//! itself: `zfs allow` grants snapshot verbs on one dataset, and Btrfs grants
//! them through subvolume ownership plus `user_subvol_rm_allowed`. LVM has no
//! such mechanism — `lvcreate -s` and the mount are unconditionally
//! privileged — which is why it, alone among the filesystem providers, needs a
//! second process. Running `yesnod` as root to make ZFS or Btrfs work is
//! not the intended deployment; it hands the network-facing daemon exactly the
//! privilege the LVM split exists to deny it.
//!
//! Local EBS materialization mounts too, so it uses the same agent. The
//! **attachment** moves with the mount rather than staying beside the other
//! cloud calls, and that is the load-bearing part of the split: `AttachVolume`
//! decides which block devices exist on the instance, so a daemon that could
//! call it would decide what a later mount exposes and constraining the agent
//! to a device name would protect nothing. The agent therefore finds the volume
//! by its ownership tags and picks the attachment name from its own configured
//! pool. The daemon keeps `CreateSnapshot`, `CreateVolume` and the deletes,
//! none of which touch this host. Deferred materialization needs no agent
//! because the archiver's worker mounts instead. Provider objects live
//! in a database-UUID namespace: startup reconciliation removes objects left by
//! a crashed server, and live cleanup failures remain retryable lease state.
pub(crate) mod agent;
mod ebs;
mod lvm;
mod stage;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use tokio::io::AsyncReadExt;
use tokio::sync::OnceCell;

use crate::config::{SnapshotBackend, SnapshotConfig};

#[cfg(test)]
use std::sync::atomic::AtomicUsize;

pub(crate) type SnapshotError = Box<dyn std::error::Error + Send + Sync>;

static LEASE_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SnapshotSource {
    Zfs,
    Btrfs,
    Lvm,
    Ebs,
    Portable,
}

use stage::database_files;
pub(crate) use stage::SnapshotFile;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SnapshotLeaseInfo {
    pub id: Vec<u8>,
    pub source: SnapshotSource,
    pub files: Vec<SnapshotFile>,
    pub ttl_secs: u64,
    pub direct_path_available: bool,
    pub deferred_ebs: Option<DeferredEbsSnapshot>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DeferredEbsSnapshot {
    pub snapshot_id: String,
    pub region: String,
    pub filesystem: String,
    pub source_subpath: String,
    pub volume_size_gib: u64,
}

pub(crate) enum SnapshotFileAccess {
    Stream { file: tokio::fs::File, size: u64 },
    Direct { path: PathBuf, size: u64 },
}

#[derive(Debug)]
enum Cleanup {
    Zfs(String),
    Btrfs(PathBuf),
    Lvm(lvm::LvmCleanup),
    Ebs(ebs::EbsCleanup),
    Portable(PathBuf),
    #[cfg(test)]
    Test,
}

struct CreatedSnapshot {
    root: PathBuf,
    source: SnapshotSource,
    cleanup: Cleanup,
    deferred_ebs: Option<DeferredEbsSnapshot>,
}

enum CapturedSnapshot {
    Ready(CreatedSnapshot),
    Lvm(lvm::LvmCapture),
    Ebs(ebs::EbsCapture),
}

#[async_trait]
trait SnapshotProvider: Send + Sync {
    async fn capture(&self, namespace: &str, name: &str)
        -> Result<CapturedSnapshot, SnapshotError>;
    async fn materialize(
        &self,
        captured: CapturedSnapshot,
    ) -> Result<CreatedSnapshot, SnapshotError> {
        match captured {
            CapturedSnapshot::Ready(created) => Ok(created),
            CapturedSnapshot::Lvm(_) | CapturedSnapshot::Ebs(_) => {
                Err("snapshot provider received foreign capture state".into())
            }
        }
    }
    async fn cleanup(&self, cleanup: &Cleanup) -> Result<(), SnapshotError>;
    async fn reconcile(&self, namespace: &str) -> Result<(), SnapshotError>;
}

struct PortableProvider {
    data_dir: PathBuf,
}

struct FilesystemProvider {
    data_dir: PathBuf,
    backend: FilesystemBackend,
}

#[cfg(test)]
struct TestProvider {
    root: PathBuf,
    cleanups: Arc<AtomicUsize>,
    failures: Arc<AtomicUsize>,
    materialize_started: Option<Arc<tokio::sync::Notify>>,
    materialize_release: Option<Arc<tokio::sync::Notify>>,
}

#[cfg(test)]
#[async_trait]
impl SnapshotProvider for TestProvider {
    async fn capture(
        &self,
        _namespace: &str,
        _name: &str,
    ) -> Result<CapturedSnapshot, SnapshotError> {
        Ok(CapturedSnapshot::Ready(CreatedSnapshot {
            root: self.root.clone(),
            source: SnapshotSource::Btrfs,
            cleanup: Cleanup::Test,
            deferred_ebs: None,
        }))
    }

    async fn materialize(
        &self,
        captured: CapturedSnapshot,
    ) -> Result<CreatedSnapshot, SnapshotError> {
        if let Some(started) = &self.materialize_started {
            started.notify_one();
        }
        if let Some(release) = &self.materialize_release {
            release.notified().await;
        }
        match captured {
            CapturedSnapshot::Ready(created) => Ok(created),
            CapturedSnapshot::Lvm(_) | CapturedSnapshot::Ebs(_) => {
                Err("test provider received foreign capture state".into())
            }
        }
    }

    async fn cleanup(&self, _cleanup: &Cleanup) -> Result<(), SnapshotError> {
        self.cleanups.fetch_add(1, Ordering::Relaxed);
        if self
            .failures
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
        {
            return Err("injected snapshot cleanup failure".into());
        }
        Ok(())
    }

    async fn reconcile(&self, _namespace: &str) -> Result<(), SnapshotError> {
        Ok(())
    }
}

enum FilesystemBackend {
    Zfs { dataset: String },
    Btrfs { snapshot_dir: PathBuf },
}

#[async_trait]
impl SnapshotProvider for PortableProvider {
    async fn capture(
        &self,
        _namespace: &str,
        name: &str,
    ) -> Result<CapturedSnapshot, SnapshotError> {
        let parent = self.data_dir.parent().unwrap_or_else(|| Path::new("."));
        let data_name = self
            .data_dir
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("yesno"))
            .to_string_lossy();
        let root = parent.join(format!(".{data_name}.{name}.portable"));
        tokio::fs::create_dir(&root).await?;

        let copy = async {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                tokio::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).await?;
            }
            for source in database_files(&self.data_dir)? {
                let destination = root.join(&source.name);
                let input = tokio::fs::File::open(self.data_dir.join(&source.name)).await?;
                let mut output = tokio::fs::File::create(&destination).await?;
                let copied = tokio::io::copy(&mut input.take(source.size), &mut output).await?;
                if copied != source.size {
                    return Err(format!(
                        "database file '{}' ended at {copied} bytes while its snapshot size was {}",
                        source.name, source.size
                    )
                    .into());
                }
                output.sync_all().await?;
            }
            tokio::fs::File::open(&root).await?.sync_all().await?;
            Ok::<(), SnapshotError>(())
        }
        .await;
        if let Err(error) = copy {
            let _ = tokio::fs::remove_dir_all(&root).await;
            return Err(error);
        }
        Ok(CapturedSnapshot::Ready(CreatedSnapshot {
            root: root.clone(),
            source: SnapshotSource::Portable,
            cleanup: Cleanup::Portable(root),
            deferred_ebs: None,
        }))
    }

    async fn cleanup(&self, cleanup: &Cleanup) -> Result<(), SnapshotError> {
        match cleanup {
            Cleanup::Portable(path) => match tokio::fs::remove_dir_all(path).await {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error.into()),
            },
            Cleanup::Zfs(_) | Cleanup::Btrfs(_) | Cleanup::Lvm(_) | Cleanup::Ebs(_) => {
                Err("portable snapshot received foreign cleanup state".into())
            }
            #[cfg(test)]
            Cleanup::Test => Err("portable snapshot received foreign cleanup state".into()),
        }
    }

    async fn reconcile(&self, namespace: &str) -> Result<(), SnapshotError> {
        let parent = self.data_dir.parent().unwrap_or_else(|| Path::new("."));
        let data_name = self
            .data_dir
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("yesno"))
            .to_string_lossy();
        let prefix = format!(".{data_name}.{namespace}-");
        let mut entries = tokio::fs::read_dir(parent).await?;
        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(&prefix)
                && name.ends_with(".portable")
                && entry.file_type().await?.is_dir()
            {
                match tokio::fs::remove_dir_all(entry.path()).await {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
        }
        Ok(())
    }
}
#[async_trait]
impl SnapshotProvider for FilesystemProvider {
    async fn capture(
        &self,
        _namespace: &str,
        name: &str,
    ) -> Result<CapturedSnapshot, SnapshotError> {
        match &self.backend {
            FilesystemBackend::Zfs { dataset } => {
                let snapshot = format!("{dataset}@{name}");
                run_command("zfs", &["snapshot", &snapshot]).await?;
                Ok(CapturedSnapshot::Ready(CreatedSnapshot {
                    root: self.data_dir.join(".zfs").join("snapshot").join(name),
                    source: SnapshotSource::Zfs,
                    cleanup: Cleanup::Zfs(snapshot),
                    deferred_ebs: None,
                }))
            }
            FilesystemBackend::Btrfs { snapshot_dir } => {
                tokio::fs::create_dir_all(snapshot_dir).await?;
                let data_dir = tokio::fs::canonicalize(&self.data_dir).await?;
                let snapshot_dir = tokio::fs::canonicalize(snapshot_dir).await?;
                if snapshot_dir.starts_with(&data_dir) {
                    return Err(format!(
                        "Btrfs snapshot directory '{}' must be outside data subvolume '{}'",
                        snapshot_dir.display(),
                        data_dir.display()
                    )
                    .into());
                }
                let path = snapshot_dir.join(name);
                run_command(
                    "btrfs",
                    &[
                        "subvolume",
                        "snapshot",
                        "-r",
                        &path_arg(&data_dir)?,
                        &path_arg(&path)?,
                    ],
                )
                .await?;
                Ok(CapturedSnapshot::Ready(CreatedSnapshot {
                    root: path.clone(),
                    source: SnapshotSource::Btrfs,
                    cleanup: Cleanup::Btrfs(path),
                    deferred_ebs: None,
                }))
            }
        }
    }

    async fn cleanup(&self, cleanup: &Cleanup) -> Result<(), SnapshotError> {
        match cleanup {
            Cleanup::Zfs(snapshot) => run_command("zfs", &["destroy", snapshot]).await,
            Cleanup::Portable(_) => {
                Err("filesystem snapshot received portable cleanup state".into())
            }
            Cleanup::Btrfs(path) => {
                if !tokio::fs::try_exists(path).await? {
                    return Ok(());
                }
                delete_btrfs_subvolume(path).await
            }
            Cleanup::Lvm(_) => Err("filesystem snapshot received LVM cleanup state".into()),
            Cleanup::Ebs(_) => Err("filesystem snapshot received EBS cleanup state".into()),
            #[cfg(test)]
            Cleanup::Test => Ok(()),
        }
    }

    async fn reconcile(&self, namespace: &str) -> Result<(), SnapshotError> {
        match &self.backend {
            FilesystemBackend::Zfs { dataset } => {
                let output = checked_command(
                    "zfs",
                    &["list", "-H", "-t", "snapshot", "-o", "name", "-r", dataset],
                )
                .await?;
                let prefix = format!("{dataset}@{namespace}-");
                for snapshot in String::from_utf8(output.stdout)?.lines() {
                    if snapshot.starts_with(&prefix) {
                        run_command("zfs", &["destroy", snapshot]).await?;
                    }
                }
            }
            FilesystemBackend::Btrfs { snapshot_dir } => {
                if !tokio::fs::try_exists(snapshot_dir).await? {
                    return Ok(());
                }
                let prefix = format!("{namespace}-");
                let mut entries = tokio::fs::read_dir(snapshot_dir).await?;
                while let Some(entry) = entries.next_entry().await? {
                    let name = entry.file_name();
                    if name.to_string_lossy().starts_with(&prefix) {
                        delete_btrfs_subvolume(&entry.path()).await?;
                    }
                }
            }
        }
        Ok(())
    }
}

struct Lease {
    root: PathBuf,
    files: Vec<SnapshotFile>,
    cleanup: Cleanup,
    expires: Instant,
    released: bool,
}

pub(crate) struct SnapshotCapture {
    id: Vec<u8>,
    captured: CapturedSnapshot,
}

/// The lease table is shared by every RPC on one control listener.
pub(crate) struct SnapshotManager {
    provider: Arc<dyn SnapshotProvider>,
    data_dir: PathBuf,
    ttl: Duration,
    cleanup_retry: Duration,
    allow_direct_path: bool,
    namespace: OnceCell<String>,
    leases: Mutex<HashMap<Vec<u8>, Lease>>,
}

impl SnapshotManager {
    #[cfg(test)]
    pub(crate) fn for_test(
        root: &Path,
        ttl: Duration,
        direct: bool,
    ) -> (Arc<Self>, Arc<AtomicUsize>) {
        Self::for_test_with_cleanup_failures(root, ttl, direct, 0)
    }

    #[cfg(test)]
    fn for_test_with_cleanup_failures(
        root: &Path,
        ttl: Duration,
        direct: bool,
        failures: usize,
    ) -> (Arc<Self>, Arc<AtomicUsize>) {
        let cleanups = Arc::new(AtomicUsize::new(0));
        let provider = TestProvider {
            root: root.to_path_buf(),
            cleanups: cleanups.clone(),
            failures: Arc::new(AtomicUsize::new(failures)),
            materialize_started: None,
            materialize_release: None,
        };
        let namespace = OnceCell::new();
        namespace
            .set("yesno-snapshot-test".to_owned())
            .expect("test snapshot namespace is unset");
        (
            Arc::new(Self {
                provider: Arc::new(provider),
                data_dir: root.to_path_buf(),
                ttl,
                cleanup_retry: Duration::from_millis(10),
                allow_direct_path: direct,
                namespace,
                leases: Mutex::new(HashMap::new()),
            }),
            cleanups,
        )
    }

    #[cfg(test)]
    pub(crate) fn for_deferred_test(
        root: &Path,
        materialize_started: Arc<tokio::sync::Notify>,
        materialize_release: Arc<tokio::sync::Notify>,
    ) -> Arc<Self> {
        let namespace = OnceCell::new();
        namespace
            .set("yesno-snapshot-deferred-test".to_owned())
            .expect("test snapshot namespace is unset");
        Arc::new(Self {
            provider: Arc::new(TestProvider {
                root: root.to_path_buf(),
                cleanups: Arc::new(AtomicUsize::new(0)),
                failures: Arc::new(AtomicUsize::new(0)),
                materialize_started: Some(materialize_started),
                materialize_release: Some(materialize_release),
            }),
            data_dir: root.to_path_buf(),
            ttl: Duration::from_secs(5),
            cleanup_retry: Duration::from_millis(10),
            allow_direct_path: false,
            namespace,
            leases: Mutex::new(HashMap::new()),
        })
    }

    pub(crate) fn from_config(
        config: &SnapshotConfig,
        data_dir: &Path,
        agent: agent::SnapshotAgentBroker,
    ) -> Option<Arc<Self>> {
        let provider: Arc<dyn SnapshotProvider> = match config.backend {
            SnapshotBackend::Disabled => Arc::new(PortableProvider {
                data_dir: data_dir.to_path_buf(),
            }),
            SnapshotBackend::Zfs => Arc::new(FilesystemProvider {
                data_dir: data_dir.to_path_buf(),
                backend: FilesystemBackend::Zfs {
                    dataset: config.zfs_dataset.clone().expect("validated ZFS dataset"),
                },
            }),
            SnapshotBackend::Btrfs => Arc::new(FilesystemProvider {
                data_dir: data_dir.to_path_buf(),
                backend: FilesystemBackend::Btrfs {
                    snapshot_dir: config
                        .btrfs_snapshot_dir
                        .clone()
                        .expect("validated Btrfs snapshot directory"),
                },
            }),
            SnapshotBackend::Lvm => Arc::new(lvm::LvmProvider::new(
                agent,
                config.lvm.as_ref().expect("validated LVM configuration"),
            )),
            SnapshotBackend::Ebs => {
                let ebs = config.ebs.as_ref().expect("validated EBS configuration");
                // Only local materialization mounts, so only it needs the agent.
                let broker = match ebs.materialization {
                    crate::config::EbsMaterialization::Local => Some(agent),
                    crate::config::EbsMaterialization::Deferred => None,
                };
                Arc::new(ebs::EbsProvider::new(data_dir, ebs, broker))
            }
        };
        Some(Arc::new(Self {
            provider,
            data_dir: data_dir.to_path_buf(),
            ttl: Duration::from_secs(config.lease_ttl_secs),
            cleanup_retry: Duration::from_secs(5),
            allow_direct_path: config.backend != SnapshotBackend::Disabled
                && config.allow_direct_path,
            namespace: OnceCell::new(),
            leases: Mutex::new(HashMap::new()),
        }))
    }

    #[cfg(test)]
    pub(crate) async fn begin(self: &Arc<Self>) -> Result<SnapshotLeaseInfo, SnapshotError> {
        let captured = self.capture().await?;
        self.materialize(captured).await
    }

    /// Capture the provider's point-in-time image while the caller holds the
    /// core backup barrier. LVM and EBS return after their point-in-time
    /// primitive; their mount work belongs to [`Self::materialize`].
    pub(crate) async fn capture(self: &Arc<Self>) -> Result<SnapshotCapture, SnapshotError> {
        let namespace = self.ensure_namespace().await?;
        let id = new_lease_id(namespace);
        let name = String::from_utf8(id.clone()).expect("lease IDs are ASCII");
        let captured = self.provider.capture(namespace, &name).await?;
        Ok(SnapshotCapture { id, captured })
    }

    /// Finish provider materialization and publish the resulting lease. This
    /// may wait for cloud resources and must run outside the backup barrier.
    pub(crate) async fn materialize(
        self: &Arc<Self>,
        captured: SnapshotCapture,
    ) -> Result<SnapshotLeaseInfo, SnapshotError> {
        let SnapshotCapture { id, captured } = captured;
        let created = self.provider.materialize(captured).await?;
        let files = if created.deferred_ebs.is_some() {
            Vec::new()
        } else {
            match database_files(&created.root) {
                Ok(files) if !files.is_empty() => files,
                Ok(_) => {
                    self.provider.cleanup(&created.cleanup).await?;
                    return Err("base snapshot contains no database files".into());
                }
                Err(error) => {
                    let cleanup = self.provider.cleanup(&created.cleanup).await;
                    return Err(cleanup.err().unwrap_or_else(|| error.into()));
                }
            }
        };
        let source = created.source;
        let deferred_ebs = created.deferred_ebs.clone();
        self.leases.lock().unwrap().insert(
            id.clone(),
            Lease {
                root: created.root,
                files: files.clone(),
                cleanup: created.cleanup,
                expires: Instant::now() + self.ttl,
                released: false,
            },
        );
        spawn_expiry(Arc::downgrade(self), id.clone(), self.ttl);
        Ok(SnapshotLeaseInfo {
            id,
            source,
            files,
            ttl_secs: self.ttl.as_secs().max(1),
            direct_path_available: deferred_ebs.is_none() && self.allow_direct_path,
            deferred_ebs,
        })
    }

    pub(crate) async fn prepare(&self) -> Result<(), SnapshotError> {
        self.ensure_namespace().await.map(|_| ())
    }

    pub(crate) fn keep_alive(&self, id: &[u8]) -> Result<u64, SnapshotError> {
        let mut leases = self.leases.lock().unwrap();
        let lease = leases
            .get_mut(id)
            .ok_or("snapshot lease does not exist or has expired")?;
        if lease.released {
            return Err("snapshot lease was released and is awaiting cleanup".into());
        }
        lease.expires = Instant::now() + self.ttl;
        Ok(self.ttl.as_secs().max(1))
    }

    pub(crate) async fn open_file(
        &self,
        id: &[u8],
        name: &str,
        request_direct_path: bool,
    ) -> Result<SnapshotFileAccess, SnapshotError> {
        let (path, size, direct) = {
            let mut leases = self.leases.lock().unwrap();
            let lease = leases
                .get_mut(id)
                .ok_or("snapshot lease does not exist or has expired")?;
            if lease.released {
                return Err("snapshot lease was released and is awaiting cleanup".into());
            }
            let file = lease
                .files
                .iter()
                .find(|file| file.name == name)
                .ok_or("file is not part of this snapshot lease")?;
            lease.expires = Instant::now() + self.ttl;
            (
                lease.root.join(&file.name),
                file.size,
                request_direct_path && self.allow_direct_path,
            )
        };
        if direct {
            return Ok(SnapshotFileAccess::Direct { path, size });
        }
        Ok(SnapshotFileAccess::Stream {
            file: tokio::fs::File::open(path).await?,
            size,
        })
    }

    pub(crate) async fn release(self: &Arc<Self>, id: &[u8]) -> Result<(), SnapshotError> {
        let mut lease = self
            .leases
            .lock()
            .unwrap()
            .remove(id)
            .ok_or("snapshot lease does not exist or has expired")?;
        if let Err(error) = self.provider.cleanup(&lease.cleanup).await {
            lease.released = true;
            lease.expires = Instant::now() + self.cleanup_retry;
            self.leases.lock().unwrap().insert(id.to_vec(), lease);
            spawn_expiry(Arc::downgrade(self), id.to_vec(), self.cleanup_retry);
            return Err(error);
        }
        Ok(())
    }

    pub(crate) async fn shutdown(&self) {
        let leases = {
            let mut leases = self.leases.lock().unwrap();
            leases
                .drain()
                .map(|(_, lease)| lease.cleanup)
                .collect::<Vec<_>>()
        };
        for cleanup in leases {
            if let Err(error) = self.provider.cleanup(&cleanup).await {
                tracing::error!(error = %error, "cannot clean up base-snapshot lease");
            }
        }
    }

    pub(crate) fn reconcile_after_database_open(self: &Arc<Self>) {
        if self.namespace.get().is_some() {
            return;
        }
        let manager = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut retry = Duration::from_secs(1);
            loop {
                let Some(manager) = manager.upgrade() else {
                    return;
                };
                match manager.ensure_namespace().await {
                    Ok(_) => return,
                    Err(error) => {
                        tracing::error!(
                            error = %error,
                            retry_secs = retry.as_secs(),
                            "cannot reconcile abandoned base snapshots"
                        );
                    }
                }
                drop(manager);
                tokio::time::sleep(retry).await;
                retry = retry.saturating_mul(2).min(Duration::from_secs(60));
            }
        });
    }

    async fn ensure_namespace(&self) -> Result<&str, SnapshotError> {
        let namespace = self
            .namespace
            .get_or_try_init(|| async {
                let namespace = snapshot_namespace(&self.data_dir)?;
                self.provider.reconcile(&namespace).await?;
                Ok::<_, SnapshotError>(namespace)
            })
            .await?;
        Ok(namespace)
    }

    async fn expire_if_due(&self, id: &[u8]) -> Result<Option<Duration>, SnapshotError> {
        let lease = {
            let mut leases = self.leases.lock().unwrap();
            let Some(lease) = leases.get(id) else {
                return Ok(None);
            };
            let now = Instant::now();
            if lease.expires > now {
                return Ok(Some(lease.expires - now));
            }
            leases.remove(id)
        };
        if let Some(mut lease) = lease {
            if let Err(error) = self.provider.cleanup(&lease.cleanup).await {
                tracing::error!(error = %error, "cannot expire base-snapshot lease; retrying");
                lease.released = true;
                lease.expires = Instant::now() + self.cleanup_retry;
                self.leases.lock().unwrap().insert(id.to_vec(), lease);
                return Ok(Some(self.cleanup_retry));
            }
        }
        Ok(None)
    }
}

/// The privileged work one backend delegates, bound to that backend's own copy
/// of the configuration.
///
/// Constructing this is the agent's whole trust model: everything the
/// privileged side may act on is read from the configuration file here, once,
/// so nothing the daemon sends later can widen it.
enum AgentExecutor {
    Lvm {
        data_dir: PathBuf,
        config: crate::config::LvmSnapshotConfig,
    },
    Ebs(ebs::EbsAgent),
}

impl AgentExecutor {
    /// Build the executor for the configured backend, or explain why the
    /// backend needs no agent at all.
    pub fn new(
        data_dir: &Path,
        config: &SnapshotConfig,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        match config.backend {
            SnapshotBackend::Lvm => Ok(Self::Lvm {
                data_dir: data_dir.to_path_buf(),
                config: config
                    .lvm
                    .clone()
                    .ok_or("server.snapshot.lvm is required")?,
            }),
            SnapshotBackend::Ebs => {
                let ebs = config.ebs.as_ref().ok_or("server.snapshot.ebs is required")?;
                if ebs.materialization != crate::config::EbsMaterialization::Local {
                    return Err(
                        "deferred EBS materialization mounts in the archiver's worker and needs no local agent"
                            .into(),
                    );
                }
                Ok(Self::Ebs(ebs::EbsAgent::new(data_dir, ebs)))
            }
            SnapshotBackend::Zfs | SnapshotBackend::Btrfs => Err(
                "ZFS and Btrfs delegate snapshot privilege to the daemon's own account and need no agent"
                    .into(),
            ),
            SnapshotBackend::Disabled => {
                Err("the portable snapshot provider needs no privileged agent".into())
            }
        }
    }

    async fn execute(
        &self,
        work: crate::control::pb::SnapshotAgentWork,
    ) -> crate::control::pb::CompleteSnapshotAgentWorkRequest {
        match self {
            Self::Lvm { data_dir, config } => lvm::execute_agent_work(data_dir, config, work).await,
            Self::Ebs(agent) => agent.execute(work).await,
        }
    }
}

/// Run one privileged agent connection until the control server closes it.
pub async fn run_snapshot_agent(
    control_socket: &Path,
    data_dir: &Path,
    config: &SnapshotConfig,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let executor = AgentExecutor::new(data_dir, config)?;
    let channel = crate::local_transport::connect_privileged_control(control_socket).await?;
    let mut client = crate::control::pb::control_plane_client::ControlPlaneClient::new(channel);
    loop {
        let work = client
            .claim_snapshot_agent_work(crate::control::pb::ClaimSnapshotAgentWorkRequest {})
            .await?
            .into_inner();
        let completion = executor.execute(work).await;
        client.complete_snapshot_agent_work(completion).await?;
    }
}

fn spawn_expiry(manager: Weak<SnapshotManager>, id: Vec<u8>, initial_delay: Duration) {
    tokio::spawn(async move {
        let mut delay = initial_delay;
        loop {
            tokio::time::sleep(delay).await;
            let Some(manager) = manager.upgrade() else {
                return;
            };
            match manager.expire_if_due(&id).await {
                Ok(Some(next)) => delay = next,
                Ok(None) => return,
                Err(error) => {
                    tracing::error!(error = %error, "cannot expire base-snapshot lease");
                    return;
                }
            }
        }
    });
}

fn snapshot_namespace(data_dir: &Path) -> Result<String, SnapshotError> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let uuid = yesno_core::database_uuid(data_dir)?;
    let mut encoded = String::with_capacity(32);
    for byte in uuid {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    Ok(format!("yesno-snapshot-{encoded}"))
}

fn new_lease_id(namespace: &str) -> Vec<u8> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let id = LEASE_ID.fetch_add(1, Ordering::Relaxed);
    format!("{namespace}-{}-{nonce:032x}-{id:016x}", std::process::id()).into_bytes()
}

fn path_arg(path: &Path) -> Result<String, SnapshotError> {
    path.to_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| format!("snapshot path '{}' is not UTF-8", path.display()).into())
}

/// Delete a Btrfs snapshot subvolume, clearing its read-only property only if
/// the kernel refuses the delete outright.
///
/// A delegated daemon cannot delete a *read-only* subvolume. Owning the
/// subvolume and mounting with `user_subvol_rm_allowed` is enough for a
/// read-write one, but the read-only flag — which is exactly what makes a
/// lease immutable, so it is not negotiable — turns the ioctl into `EROFS`
/// for anyone without `CAP_SYS_ADMIN`. Clearing `ro` is permitted for the
/// owner, and cleanup runs only after the lease is released or expired, so no
/// reader can observe the writable window. A root daemon never reaches the
/// retry because its first delete succeeds.
async fn delete_btrfs_subvolume(path: &Path) -> Result<(), SnapshotError> {
    let path = path_arg(path)?;
    let refusal = match checked_command("btrfs", &["subvolume", "delete", &path]).await {
        Ok(_) => return Ok(()),
        Err(refusal) => refusal,
    };
    run_command(
        "btrfs",
        &["property", "set", "-f", "-ts", &path, "ro", "false"],
    )
    .await
    .map_err(|error| format!("{refusal}; clearing the read-only property also failed: {error}"))?;
    run_command("btrfs", &["subvolume", "delete", &path]).await
}

async fn run_command(program: &str, args: &[&str]) -> Result<(), SnapshotError> {
    checked_command(program, args).await.map(|_| ())
}

async fn checked_command(
    program: &str,
    args: &[&str],
) -> Result<std::process::Output, SnapshotError> {
    let output = tokio::process::Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;
    if output.status.success() {
        return Ok(output);
    }
    Err(format!(
        "{program} {} failed with {}: {}",
        args.join(" "),
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    )
    .into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manager(
        root: &Path,
        ttl: Duration,
        direct: bool,
    ) -> (Arc<SnapshotManager>, Arc<AtomicUsize>) {
        SnapshotManager::for_test(root, ttl, direct)
    }

    fn fixture(root: &Path) {
        std::fs::write(root.join("MANIFEST"), b"manifest").unwrap();
        std::fs::write(root.join("shard-0000.yno"), b"image").unwrap();
        std::fs::write(root.join("shard-0000.wal.00000000000000000001"), b"wal").unwrap();
        std::fs::write(root.join("LOCK"), b"process-local").unwrap();
        std::fs::create_dir(root.join("nested")).unwrap();
        std::fs::write(root.join("nested/foreign"), b"foreign").unwrap();
    }

    #[tokio::test]
    async fn lease_lists_only_database_files_and_releases_exactly_once() {
        let root = tempfile::tempdir().unwrap();
        fixture(root.path());
        let (manager, cleanups) = manager(root.path(), Duration::from_secs(5), false);
        let lease = manager.begin().await.unwrap();
        assert_eq!(
            lease
                .files
                .iter()
                .map(|file| file.name.as_str())
                .collect::<Vec<_>>(),
            [
                "MANIFEST",
                "shard-0000.wal.00000000000000000001",
                "shard-0000.yno"
            ]
        );
        manager.release(&lease.id).await.unwrap();
        assert_eq!(cleanups.load(Ordering::Relaxed), 1);
        assert!(manager.release(&lease.id).await.is_err());
    }

    #[tokio::test]
    async fn direct_path_requires_server_opt_in_and_an_explicit_request() {
        let root = tempfile::tempdir().unwrap();
        fixture(root.path());
        let (manager, _) = manager(root.path(), Duration::from_secs(5), true);
        let lease = manager.begin().await.unwrap();
        assert!(matches!(
            manager
                .open_file(&lease.id, "MANIFEST", false)
                .await
                .unwrap(),
            SnapshotFileAccess::Stream { .. }
        ));
        assert!(matches!(
            manager
                .open_file(&lease.id, "MANIFEST", true)
                .await
                .unwrap(),
            SnapshotFileAccess::Direct { .. }
        ));
        assert!(manager
            .open_file(&lease.id, "../MANIFEST", true)
            .await
            .is_err());
        manager.release(&lease.id).await.unwrap();
    }

    #[tokio::test]
    async fn disabled_filesystem_backend_stages_an_immutable_portable_snapshot() {
        let parent = tempfile::tempdir().unwrap();
        let data = parent.path().join("database");
        {
            let _db = yesno_core::Db::open(&data).unwrap();
        }
        let original_manifest = std::fs::read(data.join("MANIFEST")).unwrap();
        let manager = SnapshotManager::from_config(
            &SnapshotConfig::default(),
            &data,
            agent::SnapshotAgentBroker::new(),
        )
        .unwrap();

        let lease = manager.begin().await.unwrap();
        assert_eq!(lease.source, SnapshotSource::Portable);
        assert!(!lease.direct_path_available);
        std::fs::write(data.join("MANIFEST"), b"changed live manifest").unwrap();

        let SnapshotFileAccess::Stream { mut file, .. } = manager
            .open_file(&lease.id, "MANIFEST", true)
            .await
            .unwrap()
        else {
            panic!("portable snapshots must never disclose a server-local path")
        };
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).await.unwrap();
        assert_eq!(bytes, original_manifest);

        manager.release(&lease.id).await.unwrap();
        assert_eq!(std::fs::read_dir(parent.path()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn abandoned_lease_expires_and_is_cleaned() {
        let root = tempfile::tempdir().unwrap();
        fixture(root.path());
        let (manager, cleanups) = manager(root.path(), Duration::from_millis(25), false);
        let lease = manager.begin().await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while cleanups.load(Ordering::Relaxed) == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert!(manager.keep_alive(&lease.id).is_err());
    }

    #[tokio::test]
    async fn failed_release_is_retried_without_reopening_the_lease() {
        let root = tempfile::tempdir().unwrap();
        fixture(root.path());
        let (manager, cleanups) = SnapshotManager::for_test_with_cleanup_failures(
            root.path(),
            Duration::from_secs(5),
            false,
            1,
        );
        let lease = manager.begin().await.unwrap();

        assert!(manager.release(&lease.id).await.is_err());
        assert!(manager.keep_alive(&lease.id).is_err());
        assert!(manager
            .open_file(&lease.id, "MANIFEST", false)
            .await
            .is_err());
        tokio::time::timeout(Duration::from_secs(1), async {
            while cleanups.load(Ordering::Relaxed) < 2 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert!(manager.release(&lease.id).await.is_err());
    }

    #[tokio::test]
    async fn restart_reconciles_abandoned_portable_snapshot_in_database_namespace() {
        let parent = tempfile::tempdir().unwrap();
        let data = parent.path().join("database");
        {
            let _db = yesno_core::Db::open(&data).unwrap();
        }
        let manager = SnapshotManager::from_config(
            &SnapshotConfig::default(),
            &data,
            agent::SnapshotAgentBroker::new(),
        )
        .unwrap();
        let lease = manager.begin().await.unwrap();
        let id = String::from_utf8(lease.id).unwrap();
        let abandoned = parent.path().join(format!(".database.{id}.portable"));
        let unrelated = parent
            .path()
            .join(".database.yesno-snapshot-another-database-1.portable");
        std::fs::create_dir(&unrelated).unwrap();
        assert!(abandoned.is_dir());
        drop(manager);

        let restarted = SnapshotManager::from_config(
            &SnapshotConfig::default(),
            &data,
            agent::SnapshotAgentBroker::new(),
        )
        .unwrap();
        restarted.ensure_namespace().await.unwrap();
        assert!(!abandoned.exists());
        assert!(unrelated.is_dir());
        assert!(data.is_dir());
    }
}
