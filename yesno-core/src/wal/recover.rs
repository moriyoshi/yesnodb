//! Redo-only recovery.
//!
//! # There is no undo log, and none is needed
//!
//! By invariant **I4** a checkpoint persists only state at or below the global
//! visible watermark, so nothing from an unresolved commit version ever reaches
//! the data file. "Undo" is therefore just "don't redo", and recovery reduces to
//! deciding how far the log can be trusted.
//!
//! # The prefix rule
//!
//! A commit version is **committed** iff every shard named in its
//! `CommitIntent` has a CRC-valid `ShardCommit` for it. A single-shard commit
//! writes no intent, so a lone `ShardCommit` implies participants `{that shard}`.
//! A version is **resolved** if it is committed or aborted.
//!
//! `global_cv` is the checkpoint's version plus the maximal run of consecutive
//! resolved versions above it. The scan stops at the first version that is
//! present-but-incomplete or absent everywhere — both are safe stops.
//!
//! # Why no acknowledged commit can be lost
//!
//! A commit `X` is acknowledged to its caller only after `visible >= X`, which
//! requires every version `<= X` to be Durable or Aborted, which in turn
//! requires their `ShardCommit` or `Abort` records to have been fsynced. Those
//! records survive the crash, so there is no gap at or below any acknowledged
//! `X` and the prefix scan necessarily reaches it. Commits *above* a gap that
//! were durable but never acknowledged are discarded, which is correct and
//! unobservable.
//!
//! # Truncation is a clean suffix
//!
//! By **I5** each shard's log is monotonic in commit version, so every discarded
//! record forms a contiguous tail. Recovery truncates each shard at the first
//! record above `global_cv`, which also stops a follower from ever seeing a
//! record the leader discarded — the exact primitive Raft will need.

use std::collections::{BTreeMap, BTreeSet};

use super::record::{decode_commit_intent, RecType, Record, Scanner};
use crate::error::{CodecError, Result};
use crate::mvcc::{Micros, Version};

/// One shard's log bytes, and where they start in that shard's address space.
pub struct ShardLog<'a> {
    pub shard: u32,
    pub bytes: &'a [u8],
    pub base_lsn: u64,
}

/// Tracks which commit versions are resolved, from records seen so far.
///
/// **Shared between crash recovery and follower apply**, deliberately. Both
/// answer the same question — "which versions are complete?" — from the same
/// records, and two implementations of a rule this subtle would drift. It is
/// also what lets a follower's visibility match the leader's exactly, preserving
/// multi-shard atomicity across the wire.
#[derive(Debug, Default, Clone)]
pub struct CommitTable {
    entries: BTreeMap<Version, CommitEntry>,
}

impl CommitTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one record from `shard` into the table. Non-commit records are
    /// ignored, so a caller can hand it every record it sees.
    pub fn observe(&mut self, shard: u32, r: &Record) -> Result<()> {
        match r.rtype {
            RecType::CommitIntent => {
                let shards = decode_commit_intent(&r.body)?;
                self.entries
                    .entry(r.commit_version)
                    .or_default()
                    .participants = Some(shards.into_iter().collect());
            }
            RecType::ShardCommit => {
                let entry = self.entries.entry(r.commit_version).or_default();
                entry.committed.insert(shard);
                observe_time(entry, r)?;
            }
            RecType::Abort => {
                let entry = self.entries.entry(r.commit_version).or_default();
                entry.aborted = true;
                observe_time(entry, r)?;
            }
            _ => {}
        }
        Ok(())
    }

    /// The commit time recorded for `v`, if the log carried one.
    ///
    /// `None` means the records predate commit-time stamping, which a
    /// wall-clock recovery target must refuse to interpret rather than read as
    /// the epoch.
    pub fn time_of(&self, v: Version) -> Option<Micros> {
        self.entries.get(&v).and_then(|e| e.time)
    }

    /// The highest commit time seen, for restoring the clock at open.
    pub fn max_time(&self) -> Micros {
        self.entries
            .values()
            .filter_map(|e| e.time)
            .max()
            .unwrap_or(0)
    }

    /// Highest version such that every version in `(floor, result]` is resolved.
    ///
    /// A prefix rule, not a set: it stops at the first version that is
    /// present-but-incomplete or absent entirely, both of which are safe stops.
    pub fn resolved_through(&self, floor: Version) -> Version {
        let mut v = floor;
        loop {
            match self.entries.get(&(v + 1)) {
                Some(e) if e.is_resolved() => v += 1,
                _ => return v,
            }
        }
    }

    pub fn is_resolved(&self, v: Version) -> bool {
        self.entries.get(&v).is_some_and(|e| e.is_resolved())
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Fold one marker's stamp into its entry.
///
/// Participants of one commit are stamped from a single `begin`, so two of
/// them disagreeing is corruption or a spliced log, not a clock skew to average
/// away. Refuse it: a commit whose time is ambiguous cannot be the boundary of a
/// wall-clock recovery target, and silently picking one of the two would put the
/// precise-looking wrong answer back.
fn observe_time(entry: &mut CommitEntry, r: &Record) -> Result<()> {
    let Some(time) = r.commit_time()? else {
        return Ok(());
    };
    match entry.time {
        Some(seen) if seen != time => Err(CodecError::Invariant(
            "participants of one commit disagree about its commit time",
        )),
        _ => {
            entry.time = Some(time);
            Ok(())
        }
    }
}

#[derive(Debug, Default, Clone)]
struct CommitEntry {
    /// Shards named by a `CommitIntent`. `None` until one is seen; a lone
    /// `ShardCommit` implies a single-shard commit.
    participants: Option<BTreeSet<u32>>,
    committed: BTreeSet<u32>,
    aborted: bool,
    /// The commit time every participant agreed on. `None` before stamping.
    time: Option<Micros>,
}

impl CommitEntry {
    fn is_resolved(&self) -> bool {
        if self.aborted {
            return true;
        }
        match &self.participants {
            // Explicit intent: every named shard must have committed.
            Some(p) => !p.is_empty() && p.is_subset(&self.committed),
            // No intent seen: a single-shard commit is complete on its own.
            None => !self.committed.is_empty(),
        }
    }
}

/// What recovery decided.
#[derive(Debug)]
pub struct RecoveryPlan {
    /// Highest commit version that may be replayed.
    pub global_cv: Version,
    /// Per shard, the offset to truncate its log to.
    pub truncate_at: BTreeMap<u32, u64>,
    /// Records to replay, in per-shard log order, filtered to
    /// `checkpoint_cv < cv <= global_cv` and excluding commit markers.
    pub replay: Vec<(u32, Record)>,
    /// Versions found in the log but above `global_cv`, hence discarded.
    pub discarded: Vec<Version>,
    /// Commit times for the replayable versions in `(checkpoint_cv, global_cv]`.
    ///
    /// Sparse on purpose: a version missing from this map carried no stamp, and a
    /// caller resolving a wall-clock target must treat that as unknown rather
    /// than as zero. [`RecoveryPlan::max_time`] is what recovery feeds back into
    /// the oracle's clock.
    pub times: BTreeMap<Version, Micros>,
}

impl RecoveryPlan {
    /// The highest commit time in the replayed range, or 0 if none was stamped.
    pub fn max_time(&self) -> Micros {
        self.times.values().copied().max().unwrap_or(0)
    }
}

/// Decide what to replay and where to truncate.
pub fn plan(logs: &[ShardLog<'_>], checkpoint_cv: Version) -> Result<RecoveryPlan> {
    let mut table = CommitTable::new();
    // Per shard: every well-formed record, plus where its scan stopped.
    let mut scanned: Vec<(u32, Vec<Record>, u64)> = Vec::new();

    for log in logs {
        let mut s = Scanner::new(log.bytes, log.base_lsn);
        let mut recs = Vec::new();
        for r in &mut s {
            let r = r?;
            table.observe(log.shard, &r)?;
            recs.push(r);
        }
        scanned.push((log.shard, recs, s.stopped_at()));
    }

    // Walk the consecutive run of resolved versions above the checkpoint.
    let global_cv = table.resolved_through(checkpoint_cv);

    let mut times = BTreeMap::new();
    let mut v = checkpoint_cv + 1;
    while v <= global_cv {
        if let Some(t) = table.time_of(v) {
            times.insert(v, t);
        }
        v += 1;
    }

    let mut plan = RecoveryPlan {
        global_cv,
        truncate_at: BTreeMap::new(),
        replay: Vec::new(),
        discarded: Vec::new(),
        times,
    };

    for (shard, recs, stopped_at) in scanned {
        // I5 makes everything above global_cv a contiguous tail, so the first
        // record over the line is the truncation point.
        let cut = recs
            .iter()
            .find(|r| r.commit_version > global_cv)
            .map(|r| r.lsn)
            .unwrap_or(stopped_at);
        plan.truncate_at.insert(shard, cut);

        for r in recs {
            if r.commit_version > global_cv {
                if !plan.discarded.contains(&r.commit_version) {
                    plan.discarded.push(r.commit_version);
                }
                continue;
            }
            if r.commit_version <= checkpoint_cv || r.rtype.is_commit_marker() {
                continue;
            }
            if matches!(
                r.rtype,
                RecType::Pad
                    | RecType::CheckpointBegin
                    | RecType::CheckpointEnd
                    | RecType::EpochFence
            ) {
                continue;
            }
            plan.replay.push((shard, r));
        }
    }
    plan.discarded.sort_unstable();
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wal::record::encode_commit_intent;

    /// Builds one shard's log, tracking offsets so `lsn` stays the byte offset.
    struct LogBuilder {
        shard: u32,
        bytes: Vec<u8>,
    }

    impl LogBuilder {
        fn new(shard: u32) -> Self {
            LogBuilder {
                shard,
                bytes: Vec::new(),
            }
        }

        fn push(&mut self, rtype: RecType, cv: Version, body: Vec<u8>) -> &mut Self {
            let lsn = self.bytes.len() as u64;
            let r = Record::new(rtype, lsn, cv, 1, body);
            self.bytes.extend_from_slice(&r.encode());
            self
        }

        /// A single-shard commit: data then `ShardCommit`, no intent.
        fn commit(&mut self, cv: Version) -> &mut Self {
            self.push(RecType::ChunkDelta, cv, vec![0u8; 24]);
            self.push(RecType::ShardCommit, cv, vec![])
        }

        /// A single-shard commit whose marker carries a commit time.
        fn commit_at(&mut self, cv: Version, time: u64) -> &mut Self {
            self.push(RecType::ChunkDelta, cv, vec![0u8; 24]);
            self.stamp(RecType::ShardCommit, cv, time)
        }

        fn stamp(&mut self, rtype: RecType, cv: Version, time: u64) -> &mut Self {
            let lsn = self.bytes.len() as u64;
            let r = Record::commit_marker(rtype, lsn, cv, 1, time);
            self.bytes.extend_from_slice(&r.encode());
            self
        }

        /// One shard's participation in a multi-shard commit.
        fn multi(&mut self, cv: Version, shards: &[u32], finish: bool) -> &mut Self {
            self.push(RecType::CommitIntent, cv, encode_commit_intent(shards));
            self.push(RecType::ChunkDelta, cv, vec![0u8; 24]);
            if finish {
                self.push(RecType::ShardCommit, cv, vec![]);
            }
            self
        }

        fn log(&self) -> ShardLog<'_> {
            ShardLog {
                shard: self.shard,
                bytes: &self.bytes,
                base_lsn: 0,
            }
        }
    }

    #[test]
    fn single_shard_commits_all_recover() {
        let mut b = LogBuilder::new(0);
        for cv in 1..=5 {
            b.commit(cv);
        }
        let p = plan(&[b.log()], 0).unwrap();
        assert_eq!(p.global_cv, 5);
        assert!(p.discarded.is_empty());
        assert_eq!(p.replay.len(), 5, "one data record per commit");
        assert_eq!(
            p.truncate_at[&0],
            b.bytes.len() as u64,
            "nothing to truncate"
        );
    }

    #[test]
    fn records_at_or_below_the_checkpoint_are_not_replayed() {
        let mut b = LogBuilder::new(0);
        for cv in 1..=6 {
            b.commit(cv);
        }
        let p = plan(&[b.log()], 4).unwrap();
        assert_eq!(p.global_cv, 6);
        assert_eq!(p.replay.len(), 2, "only versions 5 and 6 need redo");
        assert!(p.replay.iter().all(|(_, r)| r.commit_version > 4));
    }

    #[test]
    fn a_multi_shard_commit_missing_one_participant_is_not_committed() {
        // Shard 0 finished, shard 1 crashed before its ShardCommit.
        let mut a = LogBuilder::new(0);
        a.commit(1);
        a.multi(2, &[0, 1], true);

        let mut c = LogBuilder::new(1);
        c.multi(2, &[0, 1], false);

        let p = plan(&[a.log(), c.log()], 0).unwrap();
        assert_eq!(
            p.global_cv, 1,
            "version 2 is incomplete, so the prefix stops at 1"
        );
        assert_eq!(p.discarded, vec![2]);
        assert!(
            p.replay.iter().all(|(_, r)| r.commit_version <= 1),
            "no part of the incomplete batch may be replayed"
        );
    }

    #[test]
    fn a_multi_shard_commit_with_every_participant_recovers() {
        let mut a = LogBuilder::new(0);
        a.multi(1, &[0, 1], true);
        let mut c = LogBuilder::new(1);
        c.multi(1, &[0, 1], true);

        let p = plan(&[a.log(), c.log()], 0).unwrap();
        assert_eq!(p.global_cv, 1);
        assert!(p.discarded.is_empty());
        assert_eq!(p.replay.len(), 2, "one data record from each shard");
    }

    /// Without this, an abandoned version wedges recovery permanently.
    #[test]
    fn an_abort_lets_the_prefix_scan_continue() {
        let mut b = LogBuilder::new(0);
        b.commit(1);
        b.push(RecType::Abort, 2, vec![]);
        b.commit(3);

        let p = plan(&[b.log()], 0).unwrap();
        assert_eq!(
            p.global_cv, 3,
            "the aborted version resolves and is passed over"
        );
        assert!(p.discarded.is_empty());
        // The aborted version contributes nothing to replay.
        assert!(p.replay.iter().all(|(_, r)| r.commit_version != 2));
    }

    #[test]
    fn a_hole_stops_the_scan_even_if_later_versions_are_complete() {
        // Version 2 never resolved; 3 and 4 did. The prefix rule must stop at 1.
        let mut b = LogBuilder::new(0);
        b.commit(1);
        b.push(RecType::ChunkDelta, 2, vec![0u8; 24]); // no ShardCommit
        b.commit(3);
        b.commit(4);

        let p = plan(&[b.log()], 0).unwrap();
        assert_eq!(p.global_cv, 1);
        assert_eq!(p.discarded, vec![2, 3, 4]);
    }

    /// The central safety property.
    #[test]
    fn no_acknowledged_commit_is_ever_discarded() {
        // An acknowledged commit X implies every version <= X is resolved and
        // durable. Construct that and assert the plan reaches X for every X.
        for acked in 1..=8u64 {
            let mut b = LogBuilder::new(0);
            for cv in 1..=acked {
                b.commit(cv);
            }
            // Versions above the acknowledgement may or may not have finished.
            b.push(RecType::ChunkDelta, acked + 1, vec![0u8; 24]);

            let p = plan(&[b.log()], 0).unwrap();
            assert!(
                p.global_cv >= acked,
                "acknowledged commit {acked} was discarded (global_cv={})",
                p.global_cv
            );
        }
    }

    #[test]
    fn discarded_records_form_a_clean_suffix() {
        // I5: each log is cv-monotonic, so the truncation point must precede
        // every record above global_cv and follow every record at or below it.
        let mut b = LogBuilder::new(0);
        b.commit(1);
        b.commit(2);
        b.push(RecType::ChunkDelta, 3, vec![0u8; 24]); // unresolved
        b.push(RecType::ChunkDelta, 4, vec![0u8; 24]);

        let p = plan(&[b.log()], 0).unwrap();
        assert_eq!(p.global_cv, 2);
        let cut = p.truncate_at[&0];

        let kept: Vec<Record> = Scanner::new(&b.bytes[..cut as usize], 0)
            .map(|r| r.unwrap())
            .collect();
        assert!(
            kept.iter().all(|r| r.commit_version <= 2),
            "truncation left a record above global_cv"
        );
        assert_eq!(
            kept.iter()
                .filter(|r| r.rtype == RecType::ShardCommit)
                .count(),
            2,
            "truncation cut away a resolved commit"
        );
    }

    #[test]
    fn a_torn_tail_is_handled_like_a_missing_record() {
        let mut b = LogBuilder::new(0);
        b.commit(1);
        b.commit(2);
        let full = b.bytes.len();
        b.commit(3);
        // Chop the last commit in half, as a crash mid-append would.
        b.bytes.truncate(full + 20);

        let p = plan(&[b.log()], 0).unwrap();
        assert_eq!(p.global_cv, 2, "the half-written commit is not resolved");
        assert_eq!(p.truncate_at[&0], full as u64);
    }

    #[test]
    fn shards_truncate_independently_at_their_own_offsets() {
        // Shard 1 has extra unresolved records; shard 0 does not.
        let mut a = LogBuilder::new(0);
        a.commit(1);
        let a_len = a.bytes.len();

        let mut c = LogBuilder::new(1);
        c.commit(1);
        let c_resolved = c.bytes.len();
        c.push(RecType::ChunkDelta, 9, vec![0u8; 24]);

        let p = plan(&[a.log(), c.log()], 0).unwrap();
        assert_eq!(p.global_cv, 1);
        assert_eq!(p.truncate_at[&0], a_len as u64, "shard 0 keeps everything");
        assert_eq!(
            p.truncate_at[&1], c_resolved as u64,
            "shard 1 drops its tail"
        );
    }

    #[test]
    fn an_empty_log_recovers_to_the_checkpoint() {
        let b = LogBuilder::new(0);
        let p = plan(&[b.log()], 7).unwrap();
        assert_eq!(p.global_cv, 7, "nothing above the checkpoint to replay");
        assert!(p.replay.is_empty());
        assert_eq!(p.truncate_at[&0], 0);
    }

    #[test]
    fn commit_markers_and_checkpoint_records_are_not_replayed() {
        let mut b = LogBuilder::new(0);
        b.push(RecType::CheckpointBegin, 1, vec![]);
        b.commit(1);
        b.push(RecType::CheckpointEnd, 1, vec![]);
        b.push(RecType::Pad, 1, vec![]);

        let p = plan(&[b.log()], 0).unwrap();
        assert_eq!(p.global_cv, 1);
        assert_eq!(p.replay.len(), 1, "only the ChunkDelta is redo material");
        assert_eq!(p.replay[0].1.rtype, RecType::ChunkDelta);
    }

    #[test]
    fn a_three_shard_commit_needs_all_three() {
        let build = |finishers: &[u32]| {
            let mut logs = Vec::new();
            for s in 0..3u32 {
                let mut b = LogBuilder::new(s);
                b.multi(1, &[0, 1, 2], finishers.contains(&s));
                logs.push(b);
            }
            logs
        };

        let partial = build(&[0, 1]);
        let refs: Vec<ShardLog> = partial.iter().map(|b| b.log()).collect();
        assert_eq!(
            plan(&refs, 0).unwrap().global_cv,
            0,
            "two of three is not enough"
        );

        let complete = build(&[0, 1, 2]);
        let refs: Vec<ShardLog> = complete.iter().map(|b| b.log()).collect();
        assert_eq!(plan(&refs, 0).unwrap().global_cv, 1);
    }

    // ----------------------------------------------------------- commit times

    /// A log written before commit-time stamping recovers exactly as it did,
    /// and reports no times.
    ///
    /// The important half is the *absence*: an unstamped version must be
    /// missing from `times`, not present as zero. A wall-clock target reading a
    /// zero would silently place every pre-stamping commit at the epoch, which
    /// is exactly the precise-looking wrong answer the design refuses.
    #[test]
    fn an_unstamped_log_recovers_and_reports_no_times() {
        let mut b = LogBuilder::new(0);
        for cv in 1..=4 {
            b.commit(cv);
        }
        let p = plan(&[b.log()], 0).unwrap();
        assert_eq!(p.global_cv, 4);
        assert!(
            p.times.is_empty(),
            "unstamped versions must be absent, not 0"
        );
        assert_eq!(p.max_time(), 0);
    }

    #[test]
    fn stamped_commits_are_reported_with_their_times() {
        let mut b = LogBuilder::new(0);
        b.commit_at(1, 1_000)
            .commit_at(2, 2_500)
            .commit_at(3, 9_000);
        let p = plan(&[b.log()], 0).unwrap();
        assert_eq!(p.global_cv, 3);
        assert_eq!(p.times.get(&1), Some(&1_000));
        assert_eq!(p.times.get(&2), Some(&2_500));
        assert_eq!(p.times.get(&3), Some(&9_000));
        assert_eq!(p.max_time(), 9_000);
    }

    /// Only the replayable range is described.
    ///
    /// A caller resolving a wall-clock target may only choose within
    /// `(checkpoint_cv, global_cv]`; reporting a time for a version outside it
    /// would offer a target that cannot be reached.
    #[test]
    fn times_cover_only_the_replayable_range() {
        let mut b = LogBuilder::new(0);
        b.commit_at(1, 100).commit_at(2, 200).commit_at(3, 300);
        let p = plan(&[b.log()], 2).unwrap();
        assert_eq!(p.global_cv, 3);
        assert_eq!(p.times.keys().copied().collect::<Vec<_>>(), vec![3]);
    }

    /// Participants of one commit are stamped from one `begin`, so a
    /// disagreement is corruption or a spliced history — never something to
    /// resolve by picking a side.
    #[test]
    fn participants_disagreeing_about_a_commit_time_is_an_error() {
        let mut a = LogBuilder::new(0);
        a.push(RecType::CommitIntent, 1, encode_commit_intent(&[0, 1]));
        a.stamp(RecType::ShardCommit, 1, 1_000);
        let mut b = LogBuilder::new(1);
        b.push(RecType::CommitIntent, 1, encode_commit_intent(&[0, 1]));
        b.stamp(RecType::ShardCommit, 1, 1_001);

        let err = plan(&[a.log(), b.log()], 0).unwrap_err();
        assert!(
            format!("{err}").contains("disagree"),
            "expected a commit-time disagreement, got {err}"
        );
    }

    /// Agreement across participants is the normal case and must not error.
    #[test]
    fn participants_agreeing_about_a_commit_time_is_accepted() {
        let mut a = LogBuilder::new(0);
        a.push(RecType::CommitIntent, 1, encode_commit_intent(&[0, 1]));
        a.stamp(RecType::ShardCommit, 1, 4_242);
        let mut b = LogBuilder::new(1);
        b.push(RecType::CommitIntent, 1, encode_commit_intent(&[0, 1]));
        b.stamp(RecType::ShardCommit, 1, 4_242);

        let p = plan(&[a.log(), b.log()], 0).unwrap();
        assert_eq!(p.global_cv, 1);
        assert_eq!(p.times.get(&1), Some(&4_242));
    }

    /// A stamped `Abort` resolves its version and still reports its time.
    #[test]
    fn an_aborted_version_keeps_its_time() {
        let mut b = LogBuilder::new(0);
        b.commit_at(1, 10);
        b.push(RecType::ChunkDelta, 2, vec![0u8; 24]);
        b.stamp(RecType::Abort, 2, 20);
        b.commit_at(3, 30);
        let p = plan(&[b.log()], 0).unwrap();
        assert_eq!(p.global_cv, 3, "an abort resolves its version");
        assert_eq!(p.times.get(&2), Some(&20));
        assert_eq!(p.max_time(), 30);
    }
}
