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
        let mut streams = Vec::with_capacity(keys.len());
        for &key in keys {
            streams.push(snap.key_stream(key)?);
        }
        let mut heads = Vec::with_capacity(streams.len());
        for s in &mut streams {
            heads.push(s.peek_prefix()?);
        }
        Ok(KeyLanes {
            current: vec![None; streams.len()],
            streams,
            heads,
            prefix: None,
            keys: keys.into(),
            snap: snap.clone(),
            poisoned: false,
        })
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
        Ok(Some(next))
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

    fn poison(&mut self) {
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
}
