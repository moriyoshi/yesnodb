//! Local LVM file-bearing leases executed by a privileged control client.
//!
//! The provider inside `yesnod` never opens a block device, runs LVM, or
//! mounts a filesystem. It queues database-scoped work while retaining the
//! backup barrier and lease table. `yesno-snapshot-agent` authenticates with
//! the Unix control transport's explicit, peer-matched root credentials and
//! executes the fixed local LVM configuration.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;

use super::agent::{owned_name, validate_work, SnapshotAgentBroker};
use super::{
    checked_command, path_arg, run_command, CapturedSnapshot, Cleanup, CreatedSnapshot,
    SnapshotError, SnapshotProvider, SnapshotSource,
};
use crate::config::LvmSnapshotConfig;
use crate::control::pb;

pub(super) struct LvmProvider {
    broker: SnapshotAgentBroker,
    timeout: Duration,
}

pub(super) struct LvmCapture {
    namespace: String,
    name: String,
}

#[derive(Debug)]
pub(super) struct LvmCleanup {
    namespace: String,
    name: String,
}

struct LvmExecutor {
    data_dir: PathBuf,
    config: LvmSnapshotConfig,
}

impl LvmProvider {
    pub(super) fn new(broker: SnapshotAgentBroker, config: &LvmSnapshotConfig) -> Self {
        Self {
            broker,
            timeout: Duration::from_secs(config.operation_timeout_secs),
        }
    }

    async fn execute(
        &self,
        operation: pb::SnapshotAgentOperation,
        namespace: &str,
        name: &str,
    ) -> Result<super::agent::AgentReply, SnapshotError> {
        self.broker
            .execute(operation, namespace, name, self.timeout)
            .await
    }
}

#[async_trait]
impl SnapshotProvider for LvmProvider {
    async fn capture(
        &self,
        namespace: &str,
        name: &str,
    ) -> Result<CapturedSnapshot, SnapshotError> {
        self.execute(pb::SnapshotAgentOperation::Capture, namespace, name)
            .await?;
        Ok(CapturedSnapshot::Lvm(LvmCapture {
            namespace: namespace.to_owned(),
            name: name.to_owned(),
        }))
    }

    async fn materialize(
        &self,
        captured: CapturedSnapshot,
    ) -> Result<CreatedSnapshot, SnapshotError> {
        let CapturedSnapshot::Lvm(capture) = captured else {
            return Err("LVM snapshot received foreign capture state".into());
        };
        let reply = self
            .execute(
                pb::SnapshotAgentOperation::Materialize,
                &capture.namespace,
                &capture.name,
            )
            .await?;
        let root = PathBuf::from(reply.root);
        if !root.is_absolute() {
            return Err("snapshot agent returned a non-absolute materialized root".into());
        }
        Ok(CreatedSnapshot {
            root,
            source: SnapshotSource::Lvm,
            cleanup: Cleanup::Lvm(LvmCleanup {
                namespace: capture.namespace,
                name: capture.name,
            }),
            deferred_ebs: None,
        })
    }

    async fn cleanup(&self, cleanup: &Cleanup) -> Result<(), SnapshotError> {
        let Cleanup::Lvm(cleanup) = cleanup else {
            return Err("LVM snapshot received foreign cleanup state".into());
        };
        self.execute(
            pb::SnapshotAgentOperation::Cleanup,
            &cleanup.namespace,
            &cleanup.name,
        )
        .await
        .map(|_| ())
    }

    async fn reconcile(&self, namespace: &str) -> Result<(), SnapshotError> {
        self.execute(pb::SnapshotAgentOperation::Reconcile, namespace, "")
            .await
            .map(|_| ())
    }
}

impl LvmExecutor {
    fn new(data_dir: &Path, config: &LvmSnapshotConfig) -> Self {
        Self {
            data_dir: data_dir.to_path_buf(),
            config: config.clone(),
        }
    }

    fn origin(&self) -> String {
        format!(
            "{}/{}",
            self.config.volume_group, self.config.logical_volume
        )
    }

    fn snapshot(&self, name: &str) -> String {
        format!("{}/{name}", self.config.volume_group)
    }

    fn cleanup_for(&self, name: &str) -> AgentCleanup {
        AgentCleanup {
            snapshot: self.snapshot(name),
            mount_path: self.config.mount_dir.join(name),
        }
    }

    async fn prepare_paths(&self) -> Result<(PathBuf, PathBuf, PathBuf), SnapshotError> {
        let data_dir = tokio::fs::canonicalize(&self.data_dir).await?;
        let source_mount = tokio::fs::canonicalize(&self.config.source_mount).await?;
        tokio::fs::create_dir_all(&self.config.mount_dir).await?;
        let mount_dir = tokio::fs::canonicalize(&self.config.mount_dir).await?;
        if mount_dir.starts_with(&source_mount) {
            return Err(format!(
                "LVM snapshot mount directory '{}' must be outside source mount '{}'",
                mount_dir.display(),
                source_mount.display()
            )
            .into());
        }
        if !data_dir.starts_with(&source_mount) {
            return Err(format!(
                "database directory '{}' is not below LVM source mount '{}'",
                data_dir.display(),
                source_mount.display()
            )
            .into());
        }
        Ok((data_dir, source_mount, mount_dir))
    }

    async fn verify_origin(&self, source_mount: &Path) -> Result<(), SnapshotError> {
        let source = command_value(
            "findmnt",
            &[
                "--noheadings",
                "--output",
                "SOURCE",
                "--target",
                &path_arg(source_mount)?,
            ],
            "source device",
        )
        .await?;
        let source = tokio::fs::canonicalize(&source).await.map_err(|error| {
            format!(
                "cannot resolve LVM source device '{}' for mount '{}': {error}",
                source,
                source_mount.display()
            )
        })?;
        let origin = self.origin();
        let expected = logical_volume_path(&origin).await?;
        let expected = tokio::fs::canonicalize(&expected).await.map_err(|error| {
            format!(
                "cannot resolve configured LVM origin '{}': {error}",
                expected.display()
            )
        })?;
        if source != expected {
            return Err(format!(
                "LVM source mount '{}' uses '{}', not configured origin '{}'",
                source_mount.display(),
                source.display(),
                expected.display()
            )
            .into());
        }
        let filesystem = command_value(
            "findmnt",
            &[
                "--noheadings",
                "--output",
                "FSTYPE",
                "--target",
                &path_arg(source_mount)?,
            ],
            "filesystem type",
        )
        .await?;
        if filesystem != self.config.filesystem {
            return Err(format!(
                "LVM source mount '{}' has filesystem '{filesystem}', configured '{}'",
                source_mount.display(),
                self.config.filesystem
            )
            .into());
        }
        let output = checked_command(
            "lvs",
            &[
                "--noheadings",
                "--segments",
                "--options",
                "segtype",
                &origin,
            ],
        )
        .await?;
        let segments = output_lines(&output.stdout)?;
        if segments.is_empty() || segments.iter().any(|segment| segment != "linear") {
            return Err(format!(
                "LVM snapshot origin '{origin}' must contain only linear segments"
            )
            .into());
        }
        Ok(())
    }

    async fn capture(&self, name: &str) -> Result<(), SnapshotError> {
        let (_, source_mount, _) = self.prepare_paths().await?;
        self.verify_origin(&source_mount).await?;
        run_command("sync", &["-f", &path_arg(&source_mount)?]).await?;
        let args = lvcreate_args(&self.config, name);
        let args = args.iter().map(String::as_str).collect::<Vec<_>>();
        run_command("lvcreate", &args).await
    }

    async fn materialize(&self, name: &str) -> Result<PathBuf, SnapshotError> {
        let (data_dir, source_mount, _) = self.prepare_paths().await?;
        let relative_data_dir = data_dir
            .strip_prefix(&source_mount)
            .expect("prepare_paths checked the data subpath");
        let cleanup = self.cleanup_for(name);
        match self
            .mount_snapshot(&cleanup.snapshot, &cleanup.mount_path)
            .await
        {
            Ok(()) => Ok(cleanup.mount_path.join(relative_data_dir)),
            Err(error) => match self.cleanup_resources(&cleanup).await {
                Ok(()) => Err(error),
                Err(cleanup_error) => Err(format!(
                    "{error}; LVM cleanup also failed and will need reconciliation: {cleanup_error}"
                )
                .into()),
            },
        }
    }

    async fn mount_snapshot(&self, snapshot: &str, mount_path: &Path) -> Result<(), SnapshotError> {
        tokio::fs::create_dir(mount_path).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            tokio::fs::set_permissions(mount_path, std::fs::Permissions::from_mode(0o700)).await?;
        }
        let device = path_arg(&logical_volume_path(snapshot).await?)?;
        let mount_path = path_arg(mount_path)?;
        let mut args = vec!["-t", self.config.filesystem.as_str()];
        if self.config.filesystem == "xfs" {
            args.extend(["-o", "nouuid"]);
        }
        args.extend([device.as_str(), mount_path.as_str()]);
        run_command("mount", &args).await?;
        if let Err(error) = async {
            run_command("sync", &["-f", &mount_path]).await?;
            run_command("mount", &["-o", "remount,ro", &mount_path]).await
        }
        .await
        {
            let _ = run_command("umount", &[&mount_path]).await;
            return Err(error);
        }
        Ok(())
    }

    async fn cleanup(&self, name: &str) -> Result<(), SnapshotError> {
        self.cleanup_resources(&self.cleanup_for(name)).await
    }

    async fn cleanup_resources(&self, cleanup: &AgentCleanup) -> Result<(), SnapshotError> {
        if is_mountpoint(&cleanup.mount_path).await? {
            run_command("umount", &[&path_arg(&cleanup.mount_path)?]).await?;
        }
        let (_, name) = cleanup
            .snapshot
            .split_once('/')
            .ok_or("invalid internal LVM snapshot identifier")?;
        if self.logical_volume_exists(name).await? {
            run_command("lvremove", &["--yes", &cleanup.snapshot]).await?;
        }
        remove_mount_dir(&cleanup.mount_path).await
    }

    async fn logical_volume_exists(&self, name: &str) -> Result<bool, SnapshotError> {
        let output = checked_command(
            "lvs",
            &[
                "--noheadings",
                "--options",
                "lv_name",
                &self.config.volume_group,
            ],
        )
        .await?;
        Ok(output_lines(&output.stdout)?
            .iter()
            .any(|candidate| candidate == name))
    }

    async fn reconcile(&self, namespace: &str) -> Result<(), SnapshotError> {
        self.prepare_paths().await?;
        for name in self.owned_snapshots(namespace).await? {
            self.cleanup(&name).await?;
        }
        Ok(())
    }

    async fn owned_snapshots(&self, namespace: &str) -> Result<Vec<String>, SnapshotError> {
        let output = checked_command(
            "lvs",
            &[
                "--noheadings",
                "--options",
                "lv_name,origin",
                "--separator",
                "\t",
                &self.config.volume_group,
            ],
        )
        .await?;
        parse_snapshot_rows(&output.stdout).map(|rows| {
            rows.into_iter()
                .filter(|(name, origin)| {
                    origin == &self.config.logical_volume && owned_name(namespace, name)
                })
                .map(|(name, _)| name)
                .collect()
        })
    }
}

#[derive(Debug)]
struct AgentCleanup {
    snapshot: String,
    mount_path: PathBuf,
}

pub(super) async fn execute_agent_work(
    data_dir: &Path,
    config: &LvmSnapshotConfig,
    work: pb::SnapshotAgentWork,
) -> pb::CompleteSnapshotAgentWorkRequest {
    let result = async {
        let operation = pb::SnapshotAgentOperation::try_from(work.operation)
            .map_err(|_| "snapshot agent received an unknown operation")?;
        validate_work(operation, &work.namespace, &work.lease_name)?;
        let executor = LvmExecutor::new(data_dir, config);
        match operation {
            pb::SnapshotAgentOperation::Reconcile => {
                executor.reconcile(&work.namespace).await?;
                Ok(String::new())
            }
            pb::SnapshotAgentOperation::Capture => {
                executor.capture(&work.lease_name).await?;
                Ok(String::new())
            }
            pb::SnapshotAgentOperation::Materialize => executor
                .materialize(&work.lease_name)
                .await
                .and_then(|path| path_arg(&path)),
            pb::SnapshotAgentOperation::Cleanup => {
                executor.cleanup(&work.lease_name).await?;
                Ok(String::new())
            }
            pb::SnapshotAgentOperation::Unspecified => {
                Err("snapshot agent received an unspecified operation".into())
            }
        }
    }
    .await;
    match result {
        Ok(root) => pb::CompleteSnapshotAgentWorkRequest {
            operation_id: work.operation_id,
            success: true,
            root,
            error: String::new(),
        },
        Err(error) => pb::CompleteSnapshotAgentWorkRequest {
            operation_id: work.operation_id,
            success: false,
            root: String::new(),
            error: error.to_string(),
        },
    }
}

fn lvcreate_args(config: &LvmSnapshotConfig, name: &str) -> Vec<String> {
    vec![
        "--snapshot".to_owned(),
        "--size".to_owned(),
        format!("{}G", config.snapshot_size_gib),
        "--name".to_owned(),
        name.to_owned(),
        format!("{}/{}", config.volume_group, config.logical_volume),
    ]
}

async fn logical_volume_path(logical_volume: &str) -> Result<PathBuf, SnapshotError> {
    let value = command_value(
        "lvs",
        &["--noheadings", "--options", "lv_path", logical_volume],
        "logical-volume path",
    )
    .await?;
    Ok(PathBuf::from(value))
}

async fn command_value(program: &str, args: &[&str], label: &str) -> Result<String, SnapshotError> {
    let output = checked_command(program, args).await?;
    let values = output_lines(&output.stdout)?;
    match values.as_slice() {
        [value] => Ok(value.clone()),
        _ => Err(format!(
            "{program} returned {} values for {label}, expected one",
            values.len()
        )
        .into()),
    }
}

fn output_lines(bytes: &[u8]) -> Result<Vec<String>, SnapshotError> {
    Ok(String::from_utf8(bytes.to_vec())?
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect())
}

fn parse_snapshot_rows(bytes: &[u8]) -> Result<Vec<(String, String)>, SnapshotError> {
    String::from_utf8(bytes.to_vec())?
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let mut columns = line.split('\t').map(str::trim);
            let name = columns.next().unwrap_or_default();
            let origin = columns.next().unwrap_or_default();
            if name.is_empty() || columns.next().is_some() {
                return Err(format!("cannot parse LVM logical-volume row '{line}'").into());
            }
            Ok((name.to_owned(), origin.to_owned()))
        })
        .collect()
}

async fn is_mountpoint(path: &Path) -> Result<bool, SnapshotError> {
    if !tokio::fs::try_exists(path).await? {
        return Ok(false);
    }
    let status = tokio::process::Command::new("mountpoint")
        .arg("-q")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await?;
    match status.code() {
        Some(0) => Ok(true),
        Some(32) => Ok(false),
        _ => Err(format!("mountpoint failed with {status} for '{}'", path.display()).into()),
    }
}

async fn remove_mount_dir(path: &Path) -> Result<(), SnapshotError> {
    match tokio::fs::remove_dir(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> LvmSnapshotConfig {
        LvmSnapshotConfig {
            source_mount: "/var/lib/yesno".into(),
            volume_group: "data-vg".to_owned(),
            logical_volume: "yesno".to_owned(),
            mount_dir: "/run/yesno-snapshots".into(),
            filesystem: "ext4".to_owned(),
            snapshot_size_gib: 12,
            operation_timeout_secs: 300,
        }
    }

    #[test]
    fn snapshot_creation_is_scoped_to_the_configured_origin_and_cow_size() {
        assert_eq!(
            lvcreate_args(&config(), "yesno-snapshot-db-1"),
            [
                "--snapshot",
                "--size",
                "12G",
                "--name",
                "yesno-snapshot-db-1",
                "data-vg/yesno",
            ]
        );
    }

    #[test]
    fn agent_work_is_confined_to_the_database_namespace() {
        let namespace = "yesno-snapshot-0123456789abcdef0123456789abcdef";
        assert!(validate_work(
            pb::SnapshotAgentOperation::Capture,
            namespace,
            &format!("{namespace}-1-abc-1")
        )
        .is_ok());
        assert!(validate_work(
            pb::SnapshotAgentOperation::Cleanup,
            namespace,
            "yesno-snapshot-ffffffffffffffffffffffffffffffff-1-abc-1"
        )
        .is_err());
        assert!(
            validate_work(pb::SnapshotAgentOperation::Reconcile, namespace, "foreign").is_err()
        );
    }

    #[test]
    fn reconciliation_selects_only_the_database_namespace_and_origin() {
        let rows = parse_snapshot_rows(
            b" yesno\t\n yesno-snapshot-db-1\tyesno\n yesno-snapshot-other-1\tyesno\n yesno-snapshot-db-2\tother\n",
        )
        .unwrap();
        let selected = rows
            .into_iter()
            .filter(|(name, origin)| origin == "yesno" && owned_name("yesno-snapshot-db", name))
            .map(|(name, _)| name)
            .collect::<Vec<_>>();
        assert_eq!(selected, ["yesno-snapshot-db-1"]);
    }

    #[test]
    fn malformed_lvm_rows_are_rejected_instead_of_guessed() {
        assert!(parse_snapshot_rows(b"snapshot\torigin\textra\n").is_err());
        assert!(!owned_name(
            "yesno-snapshot-db",
            "yesno-snapshot-db-../../foreign"
        ));
    }
}
