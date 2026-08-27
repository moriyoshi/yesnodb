use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::ResourceRequirements;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

fn default_shards() -> u16 {
    1
}

fn default_shutdown_grace_secs() -> u64 {
    30
}

fn default_instances() -> i32 {
    1
}

fn default_failover_delay_secs() -> u64 {
    30
}

fn default_lease_ttl_secs() -> u64 {
    300
}

fn default_snapshot_filesystem() -> String {
    "ext4".into()
}

fn default_operation_timeout_secs() -> u64 {
    3_600
}

/// A yesnodb installation managed by the operator.
#[derive(CustomResource, Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[kube(
    group = "yesnodb.io",
    version = "v1alpha1",
    kind = "YesnoCluster",
    plural = "yesnoclusters",
    shortname = "ync",
    namespaced,
    status = "YesnoClusterStatus"
)]
#[kube(printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#)]
#[kube(printcolumn = r#"{"name":"Ready","type":"integer","jsonPath":".status.readyInstances"}"#)]
#[kube(printcolumn = r#"{"name":"Primary","type":"integer","jsonPath":".status.primaryInstance"}"#)]
#[kube(printcolumn = r#"{"name":"Endpoint","type":"string","jsonPath":".status.endpoint"}"#)]
#[serde(rename_all = "camelCase")]
pub struct YesnoClusterSpec {
    /// Container image containing `yesnod`.
    pub image: String,

    /// Kubernetes image pull policy. Defaults to `IfNotPresent`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_pull_policy: Option<String>,

    /// Persistent storage mounted at `/var/lib/yesno`.
    pub storage: StorageSpec,

    /// Number of independently persisted instances. Instance zero is the
    /// initial leader and every additional instance starts as a follower.
    #[serde(default = "default_instances")]
    #[schemars(range(min = 1, max = 9))]
    pub instances: i32,

    /// How long a primary may remain unavailable before automatic failover.
    /// Zero is useful for deterministic tests; production clusters should
    /// leave time for an ordinary Pod restart.
    #[serde(default = "default_failover_delay_secs")]
    #[schemars(with = "i64", range(min = 0, max = 600))]
    pub failover_delay_secs: u64,

    /// Creation-time yesnodb shard count for generated configuration.
    #[serde(default = "default_shards")]
    #[schemars(with = "i32", range(min = 1, max = 65535))]
    pub shards: u16,

    /// How yesnod obtains its configuration.
    #[serde(default)]
    pub config: ConfigSpec,

    /// Server-owned base-snapshot provider written into generated
    /// configuration. Defaults to the portable copy behind the same lease.
    #[serde(default)]
    pub snapshot: SnapshotSpec,

    /// ServiceAccount for the managed Pods, for a cloud provider that grants
    /// credentials through one. The Pods never mount a Kubernetes API token,
    /// so this is useful only where the identity arrives some other way --
    /// on EKS, a webhook projects the IRSA or Pod Identity token into a
    /// volume of its own, which `automountServiceAccountToken` does not gate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_account_name: Option<String>,

    /// Container CPU and memory requests and limits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourceRequirements>,

    /// Labels copied to the managed Pod template.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub pod_labels: BTreeMap<String, String>,

    /// Node selector copied to the managed Pod template.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub node_selector: BTreeMap<String, String>,

    /// Seconds allowed for graceful reader drain and final checkpoint.
    #[serde(default = "default_shutdown_grace_secs")]
    #[schemars(with = "i64", range(min = 1, max = 3600))]
    pub shutdown_grace_secs: u64,
}

/// Server-owned base-snapshot provider policy.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotSpec {
    /// Which provider services a snapshot lease.
    #[serde(default)]
    pub backend: SnapshotBackend,

    /// Amazon EBS settings, required for and exclusive to the `ebs` backend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ebs: Option<EbsSnapshotSpec>,

    /// Seconds a lease survives without renewal before the server destroys it.
    #[serde(default = "default_lease_ttl_secs")]
    #[schemars(with = "i64", range(min = 1, max = 86400))]
    pub lease_ttl_secs: u64,
}

impl Default for SnapshotSpec {
    fn default() -> Self {
        Self {
            backend: SnapshotBackend::Disabled,
            ebs: None,
            lease_ttl_secs: default_lease_ttl_secs(),
        }
    }
}

/// Snapshot providers the operator can generate configuration for.
///
/// This is deliberately narrower than yesnod's own provider list. ZFS and
/// Btrfs describe a host filesystem the operator does not choose and cannot
/// see through a PersistentVolumeClaim, and LVM additionally needs a second,
/// privileged process beside the daemon. Amazon EBS is the one provider whose
/// entire input -- which volume holds this instance's data -- is something
/// Kubernetes knows and the operator can read.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SnapshotBackend {
    /// Portable server-staged copy behind the control-plane lease.
    #[default]
    Disabled,
    /// Amazon EBS snapshots of each instance's own data volume.
    Ebs,
}

/// Amazon EBS snapshot settings shared by every instance of one cluster.
///
/// There is no `volumeId` here, and that absence is the feature: each
/// instance has its own PersistentVolumeClaim and therefore its own EBS
/// volume, which the controller reads from the bound PersistentVolume and
/// writes into that instance's configuration alone.
///
/// Nor is materialization selectable. Local materialization attaches a
/// restored volume to the node and mounts it, and the managed Pod is built to
/// make that impossible -- it drops every capability, refuses privilege
/// escalation, and runs as an unprivileged user. Only deferred materialization
/// is generated, where the archiver stages the snapshot elsewhere.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EbsSnapshotSpec {
    /// Region holding the instance volumes and the snapshots taken from them.
    pub region: String,

    /// Filesystem on the data volume: `ext4` or `xfs`.
    #[serde(default = "default_snapshot_filesystem")]
    pub filesystem: String,

    /// Maximum wait for each AWS state transition.
    #[serde(default = "default_operation_timeout_secs")]
    #[schemars(with = "i64", range(min = 1, max = 86400))]
    pub operation_timeout_secs: u64,

    /// Tags copied to every snapshot the server creates. The provider-owned
    /// `yesno:database` and `yesno:lease` keys are reserved.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub resource_tags: BTreeMap<String, String>,
}

/// Persistent volume selection.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageSpec {
    /// Requested capacity for an operator-created PVC, such as `10Gi`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<String>,

    /// StorageClass for an operator-created PVC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_class_name: Option<String>,

    /// Mount this existing PVC instead of creating one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub existing_claim: Option<String>,
}

/// yesnod configuration source.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigSpec {
    /// Secret containing a `yesnod.toml` key and any files it references.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_name: Option<String>,

    /// Generate plaintext, unauthenticated configuration for a trusted test
    /// namespace. This must be explicitly true when `secretName` is absent.
    #[serde(default)]
    pub allow_insecure: bool,

    /// Generate per-instance mTLS configuration and have cert-manager issue
    /// the server and client certificates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cert_manager: Option<CertManagerSpec>,
}

/// cert-manager inputs for generated, authenticated topology configuration.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CertManagerSpec {
    /// Issuer or ClusterIssuer used for all leaf certificates.
    pub issuer_ref: IssuerReference,

    /// Independently managed CA bundle trusted by yesnod and the operator.
    /// This must identify the issuer chain, not a leaf Certificate Secret.
    pub ca_secret_ref: SecretKeyReference,
}

fn default_issuer_kind() -> String {
    "Issuer".into()
}

fn default_issuer_group() -> String {
    "cert-manager.io".into()
}

/// cert-manager issuer reference. Defaults match cert-manager's Certificate API.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IssuerReference {
    pub name: String,
    #[serde(default = "default_issuer_kind")]
    pub kind: String,
    #[serde(default = "default_issuer_group")]
    pub group: String,
}

fn default_ca_key() -> String {
    "ca.crt".into()
}

/// One key in a same-namespace Kubernetes Secret.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretKeyReference {
    pub name: String,
    #[serde(default = "default_ca_key")]
    pub key: String,
}

/// High-level progress visible through `kubectl get yesnoclusters`.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub enum YesnoClusterPhase {
    Pending,
    Progressing,
    Degraded,
    FailingOver,
    Ready,
    Invalid,
}

/// Durable progress for an automatic promotion.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FailoverStatus {
    pub from_instance: i32,
    pub to_instance: i32,
    pub stage: FailoverStage,
    pub started_at_millis: i64,
}

/// Ordered stages keep failover safe and restartable across operator crashes.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub enum FailoverStage {
    Fencing,
    Promoting,
    AwaitingPromotion,
}

/// Observed state of a managed cluster.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct YesnoClusterStatus {
    pub phase: YesnoClusterPhase,
    pub ready_instances: i32,
    #[serde(default)]
    pub primary_instance: i32,
    #[serde(default)]
    pub primary_term: i64,
    #[serde(default)]
    pub promotion_count: i64,
    // `None` must serialize as JSON null so a merge patch clears stale state.
    #[serde(default)]
    pub primary_unavailable_since_millis: Option<i64>,
    // `None` must serialize as JSON null when a completed failover is cleared.
    #[serde(default)]
    pub failover: Option<FailoverStatus>,
    pub endpoint: String,
    /// Administrative mTLS client identity issued for this cluster.
    // A security-mode change must clear an obsolete client Secret name.
    #[serde(default)]
    pub client_secret: Option<String>,
    pub data_claim: String,
    pub observed_generation: i64,
    pub conditions: Vec<Condition>,
}
