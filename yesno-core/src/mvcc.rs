//! Global commit versions, the visible watermark, and snapshot registration.
//!
//! # Why there is a watermark at all
//!
//! Shards commit independently, but a batch spanning several of them must become
//! visible **atomically**. The oracle hands out one commit version per batch and
//! only advances a global `visible` watermark once every version at or below it
//! is resolved. Readers snapshot `visible`, never `next`, so a partially durable
//! batch is invisible rather than half-visible.
//!
//! # Late assignment
//!
//! The commit version is taken *after* all WAL bodies are encoded and while the
//! participating shard locks are held. Two consequences:
//!
//! - each shard's WAL is monotonic in commit version ( invariant I5 ), which is
//!   what makes recovery's discarded suffix a clean truncation rather than a
//!   scatter of holes;
//! - the window between assigning a version and durably resolving it shrinks to
//!   the fsync, which bounds how long a slow shard can stall visibility.
//!
//! # The commit clock
//!
//! [`VersionOracle::begin`] also stamps a wall-clock time, in the same call and
//! under the same ring lock that assigns the version. That placement is the whole
//! point: **time order therefore equals version order by construction**, which is
//! what lets a restore name a prefix by wall clock at all. Stamping later — at
//! `append`, say — would let two commits fsync out of order and produce a
//! timeline in which no clean cut exists.
//!
//! The clock is clamped monotone: `max( now, last + 1 )`. A system clock that
//! steps backwards ( NTP correction, VM migration, a suspended laptop ) would
//! otherwise invert two commits and make one commit's data reachable by a target
//! that excludes an earlier one. Absorbing the step means commits bunch at
//! `last + 1, last + 2, …` until real time catches up, so the stamps run slightly
//! ahead of true time for that interval. That is the deliberate trade: a
//! bounded, self-healing inaccuracy in exchange for an ordering a recovery target
//! can rely on. There is no ceiling on a *forward* jump — a clock that leaps
//! forward is reporting something real, and clamping it would be inventing data.
//!
//! The stamp lives in the ring slot rather than only being returned, because an
//! abort resolves the *same* version later and must carry the *same* time. See
//! [`VersionOracle::time_of`].
//!
//! # The abort path is not optional
//!
//! If a version is assigned and then never resolved — a panic, a poisoned lock,
//! an encoder failure — the watermark stops there **forever**. Worse, the next
//! recovery finds a hole and stops its prefix scan at it, silently discarding
//! every acknowledged commit above. So an abort must be recorded, durably, with
//! the same weight as a commit. [`VersionOracle::abort`] exists for that, and
//! `Aborted` resolves a slot exactly as `Durable` does.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Wall-clock now, in UNIX epoch microseconds.
///
/// A clock before the epoch reads as 0 rather than panicking; the caller clamps
/// it monotone anyway, so a nonsense reading costs ordering nothing.
fn now_micros() -> Micros {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

/// A commit version. Monotonic, dense, and never reused.
pub type Version = u64;

/// A commit time: UNIX epoch microseconds, non-decreasing in [`Version`].
pub type Micros = u64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotState {
    Empty,
    /// Assigned, not yet resolved. Invisible to readers.
    Pending,
    /// Every participating shard has made it durable.
    Durable,
    /// Will never complete. Resolves the slot so the watermark can pass.
    Aborted,
}

#[derive(Clone, Copy, Debug)]
struct Slot {
    version: Version,
    state: SlotState,
    pending_shards: u32,
    /// Assigned with the version, so the abort path can stamp the same value.
    time: Micros,
}

impl Default for Slot {
    fn default() -> Self {
        Slot {
            version: 0,
            state: SlotState::Empty,
            pending_shards: 0,
            time: 0,
        }
    }
}

/// The slot ring and the commit clock, under one lock.
///
/// They share a lock **on purpose**. The clock's monotonicity is only
/// meaningful relative to the version order, and the version order is what this
/// mutex establishes. Two locks would be two orderings, and a stamp assigned
/// under a different lock than its version could invert.
struct Ring {
    slots: Vec<Slot>,
    /// The highest time stamped so far. Never decreases.
    last_time: Micros,
}

/// Commit-version sequencer and visibility watermark.
///
/// The slot ring is sized so that a commit cannot lap an unresolved one; if it
/// ever would, [`VersionOracle::begin`] refuses rather than overwriting, because
/// silently reusing a slot would make a pending commit look resolved.
pub struct VersionOracle {
    next: AtomicU64,
    visible: AtomicU64,
    ring: Mutex<Ring>,
    ring_size: usize,
}

impl VersionOracle {
    pub fn new(ring_size: usize) -> Self {
        assert!(
            ring_size.is_power_of_two(),
            "ring size must be a power of two"
        );
        VersionOracle {
            // Version 0 is the "nothing committed" floor, so the first handed
            // out is 1 and `visible == 0` means an empty database.
            next: AtomicU64::new(1),
            visible: AtomicU64::new(0),
            ring: Mutex::new(Ring {
                slots: vec![Slot::default(); ring_size],
                last_time: 0,
            }),
            ring_size,
        }
    }

    /// Adopt a watermark computed elsewhere, for a replica.
    ///
    /// **Not `resume_at`**, though it would work today. That one restores
    /// the *sequencer* after recovery and sets `next` as well, which is right
    /// exactly once, at open. A replica adopts a watermark on every batch and
    /// never assigns a version of its own, so calling `resume_at` per batch
    /// would be reusing a one-shot for a repeated operation — harmless now and a
    /// trap the day somebody calls it on a leader.
    ///
    /// Monotone by construction: a watermark that could go backwards would make
    /// a version visible and then not, which no reader can be asked to tolerate.
    pub(crate) fn adopt_visible(&self, v: Version) {
        debug_assert!(
            v >= self.visible(),
            "a visible watermark must not go backwards"
        );
        self.visible
            .fetch_max(v, std::sync::atomic::Ordering::AcqRel);
    }

    /// Restore the sequencer after recovery, so new commits continue above the
    /// durable floor rather than colliding with versions already on disk.
    ///
    /// `time` is the highest commit time replayed, or 0 when the recovered log
    /// carries no stamps at all — a database written before commit-time stamping
    /// existed. Passing it is what keeps the clock monotone across a restart, and
    /// across a promotion: a standby applies the leader's stamped records, so its
    /// recovery reads them back here.
    pub fn resume_at(&self, version: Version, time: Micros) {
        self.next.store(version + 1, Ordering::Release);
        self.visible.store(version, Ordering::Release);
        let mut ring = self.ring.lock().unwrap();
        ring.last_time = ring.last_time.max(time);
    }

    /// Versions at or below this are resolved and readable.
    #[inline]
    pub fn visible(&self) -> Version {
        self.visible.load(Ordering::Acquire)
    }

    /// The next version that would be handed out.
    #[inline]
    pub fn peek_next(&self) -> Version {
        self.next.load(Ordering::Acquire)
    }

    /// Block until `version` is visible, or until `timeout` elapses.
    ///
    /// # Why waiting is meaningful at all
    ///
    /// [`advance`](Self::advance) moves the watermark over a *consecutive
    /// prefix*, so a commit that resolves while an earlier one is still pending
    /// has a version that is assigned, durable, and **not yet readable**. That
    /// window is bounded by one fsync, and until this existed nothing in the API
    /// let a caller wait it out: `commit` returned a version whose own
    /// `snapshot_at` was refused, and the refusal looked identical to naming a
    /// version that will never exist.
    ///
    /// # Why it polls
    ///
    /// Not a condvar signalled from `advance`. `advance` runs on the commit
    /// path with the ring lock held, and every committer would pay the wakeup
    /// for the rare caller that waits. The window this closes is one fsync, and
    /// an fsync costs at least a few hundred microseconds, so a first nap of
    /// 50 us converges in one or two iterations for any caller actually racing
    /// its own commit.
    ///
    /// There is deliberately **no spin phase**. One was written and removed:
    /// against an fsync it cannot pay for itself, and it would have added a
    /// tuning constant with no measurement behind it.
    ///
    /// # It does not check that `version` was ever assigned
    ///
    /// The obvious guard — refuse when `version >= peek_next()` — would be
    /// wrong on a **replica**, which adopts watermarks through
    /// [`adopt_visible`](Self::adopt_visible) and never advances `next` at all,
    /// so `peek_next` there is frozen at whatever recovery left it. A follower
    /// is exactly where a read-your-writes wait is most useful, so the guard
    /// would disable the feature in its main case. A version that is never
    /// assigned therefore reports a timeout rather than an error, which is also
    /// the honest answer: this oracle cannot distinguish "not yet" from "never".
    pub fn wait_visible(&self, version: Version, timeout: std::time::Duration) -> bool {
        // Elapsed-against-`timeout`, **not** a precomputed `now() + timeout`
        // deadline. `Instant + Duration` panics on overflow, so a caller writing
        // the obvious "wait as long as it takes" — `Duration::MAX` — would have
        // got a panic out of a library function instead of a wait. Subtracting
        // in the other direction cannot overflow, because the subtraction only
        // runs once `elapsed < timeout` is known.
        let start = std::time::Instant::now();
        let mut nap = std::time::Duration::from_micros(50);
        loop {
            // The watermark is read once per iteration and always *before*
            // the timeout test, so there is exactly one place that can answer
            // `true` and no path that gives up without having looked. That is
            // what makes a zero timeout a truthful poll with no special case —
            // an earlier draft had an entry-time early return plus a re-read in
            // the expiry branch, and no test could redden the re-read.
            if self.visible() >= version {
                return true;
            }
            let elapsed = start.elapsed();
            if elapsed >= timeout {
                return false;
            }
            std::thread::sleep(nap.min(timeout - elapsed));
            nap = (nap * 2).min(std::time::Duration::from_millis(2));
        }
    }

    /// Assign a version and a commit time to a batch touching `shards` shards.
    ///
    /// Call with the participating shard locks held: that is what gives I5.
    ///
    /// The returned time is stamped here rather than by the caller so that every
    /// participant of one commit records the *same* value — see invariant **I9**
    /// and this module's header.
    pub fn begin(&self, shards: u32) -> Option<(Version, Micros)> {
        debug_assert!(shards > 0, "a commit must touch at least one shard");
        let mut ring = self.ring.lock().unwrap();
        let v = self.next.load(Ordering::Acquire);
        let idx = (v as usize) % self.ring_size;

        // Refuse if the slot still matters, which is *not* the same as "still
        // pending". A slot resolved as Durable or Aborted is still needed until
        // `visible` has passed its version — that is precisely the record
        // `advance` reads to step forward. Overwriting one would make advance
        // see `Empty` at that version and stall the watermark permanently.
        let occupant = ring.slots[idx];
        if occupant.state != SlotState::Empty && occupant.version > self.visible() {
            return None;
        }
        // Clamped monotone under this same lock, so time order and version
        // order cannot disagree. Do not hoist this read out of the lock to
        // "avoid a syscall under a mutex": two threads reading the clock outside
        // and entering in the other order is exactly the inversion this prevents.
        let time = now_micros().max(ring.last_time + 1);
        ring.last_time = time;
        ring.slots[idx] = Slot {
            version: v,
            state: SlotState::Pending,
            pending_shards: shards,
            time,
        };
        self.next.store(v + 1, Ordering::Release);
        Some((v, time))
    }

    /// The highest commit time stamped so far.
    ///
    /// The checkpointer persists this so the clock survives a restart whose WAL
    /// prefix has already been checkpointed away. See `SuperBlock::commit_clock`.
    pub fn commit_clock(&self) -> Micros {
        self.ring.lock().unwrap().last_time
    }

    /// The commit time assigned to `v`, while its slot is still live.
    ///
    /// `None` once the slot has been recycled, or for a version this oracle never
    /// assigned. The abort path calls this to stamp its `Abort` with the version's
    /// own time rather than the time of the abort.
    pub fn time_of(&self, v: Version) -> Option<Micros> {
        let ring = self.ring.lock().unwrap();
        let slot = ring.slots[(v as usize) % self.ring_size];
        (slot.version == v && slot.state != SlotState::Empty).then_some(slot.time)
    }

    /// Record that one participating shard has made `v` durable.
    ///
    /// Returns `true` if this was the last one, resolving the version.
    pub fn shard_durable(&self, v: Version) -> bool {
        let resolved = {
            let mut ring = self.ring.lock().unwrap();
            let idx = (v as usize) % self.ring_size;
            let slot = &mut ring.slots[idx];
            if slot.version != v || slot.state != SlotState::Pending {
                return false;
            }
            slot.pending_shards = slot.pending_shards.saturating_sub(1);
            if slot.pending_shards == 0 {
                slot.state = SlotState::Durable;
                true
            } else {
                false
            }
        };
        if resolved {
            self.advance();
        }
        resolved
    }

    /// Resolve `v` as never-completing.
    ///
    /// The caller must have durably recorded an `Abort` record first. Skipping
    /// that is what turns a stalled watermark into lost acknowledged commits on
    /// the next recovery.
    pub fn abort(&self, v: Version) {
        {
            let mut ring = self.ring.lock().unwrap();
            let idx = (v as usize) % self.ring_size;
            let slot = &mut ring.slots[idx];
            if slot.version != v || slot.state != SlotState::Pending {
                return;
            }
            slot.state = SlotState::Aborted;
        }
        self.advance();
    }

    /// Advance `visible` over every consecutive resolved version.
    ///
    /// Deliberately a *prefix* rule rather than a set: it keeps recovery and
    /// follower ordering trivial, at the cost of head-of-line blocking behind a
    /// slow shard. Late assignment bounds that exposure to one fsync.
    fn advance(&self) {
        let ring = self.ring.lock().unwrap();
        loop {
            let cur = self.visible.load(Ordering::Acquire);
            let want = cur + 1;
            let idx = (want as usize) % self.ring_size;
            let slot = ring.slots[idx];
            let resolved = slot.version == want
                && matches!(slot.state, SlotState::Durable | SlotState::Aborted);
            if !resolved {
                return;
            }
            if self
                .visible
                .compare_exchange(cur, want, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                continue; // another thread advanced; re-read and retry
            }
        }
    }

    pub fn state_of(&self, v: Version) -> SlotState {
        let ring = self.ring.lock().unwrap();
        let slot = ring.slots[(v as usize) % self.ring_size];
        if slot.version == v {
            slot.state
        } else {
            SlotState::Empty
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn oracle() -> VersionOracle {
        VersionOracle::new(64)
    }

    #[test]
    fn an_empty_database_is_visible_at_zero() {
        let o = oracle();
        assert_eq!(o.visible(), 0);
        assert_eq!(o.peek_next(), 1);
    }

    #[test]
    fn a_single_shard_commit_becomes_visible() {
        let o = oracle();
        let v = o.begin(1).unwrap().0;
        assert_eq!(v, 1);
        assert_eq!(o.visible(), 0, "pending commits are invisible");
        assert!(o.shard_durable(v));
        assert_eq!(o.visible(), 1);
    }

    #[test]
    fn a_multi_shard_commit_is_invisible_until_every_shard_is_durable() {
        let o = oracle();
        let v = o.begin(3).unwrap().0;
        assert!(!o.shard_durable(v));
        assert_eq!(o.visible(), 0, "one shard down, two to go");
        assert!(!o.shard_durable(v));
        assert_eq!(o.visible(), 0);
        assert!(o.shard_durable(v), "third shard resolves it");
        assert_eq!(o.visible(), 1);
    }

    #[test]
    fn visibility_is_a_prefix_so_a_later_commit_waits() {
        let o = oracle();
        let v1 = o.begin(1).unwrap().0;
        let v2 = o.begin(1).unwrap().0;
        // v2 finishes first.
        o.shard_durable(v2);
        assert_eq!(o.visible(), 0, "v2 must not become visible ahead of v1");
        o.shard_durable(v1);
        assert_eq!(o.visible(), 2, "both become visible together");
    }

    /// The case that stalls the watermark forever if abort is not implemented.
    #[test]
    fn an_abort_lets_the_watermark_pass() {
        let o = oracle();
        let v1 = o.begin(1).unwrap().0;
        let v2 = o.begin(2).unwrap().0;
        let v3 = o.begin(1).unwrap().0;

        o.shard_durable(v1);
        assert_eq!(o.visible(), 1);

        // v2 will never complete.
        o.shard_durable(v3);
        assert_eq!(o.visible(), 1, "v3 is blocked behind the unresolved v2");

        o.abort(v2);
        assert_eq!(o.visible(), 3, "aborting v2 releases v2 and v3 together");
        assert_eq!(o.state_of(v2), SlotState::Aborted);
    }

    #[test]
    fn aborting_resolves_exactly_like_committing() {
        let o = oracle();
        let v = o.begin(1).unwrap().0;
        o.abort(v);
        assert_eq!(o.visible(), 1);
        // A late durability report for an aborted version must not un-abort it.
        assert!(!o.shard_durable(v));
        assert_eq!(o.state_of(v), SlotState::Aborted);
    }

    #[test]
    fn duplicate_or_stale_durability_reports_are_ignored() {
        let o = oracle();
        let v = o.begin(1).unwrap().0;
        assert!(o.shard_durable(v));
        assert!(!o.shard_durable(v), "a repeat must not double-resolve");
        assert!(!o.shard_durable(999), "an unknown version must be ignored");
        assert_eq!(o.visible(), 1);
    }

    #[test]
    fn the_ring_refuses_to_lap_a_pending_commit() {
        let o = VersionOracle::new(4);
        let v1 = o.begin(1).unwrap().0; // version 1, slot 1; left pending
                                        // Versions 2, 3, 4 take slots 2, 3, 0.
        for _ in 0..3 {
            o.begin(1).unwrap();
        }
        // Version 5 would take slot 1, which v1 still occupies.
        assert!(
            o.begin(1).is_none(),
            "must refuse rather than overwrite a pending slot"
        );

        // Once v1 resolves and the watermark passes it, its slot is reusable.
        o.shard_durable(v1);
        assert_eq!(o.visible(), 1);
        assert!(
            o.begin(1).is_some(),
            "a slot the watermark has passed may be reused"
        );
    }

    /// A resolved-but-not-yet-visible slot is still load-bearing.
    ///
    /// `advance` reads exactly these records to step forward, so overwriting one
    /// makes it see `Empty` at that version and stall the watermark forever.
    #[test]
    fn the_ring_refuses_to_lap_a_durable_but_not_yet_visible_commit() {
        let o = VersionOracle::new(4);
        let _v1 = o.begin(1).unwrap().0; // slot 1, stays pending, blocks the watermark
        let v2 = o.begin(1).unwrap().0; // slot 2
        o.shard_durable(v2); // Durable, but visible is still 0
        assert_eq!(o.visible(), 0);
        assert_eq!(o.state_of(v2), SlotState::Durable);

        o.begin(1).unwrap(); // v3 -> slot 3
        o.begin(1).unwrap(); // v4 -> slot 0
        assert!(o.begin(1).is_none(), "v5 would clobber v1, still pending");

        // And the durable-but-invisible record survived, so once v1 resolves the
        // watermark can pass over v2 without stalling.
        assert_eq!(o.state_of(v2), SlotState::Durable);
    }

    #[test]
    fn versions_are_dense_and_never_reused() {
        let o = oracle();
        let mut seen = Vec::new();
        for _ in 0..50 {
            let v = o.begin(1).unwrap().0;
            seen.push(v);
            o.shard_durable(v);
        }
        assert_eq!(seen, (1..=50u64).collect::<Vec<_>>());
        assert_eq!(o.visible(), 50);
    }

    #[test]
    fn concurrent_commits_reach_a_consistent_watermark() {
        use std::sync::Arc;
        let o = Arc::new(VersionOracle::new(1024));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let o = o.clone();
            handles.push(std::thread::spawn(move || {
                let mut mine = Vec::new();
                for _ in 0..50 {
                    if let Some((v, _)) = o.begin(1) {
                        mine.push(v);
                    }
                }
                for v in mine {
                    o.shard_durable(v);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(
            o.visible(),
            o.peek_next() - 1,
            "every assigned version resolved, so visible must reach the last one"
        );
    }

    /// The window between a commit being assigned a version and that version
    /// becoming visible, and the wait that closes it.
    ///
    /// The two assertions at the end are what stop this passing vacuously. A
    /// `wait_visible` that returned `true` without waiting would satisfy the
    /// call itself, and be caught by both: it would return in microseconds, with
    /// the watermark still behind. Verified by sabotage — replacing the body
    /// with `true` reddens on the elapsed assertion.
    #[test]
    fn a_wait_returns_only_once_the_blocked_prefix_has_caught_up() {
        use std::sync::Arc;
        use std::time::{Duration, Instant};

        let o = Arc::new(oracle());
        let v1 = o.begin(1).unwrap().0;
        let v2 = o.begin(1).unwrap().0;
        assert!(o.shard_durable(v2), "v2 resolves on its own");
        assert_eq!(o.visible(), 0, "and is unreadable, because v1 is pending");

        let helper = {
            let o = Arc::clone(&o);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(60));
                o.shard_durable(v1);
            })
        };

        let start = Instant::now();
        let ok = o.wait_visible(v2, Duration::from_secs(10));
        let waited = start.elapsed();
        helper.join().unwrap();

        assert!(ok, "the wait must succeed once the prefix resolves");
        assert!(
            o.visible() >= v2,
            "visible {} is behind v2 {v2}",
            o.visible()
        );
        assert!(
            waited >= Duration::from_millis(50),
            "returned after {waited:?} — it did not actually wait for v1"
        );
    }

    /// A version that never resolves must cost the caller its deadline and
    /// nothing more. Both bounds matter: returning early would make the
    /// timeout a lie, and overshooting would mean the backoff sleeps past the
    /// deadline it was given.
    #[test]
    fn a_wait_for_a_version_that_never_resolves_times_out_rather_than_hanging() {
        use std::time::{Duration, Instant};

        let o = oracle();
        let v = o.begin(2).unwrap().0;
        assert!(!o.shard_durable(v), "one of two shards is not a resolution");

        let start = Instant::now();
        let ok = o.wait_visible(v, Duration::from_millis(30));
        let waited = start.elapsed();

        assert!(!ok, "an unresolved version must not report itself visible");
        assert!(
            waited >= Duration::from_millis(30),
            "returned after {waited:?}, short of its own deadline"
        );
        assert!(
            waited < Duration::from_secs(2),
            "slept {waited:?} past a 30ms deadline"
        );
    }

    /// `Duration::MAX` is the obvious way to write "wait as long as it takes",
    /// and a precomputed `Instant::now() + timeout` deadline **panics** on it —
    /// `attempt to add with overflow`, out of a library function, on a plausible
    /// argument. Verified: restoring the deadline form reddens this test and
    /// nothing else. The version here is already visible, so the call returns
    /// on the first watermark read and the test cannot hang even if the overflow
    /// handling regresses into an actually-unbounded wait.
    #[test]
    fn an_unbounded_timeout_does_not_overflow_the_clock() {
        use std::time::Duration;

        let o = oracle();
        let v = o.begin(1).unwrap().0;
        assert!(o.shard_durable(v));
        assert!(
            o.wait_visible(v, Duration::MAX),
            "an already-visible version must be reported before any arithmetic on the timeout"
        );
    }

    /// A caller polling with `Duration::ZERO` is asking a question, not waiting,
    /// and must be told the truth in both directions. This pins the ordering
    /// inside the loop rather than any special case: read the watermark, *then*
    /// test the deadline. Swapping those two lines reddens this test and nothing
    /// else, which is the only reason the order is written down.
    #[test]
    fn a_zero_timeout_is_a_truthful_poll_in_both_directions() {
        use std::time::Duration;

        let o = oracle();
        let v = o.begin(1).unwrap().0;
        assert!(o.shard_durable(v));
        assert!(o.wait_visible(v, Duration::ZERO), "v is visible");
        assert!(
            !o.wait_visible(v + 1, Duration::ZERO),
            "v + 1 was never assigned"
        );
    }
}
