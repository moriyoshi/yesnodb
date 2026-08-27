//! `--role follower`: a cold standby that keeps a directory caught up.
//!
//! # Cold, and that is the whole design
//!
//! **It opens no `Db`.** The database's exclusive `flock` stays free, which is
//! what makes promotion a plain `Db::open_with` rather than a negotiation — and
//! `Db::open`'s recovery **truncates** any unresolved tail, so a promoted
//! standby starts from a resolved prefix for free. An in-process promotion of a
//! *live* replica would have to reimplement that truncation, and getting it
//! wrong stalls the visible watermark permanently.
//!
//! The cost is that a standby answers no queries. That is the honest trade for
//! v1: a replica that serves reads while following needs a synchronous apply
//! path in `yesno-core` that does not exist yet.
//!
//! # The error taxonomy is the interesting part
//!
//! `FollowerError` already distinguishes the cases; what a daemon adds is
//! reacting to them differently, and two of them look alike and mean opposite
//! things:
//!
//! | condition | response |
//! |---|---|
//! | transport, or the leader is down | reconnect, capped exponential backoff |
//! | `FailedPrecondition` — "the log was cut by a checkpoint" | re-bootstrap that shard, automatically |
//! | `OutOfRange` — "follower is at N, past this leader's M" | **stop and alert** |
//! | `WrongLeader` | **stop and alert** |
//!
//! **`OutOfRange` must never auto-rebuild.** It means this node holds records
//! the leader does not, which after a failover is exactly the standby that was
//! *ahead* of the one promoted. Rebuilding discards the evidence of what was
//! lost, silently, at the moment somebody most needs to know. Stopping is the
//! only response that preserves it.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::replication::pb::replication_client::ReplicationClient;
use crate::replication::{pb, FollowerClient, FollowerError};
use tonic::transport::Channel;

use crate::config::{Config, ConfigError};

/// What the standby is doing, for `/metrics` and for tests.
#[derive(Debug, Default)]
pub struct FollowerStatus {
    pub connected: AtomicBool,
    /// Versions at or below this are complete here. A lower bound after a
    /// restart — see `FollowerClient::resume_from_disk`.
    pub visible: AtomicU64,
    /// Bytes of log applied since this process started.
    pub applied_bytes: AtomicU64,
    pub records: AtomicU64,
    pub passes: AtomicU64,
    pub catchup_failures: AtomicU64,
    /// Shards re-bootstrapped because the leader reclaimed a generation they wanted.
    pub rebootstraps: AtomicU64,
    /// Set when the loop has stopped for a reason a retry cannot fix.
    pub halted: AtomicBool,
    /// True while a read-serving standby holds its database open.
    pub serving: AtomicBool,
    /// The message that halted it, if any.
    pub halt_reason: std::sync::Mutex<Option<String>>,
}

impl FollowerStatus {
    pub fn halt(&self, why: String) {
        tracing::error!(reason = %why, "the standby has stopped following");
        *self.halt_reason.lock().unwrap() = Some(why);
        self.halted.store(true, Ordering::Release);
    }
}

/// The database a read-serving standby is currently holding, if any.
///
/// **Empty while it rebuilds, and that interval is unavoidable.**
/// `bootstrap_shard` truncates the shard image, and truncating under a live
/// mapping raises `SIGBUS` — which I6 states is not catchable as a `Result` — so
/// a standby that has fallen off its leader's retained log genuinely must close
/// the database before it can take a fresh copy. The slot is what lets the port
/// stay bound and answer `unavailable` across that gap instead of dropping every
/// connection.
pub type DbSlot = crate::guard::DbSlot;

/// A running standby.
pub struct FollowerNode {
    stop: tokio::sync::watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
    pub status: Arc<FollowerStatus>,
    /// The database this standby serves reads from, when it serves any.
    pub db: Option<DbSlot>,
}

impl FollowerNode {
    pub async fn stop(self) {
        self.stop_for(yesno_core::events::ShutdownReason::Requested)
            .await;
    }

    /// Stop following and mark any read-serving replica with its lifecycle cause.
    pub async fn stop_for(self, reason: yesno_core::events::ShutdownReason) {
        if let Some(slot) = &self.db {
            if let Ok(guard) = slot.read() {
                if let Some(db) = guard.as_ref() {
                    db.begin_shutdown(reason);
                }
            }
        }
        let _ = self.stop.send(true);
        let _ = self.task.await;
    }

    /// Whether the loop has given up. An orchestrator should treat this the way
    /// it treats a crash: the node needs a human or a rebuild, not a restart.
    pub fn is_halted(&self) -> bool {
        self.status.halted.load(Ordering::Acquire)
    }
}

async fn connect(cfg: &Config) -> Result<Channel, Box<dyn std::error::Error + Send + Sync>> {
    let mut ep = Channel::from_shared(cfg.follower.leader.clone())?;
    let t = &cfg.follower.tls;
    if t.is_enabled() {
        let mut tls = tonic::transport::ClientTlsConfig::new();
        if let Some(ca) = &t.ca {
            tls = tls.ca_certificate(tonic::transport::Certificate::from_pem(std::fs::read(ca)?));
        }
        if let (Some(c), Some(k)) = (&t.cert, &t.key) {
            tls = tls.identity(tonic::transport::Identity::from_pem(
                std::fs::read(c)?,
                std::fs::read(k)?,
            ));
        }
        if let Some(d) = &t.domain {
            tls = tls.domain_name(d.clone());
        }
        ep = ep.tls_config(tls)?;
    }
    Ok(ep.connect().await?)
}

/// How a failure should be answered.
enum Reaction {
    /// Retry after a backoff. The leader may simply be restarting.
    Retry,
    /// Re-bootstrap this shard: the leader cut log we still wanted.
    Rebootstrap,
    /// Stop. A retry cannot fix this and rebuilding would destroy evidence.
    Halt(String),
}

fn react(shard: u32, e: &FollowerError) -> Reaction {
    match e {
        // The zombie. A superseded leader is the *same database* — same
        // `db_uuid`, same shard files — so identity cannot refuse it and only
        // the term can. Retrying is exactly wrong: the endpoint is not
        // temporarily unwell, it is serving a timeline that has been abandoned,
        // and every retry is another chance for somebody to point this standby
        // at it during a real outage.
        FollowerError::StaleLeader { seen, offered } => Reaction::Halt(format!(
            "shard {shard}: that endpoint is serving leadership term {offered} and this \
             standby has already seen term {seen}. It is an older leadership of the same \
             database — a leader that was replaced and came back — and following it would \
             apply records from a timeline that no longer exists. Point this standby at the \
             current leader, or wipe and re-bootstrap it."
        )),
        FollowerError::WrongLeader { expected, found } => Reaction::Halt(format!(
            "shard {shard}: this standby belongs to database {} and the endpoint answered \
             for {}. Refusing to write a byte. Check `follower.leader`.",
            hex16(expected),
            hex16(found)
        )),
        // `Transport`, not `Stream`. `Transport` carries the **leader's**
        // gRPC status, which is where `FailedPrecondition` and `OutOfRange`
        // arrive; `Stream` is a local decode or continuity failure and says
        // nothing about what the leader thinks.
        FollowerError::Transport(st) => match st.code() {
            tonic::Code::FailedPrecondition => Reaction::Rebootstrap,
            // This standby is ahead of the leader. After a failover that is
            // the node that was *further along* than the one promoted, and its
            // extra records are gone from the timeline. Rebuilding would erase
            // the only evidence of what was lost.
            tonic::Code::OutOfRange => Reaction::Halt(format!(
                "shard {shard}: this standby is ahead of the leader ( {} ). It holds records \
                 the leader does not, which after a failover means they were lost when \
                 another node was promoted. Refusing to rebuild automatically: record what \
                 this node has, then wipe and re-bootstrap deliberately.",
                st.message()
            )),
            _ => Reaction::Retry,
        },
        // A CRC mismatch or a batch that did not continue the cursor. Retrying
        // is right — the next pass re-reads from the same cursor — and it will
        // keep failing loudly rather than silently, which `catchup_failures`
        // makes visible.
        FollowerError::Stream(_) | FollowerError::Io(_) | FollowerError::Topology(_) => {
            Reaction::Retry
        }
    }
}

fn hex16(b: &[u8; 16]) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    for x in b {
        let _ = write!(s, "{x:02x}");
    }
    s
}

/// Start following. Returns as soon as the loop is running; it does not wait
/// for the standby to be caught up.
pub fn start(cfg: &Config) -> Result<FollowerNode, ConfigError> {
    start_with_events(cfg, None)
}

/// The observed form of start, used by yesnod's control plane.
pub fn start_with_events(
    cfg: &Config,
    event_sink: Option<Arc<dyn yesno_core::events::CoreEventSink>>,
) -> Result<FollowerNode, ConfigError> {
    let dir = cfg.data_dir().to_path_buf();
    let status = Arc::new(FollowerStatus::default());
    let (stop, mut stopped) = tokio::sync::watch::channel(false);

    // Only a read-serving standby has one; a cold one never opens a database.
    let slot: Option<DbSlot> = cfg
        .follower
        .serve_reads
        .then(|| Arc::new(std::sync::RwLock::new(None)));

    let cfg = cfg.clone();
    let st = status.clone();
    let db_slot = slot.clone();
    let task = tokio::spawn(async move {
        let poll = Duration::from_secs(cfg.follower.poll_interval_secs.max(1));
        let max_backoff = Duration::from_secs(cfg.follower.max_backoff_secs.max(1));
        let mut backoff = Duration::from_millis(250);

        // The connection, rebuilt on transport failure.
        let mut client: Option<ReplicationClient<Channel>> = None;

        loop {
            if *stopped.borrow() || st.halted.load(Ordering::Acquire) {
                return;
            }

            // ---- connect
            let c = match client.as_mut() {
                Some(c) => c,
                None => match connect(&cfg).await {
                    Ok(ch) => {
                        tracing::info!(leader = %cfg.follower.leader, "connected");
                        st.connected.store(true, Ordering::Release);
                        backoff = Duration::from_millis(250);
                        client = Some(ReplicationClient::new(ch));
                        client.as_mut().expect("just set")
                    }
                    Err(e) => {
                        st.connected.store(false, Ordering::Release);
                        tracing::warn!(error = %e, backoff_ms = backoff.as_millis(), "cannot reach the leader");
                        if sleep_or_stop(&mut stopped, backoff).await {
                            return;
                        }
                        backoff = (backoff * 2).min(max_backoff);
                        continue;
                    }
                },
            };

            match one_pass(&cfg, &dir, c, &st, db_slot.as_ref(), event_sink.as_ref()).await {
                Ok(moved) => {
                    st.passes.fetch_add(1, Ordering::Relaxed);
                    backoff = Duration::from_millis(250);
                    // Only wait when there was nothing to do; a standby that is
                    // behind should stay in the loop.
                    if moved == 0 && sleep_or_stop(&mut stopped, poll).await {
                        return;
                    }
                }
                Err(()) => {
                    // `one_pass` has already classified and recorded it.
                    if st.halted.load(Ordering::Acquire) {
                        return;
                    }
                    st.connected.store(false, Ordering::Release);
                    client = None;
                    if sleep_or_stop(&mut stopped, backoff).await {
                        return;
                    }
                    backoff = (backoff * 2).min(max_backoff);
                }
            }
        }
    });

    Ok(FollowerNode {
        stop,
        task,
        status,
        db: slot,
    })
}

/// Sleep, unless asked to stop first. Returns `true` when it is time to go.
async fn sleep_or_stop(stopped: &mut tokio::sync::watch::Receiver<bool>, d: Duration) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(d) => false,
        _ = stopped.changed() => true,
    }
}

/// One sweep over every shard. `Ok(n)` is the bytes applied.
async fn one_pass(
    cfg: &Config,
    dir: &std::path::Path,
    client: &mut ReplicationClient<Channel>,
    st: &Arc<FollowerStatus>,
    slot: Option<&DbSlot>,
    event_sink: Option<&Arc<dyn yesno_core::events::CoreEventSink>>,
) -> Result<u64, ()> {
    // The shard count comes from the leader, not from the config: it is a
    // property of the database, and a standby that guessed would bootstrap
    // shards that do not exist.
    let status = match client.status(pb::StatusRequest {}).await {
        Ok(r) => r.into_inner(),
        Err(e) => {
            tracing::warn!(error = %e, "status failed");
            st.catchup_failures.fetch_add(1, Ordering::Relaxed);
            return Err(());
        }
    };
    let shards = status.shard_count;

    // Rebuilt each pass from what is on disk, so a restart resumes where the
    // files say rather than from zero. See `resume_from_disk`.
    let mut f = match FollowerClient::resume_from_disk(dir, shards) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(error = %e, "cannot read this standby's own state");
            st.catchup_failures.fetch_add(1, Ordering::Relaxed);
            return Err(());
        }
    };

    // The MANIFEST first: without it this node has no identity and no routing
    // map, and `check_leader` inside every call below depends on it.
    if let Err(e) = f.fetch_manifest(client).await {
        match react(0, &e) {
            Reaction::Halt(why) => {
                st.halt(why);
                return Err(());
            }
            _ => {
                tracing::warn!(error = %e, "cannot seed this standby's MANIFEST");
                st.catchup_failures.fetch_add(1, Ordering::Relaxed);
                return Err(());
            }
        }
    }

    let budget = cfg.follower.max_batch_bytes;
    let mut moved = 0u64;

    // ---- phase one: bootstrap whatever has no image, with the database closed
    //
    // **Every shard first, and only then open.** `bootstrap_shard` truncates
    // the shard image, and truncating under a live mapping raises `SIGBUS`,
    // which I6 states is not catchable as a `Result` — it kills the process
    // rather than returning an error. So the database has to be shut for the
    // whole bootstrap.
    //
    // Hoisted out of the per-shard loop deliberately. Interleaved, a
    // four-shard first sweep closed and reopened the database four times, so a
    // read-serving standby flapped between "serving" and "rebuilding" once per
    // shard while it came up — visible to clients as intermittent `unavailable`
    // for a node that was simply starting. One close, one open.
    let needs_bootstrap: Vec<u32> = (0..shards)
        .filter(|s| !dir.join(format!("shard-{s:04}.yno")).exists() || f.cursor(*s).is_none())
        .collect();
    if !needs_bootstrap.is_empty() {
        close_for_rebuild(slot, st);
        for shard in &needs_bootstrap {
            match f.bootstrap_shard(client, *shard).await {
                Ok(off) => tracing::info!(shard, replay_from = off, "bootstrapped"),
                Err(e) => return classify(*shard, e, st),
            }
        }
    }

    // ---- phase two: stream
    for shard in 0..shards {
        if st.halted.load(Ordering::Acquire) {
            return Err(());
        }
        let from = f.cursor(shard).map(|c| c.next_lsn).unwrap_or(0);

        // ---- the live path
        //
        // A read-serving standby applies **through the open database**, not by
        // writing its log file. Doing both would put two writers on one
        // `shard-NNNN.wal` — this client and the `Db`'s own `WalWriter`, which
        // holds `len`, `base` and `synced` in memory — and the failure is a log
        // that decodes to the first foreign byte and then silently stops.
        if let Some(slot) = slot {
            let db = open_if_needed(slot, cfg, dir, st, event_sink)?;
            match f.apply_shard_into(client, &db, shard, budget).await {
                Ok(c) => {
                    moved += c.bytes;
                    st.applied_bytes.fetch_add(c.bytes, Ordering::Relaxed);
                    st.records.fetch_add(c.records, Ordering::Relaxed);
                }
                Err(e) => {
                    if matches!(react(shard, &e), Reaction::Rebootstrap) {
                        tracing::warn!(shard, error = %e, "re-bootstrapping: the leader reclaimed the required WAL generation");
                        st.rebootstraps.fetch_add(1, Ordering::Relaxed);
                        close_for_rebuild(Some(slot), st);
                        let _ = std::fs::remove_file(dir.join(format!("shard-{shard:04}.wal")));
                        let _ = std::fs::remove_file(dir.join(format!("shard-{shard:04}.yno")));
                        // The next pass finds no image and runs phase one.
                        return Err(());
                    }
                    return classify(shard, e, st);
                }
            }
            if let Err(e) = f.ack(client, shard).await {
                return classify(shard, e, st);
            }
            continue;
        }

        match f.catch_up_shard(client, shard, from, budget).await {
            Ok(c) => {
                moved += c.bytes;
                st.applied_bytes.fetch_add(c.bytes, Ordering::Relaxed);
                st.records.fetch_add(c.records, Ordering::Relaxed);
                if c.bytes > 0 {
                    tracing::debug!(shard, bytes = c.bytes, next = c.next_lsn, "caught up");
                }
            }
            Err(e) => {
                // The recoverable one, and the only failure this loop repairs
                // by itself: the leader cut log this standby still wanted, so the
                // image is stale and a fresh one is the answer.
                if matches!(react(shard, &e), Reaction::Rebootstrap) {
                    tracing::warn!(shard, error = %e, "re-bootstrapping: the leader reclaimed the required WAL generation");
                    st.rebootstraps.fetch_add(1, Ordering::Relaxed);
                    close_for_rebuild(slot, st);
                    let _ = std::fs::remove_file(dir.join(format!("shard-{shard:04}.wal")));
                    match f.bootstrap_shard(client, shard).await {
                        Ok(off) => {
                            if let Err(e) = f.catch_up_shard(client, shard, off, budget).await {
                                return classify(shard, e, st);
                            }
                        }
                        Err(e) => return classify(shard, e, st),
                    }
                } else {
                    return classify(shard, e, st);
                }
            }
        }

        // Telling the leader is what makes it hold log back for us.
        if let Err(e) = f.ack(client, shard).await {
            return classify(shard, e, st);
        }
    }

    st.visible.store(f.visible(), Ordering::Release);
    Ok(moved)
}

/// Put the database away, so a truncating bootstrap is safe.
fn close_for_rebuild(slot: Option<&DbSlot>, st: &Arc<FollowerStatus>) {
    let Some(slot) = slot else { return };
    let taken = slot.write().map(|mut g| g.take()).unwrap_or(None);
    if taken.is_some() {
        tracing::warn!(
            "closing the database to rebuild it; reads are unavailable until it reopens"
        );
        st.serving.store(false, Ordering::Release);
    }
    // Dropping the last `Arc<Db>` is what releases the directory lock, and an
    // in-flight `do_get` holds a `Snapshot` — which holds an `Arc<DbInner>` — so
    // the lock may outlive this by the length of one read. The bootstrap below
    // opens no `Db` of its own, so it does not contend; what it must not do is
    // truncate a *mapped* file, and the mapping goes with the last reader.
    if let Some(db) = taken.as_ref() {
        db.begin_shutdown(yesno_core::events::ShutdownReason::Rebuild);
    }
    drop(taken);
}

/// Open the replica if the slot is empty, and hand back a handle.
fn open_if_needed(
    slot: &DbSlot,
    cfg: &Config,
    dir: &std::path::Path,
    st: &Arc<FollowerStatus>,
    event_sink: Option<&Arc<dyn yesno_core::events::CoreEventSink>>,
) -> Result<Arc<yesno_core::Db>, ()> {
    if let Ok(g) = slot.read() {
        if let Some(db) = g.as_ref() {
            return Ok(db.clone());
        }
    }
    // `open_replica`, not `open_with`. A normal open appends an `EpochFence`
    // to every shard's log, and on a replica that is a locally generated frame
    // in an LSN space belonging to the leader — after which the next shipped
    // frame lands where its header does not say, the scan stops, and the log
    // silently ends there.
    let opened = match event_sink {
        Some(sink) => yesno_core::Db::open_replica_with_events(dir, cfg.db.into(), sink.clone()),
        None => yesno_core::Db::open_replica(dir, cfg.db.into()),
    };
    match opened {
        Ok(db) => {
            let db = Arc::new(db);
            if let Ok(mut g) = slot.write() {
                *g = Some(db.clone());
            }
            st.serving.store(true, Ordering::Release);
            tracing::info!(term = db.term(), "serving reads from the replica");
            Ok(db)
        }
        Err(e) => {
            tracing::warn!(error = ?e, "cannot open the replica");
            st.catchup_failures.fetch_add(1, Ordering::Relaxed);
            Err(())
        }
    }
}

fn classify(shard: u32, e: FollowerError, st: &Arc<FollowerStatus>) -> Result<u64, ()> {
    match react(shard, &e) {
        Reaction::Halt(why) => st.halt(why),
        _ => {
            tracing::warn!(shard, error = %e, "catch-up failed");
            st.catchup_failures.fetch_add(1, Ordering::Relaxed);
        }
    }
    Err(())
}
