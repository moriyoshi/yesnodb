//! Amazon EBS implementation of the server-owned snapshot lease.
//!
//! `CreateSnapshot` is the point-in-time operation and is the only cloud call
//! made while the core backup barrier is held. Snapshot completion, temporary
//! volume creation, attachment and mounting happen during local materialization.
//! In deferred mode the server instead returns the completed EBS snapshot as a
//! provisional lease; only the archiver decides how to materialize that lease.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use aws_config::BehaviorVersion;
use aws_sdk_ec2::client::Waiters as _;
use aws_sdk_ec2::config::Region;
use aws_sdk_ec2::error::ProvideErrorMetadata;
use aws_sdk_ec2::types::{Filter, ResourceType, Tag, TagSpecification, VolumeState, VolumeType};
use aws_sdk_ec2::Client;
use tokio::sync::OnceCell;

use super::agent::{validate_work, SnapshotAgentBroker};
use super::{
    CapturedSnapshot, Cleanup, CreatedSnapshot, DeferredEbsSnapshot, SnapshotError,
    SnapshotProvider, SnapshotSource,
};
use crate::config::{EbsMaterialization, EbsSnapshotConfig};
use crate::control::pb;

const DATABASE_TAG: &str = "yesno:database";
const LEASE_TAG: &str = "yesno:lease";

pub(super) struct EbsProvider {
    data_dir: PathBuf,
    config: EbsSnapshotConfig,
    client: OnceCell<Client>,
    /// Present exactly when materialization is local, because that is the only
    /// mode with a mount, and the mount is the only privileged step.
    agent: Option<SnapshotAgentBroker>,
}

pub(super) struct EbsCapture {
    name: String,
    snapshot_id: String,
    volume_size_gib: u64,
    relative_data_dir: PathBuf,
}

#[derive(Debug)]
pub(super) struct EbsCleanup {
    namespace: String,
    name: String,
    snapshot_id: String,
    volume_id: Option<String>,
    /// A local lease has host state — a mount and an attachment — that only the
    /// agent can undo. A deferred one has neither.
    local: bool,
}

impl EbsProvider {
    pub(super) fn new(
        data_dir: &Path,
        config: &EbsSnapshotConfig,
        agent: Option<SnapshotAgentBroker>,
    ) -> Self {
        Self {
            data_dir: data_dir.to_path_buf(),
            config: config.clone(),
            client: OnceCell::new(),
            agent,
        }
    }

    fn agent(&self) -> Result<&SnapshotAgentBroker, SnapshotError> {
        self.agent
            .as_ref()
            .ok_or_else(|| "local EBS materialization has no privileged snapshot agent".into())
    }

    async fn delegate(
        &self,
        operation: pb::SnapshotAgentOperation,
        namespace: &str,
        name: &str,
    ) -> Result<super::agent::AgentReply, SnapshotError> {
        self.agent()?
            .execute(operation, namespace, name, self.timeout())
            .await
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(self.config.operation_timeout_secs)
    }

    async fn client(&self) -> &Client {
        self.client
            .get_or_init(|| async {
                let shared = aws_config::defaults(BehaviorVersion::latest())
                    .region(Region::new(self.config.region.clone()))
                    .load()
                    .await;
                Client::new(&shared)
            })
            .await
    }

    async fn create_snapshot(
        &self,
        namespace: &str,
        name: &str,
    ) -> Result<(String, u64), SnapshotError> {
        let output = self
            .client()
            .await
            .create_snapshot()
            .volume_id(&self.config.volume_id)
            .description(name)
            .tag_specifications(tag_specification(
                ResourceType::Snapshot,
                namespace,
                name,
                &self.config.resource_tags,
            ))
            .send()
            .await
            .map_err(|error| format!("failed to create EBS snapshot: {}", sdk_detail(&error)))?;
        let snapshot_id = require_id(output.snapshot_id().unwrap_or_default().to_owned(), "snap-")?;
        let volume_size_gib = u64::try_from(output.volume_size().unwrap_or_default())
            .map_err(|_| "AWS returned a negative EBS snapshot volume size")?;
        if volume_size_gib == 0 {
            return Err("AWS returned an empty EBS snapshot volume size".into());
        }
        Ok((snapshot_id, volume_size_gib))
    }

    async fn wait_snapshot(&self, snapshot_id: &str) -> Result<(), SnapshotError> {
        self.client()
            .await
            .wait_until_snapshot_completed()
            .snapshot_ids(snapshot_id)
            .wait(self.timeout())
            .await
            .map(|_| ())
            .map_err(|error| {
                format!(
                    "failed waiting for EBS snapshot {snapshot_id}: {}",
                    error_chain(&error)
                )
                .into()
            })
    }

    async fn create_volume(
        &self,
        namespace: &str,
        capture: &EbsCapture,
    ) -> Result<String, SnapshotError> {
        let output = self
            .client()
            .await
            .create_volume()
            .snapshot_id(&capture.snapshot_id)
            .availability_zone(&self.config.availability_zone)
            .volume_type(VolumeType::Gp3)
            .tag_specifications(tag_specification(
                ResourceType::Volume,
                namespace,
                &capture.name,
                &self.config.resource_tags,
            ))
            .send()
            .await
            .map_err(|error| {
                format!(
                    "failed to create EBS volume from snapshot: {}",
                    sdk_detail(&error)
                )
            })?;
        require_id(output.volume_id().unwrap_or_default().to_owned(), "vol-")
    }

    async fn volume_state(&self, volume_id: &str) -> Result<Option<VolumeState>, SnapshotError> {
        match self
            .client()
            .await
            .describe_volumes()
            .volume_ids(volume_id)
            .send()
            .await
        {
            Ok(output) => Ok(output
                .volumes()
                .first()
                .and_then(|volume| volume.state().cloned())),
            Err(error)
                if error.as_service_error().and_then(|service| service.code())
                    == Some("InvalidVolume.NotFound") =>
            {
                Ok(None)
            }
            Err(error) => Err(format!(
                "failed to describe EBS volume {volume_id}: {}",
                sdk_detail(&error)
            )
            .into()),
        }
    }

    async fn snapshot_exists(&self, snapshot_id: &str) -> Result<bool, SnapshotError> {
        match self
            .client()
            .await
            .describe_snapshots()
            .snapshot_ids(snapshot_id)
            .send()
            .await
        {
            Ok(output) => Ok(!output.snapshots().is_empty()),
            Err(error)
                if error.as_service_error().and_then(|service| service.code())
                    == Some("InvalidSnapshot.NotFound") =>
            {
                Ok(false)
            }
            Err(error) => Err(format!(
                "failed to describe EBS snapshot {snapshot_id}: {}",
                sdk_detail(&error)
            )
            .into()),
        }
    }

    async fn wait_volume_available(&self, volume_id: &str) -> Result<(), SnapshotError> {
        self.client()
            .await
            .wait_until_volume_available()
            .volume_ids(volume_id)
            .wait(self.timeout())
            .await
            .map(|_| ())
            .map_err(|error| {
                format!(
                    "failed waiting for EBS volume {volume_id} to become available: {}",
                    error_chain(&error)
                )
                .into()
            })
    }

    async fn verify_source_volume(&self, source_mount: &Path) -> Result<(), SnapshotError> {
        let output = host_output(
            "findmnt",
            &[
                "--noheadings".to_owned(),
                "--output".to_owned(),
                "SOURCE".to_owned(),
                "--target".to_owned(),
                path_string(source_mount)?,
            ],
        )
        .await?;
        let source = resolve_device(&PathBuf::from(String::from_utf8(output.stdout)?.trim())).await;
        let volume_device = if let Some(device) = find_nvme_volume(&self.config.volume_id).await? {
            device
        } else {
            if self.config.materialization == EbsMaterialization::Deferred {
                return Err(format!(
                    "configured EBS volume {} is not visible in /sys/class/block; refusing to snapshot an unverified deferred source mount",
                    self.config.volume_id
                )
                .into());
            }
            let output = self
                .client()
                .await
                .describe_volumes()
                .volume_ids(&self.config.volume_id)
                .send()
                .await
                .map_err(|error| {
                    format!(
                        "failed to describe configured EBS volume {}: {}",
                        self.config.volume_id,
                        sdk_detail(&error)
                    )
                })?;
            let requested = output
                .volumes()
                .first()
                .into_iter()
                .flat_map(|volume| volume.attachments())
                .find(|attachment| attachment.instance_id() == Some(&self.config.instance_id))
                .and_then(|attachment| attachment.device())
                .ok_or_else(|| {
                    format!(
                        "configured EBS volume {} is not attached to instance {}",
                        self.config.volume_id, self.config.instance_id
                    )
                })?;
            let mut found = None;
            for candidate in xen_candidates(requested) {
                if tokio::fs::try_exists(&candidate).await? {
                    found = Some(candidate);
                    break;
                }
            }
            found.ok_or_else(|| {
                format!(
                    "configured EBS volume {} is not visible as a local block device",
                    self.config.volume_id
                )
            })?
        };
        let volume_device = resolve_device(&volume_device).await;
        if !same_device_or_partition(&source, &volume_device) {
            return Err(format!(
                "EBS volume {} resolves to '{}', but source mount '{}' uses '{}'",
                self.config.volume_id,
                volume_device.display(),
                source_mount.display(),
                source.display()
            )
            .into());
        }
        Ok(())
    }

    /// Undo one lease, host state first.
    ///
    /// The agent owns the unmount and the detach because both are host
    /// operations; the daemon owns the deletes because neither touches the
    /// host. Ordering matters in one direction only: the volume cannot be
    /// deleted until the agent has detached it, which is why the delegated
    /// step is not merely first but must succeed before the deletes run.
    async fn cleanup_resources(&self, cleanup: &EbsCleanup) -> Result<(), SnapshotError> {
        if cleanup.local {
            self.delegate(
                pb::SnapshotAgentOperation::Cleanup,
                &cleanup.namespace,
                &cleanup.name,
            )
            .await?;
        }
        if let Some(volume_id) = &cleanup.volume_id {
            if self.volume_state(volume_id).await?.is_some() {
                self.delete_volume(volume_id).await?;
            }
        }
        if self.snapshot_exists(&cleanup.snapshot_id).await? {
            self.delete_snapshot(&cleanup.snapshot_id).await?;
        }
        Ok(())
    }

    async fn delete_volume(&self, volume_id: &str) -> Result<(), SnapshotError> {
        match self
            .client()
            .await
            .delete_volume()
            .volume_id(volume_id)
            .send()
            .await
        {
            Ok(_) => Ok(()),
            Err(error)
                if error.as_service_error().and_then(|service| service.code())
                    == Some("InvalidVolume.NotFound") =>
            {
                Ok(())
            }
            Err(error) => Err(format!(
                "failed to delete EBS volume {volume_id}: {}",
                sdk_detail(&error)
            )
            .into()),
        }
    }

    async fn delete_snapshot(&self, snapshot_id: &str) -> Result<(), SnapshotError> {
        match self
            .client()
            .await
            .delete_snapshot()
            .snapshot_id(snapshot_id)
            .send()
            .await
        {
            Ok(_) => Ok(()),
            Err(error)
                if error.as_service_error().and_then(|service| service.code())
                    == Some("InvalidSnapshot.NotFound") =>
            {
                Ok(())
            }
            Err(error) => Err(format!(
                "failed to delete EBS snapshot {snapshot_id}: {}",
                sdk_detail(&error)
            )
            .into()),
        }
    }

    async fn tagged_volumes(
        &self,
        namespace: &str,
    ) -> Result<Vec<(String, Option<String>)>, SnapshotError> {
        let mut pages = self
            .client()
            .await
            .describe_volumes()
            .filters(database_filter(namespace))
            .into_paginator()
            .send();
        let mut resources = Vec::new();
        while let Some(page) = pages
            .try_next()
            .await
            .map_err(|error| format!("failed to list owned EBS volumes: {}", sdk_detail(&error)))?
        {
            resources.extend(page.volumes().iter().filter_map(|volume| {
                volume.volume_id().map(|id| {
                    (
                        id.to_owned(),
                        tag_value(volume.tags(), LEASE_TAG).map(ToOwned::to_owned),
                    )
                })
            }));
        }
        Ok(resources)
    }

    async fn tagged_snapshots(
        &self,
        namespace: &str,
    ) -> Result<Vec<(String, Option<String>)>, SnapshotError> {
        let mut pages = self
            .client()
            .await
            .describe_snapshots()
            .owner_ids("self")
            .filters(database_filter(namespace))
            .into_paginator()
            .send();
        let mut resources = Vec::new();
        while let Some(page) = pages.try_next().await.map_err(|error| {
            format!("failed to list owned EBS snapshots: {}", sdk_detail(&error))
        })? {
            resources.extend(page.snapshots().iter().filter_map(|snapshot| {
                snapshot.snapshot_id().map(|id| {
                    (
                        id.to_owned(),
                        tag_value(snapshot.tags(), LEASE_TAG).map(ToOwned::to_owned),
                    )
                })
            }));
        }
        Ok(resources)
    }

    /// Create the temporary volume, then hand the lease to the agent.
    ///
    /// Everything up to here is a cloud call with no effect on this host. The
    /// agent performs the attachment and the mount together, and it locates the
    /// volume by the ownership tags rather than from anything passed here, so a
    /// daemon that has been taken over cannot name a volume of its choosing.
    async fn materialize_local(
        &self,
        namespace: &str,
        capture: EbsCapture,
    ) -> Result<CreatedSnapshot, SnapshotError> {
        let volume_id = match self.create_volume(namespace, &capture).await {
            Ok(volume_id) => volume_id,
            Err(error) => {
                let _ = self.delete_snapshot(&capture.snapshot_id).await;
                return Err(error);
            }
        };
        let cleanup = EbsCleanup {
            namespace: namespace.to_owned(),
            name: capture.name.clone(),
            snapshot_id: capture.snapshot_id,
            volume_id: Some(volume_id),
            local: true,
        };
        let materialize = async {
            let reply = self
                .delegate(
                    pb::SnapshotAgentOperation::Materialize,
                    namespace,
                    &cleanup.name,
                )
                .await?;
            let root = PathBuf::from(reply.root);
            if !root.is_absolute() {
                return Err("snapshot agent returned a non-absolute materialized root".into());
            }
            Ok::<_, SnapshotError>(root)
        }
        .await;
        match materialize {
            Ok(root) => Ok(CreatedSnapshot {
                root,
                source: SnapshotSource::Ebs,
                cleanup: Cleanup::Ebs(cleanup),
                deferred_ebs: None,
            }),
            Err(error) => self.materialization_failure(error, cleanup).await,
        }
    }

    fn materialize_deferred(&self, capture: EbsCapture) -> Result<CreatedSnapshot, SnapshotError> {
        let source_subpath = capture
            .relative_data_dir
            .to_str()
            .ok_or("EBS database subpath is not UTF-8")?
            .to_owned();
        let cleanup = EbsCleanup {
            namespace: String::new(),
            name: capture.name.clone(),
            snapshot_id: capture.snapshot_id.clone(),
            volume_id: None,
            local: false,
        };
        Ok(CreatedSnapshot {
            root: PathBuf::new(),
            source: SnapshotSource::Ebs,
            cleanup: Cleanup::Ebs(cleanup),
            deferred_ebs: Some(DeferredEbsSnapshot {
                snapshot_id: capture.snapshot_id,
                region: self.config.region.clone(),
                filesystem: self.config.filesystem.clone(),
                source_subpath,
                volume_size_gib: capture.volume_size_gib,
            }),
        })
    }

    async fn materialization_failure(
        &self,
        error: SnapshotError,
        cleanup: EbsCleanup,
    ) -> Result<CreatedSnapshot, SnapshotError> {
        if let Err(cleanup_error) = self.cleanup_resources(&cleanup).await {
            return Err(format!(
                "{error}; EBS cleanup also failed and will need startup reconciliation: {cleanup_error}"
            )
            .into());
        }
        Err(error)
    }
}

#[async_trait]
impl SnapshotProvider for EbsProvider {
    async fn capture(
        &self,
        namespace: &str,
        name: &str,
    ) -> Result<CapturedSnapshot, SnapshotError> {
        let data_dir = tokio::fs::canonicalize(&self.data_dir).await?;
        let source_mount = tokio::fs::canonicalize(&self.config.source_mount).await?;
        let relative_data_dir = data_dir.strip_prefix(&source_mount).map_err(|_| {
            format!(
                "database directory '{}' is not below EBS source mount '{}'",
                data_dir.display(),
                source_mount.display()
            )
        })?;
        self.verify_source_volume(&source_mount).await?;
        // Attachment-name reservation used to happen here, which put a
        // DescribeVolumes call inside the backup barrier for no reason: the
        // agent now picks the name at attach time, outside it.
        run_host("sync", &["-f".to_owned(), path_string(&source_mount)?]).await?;
        let (snapshot_id, volume_size_gib) = self.create_snapshot(namespace, name).await?;
        Ok(CapturedSnapshot::Ebs(EbsCapture {
            name: name.to_owned(),
            snapshot_id,
            volume_size_gib,
            relative_data_dir: relative_data_dir.to_path_buf(),
        }))
    }

    async fn materialize(
        &self,
        captured: CapturedSnapshot,
    ) -> Result<CreatedSnapshot, SnapshotError> {
        let CapturedSnapshot::Ebs(capture) = captured else {
            return Err("EBS snapshot received foreign capture state".into());
        };
        let namespace = capture
            .name
            .get(.."yesno-snapshot-00000000000000000000000000000000".len())
            .ok_or("invalid EBS snapshot lease name")?
            .to_owned();
        if let Err(error) = self.wait_snapshot(&capture.snapshot_id).await {
            let cleanup = self.delete_snapshot(&capture.snapshot_id).await;
            return match cleanup {
                Ok(()) => Err(error),
                Err(cleanup_error) => Err(format!(
                    "{error}; EBS snapshot deletion also failed and will need startup reconciliation: {cleanup_error}"
                )
                .into()),
            };
        }
        match self.config.materialization {
            EbsMaterialization::Local => self.materialize_local(&namespace, capture).await,
            EbsMaterialization::Deferred => self.materialize_deferred(capture),
        }
    }

    async fn cleanup(&self, cleanup: &Cleanup) -> Result<(), SnapshotError> {
        match cleanup {
            Cleanup::Ebs(cleanup) => self.cleanup_resources(cleanup).await,
            _ => Err("EBS snapshot received foreign cleanup state".into()),
        }
    }

    async fn reconcile(&self, namespace: &str) -> Result<(), SnapshotError> {
        // The agent unmounts and detaches everything this database left behind
        // before any of it is deleted, for the same reason lease cleanup does:
        // an attached volume cannot be deleted, and only the agent can detach.
        if self.config.materialization == EbsMaterialization::Local {
            self.delegate(pb::SnapshotAgentOperation::Reconcile, namespace, "")
                .await?;
            for (volume_id, lease) in self.tagged_volumes(namespace).await? {
                let lease = lease.ok_or_else(|| {
                    format!("owned EBS volume {volume_id} has no {LEASE_TAG} tag")
                })?;
                if !valid_lease_tag(namespace, &lease) {
                    return Err(format!(
                        "owned EBS volume {volume_id} has invalid lease tag '{lease}'"
                    )
                    .into());
                }
                if let Some(state) = self.volume_state(&volume_id).await? {
                    if state == VolumeState::Creating {
                        self.wait_volume_available(&volume_id).await?;
                    }
                    self.delete_volume(&volume_id).await?;
                }
            }
        }
        for (snapshot_id, _) in self.tagged_snapshots(namespace).await? {
            if self.snapshot_exists(&snapshot_id).await? {
                self.delete_snapshot(&snapshot_id).await?;
            }
        }
        Ok(())
    }
}

fn database_filter(namespace: &str) -> Filter {
    Filter::builder()
        .name(format!("tag:{DATABASE_TAG}"))
        .values(namespace)
        .build()
}

fn tag_specification(
    resource: ResourceType,
    namespace: &str,
    name: &str,
    resource_tags: &BTreeMap<String, String>,
) -> TagSpecification {
    let mut tags = resource_tags
        .iter()
        .map(|(key, value)| Tag::builder().key(key).value(value).build())
        .collect::<Vec<_>>();
    tags.extend([
        Tag::builder().key(DATABASE_TAG).value(namespace).build(),
        Tag::builder().key(LEASE_TAG).value(name).build(),
    ]);
    TagSpecification::builder()
        .resource_type(resource)
        .set_tags(Some(tags))
        .build()
}

fn tag_value<'a>(tags: &'a [Tag], key: &str) -> Option<&'a str> {
    tags.iter()
        .find(|tag| tag.key() == Some(key))
        .and_then(Tag::value)
}

fn require_id(value: String, prefix: &str) -> Result<String, SnapshotError> {
    if value.starts_with(prefix)
        && value[prefix.len()..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        Ok(value)
    } else {
        Err(format!("AWS returned invalid {prefix} identifier '{value}'").into())
    }
}

fn valid_lease_tag(namespace: &str, lease: &str) -> bool {
    lease.strip_prefix(namespace).is_some_and(|suffix| {
        suffix.starts_with('-')
            && suffix.len() > 1
            && suffix[1..]
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    })
}

/// The AWS error behind a failed call, rather than the SDK's own `Display`.
///
/// `SdkError`'s `Display` is the bare string `"service error"` for *every*
/// rejection the service sends. The code and the message are in the error
/// metadata and `{error}` drops both, so `"failed to create EBS snapshot:
/// service error"` names neither the permission that was missing nor the
/// parameter that was wrong. That is what a live gate run produced on
/// 2026-09-02, and it is what an operator would get from a production
/// misconfiguration on the same path.
fn sdk_detail<E>(error: &E) -> String
where
    E: ProvideErrorMetadata + std::error::Error,
{
    match (error.code(), error.message()) {
        (Some(code), Some(message)) => format!("{code}: {message}"),
        (Some(code), None) => code.to_owned(),
        (None, _) => error_chain(error),
    }
}

/// Every `Display` in the source chain, joined.
///
/// What is left to read when there is no error metadata: a dispatch or
/// construction failure that never reached the service, and the waiters, whose
/// own `Display` says only that the wait ended and keeps the call it was
/// polling one level down.
fn error_chain(error: &dyn std::error::Error) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(inner) = source {
        message.push_str(": ");
        message.push_str(&inner.to_string());
        source = inner.source();
    }
    message
}

fn path_string(path: &Path) -> Result<String, SnapshotError> {
    path.to_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| format!("EBS snapshot path '{}' is not UTF-8", path.display()).into())
}

async fn run_host(program: &str, args: &[String]) -> Result<(), SnapshotError> {
    host_output(program, args).await.map(|_| ())
}

async fn host_output(program: &str, args: &[String]) -> Result<Output, SnapshotError> {
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

/// Resolve a device path through its symlinks, keeping the name when it cannot
/// be resolved.
///
/// `canonicalize` requires the device **node** to exist, and deferred
/// materialization is defined by the daemon not having that authority: the
/// operator guide selects it for exactly the case where `yesnod` cannot attach
/// a volume itself, and its configuration carries no `instance_id`,
/// `device_names` or `mount_dir`. A daemon in a container sees the source
/// filesystem and `/sys/class/block` -- verified: a plain unprivileged
/// container reads `/sys/class/block/<name>/device/serial` and has no
/// `/dev/<name>` at all -- so requiring the node turned a check that could
/// succeed into `No such file or directory`.
///
/// Nothing is weakened by the fallback. What decides is
/// `same_device_or_partition`, which compares **file names**; canonicalization
/// is only here so that a mount recorded as `/dev/disk/by-id/...` still matches
/// the `/dev/nvme...` name `find_nvme_volume` builds. A name that cannot be
/// resolved is compared as it stands, and a mismatch is still a mismatch --
/// the failure direction is a false reject, never a false accept.
async fn resolve_device(path: &Path) -> PathBuf {
    tokio::fs::canonicalize(path)
        .await
        .unwrap_or_else(|_| path.to_path_buf())
}

fn same_device_or_partition(source: &Path, device: &Path) -> bool {
    if source == device {
        return true;
    }
    let Some(source) = source.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let Some(device) = device.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let suffix = source.strip_prefix(device).unwrap_or_default();
    if device.ends_with(|ch: char| ch.is_ascii_digit()) {
        suffix
            .strip_prefix('p')
            .is_some_and(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
    } else {
        !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
    }
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

async fn find_nvme_volume(volume_id: &str) -> Result<Option<PathBuf>, SnapshotError> {
    let expected = normalize_volume_id(volume_id);
    let mut entries = match tokio::fs::read_dir("/sys/class/block").await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    while let Some(entry) = entries.next_entry().await? {
        let serial_path = entry.path().join("device/serial");
        let Ok(serial) = tokio::fs::read_to_string(serial_path).await else {
            continue;
        };
        if normalize_volume_id(serial.trim()) == expected {
            return Ok(Some(Path::new("/dev").join(entry.file_name())));
        }
    }
    Ok(None)
}

fn normalize_volume_id(value: &str) -> String {
    value.chars().filter(|ch| *ch != '-').collect()
}

fn xen_candidates(requested: &str) -> Vec<PathBuf> {
    let mut candidates = vec![PathBuf::from(requested)];
    if let Some(suffix) = requested.strip_prefix("/dev/sd") {
        candidates.push(PathBuf::from(format!("/dev/xvd{suffix}")));
    }
    candidates
}

async fn wait_partition(
    device: PathBuf,
    partition: Option<u32>,
    deadline: Instant,
) -> Result<PathBuf, SnapshotError> {
    let Some(partition) = partition else {
        return Ok(device);
    };
    let separator = if device
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(|ch: char| ch.is_ascii_digit()))
    {
        'p'
    } else {
        char::default()
    };
    let partition = PathBuf::from(format!("{}{separator}{partition}", device.display()));
    loop {
        if tokio::fs::try_exists(&partition).await? {
            return Ok(partition);
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "timed out waiting for EBS snapshot partition '{}'",
                partition.display()
            )
            .into());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Privileged half of the local EBS provider.
///
/// This is the process that holds `CAP_SYS_ADMIN`, so treat every value that
/// arrives from the daemon as hostile. Only the lease name crosses, and
/// [`validate_work`] has already proved it lies inside the database namespace
/// before anything here uses it. The volume is then found by its ownership
/// tags rather than named by the caller, and the attachment name comes from
/// this agent's own configured pool — so a daemon that has been taken over can
/// ask for "materialize a lease of mine" and nothing else. Do not add a
/// parameter that lets the caller choose a device, a volume, or a path; that
/// single change would turn this into a general-purpose mount service running
/// as root.
pub(super) struct EbsAgent {
    data_dir: PathBuf,
    config: EbsSnapshotConfig,
    client: OnceCell<Client>,
}

/// One temporary volume this database owns, as EC2 reports it.
struct OwnedVolume {
    id: String,
    lease: Option<String>,
    state: Option<VolumeState>,
    attached_here: bool,
}

impl EbsAgent {
    pub(super) fn new(data_dir: &Path, config: &EbsSnapshotConfig) -> Self {
        Self {
            data_dir: data_dir.to_path_buf(),
            config: config.clone(),
            client: OnceCell::new(),
        }
    }

    pub(super) async fn execute(
        &self,
        work: pb::SnapshotAgentWork,
    ) -> pb::CompleteSnapshotAgentWorkRequest {
        let result = async {
            let operation = pb::SnapshotAgentOperation::try_from(work.operation)
                .map_err(|_| "snapshot agent received an unknown operation")?;
            validate_work(operation, &work.namespace, &work.lease_name)?;
            match operation {
                pb::SnapshotAgentOperation::Reconcile => {
                    self.reconcile(&work.namespace).await?;
                    Ok(String::new())
                }
                pb::SnapshotAgentOperation::Materialize => self
                    .materialize(&work.namespace, &work.lease_name)
                    .await
                    .and_then(|root| path_string(&root)),
                pb::SnapshotAgentOperation::Cleanup => {
                    self.cleanup(&work.namespace, &work.lease_name).await?;
                    Ok(String::new())
                }
                // The point-in-time snapshot is a pure cloud call under the
                // backup barrier. It has no host effect and must stay in the
                // daemon, so receiving it here means the two halves disagree.
                pb::SnapshotAgentOperation::Capture => {
                    Err("EBS capture belongs to the daemon and must not reach the agent".into())
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

    async fn client(&self) -> &Client {
        self.client
            .get_or_init(|| async {
                let shared = aws_config::defaults(BehaviorVersion::latest())
                    .region(Region::new(self.config.region.clone()))
                    .load()
                    .await;
                Client::new(&shared)
            })
            .await
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(self.config.operation_timeout_secs)
    }

    /// List the temporary volumes tagged as belonging to this database, and
    /// optionally to one lease of it.
    ///
    /// This is the agent's authority for "may I touch this volume". Both
    /// tags are applied by `CreateVolume` at creation time, so a daemon that
    /// can only create tagged resources cannot make a foreign volume match.
    async fn owned_volumes(
        &self,
        namespace: &str,
        lease: Option<&str>,
    ) -> Result<Vec<OwnedVolume>, SnapshotError> {
        let mut request = self
            .client()
            .await
            .describe_volumes()
            .filters(database_filter(namespace));
        if let Some(lease) = lease {
            request = request.filters(
                Filter::builder()
                    .name(format!("tag:{LEASE_TAG}"))
                    .values(lease)
                    .build(),
            );
        }
        let mut pages = request.into_paginator().send();
        let mut volumes = Vec::new();
        while let Some(page) = pages
            .try_next()
            .await
            .map_err(|error| format!("failed to list owned EBS volumes: {}", sdk_detail(&error)))?
        {
            for volume in page.volumes() {
                let Some(id) = volume.volume_id() else {
                    continue;
                };
                volumes.push(OwnedVolume {
                    id: id.to_owned(),
                    lease: tag_value(volume.tags(), LEASE_TAG).map(ToOwned::to_owned),
                    state: volume.state().cloned(),
                    attached_here: volume.attachments().iter().any(|attachment| {
                        attachment.instance_id() == Some(self.config.instance_id.as_str())
                    }),
                });
            }
        }
        Ok(volumes)
    }

    /// Choose an attachment name from this agent's configured pool.
    ///
    /// The agent handles one work item at a time and waits for the attachment
    /// to reach `in-use` before returning, so EC2's own view is authoritative
    /// here and no in-process reservation set is needed.
    async fn reserve_device(&self) -> Result<String, SnapshotError> {
        let filter = Filter::builder()
            .name("attachment.instance-id")
            .values(&self.config.instance_id)
            .build();
        let mut pages = self
            .client()
            .await
            .describe_volumes()
            .filters(filter)
            .into_paginator()
            .send();
        let mut attached = HashSet::new();
        while let Some(page) = pages.try_next().await.map_err(|error| {
            format!(
                "failed to list EBS attachments for device reservation: {}",
                sdk_detail(&error)
            )
        })? {
            for attachment in page
                .volumes()
                .iter()
                .flat_map(|volume| volume.attachments())
            {
                if let Some(device) = attachment.device() {
                    attached.insert(device.to_owned());
                }
            }
        }
        self.config
            .device_names
            .iter()
            .find(|device| !attached.contains(device.as_str()))
            .cloned()
            .ok_or_else(|| "all configured EBS attachment names are in use".into())
    }

    async fn attach_volume(&self, volume_id: &str, device_name: &str) -> Result<(), SnapshotError> {
        self.client()
            .await
            .attach_volume()
            .volume_id(volume_id)
            .instance_id(&self.config.instance_id)
            .device(device_name)
            .send()
            .await
            .map_err(|error| {
                format!(
                    "failed to attach EBS volume {volume_id}: {}",
                    sdk_detail(&error)
                )
            })?;
        self.client()
            .await
            .wait_until_volume_in_use()
            .volume_ids(volume_id)
            .wait(self.timeout())
            .await
            .map(|_| ())
            .map_err(|error| {
                format!(
                    "failed waiting for EBS volume {volume_id} attachment: {}",
                    error_chain(&error)
                )
                .into()
            })
    }

    async fn detach_volume(&self, volume_id: &str) -> Result<(), SnapshotError> {
        self.client()
            .await
            .detach_volume()
            .volume_id(volume_id)
            .instance_id(&self.config.instance_id)
            .send()
            .await
            .map_err(|error| {
                format!(
                    "failed to detach EBS volume {volume_id}: {}",
                    sdk_detail(&error)
                )
            })?;
        self.client()
            .await
            .wait_until_volume_available()
            .volume_ids(volume_id)
            .wait(self.timeout())
            .await
            .map(|_| ())
            .map_err(|error| {
                format!(
                    "failed waiting for detached EBS volume {volume_id}: {}",
                    error_chain(&error)
                )
                .into()
            })
    }

    async fn wait_device(
        &self,
        volume_id: &str,
        requested: &str,
    ) -> Result<PathBuf, SnapshotError> {
        let deadline = Instant::now() + self.timeout();
        loop {
            if let Some(device) = find_nvme_volume(volume_id).await? {
                return wait_partition(device, self.config.partition, deadline).await;
            }
            for candidate in xen_candidates(requested) {
                if tokio::fs::try_exists(&candidate).await? {
                    return wait_partition(candidate, self.config.partition, deadline).await;
                }
            }
            if Instant::now() >= deadline {
                return Err(format!("timed out waiting for local device for {volume_id}").into());
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    async fn mount_clone(&self, device: &Path, mount_path: &Path) -> Result<(), SnapshotError> {
        tokio::fs::create_dir(mount_path).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            tokio::fs::set_permissions(mount_path, std::fs::Permissions::from_mode(0o700)).await?;
        }
        let device = path_string(device)?;
        let mount_path_arg = path_string(mount_path)?;
        let mut args = vec!["-t".to_owned(), self.config.filesystem.clone()];
        if self.config.filesystem == "xfs" {
            args.extend(["-o".to_owned(), "nouuid".to_owned()]);
        }
        args.extend([device, mount_path_arg.clone()]);
        run_host("mount", &args).await?;
        if let Err(error) = async {
            run_host("sync", &["-f".to_owned(), mount_path_arg.clone()]).await?;
            run_host(
                "mount",
                &[
                    "-o".to_owned(),
                    "remount,ro".to_owned(),
                    mount_path_arg.clone(),
                ],
            )
            .await
        }
        .await
        {
            let _ = run_host("umount", &[mount_path_arg]).await;
            return Err(error);
        }
        Ok(())
    }

    /// Attach and mount one lease's volume, and report where the database
    /// directory landed.
    ///
    /// The returned path is built from this agent's own `data_dir` and
    /// `source_mount`, not from anything the daemon said, for the same reason
    /// the LVM executor does it that way.
    async fn materialize(&self, namespace: &str, lease: &str) -> Result<PathBuf, SnapshotError> {
        let data_dir = tokio::fs::canonicalize(&self.data_dir).await?;
        let source_mount = tokio::fs::canonicalize(&self.config.source_mount).await?;
        let relative_data_dir = data_dir
            .strip_prefix(&source_mount)
            .map_err(|_| {
                format!(
                    "database directory '{}' is not below EBS source mount '{}'",
                    data_dir.display(),
                    source_mount.display()
                )
            })?
            .to_path_buf();
        let volume = match self.owned_volumes(namespace, Some(lease)).await?.as_slice() {
            [volume] => volume.id.clone(),
            [] => return Err(format!("no EBS volume is tagged for lease '{lease}'").into()),
            volumes => {
                return Err(format!(
                    "{} EBS volumes are tagged for lease '{lease}', expected one",
                    volumes.len()
                )
                .into())
            }
        };
        tokio::fs::create_dir_all(&self.config.mount_dir).await?;
        let mount_path = self.config.mount_dir.join(lease);
        let materialize = async {
            self.client()
                .await
                .wait_until_volume_available()
                .volume_ids(&volume)
                .wait(self.timeout())
                .await
                .map_err(|error| {
                    format!(
                        "failed waiting for EBS volume {volume} to become available: {}",
                        error_chain(&error)
                    )
                })?;
            let device_name = self.reserve_device().await?;
            self.attach_volume(&volume, &device_name).await?;
            let device = self.wait_device(&volume, &device_name).await?;
            self.mount_clone(&device, &mount_path).await?;
            Ok::<_, SnapshotError>(mount_path.join(&relative_data_dir))
        }
        .await;
        match materialize {
            Ok(root) => Ok(root),
            Err(error) => match self.cleanup(namespace, lease).await {
                Ok(()) => Err(error),
                Err(cleanup_error) => Err(format!(
                    "{error}; EBS agent cleanup also failed and will need reconciliation: {cleanup_error}"
                )
                .into()),
            },
        }
    }

    /// Release one lease's host state: the mount, then the attachment.
    ///
    /// Deleting the volume is the daemon's job and happens after this returns,
    /// which is why the detach has to be complete rather than merely requested.
    async fn cleanup(&self, namespace: &str, lease: &str) -> Result<(), SnapshotError> {
        let mount_path = self.config.mount_dir.join(lease);
        self.unmount(&mount_path).await?;
        for volume in self.owned_volumes(namespace, Some(lease)).await? {
            if volume.attached_here || volume.state == Some(VolumeState::InUse) {
                self.detach_volume(&volume.id).await?;
            }
        }
        Ok(())
    }

    /// Undo everything this database left attached or mounted on this host.
    ///
    /// Mount directories are swept first and independently of the volumes,
    /// because a crash between the detach and the daemon's delete leaves a
    /// directory whose volume no longer exists.
    async fn reconcile(&self, namespace: &str) -> Result<(), SnapshotError> {
        match tokio::fs::read_dir(&self.config.mount_dir).await {
            Ok(mut entries) => {
                while let Some(entry) = entries.next_entry().await? {
                    let name = entry.file_name();
                    let Some(name) = name.to_str() else {
                        continue;
                    };
                    if valid_lease_tag(namespace, name) {
                        self.unmount(&entry.path()).await?;
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        for volume in self.owned_volumes(namespace, None).await? {
            let lease = volume
                .lease
                .as_deref()
                .ok_or_else(|| format!("owned EBS volume {} has no {LEASE_TAG} tag", volume.id))?;
            if !valid_lease_tag(namespace, lease) {
                return Err(format!(
                    "owned EBS volume {} has invalid lease tag '{lease}'",
                    volume.id
                )
                .into());
            }
            if volume.attached_here || volume.state == Some(VolumeState::InUse) {
                self.detach_volume(&volume.id).await?;
            }
        }
        Ok(())
    }

    async fn unmount(&self, mount_path: &Path) -> Result<(), SnapshotError> {
        if is_mountpoint(mount_path).await? {
            run_host("umount", &[path_string(mount_path)?]).await?;
        }
        match tokio::fs::remove_dir(mount_path).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_sdk_ec2::error::ErrorMetadata;
    use aws_sdk_ec2::operation::create_snapshot::CreateSnapshotError;
    use aws_sdk_ec2::types::{InstanceType, Placement};
    use winterbaume_core::MockAws;
    use winterbaume_ec2::Ec2Service;

    /// A container running the deferred arm has the source filesystem and
    /// `/sys/class/block`, and no `/dev/<name>` at all. Verified against a
    /// plain unprivileged container: the sysfs serial reads, the node is
    /// absent. Requiring the node made a check that could succeed fail with
    /// `No such file or directory`, which is what stopped the ECS arm on its
    /// first execution.
    #[tokio::test]
    async fn a_device_node_that_is_absent_keeps_the_name_the_kernel_reported() {
        let missing = Path::new("/dev/nvme-no-such-device-for-this-test");
        assert_eq!(resolve_device(missing).await, missing.to_path_buf());
    }

    /// And the reason canonicalization is there at all still holds: a mount
    /// recorded under `/dev/disk/by-id/...` has to match the `/dev/nvme...`
    /// name `find_nvme_volume` builds out of sysfs.
    #[tokio::test]
    async fn a_symlinked_device_is_still_resolved_to_its_target() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("nvme1n1");
        tokio::fs::write(&target, b"").await.unwrap();
        let link = temp.path().join("nvme-Amazon_Elastic_Block_Store_vol0123");
        tokio::fs::symlink(&target, &link).await.unwrap();
        assert_eq!(
            resolve_device(&link).await,
            tokio::fs::canonicalize(&target).await.unwrap()
        );
        // The comparison the resolution exists to serve.
        assert!(same_device_or_partition(
            &resolve_device(&link).await,
            &tokio::fs::canonicalize(&target).await.unwrap()
        ));
    }

    /// The whole point of `sdk_detail`, pinned.
    ///
    /// A live gate run failed with `"failed to create EBS snapshot: service
    /// error"`, which named neither the permission nor the parameter at fault.
    /// The code and the message are in the error metadata; `Display` does not
    /// reach them. Do not go back to `{error}` here -- this test is what
    /// says the difference is real rather than cosmetic.
    #[test]
    fn an_aws_error_is_reported_by_its_code_and_message() {
        let error = CreateSnapshotError::generic(
            ErrorMetadata::builder()
                .code("UnauthorizedOperation")
                .message("You are not authorized to perform this operation.")
                .build(),
        );
        assert_eq!(
            sdk_detail(&error),
            "UnauthorizedOperation: You are not authorized to perform this operation."
        );
        // The reason the helper exists. This error type's own `Display` keeps
        // the code and drops the message -- and the live failure was worse
        // still, because the daemon formats the `SdkError` that wraps it and
        // *that* renders as the bare words "service error".
        //
        // Asserted as "does not contain the message" rather than against the
        // exact string, which is the SDK's to change.
        let displayed = error.to_string();
        assert!(!displayed.contains("not authorized"), "{displayed}");
    }

    /// A code with no message still beats the `Display`.
    #[test]
    fn an_aws_error_without_a_message_is_reported_by_its_code() {
        let error = CreateSnapshotError::generic(
            ErrorMetadata::builder()
                .code("RequestLimitExceeded")
                .build(),
        );
        assert_eq!(sdk_detail(&error), "RequestLimitExceeded");
    }

    /// What the waiters get, where there is no metadata to read: the wait's own
    /// `Display` says only that the wait ended, and the call it was polling is
    /// one level further down.
    #[test]
    fn a_wrapped_error_is_reported_through_its_whole_source_chain() {
        #[derive(Debug)]
        struct Inner;
        impl std::fmt::Display for Inner {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("the volume is still attaching")
            }
        }
        impl std::error::Error for Inner {}

        #[derive(Debug)]
        struct Outer(Inner);
        impl std::fmt::Display for Outer {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("exceeded max wait time")
            }
        }
        impl std::error::Error for Outer {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }

        assert_eq!(
            error_chain(&Outer(Inner)),
            "exceeded max wait time: the volume is still attaching"
        );
    }

    struct WinterbaumeFixture {
        _temp: tempfile::TempDir,
        provider: EbsProvider,
        agent: std::sync::Arc<EbsAgent>,
        broker: SnapshotAgentBroker,
        pump: tokio::task::JoinHandle<()>,
    }

    impl WinterbaumeFixture {
        /// Stop the in-process agent so a test can observe the daemon half
        /// failing when nothing privileged is listening.
        fn stop_agent(&self) {
            self.broker.close();
            self.pump.abort();
        }

        fn config_device_names(&self) -> Vec<String> {
            self.provider.config.device_names.clone()
        }
    }

    /// Drive the broker the way `run_snapshot_agent` does, without the
    /// transport. This is what makes the delegated cleanup and reconciliation
    /// paths testable: everything but the mount itself is exercised.
    fn spawn_agent(
        broker: SnapshotAgentBroker,
        agent: std::sync::Arc<EbsAgent>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            while let Ok(work) = broker.claim().await {
                let completion = agent.execute(work).await;
                let _ = broker.complete(completion);
            }
        })
    }

    async fn winterbaume_fixture() -> WinterbaumeFixture {
        let mock = MockAws::builder().with_service(Ec2Service::new()).build();
        let shared = aws_config::defaults(BehaviorVersion::latest())
            .http_client(mock.http_client())
            .credentials_provider(mock.credentials_provider())
            .region(Region::new("us-east-1"))
            .load()
            .await;
        let client = Client::new(&shared);

        let instance_id = client
            .run_instances()
            .image_id("ami-yesno-test")
            .instance_type(InstanceType::T3Micro)
            .min_count(1)
            .max_count(1)
            .placement(Placement::builder().availability_zone("us-east-1a").build())
            .send()
            .await
            .expect("seed test instance")
            .instances()
            .first()
            .and_then(|instance| instance.instance_id())
            .expect("seeded instance has an ID")
            .to_owned();
        let source_volume_id = client
            .create_volume()
            .availability_zone("us-east-1a")
            .size(8)
            .volume_type(VolumeType::Gp3)
            .send()
            .await
            .expect("seed source volume")
            .volume_id()
            .expect("seeded volume has an ID")
            .to_owned();

        let temp = tempfile::tempdir().unwrap();
        let config = EbsSnapshotConfig {
            materialization: EbsMaterialization::Local,
            region: "us-east-1".to_owned(),
            volume_id: source_volume_id,
            instance_id,
            availability_zone: "us-east-1a".to_owned(),
            source_mount: temp.path().join("source"),
            mount_dir: temp.path().join("mounts"),
            filesystem: "ext4".to_owned(),
            partition: None,
            device_names: vec!["/dev/sdf".to_owned()],
            operation_timeout_secs: 5,
            resource_tags: BTreeMap::new(),
        };
        let broker = SnapshotAgentBroker::new();
        let provider = EbsProvider::new(temp.path(), &config, Some(broker.clone()));
        assert!(provider.client.set(client.clone()).is_ok());
        let agent = std::sync::Arc::new(EbsAgent::new(temp.path(), &config));
        assert!(agent.client.set(client).is_ok());
        let pump = spawn_agent(broker.clone(), agent.clone());
        WinterbaumeFixture {
            _temp: temp,
            provider,
            agent,
            broker,
            pump,
        }
    }

    async fn create_temporary_resources(
        provider: &EbsProvider,
        namespace: &str,
        suffix: &str,
    ) -> (String, String, EbsCapture) {
        let lease = format!("{namespace}-{suffix}");
        let (snapshot_id, volume_size_gib) = provider
            .create_snapshot(namespace, &lease)
            .await
            .expect("create tagged snapshot through AWS SDK");
        provider
            .wait_snapshot(&snapshot_id)
            .await
            .expect("snapshot waiter consumes completed state");
        let capture = EbsCapture {
            name: lease,
            snapshot_id: snapshot_id.clone(),
            volume_size_gib,
            relative_data_dir: PathBuf::new(),
        };
        let volume_id = provider
            .create_volume(namespace, &capture)
            .await
            .expect("restore tagged volume through AWS SDK");
        provider
            .wait_volume_available(&volume_id)
            .await
            .expect("volume waiter consumes available state");
        (snapshot_id, volume_id, capture)
    }

    #[tokio::test]
    async fn winterbaume_exercises_the_sdk_snapshot_volume_and_cleanup_lifecycle() {
        let fixture = winterbaume_fixture().await;
        let provider = &fixture.provider;

        let namespace = "yesno-snapshot-0123456789abcdef0123456789abcdef";
        let (snapshot_id, volume_id, capture) =
            create_temporary_resources(provider, namespace, "1").await;
        assert_eq!(
            provider.tagged_snapshots(namespace).await.unwrap(),
            [(snapshot_id.clone(), Some(capture.name.clone()))]
        );
        // The attachment is the agent's, not the daemon's: this stands in for
        // the part of materialization that precedes the mount.
        let device = fixture.agent.reserve_device().await.unwrap();
        assert_eq!(device, "/dev/sdf");
        fixture
            .agent
            .attach_volume(&volume_id, &device)
            .await
            .expect("attach restored volume through AWS SDK");
        assert_eq!(
            provider.volume_state(&volume_id).await.unwrap(),
            Some(VolumeState::InUse)
        );
        assert_eq!(
            provider.tagged_volumes(namespace).await.unwrap(),
            [(volume_id.clone(), Some(capture.name.clone()))]
        );

        // The daemon cannot delete an attached volume, so this only succeeds
        // if the delegated detach really happened first.
        provider
            .cleanup_resources(&EbsCleanup {
                namespace: namespace.to_owned(),
                name: capture.name.clone(),
                snapshot_id: snapshot_id.clone(),
                volume_id: Some(volume_id.clone()),
                local: true,
            })
            .await
            .expect("detach and delete temporary EBS resources");
        assert_eq!(provider.volume_state(&volume_id).await.unwrap(), None);
        assert!(!provider.snapshot_exists(&snapshot_id).await.unwrap());
    }

    #[tokio::test]
    async fn a_local_lease_cannot_be_cleaned_up_without_the_agent() {
        let fixture = winterbaume_fixture().await;
        let provider = &fixture.provider;
        let namespace = "yesno-snapshot-0123456789abcdef0123456789abcdef";
        let (snapshot_id, volume_id, capture) =
            create_temporary_resources(provider, namespace, "1").await;
        let device = fixture.agent.reserve_device().await.unwrap();
        fixture
            .agent
            .attach_volume(&volume_id, &device)
            .await
            .unwrap();
        fixture.stop_agent();

        let error = provider
            .cleanup_resources(&EbsCleanup {
                namespace: namespace.to_owned(),
                name: capture.name,
                snapshot_id,
                volume_id: Some(volume_id.clone()),
                local: true,
            })
            .await
            .expect_err("cleanup must not silently skip the privileged half");
        // Whether the queue reports itself closed or the wait simply expires
        // depends on timing; what matters is that the daemon refuses to carry
        // on without the privileged half rather than deleting around it.
        assert!(
            error.to_string().starts_with("snapshot-agent "),
            "unexpected error: {error}"
        );
        // The volume is still attached, and deliberately still there: leaking
        // it for reconciliation beats deleting host state the agent still owns.
        assert_eq!(
            provider.volume_state(&volume_id).await.unwrap(),
            Some(VolumeState::InUse)
        );
    }

    #[tokio::test]
    async fn the_agent_refuses_work_outside_the_database_namespace() {
        let fixture = winterbaume_fixture().await;
        let namespace = "yesno-snapshot-0123456789abcdef0123456789abcdef";
        let foreign = "yesno-snapshot-ffffffffffffffffffffffffffffffff-1";

        let completion = fixture
            .agent
            .execute(pb::SnapshotAgentWork {
                operation_id: b"1".to_vec(),
                operation: pb::SnapshotAgentOperation::Materialize as i32,
                namespace: namespace.to_owned(),
                lease_name: foreign.to_owned(),
            })
            .await;
        assert!(!completion.success);
        assert_eq!(
            completion.error,
            "snapshot agent received a lease outside the database namespace"
        );

        // Capture is a pure cloud call under the barrier and must never be
        // delegated, so the agent rejects it even for a well-formed lease.
        let completion = fixture
            .agent
            .execute(pb::SnapshotAgentWork {
                operation_id: b"2".to_vec(),
                operation: pb::SnapshotAgentOperation::Capture as i32,
                namespace: namespace.to_owned(),
                lease_name: format!("{namespace}-1"),
            })
            .await;
        assert!(!completion.success);
        assert_eq!(
            completion.error,
            "EBS capture belongs to the daemon and must not reach the agent"
        );
    }

    #[tokio::test]
    async fn the_agent_only_attaches_names_from_its_own_configured_pool() {
        let fixture = winterbaume_fixture().await;
        let namespace = "yesno-snapshot-0123456789abcdef0123456789abcdef";
        let (_, volume_id, _) = create_temporary_resources(&fixture.provider, namespace, "1").await;
        let device = fixture.agent.reserve_device().await.unwrap();
        assert!(fixture.config_device_names().contains(&device));
        fixture
            .agent
            .attach_volume(&volume_id, &device)
            .await
            .unwrap();

        // The pool holds one name, and it is now in use.
        let error = fixture
            .agent
            .reserve_device()
            .await
            .expect_err("an exhausted pool must not fall back to an unconfigured name");
        assert_eq!(
            error.to_string(),
            "all configured EBS attachment names are in use"
        );
    }

    #[tokio::test]
    async fn winterbaume_reconciliation_removes_only_the_owned_database_resources() {
        let fixture = winterbaume_fixture().await;
        let provider = &fixture.provider;
        let owned_namespace = "yesno-snapshot-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let foreign_namespace = "yesno-snapshot-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let (owned_snapshot, owned_volume, _owned_capture) =
            create_temporary_resources(provider, owned_namespace, "1").await;
        let device = fixture.agent.reserve_device().await.unwrap();
        fixture
            .agent
            .attach_volume(&owned_volume, &device)
            .await
            .expect("attach abandoned owned volume");
        let (foreign_snapshot, foreign_volume, foreign_capture) =
            create_temporary_resources(provider, foreign_namespace, "1").await;

        provider
            .reconcile(owned_namespace)
            .await
            .expect("reconcile abandoned resources");

        assert_eq!(provider.volume_state(&owned_volume).await.unwrap(), None);
        assert!(!provider.snapshot_exists(&owned_snapshot).await.unwrap());
        assert_eq!(
            provider.volume_state(&foreign_volume).await.unwrap(),
            Some(VolumeState::Available)
        );
        assert!(provider.snapshot_exists(&foreign_snapshot).await.unwrap());

        provider
            .cleanup_resources(&EbsCleanup {
                namespace: foreign_namespace.to_owned(),
                name: foreign_capture.name,
                snapshot_id: foreign_snapshot,
                volume_id: Some(foreign_volume),
                local: true,
            })
            .await
            .expect("clean up preserved foreign resources");
    }

    #[tokio::test]
    async fn winterbaume_deferred_lease_keeps_only_the_server_owned_snapshot() {
        let fixture = winterbaume_fixture().await;
        let mut config = fixture.provider.config.clone();
        config.materialization = EbsMaterialization::Deferred;
        config.instance_id.clear();
        config.availability_zone.clear();
        config.mount_dir = PathBuf::new();
        config.device_names.clear();
        let provider = EbsProvider::new(fixture._temp.path(), &config, None);
        assert!(provider
            .client
            .set(fixture.provider.client().await.clone())
            .is_ok());
        let namespace = "yesno-snapshot-cccccccccccccccccccccccccccccccc";
        let name = format!("{namespace}-1");
        let (snapshot_id, volume_size_gib) = provider
            .create_snapshot(namespace, &name)
            .await
            .expect("create provisional EBS snapshot through AWS SDK");
        provider.wait_snapshot(&snapshot_id).await.unwrap();
        let created = provider
            .materialize_deferred(EbsCapture {
                name,
                snapshot_id: snapshot_id.clone(),
                volume_size_gib,
                relative_data_dir: PathBuf::from("database"),
            })
            .unwrap();
        let deferred = created.deferred_ebs.as_ref().unwrap();
        assert_eq!(deferred.snapshot_id, snapshot_id);
        assert_eq!(deferred.source_subpath, "database");
        assert_eq!(deferred.volume_size_gib, 8);
        assert!(provider.tagged_volumes(namespace).await.unwrap().is_empty());
        provider.cleanup(&created.cleanup).await.unwrap();
        assert!(!provider.snapshot_exists(&snapshot_id).await.unwrap());
    }

    #[test]
    fn validates_aws_ids_and_normalizes_nvme_serials() {
        assert_eq!(
            require_id("snap-0123abcdef".into(), "snap-").unwrap(),
            "snap-0123abcdef"
        );
        assert!(require_id("snapshot-0123".into(), "snap-").is_err());
        assert_eq!(normalize_volume_id("vol-0123abcdef"), "vol0123abcdef");
    }

    #[test]
    fn selects_exact_resource_tags_and_rejects_unsafe_lease_names() {
        let tags = [
            Tag::builder().key("other").value("lease-0").build(),
            Tag::builder().key(LEASE_TAG).value("lease-1").build(),
        ];
        assert_eq!(tag_value(&tags, LEASE_TAG), Some("lease-1"));
        assert_eq!(tag_value(&tags, "missing"), None);
        let custom = BTreeMap::from([("yesno:e2e-run".to_owned(), "run-1".to_owned())]);
        let specification = tag_specification(
            ResourceType::Snapshot,
            "yesno-snapshot-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "yesno-snapshot-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-1",
            &custom,
        );
        assert_eq!(
            tag_value(specification.tags(), "yesno:e2e-run"),
            Some("run-1")
        );
        assert!(valid_lease_tag(
            "yesno-snapshot-abcd",
            "yesno-snapshot-abcd-1"
        ));
        assert!(!valid_lease_tag(
            "yesno-snapshot-abcd",
            "yesno-snapshot-abcd-../../foreign"
        ));
    }

    #[test]
    fn derives_xen_and_partition_device_names() {
        assert_eq!(
            xen_candidates("/dev/sdf"),
            [PathBuf::from("/dev/sdf"), PathBuf::from("/dev/xvdf")]
        );
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let device = runtime
            .block_on(wait_partition(
                PathBuf::from("/dev/null"),
                None,
                Instant::now(),
            ))
            .unwrap();
        assert_eq!(device, Path::new("/dev/null"));
        assert!(same_device_or_partition(
            Path::new("/dev/nvme1n1p2"),
            Path::new("/dev/nvme1n1")
        ));
        assert!(same_device_or_partition(
            Path::new("/dev/xvdf1"),
            Path::new("/dev/xvdf")
        ));
        assert!(!same_device_or_partition(
            Path::new("/dev/nvme1n10"),
            Path::new("/dev/nvme1n1")
        ));
    }
}
