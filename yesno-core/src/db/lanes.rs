//! Lockstep iteration over several keys' chunk streams under one snapshot.
//!
//! # Why this is not an n-ary operator
//!
//! `stream/` already composes many streams, but every operator there *combines*
//! them into one answer: `And` yields the intersection, `Or` the union. A scorer
//! wants the opposite — the chunks of N keys, at the same prefix, **still
//! separate**, so it can tile them and compute something the set algebra does not
//! express. `view/fold.rs` walks sets aligned but folds as it goes, so it cannot
//! serve either. Hence a type whose whole job is to advance N streams together
//! and hand out what each one holds.
//!
//! # One snapshot, and why that is the correctness property
//!
//! Every lane comes from the same [`Snapshot`], so every lane is read at the same
//! version. Opening one stream per key *separately* takes one snapshot each, and a
//! checkpoint between two of them scores one block against two database states —
//! which is exactly what the C `yesno_cursor_open` does today, one cursor at a
//! time. The bug that motivates this type is silent: the answer is wrong, not
//! absent.
//!
//! # Blocks, and absence
//!
//! A **block** is the next prefix at which *any* lane has a chunk. A lane with no
//! chunk there is reported as `None` rather than omitted, so a caller's lane
//! indices never shift under it — the caller passed keys in an order and gets
//! answers in that order for the life of the handle.
//!
//! At most one [`Container`] per lane is retained. That bound is the reason the
//! API is block-scoped rather than handing out a cursor per lane: a caller that
//! could hold every visited chunk would grow its working set with the length of
//! the scan, and nothing needs that.

use std::sync::Arc;

use crate::container::Container;
use crate::db::keystream::KeyStream;
use crate::db::Snapshot;
use crate::error::{CodecError, Result};
use crate::mvcc::Version;
use crate::stream::ChunkStream;
use crate::Prefix48;

/// Several keys' chunks, advanced together, at one version.
pub struct KeyLanes {
    streams: Vec<KeyStream>,
    /// Prefix each lane will yield next, or `None` once it is exhausted.
    ///
    /// Kept as a lookahead so [`Self::advance`] can choose the next block
    /// without materializing a payload it may not want — `peek_prefix` exists
    /// precisely so that ordering decisions cost no refcount bump.
    heads: Vec<Option<Prefix48>>,
    /// This block's chunk per lane. `None` is "absent here", not "lane gone".
    current: Vec<Option<Container>>,
    prefix: Option<Prefix48>,
    /// The half-open prefix window every lane was planned over.
    ///
    /// `( 0, PREFIX_EXCLUSIVE_END )` for a whole-key handle, so the window is
    /// not a second mode -- every handle has one and the unbounded case is the
    /// widest rather than the absent one.
    lo: Prefix48,
    hi: Prefix48,
    /// The highest prefix this handle has already emitted or been asked for.
    ///
    /// **One position, shared by both ways of moving.** `advance` walks to the
    /// next block and `seek_block` jumps to a named one; a handle that kept a
    /// separate mark per mode would let a caller interleave them and read a
    /// prefix twice, or read one it had already passed and get an empty answer
    /// that looks like absence. Keeping one mark makes mixing the two legal and
    /// monotone instead of forbidden and unchecked.
    last: Option<Prefix48>,
    keys: Arc<[u64]>,
    /// The snapshot this handle reads at, held rather than merely consulted.
    ///
    /// **Not redundant with the streams' own slots.** Each `KeyStream` holds an
    /// `Arc<ReaderSlot>`, so a handle with lanes would pin the version anyway --
    /// but a handle with *no* lanes would pin nothing while still reporting a
    /// version, and the ABI above this counts every handle as one outstanding
    /// lease regardless of how many keys it was given. Holding the clone makes
    /// the pin independent of the lane count, so "this lease is outstanding" and
    /// "this version is pinned" cannot disagree.
    snap: Snapshot,
    /// Set when an advance failed partway.
    ///
    /// **A failed advance cannot be undone.** `next_chunk` has already moved the
    /// lanes it reached, so there is no state to roll back to. What the caller
    /// must never see is a *partly populated* block — tiling lanes 0..6 of a
    /// block whose lane 7 failed produces a plausible wrong answer. So the block
    /// is cleared and the handle refuses every later call, which turns an
    /// unrecoverable position into an honest one.
    poisoned: bool,
}

impl KeyLanes {
    /// Open one lane per key, all at `snap`'s version.
    ///
    /// Fails as a whole: a handle is never returned with some lanes open, because
    /// a caller cannot tell a lane that failed to open from one that is merely
    /// empty.
    pub fn new(snap: &Snapshot, keys: &[u64]) -> Result<KeyLanes> {
        Self::in_prefix_range(snap, keys, 0, crate::PREFIX_EXCLUSIVE_END)
    }

    /// Open one lane per key over the half-open prefix window `[lo, hi)`.
    ///
    /// # What the window is for
    ///
    /// A scorer that already knows which prefixes it cares about pays, without
    /// this, for every chunk each key holds: the lanes walk the whole key and
    /// the caller discards what it did not want. Planning the window instead
    /// moves that decision below the index descent, so the chunks outside it
    /// are never resolved, never read and never counted against the one
    /// container per lane the handle retains.
    ///
    /// # Still one batch, and still one snapshot
    ///
    /// The planning stays batched -- one memtable acquisition and one node
    /// reader per shard, as the unbounded acquisition already was -- because a
    /// bounded acquisition that planned a key at a time would hand back exactly
    /// the setup cost batching was introduced to remove. The window is a
    /// parameter to that shared planner rather than a planner of its own;
    /// `KeyStream::build_plans_range` records why, and narrowing costs the
    /// sharing nothing because it only moves the cursor's endpoints inward. And the snapshot is
    /// held here as it is in [`Self::new`], so a window that happens to contain
    /// nothing, or a call with no keys at all, still pins the version it
    /// reports.
    ///
    /// `lo == hi` is legal and yields a handle with no blocks: an empty window
    /// is a question with an empty answer, not an error, and `END..END` is the
    /// one a caller computes by accident at the end of a scan. `hi` may be
    /// [`crate::PREFIX_EXCLUSIVE_END`] and no higher; anything else is
    /// rejected before a single endpoint is packed.
    pub fn in_prefix_range(
        snap: &Snapshot,
        keys: &[u64],
        lo: Prefix48,
        hi: Prefix48,
    ) -> Result<KeyLanes> {
        let mut streams = super::keystream::KeyStream::batch_in_prefix_range(snap, keys, lo, hi)?;
        let mut heads = Vec::with_capacity(streams.len());
        for s in &mut streams {
            heads.push(s.peek_prefix()?);
        }
        Ok(KeyLanes {
            current: vec![None; streams.len()],
            streams,
            heads,
            prefix: None,
            lo,
            hi,
            last: None,
            keys: keys.into(),
            snap: snap.clone(),
            poisoned: false,
        })
    }

    /// Whether `[lo, hi)` is a window [`Self::in_prefix_range`] would accept.
    ///
    /// **The same check the constructor runs, not a second copy of the rule.** A
    /// transport has to refuse a bad window as *caller input* -- with whatever its
    /// own vocabulary calls an invalid argument -- and the constructor's `Err`
    /// cannot say that, because it also covers a snapshot that has gone away.
    /// Asking first is how a transport gets an attributable refusal; the
    /// constructor still checks, so a transport that forgets to ask cannot widen
    /// what is legal.
    pub fn check_prefix_range(lo: Prefix48, hi: Prefix48) -> Result<()> {
        super::keystream::KeyStream::check_prefix_range(lo, hi)
    }

    /// The half-open prefix window this handle was opened over.
    pub fn window(&self) -> (Prefix48, Prefix48) {
        (self.lo, self.hi)
    }

    /// The highest prefix already emitted or requested, if any.
    ///
    /// What [`Self::seek_block`] measures "strictly forward" against, exposed
    /// so a transport can reject a caller's batch with a message naming the
    /// position rather than only the verdict.
    pub fn position(&self) -> Option<Prefix48> {
        self.last
    }

    /// How many lanes, which is how many keys were asked for.
    pub fn lanes(&self) -> usize {
        self.streams.len()
    }

    /// The keys, in the order they were passed.
    pub fn keys(&self) -> &[u64] {
        &self.keys
    }

    /// The version every lane is read at.
    ///
    /// Read from the held snapshot rather than copied at construction, so there
    /// is no second copy that could disagree with the thing doing the pinning.
    pub fn version(&self) -> Version {
        self.snap.version()
    }

    /// The snapshot this handle pins.
    pub fn snapshot(&self) -> &Snapshot {
        &self.snap
    }

    /// The current block's prefix, or `None` before the first advance and after
    /// the last.
    pub fn prefix(&self) -> Option<Prefix48> {
        self.prefix
    }

    /// Advance to the next block, dropping the previous one's chunks.
    ///
    /// `Ok(None)` when every lane is exhausted. On `Err` the handle is poisoned
    /// and every later call fails; see [`KeyLanes::poisoned`].
    pub fn advance(&mut self) -> Result<Option<Prefix48>> {
        if self.poisoned {
            return Err(CodecError::Invariant(
                "these key lanes were poisoned by an earlier failed advance",
            ));
        }
        // Dropping first is what holds the one-container-per-lane bound: the
        // previous block's payloads go before the next block's arrive, rather
        // than both being live across the switch.
        for c in &mut self.current {
            *c = None;
        }
        let Some(next) = self.heads.iter().flatten().copied().min() else {
            self.prefix = None;
            return Ok(None);
        };
        for i in 0..self.streams.len() {
            if self.heads[i] != Some(next) {
                continue;
            }
            match self.step(i) {
                Ok(()) => {}
                Err(e) => {
                    self.poison();
                    return Err(e);
                }
            }
        }
        self.prefix = Some(next);
        // The shared position, moved by this mode as well as by `seek_block`.
        // A sequential walk that left it alone would let a later target
        // re-read a block this call had already emitted.
        self.last = Some(next);
        Ok(Some(next))
    }

    /// Whether this batch of target prefixes may be read, changing nothing.
    ///
    /// **The whole batch is checked before any of it is executed**, which is
    /// the property a caller cannot reconstruct from per-target checks: a batch
    /// whose fourth target is backwards must leave the handle exactly where it
    /// was, not three blocks further on with no way back. [`Self::seek_block`]
    /// re-checks each target on its own, so the invariant holds for a caller
    /// that never calls this; what this adds is that a bad batch costs nothing.
    ///
    /// Requires targets strictly ascending -- which also makes them unique --
    /// inside the handle's window, and strictly past [`Self::position`]. An
    /// empty batch is rejected rather than treated as a no-op: a caller asking
    /// for nothing has computed its targets wrongly, and the alternative is a
    /// reply with no blocks that looks like the end of the scan.
    ///
    /// **Deliberately not checked here: how many targets, or how large their
    /// payload.** Those are properties of a transport's advertised limits, and
    /// this type has no transport. A caller that has advertised a batch cap
    /// enforces it before calling, which is also the order that keeps the
    /// refusal attributable to the limit the peer was told about.
    ///
    /// **A poisoned handle is refused here as well, and a caller that tells an
    /// engine failure apart from a bad request has to ask [`Self::poisoned`]
    /// first.** The error is the same `Invariant` either way: this type has one
    /// error for a broken invariant and no notion of whose fault it is. A
    /// transport that mapped every refusal from this method onto "the peer sent
    /// something wrong" would describe a handle killed by a failed read as a
    /// batch the peer composed badly -- which is the one distinction a caller
    /// reporting statuses most needs to keep, and one that did go wrong for a day
    /// in this tree.
    pub fn check_targets(&self, prefixes: &[Prefix48]) -> Result<()> {
        if self.poisoned {
            return Err(CodecError::Invariant(
                "these key lanes were poisoned by an earlier failed advance",
            ));
        }
        if prefixes.is_empty() {
            return Err(CodecError::Invariant("a target batch must name a prefix"));
        }
        if prefixes.windows(2).any(|p| p[0] >= p[1]) {
            return Err(CodecError::Invariant(
                "target prefixes must be strictly ascending",
            ));
        }
        if prefixes.iter().any(|&p| p < self.lo || p >= self.hi) {
            return Err(CodecError::Invariant(
                "a target prefix lies outside this handle's window",
            ));
        }
        if self.last.is_some_and(|at| prefixes[0] <= at) {
            return Err(CodecError::Invariant(
                "a target prefix is at or behind a prefix already read",
            ));
        }
        Ok(())
    }

    /// Position every lane at exactly `prefix` and open that block.
    ///
    /// Unlike [`Self::advance`], which goes to the next prefix *any* lane
    /// holds, this goes to the one named -- so a caller gathering a sparse set
    /// of prefixes pays one seek per prefix it wants rather than one advance
    /// per prefix that exists. A target no lane holds is still a block, with
    /// every lane absent, because a caller tiling N lanes over M targets needs
    /// M answers in the order it asked for them; dropping the empty ones would
    /// shift the rest.
    ///
    /// Each call is checked on its own, so the forward-only invariant holds
    /// whether or not [`Self::check_targets`] ran: a prefix outside the window
    /// or at or behind [`Self::position`] is refused **with the handle
    /// unchanged**. An operational failure -- a seek or a payload read -- is
    /// not recoverable in the same way and poisons the handle, for the reason
    /// [`Self::advance`] gives: a partly populated block is a plausible wrong
    /// answer.
    pub fn seek_block(&mut self, prefix: Prefix48) -> Result<()> {
        if self.poisoned {
            return Err(CodecError::Invariant(
                "these key lanes were poisoned by an earlier failed advance",
            ));
        }
        if prefix < self.lo || prefix >= self.hi {
            return Err(CodecError::Invariant(
                "a target prefix lies outside this handle's window",
            ));
        }
        if self.last.is_some_and(|at| prefix <= at) {
            return Err(CodecError::Invariant(
                "a target prefix is at or behind a prefix already read",
            ));
        }
        // Past the point of refusal, so from here a failure poisons rather
        // than returning the handle intact.
        for c in &mut self.current {
            *c = None;
        }
        for i in 0..self.streams.len() {
            match self.seek_lane(i, prefix) {
                Ok(()) => {}
                Err(e) => {
                    self.poison();
                    return Err(e);
                }
            }
        }
        self.prefix = Some(prefix);
        self.last = Some(prefix);
        Ok(())
    }

    /// Move lane `i` to `prefix` and take its chunk if it has one there.
    fn seek_lane(&mut self, i: usize, prefix: Prefix48) -> Result<()> {
        self.streams[i].seek(prefix)?;
        self.heads[i] = self.streams[i].peek_prefix()?;
        if self.heads[i] != Some(prefix) {
            // Absent here. The lane is left where the seek put it, which is
            // at the first chunk it holds at or after `prefix`, so a later
            // target further on costs no rewind.
            return Ok(());
        }
        self.step(i)
    }

    /// Pull lane `i`'s chunk into the current block and refresh its lookahead.
    fn step(&mut self, i: usize) -> Result<()> {
        let got = self.streams[i].next_chunk()?;
        match got {
            Some((p, c)) => {
                debug_assert_eq!(
                    Some(p),
                    self.heads[i],
                    "a lane yielded a prefix its own peek did not promise"
                );
                self.current[i] = Some(c);
            }
            // `peek_prefix` is documented as a lower bound rather than a promise,
            // so a lane that peeked and then yielded nothing is permitted. It
            // means "absent at this block", which is already what `None` says.
            None => self.current[i] = None,
        }
        self.heads[i] = self.streams[i].peek_prefix()?;
        Ok(())
    }

    /// Mark this handle unusable.
    ///
    /// Public because a **caught panic** has to reach the same state an `Err`
    /// does. An FFI layer that wraps `advance` in `catch_unwind` turns a panic
    /// midway through the lane loop into a status code, at which point the
    /// streams are partly advanced and the heads partly refreshed -- exactly the
    /// position the poison exists for -- but nothing inside this type ran to set
    /// it. Without this the caller retries and gets a block with duplicated or
    /// skipped chunks, which is the silent wrong answer the whole mechanism is
    /// meant to prevent.
    pub fn poison(&mut self) {
        self.poisoned = true;
        self.prefix = None;
        for c in &mut self.current {
            *c = None;
        }
    }

    /// Whether an earlier advance failed and left this handle unusable.
    pub fn poisoned(&self) -> bool {
        self.poisoned
    }

    /// Drop the current block's chunks without advancing.
    ///
    /// Exists so a consumer that has promised its callers "these payloads are
    /// invalid after you release the block" can make that **true** rather than
    /// merely documented. Without it the containers survive until the next
    /// advance, so a use-after-release reads correct data and the bug is found
    /// later, somewhere else, by someone else.
    pub fn release_block(&mut self) {
        for c in &mut self.current {
            *c = None;
        }
        self.prefix = None;
    }

    /// Lane `i` of the current block, or `None` when that lane has no chunk here.
    ///
    /// Out of range is `None` as well: a caller indexing past its own lane count
    /// has a bug of its own, and panicking inside a read path that the C ABI
    /// wraps would cross a `catch_unwind` boundary to say so.
    pub fn lane(&self, i: usize) -> Option<&Container> {
        self.current.get(i).and_then(|c| c.as_ref())
    }

    /// How many lanes have a chunk in the current block.
    pub fn present(&self) -> usize {
        self.current.iter().filter(|c| c.is_some()).count()
    }
}

#[cfg(test)]
mod tests {
    use crate::{Db, DbOptions, KeyLanes};

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("yesno-lanes-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    struct Clean(std::path::PathBuf);
    impl Drop for Clean {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Three lanes with deliberately ragged prefixes: every combination of
    /// present and absent has to appear, or the absence reporting is untested.
    fn ragged(dir: &std::path::Path) -> Db {
        let db = Db::open_with(
            dir,
            DbOptions {
                shards: 1,
                ..Default::default()
            },
        )
        .unwrap();
        let mut b = db.batch();
        // key 10: chunks 0, 1, 5
        for c in [0u64, 1, 5] {
            b.insert(10, c * 65536 + 7);
        }
        // key 20: chunks 1, 5
        for c in [1u64, 5] {
            b.insert(20, c * 65536 + 9);
        }
        // key 30: chunk 3 only
        b.insert(30, 3 * 65536 + 11);
        b.commit().unwrap();
        db.checkpoint().unwrap();
        db
    }

    /// The block sequence is the union of the lanes' prefixes, and a lane with no
    /// chunk at a block reads absent rather than shifting the lane indices.
    #[test]
    fn a_block_is_the_union_of_prefixes_and_absence_keeps_lane_order() {
        let dir = tmpdir("union");
        let _c = Clean(dir.clone());
        let db = ragged(&dir);
        let snap = db.snapshot().unwrap();
        let mut lanes = KeyLanes::new(&snap, &[10, 20, 30]).unwrap();

        let mut seen = Vec::new();
        while let Some(p) = lanes.advance().unwrap() {
            seen.push((
                p,
                lanes.lane(0).is_some(),
                lanes.lane(1).is_some(),
                lanes.lane(2).is_some(),
            ));
        }
        assert_eq!(
            seen,
            vec![
                (0, true, false, false),
                (1, true, true, false),
                (3, false, false, true),
                (5, true, true, false),
            ],
            "blocks must be the union of lane prefixes, with absence reported in place"
        );
    }

    /// Every lane reads at the snapshot's version, which is the whole point of
    /// taking the snapshot once. A commit and checkpoint mid-scan must not show.
    #[test]
    fn a_write_during_the_scan_is_invisible_to_every_lane() {
        let dir = tmpdir("iso");
        let _c = Clean(dir.clone());
        let db = ragged(&dir);
        let snap = db.snapshot().unwrap();
        let mut lanes = KeyLanes::new(&snap, &[10, 20]).unwrap();
        assert_eq!(lanes.version(), snap.version());

        lanes.advance().unwrap();
        // Add a chunk in the middle of the block sequence and publish it.
        let mut b = db.batch();
        b.insert(20, 2 * 65536 + 1);
        b.commit().unwrap();
        db.checkpoint().unwrap();

        let mut rest = Vec::new();
        while let Some(p) = lanes.advance().unwrap() {
            rest.push(p);
        }
        assert_eq!(
            rest,
            vec![1, 5],
            "prefix 2 was committed after the snapshot"
        );

        // And a fresh snapshot does see it, so the test is about isolation rather
        // than about the write having failed.
        let after = db.snapshot().unwrap();
        let mut fresh = KeyLanes::new(&after, &[20]).unwrap();
        let mut ps = Vec::new();
        while let Some(p) = fresh.advance().unwrap() {
            ps.push(p);
        }
        assert_eq!(ps, vec![1, 2, 5]);
    }

    /// At most one container per lane is retained, which is what makes the API
    /// block-scoped rather than handle-scoped. Asserted through `present`, since
    /// the containers themselves are not observable from outside.
    #[test]
    fn the_previous_block_is_dropped_before_the_next_arrives() {
        let dir = tmpdir("bound");
        let _c = Clean(dir.clone());
        let db = ragged(&dir);
        let snap = db.snapshot().unwrap();
        let mut lanes = KeyLanes::new(&snap, &[10, 20, 30]).unwrap();

        assert_eq!(
            lanes.present(),
            0,
            "nothing is live before the first advance"
        );
        lanes.advance().unwrap();
        assert_eq!(lanes.present(), 1, "prefix 0 has only lane 0");
        lanes.advance().unwrap();
        assert_eq!(
            lanes.present(),
            2,
            "prefix 1 has lanes 0 and 1, and not lane 0 twice"
        );
        lanes.advance().unwrap();
        assert_eq!(
            lanes.present(),
            1,
            "prefix 3 has only lane 2, so the others were dropped"
        );
        while lanes.advance().unwrap().is_some() {}
        assert_eq!(
            lanes.present(),
            0,
            "the last block is released when the scan ends"
        );
    }

    /// Releasing a block drops its payloads there and then.
    ///
    /// The C ABI above promises that pointers taken from a block are invalid once
    /// the block is released. If release only flipped a flag and the containers
    /// survived to the next advance, a use-after-release would read **correct**
    /// data and the bug would surface somewhere else entirely.
    #[test]
    fn releasing_a_block_drops_its_payloads_immediately() {
        let dir = tmpdir("release");
        let _c = Clean(dir.clone());
        let db = ragged(&dir);
        let snap = db.snapshot().unwrap();
        let mut lanes = KeyLanes::new(&snap, &[10, 20]).unwrap();

        assert_eq!(lanes.advance().unwrap(), Some(0));
        assert_eq!(lanes.present(), 1);
        lanes.release_block();
        assert_eq!(lanes.present(), 0, "release must drop, not merely mark");
        assert_eq!(lanes.prefix(), None);
        // And the scan continues from where it was, rather than restarting.
        assert_eq!(lanes.advance().unwrap(), Some(1));
        assert_eq!(lanes.present(), 2);
    }

    /// A poisoned handle refuses everything, and says so the same way twice.
    ///
    /// The path that matters is a **caught panic** in the FFI layer, which turns
    /// a panic midway through the lane loop into a status code while the streams
    /// are partly advanced. That layer calls `poison` for exactly this reason, so
    /// the behaviour it depends on is asserted here rather than only there.
    #[test]
    fn a_poisoned_handle_refuses_every_later_advance() {
        let dir = tmpdir("poison");
        let _c = Clean(dir.clone());
        let db = ragged(&dir);
        let snap = db.snapshot().unwrap();
        let mut lanes = KeyLanes::new(&snap, &[10]).unwrap();

        assert_eq!(lanes.advance().unwrap(), Some(0));
        assert!(!lanes.poisoned());
        lanes.poison();
        assert!(lanes.poisoned());
        assert_eq!(lanes.present(), 0, "poisoning clears the open block");
        assert_eq!(lanes.prefix(), None);
        assert!(lanes.advance().is_err(), "and every later advance fails");
        assert!(
            lanes.advance().is_err(),
            "repeatably, rather than failing once and then appearing to recover"
        );
    }

    /// Two handles on one snapshot are independent, and both read one version.
    ///
    /// This is the single-threaded half of the concurrency contract; the threaded
    /// version lives in `tests/zero_copy_mvcc.rs`, where a writer can run beside
    /// it. Advancing one handle must not move the other.
    #[test]
    fn two_handles_on_one_snapshot_advance_independently() {
        let dir = tmpdir("two");
        let _c = Clean(dir.clone());
        let db = ragged(&dir);
        let snap = db.snapshot().unwrap();
        let mut a = KeyLanes::new(&snap, &[10]).unwrap();
        let mut b = KeyLanes::new(&snap, &[10]).unwrap();

        assert_eq!(a.advance().unwrap(), Some(0));
        assert_eq!(a.advance().unwrap(), Some(1));
        // `b` has not moved.
        assert_eq!(b.advance().unwrap(), Some(0));
        assert_eq!(a.version(), b.version());
    }

    /// An out-of-range lane index answers absent rather than panicking, because
    /// the C ABI wraps these calls and a panic would have to cross `catch_unwind`
    /// to report a caller's indexing bug.
    #[test]
    fn an_out_of_range_lane_is_absent_rather_than_a_panic() {
        let dir = tmpdir("oob");
        let _c = Clean(dir.clone());
        let db = ragged(&dir);
        let snap = db.snapshot().unwrap();
        let mut lanes = KeyLanes::new(&snap, &[10]).unwrap();
        lanes.advance().unwrap();
        assert!(lanes.lane(0).is_some());
        assert!(lanes.lane(1).is_none());
        assert!(lanes.lane(usize::MAX).is_none());
    }

    /// Zero lanes is a valid, immediately-exhausted handle rather than an error:
    /// a scorer whose block selected no keys should not have to special-case it.
    #[test]
    fn no_keys_is_an_empty_scan_not_a_failure() {
        let dir = tmpdir("empty");
        let _c = Clean(dir.clone());
        let db = ragged(&dir);
        let snap = db.snapshot().unwrap();
        let mut lanes = KeyLanes::new(&snap, &[]).unwrap();
        assert_eq!(lanes.lanes(), 0);
        assert_eq!(lanes.advance().unwrap(), None);
    }

    /// A key with no chunks at all is a lane that is always absent, not a lane
    /// that is missing. Lane indices must line up with the keys as passed.
    #[test]
    fn an_absent_key_still_occupies_its_lane() {
        let dir = tmpdir("gone");
        let _c = Clean(dir.clone());
        let db = ragged(&dir);
        let snap = db.snapshot().unwrap();
        let mut lanes = KeyLanes::new(&snap, &[999, 10]).unwrap();
        assert_eq!(lanes.lanes(), 2);
        assert_eq!(lanes.keys(), &[999, 10]);
        assert_eq!(lanes.advance().unwrap(), Some(0));
        assert!(lanes.lane(0).is_none(), "key 999 holds nothing anywhere");
        assert!(lanes.lane(1).is_some(), "and key 10 is still lane 1");
    }
    // -----------------------------------------------------------------------
    // Bounded windows
    // -----------------------------------------------------------------------

    /// The window is a planning bound, not a filter applied afterwards: the
    /// blocks outside it do not appear, and the ones inside it are unchanged.
    #[test]
    fn a_window_yields_exactly_the_blocks_inside_it() {
        let dir = tmpdir("window");
        let _c = Clean(dir.clone());
        let db = ragged(&dir);
        let snap = db.snapshot().unwrap();

        let mut lanes = KeyLanes::in_prefix_range(&snap, &[10, 20, 30], 1, 5).unwrap();
        assert_eq!(lanes.window(), (1, 5));
        let mut seen = Vec::new();
        while let Some(p) = lanes.advance().unwrap() {
            seen.push((
                p,
                lanes.lane(0).is_some(),
                lanes.lane(1).is_some(),
                lanes.lane(2).is_some(),
            ));
        }
        assert_eq!(
            seen,
            vec![(1, true, true, false), (3, false, false, true)],
            "prefix 0 is below the window and prefix 5 is the exclusive end"
        );
    }

    /// A bounded handle must agree with an unbounded one on the blocks they
    /// share. Planning the window narrows which `ChunkKey` endpoints the cursor
    /// is aimed at, and that is all it may change.
    #[test]
    fn a_window_agrees_with_the_unbounded_scan_on_the_overlap() {
        let dir = tmpdir("window-agree");
        let _c = Clean(dir.clone());
        let db = ragged(&dir);
        let snap = db.snapshot().unwrap();

        let mut whole = KeyLanes::new(&snap, &[10, 20, 30]).unwrap();
        let mut full = Vec::new();
        while let Some(p) = whole.advance().unwrap() {
            full.push((p, whole.present(), whole.lane(0).map(|c| c.len())));
        }

        let mut bounded = KeyLanes::in_prefix_range(&snap, &[10, 20, 30], 1, 6).unwrap();
        let mut part = Vec::new();
        while let Some(p) = bounded.advance().unwrap() {
            part.push((p, bounded.present(), bounded.lane(0).map(|c| c.len())));
        }

        let expect: Vec<_> = full
            .into_iter()
            .filter(|&(p, _, _)| (1..6).contains(&p))
            .collect();
        assert_eq!(part, expect, "a window may drop blocks, never alter one");
    }

    /// An empty window is a question with an empty answer. It still pins the
    /// version it reports, which is what a caller computing `END..END` at the
    /// end of a scan depends on.
    #[test]
    fn an_empty_window_still_pins_the_snapshot() {
        let dir = tmpdir("window-empty");
        let _c = Clean(dir.clone());
        let db = ragged(&dir);
        let snap = db.snapshot().unwrap();

        for (lo, hi) in [
            (3, 3),
            (crate::PREFIX_EXCLUSIVE_END, crate::PREFIX_EXCLUSIVE_END),
        ] {
            let mut lanes = KeyLanes::in_prefix_range(&snap, &[10, 20], lo, hi).unwrap();
            assert_eq!(lanes.lanes(), 2, "an empty window keeps its lane slots");
            assert_eq!(lanes.version(), snap.version(), "and keeps the snapshot");
            assert_eq!(lanes.advance().unwrap(), None);
        }

        // No keys and an empty window at once: still a handle, still pinned.
        let lanes = KeyLanes::in_prefix_range(&snap, &[], 0, 0).unwrap();
        assert_eq!(lanes.lanes(), 0);
        assert_eq!(lanes.version(), snap.version());
    }

    /// `2^48` is the one legal exclusive endpoint above a `Prefix48`, and
    /// anything past it is rejected rather than masked to zero.
    #[test]
    fn the_legal_exclusive_end_is_accepted_and_nothing_above_it_is() {
        let dir = tmpdir("window-end");
        let _c = Clean(dir.clone());
        let db = ragged(&dir);
        let snap = db.snapshot().unwrap();

        let end = crate::PREFIX_EXCLUSIVE_END;
        let mut whole = KeyLanes::in_prefix_range(&snap, &[10, 20, 30], 0, end).unwrap();
        let mut n = 0;
        while whole.advance().unwrap().is_some() {
            n += 1;
        }
        assert_eq!(n, 4, "0..2^48 is the unbounded scan, not an empty one");

        assert!(
            KeyLanes::in_prefix_range(&snap, &[10], 0, end + 1).is_err(),
            "an endpoint above 2^48 would pack to a prefix of zero"
        );
        assert!(
            KeyLanes::in_prefix_range(&snap, &[10], 5, 1).is_err(),
            "a reversed window is a caller bug, not an empty scan"
        );
    }

    // -----------------------------------------------------------------------
    // Target prefixes on a held handle
    // -----------------------------------------------------------------------

    /// One block per target, in the order asked for, including a target no lane
    /// holds and one past every stored chunk. A caller tiling N lanes over M
    /// targets needs M answers; dropping the empty ones would shift the rest.
    #[test]
    fn every_target_gets_a_block_even_when_no_lane_holds_it() {
        let dir = tmpdir("targets");
        let _c = Clean(dir.clone());
        let db = ragged(&dir);
        let snap = db.snapshot().unwrap();
        let mut lanes = KeyLanes::new(&snap, &[10, 20, 30]).unwrap();

        let targets = [1u64, 2, 3, 5, 9];
        lanes.check_targets(&targets).unwrap();
        let mut seen = Vec::new();
        for &t in &targets {
            lanes.seek_block(t).unwrap();
            seen.push((
                lanes.prefix(),
                lanes.lane(0).is_some(),
                lanes.lane(1).is_some(),
                lanes.lane(2).is_some(),
            ));
        }
        assert_eq!(
            seen,
            vec![
                (Some(1), true, true, false),
                // Nothing is stored at 2, and it is still a block.
                (Some(2), false, false, false),
                (Some(3), false, false, true),
                (Some(5), true, true, false),
                // Past the last chunk of every lane, and still a block.
                (Some(9), false, false, false),
            ],
            "a target with no chunk is an all-absent block, not a skipped one"
        );
    }

    /// A target batch is checked whole. A batch whose later element is bad must
    /// cost nothing -- not leave the handle part-way through with no way back.
    #[test]
    fn a_bad_later_target_leaves_the_position_untouched() {
        let dir = tmpdir("targets-batch");
        let _c = Clean(dir.clone());
        let db = ragged(&dir);
        let snap = db.snapshot().unwrap();
        let mut lanes = KeyLanes::new(&snap, &[10, 20]).unwrap();

        lanes.seek_block(1).unwrap();
        assert_eq!(lanes.position(), Some(1));

        for bad in [
            vec![3u64, 2], // not ascending
            vec![3, 3],    // not unique
            vec![1, 5],    // first is at the position already read
            vec![0, 5],    // first is behind it
            vec![],        // nothing asked for at all
        ] {
            assert!(
                lanes.check_targets(&bad).is_err(),
                "accepted a bad batch: {bad:?}"
            );
            assert_eq!(
                lanes.position(),
                Some(1),
                "a refused batch moved the handle"
            );
            assert_eq!(lanes.prefix(), Some(1), "a refused batch dropped the block");
            assert!(
                lanes.lane(0).is_some(),
                "a refused batch released a payload"
            );
        }

        // And the handle is still usable: the refusals were about the input.
        lanes.check_targets(&[3, 5]).unwrap();
        lanes.seek_block(3).unwrap();
        assert_eq!(lanes.prefix(), Some(3));
    }

    /// Caller-input rejection on a single target leaves the handle unchanged
    /// too, so the invariant holds for a caller that never calls
    /// `check_targets`.
    #[test]
    fn a_backwards_or_out_of_window_target_is_refused_unchanged() {
        let dir = tmpdir("targets-refuse");
        let _c = Clean(dir.clone());
        let db = ragged(&dir);
        let snap = db.snapshot().unwrap();
        let mut lanes = KeyLanes::in_prefix_range(&snap, &[10, 20], 1, 6).unwrap();

        assert!(lanes.seek_block(0).is_err(), "below the window");
        assert!(lanes.seek_block(6).is_err(), "at the exclusive end");
        assert_eq!(lanes.position(), None, "a refusal must not take a position");

        lanes.seek_block(5).unwrap();
        assert!(lanes.seek_block(5).is_err(), "at the prefix already read");
        assert!(lanes.seek_block(3).is_err(), "behind it");
        assert_eq!(lanes.position(), Some(5));
        assert!(
            lanes.lane(0).is_some(),
            "and the block survived the refusals"
        );
        assert!(!lanes.poisoned(), "caller input must not poison");
    }

    /// Both ways of moving share one forward position, so mixing them is legal
    /// and monotone rather than forbidden and unchecked.
    #[test]
    fn sequential_and_target_moves_share_one_position() {
        let dir = tmpdir("targets-mixed");
        let _c = Clean(dir.clone());
        let db = ragged(&dir);
        let snap = db.snapshot().unwrap();
        let mut lanes = KeyLanes::new(&snap, &[10, 20, 30]).unwrap();

        assert_eq!(lanes.advance().unwrap(), Some(0));
        assert_eq!(
            lanes.position(),
            Some(0),
            "a sequential block takes the position"
        );

        lanes.seek_block(3).unwrap();
        assert_eq!(lanes.position(), Some(3));
        // The sequential walk resumes past the target rather than repeating
        // what the seek consumed.
        assert_eq!(lanes.advance().unwrap(), Some(5));
        assert_eq!(lanes.position(), Some(5));
        assert!(
            lanes.seek_block(5).is_err(),
            "a target may not re-read the block the walk just emitted"
        );
        assert_eq!(lanes.advance().unwrap(), None);
    }

    /// Duplicate keys keep their own lanes under a target read, as they do
    /// under a sequential one.
    #[test]
    fn duplicate_keys_keep_separate_lanes_at_a_target() {
        let dir = tmpdir("targets-dup");
        let _c = Clean(dir.clone());
        let db = ragged(&dir);
        let snap = db.snapshot().unwrap();
        let mut lanes = KeyLanes::new(&snap, &[10, 10, 30]).unwrap();

        lanes.seek_block(0).unwrap();
        assert_eq!(lanes.present(), 2, "both copies of key 10 hold prefix 0");
        assert_eq!(
            lanes.lane(0).map(|c| c.len()),
            lanes.lane(1).map(|c| c.len()),
            "and they read the same chunk"
        );
        assert!(lanes.lane(2).is_none());
    }

    /// A poisoned handle refuses a target read for the same reason it refuses an
    /// advance: the streams are in an unknown state and a block built from them
    /// is a plausible wrong answer.
    #[test]
    fn a_poisoned_handle_refuses_targets() {
        let dir = tmpdir("targets-poison");
        let _c = Clean(dir.clone());
        let db = ragged(&dir);
        let snap = db.snapshot().unwrap();
        let mut lanes = KeyLanes::new(&snap, &[10, 20]).unwrap();

        lanes.advance().unwrap();
        lanes.poison();
        assert!(lanes.seek_block(5).is_err());
        assert!(lanes.check_targets(&[5]).is_err());
    }

    /// A target read sees the snapshot's version, not the latest. The window and
    /// the targets change which chunks are resolved, never which version they
    /// come from.
    #[test]
    fn a_write_during_a_target_scan_is_invisible() {
        let dir = tmpdir("targets-iso");
        let _c = Clean(dir.clone());
        let db = ragged(&dir);
        let snap = db.snapshot().unwrap();
        let mut lanes = KeyLanes::in_prefix_range(&snap, &[10, 20], 0, 9).unwrap();

        lanes.seek_block(1).unwrap();
        let before = lanes.lane(0).unwrap().len();

        let mut b = db.batch();
        b.insert(10, 65536 + 4242);
        b.insert(10, 4 * 65536 + 1);
        b.commit().unwrap();
        db.checkpoint().unwrap();

        lanes.seek_block(4).unwrap();
        assert_eq!(
            lanes.present(),
            0,
            "a chunk written after the snapshot is absent"
        );
        assert_eq!(lanes.version(), snap.version());

        let fresh = db.snapshot().unwrap();
        let mut after = KeyLanes::in_prefix_range(&fresh, &[10], 1, 2).unwrap();
        after.seek_block(1).unwrap();
        assert!(
            after.lane(0).unwrap().len() > before,
            "and a newer snapshot does see it"
        );
    }
}
