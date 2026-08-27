//! `op_*`: a live yesno operator, in a disposable kind cluster or on EKS.
//!
//! The existing Monty runner remains the only scenario endpoint. These verbs
//! are intentionally narrow: a scenario can vary the operator lifecycle and
//! assert Kubernetes observations, but it cannot execute arbitrary host or
//! kubectl commands. The host owns Docker, kind, kubectl, diagnostics, and
//! cleanup exactly as the `srv_*` host owns daemon processes.
//!
//! # Two backends, one set of verbs
//!
//! `op_prepare("kind")` owns its cluster end to end: it builds both images,
//! creates a kind cluster, loads them, and deletes all of it afterwards.
//!
//! `op_prepare("eks")` owns none of that. The real-AWS gate has already
//! provisioned a cluster, pushed both images to ECR and written an
//! administrative kubeconfig, and this backend is handed all of it through the
//! environment -- so what it adds is the CRD, the operator, and the same
//! `YesnoCluster` lifecycle, against storage that is genuinely Amazon EBS.
//!
//! **The point of the second backend is one assertion the first cannot
//! make.** kind provisions local-path volumes, so `SnapshotBackendReady` can
//! only ever be `UnusableVolume` there, and the operator's EBS discovery is
//! exercised right up to the answer and no further. On EKS the same code path
//! ends in a real `vol-`, in a generated configuration, in a daemon that
//! started with it.
//!
//! Neither backend subsumes the other and neither is optional. Promotion,
//! the fence and the invalid-spec lifecycle are the kind arm's, and the EKS arm
//! deliberately does not repeat them: they cost billable minutes and prove
//! nothing new about EBS.
//!
//! What the EKS arm *does* repeat is cert-manager, and that is a choice
//! rather than an oversight. `allowInsecure` would have saved ninety seconds
//! and three Pods, and would have forced a second spelling of the client Pod
//! and all three `exec` helpers -- so the two arms would have stopped running
//! the same topology, and the cheap one would no longer be evidence about the
//! expensive one. The one thing that differs between them is storage, which is
//! the one thing this second backend exists to change.

use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

type Result<T> = std::result::Result<T, String>;

use monty_types::{ExcType, MontyException, MontyObject};

use crate::convert::{dict, int_obj, value_err, Args};
use crate::world::World;

const KIND_NAMESPACE: &str = "yesno-e2e";
const KIND_STORAGE_CLASS: &str = "standard";
const CLUSTER: &str = "search";
const INVALID_CLUSTER: &str = "invalid";
/// How long a kind cluster is given to make something Ready.
const WAIT: &str = "180s";
/// The same on EKS, where every wait has more to do.
///
/// Not a padded copy of the number above. A managed claim there is a real
/// EBS volume the CSI driver has to create, attach and format; the image comes
/// from ECR rather than from the node's own store; and the database Pods are
/// then restarted once, by the configuration change that follows their claims
/// binding. Do not lower it to make a failing run fail faster -- a timeout
/// here costs the whole provisioned stack to retry.
const EKS_WAIT: &str = "900s";
/// How long `SnapshotBackendReady` is given to stop saying `AwaitingVolumeBinding`.
const SNAPSHOT_CONDITION_WAIT: Duration = Duration::from_secs(300);
const CERT_MANAGER_MANIFEST: &str =
    "https://github.com/cert-manager/cert-manager/releases/download/v1.21.1/cert-manager.yaml";
const PINNED_NODE_IMAGE: &str =
    "kindest/node:v1.36.1@sha256:3489c7674813ba5d8b1a9977baea8a6e553784dab7b84759d1014dbd78f7ebd5";
/// The environment an `eks` run is handed, and the Terraform output behind
/// each name.
///
/// Paired here for the same reason [`crate::aws::RUNNER_ENV`] is: the two
/// halves run on different machines, and a value added on the gate side must
/// not be able to go missing on this one. `cloud_operator_env()` fills the
/// right column from the stack and a unit test in `cloud.rs` asserts the left
/// column is exactly this list.
pub const OPERATOR_ENV: &[(&str, &str)] = &[
    (ENV_OPERATOR_KUBECONFIG, "operator_kubeconfig_path"),
    (ENV_OPERATOR_IMAGE, "operator_image"),
    (ENV_OPERATOR_SERVER_IMAGE, "yesnod_image"),
    (ENV_OPERATOR_NAMESPACE, "eks_operator_namespace"),
    (ENV_OPERATOR_STORAGE_CLASS, "eks_storage_class"),
    (ENV_OPERATOR_ACCOUNT, "eks_operator_account"),
    (ENV_OPERATOR_ROLE_ARN, "eks_operator_role_arn"),
    (ENV_OPERATOR_REGION, "region"),
];

/// Not `KUBECONFIG`. The archiver's kubeconfig already occupies that name in
/// the same container, and it carries a *namespace-scoped* token that cannot
/// install a CRD -- a collision would fail several minutes in, on a
/// permissions error naming an object nobody was thinking about.
pub const ENV_OPERATOR_KUBECONFIG: &str = "YESNO_OPERATOR_KUBECONFIG";
pub const ENV_OPERATOR_IMAGE: &str = "YESNO_OPERATOR_IMAGE";
pub const ENV_OPERATOR_SERVER_IMAGE: &str = "YESNO_OPERATOR_SERVER_IMAGE";
pub const ENV_OPERATOR_NAMESPACE: &str = "YESNO_OPERATOR_NAMESPACE";
pub const ENV_OPERATOR_STORAGE_CLASS: &str = "YESNO_OPERATOR_STORAGE_CLASS";
pub const ENV_OPERATOR_ACCOUNT: &str = "YESNO_OPERATOR_SERVICE_ACCOUNT";
pub const ENV_OPERATOR_ROLE_ARN: &str = "YESNO_OPERATOR_ROLE_ARN";
pub const ENV_OPERATOR_REGION: &str = "YESNO_OPERATOR_REGION";

/// Which cluster the `op_*` verbs are driving.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Backend {
    /// A disposable kind cluster this harness creates and deletes.
    Kind,
    /// An EKS cluster the real-AWS gate provisioned, reached through an
    /// administrative kubeconfig it wrote. Nothing here creates or destroys it.
    Eks,
}

impl Backend {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "kind" => Some(Self::Kind),
            "eks" => Some(Self::Eks),
            _ => None,
        }
    }
}

/// Operator verbs dispatched by the ordinary Monty scenario runner.
pub const OWNS: &[&str] = &[
    "op_prepare",
    "op_invalid_create",
    "op_invalid_status",
    "op_invalid_delete",
    "op_create",
    "op_status",
    "op_enable_ebs_snapshots",
    "op_snapshot_status",
    "op_put",
    "op_count",
    "op_wait_count",
    "op_checkpoint",
    "op_fail_primary",
    "op_wait_promotion",
    "op_delete",
    "op_deleted_status",
    "op_cleanup",
];

#[derive(Default)]
pub struct OperatorState {
    harness: Option<Harness>,
}

struct InvalidStatus {
    phase: String,
    deployment_exists: bool,
    pvc_exists: bool,
}

struct SnapshotBackendStatus {
    phase: String,
    condition_status: String,
    reason: String,
    ready: String,
    snapshot_configured: bool,
    /// Volume ids the generated configurations name, sorted.
    configured_volumes: Vec<String>,
    /// Volume handles of the PersistentVolumes those instances' claims bound
    /// to, sorted. Read from the claims rather than from the controller's
    /// own report: the two agreeing is the assertion, and a status field
    /// copying what the controller decided would agree with itself.
    bound_volumes: Vec<String>,
    /// How many times a daemon has logged that it cannot reconcile abandoned
    /// base snapshots. That message is what a Pod without working AWS
    /// credentials produces, and it is *not* fatal -- the retry loop backs off
    /// and the Pod stays Ready -- so nothing else would notice.
    reconcile_errors: u64,
    /// Instances whose running Pod carries the configuration its Deployment
    /// currently specifies.
    ///
    /// Without this the arm could pass on a configuration nothing had read.
    /// Learning the volume rewrites the ConfigMap and the Pod template in one
    /// reconcile, and the status is patched in the same pass -- so
    /// `SnapshotBackendReady=True` is observable *before* the Recreate rollout
    /// that carries it into a daemon has even started.
    settled_instances: u64,
    /// How many instances the controller currently specifies, to compare it to.
    specified_instances: u64,
}

struct ClusterStatus {
    phase: String,
    ready_instances: u64,
    primary_instance: u64,
    primary_term: u64,
    promotion_count: u64,
    endpoint: String,
    data_claim: String,
    client_secret: String,
    certificate_count: u64,
    issued_secret_count: u64,
    deployment_count: u64,
    all_recreate: bool,
    all_replicas_one: bool,
    leader_pods: u64,
    follower_pods: u64,
    pvc_count: u64,
    retention: String,
    storage_class: String,
    volume_mode: String,
}

struct PromotionStatus {
    phase: String,
    ready_instances: u64,
    old_primary: u64,
    primary_instance: u64,
    primary_term: u64,
    promotion_count: u64,
    leader_pods: u64,
    follower_pods: u64,
}

struct DeletedStatus {
    deployment_count: u64,
    service_count: u64,
    configmap_count: u64,
    certificate_count: u64,
    issued_secret_count: u64,
    pvc_count: u64,
    retained_pvc_count: u64,
}

impl OperatorState {
    fn harness_mut(&mut self, verb: &str) -> std::result::Result<&mut Harness, MontyException> {
        self.harness.as_mut().ok_or_else(|| {
            value_err(format!(
                "{verb}(): op_prepare() must be called before other operator verbs"
            ))
        })
    }
}

impl World {
    pub(crate) fn call_operator(
        &mut self,
        verb: &str,
        a: &Args<'_>,
    ) -> std::result::Result<MontyObject, MontyException> {
        match verb {
            "op_prepare" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let Some(backend) = Backend::parse(a.str_at(0)?) else {
                    return Err(value_err(
                        "op_prepare(): backend must be \"kind\" or \"eks\"",
                    ));
                };
                if self.operator.harness.is_some() {
                    return Err(value_err(
                        "op_prepare(): operator harness is already prepared",
                    ));
                }
                self.operator.harness = Some(Harness::new(
                    Config::from_env(backend).map_err(|error| operator_err(verb, error))?,
                ));
                let harness = self.operator.harness.as_mut().expect("just inserted");
                if let Err(error) = harness.prepare() {
                    harness.diagnose();
                    return Err(operator_err(verb, error));
                }
                Ok(MontyObject::Bool(true))
            }
            "op_invalid_create" => {
                a.exact(0)?;
                a.no_kwargs()?;
                self.operator
                    .harness_mut(verb)?
                    .create_invalid()
                    .map_err(|error| operator_err(verb, error))?;
                Ok(MontyObject::None)
            }
            "op_invalid_status" => {
                a.exact(0)?;
                a.no_kwargs()?;
                let status = self
                    .operator
                    .harness_mut(verb)?
                    .invalid_status()
                    .map_err(|error| operator_err(verb, error))?;
                Ok(dict(vec![
                    ("phase", MontyObject::String(status.phase)),
                    (
                        "deployment_exists",
                        MontyObject::Bool(status.deployment_exists),
                    ),
                    ("pvc_exists", MontyObject::Bool(status.pvc_exists)),
                ]))
            }
            "op_invalid_delete" => {
                a.exact(0)?;
                a.no_kwargs()?;
                self.operator
                    .harness_mut(verb)?
                    .delete_invalid()
                    .map_err(|error| operator_err(verb, error))?;
                Ok(MontyObject::None)
            }
            "op_create" => {
                a.exact(0)?;
                a.no_kwargs()?;
                self.operator
                    .harness_mut(verb)?
                    .create_database()
                    .map_err(|error| operator_err(verb, error))?;
                Ok(MontyObject::None)
            }
            "op_status" => {
                a.exact(0)?;
                a.no_kwargs()?;
                let status = self
                    .operator
                    .harness_mut(verb)?
                    .database_status()
                    .map_err(|error| operator_err(verb, error))?;
                Ok(dict(vec![
                    ("phase", MontyObject::String(status.phase)),
                    ("ready_instances", int_obj(status.ready_instances)),
                    ("primary_instance", int_obj(status.primary_instance)),
                    ("primary_term", int_obj(status.primary_term)),
                    ("promotion_count", int_obj(status.promotion_count)),
                    ("endpoint", MontyObject::String(status.endpoint)),
                    ("data_claim", MontyObject::String(status.data_claim)),
                    ("client_secret", MontyObject::String(status.client_secret)),
                    ("certificate_count", int_obj(status.certificate_count)),
                    ("issued_secret_count", int_obj(status.issued_secret_count)),
                    ("deployment_count", int_obj(status.deployment_count)),
                    ("all_recreate", MontyObject::Bool(status.all_recreate)),
                    (
                        "all_replicas_one",
                        MontyObject::Bool(status.all_replicas_one),
                    ),
                    ("leader_pods", int_obj(status.leader_pods)),
                    ("follower_pods", int_obj(status.follower_pods)),
                    ("pvc_count", int_obj(status.pvc_count)),
                    ("retention", MontyObject::String(status.retention)),
                    ("storage_class", MontyObject::String(status.storage_class)),
                    ("volume_mode", MontyObject::String(status.volume_mode)),
                ]))
            }
            "op_enable_ebs_snapshots" => {
                a.exact(0)?;
                a.no_kwargs()?;
                let status = self
                    .operator
                    .harness_mut(verb)?
                    .enable_ebs_snapshots()
                    .map_err(|error| operator_err(verb, error))?;
                Ok(snapshot_dict(status))
            }
            "op_snapshot_status" => {
                a.exact(0)?;
                a.no_kwargs()?;
                let status = self
                    .operator
                    .harness_mut(verb)?
                    .wait_snapshot_backend()
                    .map_err(|error| operator_err(verb, error))?;
                Ok(snapshot_dict(status))
            }
            "op_put" => {
                a.exact(2)?;
                a.no_kwargs()?;
                let keys = a.u64_list(0)?;
                let ordinals = a.u64_list(1)?;
                if keys.len() != ordinals.len() {
                    return Err(value_err(format!(
                        "op_put(): key and ordinal lists differ in length: {} != {}",
                        keys.len(),
                        ordinals.len()
                    )));
                }
                let count = self
                    .operator
                    .harness_mut(verb)?
                    .put(&keys, &ordinals)
                    .map_err(|error| operator_err(verb, error))?;
                Ok(int_obj(count))
            }
            "op_count" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let key = a.u64(0)?;
                let count = self
                    .operator
                    .harness_mut(verb)?
                    .count(key)
                    .map_err(|error| operator_err(verb, error))?;
                Ok(int_obj(count))
            }
            "op_wait_count" => {
                a.exact(3)?;
                a.no_kwargs()?;
                let instance = a.u64(0)?;
                let key = a.u64(1)?;
                let expected = a.u64(2)?;
                let count = self
                    .operator
                    .harness_mut(verb)?
                    .wait_count(instance, key, expected)
                    .map_err(|error| operator_err(verb, error))?;
                Ok(int_obj(count))
            }
            "op_checkpoint" => {
                a.exact(0)?;
                a.no_kwargs()?;
                let version = self
                    .operator
                    .harness_mut(verb)?
                    .checkpoint()
                    .map_err(|error| operator_err(verb, error))?;
                Ok(int_obj(version))
            }
            "op_fail_primary" => {
                a.exact(0)?;
                a.no_kwargs()?;
                let primary = self
                    .operator
                    .harness_mut(verb)?
                    .fail_primary()
                    .map_err(|error| operator_err(verb, error))?;
                Ok(int_obj(primary))
            }
            "op_wait_promotion" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let old_primary = a.u64(0)?;
                let status = self
                    .operator
                    .harness_mut(verb)?
                    .wait_promotion(old_primary)
                    .map_err(|error| operator_err(verb, error))?;
                Ok(dict(vec![
                    ("phase", MontyObject::String(status.phase)),
                    ("ready_instances", int_obj(status.ready_instances)),
                    ("old_primary", int_obj(status.old_primary)),
                    ("primary_instance", int_obj(status.primary_instance)),
                    ("primary_term", int_obj(status.primary_term)),
                    ("promotion_count", int_obj(status.promotion_count)),
                    ("leader_pods", int_obj(status.leader_pods)),
                    ("follower_pods", int_obj(status.follower_pods)),
                ]))
            }
            "op_delete" => {
                a.exact(0)?;
                a.no_kwargs()?;
                self.operator
                    .harness_mut(verb)?
                    .delete_database()
                    .map_err(|error| operator_err(verb, error))?;
                Ok(MontyObject::None)
            }
            "op_deleted_status" => {
                a.exact(0)?;
                a.no_kwargs()?;
                let status = self
                    .operator
                    .harness_mut(verb)?
                    .deleted_status()
                    .map_err(|error| operator_err(verb, error))?;
                Ok(dict(vec![
                    ("deployment_count", int_obj(status.deployment_count)),
                    ("service_count", int_obj(status.service_count)),
                    ("configmap_count", int_obj(status.configmap_count)),
                    ("certificate_count", int_obj(status.certificate_count)),
                    ("issued_secret_count", int_obj(status.issued_secret_count)),
                    ("pvc_count", int_obj(status.pvc_count)),
                    ("retained_pvc_count", int_obj(status.retained_pvc_count)),
                ]))
            }
            "op_cleanup" => {
                a.exact(0)?;
                a.no_kwargs()?;
                let mut harness =
                    self.operator.harness.take().ok_or_else(|| {
                        value_err("op_cleanup(): op_prepare() has not been called")
                    })?;
                harness
                    .cleanup()
                    .map_err(|error| operator_err(verb, error))?;
                Ok(MontyObject::Bool(true))
            }
            _ => Err(value_err(format!("{verb}() is not an operator verb"))),
        }
    }
}

/// Every `volume_id = "vol-..."` in the generated configurations, sorted.
///
/// Read out of the ConfigMap body the daemon actually mounts, not out of the
/// controller's status. The two are written by the same reconcile, so a status
/// field would be the controller vouching for itself.
fn configured_volume_ids(generated: &str) -> Vec<String> {
    let mut found = Vec::new();
    for line in generated.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != "volume_id" {
            continue;
        }
        let value = value.trim().trim_matches('"');
        if !value.is_empty() {
            found.push(value.to_owned());
        }
    }
    found.sort();
    found
}

fn snapshot_dict(status: SnapshotBackendStatus) -> MontyObject {
    dict(vec![
        ("phase", MontyObject::String(status.phase)),
        ("ready", MontyObject::String(status.ready)),
        (
            "condition_status",
            MontyObject::String(status.condition_status),
        ),
        ("reason", MontyObject::String(status.reason)),
        (
            "snapshot_configured",
            MontyObject::Bool(status.snapshot_configured),
        ),
        (
            "configured_volumes",
            MontyObject::List(
                status
                    .configured_volumes
                    .into_iter()
                    .map(MontyObject::String)
                    .collect(),
            ),
        ),
        (
            "bound_volumes",
            MontyObject::List(
                status
                    .bound_volumes
                    .into_iter()
                    .map(MontyObject::String)
                    .collect(),
            ),
        ),
        ("reconcile_errors", int_obj(status.reconcile_errors)),
        ("settled_instances", int_obj(status.settled_instances)),
        ("specified_instances", int_obj(status.specified_instances)),
    ])
}

fn operator_err(verb: &str, error: impl std::fmt::Display) -> MontyException {
    MontyException::new(
        ExcType::RuntimeError,
        Some(format!("{verb}(): operator harness failed: {error}")),
    )
}

#[derive(Debug)]
struct Config {
    backend: Backend,
    workspace: PathBuf,
    work: PathBuf,
    cluster_name: String,
    kubeconfig: PathBuf,
    operator_image: String,
    server_image: String,
    node_image: String,
    driver_container: Option<String>,
    keep_cluster: bool,
    /// Namespace the `YesnoCluster` and its objects live in.
    namespace: String,
    /// StorageClass for the managed claims. This is the whole difference
    /// between the two backends' storage: local-path against Amazon EBS.
    storage_class: String,
    /// The EBS snapshot backend, present only when the cluster can support it.
    snapshot: Option<SnapshotInputs>,
}

/// What a generated `spec.snapshot` needs that the harness cannot invent.
#[derive(Clone, Debug)]
struct SnapshotInputs {
    region: String,
    /// ServiceAccount carrying the IRSA annotation, created by this harness.
    account: String,
    /// The role that account assumes. Written as an annotation rather than
    /// assumed to exist: the trust policy names this exact namespace and
    /// account, so a mismatch is a failure to obtain credentials at all.
    role_arn: String,
}

impl Config {
    fn from_env(backend: Backend) -> Result<Self> {
        match backend {
            Backend::Kind => Self::kind_from_env(),
            Backend::Eks => Self::eks_from_env(),
        }
    }

    /// The gate's own environment, with every name required.
    ///
    /// No defaults. A missing value here means the AWS gate did not pass
    /// something it owns, and a default would substitute a plausible wrong
    /// answer -- the wrong StorageClass provisions the wrong kind of volume and
    /// the arm's one distinguishing assertion silently stops distinguishing.
    fn eks_from_env() -> Result<Self> {
        let workspace = workspace_root()?;
        let suffix = unique_suffix()?;
        let work = crate::scratch_dir(&workspace).join(format!("operator-{suffix}"));
        fs::create_dir_all(&work)
            .map_err(|error| format!("cannot create {}: {error}", work.display()))?;

        let mut values = std::collections::BTreeMap::new();
        for (name, output) in OPERATOR_ENV {
            let value = env::var(name).map_err(|_| {
                format!("op_prepare(\"eks\"): {name} is unset; the gate fills it from `{output}`")
            })?;
            if value.is_empty() {
                return Err(format!("op_prepare(\"eks\"): {name} is empty"));
            }
            values.insert(*name, value);
        }
        let take = |name: &str| values[name].clone();

        Ok(Self {
            backend: Backend::Eks,
            kubeconfig: PathBuf::from(take(ENV_OPERATOR_KUBECONFIG)),
            operator_image: take(ENV_OPERATOR_IMAGE),
            server_image: take(ENV_OPERATOR_SERVER_IMAGE),
            namespace: take(ENV_OPERATOR_NAMESPACE),
            storage_class: take(ENV_OPERATOR_STORAGE_CLASS),
            snapshot: Some(SnapshotInputs {
                region: take(ENV_OPERATOR_REGION),
                account: take(ENV_OPERATOR_ACCOUNT),
                role_arn: take(ENV_OPERATOR_ROLE_ARN),
            }),
            // Nothing below is used by this backend: the cluster exists, its
            // nodes exist, and no image is built or loaded here.
            node_image: String::new(),
            driver_container: None,
            keep_cluster: false,
            cluster_name: String::new(),
            workspace,
            work,
        })
    }

    fn kind_from_env() -> Result<Self> {
        let workspace = workspace_root()?;
        let temporary_root = crate::scratch_dir(&workspace);
        fs::create_dir_all(&temporary_root).map_err(|error| {
            format!(
                "cannot create operator E2E temporary root {}: {error}",
                temporary_root.display()
            )
        })?;

        let suffix = unique_suffix()?;
        let cluster_name = format!("yesno-e2e-{suffix}");
        let work = temporary_root.join(format!("operator-{suffix}"));
        fs::create_dir(&work)
            .map_err(|error| format!("cannot create {}: {error}", work.display()))?;

        Ok(Self {
            backend: Backend::Kind,
            kubeconfig: work.join("kubeconfig"),
            operator_image: format!("yesno-operator:{cluster_name}"),
            server_image: format!("yesnod:{cluster_name}"),
            node_image: env::var("YESNO_E2E_KIND_NODE_IMAGE")
                .unwrap_or_else(|_| PINNED_NODE_IMAGE.to_owned()),
            driver_container: env::var("YESNO_E2E_DRIVER_CONTAINER").ok(),
            keep_cluster: env::var_os("YESNO_E2E_KEEP_KIND").is_some_and(|value| value == "1"),
            namespace: KIND_NAMESPACE.to_owned(),
            storage_class: KIND_STORAGE_CLASS.to_owned(),
            // `None`, and not because kind cannot be told to try. The
            // operator gate patches the backend on deliberately and asserts
            // the refusal; see `op_enable_ebs_snapshots`.
            snapshot: None,
            workspace,
            work,
            cluster_name,
        })
    }
}

impl Config {
    /// The `--timeout` every readiness wait uses.
    fn wait(&self) -> &'static str {
        match self.backend {
            Backend::Kind => WAIT,
            Backend::Eks => EKS_WAIT,
        }
    }
}

fn workspace_root() -> Result<PathBuf> {
    Ok(PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or("yesno-e2e has no workspace parent")?
        .to_path_buf())
}

struct Harness {
    config: Config,
    cluster_created: bool,
    driver_connected: bool,
    images_built: bool,
    cleaned: bool,
}

impl Harness {
    fn new(config: Config) -> Self {
        Self {
            config,
            cluster_created: false,
            driver_connected: false,
            images_built: false,
            cleaned: false,
        }
    }

    fn prepare(&mut self) -> Result<()> {
        if self.config.backend == Backend::Eks {
            // Everything the kind path does first has already happened, in
            // Terraform and in an ECR push. What is left is what a user would
            // do to an existing cluster.
            self.check_cluster()?;
            return self.install_operator();
        }
        self.check_prerequisites()?;
        self.build_images()?;
        self.create_cluster()?;
        self.load_images()?;
        self.install_operator()
    }

    /// The handed-over cluster answers, and answers as an administrator.
    ///
    /// A `get nodes` rather than a `version`: the gate writes this
    /// kubeconfig from a token it minted, and a token that has expired or was
    /// bound to the wrong ServiceAccount still lets `kubectl version` succeed
    /// against the client alone. This fails here, in one second, instead of
    /// several minutes later while installing a CRD.
    fn check_cluster(&self) -> Result<()> {
        checked_status(
            Command::new("kubectl").args(["version", "--client=true"]),
            "kubectl client",
        )?;
        let nodes = self.kubectl_output(["get", "nodes", "--output=name"], "list cluster nodes")?;
        if nodes.trim().is_empty() {
            return Err("the EKS cluster reports no nodes".into());
        }
        Ok(())
    }

    fn check_prerequisites(&self) -> Result<()> {
        let output = checked_output(Command::new("kind").arg("version"), "kind version")?;
        require_kind_0_33(&output)?;

        checked_status(
            Command::new("kubectl").args(["version", "--client=true"]),
            "kubectl client",
        )?;
        checked_status(Command::new("docker").arg("info"), "Docker daemon")
    }

    fn build_images(&mut self) -> Result<()> {
        let mut server = Command::new("docker");
        server
            .current_dir(&self.config.workspace)
            .env("DOCKER_BUILDKIT", "1")
            .args(["build", "--file"])
            .arg(self.config.workspace.join("yesno-server/dist/Dockerfile"))
            .args(["--tag", &self.config.server_image])
            .arg(&self.config.workspace);
        checked_status(&mut server, "build yesnod image")?;
        self.images_built = true;

        let mut operator = Command::new("docker");
        operator
            .current_dir(&self.config.workspace)
            .env("DOCKER_BUILDKIT", "1")
            .args(["build", "--file"])
            .arg(self.config.workspace.join("yesno-operator/dist/Dockerfile"))
            .args(["--tag", &self.config.operator_image])
            .arg(&self.config.workspace);
        checked_status(&mut operator, "build operator image")
    }

    fn create_cluster(&mut self) -> Result<()> {
        let mut create = self.kind();
        create
            .args(["create", "cluster", "--name", &self.config.cluster_name])
            .args(["--image", &self.config.node_image])
            .arg("--kubeconfig")
            .arg(&self.config.kubeconfig)
            .args(["--wait", WAIT]);
        self.cluster_created = true;
        checked_status(&mut create, "create kind cluster")?;
        self.connect_driver()
    }

    fn connect_driver(&mut self) -> Result<()> {
        let Some(driver) = &self.config.driver_container else {
            return Ok(());
        };
        checked_status(
            Command::new("docker").args(["network", "connect", "kind", driver]),
            "connect E2E driver to kind network",
        )?;
        self.driver_connected = true;

        let cluster_name = self.config.cluster_name.clone();
        let mut kubeconfig = self.kind();
        kubeconfig.args(["get", "kubeconfig", "--internal", "--name", &cluster_name]);
        let contents = checked_output(&mut kubeconfig, "get internal kind kubeconfig")?;
        fs::write(&self.config.kubeconfig, contents).map_err(|error| {
            format!(
                "cannot write internal kubeconfig {}: {error}",
                self.config.kubeconfig.display()
            )
        })
    }

    fn load_images(&self) -> Result<()> {
        let mut load = self.kind();
        load.args(["load", "docker-image"])
            .arg(&self.config.operator_image)
            .arg(&self.config.server_image)
            .args(["--name", &self.config.cluster_name]);
        checked_status(&mut load, "load images into kind")
    }

    fn install_operator(&self) -> Result<()> {
        let crd = self.config.workspace.join("yesno-operator/deploy/crd.yaml");
        self.apply(&crd, "install YesnoCluster CRD")?;
        self.kubectl_status(
            [
                "wait",
                "--for=condition=Established",
                "crd/yesnoclusters.yesnodb.io",
                "--timeout=60s",
            ],
            "wait for CRD establishment",
        )?;

        self.install_cert_manager()?;
        self.kubectl_status(
            ["create", "namespace", &self.config.namespace],
            "create operator E2E namespace",
        )?;
        if let Some(snapshot) = &self.config.snapshot {
            let account = self.config.work.join("service-account.yaml");
            fs::write(&account, service_account_manifest(&self.config, snapshot))
                .map_err(|error| format!("cannot write {}: {error}", account.display()))?;
            self.apply(&account, "create the yesnod ServiceAccount")?;
        }
        let ca = self.config.work.join("ca.yaml");
        fs::write(&ca, ca_manifest(&self.config.namespace))
            .map_err(|error| format!("cannot write {}: {error}", ca.display()))?;
        // **Retried, because `condition=Available` on the webhook Deployment
        // does not mean the webhook is answering.** `install_cert_manager`
        // waits for all three Deployments to go Available, and that is still
        // too early: the API server also needs the webhook Service's endpoints
        // registered and the CA bundle injected into its
        // ValidatingWebhookConfiguration by cainjector. Until both land, the
        // first cert-manager-validated resource fails with
        // `Internal error occurred: failed calling webhook`.
        //
        // Observed on 2026-09-08: four consecutive gate runs got past this
        // point and the fifth did not, on a loaded host. Do not "fix" it by
        // waiting longer on the Deployments -- they were already Available; the
        // readiness that matters is not expressed as a Deployment condition.
        // The CA is the first such resource the gate creates, so retrying it is
        // what closes the window for everything after it.
        self.apply_retrying(&ca, "create cert-manager test CA", Duration::from_secs(60))?;
        self.kubectl_status(
            [
                "wait",
                "--namespace",
                &self.config.namespace,
                "--for=condition=Ready",
                "certificate/yesno-root-ca",
                "--timeout=60s",
            ],
            "wait for cert-manager test CA",
        )?;

        let source = fs::read_to_string(
            self.config
                .workspace
                .join("yesno-operator/deploy/operator.yaml"),
        )
        .map_err(|error| format!("cannot read operator manifest: {error}"))?;
        let rendered = render_operator_manifest(&source, &self.config.operator_image)?;
        let manifest = self.config.work.join("operator.yaml");
        fs::write(&manifest, rendered)
            .map_err(|error| format!("cannot write {}: {error}", manifest.display()))?;
        self.apply(&manifest, "install operator")?;
        self.kubectl_status(
            [
                "wait",
                "--namespace=yesno-system",
                "--for=condition=Available",
                "deployment/yesno-operator",
                &format!("--timeout={}", self.config.wait()),
            ],
            "wait for operator",
        )?;
        Ok(())
    }

    fn install_cert_manager(&self) -> Result<()> {
        let source = env::var("YESNO_E2E_CERT_MANAGER_MANIFEST")
            .unwrap_or_else(|_| CERT_MANAGER_MANIFEST.into());
        let mut apply = self.kubectl();
        apply.args(["apply", "--filename", &source]);
        checked_status(&mut apply, "install cert-manager v1.21.1")?;
        for deployment in [
            "cert-manager",
            "cert-manager-cainjector",
            "cert-manager-webhook",
        ] {
            self.kubectl_status(
                [
                    "wait",
                    "--namespace=cert-manager",
                    "--for=condition=Available",
                    &format!("deployment/{deployment}"),
                    &format!("--timeout={}", self.config.wait()),
                ],
                &format!("wait for {deployment}"),
            )?;
        }
        Ok(())
    }

    fn create_invalid(&self) -> Result<()> {
        let manifest = self.config.work.join("invalid.yaml");
        fs::write(&manifest, invalid_cluster_manifest(&self.config))
            .map_err(|error| format!("cannot write {}: {error}", manifest.display()))?;
        self.apply(&manifest, "create invalid YesnoCluster")?;
        self.kubectl_status(
            [
                "wait",
                "--namespace",
                &self.config.namespace,
                "--for=jsonpath={.status.phase}=Invalid",
                &format!("yesnocluster/{INVALID_CLUSTER}"),
                "--timeout=60s",
            ],
            "wait for Invalid status",
        )
    }

    fn invalid_status(&self) -> Result<InvalidStatus> {
        Ok(InvalidStatus {
            phase: self.get_value("yesnocluster", INVALID_CLUSTER, "{.status.phase}")?,
            deployment_exists: self.resource_exists("deployment", INVALID_CLUSTER)?,
            pvc_exists: self.resource_exists("pvc", &format!("{INVALID_CLUSTER}-data"))?,
        })
    }

    fn delete_invalid(&self) -> Result<()> {
        self.kubectl_status(
            [
                "delete",
                "--namespace",
                &self.config.namespace,
                "yesnocluster",
                INVALID_CLUSTER,
                "--wait=true",
            ],
            "delete invalid YesnoCluster",
        )
    }

    fn create_database(&self) -> Result<()> {
        let manifest = self.config.work.join("cluster.yaml");
        fs::write(&manifest, cluster_manifest(&self.config))
            .map_err(|error| format!("cannot write {}: {error}", manifest.display()))?;
        self.apply(&manifest, "create valid YesnoCluster")?;
        self.kubectl_status(
            [
                "wait",
                "--namespace",
                &self.config.namespace,
                "--for=condition=Ready",
                &format!("yesnocluster/{CLUSTER}"),
                &format!("--timeout={}", self.config.wait()),
            ],
            "wait for YesnoCluster readiness",
        )?;
        let client = self.config.work.join("client.yaml");
        fs::write(&client, client_pod_manifest(&self.config))
            .map_err(|error| format!("cannot write {}: {error}", client.display()))?;
        self.apply(&client, "create mTLS client Pod")?;
        self.kubectl_status(
            [
                "wait",
                "--namespace",
                &self.config.namespace,
                "--for=condition=Ready",
                "pod/yesno-client",
                &format!("--timeout={}", self.config.wait()),
            ],
            "wait for mTLS client Pod",
        )
    }

    fn database_status(&self) -> Result<ClusterStatus> {
        let status = self.get_value(
            "yesnocluster",
            CLUSTER,
            "{.status.phase}|{.status.readyInstances}|{.status.primaryInstance}|{.status.primaryTerm}|{.status.promotionCount}|{.status.endpoint}|{.status.dataClaim}|{.status.clientSecret}",
        )?;
        let fields: Vec<_> = status.split('|').collect();
        if fields.len() != 8 {
            return Err(format!("unexpected YesnoCluster status {status:?}"));
        }
        let deployments = self.list_value(
            "deployment",
            &format!("app.kubernetes.io/instance={CLUSTER}"),
            "{range .items[*]}{.spec.strategy.type}|{.spec.replicas}{\"\\n\"}{end}",
        )?;
        let deployment_rows: Vec<_> = deployments.lines().collect();
        let pvcs = self.list_value(
            "pvc",
            &format!("app.kubernetes.io/instance={CLUSTER}"),
            "{range .items[*]}{.metadata.annotations.yesnodb\\.io/retention}|{.spec.storageClassName}|{.spec.volumeMode}{\"\\n\"}{end}",
        )?;
        let pvc_rows: Vec<_> = pvcs.lines().collect();
        if pvc_rows.is_empty() || pvc_rows.iter().any(|row| row.split('|').count() != 3) {
            return Err(format!("unexpected PVC status {pvcs:?}"));
        }
        let first_pvc: Vec<_> = pvc_rows[0].split('|').collect();
        let roles = self.pod_role_counts()?;
        Ok(ClusterStatus {
            phase: fields[0].to_owned(),
            ready_instances: parse_u64("readyInstances", fields[1])?,
            primary_instance: parse_u64("primaryInstance", fields[2])?,
            primary_term: parse_u64("primaryTerm", fields[3])?,
            promotion_count: parse_u64("promotionCount", fields[4])?,
            endpoint: fields[5].to_owned(),
            data_claim: fields[6].to_owned(),
            client_secret: fields[7].to_owned(),
            certificate_count: self.resource_count(
                "certificate",
                &format!("app.kubernetes.io/instance={CLUSTER}"),
            )?,
            issued_secret_count: self
                .resource_count("secret", &format!("app.kubernetes.io/instance={CLUSTER}"))?,
            deployment_count: deployment_rows.len() as u64,
            all_recreate: deployment_rows
                .iter()
                .all(|row| row.split('|').next() == Some("Recreate")),
            all_replicas_one: deployment_rows
                .iter()
                .all(|row| row.split('|').nth(1) == Some("1")),
            leader_pods: roles.0,
            follower_pods: roles.1,
            pvc_count: pvc_rows.len() as u64,
            retention: first_pvc[0].to_owned(),
            storage_class: first_pvc[1].to_owned(),
            volume_mode: first_pvc[2].to_owned(),
        })
    }

    fn put(&self, keys: &[u64], ordinals: &[u64]) -> Result<u64> {
        if keys.len() != ordinals.len() {
            return Err(format!(
                "key and ordinal lists differ in length: {} != {}",
                keys.len(),
                ordinals.len()
            ));
        }
        let mut input = String::new();
        for (key, ordinal) in keys.iter().zip(ordinals) {
            use std::fmt::Write as _;
            writeln!(&mut input, "{key},{ordinal}")
                .map_err(|error| format!("cannot construct yesno input: {error}"))?;
        }
        let output =
            self.exec_yesno_with_input(["put", "-"], &input, "ingest through the managed Pod")?;
        let expected = format!("ingested {} of {} pairs", keys.len(), keys.len());
        assert_output("yesno put", &output, &expected)?;
        Ok(keys.len() as u64)
    }

    fn count(&self, key: u64) -> Result<u64> {
        let key = key.to_string();
        let output = self.exec_yesno(["count", &key], "count managed data")?;
        parse_u64("yesno count", output.trim())
    }

    fn wait_count(&self, instance: u64, key: u64, expected: u64) -> Result<u64> {
        let deadline = Instant::now() + Duration::from_secs(180);
        let key = key.to_string();
        let mut last = String::new();
        while Instant::now() < deadline {
            match self.exec_yesno_instance(
                instance,
                ["count", &key],
                "wait for managed instance count",
            ) {
                Ok(output) => match parse_u64("yesno count", output.trim()) {
                    Ok(count) if count == expected => return Ok(count),
                    Ok(count) => last = format!("observed count {count}"),
                    Err(error) => last = error,
                },
                Err(error) => last = error,
            }
            thread::sleep(Duration::from_secs(1));
        }
        Err(format!(
            "instance {instance} did not report count {expected} for key {key} within 180s; last observation: {last}"
        ))
    }

    fn checkpoint(&self) -> Result<u64> {
        let output = self.exec_yesnoctl(["checkpoint"], "checkpoint managed data")?;
        let value = output
            .trim()
            .strip_prefix("checkpoint at version ")
            .ok_or_else(|| {
                format!(
                    "checkpoint returned {:?}, expected a checkpoint watermark",
                    output.trim()
                )
            })?;
        parse_u64("checkpoint version", value)
    }

    fn fail_primary(&self) -> Result<u64> {
        let primary = self.primary_instance()?;
        self.kubectl_status(
            [
                "scale",
                "--namespace",
                &self.config.namespace,
                &format!("deployment/{CLUSTER}-{primary}"),
                "--replicas=0",
            ],
            "make the current primary unavailable",
        )?;
        Ok(primary)
    }

    /// Switch the running cluster to the EBS snapshot backend and report what
    /// the controller made of it.
    ///
    /// **kind has no EBS, and that is what this checks.** Every step up to
    /// the volume is real -- the CRD accepts the spec, the controller's own
    /// ClusterRole permits the cluster-scoped PersistentVolume read, and the
    /// discovery walks claim to volume for each instance. Only the answer
    /// differs from EKS: kind's `standard` class provisions a local-path
    /// volume, so the correct outcome is a reported refusal rather than a
    /// guess. A controller that invented a volume id from a `hostPath` volume,
    /// or that took the whole reconcile down when it could not read a
    /// PersistentVolume, fails here.
    fn enable_ebs_snapshots(&self) -> Result<SnapshotBackendStatus> {
        let patch = "{\"spec\":{\"snapshot\":{\"backend\":\"ebs\",\"ebs\":\
                     {\"region\":\"ap-northeast-1\",\"filesystem\":\"ext4\"}}}}";
        self.kubectl_status(
            [
                "patch",
                "--namespace",
                &self.config.namespace,
                "yesnocluster",
                CLUSTER,
                "--type=merge",
                "--patch",
                patch,
            ],
            "enable the EBS snapshot backend",
        )?;

        self.wait_snapshot_backend()
    }

    /// Wait until `SnapshotBackendReady` reaches an answer, and report it.
    ///
    /// `AwaitingVolumeBinding` is not an answer, it is the window: a claim
    /// binds when its Pod is scheduled and the controller picks the volume up
    /// on the next reconcile. Returning it would make both arms pass against a
    /// controller that never resolved anything.
    ///
    /// Long enough for EBS. On kind the volume is local and this settles in
    /// a second or two; on EKS the CSI driver has to create and attach a real
    /// volume first, and the Pod is then restarted once by the configuration
    /// change that follows.
    fn wait_snapshot_backend(&self) -> Result<SnapshotBackendStatus> {
        let deadline = Instant::now() + SNAPSHOT_CONDITION_WAIT;
        let mut last = String::new();
        while Instant::now() < deadline {
            match self.snapshot_backend_status() {
                // Three conditions, not one. The reason has to be an answer
                // rather than the window; the cluster has to be serving again
                // after the restart that answer causes; and every instance's
                // running Pod has to carry the configuration that came with it.
                // Returning on the reason alone would report a configuration
                // nothing had read yet.
                Ok(status)
                    if !status.reason.is_empty()
                        && status.reason != "AwaitingVolumeBinding"
                        && status.phase == "Ready"
                        && status.specified_instances > 0
                        && status.settled_instances == status.specified_instances =>
                {
                    return Ok(status);
                }
                Ok(status) => {
                    last = format!(
                        "phase={} ready={} condition={} reason={:?} settled={}/{}",
                        status.phase,
                        status.ready,
                        status.condition_status,
                        status.reason,
                        status.settled_instances,
                        status.specified_instances
                    );
                }
                Err(error) => last = error,
            }
            thread::sleep(Duration::from_secs(1));
        }
        Err(format!(
            "the SnapshotBackendReady condition did not settle within {}s; last observation: {last}",
            SNAPSHOT_CONDITION_WAIT.as_secs()
        ))
    }

    fn snapshot_backend_status(&self) -> Result<SnapshotBackendStatus> {
        let status = self.get_value(
            "yesnocluster",
            CLUSTER,
            "{.status.phase}|\
             {.status.conditions[?(@.type=='Ready')].status}|\
             {.status.conditions[?(@.type=='SnapshotBackendReady')].status}|\
             {.status.conditions[?(@.type=='SnapshotBackendReady')].reason}",
        )?;
        let fields: Vec<_> = status.split('|').collect();
        if fields.len() != 4 {
            return Err(format!("unexpected YesnoCluster status {status:?}"));
        }
        let (settled, specified) = self.rollout_progress()?;
        // Read from the ConfigMap the daemon actually mounts, not from the
        // controller's own report of itself.
        let generated = self.list_value(
            "configmap",
            &format!("app.kubernetes.io/instance={CLUSTER}"),
            "{range .items[*]}{.data.yesnod\\.toml}{\"\\n\"}{end}",
        )?;
        Ok(SnapshotBackendStatus {
            phase: fields[0].to_owned(),
            ready: fields[1].to_owned(),
            condition_status: fields[2].to_owned(),
            reason: fields[3].to_owned(),
            snapshot_configured: generated.contains("[server.snapshot]"),
            configured_volumes: configured_volume_ids(&generated),
            bound_volumes: self.bound_volume_handles()?,
            reconcile_errors: self.reconcile_error_count()?,
            settled_instances: settled,
            specified_instances: specified,
        })
    }

    /// Per instance: the configuration its Deployment specifies, and the
    /// configuration its running Pod was started with.
    ///
    /// The operator stamps a `yesnodb.io/config-identity` annotation on the Pod
    /// template, derived from the ConfigMap body, precisely so that a changed
    /// configuration is a changed template. Comparing the Pod's copy with the
    /// Deployment's is therefore the question "is the daemon running what the
    /// controller last decided", asked in the two places that can disagree.
    fn rollout_progress(&self) -> Result<(u64, u64)> {
        let selector = format!("app.kubernetes.io/instance={CLUSTER}");
        let specified = self.list_value(
            "deployment",
            &selector,
            "{range .items[*]}{.metadata.labels.yesnodb\\.io/instance}|\
             {.spec.template.metadata.annotations.yesnodb\\.io/config-identity}{\"\\n\"}{end}",
        )?;
        let running = self.list_value(
            "pod",
            &selector,
            "{range .items[*]}{.metadata.labels.yesnodb\\.io/instance}|\
             {.metadata.annotations.yesnodb\\.io/config-identity}|\
             {.status.phase}{\"\\n\"}{end}",
        )?;
        let wanted: Vec<&str> = specified
            .lines()
            .map(str::trim)
            .filter(|row| !row.is_empty())
            .collect();
        let mut settled = 0;
        for row in &wanted {
            // A Pod matches only when it is Running *and* carries this
            // instance's identity. A terminating Pod from the previous
            // generation still answers a plain list.
            if running.lines().map(str::trim).any(|pod| {
                pod.strip_suffix("|Running")
                    .is_some_and(|prefix| prefix == *row)
            }) {
                settled += 1;
            }
        }
        Ok((settled, wanted.len() as u64))
    }

    /// The EBS volume behind each instance's claim, straight from Kubernetes.
    fn bound_volume_handles(&self) -> Result<Vec<String>> {
        let names = self.list_value(
            "pvc",
            &format!("app.kubernetes.io/instance={CLUSTER}"),
            "{range .items[*]}{.spec.volumeName}{\"\\n\"}{end}",
        )?;
        let mut handles = Vec::new();
        for name in names.lines().map(str::trim).filter(|name| !name.is_empty()) {
            // Cluster-scoped, so no `--namespace`, and read with the same
            // jsonpath for both backends: on kind this is simply empty, which
            // is the honest answer for a local-path volume.
            let handle = self.kubectl_output(
                [
                    "get",
                    "pv",
                    name,
                    "--output",
                    "jsonpath={.spec.csi.volumeHandle}",
                ],
                &format!("read PersistentVolume {name}"),
            )?;
            let handle = handle.trim();
            if !handle.is_empty() {
                handles.push(handle.to_owned());
            }
        }
        handles.sort();
        Ok(handles)
    }

    /// How many times a daemon has said it cannot reach its snapshot provider.
    ///
    /// One-sided, and the scenario says so where it asserts on it. Startup
    /// reconciliation runs in a background task that retries with backoff, so a
    /// Pod with no usable AWS identity is Ready, serving, and wrong -- this
    /// line is the only thing it emits. Zero here does not prove EC2 was
    /// reached; a non-zero count proves it was not.
    fn reconcile_error_count(&self) -> Result<u64> {
        let logs = self.kubectl_output(
            [
                "logs",
                "--namespace",
                &self.config.namespace,
                "--selector",
                &format!("app.kubernetes.io/instance={CLUSTER}"),
                "--all-containers=true",
                "--tail=400",
            ],
            "read database logs",
        )?;
        Ok(logs
            .lines()
            .filter(|line| line.contains("cannot reconcile abandoned base snapshots"))
            .count() as u64)
    }

    fn wait_promotion(&self, old_primary: u64) -> Result<PromotionStatus> {
        let deadline = Instant::now() + Duration::from_secs(180);
        let mut last = String::new();
        while Instant::now() < deadline {
            match self.promotion_status(old_primary) {
                Ok(status)
                    if status.phase == "Ready"
                        && status.ready_instances == 2
                        && status.primary_instance != old_primary
                        && status.promotion_count >= 1
                        && status.leader_pods == 1
                        && status.follower_pods == 1 =>
                {
                    return Ok(status);
                }
                Ok(status) => {
                    last = format!(
                        "phase={} ready={} primary={} term={} promotions={} leaderPods={} followerPods={}",
                        status.phase,
                        status.ready_instances,
                        status.primary_instance,
                        status.primary_term,
                        status.promotion_count,
                        status.leader_pods,
                        status.follower_pods
                    );
                }
                Err(error) => last = error,
            }
            thread::sleep(Duration::from_secs(1));
        }
        Err(format!(
            "automatic promotion did not complete within 180s; last observation: {last}"
        ))
    }

    fn promotion_status(&self, old_primary: u64) -> Result<PromotionStatus> {
        let status = self.get_value(
            "yesnocluster",
            CLUSTER,
            "{.status.phase}|{.status.readyInstances}|{.status.primaryInstance}|{.status.primaryTerm}|{.status.promotionCount}",
        )?;
        let fields: Vec<_> = status.split('|').collect();
        if fields.len() != 5 {
            return Err(format!("unexpected promotion status {status:?}"));
        }
        let roles = self.pod_role_counts()?;
        Ok(PromotionStatus {
            phase: fields[0].to_owned(),
            ready_instances: parse_u64("readyInstances", fields[1])?,
            old_primary,
            primary_instance: parse_u64("primaryInstance", fields[2])?,
            primary_term: parse_u64("primaryTerm", fields[3])?,
            promotion_count: parse_u64("promotionCount", fields[4])?,
            leader_pods: roles.0,
            follower_pods: roles.1,
        })
    }

    fn delete_database(&self) -> Result<()> {
        self.kubectl_status(
            [
                "delete",
                "--namespace",
                &self.config.namespace,
                "yesnocluster",
                CLUSTER,
                "--wait=true",
            ],
            "delete valid YesnoCluster",
        )
    }

    fn deleted_status(&self) -> Result<DeletedStatus> {
        let selector = format!("app.kubernetes.io/instance={CLUSTER}");
        self.wait_selector_empty("deployment", &selector, Duration::from_secs(120))?;
        self.wait_selector_empty("service", &selector, Duration::from_secs(120))?;
        self.wait_selector_empty("configmap", &selector, Duration::from_secs(120))?;
        self.wait_selector_empty("certificate", &selector, Duration::from_secs(120))?;
        self.wait_selector_empty("secret", &selector, Duration::from_secs(120))?;
        let pvc_annotations = self.list_value(
            "pvc",
            &selector,
            "{range .items[*]}{.metadata.annotations.yesnodb\\.io/retention}{\"\\n\"}{end}",
        )?;
        let retained_pvc_count = pvc_annotations
            .lines()
            .filter(|annotation| *annotation == "retained-after-cluster-deletion")
            .count() as u64;
        Ok(DeletedStatus {
            deployment_count: self.resource_count("deployment", &selector)?,
            service_count: self.resource_count("service", &selector)?,
            configmap_count: self.resource_count("configmap", &selector)?,
            certificate_count: self.resource_count("certificate", &selector)?,
            issued_secret_count: self.resource_count("secret", &selector)?,
            pvc_count: self.resource_count("pvc", &selector)?,
            retained_pvc_count,
        })
    }

    fn kind(&self) -> Command {
        let mut command = Command::new("kind");
        command
            .current_dir(&self.config.workspace)
            .env("KIND_EXPERIMENTAL_PROVIDER", "docker");
        command
    }

    fn kubectl(&self) -> Command {
        let mut command = Command::new("kubectl");
        command
            .current_dir(&self.config.workspace)
            .arg("--kubeconfig")
            .arg(&self.config.kubeconfig);
        command
    }

    fn kubectl_status<I, S>(&self, args: I, label: &str) -> Result<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let mut command = self.kubectl();
        command.args(args);
        checked_status(&mut command, label)
    }

    fn kubectl_output<I, S>(&self, args: I, label: &str) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let mut command = self.kubectl();
        command.args(args);
        checked_output(&mut command, label)
    }

    /// [`apply`](Self::apply), retried until `budget` expires.
    ///
    /// For the admission-webhook window only. A resource whose *content* is
    /// wrong fails identically on every attempt and simply reports that error
    /// after the budget, so this delays a genuine failure by `budget` and never
    /// hides one. Do not reach for it to paper over an ordering bug that has
    /// a condition to wait on -- use the condition.
    fn apply_retrying(&self, path: &Path, label: &str, budget: Duration) -> Result<()> {
        let deadline = Instant::now() + budget;
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            let mut command = self.kubectl();
            command.args(["apply", "--filename"]).arg(path);
            match checked_status(&mut command, label) {
                Ok(()) => return Ok(()),
                Err(error) => {
                    if Instant::now() >= deadline {
                        return Err(format!("{error} ( after {attempt} attempts )"));
                    }
                    std::thread::sleep(Duration::from_secs(2));
                }
            }
        }
    }

    fn apply(&self, path: &Path, label: &str) -> Result<()> {
        let mut command = self.kubectl();
        command.args(["apply", "--filename"]).arg(path);
        checked_status(&mut command, label)
    }

    fn resource_exists(&self, kind: &str, name: &str) -> Result<bool> {
        let output = self.kubectl_output(
            [
                "get",
                "--namespace",
                &self.config.namespace,
                kind,
                name,
                "--ignore-not-found",
                "--output=name",
            ],
            &format!("check presence of {kind}/{name}"),
        )?;
        Ok(!output.trim().is_empty())
    }

    fn get_value(&self, kind: &str, name: &str, jsonpath: &str) -> Result<String> {
        let output = self.kubectl_output(
            [
                "get",
                "--namespace",
                &self.config.namespace,
                kind,
                name,
                "--output",
                &format!("jsonpath={jsonpath}"),
            ],
            &format!("read {kind}/{name}"),
        )?;
        Ok(output.trim().to_owned())
    }

    fn list_value(&self, kind: &str, selector: &str, jsonpath: &str) -> Result<String> {
        self.kubectl_output(
            [
                "get",
                "--namespace",
                &self.config.namespace,
                kind,
                "--selector",
                selector,
                "--output",
                &format!("jsonpath={jsonpath}"),
            ],
            &format!("list {kind} for {CLUSTER}"),
        )
    }

    fn resource_count(&self, kind: &str, selector: &str) -> Result<u64> {
        let output = self.list_value(kind, selector, "{range .items[*]}x{\"\\n\"}{end}")?;
        Ok(output.lines().count() as u64)
    }

    fn primary_instance(&self) -> Result<u64> {
        let value = self.get_value("yesnocluster", CLUSTER, "{.status.primaryInstance}")?;
        parse_u64("primaryInstance", &value)
    }

    fn pod_role_counts(&self) -> Result<(u64, u64)> {
        let output = self.list_value(
            "pod",
            &format!("app.kubernetes.io/instance={CLUSTER}"),
            "{range .items[*]}{.metadata.labels.yesnodb\\.io/role}{\"\\n\"}{end}",
        )?;
        Ok((
            output.lines().filter(|role| *role == "leader").count() as u64,
            output.lines().filter(|role| *role == "follower").count() as u64,
        ))
    }

    fn exec_yesno<const N: usize>(&self, args: [&str; N], label: &str) -> Result<String> {
        let primary = self.primary_instance()?;
        self.exec_yesno_instance(primary, args, label)
    }

    fn exec_yesno_instance<const N: usize>(
        &self,
        instance: u64,
        args: [&str; N],
        label: &str,
    ) -> Result<String> {
        let mut command = self.kubectl();
        command
            .args([
                "exec",
                "--namespace",
                &self.config.namespace,
                "pod/yesno-client",
                "--",
            ])
            .args([
                "yesno",
                "--endpoint",
                &format!("https://{CLUSTER}-{instance}:50051"),
                "--ca",
                "/tls/ca/ca.crt",
                "--cert",
                "/tls/client/tls.crt",
                "--key",
                "/tls/client/tls.key",
                "--server-name",
                &format!("{CLUSTER}-{instance}"),
            ])
            .args(args);
        checked_output(&mut command, label)
    }

    fn exec_yesnoctl<const N: usize>(&self, args: [&str; N], label: &str) -> Result<String> {
        let mut command = self.kubectl();
        command
            .args([
                "exec",
                "--namespace",
                &self.config.namespace,
                "pod/yesno-client",
                "--",
            ])
            .args(["yesnoctl"])
            .args(args)
            .args([
                "--endpoint",
                &format!("https://{CLUSTER}-rw:50052"),
                "--ca",
                "/tls/ca/ca.crt",
                "--cert",
                "/tls/client/tls.crt",
                "--key",
                "/tls/client/tls.key",
                "--server-name",
                &format!("{CLUSTER}-rw"),
            ]);
        checked_output(&mut command, label)
    }

    fn exec_yesno_with_input<const N: usize>(
        &self,
        args: [&str; N],
        input: &str,
        label: &str,
    ) -> Result<String> {
        let mut command = self.kubectl();
        command
            .args([
                "exec",
                "--stdin",
                "--namespace",
                &self.config.namespace,
                "pod/yesno-client",
                "--",
                "yesno",
                "--endpoint",
                &format!("https://{CLUSTER}-rw:50051"),
                "--ca",
                "/tls/ca/ca.crt",
                "--cert",
                "/tls/client/tls.crt",
                "--key",
                "/tls/client/tls.key",
                "--server-name",
                &format!("{CLUSTER}-rw"),
            ])
            .args(args);
        checked_input(&mut command, input, label)
    }

    fn wait_selector_empty(&self, kind: &str, selector: &str, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.resource_count(kind, selector)? == 0 {
                return Ok(());
            }
            thread::sleep(Duration::from_secs(1));
        }
        Err(format!(
            "{kind} resources matching {selector:?} still exist after {}s",
            timeout.as_secs()
        ))
    }

    fn diagnose(&self) {
        // Not `cluster_created`: an EKS run never creates one, and it is the
        // arm whose failures cost the most to reproduce. The condition is
        // whether there is a cluster to ask, not whether this harness made it.
        if !self.cluster_created && self.config.backend != Backend::Eks {
            return;
        }
        // **Ordered by what answers a failure, because the transport
        // truncates.** The AWS arm ships this through Systems Manager, which
        // caps a command's output at 24 KB -- so a section placed last is a
        // section that is not there. A live run on 2026-09-05 lost the end of
        // the database log to exactly that, and the lost part was the answer.
        // Do not move the object dump back to the top: it is the longest
        // section and the least specific.
        let selector = format!("app.kubernetes.io/instance={CLUSTER}");

        // First, because it settles a question the logs cannot. A restarted
        // Pod and a crash-looping container produce the same log -- the
        // container's own output since it last started -- and RESTARTS is what
        // tells them apart.
        eprintln!("\n--- database pods ---");
        let mut pods = self.kubectl();
        pods.args([
            "get",
            "pods",
            "--namespace",
            &self.config.namespace,
            "--selector",
            &selector,
            "--output=wide",
        ]);
        best_effort(&mut pods);

        eprintln!("\n--- database log ---");
        let mut database = self.kubectl();
        database.args([
            "logs",
            "--namespace",
            &self.config.namespace,
            "--selector",
            &selector,
            "--all-containers=true",
            // Without this the two instances' lines are concatenated with
            // nothing saying which said what, and a leader/follower failure is
            // unreadable. The same run that lost the tail also could not tell
            // whose log it was looking at.
            "--prefix",
            "--tail=120",
        ]);
        best_effort(&mut database);

        // A container that died took its explanation with it: `kubectl logs`
        // without this shows only what the *current* one has said since it
        // started, which for a crash loop is the symptom and never the cause.
        eprintln!("\n--- database log, previous container ---");
        for pod in self.pod_names(&selector) {
            let mut previous = self.kubectl();
            previous.args([
                "logs",
                "--namespace",
                &self.config.namespace,
                &pod,
                "--previous",
                "--prefix",
                "--tail=60",
            ]);
            // Fails when there is no previous container, which is the ordinary
            // case and not worth reporting as a diagnostic failure.
            best_effort(&mut previous);
        }

        eprintln!("\n--- events (warnings) ---");
        let mut events = self.kubectl();
        events.args([
            "get",
            "events",
            "--namespace",
            &self.config.namespace,
            "--field-selector",
            "type!=Normal",
            "--sort-by=.lastTimestamp",
        ]);
        best_effort(&mut events);

        eprintln!("\n--- operator log ---");
        let mut operator = self.kubectl();
        operator.args([
            "logs",
            "--namespace=yesno-system",
            "deployment/yesno-operator",
            "--tail=80",
        ]);
        best_effort(&mut operator);

        // The cluster's own status, in full, and **before** the object table.
        //
        // A promotion failure on 2026-09-08 could not be diagnosed from this
        // dump at all. The operator was reconciling every second -- visibly so,
        // once its `RUST_LOG` was raised -- and every artifact here reported
        // only `phase=FailingOver`. The thing that distinguishes "waiting for
        // the old primary's Pods to disappear" from "promotion was attempted
        // and failed" is `status.failover.stage` and `status.reason`, which the
        // CR carries and nothing printed: the table below is `--output=wide`,
        // whose columns are NAME/PHASE/READY/PRIMARY/ENDPOINT, and the operator
        // does not log the stage even at debug.
        //
        // Do not replace this with more columns. The failover sub-object is
        // nested, so a column set that covers today's question will not cover
        // the next one; the whole status is a few lines and always answers it.
        eprintln!("\n--- YesnoCluster status ---");
        let mut status = self.kubectl();
        status.args([
            "get",
            "yesnocluster",
            CLUSTER,
            "--namespace",
            &self.config.namespace,
            "--output=jsonpath={.status}",
        ]);
        best_effort(&mut status);
        eprintln!();

        eprintln!("\n--- Kubernetes objects ---");
        let mut objects = self.kubectl();
        objects.args([
            "get",
            "yesnoclusters,deployments,pods,persistentvolumeclaims,services",
            "--namespace",
            &self.config.namespace,
            "--output=wide",
        ]);
        best_effort(&mut objects);

        if self.config.backend != Backend::Kind {
            return;
        }
        let logs = self.config.work.join("kind-logs");
        let mut export = self.kind();
        export
            .args(["export", "logs"])
            .arg(&logs)
            .args(["--name", &self.config.cluster_name]);
        if export.status().is_ok_and(|status| status.success()) {
            eprintln!("kind diagnostics: {}", logs.display());
        }
    }

    /// Pod names for one selector, or nothing when the API cannot be reached.
    ///
    /// Best effort by design: this feeds diagnostics that run *because*
    /// something already failed, and a diagnostic that itself returns an error
    /// reports nothing at the moment it is most wanted.
    fn pod_names(&self, selector: &str) -> Vec<String> {
        self.kubectl_output(
            [
                "get",
                "pods",
                "--namespace",
                &self.config.namespace,
                "--selector",
                selector,
                "--output=name",
            ],
            "list database pods",
        )
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect()
    }

    fn cleanup(&mut self) -> Result<()> {
        if self.cleaned {
            return Ok(());
        }
        if self.config.backend == Backend::Eks {
            // Deliberately nothing. The cluster, its node group and its
            // volumes belong to Terraform, which destroys them with the rest of
            // the run -- and on a retained run the objects left here are the
            // whole of what a post-mortem has to look at. Deleting the
            // namespace would take the events with it.
            self.cleaned = true;
            return Ok(());
        }
        if self.config.keep_cluster {
            println!(
                "keeping kind cluster {}; kubeconfig: {}",
                self.config.cluster_name,
                self.config.kubeconfig.display()
            );
            self.cluster_created = false;
            self.cleaned = true;
            return Ok(());
        }

        let mut failures = Vec::new();
        if self.driver_connected {
            let driver = self
                .config
                .driver_container
                .as_deref()
                .expect("a connected driver has a container name");
            if let Err(error) = checked_status(
                Command::new("docker").args(["network", "disconnect", "kind", driver]),
                "disconnect E2E driver from kind network",
            ) {
                failures.push(error);
            } else {
                self.driver_connected = false;
            }
        }
        if self.cluster_created {
            let mut delete = self.kind();
            delete.args(["delete", "cluster", "--name", &self.config.cluster_name]);
            if let Err(error) = checked_status(&mut delete, "delete kind cluster") {
                failures.push(error);
            } else {
                self.cluster_created = false;
            }
        }

        if self.images_built {
            for image in [&self.config.operator_image, &self.config.server_image] {
                let _ = Command::new("docker")
                    .args(["image", "rm", "--force", image])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
            self.images_built = false;
        }

        if !failures.is_empty() {
            return Err(failures.join("\n"));
        }
        fs::remove_dir_all(&self.config.work)
            .map_err(|error| format!("cannot remove {}: {error}", self.config.work.display()))?;
        self.cleaned = true;
        Ok(())
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        if self.cleaned || self.config.keep_cluster {
            return;
        }
        self.diagnose();
        if let Err(error) = self.cleanup() {
            eprintln!("operator E2E cleanup failed: {error}");
        }
    }
}

fn checked_status(command: &mut Command, label: &str) -> Result<()> {
    println!("== {label}");
    let status = command
        .status()
        .map_err(|error| format!("cannot run {label}: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{label} exited with {status}"))
    }
}

fn checked_output(command: &mut Command, label: &str) -> Result<String> {
    let output = command
        .output()
        .map_err(|error| format!("cannot run {label}: {error}"))?;
    output_text(output, label)
}

fn checked_input(command: &mut Command, input: &str, label: &str) -> Result<String> {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| format!("cannot run {label}: {error}"))?;
    child
        .stdin
        .take()
        .ok_or_else(|| format!("{label} has no stdin"))?
        .write_all(input.as_bytes())
        .map_err(|error| format!("cannot write input for {label}: {error}"))?;
    let output = child
        .wait_with_output()
        .map_err(|error| format!("cannot wait for {label}: {error}"))?;
    output_text(output, label)
}

fn output_text(output: Output, label: &str) -> Result<String> {
    if output.status.success() {
        String::from_utf8(output.stdout)
            .map_err(|error| format!("{label} emitted non-UTF-8 output: {error}"))
    } else {
        Err(format!(
            "{label} exited with {}:\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

fn best_effort(command: &mut Command) {
    match command.output() {
        Ok(output) => {
            eprint!("{}", String::from_utf8_lossy(&output.stdout));
            eprint!("{}", String::from_utf8_lossy(&output.stderr));
        }
        Err(error) => eprintln!("diagnostic command failed: {error}"),
    }
}

fn parse_u64(label: &str, value: &str) -> Result<u64> {
    value
        .parse::<u64>()
        .map_err(|error| format!("{label} returned {value:?}, expected a whole number: {error}"))
}

fn assert_output(label: &str, actual: &str, expected: &str) -> Result<()> {
    if actual.trim() == expected {
        Ok(())
    } else {
        Err(format!(
            "{label} returned {:?}, expected {expected:?}",
            actual.trim()
        ))
    }
}

fn require_kind_0_33(output: &str) -> Result<()> {
    let version = output
        .split_whitespace()
        .find_map(|word| word.strip_prefix('v'))
        .ok_or_else(|| format!("cannot parse kind version from {output:?}"))?;
    let mut parts = version.split('.');
    let major = parts
        .next()
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| format!("cannot parse kind version from {output:?}"))?;
    let minor = parts
        .next()
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| format!("cannot parse kind version from {output:?}"))?;
    if major > 0 || minor >= 33 {
        Ok(())
    } else {
        Err(format!(
            "kind v0.33.0 or newer is required for the pinned Kubernetes 1.36 node image; found v{version}"
        ))
    }
}

fn render_operator_manifest(source: &str, image: &str) -> Result<String> {
    const NEEDLE: &str = "          image: yesno-operator:local";
    if source.matches(NEEDLE).count() != 1 {
        return Err("operator manifest must contain exactly one local image placeholder".into());
    }
    Ok(source.replace(NEEDLE, &format!("          image: {image}")))
}

/// How much storage one instance's claim asks for.
///
/// **This was load-bearing and no longer is.** A shard image is grown to a
/// whole 1 GiB mmap segment with `set_len`, so it is sparse: a four-row
/// database occupies about 20 KiB and reports 1 GiB. Bootstrap used to ship and
/// write every byte, holes included, so a two-shard standby materialized 2 GiB
/// and filled a 1 GiB volume during its first bootstrap on 2026-09-05. That is
/// fixed -- the leader skips the zero runs and the follower sets the length --
/// so the claim now needs to hold the database rather than the address space.
///
/// **kind could never have shown it.** Its `standard` class is
/// `rancher.io/local-path`, which ignores the requested size entirely and hands
/// out a directory on the node -- so `1Gi` there means "as much as the node
/// has" and the same manifest line was inert on one arm and fatal on the other.
///
/// The headroom stays. It is not a workaround any more, just room for a
/// database to grow into, and gp3 bills provisioned capacity in pennies at this
/// size. Do not reduce it to prove the fix; the test below proves the fix.
fn claim_size(config: &Config) -> &'static str {
    match config.backend {
        // Whatever the node has; the number is not read.
        Backend::Kind => "1Gi",
        // Two shards at one 1 GiB segment each, plus slack for the WAL.
        Backend::Eks => "4Gi",
    }
}

/// `Never` on kind, where the image was side-loaded and no registry exists
/// to fall back to, and `IfNotPresent` on EKS, where the node pulls it from
/// ECR. Not `Always` there either: the three per-run tags are immutable, so
/// a re-pull would cost transfer on every Pod for a byte-identical image.
fn image_pull_policy(config: &Config) -> &'static str {
    match config.backend {
        Backend::Kind => "Never",
        Backend::Eks => "IfNotPresent",
    }
}

fn invalid_cluster_manifest(config: &Config) -> String {
    format!(
        "apiVersion: yesnodb.io/v1alpha1\n\
         kind: YesnoCluster\n\
         metadata:\n\
         \x20 name: {INVALID_CLUSTER}\n\
         \x20 namespace: {}\n\
         spec:\n\
         \x20 image: {}\n\
         \x20 imagePullPolicy: {}\n\
         \x20 storage:\n\
         \x20   size: {}\n\
         \x20   storageClassName: {}\n",
        config.namespace,
        config.server_image,
        image_pull_policy(config),
        claim_size(config),
        config.storage_class,
    )
}

/// The `YesnoCluster` both backends drive.
///
/// Identical apart from where its storage comes from and, on EKS, the
/// snapshot backend that storage makes possible. Keeping one builder is what
/// makes the two arms comparable: a divergence here would let the cheap arm
/// pass on a manifest the expensive one never runs.
fn cluster_manifest(config: &Config) -> String {
    let mut manifest = format!(
        "apiVersion: yesnodb.io/v1alpha1\n\
         kind: YesnoCluster\n\
         metadata:\n\
         \x20 name: {CLUSTER}\n\
         \x20 namespace: {}\n\
         spec:\n\
         \x20 image: {}\n\
         \x20 imagePullPolicy: {}\n\
         \x20 instances: 2\n\
         \x20 failoverDelaySecs: 0\n\
         \x20 shards: 2\n\
         \x20 shutdownGraceSecs: 10\n\
         \x20 storage:\n\
         \x20   size: {}\n\
         \x20   storageClassName: {}\n\
         \x20 config:\n\
         \x20   certManager:\n\
         \x20     issuerRef:\n\
         \x20       name: yesno-ca\n\
         \x20     caSecretRef:\n\
         \x20       name: yesno-root-ca\n\
         \x20       key: tls.crt\n",
        config.namespace,
        config.server_image,
        image_pull_policy(config),
        claim_size(config),
        config.storage_class,
    );
    if let Some(snapshot) = &config.snapshot {
        // `filesystem: ext4` matches what the EBS CSI driver formats a gp3
        // volume with by default. It is stated rather than defaulted because
        // the daemon passes it to the materializer, and a wrong answer here
        // surfaces as a mount failure inside a Job rather than as a bad field.
        manifest.push_str(&format!(
            "\x20 serviceAccountName: {}\n\
             \x20 snapshot:\n\
             \x20   backend: ebs\n\
             \x20   ebs:\n\
             \x20     region: {}\n\
             \x20     filesystem: ext4\n",
            snapshot.account, snapshot.region,
        ));
    }
    manifest
}

/// The ServiceAccount the managed Pods run as, annotated for IRSA.
///
/// Created by the harness rather than by Terraform, because Terraform in
/// this gate has no Kubernetes provider on purpose -- it would need a working
/// cluster client at plan time. The role's trust policy names this exact
/// namespace and account, so the two files have to agree and the outputs are
/// what make them.
fn service_account_manifest(config: &Config, snapshot: &SnapshotInputs) -> String {
    format!(
        "apiVersion: v1\n\
         kind: ServiceAccount\n\
         metadata:\n\
         \x20 name: {}\n\
         \x20 namespace: {}\n\
         \x20 annotations:\n\
         \x20   eks.amazonaws.com/role-arn: {}\n",
        snapshot.account, config.namespace, snapshot.role_arn,
    )
}

fn ca_manifest(namespace: &str) -> String {
    format!(
        "apiVersion: cert-manager.io/v1\n\
     kind: Issuer\n\
     metadata:\n\
     \x20 name: yesno-selfsigned\n\
     \x20 namespace: {namespace}\n\
     spec:\n\
     \x20 selfSigned: {{}}\n\
     ---\n\
     apiVersion: cert-manager.io/v1\n\
     kind: Certificate\n\
     metadata:\n\
     \x20 name: yesno-root-ca\n\
     \x20 namespace: {namespace}\n\
     spec:\n\
     \x20 isCA: true\n\
     \x20 commonName: yesno-e2e-root\n\
     \x20 secretName: yesno-root-ca\n\
     \x20 issuerRef:\n\
     \x20   name: yesno-selfsigned\n\
     ---\n\
     apiVersion: cert-manager.io/v1\n\
     kind: Issuer\n\
     metadata:\n\
     \x20 name: yesno-ca\n\
     \x20 namespace: {namespace}\n\
     spec:\n\
     \x20 ca:\n\
     \x20   secretName: yesno-root-ca\n"
    )
}

fn client_pod_manifest(config: &Config) -> String {
    format!(
        "apiVersion: v1\n\
         kind: Pod\n\
         metadata:\n\
         \x20 name: yesno-client\n\
         \x20 namespace: {namespace}\n\
         spec:\n\
         \x20 automountServiceAccountToken: false\n\
         \x20 restartPolicy: Never\n\
         \x20 containers:\n\
         \x20 - name: client\n\
         \x20   image: {server_image}\n\
         \x20   imagePullPolicy: {pull}\n\
         \x20   command: [\"/bin/sleep\", \"3600\"]\n\
         \x20   volumeMounts:\n\
         \x20   - name: client\n\
         \x20     mountPath: /tls/client\n\
         \x20     readOnly: true\n\
         \x20   - name: ca\n\
         \x20     mountPath: /tls/ca\n\
         \x20     readOnly: true\n\
         \x20 volumes:\n\
         \x20 - name: client\n\
         \x20   secret:\n\
         \x20     secretName: search-client-tls\n\
         \x20 - name: ca\n\
         \x20   secret:\n\
         \x20     secretName: yesno-root-ca\n\
         \x20     items:\n\
         \x20     - key: tls.crt\n\
         \x20       path: ca.crt\n",
        namespace = config.namespace,
        server_image = config.server_image,
        pull = image_pull_policy(config),
    )
}

fn unique_suffix() -> Result<String> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("system clock is before the Unix epoch: {error}"))?;
    Ok(format!("{:x}-{:x}", std::process::id(), elapsed.as_nanos()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operator_image_replacement_is_exact() {
        let source = "before\n          image: yesno-operator:local\nafter\n";
        let rendered = render_operator_manifest(source, "yesno-operator:test").unwrap();
        assert_eq!(
            rendered,
            "before\n          image: yesno-operator:test\nafter\n"
        );
        assert!(render_operator_manifest("no placeholder", "x").is_err());
    }

    fn test_config(backend: Backend) -> Config {
        Config {
            backend,
            workspace: PathBuf::from("/workspace"),
            work: PathBuf::from("/workspace/work"),
            cluster_name: "yesno-e2e-test".into(),
            kubeconfig: PathBuf::from("/kube/config"),
            operator_image: "registry/yesno:operator".into(),
            server_image: "yesnod:test".into(),
            node_image: String::new(),
            driver_container: None,
            keep_cluster: false,
            namespace: match backend {
                Backend::Kind => KIND_NAMESPACE.into(),
                Backend::Eks => "yesno-operator-e2e".into(),
            },
            storage_class: match backend {
                Backend::Kind => KIND_STORAGE_CLASS.into(),
                Backend::Eks => "yesno-ebs".into(),
            },
            snapshot: match backend {
                Backend::Kind => None,
                Backend::Eks => Some(SnapshotInputs {
                    region: "ap-northeast-1".into(),
                    account: "yesno-snapshotter".into(),
                    role_arn: "arn:aws:iam::123456789012:role/yesno".into(),
                }),
            },
        }
    }

    #[test]
    fn cluster_manifests_make_security_and_storage_explicit() {
        let config = test_config(Backend::Kind);
        let valid = cluster_manifest(&config);
        assert!(valid.contains("imagePullPolicy: Never"));
        assert!(valid.contains("instances: 2"));
        assert!(valid.contains("failoverDelaySecs: 0"));
        assert!(valid.contains("storageClassName: standard"));
        assert!(valid.contains("certManager:"));
        assert!(valid.contains("key: tls.crt"));
        assert!(!valid.contains("allowInsecure"));
        // The kind arm must not be handed the backend in its manifest. It
        // patches it on afterwards precisely so that it observes the window
        // and the refusal; a manifest that carried it would skip both.
        assert!(!valid.contains("snapshot:"), "{valid}");

        let invalid = invalid_cluster_manifest(&config);
        assert!(!invalid.contains("allowInsecure"));
        assert!(invalid.contains("storageClassName: standard"));
    }

    /// The claim carries headroom past the address space a shard reserves.
    ///
    /// Kept after sparse base snapshots landed rather than tightened back.
    /// A standby no longer materializes its leader's holes, so the old reason
    /// is gone -- but a claim sized to exactly what a fresh database occupies
    /// is a claim that cannot take a write, and kind's local-path class ignores
    /// the number entirely, so nothing else in the suite would notice it
    /// shrinking to something unusable.
    #[test]
    fn the_eks_claim_can_hold_every_shard_image() {
        let manifest = cluster_manifest(&test_config(Backend::Eks));
        assert!(manifest.contains("shards: 2"), "{manifest}");
        let size = manifest
            .lines()
            .find_map(|line| line.trim().strip_prefix("size: "))
            .expect("the manifest must request storage");
        let gib: u64 = size
            .strip_suffix("Gi")
            .expect("the request must be in GiB")
            .parse()
            .expect("a whole number of GiB");
        assert!(
            gib > 2,
            "a two-shard standby materializes 2 GiB of image; {size} cannot hold it"
        );
    }

    #[test]
    fn the_eks_manifest_asks_for_ebs_snapshots_on_ebs_storage() {
        let config = test_config(Backend::Eks);
        let manifest = cluster_manifest(&config);
        // The node pulls this from ECR; `Never` is a kind-only affordance and
        // would leave every Pod in ErrImageNeverPull here.
        assert!(
            manifest.contains("imagePullPolicy: IfNotPresent"),
            "{manifest}"
        );
        assert!(
            manifest.contains("storageClassName: yesno-ebs"),
            "{manifest}"
        );
        assert!(manifest.contains("backend: ebs"), "{manifest}");
        assert!(manifest.contains("region: ap-northeast-1"), "{manifest}");
        assert!(manifest.contains("filesystem: ext4"), "{manifest}");
        // Without this the Pods run as the namespace default account, the
        // IRSA webhook injects nothing, and the daemon has no AWS identity --
        // which is not a startup failure, only a background retry loop.
        assert!(
            manifest.contains("serviceAccountName: yesno-snapshotter"),
            "{manifest}"
        );
        // The topology the kind arm proves is not weakened to buy the storage
        // the EKS arm proves.
        assert!(manifest.contains("certManager:"), "{manifest}");
        assert!(!manifest.contains("allowInsecure"), "{manifest}");
    }

    #[test]
    fn the_service_account_carries_the_role_the_trust_policy_names() {
        let config = test_config(Backend::Eks);
        let snapshot = config.snapshot.clone().unwrap();
        let manifest = service_account_manifest(&config, &snapshot);
        assert!(manifest.contains("name: yesno-snapshotter"), "{manifest}");
        assert!(
            manifest.contains("namespace: yesno-operator-e2e"),
            "{manifest}"
        );
        assert!(
            manifest.contains("eks.amazonaws.com/role-arn: arn:aws:iam::123456789012:role/yesno"),
            "{manifest}"
        );
    }

    #[test]
    fn configured_volume_ids_are_read_from_every_generated_configuration() {
        let generated = concat!(
            "[server.snapshot.ebs]\nvolume_id = \"vol-00ff\"\nregion = \"eu-west-1\"\n",
            "[server.snapshot.ebs]\n  volume_id  =  \"vol-00fe\"\n",
        );
        assert_eq!(
            configured_volume_ids(generated),
            vec!["vol-00fe".to_owned(), "vol-00ff".to_owned()]
        );
        // A configuration with no backend must yield nothing rather than a
        // plausible empty string: the scenario compares the whole list against
        // what Kubernetes says, and a phantom entry would make lengths agree.
        assert!(configured_volume_ids("[server]\nrole = \"leader\"\n").is_empty());
        // And nothing that merely mentions the key. Both of these were
        // accepted by a first version that matched the key as a substring:
        // the comment is what the generated file carries around the value, and
        // the second is a different field entirely.
        assert!(configured_volume_ids("# volume_id is discovered\n").is_empty());
        assert!(configured_volume_ids("# volume_id = \"vol-dead\"\n").is_empty());
        assert!(configured_volume_ids("source_volume_id = \"vol-dead\"\n").is_empty());
    }

    #[test]
    fn only_the_two_backends_are_accepted() {
        assert_eq!(Backend::parse("kind"), Some(Backend::Kind));
        assert_eq!(Backend::parse("eks"), Some(Backend::Eks));
        for wrong in ["", "EKS", "gke", "kind ", "aws"] {
            assert_eq!(Backend::parse(wrong), None, "accepted {wrong:?}");
        }
    }

    #[test]
    fn pinned_node_image_requires_a_compatible_kind() {
        assert!(require_kind_0_33("kind v0.33.0 go1.25 linux/amd64").is_ok());
        assert!(require_kind_0_33("kind v1.0.0 go1.25 linux/amd64").is_ok());
        assert!(require_kind_0_33("kind v0.32.0 go1.24 linux/amd64").is_err());
        assert!(require_kind_0_33("unknown").is_err());
    }

    #[test]
    fn server_image_builds_every_binary_it_copies() {
        let dockerfile = fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../yesno-server/dist/Dockerfile"),
        )
        .unwrap();
        assert!(dockerfile.contains("-p yesno-server-utils"));
        assert!(dockerfile.contains("target/release/yesno"));
        assert!(dockerfile.contains("target/release/yesnoctl"));
    }

    /// Every path that can build a Rustls configuration must first choose a
    /// provider.
    ///
    /// This is structural on purpose. The workspace graph carries **two**
    /// Rustls providers — `ring`, from `tonic`'s `tls-ring` and `kube`, and
    /// `aws-lc-rs`, which the EC2 client's default HTTPS transport compiles in.
    /// Rustls refuses to guess between them and panics when a configuration is
    /// built, so a missing call is invisible to `cargo build`, to Clippy, and to
    /// any unit test: provider installation is process-global and
    /// first-writer-wins, so once *one* test installs it the rest of the process
    /// cannot observe the omission. It surfaces only as a crash-looping binary.
    ///
    /// That is exactly how it was found — `yesno-operator` shipped without the
    /// call and crash-looped in kind on 2026-09-01, failing `gate-operator` at a
    /// symptom (`op_invalid_create()` timing out) a hundred lines from the cause.
    #[test]
    fn every_rustls_configuration_path_installs_a_provider() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        const CALL: &str = "install_crypto_provider()";

        // Binaries install before initializing any transport, which is the
        // contract stated on `yesno_server::tls::install_crypto_provider`.
        for entry in [
            "yesno-server/src/main.rs",
            "yesno-server/src/bin/yesno.rs",
            "yesno-operator/src/main.rs",
        ] {
            let source = fs::read_to_string(root.join(entry)).unwrap();
            assert!(
                source.contains(CALL),
                "{entry} builds a TLS client without choosing a Rustls provider"
            );
        }

        // The library entry points that build a configuration on behalf of a
        // caller install defensively. This used to say that `yesnoctl` and
        // `yesno-archive` reach TLS only through `connect`; that stopped being
        // true when the deferred EKS materializer gave `yesno-archive` a second
        // TLS path, and a live run panicked there on 2026-09-02. A binary is not
        // covered by the call its *other* transport makes.
        let listener = fs::read_to_string(root.join("yesno-server/src/tls.rs")).unwrap();
        assert!(
            listener.matches(CALL).count() >= 2,
            "yesno-server/src/tls.rs must call the installer from server_config, \
             not merely define it"
        );

        let client = fs::read_to_string(root.join("yesno-server-utils/src/transport.rs")).unwrap();
        let (before_config, _) = client
            .split_once("ClientTlsConfig::new()")
            .expect("transport.rs no longer builds a ClientTlsConfig");
        assert!(
            before_config.contains(CALL),
            "yesno-server-utils/src/transport.rs must install a provider before \
             building a ClientTlsConfig"
        );

        // The deferred EKS materializer builds a Kubernetes client, which is a
        // TLS path of its own inside `yesno-archive` and reaches Rustls without
        // going near `transport.rs`.
        let deferred = fs::read_to_string(root.join("yesno-server-utils/src/deferred.rs")).unwrap();
        let (before_client, _) = deferred
            .split_once("KubeClient::try_default()")
            .expect("deferred.rs no longer builds a Kubernetes client");
        assert!(
            before_client.contains(CALL),
            "yesno-server-utils/src/deferred.rs must install a provider before \
             building a Kubernetes client"
        );
    }

    /// The scratch path is named in exactly two places, one per language,
    /// and both are overridden by the same environment variable.
    ///
    /// It used to be written out longhand in three Rust modules and seven shell
    /// scripts. Every copy is one that can drift from `.bazelrc`, from
    /// CLAUDE.md's rule about where temporary files go, and from the others --
    /// and a gate script that disagreed with the harness it runs would put the
    /// state file somewhere the destroy path does not look.
    ///
    /// `.bazelrc` is not scanned and is the deliberate third copy: Bazel's
    /// rc files take a literal `--symlink_prefix` and expand no variables.
    #[test]
    fn the_scratch_directory_is_named_in_one_place_per_language() {
        // Assembled, not written. A scanner that spells out what it forbids
        // matches itself, and adding this file to the allow-list would let the
        // path be hard-coded anywhere else in it.
        let path = format!(".agents-{}/tmp", "workspace");
        let path = path.as_str();
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        let allowed = ["yesno-e2e/src/lib.rs", "scripts/scratch.sh"];

        fn walk(dir: &std::path::Path, found: &mut Vec<PathBuf>) {
            let Ok(entries) = fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if path.is_dir() {
                    // Not `target`, which holds copies of the sources, nor
                    // the scratch directory itself, which is full of them.
                    if name != "target" && name != ".git" && name != ".agents-workspace" {
                        walk(&path, found);
                    }
                } else if name.ends_with(".rs") || name.ends_with(".sh") {
                    found.push(path);
                }
            }
        }

        let mut files = Vec::new();
        walk(&root, &mut files);
        assert!(files.len() > 50, "walked only {} files", files.len());

        let mut offenders = Vec::new();
        for file in &files {
            let Ok(text) = fs::read_to_string(file) else {
                continue;
            };
            if !text.contains(path) {
                continue;
            }
            let relative = file.strip_prefix(&root).unwrap_or(file);
            let relative = relative.to_string_lossy().replace('\\', "/");
            if !allowed.iter().any(|ok| relative.ends_with(ok)) {
                offenders.push(relative.to_owned());
            }
        }
        assert!(
            offenders.is_empty(),
            "{path} is spelled out in {offenders:?}; derive it from \
             YESNO_SCRATCH_DIR instead -- scripts source scripts/scratch.sh, \
             Rust calls yesno_e2e::scratch_dir()"
        );
    }

    #[test]
    fn one_all_in_one_e2e_image_serves_every_containerized_gate() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        let dockerfile = fs::read_to_string(root.join("e2e/Dockerfile")).unwrap();

        // The harness, the operator, and the shipped binaries the KVM guest and
        // the operator scenarios exercise are built once, at one Rust version.
        assert!(dockerfile.contains("cargo build --locked --release"));
        for package in [
            "-p yesno-e2e",
            "-p yesno-operator",
            "-p yesno-server",
            "-p yesno-server-utils",
        ] {
            assert!(
                dockerfile.contains(package),
                "missing build package {package}"
            );
        }
        assert!(dockerfile.contains("target/release/yesno-archive"));
        assert!(dockerfile.contains("target/release/yesnoctl"));
        assert!(!dockerfile.contains("cargo install --locked"));
        assert_eq!(
            dockerfile.matches("ARG RUST_VERSION=").count(),
            1,
            "one image, one Rust version: the three images this replaced pinned two"
        );

        // Operator scenarios.
        assert!(dockerfile.contains("/usr/local/bin/kind"));
        assert!(dockerfile.contains("/usr/local/bin/kubectl"));
        assert!(dockerfile.contains("/usr/local/bin/docker"));

        // Filesystem scenarios: the KVM guest and the stateful S3 endpoint.
        assert!(dockerfile.contains("qemu-system-x86"));
        assert!(dockerfile.contains("qemu-system-arm"));
        assert!(dockerfile.contains("zfsutils-linux"));
        assert!(dockerfile.contains("btrfs-progs"));
        assert!(dockerfile.contains("initramfs-tools iproute2 linux-image-virtual"));
        assert!(dockerfile.contains("serial-autologin.conf"));
        assert!(dockerfile.contains("root-profile /root/.profile"));
        assert!(dockerfile.contains("/opt/yesno-e2e/ssh-key"));
        assert!(dockerfile.contains("winterbaume-server /usr/local/bin/"));
        assert!(dockerfile.contains("winterbaume-server-v${WINTERBAUME_VERSION}"));
        assert!(dockerfile.contains("WINTERBAUME_SHA256_AMD64="));
        assert!(dockerfile.contains("WINTERBAUME_SHA256_ARM64="));

        // Database and search gates: the pinned Bazel binary, JDK 21, the
        // non-root builder identity, and every integration's baked artifacts.
        assert!(dockerfile.contains("BAZEL_SHA256_AMD64="));
        assert!(dockerfile.contains("BAZEL_SHA256_ARM64="));
        assert!(dockerfile.contains("/opt/java/openjdk"));
        assert!(dockerfile.contains("--uid 10001"));
        assert!(dockerfile.contains("USER yesno-builder"));
        for edge in ["postgresql", "mysql", "search"] {
            assert!(
                dockerfile.contains(&format!("./scripts/build-database-artifacts.sh {edge}")),
                "missing baked artifacts for {edge}"
            );
        }

        // One harness entrypoint. The operator binary and the KVM guest are
        // workload artifacts, not additional scenario endpoints.
        assert!(dockerfile.contains("ENTRYPOINT [\"/usr/local/bin/yesno-e2e-entrypoint\"]"));
        assert!(dockerfile.contains("CMD [\"/usr/local/bin/yesno-e2e\","));
        assert!(!dockerfile.contains("ENTRYPOINT [\"/usr/local/bin/yesno-operator\"]"));
        assert!(!dockerfile.contains("CMD [\"/usr/local/bin/yesno-operator\""));

        // The per-integration images this one replaced must not come back: they
        // duplicated the source copy, the release build and the toolchain three
        // ways and drifted apart. The real-AWS runner is the one deliberate
        // exception, because its image is pushed to ECR and pulled onto EC2.
        for retired in ["e2e/database/Dockerfile", "yesno-operator/e2e/Dockerfile"] {
            assert!(
                !root.join(retired).exists(),
                "{retired} is superseded by e2e/Dockerfile"
            );
        }
        assert!(root.join("yesno-operator/e2e/aws.Dockerfile").exists());

        // Every containerized gate builds through the one helper, so no gate can
        // drift onto a private tag or a stale `--target`.
        for gate in [
            "scripts/run-database-gate.sh",
            "scripts/gate-operator.sh",
            "scripts/gate-filesystems.sh",
        ] {
            let script = fs::read_to_string(root.join(gate)).unwrap();
            assert!(
                script.contains("./scripts/build-e2e-image.sh"),
                "{gate} does not build through the shared helper"
            );
            assert!(
                !script.contains("docker build"),
                "{gate} builds an image of its own"
            );
        }
        let helper = fs::read_to_string(root.join("scripts/build-e2e-image.sh")).unwrap();
        assert!(helper.contains("--file e2e/Dockerfile"));
        assert!(
            !helper
                .lines()
                .any(|line| line.trim_start().starts_with("--target")),
            "the one image is the Dockerfile's last stage; it needs no --target"
        );

        // The two entry identities the one entrypoint arbitrates: the database
        // and search gates keep the image default, and only the gates that need
        // a Docker socket or /dev/kvm ask for root.
        let entrypoint = fs::read_to_string(root.join("e2e/entrypoint.sh")).unwrap();
        for gate in ["gate-pg.sh", "gate-mysql.sh", "gate-search.sh"] {
            assert!(
                entrypoint.contains(gate),
                "{gate} is not covered by the uid guard"
            );
        }
        assert!(entrypoint.contains("10001"));
        // `docker run <image> --timeout 1200 x.py` addressed the harness
        // directly in the image this replaced, and must keep doing so; without
        // this it would reach bash's `exec` builtin.
        assert!(entrypoint.contains(r#"set -- /usr/local/bin/yesno-e2e "$@""#));
        assert!(
            !fs::read_to_string(root.join("scripts/run-database-gate.sh"))
                .unwrap()
                .lines()
                .any(|line| !line.trim_start().starts_with('#') && line.contains("--user")),
            "the database gates must enter as the image default identity"
        );
        for gate in ["scripts/gate-operator.sh", "scripts/gate-filesystems.sh"] {
            assert!(
                fs::read_to_string(root.join(gate))
                    .unwrap()
                    .contains("--user 0:0"),
                "{gate} must enter as root explicitly"
            );
        }
    }
}
