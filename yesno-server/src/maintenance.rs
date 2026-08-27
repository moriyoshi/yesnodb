//! The checkpoint driver, and the counters it feeds.
//!
//! # Why a daemon has to own a ticker at all
//!
//! **`CheckpointPolicy::interval_secs` does not fire on its own — on a leader
//! either, not just on a replica.** `Db::enforce_policy` is reached only from
//! `WriteBatch::commit`, and `yesno-core` starts no background threads in
//! production. So on a database whose writes stop, the 60-second interval never
//! elapses into anything: dirty chunks sit in the memtable and the log keeps
//! whatever it was holding, indefinitely. The documented trigger is real only if
//! something outside the engine calls in.
//!
//! # `checkpoint()` unconditionally, rather than re-deriving the trigger
//!
//! `should_checkpoint` is public and so are `dirty_bytes()` and `wal_bytes()`,
//! but `Db::elapsed_secs` is private and there is no accessor for the last
//! checkpoint time — so asking "has the interval elapsed" would mean keeping a
//! **second clock**, which write-path checkpoints would silently desynchronise.
//! Calling in unconditionally is also cheap where it matters: `checkpoint()`
//! early-outs on a quiescent database, and flips a superblock only when
//! something is actually queued for reclamation.
//!
//! # Evict, then checkpoint
//!
//! The order is load-bearing and not alphabetical. `enforce_space_amp` only
//! **marks** readers evicted; it frees nothing. Only a subsequent `checkpoint()`
//! runs `reclaim_deferred`. Evict-then-checkpoint returns the space in one tick;
//! the other order takes two.
//!
//! # Overlap
//!
//! A tick that arrives while the previous one is still running is **skipped, not
//! queued** — a queue here would let a slow checkpoint accumulate a backlog of
//! redundant ones, which is the opposite of what a backlog should do.
//!
//! This deliberately does **not** serialise against the control-plane
//! checkpoint RPC. It does not need to: `enforce_policy` runs on whichever
//! thread committed, so two concurrent writers already reach `checkpoint()` at
//! once and the engine tolerates it. A cross-layer lock would buy nothing.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use yesno_core::Db;

/// What the daemon has done, for `/metrics` and for tests.
///
/// Separate from the `Db`'s own counters on purpose: those describe the
/// database, these describe the process driving it, and conflating them makes
/// it impossible to tell "the engine checkpointed" from "we asked it to".
#[derive(Debug, Default)]
pub struct Counters {
    pub checkpoints: AtomicU64,
    pub checkpoint_failures: AtomicU64,
    pub readers_evicted: AtomicU64,
    /// Wall time of the most recent checkpoint, in milliseconds.
    pub last_checkpoint_ms: AtomicU64,
    /// Cleared on a successful checkpoint, set on a failed one. `/readyz`
    /// reads it, because a node whose checkpoints are failing is still serving
    /// reads and is not somewhere you want more traffic sent.
    pub checkpoint_failing: AtomicBool,
}

/// A running checkpoint ticker.
pub struct Ticker {
    stop: tokio::sync::watch::Sender<bool>,
    handle: tokio::task::JoinHandle<()>,
}

impl Ticker {
    /// Start ticking.
    ///
    /// `interval_secs == 0` stops *this* driver, and the daemon's config
    /// refuses it for a reason worth repeating here: the **engine** reads zero
    /// as "always", not "never" — `should_checkpoint` compares
    /// `elapsed_secs >= interval_secs` — so a zero that reached `DbOptions`
    /// would make every commit take a full checkpoint. This branch exists
    /// because `spawn` is public, not because the configuration can produce it.
    pub fn spawn(db: Arc<Db>, interval_secs: u64, counters: Arc<Counters>) -> Ticker {
        let (stop, mut stopped) = tokio::sync::watch::channel(false);
        let handle = tokio::spawn(async move {
            if interval_secs == 0 {
                tracing::warn!(
                    "the checkpoint interval is 0, so this driver will not run; note that \
                     the engine reads the same 0 as \"checkpoint on every commit\""
                );
                let _ = stopped.changed().await;
                return;
            }
            let mut tick = tokio::time::interval(Duration::from_secs(interval_secs));
            // `Delay`, not `Burst`: a checkpoint slower than the interval must
            // not be followed by a flurry of catch-up ticks.
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // The first tick resolves immediately; a checkpoint at startup is
            // pointless when nothing has been written yet.
            tick.tick().await;

            loop {
                tokio::select! {
                    _ = tick.tick() => {}
                    _ = stopped.changed() => return,
                }
                run_once(&db, &counters).await;
            }
        });
        Ticker { stop, handle }
    }

    /// Stop ticking and wait for any checkpoint in flight.
    pub async fn stop(self) {
        let _ = self.stop.send(true);
        let _ = self.handle.await;
    }
}

/// One maintenance pass: evict what the space bound requires, then checkpoint.
///
/// Public so the shutdown path and tests can drive a pass without a timer.
pub async fn run_once(db: &Arc<Db>, counters: &Arc<Counters>) {
    let db = db.clone();
    let started = Instant::now();

    // Both halves on the blocking pool. `checkpoint` writes extents, rebuilds
    // the index and fsyncs once per shard; `enforce_space_amp` takes an
    // allocator lock per shard. Neither belongs on a reactor thread.
    let outcome = tokio::task::spawn_blocking(move || {
        // Observation before intervention. `observe_space_amp` reports the
        // soft threshold and changes nothing; running it *after*
        // `enforce_space_amp` would measure the state the eviction just
        // produced and could never report the breach that caused it.
        db.observe_space_amp();
        db.observe_snapshot_ages();
        (db.enforce_space_amp(), db.checkpoint())
    })
    .await;

    let elapsed = started.elapsed().as_millis() as u64;
    counters
        .last_checkpoint_ms
        .store(elapsed, Ordering::Relaxed);

    match outcome {
        Ok((evicted, Ok(w))) => {
            if evicted > 0 {
                counters
                    .readers_evicted
                    .fetch_add(evicted as u64, Ordering::Relaxed);
                tracing::warn!(
                    evicted,
                    "evicted the oldest readers to hold the space-amplification bound"
                );
            }
            counters.checkpoints.fetch_add(1, Ordering::Relaxed);
            counters.checkpoint_failing.store(false, Ordering::Relaxed);
            tracing::debug!(watermark = w, ms = elapsed, "checkpoint");
        }
        Ok((_, Err(e))) => {
            counters.checkpoint_failures.fetch_add(1, Ordering::Relaxed);
            counters.checkpoint_failing.store(true, Ordering::Relaxed);
            tracing::error!(error = ?e, "the background checkpoint failed");
        }
        Err(e) => {
            counters.checkpoint_failures.fetch_add(1, Ordering::Relaxed);
            counters.checkpoint_failing.store(true, Ordering::Relaxed);
            tracing::error!(error = %e, "the background checkpoint panicked");
        }
    }
}
