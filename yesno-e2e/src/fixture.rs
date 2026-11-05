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
    "fx_flight_requests",
    "fx_channel_start",
    "fx_channel_stop",
    "fx_silent_start",
    "fx_silent_stop",
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
    channel: Option<ChannelFixture>,
    silent: Option<SilentFixture>,
    processes: Vec<Option<ManagedProcess>>,
    /// When the running scenario's time limit passes. See
    /// [`FixtureState::set_deadline`].
    deadline: Option<Instant>,
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
    /// Every wait on a child process in this module ends at `deadline`, and so
    /// do the `java_client` and `search_*` verbs, through [`output_by`] and
    /// [`run_to_exit`].
    ///
    /// The runner checks its limit only between verbs, so a verb waiting on a
    /// child that never exits would otherwise hold the scenario forever. At
    /// the deadline the wait kills the child and returns an error naming it,
    /// and the runner then ends the scenario with that error.
    ///
    /// Only the direct child is killed, not a process group. A child moved
    /// into its own group would no longer receive the Ctrl-C or the
    /// group-wide kill a test runner sends the harness, which is the worse
    /// leak. Output goes to files rather than pipes, so a grandchild that
    /// keeps one open cannot hold the wait either.
    pub(crate) fn set_deadline(&mut self, deadline: Instant) {
        self.deadline = Some(deadline);
    }

    /// When the running scenario's time limit passes, for a verb outside this
    /// module that waits on a child of its own.
    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    pub(crate) fn stop_all(&mut self) {
        drop(self.flight.take());
        drop(self.silent.take());
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
            "fx_channel_start" => {
                a.exact(3)?;
                a.no_kwargs()?;
                if self.fixture.channel.is_some() {
                    return Err(value_err(format!(
                        "{verb}(): fixture plugin channel is already running"
                    )));
                }
                let dir = std::path::PathBuf::from(a.str_at(0)?);
                let keys = a.u64_list(1)?;
                let ordinals = a.u64_list(2)?;
                if keys.len() != ordinals.len() {
                    return Err(value_err(format!(
                        "{verb}(): key and ordinal lists differ in length"
                    )));
                }
                let (channel, socket) = ChannelFixture::start(keys, ordinals, dir)
                    .map_err(|error| fixture_err(verb, error))?;
                self.fixture.channel = Some(channel);
                Ok(MontyObject::String(socket))
            }
            "fx_channel_stop" => {
                a.exact(0)?;
                a.no_kwargs()?;
                let channel = self.fixture.channel.take().ok_or_else(|| {
                    value_err(format!("{verb}(): fixture plugin channel is not running"))
                })?;
                drop(channel);
                Ok(MontyObject::None)
            }
            "fx_silent_start" => {
                a.exact(1)?;
                a.no_kwargs()?;
                if self.fixture.silent.is_some() {
                    return Err(value_err(format!(
                        "{verb}(): the silent listener is already running"
                    )));
                }
                let dir = std::path::PathBuf::from(a.str_at(0)?);
                let (silent, endpoint, socket) =
                    SilentFixture::start(&dir).map_err(|error| fixture_err(verb, error))?;
                self.fixture.silent = Some(silent);
                Ok(dict(vec![
                    ("endpoint", MontyObject::String(endpoint)),
                    ("socket", MontyObject::String(socket)),
                ]))
            }
            "fx_silent_stop" => {
                a.exact(0)?;
                a.no_kwargs()?;
                let silent = self.fixture.silent.take().ok_or_else(|| {
                    value_err(format!("{verb}(): the silent listener is not running"))
                })?;
                drop(silent);
                Ok(MontyObject::None)
            }
            "fx_flight_requests" => {
                a.exact(0)?;
                a.no_kwargs()?;
                let flight = self.fixture.flight.as_ref().ok_or_else(|| {
                    value_err(format!("{verb}(): fixture Flight server is not running"))
                })?;
                let (infos, gets, puts) = flight.requests.read();
                Ok(dict(vec![
                    ("flight_infos", crate::convert::int_obj(infos)),
                    ("do_gets", crate::convert::int_obj(gets)),
                    ("do_puts", crate::convert::int_obj(puts)),
                ]))
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
                        self.fixture.deadline,
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
                    self.fixture.deadline,
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
                let deadline = self.fixture.deadline;
                let process = self.fixture.processes[slot]
                    .as_mut()
                    .ok_or_else(|| value_err(format!("{verb}(): process is closed")))?;
                wait_ready(
                    process, &program, &args, &env, timeout, &self.root, deadline,
                )
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
                let timeout = within(Duration::from_millis(a.u64(2)?), self.fixture.deadline);
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
                let timeout = within(Duration::from_millis(a.u64(1)?), self.fixture.deadline);
                let status = wait_child(&mut process.child, timeout)
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
    deadline: Option<Instant>,
) -> Result<Output, String> {
    let command = prepared_command(program, args, env, home).map_err(|error| error.to_string())?;
    output_by(command, program, stdin, home, deadline)
}

/// Run `command` to exit with its output captured in files under `home`,
/// killing it at `deadline`. See [`FixtureState::set_deadline`].
pub(crate) fn output_by(
    mut command: Command,
    program: &Path,
    stdin: &str,
    home: &Path,
    deadline: Option<Instant>,
) -> Result<Output, String> {
    let file = |what: &str| {
        tempfile::tempfile_in(home)
            .map_err(|error| format!("cannot create command {what}: {error}"))
    };
    let (mut stdout, mut stderr) = (file("stdout")?, file("stderr")?);
    command
        .stdout(Stdio::from(stdout.try_clone().map_err(|error| {
            format!("cannot duplicate command stdout: {error}")
        })?))
        .stderr(Stdio::from(stderr.try_clone().map_err(|error| {
            format!("cannot duplicate command stderr: {error}")
        })?));
    let status = run_to_exit(command, program, stdin, deadline)?;
    Ok(Output {
        status,
        stdout: read_back(&mut stdout, "stdout")?,
        stderr: read_back(&mut stderr, "stderr")?,
    })
}

/// Start `command`, feed it `stdin`, and wait for it to exit, killing it at
/// `deadline`. See [`FixtureState::set_deadline`].
pub(crate) fn run_to_exit(
    mut command: Command,
    program: &Path,
    stdin: &str,
    deadline: Option<Instant>,
) -> Result<std::process::ExitStatus, String> {
    command.stdin(if stdin.is_empty() {
        Stdio::null()
    } else {
        Stdio::piped()
    });
    let mut child = command
        .spawn()
        .map_err(|error| format!("cannot execute {}: {error}", program.display()))?;
    // Written from a thread: a child that stops reading would otherwise block
    // this one in `write_all` with no deadline. Killing the child ends the
    // write with a broken pipe.
    let feeder = child.stdin.take().map(|mut pipe| {
        let input = stdin.as_bytes().to_vec();
        thread::spawn(move || pipe.write_all(&input))
    });
    let mut pause = Duration::from_millis(1);
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("cannot wait for {}: {error}", program.display()))?
        {
            break status;
        }
        if deadline.is_some_and(|at| Instant::now() >= at) {
            let _ = child.kill();
            let _ = child.wait();
            if let Some(feeder) = feeder {
                let _ = feeder.join();
            }
            return Err(format!(
                "the scenario's time limit passed while {} was still running, so it was killed",
                program.display()
            ));
        }
        thread::sleep(pause);
        pause = (pause * 2).min(Duration::from_millis(25));
    };
    if let Some(feeder) = feeder {
        feeder
            .join()
            .map_err(|_| format!("the {} stdin writer panicked", program.display()))?
            .map_err(|error| format!("cannot write {} stdin: {error}", program.display()))?;
    }
    Ok(status)
}

fn read_back(file: &mut File, what: &str) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    file.rewind()
        .and_then(|()| file.read_to_end(&mut bytes))
        .map_err(|error| format!("cannot read back command {what}: {error}"))?;
    Ok(bytes)
}

/// A harness server's connections, each with `TCP_NODELAY` set.
///
/// tonic ignores its own `tcp_nodelay` under `serve_with_incoming`, so a
/// harness server fed a bare `TcpListenerStream` kept Nagle on, and every
/// streamed response waited out the client's 40 ms delayed ACK: 42 ms for a
/// three-ordinal `flight_get`, measured 2026-10-27. The daemon's listeners do
/// the same in `yesno-server`'s `tls::serve_router`; this crate's Bazel target
/// does not link that crate, so the few lines are repeated here.
pub(crate) fn incoming_nodelay(
    listener: tokio::net::TcpListener,
) -> impl tokio_stream::Stream<Item = std::io::Result<tokio::net::TcpStream>> {
    use tokio_stream::StreamExt as _;
    tokio_stream::wrappers::TcpListenerStream::new(listener).map(|accepted| {
        let stream = accepted?;
        // A refusal leaves a slower connection, not a broken one.
        let _ = stream.set_nodelay(true);
        Ok(stream)
    })
}

/// `timeout`, shortened so it ends no later than `deadline`.
fn within(timeout: Duration, deadline: Option<Instant>) -> Duration {
    deadline.map_or(timeout, |at| {
        timeout.min(at.saturating_duration_since(Instant::now()))
    })
}

fn wait_ready(
    process: &mut ManagedProcess,
    program: &Path,
    args: &[String],
    env: &[String],
    timeout: Duration,
    home: &Path,
    deadline: Option<Instant>,
) -> Result<(), String> {
    let timeout = within(timeout, deadline);
    let started = Instant::now();
    // A probe that hangs is killed when the readiness wait ends, not only at
    // the scenario's deadline, so it cannot outlast the timeout it serves.
    let probe_deadline = Some(started + timeout);
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
        if run_command(program, args, env, "", home, probe_deadline)
            .is_ok_and(|output| output.status.success())
        {
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
    /// Shared with the service, so a scenario can assert **how many requests** a
    /// query made rather than only what it returned. Taken before the service
    /// moves into the server task, which is the only moment it can be.
    requests: Arc<yesno_flight::RequestCounts>,
}

/// A real plugin channel over a real socket, for a scenario that drives a peer.
///
/// **Serves the channel itself rather than borrowing `yesno-server`'s**, which
/// is not a shortcut but the constraint this target is built around: the Bazel
/// `fixture_host` deliberately depends on `yesno-core` and `yesno-flight` only,
/// so that backend identity stays scenario data. Reaching for
/// `yesno_server::plugin::Channel` compiled under cargo and broke that target
/// immediately. The accept loop below needs nothing but `yesno-plugin`.
///
/// It hands over an arena, exactly as yesnod does on accept: a `memfd` whose
/// descriptor goes first with the protocol version as its single byte, then the
/// greeting. Inline mode is the fallback if the arena cannot be created, which
/// is the same decision the server makes, so a scenario exercises the zero-copy
/// path rather than a simpler one that would hide its arithmetic.
///
/// **It falls silent while a file named `mute` exists in its directory.** See
/// [`Muted`].
struct ChannelFixture {
    stop: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl ChannelFixture {
    fn limits() -> yesno_plugin::channel::Limits {
        yesno_plugin::channel::Limits {
            max_handles: 4,
            max_lanes: 8,
            // Larger than one, so a batch can come back short and a peer's
            // termination rule is exercised rather than assumed.
            max_blocks: 8,
            max_snapshots: 16,
            max_writes: yesno_plugin::ipc::MAX_WRITES,
            max_expr_bytes: yesno_plugin::ipc::MAX_EXPR_BYTES,
        }
    }

    fn start(
        keys: Vec<u64>,
        ordinals: Vec<u64>,
        dir: std::path::PathBuf,
    ) -> Result<(Self, String), String> {
        use std::os::unix::net::UnixListener;
        use std::sync::atomic::{AtomicBool, Ordering};

        let db = Db::new();
        for (key, ordinal) in keys.into_iter().zip(ordinals) {
            db.insert(key, ordinal)
                .map_err(|error| format!("cannot seed key {key}: {error}"))?;
        }
        let slot: yesno_plugin::DbSlot = Arc::new(std::sync::RwLock::new(Some(Arc::new(db))));
        let host = yesno_plugin::Host::new(slot, 1, yesno_plugin::abi::Role::Leader);

        std::fs::create_dir_all(&dir)
            .map_err(|error| format!("cannot create the channel directory: {error}"))?;
        let socket = dir.join("plugin.sock");
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket)
            .map_err(|error| format!("cannot bind the channel socket: {error}"))?;
        // Non-blocking so the accept loop can notice `stop` instead of needing a
        // self-connect to wake it.
        listener
            .set_nonblocking(true)
            .map_err(|error| format!("cannot set the channel socket non-blocking: {error}"))?;

        let mute = dir.join("mute");
        let stop = Arc::new(AtomicBool::new(false));
        let stop_loop = Arc::clone(&stop);
        let thread = thread::Builder::new()
            .name("yesno-e2e-channel-fixture".to_owned())
            .spawn(move || {
                let mut workers: Vec<thread::JoinHandle<()>> = Vec::new();
                while !stop_loop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let host = host.clone();
                            let muted = Muted {
                                mute: mute.clone(),
                                stop: Arc::clone(&stop_loop),
                            };
                            workers.push(thread::spawn(move || {
                                // The listener is non-blocking; the accepted
                                // stream must not be, or `serve_blocking` spins.
                                if stream.set_nonblocking(false).is_err() {
                                    return;
                                }
                                Self::serve_one(stream, host, muted);
                            }));
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(20));
                        }
                        Err(_) => break,
                    }
                }
                for worker in workers {
                    let _ = worker.join();
                }
            })
            .map_err(|error| format!("cannot spawn the channel fixture: {error}"))?;

        Ok((
            Self {
                stop,
                thread: Some(thread),
            },
            socket.to_string_lossy().into_owned(),
        ))
    }

    fn serve_one(stream: std::os::unix::net::UnixStream, host: yesno_plugin::Host, muted: Muted) {
        use yesno_plugin::channel::{send_fd, serve_blocking, Arena, Session};
        let limits = Self::limits();
        match Arena::new(limits.arena_bytes()) {
            Ok(arena) => {
                if send_fd(&stream, arena.as_fd()).is_err() {
                    return;
                }
                let mut session = Session::new(host, arena, limits);
                let _ = serve_blocking(&mut session, muted.over(stream));
            }
            Err(_) => {
                let mut session = Session::new_inline(host, limits);
                let _ = serve_blocking(&mut session, muted.over(stream));
            }
        }
    }
}

/// Holds every reply of a [`ChannelFixture`] connection while the file `mute`
/// exists, until it is removed or the fixture stops.
///
/// The server a *release* must not wait on for ever. [`SilentFixture`] never
/// greets, so no client gets far enough to hold a snapshot on it; this one
/// answers until told not to. A scenario tells it from inside a `psql` session,
/// with `\! touch`, because no verb can run while that session does. Requests
/// are still read and handled while muted, so the session's state moves on
/// exactly as it would on a server whose answers were lost.
struct Muted {
    mute: PathBuf,
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl Muted {
    fn over(self, stream: std::os::unix::net::UnixStream) -> MutedStream {
        MutedStream {
            stream,
            muted: self,
        }
    }
}

struct MutedStream {
    stream: std::os::unix::net::UnixStream,
    muted: Muted,
}

impl Read for MutedStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.stream.read(buf)
    }
}

impl Write for MutedStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        while self.muted.mute.exists()
            && !self.muted.stop.load(std::sync::atomic::Ordering::Relaxed)
        {
            thread::sleep(Duration::from_millis(20));
        }
        self.stream.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.stream.flush()
    }
}

/// Listeners that accept every connection and never say anything: one on TCP
/// for Flight, one on a Unix socket for the plugin channel.
///
/// The server a client must not wait on forever. The connect succeeds, so a
/// client goes on to wait for the protocol's first bytes, which never come --
/// the case a refused or unrouted address cannot produce, because both fail
/// promptly. Accepted sockets are held, not read, until the fixture stops.
struct SilentFixture {
    stop: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl SilentFixture {
    fn start(dir: &Path) -> Result<(Self, String, String), String> {
        use std::sync::atomic::{AtomicBool, Ordering};
        let listener = std::net::TcpListener::bind("127.0.0.1:0")
            .map_err(|e| format!("cannot bind a silent listener: {e}"))?;
        let addr = listener
            .local_addr()
            .map_err(|e| format!("cannot read the silent listener's address: {e}"))?;
        let path = dir.join("silent.sock");
        let unix = std::os::unix::net::UnixListener::bind(&path)
            .map_err(|e| format!("cannot bind a silent socket at {}: {e}", path.display()))?;
        // Non-blocking so the loop can notice `stop`, as `ChannelFixture` does.
        listener
            .set_nonblocking(true)
            .map_err(|e| format!("cannot make the silent listener non-blocking: {e}"))?;
        unix.set_nonblocking(true)
            .map_err(|e| format!("cannot make the silent socket non-blocking: {e}"))?;
        let stop = Arc::new(AtomicBool::new(false));
        let stop_loop = Arc::clone(&stop);
        let thread = thread::Builder::new()
            .name("fixture-silent".into())
            .spawn(move || {
                let (mut tcp, mut uds) = (Vec::new(), Vec::new());
                while !stop_loop.load(Ordering::Relaxed) {
                    let mut idle = true;
                    if let Ok((stream, _)) = listener.accept() {
                        tcp.push(stream);
                        idle = false;
                    }
                    if let Ok((stream, _)) = unix.accept() {
                        uds.push(stream);
                        idle = false;
                    }
                    if idle {
                        thread::sleep(Duration::from_millis(20));
                    }
                }
            })
            .map_err(|e| format!("cannot start the silent listener: {e}"))?;
        Ok((
            SilentFixture {
                stop,
                thread: Some(thread),
            },
            format!("http://{addr}"),
            path.display().to_string(),
        ))
    }
}

impl Drop for SilentFixture {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for ChannelFixture {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
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
                let requests = service.request_counts();
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
                        .send(Ok((address, requests)))
                        .map_err(|_| "scenario stopped before Flight startup".to_owned())?;
                    tonic::transport::Server::builder()
                        .add_service(FlightServiceServer::new(service))
                        .serve_with_incoming_shutdown(incoming_nodelay(listener), async {
                            let _ = shutdown_rx.await;
                        })
                        .await
                        .map_err(|error| format!("Flight fixture failed: {error}"))
                })
            })
            .map_err(|error| format!("cannot spawn Flight fixture: {error}"))?;
        let mut fixture = Self {
            shutdown: Some(shutdown),
            thread: Some(thread),
            requests: Arc::new(yesno_flight::RequestCounts::default()),
        };
        match ready_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok((address, requests))) => {
                // Replaces the placeholder above with the service's own, which
                // is the one it increments. A scenario reading the placeholder
                // would see zeroes forever and every assertion about request
                // counts would pass vacuously.
                fixture.requests = requests;
                Ok((fixture, format!("http://{address}")))
            }
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
    deadline: Option<Instant>,
) -> Result<(std::process::ExitStatus, String), String> {
    let mut transcript = tempfile::tempfile_in(home)
        .map_err(|error| format!("cannot create command transcript: {error}"))?;
    let duplicate = || {
        transcript
            .try_clone()
            .map_err(|error| format!("cannot duplicate command transcript: {error}"))
    };
    let mut command =
        prepared_command(program, args, env, home).map_err(|error| error.to_string())?;
    command
        .stdout(Stdio::from(duplicate()?))
        .stderr(Stdio::from(duplicate()?));
    let status = run_to_exit(command, program, stdin, deadline)?;
    let output = String::from_utf8(read_back(&mut transcript, "transcript")?)
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

#[cfg(test)]
mod accept {
    use tokio_stream::StreamExt as _;

    /// The server's own end, which is the end whose Nagle delay stalls a
    /// streamed response; the client's setting says nothing about it.
    #[test]
    fn a_harness_server_connection_is_served_without_nagle() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()
            .unwrap();
        runtime.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let incoming = super::incoming_nodelay(listener);
            tokio::pin!(incoming);
            let _client = tokio::net::TcpStream::connect(addr).await.unwrap();
            let served = incoming.next().await.unwrap().unwrap();
            assert!(served.nodelay().unwrap());
        });
    }
}
