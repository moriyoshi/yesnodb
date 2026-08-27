//! `/metrics`, `/healthz`, `/readyz`, on their own plain-HTTP listener.
//!
//! # Why not extend `do_action( "stats" )`
//!
//! Three reasons, and the first is decisive on its own.
//!
//! 1. Every monitoring system speaks `GET /metrics`. Requiring Prometheus to
//!    hold an authenticated Arrow Flight client to scrape a database is a
//!    non-starter, and so is asking a load balancer to speak gRPC for a health
//!    check.
//! 2. `do_action( "stats" )` is a public, database-scoped protobuf contract in
//!    the shipped `yesno-flight` library. Process health does not belong in
//!    that message merely because both surfaces contain counters.
//! 3. Health and readiness are properties of the *process*, not of the
//!    database, and the Flight surface has nowhere to say them.
//!
//! # One listener, across role changes
//!
//! **The listener outlives the role; it is not owned by it.** A follower and
//! a leader render different `/metrics` and answer `/readyz` differently, but
//! they bind the *same* configured address. Serving them from two listeners
//! makes a promotion a stop-then-rebind, and the port is then closed for the
//! whole of the follower's close plus the leader's open -- WAL replay and
//! checkpoint load included.
//!
//! Kubernetes points its **liveness** probe at that port. The operator sets
//! `periodSeconds: 10, failureThreshold: 3`, so roughly 30 s of that window
//! kills the container -- during a promotion, which then loses the promotion
//! outright, because `Promote` is accept-and-journal and the restarted process
//! comes back a follower with no memory of it. Demotion has the same shape.
//!
//! So `Shared` holds the role-dependent half behind a lock and `serve_shared`
//! binds once, for the life of the process. `run_node` owns it; `follow` and
//! `start_with_events` swap the surface rather than binding their own.
//! Do not reintroduce a per-role listener, and do not "fix" a probe
//! failure here by raising `failureThreshold` -- that hides the window and
//! slows detection of a genuine hang.
//!
//! # Rendered on demand, with no registry crate
//!
//! Prometheus' text format is a dozen `write!` calls over getters the `Db`
//! already exposes, so a metrics framework would be a dependency earning
//! nothing. There is no background sampling either: a scrape reads the counters
//! at the moment it is asked.
//!
//! **Every `Db` getter here takes `store.lock()` once per shard.** At 8 shards
//! and a 15-second scrape that is nothing; at 1 Hz during a large checkpoint it
//! is measurable. Scrape no faster than 15 s rather than adding a cache, because
//! a cache would make the numbers lie about *when* they were true.
//!
//! Deliberately not exposed here: `Db::verify()`, which is a full index walk
//! per shard, and `live_fractions()`, which allocates per slab. Those are
//! diagnostics, and a diagnostic on a scrape path becomes a load generator.

use std::fmt::Write as _;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::Router;
use yesno_core::Db;

use crate::maintenance::Counters;

#[derive(Clone)]
pub struct Metrics {
    db: Arc<Db>,
    counters: Arc<Counters>,
    role: &'static str,
}

/// A running metrics listener.
pub struct Serving {
    pub addr: std::net::SocketAddr,
    stop: tokio::sync::oneshot::Sender<()>,
    handle: tokio::task::JoinHandle<()>,
}

impl Serving {
    pub async fn stop(self) {
        let _ = self.stop.send(());
        let _ = self.handle.await;
    }
}

pub async fn serve(
    addr: std::net::SocketAddr,
    db: Arc<Db>,
    counters: Arc<Counters>,
) -> std::io::Result<Serving> {
    let state = Metrics {
        db,
        counters,
        role: "leader",
    };
    let app = Router::new()
        .route("/metrics", get(metrics))
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .with_state(state);

    bind(addr, app).await
}

/// Bind one plain-HTTP listener and serve `app` on it until [`Serving::stop`].
///
/// Shared by every role's listener so they cannot drift in how they bind,
/// report their actual address, or shut down.
async fn bind(addr: std::net::SocketAddr, app: Router) -> std::io::Result<Serving> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let addr = listener.local_addr()?;
    let (stop, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let handle = tokio::spawn(async move {
        let r = axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = stop_rx.await;
            })
            .await;
        if let Err(e) = r {
            tracing::error!(error = %e, "the metrics listener stopped with an error");
        }
    });
    Ok(Serving { addr, stop, handle })
}

/// A listener for a process that renders its own Prometheus text.
///
/// The archive sidecar is a *separate process* from the daemon, so it cannot
/// share the leader's listener — but it must not grow a second, subtly
/// different way to answer `GET /metrics` either. It therefore supplies a
/// renderer, built from [`gauge`] and [`counter`] like every family here, and
/// borrows this crate's listener, content type, and `/healthz`.
///
/// No `/readyz`. Readiness answers "should this node be sent traffic", and
/// nothing sends traffic to a sidecar; an endpoint that always said `ready`
/// would be a claim nobody may act on.
pub async fn serve_text(
    addr: std::net::SocketAddr,
    render: Arc<dyn Fn() -> String + Send + Sync>,
) -> std::io::Result<Serving> {
    let app = Router::new()
        .route("/metrics", get(text_metrics))
        .route("/healthz", get(healthz))
        .with_state(TextMetrics { render });
    bind(addr, app).await
}

#[derive(Clone)]
struct TextMetrics {
    render: Arc<dyn Fn() -> String + Send + Sync>,
}

async fn text_metrics(
    State(m): State<TextMetrics>,
) -> (StatusCode, [(&'static str, &'static str); 1], String) {
    (
        StatusCode::OK,
        [("content-type", "text/plain; version=0.0.4; charset=utf-8")],
        (m.render)(),
    )
}

/// The role-dependent half of the surface, swapped in place.
///
/// **This type exists so that one listener can outlive a role change.**
/// A follower and a leader render different `/metrics` and answer `/readyz`
/// differently, but they bind the *same* `metrics_addr`. Serving them from two
/// listeners means promotion is a stop-then-rebind, and the port is closed for
/// the whole of the follower's close plus the leader's open -- WAL replay and
/// checkpoint load included.
///
/// That window is judged by the liveness probe. The Kubernetes operator sets
/// `periodSeconds: 10, failureThreshold: 3`, so roughly 30 s of it gets the
/// container killed -- during a promotion, which loses the promotion. Demotion
/// has the identical shape in the other direction.
enum Surface {
    /// Before a role has opened anything. `/healthz` already answers here,
    /// which is the point: the process is alive well before it is ready.
    Starting,
    Follower(FollowerMetrics),
    Leader(Metrics),
}

/// A metrics surface whose role can change without closing the listener.
#[derive(Clone)]
pub struct Shared(Arc<std::sync::RwLock<Surface>>);

impl Default for Shared {
    fn default() -> Self {
        Self::new()
    }
}

impl Shared {
    pub fn new() -> Self {
        Shared(Arc::new(std::sync::RwLock::new(Surface::Starting)))
    }

    pub fn set_follower(&self, status: Arc<crate::follower::FollowerStatus>) {
        *self.0.write().unwrap() = Surface::Follower(FollowerMetrics { status });
    }

    pub fn set_leader(&self, db: Arc<Db>, counters: Arc<Counters>) {
        *self.0.write().unwrap() = Surface::Leader(Metrics {
            db,
            counters,
            role: "leader",
        });
    }
}

/// A listener the caller already bound, handed to a role so it can take the
/// surface over instead of binding its own.
pub struct Adopted {
    pub addr: std::net::SocketAddr,
    pub shared: Shared,
}

/// Serve on one listener whose role state may change underneath it.
pub async fn serve_shared(addr: std::net::SocketAddr, shared: Shared) -> std::io::Result<Serving> {
    let app = Router::new()
        .route("/metrics", get(shared_metrics))
        .route("/healthz", get(healthz))
        .route("/readyz", get(shared_readyz))
        .with_state(shared);
    bind(addr, app).await
}

// These read the lock and return; nothing is held across an `.await`, which
// is what keeps a `std::sync::RwLock` correct in an async handler.
async fn shared_readyz(State(s): State<Shared>) -> (StatusCode, String) {
    match &*s.0.read().unwrap() {
        Surface::Starting => (StatusCode::SERVICE_UNAVAILABLE, "starting\n".into()),
        Surface::Follower(f) => follower_readyz_of(f),
        Surface::Leader(m) => readyz_of(m),
    }
}

async fn shared_metrics(
    State(s): State<Shared>,
) -> (StatusCode, [(&'static str, &'static str); 1], String) {
    let body = match &*s.0.read().unwrap() {
        // Not the leader's gauges with zeros in them, for the reason
        // `FollowerMetrics` gives: an absence beats a false claim.
        Surface::Starting => String::new(),
        Surface::Follower(f) => render_follower(f),
        Surface::Leader(m) => render(m),
    };
    (
        StatusCode::OK,
        [("content-type", "text/plain; version=0.0.4; charset=utf-8")],
        body,
    )
}

/// Liveness: the process is up and answering. Deliberately says nothing about
/// the database — a `/healthz` that fails on a database problem gets the
/// container killed and restarted into the same problem, having thrown away the
/// diagnostics.
async fn healthz() -> &'static str {
    "ok\n"
}

/// Readiness: should this node be sent traffic.
///
/// Fails while checkpoints are failing. Such a node still serves reads
/// correctly, so this is not a liveness question — but its log and memtable are
/// growing without bound and it will eventually stall writers, so it is exactly
/// the node a load balancer should stop favouring.
async fn readyz(State(m): State<Metrics>) -> (StatusCode, String) {
    readyz_of(&m)
}

fn readyz_of(m: &Metrics) -> (StatusCode, String) {
    if m.counters.checkpoint_failing.load(Ordering::Relaxed) {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "checkpoints are failing\n".into(),
        );
    }
    (StatusCode::OK, "ready\n".into())
}

async fn metrics(
    State(m): State<Metrics>,
) -> (StatusCode, [(&'static str, &'static str); 1], String) {
    (
        StatusCode::OK,
        [("content-type", "text/plain; version=0.0.4; charset=utf-8")],
        render(&m),
    )
}

fn render(m: &Metrics) -> String {
    let db = &m.db;
    let c = &m.counters;
    let mut s = String::with_capacity(4096);

    gauge(
        &mut s,
        "build_info",
        "Always 1; the labels carry the version and role.",
        format!(
            "{{version=\"{}\",role=\"{}\"}} 1",
            env!("CARGO_PKG_VERSION"),
            m.role
        ),
    );

    // ---- space
    gauge(
        &mut s,
        "allocated_bytes",
        "File space allocated to live extents.",
        db.allocated_bytes().to_string(),
    );
    gauge(
        &mut s,
        "space_amplification_permille",
        "Allocated over live, in parts per thousand; 2000 is the hard bound.",
        db.space_amplification_permille().to_string(),
    );
    gauge(
        &mut s,
        "oldest_reader_age_seconds",
        "Age of the longest-lived live snapshot. Evicted readers are skipped; they no longer pin retention.",
        db.oldest_reader_age_secs().to_string(),
    );
    gauge(
        &mut s,
        "snapshot_age_soft_breached",
        "1 while a snapshot is older than the configured soft age. Observation only.",
        u8::from(db.snapshot_age_soft_breached()).to_string(),
    );
    gauge(
        &mut s,
        "space_amp_soft_breached",
        "1 while amplification is above the configured soft threshold. Observation only; nothing is evicted for it.",
        u8::from(db.space_amp_soft_breached()).to_string(),
    );
    gauge(
        &mut s,
        "deferred_bytes",
        "Superseded extents awaiting reclamation.",
        db.deferred_bytes().to_string(),
    );
    gauge(
        &mut s,
        "dirty_bytes",
        "Memtable bytes not yet checkpointed.",
        db.dirty_bytes().to_string(),
    );
    gauge(
        &mut s,
        "wal_bytes",
        "Write-ahead log bytes retained.",
        db.wal_bytes().to_string(),
    );
    gauge(
        &mut s,
        "space_amplification",
        "Allocated over live. Bounded at 2 by construction.",
        format!("{:.4}", db.space_amplification()),
    );
    gauge(
        &mut s,
        "used_extents",
        "Extents holding live data.",
        db.used_extents().to_string(),
    );
    gauge(
        &mut s,
        "deferred_extents",
        "Extents queued for reclamation.",
        db.deferred_extents().to_string(),
    );
    gauge(
        &mut s,
        "pinned_extents",
        "Extents a live zero-copy buffer still aliases.",
        db.pinned_extents().to_string(),
    );
    gauge(
        &mut s,
        "slab_count",
        "Slabs in the store.",
        db.slab_count().to_string(),
    );

    // ---- versions and readers
    gauge(
        &mut s,
        "visible_version",
        "Versions at or below this are readable.",
        db.visible().to_string(),
    );
    gauge(
        &mut s,
        "safe_version",
        "Below this, no live reader can reach superseded state.",
        db.safe_version().to_string(),
    );
    gauge(
        &mut s,
        "live_readers",
        "Registered snapshots. Each pins the database's file lock.",
        db.live_readers().to_string(),
    );
    gauge(
        &mut s,
        "shards",
        "Shard count, as the MANIFEST records it.",
        db.shard_count().to_string(),
    );
    gauge(
        &mut s,
        "epoch",
        "The lock file's fencing epoch for this open.",
        db.epoch().to_string(),
    );
    gauge(
        &mut s,
        "durable",
        "1 when this database is file-backed.",
        u8::from(db.is_durable()).to_string(),
    );

    // ---- monotone engine counters
    counter(
        &mut s,
        "wal_syncs_total",
        "fsyncs of the write-ahead log.",
        db.wal_syncs(),
    );
    counter(
        &mut s,
        "freed_extents_total",
        "Extents returned to the allocator.",
        db.freed_extents(),
    );
    counter(
        &mut s,
        "index_nodes_written_total",
        "Index nodes written by checkpoints.",
        db.index_nodes_written(),
    );
    counter(
        &mut s,
        "index_nodes_freed_total",
        "Index nodes released by checkpoints.",
        db.index_nodes_freed(),
    );
    counter(
        &mut s,
        "evacuated_chunks_total",
        "Chunks relocated by compaction.",
        db.evacuated_chunks(),
    );
    counter(
        &mut s,
        "evicted_readers_total",
        "Snapshots the engine invalidated to hold the space bound.",
        db.evicted_reader_count() as u64,
    );

    // ---- what this process did
    counter(
        &mut s,
        "checkpoints_total",
        "Checkpoints the background driver completed.",
        c.checkpoints.load(Ordering::Relaxed),
    );
    counter(
        &mut s,
        "checkpoint_failures_total",
        "Background checkpoints that failed or panicked.",
        c.checkpoint_failures.load(Ordering::Relaxed),
    );
    counter(
        &mut s,
        "readers_evicted_total",
        "Readers the driver evicted for the space bound.",
        c.readers_evicted.load(Ordering::Relaxed),
    );
    gauge(
        &mut s,
        "last_checkpoint_ms",
        "Wall time of the most recent background checkpoint.",
        c.last_checkpoint_ms.load(Ordering::Relaxed).to_string(),
    );

    // Per-class slab occupancy. Cheap: one allocator lock per shard, same as
    // the gauges above.
    let _ = writeln!(s, "# HELP yesnod_slabs Slabs by size class.");
    let _ = writeln!(s, "# TYPE yesnod_slabs gauge");
    for (class, n) in db.slabs_by_class() {
        let _ = writeln!(s, "yesnod_slabs{{class=\"{class}\"}} {n}");
    }

    let (free, in_use, opaque) = db.slab_states();
    let _ = writeln!(s, "# HELP yesnod_slab_states Slabs by state.");
    let _ = writeln!(s, "# TYPE yesnod_slab_states gauge");
    let _ = writeln!(s, "yesnod_slab_states{{state=\"free\"}} {free}");
    let _ = writeln!(s, "yesnod_slab_states{{state=\"in_use\"}} {in_use}");
    let _ = writeln!(s, "yesnod_slab_states{{state=\"opaque\"}} {opaque}");

    s
}

/// Write one gauge family: a value that may move in either direction, and whose
/// meaning is "what this was when the scrape asked".
///
/// Public so that a role in another process — the archive sidecar — emits the
/// same `yesnod_` namespace, help text shape and type lines as the daemon
/// rather than an approximation of them.
pub fn gauge(s: &mut String, name: &str, help: &str, v: String) {
    let _ = writeln!(s, "# HELP yesnod_{name} {help}");
    let _ = writeln!(s, "# TYPE yesnod_{name} gauge");
    let _ = writeln!(s, "yesnod_{name} {v}");
}

/// Write one counter family: monotone within a process lifetime, reset to zero
/// by a restart. Never use it for a "last pass" reading, which is a gauge.
pub fn counter(s: &mut String, name: &str, help: &str, v: u64) {
    let _ = writeln!(s, "# HELP yesnod_{name} {help}");
    let _ = writeln!(s, "# TYPE yesnod_{name} counter");
    let _ = writeln!(s, "yesnod_{name} {v}");
}

// ---------------------------------------------------------------------------
// The standby's own listener
// ---------------------------------------------------------------------------

/// A standby has no `Db`, so it reports a different family entirely.
///
/// Deliberately **not** the leader's gauges with zeros in them. A dashboard
/// showing `yesnod_wal_bytes 0` for a node that has no database is worse than a
/// dashboard showing nothing: the first is a claim, the second is an absence.
#[derive(Clone)]
pub struct FollowerMetrics {
    status: Arc<crate::follower::FollowerStatus>,
}

/// Readiness for a standby: connected and still following.
///
/// A **halted** standby is unready and must stay that way. It has stopped for
/// something a retry cannot fix — it is pointed at the wrong database, or it
/// holds records the leader does not — and a node that looks ready while it is
/// no longer replicating is the one an operator promotes by mistake.
fn follower_readyz_of(m: &FollowerMetrics) -> (StatusCode, String) {
    let s = &m.status;
    if s.halted.load(Ordering::Relaxed) {
        let why = s
            .halt_reason
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| "halted".into());
        return (StatusCode::SERVICE_UNAVAILABLE, format!("{why}\n"));
    }
    if !s.connected.load(Ordering::Relaxed) {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "not connected to the leader\n".into(),
        );
    }
    (StatusCode::OK, "following\n".into())
}

fn render_follower(m: &FollowerMetrics) -> String {
    let s = &m.status;
    let mut out = String::with_capacity(1024);

    gauge(
        &mut out,
        "build_info",
        "Always 1; the labels carry the version and role.",
        format!(
            "{{version=\"{}\",role=\"follower\"}} 1",
            env!("CARGO_PKG_VERSION")
        ),
    );
    gauge(
        &mut out,
        "follower_connected",
        "1 while a connection to the leader is up.",
        u8::from(s.connected.load(Ordering::Relaxed)).to_string(),
    );
    gauge(
        &mut out,
        "follower_halted",
        "1 when following has stopped for something a retry cannot fix.",
        u8::from(s.halted.load(Ordering::Relaxed)).to_string(),
    );
    gauge(
        &mut out,
        "follower_visible_version",
        "Versions at or below this are complete here. A lower bound after a restart.",
        s.visible.load(Ordering::Relaxed).to_string(),
    );
    counter(
        &mut out,
        "follower_applied_bytes_total",
        "WAL bytes applied since this process started.",
        s.applied_bytes.load(Ordering::Relaxed),
    );
    counter(
        &mut out,
        "follower_records_total",
        "WAL records applied since this process started.",
        s.records.load(Ordering::Relaxed),
    );
    counter(
        &mut out,
        "follower_passes_total",
        "Completed sweeps over every shard.",
        s.passes.load(Ordering::Relaxed),
    );
    counter(
        &mut out,
        "follower_catchup_failures_total",
        "Passes that ended in a retryable failure.",
        s.catchup_failures.load(Ordering::Relaxed),
    );
    counter(
        &mut out,
        "follower_rebootstraps_total",
        "Shards rebuilt because the leader reclaimed a WAL generation they wanted.",
        s.rebootstraps.load(Ordering::Relaxed),
    );

    out
}
