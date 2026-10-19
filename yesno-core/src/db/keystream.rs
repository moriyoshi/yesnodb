//! A key's posting list as a lazy [`ChunkStream`], without materializing it.
//!
//! Automatic bitmap-JIT admission also reads each step's representation tag
//! from the plan. Disk `ChunkRef` tags and resident container variants reveal
//! it without decoding a payload.
//!
//! # What was missing
//!
//! [`Snapshot::load`](super::Snapshot::load) builds **every** container of a
//! key into an [`OrdSet`] before anything can stream it. A key with `10^9`
//! ordinals is about 15 000 chunks, and a consumer that wanted one chunk at a
//! time — a filter pushed into a scan, an intersection that will skip most of
//! the key, a count — paid all of them up front.
//!
//! **The cost is the per-chunk work, not the payload bytes**, and it is worth
//! being exact about which. `ShardStore::read_container_for` is zero-copy: a
//! store-backed container aliases the mapping rather than copying it, so `load`
//! does not read 125 MB into memory and the pages fault in only as they are
//! touched. What it does pay is a container built per chunk, measured at about
//! three allocations each — so ~15 000 chunks is tens of thousands of
//! allocations before the consumer sees its first chunk, whether or not it ever
//! asks for the second.
//!
//! [`Backing::Paged`](crate::stream::Backing::Paged) has existed since the
//! planner was written and its own doc admitted that **no leaf reported it**,
//! for exactly this reason: every leaf bottomed out in memory because
//! `Snapshot::load` had already been paid. This is that leaf.
//!
//! # What is lazy, and what deliberately is not
//!
//! Construction performs **one index range scan** and merges the memtable over
//! it, producing one [`Step`] per visible chunk. A step is a prefix, a
//! cardinality, and either a memtable container already in hand or a
//! `( ChunkKey, ChunkRef )` pair — sixteen bytes of reference, not a payload.
//! [`Snapshot::key_stream_prefix_range`](super::Snapshot::key_stream_prefix_range)
//! applies its half-open chunk-prefix bounds to both the index cursor and the
//! memtable iterator before this plan is collected. It therefore pays metadata
//! planning and plan storage only for the requested prefix interval, while
//! retaining the same cursor and payload-reading machinery.
//! Disk payloads are decoded in [`ChunkStream::next_chunk`], one at a time,
//! and the store lock is taken and released per chunk rather than held across
//! the walk.
//!
//! The **memtable overlay is collected eagerly**, as it is in every other read
//! path here, and that is a lock-order requirement rather than an oversight:
//! `merged_chunks`'s comment records that the overlay is taken and released
//! before the store lock, and that a merge streaming from both live would be
//! the first thing in the crate to impose an order between them. The overlay is
//! bounded by the flush threshold, and the per-chunk work this exists to avoid
//! is on the disk side.
//!
//! # Counting costs no payload at all
//!
//! Each step carries its cardinality, read from `ChunkRef`'s `card_m1` for a
//! disk chunk and from the container's cached length for a memtable one. So
//! [`ChunkStream::next_cardinality`], [`ChunkStream::cardinality_dyn`] and
//! [`ChunkStream::cardinality_hint`] all answer **without decoding anything**,
//! which is the same property [`Snapshot::cardinality`](super::Snapshot::cardinality)
//! has and the reason `ChunkRef` carries the field.
//!
//! # Errors surface at the stage that encounters them
//!
//! An index error aborts plan construction. Discarding it would turn an empty
//! or truncated plan into a successful answer, and a later payload checksum
//! cannot detect references that never entered the plan. A payload error
//! surfaces later from [`ChunkStream::next_chunk`], because payloads remain
//! lazy. Neither failure is converted into an absent chunk: a short answer
//! presented as a complete one is the worse failure.

use std::sync::Arc;

use super::{DbInner, ReaderSlot, Snapshot};
use crate::container::Container;
use crate::error::Result;
use crate::mvcc::Version;
use crate::store::extent::{ChunkKey, ChunkRef};
use crate::stream::{Backing, BoxedStream, ChunkSource, ChunkStream, StreamStats};
use crate::Prefix48;

/// The legal exclusive endpoint for a half-open chunk-prefix range.
///
/// Unlike an individual [`Prefix48`], the upper endpoint may equal `2^48`.
///
/// Public because the bounded readers built on it -- notably
/// [`crate::KeyLanes::in_prefix_range`], and this crate's own single-key and
/// batched prefix-range planners -- are unusable without the one value their
/// upper endpoint may legally take, and the alternative is every caller, in
/// every crate and every language binding, writing `1 << 48` and being right by
/// coincidence.
pub const PREFIX_EXCLUSIVE_END: u64 = 1u64 << 48;

/// Where one chunk of the plan comes from.
enum Source {
    /// Already in hand: the memtable's container, cloned at construction.
    ///
    /// Containers are frozen, so this is a refcount bump rather than a copy.
    Mem(Container),
    /// On disk: decoded on demand, under the store lock, one chunk at a time.
    Disk(ChunkKey, ChunkRef),
}

/// Every visible chunk of a key, resolved to references and **immutable**.
///
/// Shared behind an `Arc` because a [`KeyStream`] only ever *reads* it -- the
/// cursor is `idx`, not the plan -- so opening a second stream over the same key
/// is a refcount bump rather than a second index range scan. That is what makes
/// [`KeySource::open`] `O( 1 )` instead of `O( chunks )`.
struct Plan {
    steps: Vec<Step>,
    /// Steps needing a disk read. Counted once here; each stream decrements its
    /// own copy as it consumes them.
    disk: usize,
}

/// One visible chunk, resolved down to a reference but not yet decoded.
struct Step {
    prefix: Prefix48,
    /// Known without decoding: `card_m1` on disk, the cached length in memory.
    card: u32,
    src: Source,
}

/// A lazy [`ChunkStream`] over one key of a [`Snapshot`].
///
/// Built by [`Snapshot::key_stream`]. See the module documentation for what is
/// lazy and what is not.
pub struct KeyStream {
    db: Arc<DbInner>,
    version: Version,
    /// Holds the reclamation floor for as long as the stream can still read.
    ///
    /// **This is load-bearing, not hygiene.** The plan holds `ChunkRef`s that
    /// will be followed later, so the extents behind them must not be reclaimed
    /// in the meantime. A snapshot pins them through this slot; a stream that
    /// outlived its `Snapshot` without holding one would follow a reference
    /// into a reused extent.
    slot: Arc<ReaderSlot>,
    /// A lock-free reader for this shard's published chunks, captured at open.
    ///
    /// **This is why `next_chunk` takes no lock.** It used to take
    /// `Mutex<ShardStore>` once per chunk and hold it across the whole of
    /// `read_container_for` -- trailer read, checksum, `buffer_at`, decode -- so
    /// concurrent readers serialized per shard. A consumer measured about 1 us a
    /// chunk under that lock and a floor of 71 ms however many workers ran, with
    /// eight workers slower than one, because the rows of one index are one key
    /// and therefore one shard: no shard count can spread them.
    ///
    /// Sound for the reason [`Self::slot`] above already states. The slot pins
    /// the reclamation floor for as long as this stream can read, so the extents
    /// the plan names cannot be reclaimed or their slabs reclassified while it
    /// lives -- which is exactly what a published view of the class map needs.
    /// A published extent is immutable besides, by invariant I2.
    ///
    /// `None` for a shard with no store, which is the in-memory case and reads
    /// nothing from disk anyway.
    chunks: Option<crate::db::store::PublishedChunks>,
    plan: Arc<Plan>,
    idx: usize,
    /// Steps at or after `idx` that still need a disk read. Kept incrementally
    /// so [`ChunkStream::stats`] stays `O(1)` and still reports honestly.
    disk_remaining: usize,
}

impl KeyStream {
    pub(super) fn new(snap: &Snapshot, key: u64) -> Result<KeyStream> {
        let shard = snap.shard_index(key);
        Ok(KeyStream::over(
            snap,
            shard,
            Arc::new(KeyStream::build_plan(snap, key)?),
        ))
    }

    /// One stream per key over `[lo, hi)`, all planned in one pass per shard.
    ///
    /// `0 .. PREFIX_EXCLUSIVE_END` is the whole key, so this is the only batched
    /// constructor rather than a bounded variant of one -- there was an unbounded
    /// `batch` beside it and it was a one-line forward to this, which is a second
    /// name for the same thing and a second place to look.
    ///
    /// See [`KeyStream::build_plans_range`] for what the sharing saves, what it
    /// deliberately leaves alone, and why narrowing costs it nothing.
    pub(super) fn batch_in_prefix_range(
        snap: &Snapshot,
        keys: &[u64],
        lo: Prefix48,
        hi: Prefix48,
    ) -> Result<Vec<KeyStream>> {
        let plans = Self::build_plans_range(snap, keys, lo, hi)?;
        Ok(keys
            .iter()
            .zip(plans)
            .map(|(&key, plan)| KeyStream::over(snap, snap.shard_index(key), plan))
            .collect())
    }

    /// Construct a stream whose plan is restricted to `[lo, hi)` prefixes.
    pub(super) fn in_prefix_range(
        snap: &Snapshot,
        key: u64,
        lo: Prefix48,
        hi: Prefix48,
    ) -> Result<KeyStream> {
        let shard = snap.shard_index(key);
        Ok(KeyStream::over(
            snap,
            shard,
            Arc::new(KeyStream::build_plan_range(snap, key, lo, hi)?),
        ))
    }

    /// A cursor over an already-resolved plan. No scan, no locks.
    fn over(snap: &Snapshot, shard: usize, plan: Arc<Plan>) -> KeyStream {
        // One acquisition here, in exchange for none per chunk. The haiiie
        // workload reads 71 588 chunks a query, so this is the whole trade.
        let chunks = snap.db.shards[shard]
            .store
            .as_ref()
            .map(|s| s.lock().unwrap().published_chunks());
        KeyStream {
            db: snap.db.clone(),
            version: snap.version,
            slot: snap._slot.clone(),
            chunks,
            disk_remaining: plan.disk,
            plan,
            idx: 0,
        }
    }

    /// One index range scan, merged with the memtable. The expensive half.
    fn build_plan(snap: &Snapshot, key: u64) -> Result<Plan> {
        Self::build_plan_range(snap, key, 0, PREFIX_EXCLUSIVE_END)
    }

    /// Reject a prefix interval no planner can honour.
    ///
    /// Shared by the single-key and batched planners because the check has to
    /// happen **before** the interval is packed into a [`ChunkKey`]: a planner
    /// that packed first would mask the one legal exclusive endpoint to zero
    /// rather than reject an illegal one. Two planners each remembering that is
    /// two chances to forget it.
    pub(super) fn check_prefix_range(lo: Prefix48, hi: Prefix48) -> Result<()> {
        match lo > hi || hi > PREFIX_EXCLUSIVE_END {
            true => Err(crate::error::CodecError::Invariant(
                "invalid chunk prefix range",
            )),
            false => Ok(()),
        }
    }

    /// The `ChunkKey` endpoints one key's `[lo, hi)` prefixes occupy.
    ///
    /// `PREFIX_EXCLUSIVE_END` becomes the key's own range end rather than a
    /// packed prefix, which is exactly the case [`Self::check_prefix_range`]
    /// has to run before: `ChunkKey::new` would mask it to zero and plan
    /// nothing at all.
    ///
    /// **`lo` is packed unconditionally, and that is sound rather than an
    /// oversight.** `check_prefix_range` admits `lo <= hi <= PREFIX_EXCLUSIVE_END`,
    /// so `lo` can only reach the exclusive end when `hi` is there too -- and both
    /// callers return on `lo == hi` *before* reaching this function. A `lo` that
    /// arrives here is therefore strictly below the end and packs exactly. The
    /// early return is load-bearing for this reason as well as for the empty plan
    /// it produces. Moving it after this call would turn `END..END` into a scan of
    /// the whole key, which is caught -- `an_empty_window_still_pins_the_snapshot`
    /// asserts that window yields no block -- so the argument is pinned by a test
    /// and not only by this paragraph.
    fn key_bounds(key: u64, lo: Prefix48, hi: Prefix48) -> (ChunkKey, ChunkKey) {
        (
            ChunkKey::new(key, lo),
            match hi == PREFIX_EXCLUSIVE_END {
                true => ChunkKey::range_end(key),
                false => ChunkKey::new(key, hi),
            },
        )
    }

    /// Build only the plan entries in the half-open prefix interval.
    fn build_plan_range(snap: &Snapshot, key: u64, lo: Prefix48, hi: Prefix48) -> Result<Plan> {
        snap.check_live()?;
        Self::check_prefix_range(lo, hi)?;
        if lo == hi {
            return Ok(Plan {
                steps: Vec::new(),
                disk: 0,
            });
        }
        let (start, end) = Self::key_bounds(key, lo, hi);
        let i = snap.shard_index(key);
        let shard = &snap.db.shards[i];

        // Taken and released before the store lock; see the module docs and the
        // matching comment in `Snapshot::merged_chunks`.
        let overlay: Vec<(Prefix48, Option<Container>)> = {
            let mem = shard.mem.read().unwrap();
            mem.chunks_in_key_range(start, end, snap.version)
                .map(|(p, c)| (p, c.cloned()))
                .collect()
        };

        match snap.roots[i].zip(shard.store.as_ref()) {
            None => Self::merge_plan::<std::iter::Empty<_>>(overlay, None),
            Some((tree, store)) => {
                // **The lock is held only long enough to build the reader.** It
                // used to span the whole `tree.range` walk, so one key's open
                // serialised every other open on that shard -- and key-only
                // sharding means a parallel query's workers all want the same
                // shards at once. Measured at 8 threads: sharing shards cost 1.80x
                // the same work on disjoint shards, 44% of it above the
                // no-sharing floor.
                //
                // Sound because `tree` is a **snapshot** root, hence a superblock
                // root, and `pending_nodes` holds only pages not yet reachable
                // from any superblock. See `PublishedNodes`.
                let reader = store.lock().unwrap().published_nodes();
                Self::merge_plan(overlay, Some(tree.range(&reader, start, end)))
            }
        }
    }

    /// Plans for several keys in `[lo, hi)`, sharing one node reader per shard.
    ///
    /// # What this saves, and what it deliberately does not
    ///
    /// Per-key planning takes the memtable read lock, takes the store lock to
    /// build a [`PublishedNodes`](crate::store::PublishedNodes) reader, and then
    /// descends the index from the root. This hoists the first two out of the
    /// loop: **one** memtable acquisition and **one** reader for every key on a
    /// shard, with the descents unchanged. That is strictly less work for any
    /// key distribution, which is why it needs no density heuristic.
    ///
    /// **Do not mistake this for the fix to
    /// `numeric-fold-cost-is-the-plan-not-the-arithmetic`.** That entry measured
    /// planning at ~545 ns of a 953 ns element, and what this removes is the
    /// per-key *setup*: one uncontended mutex, one rwlock read and two `Arc`
    /// clones, since `published_nodes` allocates nothing. Tens of nanoseconds,
    /// not hundreds. The **descents** are the bulk and this leaves every one of
    /// them in place.
    ///
    /// Cutting the descents is a separate, measured decision. Collapsing them
    /// into one scan over `[first key, last key]` reads every chunk of every key
    /// *between* the requested ones, so it wins on a batch dense in the index
    /// and loses on a sparse one -- which needs a density threshold, hence a
    /// measurement. The threshold-free alternative is to resume within the leaf
    /// a cursor already holds and re-descend only when the next key is outside
    /// it, and that needs a forward seek `Cursor` does not have: `seek_to`
    /// clears the stack and descends from the root, so re-seeking costs what a
    /// new descent costs.
    ///
    /// # The lock discipline is the one already established here
    ///
    /// The store lock is held only long enough to build the reader, exactly as
    /// in [`Self::build_plan_range`], and for the same measured reason: it once
    /// spanned the `tree.range` walk and cost 1.80x at 8 threads sharing a
    /// shard. Batching must not re-widen it, so the reader is built once under
    /// a lock held no longer than before and every descent runs outside it.
    ///
    /// # The window is a parameter rather than a second planner
    ///
    /// **Everything above is the reason.** A bounded acquisition that planned one
    /// key at a time would give up the one memtable acquisition and the one node
    /// reader per shard, and on a wide batch that setup is the whole saving
    /// batching bought. Narrowing changes which `ChunkKey` endpoints the cursor
    /// is aimed at and nothing else -- a narrowed interval is still inside its
    /// key's, and the keys are still ascending -- so the resume property the
    /// sharing rests on survives it and the window costs nothing.
    ///
    /// `0 .. PREFIX_EXCLUSIVE_END` is the whole key, which is how the unbounded
    /// callers spell themselves; there is no second entry point for them.
    ///
    /// The interval is validated once, before any endpoint is packed, for the
    /// reason [`Self::check_prefix_range`] gives. `lo == hi` is legal and plans
    /// nothing, which is what lets a caller ask for an empty window without
    /// special-casing it -- and the handle it produces still pins a snapshot.
    fn build_plans_range(
        snap: &Snapshot,
        keys: &[u64],
        lo: Prefix48,
        hi: Prefix48,
    ) -> Result<Vec<Arc<Plan>>> {
        snap.check_live()?;
        Self::check_prefix_range(lo, hi)?;
        if lo == hi {
            // One shared empty plan: every key's answer is the same and a plan
            // is immutable, so `keys.len()` copies of it would be waste.
            let empty = Arc::new(Plan {
                steps: Vec::new(),
                disk: 0,
            });
            return Ok(keys.iter().map(|_| Arc::clone(&empty)).collect());
        }
        let mut out: Vec<Option<Arc<Plan>>> = vec![None; keys.len()];
        let mut by_shard: std::collections::BTreeMap<usize, Vec<usize>> =
            std::collections::BTreeMap::new();
        for (at, &key) in keys.iter().enumerate() {
            by_shard.entry(snap.shard_index(key)).or_default().push(at);
        }

        for (i, positions) in by_shard {
            let shard = &snap.db.shards[i];

            // Distinct keys in **ascending** order, each with the output
            // positions it feeds. Ascending because one cursor resumes forward
            // and never rewinds; distinct because a repeated key must be
            // planned once and shared, not scanned twice from a cursor that has
            // already passed it.
            let mut distinct: std::collections::BTreeMap<u64, Vec<usize>> =
                std::collections::BTreeMap::new();
            for &at in &positions {
                distinct.entry(keys[at]).or_default().push(at);
            }
            let order: Vec<u64> = distinct.keys().copied().collect();

            // One acquisition for every key on this shard. The memtable range
            // is a `BTreeMap` walk with no I/O under it, so collecting several
            // keys' overlays does not widen the hold in the way the store lock
            // would.
            let overlays: Vec<Vec<(Prefix48, Option<Container>)>> = {
                let mem = shard.mem.read().unwrap();
                order
                    .iter()
                    .map(|&key| {
                        let (start, end) = Self::key_bounds(key, lo, hi);
                        mem.chunks_in_key_range(start, end, snap.version)
                            .map(|(p, c)| (p, c.cloned()))
                            .collect()
                    })
                    .collect()
            };

            match snap.roots[i].zip(shard.store.as_ref()) {
                None => {
                    for (overlay, &key) in overlays.into_iter().zip(order.iter()) {
                        let plan =
                            Arc::new(Self::merge_plan::<std::iter::Empty<_>>(overlay, None)?);
                        for &at in &distinct[&key] {
                            out[at] = Some(Arc::clone(&plan));
                        }
                    }
                }
                Some((tree, store)) => {
                    let reader = store.lock().unwrap().published_nodes();

                    // **One cursor for the shard.** Each key's interval lies
                    // inside `[key, key+1)` and ascending keys are adjacent in
                    // `ChunkKey` order, so `reaim` usually resumes inside the
                    // leaf already in hand and descends only when the next key
                    // is outside it. A cursor per key descends every time.
                    //
                    // Narrowing only moves these endpoints inward, which leaves
                    // the ascending-resume property the sharing rests on
                    // intact: a narrowed interval is still inside its key's,
                    // and the keys are still in order.
                    let first = Self::key_bounds(order[0], lo, hi);
                    let mut cursor = tree.range(&reader, first.0, first.1);
                    for (n, (overlay, &key)) in overlays.into_iter().zip(order.iter()).enumerate() {
                        if n > 0 {
                            let (start, end) = Self::key_bounds(key, lo, hi);
                            cursor.reaim(start, end)?;
                        }
                        let plan = Arc::new(Self::merge_plan(overlay, Some(cursor.by_ref()))?);
                        for &at in &distinct[&key] {
                            out[at] = Some(Arc::clone(&plan));
                        }
                    }
                }
            }
        }

        Ok(out
            .into_iter()
            .map(|p| p.expect("every position belongs to exactly one shard"))
            .collect())
    }

    /// Merge one key's memtable overlay with its index scan into a [`Plan`].
    ///
    /// Extracted so that [`Self::build_plan_range`] and [`Self::build_plans_range`]
    /// run the **same** merge rather than two copies of it. A batched planner
    /// that reimplemented this could diverge from the single-key one on a
    /// tombstone or an empty container and no test of either alone would see it;
    /// `batched_plans_equal_the_per_key_plans` compares them, and it compares
    /// them through this.
    fn merge_plan<I>(overlay: Vec<(Prefix48, Option<Container>)>, scan: Option<I>) -> Result<Plan>
    where
        I: Iterator<Item = Result<(ChunkKey, ChunkRef)>>,
    {
        let mut plan: Vec<Step> = Vec::new();

        // An empty container carries no ordinals, and the stream contract
        // forbids yielding a zero count. `OrdSet::from_chunks` asserts the same
        // thing, so dropping them here keeps this identical to `load`.
        let push_mem = |plan: &mut Vec<Step>, p: Prefix48, c: Option<Container>| {
            if let Some(c) = c.filter(|c| !c.is_empty()) {
                plan.push(Step {
                    prefix: p,
                    card: c.len(),
                    src: Source::Mem(c),
                });
            }
        };

        match scan {
            None => {
                for (p, c) in overlay {
                    push_mem(&mut plan, p, c);
                }
            }
            Some(scan) => {
                let mut overlay = overlay.into_iter().peekable();
                for item in scan {
                    let (ck, cref) = item?;
                    let prefix = ck.prefix();

                    // Memtable chunks below this prefix are uncontested.
                    while overlay.peek().is_some_and(|&(p, _)| p < prefix) {
                        let (p, c) = overlay.next().unwrap();
                        push_mem(&mut plan, p, c);
                    }

                    // The memtable wins wherever it has an opinion, including
                    // tombstones — and when it does, the payload is never read.
                    if overlay.peek().is_some_and(|&(p, _)| p == prefix) {
                        let (p, c) = overlay.next().unwrap();
                        push_mem(&mut plan, p, c);
                        continue;
                    }

                    plan.push(Step {
                        prefix,
                        card: cref.cardinality(),
                        src: Source::Disk(ck, cref),
                    });
                }

                for (p, c) in overlay {
                    push_mem(&mut plan, p, c);
                }
            }
        }

        let disk = plan
            .iter()
            .filter(|s| matches!(s.src, Source::Disk(..)))
            .count();

        Ok(Plan { steps: plan, disk })
    }

    /// The version this stream reads at. Diagnostics.
    #[inline]
    pub fn version(&self) -> Version {
        self.version
    }

    /// Chunks not yet consumed. Exact, and known without reading anything.
    ///
    /// **The `min` is unreachable, and saying so is the honest version.** A
    /// first draft of this comment claimed it guarded a live underflow --
    /// that `seek` "can land past the end" -- and a sabotage refuted it:
    /// removing the `min` leaves every test green. `idx <= steps.len()` is an
    /// invariant, maintained at all three mutation sites: it starts at `0`,
    /// `advance` increments it only under a `idx < len` guard, and
    /// `cardinality_dyn` assigns exactly `steps.len()`. `seek` cannot exceed it
    /// either, since its `found` is bounded by `hi`, itself a `min` against the
    /// length.
    ///
    /// It stays because the invariant lives in three places and nothing checks
    /// it, so the cost is one comparison against a silent wraparound if a fourth
    /// mutator ever appears. What it is *not* is a guard any test can exercise,
    /// and a comment implying otherwise invites someone to go looking for the
    /// case that makes it fire.
    ///
    /// [`stats`](ChunkStream::stats) is its production caller and open-coded
    /// this same subtraction until 2026-09-14, which is why a sweep found this
    /// method with no caller anywhere one day after it was written. The two
    /// agreeing is now structural rather than a thing to keep true by hand.
    #[inline]
    pub fn chunks_remaining(&self) -> usize {
        self.plan.steps.len() - self.idx.min(self.plan.steps.len())
    }

    /// The same liveness gate every `Snapshot` read passes.
    ///
    /// **Checked per chunk, not once at construction.** `load` reads everything
    /// under a single check; a stream reads over an unbounded window, and a
    /// snapshot can be evicted inside it. After eviction the floor no longer
    /// holds this reader's extents, so a `ChunkRef` captured at construction may
    /// point at reused space — [`super::readers`] explains why that surfaces as
    /// a decode error rather than as another key's data, and why an error on a
    /// read that should have succeeded is still a wrong answer.
    #[inline]
    fn check_live(&self) -> Result<()> {
        if self.db.reader_evicted[self.slot.slot].load(std::sync::atomic::Ordering::Acquire) {
            return Err(crate::error::CodecError::SnapshotTooOld {
                version: self.version,
                last_key: None,
            });
        }
        Ok(())
    }

    /// Consume the step at `idx`, keeping `disk_remaining` in step with it.
    #[inline]
    fn advance(&mut self) -> &Step {
        let s = &self.plan.steps[self.idx];
        if matches!(s.src, Source::Disk(..)) {
            self.disk_remaining -= 1;
        }
        self.idx += 1;
        s
    }
}

impl ChunkStream for KeyStream {
    fn next_chunk(&mut self) -> Result<Option<(Prefix48, Container)>> {
        if self.idx >= self.plan.steps.len() {
            return Ok(None);
        }
        self.check_live()?;
        // Borrow ends before the store lock is taken.
        let (prefix, want) = {
            let s = self.advance();
            match &s.src {
                Source::Mem(c) => return Ok(Some((s.prefix, c.clone()))),
                Source::Disk(ck, cref) => (s.prefix, (*ck, *cref)),
            }
        };
        let Some(chunks) = self.chunks.as_ref() else {
            return Ok(None);
        };
        // No lock. Checked: a stale or corrupt reference must not decode as this key.
        match chunks.read_container_for(want.0, want.1)? {
            Some(c) => Ok(Some((prefix, c))),
            // The index named a chunk the store no longer holds. `load` drops
            // this silently; see the module docs for why this does not.
            None => Err(crate::error::CodecError::Invariant(
                "index entry names a chunk the store did not return",
            )),
        }
    }

    /// No payload is read at all: the count came out of the index.
    fn next_cardinality(&mut self) -> Result<Option<(Prefix48, u64)>> {
        if self.idx >= self.plan.steps.len() {
            return Ok(None);
        }
        self.check_live()?;
        let s = self.advance();
        Ok(Some((s.prefix, s.card as u64)))
    }

    fn seek(&mut self, prefix: Prefix48) -> Result<()> {
        // Gallop from the current position, as `SetStream` does: a merge-join
        // skews forward, so this is O(log delta) rather than O(log n).
        let start = self.idx;
        let mut step = 1usize;
        while start + step < self.plan.steps.len() && self.plan.steps[start + step].prefix < prefix
        {
            step *= 2;
        }
        let lo = start + step / 2;
        let hi = (start + step).min(self.plan.steps.len());
        let found = lo + self.plan.steps[lo..hi].partition_point(|s| s.prefix < prefix);
        for _ in self.idx..found {
            self.advance();
        }
        Ok(())
    }

    fn peek_prefix(&mut self) -> Result<Option<Prefix48>> {
        Ok(self.plan.steps.get(self.idx).map(|s| s.prefix))
    }

    /// Exact on both ends: every remaining chunk's count is already known.
    fn cardinality_hint(&self) -> (u64, Option<u64>) {
        let n: u64 = self.plan.steps[self.idx.min(self.plan.steps.len())..]
            .iter()
            .map(|s| s.card as u64)
            .sum();
        (n, Some(n))
    }

    /// Reports [`Backing::Paged`] while any remaining chunk still needs a read.
    ///
    /// This is the first leaf in the crate that ever reports it. A plan that
    /// counted chunks without asking what backed them would prefer exactly the
    /// wrong operand — which is what that variant was written for.
    fn stats(&self) -> StreamStats {
        let n = self.plan.steps.len();
        if self.idx >= n {
            return StreamStats::new(0, None, Backing::Memory);
        }
        let backing = if self.disk_remaining > 0 {
            Backing::Paged
        } else {
            Backing::Memory
        };
        let span = Some((
            self.plan.steps[self.idx].prefix,
            self.plan.steps[n - 1].prefix,
        ));
        StreamStats::new(self.chunks_remaining() as u64, span, backing)
    }

    /// Sums what the index already recorded. Decodes nothing.
    fn cardinality_dyn(&mut self) -> Result<u64> {
        self.check_live()?;
        let n = self.plan.steps[self.idx.min(self.plan.steps.len())..]
            .iter()
            .map(|s| s.card as u64)
            .sum();
        self.idx = self.plan.steps.len();
        self.disk_remaining = 0;
        Ok(n)
    }
}

/// One key of a [`Snapshot`], as a re-openable [`ChunkSource`].
///
/// This is what puts a lazy leaf in an expression. [`Snapshot::key_expr`] wraps
/// one in an [`Expr`](crate::Expr), so a query over a key no longer has to
/// materialize the key first.
///
/// # It answers the planner's three questions without decoding anything
///
/// `chunk_count`, `prefix_span` and `cardinality` are exactly what
/// `stream::plan` used to read off a materialized `OrdSet`. All three come from
/// the index: the range scan that builds a [`KeyStream`] counts the chunks,
/// knows their prefixes, and reads each cardinality out of `card_m1`. So a lazy
/// leaf plans as well as a resident one — but **each call opens a plan**, which
/// is one index range scan and a memtable collect. That is cheap next to
/// decoding the key and is not free, which is why they are cached here on first
/// use rather than recomputed per query.
///
/// # Holding a `Snapshot`, not a `Db`
///
/// The source pins the version it was built at, so an expression built now and
/// evaluated later reads what it would have read at build time — and the reader
/// slot it holds keeps those extents unreclaimable meanwhile. That is the same
/// guarantee [`Snapshot`] itself gives, and it is why this is `Clone`.
pub struct KeySource {
    snap: Snapshot,
    key: u64,
    shard: usize,
    /// The resolved plan, built at most once.
    ///
    /// **This is what makes reopening cheap.** A [`Plan`] is immutable and a
    /// [`KeyStream`] only reads it, so every open after the first is a refcount
    /// bump instead of another index range scan. The `Err` is kept rather than
    /// discarded so [`ChunkSource::open`] can still report *why* it failed.
    ///
    /// Caching is sound because the plan is a pure function of
    /// `( snapshot version, key )`: MVCC fixes what the memtable shows at this
    /// version, so building it twice cannot differ. The cost is memory — the
    /// plan holds a reference per chunk, and an `Arc` to each memtable-resident
    /// container, for as long as the source lives.
    plan: std::sync::OnceLock<std::result::Result<Arc<Plan>, crate::error::CodecError>>,
    /// Derived from the plan, cached so repeated planning does not re-sum it.
    stats: std::sync::OnceLock<Option<SourceStats>>,
}

/// Planner statistics and the no-payload bitmap-JIT admission hint.
///
/// The three planner fields are named rather than a tuple because they are
/// integers or spans; a positional `( u64, _, u64 )` invites a mix-up that makes
/// the planner confidently wrong.
#[derive(Clone, Copy, Debug)]
struct SourceStats {
    chunks: u64,
    span: Option<(Prefix48, Prefix48)>,
    cardinality: u64,
    all_bitmap: bool,
}

impl KeySource {
    pub(super) fn new(snap: Snapshot, key: u64) -> KeySource {
        let shard = snap.shard_index(key);
        KeySource {
            snap,
            key,
            shard,
            plan: std::sync::OnceLock::new(),
            stats: std::sync::OnceLock::new(),
        }
    }

    /// The plan, scanned on first demand and shared thereafter.
    fn plan(&self) -> Option<&Arc<Plan>> {
        self.plan
            .get_or_init(|| KeyStream::build_plan(&self.snap, self.key).map(Arc::new))
            .as_ref()
            .ok()
    }

    /// The key this reads.
    #[inline]
    pub fn key(&self) -> u64 {
        self.key
    }

    /// Statistics off one plan, computed at most once.
    ///
    /// `None` when the plan cannot be built at all — an evicted snapshot. The
    /// planner then treats the leaf as opaque, and the error surfaces properly
    /// at `open`, which is where a caller is equipped to handle it.
    fn stats(&self) -> &Option<SourceStats> {
        self.stats.get_or_init(|| {
            let plan = self.plan()?;
            let n = plan.steps.len();
            Some(SourceStats {
                chunks: n as u64,
                span: (n > 0).then(|| (plan.steps[0].prefix, plan.steps[n - 1].prefix)),
                cardinality: plan.steps.iter().map(|st| st.card as u64).sum(),
                all_bitmap: plan.steps.iter().all(|step| match &step.src {
                    Source::Mem(container) => matches!(container, Container::Bitmap(_)),
                    Source::Disk(_, chunk_ref) => chunk_ref.kind() == crate::ContainerKind::Bitmap,
                }),
            })
        })
    }
}

impl Clone for KeySource {
    /// The cached plan and statistics are **not** carried over.
    ///
    /// Both are caches of a pure function of `( snapshot version, key )`, so
    /// copying them would be sound — but `OnceLock` is not `Clone`, and a clone
    /// is made to be handed elsewhere. The cost is one index scan on the clone's
    /// first demand. Cloning a source is rare; reopening one is not, and that is
    /// the case the sharing is built for.
    fn clone(&self) -> Self {
        KeySource::new(self.snap.clone(), self.key)
    }
}

impl std::fmt::Debug for KeySource {
    /// Compact on purpose.
    ///
    /// `Debug` on an `Expr` walks every leaf, and `plan.rs` records what that
    /// cost when a set leaf rendered its whole contents: `plan()` took 192 ms
    /// for a query that executes in 37 us. A source must never reintroduce that,
    /// so this prints the key and version and never touches a chunk.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeySource")
            .field("key", &self.key)
            .field("version", &self.snap.version)
            .finish()
    }
}

impl ChunkSource for KeySource {
    /// `O( 1 )` after the first call: a refcount bump on the shared plan and a
    /// fresh cursor. The index range scan happens once per source, not once per
    /// open.
    fn open(&self) -> BoxedStream {
        match self
            .plan
            .get_or_init(|| KeyStream::build_plan(&self.snap, self.key).map(Arc::new))
        {
            Ok(p) => Box::new(KeyStream::over(&self.snap, self.shard, p.clone())),
            // Never an `EmptyStream`: that would turn an evicted snapshot into
            // a silently empty answer. See `ErrStream`.
            Err(e) => Box::new(crate::stream::ErrStream::new(e.clone())),
        }
    }

    fn chunk_count(&self) -> Option<u64> {
        self.stats().map(|s| s.chunks)
    }

    fn prefix_span(&self) -> Option<(Prefix48, Prefix48)> {
        self.stats().and_then(|s| s.span)
    }

    fn all_bitmap_chunks(&self) -> Option<bool> {
        self.stats().map(|s| s.all_bitmap)
    }

    fn cardinality(&self) -> Option<u64> {
        self.stats().map(|s| s.cardinality)
    }

    /// Exact at the bucketing's resolution: the plan holds every visible
    /// prefix, so this enumerates rather than approximates. One pass over the
    /// plan, no payload touched, and the result is 256 bits regardless of how
    /// many chunks went into it.
    fn occupancy(
        &self,
        b: crate::stream::sketch::Bucketing,
    ) -> Option<crate::stream::sketch::PrefixOccupancy> {
        let plan = self.plan()?;
        Some(crate::stream::sketch::PrefixOccupancy::from_prefixes(
            b,
            plan.steps.iter().map(|s| s.prefix),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::ChunkStreamExt;
    use crate::{Db, DbOptions, OrdSet};

    fn tmpdir(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("yesno-keystream-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    struct CleanDir(std::path::PathBuf);
    impl Drop for CleanDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn opts() -> DbOptions {
        DbOptions {
            shards: 1,
            ..Default::default()
        }
    }

    fn drain(mut s: KeyStream) -> OrdSet {
        let mut out = Vec::new();
        while let Some((p, c)) = s.next_chunk().unwrap() {
            out.push((p, c));
        }
        OrdSet::from_chunks(out)
    }

    /// The shape of a plan, for comparing two planners without `PartialEq`.
    fn shape(p: &Plan) -> (usize, Vec<(Prefix48, u32, bool)>) {
        (
            p.disk,
            p.steps
                .iter()
                .map(|st| (st.prefix, st.card, matches!(st.src, Source::Disk(..))))
                .collect(),
        )
    }

    /// Assert the batched planner agrees with the per-key one for every key.
    fn plans_agree(snap: &Snapshot, keys: &[u64]) {
        let batched = KeyStream::build_plans_range(snap, keys, 0, PREFIX_EXCLUSIVE_END).unwrap();
        assert_eq!(batched.len(), keys.len());
        for (at, &key) in keys.iter().enumerate() {
            let alone = KeyStream::build_plan(snap, key).unwrap();
            assert_eq!(
                shape(&batched[at]),
                shape(&alone),
                "key {key} at position {at} planned differently in a batch"
            );
        }
    }

    /// The batched planner must produce exactly the per-key planner's plans.
    ///
    /// They share [`KeyStream::merge_plan`], so agreement on the merge is
    /// structural. What this pins is everything around it: order preservation
    /// across shard grouping, a repeated key planned once and shared rather
    /// than re-scanned from a cursor that has already passed it, a key absent
    /// from the index planning empty, and one cursor resuming with `reaim`
    /// seeing exactly what a fresh cursor per key sees.
    ///
    /// # The fixture is shaped by three sabotages that passed against earlier ones
    ///
    /// First version: every key had the same chunk count and cardinality, so a
    /// chunk attributed to the wrong key was **indistinguishable** from the
    /// right one. Second: 72 index entries fitted one leaf, so `reaim` never
    /// took its descend arm. Third, and the one that actually matters: every
    /// written key was also a requested key, so after one key's scan the cursor
    /// already sat on the next requested key's first entry -- and a `reaim` that
    /// skipped **no** entries below `lo` was still correct. Catching that needs
    /// a key that is **in the index and not in the batch**, between two keys
    /// that are, on the same shard. Shard routing goes through a vshard map, so
    /// the only way to place one there is a single-shard database, which is what
    /// the first half below is for. The second half keeps the multi-shard
    /// grouping honest.
    #[test]
    fn batched_plans_equal_the_per_key_plans() {
        let dir = tmpdir("batched-plans");
        let _guard = CleanDir(dir.clone());

        // Distinct per key, so a misattributed chunk changes the shape, and
        // ~250 entries so the tree is several 1 KiB leaves deep.
        let chunks_for = |key: u64| 8 + (key % 5) * 4;
        let card_for = |key: u64, chunk: u64| 1 + ((key + chunk) % 4);
        let fill = |db: &Db, keys: &[u64]| {
            for &key in keys {
                let mut vals = Vec::new();
                for chunk in 0..chunks_for(key) {
                    for j in 0..card_for(key, chunk) {
                        vals.push((chunk << 16) | j);
                    }
                }
                db.insert_many(key, &vals).unwrap();
            }
        };

        // --- One shard: index order is exact, so gaps can be placed. ---
        let one = DbOptions {
            shards: 1,
            ..Default::default()
        };
        let written: Vec<u64> = (1..=16u64).chain([40, 41, 100, 101, 1000]).collect();
        {
            let db = Db::open_with(&dir, one.clone()).unwrap();
            fill(&db, &written);
            db.checkpoint().unwrap();
        }
        let db = Db::open_with(&dir, one).unwrap();

        // All three chunk states on top of the tree.
        db.insert_many(99, &[(3u64 << 16) | 5]).unwrap();
        db.insert_many(1, &[12_345]).unwrap();
        db.remove(2, 2).unwrap();

        let snap = db.snapshot().unwrap();

        // Every odd key is requested and every even one is not, so between any
        // two requested keys the index holds one that is not in the batch.
        // Unsorted, with a repeat, a memtable-only key and a key never written.
        let keys = [
            1000u64, 1, 3, 99, 5, 123_456, 7, 9, 11, 13, 15, 41, 101, 1, 3,
        ];
        plans_agree(&snap, &keys);

        // The fixture must distinguish keys at all, or every assertion above is
        // satisfied by any permutation of the plans.
        let distinct: std::collections::BTreeSet<_> = written
            .iter()
            .map(|&k| shape(&KeyStream::build_plan(&snap, k).unwrap()))
            .collect();
        assert!(
            distinct.len() > 1,
            "every key has the same plan shape, so misattribution would be invisible"
        );

        // And the plans drive identical streams, which is what a caller depends on.
        for &key in &keys {
            let want = snap.load(key).unwrap();
            let got = KeyStream::batch_in_prefix_range(&snap, &[key], 0, PREFIX_EXCLUSIVE_END)
                .unwrap()
                .pop()
                .unwrap();
            assert_eq!(drain(got), want, "key {key}");
        }
        drop(snap);
        drop(db);

        // --- Four shards: grouping must not reorder the output. ---
        let dir2 = tmpdir("batched-plans-shards");
        let _guard2 = CleanDir(dir2.clone());
        let many = DbOptions {
            shards: 4,
            ..Default::default()
        };
        {
            let db = Db::open_with(&dir2, many.clone()).unwrap();
            fill(&db, &written);
            db.checkpoint().unwrap();
        }
        let db = Db::open_with(&dir2, many).unwrap();
        let snap = db.snapshot().unwrap();
        plans_agree(&snap, &keys);
    }

    /// The equivalence that makes this a *parallel implementation* of `load`
    /// rather than a second answer.
    ///
    /// Covered in all three states a chunk can be in, because the merge is
    /// where a lazy reimplementation would drift: **only on disk**, **only in
    /// the memtable**, and **on disk with the memtable holding an opinion** —
    /// including a tombstone, which must suppress the on-disk chunk rather than
    /// fall through to it.
    #[test]
    fn a_key_stream_yields_exactly_what_load_materializes() {
        let dir = tmpdir("equivalence");
        let _guard = CleanDir(dir.clone());

        {
            let db = Db::open_with(&dir, opts()).unwrap();
            // Spread over chunks so the index scan has several entries.
            let vals: Vec<u64> = (0..40u64).map(|c| (c << 16) | 11).collect();
            db.insert_many(1, &vals).unwrap();
            db.checkpoint().unwrap();
        }

        let db = Db::open_with(&dir, opts()).unwrap();

        // 1. Purely on disk.
        let snap = db.snapshot().unwrap();
        assert_eq!(drain(snap.key_stream(1).unwrap()), snap.load(1).unwrap());
        assert_eq!(snap.load(1).unwrap().len(), 40);
        drop(snap);

        // 2. Memtable chunks interleaved with disk ones, an overlay *on* a disk
        //    chunk, and a tombstone removing one that is on disk.
        db.insert(1, (5 << 16) | 12).unwrap(); // same chunk as a disk entry
        db.insert(1, (100 << 16) | 3).unwrap(); // a chunk with nothing on disk
        db.remove(1, (7 << 16) | 11).unwrap(); // empties a disk chunk
        let snap = db.snapshot().unwrap();
        let eager = snap.load(1).unwrap();
        assert_eq!(drain(snap.key_stream(1).unwrap()), eager);
        assert_eq!(eager.len(), 40 + 1 + 1 - 1);
        assert!(!eager.contains((7 << 16) | 11), "the tombstone must win");
        drop(snap);

        // 3. A key that exists only in the memtable, and one that exists at all.
        db.insert(2, 9).unwrap();
        let snap = db.snapshot().unwrap();
        assert_eq!(drain(snap.key_stream(2).unwrap()), snap.load(2).unwrap());
        assert_eq!(
            drain(snap.key_stream(999).unwrap()),
            snap.load(999).unwrap()
        );
        assert!(snap.load(999).unwrap().is_empty());
    }

    /// `Backing::Paged` was written when the planner was, and its own doc said
    /// no leaf reported it. This is the leaf that does.
    #[test]
    fn a_key_stream_over_disk_chunks_reports_paged() {
        let dir = tmpdir("paged");
        let _guard = CleanDir(dir.clone());

        {
            let db = Db::open_with(&dir, opts()).unwrap();
            let vals: Vec<u64> = (0..16u64).map(|c| (c << 16) | 1).collect();
            db.insert_many(1, &vals).unwrap();
            db.checkpoint().unwrap();
        }

        let db = Db::open_with(&dir, opts()).unwrap();
        let snap = db.snapshot().unwrap();
        let mut s = snap.key_stream(1).unwrap();

        let st = s.stats();
        assert_eq!(st.backing, Backing::Paged, "disk chunks must report paged");
        assert_eq!(st.chunks, Some(16));
        assert!(st.drain_cost() > 16, "paged chunks must cost more than one");

        // A memtable-only key is backed by memory, not by pages, and the two
        // must not report the same thing — otherwise the variant says nothing.
        db.insert(2, 4).unwrap();
        let snap2 = db.snapshot().unwrap();
        assert_eq!(
            snap2.key_stream(2).unwrap().stats().backing,
            Backing::Memory
        );

        // Draining retires the disk steps, and an exhausted stream is neither.
        while s.next_chunk().unwrap().is_some() {}
        assert_eq!(s.stats().backing, Backing::Memory);
        assert_eq!(s.stats().chunks, Some(0));
    }

    /// Counting must not decode, and must agree with `load`'s count.
    ///
    /// The allocation guard is
    /// `counting_a_key_stream_reads_no_payload` in `tests/allocation.rs`; this
    /// is only the value half.
    #[test]
    fn counting_a_key_stream_agrees_with_materializing_it() {
        let dir = tmpdir("count");
        let _guard = CleanDir(dir.clone());

        {
            let db = Db::open_with(&dir, opts()).unwrap();
            let vals: Vec<u64> = (0..24u64)
                .flat_map(|c| [(c << 16) | 1, (c << 16) | 2])
                .collect();
            db.insert_many(1, &vals).unwrap();
            db.checkpoint().unwrap();
        }
        let db = Db::open_with(&dir, opts()).unwrap();
        db.insert(1, (300 << 16) | 5).unwrap();

        let snap = db.snapshot().unwrap();
        let want = snap.load(1).unwrap().len();
        assert_eq!(snap.key_stream(1).unwrap().cardinality().unwrap(), want);
        assert_eq!(snap.cardinality(1).unwrap(), want);

        // The hint is exact on both ends, since every count is already known.
        let s = snap.key_stream(1).unwrap();
        assert_eq!(s.cardinality_hint(), (want, Some(want)));

        // Per-chunk counting advances exactly as `next_chunk` does.
        let mut a = snap.key_stream(1).unwrap();
        let mut b = snap.key_stream(1).unwrap();
        while let Some((p, n)) = a.next_cardinality().unwrap() {
            let (q, c) = b.next_chunk().unwrap().expect("same length");
            assert_eq!((p, n), (q, c.len() as u64));
        }
        assert!(b.next_chunk().unwrap().is_none());
    }

    /// `chunks_remaining` counts down exactly, and survives every way of
    /// reaching the end.
    ///
    /// # Why this is not just an accessor test
    ///
    /// A sweep on 2026-09-14 found this method with **no caller anywhere**, one
    /// day after it was written, while `stats()` open-coded the identical
    /// subtraction beside it. Two expressions for one quantity is the shape that
    /// drifts, so the assertion that matters here is that they **agree** --
    /// `stats().chunks` and `chunks_remaining()` are now one expression, and this
    /// pins that rather than trusting it.
    ///
    /// The three exhaustion routes are covered because they reach `idx == len`
    /// by genuinely different means -- draining stops *at* the end,
    /// `cardinality_dyn` assigns `steps.len()`, and `seek` galloping past the
    /// last prefix advances to it. **None of them overshoots**, which a sabotage
    /// established rather than a reading: deleting the `min` from
    /// `chunks_remaining` leaves this test green. The routes are worth covering
    /// anyway -- they are three different ways for the count to be wrong at the
    /// boundary -- but this test does not pin the `min`, and nothing can.
    #[test]
    fn chunks_remaining_counts_down_exactly_and_never_underflows() {
        let dir = tmpdir("remaining");
        let _guard = CleanDir(dir.clone());

        {
            let db = Db::open_with(&dir, opts()).unwrap();
            let vals: Vec<u64> = (0..12u64).map(|c| (c << 16) | 3).collect();
            db.insert_many(1, &vals).unwrap();
            db.checkpoint().unwrap();
        }
        let db = Db::open_with(&dir, opts()).unwrap();
        let snap = db.snapshot().unwrap();

        // Route 1: drain one chunk at a time. The count must fall by exactly one
        // per chunk, and must equal what `stats()` reports at every step.
        let mut s = snap.key_stream(1).unwrap();
        let total = s.chunks_remaining();
        assert_eq!(total, 12, "one chunk per distinct prefix");
        for expect in (0..total).rev() {
            assert_eq!(
                s.stats().chunks,
                Some(s.chunks_remaining() as u64),
                "stats and the accessor must not drift"
            );
            assert!(s.next_chunk().unwrap().is_some());
            assert_eq!(s.chunks_remaining(), expect);
        }
        assert!(s.next_chunk().unwrap().is_none());
        assert_eq!(s.chunks_remaining(), 0);

        // Route 2: `cardinality_dyn` drains by assigning `idx = len`.
        let mut s = snap.key_stream(1).unwrap();
        assert_eq!(s.chunks_remaining(), total);
        let _ = s.cardinality_dyn().unwrap();
        assert_eq!(
            s.chunks_remaining(),
            0,
            "a counted-out stream has none left"
        );

        // Route 3: `seek` past the last prefix. This is the one that underflows
        // without the `min`, because `idx` can exceed `steps.len()`.
        let mut s = snap.key_stream(1).unwrap();
        s.seek(u64::MAX >> 16).unwrap();
        assert_eq!(
            s.chunks_remaining(),
            0,
            "seeking past the end must answer 0"
        );
        assert_eq!(s.stats().chunks, Some(0));
    }

    #[test]
    fn seek_lands_on_the_first_chunk_at_or_after_the_prefix() {
        let dir = tmpdir("seek");
        let _guard = CleanDir(dir.clone());

        {
            let db = Db::open_with(&dir, opts()).unwrap();
            // Even prefixes only, so a seek to an odd one must round up.
            let vals: Vec<u64> = (0..20u64).map(|c| ((c * 2) << 16) | 1).collect();
            db.insert_many(1, &vals).unwrap();
            db.checkpoint().unwrap();
        }
        let db = Db::open_with(&dir, opts()).unwrap();
        let snap = db.snapshot().unwrap();

        for target in [0u64, 1, 2, 7, 8, 38, 39, 40, 1000] {
            let mut s = snap.key_stream(1).unwrap();
            s.seek(target).unwrap();
            let got = s.peek_prefix().unwrap();
            let want = (0..20u64).map(|c| c * 2).find(|&p| p >= target);
            assert_eq!(got, want, "seek({target})");
            // And the chunk actually yielded matches the prefix peeked.
            assert_eq!(s.next_chunk().unwrap().map(|(p, _)| p), want);
        }

        // Seeking past the end exhausts, and `stats` must agree it is empty
        // rather than still claiming the chunks it skipped.
        let mut s = snap.key_stream(1).unwrap();
        s.seek(u64::MAX >> 16).unwrap();
        assert!(s.next_chunk().unwrap().is_none());
        assert_eq!(s.stats().chunks, Some(0));
        assert_eq!(s.stats().backing, Backing::Memory);
    }

    /// A lazy leaf must answer exactly what a materialized one answers.
    ///
    /// This is the equivalence that makes `key_expr` a *parallel
    /// implementation* of `Expr::set( load( k ) )` rather than a second
    /// evaluator. Run over every operator, because the planner rewrites each of
    /// them differently and a lazy leaf is a shape none of those rewrites had
    /// ever seen.
    #[test]
    fn a_lazy_leaf_evaluates_to_what_a_materialized_one_does() {
        use crate::Expr;

        let dir = tmpdir("lazy_leaf");
        let _guard = CleanDir(dir.clone());
        {
            let db = Db::open_with(&dir, opts()).unwrap();
            // Overlapping in some chunks, disjoint in others, so And/AndNot are
            // not trivially empty or trivially pass-through.
            db.insert_many(1, &(0..30u64).map(|c| (c << 16) | 5).collect::<Vec<_>>())
                .unwrap();
            db.insert_many(2, &(10..40u64).map(|c| (c << 16) | 5).collect::<Vec<_>>())
                .unwrap();
            db.insert_many(3, &(0..30u64).map(|c| (c << 16) | 9).collect::<Vec<_>>())
                .unwrap();
            db.checkpoint().unwrap();
        }
        let db = Db::open_with(&dir, opts()).unwrap();
        // A memtable-resident key too, so the mixed backing is covered.
        db.insert_many(4, &[1u64, 2, 3]).unwrap();
        let snap = db.snapshot().unwrap();

        let eager = |k: u64| Expr::set(snap.load(k).unwrap());
        let lazy = |k: u64| snap.key_expr(k);

        // ( name, built from a leaf-maker ) so each shape is built twice.
        /// Builds one expression shape from whichever kind of leaf it is given.
        type Shape = Box<dyn Fn(&dyn Fn(u64) -> Expr) -> Expr>;
        let shapes: Vec<(&str, Shape)> = vec![
            ("leaf", Box::new(|f| f(1))),
            ("and", Box::new(|f| f(1).and(f(2)))),
            ("or", Box::new(|f| f(1).or(f(2)))),
            ("xor", Box::new(|f| f(1).xor(f(2)))),
            ("and_not", Box::new(|f| f(1).and_not(f(2)))),
            ("disjoint_and", Box::new(|f| f(1).and(f(3)))),
            ("not", Box::new(|f| f(1).not_in(0, 40 << 16))),
            (
                "nested",
                Box::new(|f| f(1).and(f(2)).or(f(3).and_not(f(1)))),
            ),
            ("mixed_backing", Box::new(|f| f(1).or(f(4)))),
            (
                "with_range",
                Box::new(|f| f(1).and(Expr::Range(0, 15 << 16))),
            ),
        ];

        for (name, build) in &shapes {
            let e = build(&eager);
            let l = build(&lazy);

            let want = e.clone().collect_set().unwrap();
            let got = l.clone().collect_set().unwrap();
            assert_eq!(got, want, "{name}: lazy leaf disagreed on contents");

            // The non-materializing count must agree too -- it is a separate
            // walk, and `KeyStream::cardinality_dyn` never decodes a payload.
            assert_eq!(
                l.clone().cardinality().unwrap(),
                want.len(),
                "{name}: lazy cardinality disagreed"
            );
            assert_eq!(e.cardinality().unwrap(), want.len(), "{name}: control");
        }
    }

    /// Segmentation over lazy leaves must not change the answer.
    ///
    /// **The other equivalence test cannot reach this.** Its keys are 30 chunks
    /// and `SEGMENT_MIN_CHUNKS` is 128, so it exercises the unsegmented path
    /// only. Segmentation is a *rewrite* -- it cuts the domain, attributes
    /// contributors per segment and reopens each one -- so an operand dropped
    /// from a segment it actually occupied is a silently short answer, not an
    /// error. This is the shape that reaches it: operands above the threshold,
    /// nearly disjoint so the span pre-check passes, overlapping enough that
    /// `concat_disjoint_or` declines and segmentation is what runs.
    #[test]
    fn a_segmented_union_of_lazy_leaves_answers_what_an_eager_one_does() {
        use crate::Expr;
        use std::collections::BTreeSet;

        let dir = tmpdir("segmented");
        let _guard = CleanDir(dir.clone());

        const K: u64 = 8;
        // 200 chunks per key plus a 20-chunk lap into the next: comfortably over
        // the 128 threshold, and every neighbour pair genuinely overlaps.
        let (step, lap) = (200u64, 20u64);
        let mut oracle: BTreeSet<u64> = BTreeSet::new();
        {
            let db = Db::open_with(&dir, opts()).unwrap();
            for k in 0..K {
                let v: Vec<u64> = (k * step..k * step + step + lap)
                    .map(|c| (c << 16) | 7)
                    .collect();
                oracle.extend(v.iter().copied());
                db.insert_many(k, &v).unwrap();
            }
            db.checkpoint().unwrap();
        }
        let db = Db::open_with(&dir, opts()).unwrap();
        let snap = db.snapshot().unwrap();

        let or_of = |f: &dyn Fn(u64) -> Expr| {
            let mut e = f(0);
            for k in 1..K {
                e = e.or(f(k));
            }
            e
        };
        let lazy = or_of(&|k| snap.key_expr(k));
        let eager = or_of(&|k| Expr::set(snap.load(k).unwrap()));

        // Against the eager tree *and* an independent oracle, because both
        // trees could be rewritten by the same faulty rule.
        let got = lazy.clone().collect_set().unwrap();
        assert_eq!(got, eager.clone().collect_set().unwrap());
        assert_eq!(got.iter().collect::<BTreeSet<_>>(), oracle);
        assert_eq!(
            lazy.clone().cardinality().unwrap(),
            oracle.len() as u64,
            "the non-materializing count must survive segmentation too"
        );

        // Intersecting with a window keeps the union segmentable while changing
        // what each segment contributes -- the attribution, not just the cuts.
        let win = Expr::Range(300 << 16, 900 << 16);
        assert_eq!(
            lazy.and(win.clone()).collect_set().unwrap(),
            eager.and(win).collect_set().unwrap()
        );
    }

    /// The planner must see a lazy leaf, not an opaque blob.
    ///
    /// `ChunkSource` carries the three statistics precisely so the rewrites keep
    /// working; if they came back `None` the leaf would be charged
    /// `OPAQUE_CHUNKS` and every cost-guided decision over it would be a guess.
    #[test]
    fn the_planner_reads_a_lazy_leaf_the_way_it_reads_a_resident_one() {
        let dir = tmpdir("lazy_plan");
        let _guard = CleanDir(dir.clone());
        {
            let db = Db::open_with(&dir, opts()).unwrap();
            db.insert_many(1, &(0..24u64).map(|c| (c << 16) | 7).collect::<Vec<_>>())
                .unwrap();
            db.checkpoint().unwrap();
        }
        let db = Db::open_with(&dir, opts()).unwrap();
        let snap = db.snapshot().unwrap();

        let resident = snap.load(1).unwrap();
        let src = KeySource::new(snap.clone(), 1);

        assert_eq!(src.chunk_count(), Some(resident.chunk_count() as u64));
        assert_eq!(src.cardinality(), Some(resident.len()));
        assert_eq!(
            src.prefix_span(),
            Some((
                crate::split(resident.min().unwrap()).0,
                crate::split(resident.max().unwrap()).0
            ))
        );

        // Paged, which nothing else in the crate reports. Asserted off the
        // opened stream, because `StreamStats` is where the planner reads it --
        // there is deliberately no `backing()` on `ChunkSource`.
        assert_eq!(snap.key_expr(1).open().stats().backing, Backing::Paged);

        // And the planner's own view agrees with the resident leaf's.
        use crate::stream::plan::yield_chunks;
        assert_eq!(
            yield_chunks(&snap.key_expr(1)),
            yield_chunks(&crate::Expr::set(resident.clone()))
        );
    }

    #[test]
    fn key_source_bitmap_hint_matches_disk_and_memtable_encodings() {
        let dir = tmpdir("bitmap_hint");
        let _guard = CleanDir(dir.clone());
        {
            let db = Db::open_with(&dir, opts()).unwrap();
            let scattered: Vec<u64> = (0..5_000u64).map(|i| (i * 37) & 0xffff).collect();
            db.insert_many(1, &scattered).unwrap();
            db.insert(2, 7).unwrap();
            db.checkpoint().unwrap();
        }

        let db = Db::open_with(&dir, opts()).unwrap();
        let snap = db.snapshot().unwrap();
        assert!(snap
            .load(1)
            .unwrap()
            .chunks()
            .all(|(_, chunk)| matches!(chunk, Container::Bitmap(_))));
        assert_eq!(
            KeySource::new(snap.clone(), 1).all_bitmap_chunks(),
            Some(true)
        );
        assert_eq!(
            KeySource::new(snap.clone(), 2).all_bitmap_chunks(),
            Some(false)
        );
        drop(snap);

        db.insert(1, (1 << 16) | 9).unwrap();
        let snap = db.snapshot().unwrap();
        assert_eq!(KeySource::new(snap, 1).all_bitmap_chunks(), Some(false));
    }

    /// An evicted snapshot must not turn into an empty answer.
    ///
    /// The failure has to survive the whole lowering: `open` cannot return a
    /// `Result`, so it hands back an `ErrStream`, and the error has to reach the
    /// caller through whatever operator sits above it.
    #[test]
    fn a_lazy_leaf_on_an_evicted_snapshot_errors_rather_than_reading_empty() {
        let dir = tmpdir("lazy_evicted");
        let _guard = CleanDir(dir.clone());
        {
            let db = Db::open_with(&dir, opts()).unwrap();
            db.insert_many(1, &(0..8u64).map(|c| (c << 16) | 1).collect::<Vec<_>>())
                .unwrap();
            db.checkpoint().unwrap();
        }
        let db = Db::open_with(&dir, opts()).unwrap();
        let snap = db.snapshot().unwrap();
        let e = snap.key_expr(1);
        assert!(e.clone().cardinality().unwrap() > 0, "healthy first");

        assert!(db.evict_oldest_reader());

        assert!(
            e.clone().collect_set().is_err(),
            "an evicted lazy leaf must report, not read empty"
        );
        assert!(e.clone().cardinality().is_err());
        // Through an operator, which is where an EmptyStream would have hidden.
        assert!(e.or(crate::Expr::Range(0, 4)).collect_set().is_err());
    }

    /// A stream reads over an unbounded window, so one liveness check at
    /// construction is not enough.
    ///
    /// After eviction the reclamation floor no longer holds this reader's
    /// extents, and the `ChunkRef`s captured at construction may point at reused
    /// space. The stream must refuse rather than follow them.
    #[test]
    fn an_evicted_snapshot_stops_a_key_stream_that_is_already_open() {
        let dir = tmpdir("evicted");
        let _guard = CleanDir(dir.clone());

        {
            let db = Db::open_with(&dir, opts()).unwrap();
            let vals: Vec<u64> = (0..8u64).map(|c| (c << 16) | 1).collect();
            db.insert_many(1, &vals).unwrap();
            db.checkpoint().unwrap();
        }
        let db = Db::open_with(&dir, opts()).unwrap();
        let snap = db.snapshot().unwrap();
        let mut s = snap.key_stream(1).unwrap();

        assert!(s.next_chunk().unwrap().is_some(), "healthy before eviction");
        assert!(db.evict_oldest_reader(), "the snapshot must be evictable");

        let err = s.next_chunk().unwrap_err();
        assert!(
            matches!(err, crate::error::CodecError::SnapshotTooOld { .. }),
            "an evicted stream must refuse, got {err:?}"
        );
        // Counting is not a way around the gate.
        assert!(s.cardinality_dyn().is_err());
        assert!(
            snap.load(1).is_err(),
            "and `load` refuses for the same reason"
        );
    }

    /// The stream holds the reader slot, so it may outlive the `Snapshot` value.
    #[test]
    fn a_key_stream_outlives_the_snapshot_it_came_from() {
        let dir = tmpdir("outlive");
        let _guard = CleanDir(dir.clone());

        {
            let db = Db::open_with(&dir, opts()).unwrap();
            let vals: Vec<u64> = (0..12u64).map(|c| (c << 16) | 1).collect();
            db.insert_many(1, &vals).unwrap();
            db.checkpoint().unwrap();
        }
        let db = Db::open_with(&dir, opts()).unwrap();

        let (s, want) = {
            let snap = db.snapshot().unwrap();
            (snap.key_stream(1).unwrap(), snap.load(1).unwrap())
        };
        assert_eq!(db.live_readers(), 1, "the stream still pins the slot");

        // Churn and checkpoint under it: the extents it will read must survive.
        for c in 0..12u64 {
            db.insert(1, (c << 16) | 2).unwrap();
        }
        db.checkpoint().unwrap();

        assert_eq!(drain(s), want, "the stream still reads its own version");
    }

    /// Prefix bounds do not weaken the reader-slot lifetime or eviction checks.
    #[test]
    fn a_prefix_bounded_stream_keeps_and_checks_its_snapshot() {
        let dir = tmpdir("prefix_lifetime");
        let _guard = CleanDir(dir.clone());

        {
            let db = Db::open_with(&dir, opts()).unwrap();
            let vals: Vec<u64> = (0..12u64).map(|c| (c << 16) | 1).collect();
            db.insert_many(1, &vals).unwrap();
            db.checkpoint().unwrap();
        }
        let db = Db::open_with(&dir, opts()).unwrap();

        let (stream, want) = {
            let snap = db.snapshot().unwrap();
            let want = OrdSet::from_sorted_slice(
                &snap
                    .load(1)
                    .unwrap()
                    .iter()
                    .filter(|ordinal| {
                        let prefix = ordinal >> 16;
                        (3..7).contains(&prefix)
                    })
                    .collect::<Vec<_>>(),
            );
            (snap.key_stream_prefix_range(1, 3, 7).unwrap(), want)
        };
        assert_eq!(
            db.live_readers(),
            1,
            "the bounded stream must retain the dropped snapshot's slot"
        );

        for c in 0..12u64 {
            db.insert(1, (c << 16) | 2).unwrap();
        }
        db.checkpoint().unwrap();
        assert_eq!(drain(stream), want, "the stream must keep its own version");

        let snap = db.snapshot().unwrap();
        let mut stream = snap.key_stream_prefix_range(1, 0, 12).unwrap();
        assert!(stream.next_chunk().unwrap().is_some());
        assert!(db.evict_oldest_reader(), "the snapshot must be evictable");
        let err = stream.next_chunk().unwrap_err();
        assert!(
            matches!(err, crate::error::CodecError::SnapshotTooOld { .. }),
            "an evicted bounded stream must refuse, got {err:?}"
        );
        assert!(stream.cardinality_dyn().is_err());
    }
}
