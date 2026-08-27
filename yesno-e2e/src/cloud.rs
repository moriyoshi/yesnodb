//! `cloud_*`: the host tools the real-AWS EBS gate needs, and nothing above them.
//!
//! [`crate::aws`] owns what happens *inside* the EC2 runner. Everything that
//! had to happen before any of it could run was, until 2026-09-02, two shell
//! scripts and a Terraform `local-exec` graph — an image push, an SSM round
//! trip, and a `.tftpl` rendering a ninety-line runner through cloud-init —
//! and Terraform reported one boolean for the lot. A gate costing twenty
//! billable minutes per attempt gets one chance to say *where* it broke, and
//! "provisioner returned non-zero" spends it.
//!
//! # Intrinsics, not a workflow
//!
//! These verbs are the smallest useful unit of *host authority*: one Terraform
//! subcommand, one output, one image push, the wait for the Systems Manager
//! channel, one remote command. They do not know the gate's sequence and must
//! not learn it. `e2e/aws/gate.py` owns the sequence, the runner script it
//! ships, and every assertion — so a change to what the runner does is a
//! change to a `.py` file rather than a recompile.
//!
//! The one thing the scenario is not trusted to restate is the environment the
//! runner image requires: [`runner_env`] builds it from
//! [`crate::aws::RUNNER_ENV`], the same list `aws_prepare` reads back on the
//! far side of that round trip.
//!
//! Systems Manager and ECR go through the `aws` CLI rather than the SDK
//! [`crate::aws`] already links. `aws-sdk-ssm` and `aws-sdk-ecr` would be new
//! crates in a workspace whose Bazel dependency set is pinned from
//! `Cargo.lock`, and nothing here observes anything a scenario asserts on —
//! those assertions all live on the runner side, where they already go through
//! the production SDK path. `op_*` owns `docker`, `kind` and `kubectl` on the
//! same terms.
//!
//! Every verb refuses unless `YESNO_AWS_GATE=1`, which
//! `scripts/gate-aws.sh` sets and nothing else does. Without it the harness's
//! own `every_advertised_verb_is_dispatched` test — which calls every verb with
//! no arguments — would create billable infrastructure during `cargo test`.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use monty_types::{ExcType, MontyException, MontyObject};

use crate::aws::{
    ENV_AVAILABILITY_ZONE, ENV_ECS_CLUSTER, ENV_ECS_CONTAINER, ENV_ECS_ROLE,
    ENV_ECS_SECURITY_GROUPS, ENV_ECS_SUBNETS, ENV_ECS_TASK_DEFINITION, ENV_ECS_VOLUME,
    ENV_EKS_IMAGE, ENV_EKS_NAMESPACE, ENV_EKS_SNAPSHOT_CLASS, ENV_EKS_STAGING_CLAIM,
    ENV_EKS_STORAGE_CLASS, ENV_INSTANCE_ID, ENV_KUBECONFIG, ENV_MATERIALIZER, ENV_REGION,
    ENV_RUN_ID, ENV_SNAPSHOT_MOUNT, ENV_SOURCE_MOUNT, ENV_SOURCE_PATH, ENV_STAGING_PATH,
    ENV_VOLUME_ID,
};
use crate::convert::{dict, value_err, Args};
use crate::operator::OPERATOR_ENV;
use crate::world::World;

type Result<T> = std::result::Result<T, String>;

/// Host intrinsics dispatched by the ordinary Monty scenario runner.
pub const OWNS: &[&str] = &[
    "cloud_config",
    "cloud_terraform",
    "cloud_output",
    "cloud_push_image",
    "cloud_runner_env",
    "cloud_deferred_env",
    "cloud_operator_env",
    "cloud_wait_managed",
    "cloud_run",
];

/// The opt-in switch. See the module header: without it, `cargo test` provisions AWS.
const OPT_IN: &str = "YESNO_AWS_GATE";
/// The Terraform subcommands this gate may issue. An allow-list rather than a
/// pass-through: `cloud_terraform` is a verb, not a shell.
const ACTIONS: [&str; 4] = ["init", "validate", "apply", "destroy"];
/// Which of those carry the run's state file and variables.
const STATEFUL: [&str; 2] = ["apply", "destroy"];
const SSM_DOCUMENT: &str = "AWS-RunShellScript";
/// How often the two waits re-ask. Both are minutes-long by nature, so a
/// tighter poll would only add API calls.
const POLL: Duration = Duration::from_secs(5);
/// Terraform's own validation caps `run_id` at 32 lowercase characters; it is
/// re-checked here so a bad one fails before `apply` rather than inside it.
const RUN_ID_MAX: usize = 32;

/// The Terraform output behind each variable a deferred arm needs.
///
/// `None` marks the one value the stack does not own: *which* materializer
/// to use is what the scenario is choosing, not something Terraform reports.
///
/// Paired here rather than in the scenario for the same reason [`runner_env`]
/// is — see its comment. The unit test below asserts each arm's left column is
/// exactly what [`crate::aws::archive_env`] requires, so a variable added on
/// the runner side cannot be forgotten on this one.
const DEFERRED_ENV_COMMON: [(&str, Option<&str>); 3] = [
    (ENV_MATERIALIZER, None),
    (ENV_SOURCE_PATH, Some("task_source_path")),
    (ENV_STAGING_PATH, Some("staging_mount")),
];

const DEFERRED_ENV_ECS: [(&str, Option<&str>); 7] = [
    (ENV_ECS_CLUSTER, Some("ecs_cluster")),
    (ENV_ECS_TASK_DEFINITION, Some("ecs_task_definition")),
    (ENV_ECS_CONTAINER, Some("ecs_container_name")),
    (ENV_ECS_VOLUME, Some("ecs_volume_name")),
    (ENV_ECS_ROLE, Some("ecs_infrastructure_role_arn")),
    (ENV_ECS_SUBNETS, Some("ecs_subnets")),
    (ENV_ECS_SECURITY_GROUPS, Some("ecs_security_groups")),
];

/// `driver_image` is the same per-run ECR image the ECS arm's task
/// definition names and the runner itself runs. One image, three roles: the
/// harness, the ECS worker, and this Job's worker.
const DEFERRED_ENV_EKS: [(&str, Option<&str>); 6] = [
    (ENV_EKS_NAMESPACE, Some("eks_namespace")),
    (ENV_EKS_IMAGE, Some("driver_image")),
    (ENV_EKS_SNAPSHOT_CLASS, Some("eks_snapshot_class")),
    (ENV_EKS_STORAGE_CLASS, Some("eks_storage_class")),
    (ENV_EKS_STAGING_CLAIM, Some("eks_staging_claim")),
    (ENV_KUBECONFIG, Some("kubeconfig_path")),
];

#[derive(Default)]
pub struct CloudState {
    run: Option<Run>,
}

impl CloudState {
    fn run(&self) -> std::result::Result<&Run, MontyException> {
        self.run
            .as_ref()
            .ok_or_else(|| value_err("call cloud_config() before any other cloud verb"))
    }
}

impl World {
    pub(crate) fn call_cloud(
        &mut self,
        verb: &str,
        a: &Args<'_>,
    ) -> std::result::Result<MontyObject, MontyException> {
        match verb {
            "cloud_config" => {
                a.exact(0)?;
                a.no_kwargs()?;
                if self.cloud.run.is_some() {
                    return Err(value_err("cloud_config(): this run is already configured"));
                }
                let run = Run::from_env().map_err(|error| cloud_err(verb, error))?;
                let run = self.cloud.run.insert(run);
                Ok(dict(vec![
                    ("region", MontyObject::String(run.region.clone())),
                    ("run_id", MontyObject::String(run.run_id.clone())),
                    (
                        "architecture",
                        MontyObject::String(run.architecture.clone()),
                    ),
                    (
                        "instance_type",
                        MontyObject::String(run.instance_type.clone().unwrap_or_default()),
                    ),
                    (
                        "state_dir",
                        MontyObject::String(run.state_dir.display().to_string()),
                    ),
                    // Which arms this run provisions. The scenario branches on
                    // it rather than deciding it, because the stack and the
                    // wrapper's destroy have to agree with whatever it says.
                    ("eks", MontyObject::Bool(run.eks)),
                    ("only", MontyObject::String(run.only.clone())),
                    // The scenario has to see this, not just `Drop`. Keeping
                    // the Terraform stack is worth little on its own: the
                    // runner scripts delete the Kubernetes namespace and stop
                    // the workers as they exit, so a retained cluster would
                    // still have nothing in it to look at.
                    ("keep", MontyObject::Bool(run.keep)),
                ]))
            }
            "cloud_terraform" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let action = a.str_at(0)?.to_owned();
                check_action(&action).map_err(|error| cloud_err(verb, error))?;
                // Raises the "call cloud_config() first" error before the
                // mutable borrow the flag below needs.
                self.cloud.run()?;
                let run = self.cloud.run.as_mut().expect("just checked");
                // Marked *before* the call, and cleared only after a destroy
                // that returned. An `apply` that fails partway still owns
                // resources, and forgetting that is how a failed run becomes a
                // monthly bill.
                if action == "apply" {
                    run.applied = true;
                }
                run.terraform_action(&action)
                    .map_err(|error| cloud_err(verb, error))?;
                if action == "destroy" {
                    run.applied = false;
                }
                Ok(MontyObject::Bool(true))
            }
            "cloud_output" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let name = a.str_at(0)?;
                let value = self
                    .cloud
                    .run()?
                    .output(name)
                    .map_err(|error| cloud_err(verb, error))?;
                Ok(MontyObject::String(value))
            }
            "cloud_push_image" => {
                a.exact(4)?;
                a.no_kwargs()?;
                let (dockerfile, image, platform, target) = (
                    a.str_at(0)?,
                    a.str_at(1)?,
                    a.str_at(2)?.to_owned(),
                    a.str_at(3)?.to_owned(),
                );
                self.cloud
                    .run()?
                    .push_image(dockerfile, image, &platform, &target)
                    .map_err(|error| cloud_err(verb, error))?;
                Ok(MontyObject::Bool(true))
            }
            "cloud_runner_env" => {
                a.exact(4)?;
                a.no_kwargs()?;
                let instance_id = a.str_at(0)?.to_owned();
                let volume_id = a.str_at(1)?.to_owned();
                let availability_zone = a.str_at(2)?.to_owned();
                let mounts = a.str_at(3)?.to_owned();
                let run = self.cloud.run()?;
                let pairs = runner_env(run, &instance_id, &volume_id, &availability_zone, &mounts)
                    .map_err(|error| cloud_err(verb, error))?;
                Ok(dict(
                    pairs
                        .into_iter()
                        .map(|(name, value)| (name, MontyObject::String(value)))
                        .collect(),
                ))
            }
            "cloud_deferred_env" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let materializer = a.str_at(0)?.to_owned();
                let run = self.cloud.run()?;
                let pairs =
                    deferred_env(run, &materializer).map_err(|error| cloud_err(verb, error))?;
                Ok(dict(
                    pairs
                        .into_iter()
                        .map(|(name, value)| (name, MontyObject::String(value)))
                        .collect(),
                ))
            }
            "cloud_operator_env" => {
                a.exact(0)?;
                a.no_kwargs()?;
                let run = self.cloud.run()?;
                let pairs = operator_env(run).map_err(|error| cloud_err(verb, error))?;
                Ok(dict(
                    pairs
                        .into_iter()
                        .map(|(name, value)| (name, MontyObject::String(value)))
                        .collect(),
                ))
            }
            "cloud_wait_managed" => {
                a.exact(2)?;
                a.no_kwargs()?;
                let instance_id = a.str_at(0)?.to_owned();
                let seconds = a.u64(1)?;
                self.cloud
                    .run()?
                    .wait_managed(&instance_id, Duration::from_secs(seconds))
                    .map_err(|error| cloud_err(verb, error))?;
                Ok(MontyObject::Bool(true))
            }
            "cloud_run" => {
                a.exact(3)?;
                a.no_kwargs()?;
                let instance_id = a.str_at(0)?.to_owned();
                let script = a.str_at(1)?.to_owned();
                let seconds = a.u64(2)?;
                let done = self
                    .cloud
                    .run()?
                    .run_remote(&instance_id, &script, seconds)
                    .map_err(|error| cloud_err(verb, error))?;
                Ok(dict(vec![
                    ("status", MontyObject::String(done.status)),
                    ("exit_code", MontyObject::Int(done.exit_code)),
                    ("stdout", MontyObject::String(done.stdout)),
                    ("stderr", MontyObject::String(done.stderr)),
                ]))
            }
            other => Err(value_err(format!("{other}() is not a harness verb"))),
        }
    }
}

fn cloud_err(verb: &str, error: impl std::fmt::Display) -> MontyException {
    MontyException::new(
        ExcType::RuntimeError,
        Some(format!("{verb}(): AWS orchestration failed: {error}")),
    )
}

/// One Systems Manager command invocation, after it reached a terminal state.
struct Invocation {
    status: String,
    /// Systems Manager's `ResponseCode`, which is `-1` when no exit status was
    /// collected at all. That is not an exit code and must not read as one, so
    /// it is passed through rather than folded into a number.
    exit_code: i64,
    stdout: String,
    stderr: String,
}

/// Everything one gate run needs from its environment, resolved once.
struct Run {
    workspace: PathBuf,
    region: String,
    run_id: String,
    architecture: String,
    instance_type: Option<String>,
    state_dir: PathBuf,
    data_dir: PathBuf,
    state_file: PathBuf,
    var_file: PathBuf,
    keep: bool,
    /// Whether this run provisions the deferred EKS arm.
    ///
    /// Default on. It is the only live coverage that path has anywhere, and
    /// an arm that is off by default is an arm that rots; `YESNO_AWS_EKS=0` is
    /// for a developer iterating on something else, and buys back the fifteen
    /// minutes a cluster takes to create and the ten it takes to destroy.
    eks: bool,
    /// Which single arm to run, or empty for all of them.
    ///
    /// This one *reduces* coverage, unlike `eks`, so it is validated rather
    /// than merely read: an unrecognised value aborts the run instead of
    /// silently matching no arm and testing nothing. `YESNO_AWS_ONLY=eks` skips
    /// the local and ECS arms, which pass and cost about twelve minutes to
    /// re-prove, so that a developer iterating on the Kubernetes bootstrap
    /// waits for the stack and the runner and nothing else.
    ///
    /// It is not a Terraform variable. The whole stack is still built, so
    /// the wrapper's destroy cannot disagree with the apply about what exists
    /// -- which is the property `eks` needed a `gate.tfvars` entry to keep.
    only: String,
    /// Whether an `apply` has run without a matching `destroy`. The only piece
    /// of sequence this module tracks, and it tracks it to be able to clean up
    /// after a scenario that failed rather than to enforce an order.
    applied: bool,
}

impl Run {
    fn from_env() -> Result<Self> {
        if env::var(OPT_IN).ok().as_deref() != Some("1") {
            return Err(format!(
                "the real-AWS gate is opt-in and creates billable resources; it runs only with \
                 {OPT_IN}=1, which scripts/gate-aws.sh sets"
            ));
        }
        let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .ok_or("yesno-e2e has no workspace parent")?
            .to_path_buf();

        let region = env::var("AWS_REGION")
            .or_else(|_| env::var("AWS_DEFAULT_REGION"))
            .ok()
            .filter(|value| !value.is_empty())
            .ok_or("set AWS_REGION or AWS_DEFAULT_REGION")?;

        let run_id = match env::var("YESNO_AWS_RUN_ID") {
            Ok(value) if !value.is_empty() => value,
            _ => generated_run_id()?,
        };
        check_run_id(&run_id)?;

        // Checked here, before `cloud_terraform("apply")` can be reached.
        // `docker info` rather than `docker --version`: a CLI with no reachable
        // daemon passes the latter and then fails at the image push, with the
        // stack already standing.
        checked_output(
            Command::new("terraform").arg("version"),
            "terraform version",
        )?;
        checked_output(Command::new("aws").arg("--version"), "aws --version")?;
        checked_output(Command::new("docker").arg("info"), "docker info")?;

        let architecture = match env::var("YESNO_AWS_ARCHITECTURE") {
            Ok(value) if !value.is_empty() => value,
            _ => normalize_architecture(&checked_output(
                Command::new("docker").args(["info", "--format", "{{.Architecture}}"]),
                "docker info",
            )?)?,
        };
        normalize_architecture(&architecture)?;

        // One directory per run holds Terraform's provider cache, its state
        // and its variables, so two runs in one checkout cannot collide.
        // `scripts/gate-aws.sh` names the same directory, which is how its
        // signal backstop finds the state this module wrote.
        let state_dir = match env::var("YESNO_AWS_STATE_DIR") {
            Ok(value) if !value.is_empty() => PathBuf::from(value),
            _ => crate::scratch_dir(&workspace).join(format!("aws-{run_id}")),
        };
        fs::create_dir_all(&state_dir)
            .map_err(|error| format!("cannot create {}: {error}", state_dir.display()))?;

        // Opt *out*, unlike every other switch here: only an explicit "0"
        // turns the arm off, so a typo leaves the gate testing more rather than
        // silently less.
        let eks = env::var("YESNO_AWS_EKS").ok().as_deref() != Some("0");
        let only = check_only(env::var("YESNO_AWS_ONLY").unwrap_or_default(), eks)?;

        let run = Self {
            data_dir: state_dir.join("terraform"),
            state_file: state_dir.join("terraform.tfstate"),
            var_file: state_dir.join("gate.tfvars"),
            instance_type: env::var("YESNO_AWS_INSTANCE_TYPE")
                .ok()
                .filter(|value| !value.is_empty()),
            keep: env::var("YESNO_AWS_KEEP").is_ok_and(|value| value == "1"),
            eks,
            only,
            state_dir,
            workspace,
            region,
            run_id,
            architecture,
            applied: false,
        };
        run.write_var_file()?;
        Ok(run)
    }

    /// Write the run's variables next to its state.
    ///
    /// A file rather than repeated `-var` flags because it is also the
    /// interface to the wrapper's signal backstop: `scripts/gate-aws.sh`
    /// destroys with this same file, so the architecture the Docker host
    /// selected cannot disagree between the two destroy paths.
    ///
    /// `eks` belongs here for a sharper version of the same reason. A
    /// `destroy` that disagreed with the `apply` about whether the arm exists
    /// would either fail on a resource the state does not have or, worse, leave
    /// a cluster standing that nothing else will ever remove.
    fn write_var_file(&self) -> Result<()> {
        let mut body = String::new();
        let _ = writeln!(body, "region        = {}", tf_string(&self.region));
        let _ = writeln!(body, "run_id        = {}", tf_string(&self.run_id));
        let _ = writeln!(body, "architecture  = {}", tf_string(&self.architecture));
        let _ = writeln!(body, "eks           = {}", self.eks);
        if let Some(instance_type) = &self.instance_type {
            let _ = writeln!(body, "instance_type = {}", tf_string(instance_type));
        }
        fs::write(&self.var_file, body)
            .map_err(|error| format!("cannot write {}: {error}", self.var_file.display()))
    }

    fn terraform_action(&self, action: &str) -> Result<()> {
        let mut command = self.terraform_command(action)?;
        checked_reporting_stderr(&mut command, &format!("terraform {action}"))
    }

    /// The argv for one Terraform action, built but not run.
    ///
    /// Separated from running it so the flags can be asserted. Nothing did
    /// before, and `-no-color` is exactly the kind of flag that is easy to add
    /// to one call and forget on the next.
    fn terraform_command(&self, action: &str) -> Result<Command> {
        check_action(action)?;
        let mut command = self.terraform();
        command.arg(action);
        // Always, on every action. Terraform suppresses colour on its own
        // when it thinks it is not talking to a terminal, and that guess is not
        // one to depend on: this gate's own log is full of escape sequences,
        // and every reading of it so far has begun by stripping them.
        //
        // It matters beyond the log being tidy. `checked_reporting_stderr`
        // puts Terraform's stderr tail into the error a scenario raises, so
        // without this the escape sequences end up *inside* a Python
        // AssertionError, wrapped in quotes, in the one message that has to be
        // readable. `TF_IN_AUTOMATION` is already set; this is its companion.
        command.arg("-no-color");
        if action == "init" {
            command.arg("-input=false");
        }
        if STATEFUL.contains(&action) {
            command
                .args(["-auto-approve", "-input=false"])
                .arg(format!("-state={}", self.state_file.display()))
                .arg(format!("-var-file={}", self.var_file.display()));
        }
        Ok(command)
    }

    fn terraform(&self) -> Command {
        let mut command = Command::new("terraform");
        command
            .arg(format!(
                "-chdir={}",
                self.workspace.join("e2e/aws").display()
            ))
            .env("TF_DATA_DIR", &self.data_dir)
            .env("TF_IN_AUTOMATION", "1");
        command
    }

    fn output(&self, name: &str) -> Result<String> {
        let value = checked_output(
            self.terraform()
                .args(["output", "-no-color", "-raw"])
                .arg(format!("-state={}", self.state_file.display()))
                .arg(name),
            &format!("terraform output {name}"),
        )?;
        let value = value.trim().to_owned();
        if value.is_empty() {
            return Err(format!("Terraform output {name} is empty"));
        }
        Ok(value)
    }

    /// Log in to the image's own registry and push a native build of it.
    ///
    /// Login and push are one verb because the credential is scoped to the
    /// registry in the image reference; separating them would invite a push to
    /// a registry the scenario never authenticated against.
    /// Build one stage of a Dockerfile and push it to the run's ECR repository.
    ///
    /// `target` is not optional in practice even though the caller may omit
    /// it. `docker build` with no `--target` builds the **last** stage, and the
    /// runner Dockerfile deliberately ends with two ENTRYPOINT stages that the
    /// deferred arms must never receive -- an ENTRYPOINT there would prepend
    /// `yesnod` to the staging worker's argv.
    fn push_image(
        &self,
        dockerfile: &str,
        image: &str,
        platform: &str,
        target: &str,
    ) -> Result<()> {
        let registry = image
            .split_once('/')
            .map(|(registry, _)| registry.to_owned())
            .ok_or_else(|| format!("{image} is not a registry-qualified image reference"))?;
        let dockerfile = self.workspace.join(dockerfile);
        if !dockerfile.is_file() {
            return Err(format!("{} does not exist", dockerfile.display()));
        }

        let password = checked_output(
            self.aws().args(["ecr", "get-login-password"]),
            "aws ecr get-login-password",
        )?;
        // Piped rather than passed as an argument: a registry password on a
        // command line is visible in every process listing on the host.
        let mut login = Command::new("docker")
            .args(["login", "--username", "AWS", "--password-stdin", &registry])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .map_err(|error| format!("cannot run docker login: {error}"))?;
        login
            .stdin
            .as_mut()
            .ok_or("docker login has no stdin")?
            .write_all(password.trim_end().as_bytes())
            .map_err(|error| format!("cannot write the ECR password: {error}"))?;
        drop(login.stdin.take());
        let status = login
            .wait()
            .map_err(|error| format!("cannot wait for docker login: {error}"))?;
        if !status.success() {
            return Err(format!("docker login exited with {status}"));
        }

        let mut build = Command::new("docker");
        build
            .current_dir(&self.workspace)
            .args(["buildx", "build", "--platform", platform, "--file"])
            .arg(&dockerfile)
            .args(["--target", target])
            .args(["--tag", image, "--push"])
            .arg(&self.workspace);
        checked_status(&mut build, "docker buildx build --push")
    }

    /// Wait until Systems Manager reports the runner as a managed instance.
    ///
    /// The boundary between "EC2 says the instance is running" and "the
    /// instance can be told to do something", and the reason the gate needs no
    /// ingress rule at all.
    fn wait_managed(&self, instance_id: &str, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        let mut last = String::new();
        let started = Instant::now();
        let mut announced = Instant::now();
        progress(&format!(
            "waiting for {instance_id} to register with Systems Manager"
        ));
        while Instant::now() < deadline {
            match checked_output(
                self.aws().args([
                    "ssm",
                    "describe-instance-information",
                    "--filters",
                    &format!("Key=InstanceIds,Values={instance_id}"),
                    "--query",
                    "length(InstanceInformationList)",
                    "--output",
                    "text",
                ]),
                "aws ssm describe-instance-information",
            ) {
                Ok(count) if count.trim() == "1" => return Ok(()),
                Ok(count) => last = format!("Systems Manager reported {} instances", count.trim()),
                Err(error) => last = error,
            }
            if announced.elapsed() >= HEARTBEAT {
                announced = Instant::now();
                progress(&format!(
                    "{instance_id} has not registered after {}s: {last}",
                    started.elapsed().as_secs()
                ));
            }
            thread::sleep(POLL);
        }
        Err(format!(
            "the EC2 runner did not register with Systems Manager within {}s: {last}",
            timeout.as_secs()
        ))
    }

    /// Run one shell script on the runner and wait for its terminal status.
    ///
    /// The script is the scenario's, which is the point: what the runner
    /// does is the part of this gate most likely to need changing, and it
    /// should not need a recompile. The authority is bounded to an instance
    /// this run created and destroys, in a VPC with no ingress.
    fn run_remote(&self, instance_id: &str, script: &str, timeout: u64) -> Result<Invocation> {
        // Through a file rather than an inline `--parameters` string: the
        // script is multi-line shell and quoting it through both a JSON
        // document and an argv would be two escaping layers deep.
        let parameters = self.state_dir.join("ssm-parameters.json");
        fs::write(
            &parameters,
            format!("{{\"commands\":[{}]}}", json_string(script)),
        )
        .map_err(|error| format!("cannot write {}: {error}", parameters.display()))?;

        let command_id = checked_output(
            self.aws()
                .args([
                    "ssm",
                    "send-command",
                    "--instance-ids",
                    instance_id,
                    "--document-name",
                    SSM_DOCUMENT,
                    "--timeout-seconds",
                    &timeout.to_string(),
                    "--query",
                    "Command.CommandId",
                    "--output",
                    "text",
                    "--parameters",
                ])
                .arg(format!("file://{}", parameters.display())),
            "aws ssm send-command",
        )?;
        let command_id = command_id.trim().to_owned();
        if command_id.is_empty() {
            return Err("Systems Manager returned no command id".to_owned());
        }
        let label = script_label(script);
        progress(&format!(
            "{label} sent to {instance_id} as {command_id} (timeout {timeout}s)"
        ));

        // Generous over the document's own timeout: SSM stops the command at
        // that point and the terminal status still has to be read back.
        let deadline = Instant::now() + Duration::from_secs(timeout + 120);
        let mut status = String::from("Pending");
        let started = Instant::now();
        let mut announced = Instant::now();
        while Instant::now() < deadline {
            // An invocation is briefly absent right after `send-command`, so a
            // failed query here means "not yet", not an error.
            if let Ok(current) = self.invocation_field(&command_id, instance_id, "Status") {
                status = current;
                if is_terminal(&status) {
                    break;
                }
            }
            // Not every poll. A twenty-minute scenario polls hundreds of
            // times, and a heartbeat that scrolls the failure off the screen is
            // no better than the silence it replaced.
            if announced.elapsed() >= HEARTBEAT {
                announced = Instant::now();
                progress(&format!(
                    "{label} is {status} after {}s",
                    started.elapsed().as_secs()
                ));
            }
            thread::sleep(POLL);
        }

        let stdout = self
            .invocation_field(&command_id, instance_id, "StandardOutputContent")
            .unwrap_or_default();
        let stderr = self
            .invocation_field(&command_id, instance_id, "StandardErrorContent")
            .unwrap_or_default();
        if !is_terminal(&status) {
            return Err(format!(
                "the command was still {status} after {}s:\n{stdout}\n{stderr}",
                timeout + 120
            ));
        }
        let exit_code = self
            .invocation_field(&command_id, instance_id, "ResponseCode")
            .ok()
            .and_then(|value| value.trim().parse::<i64>().ok())
            .unwrap_or(-1);
        progress(&format!(
            "{label} {status} with exit {exit_code} after {}s",
            started.elapsed().as_secs()
        ));
        Ok(Invocation {
            status,
            exit_code,
            stdout,
            stderr,
        })
    }

    fn invocation_field(&self, command_id: &str, instance_id: &str, field: &str) -> Result<String> {
        let value = checked_output(
            self.aws().args([
                "ssm",
                "get-command-invocation",
                "--command-id",
                command_id,
                "--instance-id",
                instance_id,
                "--query",
                field,
                "--output",
                "text",
            ]),
            &format!("aws ssm get-command-invocation --query {field}"),
        )?;
        // `--output text` prints a bare `None` for an absent field.
        let trimmed = value.trim_end_matches('\n');
        Ok(if trimmed == "None" {
            String::new()
        } else {
            trimmed.to_owned()
        })
    }

    fn aws(&self) -> Command {
        let mut command = Command::new("aws");
        command.args(["--region", &self.region]);
        command
    }
}

/// The guarantee the shell wrapper used to make with `trap ... EXIT`.
///
/// A destructor cannot cover a signal, which is why `scripts/gate-aws.sh`
/// still runs a best-effort `terraform destroy` after the harness exits. This
/// covers the case a signal does not: a failed assertion, a raised verb, or the
/// runner's own time limit, all of which unwind through here with the stack
/// still standing.
impl Drop for Run {
    fn drop(&mut self) {
        if !self.applied {
            return;
        }
        if self.keep {
            eprintln!(
                "gate-aws: YESNO_AWS_KEEP=1; retaining run {} under {}",
                self.run_id,
                self.state_dir.display()
            );
            return;
        }
        eprintln!(
            "gate-aws: the scenario left run {} standing; destroying it",
            self.run_id
        );
        if let Err(error) = self.terraform_action("destroy") {
            eprintln!("gate-aws: destroying run {} failed: {error}", self.run_id);
            // The stack is still standing and nothing else will remove it:
            // the wrapper's trap runs the same command and fails the same way,
            // and an EKS cluster bills by the hour until someone acts. A run on
            // 2026-09-02 whose SSO session expired mid-flight left 65 resources
            // up for over an hour.
            //
            // The command is printed rather than described because every path
            // in it is per-run -- the state file, the variables and the data
            // directory are all under this run's own directory, and no default
            // invocation of Terraform would find any of them.
            eprintln!(
                "gate-aws: run {} is STILL STANDING and bills until removed. \
                 Restore credentials, then run:\n  \
                 TF_DATA_DIR={} terraform -chdir={} destroy -auto-approve \
                 -input=false -state={} -var-file={}",
                self.run_id,
                self.data_dir.display(),
                self.workspace.join("e2e/aws").display(),
                self.state_file.display(),
                self.var_file.display(),
            );
        }
    }
}

/// What the runner image must be given, keyed by the names [`crate::aws`]
/// reads back on the instance.
///
/// The two halves of this gate run on different machines, separated by a
/// billable twenty-minute round trip. Building the environment here, from the
/// same list `aws_prepare` requires, is what stops a variable added on one side
/// from being silently absent on the other — so the scenario composes its
/// `docker run` flags by iterating this dict, and never by naming the
/// variables itself.
fn runner_env(
    run: &Run,
    instance_id: &str,
    volume_id: &str,
    availability_zone: &str,
    mounts: &str,
) -> Result<Vec<(&'static str, String)>> {
    let (source, snapshots) = mounts
        .split_once(':')
        .ok_or("the mounts argument must be \"<source>:<snapshots>\"")?;
    for mount in [source, snapshots] {
        if !mount.starts_with('/') {
            return Err(format!("{mount} is not an absolute path"));
        }
    }
    Ok(vec![
        (ENV_REGION, run.region.clone()),
        (ENV_RUN_ID, run.run_id.clone()),
        (ENV_INSTANCE_ID, instance_id.to_owned()),
        (ENV_VOLUME_ID, volume_id.to_owned()),
        (ENV_AVAILABILITY_ZONE, availability_zone.to_owned()),
        (ENV_SOURCE_MOUNT, source.to_owned()),
        (ENV_SNAPSHOT_MOUNT, snapshots.to_owned()),
    ])
}

/// What one deferred arm's container must be given, on top of [`runner_env`].
///
/// These are `yesno-archive`'s *own* documented variable names — plus
/// `KUBECONFIG`, which is Kubernetes'. Filling them from Terraform and handing
/// them to the shipped binary is the assertion that a deployment configured
/// only through the documented surface can materialize a provisional lease,
/// which is the whole claim the deferred path makes.
fn deferred_env(run: &Run, materializer: &str) -> Result<Vec<(&'static str, String)>> {
    let arm = match materializer {
        "ecs" => DEFERRED_ENV_ECS.as_slice(),
        "eks" => DEFERRED_ENV_EKS.as_slice(),
        other => {
            return Err(format!(
                "'{other}' is not a deferred materializer this gate provisions ({})",
                crate::aws::MATERIALIZERS.join(", ")
            ));
        }
    };
    let mut pairs = Vec::with_capacity(DEFERRED_ENV_COMMON.len() + arm.len());
    for (name, output) in DEFERRED_ENV_COMMON.iter().chain(arm) {
        let value = match output {
            Some(output) => run.output(output)?,
            None => materializer.to_owned(),
        };
        pairs.push((*name, value));
    }
    Ok(pairs)
}

/// What the operator arm's container must be given.
///
/// The pairing lives in [`crate::operator::OPERATOR_ENV`] rather than here,
/// because the far side is what fails when it is wrong: a name added there and
/// forgotten here would abort `op_prepare("eks")` several minutes into a
/// billable run. This function only resolves the right column.
///
/// `region` is the one value that is not a Terraform output. It is the run's
/// own region, which the stack is *given* rather than reports.
fn operator_env(run: &Run) -> Result<Vec<(&'static str, String)>> {
    let mut pairs = Vec::with_capacity(OPERATOR_ENV.len());
    for (name, output) in OPERATOR_ENV {
        let value = if *output == "region" {
            run.region.clone()
        } else {
            run.output(output)?
        };
        pairs.push((*name, value));
    }
    Ok(pairs)
}

/// `cloud_terraform` is a verb, not a shell: only these four subcommands, and
/// only ever as the whole argument.
fn check_action(action: &str) -> Result<()> {
    if ACTIONS.contains(&action) {
        Ok(())
    } else {
        Err(format!(
            "{action} is not one of the Terraform actions this gate issues ({})",
            ACTIONS.join(", ")
        ))
    }
}

/// Whether Systems Manager will report anything further about an invocation.
fn is_terminal(status: &str) -> bool {
    matches!(
        status,
        "Success" | "Cancelled" | "Failed" | "TimedOut" | "Cancelling"
    )
}

/// A quoted HCL string, for the one variables file this module writes.
fn tf_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// A JSON string literal, for the one document this module writes.
fn json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Terraform validates this too; checking it here turns a bad run id into an
/// immediate message instead of an `apply` that fails after provider setup.
/// The arms `YESNO_AWS_ONLY` may name, and nothing else.
///
/// Rejecting an unknown value is the whole point. A switch that skips work
/// fails the wrong way round if a typo means "match nothing": the run would go
/// green having tested no arm at all, which is the one outcome a gate must
/// never produce. `YESNO_AWS_EKS` can afford to be lenient because a typo there
/// leaves the gate testing *more*.
const ARMS: [&str; 4] = ["local", "ecs", "eks", "operator"];

fn check_only(only: String, eks: bool) -> Result<String> {
    if !only.is_empty() && !ARMS.contains(&only.as_str()) {
        return Err(format!(
            "YESNO_AWS_ONLY={only:?} names no arm; use one of {} or leave it unset for all",
            ARMS.join(", ")
        ));
    }
    // The two switches can be set to mean "run the EKS arm and do not build
    // it". Provisioning a cluster and running nothing on it is the expensive
    // half of that mistake; the cheap half is the green result it would print.
    // `operator` is on the same footing: it drives a YesnoCluster on the
    // EKS cluster, so `YESNO_AWS_EKS=0` leaves it nothing to run against.
    if (only == "eks" || only == "operator") && !eks {
        return Err(format!(
            "YESNO_AWS_ONLY={only} needs the EKS cluster; YESNO_AWS_EKS=0 turns it off"
        ));
    }
    Ok(only)
}

fn check_run_id(run_id: &str) -> Result<()> {
    let ok = (2..=RUN_ID_MAX).contains(&run_id.len())
        && run_id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && run_id.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit());
    if ok {
        Ok(())
    } else {
        Err(format!(
            "run id {run_id:?} must be 2-{RUN_ID_MAX} lowercase letters, digits or hyphens, \
             starting with a letter or digit"
        ))
    }
}

fn generated_run_id() -> Result<String> {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("the clock is before the epoch: {error}"))?
        .as_secs();
    Ok(format!("yn-{seconds}-{}", std::process::id()))
}

/// The two architectures this gate has an AMI and an instance type for.
fn normalize_architecture(reported: &str) -> Result<String> {
    match reported.trim() {
        "amd64" | "x86_64" => Ok("x86_64".to_owned()),
        "arm64" | "aarch64" => Ok("arm64".to_owned()),
        other => Err(format!(
            "architecture {other} has no AMI in this gate; set YESNO_AWS_ARCHITECTURE"
        )),
    }
}

/// Run a command, and put what it said on its stderr into the failure.
///
/// `checked_status` inherits both streams, so a `terraform apply` that fails
/// after twenty minutes reports `exited with exit status: 1` and nothing else.
/// The error itself -- which resource, and why -- is in the operator's console
/// and not in the scenario failure, where every other diagnosis in this gate
/// lives. That cost a 43-minute run on 2026-09-02 whose only finding was a
/// resource name.
///
/// stdout stays inherited. Terraform streams progress there, and a plan or
/// apply that only printed on completion would look hung for twenty minutes.
/// Errors go to stderr, so piping just that one gets the diagnosis without
/// costing the progress. This is why `checked_status` is left alone for
/// `docker buildx`, which writes its whole progress display to stderr.
/// One progress line, on stderr.
///
/// stderr, not `print()` from the scenario. A scenario's own output is
/// captured and shown only when it fails or when `--show-output` is passed, so
/// a `print()` here would say nothing at the moment it is needed. These lines
/// go straight to the terminal the gate is running in.
///
/// Why they exist at all: `cloud_run` sends one Systems Manager command and
/// polls for its terminal status, printing nothing until it returns. With the
/// EKS arm on, that is four silent stretches of several minutes each inside a
/// run that already takes forty. A gate that looks hung gets killed, and on
/// 2026-09-02 one was reported as stuck while it was two minutes from finishing
/// `ebs.py`.
fn progress(message: &str) {
    eprintln!("gate-aws: {message}");
}

/// What to call a runner script in a progress line.
///
/// The prologue `gate.py` builds is sorted `name=value` lines, so a scenario
/// run carries its own path in `scenario=`. Everything else -- provisioning,
/// the Kubernetes bootstrap -- has no such line and is named generically.
/// Deliberately not the whole script: these are hundreds of lines of shell.
fn script_label(script: &str) -> String {
    for line in script.lines() {
        if let Some(scenario) = line.strip_prefix("scenario=") {
            let scenario = scenario.trim();
            if !scenario.is_empty() {
                return scenario.to_owned();
            }
        }
    }
    "a runner script".to_owned()
}

/// Between progress lines while a command is in flight.
const HEARTBEAT: Duration = Duration::from_secs(30);

fn checked_reporting_stderr(command: &mut Command, label: &str) -> Result<()> {
    let mut child = command
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("cannot run {label}: {error}"))?;
    let mut captured = String::new();
    if let Some(mut stderr) = child.stderr.take() {
        use std::io::Read as _;
        let _ = stderr.read_to_string(&mut captured);
    }
    let status = child
        .wait()
        .map_err(|error| format!("cannot wait for {label}: {error}"))?;
    // Echoed either way: a warning on a successful apply is still worth seeing,
    // and a failure has to stay visible in the console it was streaming to.
    if !captured.is_empty() {
        eprint!("{captured}");
    }
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "{label} exited with {status}{}",
            error_tail(&captured)
        ))
    }
}

/// The last few meaningful lines of a captured stream, for a one-line error.
///
/// Terraform prints the failing resource and the provider's own message
/// together, and they are the last thing on the stream. Trimming to a tail
/// keeps the failure readable when several resources fail at once.
fn error_tail(captured: &str) -> String {
    let lines = captured
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    if lines.is_empty() {
        return String::new();
    }
    let start = lines.len().saturating_sub(ERROR_TAIL_LINES);
    let mut out = String::from(":");
    if start > 0 {
        out.push_str(&format!(" [{} earlier line(s) omitted]", start));
    }
    for line in &lines[start..] {
        out.push('\n');
        out.push_str(line);
    }
    out
}

/// Enough for a Terraform error block ( the `Error:` line, the resource, and
/// the provider's explanation ) without pasting a whole apply log into one
/// assertion message.
const ERROR_TAIL_LINES: usize = 24;

fn checked_status(command: &mut Command, label: &str) -> Result<()> {
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
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| format!("cannot run {label}: {error}"))?;
    output_text(output, label)
}

fn output_text(output: Output, label: &str) -> Result<String> {
    if !output.status.success() {
        return Err(format!(
            "{label} exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8(output.stdout).map_err(|error| format!("{label} wrote non-UTF-8: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::aws::{archive_env, MATERIALIZERS, RUNNER_ENV};

    fn run() -> Run {
        Run {
            workspace: PathBuf::from("/workspace"),
            region: "ap-northeast-1".to_owned(),
            run_id: "yn-1756800000-4242".to_owned(),
            architecture: "arm64".to_owned(),
            instance_type: None,
            state_dir: PathBuf::from("/state"),
            data_dir: PathBuf::from("/state/terraform"),
            state_file: PathBuf::from("/state/terraform.tfstate"),
            var_file: PathBuf::from("/state/gate.tfvars"),
            keep: false,
            eks: true,
            only: String::new(),
            // Never true in a unit test: `Drop` would run `terraform destroy`.
            applied: false,
        }
    }

    /// The variables file is HCL, and `eks` is the one value in it that is not
    /// a string.
    ///
    /// Worth its own test because the failure is silent here and loud there:
    /// `eks = "true"` is a perfectly good line that Terraform rejects as the
    /// wrong type, minutes into a billable run, and the wrapper's signal
    /// backstop destroys through this same file.
    #[test]
    fn the_variables_file_writes_the_arm_switch_as_a_bool() {
        let dir = tempfile::tempdir().expect("temporary directory");
        for (eks, expected) in [
            (true, "eks           = true"),
            (false, "eks           = false"),
        ] {
            let mut run = run();
            run.var_file = dir.path().join(format!("gate-{eks}.tfvars"));
            run.eks = eks;
            run.write_var_file().expect("write the variables file");
            let body = std::fs::read_to_string(&run.var_file).expect("read it back");
            assert!(body.lines().any(|line| line == expected), "{body}");
            // Every other value is a quoted string, and the architecture is the
            // one the wrapper's destroy has to agree with.
            assert!(body.contains("architecture  = \"arm64\""), "{body}");
        }
    }

    /// The same check for both deferred arms. `deferred_env` cannot be
    /// exercised here -- it shells out to `terraform output` -- but the pairing
    /// tables can, and the pairing tables are the part that drifts.
    #[test]
    fn each_deferred_arm_covers_exactly_what_the_archiver_requires() {
        for materializer in MATERIALIZERS {
            let arm = match materializer {
                "ecs" => DEFERRED_ENV_ECS.as_slice(),
                "eks" => DEFERRED_ENV_EKS.as_slice(),
                other => panic!("{other} has no pairing table"),
            };
            let names: Vec<&str> = DEFERRED_ENV_COMMON
                .iter()
                .chain(arm)
                .map(|(name, _)| *name)
                .collect();
            assert_eq!(names, archive_env(materializer).expect("a known arm"));
        }
        assert!(deferred_env(&run(), "gce").is_err());
    }

    /// Exactly one value in each table is the scenario's own choice rather than
    /// a fact about the stack. If a second one appeared, something Terraform
    /// owns would be being restated in Rust.
    #[test]
    fn only_the_materializer_choice_is_not_a_terraform_output() {
        for arm in [DEFERRED_ENV_ECS.as_slice(), DEFERRED_ENV_EKS.as_slice()] {
            let pairs: Vec<_> = DEFERRED_ENV_COMMON.iter().chain(arm).collect();
            let unsourced: Vec<&str> = pairs
                .iter()
                .filter(|(_, output)| output.is_none())
                .map(|(name, _)| *name)
                .collect();
            assert_eq!(unsourced, [ENV_MATERIALIZER]);

            // Two variables reading one output would mean the stack had
            // stopped distinguishing two things the archiver distinguishes --
            // the path the worker reads and the path both processes share are
            // never the same directory, and `yesno-archive` refuses them equal.
            let mut outputs: Vec<&str> = pairs.iter().filter_map(|(_, output)| *output).collect();
            outputs.sort_unstable();
            let count = outputs.len();
            outputs.dedup();
            assert_eq!(outputs.len(), count, "an output is claimed twice");
        }
    }

    /// The check that keeps the two machines agreeing about what the runner is
    /// handed. The scenario iterates this dict rather than naming variables,
    /// so a name added on the runner side arrives here or fails loudly.
    #[test]
    fn the_runner_environment_covers_exactly_what_the_runner_requires() {
        let env = runner_env(
            &run(),
            "i-0123456789abcdef0",
            "vol-0fedcba9876543210",
            "ap-northeast-1a",
            "/mnt/yesno-source:/mnt/yesno-snapshots",
        )
        .expect("well-formed mounts");
        let names: Vec<&str> = env.iter().map(|(name, _)| *name).collect();
        assert_eq!(names, RUNNER_ENV);
        assert!(env.iter().all(|(_, value)| !value.is_empty()), "{env:?}");
    }

    #[test]
    fn the_runner_environment_refuses_mounts_it_cannot_bind() {
        let cases = ["/mnt/source", "mnt/source:/mnt/snapshots", ":"];
        for mounts in cases {
            assert!(
                runner_env(&run(), "i-1", "vol-1", "ap-northeast-1a", mounts).is_err(),
                "{mounts} must be refused"
            );
        }
    }

    /// `cloud_terraform` is a verb, not a shell.
    #[test]
    fn only_the_gates_own_terraform_actions_are_accepted() {
        for action in ACTIONS {
            assert!(check_action(action).is_ok(), "{action} must be accepted");
        }
        for action in ["plan", "import", "state rm", "apply; rm -rf /", ""] {
            assert!(check_action(action).is_err(), "{action} must be refused");
        }
    }

    /// `apply` and `destroy` must carry this run's state; `init` must not, or
    /// it would be reading a state file that does not exist yet.
    fn argv(run: &Run, action: &str) -> Vec<String> {
        run.terraform_command(action)
            .unwrap()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn only_the_stateful_actions_carry_state_and_variables() {
        for action in STATEFUL {
            assert!(ACTIONS.contains(&action), "{action}");
        }
        assert!(!STATEFUL.contains(&"init"));
        assert!(!STATEFUL.contains(&"validate"));

        // The constants above say which actions are stateful; this says the
        // argv agrees with them. Nothing checked that before.
        let run = run();
        for action in ACTIONS {
            let args = argv(&run, action);
            let stateful = args.iter().any(|arg| arg.starts_with("-state="));
            assert_eq!(stateful, STATEFUL.contains(&action), "{action}: {args:?}");
            assert_eq!(
                args.iter().any(|arg| arg.starts_with("-var-file=")),
                STATEFUL.contains(&action),
                "{action}: {args:?}"
            );
        }
    }

    /// Every action, not just the noisy ones. Terraform decides on its own
    /// whether to emit colour, and this gate's log shows it deciding wrongly;
    /// worse, `checked_reporting_stderr` folds Terraform's stderr into the
    /// error a scenario raises, so escape sequences would land inside an
    /// assertion message.
    #[test]
    fn every_terraform_invocation_disables_colour() {
        let run = run();
        for action in ACTIONS {
            let args = argv(&run, action);
            assert!(
                args.iter().any(|arg| arg == "-no-color"),
                "terraform {action} was built without -no-color: {args:?}"
            );
        }
    }

    #[test]
    fn a_terminal_status_is_only_one_systems_manager_will_not_change() {
        assert!(is_terminal("Success"));
        assert!(is_terminal("Failed"));
        assert!(is_terminal("TimedOut"));
        assert!(!is_terminal("Pending"));
        assert!(!is_terminal("InProgress"));
    }

    #[test]
    fn the_ssm_parameter_document_escapes_the_script() {
        assert_eq!(
            json_string("echo \"a\\b\"\nexit 0\n"),
            r#""echo \"a\\b\"\nexit 0\n""#
        );
    }

    /// The tail is the diagnosis. A `terraform apply` failure names the
    /// resource and the provider's reason on its last lines, and those are what
    /// has to reach the scenario failure rather than only the exit status.
    #[test]
    fn an_error_tail_keeps_the_last_lines_and_says_what_it_dropped() {
        let captured = (1..=40)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let tail = error_tail(&captured);
        assert!(tail.contains("line 40"), "{tail}");
        assert!(tail.contains("line 17"), "{tail}");
        assert!(!tail.contains("line 16"), "{tail}");
        // Never silently: a tail that hid what it cut would read as the
        // whole error.
        assert!(tail.contains("[16 earlier line(s) omitted]"), "{tail}");
    }

    #[test]
    fn a_short_error_is_reported_whole_and_unannotated() {
        let tail = error_tail("Error: creating EKS Add-On\n\n  unexpected state 'DEGRADED'\n");
        assert!(tail.contains("Error: creating EKS Add-On"), "{tail}");
        assert!(tail.contains("unexpected state 'DEGRADED'"), "{tail}");
        assert!(!tail.contains("omitted"), "{tail}");
    }

    #[test]
    fn a_silent_failure_adds_nothing_to_the_exit_status() {
        assert_eq!(error_tail(""), "");
        assert_eq!(error_tail("\n  \n"), "");
    }

    #[test]
    fn a_scenario_run_is_named_by_its_scenario() {
        let script = "set -eu\nimage=example\nscenario=e2e/aws/ebs.py\ntimeout=1200\ndocker pull\n";
        assert_eq!(script_label(script), "e2e/aws/ebs.py");
    }

    /// Provisioning and the Kubernetes bootstrap carry no `scenario=` line.
    /// They must still be named, or the silence they were added to explain
    /// comes back for exactly the steps that run before any scenario does.
    #[test]
    fn a_script_with_no_scenario_still_gets_a_name() {
        assert_eq!(
            script_label("set -eu\nregion=ap-northeast-1\n"),
            "a runner script"
        );
        assert_eq!(script_label(""), "a runner script");
        assert_eq!(script_label("set -eu\nscenario=\n"), "a runner script");
    }

    #[test]
    fn every_arm_name_is_accepted_and_nothing_else_is() {
        assert_eq!(check_only(String::new(), true), Ok(String::new()));
        for arm in ARMS {
            assert_eq!(check_only(arm.to_owned(), true), Ok(arm.to_owned()));
        }
        // A typo must stop the run. If an unknown value merely matched no
        // arm, the gate would go green having executed none of them, which is
        // the one result it must never produce.
        assert!(check_only("kes".to_owned(), true).is_err());
        assert!(check_only("EKS".to_owned(), true).is_err());
        assert!(check_only("local,ecs".to_owned(), true).is_err());
    }

    /// "Run only the EKS arm" and "do not build the EKS arm" is a pair that
    /// would provision a cluster and run nothing on it, then print a pass.
    #[test]
    fn asking_for_only_the_arm_that_is_switched_off_is_refused() {
        assert!(check_only("eks".to_owned(), false).is_err());
        // And the operator arm on the same terms: it drives a YesnoCluster
        // *inside* that cluster, so without one there is nothing to drive.
        assert!(check_only("operator".to_owned(), false).is_err());
        assert_eq!(
            check_only("operator".to_owned(), true),
            Ok("operator".to_owned())
        );
        // The other two do not need it, so turning it off is not a conflict.
        assert_eq!(
            check_only("local".to_owned(), false),
            Ok("local".to_owned())
        );
        assert_eq!(check_only("ecs".to_owned(), false), Ok("ecs".to_owned()));
        assert_eq!(check_only(String::new(), false), Ok(String::new()));
    }

    /// Every name the operator arm's far side requires is filled here.
    ///
    /// The list lives in `operator.rs` because that is where a missing one
    /// fails -- several minutes into a billable run, inside a container, on an
    /// EC2 instance. This asserts the mapping is total and that the one value
    /// which is not a Terraform output is still spoken for.
    #[test]
    fn the_operator_arm_is_given_every_variable_it_reads() {
        assert!(!OPERATOR_ENV.is_empty());
        let mut names: Vec<&str> = OPERATOR_ENV.iter().map(|(name, _)| *name).collect();
        names.sort_unstable();
        let count = names.len();
        names.dedup();
        assert_eq!(names.len(), count, "a variable is filled twice");
        for (name, output) in OPERATOR_ENV {
            assert!(
                name.starts_with("YESNO_OPERATOR_"),
                "{name} is not one of this arm's variables"
            );
            assert!(!output.is_empty(), "{name} names no source");
        }
        // Must not collide with the runner's own contract: a name in both
        // would have two sources and no owner, and `gate.py` asserts the same
        // thing about the values at run time.
        for (name, _) in OPERATOR_ENV {
            assert!(
                !crate::aws::RUNNER_ENV.contains(name),
                "{name} is claimed by both contracts"
            );
        }
    }

    /// Every output this arm reads is one the stack declares.
    ///
    /// A renamed output is otherwise a runtime failure that costs a cluster
    /// to discover: `cloud_output()` refuses an empty value, so the arm aborts
    /// after the fifteen minutes EKS takes to come up. Reading the file is
    /// crude and catches exactly that.
    #[test]
    fn every_operator_output_is_declared_by_the_stack() {
        let outputs = include_str!("../../e2e/aws/outputs.tf");
        for (name, output) in OPERATOR_ENV {
            // The one value the stack is given rather than reports.
            if *output == "region" {
                continue;
            }
            assert!(
                outputs.contains(&format!("output \"{output}\"")),
                "{name} reads `{output}`, which e2e/aws/outputs.tf does not declare"
            );
        }
    }

    /// This had no `#[test]` and had therefore **never run**, from the day
    /// it was written until 2026-09-06. It compiled, it read as coverage, and
    /// `cargo test` never called it. Found by clippy's `dead_code`, which only
    /// reaches this crate with `--workspace`.
    #[test]
    fn terraform_variables_are_quoted() {
        assert_eq!(tf_string("ap-northeast-1"), "\"ap-northeast-1\"");
        assert_eq!(tf_string("a\"b"), "\"a\\\"b\"");
    }

    #[test]
    fn a_run_id_terraform_would_reject_is_rejected_first() {
        assert!(check_run_id("yn-1756800000-4242").is_ok());
        assert!(check_run_id("Yn-1").is_err(), "uppercase");
        assert!(check_run_id("-yn").is_err(), "leading hyphen");
        assert!(check_run_id("y").is_err(), "too short");
        assert!(
            check_run_id(&"y".repeat(RUN_ID_MAX + 1)).is_err(),
            "too long"
        );
    }

    #[test]
    fn only_the_two_architectures_with_an_ami_are_accepted() {
        assert_eq!(normalize_architecture("aarch64\n").unwrap(), "arm64");
        assert_eq!(normalize_architecture("amd64").unwrap(), "x86_64");
        assert!(normalize_architecture("riscv64").is_err());
    }
}
