use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::{future::join_all, StreamExt};
use jiff::Timestamp;
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::{
    ConfigMap, PersistentVolume, PersistentVolumeClaim, Pod, Secret, Service,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};
use kube::api::{Api, DeleteParams, ListParams, Patch, PatchParams};
use kube::core::DynamicObject;
use kube::runtime::controller::{Action, Controller};
use kube::runtime::watcher;
use kube::{Client, ResourceExt};
use serde_json::json;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity};
use yesno_server::control::pb::{
    control_plane_client::ControlPlaneClient, GetSnapshotRequest, PromoteRequest, Role,
    StateSnapshot,
};

use crate::api::{
    FailoverStage, FailoverStatus, SnapshotBackend, YesnoCluster, YesnoClusterPhase,
    YesnoClusterStatus,
};
use crate::resources::{
    base_name, certificate_api_resource, client_certificate, client_certificate_name, config_map,
    config_source, data_claim_name, deployment, instance_certificate, instance_certificate_name,
    instance_name, instance_service, owner, persistent_volume_claim, read_only_service,
    read_write_service, ConfigSource, InstanceRole, TlsTopology, FIELD_MANAGER, INSTANCE_LABEL,
};

#[derive(Clone)]
pub struct Context {
    client: Client,
}

impl Context {
    pub fn new(client: Client) -> Self {
        Self { client }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Kubernetes API request failed: {0}")]
    Kube(#[from] kube::Error),
    #[error("YesnoCluster `{0}` has no namespace")]
    MissingNamespace(String),
    #[error("control-plane request failed: {0}")]
    Control(String),
}

#[derive(Clone, Debug)]
struct ObservedInstance {
    deployment: Option<Deployment>,
    snapshot: Option<StateSnapshot>,
}

#[derive(Clone, Debug)]
struct TlsClientMaterial {
    ca: Vec<u8>,
    cert: Vec<u8>,
    key: Vec<u8>,
}

#[derive(Clone, Debug)]
struct ManagedTls {
    topology: TlsTopology,
    client: TlsClientMaterial,
}

#[derive(Clone, Copy)]
struct ResolvedConfig<'a> {
    secret_identity: Option<&'a str>,
    managed_tls: Option<&'a ManagedTls>,
    volumes: &'a [VolumeBinding],
}

/// The CSI driver whose volume handle is an EBS volume id.
const EBS_CSI_DRIVER: &str = "ebs.csi.aws.com";

/// Condition reporting whether the configured snapshot backend is in effect.
const SNAPSHOT_CONDITION: &str = "SnapshotBackendReady";

/// One instance's data volume, as far as Kubernetes has resolved it.
#[derive(Clone, Debug, PartialEq, Eq)]
enum VolumeBinding {
    /// The claim has not bound to a PersistentVolume yet, or the volume it
    /// bound to has not been observed. Both are ordinary startup states.
    Pending,
    /// Resolution cannot succeed, for the reason carried here. The named
    /// backend will not take effect for this instance until it changes.
    Unusable(String),
    /// Bound, and backed by this EBS volume.
    Bound(String),
}

impl VolumeBinding {
    fn volume_id(&self) -> Option<&str> {
        match self {
            Self::Bound(volume_id) => Some(volume_id),
            _ => None,
        }
    }
}

#[derive(Clone)]
struct StatusView {
    phase: YesnoClusterPhase,
    ready_instances: i32,
    primary_instance: i32,
    primary_term: i64,
    promotion_count: i64,
    primary_unavailable_since_millis: Option<i64>,
    failover: Option<FailoverStatus>,
    reason: &'static str,
    message: String,
}

pub async fn run(client: Client) {
    let clusters = Api::<YesnoCluster>::all(client.clone());
    let deployments = Api::<Deployment>::all(client.clone());
    let services = Api::<Service>::all(client.clone());
    let config_maps = Api::<ConfigMap>::all(client.clone());
    let context = Arc::new(Context::new(client));

    Controller::new(clusters, watcher::Config::default())
        .owns(deployments, watcher::Config::default())
        .owns(services, watcher::Config::default())
        .owns(config_maps, watcher::Config::default())
        .run(reconcile, error_policy, context)
        .for_each(|result| async move {
            match result {
                Ok(object) => tracing::debug!(?object, "reconciled YesnoCluster"),
                Err(error) => tracing::error!(%error, "reconciliation failed"),
            }
        })
        .await;
}

async fn reconcile(cluster: Arc<YesnoCluster>, context: Arc<Context>) -> Result<Action, Error> {
    let name = cluster.name_any();
    let namespace = cluster
        .namespace()
        .ok_or_else(|| Error::MissingNamespace(name.clone()))?;
    let cluster_api = Api::<YesnoCluster>::namespaced(context.client.clone(), &namespace);

    if let Some(message) = validate(&cluster) {
        patch_status(
            &cluster_api,
            &cluster,
            status(
                &cluster,
                &[],
                StatusView {
                    phase: YesnoClusterPhase::Invalid,
                    ready_instances: 0,
                    primary_instance: 0,
                    primary_term: 0,
                    promotion_count: cluster
                        .status
                        .as_ref()
                        .map_or(0, |status| status.promotion_count),
                    primary_unavailable_since_millis: None,
                    failover: None,
                    reason: "InvalidSpec",
                    message,
                },
            ),
        )
        .await?;
        return Ok(Action::await_change());
    }

    let secret_identity = resolve_secret_identity(&cluster, &context, &namespace).await?;
    if cluster.spec.config.secret_name.is_some() && secret_identity.is_none() {
        let secret_name = cluster
            .spec
            .config
            .secret_name
            .as_deref()
            .unwrap_or_default();
        patch_status(
            &cluster_api,
            &cluster,
            status(
                &cluster,
                &[],
                StatusView {
                    phase: YesnoClusterPhase::Pending,
                    ready_instances: 0,
                    primary_instance: current_primary(&cluster),
                    primary_term: current_term(&cluster),
                    promotion_count: promotion_count(&cluster),
                    primary_unavailable_since_millis: None,
                    failover: None,
                    reason: "ConfigurationMissing",
                    message: format!("Secret `{secret_name}` must contain a `yesnod.toml` key"),
                },
            ),
        )
        .await?;
        return Ok(Action::requeue(Duration::from_secs(30)));
    }

    let managed_tls = if cluster.spec.config.cert_manager.is_some() {
        ensure_certificates(context.client.clone(), &namespace, &cluster).await?;
        match resolve_managed_tls(&cluster, &context, &namespace).await? {
            Some(tls) => Some(tls),
            None => {
                patch_status(
                    &cluster_api,
                    &cluster,
                    status(
                        &cluster,
                        &[],
                        StatusView {
                            phase: YesnoClusterPhase::Pending,
                            ready_instances: 0,
                            primary_instance: current_primary(&cluster),
                            primary_term: current_term(&cluster),
                            promotion_count: promotion_count(&cluster),
                            primary_unavailable_since_millis: None,
                            failover: None,
                            reason: "CertificatesPending",
                            message:
                                "waiting for the CA bundle and all cert-manager certificate Secrets"
                                    .into(),
                        },
                    ),
                )
                .await?;
                return Ok(Action::requeue(Duration::from_secs(5)));
            }
        }
    } else {
        None
    };

    let primary = current_primary(&cluster);
    let volumes = resolve_data_volumes(context.client.clone(), &namespace, &cluster).await?;
    let observed = observe_instances(
        &cluster,
        &context,
        &namespace,
        managed_tls.as_ref().map(|tls| &tls.client),
    )
    .await?;

    if let Some(failover) = cluster
        .status
        .as_ref()
        .and_then(|status| status.failover.clone())
    {
        return reconcile_failover(
            &cluster_api,
            &cluster,
            &context,
            &namespace,
            ResolvedConfig {
                secret_identity: secret_identity.as_deref(),
                managed_tls: managed_tls.as_ref(),
                volumes: &volumes,
            },
            observed,
            failover,
        )
        .await;
    }

    let primary_ready = observed
        .get(primary as usize)
        .and_then(|instance| instance.deployment.as_ref())
        .is_some_and(deployment_is_ready);
    let prior_unavailable = cluster
        .status
        .as_ref()
        .and_then(|status| status.primary_unavailable_since_millis);
    let failover_armed = prior_unavailable.is_some()
        || cluster.status.as_ref().is_some_and(|status| {
            matches!(
                status.phase,
                YesnoClusterPhase::Ready | YesnoClusterPhase::Degraded
            )
        });

    if !primary_ready && cluster.spec.instances > 1 && failover_armed {
        let unavailable_since = prior_unavailable.unwrap_or_else(now_millis);
        let elapsed = now_millis().saturating_sub(unavailable_since);
        let delay_millis = i64::try_from(cluster.spec.failover_delay_secs)
            .unwrap_or(i64::MAX)
            .saturating_mul(1000);
        if elapsed >= delay_millis {
            if let Some(target) = promotion_candidate(&observed, primary) {
                let failover = FailoverStatus {
                    from_instance: primary,
                    to_instance: target,
                    stage: FailoverStage::Fencing,
                    started_at_millis: now_millis(),
                };
                patch_status(
                    &cluster_api,
                    &cluster,
                    status(
                        &cluster,
                        &volumes,
                        StatusView {
                            phase: YesnoClusterPhase::FailingOver,
                            ready_instances: ready_count(&observed),
                            primary_instance: primary,
                            primary_term: current_term(&cluster),
                            promotion_count: promotion_count(&cluster),
                            primary_unavailable_since_millis: Some(unavailable_since),
                            failover: Some(failover),
                            reason: "FencingPrimary",
                            message: format!(
                                "fencing unavailable primary instance {primary} before promotion"
                            ),
                        },
                    ),
                )
                .await?;
                return Ok(Action::requeue(Duration::from_secs(1)));
            }
        }
    }

    let applied = ensure_topology(
        context.client.clone(),
        &namespace,
        &cluster,
        primary,
        None,
        ResolvedConfig {
            secret_identity: secret_identity.as_deref(),
            managed_tls: managed_tls.as_ref(),
            volumes: &volumes,
        },
    )
    .await?;
    let ready = applied
        .iter()
        .filter(|item| deployment_is_ready(item))
        .count() as i32;
    let available = ready == cluster.spec.instances;
    let unavailable_since = if primary_ready || !failover_armed {
        None
    } else {
        Some(prior_unavailable.unwrap_or_else(now_millis))
    };
    let primary_term = observed
        .get(primary as usize)
        .and_then(|instance| instance.snapshot.as_ref())
        .filter(|snapshot| role(snapshot) == Some(Role::Leader))
        .map_or_else(
            || current_term(&cluster),
            |snapshot| i64::from(snapshot.term),
        );
    let (phase, reason, message) = if available {
        (
            YesnoClusterPhase::Ready,
            "Available",
            format!(
                "primary instance {primary} and {} follower(s) are ready",
                cluster.spec.instances - 1
            ),
        )
    } else if failover_armed && !primary_ready {
        (
            YesnoClusterPhase::Degraded,
            "PrimaryUnavailable",
            format!(
                "primary instance {primary} is unavailable; waiting for the failover delay or an eligible follower"
            ),
        )
    } else {
        (
            YesnoClusterPhase::Progressing,
            "InstancesStarting",
            format!(
                "waiting for all instances: {ready}/{} are ready",
                cluster.spec.instances
            ),
        )
    };
    patch_status(
        &cluster_api,
        &cluster,
        status(
            &cluster,
            &volumes,
            StatusView {
                phase,
                ready_instances: ready,
                primary_instance: primary,
                primary_term,
                promotion_count: promotion_count(&cluster),
                primary_unavailable_since_millis: unavailable_since,
                failover: None,
                reason,
                message,
            },
        ),
    )
    .await?;

    Ok(Action::requeue(Duration::from_secs(5)))
}

async fn reconcile_failover(
    cluster_api: &Api<YesnoCluster>,
    cluster: &YesnoCluster,
    context: &Context,
    namespace: &str,
    config: ResolvedConfig<'_>,
    observed: Vec<ObservedInstance>,
    mut failover: FailoverStatus,
) -> Result<Action, Error> {
    let from = failover.from_instance;
    let to = failover.to_instance;

    ensure_topology(
        context.client.clone(),
        namespace,
        cluster,
        from,
        Some(from),
        config,
    )
    .await?;

    match failover.stage {
        FailoverStage::Fencing => {
            if !instance_pods_absent(context.client.clone(), namespace, cluster, from).await? {
                patch_failover_status(
                    cluster_api,
                    cluster,
                    config.volumes,
                    &observed,
                    failover,
                    "FencingPrimary",
                    format!(
                        "waiting for every Pod of primary instance {from} to disappear before promotion"
                    ),
                )
                .await?;
                return Ok(Action::requeue(Duration::from_secs(1)));
            }
            failover.stage = FailoverStage::Promoting;
            patch_failover_status(
                cluster_api,
                cluster,
                config.volumes,
                &observed,
                failover,
                "PrimaryFenced",
                format!("primary instance {from} is fenced; promotion may proceed"),
            )
            .await?;
        }
        FailoverStage::Promoting => {
            let snapshot = snapshot_instance(
                namespace,
                cluster,
                to,
                config.managed_tls.map(|tls| &tls.client),
            )
            .await
            .ok_or_else(|| {
                Error::Control(format!("promotion target instance {to} is unreachable"))
            })?;
            if role(&snapshot) != Some(Role::Leader) {
                if role(&snapshot) != Some(Role::Follower)
                    || !snapshot.process_running
                    || !snapshot.storage_available
                {
                    return Err(Error::Control(format!(
                        "promotion target instance {to} is no longer an eligible follower"
                    )));
                }
                promote(
                    namespace,
                    cluster,
                    &failover,
                    config.managed_tls.map(|tls| &tls.client),
                )
                .await?;
            }
            failover.stage = FailoverStage::AwaitingPromotion;
            patch_failover_status(
                cluster_api,
                cluster,
                config.volumes,
                &observed,
                failover,
                "PromotionRequested",
                format!("waiting for instance {to} to open as leader"),
            )
            .await?;
        }
        FailoverStage::AwaitingPromotion => {
            let snapshot = snapshot_instance(
                namespace,
                cluster,
                to,
                config.managed_tls.map(|tls| &tls.client),
            )
            .await;
            if let Some(snapshot) = &snapshot {
                if role(snapshot) == Some(Role::Leader)
                    && snapshot.process_running
                    && snapshot.database_open
                    && snapshot.storage_available
                {
                    patch_status(
                        cluster_api,
                        cluster,
                        status(
                            cluster,
                            config.volumes,
                            StatusView {
                                phase: YesnoClusterPhase::Progressing,
                                ready_instances: ready_count(&observed),
                                primary_instance: to,
                                primary_term: i64::from(snapshot.term),
                                promotion_count: promotion_count(cluster).saturating_add(1),
                                primary_unavailable_since_millis: None,
                                failover: None,
                                reason: "PromotionCompleted",
                                message: format!(
                                    "instance {to} is leader; reconfiguring instance {from} as a follower"
                                ),
                            },
                        ),
                    )
                    .await?;
                    return Ok(Action::requeue(Duration::from_secs(1)));
                }
            }
            // A promotion can be lost, and waiting will not recover it: see
            // `promotion_was_lost` for why a follower observation is the signal.
            //
            // Observed 2026-09-08: the target's liveness probe killed its
            // container during the failover window, kubelet restarted it as a
            // follower, and the cluster stayed `FailingOver` indefinitely. A
            // `kill -USR1 1` in that pod completed the whole failover in
            // seconds, which is what proved the promotion logic sound and the
            // *delivery* lost.
            //
            // Re-issuing is safe by construction: `Promoting` re-snapshots
            // and calls `promote` only when the role is not already `Leader`,
            // and the server's leader arm answers a duplicate with "promotion
            // already completed" precisely so this API is retry-safe across
            // that durability gap.
            if promotion_was_lost(snapshot.as_ref()) {
                failover.stage = FailoverStage::Promoting;
                patch_failover_status(
                    cluster_api,
                    cluster,
                    config.volumes,
                    &observed,
                    failover,
                    "PromotionLost",
                    format!("instance {to} is a follower again; re-issuing the promotion"),
                )
                .await?;
                return Ok(Action::requeue(Duration::from_secs(1)));
            }
            patch_failover_status(
                cluster_api,
                cluster,
                config.volumes,
                &observed,
                failover,
                "AwaitingPromotion",
                format!("waiting for instance {to} to report an open leader database"),
            )
            .await?;
        }
    }
    Ok(Action::requeue(Duration::from_secs(1)))
}

async fn patch_failover_status(
    api: &Api<YesnoCluster>,
    cluster: &YesnoCluster,
    volumes: &[VolumeBinding],
    observed: &[ObservedInstance],
    failover: FailoverStatus,
    reason: &'static str,
    message: String,
) -> Result<(), kube::Error> {
    patch_status(
        api,
        cluster,
        status(
            cluster,
            volumes,
            StatusView {
                phase: YesnoClusterPhase::FailingOver,
                ready_instances: ready_count(observed),
                primary_instance: failover.from_instance,
                primary_term: current_term(cluster),
                promotion_count: promotion_count(cluster),
                primary_unavailable_since_millis: cluster
                    .status
                    .as_ref()
                    .and_then(|status| status.primary_unavailable_since_millis),
                failover: Some(failover),
                reason,
                message,
            },
        ),
    )
    .await
}

/// Read each instance's EBS volume id out of the PersistentVolume its claim
/// bound to.
///
/// This is the whole reason the EBS backend belongs in the operator rather
/// than in a hand-written configuration. The daemon could in principle read
/// the serial off the device under its own data directory, but that would make
/// `verify_source_volume` vacuous: its entire job is to check that the volume
/// named in the configuration is the one the source mount actually uses, and a
/// volume id derived from that same mount agrees with it by construction.
/// Kubernetes is the independent witness, and only the operator can ask it.
async fn resolve_data_volumes(
    client: Client,
    namespace: &str,
    cluster: &YesnoCluster,
) -> Result<Vec<VolumeBinding>, Error> {
    if cluster.spec.snapshot.backend != SnapshotBackend::Ebs {
        return Ok(Vec::new());
    }
    let claims = Api::<PersistentVolumeClaim>::namespaced(client.clone(), namespace);
    let volumes = Api::<PersistentVolume>::all(client);
    let mut resolved = Vec::with_capacity(cluster.spec.instances.max(0) as usize);
    for instance in 0..cluster.spec.instances {
        let claim_name = data_claim_name(cluster, instance);
        let bound = claims
            .get_opt(&claim_name)
            .await?
            .and_then(|claim| claim.spec)
            .and_then(|spec| spec.volume_name)
            .filter(|volume_name| !volume_name.is_empty());
        let Some(volume_name) = bound else {
            resolved.push(VolumeBinding::Pending);
            continue;
        };
        // A missing `persistentvolumes` grant is reported, not raised. It
        // is a permanent misconfiguration of the controller's own ClusterRole,
        // and an unhandled error here would fail the whole reconcile -- taking
        // failover down with it, because a backup setting went unread.
        let volume = match volumes.get_opt(&volume_name).await {
            Ok(volume) => volume,
            Err(kube::Error::Api(response)) if response.code == 403 => {
                resolved.push(VolumeBinding::Unusable(format!(
                    "the controller may not read PersistentVolume `{volume_name}`: {}",
                    response.message
                )));
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        let Some(volume) = volume else {
            resolved.push(VolumeBinding::Pending);
            continue;
        };
        resolved.push(match ebs_volume_id(&volume) {
            Some(volume_id) => VolumeBinding::Bound(volume_id),
            None => VolumeBinding::Unusable(format!(
                "PersistentVolume `{volume_name}` is not an Amazon EBS volume"
            )),
        });
    }
    Ok(resolved)
}

/// The EBS volume id behind one PersistentVolume, in either spelling.
///
/// `ebs.csi.aws.com` writes the bare volume id as its CSI volume handle. A
/// volume provisioned before CSI carries `aws://<zone>/<volume>` instead, and
/// in-tree migration translates that at attach time rather than rewriting the
/// object, so both spellings outlive the plugin that created them.
fn ebs_volume_id(volume: &PersistentVolume) -> Option<String> {
    let spec = volume.spec.as_ref()?;
    let handle = spec
        .csi
        .as_ref()
        .filter(|csi| csi.driver == EBS_CSI_DRIVER)
        .map(|csi| csi.volume_handle.as_str())
        .or_else(|| {
            spec.aws_elastic_block_store
                .as_ref()
                .map(|source| source.volume_id.as_str())
        })?;
    let candidate = handle.rsplit('/').next().unwrap_or(handle);
    let digits = candidate.strip_prefix("vol-")?;
    (!digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| candidate.to_owned())
}

async fn resolve_secret_identity(
    cluster: &YesnoCluster,
    context: &Context,
    namespace: &str,
) -> Result<Option<String>, Error> {
    let Some(secret_name) = cluster.spec.config.secret_name.as_deref() else {
        return Ok(None);
    };
    let secrets = Api::<Secret>::namespaced(context.client.clone(), namespace);
    let secret = secrets.get_opt(secret_name).await?;
    let has_config = secret
        .as_ref()
        .and_then(|secret| secret.data.as_ref())
        .is_some_and(|data| data.contains_key("yesnod.toml"));
    if !has_config {
        return Ok(None);
    }
    let data = secret.and_then(|secret| secret.data).unwrap_or_default();
    let encoded =
        serde_json::to_vec(&data).expect("Kubernetes Secret data is always JSON serializable");
    Ok(Some(format!(
        "{secret_name}:{:08x}",
        crc32c::crc32c(&encoded)
    )))
}

async fn ensure_certificates(
    client: Client,
    namespace: &str,
    cluster: &YesnoCluster,
) -> Result<(), Error> {
    let certificates =
        Api::<DynamicObject>::namespaced_with(client, namespace, &certificate_api_resource());
    let params = PatchParams::apply(FIELD_MANAGER).force();
    let client_cert = client_certificate(cluster);
    certificates
        .patch(&client_cert.name_any(), &params, &Patch::Apply(client_cert))
        .await?;
    for instance in 0..cluster.spec.instances {
        let cert = instance_certificate(cluster, instance);
        certificates
            .patch(&cert.name_any(), &params, &Patch::Apply(cert))
            .await?;
    }
    Ok(())
}

fn secret_part(secret: &Secret, key: &str) -> Option<Vec<u8>> {
    secret
        .data
        .as_ref()
        .and_then(|data| data.get(key))
        .map(|value| value.0.clone())
}

fn certificate_fingerprint(pem_bytes: &[u8]) -> Option<String> {
    let certificate = pem::parse_many(pem_bytes)
        .ok()?
        .into_iter()
        .find(|block| block.tag() == "CERTIFICATE")?;
    let digest = ring::digest::digest(&ring::digest::SHA256, certificate.contents());
    Some(
        digest
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    )
}

async fn adopt_certificate_secret(
    secrets: &Api<Secret>,
    secret: &Secret,
    cluster: &YesnoCluster,
) -> Result<(), Error> {
    let cluster_owner = owner(cluster);
    let mut owners = secret.metadata.owner_references.clone().unwrap_or_default();
    if owners
        .iter()
        .any(|reference| reference.uid == cluster_owner.uid)
    {
        return Ok(());
    }
    owners.push(cluster_owner);
    secrets
        .patch(
            &secret.name_any(),
            &PatchParams::default(),
            &Patch::Merge(json!({ "metadata": { "ownerReferences": owners } })),
        )
        .await?;
    Ok(())
}

async fn resolve_managed_tls(
    cluster: &YesnoCluster,
    context: &Context,
    namespace: &str,
) -> Result<Option<ManagedTls>, Error> {
    let config = cluster
        .spec
        .config
        .cert_manager
        .as_ref()
        .expect("caller checked certManager config");
    let secrets = Api::<Secret>::namespaced(context.client.clone(), namespace);
    let Some(ca_secret) = secrets.get_opt(&config.ca_secret_ref.name).await? else {
        return Ok(None);
    };
    let Some(ca) = secret_part(&ca_secret, &config.ca_secret_ref.key) else {
        return Ok(None);
    };
    let Some(client_secret) = secrets.get_opt(&client_certificate_name(cluster)).await? else {
        return Ok(None);
    };
    adopt_certificate_secret(&secrets, &client_secret, cluster).await?;
    let (Some(client_cert), Some(client_key)) = (
        secret_part(&client_secret, "tls.crt"),
        secret_part(&client_secret, "tls.key"),
    ) else {
        return Ok(None);
    };
    let Some(client_fingerprint) = certificate_fingerprint(&client_cert) else {
        return Ok(None);
    };

    let mut instance_fingerprints = Vec::with_capacity(cluster.spec.instances as usize);
    let mut identity_bytes = Vec::new();
    identity_bytes.extend_from_slice(&ca);
    identity_bytes.extend_from_slice(&client_cert);
    identity_bytes.extend_from_slice(&client_key);
    for instance in 0..cluster.spec.instances {
        let Some(secret) = secrets
            .get_opt(&instance_certificate_name(cluster, instance))
            .await?
        else {
            return Ok(None);
        };
        adopt_certificate_secret(&secrets, &secret, cluster).await?;
        let (Some(cert), Some(key)) = (
            secret_part(&secret, "tls.crt"),
            secret_part(&secret, "tls.key"),
        ) else {
            return Ok(None);
        };
        let Some(fingerprint) = certificate_fingerprint(&cert) else {
            return Ok(None);
        };
        identity_bytes.extend_from_slice(&cert);
        identity_bytes.extend_from_slice(&key);
        instance_fingerprints.push(fingerprint);
    }
    let identity = format!("{:08x}", crc32c::crc32c(&identity_bytes));
    Ok(Some(ManagedTls {
        topology: TlsTopology {
            client_fingerprint,
            instance_fingerprints,
            ca_secret: config.ca_secret_ref.name.clone(),
            ca_key: config.ca_secret_ref.key.clone(),
            identity,
        },
        client: TlsClientMaterial {
            ca,
            cert: client_cert,
            key: client_key,
        },
    }))
}

async fn observe_instances(
    cluster: &YesnoCluster,
    context: &Context,
    namespace: &str,
    tls: Option<&TlsClientMaterial>,
) -> Result<Vec<ObservedInstance>, Error> {
    let deployments = Api::<Deployment>::namespaced(context.client.clone(), namespace);
    let deployment_results = join_all((0..cluster.spec.instances).map(|instance| {
        let deployments = deployments.clone();
        let name = instance_name(cluster, instance);
        async move { deployments.get_opt(&name).await }
    }))
    .await;
    let snapshots = join_all(
        (0..cluster.spec.instances)
            .map(|instance| snapshot_instance(namespace, cluster, instance, tls)),
    )
    .await;

    deployment_results
        .into_iter()
        .zip(snapshots)
        .map(|(deployment, snapshot)| {
            Ok(ObservedInstance {
                deployment: deployment?,
                snapshot,
            })
        })
        .collect()
}

async fn snapshot_instance(
    namespace: &str,
    cluster: &YesnoCluster,
    instance: i32,
    tls: Option<&TlsClientMaterial>,
) -> Option<StateSnapshot> {
    let host = format!("{}.{}.svc", instance_name(cluster, instance), namespace);
    let request = async {
        let channel = control_channel(&host, tls).await.ok()?;
        let mut client = ControlPlaneClient::new(channel);
        client
            .get_snapshot(GetSnapshotRequest {})
            .await
            .ok()
            .map(|response| response.into_inner())
    };
    tokio::time::timeout(Duration::from_secs(2), request)
        .await
        .ok()
        .flatten()
}

async fn promote(
    namespace: &str,
    cluster: &YesnoCluster,
    failover: &FailoverStatus,
    tls: Option<&TlsClientMaterial>,
) -> Result<(), Error> {
    let host = format!(
        "{}.{}.svc",
        instance_name(cluster, failover.to_instance),
        namespace
    );
    let request_id = format!(
        "{}:{}:{}:{}",
        cluster
            .metadata
            .uid
            .as_deref()
            .unwrap_or(&cluster.name_any()),
        failover.from_instance,
        failover.to_instance,
        failover.started_at_millis
    )
    .into_bytes();
    let request = async {
        let channel = control_channel(&host, tls)
            .await
            .map_err(|error| Error::Control(error.to_string()))?;
        let mut client = ControlPlaneClient::new(channel);
        client
            .promote(PromoteRequest { request_id })
            .await
            .map_err(|error| Error::Control(error.to_string()))?;
        Ok(())
    };
    tokio::time::timeout(Duration::from_secs(5), request)
        .await
        .map_err(|_| Error::Control("promotion request timed out".into()))?
}

async fn control_channel(
    host: &str,
    tls: Option<&TlsClientMaterial>,
) -> Result<Channel, tonic::transport::Error> {
    let scheme = if tls.is_some() { "https" } else { "http" };
    let mut endpoint = Endpoint::from_shared(format!("{scheme}://{host}:50052"))?;
    if let Some(tls) = tls {
        endpoint = endpoint.tls_config(
            ClientTlsConfig::new()
                .ca_certificate(Certificate::from_pem(tls.ca.clone()))
                .identity(Identity::from_pem(tls.cert.clone(), tls.key.clone()))
                .domain_name(host.to_owned()),
        )?;
    }
    endpoint.connect().await
}

fn role(snapshot: &StateSnapshot) -> Option<Role> {
    Role::try_from(snapshot.role).ok()
}

/// Whether an `AwaitingPromotion` observation means the promotion was **lost**
/// rather than merely still in flight.
///
/// `ControlPlane::Promote` answers `CommandAccepted`, which reports that the
/// request was journaled -- not that it took effect. Delivery to the lifecycle
/// loop is asynchronous, so a target that restarts between the two comes back a
/// follower with no memory of the command, and `AwaitingPromotion` would poll
/// for it forever.
///
/// The trigger is state, not a timer. A target observed as a **healthy,
/// settled follower** -- process up, storage available, database open -- has not
/// "not promoted yet", it has gone backwards. Requiring `database_open` is what
/// keeps this from firing mid-promotion, when the target has closed its follower
/// database and not yet opened it as leader.
fn promotion_was_lost(snapshot: Option<&StateSnapshot>) -> bool {
    let Some(snapshot) = snapshot else {
        // Unreachable instances are the ordinary case during a failover
        // window; only a positive observation of a follower is evidence.
        return false;
    };
    role(snapshot) == Some(Role::Follower)
        && snapshot.process_running
        && snapshot.database_open
        && snapshot.storage_available
}

fn promotion_candidate(observed: &[ObservedInstance], primary: i32) -> Option<i32> {
    observed
        .iter()
        .enumerate()
        .filter(|(instance, _)| *instance != primary as usize)
        .filter_map(|(instance, observed)| {
            let snapshot = observed.snapshot.as_ref()?;
            (snapshot.process_running
                && snapshot.storage_available
                && role(snapshot) == Some(Role::Follower))
            .then_some((snapshot.visible_version, instance as i32))
        })
        .max_by_key(|(visible_version, instance)| (*visible_version, -(*instance as i64)))
        .map(|(_, instance)| instance)
}

async fn instance_pods_absent(
    client: Client,
    namespace: &str,
    cluster: &YesnoCluster,
    instance: i32,
) -> Result<bool, Error> {
    let pods = Api::<Pod>::namespaced(client, namespace);
    let selector = format!(
        "app.kubernetes.io/instance={},{}={instance}",
        base_name(cluster),
        INSTANCE_LABEL
    );
    Ok(pods
        .list(&ListParams::default().labels(&selector))
        .await?
        .items
        .is_empty())
}

/// `config` rather than three loose arguments. The three travel together
/// everywhere else -- `reconcile_failover` already passes them as one
/// `ResolvedConfig` -- and separating them here is what took this function past
/// the argument count clippy will accept. Not silenced with an `allow`: the
/// bundle already existed and the parameters were the odd ones out.
async fn ensure_topology(
    client: Client,
    namespace: &str,
    cluster: &YesnoCluster,
    primary: i32,
    fenced_instance: Option<i32>,
    config: ResolvedConfig<'_>,
) -> Result<Vec<Deployment>, Error> {
    let secret_identity = config.secret_identity;
    let tls = config.managed_tls.map(|tls| &tls.topology);
    let volumes = config.volumes;
    let patch_params = PatchParams::apply(FIELD_MANAGER).force();
    let config_maps = Api::<ConfigMap>::namespaced(client.clone(), namespace);
    let services = Api::<Service>::namespaced(client.clone(), namespace);
    let deployments = Api::<Deployment>::namespaced(client.clone(), namespace);

    for service in [read_write_service(cluster), read_only_service(cluster)] {
        let name = service.name_any();
        services
            .patch(&name, &patch_params, &Patch::Apply(service))
            .await?;
    }

    let mut result = Vec::with_capacity(cluster.spec.instances as usize);
    for instance in 0..cluster.spec.instances {
        ensure_claim(client.clone(), namespace, cluster, instance).await?;
        let instance_service = instance_service(cluster, instance);
        let service_name = instance_service.name_any();
        services
            .patch(
                &service_name,
                &patch_params,
                &Patch::Apply(instance_service),
            )
            .await?;

        let role = if instance == primary {
            InstanceRole::Leader
        } else {
            InstanceRole::Follower
        };
        let source = config_source(
            cluster,
            instance,
            role,
            secret_identity,
            tls,
            volumes
                .get(instance as usize)
                .and_then(VolumeBinding::volume_id),
        );
        if let ConfigSource::Generated { name, body, .. } = &source {
            config_maps
                .patch(
                    name,
                    &patch_params,
                    &Patch::Apply(config_map(cluster, instance, name, body)),
                )
                .await?;
        }
        let replicas = i32::from(fenced_instance != Some(instance));
        let desired = deployment(cluster, instance, role, &source, replicas);
        let deployment_name = desired.name_any();
        result.push(
            deployments
                .patch(&deployment_name, &patch_params, &Patch::Apply(desired))
                .await?,
        );
    }

    if cluster.spec.config.secret_name.is_some() {
        for generated_name in [
            format!("{}-config", base_name(cluster)),
            format!("{}-config", instance_name(cluster, 0)),
        ] {
            if let Err(error) = config_maps
                .delete(&generated_name, &DeleteParams::default())
                .await
            {
                if !matches!(&error, kube::Error::Api(response) if response.code == 404) {
                    return Err(error.into());
                }
            }
        }
    }

    Ok(result)
}

async fn ensure_claim(
    client: Client,
    namespace: &str,
    cluster: &YesnoCluster,
    instance: i32,
) -> Result<(), Error> {
    if cluster.spec.storage.existing_claim.is_some() {
        return Ok(());
    }
    let claims = Api::<PersistentVolumeClaim>::namespaced(client, namespace);
    let name = data_claim_name(cluster, instance);
    if claims.get_opt(&name).await?.is_none() {
        claims
            .patch(
                &name,
                &PatchParams::apply(FIELD_MANAGER),
                &Patch::Apply(persistent_volume_claim(cluster, instance)),
            )
            .await?;
    }
    Ok(())
}

fn deployment_is_ready(deployment: &Deployment) -> bool {
    if deployment.spec.as_ref().and_then(|spec| spec.replicas) != Some(1) {
        return false;
    }
    let generation = deployment.metadata.generation.unwrap_or(0);
    deployment.status.as_ref().is_some_and(|status| {
        status.observed_generation.unwrap_or(0) >= generation
            && status.updated_replicas == Some(1)
            && status.available_replicas == Some(1)
            && status.unavailable_replicas.unwrap_or(0) == 0
    })
}

fn ready_count(observed: &[ObservedInstance]) -> i32 {
    observed
        .iter()
        .filter(|instance| {
            instance
                .deployment
                .as_ref()
                .is_some_and(deployment_is_ready)
        })
        .count() as i32
}

/// Refuse a snapshot spec the generated configuration could not honour.
///
/// The daemon validates its own configuration and refuses to start on a bad
/// one, which in a Deployment is a crash loop with the reason buried in a Pod
/// log. Everything checkable from the spec is therefore checked here, where it
/// becomes `.status` on the object the user just edited.
fn validate_snapshot(cluster: &YesnoCluster) -> Option<String> {
    let snapshot = &cluster.spec.snapshot;
    if snapshot.lease_ttl_secs == 0 {
        return Some("spec.snapshot.leaseTtlSecs must be positive".into());
    }
    let ebs = match (snapshot.backend, snapshot.ebs.as_ref()) {
        (SnapshotBackend::Disabled, None) => return None,
        (SnapshotBackend::Disabled, Some(_)) => {
            return Some("spec.snapshot.ebs requires spec.snapshot.backend to be `ebs`".into());
        }
        (SnapshotBackend::Ebs, None) => {
            return Some("spec.snapshot.backend `ebs` requires spec.snapshot.ebs".into());
        }
        (SnapshotBackend::Ebs, Some(ebs)) => ebs,
    };

    // A Secret-supplied configuration is written by the user and copied
    // verbatim, so there is nowhere for the controller to put the volume id it
    // discovers. Refusing the combination is better than accepting a snapshot
    // spec and silently ignoring it.
    if cluster.spec.config.secret_name.is_some() {
        return Some(
            "spec.snapshot.backend `ebs` needs generated configuration; it cannot be combined with spec.config.secretName"
                .into(),
        );
    }
    if ebs.region.is_empty()
        || ebs.region.starts_with('-')
        || ebs
            .region
            .bytes()
            .any(|byte| !(byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'))
    {
        return Some(
            "spec.snapshot.ebs.region must contain only lowercase ASCII letters, digits, and hyphens"
                .into(),
        );
    }
    if !matches!(ebs.filesystem.as_str(), "ext4" | "xfs") {
        return Some("spec.snapshot.ebs.filesystem must be `ext4` or `xfs`".into());
    }
    if ebs.operation_timeout_secs == 0 {
        return Some("spec.snapshot.ebs.operationTimeoutSecs must be positive".into());
    }
    for (key, value) in &ebs.resource_tags {
        if key.is_empty() || matches!(key.as_str(), "yesno:database" | "yesno:lease") {
            return Some(format!(
                "spec.snapshot.ebs.resourceTags key `{key}` is empty or reserved by the provider"
            ));
        }
        if key.len() > 128 || value.len() > 256 {
            return Some(format!(
                "spec.snapshot.ebs.resourceTags.{key} exceeds the AWS tag length limit"
            ));
        }
        // The AWS tag alphabet, which excludes every character that could
        // change the meaning of the generated TOML rather than sit inside a
        // string. The renderer escapes regardless; this is here so the user
        // is told, not so the rendering is safe.
        if let Some(bad) = key
            .chars()
            .chain(value.chars())
            .find(|character| !(character.is_alphanumeric() || " +-=._:/@".contains(*character)))
        {
            return Some(format!(
                "spec.snapshot.ebs.resourceTags.{key} contains `{bad}`, which AWS tags do not allow"
            ));
        }
    }
    None
}

fn validate(cluster: &YesnoCluster) -> Option<String> {
    if cluster.spec.image.trim().is_empty() {
        return Some("spec.image must not be empty".into());
    }
    if !(1..=9).contains(&cluster.spec.instances) {
        return Some("spec.instances must be between 1 and 9".into());
    }
    if cluster.spec.failover_delay_secs > 600 {
        return Some("spec.failoverDelaySecs must be between 0 and 600".into());
    }
    if cluster.spec.shards == 0 {
        return Some("spec.shards must be at least 1".into());
    }
    if cluster.spec.shutdown_grace_secs == 0 || cluster.spec.shutdown_grace_secs > 3600 {
        return Some("spec.shutdownGraceSecs must be between 1 and 3600".into());
    }

    match (
        cluster.spec.storage.existing_claim.as_deref(),
        cluster.spec.storage.size.as_deref(),
    ) {
        (Some(""), _) => return Some("spec.storage.existingClaim must not be empty".into()),
        (Some(_), Some(_)) => {
            return Some(
                "spec.storage.existingClaim and spec.storage.size are mutually exclusive".into(),
            );
        }
        (None, Some("")) => return Some("spec.storage.size must not be empty".into()),
        (None, None) => {
            return Some(
                "spec.storage requires either size or existingClaim; data is never ephemeral"
                    .into(),
            );
        }
        _ => {}
    }
    if cluster.spec.instances > 1 && cluster.spec.storage.existing_claim.is_some() {
        return Some(
            "spec.storage.existingClaim is supported only when spec.instances is 1; each follower needs an independent PVC"
                .into(),
        );
    }
    if cluster.spec.storage.existing_claim.is_none()
        && cluster
            .spec
            .storage
            .storage_class_name
            .as_deref()
            .is_none_or(str::is_empty)
    {
        return Some("spec.storage.storageClassName is required for a managed PVC".into());
    }

    if cluster
        .spec
        .service_account_name
        .as_deref()
        .is_some_and(str::is_empty)
    {
        return Some("spec.serviceAccountName must not be empty".into());
    }
    if let Some(message) = validate_snapshot(cluster) {
        return Some(message);
    }

    let security_modes = usize::from(cluster.spec.config.secret_name.is_some())
        + usize::from(cluster.spec.config.allow_insecure)
        + usize::from(cluster.spec.config.cert_manager.is_some());
    if security_modes != 1 {
        return Some(
            "set exactly one of spec.config.secretName, spec.config.certManager, or spec.config.allowInsecure"
                .into(),
        );
    }
    if cluster.spec.config.secret_name.as_deref() == Some("") {
        return Some("spec.config.secretName must not be empty".into());
    }
    if let Some(cert_manager) = &cluster.spec.config.cert_manager {
        if cert_manager.issuer_ref.name.trim().is_empty()
            || cert_manager.issuer_ref.kind.trim().is_empty()
            || cert_manager.issuer_ref.group.trim().is_empty()
        {
            return Some("spec.config.certManager.issuerRef fields must not be empty".into());
        }
        if cert_manager.ca_secret_ref.name.trim().is_empty()
            || cert_manager.ca_secret_ref.key.trim().is_empty()
        {
            return Some("spec.config.certManager.caSecretRef fields must not be empty".into());
        }
    }
    if cluster.spec.instances > 1 && cluster.spec.config.secret_name.is_some() {
        return Some(
            "spec.config.secretName is supported only when spec.instances is 1; secure topology configuration needs a per-instance template API"
                .into(),
        );
    }

    if let Some(policy) = cluster.spec.image_pull_policy.as_deref() {
        if !matches!(policy, "Always" | "IfNotPresent" | "Never") {
            return Some("spec.imagePullPolicy must be Always, IfNotPresent, or Never".into());
        }
    }
    None
}

fn current_primary(cluster: &YesnoCluster) -> i32 {
    cluster
        .status
        .as_ref()
        .map(|status| status.primary_instance)
        .filter(|instance| (0..cluster.spec.instances).contains(instance))
        .unwrap_or(0)
}

fn current_term(cluster: &YesnoCluster) -> i64 {
    cluster
        .status
        .as_ref()
        .map_or(0, |status| status.primary_term)
}

fn promotion_count(cluster: &YesnoCluster) -> i64 {
    cluster
        .status
        .as_ref()
        .map_or(0, |status| status.promotion_count)
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}

/// Keep a condition's transition time when its status has not changed.
fn transition_time(cluster: &YesnoCluster, type_: &str, condition_status: &str) -> Time {
    cluster
        .status
        .as_ref()
        .and_then(|status| {
            status
                .conditions
                .iter()
                .find(|condition| condition.type_ == type_)
        })
        .filter(|condition| condition.status == condition_status)
        .map(|condition| condition.last_transition_time.clone())
        .unwrap_or_else(|| Time(Timestamp::now()))
}

/// Whether the configured snapshot backend has reached every instance.
///
/// Absent when no backend is configured, so an ordinary cluster gains no
/// condition it would have to explain. Note that this is deliberately *not*
/// folded into `Ready`: an instance whose claim has not bound yet is serving
/// queries perfectly well, and a cluster that cannot ever satisfy the backend
/// -- a claim bound to something that is not an EBS volume -- is a backup
/// misconfiguration, not a reason to report the database as down.
fn snapshot_condition(
    cluster: &YesnoCluster,
    volumes: &[VolumeBinding],
) -> Option<(&'static str, &'static str, String)> {
    if cluster.spec.snapshot.backend != SnapshotBackend::Ebs {
        return None;
    }
    if let Some((instance, reason)) =
        volumes
            .iter()
            .enumerate()
            .find_map(|(instance, binding)| match binding {
                VolumeBinding::Unusable(reason) => Some((instance, reason)),
                _ => None,
            })
    {
        return Some((
            "False",
            "UnusableVolume",
            format!(
                "instance {instance}: {reason}; its configuration keeps the portable snapshot \
                 provider"
            ),
        ));
    }
    let pending = volumes
        .iter()
        .filter(|binding| **binding == VolumeBinding::Pending)
        .count();
    if volumes.len() != cluster.spec.instances.max(0) as usize || pending > 0 {
        return Some((
            "False",
            "AwaitingVolumeBinding",
            "waiting for every instance's PersistentVolumeClaim to bind before its configuration \
             can name an EBS volume"
                .into(),
        ));
    }
    Some((
        "True",
        "Configured",
        format!(
            "all {} instance(s) snapshot their own EBS volume",
            volumes.len()
        ),
    ))
}

fn status(
    cluster: &YesnoCluster,
    volumes: &[VolumeBinding],
    view: StatusView,
) -> YesnoClusterStatus {
    let ready_status = if matches!(view.phase, YesnoClusterPhase::Ready) {
        "True"
    } else {
        "False"
    };
    let last_transition_time = transition_time(cluster, "Ready", ready_status);
    let namespace = cluster.namespace().unwrap_or_default();
    let endpoint = format!("{}-rw.{namespace}.svc:50051", base_name(cluster));

    YesnoClusterStatus {
        phase: view.phase,
        ready_instances: view.ready_instances,
        primary_instance: view.primary_instance,
        primary_term: view.primary_term,
        promotion_count: view.promotion_count,
        primary_unavailable_since_millis: view.primary_unavailable_since_millis,
        failover: view.failover,
        endpoint,
        client_secret: cluster
            .spec
            .config
            .cert_manager
            .as_ref()
            .map(|_| client_certificate_name(cluster)),
        data_claim: data_claim_name(cluster, view.primary_instance),
        observed_generation: cluster.metadata.generation.unwrap_or(0),
        conditions: {
            let mut conditions = vec![Condition {
                last_transition_time,
                message: view.message,
                observed_generation: cluster.metadata.generation,
                reason: view.reason.into(),
                status: ready_status.into(),
                type_: "Ready".into(),
            }];
            if let Some((condition_status, reason, message)) = snapshot_condition(cluster, volumes)
            {
                conditions.push(Condition {
                    last_transition_time: transition_time(
                        cluster,
                        SNAPSHOT_CONDITION,
                        condition_status,
                    ),
                    message,
                    observed_generation: cluster.metadata.generation,
                    reason: reason.into(),
                    status: condition_status.into(),
                    type_: SNAPSHOT_CONDITION.into(),
                });
            }
            conditions
        },
    }
}

async fn patch_status(
    api: &Api<YesnoCluster>,
    cluster: &YesnoCluster,
    next: YesnoClusterStatus,
) -> Result<(), kube::Error> {
    if cluster.status.as_ref() == Some(&next) {
        return Ok(());
    }
    api.patch_status(
        &cluster.name_any(),
        &PatchParams::apply(FIELD_MANAGER),
        &Patch::Merge(json!({ "status": next })),
    )
    .await?;
    Ok(())
}

fn error_policy(_cluster: Arc<YesnoCluster>, error: &Error, _context: Arc<Context>) -> Action {
    tracing::warn!(%error, "reconciliation will retry");
    Action::requeue(Duration::from_secs(5))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::api::{ConfigSpec, SnapshotSpec, StorageSpec, YesnoClusterSpec};

    use super::*;

    fn cluster() -> YesnoCluster {
        let mut cluster = YesnoCluster::new(
            "search",
            YesnoClusterSpec {
                image: "yesnod:test".into(),
                image_pull_policy: None,
                storage: StorageSpec {
                    size: Some("1Gi".into()),
                    storage_class_name: Some("local-storage".into()),
                    ..Default::default()
                },
                instances: 2,
                failover_delay_secs: 30,
                shards: 1,
                config: ConfigSpec {
                    allow_insecure: true,
                    ..Default::default()
                },
                snapshot: SnapshotSpec::default(),
                service_account_name: None,
                resources: None,
                pod_labels: BTreeMap::new(),
                node_selector: BTreeMap::new(),
                shutdown_grace_secs: 30,
            },
        );
        cluster.metadata.namespace = Some("default".into());
        cluster
    }

    fn observed(role: Role, version: u64) -> ObservedInstance {
        ObservedInstance {
            deployment: None,
            snapshot: Some(StateSnapshot {
                process_running: true,
                database_open: true,
                role: role as i32,
                term: 1,
                visible_version: version,
                storage_available: true,
                ..Default::default()
            }),
        }
    }

    #[test]
    fn validation_requires_independent_storage_and_generated_topology_config() {
        let mut c = cluster();
        assert_eq!(validate(&c), None);
        c.spec.storage.storage_class_name = None;
        assert!(validate(&c).unwrap().contains("storageClassName"));
        c.spec.storage.storage_class_name = Some("local-storage".into());

        c.spec.storage.existing_claim = Some("shared".into());
        c.spec.storage.size = None;
        assert!(validate(&c).unwrap().contains("independent PVC"));
        c.spec.storage.existing_claim = None;
        c.spec.storage.size = Some("1Gi".into());

        c.spec.config.allow_insecure = false;
        c.spec.config.secret_name = Some("config".into());
        assert!(validate(&c).unwrap().contains("per-instance template"));
    }

    fn snap(role: Role) -> StateSnapshot {
        StateSnapshot {
            process_running: true,
            database_open: true,
            role: role as i32,
            term: 1,
            storage_available: true,
            ..Default::default()
        }
    }

    /// A promotion whose `CommandAccepted` was journaled but never applied
    /// leaves `AwaitingPromotion` polling forever. Only a *settled* follower is
    /// evidence of that; every in-flight shape must stay silent, or the operator
    /// would re-issue on top of a promotion that is working.
    #[test]
    fn a_lost_promotion_is_recognised_only_from_a_settled_follower() {
        assert!(
            promotion_was_lost(Some(&snap(Role::Follower))),
            "a healthy follower has gone backwards and must be re-promoted"
        );

        assert!(
            !promotion_was_lost(None),
            "an unreachable instance is the ordinary case mid-failover"
        );
        assert!(
            !promotion_was_lost(Some(&snap(Role::Leader))),
            "the promotion succeeded"
        );

        // Mid-promotion the target has closed its follower database and not
        // yet opened it as leader. Without the `database_open` term this arm
        // would fire on every promotion in progress.
        for (label, mut snapshot) in [
            ("database closed", snap(Role::Follower)),
            ("process down", snap(Role::Follower)),
            ("storage away", snap(Role::Follower)),
        ] {
            match label {
                "database closed" => snapshot.database_open = false,
                "process down" => snapshot.process_running = false,
                _ => snapshot.storage_available = false,
            }
            assert!(
                !promotion_was_lost(Some(&snapshot)),
                "{label}: an unsettled follower is not evidence of a lost promotion"
            );
        }
    }

    #[test]
    fn candidate_is_the_most_caught_up_healthy_follower() {
        let mut stale = observed(Role::Follower, 7);
        stale.snapshot.as_mut().unwrap().storage_available = false;
        let instances = vec![
            observed(Role::Leader, 10),
            observed(Role::Follower, 8),
            observed(Role::Follower, 9),
            stale,
        ];
        assert_eq!(promotion_candidate(&instances, 0), Some(2));
    }

    fn ebs_cluster() -> YesnoCluster {
        let mut c = cluster();
        c.spec.snapshot = SnapshotSpec {
            backend: SnapshotBackend::Ebs,
            ebs: Some(crate::api::EbsSnapshotSpec {
                region: "ap-northeast-1".into(),
                filesystem: "ext4".into(),
                operation_timeout_secs: 900,
                resource_tags: BTreeMap::new(),
            }),
            lease_ttl_secs: 300,
        };
        c
    }

    fn persistent_volume(source: serde_json::Value) -> PersistentVolume {
        serde_json::from_value(serde_json::json!({
            "apiVersion": "v1",
            "kind": "PersistentVolume",
            "metadata": { "name": "pvc-abcd" },
            "spec": source,
        }))
        .unwrap()
    }

    #[test]
    fn an_ebs_volume_is_recognised_in_both_spellings() {
        let csi = persistent_volume(serde_json::json!({
            "csi": { "driver": EBS_CSI_DRIVER, "volumeHandle": "vol-0123456789abcdef0" }
        }));
        assert_eq!(
            ebs_volume_id(&csi).as_deref(),
            Some("vol-0123456789abcdef0")
        );
        // A volume provisioned before CSI. In-tree migration translates this
        // at attach time and leaves the object alone, so the spelling outlives
        // the plugin that wrote it.
        let in_tree = persistent_volume(serde_json::json!({
            "awsElasticBlockStore": { "volumeID": "aws://ap-northeast-1a/vol-00ff" }
        }));
        assert_eq!(ebs_volume_id(&in_tree).as_deref(), Some("vol-00ff"));
    }

    #[test]
    fn a_volume_that_is_not_ebs_is_not_guessed_at() {
        for source in [
            serde_json::json!({
                "csi": { "driver": "efs.csi.aws.com", "volumeHandle": "fs-00ff" }
            }),
            // Right driver, unusable handle. Passing it through would put a
            // value in the configuration that the daemon then rejects at
            // startup, turning a reportable condition into a crash loop.
            serde_json::json!({
                "csi": { "driver": EBS_CSI_DRIVER, "volumeHandle": "not-a-volume" }
            }),
            serde_json::json!({
                "csi": { "driver": EBS_CSI_DRIVER, "volumeHandle": "vol-" }
            }),
            serde_json::json!({
                "csi": { "driver": EBS_CSI_DRIVER, "volumeHandle": "vol-00zz" }
            }),
            serde_json::json!({ "hostPath": { "path": "/data" } }),
        ] {
            let volume = persistent_volume(source.clone());
            assert_eq!(ebs_volume_id(&volume), None, "{source}");
        }
    }

    #[test]
    fn the_snapshot_condition_reports_each_stage_of_binding() {
        let c = ebs_cluster();
        let condition = |volumes: &[VolumeBinding]| {
            let (status, reason, _) = snapshot_condition(&c, volumes).expect("a condition");
            (status, reason)
        };
        assert_eq!(
            condition(&[]),
            ("False", "AwaitingVolumeBinding"),
            "an unresolved cluster must not read as configured"
        );
        assert_eq!(
            condition(&[
                VolumeBinding::Bound("vol-00ff".into()),
                VolumeBinding::Pending
            ]),
            ("False", "AwaitingVolumeBinding"),
            "one bound instance is not the cluster"
        );
        assert_eq!(
            condition(&[
                VolumeBinding::Bound("vol-00ff".into()),
                VolumeBinding::Unusable(
                    "PersistentVolume `pvc-local` is not an Amazon EBS volume".into()
                ),
            ]),
            ("False", "UnusableVolume")
        );
        assert_eq!(
            condition(&[
                VolumeBinding::Bound("vol-00ff".into()),
                VolumeBinding::Bound("vol-00fe".into()),
            ]),
            ("True", "Configured")
        );
        // A cluster that asked for nothing gains no condition to explain.
        assert!(snapshot_condition(&cluster(), &[]).is_none());
    }

    #[test]
    fn the_snapshot_condition_does_not_gate_readiness() {
        let c = ebs_cluster();
        let rendered = status(
            &c,
            &[VolumeBinding::Pending, VolumeBinding::Pending],
            StatusView {
                phase: YesnoClusterPhase::Ready,
                ready_instances: 2,
                primary_instance: 0,
                primary_term: 1,
                promotion_count: 0,
                primary_unavailable_since_millis: None,
                failover: None,
                reason: "Available",
                message: "ready".into(),
            },
        );
        // A claim that has not bound yet is a backup-configuration state, not
        // a serving state: the database is up and answering.
        assert_eq!(rendered.conditions[0].type_, "Ready");
        assert_eq!(rendered.conditions[0].status, "True");
        assert_eq!(rendered.conditions[1].type_, SNAPSHOT_CONDITION);
        assert_eq!(rendered.conditions[1].status, "False");
    }

    #[test]
    fn validation_refuses_a_snapshot_spec_the_daemon_would_reject() {
        assert_eq!(validate(&ebs_cluster()), None);

        let mut c = ebs_cluster();
        c.spec.snapshot.ebs = None;
        assert!(validate(&c).unwrap().contains("requires spec.snapshot.ebs"));

        let mut c = ebs_cluster();
        c.spec.snapshot.backend = SnapshotBackend::Disabled;
        assert!(validate(&c).unwrap().contains("to be `ebs`"));

        let mut c = ebs_cluster();
        c.spec.snapshot.lease_ttl_secs = 0;
        assert!(validate(&c).unwrap().contains("leaseTtlSecs"));

        // An empty string is not "no ServiceAccount"; Kubernetes reads it as
        // `default`, which is exactly the identity the field exists to avoid.
        let mut c = ebs_cluster();
        c.spec.service_account_name = Some(String::new());
        assert!(validate(&c).unwrap().contains("serviceAccountName"));
        c.spec.service_account_name = Some("yesno-snapshotter".into());
        assert_eq!(validate(&c), None);

        // The controller has nowhere to put a discovered volume id in a
        // configuration it does not write.
        let mut c = ebs_cluster();
        c.spec.config.allow_insecure = false;
        c.spec.config.secret_name = Some("yesnod-config".into());
        assert!(validate(&c).unwrap().contains("secretName"));

        for region in ["", "-east", "AP-Northeast-1", "ap northeast 1"] {
            let mut c = ebs_cluster();
            c.spec.snapshot.ebs.as_mut().unwrap().region = region.into();
            assert!(
                validate(&c).unwrap().contains("region"),
                "accepted region `{region}`"
            );
        }

        let mut c = ebs_cluster();
        c.spec.snapshot.ebs.as_mut().unwrap().filesystem = "btrfs".into();
        assert!(validate(&c).unwrap().contains("filesystem"));

        let mut c = ebs_cluster();
        c.spec.snapshot.ebs.as_mut().unwrap().operation_timeout_secs = 0;
        assert!(validate(&c).unwrap().contains("operationTimeoutSecs"));

        for (key, value) in [
            ("yesno:lease", "mine"),
            ("", "mine"),
            ("owner", "a\"b"),
            ("own\ner", "x"),
            (&"k".repeat(129), "x"),
        ] {
            let mut c = ebs_cluster();
            c.spec.snapshot.ebs.as_mut().unwrap().resource_tags =
                BTreeMap::from([(key.to_owned(), value.to_owned())]);
            assert!(validate(&c).is_some(), "accepted tag `{key}` = `{value}`");
        }
    }

    #[test]
    fn absent_status_values_are_null_in_a_merge_patch() {
        let c = cluster();
        let value = serde_json::to_value(status(
            &c,
            &[],
            StatusView {
                phase: YesnoClusterPhase::Ready,
                ready_instances: 2,
                primary_instance: 1,
                primary_term: 1,
                promotion_count: 1,
                primary_unavailable_since_millis: None,
                failover: None,
                reason: "Available",
                message: "ready".into(),
            },
        ))
        .unwrap();
        let fields = value.as_object().unwrap();
        for field in ["primaryUnavailableSinceMillis", "failover", "clientSecret"] {
            assert!(fields.contains_key(field), "status omitted {field}");
            assert!(fields[field].is_null(), "status did not clear {field}");
        }
    }

    #[test]
    fn status_uses_the_stable_service_and_primary_claim() {
        let c = cluster();
        let status = status(
            &c,
            &[],
            StatusView {
                phase: YesnoClusterPhase::FailingOver,
                ready_instances: 1,
                primary_instance: 1,
                primary_term: 4,
                promotion_count: 2,
                primary_unavailable_since_millis: Some(10),
                failover: Some(FailoverStatus {
                    from_instance: 0,
                    to_instance: 1,
                    stage: FailoverStage::Fencing,
                    started_at_millis: 10,
                }),
                reason: "FencingPrimary",
                message: "waiting".into(),
            },
        );
        assert_eq!(status.endpoint, "search-rw.default.svc:50051");
        assert_eq!(status.data_claim, "search-1-data");
        assert_eq!(status.primary_term, 4);
        assert_eq!(status.conditions[0].status, "False");
    }

    #[test]
    fn readiness_belongs_to_the_current_deployment_generation_and_desired_replica() {
        let mut deployment = Deployment::default();
        deployment.metadata.generation = Some(2);
        deployment.spec = Some(k8s_openapi::api::apps::v1::DeploymentSpec {
            replicas: Some(1),
            ..Default::default()
        });
        deployment.status = Some(k8s_openapi::api::apps::v1::DeploymentStatus {
            observed_generation: Some(1),
            updated_replicas: Some(1),
            available_replicas: Some(1),
            replicas: Some(1),
            ..Default::default()
        });
        assert!(!deployment_is_ready(&deployment));

        deployment.status.as_mut().unwrap().observed_generation = Some(2);
        assert!(deployment_is_ready(&deployment));

        deployment.spec.as_mut().unwrap().replicas = Some(0);
        assert!(!deployment_is_ready(&deployment));
    }
}
