//! Open, serve, and — the part that needs designing — stop.
//!
//! # There is no `Db::close()`
//!
//! Teardown is `Drop` on the last `Arc<DbInner>`, and the database's exclusive
//! `flock` lives inside it. So "did we shut down cleanly" is not a call, it is a
//! proof obligation, and it has two halves that do **not** imply each other:
//!
//! * `Arc::into_inner( db )` returning `Some` proves no `Arc<Db>` clone
//!   survives — the services, the client tasks, every closure.
//! * It proves **nothing** about `Snapshot`s. A `Snapshot` holds its own
//!   `Arc<DbInner>`, not an `Arc<Db>`, so a live one is invisible to that
//!   refcount while still pinning the lock. `Db::live_readers()` is the
//!   instrument for exactly that gap.
//!
//! Between them they cover every holder of an `Arc<DbInner>`. The observational
//! proof — that the directory can be reopened afterwards — is what the test
//! asserts, because `acquire_lock` uses a non-blocking `try_lock` and answers
//! `AlreadyOpen` otherwise.
//!
//! # `spawn_blocking` is not optional here either
//!
//! `do_get` faults the mmap and `commit` fsyncs; the final checkpoint does both
//! and more. Anything that touches the engine goes onto the blocking pool.

use std::sync::Arc;
use std::time::{Duration, Instant};

use arrow_flight::flight_service_server::FlightServiceServer;
use tonic::transport::Server;
use yesno_core::Db;

use crate::config::Config;

/// A running `yesnod`.
pub struct Running {
    /// The address the Flight service actually bound. Not the configured one:
    /// port 0 is how a test asks the OS to choose, and then the configured
    /// value is a lie.
    pub flight_addr: std::net::SocketAddr,
    /// Where `/metrics` is, when it is enabled.
    pub metrics_addr: Option<std::net::SocketAddr>,
    /// The shared control and replication endpoint, when this convenience
    /// start path owns it.
    pub control_addr: Option<std::net::SocketAddr>,
    /// Local shared control and replication channel, when configured.
    pub control_socket: Option<std::path::PathBuf>,
    stop: tokio::sync::oneshot::Sender<()>,
    served: tokio::task::JoinHandle<()>,
    control: Option<crate::control::Serving>,
    control_commands: Option<tokio::sync::mpsc::Receiver<crate::control::Command>>,
    ticker: crate::maintenance::Ticker,
    metrics: Option<crate::metrics::Serving>,
    counters: Arc<crate::maintenance::Counters>,
    db: Arc<Db>,
    grace: Duration,
}

/// What teardown observed, so a caller can assert on it rather than trust it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Teardown {
    /// The watermark the final checkpoint reached, if it ran and succeeded.
    pub checkpoint: Option<u64>,
    /// Readers still live when the drain gave up. Zero on a clean shutdown.
    pub readers_left: usize,
    /// Whether the last `Arc<Db>` was ours, i.e. nothing leaked a handle.
    pub sole_owner: bool,
}

impl Teardown {
    /// Everything drained and nothing leaked.
    pub fn is_clean(&self) -> bool {
        self.readers_left == 0 && self.sole_owner
    }
}

/// Serve Flight from a slot somebody else fills — the read-serving standby.
///
/// Bound **once**, and that is the point of the slot. A standby that falls off
/// its leader's retained log must close the database to take a fresh image, and
/// closing the port with it would drop every connection for a condition the
/// caller could simply be told about. Reads answer `unavailable` across the gap
/// instead.
pub async fn serve_reads(
    cfg: &Config,
    slot: crate::guard::DbSlot,
) -> Result<ReadListener, Box<dyn std::error::Error + Send + Sync>> {
    let auth = Arc::new(crate::auth::Authenticator::new(&cfg.auth));
    let service = crate::guard::GuardedFlight::new(slot);
    let service =
        FlightServiceServer::with_interceptor(service, crate::auth::interceptor(auth.clone()));

    let listener = tokio::net::TcpListener::bind(cfg.flight_addr()?).await?;
    let addr = listener.local_addr()?;
    let (stop, stop_rx) = tokio::sync::oneshot::channel::<()>();

    let tls = crate::tls::ReloadableTls::new(&cfg.server.flight.tls, "flight")?;
    let router = Server::builder().add_service(service);
    let served = tokio::spawn(async move {
        let r = crate::tls::serve_router(router, listener, tls, async {
            let _ = stop_rx.await;
        })
        .await;
        if let Err(e) = r {
            tracing::error!(error = %e, "the replica's Flight listener stopped with an error");
        }
    });

    Ok(ReadListener { addr, stop, served })
}

/// A standby's read listener.
pub struct ReadListener {
    pub addr: std::net::SocketAddr,
    stop: tokio::sync::oneshot::Sender<()>,
    served: tokio::task::JoinHandle<()>,
}

impl ReadListener {
    pub async fn stop(self) {
        let _ = self.stop.send(());
        let _ = self.served.await;
    }
}

/// Raise this directory's leadership term, as a promotion must.
///
/// **Before opening, never after.** The term is read at open and stamped onto
/// every record the leadership writes, so bumping it afterwards would leave the
/// first records of a new leadership claiming the old one — which is exactly the
/// ambiguity the term exists to remove. `promote_database` refuses while the
/// database is open, so the ordering is enforced rather than remembered.
pub fn raise_term(dir: &std::path::Path) -> Result<u32, Box<dyn std::error::Error + Send + Sync>> {
    let now = yesno_core::database_term(dir)?;
    let next = now
        .checked_add(1)
        .ok_or("the leadership term would overflow")?;
    let got = yesno_core::promote_database(dir, next)?;
    tracing::info!(from = now, to = got, "leadership term raised");
    Ok(got)
}

/// Open the database and start serving. Does not return until it is bound.
pub async fn start(cfg: &Config) -> Result<Running, Box<dyn std::error::Error + Send + Sync>> {
    let hub = match cfg.control_enabled() {
        false => None,
        true => {
            let journal = cfg
                .control_journal_dir()
                .ok_or("the control endpoint has no journal directory")?;
            Some(crate::control::EventHub::open(journal)?)
        }
    };
    // The sink must exist before the database opens. Attaching the control
    // service afterwards leaves recovery, checkpoint and storage events on the
    // core no-op sink even though subscribers can connect successfully.
    let mut running = start_with_events(cfg, hub.as_ref().map(|hub| hub.core_sink()), None).await?;
    let Some(hub) = hub else {
        return Ok(running);
    };

    let start_control = async {
        let (commands, receiver) = tokio::sync::mpsc::channel(16);
        let serving = crate::control::serve(&cfg.server, &cfg.auth, hub, commands).await?;
        let db = running.db();
        serving.replication_slot().install_with_db(
            crate::replication::LeaderService::with_retention(
                cfg.data_dir(),
                db.shard_count(),
                db.retention_floor(),
            ),
            db.clone(),
        );
        drop(db);
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((serving, receiver))
    }
    .await;

    match start_control {
        Ok((serving, receiver)) => {
            running.control_addr = serving.addr;
            running.control_socket = serving.unix_socket.clone();
            running.control = Some(serving);
            running.control_commands = Some(receiver);
            Ok(running)
        }
        Err(error) => {
            running.shutdown().await;
            Err(error)
        }
    }
}

/// The observed form of start, used by yesnod's control plane.
pub async fn start_with_events(
    cfg: &Config,
    event_sink: Option<Arc<dyn yesno_core::events::CoreEventSink>>,
    adopted: Option<crate::metrics::Adopted>,
) -> Result<Running, Box<dyn std::error::Error + Send + Sync>> {
    crate::tls::install_crypto_provider();
    let dir = cfg.data_dir().to_path_buf();
    let opts = cfg.db.into();

    // Opening takes the exclusive lock and may replay a log, so it is blocking
    // work even though it happens once.
    let db = {
        let dir = dir.clone();
        tokio::task::spawn_blocking(move || match event_sink {
            Some(sink) => Db::open_with_events(&dir, opts, sink),
            None => Db::open_with(&dir, opts),
        })
        .await??
    };
    let db = Arc::new(db);

    // The persisted shard count wins over the configured one, silently, and
    // it must: the count is part of the routing function, so there is no way to
    // serve four shards' data as eight. Saying so turns a silent adoption into
    // something an operator can see and act on.
    if db.shard_count() != cfg.db.shards {
        tracing::warn!(
            configured = cfg.db.shards,
            actual = db.shard_count(),
            "db.shards is a creation parameter; this database's MANIFEST says otherwise \
             and the MANIFEST wins"
        );
    }

    let listener = tokio::net::TcpListener::bind(cfg.flight_addr()?).await?;
    let flight_addr = listener.local_addr()?;
    let (stop, stop_rx) = tokio::sync::oneshot::channel::<()>();

    // Authentication resolves a principal into the request's extensions;
    // authorization is the service wrapper that reads it. Two layers because
    // an interceptor cannot see which RPC it is guarding — see `auth` — and a
    // tower layer cannot separate `do_action( "stats" )` from mutation actions,
    // which share a URI.
    let auth = Arc::new(crate::auth::Authenticator::new(&cfg.auth));
    let service = crate::guard::GuardedFlight::new(crate::guard::slot_of(db.clone()));
    let service =
        FlightServiceServer::with_interceptor(service, crate::auth::interceptor(auth.clone()));

    let tls = crate::tls::ReloadableTls::new(&cfg.server.flight.tls, "flight")?;
    let router = Server::builder().add_service(service);
    let served = tokio::spawn(async move {
        let r = crate::tls::serve_router(router, listener, tls, async {
            let _ = stop_rx.await;
        })
        .await;
        if let Err(e) = r {
            tracing::error!(error = %e, "the Flight listener stopped with an error");
        }
    });

    // The thing without which `CheckpointPolicy::interval_secs` is dead config:
    // nothing in the engine ticks, so an idle database never checkpoints.
    let counters = Arc::new(crate::maintenance::Counters::default());
    let ticker = crate::maintenance::Ticker::spawn(
        db.clone(),
        cfg.db.checkpoint.interval_secs,
        counters.clone(),
    );

    // When the caller already bound a listener, take its surface over rather
    // than binding a second one. Binding here would mean the caller had to close
    // theirs first, and that gap is what the liveness probe kills the process
    // for. See `metrics::Surface`.
    let (metrics, metrics_addr) = match adopted {
        Some(adopted) => {
            adopted.shared.set_leader(db.clone(), counters.clone());
            (None, Some(adopted.addr))
        }
        None => match cfg.metrics_addr()? {
            Some(a) => {
                let serving = crate::metrics::serve(a, db.clone(), counters.clone()).await?;
                let addr = serving.addr;
                (Some(serving), Some(addr))
            }
            None => (None, None),
        },
    };

    tracing::info!(
        dir = %dir.display(),
        flight = %flight_addr,
        tls = cfg.server.flight.tls.is_enabled(),
        mtls = cfg.server.flight.tls.client_ca.is_some(),
        principals = cfg.auth.principals.len(),
        anonymous = ?cfg.auth.anonymous,
        metrics = ?metrics_addr,
        shards = db.shard_count(),
        visible = db.visible(),
        epoch = db.epoch(),
        durable = db.is_durable(),
        term = db.term(),
        "yesnod is serving"
    );

    Ok(Running {
        flight_addr,
        metrics_addr,
        control_addr: None,
        control_socket: None,
        stop,
        served,
        control: None,
        control_commands: None,
        ticker,
        metrics,
        counters,
        db,
        grace: Duration::from_secs(cfg.server.shutdown_grace_secs),
    })
}

impl Running {
    /// A handle on the database, for callers that need to look at it. Drop it
    /// before calling [`Running::shutdown`], or `sole_owner` will report your
    /// clone.
    pub fn db(&self) -> Arc<Db> {
        self.db.clone()
    }

    /// The maintenance counters, for `/metrics` and for tests.
    pub fn counters(&self) -> Arc<crate::maintenance::Counters> {
        self.counters.clone()
    }

    /// Stop accepting, drain, checkpoint, and release the lock.
    pub async fn shutdown(self) -> Teardown {
        self.shutdown_for(yesno_core::events::ShutdownReason::Requested)
            .await
    }

    /// Shutdown with the lifecycle reason recorded by yesno-core.
    pub async fn shutdown_for(self, reason: yesno_core::events::ShutdownReason) -> Teardown {
        self.db.begin_shutdown(reason);
        let Running {
            stop,
            served,
            control,
            ticker,
            metrics,
            db,
            grace,
            counters,
            ..
        } = self;
        drop(counters);

        // 1. Stop accepting. `serve_with_incoming_shutdown` signals every live
        //    connection and then waits for each connection task to drop its
        //    handle, so awaiting the join is "every in-flight RPC finished or
        //    its connection closed".
        let _ = stop.send(());
        match tokio::time::timeout(grace, served).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::error!(error = %e, "the Flight listener task panicked"),
            Err(_) => tracing::warn!(
                grace_secs = grace.as_secs(),
                "the Flight listener did not finish within the grace period"
            ),
        }

        if let Some(control) = control {
            control.stop().await;
        }

        // 1a. Stop the metrics listener, and stop the ticker — the latter with
        //     an await, so a checkpoint already in flight finishes before the
        //     final one below rather than racing it.
        if let Some(m) = metrics {
            m.stop().await;
        }
        ticker.stop().await;

        // 2. Drain the blocking readers.
        //
        // Step 1 does **not** cover these. `do_get` hands its work to
        // `spawn_blocking`, and a blocking task is not cancellable — the
        // runtime can stop *waiting* for it but cannot abort the thread. A
        // reader notices its client is gone at the next `blocking_send`, which
        // is within one batch, but the first `load` of a large key runs before
        // there is any batch to send. `live_readers()` counts registered
        // reader slots, and a registered slot is a live `Snapshot` holding the
        // lock, so this is the exact quantity and not a proxy for it.
        let deadline = Instant::now() + grace;
        let mut readers_left = db.live_readers();
        let mut announced = false;
        while readers_left > 0 && Instant::now() < deadline {
            if !announced {
                tracing::info!(readers = readers_left, "waiting for readers to drain");
                announced = true;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
            readers_left = db.live_readers();
        }
        if readers_left > 0 {
            tracing::warn!(
                readers = readers_left,
                "readers did not drain within the grace period; the database lock will not \
                 be released until they do"
            );
        }

        // 3. One last checkpoint, on the blocking pool.
        //
        // Its failure is logged and not fatal: the WAL is the truth and
        // recovery is redo-only, so a failed checkpoint costs a longer replay
        // at next start and nothing else. Refusing to exit would be worse.
        let checkpoint = {
            let db = db.clone();
            match tokio::task::spawn_blocking(move || db.checkpoint()).await {
                Ok(Ok(w)) => Some(w),
                Ok(Err(e)) => {
                    tracing::error!(error = ?e, "the final checkpoint failed");
                    None
                }
                Err(e) => {
                    tracing::error!(error = %e, "the final checkpoint panicked");
                    None
                }
            }
        };

        // 4. Prove it. `into_inner` answers `Some` only when this was the last
        //    `Arc<Db>`; anything else means a task kept a handle, and the next
        //    start would fail with `AlreadyOpen` for a reason nothing recorded.
        let sole_owner = match Arc::into_inner(db) {
            Some(db) => {
                drop(db);
                true
            }
            None => {
                tracing::error!(
                    "a task still holds a database handle at shutdown; the file lock will \
                     not be released until this process exits"
                );
                false
            }
        };

        let t = Teardown {
            checkpoint,
            readers_left,
            sole_owner,
        };
        // One line, because this is what an operator greps for.
        tracing::info!(
            checkpoint = ?t.checkpoint,
            readers_left = t.readers_left,
            lock_released = t.sole_owner,
            "shutdown complete"
        );
        t
    }
}

/// Resolve on SIGTERM or SIGINT, whichever arrives first.
#[cfg(unix)]
pub async fn terminated() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
    let mut int = signal(SignalKind::interrupt()).expect("SIGINT handler");
    tokio::select! {
        _ = term.recv() => tracing::info!("SIGTERM"),
        _ = int.recv() => tracing::info!("SIGINT"),
    }
}

#[cfg(not(unix))]
pub async fn terminated() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("interrupt");
}
