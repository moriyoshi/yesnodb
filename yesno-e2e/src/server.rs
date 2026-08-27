//! `srv_*`: the shipped `yesnod` daemon, started in-process.
//!
//! # What this adds over the `flight_*` verbs
//!
//! `flight_serve` stands a bare `YesnoFlightService` over a database the
//! scenario already opened. That tests the **protocol**. It does not test the
//! thing an operator actually runs, which owns the database's lifetime and has
//! to give it back: open, drive a checkpoint on a timer, drain readers, take a
//! final checkpoint, and release the exclusive lock so the next start works.
//!
//! That last step is the one worth scripting, because `yesno-core` has no
//! `Db::close()` — teardown is `Drop` on the last `Arc<DbInner>`, and a handle
//! leaked anywhere leaves the directory locked with nothing saying so. The
//! daemon reports what it observed and `srv_stop` hands that to the scenario,
//! so a scenario can assert on the teardown rather than on a sleep.
//!
//! # Why a scenario rather than another Rust test
//!
//! `yesno-server/tests/` already pins the fixed shapes — one start, one stop,
//! one leak. What is expensive there and free here is *varying the sequence*:
//! restart, ingest between restarts, stop with a reader open, run with the
//! checkpoint driver at one second versus at a minute. Each of those is a
//! recompile in Rust and a line here.
//!
//! These verbs deliberately do not reach the daemon's config **file**. They
//! build a `Config` value directly, because a scenario asserting on TOML parsing
//! would be testing `serde` — the config format has unit tests where it lives.

use std::path::{Path, PathBuf};

use monty_types::{MontyException, MontyObject};
use prost::Message;
use tonic::metadata::AsciiMetadataValue;
use tonic::transport::Channel;
use tonic::Request;

use crate::convert::{db_err, dict, int_obj, opt_int_obj, value_err, whole_obj, Args};
use crate::world::{stale_handle, HandleKind, World};

/// The verbs this module dispatches. Merged into [`crate::world::all_names`].
pub const OWNS: &[&str] = &[
    "srv_start",
    "srv_config",
    "srv_principal",
    "srv_anonymous",
    "srv_control_admin",
    "srv_replication",
    "srv_archive_access",
    "srv_archive_retention",
    "srv_follows",
    "srv_serve_reads",
    "srv_launch",
    "srv_flight",
    "srv_flight_as",
    "srv_metrics",
    "srv_follower_state",
    "srv_term",
    "srv_follower_wait",
    "srv_promote",
    "srv_checkpoint",
    "srv_checkpoint_as",
    "srv_basebackup",
    "srv_restore",
    "srv_restore_at",
    "srv_restore_windows",
    "srv_archive_gc",
    "srv_archive_second_error",
    "srv_archive_start",
    "srv_archive_wait_unactivated",
    "srv_archive_wait",
    "srv_archive_stop",
    "srv_stop",
];

/// A daemon, in whichever role it is currently running.
///
/// The two are genuinely different objects rather than one with a flag: a
/// leader owns a `Db` and three listeners, a standby owns a loop and no
/// database at all. `srv_promote` is the edge between them, and it is the same
/// edge `main.rs` walks.
enum Node {
    Leader(Box<yesno_server::Running>),
    Standby(yesno_server::follower::FollowerNode),
}

struct Server {
    node: Option<Node>,
    /// A read-serving standby's Flight listener.
    reads: Option<yesno_server::lifecycle::ReadListener>,
    /// Kept so `srv_promote` can restart this node in the other role.
    cfg: yesno_server::Config,
    flight_addr: Option<std::net::SocketAddr>,
    control_addr: Option<std::net::SocketAddr>,
    control_socket: Option<PathBuf>,
    metrics_addr: Option<std::net::SocketAddr>,
}

#[derive(Default)]
pub struct ServerState {
    servers: Vec<Option<Server>>,
    /// Configurations under construction. A builder rather than a pile of
    /// keyword arguments because principals are a *list*, and the harness's
    /// argument conversion carries whole numbers and strings, not records.
    configs: Vec<Option<yesno_server::Config>>,
    archives: Vec<Option<ArchiveSidecar>>,
}

struct ArchiveSidecar {
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<Result<(), yesno_server_utils::archive::ArchiveError>>>,
    object_dir: PathBuf,
}

/// Also read by `aws.rs`: the deferred gate's published base is inspected
/// through this same reader, so the two cannot disagree about what "the
/// archive has a base" means.
pub(crate) struct ArchiveStats {
    pub(crate) base_generation: u64,
    pub(crate) event_sequence: u64,
    term: u32,
    pub(crate) base_files: usize,
    pub(crate) wal_objects: usize,
    pub(crate) wal_bytes: u64,
    cursor_total: u64,
}

impl ArchiveStats {
    fn metric(&self, name: &str) -> Option<u64> {
        match name {
            "base_generation" => Some(self.base_generation),
            "event_sequence" => Some(self.event_sequence),
            "wal_objects" => Some(self.wal_objects as u64),
            "wal_bytes" => Some(self.wal_bytes),
            "cursor_total" => Some(self.cursor_total),
            _ => None,
        }
    }

    fn object(&self) -> MontyObject {
        dict(vec![
            ("base_generation", int_obj(self.base_generation)),
            ("event_sequence", int_obj(self.event_sequence)),
            ("term", int_obj(self.term as u64)),
            ("base_files", whole_obj(self.base_files)),
            ("wal_objects", whole_obj(self.wal_objects)),
            ("wal_bytes", int_obj(self.wal_bytes)),
            ("cursor_total", int_obj(self.cursor_total)),
        ])
    }
}

fn files_below(path: &Path, output: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            files_below(&entry.path(), output)?;
        } else {
            output.push(entry.path());
        }
    }
    Ok(())
}

pub(crate) fn inspect_archive(object_dir: &Path) -> Result<ArchiveStats, String> {
    use yesno_server_utils::archive::pb::BaseManifest;

    let state = read_archive_state(object_dir)?;
    if state.latest_base_manifest.is_empty() {
        return Err("archive state has no completed base manifest".to_owned());
    }
    let manifest_path = object_dir.join(&state.latest_base_manifest);
    let manifest_bytes = std::fs::read(&manifest_path).map_err(|error| {
        format!(
            "base manifest '{}' is not readable: {error}",
            manifest_path.display()
        )
    })?;
    let manifest = BaseManifest::decode(manifest_bytes.as_slice())
        .map_err(|error| format!("base manifest is not valid Protobuf: {error}"))?;
    for file in &manifest.files {
        let path = object_dir.join(&file.object_key);
        let size = std::fs::metadata(&path)
            .map_err(|error| format!("base object '{}' is not readable: {error}", path.display()))?
            .len();
        if size != file.size {
            return Err(format!(
                "base object '{}' is {size} bytes, manifest says {}",
                path.display(),
                file.size
            ));
        }
    }

    let mut files = Vec::new();
    files_below(object_dir, &mut files)
        .map_err(|error| format!("archive objects are not readable: {error}"))?;
    let mut wal_objects = 0usize;
    let mut wal_bytes = 0u64;
    for path in files {
        if path.extension().is_none_or(|extension| extension != "wal")
            || !path
                .components()
                .any(|component| component.as_os_str() == "wal")
        {
            continue;
        }
        let name = path
            .file_stem()
            .and_then(|name| name.to_str())
            .ok_or_else(|| format!("WAL object '{}' has no UTF-8 stem", path.display()))?;
        let (first, last) = name
            .split_once('-')
            .ok_or_else(|| format!("WAL object '{}' has no LSN range", path.display()))?;
        let first = first.parse::<u64>().map_err(|error| {
            format!(
                "WAL object '{}' has a bad first LSN: {error}",
                path.display()
            )
        })?;
        let last = last.parse::<u64>().map_err(|error| {
            format!(
                "WAL object '{}' has a bad last LSN: {error}",
                path.display()
            )
        })?;
        let size = std::fs::metadata(&path)
            .map_err(|error| format!("WAL object '{}' is not readable: {error}", path.display()))?
            .len();
        if last.checked_sub(first) != Some(size) {
            return Err(format!(
                "WAL object '{}' covers {first}..{last} but is {size} bytes",
                path.display()
            ));
        }
        wal_objects += 1;
        wal_bytes = wal_bytes.saturating_add(size);
    }

    Ok(ArchiveStats {
        base_generation: state.base_generation,
        event_sequence: state.event_sequence,
        term: state.term,
        base_files: manifest.files.len(),
        wal_objects,
        wal_bytes,
        cursor_total: state.wal_cursors.iter().fold(0u64, |total, cursor| {
            total.saturating_add(cursor.archived_lsn)
        }),
    })
}

fn read_archive_state(
    object_dir: &Path,
) -> Result<yesno_server_utils::archive::pb::ArchiveState, String> {
    use yesno_server_utils::archive::pb::ArchiveState;

    let state_bytes = std::fs::read(object_dir.join("state.pb"))
        .map_err(|error| format!("archive state is not readable: {error}"))?;
    ArchiveState::decode(state_bytes.as_slice())
        .map_err(|error| format!("archive state is not valid Protobuf: {error}"))
}

/// A one-shot HTTP/1.0 GET, because the daemon's metrics listener is plain HTTP
/// and this harness has no HTTP client. Adding one to read a few lines of
/// text would be a dependency earning nothing.
async fn http_get(addr: std::net::SocketAddr, path: &str) -> std::io::Result<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = tokio::net::TcpStream::connect(addr).await?;
    s.write_all(format!("GET {path} HTTP/1.0\r\nHost: localhost\r\n\r\n").as_bytes())
        .await?;
    let mut buf = String::new();
    s.read_to_string(&mut buf).await?;
    let (head, body) = buf
        .split_once("\r\n\r\n")
        .ok_or_else(|| std::io::Error::other("no header terminator in the response"))?;
    let status = head.lines().next().unwrap_or_default();
    if !status.contains(" 200 ") {
        return Err(std::io::Error::other(format!("{path} answered: {status}")));
    }
    Ok(body.to_owned())
}

fn enable_control_admin(cfg: &mut yesno_server::Config, principal: &str) {
    cfg.server.control.listen = "127.0.0.1:0".into();
    cfg.server.control.journal_dir = cfg.data_dir().with_extension("control");
    cfg.auth.rules.push(yesno_server::config::AuthzRule {
        channel: yesno_server::config::AuthzChannel::Host,
        principal: principal.into(),
        address: "127.0.0.0/8".into(),
        capability: yesno_server::config::EndpointCapability::ControlAdmin,
        action: yesno_server::config::RuleAction::Allow,
    });
}

/// Start whatever `cfg` describes, and record where it landed.
fn launch(
    rt: &tokio::runtime::Handle,
    cfg: yesno_server::Config,
    verb: &str,
) -> Result<Server, MontyException> {
    cfg.validate_with(false, true)
        .map_err(|e| value_err(format!("{verb}(): {e}")))?;

    match cfg.server.role {
        yesno_server::config::Role::Follower => {
            // Inside `block_on`, because `follower::start` spawns and a
            // `tokio::spawn` outside a runtime context panics rather than
            // returning an error. The daemon and the Rust tests never hit this —
            // both are already inside a runtime — so the harness is the only
            // caller that has to say so.
            let node = rt
                .block_on(async { yesno_server::follower::start(&cfg) })
                .map_err(|e| value_err(format!("{verb}(): {e}")))?;

            // A **cold** standby has no Flight listener at all — it serves
            // nothing — so a scenario asking for one gets told rather than a
            // handle that fails later somewhere less obvious. A read-serving one
            // does have a listener, bound once and kept across a rebuild.
            let (reads, flight_addr) = match node.db.clone() {
                None => (None, None),
                Some(slot) => {
                    let l = rt
                        .block_on(yesno_server::lifecycle::serve_reads(&cfg, slot))
                        .map_err(|e| db_err(verb, e))?;
                    let addr = l.addr;
                    (Some(l), Some(addr))
                }
            };
            Ok(Server {
                node: Some(Node::Standby(node)),
                reads,
                cfg,
                flight_addr,
                control_addr: None,
                control_socket: None,
                metrics_addr: None,
            })
        }
        yesno_server::config::Role::Leader => {
            let running = rt
                .block_on(yesno_server::start(&cfg))
                .map_err(|e| db_err(verb, e))?;
            let flight_addr = Some(running.flight_addr);
            let control_addr = running.control_addr;
            let control_socket = running.control_socket.clone();
            let metrics_addr = running.metrics_addr;
            Ok(Server {
                node: Some(Node::Leader(Box::new(running))),
                reads: None,
                cfg,
                flight_addr,
                control_addr,
                control_socket,
                metrics_addr,
            })
        }
    }
}

impl World {
    fn cfg_mut(
        &mut self,
        h: usize,
        verb: &str,
    ) -> Result<&mut yesno_server::Config, MontyException> {
        let i = self.slot(h, HandleKind::ServerConfig, verb)?;
        self.server_state.configs[i]
            .as_mut()
            .ok_or_else(|| stale_handle(verb, "server config", h))
    }

    /// The shape every `srv_*` node starts from.
    fn base_config(
        &mut self,
        name: &str,
        shards: usize,
        interval_secs: u64,
        verb: &str,
    ) -> Result<yesno_server::Config, MontyException> {
        let dir = self.scenario_path(name, verb)?;
        let mut cfg = yesno_server::Config::default();
        cfg.server.data_dir = Some(dir);
        // Port 0 everywhere: the OS chooses, and the scenario is told what it
        // chose. Fixed ports would make two scenarios running concurrently
        // collide on something unrelated to either.
        cfg.server.flight.listen = "127.0.0.1:0".into();
        cfg.server.metrics.listen = "127.0.0.1:0".into();
        cfg.server.shutdown_grace_secs = 20;
        cfg.db.shards = shards;
        cfg.db.checkpoint.interval_secs = interval_secs;
        Ok(cfg)
    }

    fn server(&mut self, h: usize, verb: &str) -> Result<&mut Server, MontyException> {
        let i = self.slot(h, HandleKind::Server, verb)?;
        self.server_state.servers[i]
            .as_mut()
            .ok_or_else(|| stale_handle(verb, "server", h))
    }

    pub(crate) fn call_server(
        &mut self,
        verb: &str,
        a: &Args<'_>,
    ) -> Result<MontyObject, MontyException> {
        match verb {
            // Start a daemon over a directory in the scenario's temporary root.
            //
            // It opens its **own** `Db` and takes the directory's exclusive
            // lock, so a scenario must not also `db_open` the same name while it
            // runs. That is the design, not a harness limitation, and a scenario
            // that tries gets the same `AlreadyOpen` an operator would.
            "srv_start" => {
                a.exact(1)?;
                a.kw_allowed(&["shards", "interval_secs"])?;
                let name = a.str_at(0)?.to_owned();
                let shards = a.kw_usize_min("shards", 2, 1)?;
                let interval_secs = a.kw_usize_min("interval_secs", 60, 1)? as u64;
                let mut cfg = self.base_config(&name, shards, interval_secs, verb)?;
                enable_control_admin(&mut cfg, "all");
                let rt = self.rt_handle(verb)?;
                let srv = launch(&rt, cfg, verb)?;
                self.server_state.servers.push(Some(srv));
                let idx = self.server_state.servers.len() - 1;
                Ok(self.mint(HandleKind::Server, idx))
            }

            // A configuration to build up before starting. Everything the
            // simple `srv_start` sets, plus the parts that are lists.
            "srv_config" => {
                a.exact(1)?;
                a.kw_allowed(&["shards", "interval_secs"])?;
                let name = a.str_at(0)?.to_owned();
                let shards = a.kw_usize_min("shards", 2, 1)?;
                let interval_secs = a.kw_usize_min("interval_secs", 60, 1)? as u64;
                let cfg = self.base_config(&name, shards, interval_secs, verb)?;
                self.server_state.configs.push(Some(cfg));
                let idx = self.server_state.configs.len() - 1;
                Ok(self.mint(HandleKind::ServerConfig, idx))
            }

            // The **token** goes in, not its digest. The harness hashes it
            // with the shipped `token_digest`, which is the same function the
            // documented `printf ... | sha256sum` recipe has to agree with — so
            // a scenario cannot accidentally pin a digest this build does not
            // actually compute.
            "srv_principal" => {
                a.exact(4)?;
                a.no_kwargs()?;
                let h = a.handle(0)?;
                let name = a.str_at(1)?.to_owned();
                let role = a.str_at(2)?.to_owned();
                let token = a.str_at(3)?.to_owned();
                let role = match role.as_str() {
                    "reader" => yesno_server::config::PrincipalRole::Reader,
                    "writer" => yesno_server::config::PrincipalRole::Writer,
                    "admin" => yesno_server::config::PrincipalRole::Admin,
                    "replica" => yesno_server::config::PrincipalRole::Replica,
                    other => {
                        return Err(value_err(format!(
                        "{verb}(): '{other}' is not a role; use reader, writer, admin or replica"
                    )))
                    }
                };
                let cfg = self.cfg_mut(h, verb)?;
                cfg.auth
                    .principals
                    .push(yesno_server::config::PrincipalConfig {
                        name,
                        role,
                        token_sha256: Some(yesno_server::auth::token_digest(&token)),
                        cert_sha256: None,
                    });
                Ok(MontyObject::None)
            }

            "srv_anonymous" => {
                a.exact(2)?;
                a.no_kwargs()?;
                let h = a.handle(0)?;
                let mode = a.str_at(1)?.to_owned();
                let anon = match mode.as_str() {
                    "none" => yesno_server::config::Anonymous::None,
                    "read" => yesno_server::config::Anonymous::Read,
                    other => {
                        return Err(value_err(format!(
                            "{verb}(): '{other}' is not an anonymous mode; use none or read"
                        )))
                    }
                };
                self.cfg_mut(h, verb)?.auth.anonymous = anon;
                Ok(MontyObject::None)
            }

            // Grant one named principal the typed administrative control
            // capability and make the shared listener available. Other
            // principals remain default-denied by the ordered rule table.
            "srv_control_admin" => {
                a.exact(2)?;
                a.no_kwargs()?;
                let h = a.handle(0)?;
                let principal = a.str_at(1)?.to_owned();
                let cfg = self.cfg_mut(h, verb)?;
                enable_control_admin(cfg, &principal);
                Ok(MontyObject::None)
            }

            // Serve WAL shipping on the shared control endpoint. Plaintext on
            // loopback: the transport and rule matcher have their own tests,
            // and what a scenario scripts is the *sequence*.
            "srv_replication" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let h = a.handle(0)?;
                let cfg = self.cfg_mut(h, verb)?;
                cfg.server.control.listen = "127.0.0.1:0".into();
                cfg.server.control.journal_dir = cfg.data_dir().with_extension("control");
                cfg.auth.rules.push(yesno_server::config::AuthzRule {
                    channel: yesno_server::config::AuthzChannel::Host,
                    principal: "all".into(),
                    address: "127.0.0.0/8".into(),
                    capability: yesno_server::config::EndpointCapability::Replication,
                    action: yesno_server::config::RuleAction::Allow,
                });
                Ok(MontyObject::None)
            }

            // The archive sidecar consumes lifecycle events and WAL through
            // the same shared control listener.
            "srv_archive_access" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let h = a.handle(0)?;
                let cfg = self.cfg_mut(h, verb)?;
                cfg.server.control.unix_socket =
                    Some(cfg.data_dir().with_extension("control.sock"));
                cfg.server.control.journal_dir = cfg.data_dir().with_extension("control");
                for capability in [
                    yesno_server::config::EndpointCapability::ControlRead,
                    yesno_server::config::EndpointCapability::Replication,
                ] {
                    cfg.auth.rules.push(yesno_server::config::AuthzRule {
                        channel: yesno_server::config::AuthzChannel::Local,
                        principal: "all".into(),
                        address: "127.0.0.0/8".into(),
                        capability,
                        action: yesno_server::config::RuleAction::Allow,
                    });
                }
                Ok(MontyObject::None)
            }

            // Keep only a small WAL window so the recovery scenario can force
            // a genuine archive gap without manufacturing invalid files.
            "srv_archive_retention" => {
                a.exact(2)?;
                a.no_kwargs()?;
                let h = a.handle(0)?;
                let bytes = a.u64(1)?;
                if bytes == 0 {
                    return Err(value_err(format!("{verb}(): bytes must be positive")));
                }
                let cfg = self.cfg_mut(h, verb)?;
                cfg.db.checkpoint.wal_bytes = yesno_server::config::Bytes(bytes);
                cfg.db.checkpoint.max_wal_bytes = yesno_server::config::Bytes(bytes);
                Ok(MontyObject::None)
            }

            // Turn this configuration into a standby of a running leader.
            "srv_follows" => {
                a.exact(2)?;
                a.no_kwargs()?;
                let (ch, lh) = (a.handle(0)?, a.handle(1)?);
                let leader = self.server(lh, verb)?.control_addr.ok_or_else(|| {
                    value_err(format!(
                        "{verb}(): that leader is not serving replication; call \
                         srv_replication() on its config before starting it"
                    ))
                })?;
                let cfg = self.cfg_mut(ch, verb)?;
                cfg.server.role = yesno_server::config::Role::Follower;
                cfg.follower.leader = format!("http://{leader}");
                cfg.follower.poll_interval_secs = 1;
                cfg.follower.max_backoff_secs = 1;
                Ok(MontyObject::None)
            }

            // Make this standby serve reads while it follows.
            //
            // It then holds the database **open** and applies shipped frames
            // into it, rather than writing files behind a closed one — which is
            // the whole difference between a cold standby and a live replica.
            "srv_serve_reads" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let h = a.handle(0)?;
                let cfg = self.cfg_mut(h, verb)?;
                cfg.follower.serve_reads = true;
                cfg.server.flight.listen = "127.0.0.1:0".into();
                Ok(MontyObject::None)
            }

            "srv_launch" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let h = a.handle(0)?;
                let i = self.slot(h, HandleKind::ServerConfig, verb)?;
                let cfg = self.server_state.configs[i]
                    .take()
                    .ok_or_else(|| stale_handle(verb, "server config", h))?;
                let rt = self.rt_handle(verb)?;
                let srv = launch(&rt, cfg, verb)?;
                self.server_state.servers.push(Some(srv));
                let idx = self.server_state.servers.len() - 1;
                Ok(self.mint(HandleKind::Server, idx))
            }

            // A Flight handle against the daemon's port, so every `flight_*`
            // verb works unchanged against a real `yesnod`. That is the point
            // of the split: the same assertions run against the bare service and
            // against the daemon, and a difference between them is a finding.
            "srv_flight" | "srv_flight_as" => {
                let token = if verb == "srv_flight_as" {
                    a.exact(2)?;
                    Some(a.str_at(1)?.to_owned())
                } else {
                    a.exact(1)?;
                    None
                };
                a.no_kwargs()?;
                let h = a.handle(0)?;
                let addr = self.server(h, verb)?.flight_addr.ok_or_else(|| {
                    value_err(format!(
                        "{verb}(): this node is a standby and serves no queries — it opens no \
                         database at all. Promote it first."
                    ))
                })?;
                let rt = self.rt_handle(verb)?;
                let client = rt
                    .block_on(async move {
                        let ch = Channel::from_shared(format!("http://{addr}"))
                            .map_err(std::io::Error::other)?
                            .connect()
                            .await
                            .map_err(std::io::Error::other)?;
                        Ok::<_, std::io::Error>(
                            arrow_flight::flight_service_client::FlightServiceClient::new(ch),
                        )
                    })
                    .map_err(|e| db_err(verb, e))?;
                Ok(self.adopt_flight_endpoint(client, token))
            }

            // The Prometheus text, as a string. Left unparsed on purpose: a
            // scenario asserting `"yesnod_shards 2" in text` is asserting on the
            // exposition format a scraper actually consumes, where a parsed dict
            // would hide a malformed one.
            "srv_metrics" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let h = a.handle(0)?;
                let addr = self.server(h, verb)?.metrics_addr.ok_or_else(|| {
                    value_err(format!("{verb}(): this server has no metrics listener"))
                })?;
                let rt = self.rt_handle(verb)?;
                let body = rt
                    .block_on(http_get(addr, "/metrics"))
                    .map_err(|e| db_err(verb, e))?;
                Ok(MontyObject::String(body))
            }

            // What the standby's loop is doing. `halted` is the one to assert
            // on: a standby that has stopped is not a standby, and the whole
            // point of the taxonomy is that two of the failures must **not** be
            // repaired automatically.
            "srv_follower_state" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let h = a.handle(0)?;
                let srv = self.server(h, verb)?;
                let Some(Node::Standby(n)) = srv.node.as_ref() else {
                    return Err(value_err(format!("{verb}(): this node is not a standby")));
                };
                use std::sync::atomic::Ordering;
                let st = &n.status;
                let reason = st.halt_reason.lock().unwrap().clone();
                Ok(dict(vec![
                    (
                        "connected",
                        MontyObject::Bool(st.connected.load(Ordering::Relaxed)),
                    ),
                    (
                        "halted",
                        MontyObject::Bool(st.halted.load(Ordering::Relaxed)),
                    ),
                    ("records", int_obj(st.records.load(Ordering::Relaxed))),
                    ("passes", int_obj(st.passes.load(Ordering::Relaxed))),
                    (
                        "applied_bytes",
                        int_obj(st.applied_bytes.load(Ordering::Relaxed)),
                    ),
                    (
                        "rebootstraps",
                        int_obj(st.rebootstraps.load(Ordering::Relaxed)),
                    ),
                    (
                        "reason",
                        match reason {
                            Some(r) => MontyObject::String(r),
                            None => MontyObject::None,
                        },
                    ),
                ]))
            }

            // The leadership term this node's directory records.
            //
            // Read from the **MANIFEST**, not from a running `Db`, so a
            // scenario can ask it of a standby that has no database open — which
            // is the node whose term matters most, because it is the one a
            // failover is about to raise.
            "srv_term" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let h = a.handle(0)?;
                let dir = self.server(h, verb)?.cfg.data_dir().to_path_buf();
                let t =
                    yesno_core::database_term(&dir).map_err(|e| db_err(verb, format!("{e:?}")))?;
                Ok(int_obj(t as u64))
            }

            // Block until a counter reaches a value, or give up.
            //
            // **The waiting happens in the host, deliberately.** A poll loop
            // written in the scenario language spins hundreds of times a
            // millisecond — 412 iterations in 81 ms on the first draft of
            // `failover.py`, against a standby whose poll interval is a second —
            // so it is not a wait at all, it is a busy loop that gives up before
            // the subject has had a chance to act. It is also the same reason
            // TESTING §1 puts timing loops in the host.
            //
            // Raises on timeout rather than returning, so a scenario cannot
            // accidentally carry on against a standby that never caught up.
            "srv_follower_wait" => {
                a.exact(4)?;
                a.no_kwargs()?;
                let h = a.handle(0)?;
                let field = a.str_at(1)?.to_owned();
                let want = a.u64(2)?;
                let timeout_ms = a.u64(3)?;

                let deadline =
                    std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
                loop {
                    let (got, halted, reason) = {
                        let srv = self.server(h, verb)?;
                        let Some(Node::Standby(n)) = srv.node.as_ref() else {
                            return Err(value_err(format!("{verb}(): this node is not a standby")));
                        };
                        use std::sync::atomic::Ordering;
                        let st = &n.status;
                        let got = match field.as_str() {
                            "records" => st.records.load(Ordering::Relaxed),
                            "passes" => st.passes.load(Ordering::Relaxed),
                            "applied_bytes" => st.applied_bytes.load(Ordering::Relaxed),
                            "rebootstraps" => st.rebootstraps.load(Ordering::Relaxed),
                            other => {
                                return Err(value_err(format!(
                                    "{verb}(): '{other}' is not a counter; use records, passes, \
                                     applied_bytes or rebootstraps"
                                )))
                            }
                        };
                        (
                            got,
                            st.halted.load(Ordering::Relaxed),
                            st.halt_reason.lock().unwrap().clone(),
                        )
                    };
                    if got >= want {
                        return Ok(int_obj(got));
                    }
                    // A halted standby will never reach the target, and
                    // waiting out the timeout would report the wrong failure.
                    if halted {
                        return Err(value_err(format!(
                            "{verb}(): the standby halted while waiting for {field} >= {want}: {}",
                            reason.unwrap_or_else(|| "no reason recorded".into())
                        )));
                    }
                    if std::time::Instant::now() >= deadline {
                        return Err(value_err(format!(
                            "{verb}(): {field} reached {got}, not {want}, in {timeout_ms} ms"
                        )));
                    }
                    // `std::thread::sleep`, not `tokio::time::sleep`: the
                    // latter needs a reactor at *construction*, and this is a
                    // synchronous verb. Blocking this thread is harmless — the
                    // standby's loop runs on the runtime's worker threads, not
                    // on the interpreter's.
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
            }

            // The failover edge, and it is the same one `main.rs` walks: stop
            // following, then open the database as a leader.
            //
            // It works because a standby holds **no `Db`** — promoting is
            // `Db::open_with` on a directory nothing has open, and recovery
            // truncates any unresolved tail for free.
            "srv_promote" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let h = a.handle(0)?;
                let rt = self.rt_handle(verb)?;
                let i = self.slot(h, HandleKind::Server, verb)?;
                let srv = self.server_state.servers[i]
                    .as_mut()
                    .ok_or_else(|| stale_handle(verb, "server", h))?;
                let Some(Node::Standby(n)) = srv.node.take() else {
                    return Err(value_err(format!(
                        "{verb}(): this node is already a leader"
                    )));
                };
                if let Some(r) = srv.reads.take() {
                    rt.block_on(r.stop());
                }
                rt.block_on(n.stop());

                // **The same call `main.rs` makes**, and it must be: the term
                // is what distinguishes a promoted node from the leadership it
                // replaced, and a harness that reimplemented the edge would drift
                // from the daemon and then test something nobody runs. The first
                // version of this verb did exactly that and `failover.py` caught
                // it — a promotion that raised no term at all.
                let mut cfg = srv.cfg.clone();
                yesno_server::lifecycle::raise_term(cfg.data_dir()).map_err(|e| db_err(verb, e))?;
                cfg.server.role = yesno_server::config::Role::Leader;
                let promoted = launch(&rt, cfg, verb)?;
                let srv = self.server_state.servers[i].as_mut().expect("checked");
                *srv = promoted;
                Ok(MontyObject::None)
            }

            // Checkpoint is an administrative control-plane RPC, never a
            // Flight action. The authenticated form keeps that split under an
            // end-to-end authorization test as well as a protocol assertion.
            "srv_checkpoint" | "srv_checkpoint_as" => {
                let token = if verb == "srv_checkpoint_as" {
                    a.exact(2)?;
                    Some(a.str_at(1)?.to_owned())
                } else {
                    a.exact(1)?;
                    None
                };
                a.no_kwargs()?;
                let h = a.handle(0)?;
                let addr = self.server(h, verb)?.control_addr.ok_or_else(|| {
                    value_err(format!("{verb}(): this server has no control listener"))
                })?;
                let rt = self.rt_handle(verb)?;
                let watermark = rt
                    .block_on(async move {
                        let channel = Channel::from_shared(format!("http://{addr}"))?
                            .connect()
                            .await?;
                        let mut client = yesno_server::control::pb::control_plane_client::ControlPlaneClient::new(channel);
                        let mut request = Request::new(
                            yesno_server::control::pb::CheckpointRequest {},
                        );
                        if let Some(token) = token {
                            let value: AsciiMetadataValue =
                                format!("Bearer {token}").parse()?;
                            request.metadata_mut().insert("authorization", value);
                        }
                        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(
                            client.checkpoint(request).await?.into_inner().watermark,
                        )
                    })
                    .map_err(|error| db_err(verb, error))?;
                Ok(int_obj(watermark))
            }

            // Run the shipped base-backup library against the daemon. The
            // scenario subsequently opens the output as a normal database.
            "srv_basebackup" => {
                a.exact(2)?;
                a.no_kwargs()?;
                let h = a.handle(0)?;
                let target = self.scenario_path(a.str_at(1)?, verb)?;
                let addr = self.server(h, verb)?.control_addr.ok_or_else(|| {
                    value_err(format!(
                        "{verb}(): this server has no control listener; call \
                         srv_archive_access() or srv_replication() before launch"
                    ))
                })?;
                let options = yesno_server_utils::basebackup::BasebackupOptions::plaintext(
                    format!("http://{addr}"),
                    target,
                );
                let rt = self.rt_handle(verb)?;
                let report = rt
                    .block_on(yesno_server_utils::basebackup::run(options))
                    .map_err(|error| db_err(verb, error))?;
                Ok(dict(vec![
                    ("shards", int_obj(report.shards as u64)),
                    ("checkpoint", int_obj(report.checkpoint_version)),
                    ("recovered", int_obj(report.recovered_version)),
                    ("term", int_obj(report.term as u64)),
                    ("bytes", int_obj(report.bytes)),
                    ("attempts", int_obj(report.attempts as u64)),
                ]))
            }

            // Run the shipped archive restore and return the recovery oracle.
            "srv_restore" => {
                a.exact(3)?;
                a.no_kwargs()?;
                let object_dir = self.scenario_path(a.str_at(0)?, verb)?;
                let target = self.scenario_path(a.str_at(1)?, verb)?;
                let target_version = a.u64(2)?;
                let store = yesno_server_utils::archive::ArchiveStore::connect(&format!(
                    "file://{}",
                    object_dir.display()
                ))
                .map_err(|error| db_err(verb, error))?;
                let rt = self.rt_handle(verb)?;
                let report = rt
                    .block_on(yesno_server_utils::restore::restore_to(
                        store,
                        target,
                        yesno_server_utils::restore::RecoveryTarget::Version(target_version),
                        yesno_server_utils::restore::TargetAction::Publish,
                    ))
                    .map_err(|error| db_err(verb, error))?;
                Ok(dict(vec![
                    ("shards", int_obj(report.shards as u64)),
                    ("checkpoint", int_obj(report.checkpoint_version)),
                    ("recovered", int_obj(report.recovered_version)),
                    ("term", int_obj(report.term as u64)),
                    ("base_generation", int_obj(report.base_generation)),
                    ("bytes", int_obj(report.bytes)),
                ]))
            }

            // Restore to a wall clock, through the same shipped entry point.
            //
            // The time is microseconds, not a formatted string, and a
            // scenario is expected to take it from `srv_restore_windows` rather
            // than from `clock_ns()`. The harness clock is not the server's
            // commit clock, and comparing them would make the assertion about
            // two clocks agreeing rather than about the resolution rule.
            "srv_restore_at" => {
                a.exact(5)?;
                a.no_kwargs()?;
                let object_dir = self.scenario_path(a.str_at(0)?, verb)?;
                let target = self.scenario_path(a.str_at(1)?, verb)?;
                let micros = a.u64(2)?;
                let inclusive = a.u64(3)? != 0;
                let action = match a.str_at(4)? {
                    "publish" => yesno_server_utils::restore::TargetAction::Publish,
                    "pause" => yesno_server_utils::restore::TargetAction::Pause,
                    "promote" => yesno_server_utils::restore::TargetAction::Promote,
                    other => {
                        return Err(value_err(format!(
                            "{verb}: action must be publish, pause or promote, not '{other}'"
                        )))
                    }
                };
                let store = yesno_server_utils::archive::ArchiveStore::connect(&format!(
                    "file://{}",
                    object_dir.display()
                ))
                .map_err(|error| db_err(verb, error))?;
                let rt = self.rt_handle(verb)?;
                let report = rt
                    .block_on(yesno_server_utils::restore::restore_to(
                        store,
                        target,
                        yesno_server_utils::restore::RecoveryTarget::Time {
                            micros: micros as i64,
                            inclusive,
                        },
                        action,
                    ))
                    .map_err(|error| db_err(verb, error))?;
                Ok(dict(vec![
                    ("shards", int_obj(report.shards as u64)),
                    ("checkpoint", int_obj(report.checkpoint_version)),
                    ("recovered", int_obj(report.recovered_version)),
                    ("recovered_time", opt_int_obj(report.recovered_time)),
                    ("term", int_obj(report.term as u64)),
                    ("base_generation", int_obj(report.base_generation)),
                    ("bytes", int_obj(report.bytes)),
                    ("published", int_obj(u64::from(report.paused_at.is_none()))),
                    (
                        "directory",
                        MontyObject::String(report.directory.to_string_lossy().into_owned()),
                    ),
                ]))
            }

            // The recovery windows the archive offers, as the shipped
            // `--inspect` computes them. This is where a scenario gets a target
            // time it can assert against.
            "srv_restore_windows" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let object_dir = self.scenario_path(a.str_at(0)?, verb)?;
                let store = yesno_server_utils::archive::ArchiveStore::connect(&format!(
                    "file://{}",
                    object_dir.display()
                ))
                .map_err(|error| db_err(verb, error))?;
                let rt = self.rt_handle(verb)?;
                let windows = rt
                    .block_on(yesno_server_utils::restore::inspect(&store))
                    .map_err(|error| db_err(verb, error))?;
                Ok(MontyObject::List(
                    windows
                        .into_iter()
                        .map(|w| {
                            dict(vec![
                                ("term", int_obj(w.term as u64)),
                                ("base_generation", int_obj(w.base_generation)),
                                ("checkpoint_version", int_obj(w.checkpoint_version)),
                                ("end_version", int_obj(w.end_version)),
                                ("checkpoint_time", opt_int_obj(w.checkpoint_time)),
                                ("end_time", opt_int_obj(w.end_time)),
                                ("fully_stamped", int_obj(u64::from(w.fully_stamped))),
                                ("active", int_obj(u64::from(w.active))),
                            ])
                        })
                        .collect(),
                ))
            }

            // Run one shipped reclamation pass and report what it removed.
            //
            // The window is seconds so a scenario can place a boundary inside a
            // run; production windows are days.
            "srv_archive_gc" => {
                a.exact(3)?;
                a.no_kwargs()?;
                let object_dir = self.scenario_path(a.str_at(0)?, verb)?;
                let window_secs = a.u64(1)?;
                let min_bases = a.u64(2)? as usize;
                let store = yesno_server_utils::archive::ArchiveStore::connect(&format!(
                    "file://{}",
                    object_dir.display()
                ))
                .map_err(|error| db_err(verb, error))?;
                let policy = yesno_server_utils::gc::RetentionPolicy {
                    window_micros: window_secs.saturating_mul(1_000_000),
                    min_bases: min_bases.max(1),
                };
                let rt = self.rt_handle(verb)?;
                let plan = rt
                    .block_on(yesno_server_utils::gc::collect(&store, policy))
                    .map_err(|error| db_err(verb, error))?;
                Ok(dict(vec![
                    ("deleted", whole_obj(plan.delete.len())),
                    ("retained_bases", whole_obj(plan.retained_bases.len())),
                    ("unrecognized", whole_obj(plan.unrecognized.len())),
                ]))
            }

            // A second shipped sidecar must be rejected by the archive prefix,
            // not merely race the first writer's state.pb publication.
            "srv_archive_second_error" => {
                a.exact(3)?;
                a.no_kwargs()?;
                let h = a.handle(0)?;
                let object_dir = self.scenario_path(a.str_at(1)?, verb)?;
                let work_dir = self.scenario_path(a.str_at(2)?, verb)?;
                let socket = self
                    .server(h, verb)?
                    .control_socket
                    .as_ref()
                    .ok_or_else(|| {
                        value_err(format!(
                            "{verb}(): this server has no local control listener; call \
                         srv_archive_access() before launch"
                        ))
                    })?;
                let options = yesno_server_utils::sidecar::ArchiveOptions::network(
                    format!("unix://{}", socket.display()),
                    format!("file://{}", object_dir.display()),
                    work_dir,
                );
                let rt = self.rt_handle(verb)?;
                let result = rt.block_on(async {
                    tokio::time::timeout(
                        std::time::Duration::from_secs(2),
                        yesno_server_utils::sidecar::run_with_shutdown(
                            options,
                            std::future::pending(),
                        ),
                    )
                    .await
                });
                match result {
                    Ok(Err(error)) => Ok(MontyObject::String(error.to_string())),
                    Ok(Ok(())) => Err(value_err(format!(
                        "{verb}(): competing archive writer exited successfully"
                    ))),
                    Err(_) => Err(value_err(format!(
                        "{verb}(): competing archive writer was not rejected within 2 seconds"
                    ))),
                }
            }

            // Start the shipped archive sidecar against a local object-store
            // backend. The durable work directory is reused on restart.
            "srv_archive_start" => {
                a.exact(3)?;
                a.no_kwargs()?;
                let h = a.handle(0)?;
                let object_dir = self.scenario_path(a.str_at(1)?, verb)?;
                let work_dir = self.scenario_path(a.str_at(2)?, verb)?;
                std::fs::create_dir_all(&object_dir).map_err(|error| db_err(verb, error))?;
                let socket = self
                    .server(h, verb)?
                    .control_socket
                    .as_ref()
                    .ok_or_else(|| {
                        value_err(format!(
                            "{verb}(): this server has no local control listener; call \
                         srv_archive_access() before launch"
                        ))
                    })?;
                let options = yesno_server_utils::sidecar::ArchiveOptions::network(
                    format!("unix://{}", socket.display()),
                    format!("file://{}", object_dir.display()),
                    work_dir,
                );
                let (stop, stopped) = tokio::sync::oneshot::channel();
                let rt = self.rt_handle(verb)?;
                let task = rt.spawn(yesno_server_utils::sidecar::run_with_shutdown(
                    options,
                    async move {
                        let _ = stopped.await;
                    },
                ));
                self.server_state.archives.push(Some(ArchiveSidecar {
                    stop: Some(stop),
                    task: Some(task),
                    object_dir,
                }));
                let i = self.server_state.archives.len() - 1;
                Ok(self.mint(HandleKind::Archive, i))
            }

            // Prove that the event subscriber is live while a fresh archive
            // remains unactivated. This waits for a nonzero durable event
            // cursor, not for an arbitrary sleep, so the scenario can
            // distinguish checkpoint-driven activation from a slow eager base.
            "srv_archive_wait_unactivated" => {
                a.exact(2)?;
                a.no_kwargs()?;
                let h = a.handle(0)?;
                let timeout_ms = a.u64(1)?;
                let i = self.slot(h, HandleKind::Archive, verb)?;
                let deadline =
                    std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
                loop {
                    let finished = self.server_state.archives[i]
                        .as_ref()
                        .ok_or_else(|| stale_handle(verb, "archive sidecar", h))?
                        .task
                        .as_ref()
                        .is_some_and(tokio::task::JoinHandle::is_finished);
                    if finished {
                        let sidecar = self.server_state.archives[i]
                            .as_mut()
                            .expect("checked above");
                        let task = sidecar.task.take().expect("checked above");
                        let rt = self.rt_handle(verb)?;
                        let outcome = rt.block_on(task);
                        self.server_state.archives[i] = None;
                        return match outcome {
                            Ok(Ok(())) => Err(value_err(format!(
                                "{verb}(): archive sidecar stopped before subscribing"
                            ))),
                            Ok(Err(error)) => Err(db_err(verb, error)),
                            Err(error) => Err(db_err(verb, error)),
                        };
                    }

                    let object_dir = &self.server_state.archives[i]
                        .as_ref()
                        .expect("checked above")
                        .object_dir;
                    match read_archive_state(object_dir) {
                        Ok(state) if !state.latest_base_manifest.is_empty() => {
                            return Err(value_err(format!(
                                "{verb}(): archive activated before a completed checkpoint event"
                            )));
                        }
                        Ok(state) if state.event_sequence > 0 => {
                            return Ok(int_obj(state.event_sequence));
                        }
                        Ok(_) => {}
                        Err(error) if error.starts_with("archive state is not readable:") => {}
                        Err(error) => return Err(value_err(format!("{verb}(): {error}"))),
                    }

                    if std::time::Instant::now() >= deadline {
                        return Err(value_err(format!(
                            "{verb}(): archive did not subscribe while unactivated in {timeout_ms} ms"
                        )));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
            }

            // Wait for an externally visible archive invariant, checking the
            // Protobuf state/manifest and every referenced local object.
            "srv_archive_wait" => {
                a.exact(4)?;
                a.no_kwargs()?;
                let h = a.handle(0)?;
                let metric = a.str_at(1)?.to_owned();
                let want = a.u64(2)?;
                let timeout_ms = a.u64(3)?;
                if !matches!(
                    metric.as_str(),
                    "base_generation"
                        | "event_sequence"
                        | "wal_objects"
                        | "wal_bytes"
                        | "cursor_total"
                ) {
                    return Err(value_err(format!(
                        "{verb}(): '{metric}' is not an archive metric; use base_generation, \
                         event_sequence, wal_objects, wal_bytes or cursor_total"
                    )));
                }
                let i = self.slot(h, HandleKind::Archive, verb)?;
                let deadline =
                    std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
                loop {
                    let sidecar = self.server_state.archives[i]
                        .as_mut()
                        .ok_or_else(|| stale_handle(verb, "archive sidecar", h))?;
                    if sidecar
                        .task
                        .as_ref()
                        .is_some_and(tokio::task::JoinHandle::is_finished)
                    {
                        let task = sidecar.task.take().expect("checked");
                        let rt = self.rt_handle(verb)?;
                        let outcome = rt.block_on(task);
                        self.server_state.archives[i] = None;
                        return match outcome {
                            Ok(Ok(())) => Err(value_err(format!(
                                "{verb}(): archive sidecar stopped before {metric} reached {want}"
                            ))),
                            Ok(Err(error)) => Err(db_err(verb, error)),
                            Err(error) => Err(db_err(verb, error)),
                        };
                    }
                    let last = match inspect_archive(&sidecar.object_dir) {
                        Ok(stats) => {
                            let got = stats.metric(&metric).expect("validated");
                            if got >= want {
                                return Ok(stats.object());
                            }
                            format!("{metric} reached {got}, not {want}")
                        }
                        Err(error) => error,
                    };
                    if std::time::Instant::now() >= deadline {
                        return Err(value_err(format!("{verb}(): {last} in {timeout_ms} ms")));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
            }

            "srv_archive_stop" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let h = a.handle(0)?;
                let i = self.slot(h, HandleKind::Archive, verb)?;
                let mut sidecar = self.server_state.archives[i]
                    .take()
                    .ok_or_else(|| stale_handle(verb, "archive sidecar", h))?;
                if let Some(stop) = sidecar.stop.take() {
                    let _ = stop.send(());
                }
                let task = sidecar.task.take().expect("live sidecar has a task");
                let rt = self.rt_handle(verb)?;
                match rt.block_on(task) {
                    Ok(Ok(())) => Ok(MontyObject::None),
                    Ok(Err(error)) => Err(db_err(verb, error)),
                    Err(error) => Err(db_err(verb, error)),
                }
            }

            // Stop, and hand back what teardown observed.
            //
            // `clean` is the assertion worth making: it is false when a
            // reader failed to drain or a `Db` handle outlived the services, and
            // the visible consequence of either is that the **next** `srv_start`
            // or `db_open` of this directory fails with `AlreadyOpen` — a
            // failure that points nowhere near its cause.
            "srv_stop" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let h = a.handle(0)?;
                let i = self.slot(h, HandleKind::Server, verb)?;
                let rt = self.rt_handle(verb)?;
                match self.server_state.servers[i].take() {
                    None => Err(stale_handle(verb, "server", h)),
                    Some(mut s) => match s.node.take() {
                        Some(Node::Leader(running)) => {
                            let t = rt.block_on(running.shutdown());
                            Ok(dict(vec![
                                (
                                    "checkpoint",
                                    match t.checkpoint {
                                        Some(w) => int_obj(w),
                                        None => MontyObject::None,
                                    },
                                ),
                                ("readers_left", whole_obj(t.readers_left)),
                                ("lock_released", MontyObject::Bool(t.sole_owner)),
                                ("clean", MontyObject::Bool(t.is_clean())),
                            ]))
                        }
                        Some(Node::Standby(n)) => {
                            if let Some(r) = s.reads.take() {
                                rt.block_on(r.stop());
                            }
                            rt.block_on(n.stop());
                            // A standby holds no database, so there is no
                            // checkpoint and no lock to report on. Saying so is
                            // better than reporting a leader's shape with
                            // zeroes in it.
                            Ok(dict(vec![
                                ("checkpoint", MontyObject::None),
                                ("readers_left", whole_obj(0)),
                                ("lock_released", MontyObject::Bool(true)),
                                ("clean", MontyObject::Bool(true)),
                            ]))
                        }
                        None => Err(stale_handle(verb, "server", h)),
                    },
                }
            }

            _ => Err(value_err(format!("{verb}(): not a server verb"))),
        }
    }
}
