//! The daemon binary, started the way an operator starts it.
//!
//! # Why this file exists
//!
//! Until it did, **nothing in this repository executed either daemon binary**.
//! `main.rs` is 742 lines and `bin/yesno.rs` is 2084, and every test assembled its
//! own server from library calls -- `lifecycle::serve_reads`, `follower::start`,
//! `start_with_plugin`. So the verification boundary sat exactly on the reachability
//! boundary: everything *below* `main.rs` was well tested, and `main.rs` is where
//! "can an operator reach this" is decided.
//!
//! That let two bugs through in one day. A plugin channel that was complete,
//! correct and called by nobody, and an inline fallback that existed in the session
//! and not in the server. Both were found by a consumer trying to use them, because
//! a consumer was the only thing in the loop playing the operator.
//!
//! **A test that constructs its own server cannot notice that production never
//! constructs one.** These tests construct a config file and a process instead, so
//! what they exercise is argument parsing, config loading, role selection and
//! wiring -- the region nothing else touches.
//!
//! # What they deliberately do not test
//!
//! Protocol behaviour. The channel's frames, paging and reclamation are covered in
//! `plugin_channel.rs` against a `Channel` built in-process, which is faster and
//! more precise. Here the question is only whether a configured daemon *reaches*
//! them, so the greeting is enough and a full scan would be duplication.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

struct Clean(PathBuf);
impl Drop for Clean {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A child that is always signalled and reaped, however the test ends.
struct Daemon {
    child: Child,
    reaped: bool,
    /// Drained on a thread, because reading it inline would block the test whenever
    /// the daemon has nothing to say -- and the interesting cases are exactly the
    /// ones where it says something unexpected.
    log: std::sync::Arc<std::sync::Mutex<String>>,
}

impl Daemon {
    /// Everything the daemon has said so far.
    fn log(&self) -> String {
        self.log.lock().map(|g| g.clone()).unwrap_or_default()
    }

    /// Wait until the daemon reports it is serving, not merely that a file appeared.
    ///
    /// # Why "ready" and not "serving", and not the socket
    ///
    /// `plugin::wire` binds the channel **before** the startup path opens the
    /// database, so the socket exists while `host.db()` is still `None` and every
    /// request answers `UNAVAILABLE`. That is the documented unavailable state and is
    /// fine for a peer, which must handle it during a rebootstrap anyway -- but it
    /// means **the socket's existence is not readiness**, and a test that treated it
    /// as readiness raced the daemon's own signal handler and was killed by the
    /// `SIGTERM` it sent.
    ///
    /// "yesnod is serving" is no better: it is logged when the listeners bind, and
    /// the signal handler is installed later still. "yesnod is ready" marks the point
    /// the daemon enters its supervision loop, which is the first moment a `SIGTERM`
    /// shuts it down rather than killing it.
    fn wait_until_serving(&mut self, secs: u64) {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            if self.log().contains("yesnod is ready") {
                return;
            }
            if let Ok(Some(status)) = self.child.try_wait() {
                self.reaped = true;
                panic!(
                    "the daemon exited with {status} before becoming ready:\n{}",
                    self.log()
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!(
            "the daemon did not report readiness within {secs}s:\n{}",
            self.log()
        );
    }

    /// Ask it to stop the way an init system would, and wait.
    ///
    /// `SIGTERM` rather than `Child::kill`, which sends `SIGKILL`: the point is to
    /// exercise the shutdown path, and a killed process runs none of it.
    fn terminate(&mut self) -> std::process::ExitStatus {
        // SAFETY: `kill` with a signal performs no memory access; the pid is this
        // child's and it has not been reaped.
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        let status = self
            .child
            .wait()
            .expect("the daemon must be waitable after SIGTERM");
        self.reaped = true;
        status
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if !self.reaped {
            // A test that failed before terminating must not leave a daemon holding
            // the database lock: the next test with the same directory would fail
            // with `AlreadyOpen` for a reason unrelated to itself.
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// Write a config an operator could write, and return its path.
fn write_config(dir: &Path, extra: &str) -> PathBuf {
    let path = dir.join("yesnod.toml");
    // Port zero everywhere: the OS chooses, so two of these running concurrently do
    // not collide on a fixed default and fail for a reason unrelated to the test.
    let text = format!(
        "[server]\n\
         role = \"leader\"\n\
         data_dir = {data:?}\n\
         shutdown_grace_secs = 5\n\
         \n\
         [server.flight]\n\
         listen = \"127.0.0.1:0\"\n\
         \n\
         [server.control]\n\
         listen = \"127.0.0.1:0\"\n\
         journal_dir = {journal:?}\n\
         \n\
         [server.metrics]\n\
         listen = \"127.0.0.1:0\"\n\
         \n\
         [db]\n\
         shards = 1\n\
         \n\
         # The daemon refuses a shared control endpoint with no rule, because an\n\
         # unmatched request is denied and a rule-less endpoint answers nothing.\n\
         # An operator's file must carry one, so this one does too.\n\
         [[auth.rule]]\n\
         channel = \"host\"\n\
         principal = \"all\"\n\
         address = \"127.0.0.0/8\"\n\
         capability = \"control-admin\"\n\
         action = \"allow\"\n\
         {extra}",
        data = dir.join("data"),
        journal = dir.join("control"),
    );
    std::fs::write(&path, text).unwrap();
    path
}

fn spawn(config: &Path) -> Daemon {
    let mut child = Command::new(env!("CARGO_BIN_EXE_yesnod"))
        .arg("--config")
        .arg(config)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("the daemon binary must be spawnable");

    let log = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    for stream in [
        child.stderr.take().map(Pipe::Err),
        child.stdout.take().map(Pipe::Out),
    ]
    .into_iter()
    .flatten()
    {
        let log = log.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            let mut r: Box<dyn Read + Send> = match stream {
                Pipe::Err(e) => Box::new(e),
                Pipe::Out(o) => Box::new(o),
            };
            while let Ok(n) = r.read(&mut buf) {
                if n == 0 {
                    break;
                }
                if let Ok(mut g) = log.lock() {
                    g.push_str(&String::from_utf8_lossy(&buf[..n]));
                }
            }
        });
    }

    Daemon {
        child,
        reaped: false,
        log,
    }
}

/// Which pipe a drainer thread owns. Both go to one buffer, because the daemon's
/// readiness line and its failures do not reliably use the same stream.
enum Pipe {
    Err(std::process::ChildStderr),
    Out(std::process::ChildStdout),
}

/// Wait for `path` to exist, or explain what the daemon said instead.
fn wait_for(path: &Path, daemon: &mut Daemon, secs: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        // If it has already exited, waiting out the timeout tells us nothing.
        if let Ok(Some(status)) = daemon.child.try_wait() {
            daemon.reaped = true;
            panic!(
                "the daemon exited with {status} before binding:\n{}",
                daemon.log()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn tmpdir(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("yesnod-smoke-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// A configured `plugin.channel_socket` is bound by the daemon and reachable.
///
/// This is the test whose absence let a complete, correct, uncalled channel ship.
#[test]
fn a_configured_plugin_channel_is_bound_by_the_daemon() {
    let dir = tmpdir("chan");
    let _c = Clean(dir.clone());
    let sock = dir.join("plugin.sock");
    let cfg = write_config(
        &dir,
        &format!("\n[plugin]\nchannel_socket = {sock:?}\nchannel_max_lanes = 8\n"),
    );
    let mut daemon = spawn(&cfg);

    assert!(
        wait_for(&sock, &mut daemon, 30),
        "the daemon must bind the socket its configuration names"
    );
    // Bound is not ready: wire() binds before the database opens, so wait for the
    // daemon to say it is serving before expecting it to handle a signal.
    daemon.wait_until_serving(30);

    // Reachable, not merely present: a peer gets a greeting.
    let mut peer = std::os::unix::net::UnixStream::connect(&sock)
        .expect("a bound socket must accept a connection");
    let (fd, version) =
        yesno_plugin::channel::recv_fd(&peer).expect("the arena descriptor comes first");
    assert_eq!(version, 1);
    drop(fd);

    let mut buf = Vec::new();
    let hello = loop {
        match yesno_plugin::ipc::Frame::decode(&buf) {
            Ok((f, _)) => break f,
            Err(yesno_plugin::ipc::IpcError::Truncated) => {}
            Err(e) => panic!("{e}"),
        }
        let mut chunk = [0u8; 4096];
        let n = peer.read(&mut chunk).unwrap();
        assert!(n > 0, "the daemon closed instead of greeting");
        buf.extend_from_slice(&chunk[..n]);
    };
    match hello {
        yesno_plugin::ipc::Frame::ServerHello {
            max_lanes,
            arena_bytes,
            ..
        } => {
            assert_eq!(max_lanes, 8, "the greeting reflects the operator's file");
            assert!(arena_bytes > 0, "an arena host advertises its size");
        }
        other => panic!("expected ServerHello, got {other:?}"),
    }
    // Write something, so the connection is real in both directions.
    peer.write_all(
        &yesno_plugin::ipc::Frame::ClientHello {
            protocol: 1,
            name: "smoke".into(),
        }
        .encode()
        .unwrap(),
    )
    .unwrap();
    drop(peer);

    let status = daemon.terminate();
    assert!(
        status.success(),
        "a SIGTERM shutdown must be clean: {status}"
    );
    assert!(
        !sock.exists(),
        "the daemon must remove its socket on the way out"
    );
}

/// An unconfigured daemon binds no plugin socket.
///
/// The other half of the previous test: a default configuration must not start a
/// listener nobody asked for.
#[test]
fn an_unconfigured_daemon_binds_no_plugin_socket() {
    let dir = tmpdir("nochan");
    let _c = Clean(dir.clone());
    let cfg = write_config(&dir, "");
    let mut daemon = spawn(&cfg);

    // There is no socket to wait for, so wait for the database instead -- the
    // daemon creates its directory, which proves it got past configuration.
    daemon.wait_until_serving(30);
    assert!(
        !dir.join("plugin.sock").exists(),
        "nothing configured, nothing bound"
    );

    let status = daemon.terminate();
    assert!(
        status.success(),
        "a SIGTERM shutdown must be clean, got {status}:\n{}",
        daemon.log()
    );
}

/// A plugin on a cold standby is refused by the binary, with the flag named.
///
/// The library-level test asserts the refusal; this asserts an operator actually
/// sees it, because a refusal that never reaches `main.rs` is a refusal nobody gets.
#[test]
fn a_plugin_on_a_cold_standby_refuses_to_start() {
    let dir = tmpdir("cold");
    let _c = Clean(dir.clone());
    let sock = dir.join("plugin.sock");
    let path = dir.join("yesnod.toml");
    // A follower that does not serve reads never opens a database, so a channel on
    // it would answer UNAVAILABLE for the life of the process.
    let text = format!(
        "[server]\n\
         role = \"follower\"\n\
         data_dir = {data:?}\n\
         \n\
         [follower]\n\
         leader = \"http://127.0.0.1:1\"\n\
         serve_reads = false\n\
         \n\
         [plugin]\n\
         channel_socket = {sock:?}\n",
        data = dir.join("data"),
        sock = sock,
    );
    std::fs::write(&path, text).unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_yesnod"))
        .arg("--config")
        .arg(&path)
        .output()
        .expect("the daemon binary must be spawnable");
    assert!(
        !out.status.success(),
        "a plugin on a cold standby must not start"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("serve_reads"),
        "the operator must be told which flag to change, got: {err}"
    );
    assert!(!sock.exists(), "and nothing is bound before the refusal");
}
