//! Generic external-process fixtures for ordinary E2E scenarios.
//!
//! These verbs know nothing about PostgreSQL or MySQL. Bazel declares immutable
//! resources through `YESNO_E2E_RESOURCE_*`; a checked scenario composes private
//! paths, file preparation, commands, managed servers, readiness probes, and
//! interactive sessions. Monty still has no direct filesystem or process access.

use arrow_flight::flight_service_server::FlightServiceServer;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::FromRawFd;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Output, Stdio};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use monty_types::{ExcType, MontyException, MontyObject};
use yesno_core::Db;
use yesno_flight::YesnoFlightService;

use crate::convert::{dict, value_err, Args};
use crate::world::{HandleKind, World};

pub const OWNS: &[&str] = &[
    "fx_flight_start",
    "fx_flight_stop",
    "fx_resource",
    "fx_temp",
    "fx_join",
    "fx_mkdir",
    "fx_copy",
    "fx_copy_tree",
    "fx_write",
    "fx_read",
    "fx_list",
    "fx_run",
    "fx_run_merged",
    "fx_start",
    "fx_start_tty",
    "fx_wait_ready",
    "fx_send",
    "fx_wait_output",
    "fx_close",
    "fx_stop",
    "fx_alive",
];

#[derive(Default)]
pub struct FixtureState {
    flight: Option<FlightFixture>,
    processes: Vec<Option<ManagedProcess>>,
}

struct ManagedProcess {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: PathBuf,
    stderr: PathBuf,
    emitted: usize,
    reader: Option<thread::JoinHandle<()>>,
}

impl FixtureState {
    pub(crate) fn stop_all(&mut self) {
        drop(self.flight.take());
        for process in self.processes.iter_mut().flatten() {
            drop(process.stdin.take());
            let _ = process.child.kill();
            let _ = process.child.wait();
            join_reader(process);
        }
        self.processes.clear();
    }
}

impl Drop for FixtureState {
    fn drop(&mut self) {
        self.stop_all();
    }
}

impl World {
    pub(crate) fn call_fixture(
        &mut self,
        verb: &str,
        a: &Args<'_>,
    ) -> Result<MontyObject, MontyException> {
        match verb {
            "fx_flight_start" => {
                a.exact(2)?;
                a.no_kwargs()?;
                if self.fixture.flight.is_some() {
                    return Err(value_err(format!(
                        "{verb}(): fixture Flight server is already running"
                    )));
                }
                let keys = a.u64_list(0)?;
                let ordinals = a.u64_list(1)?;
                if keys.len() != ordinals.len() {
                    return Err(value_err(format!(
                        "{verb}(): key and ordinal lists differ in length"
                    )));
                }
                let (flight, location) = FlightFixture::start(keys, ordinals)
                    .map_err(|error| fixture_err(verb, error))?;
                self.fixture.flight = Some(flight);
                Ok(MontyObject::String(location))
            }
            "fx_flight_stop" => {
                a.exact(0)?;
                a.no_kwargs()?;
                let flight = self.fixture.flight.take().ok_or_else(|| {
                    value_err(format!("{verb}(): fixture Flight server is not running"))
                })?;
                drop(flight);
                Ok(MontyObject::None)
            }
            "fx_resource" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let logical = a.str_at(0)?;
                if logical.is_empty()
                    || !logical.bytes().all(|byte| {
                        byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_'
                    })
                {
                    return Err(value_err(format!(
                        "{verb}(): resource name must contain only A-Z, 0-9 and _"
                    )));
                }
                let name = format!("YESNO_E2E_RESOURCE_{logical}");
                let path = std::env::var_os(&name).map(PathBuf::from).ok_or_else(|| {
                    fixture_err(verb, format!("resource {logical} is not declared"))
                })?;
                if !path.exists() {
                    return Err(fixture_err(
                        verb,
                        format!(
                            "declared resource {logical} does not exist: {}",
                            path.display()
                        ),
                    ));
                }
                Ok(path_obj(path))
            }
            "fx_temp" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let relative = checked_relative(verb, a.str_at(0)?)?;
                let path = self.root.join(relative);
                fs::create_dir_all(&path).map_err(|error| {
                    fixture_err(verb, format!("cannot create {}: {error}", path.display()))
                })?;
                Ok(path_obj(path))
            }
            "fx_join" => {
                a.exact(2)?;
                a.no_kwargs()?;
                let relative = checked_relative(verb, a.str_at(1)?)?;
                Ok(path_obj(Path::new(a.str_at(0)?).join(relative)))
            }
            "fx_mkdir" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let path = Path::new(a.str_at(0)?);
                fs::create_dir_all(path).map_err(|error| {
                    fixture_err(verb, format!("cannot create {}: {error}", path.display()))
                })?;
                Ok(MontyObject::None)
            }
            "fx_copy" => {
                a.exact(3)?;
                a.no_kwargs()?;
                copy_file(
                    Path::new(a.str_at(0)?),
                    Path::new(a.str_at(1)?),
                    u32::try_from(a.u64(2)?)
                        .map_err(|_| value_err(format!("{verb}(): mode does not fit in a u32")))?,
                )
                .map_err(|error| fixture_err(verb, error))?;
                Ok(MontyObject::None)
            }
            "fx_copy_tree" => {
                a.exact(2)?;
                a.no_kwargs()?;
                copy_tree(Path::new(a.str_at(0)?), Path::new(a.str_at(1)?))
                    .map_err(|error| fixture_err(verb, error))?;
                Ok(MontyObject::None)
            }
            "fx_write" => {
                a.exact(2)?;
                a.no_kwargs()?;
                let path = Path::new(a.str_at(0)?);
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).map_err(|error| {
                        fixture_err(verb, format!("cannot create {}: {error}", parent.display()))
                    })?;
                }
                fs::write(path, a.str_at(1)?).map_err(|error| {
                    fixture_err(verb, format!("cannot write {}: {error}", path.display()))
                })?;
                Ok(MontyObject::None)
            }
            "fx_read" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let path = Path::new(a.str_at(0)?);
                let value = fs::read_to_string(path).map_err(|error| {
                    fixture_err(verb, format!("cannot read {}: {error}", path.display()))
                })?;
                Ok(MontyObject::String(value))
            }
            "fx_list" => {
                a.exact(2)?;
                a.no_kwargs()?;
                let directory = Path::new(a.str_at(0)?);
                let suffix = a.str_at(1)?;
                let mut paths = fs::read_dir(directory)
                    .map_err(|error| {
                        fixture_err(
                            verb,
                            format!("cannot read {}: {error}", directory.display()),
                        )
                    })?
                    .map(|entry| {
                        entry.map(|entry| entry.path()).map_err(|error| {
                            fixture_err(verb, format!("cannot read directory entry: {error}"))
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                paths.retain(|path| {
                    path.file_name()
                        .is_some_and(|name| name.to_string_lossy().ends_with(suffix))
                });
                paths.sort();
                Ok(MontyObject::List(paths.into_iter().map(path_obj).collect()))
            }
            "fx_run" | "fx_run_merged" => {
                a.exact(4)?;
                a.no_kwargs()?;
                if verb == "fx_run_merged" {
                    let (status, transcript) = run_command_merged(
                        Path::new(a.str_at(0)?),
                        &a.string_list(1)?,
                        &a.string_list(2)?,
                        a.str_at(3)?,
                        &self.root,
                    )
                    .map_err(|error| fixture_err(verb, error))?;
                    return Ok(status_dict(
                        status,
                        MontyObject::String(transcript),
                        MontyObject::String(String::new()),
                    ));
                }
                let output = run_command(
                    Path::new(a.str_at(0)?),
                    &a.string_list(1)?,
                    &a.string_list(2)?,
                    a.str_at(3)?,
                    &self.root,
                )
                .map_err(|error| fixture_err(verb, error))?;
                output_obj(output, verb)
            }
            "fx_start" | "fx_start_tty" => {
                a.exact(3)?;
                a.no_kwargs()?;
                let index = self.fixture.processes.len();
                let stdout = self.root.join(format!("process-{index}.stdout"));
                let stderr = self.root.join(format!("process-{index}.stderr"));
                let stdout_file =
                    File::create(&stdout).map_err(|error| fixture_err(verb, error.to_string()))?;
                let stderr_file =
                    File::create(&stderr).map_err(|error| fixture_err(verb, error.to_string()))?;
                let mut command = prepared_command(
                    Path::new(a.str_at(0)?),
                    &a.string_list(1)?,
                    &a.string_list(2)?,
                    &self.root,
                )?;
                let mut reader = None;
                if verb == "fx_start_tty" {
                    let (master, slave) =
                        open_pty().map_err(|error| fixture_err(verb, error.to_string()))?;
                    let errors = slave
                        .try_clone()
                        .map_err(|error| fixture_err(verb, error.to_string()))?;
                    command
                        .stdout(Stdio::from(slave))
                        .stderr(Stdio::from(errors));
                    reader = Some(
                        thread::Builder::new()
                            .name(format!("yesno-e2e-process-{index}-pty"))
                            .spawn(move || copy_pty_transcript(master, stdout_file))
                            .map_err(|error| fixture_err(verb, error.to_string()))?,
                    );
                } else {
                    command
                        .stdout(Stdio::from(stdout_file))
                        .stderr(Stdio::from(stderr_file));
                }
                let mut child = command.stdin(Stdio::piped()).spawn().map_err(|error| {
                    fixture_err(
                        verb,
                        format!("cannot start {:?}: {error}", command.get_program()),
                    )
                })?;
                let stdin = child.stdin.take();
                self.fixture.processes.push(Some(ManagedProcess {
                    child,
                    stdin,
                    stdout,
                    stderr,
                    emitted: 0,
                    reader,
                }));
                Ok(self.mint(HandleKind::FixtureProcess, index))
            }
            "fx_wait_ready" => {
                a.exact(5)?;
                a.no_kwargs()?;
                let slot = self.slot(a.handle(0)?, HandleKind::FixtureProcess, verb)?;
                let program = PathBuf::from(a.str_at(1)?);
                let args = a.string_list(2)?;
                let env = a.string_list(3)?;
                let timeout = Duration::from_millis(a.u64(4)?);
                let process = self.fixture.processes[slot]
                    .as_mut()
                    .ok_or_else(|| value_err(format!("{verb}(): process is closed")))?;
                wait_ready(process, &program, &args, &env, timeout, &self.root)
                    .map_err(|error| fixture_err(verb, error))?;
                Ok(MontyObject::None)
            }
            "fx_send" => {
                a.exact(2)?;
                a.no_kwargs()?;
                let process = self.fixture_process_mut(a.handle(0)?, verb)?;
                let stdin = process
                    .stdin
                    .as_mut()
                    .ok_or_else(|| value_err(format!("{verb}(): process stdin is closed")))?;
                stdin
                    .write_all(a.str_at(1)?.as_bytes())
                    .and_then(|()| stdin.flush())
                    .map_err(|error| {
                        fixture_err(verb, format!("cannot write process stdin: {error}"))
                    })?;
                Ok(MontyObject::None)
            }
            "fx_wait_output" => {
                a.exact(3)?;
                a.no_kwargs()?;
                let marker = a.str_at(1)?.to_owned();
                let timeout = Duration::from_millis(a.u64(2)?);
                let process = self.fixture_process_mut(a.handle(0)?, verb)?;
                Ok(MontyObject::String(
                    wait_output(process, &marker, timeout)
                        .map_err(|error| fixture_err(verb, error))?,
                ))
            }
            "fx_close" => {
                a.exact(2)?;
                a.no_kwargs()?;
                let slot = self.slot(a.handle(0)?, HandleKind::FixtureProcess, verb)?;
                let mut process = self.fixture.processes[slot]
                    .take()
                    .ok_or_else(|| value_err(format!("{verb}(): process is already closed")))?;
                drop(process.stdin.take());
                let status = wait_child(&mut process.child, Duration::from_millis(a.u64(1)?))
                    .map_err(|error| fixture_err(verb, error))?;
                join_reader(&mut process);
                process_output_obj(status, &process.stdout, &process.stderr)
            }
            "fx_stop" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let slot = self.slot(a.handle(0)?, HandleKind::FixtureProcess, verb)?;
                let mut process = self.fixture.processes[slot]
                    .take()
                    .ok_or_else(|| value_err(format!("{verb}(): process is already closed")))?;
                drop(process.stdin.take());
                let _ = process.child.kill();
                let status = process
                    .child
                    .wait()
                    .map_err(|error| fixture_err(verb, error.to_string()))?;
                join_reader(&mut process);
                process_output_obj(status, &process.stdout, &process.stderr)
            }
            "fx_alive" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let process = self.fixture_process_mut(a.handle(0)?, verb)?;
                let alive = process
                    .child
                    .try_wait()
                    .map_err(|error| fixture_err(verb, error.to_string()))?
                    .is_none();
                Ok(MontyObject::Bool(alive))
            }
            _ => Err(value_err(format!("{verb}() is not a fixture verb"))),
        }
    }

    fn fixture_process_mut(
        &mut self,
        handle: usize,
        verb: &str,
    ) -> Result<&mut ManagedProcess, MontyException> {
        let slot = self.slot(handle, HandleKind::FixtureProcess, verb)?;
        self.fixture.processes[slot]
            .as_mut()
            .ok_or_else(|| value_err(format!("{verb}(): process is closed")))
    }
}

fn checked_relative(verb: &str, value: &str) -> Result<PathBuf, MontyException> {
    let path = Path::new(value);
    if value.is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(value_err(format!(
            "{verb}(): expected a non-empty relative path without . or .."
        )));
    }
    Ok(path.to_owned())
}

fn prepared_command(
    program: &Path,
    args: &[String],
    env: &[String],
    home: &Path,
) -> Result<Command, MontyException> {
    let mut command = Command::new(program);
    command
        .args(args)
        .env_clear()
        .env("HOME", home)
        .env("LC_ALL", "C")
        .env("LANG", "C");
    for assignment in env {
        let (name, value) = assignment.split_once('=').ok_or_else(|| {
            value_err(format!(
                "environment entry must be NAME=VALUE: {assignment}"
            ))
        })?;
        if name.is_empty() || name.contains('\0') || value.contains('\0') {
            return Err(value_err(format!(
                "invalid environment entry: {assignment:?}"
            )));
        }
        command.env(name, value);
    }
    Ok(command)
}

fn run_command(
    program: &Path,
    args: &[String],
    env: &[String],
    stdin: &str,
    home: &Path,
) -> Result<Output, String> {
    let mut command =
        prepared_command(program, args, env, home).map_err(|error| error.to_string())?;
    if stdin.is_empty() {
        return command
            .output()
            .map_err(|error| format!("cannot execute {}: {error}", program.display()));
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("cannot execute {}: {error}", program.display()))?;
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(stdin.as_bytes())
        .map_err(|error| format!("cannot write {} stdin: {error}", program.display()))?;
    child
        .wait_with_output()
        .map_err(|error| format!("cannot wait for {}: {error}", program.display()))
}

fn wait_ready(
    process: &mut ManagedProcess,
    program: &Path,
    args: &[String],
    env: &[String],
    timeout: Duration,
    home: &Path,
) -> Result<(), String> {
    let started = Instant::now();
    loop {
        if process
            .child
            .try_wait()
            .map_err(|error| format!("cannot inspect server: {error}"))?
            .is_some()
        {
            return Err(format!(
                "server exited before readiness:\n{}",
                read_pair(process)
            ));
        }
        if run_command(program, args, env, "", home).is_ok_and(|output| output.status.success()) {
            return Ok(());
        }
        if started.elapsed() >= timeout {
            return Err(format!(
                "server was not ready within {timeout:?}:\n{}",
                read_pair(process)
            ));
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn wait_output(
    process: &mut ManagedProcess,
    marker: &str,
    timeout: Duration,
) -> Result<String, String> {
    let started = Instant::now();
    loop {
        let bytes = fs::read(&process.stdout)
            .map_err(|error| format!("cannot read process output: {error}"))?;
        let text = String::from_utf8_lossy(&bytes);
        if let Some(relative) = text[process.emitted..].find(marker) {
            let at = process.emitted + relative;
            let chunk = text[process.emitted..at].to_owned();
            process.emitted = at + marker.len();
            return Ok(chunk);
        }
        if process
            .child
            .try_wait()
            .map_err(|error| format!("cannot inspect process: {error}"))?
            .is_some()
        {
            return Err(format!(
                "process exited before marker {marker:?}:\n{}",
                read_pair(process)
            ));
        }
        if started.elapsed() >= timeout {
            return Err(format!(
                "timed out waiting for marker {marker:?}:\n{}",
                read_pair(process)
            ));
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn wait_child(child: &mut Child, timeout: Duration) -> Result<std::process::ExitStatus, String> {
    let started = Instant::now();
    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("cannot inspect process: {error}"))?
        {
            return Ok(status);
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            return child
                .wait()
                .map_err(|error| format!("cannot reap timed-out process: {error}"));
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn copy_tree(source: &Path, destination: &Path) -> Result<(), String> {
    let metadata = fs::metadata(source)
        .map_err(|error| format!("cannot inspect {}: {error}", source.display()))?;
    if metadata.is_dir() {
        fs::create_dir_all(destination)
            .map_err(|error| format!("cannot create {}: {error}", destination.display()))?;
        let mut permissions = metadata.permissions();
        permissions.set_mode(permissions.mode() | 0o700);
        fs::set_permissions(destination, permissions)
            .map_err(|error| format!("cannot make {} writable: {error}", destination.display()))?;
        for entry in fs::read_dir(source)
            .map_err(|error| format!("cannot read {}: {error}", source.display()))?
        {
            let entry =
                entry.map_err(|error| format!("cannot read {}: {error}", source.display()))?;
            copy_tree(&entry.path(), &destination.join(entry.file_name()))?;
        }
        Ok(())
    } else if metadata.is_file() {
        copy_file(source, destination, metadata.permissions().mode() | 0o200)
    } else {
        Err(format!("unsupported file type: {}", source.display()))
    }
}

fn copy_file(source: &Path, destination: &Path, mode: u32) -> Result<(), String> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    }
    fs::copy(source, destination).map_err(|error| {
        format!(
            "cannot copy {} to {}: {error}",
            source.display(),
            destination.display()
        )
    })?;
    fs::set_permissions(destination, fs::Permissions::from_mode(mode))
        .map_err(|error| format!("cannot set mode on {}: {error}", destination.display()))
}

fn output_obj(output: Output, verb: &str) -> Result<MontyObject, MontyException> {
    let stdout = String::from_utf8(output.stdout)
        .map_err(|error| fixture_err(verb, format!("stdout is not UTF-8: {error}")))?;
    let stderr = String::from_utf8(output.stderr)
        .map_err(|error| fixture_err(verb, format!("stderr is not UTF-8: {error}")))?;
    Ok(status_dict(
        output.status,
        MontyObject::String(stdout),
        MontyObject::String(stderr),
    ))
}

fn process_output_obj(
    status: std::process::ExitStatus,
    stdout: &Path,
    stderr: &Path,
) -> Result<MontyObject, MontyException> {
    Ok(status_dict(
        status,
        MontyObject::String(fs::read_to_string(stdout).unwrap_or_default()),
        MontyObject::String(fs::read_to_string(stderr).unwrap_or_default()),
    ))
}

fn status_dict(
    status: std::process::ExitStatus,
    stdout: MontyObject,
    stderr: MontyObject,
) -> MontyObject {
    dict(vec![
        ("success", MontyObject::Bool(status.success())),
        (
            "code",
            status
                .code()
                .map_or(MontyObject::None, |code| MontyObject::Int(i64::from(code))),
        ),
        ("stdout", stdout),
        ("stderr", stderr),
    ])
}

fn read_pair(process: &ManagedProcess) -> String {
    format!(
        "stdout:\n{}\nstderr:\n{}",
        fs::read_to_string(&process.stdout).unwrap_or_default(),
        fs::read_to_string(&process.stderr).unwrap_or_default()
    )
}

fn path_obj(path: PathBuf) -> MontyObject {
    MontyObject::String(path.to_string_lossy().into_owned())
}

fn fixture_err(verb: &str, message: impl Into<String>) -> MontyException {
    MontyException::new(
        ExcType::RuntimeError,
        Some(format!("{verb}(): {}", message.into())),
    )
}

struct FlightFixture {
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<thread::JoinHandle<Result<(), String>>>,
}

impl FlightFixture {
    fn start(keys: Vec<u64>, ordinals: Vec<u64>) -> Result<(Self, String), String> {
        let (shutdown, shutdown_rx) = tokio::sync::oneshot::channel();
        let (ready, ready_rx) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("yesno-e2e-flight-fixture".to_owned())
            .spawn(move || {
                let db = Db::new();
                for (key, ordinal) in keys.into_iter().zip(ordinals) {
                    db.insert(key, ordinal)
                        .map_err(|error| format!("cannot seed key {key}: {error}"))?;
                }
                let service = YesnoFlightService::new(Arc::new(db));
                let runtime = tokio::runtime::Runtime::new()
                    .map_err(|error| format!("cannot create Flight runtime: {error}"))?;
                runtime.block_on(async move {
                    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                        .await
                        .map_err(|error| format!("cannot bind Flight fixture: {error}"))?;
                    let address = listener
                        .local_addr()
                        .map_err(|error| format!("cannot read Flight address: {error}"))?;
                    ready
                        .send(Ok(address))
                        .map_err(|_| "scenario stopped before Flight startup".to_owned())?;
                    tonic::transport::Server::builder()
                        .add_service(FlightServiceServer::new(service))
                        .serve_with_incoming_shutdown(
                            tokio_stream::wrappers::TcpListenerStream::new(listener),
                            async {
                                let _ = shutdown_rx.await;
                            },
                        )
                        .await
                        .map_err(|error| format!("Flight fixture failed: {error}"))
                })
            })
            .map_err(|error| format!("cannot spawn Flight fixture: {error}"))?;
        let fixture = Self {
            shutdown: Some(shutdown),
            thread: Some(thread),
        };
        match ready_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(address)) => Ok((fixture, format!("http://{address}"))),
            Ok(Err(error)) => {
                drop(fixture);
                Err(error)
            }
            Err(error) => {
                drop(fixture);
                Err(format!("Flight fixture did not start in time: {error}"))
            }
        }
    }
}

impl Drop for FlightFixture {
    fn drop(&mut self) {
        drop(self.shutdown.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run_command_merged(
    program: &Path,
    args: &[String],
    env: &[String],
    stdin: &str,
    home: &Path,
) -> Result<(std::process::ExitStatus, String), String> {
    let mut transcript = tempfile::tempfile_in(home)
        .map_err(|error| format!("cannot create command transcript: {error}"))?;
    let errors = transcript
        .try_clone()
        .map_err(|error| format!("cannot duplicate command transcript: {error}"))?;
    let mut command =
        prepared_command(program, args, env, home).map_err(|error| error.to_string())?;
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::from(transcript.try_clone().map_err(|error| {
            format!("cannot duplicate command transcript: {error}")
        })?))
        .stderr(Stdio::from(errors))
        .spawn()
        .map_err(|error| format!("cannot execute {}: {error}", program.display()))?;
    if !stdin.is_empty() {
        child
            .stdin
            .take()
            .expect("piped stdin")
            .write_all(stdin.as_bytes())
            .map_err(|error| format!("cannot write {} stdin: {error}", program.display()))?;
    } else {
        drop(child.stdin.take());
    }
    let status = child
        .wait()
        .map_err(|error| format!("cannot wait for {}: {error}", program.display()))?;
    transcript
        .rewind()
        .map_err(|error| format!("cannot rewind command transcript: {error}"))?;
    let mut output = String::new();
    transcript
        .read_to_string(&mut output)
        .map_err(|error| format!("command transcript is not UTF-8: {error}"))?;
    Ok((status, output))
}

fn open_pty() -> std::io::Result<(File, File)> {
    let mut master = -1;
    let mut slave = -1;
    // SAFETY: `openpty` receives valid writable pointers for both descriptors
    // and null optional name/termios/winsize pointers. On success it gives this
    // function ownership of two distinct open descriptors, each converted into
    // exactly one `File` below. On failure neither value is converted.
    let result = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if result == -1 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the successful `openpty` call above initialized both descriptors,
    // transferred their ownership to us, and neither descriptor has another
    // owning wrapper.
    Ok(unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) })
}

fn copy_pty_transcript(master: File, mut transcript: File) {
    let mut master = BufReader::new(master);
    let mut line = Vec::new();
    loop {
        line.clear();
        match master.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {
                if line.ends_with(b"\r\n") {
                    line.remove(line.len() - 2);
                }
                if transcript.write_all(&line).is_err() {
                    return;
                }
            }
        }
    }
}

fn join_reader(process: &mut ManagedProcess) {
    if let Some(reader) = process.reader.take() {
        let _ = reader.join();
    }
}

#[cfg(test)]
mod pty_properties {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn pty_pair_transfers_every_printable_line(line in "[ -~]{1,64}") {
            let (mut master, mut slave) = open_pty().unwrap();
            slave.write_all(line.as_bytes()).unwrap();
            slave.write_all(b"\n").unwrap();
            slave.flush().unwrap();
            let mut received = vec![0; line.len() + 2];
            master.read_exact(&mut received).unwrap();
            let mut expected = line.into_bytes();
            expected.extend_from_slice(b"\r\n");
            prop_assert_eq!(received, expected);
        }
    }
}
