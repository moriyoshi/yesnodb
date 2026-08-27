//! `aws_*`: an opt-in EBS scenario running on a Terraform-owned EC2 runner.
//!
//! Terraform supplies an instance, an attached source volume, an instance role,
//! and a unique run tag. These verbs own only the processes and RPCs inside that
//! runner. The Python scenario remains the behavioral oracle and the production
//! EBS provider remains the only code issuing snapshot/volume operations.
//!
//! The daemon runs under an unprivileged account here, and the privileged
//! snapshot agent runs beside it as root. That is not incidental tidiness: this
//! is the only gate that executes the EBS mount at all, so if it ran the daemon
//! as root — as it did until 2026-09-01 — a provider that quietly needed
//! `CAP_SYS_ADMIN` in the daemon would pass it. `aws_privileges()` exists to
//! make the split an assertion in the scenario rather than a property of how
//! the harness happens to spawn things.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use aws_config::BehaviorVersion;
use aws_sdk_ec2::config::Region;
use aws_sdk_ec2::types::Filter;
use aws_sdk_ec2::Client;
use monty_types::{ExcType, MontyException, MontyObject};
use tonic::transport::Channel;
use yesno_server::control::pb;
use yesno_server::control::pb::control_plane_client::ControlPlaneClient;

use crate::convert::{dict, int_obj, value_err, whole_obj, Args};
use crate::world::World;

pub const OWNS: &[&str] = &[
    "aws_prepare",
    "aws_put",
    "aws_get",
    "aws_checkpoint",
    "aws_snapshot_begin",
    "aws_snapshot_release",
    "aws_privileges",
    "aws_resource_counts",
    "aws_wait_resource_counts",
    "aws_basebackup",
    "aws_crash_server",
    "aws_restart_server",
    "aws_cleanup",
    // The deferred half. Everything above is shared with it; these are the
    // verbs that exist because nothing is mounted on this host at all.
    "aws_prepare_deferred",
    "aws_deferred_config",
    "aws_archive_start",
    "aws_archive_wait_base",
    "aws_archive_wait_exit",
    "aws_archive_stop",
    "aws_staged_entries",
    "aws_wait_staged_entries",
    "aws_materializer_volumes",
];

/// The environment the runner image must receive, named once so the
/// orchestration side composes exactly what this side requires.
///
/// These two halves run on different machines and are separated by a
/// billable twenty-minute round trip: a variable added here and forgotten in
/// the remote invocation is a failure that costs a full run to discover.
/// [`crate::cloud`] builds its `docker run` environment *from this list* and
/// unit-tests that it covers every name, so the drift cannot happen silently.
pub const ENV_REGION: &str = "YESNO_AWS_REGION";
pub const ENV_RUN_ID: &str = "YESNO_AWS_E2E_RUN_ID";
pub const ENV_INSTANCE_ID: &str = "YESNO_AWS_INSTANCE_ID";
pub const ENV_VOLUME_ID: &str = "YESNO_AWS_VOLUME_ID";
pub const ENV_AVAILABILITY_ZONE: &str = "YESNO_AWS_AVAILABILITY_ZONE";
pub const ENV_SOURCE_MOUNT: &str = "YESNO_AWS_SOURCE_MOUNT";
pub const ENV_SNAPSHOT_MOUNT: &str = "YESNO_AWS_SNAPSHOT_MOUNT";

/// Every name `Fixture::from_env` requires, in one place.
pub const RUNNER_ENV: &[&str] = &[
    ENV_REGION,
    ENV_RUN_ID,
    ENV_INSTANCE_ID,
    ENV_VOLUME_ID,
    ENV_AVAILABILITY_ZONE,
    ENV_SOURCE_MOUNT,
    ENV_SNAPSHOT_MOUNT,
];

/// The deferred gate's second contract: what the shipped `yesno-archive`
/// itself reads.
///
/// These are the binary's own variable names, not the harness's. A
/// deployment that uses deferred materialization configures exactly these, so
/// filling them from Terraform and handing them to the shipped binary is also
/// the assertion that the documented deployment surface is sufficient.
/// [`crate::cloud`] builds them from Terraform outputs and unit-tests that it
/// covers every one.
pub const ENV_MATERIALIZER: &str = "YESNO_ARCHIVE_DEFERRED_MATERIALIZER";
pub const ENV_SOURCE_PATH: &str = "YESNO_ARCHIVE_MATERIALIZER_SOURCE_PATH";
pub const ENV_STAGING_PATH: &str = "YESNO_ARCHIVE_MATERIALIZER_STAGING_PATH";
pub const ENV_ECS_CLUSTER: &str = "YESNO_ARCHIVE_ECS_CLUSTER";
pub const ENV_ECS_TASK_DEFINITION: &str = "YESNO_ARCHIVE_ECS_TASK_DEFINITION";
pub const ENV_ECS_CONTAINER: &str = "YESNO_ARCHIVE_ECS_CONTAINER_NAME";
pub const ENV_ECS_VOLUME: &str = "YESNO_ARCHIVE_ECS_VOLUME_NAME";
pub const ENV_ECS_ROLE: &str = "YESNO_ARCHIVE_ECS_INFRASTRUCTURE_ROLE_ARN";
pub const ENV_ECS_SUBNETS: &str = "YESNO_ARCHIVE_ECS_SUBNETS";
pub const ENV_ECS_SECURITY_GROUPS: &str = "YESNO_ARCHIVE_ECS_SECURITY_GROUPS";
pub const ENV_EKS_NAMESPACE: &str = "YESNO_ARCHIVE_EKS_NAMESPACE";
pub const ENV_EKS_IMAGE: &str = "YESNO_ARCHIVE_EKS_IMAGE";
pub const ENV_EKS_SNAPSHOT_CLASS: &str = "YESNO_ARCHIVE_EKS_SNAPSHOT_CLASS";
pub const ENV_EKS_STORAGE_CLASS: &str = "YESNO_ARCHIVE_EKS_STORAGE_CLASS";
pub const ENV_EKS_STAGING_CLAIM: &str = "YESNO_ARCHIVE_EKS_STAGING_CLAIM";
/// Kubernetes' own variable, not ours. `yesno-archive` builds its client
/// with `Client::try_default()`, which reads a kubeconfig the way every other
/// Kubernetes client does; the gate has no private channel to the cluster.
pub const ENV_KUBECONFIG: &str = "KUBECONFIG";

/// What both deferred arms need, whichever materializer is configured.
pub const ARCHIVE_ENV_COMMON: &[&str] = &[ENV_MATERIALIZER, ENV_SOURCE_PATH, ENV_STAGING_PATH];
/// What only the ECS arm needs.
pub const ARCHIVE_ENV_ECS: &[&str] = &[
    ENV_ECS_CLUSTER,
    ENV_ECS_TASK_DEFINITION,
    ENV_ECS_CONTAINER,
    ENV_ECS_VOLUME,
    ENV_ECS_ROLE,
    ENV_ECS_SUBNETS,
    ENV_ECS_SECURITY_GROUPS,
];
/// What only the EKS arm needs.
///
/// Shorter than the ECS list because Kubernetes carries the equivalent of
/// the subnets, the security groups and the infrastructure role in the cluster
/// itself. The CSI driver name is not here either: `yesno-archive` defaults it
/// to `ebs.csi.aws.com`, and a gate that restated the default would stop
/// testing it.
pub const ARCHIVE_ENV_EKS: &[&str] = &[
    ENV_EKS_NAMESPACE,
    ENV_EKS_IMAGE,
    ENV_EKS_SNAPSHOT_CLASS,
    ENV_EKS_STORAGE_CLASS,
    ENV_EKS_STAGING_CLAIM,
    ENV_KUBECONFIG,
];

/// The materializer names this gate has a stack for.
pub const MATERIALIZERS: [&str; 2] = ["ecs", "eks"];

/// Every variable one deferred arm requires, common part first.
///
/// The two arms run in *different containers* and are given only their own
/// arm's variables, so this is the one place that knows which is which.
pub fn archive_env(materializer: &str) -> Result<Vec<&'static str>, String> {
    let arm = match materializer {
        "ecs" => ARCHIVE_ENV_ECS,
        "eks" => ARCHIVE_ENV_EKS,
        other => {
            return Err(format!(
                "'{other}' is not a deferred materializer this gate provisions ({})",
                MATERIALIZERS.join(", ")
            ));
        }
    };
    Ok(ARCHIVE_ENV_COMMON.iter().chain(arm).copied().collect())
}

const SERVER_WAIT: Duration = Duration::from_secs(60);
/// The unprivileged account the runner image creates for the daemon. The agent
/// deliberately stays root.
const DAEMON_USER: &str = "yesno";
/// The attachment names the agent may choose from. Shared with the rendered
/// configuration so the scenario can assert the agent stayed inside its pool
/// without restating the list.
const DEVICE_NAMES: [&str; 3] = ["/dev/sdf", "/dev/sdg", "/dev/sdh"];
/// What the loopback callers on this runner are allowed to do.
///
/// The shared control endpoint denies anything no rule matches, and the
/// daemon refuses to start with an empty table — so this is not a hardening
/// choice, it is the minimum that boots. The three cover what actually calls:
/// `GetSnapshot` is control-read, `Checkpoint` is control-admin, and the whole
/// snapshot-lease and replication surface — which is what the harness and
/// `yesno-archive` use — is replication.
const CONTROL_CAPABILITIES: [&str; 3] = ["control-read", "control-admin", "replication"];

/// The tag the EBS CSI driver puts on a volume it provisions for a claim.
///
/// Written by the driver, not by us, and only when it runs with
/// `--extra-create-metadata`. See [`AwsState::count_materializer_volumes`].
const CLAIM_NAMESPACE_TAG: &str = "kubernetes.io/created-for/pvc/namespace";
/// The tag the deferred materializer puts on the volume ECS restores for it.
///
/// Its value is a lease token derived inside `yesno-archive` and never seen
/// here, so this cannot be narrowed to one run the way
/// [`provider_resource_filters`] is. What separates it from a volume the *EBS
/// provider* made is the absence of the run tag, which only provider-created
/// resources carry — see [`AwsState::materializer_volumes`].
/// How long the archiver may wait for a materializer it launched.
///
/// **This has to be shorter than the scenario's wait for the archiver to
/// exit, and that is the whole point of setting it.** The archiver's own
/// default is an hour; the scenarios wait 900 seconds. With the default in
/// place the harness always gave up first, and its "yesno-archive was still
/// running after 900s" replaced the message the archiver was about to
/// produce -- which names the Kubernetes resource it was waiting on. An
/// observer less patient than the thing it observes cannot report a diagnosis,
/// only its own impatience.
///
/// That is exactly what a live EKS run cost on 2026-09-02: 900 seconds, an
/// empty log, and nothing about which of the four objects had stalled.
/// `an_archiver_timeout_is_shorter_than_the_wait_for_it_to_exit` pins the
/// ordering against the scenarios themselves.
const MATERIALIZER_TIMEOUT_SECS: u64 = 600;

const MATERIALIZER_TAG: &str = "yesno:lease";
/// The run tag Terraform sets and the daemon copies onto what it creates.
const RUN_TAG: &str = "yesno:e2e-run";

/// Which arm of the EBS backend a scenario is exercising.
///
/// Not a harness convenience: it is the daemon's own
/// `server.snapshot.ebs.materialization`, and the two arms need genuinely
/// different processes on this host. Local materialization needs a privileged
/// agent and a shared mount; deferred materialization needs neither, and
/// `deferred_ecs.py` asserts that neither is there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Materialization {
    Local,
    Deferred,
}

impl Materialization {
    fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Deferred => "deferred",
        }
    }

    /// The directory below the source mount this arm's database lives in.
    ///
    /// The two arms share one EBS volume and run one after the other on one
    /// runner, so they must not share a data directory: the second would open
    /// the first's database and its assertions would be about data it did not
    /// write. Separate directories also leave both for inspection after a
    /// failure, which deleting one would not.
    fn subdirectory(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Deferred => "deferred",
        }
    }
}

/// What one snapshot lease turned out to be, as the scenario sees it.
struct LeaseFacts {
    source: String,
    files: usize,
    ttl: u64,
    direct: bool,
    mounted: bool,
    device: Option<String>,
}

/// The effective uid and capability set of one running process.
struct ProcessPrivileges {
    pid: u32,
    uid: u64,
    capabilities: u64,
}

struct Fixture {
    materialization: Materialization,
    region: String,
    run_id: String,
    instance_id: String,
    volume_id: String,
    availability_zone: String,
    source_mount: PathBuf,
    /// Empty under deferred materialization, which mounts nothing here.
    mount_dir: PathBuf,
    config_path: PathBuf,
    log_path: PathBuf,
    socket_path: PathBuf,
    agent_log_path: PathBuf,
    archive_log_path: PathBuf,
    daemon_uid: u32,
    daemon_gid: u32,
}

impl Fixture {
    fn data_dir(&self) -> PathBuf {
        self.source_mount
            .join(self.materialization.subdirectory())
            .join("data")
    }

    fn journal_dir(&self) -> PathBuf {
        self.source_mount
            .join(self.materialization.subdirectory())
            .join("control")
    }
}

#[derive(Default)]
pub struct AwsState {
    child: Option<Child>,
    /// Local materialization mounts, and the mount belongs to the privileged
    /// agent. It outlives a daemon crash on purpose: reconnecting is part of
    /// what this gate is checking.
    agent: Option<Child>,
    runtime: Option<tokio::runtime::Runtime>,
    client: Option<Client>,
    fixture: Option<Fixture>,
    lease: Option<Vec<u8>>,
    /// The shipped `yesno-archive`, run as a child process rather than as a
    /// library call. It is the *binary* whose deployment surface the deferred
    /// gate is checking, and a library call would take a struct nobody
    /// configures in production.
    archiver: Option<Archiver>,
}

/// The archive sidecar, as a process this scenario supervises.
struct Archiver {
    child: Child,
    object_dir: PathBuf,
    log_path: PathBuf,
}

impl Drop for AwsState {
    fn drop(&mut self) {
        for child in [
            self.child.as_mut(),
            self.agent.as_mut(),
            self.archiver.as_mut().map(|archiver| &mut archiver.child),
        ]
        .into_iter()
        .flatten()
        {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn aws_err(verb: &str, error: impl std::fmt::Display) -> MontyException {
    MontyException::new(
        ExcType::RuntimeError,
        Some(format!("{verb}(): AWS E2E failed: {error}")),
    )
}

fn required_env(name: &str) -> Result<String, String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("required environment variable {name} is not set"))
}

fn checked_output(subject: &str, output: Output) -> Result<String, String> {
    if output.status.success() {
        String::from_utf8(output.stdout)
            .map_err(|error| format!("{subject} wrote non-UTF-8 output: {error}"))
    } else {
        Err(format!(
            "{subject} exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn toml_string(value: &str) -> String {
    format!("{value:?}")
}

/// The daemon's configuration, in whichever arm the scenario asked for.
///
/// The deferred arm deliberately omits `unix_socket`, `mount_dir`,
/// `instance_id`, `availability_zone` and `device_names`. Every one of them is
/// documented as needed only by local materialization, and the daemon's own
/// validation agrees; leaving them out is what makes "the deferred path needs
/// no privileged helper and no attachment name" an assertion rather than a
/// claim.
fn server_config(fixture: &Fixture) -> String {
    let data_dir = fixture.data_dir();
    let journal = fixture.journal_dir();
    let control = match fixture.materialization {
        Materialization::Local => format!(
            "unix_socket = {}\n",
            toml_string(&fixture.socket_path.to_string_lossy())
        ),
        Materialization::Deferred => String::new(),
    };
    // Loopback only. The runner opens no ingress at all, and every caller --
    // the harness, `yesno`, and `yesno-archive` -- is a process in this same
    // container reaching 127.0.0.1. The agent needs no row: it comes in over
    // the Unix socket and is authorized by its uid instead.
    let rules = CONTROL_CAPABILITIES
        .iter()
        .map(|capability| {
            format!(
                r#"
[[auth.rule]]
channel = "host"
principal = "all"
address = "127.0.0.0/8"
capability = "{capability}"
action = "allow"
"#
            )
        })
        .collect::<String>();
    let snapshot = match fixture.materialization {
        Materialization::Local => format!(
            r#"allow_direct_path = true

[server.snapshot.ebs]
materialization = "local"
instance_id = {}
availability_zone = {}
mount_dir = {}
device_names = [{}]
"#,
            toml_string(&fixture.instance_id),
            toml_string(&fixture.availability_zone),
            toml_string(&fixture.mount_dir.to_string_lossy()),
            DEVICE_NAMES
                .iter()
                .map(|device| toml_string(device))
                .collect::<Vec<_>>()
                .join(", "),
        ),
        Materialization::Deferred => r#"
[server.snapshot.ebs]
materialization = "deferred"
"#
        .to_owned(),
    };
    format!(
        r#"[server]
role = "leader"
data_dir = {}

[server.flight]
listen = "127.0.0.1:50051"

[server.control]
listen = "127.0.0.1:50052"
journal_dir = {}
{control}
[server.snapshot]
backend = "ebs"
lease_ttl_secs = 300
{snapshot}region = {}
volume_id = {}
source_mount = {}
filesystem = "ext4"
operation_timeout_secs = 600
resource_tags = {{ "{RUN_TAG}" = {}, "yesno:e2e-object" = "lease" }}

[server.metrics]
listen = ""

[auth]
anonymous = "none"
{rules}
[db]
shards = 1
"#,
        toml_string(&data_dir.to_string_lossy()),
        toml_string(&journal.to_string_lossy()),
        toml_string(&fixture.region),
        toml_string(&fixture.volume_id),
        toml_string(&fixture.source_mount.to_string_lossy()),
        toml_string(&fixture.run_id),
    )
}

impl AwsState {
    fn runtime(&mut self) -> Result<&tokio::runtime::Runtime, String> {
        if self.runtime.is_none() {
            self.runtime = Some(
                tokio::runtime::Runtime::new()
                    .map_err(|error| format!("cannot start AWS E2E runtime: {error}"))?,
            );
        }
        Ok(self.runtime.as_ref().expect("runtime was installed"))
    }

    fn fixture(&self) -> Result<&Fixture, String> {
        self.fixture
            .as_ref()
            .ok_or("AWS fixture is not prepared; call aws_prepare() first".into())
    }

    fn endpoint(&self) -> Result<String, String> {
        self.fixture()?;
        Ok("http://127.0.0.1:50052".into())
    }

    fn prepare(&mut self, root: &Path, materialization: Materialization) -> Result<(), String> {
        if self.fixture.is_some() {
            return Err("AWS fixture is already prepared".into());
        }
        let (daemon_uid, daemon_gid) = daemon_account(DAEMON_USER)?;
        let fixture = Fixture {
            materialization,
            region: required_env(ENV_REGION)?,
            run_id: required_env(ENV_RUN_ID)?,
            instance_id: required_env(ENV_INSTANCE_ID)?,
            volume_id: required_env(ENV_VOLUME_ID)?,
            availability_zone: required_env(ENV_AVAILABILITY_ZONE)?,
            source_mount: PathBuf::from(required_env(ENV_SOURCE_MOUNT)?),
            // Not merely unused under deferred materialization — *absent*.
            // Reading the variable anyway would leave a path here that names
            // no shared mount on this host, and a later change could start
            // trusting it.
            mount_dir: match materialization {
                Materialization::Local => PathBuf::from(required_env(ENV_SNAPSHOT_MOUNT)?),
                Materialization::Deferred => PathBuf::new(),
            },
            config_path: root.join("aws-yesnod.toml"),
            log_path: root.join("aws-yesnod.log"),
            socket_path: root.join("aws-run").join("control.sock"),
            agent_log_path: root.join("aws-snapshot-agent.log"),
            archive_log_path: root.join("aws-archive.log"),
            daemon_uid,
            daemon_gid,
        };
        if !fixture.source_mount.is_absolute() {
            return Err("the AWS source mount path must be absolute".into());
        }
        if materialization == Materialization::Local && !fixture.mount_dir.is_absolute() {
            return Err("the AWS snapshot mount path must be absolute".into());
        }
        // The scenario's temporary root is 0700 by construction, which the
        // daemon's account cannot traverse — it would fail to read its own
        // configuration and to bind its socket. Traverse-only is enough and
        // keeps the directory listing closed.
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o711))
            .map_err(|error| format!("cannot widen the scenario root: {error}"))?;
        // Everything the daemon writes has to be handed over first: it can no
        // longer create a directory under a root-owned mount, and it can no
        // longer bind a socket in a root-owned directory.
        let socket_dir = fixture
            .socket_path
            .parent()
            .ok_or("control socket path has no directory")?
            .to_path_buf();
        for directory in [fixture.data_dir(), fixture.journal_dir(), socket_dir] {
            std::fs::create_dir_all(&directory)
                .map_err(|error| format!("cannot create '{}': {error}", directory.display()))?;
            chown_tree(&directory, daemon_uid, daemon_gid)?;
        }
        // The snapshot directory stays root-owned. The agent creates the
        // per-lease mounts in it; the daemon only needs to traverse it to read
        // the staged files, which the default mode already allows.
        if materialization == Materialization::Local {
            std::fs::create_dir_all(&fixture.mount_dir)
                .map_err(|error| format!("cannot create snapshot mount directory: {error}"))?;
        }
        std::fs::write(&fixture.config_path, server_config(&fixture))
            .map_err(|error| format!("cannot write yesnod configuration: {error}"))?;

        let shared = self.runtime()?.block_on(async {
            aws_config::defaults(BehaviorVersion::latest())
                .region(Region::new(fixture.region.clone()))
                .load()
                .await
        });
        self.client = Some(Client::new(&shared));
        self.fixture = Some(fixture);
        self.start_server()?;
        // No agent under deferred materialization: nothing on this host is
        // ever mounted, so nothing on this host needs to be privileged.
        if materialization == Materialization::Local {
            self.start_agent()?;
        }
        self.wait_server(SERVER_WAIT)
    }

    fn start_server(&mut self) -> Result<(), String> {
        if self.child.is_some() {
            return Err("yesnod is already running".into());
        }
        let fixture = self.fixture()?;
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&fixture.log_path)
            .map_err(|error| format!("cannot open yesnod log: {error}"))?;
        let stderr = log
            .try_clone()
            .map_err(|error| format!("cannot clone yesnod log: {error}"))?;
        let binary = std::env::var_os("YESNO_E2E_YESNOD_BIN")
            .map_or_else(|| PathBuf::from("/usr/local/bin/yesnod"), PathBuf::from);
        // Changing uid drops every capability, which is the whole point: a
        // provider step that silently needs one fails here instead of passing
        // on borrowed privilege.
        self.child = Some(
            Command::new(binary)
                .args(["--config"])
                .arg(&fixture.config_path)
                .args(["--insecure", "--insecure-replication"])
                .uid(fixture.daemon_uid)
                .gid(fixture.daemon_gid)
                .stdout(Stdio::from(log))
                .stderr(Stdio::from(stderr))
                .spawn()
                .map_err(|error| format!("cannot start yesnod: {error}"))?,
        );
        Ok(())
    }

    fn start_agent(&mut self) -> Result<(), String> {
        if self.agent.is_some() {
            return Err("yesno-snapshot-agent is already running".into());
        }
        let fixture = self.fixture()?;
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&fixture.agent_log_path)
            .map_err(|error| format!("cannot open snapshot-agent log: {error}"))?;
        let stderr = log
            .try_clone()
            .map_err(|error| format!("cannot clone snapshot-agent log: {error}"))?;
        let binary = std::env::var_os("YESNO_E2E_SNAPSHOT_AGENT_BIN").map_or_else(
            || PathBuf::from("/usr/local/bin/yesno-snapshot-agent"),
            PathBuf::from,
        );
        self.agent = Some(
            Command::new(binary)
                .args(["--config"])
                .arg(&fixture.config_path)
                .stdout(Stdio::from(log))
                .stderr(Stdio::from(stderr))
                .spawn()
                .map_err(|error| format!("cannot start yesno-snapshot-agent: {error}"))?,
        );
        Ok(())
    }

    fn diagnostics(&self) -> String {
        let Some(fixture) = &self.fixture else {
            return "AWS fixture was not prepared".into();
        };
        format!("yesnod log (last bytes):\n{}", tail(&fixture.log_path))
    }

    fn wait_server(&mut self, timeout: Duration) -> Result<(), String> {
        let endpoint = self.endpoint()?;
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self
                .child
                .as_mut()
                .ok_or("yesnod process is absent")?
                .try_wait()
                .map_err(|error| format!("cannot poll yesnod: {error}"))?
            {
                return Err(format!(
                    "yesnod exited with {status} before becoming ready\n{}",
                    self.diagnostics()
                ));
            }
            let ready = self.runtime()?.block_on(async {
                let channel = Channel::from_shared(endpoint.clone())?.connect().await?;
                let response = ControlPlaneClient::new(channel)
                    .get_snapshot(pb::GetSnapshotRequest {})
                    .await?;
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(
                    response.into_inner().database_open,
                )
            });
            let last_error = match ready {
                Ok(true) => return Ok(()),
                Ok(false) => "database is not open".to_owned(),
                Err(error) => error.to_string(),
            };
            if Instant::now() >= deadline {
                return Err(format!(
                    "yesnod did not become ready; last RPC error: {last_error}\n{}",
                    self.diagnostics()
                ));
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    fn checkpoint(&mut self) -> Result<u64, String> {
        let endpoint = self.endpoint()?;
        self.runtime()?
            .block_on(async move {
                let channel = Channel::from_shared(endpoint)?.connect().await?;
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(
                    ControlPlaneClient::new(channel)
                        .checkpoint(pb::CheckpointRequest {})
                        .await?
                        .into_inner()
                        .watermark,
                )
            })
            .map_err(|error| error.to_string())
    }

    fn begin_snapshot(&mut self) -> Result<LeaseFacts, String> {
        if self.lease.is_some() {
            return Err("a snapshot lease is already held".into());
        }
        let endpoint = self.endpoint()?;
        let (lease, direct_path) = self
            .runtime()?
            .block_on(async move {
                let channel = Channel::from_shared(endpoint)?.connect().await?;
                let mut client = ControlPlaneClient::new(channel);
                let lease = client
                    .begin_base_snapshot(pb::BeginBaseSnapshotRequest {})
                    .await?
                    .into_inner();
                let file = lease
                    .files
                    .first()
                    .ok_or("snapshot lease contains no files")?;
                let mut chunks = client
                    .fetch_snapshot_file(pb::FetchSnapshotFileRequest {
                        lease_id: lease.lease_id.clone(),
                        name: file.name.clone(),
                        allow_direct_path: true,
                    })
                    .await?
                    .into_inner();
                let chunk = chunks.message().await?.ok_or("snapshot stream is empty")?;
                let direct_path = match chunk.payload {
                    Some(pb::snapshot_file_chunk::Payload::DirectPath(ref path))
                        if !path.is_empty() && chunk.last && chunk.total_size == file.size =>
                    {
                        Some(PathBuf::from(path))
                    }
                    _ => None,
                };
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>((lease, direct_path))
            })
            .map_err(|error| error.to_string())?;
        let source = match pb::BaseSnapshotSource::try_from(lease.source) {
            Ok(pb::BaseSnapshotSource::Ebs) => "ebs",
            _ => "unexpected",
        }
        .to_owned();
        let files = lease.files.len();
        let ttl = lease.lease_ttl_secs;
        let direct = direct_path.is_some() && lease.direct_path_available;
        let mounted = match &direct_path {
            Some(path) => direct_path_is_mounted(&self.fixture()?.mount_dir, path)?,
            None => false,
        };
        let device = self.lease_attachment()?;
        self.lease = Some(lease.lease_id);
        Ok(LeaseFacts {
            source,
            files,
            ttl,
            direct,
            mounted,
            device,
        })
    }

    /// The attachment name EC2 reports for this run's temporary volume.
    ///
    /// This is what proves the agent chose from its own configured pool: the
    /// daemon never names a device, so a value from outside the pool would mean
    /// the constraint had been lost.
    fn lease_attachment(&mut self) -> Result<Option<String>, String> {
        let filters = provider_resource_filters(&self.fixture()?.run_id);
        let client = self
            .client
            .clone()
            .ok_or("AWS client was not initialized")?;
        self.runtime()?
            .block_on(async move {
                let volumes = client
                    .describe_volumes()
                    .set_filters(Some(filters))
                    .send()
                    .await?
                    .volumes
                    .unwrap_or_default();
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(
                    volumes
                        .iter()
                        .flat_map(|volume| volume.attachments())
                        .find_map(|attachment| attachment.device().map(ToOwned::to_owned)),
                )
            })
            .map_err(|error| error.to_string())
    }

    /// The daemon's privileges, and the agent's if there is an agent at all.
    ///
    /// The `Option` is the deferred arm's assertion, not a convenience: a
    /// scenario that reads `agent_pid is None` is stating that the run reached
    /// a materialized snapshot with no root process anywhere on the host.
    fn privileges(&mut self) -> Result<(ProcessPrivileges, Option<ProcessPrivileges>), String> {
        let daemon = self.child.as_ref().ok_or("yesnod is not running")?.id();
        let agent = match self.agent.as_ref() {
            Some(agent) => Some(process_privileges(agent.id())?),
            None => None,
        };
        Ok((process_privileges(daemon)?, agent))
    }

    fn release_snapshot(&mut self) -> Result<(), String> {
        let lease_id = self.lease.take().ok_or("no snapshot lease is held")?;
        let endpoint = self.endpoint()?;
        self.runtime()?
            .block_on(async move {
                let channel = Channel::from_shared(endpoint)?.connect().await?;
                ControlPlaneClient::new(channel)
                    .release_base_snapshot(pb::ReleaseBaseSnapshotRequest { lease_id })
                    .await?;
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
            })
            .map_err(|error| error.to_string())
    }

    fn resource_counts(&mut self) -> Result<(usize, usize), String> {
        let fixture = self.fixture()?;
        let filters = provider_resource_filters(&fixture.run_id);
        let client = self
            .client
            .clone()
            .ok_or("AWS client was not initialized")?;
        self.runtime()?
            .block_on(async move {
                let snapshots = client
                    .describe_snapshots()
                    .owner_ids("self")
                    .set_filters(Some(filters.clone()))
                    .send()
                    .await?
                    .snapshots
                    .unwrap_or_default()
                    .len();
                let volumes = client
                    .describe_volumes()
                    .set_filters(Some(filters))
                    .send()
                    .await?
                    .volumes
                    .unwrap_or_default()
                    .len();
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>((snapshots, volumes))
            })
            .map_err(|error| error.to_string())
    }

    fn wait_resource_counts(
        &mut self,
        expected_snapshots: usize,
        expected_volumes: usize,
        timeout: Duration,
    ) -> Result<(usize, usize), String> {
        let deadline = Instant::now() + timeout;
        loop {
            let observed = self.resource_counts()?;
            if observed == (expected_snapshots, expected_volumes) {
                return Ok(observed);
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "provider resources stayed at {} snapshot(s), {} volume(s); expected {expected_snapshots}, {expected_volumes}",
                    observed.0, observed.1
                ));
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    fn basebackup(
        &mut self,
        target: PathBuf,
    ) -> Result<yesno_server_utils::basebackup::BaseBackupReport, String> {
        let options =
            yesno_server_utils::basebackup::BasebackupOptions::plaintext(self.endpoint()?, target);
        self.runtime()?
            .block_on(yesno_server_utils::basebackup::run(options))
            .map_err(|error| error.to_string())
    }

    fn crash(&mut self) -> Result<(), String> {
        let mut child = self.child.take().ok_or("yesnod is not running")?;
        child
            .kill()
            .map_err(|error| format!("cannot kill yesnod: {error}"))?;
        child
            .wait()
            .map_err(|error| format!("cannot reap yesnod: {error}"))?;
        self.lease = None;
        Ok(())
    }

    fn restart(&mut self) -> Result<(), String> {
        self.start_server()?;
        self.wait_server(SERVER_WAIT)
    }

    /// Start the shipped `yesno-archive` against this run's ECS materializer.
    ///
    /// `source_path` is the scenario's argument, not the environment's. The
    /// failure half of `deferred_ecs.py` passes a path the task will not have,
    /// which is how a worker is made to fail without a second image, a second
    /// task definition, or a knob inside the daemon.
    ///
    /// Every other setting is read from the environment *and* passed as a
    /// flag. The binary would accept either — these are its own documented
    /// variable names — but reading them here turns a missing one into a named
    /// error before a billable Fargate task is launched.
    /// `override_env` replaces one archive variable for this start only.
    ///
    /// The deferred scenarios fail a worker on purpose, and the ECS arm does it
    /// by pointing `--materializer-source-path` at nothing. That does not
    /// work on EKS: the archiver builds the pod there, so the same value
    /// becomes the volume's `mountPath`, Kubernetes creates it, and the worker
    /// finds a perfectly good source and exits zero. A live run on 2026-09-04
    /// proved the whole materialization path works and failed only because its
    /// failure half had injected nothing.
    ///
    /// The EKS arm therefore breaks the *claim* rather than the source, with a
    /// storage class that does not exist. That tests a materializer which
    /// never completes rather than one whose container fails -- a weaker
    /// statement than the ECS arm's, and part of why both arms exist.
    fn start_archiver(
        &mut self,
        object_dir: PathBuf,
        work_dir: PathBuf,
        source_path: &str,
        override_env: Option<(String, String)>,
    ) -> Result<(), String> {
        if self.archiver.is_some() {
            return Err("yesno-archive is already running".into());
        }
        let fixture = self.fixture()?;
        if fixture.materialization != Materialization::Deferred {
            return Err(format!(
                "the archive sidecar belongs to the deferred scenario; this fixture is {}",
                fixture.materialization.as_str()
            ));
        }
        if !Path::new(source_path).is_absolute() {
            return Err(format!(
                "the materializer source path '{source_path}' is not absolute"
            ));
        }
        let log_path = fixture.archive_log_path.clone();
        for directory in [&object_dir, &work_dir] {
            std::fs::create_dir_all(directory)
                .map_err(|error| format!("cannot create '{}': {error}", directory.display()))?;
        }
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .map_err(|error| format!("cannot open the archive log: {error}"))?;
        let stderr = log
            .try_clone()
            .map_err(|error| format!("cannot clone the archive log: {error}"))?;
        let binary = std::env::var_os("YESNO_E2E_ARCHIVE_BIN").map_or_else(
            || PathBuf::from("/usr/local/bin/yesno-archive"),
            PathBuf::from,
        );
        // Checked against the contract rather than accepted as given. A
        // misspelled name would override nothing, and the scenario would then
        // assert against a run that was never sabotaged -- a failure half that
        // passes because it quietly tested the success path.
        if let Some((name, _)) = &override_env {
            let known = ARCHIVE_ENV_COMMON
                .iter()
                .chain(ARCHIVE_ENV_ECS)
                .chain(ARCHIVE_ENV_EKS)
                .any(|candidate| candidate == name);
            if !known {
                return Err(format!("{name} is not an archive environment variable"));
            }
        }
        let value = |name: &str| -> Result<String, String> {
            match &override_env {
                Some((overridden, replacement)) if overridden == name => Ok(replacement.clone()),
                _ => required_env(name),
            }
        };
        let materializer = value(ENV_MATERIALIZER)?;
        let mut command = Command::new(binary);
        command
            .args(["--endpoint", "http://127.0.0.1:50052"])
            .arg("--store")
            .arg(format!("file://{}", object_dir.display()))
            .arg("--work-dir")
            .arg(&work_dir)
            .args(["--snapshot-mode", "server"])
            .arg("--deferred-materializer")
            .arg(&materializer)
            .arg("--materializer-source-path")
            .arg(source_path)
            .arg("--materializer-staging-path")
            .arg(value(ENV_STAGING_PATH)?)
            .arg("--materializer-timeout-secs")
            .arg(MATERIALIZER_TIMEOUT_SECS.to_string());
        match materializer.as_str() {
            "ecs" => {
                command
                    .arg("--ecs-cluster")
                    .arg(value(ENV_ECS_CLUSTER)?)
                    .arg("--ecs-task-definition")
                    .arg(value(ENV_ECS_TASK_DEFINITION)?)
                    .arg("--ecs-container-name")
                    .arg(value(ENV_ECS_CONTAINER)?)
                    .arg("--ecs-volume-name")
                    .arg(value(ENV_ECS_VOLUME)?)
                    .arg("--ecs-infrastructure-role-arn")
                    .arg(value(ENV_ECS_ROLE)?)
                    .arg("--ecs-subnet")
                    .arg(value(ENV_ECS_SUBNETS)?)
                    .arg("--ecs-security-group")
                    .arg(value(ENV_ECS_SECURITY_GROUPS)?)
                    // The runner's subnet is public with an internet gateway
                    // and no NAT, so a task with no address of its own cannot
                    // pull the image it was just told to run.
                    .arg("--ecs-assign-public-ip");
            }
            "eks" => {
                // No subnets, no security groups, no infrastructure role: the
                // cluster carries all three, which is the actual difference
                // between the two arms. `--eks-csi-driver` is left at its
                // default, because a gate that restated the default would
                // stop testing it.
                command
                    .arg("--eks-namespace")
                    .arg(value(ENV_EKS_NAMESPACE)?)
                    .arg("--eks-image")
                    .arg(value(ENV_EKS_IMAGE)?)
                    .arg("--eks-snapshot-class")
                    .arg(value(ENV_EKS_SNAPSHOT_CLASS)?)
                    .arg("--eks-storage-class")
                    .arg(value(ENV_EKS_STORAGE_CLASS)?)
                    .arg("--eks-staging-claim")
                    .arg(value(ENV_EKS_STAGING_CLAIM)?);
                // Read here only to fail by name: `Client::try_default()`
                // picks it up from the inherited environment, and a kubeconfig
                // that is simply absent would surface as a connection error to
                // localhost:8080 several minutes into the run.
                let kubeconfig = PathBuf::from(required_env(ENV_KUBECONFIG)?);
                if !kubeconfig.is_file() {
                    return Err(format!(
                        "{ENV_KUBECONFIG} names '{}', which is not a file",
                        kubeconfig.display()
                    ));
                }
            }
            other => {
                return Err(format!(
                    "'{other}' is not a deferred materializer this gate provisions ({})",
                    MATERIALIZERS.join(", ")
                ));
            }
        }
        command.stdout(Stdio::from(log)).stderr(Stdio::from(stderr));
        self.archiver = Some(Archiver {
            child: command
                .spawn()
                .map_err(|error| format!("cannot start yesno-archive: {error}"))?,
            object_dir,
            log_path,
        });
        Ok(())
    }

    fn archiver(&self) -> Result<&Archiver, String> {
        self.archiver
            .as_ref()
            .ok_or("no archive sidecar is running; call aws_archive_start() first".into())
    }

    fn archive_diagnostics(&self) -> String {
        let Some(archiver) = &self.archiver else {
            return "no archive sidecar was started".into();
        };
        format!(
            "yesno-archive log (last bytes):\n{}",
            tail(&archiver.log_path)
        )
    }

    /// Wait until the object store holds a completed base manifest.
    ///
    /// A base is the whole point of the deferred path: it is only reachable
    /// through an ECS task that restored the snapshot, staged the file set, and
    /// exited zero. Polling the *store* rather than the task is deliberate —
    /// the archiver already validates the container's exit code, and asserting
    /// on the published result is what proves the two halves met.
    fn wait_archive_base(
        &mut self,
        timeout: Duration,
    ) -> Result<crate::server::ArchiveStats, String> {
        let deadline = Instant::now() + timeout;
        let object_dir = self.archiver()?.object_dir.clone();
        // Assigned on every pass before the deadline is consulted; the
        // message is only ever the *last* reason the store had nothing.
        let mut last;
        loop {
            if let Some(status) = self
                .archiver
                .as_mut()
                .ok_or("no archive sidecar is running")?
                .child
                .try_wait()
                .map_err(|error| format!("cannot poll yesno-archive: {error}"))?
            {
                return Err(format!(
                    "yesno-archive exited with {status} before publishing a base\n{}",
                    self.archive_diagnostics()
                ));
            }
            match crate::server::inspect_archive(&object_dir) {
                Ok(stats) => return Ok(stats),
                Err(error) => last = error,
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "no base was published within {}s: {last}\n{}",
                    timeout.as_secs(),
                    self.archive_diagnostics()
                ));
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    /// Wait for the sidecar to exit on its own, and report how.
    ///
    /// The failure half's assertion: a materializer that fails must take the
    /// archiver down with a message that names it, rather than leaving it
    /// retrying against a lease nobody will materialize.
    fn wait_archiver_exit(&mut self, timeout: Duration) -> Result<(Option<i32>, String), String> {
        let deadline = Instant::now() + timeout;
        loop {
            let exited = self
                .archiver
                .as_mut()
                .ok_or("no archive sidecar is running")?
                .child
                .try_wait()
                .map_err(|error| format!("cannot poll yesno-archive: {error}"))?;
            if let Some(status) = exited {
                let log = tail(&self.archiver()?.log_path);
                self.archiver = None;
                return Ok((status.code(), log));
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "yesno-archive was still running after {}s\n{}",
                    timeout.as_secs(),
                    self.archive_diagnostics()
                ));
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    fn stop_archiver(&mut self) -> Result<(), String> {
        let mut archiver = self
            .archiver
            .take()
            .ok_or("no archive sidecar is running".to_owned())?;
        archiver
            .child
            .kill()
            .map_err(|error| format!("cannot stop yesno-archive: {error}"))?;
        archiver
            .child
            .wait()
            .map_err(|error| format!("cannot reap yesno-archive: {error}"))?;
        Ok(())
    }

    /// Wait until the materializer owns between `minimum` and `maximum`
    /// volumes.
    ///
    /// A wait rather than an observation because every interesting moment is
    /// asynchronous: the restored volume appears while the worker is starting
    /// and goes when it stops, which is after the archiver has already been
    /// told the outcome. A timeout of zero is a plain observation that names
    /// the discrepancy.
    ///
    /// A range rather than an exact count because the scenarios use it for
    /// two different things. `(0, 0)` is the cleanup assertion. `(1, n)` is the
    /// one that keeps the other honest: a tag filter that matched nothing at
    /// all would satisfy `(0, 0)` at every point in the run and prove nothing,
    /// so each arm requires its filter to find the volume *while it exists*.
    /// Wait for the staged copy to disappear, which the archiver does *after*
    /// it has published.
    ///
    /// Why this is a wait while its failure-path twin is a plain read.
    /// `remove_staged_base` runs after `publish_captured_base` returns, so a
    /// base visible in the object store does not yet mean staging is empty --
    /// a live run lost exactly that race on 2026-09-02 and an earlier one had
    /// won it. The failure half observes an archiver that *exited on its own*,
    /// whose cleanup is therefore complete by definition, so an instantaneous
    /// read is the right observation there.
    ///
    /// It has to be taken while the archiver is still running. Stopping it
    /// aborts in-flight tasks, so a staged copy still present at that moment
    /// stays forever, and waiting after the stop would only turn a fast failure
    /// into a slow one.
    fn wait_staged_entries(&mut self, expected: usize, timeout: Duration) -> Result<usize, String> {
        let deadline = Instant::now() + timeout;
        loop {
            let observed = staged_entries()?;
            if observed == expected {
                return Ok(observed);
            }
            // Nothing will change once the archiver is gone, and it carries the
            // reason. Waiting out the full timeout would replace that reason
            // with an entry count.
            let gone = match self.archiver.as_mut() {
                Some(archiver) => archiver
                    .child
                    .try_wait()
                    .map_err(|error| format!("cannot poll yesno-archive: {error}"))?,
                None => None,
            };
            if let Some(status) = gone {
                return Err(format!(
                    "the staging directory holds {observed} entr(ies), wanted {expected}, and \
                     yesno-archive has already exited with {status}\n{}",
                    self.archive_diagnostics()
                ));
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "the staging directory still holds {observed} entr(ies), wanted {expected}, \
                     after {}s\n{}",
                    timeout.as_secs(),
                    self.archive_diagnostics()
                ));
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    fn wait_materializer_volumes(
        &mut self,
        minimum: usize,
        maximum: usize,
        timeout: Duration,
    ) -> Result<usize, String> {
        if minimum > maximum {
            return Err(format!("{minimum} is not a lower bound below {maximum}"));
        }
        let deadline = Instant::now() + timeout;
        loop {
            let ids = self.count_materializer_volumes()?;
            let observed = ids.len();
            if (minimum..=maximum).contains(&observed) {
                return Ok(observed);
            }
            let named = if ids.is_empty() {
                String::new()
            } else {
                format!(" ({})", ids.join(", "))
            };
            // Nothing will change once the archiver is gone, and it carries
            // the reason. Waiting out the full timeout would replace that
            // reason with a volume count.
            let gone = match self.archiver.as_mut() {
                Some(archiver) => archiver
                    .child
                    .try_wait()
                    .map_err(|error| format!("cannot poll yesno-archive: {error}"))?,
                None => None,
            };
            if let Some(status) = gone {
                return Err(format!(
                    "the deferred materializer owns {observed} volume(s){named}, wanted \
                     {minimum}..={maximum}, and yesno-archive has already exited with \
                     {status}\n{}",
                    self.archive_diagnostics()
                ));
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "the deferred materializer owns {observed} volume(s){named}; wanted \
                     {minimum}..={maximum}. A volume left by an earlier run fails this too: \
                     the filter keys on the namespace, which is not per-run. \
                     Clear them with scripts/gate-aws-destroy.sh --orphans"
                ));
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    /// Volumes the *materializer* owns, as opposed to the provider's own.
    ///
    /// The two arms are found by different tags because they are restored by
    /// different things. ECS creates the volume itself and copies the
    /// archiver's `yesno:lease` tag onto it; Kubernetes creates it through the
    /// EBS CSI driver, which knows only about the claim.
    ///
    /// Neither filter can be narrowed to one run. ECS's tag value is a lease
    /// token `yesno-archive` derives internally and never publishes, so what
    /// separates it from a volume the *EBS provider* made is the absence of the
    /// run tag, which only provider-created resources carry. The Kubernetes one
    /// is the claim's namespace, which is per-gate rather than per-run. Two
    /// deferred gates running at once in one account would count each other's
    /// volumes; the gate is opt-in and serial.
    ///
    /// The Kubernetes tag exists only because the EBS CSI driver runs with
    /// `--extra-create-metadata`, which is its default and the EKS add-on's.
    /// Were it off, this would find nothing rather than the wrong thing — which
    /// is why `deferred_eks.py` requires a nonzero count while the volume
    /// exists instead of only requiring zero afterwards.
    /// The ids, not a count. The scenarios open by requiring this to be
    /// empty, and a volume orphaned by an *earlier* run fails that precondition
    /// -- the EKS filter keys on the namespace, which is a constant, so one
    /// leak blocks every run after it. Two 26-minute runs were spent on
    /// "owns 1 volume(s)" without being told which one.
    fn count_materializer_volumes(&mut self) -> Result<Vec<String>, String> {
        let run_id = self.fixture()?.run_id.clone();
        let materializer = required_env(ENV_MATERIALIZER)?;
        let filter = match materializer.as_str() {
            "ecs" => Filter::builder()
                .name("tag-key")
                .values(MATERIALIZER_TAG)
                .build(),
            "eks" => Filter::builder()
                .name(format!("tag:{CLAIM_NAMESPACE_TAG}"))
                .values(required_env(ENV_EKS_NAMESPACE)?)
                .build(),
            other => {
                return Err(format!(
                    "'{other}' is not a deferred materializer this gate provisions ({})",
                    MATERIALIZERS.join(", ")
                ));
            }
        };
        let provider_owned = materializer == "ecs";
        let client = self
            .client
            .clone()
            .ok_or("AWS client was not initialized")?;
        self.runtime()?
            .block_on(async move {
                let volumes = client
                    .describe_volumes()
                    .filters(filter)
                    .send()
                    .await?
                    .volumes
                    .unwrap_or_default();
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(
                    volumes
                        .iter()
                        .filter(|volume| {
                            !provider_owned
                                || !volume.tags().iter().any(|tag| {
                                    tag.key() == Some(RUN_TAG)
                                        && tag.value() == Some(run_id.as_str())
                                })
                        })
                        .filter_map(|volume| volume.volume_id().map(ToOwned::to_owned))
                        .collect::<Vec<_>>(),
                )
            })
            .map_err(|error| error.to_string())
    }

    fn cleanup(&mut self) -> Result<(), String> {
        if self.archiver.is_some() {
            self.stop_archiver()?;
        }
        if self.lease.is_some() {
            self.release_snapshot()?;
        }
        for mut child in [self.child.take(), self.agent.take()].into_iter().flatten() {
            child
                .kill()
                .map_err(|error| format!("cannot stop an E2E process: {error}"))?;
            child
                .wait()
                .map_err(|error| format!("cannot reap an E2E process: {error}"))?;
        }
        let (snapshots, volumes) = self.resource_counts()?;
        if snapshots != 0 || volumes != 0 {
            return Err(format!(
                "AWS resources remain after cleanup: {snapshots} snapshot(s), {volumes} volume(s)"
            ));
        }
        self.fixture = None;
        self.client = None;
        Ok(())
    }
}

impl World {
    pub(crate) fn call_aws(
        &mut self,
        verb: &str,
        a: &Args<'_>,
    ) -> Result<MontyObject, MontyException> {
        match verb {
            "aws_prepare" => {
                a.exact(0)?;
                a.no_kwargs()?;
                self.aws
                    .prepare(&self.root, Materialization::Local)
                    .map_err(|error| aws_err(verb, error))?;
                Ok(MontyObject::String("ebs".into()))
            }
            "aws_prepare_deferred" => {
                a.exact(0)?;
                a.no_kwargs()?;
                self.aws
                    .prepare(&self.root, Materialization::Deferred)
                    .map_err(|error| aws_err(verb, error))?;
                Ok(MontyObject::String(
                    Materialization::Deferred.as_str().into(),
                ))
            }
            "aws_put" => {
                a.exact(2)?;
                a.no_kwargs()?;
                let key = a.u64(0)?;
                let ordinals = a.u64_list(1)?;
                let input = ordinals
                    .iter()
                    .map(|ordinal| format!("{key},{ordinal}\n"))
                    .collect::<String>();
                let mut child = Command::new("/usr/local/bin/yesno")
                    .args(["--endpoint", "http://127.0.0.1:50051", "put", "-"])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .map_err(|error| aws_err(verb, error))?;
                child
                    .stdin
                    .take()
                    .ok_or_else(|| aws_err(verb, "yesno stdin is absent"))?
                    .write_all(input.as_bytes())
                    .map_err(|error| aws_err(verb, error))?;
                checked_output(
                    "yesno put",
                    child
                        .wait_with_output()
                        .map_err(|error| aws_err(verb, error))?,
                )
                .map_err(|error| aws_err(verb, error))?;
                Ok(whole_obj(ordinals.len()))
            }
            "aws_get" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let output = Command::new("/usr/local/bin/yesno")
                    .args([
                        "--endpoint",
                        "http://127.0.0.1:50051",
                        "get",
                        &a.u64(0)?.to_string(),
                    ])
                    .output()
                    .map_err(|error| aws_err(verb, error))?;
                let output =
                    checked_output("yesno get", output).map_err(|error| aws_err(verb, error))?;
                let values = output
                    .lines()
                    .filter(|line| !line.trim().is_empty())
                    .map(|line| line.trim().parse::<u64>())
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|error| aws_err(verb, format!("invalid ordinal: {error}")))?;
                Ok(MontyObject::List(values.into_iter().map(int_obj).collect()))
            }
            "aws_checkpoint" => {
                a.exact(0)?;
                a.no_kwargs()?;
                self.aws
                    .checkpoint()
                    .map(int_obj)
                    .map_err(|error| aws_err(verb, error))
            }
            "aws_snapshot_begin" => {
                a.exact(0)?;
                a.no_kwargs()?;
                let lease = self
                    .aws
                    .begin_snapshot()
                    .map_err(|error| aws_err(verb, error))?;
                let pooled = lease
                    .device
                    .as_deref()
                    .is_some_and(|device| DEVICE_NAMES.contains(&device));
                Ok(dict(vec![
                    ("source", MontyObject::String(lease.source)),
                    ("files", whole_obj(lease.files)),
                    ("ttl", int_obj(lease.ttl)),
                    ("direct", MontyObject::Bool(lease.direct)),
                    ("mounted", MontyObject::Bool(lease.mounted)),
                    (
                        "device",
                        lease.device.map_or(MontyObject::None, MontyObject::String),
                    ),
                    ("pooled", MontyObject::Bool(pooled)),
                ]))
            }
            "aws_privileges" => {
                a.exact(0)?;
                a.no_kwargs()?;
                let (daemon, agent) = self
                    .aws
                    .privileges()
                    .map_err(|error| aws_err(verb, error))?;
                let field = |read: fn(&ProcessPrivileges) -> u64| {
                    agent
                        .as_ref()
                        .map_or(MontyObject::None, |a| int_obj(read(a)))
                };
                Ok(dict(vec![
                    ("daemon_pid", int_obj(u64::from(daemon.pid))),
                    ("daemon_uid", int_obj(daemon.uid)),
                    ("daemon_caps", int_obj(daemon.capabilities)),
                    ("agent_pid", field(|a| u64::from(a.pid))),
                    ("agent_uid", field(|a| a.uid)),
                    ("agent_caps", field(|a| a.capabilities)),
                ]))
            }
            "aws_snapshot_release" => {
                a.exact(0)?;
                a.no_kwargs()?;
                self.aws
                    .release_snapshot()
                    .map_err(|error| aws_err(verb, error))?;
                Ok(MontyObject::None)
            }
            "aws_resource_counts" => {
                a.exact(0)?;
                a.no_kwargs()?;
                let (snapshots, volumes) = self
                    .aws
                    .resource_counts()
                    .map_err(|error| aws_err(verb, error))?;
                Ok(dict(vec![
                    ("snapshots", whole_obj(snapshots)),
                    ("volumes", whole_obj(volumes)),
                ]))
            }
            "aws_wait_resource_counts" => {
                a.exact(3)?;
                a.no_kwargs()?;
                let expected_snapshots = usize::try_from(a.u64(0)?)
                    .map_err(|_| value_err("snapshot count does not fit usize"))?;
                let expected_volumes = usize::try_from(a.u64(1)?)
                    .map_err(|_| value_err("volume count does not fit usize"))?;
                let timeout = Duration::from_millis(a.u64(2)?);
                let (snapshots, volumes) = self
                    .aws
                    .wait_resource_counts(expected_snapshots, expected_volumes, timeout)
                    .map_err(|error| aws_err(verb, error))?;
                Ok(dict(vec![
                    ("snapshots", whole_obj(snapshots)),
                    ("volumes", whole_obj(volumes)),
                ]))
            }
            "aws_basebackup" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let name = a.str_at(0)?;
                if name.is_empty() || name.contains('/') || name == "." || name == ".." {
                    return Err(value_err(format!("{verb}(): unsafe backup name '{name}'")));
                }
                let report = self
                    .aws
                    .basebackup(self.root.join(name))
                    .map_err(|error| aws_err(verb, error))?;
                Ok(dict(vec![
                    ("shards", int_obj(u64::from(report.shards))),
                    ("checkpoint", int_obj(report.checkpoint_version)),
                    ("recovered", int_obj(report.recovered_version)),
                    ("bytes", int_obj(report.bytes)),
                ]))
            }
            "aws_crash_server" => {
                a.exact(0)?;
                a.no_kwargs()?;
                self.aws.crash().map_err(|error| aws_err(verb, error))?;
                Ok(MontyObject::None)
            }
            "aws_restart_server" => {
                a.exact(0)?;
                a.no_kwargs()?;
                self.aws.restart().map_err(|error| aws_err(verb, error))?;
                Ok(MontyObject::None)
            }
            "aws_cleanup" => {
                a.exact(0)?;
                a.no_kwargs()?;
                self.aws.cleanup().map_err(|error| aws_err(verb, error))?;
                Ok(MontyObject::Bool(true))
            }

            // What Terraform put in this runner's environment for the deferred
            // arm, read back rather than restated. The scenario needs the real
            // source path in order to pass a deliberately wrong one beside it.
            "aws_deferred_config" => {
                a.exact(0)?;
                a.no_kwargs()?;
                let materializer =
                    required_env(ENV_MATERIALIZER).map_err(|error| aws_err(verb, error))?;
                let names = archive_env(&materializer).map_err(|error| aws_err(verb, error))?;
                let mut pairs = Vec::with_capacity(names.len());
                for name in names {
                    pairs.push((
                        name,
                        MontyObject::String(
                            required_env(name).map_err(|error| aws_err(verb, error))?,
                        ),
                    ));
                }
                Ok(dict(pairs))
            }
            "aws_archive_start" => {
                // Three arguments, or five: the last two name one archive
                // variable to replace for this start. Not a dictionary, because
                // exactly one override is the contract -- a map would invite a
                // scenario to rewrite the archiver's whole environment.
                a.between(3, 5)?;
                a.no_kwargs()?;
                let object_dir = self.scenario_path(a.str_at(0)?, verb)?;
                let work_dir = self.scenario_path(a.str_at(1)?, verb)?;
                let source_path = a.str_at(2)?.to_owned();
                let override_env = match (a.opt_str(3)?, a.opt_str(4)?) {
                    (Some(name), Some(replacement)) => Some((name, replacement)),
                    (None, None) => None,
                    _ => {
                        return Err(value_err(
                            "an archive override needs both a name and a value",
                        ))
                    }
                };
                self.aws
                    .start_archiver(object_dir, work_dir, &source_path, override_env)
                    .map_err(|error| aws_err(verb, error))?;
                Ok(MontyObject::None)
            }
            "aws_archive_wait_base" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let timeout = Duration::from_millis(a.u64(0)?);
                let stats = self
                    .aws
                    .wait_archive_base(timeout)
                    .map_err(|error| aws_err(verb, error))?;
                Ok(dict(vec![
                    ("base_generation", int_obj(stats.base_generation)),
                    ("base_files", whole_obj(stats.base_files)),
                    ("wal_objects", whole_obj(stats.wal_objects)),
                    ("wal_bytes", int_obj(stats.wal_bytes)),
                    ("event_sequence", int_obj(stats.event_sequence)),
                ]))
            }
            "aws_archive_wait_exit" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let timeout = Duration::from_millis(a.u64(0)?);
                let (code, log) = self
                    .aws
                    .wait_archiver_exit(timeout)
                    .map_err(|error| aws_err(verb, error))?;
                Ok(dict(vec![
                    // `None` is a signalled death, which is not the same thing
                    // as a nonzero exit and must not read as one.
                    (
                        "code",
                        code.map_or(MontyObject::None, |code| MontyObject::Int(i64::from(code))),
                    ),
                    ("log", MontyObject::String(log)),
                ]))
            }
            "aws_archive_stop" => {
                a.exact(0)?;
                a.no_kwargs()?;
                self.aws
                    .stop_archiver()
                    .map_err(|error| aws_err(verb, error))?;
                Ok(MontyObject::None)
            }
            "aws_staged_entries" => {
                a.exact(0)?;
                a.no_kwargs()?;
                staged_entries()
                    .map(whole_obj)
                    .map_err(|error| aws_err(verb, error))
            }
            "aws_wait_staged_entries" => {
                a.exact(2)?;
                a.no_kwargs()?;
                let expected = usize::try_from(a.u64(0)?)
                    .map_err(|_| value_err("staged entry count does not fit usize"))?;
                let timeout = Duration::from_millis(a.u64(1)?);
                self.aws
                    .wait_staged_entries(expected, timeout)
                    .map(whole_obj)
                    .map_err(|error| aws_err(verb, error))
            }
            "aws_materializer_volumes" => {
                a.exact(3)?;
                a.no_kwargs()?;
                let bound = |i| {
                    usize::try_from(a.u64(i)?)
                        .map_err(|_| value_err("volume count does not fit usize"))
                };
                let (minimum, maximum) = (bound(0)?, bound(1)?);
                let timeout = Duration::from_millis(a.u64(2)?);
                self.aws
                    .wait_materializer_volumes(minimum, maximum, timeout)
                    .map(whole_obj)
                    .map_err(|error| aws_err(verb, error))
            }
            _ => Err(value_err(format!("{verb}() is not an AWS verb"))),
        }
    }
}

/// Exact AWS filters shared by observations and destructive failure cleanup.
///
/// The Terraform-owned source volume has the run tag but deliberately lacks
/// the object discriminator, so requiring both prevents cleanup from matching it.
#[must_use]
pub fn provider_resource_filters(run_id: &str) -> Vec<Filter> {
    vec![
        Filter::builder()
            .name("tag:yesno:e2e-run")
            .values(run_id)
            .build(),
        Filter::builder()
            .name("tag:yesno:e2e-object")
            .values("lease")
            .build(),
    ]
}

/// The tail of a log, for a failure message that has to be self-contained.
///
/// A gate whose feedback loop is a twenty-minute billable round trip cannot
/// send anyone back to the instance to read a file, so the message carries it.
fn tail(path: &Path) -> String {
    let bytes = std::fs::read(path).unwrap_or_default();
    let start = bytes.len().saturating_sub(32 * 1024);
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}

/// How many entries the shared staging filesystem currently holds.
///
/// Read from the *host* side of a filesystem the Fargate task writes to.
/// The archiver removes its staged base after publishing, so a nonzero count
/// once the sidecar has stopped means either a worker left something behind or
/// the archiver did — and neither is visible from the object store.
fn staged_entries() -> Result<usize, String> {
    let staging = PathBuf::from(required_env(ENV_STAGING_PATH)?);
    let entries = std::fs::read_dir(&staging)
        .map_err(|error| format!("cannot read '{}': {error}", staging.display()))?;
    let mut count = 0;
    for entry in entries {
        entry.map_err(|error| format!("cannot walk '{}': {error}", staging.display()))?;
        count += 1;
    }
    Ok(count)
}

/// Resolve an account from `/etc/passwd`.
///
/// The runner image is fixed and creates this account, so reading the file
/// directly is both sufficient and one less thing than binding `getpwnam`.
fn daemon_account(user: &str) -> Result<(u32, u32), String> {
    let passwd = std::fs::read_to_string("/etc/passwd")
        .map_err(|error| format!("cannot read /etc/passwd: {error}"))?;
    account_in_passwd(&passwd, user)
}

fn account_in_passwd(passwd: &str, user: &str) -> Result<(u32, u32), String> {
    for line in passwd.lines() {
        let mut fields = line.split(':');
        if fields.next() != Some(user) {
            continue;
        }
        let uid = fields.nth(1).and_then(|value| value.parse().ok());
        let gid = fields.next().and_then(|value| value.parse().ok());
        return match (uid, gid) {
            (Some(uid), Some(gid)) => Ok((uid, gid)),
            _ => Err(format!("account '{user}' has an unreadable uid or gid")),
        };
    }
    Err(format!(
        "the runner image has no '{user}' account for the unprivileged daemon"
    ))
}

fn chown_tree(path: &Path, uid: u32, gid: u32) -> Result<(), String> {
    std::os::unix::fs::chown(path, Some(uid), Some(gid))
        .map_err(|error| format!("cannot chown '{}': {error}", path.display()))?;
    if path.is_dir() {
        let entries = std::fs::read_dir(path)
            .map_err(|error| format!("cannot read '{}': {error}", path.display()))?;
        for entry in entries {
            let entry =
                entry.map_err(|error| format!("cannot walk '{}': {error}", path.display()))?;
            chown_tree(&entry.path(), uid, gid)?;
        }
    }
    Ok(())
}

/// Read one process's effective uid and effective capability mask.
///
/// `/proc` is the authority here rather than what the harness believes it
/// spawned: the assertion is worth nothing if it only re-reads the harness's
/// own intent.
fn process_privileges(pid: u32) -> Result<ProcessPrivileges, String> {
    let path = format!("/proc/{pid}/status");
    let status =
        std::fs::read_to_string(&path).map_err(|error| format!("cannot read '{path}': {error}"))?;
    privileges_in_status(&status, pid)
        .ok_or_else(|| format!("'{path}' did not report a uid and capability mask"))
}

fn privileges_in_status(status: &str, pid: u32) -> Option<ProcessPrivileges> {
    let mut uid = None;
    let mut capabilities = None;
    for line in status.lines() {
        if let Some(values) = line.strip_prefix("Uid:") {
            // real, effective, saved, filesystem
            uid = values
                .split_whitespace()
                .nth(1)
                .and_then(|value| value.parse().ok());
        } else if let Some(value) = line.strip_prefix("CapEff:") {
            capabilities = u64::from_str_radix(value.trim(), 16).ok();
        }
    }
    Some(ProcessPrivileges {
        pid,
        uid: uid?,
        capabilities: capabilities?,
    })
}

/// Report whether the lease's direct path is served by a mount the agent made
/// below the configured snapshot directory.
///
/// A direct path that merely exists proves nothing — a failed materialization
/// could leave an ordinary directory behind — so the check is that some mount
/// point strictly inside the snapshot directory is a prefix of the path.
fn direct_path_is_mounted(mount_dir: &Path, direct: &Path) -> Result<bool, String> {
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo")
        .map_err(|error| format!("cannot read /proc/self/mountinfo: {error}"))?;
    Ok(mount_covers_path(&mountinfo, mount_dir, direct))
}

fn mount_covers_path(mountinfo: &str, mount_dir: &Path, direct: &Path) -> bool {
    for line in mountinfo.lines() {
        let Some(point) = line.split_whitespace().nth(4) else {
            continue;
        };
        let point = Path::new(point);
        if point != mount_dir && point.starts_with(mount_dir) && direct.starts_with(point) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {

    /// The observer must outlast the observed, or the diagnosis is lost.
    ///
    /// The archiver reports *which* Kubernetes object it gave up on; the
    /// harness can only report that the archiver was still running. Whichever
    /// deadline fires first is the message the run produces, so the archiver's
    /// has to be the shorter one. Raising `MATERIALIZER_TIMEOUT_SECS` past a
    /// scenario's wait, or lowering a scenario's wait beneath it, silently
    /// trades a named failure for "still running after N seconds".
    #[test]
    fn an_archiver_timeout_is_shorter_than_the_wait_for_it_to_exit() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../e2e/aws");
        let mut checked = 0;
        for name in ["deferred_ecs.py", "deferred_eks.py"] {
            let source = std::fs::read_to_string(root.join(name)).unwrap();
            for (index, _) in source.match_indices("aws_archive_wait_exit(") {
                let rest = &source[index + "aws_archive_wait_exit(".len()..];
                let millis: u64 = rest[..rest.find(')').expect("a closing paren")]
                    .trim()
                    .parse()
                    .expect("a literal millisecond argument");
                assert!(
                    millis > MATERIALIZER_TIMEOUT_SECS * 1000,
                    "{name} waits {millis}ms for an archiver allowed \
                     {MATERIALIZER_TIMEOUT_SECS}s, so the harness gives up first \
                     and the archiver's own message is never produced"
                );
                checked += 1;
            }
        }
        // Not vacuous: a rename of the verb would otherwise make this pass
        // over nothing.
        assert!(
            checked >= 2,
            "found {checked} archiver waits, expected one per arm"
        );
    }

    use super::*;

    #[test]
    fn cleanup_selection_requires_run_and_provider_object_tags() {
        let filters = provider_resource_filters("run-1");
        assert_eq!(filters.len(), 2);
        assert!(filters.iter().any(|filter| {
            filter.name() == Some("tag:yesno:e2e-run") && filter.values() == ["run-1"]
        }));
        assert!(filters.iter().any(|filter| {
            filter.name() == Some("tag:yesno:e2e-object") && filter.values() == ["lease"]
        }));
    }

    /// The runner image's own line, as `useradd --system` writes it.
    const PASSWD: &str = concat!(
        "root:x:0:0:root:/root:/bin/bash\n",
        "daemon:x:1:1:daemon:/usr/sbin:/usr/sbin/nologin\n",
        "yesno:x:999:999::/home/yesno:/usr/sbin/nologin\n",
    );

    #[test]
    fn the_daemon_account_is_read_from_its_own_passwd_line() {
        assert_eq!(account_in_passwd(PASSWD, "yesno").unwrap(), (999, 999));
        assert_eq!(account_in_passwd(PASSWD, "root").unwrap(), (0, 0));
        // A missing account must name the problem: an image built without it
        // is the likely cause and is not otherwise visible from a spawn error.
        let error = account_in_passwd(PASSWD, "absent").unwrap_err();
        assert!(error.contains("no 'absent' account"), "{error}");
    }

    #[test]
    fn process_status_yields_the_effective_uid_and_capability_mask() {
        let daemon = concat!(
            "Name:\tyesnod\n",
            "Uid:\t999\t999\t999\t999\n",
            "CapEff:\t0000000000000000\n",
        );
        let parsed = privileges_in_status(daemon, 41).expect("status parses");
        assert_eq!((parsed.pid, parsed.uid, parsed.capabilities), (41, 999, 0));

        // The second column is the effective uid, which is the one that
        // matters: a process that merely started as root would still show 0 in
        // the first.
        let agent = concat!("Uid:\t0\t0\t0\t0\n", "CapEff:\t000001ffffffffff\n");
        let parsed = privileges_in_status(agent, 42).expect("status parses");
        assert_eq!(parsed.uid, 0);
        assert_ne!(parsed.capabilities, 0);

        assert!(privileges_in_status("Uid:\t0\t0\t0\t0\n", 43).is_none());
    }

    #[test]
    fn only_a_mount_below_the_snapshot_directory_vouches_for_a_direct_path() {
        let mounts = "25 1 259:1 / / rw,relatime - ext4 /dev/nvme0n1p1 rw\n31 25 259:2 / /mnt/yesno-source rw,relatime - ext4 /dev/nvme1n1 rw\n44 25 259:3 / /mnt/yesno-snapshots/yesno-snapshot-abc-1 ro,relatime - ext4 /dev/nvme2n1 ro\n";
        let snapshots = Path::new("/mnt/yesno-snapshots");
        assert!(mount_covers_path(
            mounts,
            snapshots,
            Path::new("/mnt/yesno-snapshots/yesno-snapshot-abc-1/data/000001.yno"),
        ));
        // A path under the snapshot directory that no mount covers is exactly
        // what a failed materialization leaves behind, so it must not pass.
        assert!(!mount_covers_path(
            mounts,
            snapshots,
            Path::new("/mnt/yesno-snapshots/yesno-snapshot-abc-2/data/000001.yno"),
        ));
        // Nor may the snapshot directory's own mount vouch for anything.
        let parent_only = "31 25 259:2 / /mnt/yesno-snapshots rw,relatime - ext4 /dev/nvme1n1 rw\n";
        assert!(!mount_covers_path(
            parent_only,
            snapshots,
            Path::new("/mnt/yesno-snapshots/yesno-snapshot-abc-1/data/000001.yno"),
        ));
    }

    fn fixture_for(materialization: Materialization, root: &Path) -> Fixture {
        Fixture {
            materialization,
            region: "us-east-1".into(),
            run_id: "run-1".into(),
            instance_id: "i-0123456789abcdef0".into(),
            volume_id: "vol-0123456789abcdef0".into(),
            availability_zone: "us-east-1a".into(),
            source_mount: root.join("source"),
            mount_dir: match materialization {
                Materialization::Local => root.join("snapshots"),
                Materialization::Deferred => PathBuf::new(),
            },
            config_path: root.join("yesnod.toml"),
            log_path: root.join("yesnod.log"),
            socket_path: root.join("run").join("control.sock"),
            agent_log_path: root.join("agent.log"),
            archive_log_path: root.join("archive.log"),
            daemon_uid: 999,
            daemon_gid: 999,
        }
    }

    /// Both arms are parsed and validated by the daemon's own loader.
    ///
    /// # Why this is worth a temporary directory
    ///
    /// The rendered TOML is the one thing in this module that can be wrong in a
    /// way nothing here notices: a section header in the wrong place puts
    /// `region` under `[server.snapshot]` instead of `[server.snapshot.ebs]`,
    /// which is still valid TOML. The gate would then fail on an EC2 instance,
    /// twenty billable minutes later, with a message about a missing region.
    /// `yesnod` is started with `--insecure --insecure-replication`, so that is
    /// the form validated here.
    #[test]
    fn both_arms_render_a_configuration_the_daemon_accepts() {
        for materialization in [Materialization::Local, Materialization::Deferred] {
            let root = tempfile::tempdir().expect("temporary directory");
            let fixture = fixture_for(materialization, root.path());
            std::fs::write(&fixture.config_path, server_config(&fixture))
                .expect("write the rendered configuration");
            let config =
                yesno_server::Config::from_file(&fixture.config_path).unwrap_or_else(|error| {
                    panic!(
                        "{} config does not parse: {error}",
                        materialization.as_str()
                    )
                });
            config.validate_with(true, true).unwrap_or_else(|error| {
                panic!("{} config is invalid: {error}", materialization.as_str())
            });
        }
    }

    /// One volume, one runner, two scenarios -- and two databases.
    ///
    /// The arms run one after the other against the same EBS fixture. If
    /// they shared a data directory the second would open the first's database
    /// and every assertion it made about its own writes would be about data it
    /// did not write.
    #[test]
    fn the_two_arms_do_not_share_a_database_directory() {
        let root = tempfile::tempdir().expect("temporary directory");
        let local = fixture_for(Materialization::Local, root.path());
        let deferred = fixture_for(Materialization::Deferred, root.path());
        assert_ne!(local.data_dir(), deferred.data_dir());
        assert_ne!(local.journal_dir(), deferred.journal_dir());
        // Both must still be below the source mount: the EBS provider snapshots
        // that volume and refuses a database that is not on it.
        assert!(local.data_dir().starts_with(&local.source_mount));
        assert!(deferred.data_dir().starts_with(&deferred.source_mount));
    }

    /// The deferred arm's claim, stated as an absence.
    ///
    /// Every name below is something only local materialization needs. If
    /// one of them reappears here, the deferred scenario is no longer proving
    /// that the path needs no privileged helper and no attachment name — it is
    /// merely not using one it was handed.
    #[test]
    fn the_deferred_configuration_names_no_local_materialization_machinery() {
        let root = tempfile::tempdir().expect("temporary directory");
        let rendered = server_config(&fixture_for(Materialization::Deferred, root.path()));
        assert!(
            rendered.contains("materialization = \"deferred\""),
            "{rendered}"
        );
        for absent in [
            "unix_socket",
            "mount_dir",
            "device_names",
            "instance_id",
            "availability_zone",
            "allow_direct_path",
        ] {
            assert!(!rendered.contains(absent), "{absent} is in {rendered}");
        }
        // What it does still need: the volume it snapshots, and where that
        // volume is mounted on this host.
        assert!(rendered.contains("volume_id = "), "{rendered}");
        assert!(rendered.contains("source_mount = "), "{rendered}");
    }

    #[test]
    fn the_rendered_configuration_offers_only_the_shared_device_pool() {
        let fixture = Fixture {
            materialization: Materialization::Local,
            region: "us-east-1".into(),
            run_id: "run-1".into(),
            instance_id: "i-0123".into(),
            volume_id: "vol-0123".into(),
            availability_zone: "us-east-1a".into(),
            source_mount: PathBuf::from("/mnt/yesno-source"),
            mount_dir: PathBuf::from("/mnt/yesno-snapshots"),
            config_path: PathBuf::from("/tmp/yesnod.toml"),
            log_path: PathBuf::from("/tmp/yesnod.log"),
            socket_path: PathBuf::from("/tmp/run/control.sock"),
            agent_log_path: PathBuf::from("/tmp/agent.log"),
            archive_log_path: PathBuf::from("/tmp/archive.log"),
            daemon_uid: 999,
            daemon_gid: 999,
        };
        let rendered = server_config(&fixture);
        for device in DEVICE_NAMES {
            assert!(
                rendered.contains(device),
                "{device} missing from {rendered}"
            );
        }
        // The agent reaches the daemon only over this socket, and local EBS
        // materialization is refused without it.
        assert!(
            rendered.contains("unix_socket = \"/tmp/run/control.sock\""),
            "{rendered}"
        );
        assert!(
            rendered.contains("materialization = \"local\""),
            "{rendered}"
        );
    }
}
