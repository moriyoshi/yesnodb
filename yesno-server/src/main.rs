//! `yesnod`.

use std::sync::Arc;

use clap::Parser;
use yesno_server::config::{Cli, Config, Role};
use yesno_server::control::{self, pb, Command, EventHub};

fn main() -> std::process::ExitCode {
    yesno_server::tls::install_crypto_provider();
    let cli = Cli::parse();

    let cfg = match Config::resolve(&cli) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("yesnod: {e}");
            return std::process::ExitCode::from(2);
        }
    };

    // Before the runtime, and before anything opens the directory. This is what
    // a systemd ExecStartPre runs; taking the real database lock here is wrong.
    if cli.check_config {
        match toml::to_string_pretty(&cfg) {
            Ok(s) => {
                print!("{s}");
                return std::process::ExitCode::SUCCESS;
            }
            Err(e) => {
                eprintln!("yesnod: cannot render the effective configuration: {e}");
                return std::process::ExitCode::FAILURE;
            }
        }
    }

    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("yesnod: cannot start a runtime: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let telemetry = {
        // The current OTLP/gRPC exporter creates its Tonic channel from the
        // ambient Tokio handle. Keep the runtime alive through SDK shutdown.
        let _entered = rt.enter();
        match yesno_server::init_tracing_with_telemetry(&cli.log, &cfg.server.telemetry) {
            Ok(telemetry) => telemetry,
            Err(error) => {
                eprintln!("yesnod: cannot initialize telemetry: {error}");
                return std::process::ExitCode::FAILURE;
            }
        }
    };

    let code = rt.block_on(run(cfg));
    if let Err(error) = telemetry.shutdown() {
        eprintln!("yesnod: cannot flush OpenTelemetry traces: {error}");
    }
    code
}

async fn run(cfg: Config) -> std::process::ExitCode {
    // Before any listener binds, so a SIGHUP arriving during startup is queued
    // by the handler rather than landing on the default disposition, which for
    // SIGHUP is "terminate the process".
    yesno_server::tls::watch_for_reload_signal();

    let control_enabled = cfg.control_enabled();
    let (hub, control, mut commands, shared_replication) = match control_enabled {
        false => (None, None, None, None),
        true => {
            let journal = cfg
                .control_journal_dir()
                .expect("validated control journal")
                .to_path_buf();
            let hub = match EventHub::open(journal) {
                Ok(hub) => hub,
                Err(error) => {
                    eprintln!("yesnod: cannot open the control journal: {error}");
                    return std::process::ExitCode::FAILURE;
                }
            };
            let (tx, rx) = tokio::sync::mpsc::channel(16);
            let serving = match control::serve(&cfg.server, &cfg.auth, hub.clone(), tx).await {
                Ok(serving) => serving,
                Err(error) => {
                    eprintln!("yesnod: cannot start the control-plane endpoint: {error}");
                    return std::process::ExitCode::FAILURE;
                }
            };
            let replication = serving.replication_slot();
            (Some(hub), Some(serving), Some(rx), Some(replication))
        }
    };

    let code = run_node(cfg, hub.clone(), &mut commands, shared_replication).await;
    if let Some(control) = control {
        control.stop().await;
    }
    code
}

async fn run_node(
    mut cfg: Config,
    hub: Option<Arc<EventHub>>,
    commands: &mut Option<tokio::sync::mpsc::Receiver<Command>>,
    shared_replication: Option<control::ReplicationSlot>,
) -> std::process::ExitCode {
    let mut pending_role: Option<Vec<u8>> = None;

    // One metrics listener for the life of the process, not one per role.
    // A role change swaps the surface underneath it; it never closes the port.
    // Binding per role made promotion a stop-then-rebind, and the liveness
    // probe watches that port -- see `metrics::Surface`.
    let shared = yesno_server::metrics::Shared::new();
    let metrics = match cfg.metrics_addr() {
        Ok(Some(addr)) => match yesno_server::metrics::serve_shared(addr, shared.clone()).await {
            Ok(serving) => Some(serving),
            Err(error) => {
                eprintln!("yesnod: cannot serve metrics: {error}");
                return std::process::ExitCode::FAILURE;
            }
        },
        Ok(None) => None,
        Err(error) => {
            eprintln!("yesnod: cannot serve metrics: {error}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let metrics_addr = metrics.as_ref().map(|m| m.addr);

    loop {
        match cfg.server.role {
            Role::Follower => {
                match follow(&cfg, hub.as_ref(), commands, pending_role.take(), &shared).await {
                    Outcome::Promote { correlation_id } => {
                        let previous_term = yesno_core::database_term(cfg.data_dir()).unwrap_or(0);
                        publish_role(
                            hub.as_ref(),
                            correlation_id.clone(),
                            pb::EventPhase::Started,
                            pb::Role::Follower,
                            pb::Role::Leader,
                            previous_term,
                            previous_term,
                            String::new(),
                        );
                        // Raise the term before opening as a leader. The promoted
                        // directory has the same UUID as the leadership it replaces;
                        // only the monotonically increasing term fences the old one.
                        match yesno_server::lifecycle::raise_term(cfg.data_dir()) {
                            Ok(term) => {
                                publish_role(
                                    hub.as_ref(),
                                    correlation_id.clone(),
                                    pb::EventPhase::Observed,
                                    pb::Role::Follower,
                                    pb::Role::Leader,
                                    previous_term,
                                    term,
                                    "leadership term advanced".into(),
                                );
                            }
                            Err(error) => {
                                publish_role(
                                    hub.as_ref(),
                                    correlation_id,
                                    pb::EventPhase::Failed,
                                    pb::Role::Follower,
                                    pb::Role::Leader,
                                    previous_term,
                                    previous_term,
                                    error.to_string(),
                                );
                                eprintln!("yesnod: cannot raise the leadership term: {error}");
                                return std::process::ExitCode::FAILURE;
                            }
                        }
                        cfg.server.role = Role::Leader;
                        pending_role = Some(correlation_id);
                    }
                    Outcome::Shutdown { .. } => return std::process::ExitCode::SUCCESS,
                    Outcome::Exit(code) => return code,
                    Outcome::Demote { .. } => unreachable!("a follower cannot demote"),
                }
            }
            Role::Leader => match lead(
                &cfg,
                hub.as_ref(),
                commands,
                pending_role.take(),
                shared_replication.as_ref(),
                metrics_addr.map(|addr| yesno_server::metrics::Adopted {
                    addr,
                    shared: shared.clone(),
                }),
            )
            .await
            {
                Outcome::Demote {
                    correlation_id,
                    leader,
                } => {
                    if !leader.is_empty() {
                        cfg.follower.leader = leader;
                    }
                    if cfg.follower.leader.is_empty() {
                        publish_role(
                            hub.as_ref(),
                            correlation_id,
                            pb::EventPhase::Failed,
                            pb::Role::Leader,
                            pb::Role::Follower,
                            yesno_core::database_term(cfg.data_dir()).unwrap_or(0),
                            yesno_core::database_term(cfg.data_dir()).unwrap_or(0),
                            "demotion needs a replacement leader endpoint".into(),
                        );
                        return std::process::ExitCode::FAILURE;
                    }
                    cfg.server.role = Role::Follower;
                    pending_role = Some(correlation_id);
                }
                Outcome::Shutdown { .. } => return std::process::ExitCode::SUCCESS,
                Outcome::Exit(code) => return code,
                Outcome::Promote { .. } => unreachable!("a leader cannot promote"),
            },
        }
    }
}

enum Outcome {
    Promote {
        correlation_id: Vec<u8>,
    },
    Demote {
        correlation_id: Vec<u8>,
        leader: String,
    },
    Shutdown {
        correlation_id: Vec<u8>,
    },
    Exit(std::process::ExitCode),
}

async fn follow(
    cfg: &Config,
    hub: Option<&Arc<EventHub>>,
    commands: &mut Option<tokio::sync::mpsc::Receiver<Command>>,
    transition: Option<Vec<u8>>,
    shared: &yesno_server::metrics::Shared,
) -> Outcome {
    publish_server(
        hub,
        pb::server_lifecycle_event::Operation::Start,
        pb::EventPhase::Started,
        false,
        "",
    );
    let event_sink = hub.map(|hub| hub.core_sink());
    let node = match yesno_server::follower::start_with_events(cfg, event_sink) {
        Ok(node) => node,
        Err(error) => {
            publish_server(
                hub,
                pb::server_lifecycle_event::Operation::Start,
                pb::EventPhase::Failed,
                false,
                &error.to_string(),
            );
            eprintln!("yesnod: cannot start following: {error}");
            return Outcome::Exit(std::process::ExitCode::FAILURE);
        }
    };

    let role_from = if transition.is_some() {
        pb::Role::Leader
    } else {
        pb::Role::Offline
    };
    publish_role(
        hub,
        transition.unwrap_or_default(),
        pb::EventPhase::Completed,
        role_from,
        pb::Role::Follower,
        yesno_core::database_term(cfg.data_dir()).unwrap_or(0),
        yesno_core::database_term(cfg.data_dir()).unwrap_or(0),
        String::new(),
    );
    let reads = match node.db.clone() {
        None => None,
        Some(slot) => match yesno_server::lifecycle::serve_reads(cfg, slot).await {
            Ok(listener) => Some(listener),
            Err(error) => {
                publish_server(
                    hub,
                    pb::server_lifecycle_event::Operation::Start,
                    pb::EventPhase::Failed,
                    false,
                    &error.to_string(),
                );
                eprintln!("yesnod: cannot serve reads: {error}");
                node.stop().await;
                return Outcome::Exit(std::process::ExitCode::FAILURE);
            }
        },
    };

    // The listener is already bound and answering `/healthz`; following only
    // changes what `/metrics` and `/readyz` say. Do not bind one here -- that
    // is what made a role change close the port.
    shared.set_follower(node.status.clone());

    publish_server(
        hub,
        pb::server_lifecycle_event::Operation::Start,
        pb::EventPhase::Completed,
        true,
        "follower loop and listeners running",
    );

    tracing::info!(
        dir = %cfg.data_dir().display(),
        leader = %cfg.follower.leader,
        reads = ?reads.as_ref().map(|r| r.addr),
        "yesnod is following; send SIGUSR1 or use the control-plane endpoint to promote"
    );

    let outcome = tokio::select! {
        _ = yesno_server::lifecycle::terminated() => Outcome::Exit(std::process::ExitCode::SUCCESS),
        _ = promotion_requested() => {
            let correlation_id = new_transition(hub, pb::Role::Follower, pb::Role::Leader);
            Outcome::Promote { correlation_id }
        },
        command = next_command(commands) => match command {
            Command::Promote { correlation_id } => Outcome::Promote { correlation_id },
            Command::Shutdown { correlation_id } => Outcome::Shutdown { correlation_id },
            Command::Demote { correlation_id, .. } => {
                publish_role(
                    hub,
                    correlation_id,
                    pb::EventPhase::Failed,
                    pb::Role::Follower,
                    pb::Role::Follower,
                    0,
                    0,
                    "this node is already a follower".into(),
                );
                Outcome::Exit(std::process::ExitCode::FAILURE)
            }
        },
        _ = halted(&node) => Outcome::Exit(std::process::ExitCode::FAILURE),
    };

    let shutdown_correlation = match &outcome {
        Outcome::Shutdown { correlation_id } => correlation_id.clone(),
        _ => Vec::new(),
    };
    publish_server_correlated(
        hub,
        shutdown_correlation.clone(),
        pb::server_lifecycle_event::Operation::Shutdown,
        pb::EventPhase::Started,
        false,
        "",
    );

    if let Some(reads) = reads {
        reads.stop().await;
    }
    let halt_reason = node.status.halt_reason.lock().unwrap().clone();
    match &outcome {
        Outcome::Promote { .. } => {
            node.stop_for(yesno_core::events::ShutdownReason::Promotion)
                .await;
        }
        _ => node.stop().await,
    }
    publish_server_correlated(
        hub,
        shutdown_correlation,
        pb::server_lifecycle_event::Operation::Shutdown,
        if halt_reason.is_some() {
            pb::EventPhase::Failed
        } else {
            pb::EventPhase::Completed
        },
        halt_reason.is_none(),
        halt_reason.as_deref().unwrap_or(""),
    );
    if let Some(reason) = halt_reason {
        eprintln!("yesnod: {reason}");
    }
    outcome
}

async fn halted(node: &yesno_server::follower::FollowerNode) {
    loop {
        if node.is_halted() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

#[cfg(unix)]
async fn promotion_requested() {
    use tokio::signal::unix::{signal, SignalKind};
    match signal(SignalKind::user_defined1()) {
        Ok(mut signal) => {
            signal.recv().await;
        }
        Err(error) => {
            tracing::error!(error = %error, "cannot listen for SIGUSR1; promotion is unavailable");
            std::future::pending::<()>().await;
        }
    }
}

#[cfg(not(unix))]
async fn promotion_requested() {
    std::future::pending::<()>().await;
}

async fn lead(
    cfg: &Config,
    hub: Option<&Arc<EventHub>>,
    commands: &mut Option<tokio::sync::mpsc::Receiver<Command>>,
    transition: Option<Vec<u8>>,
    shared_replication: Option<&control::ReplicationSlot>,
    adopted: Option<yesno_server::metrics::Adopted>,
) -> Outcome {
    publish_server(
        hub,
        pb::server_lifecycle_event::Operation::Start,
        pb::EventPhase::Started,
        false,
        "",
    );
    let event_sink = hub.map(|hub| hub.core_sink());
    let running = match yesno_server::lifecycle::start_with_events(cfg, event_sink, adopted).await {
        Ok(running) => running,
        Err(error) => {
            publish_server(
                hub,
                pb::server_lifecycle_event::Operation::Start,
                pb::EventPhase::Failed,
                false,
                &error.to_string(),
            );
            eprintln!("yesnod: cannot start: {error}");
            return Outcome::Exit(std::process::ExitCode::FAILURE);
        }
    };

    let db = running.db();
    let term = db.term();
    if let Some(slot) = shared_replication {
        slot.install_with_db(
            yesno_server::replication::LeaderService::with_retention(
                cfg.data_dir(),
                db.shard_count(),
                db.retention_floor(),
            ),
            db.clone(),
        );
    }
    drop(db);
    let role_from = if transition.is_some() {
        pb::Role::Follower
    } else {
        pb::Role::Offline
    };
    publish_role(
        hub,
        transition.unwrap_or_default(),
        pb::EventPhase::Completed,
        role_from,
        pb::Role::Leader,
        term,
        term,
        String::new(),
    );
    publish_server(
        hub,
        pb::server_lifecycle_event::Operation::Start,
        pb::EventPhase::Completed,
        true,
        "leader listeners running",
    );

    let outcome = loop {
        tokio::select! {
            _ = yesno_server::lifecycle::terminated() => {
                break Outcome::Exit(std::process::ExitCode::SUCCESS);
            }
            command = next_command(commands) => match command {
                Command::Shutdown { correlation_id } => {
                    break Outcome::Shutdown { correlation_id };
                }
                Command::Demote { correlation_id, leader } => {
                    publish_role(
                        hub,
                        correlation_id.clone(),
                        pb::EventPhase::Started,
                        pb::Role::Leader,
                        pb::Role::Follower,
                        term,
                        term,
                        String::new(),
                    );
                    break Outcome::Demote { correlation_id, leader };
                }
                Command::Promote { correlation_id } => {
                    // The command may have been accepted while this process was
                    // still a follower and then duplicated before Kubernetes
                    // persisted the operator's next failover stage. Once the
                    // first command has promoted us, the duplicate is success:
                    // tearing down an already-correct leader would make the
                    // control API unsafe to retry across that durability gap.
                    publish_role(
                        hub,
                        correlation_id,
                        pb::EventPhase::Completed,
                        pb::Role::Leader,
                        pb::Role::Leader,
                        term,
                        term,
                        "promotion already completed".into(),
                    );
                }
            },
        }
    };

    let shutdown_correlation = match &outcome {
        Outcome::Shutdown { correlation_id } => correlation_id.clone(),
        _ => Vec::new(),
    };
    publish_server_correlated(
        hub,
        shutdown_correlation.clone(),
        pb::server_lifecycle_event::Operation::Shutdown,
        pb::EventPhase::Started,
        false,
        "",
    );
    if let Some(slot) = shared_replication {
        slot.clear();
    }
    let teardown = match &outcome {
        Outcome::Demote { .. } => {
            running
                .shutdown_for(yesno_core::events::ShutdownReason::Demotion)
                .await
        }
        _ => running.shutdown().await,
    };
    publish_server_correlated(
        hub,
        shutdown_correlation,
        pb::server_lifecycle_event::Operation::Shutdown,
        pb::EventPhase::Completed,
        teardown.is_clean(),
        "",
    );

    if !teardown.is_clean() {
        return Outcome::Exit(std::process::ExitCode::FAILURE);
    }
    outcome
}

async fn next_command(commands: &mut Option<tokio::sync::mpsc::Receiver<Command>>) -> Command {
    match commands {
        Some(commands) => match commands.recv().await {
            Some(command) => command,
            None => std::future::pending().await,
        },
        None => std::future::pending().await,
    }
}

fn new_transition(hub: Option<&Arc<EventHub>>, from: pb::Role, to: pb::Role) -> Vec<u8> {
    let Some(hub) = hub else {
        return Vec::new();
    };
    let correlation_id = hub.correlation_id(&[]);
    publish_role(
        Some(hub),
        correlation_id.clone(),
        pb::EventPhase::Requested,
        from,
        to,
        hub.snapshot().term,
        hub.snapshot().term,
        "requested by SIGUSR1".into(),
    );
    correlation_id
}

// A role fact is kept as explicit scalar fields at its single call seam so
// reviewers can audit every transition047s before/after term alongside its role.
#[allow(clippy::too_many_arguments)]
fn publish_role(
    hub: Option<&Arc<EventHub>>,
    correlation_id: Vec<u8>,
    phase: pb::EventPhase,
    from: pb::Role,
    to: pb::Role,
    previous_term: u32,
    term: u32,
    detail: String,
) {
    let Some(hub) = hub else { return };
    if let Err(error) = hub.publish_server(
        if phase == pb::EventPhase::Failed {
            pb::Severity::Error
        } else {
            pb::Severity::Info
        },
        correlation_id,
        Vec::new(),
        pb::event_envelope::Payload::RoleTransition(pb::RoleTransitionEvent {
            phase: phase as i32,
            from: from as i32,
            to: to as i32,
            previous_term,
            term,
            detail,
        }),
    ) {
        tracing::error!(error = %error, "cannot append a role event");
    }
}

fn publish_server(
    hub: Option<&Arc<EventHub>>,
    operation: pb::server_lifecycle_event::Operation,
    phase: pb::EventPhase,
    clean: bool,
    detail: &str,
) {
    publish_server_correlated(hub, Vec::new(), operation, phase, clean, detail);
}

fn publish_server_correlated(
    hub: Option<&Arc<EventHub>>,
    correlation_id: Vec<u8>,
    operation: pb::server_lifecycle_event::Operation,
    phase: pb::EventPhase,
    clean: bool,
    detail: &str,
) {
    let Some(hub) = hub else { return };
    if let Err(error) = hub.publish_server(
        if phase == pb::EventPhase::Failed {
            pb::Severity::Error
        } else {
            pb::Severity::Info
        },
        correlation_id,
        Vec::new(),
        pb::event_envelope::Payload::ServerLifecycle(pb::ServerLifecycleEvent {
            operation: operation as i32,
            phase: phase as i32,
            clean,
            detail: detail.into(),
        }),
    ) {
        tracing::error!(error = %error, "cannot append a server lifecycle event");
    }
}
