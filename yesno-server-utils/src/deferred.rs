//! Archiver-owned materialization of provisional server snapshots.
//!
//! The server owns the EBS snapshot lifetime. This module only starts a
//! short-lived workload that restores and reads that snapshot, stages the
//! bounded database file set into storage shared with the parent archiver, and
//! exits. The parent archiver remains the only object-store publisher.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use aws_config::BehaviorVersion;
use aws_sdk_ecs::client::Waiters as _;
use aws_sdk_ecs::config::Region;
use aws_sdk_ecs::types::{
    AssignPublicIp, AwsVpcConfiguration, ContainerOverride, EbsResourceType, EbsTagSpecification,
    LaunchType, NetworkConfiguration, Tag, TaskFilesystemType, TaskManagedEbsVolumeConfiguration,
    TaskManagedEbsVolumeTerminationPolicy, TaskOverride, TaskVolumeConfiguration,
};
use aws_sdk_ecs::Client as EcsClient;
use kube::api::{ApiResource, DeleteParams, DynamicObject, PostParams, PropagationPolicy};
use kube::core::GroupVersionKind;
use kube::{Api, Client as KubeClient};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use yesno_server::control::pb::DeferredEbsSnapshot;

use crate::archive::ArchiveError;

const DATABASE_TAG: &str = "yesno:database";
const LEASE_TAG: &str = "yesno:lease";

#[derive(Clone, Debug)]
pub(crate) struct EcsConfig {
    pub cluster: String,
    pub task_definition: String,
    pub container_name: String,
    pub volume_name: String,
    pub infrastructure_role_arn: String,
    pub subnets: Vec<String>,
    pub security_groups: Vec<String>,
    pub assign_public_ip: bool,
    pub source_path: PathBuf,
    pub staging_path: PathBuf,
    pub timeout: Duration,
}

#[derive(Clone, Debug)]
pub(crate) struct EksConfig {
    pub namespace: String,
    pub image: String,
    pub service_account: Option<String>,
    pub snapshot_class: String,
    pub storage_class: String,
    pub csi_driver: String,
    pub staging_claim: String,
    pub node_selector: BTreeMap<String, String>,
    pub source_path: PathBuf,
    pub staging_path: PathBuf,
    pub timeout: Duration,
}

#[derive(Clone, Debug)]
pub(crate) enum DeferredMaterializer {
    Ecs(EcsConfig),
    Eks(EksConfig),
}

impl DeferredMaterializer {
    pub(crate) async fn materialize(
        &self,
        snapshot: &DeferredEbsSnapshot,
        lease_id: &[u8],
    ) -> Result<PathBuf, ArchiveError> {
        validate_snapshot(snapshot)?;
        let token = lease_token(lease_id);
        match self {
            Self::Ecs(config) => materialize_ecs(config, snapshot, &token).await,
            Self::Eks(config) => materialize_eks(config, snapshot, &token).await,
        }
    }
}

fn validate_snapshot(snapshot: &DeferredEbsSnapshot) -> Result<(), ArchiveError> {
    if !snapshot.snapshot_id.starts_with("snap-")
        || snapshot.snapshot_id.len() == "snap-".len()
        || !snapshot.snapshot_id["snap-".len()..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("server returned an invalid deferred EBS snapshot ID".into());
    }
    if snapshot.region.is_empty()
        || !matches!(snapshot.filesystem.as_str(), "ext4" | "xfs")
        || snapshot.volume_size_gib == 0
    {
        return Err("server returned an incomplete deferred EBS snapshot".into());
    }
    let subpath = Path::new(&snapshot.source_subpath);
    if subpath.is_absolute()
        || subpath
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::RootDir))
    {
        return Err("server returned an unsafe deferred EBS source subpath".into());
    }
    Ok(())
}

fn lease_token(lease_id: &[u8]) -> String {
    let digest = Sha256::digest(lease_id);
    let mut token = String::with_capacity(32);
    for byte in &digest[..16] {
        use std::fmt::Write as _;
        write!(&mut token, "{byte:02x}").expect("writing to String cannot fail");
    }
    token
}

fn ecs_tag(key: &str, value: &str) -> Tag {
    Tag::builder().key(key).value(value).build()
}

async fn materialize_ecs(
    config: &EcsConfig,
    snapshot: &DeferredEbsSnapshot,
    token: &str,
) -> Result<PathBuf, ArchiveError> {
    let shared = aws_config::defaults(BehaviorVersion::latest())
        .region(Region::new(snapshot.region.clone()))
        .load()
        .await;
    let client = EcsClient::new(&shared);
    materialize_ecs_with_client(&client, config, snapshot, token).await
}

async fn materialize_ecs_with_client(
    client: &EcsClient,
    config: &EcsConfig,
    snapshot: &DeferredEbsSnapshot,
    token: &str,
) -> Result<PathBuf, ArchiveError> {
    let source = config.source_path.join(&snapshot.source_subpath);
    let target = config.staging_path.join(token);
    let container_override = ContainerOverride::builder()
        .name(&config.container_name)
        .set_command(Some(vec![
            "yesno-snapshot-stage".to_owned(),
            "--source".to_owned(),
            path_string(&source)?,
            "--target".to_owned(),
            path_string(&target)?,
        ]))
        .build();
    let network = NetworkConfiguration::builder()
        .awsvpc_configuration(
            AwsVpcConfiguration::builder()
                .set_subnets(Some(config.subnets.clone()))
                .set_security_groups(Some(config.security_groups.clone()))
                .assign_public_ip(if config.assign_public_ip {
                    AssignPublicIp::Enabled
                } else {
                    AssignPublicIp::Disabled
                })
                .build()?,
        )
        .build();
    let volume_tags = EbsTagSpecification::builder()
        .resource_type(EbsResourceType::Volume)
        .tags(ecs_tag(DATABASE_TAG, token))
        .tags(ecs_tag(LEASE_TAG, token))
        .build()?;
    let volume = TaskVolumeConfiguration::builder()
        .name(&config.volume_name)
        .managed_ebs_volume(
            TaskManagedEbsVolumeConfiguration::builder()
                .snapshot_id(&snapshot.snapshot_id)
                .volume_type("gp3")
                .filesystem_type(TaskFilesystemType::from(snapshot.filesystem.as_str()))
                .role_arn(&config.infrastructure_role_arn)
                .termination_policy(
                    TaskManagedEbsVolumeTerminationPolicy::builder()
                        .delete_on_termination(true)
                        .build()?,
                )
                .tag_specifications(volume_tags)
                .build()?,
        )
        .build()?;
    let output = client
        .run_task()
        .cluster(&config.cluster)
        .task_definition(&config.task_definition)
        .count(1)
        .launch_type(LaunchType::Fargate)
        .started_by(token)
        .group(format!("yesno-{token}"))
        .network_configuration(network)
        .overrides(
            TaskOverride::builder()
                .container_overrides(container_override)
                .build(),
        )
        .tags(ecs_tag(DATABASE_TAG, token))
        .tags(ecs_tag(LEASE_TAG, token))
        .volume_configurations(volume)
        .send()
        .await
        .map_err(|error| format!("failed to launch deferred EBS ECS task: {error}"))?;
    if !output.failures().is_empty() {
        return Err(format!(
            "ECS refused deferred EBS task launch: {}",
            output
                .failures()
                .iter()
                .map(|failure| failure.reason().unwrap_or("unknown reason"))
                .collect::<Vec<_>>()
                .join(", ")
        )
        .into());
    }
    let task_arn = output
        .tasks()
        .first()
        .and_then(|task| task.task_arn())
        .ok_or("ECS returned no deferred materializer task ARN")?
        .to_owned();
    let result = wait_ecs_task(client, config, &task_arn).await;
    if result.is_err() {
        let _ = client
            .stop_task()
            .cluster(&config.cluster)
            .task(&task_arn)
            .reason("yesno deferred snapshot materialization failed")
            .send()
            .await;
    }
    result?;
    Ok(target)
}

async fn wait_ecs_task(
    client: &EcsClient,
    config: &EcsConfig,
    task_arn: &str,
) -> Result<(), ArchiveError> {
    client
        .wait_until_tasks_stopped()
        .cluster(&config.cluster)
        .tasks(task_arn)
        .wait(config.timeout)
        .await
        .map_err(|error| format!("failed waiting for ECS materializer task: {error}"))?;
    let output = client
        .describe_tasks()
        .cluster(&config.cluster)
        .tasks(task_arn)
        .send()
        .await
        .map_err(|error| format!("failed to describe ECS materializer task: {error}"))?;
    let task = output
        .tasks()
        .first()
        .ok_or("ECS materializer task disappeared before validation")?;
    let container = task
        .containers()
        .iter()
        .find(|container| container.name() == Some(config.container_name.as_str()))
        .ok_or("ECS materializer task has no configured container")?;
    match container.exit_code() {
        Some(0) => Ok(()),
        Some(code) => Err(format!(
            "ECS materializer container exited with {code}: {}",
            container.reason().unwrap_or("no reason reported")
        )
        .into()),
        None => Err(format!(
            "ECS materializer stopped without an exit code: {}",
            task.stopped_reason().unwrap_or("no reason reported")
        )
        .into()),
    }
}

fn api_resource(group: &str, version: &str, kind: &str, plural: &str) -> ApiResource {
    ApiResource::from_gvk_with_plural(&GroupVersionKind::gvk(group, version, kind), plural)
}

fn dynamic(value: Value) -> Result<DynamicObject, ArchiveError> {
    serde_json::from_value(value).map_err(|error| {
        format!("cannot construct Kubernetes materializer resource: {error}").into()
    })
}

fn eks_resources(
    config: &EksConfig,
    snapshot: &DeferredEbsSnapshot,
    token: &str,
) -> Result<[DynamicObject; 4], ArchiveError> {
    let name = format!("yesno-{token}");
    let labels = json!({"app.kubernetes.io/managed-by": "yesno-archive", "yesno.io/lease": token});
    let snapshot_content = dynamic(json!({
        "apiVersion": "snapshot.storage.k8s.io/v1",
        "kind": "VolumeSnapshotContent",
        "metadata": {"name": name, "labels": labels},
        "spec": {
            "deletionPolicy": "Retain",
            "driver": config.csi_driver,
            "source": {"snapshotHandle": snapshot.snapshot_id},
            "sourceVolumeMode": "Filesystem",
            "volumeSnapshotClassName": config.snapshot_class,
            "volumeSnapshotRef": {"name": name, "namespace": config.namespace}
        }
    }))?;
    let volume_snapshot = dynamic(json!({
        "apiVersion": "snapshot.storage.k8s.io/v1",
        "kind": "VolumeSnapshot",
        "metadata": {"name": name, "namespace": config.namespace, "labels": labels},
        "spec": {
            "volumeSnapshotClassName": config.snapshot_class,
            "source": {"volumeSnapshotContentName": name}
        }
    }))?;
    let claim = dynamic(json!({
        "apiVersion": "v1",
        "kind": "PersistentVolumeClaim",
        "metadata": {"name": name, "namespace": config.namespace, "labels": labels},
        "spec": {
            "accessModes": ["ReadWriteOnce"],
            "storageClassName": config.storage_class,
            "resources": {"requests": {"storage": format!("{}Gi", snapshot.volume_size_gib)}},
            "dataSource": {
                "name": name,
                "kind": "VolumeSnapshot",
                "apiGroup": "snapshot.storage.k8s.io"
            }
        }
    }))?;
    let mut pod_spec = json!({
        "restartPolicy": "Never",
        "containers": [{
            "name": "materializer",
            "image": config.image,
            "command": [
                "yesno-snapshot-stage",
                "--source", path_string(&config.source_path.join(&snapshot.source_subpath))?,
                "--target", path_string(&config.staging_path.join(token))?
            ],
            "volumeMounts": [
                {"name": "source", "mountPath": config.source_path, "readOnly": true},
                {"name": "staging", "mountPath": config.staging_path}
            ]
        }],
        "volumes": [
            {"name": "source", "persistentVolumeClaim": {"claimName": name, "readOnly": true}},
            {"name": "staging", "persistentVolumeClaim": {"claimName": config.staging_claim}}
        ]
    });
    if let Some(service_account) = &config.service_account {
        pod_spec["serviceAccountName"] = json!(service_account);
    }
    if !config.node_selector.is_empty() {
        pod_spec["nodeSelector"] = json!(config.node_selector);
    }
    let job = dynamic(json!({
        "apiVersion": "batch/v1",
        "kind": "Job",
        "metadata": {"name": name, "namespace": config.namespace, "labels": labels},
        "spec": {
            "backoffLimit": 0,
            "ttlSecondsAfterFinished": 300,
            "template": {"metadata": {"labels": labels}, "spec": pod_spec}
        }
    }))?;
    Ok([snapshot_content, volume_snapshot, claim, job])
}

async fn materialize_eks(
    config: &EksConfig,
    snapshot: &DeferredEbsSnapshot,
    token: &str,
) -> Result<PathBuf, ArchiveError> {
    // Before the client, because building one negotiates TLS and Rustls
    // refuses to guess a provider. This crate's graph carries both -- `aws-lc-rs`
    // through the AWS SDK's HTTPS client, `ring` through `kube`'s default
    // features -- so without this the archiver panics at its first Kubernetes
    // request rather than returning an error. Installation is process-global and
    // first-writer-wins; `transport.rs` makes the same call for the Winterbaume
    // side, and either winning is fine.
    //
    // A live EKS run panicked here on 2026-09-02. The note beside the
    // structural test said `yesno-archive` "reaches TLS only through `connect`
    // and never touches Rustls itself" -- true when it was written, and made
    // false by the deferred EKS materializer, which is a second TLS path in the
    // same binary.
    yesno_server::tls::install_crypto_provider();
    let client = KubeClient::try_default()
        .await
        .map_err(|error| format!("cannot configure Kubernetes client: {error}"))?;
    let [content, volume_snapshot, claim, job] = eks_resources(config, snapshot, token)?;
    let name = format!("yesno-{token}");
    let contents: Api<DynamicObject> = Api::all_with(
        client.clone(),
        &api_resource(
            "snapshot.storage.k8s.io",
            "v1",
            "VolumeSnapshotContent",
            "volumesnapshotcontents",
        ),
    );
    let snapshots: Api<DynamicObject> = Api::namespaced_with(
        client.clone(),
        &config.namespace,
        &api_resource(
            "snapshot.storage.k8s.io",
            "v1",
            "VolumeSnapshot",
            "volumesnapshots",
        ),
    );
    let claims: Api<DynamicObject> = Api::namespaced_with(
        client.clone(),
        &config.namespace,
        &api_resource("", "v1", "PersistentVolumeClaim", "persistentvolumeclaims"),
    );
    let jobs: Api<DynamicObject> = Api::namespaced_with(
        client,
        &config.namespace,
        &api_resource("batch", "v1", "Job", "jobs"),
    );
    let result = async {
        contents.create(&PostParams::default(), &content).await?;
        snapshots
            .create(&PostParams::default(), &volume_snapshot)
            .await?;
        wait_json(
            &snapshots,
            &name,
            "the VolumeSnapshot to become ready",
            config.timeout,
            |value| value.pointer("/status/readyToUse") == Some(&Value::Bool(true)),
        )
        .await?;
        claims.create(&PostParams::default(), &claim).await?;
        jobs.create(&PostParams::default(), &job).await?;
        wait_job(&jobs, &name, config.timeout).await
    }
    .await;
    let cleanup = cleanup_eks(&jobs, &claims, &snapshots, &contents, &name).await;
    match (result, cleanup) {
        (Ok(()), Ok(())) => {}
        (Ok(()), Err(error)) | (Err(error), Ok(())) => return Err(error),
        (Err(error), Err(cleanup_error)) => {
            return Err(format!(
                "{error}; Kubernetes materializer cleanup also failed: {cleanup_error}"
            )
            .into());
        }
    }
    Ok(config.staging_path.join(token))
}

/// `what` describes the condition, not the object, and it is the only thing
/// that distinguishes this timeout from the Job's. Both waits key on the same
/// name, so "timed out waiting for 'yesno-<token>'" would leave an operator --
/// or a gate -- unable to tell a snapshot that never became ready from a Job
/// that never finished.
/// An object's `status`, or a note that it has none yet.
///
/// `status` only. A whole `DynamicObject` carries the spec the archiver just
/// wrote and the managed fields the API server added, neither of which says
/// anything about why the object is not ready, and both of which are long
/// enough to bury what does.
fn status_of(value: &Value) -> String {
    match value.pointer("/status") {
        Some(status) => status.to_string(),
        None => "absent".to_owned(),
    }
}

async fn wait_json<F>(
    api: &Api<DynamicObject>,
    name: &str,
    what: &str,
    timeout: Duration,
    ready: F,
) -> Result<(), ArchiveError>
where
    F: Fn(&Value) -> bool,
{
    let deadline = Instant::now() + timeout;
    loop {
        let object = api.get(name).await?;
        let value = serde_json::to_value(object)?;
        if ready(&value) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            // With the object's own status. A snapshot that never becomes
            // ready explains itself in `status.error.message`, and without it
            // the caller is told which object stalled but not why -- which is
            // one round trip short of a diagnosis when the round trip costs a
            // provisioned cluster.
            return Err(format!(
                "timed out waiting for {what} '{name}'; its status is {}",
                status_of(&value)
            )
            .into());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn wait_job(
    jobs: &Api<DynamicObject>,
    name: &str,
    timeout: Duration,
) -> Result<(), ArchiveError> {
    let deadline = Instant::now() + timeout;
    loop {
        let value = serde_json::to_value(jobs.get(name).await?)?;
        if value.pointer("/status/succeeded").and_then(Value::as_u64) == Some(1) {
            return Ok(());
        }
        if value
            .pointer("/status/failed")
            .and_then(Value::as_u64)
            .is_some_and(|failed| failed > 0)
        {
            return Err(format!("Kubernetes materializer Job '{name}' failed").into());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "timed out waiting for Kubernetes Job '{name}'; its status is {}",
                status_of(&value)
            )
            .into());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// How the Job is deleted, which is not how everything else is deleted.
///
/// **Background propagation, explicitly.** The API server's default garbage
/// collection for a Job *orphans* its pods, and an orphaned pod holds the
/// `kubernetes.io/pvc-protection` finalizer on the claim it mounted. The claim
/// then cannot finish deleting, so the `PersistentVolume` is never released and
/// **the EBS volume the CSI driver provisioned is never deleted**.
///
/// That is a leak that outlives the cluster: the volume is not Terraform's,
/// so destroying the stack does not remove it either. Two live runs on
/// 2026-09-05 ended with a completed pod, a claim still bound, and one
/// orphaned volume each -- and because the gate's precondition counts volumes
/// by a namespace that is not per-run, each leak also failed the *next* run.
///
/// Do not "simplify" this back to `DeleteParams::default()`.
fn job_params() -> DeleteParams {
    DeleteParams {
        propagation_policy: Some(PropagationPolicy::Background),
        ..DeleteParams::default()
    }
}

async fn cleanup_eks(
    jobs: &Api<DynamicObject>,
    claims: &Api<DynamicObject>,
    snapshots: &Api<DynamicObject>,
    contents: &Api<DynamicObject>,
    name: &str,
) -> Result<(), ArchiveError> {
    let params = DeleteParams::default();
    delete_if_present(jobs, name, &job_params()).await?;
    delete_if_present(claims, name, &params).await?;
    delete_if_present(snapshots, name, &params).await?;
    delete_if_present(contents, name, &params).await
}

async fn delete_if_present(
    api: &Api<DynamicObject>,
    name: &str,
    params: &DeleteParams,
) -> Result<(), ArchiveError> {
    match api.delete(name, params).await {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(response)) if response.code == 404 => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn path_string(path: &Path) -> Result<String, ArchiveError> {
    path.to_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| format!("materializer path '{}' is not UTF-8", path.display()).into())
}

#[cfg(test)]
mod tests {

    /// The Job is the one object whose deletion must propagate.
    ///
    /// Orphaning its pod leaves the `pvc-protection` finalizer on the claim,
    /// which pins the `PersistentVolume`, which pins the EBS volume the CSI
    /// driver created -- a leak that survives `terraform destroy`, because that
    /// volume is not Terraform's. A default `DeleteParams` orphans.
    #[test]
    fn the_job_is_deleted_with_its_pods() {
        assert_eq!(
            job_params().propagation_policy,
            Some(PropagationPolicy::Background)
        );
        // The others have no dependents worth propagating to, and are left at
        // the default so this stays a statement about the Job.
        assert_eq!(DeleteParams::default().propagation_policy, None);
    }

    use super::*;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};

    use aws_sdk_ecs::types::{Compatibility, ContainerDefinition, Volume};
    use winterbaume_core::{MockAws, MockRequest, MockResponse, MockService};
    use winterbaume_ecs::EcsService;

    struct RecordingEcs {
        inner: EcsService,
        run_task_bodies: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    impl MockService for RecordingEcs {
        fn service_name(&self) -> &str {
            self.inner.service_name()
        }

        fn url_patterns(&self) -> Vec<&str> {
            self.inner.url_patterns()
        }

        fn handle(
            &self,
            request: MockRequest,
        ) -> Pin<Box<dyn Future<Output = MockResponse> + Send + '_>> {
            if request
                .headers
                .get("x-amz-target")
                .and_then(|value| value.to_str().ok())
                .is_some_and(|target| target.ends_with(".RunTask"))
            {
                self.run_task_bodies
                    .lock()
                    .unwrap()
                    .push(request.body.to_vec());
            }
            self.inner.handle(request)
        }
    }

    fn snapshot() -> DeferredEbsSnapshot {
        DeferredEbsSnapshot {
            snapshot_id: "snap-0123456789abcdef0".to_owned(),
            region: "us-east-1".to_owned(),
            filesystem: "ext4".to_owned(),
            source_subpath: "data".to_owned(),
            volume_size_gib: 20,
        }
    }

    #[test]
    fn eks_resources_retain_the_server_owned_snapshot_and_mount_a_restored_pvc() {
        let config = EksConfig {
            namespace: "archive".to_owned(),
            image: "yesno:test".to_owned(),
            service_account: Some("archive".to_owned()),
            snapshot_class: "ebs-snapshots".to_owned(),
            storage_class: "gp3".to_owned(),
            csi_driver: "ebs.csi.aws.com".to_owned(),
            staging_claim: "archive-staging".to_owned(),
            node_selector: BTreeMap::from([(
                "eks.amazonaws.com/compute-type".to_owned(),
                "ec2".to_owned(),
            )]),
            source_path: "/snapshot".into(),
            staging_path: "/staging".into(),
            timeout: Duration::from_secs(30),
        };
        let [content, volume_snapshot, claim, job] =
            eks_resources(&config, &snapshot(), "0123456789abcdef").unwrap();
        let content = serde_json::to_value(content).unwrap();
        assert_eq!(
            content.pointer("/spec/deletionPolicy"),
            Some(&json!("Retain"))
        );
        assert_eq!(
            content.pointer("/spec/source/snapshotHandle"),
            Some(&json!("snap-0123456789abcdef0"))
        );
        let volume_snapshot = serde_json::to_value(volume_snapshot).unwrap();
        assert_eq!(
            volume_snapshot.pointer("/spec/source/volumeSnapshotContentName"),
            Some(&json!("yesno-0123456789abcdef"))
        );
        let claim = serde_json::to_value(claim).unwrap();
        assert_eq!(
            claim.pointer("/spec/resources/requests/storage"),
            Some(&json!("20Gi"))
        );
        let job = serde_json::to_value(job).unwrap();
        assert_eq!(
            job.pointer("/spec/template/spec/volumes/0/persistentVolumeClaim/claimName"),
            Some(&json!("yesno-0123456789abcdef"))
        );
        assert_eq!(
            job.pointer("/spec/template/spec/nodeSelector/eks.amazonaws.com~1compute-type"),
            Some(&json!("ec2"))
        );
    }

    #[test]
    fn provisional_snapshot_validation_rejects_parent_traversal() {
        let mut snapshot = snapshot();
        snapshot.source_subpath = "../live".to_owned();
        assert!(validate_snapshot(&snapshot).is_err());
    }

    #[tokio::test]
    async fn winterbaume_records_the_fargate_snapshot_volume_request() {
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let mock = MockAws::builder()
            .with_service(RecordingEcs {
                inner: EcsService::new(),
                run_task_bodies: bodies.clone(),
            })
            .build();
        let shared = aws_config::defaults(BehaviorVersion::latest())
            .http_client(mock.http_client())
            .credentials_provider(mock.credentials_provider())
            .region(Region::new("us-east-1"))
            .load()
            .await;
        let client = EcsClient::new(&shared);
        client
            .create_cluster()
            .cluster_name("archive")
            .send()
            .await
            .unwrap();
        client
            .register_task_definition()
            .family("archive")
            .requires_compatibilities(Compatibility::Fargate)
            .cpu("256")
            .memory("512")
            .container_definitions(
                ContainerDefinition::builder()
                    .name("archive")
                    .image("yesno:test")
                    .essential(true)
                    .build(),
            )
            .volumes(
                Volume::builder()
                    .name("snapshot")
                    .configured_at_launch(true)
                    .build(),
            )
            .send()
            .await
            .unwrap();
        let config = EcsConfig {
            cluster: "archive".to_owned(),
            task_definition: "archive".to_owned(),
            container_name: "archive".to_owned(),
            volume_name: "snapshot".to_owned(),
            infrastructure_role_arn: "arn:aws:iam::123456789012:role/ecsInfrastructureRole"
                .to_owned(),
            subnets: vec!["subnet-0123456789abcdef0".to_owned()],
            security_groups: vec!["sg-0123456789abcdef0".to_owned()],
            assign_public_ip: false,
            source_path: "/snapshot".into(),
            staging_path: "/staging".into(),
            timeout: Duration::from_secs(5),
        };
        let run_client = client.clone();
        let run_config = config.clone();
        let run_snapshot = snapshot();
        let task = tokio::spawn(async move {
            materialize_ecs_with_client(
                &run_client,
                &run_config,
                &run_snapshot,
                "0123456789abcdef0123456789abcdef",
            )
            .await
        });
        let task_arn = loop {
            let tasks = client.list_tasks().cluster("archive").send().await.unwrap();
            if let Some(task_arn) = tasks.task_arns().first() {
                break task_arn.clone();
            }
            tokio::task::yield_now().await;
        };
        client
            .stop_task()
            .cluster("archive")
            .task(task_arn)
            .send()
            .await
            .unwrap();
        let error = task
            .await
            .unwrap()
            .expect_err("Winterbaume stops without a container exit code");
        assert!(error.to_string().contains("without an exit code"));

        let bodies = bodies.lock().unwrap();
        assert_eq!(bodies.len(), 1);
        winterbaume_ecs::wire::deserialize_run_task_request(&bodies[0])
            .expect("Winterbaume parses the production RunTask body");
        let request: Value = serde_json::from_slice(&bodies[0]).unwrap();
        assert_eq!(request.pointer("/launchType"), Some(&json!("FARGATE")));
        assert_eq!(
            request.pointer("/volumeConfigurations/0/managedEBSVolume/snapshotId"),
            Some(&json!("snap-0123456789abcdef0"))
        );
        assert_eq!(
            request.pointer("/volumeConfigurations/0/managedEBSVolume/filesystemType"),
            Some(&json!("ext4"))
        );
        assert_eq!(
            request.pointer("/volumeConfigurations/0/managedEBSVolume/roleArn"),
            Some(&json!(
                "arn:aws:iam::123456789012:role/ecsInfrastructureRole"
            ))
        );
        assert_eq!(
            request.pointer(
                "/volumeConfigurations/0/managedEBSVolume/terminationPolicy/deleteOnTermination"
            ),
            Some(&json!(true))
        );
        assert_eq!(
            request.pointer("/overrides/containerOverrides/0/command/0"),
            Some(&json!("yesno-snapshot-stage"))
        );
        assert_eq!(
            request.pointer("/networkConfiguration/awsvpcConfiguration/subnets/0"),
            Some(&json!("subnet-0123456789abcdef0"))
        );
    }
}
