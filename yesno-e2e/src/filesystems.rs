//! `fs_*`: real ZFS, Btrfs, and LVM snapshot providers inside a KVM guest.
//!
//! The routine Cargo suite cannot assume either filesystem, root privileges,
//! or `/dev/kvm`. The opt-in all-in-one driver can: it carries a bootable Linux
//! guest with both filesystem toolchains and runs it with one disposable data
//! disk. The scenario still owns the sequence and assertions; these verbs own
//! only QEMU, the framed serial console, the shipped clients, bounded waits,
//! and the Protobuf RPCs.
//!
//! A base backup intentionally crosses the guest boundary through the normal
//! control-plane stream. The deployed sidecar instead uses the dual-opt-in
//! direct path because it is co-located in the guest, publishes to a stateful
//! Winterbaume S3 endpoint, and is finally proved by restoring through yesnoctl.

use std::fs::{File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use base64::Engine as _;
use monty_types::{ExcType, MontyException, MontyObject};
use prost::Message;
use tonic::transport::Channel;
use yesno_server::control::pb;
use yesno_server::control::pb::control_plane_client::ControlPlaneClient;

use crate::convert::{dict, int_obj, value_err, whole_obj, Args};
use crate::world::World;

/// Native-filesystem verbs dispatched by the ordinary Monty scenario runner.
pub const OWNS: &[&str] = &[
    "fs_prepare",
    "fs_put",
    "fs_get",
    "fs_checkpoint",
    "fs_snapshot_begin",
    "fs_snapshot_count",
    "fs_snapshot_release",
    "fs_basebackup",
    "fs_archive_start",
    "fs_archive_wait",
    "fs_archive_stop",
    "fs_archive_restore",
    "fs_crash_server",
    "fs_restart_server",
    "fs_snapshot_wait",
    "fs_mount_isolation",
    "fs_cleanup",
];

const GUEST_WAIT: Duration = Duration::from_secs(90);
const SERVER_WAIT: Duration = Duration::from_secs(45);
const SNAPSHOT_WAIT: Duration = Duration::from_secs(30);
// Keep fixture boot failure below the guest startup bound while allowing slow CI hosts.
const S3_WAIT: Duration = Duration::from_secs(90);
const CONSOLE_READY: &[u8] = b"__YESNO_E2E_READY__";
// One fixed bucket and prefix are safe because every world owns a private server.
const S3_BUCKET: &str = "yesno-e2e";
const S3_PREFIX: &str = "archive";
// The daemon never runs as root in any guest, whatever the backend.
const DAEMON_USER: &str = "yesno";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Backend {
    Zfs,
    Btrfs,
    Lvm,
}

impl Backend {
    fn parse(value: &str) -> Result<Self, MontyException> {
        match value {
            "zfs" => Ok(Self::Zfs),
            "btrfs" => Ok(Self::Btrfs),
            "lvm" => Ok(Self::Lvm),
            other => Err(value_err(format!(
                "fs_prepare(): backend must be 'zfs', 'btrfs', or 'lvm', got '{other}'"
            ))),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Zfs => "zfs",
            Self::Btrfs => "btrfs",
            Self::Lvm => "lvm",
        }
    }

    fn data_dir(self) -> &'static str {
        match self {
            Self::Zfs => "/var/lib/yesno",
            Self::Btrfs => "/mnt/yesno/data",
            Self::Lvm => "/mnt/yesno/data",
        }
    }

    fn restore_dir(self) -> &'static str {
        match self {
            Self::Zfs => "/var/lib/yesno-restore/data",
            Self::Btrfs => "/mnt/yesno/restored",
            Self::Lvm => "/mnt/yesno/restored",
        }
    }

    /// Grant the daemon account exactly the snapshot privilege the backend can
    /// delegate, and nothing beyond it.
    ///
    /// This is the whole point of running all three guests unprivileged. ZFS
    /// delegates snapshot verbs on one dataset, Btrfs grants them through
    /// directory ownership plus one mount option, and LVM can delegate nothing
    /// at all — which is why it, and only it, needs the second agent process.
    fn delegation(self) -> String {
        match self {
            Self::Zfs => format!(
                "zfs allow {DAEMON_USER} snapshot,destroy,mount yesno-e2e && chown -R {DAEMON_USER}:{DAEMON_USER} /var/lib/yesno /var/lib/yesno-restore"
            ),
            Self::Btrfs => format!(
                "chown {DAEMON_USER}:{DAEMON_USER} /mnt/yesno /mnt/yesno/data /mnt/yesno/snapshots"
            ),
            Self::Lvm => format!("chown {DAEMON_USER}:{DAEMON_USER} /mnt/yesno/data"),
        }
    }
}

struct Vm {
    child: Child,
    control_port: u16,
    console: GuestConsole,
    log: PathBuf,
    qemu_log: PathBuf,
    backend: Backend,
}

struct GuestConsole {
    stream: UnixStream,
    buffered: Vec<u8>,
    log: PathBuf,
    sequence: u64,
}

struct S3Fixture {
    child: Child,
    port: u16,
    log: PathBuf,
}

#[derive(Clone, Debug)]
struct ArchiveStats {
    base_generation: u64,
    event_sequence: u64,
    term: u32,
    cursor_total: u64,
    base_ready: bool,
}

impl ArchiveStats {
    fn metric(&self, name: &str) -> Option<u64> {
        match name {
            "base_generation" => Some(self.base_generation),
            "event_sequence" => Some(self.event_sequence),
            "cursor_total" => Some(self.cursor_total),
            _ => None,
        }
    }
}

/// Where the daemon, the agent and the host each stand with respect to mount
/// namespaces, and what each of them can see under the snapshot directory.
#[derive(Clone, Debug)]
struct MountIsolation {
    daemon_ns: String,
    agent_ns: String,
    host_ns: String,
    isolated: bool,
    daemon_mounts: usize,
    host_mounts: usize,
}

#[derive(Clone, Debug)]
struct RestoreStats {
    base_generation: u64,
    checkpoint_version: u64,
    recovered_version: u64,
    shards: u64,
    bytes: u64,
}

impl GuestConsole {
    fn connect(socket: &Path, log: PathBuf, timeout: Duration) -> Result<Self, String> {
        let deadline = Instant::now() + timeout;
        let stream = loop {
            match UnixStream::connect(socket) {
                Ok(stream) => break stream,
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(error) => {
                    return Err(format!(
                        "cannot connect to guest serial console '{}': {error}",
                        socket.display()
                    ));
                }
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_millis(250)))
            .map_err(|error| format!("cannot set guest console read timeout: {error}"))?;
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .map_err(|error| format!("cannot set guest console write timeout: {error}"))?;
        let mut console = Self {
            stream,
            buffered: Vec::new(),
            log,
            sequence: 0,
        };
        console.read_until(CONSOLE_READY, deadline)?;
        Ok(console)
    }

    fn read_until(&mut self, marker: &[u8], deadline: Instant) -> Result<Vec<u8>, String> {
        loop {
            if let Some(position) = self
                .buffered
                .windows(marker.len())
                .position(|window| window == marker)
            {
                let before = self.buffered.drain(..position).collect();
                self.buffered.drain(..marker.len());
                return Ok(before);
            }
            if Instant::now() >= deadline {
                let serial = std::fs::read(&self.log).unwrap_or_default();
                let start = serial.len().saturating_sub(16 * 1024);
                return Err(format!(
                    "timed out waiting for guest console marker '{}'\nguest serial log (last bytes):\n{}",
                    String::from_utf8_lossy(marker),
                    String::from_utf8_lossy(&serial[start..]),
                ));
            }
            let mut chunk = [0_u8; 8192];
            match self.stream.read(&mut chunk) {
                Ok(0) => return Err("guest serial console closed".into()),
                Ok(length) => {
                    OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&self.log)
                        .and_then(|mut file| file.write_all(&chunk[..length]))
                        .map_err(|error| format!("cannot append guest console log: {error}"))?;
                    self.buffered.extend_from_slice(&chunk[..length]);
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(error) => return Err(format!("cannot read guest serial console: {error}")),
            }
        }
    }

    fn command(&mut self, command: &str) -> Result<String, String> {
        self.sequence += 1;
        let prefix = format!("__YESNO_E2E_DONE_{}_", self.sequence);
        let framed = format!("{command}\nstatus=$?\nprintf '\\n{prefix}%s__\\n' \"$status\"\n");
        self.stream
            .write_all(framed.as_bytes())
            .map_err(|error| format!("cannot write guest serial command: {error}"))?;
        self.stream
            .flush()
            .map_err(|error| format!("cannot flush guest serial command: {error}"))?;

        let deadline = Instant::now() + GUEST_WAIT;
        let output = self.read_until(prefix.as_bytes(), deadline)?;
        let status = self.read_until(b"__", deadline)?;
        let status = String::from_utf8_lossy(&status)
            .trim()
            .parse::<i32>()
            .map_err(|error| {
                format!("guest serial command returned an invalid status marker: {error}")
            })?;
        let output = String::from_utf8_lossy(&output).trim().to_owned();
        if status == 0 {
            Ok(output)
        } else {
            Err(format!("guest command exited with {status}: {output}"))
        }
    }
}

impl Vm {
    fn command(&mut self, command: &str) -> Result<String, String> {
        self.console.command(command)
    }

    fn diagnostics(&self) -> String {
        let serial = std::fs::read(&self.log).unwrap_or_default();
        let serial_start = serial.len().saturating_sub(16 * 1024);
        let qemu = std::fs::read(&self.qemu_log).unwrap_or_default();
        let qemu_start = qemu.len().saturating_sub(4 * 1024);
        format!(
            "guest serial log (last bytes):\n{}\nQEMU process log (last bytes):\n{}",
            String::from_utf8_lossy(&serial[serial_start..]),
            String::from_utf8_lossy(&qemu[qemu_start..])
        )
    }

    fn snapshot_count(&mut self) -> Result<usize, String> {
        let output = match self.backend {
            Backend::Zfs => self.command("zfs list -H -t snapshot -o name -r yesno-e2e")?,
            Backend::Btrfs => self.command("btrfs subvolume list -o /mnt/yesno/snapshots")?,
            Backend::Lvm => self.command("lvs --noheadings -o lv_name yesno-e2e")?,
        };
        Ok(output
            .lines()
            .filter(|line| line.contains("yesno-snapshot-"))
            .count())
    }

    fn stop_server(&mut self) -> Result<(), String> {
        self.command("systemctl stop yesnod.service").map(|_| ())
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl S3Fixture {
    fn start(root: &Path, backend: Backend) -> Result<Self, String> {
        let port = free_port()?;
        let log = root.join(format!("{}-winterbaume.log", backend.name()));
        let blobs = root.join(format!("{}-winterbaume-blobs", backend.name()));
        std::fs::create_dir_all(&blobs)
            .map_err(|error| format!("cannot create Winterbaume VFS directory: {error}"))?;
        let stdout = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&log)
            .map_err(|error| format!("cannot create Winterbaume log: {error}"))?;
        let stderr = stdout
            .try_clone()
            .map_err(|error| format!("cannot clone Winterbaume log: {error}"))?;
        let child = Command::new("winterbaume-server")
            .args([
                "--host",
                "0.0.0.0",
                "--port",
                &port.to_string(),
                "--vfs-dir",
            ])
            .arg(blobs)
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .spawn()
            .map_err(|error| format!("cannot start winterbaume-server: {error}"))?;
        let mut fixture = Self { child, port, log };
        let deadline = Instant::now() + S3_WAIT;
        loop {
            if let Some(status) = fixture
                .child
                .try_wait()
                .map_err(|error| format!("cannot poll winterbaume-server: {error}"))?
            {
                return Err(format!(
                    "winterbaume-server exited with {status} before S3 became ready\n{}",
                    fixture.diagnostics()
                ));
            }
            match provision_s3_bucket(port) {
                Ok(()) => return Ok(fixture),
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(error) => {
                    return Err(format!(
                        "Winterbaume S3 did not become ready: {error}\n{}",
                        fixture.diagnostics()
                    ));
                }
            }
        }
    }

    fn diagnostics(&self) -> String {
        let bytes = std::fs::read(&self.log).unwrap_or_default();
        let start = bytes.len().saturating_sub(16 * 1024);
        format!(
            "Winterbaume log (last bytes):\n{}",
            String::from_utf8_lossy(&bytes[start..])
        )
    }
}

impl Drop for S3Fixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// One disposable native-filesystem guest owned by a scenario world.
#[derive(Default)]
pub struct FilesystemState {
    vm: Option<Vm>,
    runtime: Option<tokio::runtime::Runtime>,
    lease: Option<Vec<u8>>,
    s3: Option<S3Fixture>,
    archive_running: bool,
}

fn fs_err(verb: &str, error: impl std::fmt::Display) -> MontyException {
    MontyException::new(
        ExcType::RuntimeError,
        Some(format!("{verb}(): filesystem E2E failed: {error}")),
    )
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

fn free_port() -> Result<u16, String> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|error| format!("cannot reserve a QEMU forwarding port: {error}"))?;
    listener
        .local_addr()
        .map(|address| address.port())
        .map_err(|error| format!("cannot inspect a QEMU forwarding port: {error}"))
}

fn provision_s3_bucket(port: u16) -> Result<(), String> {
    let address = format!("127.0.0.1:{port}")
        .parse()
        .map_err(|error| format!("cannot parse Winterbaume address: {error}"))?;
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_millis(500))
        .map_err(|error| format!("cannot connect to Winterbaume: {error}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|error| format!("cannot set Winterbaume read timeout: {error}"))?;
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .map_err(|error| format!("cannot set Winterbaume write timeout: {error}"))?;
    let request = format!(
        "PUT /{S3_BUCKET} HTTP/1.1\r\nHost: s3.amazonaws.com\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|error| format!("cannot create Winterbaume bucket: {error}"))?;
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|error| format!("cannot read Winterbaume response: {error}"))?;
    let status = response
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .ok_or_else(|| format!("Winterbaume returned an invalid HTTP response: {response:?}"))?;
    if matches!(status, "200" | "204" | "409") {
        Ok(())
    } else {
        Err(format!(
            "Winterbaume bucket creation returned HTTP {status}: {response}"
        ))
    }
}

fn image_path(variable: &str, default: &str) -> PathBuf {
    std::env::var_os(variable).map_or_else(|| PathBuf::from(default), PathBuf::from)
}

fn require_file(path: &Path, subject: &str) -> Result<(), String> {
    if path.is_file() {
        Ok(())
    } else {
        Err(format!(
            "{subject} '{}' is missing; run this scenario through scripts/gate-filesystems.sh",
            path.display()
        ))
    }
}

fn qemu_binary() -> &'static str {
    if cfg!(target_arch = "x86_64") {
        "qemu-system-x86_64"
    } else if cfg!(target_arch = "aarch64") {
        "qemu-system-aarch64"
    } else {
        "qemu-system-unsupported"
    }
}

fn qemu_machine() -> &'static str {
    if cfg!(target_arch = "x86_64") {
        "q35"
    } else {
        "virt,gic-version=host"
    }
}

fn guest_console() -> &'static str {
    if cfg!(target_arch = "x86_64") {
        "ttyS0"
    } else {
        "ttyAMA0"
    }
}

fn guest_root_device() -> &'static str {
    if cfg!(target_arch = "x86_64") {
        "/dev/vda"
    } else {
        // QEMU's ARM virtio-mmio transports are enumerated in reverse command
        // order: the first declared block device becomes vdb.
        "/dev/vdb"
    }
}

fn guest_block_device(drive: &str) -> String {
    if cfg!(target_arch = "x86_64") {
        format!("virtio-blk-pci,drive={drive},serial={drive}")
    } else {
        format!("virtio-blk-device,drive={drive},serial={drive}")
    }
}

fn guest_net_device() -> &'static str {
    if cfg!(target_arch = "x86_64") {
        "virtio-net-pci,netdev=net0"
    } else {
        "virtio-net-device,netdev=net0"
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn s3_environment(port: u16) -> String {
    format!(
        "AWS_ACCESS_KEY_ID=yesno-e2e AWS_SECRET_ACCESS_KEY=yesno-e2e AWS_DEFAULT_REGION=us-east-1 AWS_ALLOW_HTTP=true AWS_ENDPOINT=http://10.0.2.2:{port} AWS_VIRTUAL_HOSTED_STYLE_REQUEST=false"
    )
}

fn archive_service(port: u16) -> String {
    format!(
        r#"[Unit]
Description=yesno E2E archive sidecar
Requires=yesnod.service
After=yesnod.service network-online.target

[Service]
Type=simple
Environment=AWS_ACCESS_KEY_ID=yesno-e2e
Environment=AWS_SECRET_ACCESS_KEY=yesno-e2e
Environment=AWS_DEFAULT_REGION=us-east-1
Environment=AWS_ALLOW_HTTP=true
Environment=AWS_ENDPOINT=http://10.0.2.2:{port}
Environment=AWS_VIRTUAL_HOSTED_STYLE_REQUEST=false
ExecStart=/usr/local/bin/yesno-archive --endpoint unix:///run/yesno/control.sock --store s3://{S3_BUCKET}/{S3_PREFIX} --work-dir /var/lib/yesno-archive --snapshot-mode server --direct-snapshot-path
Restart=no
TimeoutStopSec=45
"#
    )
}

fn archive_stats_object(stats: ArchiveStats) -> MontyObject {
    dict(vec![
        ("base_generation", int_obj(stats.base_generation)),
        ("event_sequence", int_obj(stats.event_sequence)),
        ("term", int_obj(u64::from(stats.term))),
        ("cursor_total", int_obj(stats.cursor_total)),
        ("base_ready", MontyObject::Bool(stats.base_ready)),
    ])
}

fn restore_stats_object(stats: RestoreStats) -> MontyObject {
    dict(vec![
        ("base_generation", int_obj(stats.base_generation)),
        ("checkpoint", int_obj(stats.checkpoint_version)),
        ("recovered", int_obj(stats.recovered_version)),
        ("shards", int_obj(stats.shards)),
        ("bytes", int_obj(stats.bytes)),
    ])
}

fn completion_field(output: &str, name: &str) -> Result<u64, String> {
    let prefix = format!("{name}=");
    output
        .split_whitespace()
        .find_map(|field| field.strip_prefix(&prefix))
        .ok_or_else(|| format!("restore completion omitted {name}: {output}"))?
        .parse()
        .map_err(|error| format!("restore completion has an invalid {name}: {error}"))
}

fn parse_restore_stats(output: &str) -> Result<RestoreStats, String> {
    Ok(RestoreStats {
        base_generation: completion_field(output, "base")?,
        checkpoint_version: completion_field(output, "checkpoint")?,
        recovered_version: completion_field(output, "recovered")?,
        shards: completion_field(output, "shards")?,
        bytes: completion_field(output, "bytes")?,
    })
}

fn make_server_config(data_dir: &str, journal_dir: &str, provider: &str) -> String {
    format!(
        r#"[server]
role = "leader"
data_dir = "{data_dir}"
shutdown_grace_secs = 5

[server.flight]
listen = "0.0.0.0:50051"

[server.control]
listen = "0.0.0.0:50052"
unix_socket = "/run/yesno/control.sock"
unix_socket_mode = "0660"
journal_dir = "{journal_dir}"

[server.snapshot]
{provider}

[server.metrics]
listen = ""

[db]
shards = 1

[db.checkpoint]
interval_secs = 3600

[[auth.rule]]
channel = "hostnossl"
principal = "all"
address = "all"
capability = "control-read"
action = "allow"

[[auth.rule]]
channel = "hostnossl"
principal = "all"
address = "all"
capability = "control-admin"
action = "allow"

[[auth.rule]]
channel = "hostnossl"
principal = "all"
address = "all"
capability = "replication"
action = "allow"

[[auth.rule]]
channel = "local"
principal = "uid:0"
address = "all"
capability = "control-read"
action = "allow"

[[auth.rule]]
channel = "local"
principal = "uid:0"
address = "all"
capability = "replication"
action = "allow"
"#,
    )
}

fn server_config(backend: Backend) -> String {
    let provider = match backend {
        Backend::Zfs => {
            "backend = \"zfs\"\nzfs_dataset = \"yesno-e2e\"\nallow_direct_path = true\nlease_ttl_secs = 300"
        }
        Backend::Btrfs => {
            "backend = \"btrfs\"\nbtrfs_snapshot_dir = \"/mnt/yesno/snapshots\"\nallow_direct_path = true\nlease_ttl_secs = 300"
        }
        Backend::Lvm => {
            "backend = \"lvm\"\nallow_direct_path = true\nlease_ttl_secs = 300\n\n[server.snapshot.lvm]\nsource_mount = \"/mnt/yesno/data\"\nvolume_group = \"yesno-e2e\"\nlogical_volume = \"data\"\nmount_dir = \"/mnt/yesno/snapshots\"\nfilesystem = \"ext4\"\nsnapshot_size_gib = 1\noperation_timeout_secs = 30"
        }
    };
    make_server_config(backend.data_dir(), "/var/lib/yesno-control", provider)
}

fn restored_server_config(backend: Backend) -> String {
    make_server_config(
        backend.restore_dir(),
        "/var/lib/yesno-restored-control",
        "backend = \"disabled\"",
    )
}

impl FilesystemState {
    fn runtime(&mut self) -> Result<&tokio::runtime::Runtime, String> {
        if self.runtime.is_none() {
            self.runtime =
                Some(tokio::runtime::Runtime::new().map_err(|error| {
                    format!("cannot start the filesystem E2E runtime: {error}")
                })?);
        }
        Ok(self.runtime.as_ref().expect("runtime was installed"))
    }

    fn vm(&self) -> Result<&Vm, String> {
        self.vm
            .as_ref()
            .ok_or("no filesystem guest; call fs_prepare() first".to_owned())
    }

    fn vm_mut(&mut self) -> Result<&mut Vm, String> {
        self.vm
            .as_mut()
            .ok_or("no filesystem guest; call fs_prepare() first".to_owned())
    }

    fn prepare(
        &mut self,
        root: &Path,
        backend: Backend,
        isolate_mounts: bool,
    ) -> Result<(), String> {
        if isolate_mounts && backend != Backend::Lvm {
            // ZFS materializes through a kernel automount the daemon triggers
            // itself, and Btrfs materializes no mount at all, so neither can
            // show anything about propagation across a namespace boundary.
            return Err(format!(
                "fs_prepare(): isolate_mounts is only meaningful for 'lvm', not '{}'",
                backend.name()
            ));
        }
        if self.vm.is_some() {
            return Err("a filesystem guest is already running".into());
        }
        let kvm = Path::new("/dev/kvm");
        if !kvm.exists() {
            return Err("/dev/kvm is not available inside the harness container".into());
        }
        File::open(kvm).map_err(|error| format!("/dev/kvm is not usable: {error}"))?;

        let base = image_path("YESNO_E2E_GUEST_IMAGE", "/opt/yesno-e2e/guest.qcow2");
        let kernel = image_path("YESNO_E2E_GUEST_KERNEL", "/opt/yesno-e2e/vmlinuz");
        require_file(&base, "guest image")?;
        require_file(&kernel, "guest kernel")?;

        let overlay = root.join(format!("{}-root.qcow2", backend.name()));
        let data = root.join(format!("{}-data.raw", backend.name()));
        checked_output(
            "qemu-img root overlay creation",
            Command::new("qemu-img")
                .args(["create", "-f", "qcow2", "-F", "qcow2", "-b"])
                .arg(&base)
                .arg(&overlay)
                .output()
                .map_err(|error| format!("cannot start qemu-img: {error}"))?,
        )?;
        checked_output(
            "qemu-img data disk creation",
            Command::new("qemu-img")
                .args(["create", "-f", "raw"])
                .arg(&data)
                .arg("4G")
                .output()
                .map_err(|error| format!("cannot start qemu-img: {error}"))?,
        )?;

        let flight_port = free_port()?;
        let control_port = free_port()?;
        let log = root.join(format!("{}-serial.log", backend.name()));
        let qemu_log = root.join(format!("{}-qemu.log", backend.name()));
        let serial_socket = root.join(format!("{}-console.sock", backend.name()));
        let process_log = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&qemu_log)
            .map_err(|error| format!("cannot create QEMU process log: {error}"))?;
        let process_err = process_log
            .try_clone()
            .map_err(|error| format!("cannot clone QEMU process log: {error}"))?;
        let forwarding = format!(
            "user,id=net0,hostfwd=tcp:127.0.0.1:{flight_port}-:50051,hostfwd=tcp:127.0.0.1:{control_port}-:50052"
        );
        let chardev = format!(
            "socket,id=console,path={},server=on,wait=on",
            serial_socket.display()
        );
        let append = format!(
            "root={} rootfstype=ext4 rw rootwait console={} net.ifnames=0 biosdevname=0 systemd.show_status=yes",
            guest_root_device(),
            guest_console()
        );
        let root_device = guest_block_device("root");
        let data_device = guest_block_device("data");
        let mut child = Command::new(qemu_binary())
            .args(["-machine", qemu_machine(), "-accel", "kvm", "-cpu", "host"])
            .args([
                "-m", "2048", "-smp", "2", "-display", "none", "-monitor", "none", "-chardev",
            ])
            .arg(&chardev)
            .args(["-serial", "chardev:console"])
            .arg("-kernel")
            .arg(&kernel)
            .arg("-append")
            .arg(append)
            .args([
                "-drive",
                &format!("if=none,id=root,file={},format=qcow2", overlay.display()),
            ])
            .args(["-device", &root_device])
            .args([
                "-drive",
                &format!("if=none,id=data,file={},format=raw", data.display()),
            ])
            .args(["-device", &data_device])
            .args(["-netdev", &forwarding, "-device", guest_net_device()])
            .args(["-no-reboot"])
            .stdout(Stdio::from(process_log))
            .stderr(Stdio::from(process_err))
            .spawn()
            .map_err(|error| format!("cannot start {}: {error}", qemu_binary()))?;
        let console = match GuestConsole::connect(&serial_socket, log.clone(), GUEST_WAIT) {
            Ok(console) => console,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let qemu = std::fs::read_to_string(&qemu_log).unwrap_or_default();
                return Err(format!("{error}\nQEMU process log:\n{qemu}"));
            }
        };
        self.vm = Some(Vm {
            child,
            control_port,
            console,
            log,
            qemu_log,
            backend,
        });

        self.vm_mut()?
            .command("test -b /dev/disk/by-id/virtio-data")?;
        self.vm_mut()?.command(
            "ip link set eth0 up && ip address replace 10.0.2.15/24 dev eth0 && ip route replace default via 10.0.2.2",
        )?;
        let setup = match backend {
            Backend::Zfs => {
                "modprobe zfs && zpool create -f -m /var/lib/yesno yesno-e2e /dev/disk/by-id/virtio-data && zfs set snapdir=visible yesno-e2e && zfs create -o mountpoint=/var/lib/yesno-restore yesno-e2e/restore"
            }
            // `user_subvol_rm_allowed` is not decoration: without it an
            // unprivileged owner can create a subvolume but never delete one,
            // so lease release and startup reconciliation would both fail.
            Backend::Btrfs => {
                "modprobe btrfs && mkfs.btrfs -f /dev/disk/by-id/virtio-data && mkdir -p /mnt/yesno && mount -o user_subvol_rm_allowed /dev/disk/by-id/virtio-data /mnt/yesno && btrfs subvolume create /mnt/yesno/data && mkdir -p /mnt/yesno/snapshots"
            }
            Backend::Lvm => {
                "modprobe dm_snapshot && pvcreate -ff -y /dev/disk/by-id/virtio-data && vgcreate yesno-e2e /dev/disk/by-id/virtio-data && lvcreate -L 2G -n data yesno-e2e && mkfs.ext4 -q /dev/yesno-e2e/data && mkdir -p /mnt/yesno/data /mnt/yesno/snapshots && mount /dev/yesno-e2e/data /mnt/yesno/data"
            }
        };
        self.vm_mut()?.command(setup)?;
        // The daemon owns its runtime and journal directories because it can
        // no longer create them under a root-owned /var/lib.
        self.vm_mut()?.command(&format!(
            "useradd --system --no-create-home --shell /usr/sbin/nologin {DAEMON_USER} && install -d -m 0755 -o {DAEMON_USER} -g {DAEMON_USER} /run/yesno /var/lib/yesno-control /var/lib/yesno-restored-control"
        ))?;
        self.vm_mut()?.command(&backend.delegation())?;
        let config = shell_quote(&server_config(backend));
        self.vm_mut()?.command(&format!(
            "install -d -m 0755 /etc/yesno && printf %s {config} > /etc/yesno/yesnod.toml"
        ))?;
        // The daemon is denied every capability for every backend, so a
        // provider that cannot work under delegation fails here rather than
        // silently passing because the gate handed it root.
        //
        // The control socket's mode is load-bearing for LVM. A root agent
        // whose bounding set holds only CAP_SYS_ADMIN has no CAP_DAC_OVERRIDE,
        // so a socket the daemon owns is unreachable to it through the "other"
        // class. `server.control.unix_socket_mode` and the supplementary group
        // below are what let the two processes share it; leaving the mode to
        // the daemon's umask is what made this fail when the daemon first
        // stopped running as root.
        // `PrivateMounts=yes` gives the daemon its own mount namespace with
        // slave propagation, which is what a container gets from Docker's
        // `bind-propagation=rslave` or Kubernetes' `mountPropagation:
        // HostToContainer`. The daemon must be started *before* any lease
        // mount exists, because a namespace only receives mounts made after it
        // was created — a namespace created later just inherits a copy of the
        // table and would see the mount whatever its propagation says.
        let isolation = if isolate_mounts {
            "\\nPrivateMounts=yes"
        } else {
            ""
        };
        self.vm_mut()?.command(&format!(
            "install -d /etc/systemd/system/yesnod.service.d && printf '[Service]\\nUser={DAEMON_USER}\\nGroup={DAEMON_USER}\\nCapabilityBoundingSet=\\nNoNewPrivileges=true{isolation}\\n' > /etc/systemd/system/yesnod.service.d/unprivileged.conf && systemctl daemon-reload"
        ))?;
        if backend == Backend::Lvm {
            self.vm_mut()?.command(&format!(
                "install -d /etc/systemd/system/yesno-snapshot-agent.service.d && printf '[Service]\\nSupplementaryGroups={DAEMON_USER}\\n' > /etc/systemd/system/yesno-snapshot-agent.service.d/socket-group.conf && systemctl daemon-reload"
            ))?;
        }
        self.vm_mut()?.command("systemctl start yesnod.service")?;
        if backend == Backend::Lvm {
            self.vm_mut()?
                .command("systemctl start yesno-snapshot-agent.service")?;
        }
        self.wait_server(SERVER_WAIT)
    }

    fn endpoint(&self) -> Result<String, String> {
        Ok(format!("http://127.0.0.1:{}", self.vm()?.control_port))
    }

    fn wait_server(&mut self, timeout: Duration) -> Result<(), String> {
        let endpoint = self.endpoint()?;
        let deadline = Instant::now() + timeout;
        loop {
            let (ready, last_error) = self
                .runtime()?
                .block_on(async {
                    let channel = match tokio::time::timeout(
                        Duration::from_secs(2),
                        Channel::from_shared(endpoint.clone())?.connect(),
                    )
                    .await
                    {
                        Ok(Ok(channel)) => channel,
                        Ok(Err(error)) => return Ok((false, error.to_string())),
                        Err(_) => return Ok((false, "connection attempt timed out".to_owned())),
                    };
                    match ControlPlaneClient::new(channel)
                        .get_snapshot(pb::GetSnapshotRequest {})
                        .await
                    {
                        Ok(response) => Ok::<_, Box<dyn std::error::Error + Send + Sync>>((
                            response.into_inner().database_open,
                            "database is not open".to_owned(),
                        )),
                        Err(error) => Ok((false, error.to_string())),
                    }
                })
                .map_err(|error| error.to_string())?;
            if ready {
                return Ok(());
            }
            if Instant::now() >= deadline {
                let _ = self.vm_mut()?.command(
                    "cat /sys/class/net/eth0/operstate || true; cat /proc/net/fib_trie || true; cat /proc/net/tcp /proc/net/tcp6 || true; systemctl status yesnod.service --no-pager -l || true; journalctl -u yesnod.service --no-pager -n 80 || true",
                );
                return Err(format!(
                    "yesnod did not become ready in the guest; last RPC error: {last_error}\n{}",
                    self.vm()?.diagnostics()
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

    fn begin_snapshot(&mut self) -> Result<(String, usize, u64, bool), String> {
        if self.lease.is_some() {
            return Err("a snapshot lease is already held".into());
        }
        let endpoint = self.endpoint()?;
        let (lease, direct) = self
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
                let chunk = chunks
                    .message()
                    .await?
                    .ok_or("direct-path stream is empty")?;
                let direct = matches!(
                    chunk.payload,
                    Some(pb::snapshot_file_chunk::Payload::DirectPath(ref path)) if !path.is_empty()
                ) && chunk.last
                    && chunk.total_size == file.size;
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>((lease, direct))
            })
            .map_err(|error| error.to_string())?;
        let source = match pb::BaseSnapshotSource::try_from(lease.source) {
            Ok(pb::BaseSnapshotSource::Zfs) => "zfs",
            Ok(pb::BaseSnapshotSource::Btrfs) => "btrfs",
            Ok(pb::BaseSnapshotSource::Lvm) => "lvm",
            Ok(pb::BaseSnapshotSource::Ebs) => "ebs",
            Ok(pb::BaseSnapshotSource::Portable) => "portable",
            _ => "unspecified",
        }
        .to_owned();
        let files = lease.files.len();
        let ttl = lease.lease_ttl_secs;
        self.lease = Some(lease.lease_id);
        Ok((source, files, ttl, direct && lease.direct_path_available))
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

    fn wait_snapshot_count(&mut self, expected: usize, timeout: Duration) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        loop {
            let count = self.vm_mut()?.snapshot_count()?;
            if count == expected {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "snapshot count stayed at {count}, expected {expected}\n{}",
                    self.vm()?.diagnostics()
                ));
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    fn get(&mut self, key: u64) -> Result<Vec<u64>, String> {
        let output = self.vm_mut()?.command(&format!(
            "yesno --endpoint http://127.0.0.1:50051 get {key} 2>/dev/null"
        ))?;
        output
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                line.trim()
                    .parse()
                    .map_err(|error| format!("yesno get returned an invalid ordinal: {error}"))
            })
            .collect()
    }

    fn start_archive(&mut self, root: &Path) -> Result<(), String> {
        if self.archive_running {
            return Err("yesno-archive is already running".into());
        }
        let backend = self.vm()?.backend;
        if self.s3.is_none() {
            self.s3 = Some(S3Fixture::start(root, backend)?);
        }
        let port = self.s3.as_ref().expect("S3 fixture was installed").port;
        let service = shell_quote(&archive_service(port));
        self.vm_mut()?.command(&format!(
            "install -d -m 0755 /var/lib/yesno-archive && printf %s {service} > /etc/systemd/system/yesno-archive-e2e.service && systemctl daemon-reload && systemctl reset-failed yesno-archive-e2e.service >/dev/null 2>&1 || true"
        ))?;
        if let Err(error) = self
            .vm_mut()?
            .command("systemctl start yesno-archive-e2e.service && systemctl is-active --quiet yesno-archive-e2e.service")
        {
            return Err(format!("{error}\n{}", self.archive_diagnostics()));
        }
        self.archive_running = true;
        Ok(())
    }

    fn archive_state(&mut self) -> Result<Option<ArchiveStats>, String> {
        let encoded = self.vm_mut()?.command(
            "if test -s /var/lib/yesno-archive/state.pb; then base64 -w0 /var/lib/yesno-archive/state.pb; fi",
        )?;
        if encoded.is_empty() {
            return Ok(None);
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded.trim())
            .map_err(|error| format!("cannot decode archive state copied from guest: {error}"))?;
        let state = yesno_server_utils::archive::pb::ArchiveState::decode(bytes.as_slice())
            .map_err(|error| format!("cannot decode archive state protobuf: {error}"))?;
        yesno_server_utils::archive::validate_state(&state)
            .map_err(|error| format!("archive state is invalid: {error}"))?;
        let cursor_total = state
            .wal_cursors
            .iter()
            .try_fold(0_u64, |total, cursor| {
                total.checked_add(cursor.archived_lsn)
            })
            .ok_or("archive cursor total overflowed")?;
        Ok(Some(ArchiveStats {
            base_generation: state.base_generation,
            event_sequence: state.event_sequence,
            term: state.term,
            cursor_total,
            base_ready: !state.latest_base_manifest.is_empty(),
        }))
    }

    fn archive_diagnostics(&mut self) -> String {
        let guest = self
            .vm_mut()
            .and_then(|vm| {
                vm.command(
                    "systemctl status yesno-archive-e2e.service --no-pager -l || true; journalctl -u yesno-archive-e2e.service --no-pager -n 100 || true; systemctl status yesno-snapshot-agent.service --no-pager -l || true; journalctl -u yesno-snapshot-agent.service --no-pager -n 100 || true; journalctl -u yesnod.service --no-pager -n 100 || true",
                )
            })
            .unwrap_or_else(|error| format!("cannot read guest archive diagnostics: {error}"));
        let s3 = self.s3.as_ref().map_or_else(
            || "Winterbaume was not started".to_owned(),
            S3Fixture::diagnostics,
        );
        format!("guest archive diagnostics:\n{guest}\n{s3}")
    }

    fn wait_archive(
        &mut self,
        metric: &str,
        expected: u64,
        timeout: Duration,
    ) -> Result<ArchiveStats, String> {
        if !matches!(
            metric,
            "base_generation" | "event_sequence" | "cursor_total"
        ) {
            return Err(format!("unknown archive metric '{metric}'"));
        }
        let deadline = Instant::now() + timeout;
        let mut last = None;
        loop {
            if let Some(stats) = self.archive_state()? {
                let observed = stats.metric(metric).expect("metric was validated");
                if observed >= expected {
                    return Ok(stats);
                }
                last = Some(stats);
            }
            if self
                .vm_mut()?
                .command("systemctl is-active --quiet yesno-archive-e2e.service")
                .is_err()
            {
                self.archive_running = false;
                return Err(format!(
                    "yesno-archive stopped before {metric} reached {expected}; last state: {last:?}\n{}",
                    self.archive_diagnostics()
                ));
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "timed out waiting for archive {metric} >= {expected}; last state: {last:?}\n{}",
                    self.archive_diagnostics()
                ));
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    fn stop_archive(&mut self) -> Result<(), String> {
        if !self.archive_running {
            return Err("yesno-archive is not running".into());
        }
        self.vm_mut()?
            .command("systemctl stop yesno-archive-e2e.service")?;
        self.archive_running = false;
        Ok(())
    }

    fn restore_archive(&mut self) -> Result<RestoreStats, String> {
        if self.archive_running {
            return Err("stop yesno-archive before restoring its archive".into());
        }
        let backend = self.vm()?.backend;
        let port = self
            .s3
            .as_ref()
            .ok_or("Winterbaume has not been started")?
            .port;
        self.vm_mut()?.stop_server()?;
        let output = self.vm_mut()?.command(&format!(
            "{} yesnoctl restore --store s3://{S3_BUCKET}/{S3_PREFIX} --target {}",
            s3_environment(port),
            shell_quote(backend.restore_dir())
        ))?;
        let stats = parse_restore_stats(&output)?;
        // yesnoctl restores as an operator, so hand the tree to the daemon
        // account before the unprivileged unit is asked to open it.
        self.vm_mut()?.command(&format!(
            "chown -R {DAEMON_USER}:{DAEMON_USER} {}",
            shell_quote(backend.restore_dir())
        ))?;
        let config = shell_quote(&restored_server_config(backend));
        self.vm_mut()?.command(&format!(
            "printf %s {config} > /etc/yesno/yesnod.toml && systemctl start yesnod.service"
        ))?;
        self.wait_server(SERVER_WAIT)?;
        Ok(stats)
    }

    /// Read the mount-namespace topology out of the guest.
    ///
    /// `isolated` is the assertion that keeps the rest honest. A daemon
    /// sharing the host's namespace sees every snapshot mount trivially, so
    /// without this the propagation assertions would pass on a deployment that
    /// proves nothing about propagation at all.
    fn mount_isolation(&mut self) -> Result<MountIsolation, String> {
        // One shell round trip: three namespace links and two mount counts,
        // each counted from inside the namespace it describes.
        let output = self.vm_mut()?.command(
            r#"dpid=$(systemctl show -p MainPID --value yesnod.service); apid=$(systemctl show -p MainPID --value yesno-snapshot-agent.service); count() { nsenter -t "$1" -m -- findmnt -rno TARGET | awk '/^\/mnt\/yesno\/snapshots\//{n++} END{print n+0}'; }; printf 'daemon_ns=%s agent_ns=%s host_ns=%s daemon_mounts=%s host_mounts=%s\n' "$(readlink /proc/$dpid/ns/mnt)" "$(readlink /proc/$apid/ns/mnt)" "$(readlink /proc/1/ns/mnt)" "$(count $dpid)" "$(count 1)""#,
        )?;
        let field = |name: &str| -> Result<String, String> {
            let prefix = format!("{name}=");
            output
                .split_whitespace()
                .find_map(|token| token.strip_prefix(&prefix))
                .map(ToOwned::to_owned)
                .ok_or_else(|| format!("guest mount report omitted {name}: {output}"))
        };
        let count = |name: &str| -> Result<usize, String> {
            field(name)?
                .parse()
                .map_err(|error| format!("guest mount report has an invalid {name}: {error}"))
        };
        let daemon_ns = field("daemon_ns")?;
        let host_ns = field("host_ns")?;
        Ok(MountIsolation {
            isolated: daemon_ns != host_ns,
            agent_ns: field("agent_ns")?,
            daemon_mounts: count("daemon_mounts")?,
            host_mounts: count("host_mounts")?,
            daemon_ns,
            host_ns,
        })
    }

    fn cleanup(&mut self) -> Result<(), String> {
        if self.archive_running {
            self.stop_archive()?;
        }
        let Some(mut vm) = self.vm.take() else {
            return Err("no filesystem guest; call fs_prepare() first".into());
        };
        let stopped = vm.stop_server();
        drop(vm);
        self.s3.take();
        self.lease = None;
        self.archive_running = false;
        stopped
    }
}

impl World {
    pub(crate) fn call_filesystems(
        &mut self,
        verb: &str,
        a: &Args<'_>,
    ) -> Result<MontyObject, MontyException> {
        match verb {
            "fs_prepare" => {
                a.exact(1)?;
                a.kw_allowed(&["isolate_mounts"])?;
                let isolate_mounts = a.kw_bool("isolate_mounts", false)?;
                let backend = Backend::parse(a.str_at(0)?)?;
                self.filesystems
                    .prepare(&self.root, backend, isolate_mounts)
                    .map_err(|error| fs_err(verb, error))?;
                Ok(MontyObject::String(backend.name().to_owned()))
            }
            "fs_mount_isolation" => {
                a.exact(0)?;
                a.no_kwargs()?;
                let isolation = self
                    .filesystems
                    .mount_isolation()
                    .map_err(|error| fs_err(verb, error))?;
                Ok(dict(vec![
                    ("daemon_ns", MontyObject::String(isolation.daemon_ns)),
                    ("agent_ns", MontyObject::String(isolation.agent_ns)),
                    ("host_ns", MontyObject::String(isolation.host_ns)),
                    ("isolated", MontyObject::Bool(isolation.isolated)),
                    ("daemon_mounts", whole_obj(isolation.daemon_mounts)),
                    ("host_mounts", whole_obj(isolation.host_mounts)),
                ]))
            }
            "fs_put" => {
                a.exact(2)?;
                a.no_kwargs()?;
                let key = a.u64(0)?;
                let ordinals = a.u64_list(1)?;
                let input = ordinals
                    .iter()
                    .map(|ordinal| format!("{key},{ordinal}\n"))
                    .collect::<String>();
                let command = format!(
                    "printf %s {} | yesno --endpoint http://127.0.0.1:50051 put -",
                    shell_quote(&input)
                );
                self.filesystems
                    .vm_mut()
                    .and_then(|vm| vm.command(&command))
                    .map_err(|error| fs_err(verb, error))?;
                Ok(whole_obj(ordinals.len()))
            }
            "fs_get" => {
                a.exact(1)?;
                a.no_kwargs()?;
                self.filesystems
                    .get(a.u64(0)?)
                    .map(|values| MontyObject::List(values.into_iter().map(int_obj).collect()))
                    .map_err(|error| fs_err(verb, error))
            }
            "fs_checkpoint" => {
                a.exact(0)?;
                a.no_kwargs()?;
                self.filesystems
                    .checkpoint()
                    .map(int_obj)
                    .map_err(|error| fs_err(verb, error))
            }
            "fs_snapshot_begin" => {
                a.exact(0)?;
                a.no_kwargs()?;
                let (source, files, ttl, direct) = self
                    .filesystems
                    .begin_snapshot()
                    .map_err(|error| fs_err(verb, error))?;
                Ok(dict(vec![
                    ("source", MontyObject::String(source)),
                    ("files", whole_obj(files)),
                    ("ttl", int_obj(ttl)),
                    ("direct", MontyObject::Bool(direct)),
                ]))
            }
            "fs_snapshot_count" => {
                a.exact(0)?;
                a.no_kwargs()?;
                self.filesystems
                    .vm_mut()
                    .and_then(Vm::snapshot_count)
                    .map(whole_obj)
                    .map_err(|error| fs_err(verb, error))
            }
            "fs_snapshot_release" => {
                a.exact(0)?;
                a.no_kwargs()?;
                self.filesystems
                    .release_snapshot()
                    .map_err(|error| fs_err(verb, error))?;
                Ok(MontyObject::None)
            }
            "fs_basebackup" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let target = self.scenario_path(a.str_at(0)?, verb)?;
                let report = self
                    .filesystems
                    .basebackup(target)
                    .map_err(|error| fs_err(verb, error))?;
                Ok(dict(vec![
                    ("shards", int_obj(report.shards as u64)),
                    ("checkpoint", int_obj(report.checkpoint_version)),
                    ("recovered", int_obj(report.recovered_version)),
                    ("bytes", int_obj(report.bytes)),
                ]))
            }
            "fs_archive_start" => {
                a.exact(0)?;
                a.no_kwargs()?;
                self.filesystems
                    .start_archive(&self.root)
                    .map_err(|error| fs_err(verb, error))?;
                Ok(MontyObject::String("winterbaume".to_owned()))
            }
            "fs_archive_wait" => {
                a.exact(3)?;
                a.no_kwargs()?;
                let metric = a.str_at(0)?;
                let expected = a.u64(1)?;
                let timeout = Duration::from_millis(a.u64(2)?);
                self.filesystems
                    .wait_archive(metric, expected, timeout)
                    .map(archive_stats_object)
                    .map_err(|error| fs_err(verb, error))
            }
            "fs_archive_stop" => {
                a.exact(0)?;
                a.no_kwargs()?;
                self.filesystems
                    .stop_archive()
                    .map_err(|error| fs_err(verb, error))?;
                Ok(MontyObject::Bool(true))
            }
            "fs_archive_restore" => {
                a.exact(0)?;
                a.no_kwargs()?;
                self.filesystems
                    .restore_archive()
                    .map(restore_stats_object)
                    .map_err(|error| fs_err(verb, error))
            }
            "fs_crash_server" => {
                a.exact(0)?;
                a.no_kwargs()?;
                self.filesystems
                    .vm_mut()
                    .and_then(|vm| vm.command("systemctl kill --kill-who=main --signal=KILL yesnod.service && until systemctl is-failed --quiet yesnod.service; do sleep 0.1; done"))
                    .map_err(|error| fs_err(verb, error))?;
                self.filesystems.lease = None;
                Ok(MontyObject::None)
            }
            "fs_restart_server" => {
                a.exact(0)?;
                a.no_kwargs()?;
                self.filesystems
                    .vm_mut()
                    .and_then(|vm| vm.command("systemctl reset-failed yesnod.service && systemctl start yesnod.service"))
                    .map_err(|error| fs_err(verb, error))?;
                self.filesystems
                    .wait_server(SERVER_WAIT)
                    .map_err(|error| fs_err(verb, error))?;
                Ok(MontyObject::None)
            }
            "fs_snapshot_wait" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let expected = usize::try_from(a.u64(0)?).map_err(|_| {
                    value_err(format!("{verb}(): snapshot count does not fit usize"))
                })?;
                self.filesystems
                    .wait_snapshot_count(expected, SNAPSHOT_WAIT)
                    .map_err(|error| fs_err(verb, error))?;
                Ok(whole_obj(expected))
            }
            "fs_cleanup" => {
                a.exact(0)?;
                a.no_kwargs()?;
                self.filesystems
                    .cleanup()
                    .map_err(|error| fs_err(verb, error))?;
                Ok(MontyObject::Bool(true))
            }
            _ => Err(value_err(format!("{verb}() is not a filesystem verb"))),
        }
    }
}
