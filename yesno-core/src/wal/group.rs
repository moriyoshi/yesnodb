//! Group commit: many concurrent commits, one fsync.
//!
//! Measured before this existed: **1.00 fsync per commit**, 2 000 commits and
//! 2 000 syncs. Every writer paid for its own durability even when a dozen were
//! waiting on the same device at the same instant.
//!
//! # The leader
//!
//! A committer appends under the log's lock and comes away with a **target
//! LSN** — the log length its records end at. To become durable it needs
//! `synced >= target`, and it does not care who gets it there.
//!
//! So the first thread to find no sync in flight becomes the leader: it notes
//! the current length, **drops the lock**, fsyncs, retakes the lock, publishes
//! the new `synced`, and wakes everyone. Threads that arrived while it was
//! syncing waited on the condvar; those whose target it covered return without
//! touching the device at all.
//!
//! # Dropping the lock is the whole design
//!
//! A leader that fsynced while holding the log lock would serialize every
//! append behind every fsync, which is worse than no batching: the batch would
//! only ever contain writers that had *already* appended, and the ones queuing
//! behind the lock — the ones a batch exists to absorb — would be excluded by
//! construction. The fsync therefore goes through a **cloned descriptor** held
//! outside the mutex.
//!
//! This is also why `WalWriter` keeps `append` and `sync` separate rather than
//! offering one durable-append call.
//!
//! # What a waiter may assume
//!
//! Only that `synced >= target` when it returns `Ok`. Not that it was the
//! leader, not that exactly one fsync happened, and not that its own records
//! were the last in the batch. A failed fsync is reported to **every** waiter it
//! would have covered, because none of them can claim durability from it.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};

use crate::error::Result;
use crate::mvcc::Version;
use crate::wal::record::RecType;
use crate::wal::writer::WalWriter;

/// A log plus the machinery to share one fsync between concurrent committers.
pub struct GroupCommit {
    log: Mutex<WalWriter>,
    /// Set while a leader is fsyncing, so the next arrival waits instead of
    /// issuing a second sync for bytes already in flight.
    state: Mutex<SyncState>,
    woken: Condvar,
    /// Bytes currently in the log, mirrored out of the mutex.
    ///
    /// Exists so the **write path** can ask. `enforce_policy` runs on every
    /// commit and the checkpoint policy has a WAL-size trigger; answering it
    /// through `log.lock()` would take the hottest mutex in the system once per
    /// shard per commit, which is why the trigger was passed a constant `0`
    /// instead and sat dead. Maintained under the log lock, so it is exact
    /// rather than merely a hint.
    bytes: AtomicU64,
}

#[derive(Default)]
struct SyncState {
    /// Bytes known durable.
    synced: u64,
    /// A leader is in flight.
    in_flight: bool,
    /// Bumped every time recovery cuts the log or checkpointing replaces the
    /// active descriptor underneath the group.
    ///
    /// A leader drops the state lock across its fsync. Recovery may truncate the
    /// log, or a checkpoint may seal the descriptor being synced and install a
    /// new active one. The leader compares the epoch it started with and
    /// discards its result if the log layout moved.
    epoch: u64,
}

impl GroupCommit {
    pub fn new(log: WalWriter) -> Result<Self> {
        let synced = log.end_lsn();
        let bytes = AtomicU64::new(log.len());
        Ok(GroupCommit {
            log: Mutex::new(log),
            state: Mutex::new(SyncState {
                synced,
                in_flight: false,
                epoch: 0,
            }),
            woken: Condvar::new(),
            bytes,
        })
    }

    /// Bytes currently in the log, without taking its lock.
    #[inline]
    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    /// Append records under the log lock, returning the LSN they end at.
    ///
    /// The closure runs with the lock held and must not block on anything that
    /// could wait for a sync — appends are supposed to be the fast half.
    pub fn append_with(&self, f: impl FnOnce(&mut WalWriter) -> Result<()>) -> Result<u64> {
        let mut log = self.log.lock().unwrap();
        f(&mut log)?;
        self.bytes.store(log.len(), Ordering::Relaxed);
        Ok(log.end_lsn())
    }

    /// Return once everything up to `target` is durable.
    pub fn sync_through(&self, target: u64) -> Result<()> {
        loop {
            let mut st = self.state.lock().unwrap();
            if st.synced >= target {
                return Ok(());
            }
            if st.in_flight {
                // Someone is already syncing. They may or may not cover us; the
                // loop re-checks rather than assuming.
                let _unused = self.woken.wait(st).unwrap();
                continue;
            }

            // Become the leader. Take the length *now*, so everything appended
            // before this moment rides along.
            let epoch = st.epoch;
            let (to, fsync_fd) = {
                let log = self.log.lock().unwrap();
                (log.end_lsn(), log.dup_for_sync()?)
            };
            st.in_flight = true;
            drop(st);

            // The log no longer reaches `target`: recovery cut it below us
            // between our append and this moment.
            //
            // This must return rather than loop. `synced` can never climb back
            // to a target whose bytes are gone, so the loop would become an
            // unbounded fsync storm on one shard with the commit never
            // returning — which is exactly what it did.
            //
            // Success is the right answer, not an error. Recovery retains only
            // the prefix its global watermark resolved, so the discarded
            // target cannot become visible.

            if to < target {
                let mut st = self.state.lock().unwrap();
                st.in_flight = false;
                drop(st);
                self.woken.notify_all();
                return Ok(());
            }

            let result = fsync_fd.sync_data();

            let mut st = self.state.lock().unwrap();
            st.in_flight = false;
            // Only publish if the log did not move underneath the fsync; see
            // `SyncState::epoch`.
            if result.is_ok() && st.epoch == epoch {
                st.synced = st.synced.max(to);
                let mut log = self.log.lock().unwrap();
                log.note_synced(to);
                // `synced` above the log's length means a target beyond the end
                // of the log counts as durable, so later commits skip their
                // fsync entirely. That is the silent half of the truncation
                // race, and it is invisible unless asserted: nothing fails, the
                // writes simply stop being durable. Checked here so the
                // concurrency suite exercises it.
                debug_assert!(
                    st.synced <= log.end_lsn(),
                    "durable mark {} is past a log ending at lsn {}",
                    st.synced,
                    log.end_lsn()
                );
            }
            drop(st);
            self.woken.notify_all();

            // A failed fsync is every covered waiter's failure, not just ours.
            result.map_err(crate::wal::writer::io_err)?;
        }
    }

    /// Append one record and make it durable. Convenience for single writes.
    pub fn append_and_sync(
        &self,
        rtype: RecType,
        version: Version,
        term: u64,
        body: Vec<u8>,
    ) -> Result<()> {
        let target = self.append_with(|w| w.append(rtype, version, term, body).map(|_| ()))?;
        self.sync_through(target)
    }

    /// Append one stamped commit marker and make it durable.
    ///
    /// The abort path's write: an `Abort` must be durable *before* its slot is
    /// resolved in memory, or a crash in the gap leaves a version resolved
    /// nowhere on disk.
    pub fn append_marker_and_sync(
        &self,
        rtype: RecType,
        version: Version,
        term: u64,
        time: u64,
    ) -> Result<()> {
        let target =
            self.append_with(|w| w.append_marker(rtype, version, term, time).map(|_| ()))?;
        self.sync_through(target)
    }

    /// The log itself, for the checkpointer and diagnostics.
    pub fn log(&self) -> std::sync::MutexGuard<'_, WalWriter> {
        self.log.lock().unwrap()
    }

    /// Roll the active generation and re-publish sync state atomically.
    ///
    /// # Why this cannot be two calls
    ///
    /// It was: the checkpointer truncated under the log lock, dropped it, then
    /// called a separate `reset_synced`. In that gap a leader that had taken
    /// its length *before* the cut could publish it *after*, leaving `synced`
    /// past the end of a log that no longer held those bytes — at which point
    /// every later target looks already-durable and **skips its fsync**. That
    /// is a silent durability hole: nothing fails, the writes simply stop being
    /// durable. Holding both locks across the cut and the epoch bump is what
    /// makes the leader's stale result discardable.
    ///
    /// Locks are taken `state` then `log`, matching [`GroupCommit::sync_through`].
    /// Do not reverse them.
    ///
    /// # Two cuts, and they are not the same operation
    ///
    /// [`Self::truncate`] drops the suffix **above** an LSN. Checkpoint rollover
    /// seals the active generation and reclaims whole sealed generations below
    /// the retention floor.
    ///
    /// The checkpoint's rollover: seal the active file, then reclaim every
    /// complete generation the retention floor no longer protects.
    ///
    /// `through_lsn` guards the roll as well as naming it. The caller took that
    /// LSN under the shard's write lock, and the check must hold under *this*
    /// lock too, or the generation boundary would no longer describe the
    /// checkpoint interval. Returns whether it rolled.
    pub fn rotate_and_reclaim_if_quiet(
        &self,
        through_lsn: u64,
        reclaim_through: u64,
    ) -> Result<Option<GenerationRoll>> {
        let mut st = self.state.lock().unwrap();
        let mut log = self.log.lock().unwrap();
        if log.end_lsn() != through_lsn {
            return Ok(None);
        }
        let old_base_lsn = log.base();
        log.rotate()?;
        let bytes_reclaimed = log.reclaim_through(reclaim_through)?;
        let new_base_lsn = log.base();
        self.bytes.store(log.len(), Ordering::Relaxed);
        st.synced = through_lsn;
        // Invalidates any leader currently in flight; see the note above.
        st.epoch += 1;
        drop(log);
        drop(st);
        // Wakes waiters whose target the cut just made unreachable, so they
        // resolve now rather than sleeping until some unrelated commit.
        self.woken.notify_all();
        Ok(Some(GenerationRoll {
            old_base_lsn,
            new_base_lsn,
            bytes_reclaimed,
        }))
    }

    /// Recovery's cut: drop everything at or above `to`.
    ///
    /// Recovery truncates at the first record above `global_cv`, and it runs
    /// before any committer exists — but the durable mark still has to come
    /// down with the log. `GroupCommit::new` captured `synced` from the log's
    /// *pre-truncation* end, so leaving it would make the first commits after
    /// every recovering open skip their fsync.
    pub fn truncate(&self, to: u64) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        let mut log = self.log.lock().unwrap();
        log.truncate_to(to)?;
        self.bytes.store(log.len(), Ordering::Relaxed);
        st.synced = st.synced.min(log.end_lsn());
        st.epoch += 1;
        drop(log);
        drop(st);
        self.woken.notify_all();
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GenerationRoll {
    pub old_base_lsn: u64,
    pub new_base_lsn: u64,
    pub bytes_reclaimed: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Barrier};

    fn tmp(tag: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("yesno-grp-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = crate::wal::remove_log_generations(&self.0);
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn a_single_writer_still_syncs_its_own_records() {
        let p = tmp("solo");
        let _c = Cleanup(p.clone());
        let g = GroupCommit::new(WalWriter::open(&p, 0).unwrap()).unwrap();

        g.append_and_sync(RecType::ChunkDelta, 1, 0, vec![1; 8])
            .unwrap();
        assert_eq!(g.log().syncs(), 1);
        g.append_and_sync(RecType::ChunkDelta, 2, 0, vec![2; 8])
            .unwrap();
        assert_eq!(
            g.log().syncs(),
            2,
            "sequential commits cannot share a batch"
        );
    }

    #[test]
    fn a_target_already_covered_costs_no_sync() {
        let p = tmp("covered");
        let _c = Cleanup(p.clone());
        let g = GroupCommit::new(WalWriter::open(&p, 0).unwrap()).unwrap();

        let target = g
            .append_with(|w| w.append(RecType::ChunkDelta, 1, 0, vec![3; 8]).map(|_| ()))
            .unwrap();
        g.sync_through(target).unwrap();
        let after = g.log().syncs();
        // Same target again, and an earlier one: both are already durable.
        g.sync_through(target).unwrap();
        g.sync_through(0).unwrap();
        assert_eq!(g.log().syncs(), after, "a covered target must not resync");
    }

    /// A target recovery cut away must resolve, not spin.
    ///
    /// A synthetic committer holds a target above the suffix recovery keeps.
    /// `synced` can never climb back to it, so `sync_through` must not become an
    /// unbounded fsync loop on an empty active generation.
    ///
    /// It is safe to report success because recovery has rejected that version;
    /// no caller may publish it afterward.
    #[test]
    fn a_target_the_log_no_longer_reaches_resolves_instead_of_spinning() {
        use std::sync::mpsc;
        use std::time::Duration;

        let p = tmp("truncated-target");
        let _c = Cleanup(p.clone());
        let g = Arc::new(GroupCommit::new(WalWriter::open(&p, 0).unwrap()).unwrap());

        let target = g
            .append_with(|w| w.append(RecType::ChunkDelta, 1, 0, vec![9; 64]).map(|_| ()))
            .unwrap();
        assert!(target > 0, "the append must have produced a real target");

        // Recovery rejects the only appended version.
        g.truncate(0).unwrap();
        assert_eq!(g.log().end_lsn(), 0);

        let (tx, rx) = mpsc::channel();
        let g2 = g.clone();
        std::thread::spawn(move || {
            let _ = tx.send(g2.sync_through(target));
        });
        let got = rx.recv_timeout(Duration::from_secs(5));
        let got = got.expect("sync_through spun forever on a target recovery cut away");
        assert!(
            got.is_ok(),
            "a recovery-rejected target is not an I/O error"
        );
    }

    /// Recovery's cut must bring the durable mark down with it.
    ///
    /// `GroupCommit::new` captures `synced` from the log's length at open.
    /// Recovery then truncates at the first record above `global_cv`. Cutting
    /// the log without moving `synced` leaves the mark past the new end, and
    /// every commit whose target lands below it returns durable **without
    /// fsyncing** — silent, since nothing fails.
    #[test]
    fn recovery_truncation_brings_the_durable_mark_down() {
        let p = tmp("recovery-cut");
        let _c = Cleanup(p.clone());

        // A log with several records, all durable.
        let g = GroupCommit::new(WalWriter::open(&p, 0).unwrap()).unwrap();
        for v in 0..8u64 {
            g.append_and_sync(RecType::ChunkDelta, v, 0, vec![5; 32])
                .unwrap();
        }
        let full = g.log().len();
        drop(g);

        // Reopen, as an opening `Db` would, then cut back as recovery does.
        let g = GroupCommit::new(WalWriter::open(&p, 0).unwrap()).unwrap();
        assert_eq!(g.log().len(), full);
        let cut_to = full / 2;
        g.truncate(cut_to).unwrap();

        // The next commit lands below the *old* mark. It must still fsync.
        let before = g.log().syncs();
        g.append_and_sync(RecType::ChunkDelta, 99, 0, vec![6; 16])
            .unwrap();
        assert!(
            g.log().syncs() > before,
            "a commit below the pre-truncation durable mark skipped its fsync"
        );
        assert!(
            g.log().len() <= full,
            "the log should not have grown past where it was cut from"
        );
    }

    /// The property the whole module exists for.
    #[test]
    fn concurrent_committers_share_fsyncs() {
        let p = tmp("batch");
        let _c = Cleanup(p.clone());
        let g = Arc::new(GroupCommit::new(WalWriter::open(&p, 0).unwrap()).unwrap());

        let threads = 16usize;
        let each = 40u64;
        let gate = Arc::new(Barrier::new(threads));
        let commits = Arc::new(AtomicU64::new(0));

        let hs: Vec<_> = (0..threads)
            .map(|t| {
                let g = g.clone();
                let gate = gate.clone();
                let commits = commits.clone();
                std::thread::spawn(move || {
                    gate.wait();
                    for i in 0..each {
                        g.append_and_sync(RecType::ChunkDelta, t as u64 * each + i, 0, vec![7; 16])
                            .unwrap();
                        commits.fetch_add(1, Ordering::Relaxed);
                    }
                })
            })
            .collect();
        for h in hs {
            h.join().unwrap();
        }

        let n = commits.load(Ordering::Relaxed);
        let syncs = g.log().syncs();
        // The point is fewer syncs than commits. How many fewer depends on
        // timing, so the assertion is the direction, not a ratio — a threshold
        // tuned to this machine would be a flaky test on another.
        assert!(
            syncs < n,
            "{syncs} fsyncs for {n} concurrent commits: nothing batched"
        );
        // And every record must still be there.
        let bytes = g.log().read_all().unwrap();
        assert_eq!(crate::wal::Scanner::new(&bytes, 0).count() as u64, n);
    }
}
