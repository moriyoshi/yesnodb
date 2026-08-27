//! Asynchronous single-leader replication.
//!
//! # The leader ships raw on-disk WAL frames
//!
//! Not re-encoded, not wrapped in another format. The bytes a follower receives
//! are byte-for-byte the bytes on the leader's disk, which means **the follower's
//! apply path is the same decoder as crash recovery**: one framing, one
//! [`Scanner`], one fuzz target. A second wire format would be a second decoder
//! that can drift from the first, and drift here means a replica that silently
//! disagrees with its leader.
//!
//! It also makes a shipped byte range offset-identical on both sides, which is
//! the log-matching property consensus would later need.
//!
//! # Why not Arrow Flight for this
//!
//! Flight moves `RecordBatch`es. A WAL is an opaque self-framed binary log, so
//! wrapping it costs an IPC schema message, per-batch FlatBuffer headers and an
//! extra copy on both ends for no benefit; batching fights commit latency; and
//! `DoGet` is unidirectional, so follower-driven resume and applied-LSN acks
//! push you to `DoExchange` — at which point it is plain gRPC with extra steps.
//! Flight is the right answer for *query results*, which is a different problem.
//!
//! # Determinism is about set contents, not encoding
//!
//! Records are logical and carry no extent addresses, so a follower runs its own
//! allocator, checkpointer and compaction. It may legitimately hold a `Bitmap`
//! where the leader holds an `Array` for the same chunk, because demotion
//! happens at each node's own checkpoint. Comparing replicas therefore means
//! comparing **set equality**, never byte equality — stated here because a
//! byte-level comparison would report divergence on a perfectly healthy pair.

use std::collections::BTreeMap;

use crate::error::{CodecError, Result};
use crate::mvcc::Version;
use crate::store::checksum::crc32c;
use crate::wal::record::{Record, Scanner};
use crate::wal::recover::CommitTable;

/// A contiguous run of one shard's WAL, exactly as it sits on the leader's disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WalBatch {
    pub shard: u32,
    /// Byte offset of `records[0]` in the shard's address space.
    pub first_lsn: u64,
    pub records: Vec<u8>,
    pub crc32c: u32,
    /// A keepalive carrying no records, so a quiet leader still reports liveness.
    pub heartbeat: bool,
}

impl WalBatch {
    pub fn new(shard: u32, first_lsn: u64, records: Vec<u8>) -> Self {
        let crc32c = crc32c(&records);
        WalBatch {
            shard,
            first_lsn,
            records,
            crc32c,
            heartbeat: false,
        }
    }

    pub fn heartbeat(shard: u32, at_lsn: u64) -> Self {
        WalBatch {
            shard,
            first_lsn: at_lsn,
            records: Vec::new(),
            crc32c: crc32c(&[]),
            heartbeat: true,
        }
    }

    /// One past the last byte this batch covers.
    #[inline]
    pub fn end_lsn(&self) -> u64 {
        self.first_lsn + self.records.len() as u64
    }

    pub(crate) fn verify(&self) -> Result<()> {
        if crc32c(&self.records) != self.crc32c {
            return Err(CodecError::Invariant("WAL batch checksum mismatch"));
        }
        Ok(())
    }
}

/// Per-shard LSNs whose containing generation a checkpoint must retain,
/// published by whatever is serving followers.
///
/// # Why the leader needs telling at all
///
/// A checkpoint reclaims redundant generations on a timer, and a follower that
/// falls below the retained base has one remedy: copy the whole base image
/// again. `Ack` has carried the follower's applied LSN since M7 and the leader
/// answered with a lag it then discarded — the design names "applied-LSN acks
/// -> lag + retention floor" and only the lag existed.
///
/// # What a floor is
///
/// The lowest LSN any follower the leader still believes in has yet to apply.
/// Two things decide it — who is counted, and how long a silent follower keeps
/// counting — and both are answered differently depending on whether the acks
/// identify their sender.
///
/// ## With identity ( [`observe_from`](Self::observe_from) )
///
/// One entry per `( follower, shard )`, holding that follower's own position.
/// [`take`](Self::take) returns the minimum across them.
///
/// **A follower that misses one window keeps its protection**, for up to
/// [`RETENTION_GRACE`] consecutive windows. That is the whole reason identity is
/// worth having here, and the failure it removes is expensive: with two
/// standbys, one at LSN 1 000 and one at 100, a window in which only the fast
/// one acks used to yield a floor of 1 000 and cut the slow one out of the log —
/// costing it a fresh copy of the **entire database image**, for the crime of
/// being mid-bootstrap when a checkpoint happened to run.
///
/// ## Without identity ( [`observe`](Self::observe) )
///
/// Every anonymous acker shares one entry, and it takes the **minimum** within a
/// window rather than the latest value. That asymmetry is not an oversight:
/// with no identity there is no way to tell "this follower moved forward" from
/// "a different follower reported", so the only sound reading is the pessimistic
/// one. It is the pre-2026-08-29 behaviour, kept exactly, for a deployment
/// running `--insecure-replication`.
///
/// # What still bounds the cost
///
/// A stalled follower must not be able to halt ingestion or fill the disk —
/// the same call the design makes for snapshot space with `AbortOldestReader`.
/// Two things enforce it, and this change moved only the first:
///
/// - **Grace, not eternity.** An entry is evicted after `RETENTION_GRACE`
///   consecutive silent windows. This *is* a deliberate loosening — a dead
///   follower now holds the log for a few checkpoints rather than one — and it
///   is bounded, stated, and tested.
/// - **`CheckpointPolicy::max_wal_bytes`, unchanged.** Past it the floor is
///   overridden entirely, because a follower that keeps acking while falling
///   further behind would otherwise pin the log for ever. Do not weaken this
///   one to make a slow follower work; it is the only hard stop.
///
/// The map is bounded by the number of distinct principals that ack, which the
/// configured principal table already bounds — and every anonymous caller
/// collapses to one key, so an unauthenticated deployment cannot grow it at all.
#[derive(Clone, Debug, Default)]
pub struct RetentionFloor {
    per: std::sync::Arc<std::sync::Mutex<BTreeMap<(FollowerId, u32), Entry>>>,
}

/// How many consecutive checkpoints a follower may stay silent through and still
/// hold the log down.
///
/// Not a tuning knob dressed as one. `2` is chosen against the *ratio* of the
/// two intervals rather than as a duration: a follower polls far more often than
/// a leader checkpoints ( 1 s against 60 s by default ), so a live follower that
/// misses even one window is already anomalous and two is generous. Raising it
/// trades disk against a rebuild that is already survivable; the case it must
/// not cover is a follower that is simply gone.
pub const RETENTION_GRACE: u32 = 2;

/// A follower's name as the transport established it. Empty means anonymous.
type FollowerId = String;

#[derive(Clone, Copy, Debug)]
struct Entry {
    lsn: u64,
    /// Set by an ack, cleared by the checkpoint that reads it.
    fresh: bool,
    /// Consecutive windows this entry has gone unrefreshed.
    misses: u32,
}

impl RetentionFloor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that **some** follower has applied `shard` through `applied_lsn`.
    ///
    /// Pessimistic by necessity — see the type's docs. Prefer
    /// [`observe_from`](Self::observe_from) wherever the caller knows who is
    /// speaking, which on an authenticated listener it always does.
    pub fn observe(&self, shard: u32, applied_lsn: u64) {
        let mut g = self.per.lock().unwrap();
        let e = g.entry((FollowerId::new(), shard)).or_insert(Entry {
            lsn: applied_lsn,
            fresh: false,
            misses: 0,
        });
        // The minimum *within the window*: a fresh entry is one this window
        // already wrote, and it may have been a different follower.
        if e.fresh {
            e.lsn = e.lsn.min(applied_lsn);
        } else {
            e.lsn = applied_lsn;
        }
        e.fresh = true;
        e.misses = 0;
    }

    /// Record that the follower named `who` has applied `shard` through
    /// `applied_lsn`.
    ///
    /// `who` must be an identity the **transport** established, never one the
    /// message carried. A self-reported id is a claim, and a follower that
    /// misreports it — by accident or otherwise — moves another follower's floor.
    /// That is why `AckRequest` deliberately has no such field.
    ///
    /// Assignment, not `min`: within one follower the newest ack is the truth,
    /// and a `min` here would pin the floor at wherever it started. A follower's
    /// LSN is **not** monotonic across a log cut — it restarts near zero — which
    /// is precisely the case a `min` would get wrong for ever.
    pub fn observe_from(&self, who: &str, shard: u32, applied_lsn: u64) {
        if who.is_empty() {
            return self.observe(shard, applied_lsn);
        }
        let mut g = self.per.lock().unwrap();
        g.insert(
            (who.to_string(), shard),
            Entry {
                lsn: applied_lsn,
                fresh: true,
                misses: 0,
            },
        );
    }

    /// The lowest LSN any follower still needs on `shard`, and one window passes.
    ///
    /// **Not a peek.** Every call is a window boundary: entries refreshed
    /// since the last one have their miss count reset, the rest age, and those
    /// past [`RETENTION_GRACE`] are evicted. Do not add a peeking variant and
    /// call it from a second place — two callers would age the window at twice
    /// the rate and evict live followers, which is the same class of defect as
    /// a retention floor consumed by something other than the checkpoint.
    pub fn take(&self, shard: u32) -> Option<u64> {
        let mut g = self.per.lock().unwrap();
        let mut floor: Option<u64> = None;
        g.retain(|(_, s), e| {
            if *s != shard {
                return true;
            }
            if e.fresh {
                e.fresh = false;
                e.misses = 0;
            } else {
                e.misses += 1;
                if e.misses > RETENTION_GRACE {
                    return false;
                }
            }
            floor = Some(floor.map_or(e.lsn, |f: u64| f.min(e.lsn)));
            true
        });
        floor
    }

    /// How many followers currently hold `shard`'s log down.
    ///
    /// Reads only; it does not age the window.
    ///
    /// **"For metrics and for tests" was half true and is corrected here**:
    /// as of 2026-09-09 nothing under `yesno-server` renders this, so every
    /// caller is a test. It is kept because the retention floor is exactly the
    /// thing an operator cannot otherwise see -- a follower pinning a log
    /// generation is invisible until the disk fills -- but until something
    /// scrapes it, do not cite it as a metric that exists.
    pub fn holders(&self, shard: u32) -> usize {
        self.per
            .lock()
            .unwrap()
            .keys()
            .filter(|(_, s)| *s == shard)
            .count()
    }
}

/// Where a follower is in one shard's log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WalCursor {
    pub shard: u32,
    /// The next byte offset the follower expects.
    pub next_lsn: u64,
}

/// Applies shipped WAL batches and tracks follower visibility.
///
/// Runs **the same watermark algorithm as the leader** — the shared
/// [`CommitTable`] plus the same prefix rule — so a multi-shard batch becomes
/// visible on the follower only when every participant's records have arrived.
/// Multi-shard atomicity is preserved end to end rather than re-derived.
pub struct Follower {
    cursors: BTreeMap<u32, u64>,
    table: CommitTable,
    visible: Version,
    applied: u64,
}

impl Follower {
    /// Start from a base snapshot taken at `checkpoint_cv`, with each shard's
    /// cursor at the offset that snapshot recorded.
    pub fn new(checkpoint_cv: Version, cursors: impl IntoIterator<Item = WalCursor>) -> Self {
        Follower {
            cursors: cursors.into_iter().map(|c| (c.shard, c.next_lsn)).collect(),
            table: CommitTable::new(),
            visible: checkpoint_cv,
            applied: 0,
        }
    }

    /// Versions at or below this are readable on the follower.
    #[inline]
    pub fn visible(&self) -> Version {
        self.visible
    }

    #[inline]
    pub fn applied_records(&self) -> u64 {
        self.applied
    }

    /// The cursor to resume this shard from, for an `Ack` or a reconnect.
    pub fn cursor(&self, shard: u32) -> Option<WalCursor> {
        self.cursors
            .get(&shard)
            .map(|&next_lsn| WalCursor { shard, next_lsn })
    }

    /// Re-base one shard's cursor onto a freshly fetched base image.
    ///
    /// Required, not a convenience. A follower whose leader checkpointed has
    /// a cursor into a log that no longer exists, and the only repair is a new
    /// base image — at which point the old cursor is not merely stale but
    /// actively wrong, and [`Self::apply`] rejects every batch that does not
    /// continue it. Without this the remedy the leader names is unreachable
    /// through the shipped client: bootstrapping again produced
    /// `"WAL batch does not continue the cursor"` on the very next call.
    ///
    /// Does **not** move the watermark. The image's own version is not
    /// something the leader currently reports, so `visible()` stays where it
    /// was; contents are correct either way because replay is what applies them.
    pub fn reset_shard(&mut self, shard: u32, next_lsn: u64) {
        self.cursors.insert(shard, next_lsn);
    }

    /// How far behind the leader this follower is, in versions.
    #[inline]
    pub fn lag_versions(&self, leader_visible: Version) -> u64 {
        leader_visible.saturating_sub(self.visible)
    }

    /// Apply one batch. Returns how many records it contributed.
    ///
    /// Rejects a batch that does not start exactly where the follower expects.
    /// A gap would silently skip records, and the prefix watermark would then
    /// stall forever waiting for versions that were never delivered — so this
    /// fails loudly and the follower reconnects at its cursor.
    pub fn apply(&mut self, batch: &WalBatch) -> Result<usize> {
        batch.verify()?;
        let expected = self.cursors.entry(batch.shard).or_insert(batch.first_lsn);
        if batch.heartbeat {
            return Ok(0);
        }
        if batch.first_lsn != *expected {
            return Err(CodecError::Invariant(
                "WAL batch does not continue the cursor",
            ));
        }

        let mut n = 0usize;
        let mut scanner = Scanner::new(&batch.records, batch.first_lsn);
        for r in &mut scanner {
            let r = r?;
            // Every record drives the watermark; none is retained. A
            // follower applies by *recovering* from the bytes it wrote, so
            // holding the decoded records here was a second apply path that
            // nothing consumed — and, since nothing drained it, an unbounded
            // buffer of every record the follower had ever seen.
            self.table.observe(batch.shard, &r)?;
            n += 1;
        }

        // A batch that stops short is a truncated ship, not a torn local log:
        // advance only over what actually decoded, so the next request resumes
        // at exactly the right byte.
        let consumed = scanner.stopped_at();
        self.cursors.insert(batch.shard, consumed);
        self.applied += n as u64;

        // The leader's own rule, unchanged.
        self.visible = self.table.resolved_through(self.visible);
        Ok(n)
    }
}

/// Serves WAL byte ranges to followers.
///
/// Deliberately hands out **raw slices** rather than parsed records: parsing on
/// the leader would be wasted work and would let the two sides' framing diverge.
pub struct WalPublisher<'a> {
    shard: u32,
    bytes: &'a [u8],
    base_lsn: u64,
}

impl<'a> WalPublisher<'a> {
    pub fn new(shard: u32, bytes: &'a [u8], base_lsn: u64) -> Self {
        WalPublisher {
            shard,
            bytes,
            base_lsn,
        }
    }

    /// A publisher over contiguous logical WAL bytes, taking the base from the
    /// first record when one exists.
    ///
    /// The base is **not** zero. Byte 0 of this slice is byte `base` of the
    /// shard's history, whether the slice begins at the oldest sealed generation
    /// or at a bounded replication window. The first record carries the answer
    /// in its header.
    ///
    /// `if_empty` covers the one retained log that cannot answer — an empty one.
    /// The caller supplies the
    /// superblock's `wal_replay_lsn`, and it matters even though an empty log
    /// ships nothing: it is what tells a caught-up follower it is caught up
    /// rather than out of range.
    pub fn over_log(shard: u32, bytes: &'a [u8], if_empty: u64) -> Self {
        let base_lsn = Record::peek_lsn(bytes).unwrap_or(if_empty);
        WalPublisher {
            shard,
            bytes,
            base_lsn,
        }
    }

    /// One past the last byte available.
    #[inline]
    pub fn end_lsn(&self) -> u64 {
        self.base_lsn + self.bytes.len() as u64
    }

    /// Whether `from_lsn` is the start of a record, or the point a torn tail
    /// begins — the two offsets a follower may legitimately hold.
    ///
    /// Walks from the base rather than probing at `from_lsn`, because probing is
    /// what cannot tell the cases apart: an offset inside a *different*
    /// generation's record decodes as garbage exactly the way a half-written one
    /// does. Only "did any record end here" distinguishes them.
    ///
    /// O( records ) and deliberately off the hot path: `batch_from` reaches it
    /// only after the ordinary decode has already failed.
    fn names_a_record(&self, from_lsn: u64) -> bool {
        let mut at = self.base_lsn;
        if at == from_lsn {
            return true;
        }
        let mut scanner = Scanner::new(self.bytes, self.base_lsn);
        for r in &mut scanner {
            let Ok(r) = r else { return false };
            at += Record::framed_len(r.body.len()) as u64;
            match at.cmp(&from_lsn) {
                std::cmp::Ordering::Equal => return true,
                std::cmp::Ordering::Greater => return false,
                std::cmp::Ordering::Less => {}
            }
        }
        // The scan ran out before reaching it: `from_lsn` is past everything
        // this log can account for.
        false
    }

    /// A batch starting at `from_lsn`, at most `max_bytes` long.
    ///
    /// Truncates on a record boundary, because a follower cannot decode half a
    /// record and shipping one would make it reject the batch as a gap.
    pub fn batch_from(&self, from_lsn: u64, max_bytes: usize) -> Result<WalBatch> {
        if from_lsn < self.base_lsn || from_lsn > self.end_lsn() {
            return Err(CodecError::Invariant("requested lsn is outside this log"));
        }
        let start = (from_lsn - self.base_lsn) as usize;
        if start == self.bytes.len() {
            return Ok(WalBatch::heartbeat(self.shard, from_lsn));
        }
        let window = &self.bytes[start..];

        // Walk records until the budget is reached, so the cut lands on a
        // boundary rather than mid-frame.
        let mut end = 0usize;
        let mut scanner = Scanner::new(window, from_lsn);
        for r in &mut scanner {
            let r = r?;
            let framed = Record::framed_len(r.body.len());
            if end + framed > max_bytes && end > 0 {
                break;
            }
            end += framed;
            if end >= max_bytes {
                break;
            }
        }
        if end == 0 {
            // The window is non-empty and its first record did not decode here.
            // That is **two** conditions, and answering both with a heartbeat
            // — as this did until 2026-08-28 — is what made a divergent follower
            // silent:
            //
            //   * `from_lsn` *is* a boundary and the record at it is half-written.
            //     A leader mid-append leaves that behind and it heals itself on
            //     the next poll, so a heartbeat is exactly right.
            //   * `from_lsn` is not a boundary at all. A checkpoint cut this log
            //     and the records after it were written from zero again, so every
            //     LSN the follower holds is meaningless. Answering "you have
            //     everything I have" leaves it permanently, silently behind:
            //     measured at a 22 400-byte second generation against a follower
            //     stuck at 160, `Ok( bytes: 0, records: 0 )`, and a replica
            //     missing an entire key with nothing reporting a gap.
            //
            // Telling them apart costs a scan, and only on this path — never on
            // the one a healthy follower takes.
            if self.names_a_record(from_lsn) {
                return Ok(WalBatch::heartbeat(self.shard, from_lsn));
            }
            return Err(CodecError::WalCursorNotOnRecordBoundary(from_lsn));
        }
        Ok(WalBatch::new(self.shard, from_lsn, window[..end].to_vec()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wal::record::encode_commit_intent;
    use crate::wal::record::RecType;

    struct Log {
        shard: u32,
        bytes: Vec<u8>,
    }

    impl Log {
        fn new(shard: u32) -> Self {
            Log {
                shard,
                bytes: Vec::new(),
            }
        }

        fn push(&mut self, rtype: RecType, cv: Version, body: Vec<u8>) -> &mut Self {
            let lsn = self.bytes.len() as u64;
            self.bytes
                .extend_from_slice(&Record::new(rtype, lsn, cv, 1, body).encode());
            self
        }

        fn commit(&mut self, cv: Version) -> &mut Self {
            self.push(RecType::ChunkDelta, cv, vec![0u8; 24]);
            self.push(RecType::ShardCommit, cv, vec![])
        }

        fn multi(&mut self, cv: Version, shards: &[u32], finish: bool) -> &mut Self {
            self.push(RecType::CommitIntent, cv, encode_commit_intent(shards));
            self.push(RecType::ChunkDelta, cv, vec![0u8; 24]);
            if finish {
                self.push(RecType::ShardCommit, cv, vec![]);
            }
            self
        }

        fn publisher(&self) -> WalPublisher<'_> {
            WalPublisher::new(self.shard, &self.bytes, 0)
        }
    }

    fn follower(shards: &[u32]) -> Follower {
        Follower::new(
            0,
            shards.iter().map(|&s| WalCursor {
                shard: s,
                next_lsn: 0,
            }),
        )
    }

    #[test]
    fn a_follower_catches_up_on_single_shard_commits() {
        let mut l = Log::new(0);
        for cv in 1..=5 {
            l.commit(cv);
        }
        let mut f = follower(&[0]);
        let b = l.publisher().batch_from(0, 1 << 20).unwrap();
        f.apply(&b).unwrap();

        assert_eq!(f.visible(), 5);
        assert_eq!(f.lag_versions(5), 0);
        assert_eq!(f.cursor(0).unwrap().next_lsn, l.bytes.len() as u64);
    }

    /// The property that makes replication worth the design: a batch spanning
    /// shards must not become visible on the follower until every participant
    /// has arrived.
    #[test]
    fn a_multi_shard_commit_is_invisible_until_every_shard_arrives() {
        let mut a = Log::new(0);
        a.multi(1, &[0, 1], true);
        let mut b = Log::new(1);
        b.multi(1, &[0, 1], true);

        let mut f = follower(&[0, 1]);
        f.apply(&a.publisher().batch_from(0, 1 << 20).unwrap())
            .unwrap();
        assert_eq!(
            f.visible(),
            0,
            "one participant is not enough; the version must stay invisible"
        );

        f.apply(&b.publisher().batch_from(0, 1 << 20).unwrap())
            .unwrap();
        assert_eq!(
            f.visible(),
            1,
            "both participants present, so it becomes visible"
        );
    }

    #[test]
    fn an_incomplete_multi_shard_commit_blocks_later_ones() {
        // Same prefix rule as the leader: a hole stops everything above it.
        let mut a = Log::new(0);
        a.commit(1);
        a.multi(2, &[0, 1], true);
        a.commit(3);

        let mut f = follower(&[0, 1]);
        f.apply(&a.publisher().batch_from(0, 1 << 20).unwrap())
            .unwrap();
        assert_eq!(
            f.visible(),
            1,
            "version 2 is incomplete, so 3 cannot be visible either"
        );
    }

    #[test]
    fn an_abort_lets_follower_visibility_advance() {
        let mut l = Log::new(0);
        l.commit(1);
        l.push(RecType::Abort, 2, vec![]);
        l.commit(3);

        let mut f = follower(&[0]);
        f.apply(&l.publisher().batch_from(0, 1 << 20).unwrap())
            .unwrap();
        assert_eq!(f.visible(), 3, "an abort resolves just like a commit");
    }

    #[test]
    fn a_gap_is_rejected_rather_than_silently_skipped() {
        // Skipping records would stall the prefix watermark forever waiting for
        // versions that were never delivered.
        let mut l = Log::new(0);
        for cv in 1..=4 {
            l.commit(cv);
        }
        let mut f = follower(&[0]);
        let mid = l.publisher().batch_from(0, 1 << 20).unwrap();
        // Ship a batch starting past the cursor.
        let skipped = WalBatch::new(0, 200, mid.records.clone());
        let err = f.apply(&skipped).unwrap_err();
        assert!(format!("{err}").contains("continue"), "{err}");
        assert_eq!(f.visible(), 0, "nothing may be applied out of order");
    }

    #[test]
    fn a_corrupted_batch_is_rejected() {
        let mut l = Log::new(0);
        l.commit(1);
        let mut b = l.publisher().batch_from(0, 1 << 20).unwrap();
        b.records[10] ^= 0xFF;
        let mut f = follower(&[0]);
        assert!(f.apply(&b).is_err(), "the wire checksum must catch this");
    }

    #[test]
    fn batches_are_cut_on_record_boundaries() {
        // A follower cannot decode half a record, so a size-limited batch must
        // stop at a boundary.
        let mut l = Log::new(0);
        for cv in 1..=20 {
            l.commit(cv);
        }
        let mut f = follower(&[0]);
        let mut from = 0u64;
        let mut rounds = 0;
        while from < l.publisher().end_lsn() {
            let b = l.publisher().batch_from(from, 100).unwrap();
            if b.heartbeat {
                break;
            }
            f.apply(&b).unwrap();
            from = f.cursor(0).unwrap().next_lsn;
            rounds += 1;
            assert!(rounds < 200, "not making progress");
        }
        assert_eq!(f.visible(), 20, "small batches must still reach the end");
        assert!(rounds > 5, "the budget should have forced several batches");
    }

    #[test]
    fn a_quiet_leader_sends_a_heartbeat() {
        let mut l = Log::new(0);
        l.commit(1);
        let end = l.publisher().end_lsn();
        let b = l.publisher().batch_from(end, 1 << 20).unwrap();
        assert!(b.heartbeat);
        assert!(b.records.is_empty());

        let mut f = follower(&[0]);
        // A heartbeat applies cleanly and changes nothing.
        assert_eq!(f.apply(&b).unwrap(), 0);
        assert_eq!(f.visible(), 0);
    }

    #[test]
    fn resuming_from_a_cursor_continues_exactly() {
        let mut l = Log::new(0);
        for cv in 1..=10 {
            l.commit(cv);
        }
        let mut f = follower(&[0]);
        let first = l.publisher().batch_from(0, 300).unwrap();
        f.apply(&first).unwrap();
        let resume = f.cursor(0).unwrap().next_lsn;
        assert_eq!(resume, first.end_lsn());

        let rest = l.publisher().batch_from(resume, 1 << 20).unwrap();
        f.apply(&rest).unwrap();
        assert_eq!(f.visible(), 10);
    }

    /// Every record counts toward the watermark, markers included.
    ///
    /// This replaces a test that asserted markers were filtered *out of a
    /// pending buffer* — a buffer nothing drained, whose filter therefore had
    /// no observable effect. What actually matters is that a marker still
    /// drives `CommitTable`, since that is how a commit becomes visible.
    #[test]
    fn markers_and_data_both_reach_the_watermark() {
        let mut l = Log::new(0);
        l.commit(1);
        l.push(RecType::CheckpointBegin, 1, vec![]);
        let mut f = follower(&[0]);
        let n = f
            .apply(&l.publisher().batch_from(0, 1 << 20).unwrap())
            .unwrap();

        assert_eq!(n, 3, "the data record, its commit marker, and the begin");
        assert_eq!(f.applied_records(), 3);
        assert_eq!(f.visible(), 1, "the commit marker resolved version 1");
    }

    /// "Nothing decoded at your offset" and "nothing to send" are different
    /// answers, and giving the first one the second's reply is what made a
    /// checkpoint silently strand every follower.
    #[test]
    fn a_cursor_off_a_record_boundary_is_refused_rather_than_called_caught_up() {
        let mut l = Log::new(0);
        for cv in 1..=4 {
            l.commit(cv);
        }
        let end = l.publisher().end_lsn();

        // Exactly the end is the genuine caught-up case and must stay a heartbeat.
        assert!(
            l.publisher().batch_from(end, 1 << 20).unwrap().heartbeat,
            "a caught-up follower must still get a heartbeat"
        );

        // Four bytes into the first record is not a boundary. Before this was
        // separated, it returned a heartbeat too — the same reply, for a
        // follower that had everything and one that had nothing.
        let err = l.publisher().batch_from(4, 1 << 20).unwrap_err();
        assert!(
            matches!(err, CodecError::WalCursorNotOnRecordBoundary(4)),
            "{err:?}"
        );

        // And an interior boundary is still served, so the check has not simply
        // become "refuse anything but zero".
        let first = l.publisher().batch_from(0, 60).unwrap();
        let mid = first.end_lsn();
        assert!(mid > 0 && mid < end);
        assert!(!l.publisher().batch_from(mid, 1 << 20).unwrap().heartbeat);
    }

    /// The regression the obvious fix would have caused. A leader mid-append
    /// leaves a half-written record; a follower sitting exactly at its start has
    /// a *valid* cursor and must be told "nothing yet" so it retries, not "your
    /// cursor is dead". Refusing here would turn a self-healing 50 ms wait into a
    /// spurious re-bootstrap on every busy leader.
    #[test]
    fn a_torn_tail_at_the_cursor_is_a_heartbeat_not_a_dead_cursor() {
        let mut l = Log::new(0);
        for cv in 1..=3 {
            l.commit(cv);
        }
        let whole = l.bytes.len() as u64;
        // Chop mid-record, as a partially flushed append leaves it.
        l.bytes.truncate(l.bytes.len() - 10);

        let torn_at = {
            let mut at = 0u64;
            let mut sc = Scanner::new(&l.bytes, 0);
            for r in &mut sc {
                at += Record::framed_len(r.unwrap().body.len()) as u64;
            }
            at
        };
        assert!(torn_at < whole, "the cut did not leave a torn record");

        let b = l.publisher().batch_from(torn_at, 1 << 20).unwrap();
        assert!(
            b.heartbeat && b.records.is_empty(),
            "a cursor at the start of a torn tail is valid and must simply wait"
        );
    }

    #[test]
    fn a_request_outside_the_log_is_an_error() {
        let mut l = Log::new(0);
        l.commit(1);
        assert!(l.publisher().batch_from(99_999, 1024).is_err());
    }

    #[test]
    fn a_truncated_ship_advances_only_over_what_decoded() {
        let mut l = Log::new(0);
        for cv in 1..=4 {
            l.commit(cv);
        }
        let full = l.publisher().batch_from(0, 1 << 20).unwrap();
        // Chop the tail mid-record, as a dropped connection would.
        let cut = full.records.len() - 10;
        let partial = WalBatch::new(0, 0, full.records[..cut].to_vec());

        let mut f = follower(&[0]);
        f.apply(&partial).unwrap();
        let next = f.cursor(0).unwrap().next_lsn;
        assert!(
            next < cut as u64 + 10,
            "cursor must not run past decoded data"
        );

        // Resuming from there completes cleanly.
        f.apply(&l.publisher().batch_from(next, 1 << 20).unwrap())
            .unwrap();
        assert_eq!(f.visible(), 4);
    }

    // ---- RetentionFloor -----------------------------------------------------

    /// The failure per-follower retention exists to remove.
    ///
    /// Two standbys, one far behind. A window in which only the fast one acks
    /// must not cut the slow one out of the log — under the old
    /// minimum-since-last-ask rule it did, and the cost to the slow follower was
    /// a fresh copy of the entire database image.
    #[test]
    fn a_window_only_the_fast_follower_acked_in_still_protects_the_slow_one() {
        let r = RetentionFloor::new();
        r.observe_from("fast", 0, 1_000);
        r.observe_from("slow", 0, 100);
        assert_eq!(r.take(0), Some(100));

        // The window in which `slow` is busy bootstrapping and says nothing.
        r.observe_from("fast", 0, 2_000);
        assert_eq!(
            r.take(0),
            Some(100),
            "the slow follower lost its floor after one silent window"
        );
    }

    /// And the grace is finite. A follower that is genuinely gone must stop
    /// holding the log, or a dead standby halts reclamation for ever.
    #[test]
    fn a_silent_follower_is_evicted_after_the_grace_and_not_before() {
        let r = RetentionFloor::new();
        r.observe_from("fast", 0, 1_000);
        r.observe_from("gone", 0, 100);
        assert_eq!(r.take(0), Some(100));

        for window in 1..=RETENTION_GRACE {
            r.observe_from("fast", 0, 1_000 + window as u64);
            assert_eq!(
                r.take(0),
                Some(100),
                "evicted during the grace, at window {window}"
            );
        }
        r.observe_from("fast", 0, 9_000);
        assert_eq!(
            r.take(0),
            Some(9_000),
            "a dead follower held the log past the grace"
        );
        assert_eq!(r.holders(0), 1);
    }

    /// `holders` must count the followers of **one** shard, and every test
    /// that called it passed shard `0`.
    ///
    /// Found 2026-09-09 by the always-identity-argument sweep: `holders( .. )`
    /// was `0` at all six call sites, so the `shard` parameter was never varied
    /// and the discrimination it exists for was never exercised. Replacing the
    /// filter with `true` — counting every follower of every shard — broke
    /// **no** test in the workspace.
    ///
    /// The general shape: a parameter that only ever takes one value is a
    /// branch that is never taken. A caller sweep cannot see it, because the
    /// function *is* called.
    #[test]
    fn holders_counts_one_shard_and_not_the_others() {
        let r = RetentionFloor::new();
        r.observe_from("a", 0, 10);
        r.observe_from("b", 0, 11);
        r.observe_from("c", 1, 12);
        r.observe_from("d", 2, 13);
        r.observe_from("e", 2, 14);

        assert_eq!(r.holders(0), 2, "shard 0 has two followers");
        assert_eq!(r.holders(1), 1, "shard 1 has one");
        assert_eq!(r.holders(2), 2, "shard 2 has two");
        assert_eq!(r.holders(3), 0, "a shard nobody follows has none");

        // The sum is what a filter of `true` would return for every shard,
        // so asserting it separately is what makes the discrimination visible.
        let total: usize = (0..4).map(|s| r.holders(s)).sum();
        assert_eq!(total, 5, "every follower is counted exactly once");
    }

    /// A follower that keeps acking keeps its protection indefinitely, which is
    /// the case the eviction rule must not catch.
    #[test]
    fn a_live_follower_is_never_evicted_however_long_it_runs() {
        let r = RetentionFloor::new();
        for round in 0..(RETENTION_GRACE as u64 + 5) * 3 {
            r.observe_from("a", 0, round);
            r.observe_from("b", 0, round * 2);
            assert_eq!(r.take(0), Some(round));
        }
        assert_eq!(r.holders(0), 2);
    }

    /// Assignment, not `min`, within one follower — and the case that proves
    /// it matters is a **log cut**, after which a follower's LSN restarts near
    /// zero. A `min` would pin the floor at the pre-cut value for ever.
    #[test]
    fn one_followers_lsn_may_fall_after_a_log_cut() {
        let r = RetentionFloor::new();
        r.observe_from("a", 0, 5_000);
        assert_eq!(r.take(0), Some(5_000));
        r.observe_from("a", 0, 12); // the log was cut; it resumes near zero
        assert_eq!(r.take(0), Some(12));
        r.observe_from("a", 0, 900);
        assert_eq!(r.take(0), Some(900), "a min would have pinned this at 12");
    }

    /// Shards are independent, and `take` ages only the shard it was asked for.
    #[test]
    fn taking_one_shard_does_not_age_another() {
        let r = RetentionFloor::new();
        r.observe_from("a", 0, 10);
        r.observe_from("a", 1, 20);
        for _ in 0..(RETENTION_GRACE + 3) {
            assert_eq!(r.take(0), Some(10));
            r.observe_from("a", 0, 10);
        }
        assert_eq!(r.take(1), Some(20), "shard 1 aged while shard 0 was asked");
    }

    /// With no identity every acker shares one entry, and it must read the
    /// **minimum** within the window — because "a different follower reported"
    /// is indistinguishable from "this follower moved".
    #[test]
    fn anonymous_acks_collapse_pessimistically() {
        let r = RetentionFloor::new();
        r.observe(0, 1_000);
        r.observe(0, 100);
        r.observe(0, 5_000);
        assert_eq!(r.take(0), Some(100));
        assert_eq!(r.holders(0), 1, "anonymous callers must not grow the map");

        // And across windows it is the latest window that counts, not the
        // minimum ever seen — otherwise the floor could never rise.
        r.observe(0, 4_000);
        r.observe(0, 6_000);
        assert_eq!(r.take(0), Some(4_000));
    }

    /// An empty name is anonymous, not a follower called "".
    #[test]
    fn an_empty_identity_is_the_anonymous_entry() {
        let r = RetentionFloor::new();
        r.observe_from("", 0, 900);
        r.observe(0, 100);
        assert_eq!(r.take(0), Some(100));
        assert_eq!(r.holders(0), 1);
    }

    /// No acks at all means no floor, so a checkpoint with no followers is free.
    #[test]
    fn a_floor_nobody_asked_for_is_none() {
        let r = RetentionFloor::new();
        assert_eq!(r.take(0), None);
        r.observe_from("a", 0, 10);
        assert_eq!(r.take(0), Some(10));
        for _ in 0..=RETENTION_GRACE {
            r.take(0);
        }
        assert_eq!(
            r.take(0),
            None,
            "an evicted follower must leave no floor behind"
        );
        assert_eq!(r.holders(0), 0);
    }
}
