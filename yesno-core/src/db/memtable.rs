//! Per-shard in-memory delta, with an MVCC version chain per chunk.
//!
//! # Why a chain and not a single value
//!
//! A reader holds a snapshot version and must see the state as of *that*
//! version, even while writers move ahead. Each chunk therefore keeps its
//! versions newest-first, and a read walks to the first entry at or below the
//! snapshot. Writers only ever prepend.
//!
//! # Tombstones are values
//!
//! `None` means "deleted at this version", not "absent". Without it, deleting a
//! chunk that exists on disk would be invisible to a reader whose snapshot is
//! newer than the delete — the memtable would simply have nothing to say and
//! the on-disk value would show through.
//!
//! # Pruning
//!
//! Versions below the oldest live snapshot are unreachable and can be dropped,
//! but the *newest* entry at or below that floor must be kept: it is still the
//! answer for every reader at or above it. Dropping it because it is "old" is
//! the classic MVCC truncation bug, so [`Memtable::prune`] keeps one.

use std::collections::BTreeMap;

use crate::container::Container;
use crate::mvcc::Version;
use crate::store::extent::ChunkKey;
use crate::{Prefix48, CHUNK_CARD};

/// One version of one chunk. `value: None` is a tombstone.
#[derive(Clone, Debug)]
struct Versioned {
    version: Version,
    value: Option<Container>,
}

/// A shard's uncommitted and recently-committed chunk state.
#[derive(Default)]
pub struct Memtable {
    /// Newest-first version chains, keyed for ordered iteration.
    chunks: BTreeMap<ChunkKey, Vec<Versioned>>,
    bytes: usize,
}

impl Memtable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Approximate resident size. Drives the checkpoint trigger and the
    /// mandatory write stall.
    #[inline]
    pub fn dirty_bytes(&self) -> usize {
        self.bytes
    }

    /// Test scaffolding: nothing in the crate consults this, and the lib target
    /// says so once the module is no longer `pub`.
    #[cfg(test)]
    #[inline]
    pub fn chunk_count(&self) -> usize {
        self.chunks.len()
    }

    /// State of a chunk as of `at`, or `None` if the memtable has nothing to say
    /// ( in which case the caller falls through to the on-disk index ).
    ///
    /// Returns `Some(None)` for a tombstone: the memtable positively knows the
    /// chunk is gone, which is different from not knowing.
    pub fn get(&self, key: ChunkKey, at: Version) -> Option<Option<&Container>> {
        let chain = self.chunks.get(&key)?;
        chain
            .iter()
            .find(|v| v.version <= at)
            .map(|v| v.value.as_ref())
    }

    /// Newest state regardless of version, for the writer's read-modify-write.
    fn latest(&self, key: ChunkKey) -> Option<&Container> {
        self.chunks.get(&key)?.first()?.value.as_ref()
    }

    fn put(&mut self, key: ChunkKey, version: Version, value: Option<Container>) {
        let added = value.as_ref().map_or(0, container_bytes);
        let chain = self.chunks.entry(key).or_default();
        // Writers only prepend, so the chain stays newest-first by construction.
        debug_assert!(
            chain.first().is_none_or(|v| v.version <= version),
            "versions must not go backwards"
        );
        if let Some(first) = chain.first_mut() {
            if first.version == version {
                // Same version writing again: replace in place rather than
                // stacking, so a batch touching one chunk repeatedly does not
                // grow an unbounded chain.
                self.bytes = self
                    .bytes
                    .saturating_sub(first.value.as_ref().map_or(0, container_bytes));
                first.value = value;
                self.bytes += added;
                return;
            }
        }
        chain.insert(0, Versioned { version, value });
        self.bytes += added;
    }

    /// Insert one ordinal, returning whether it changed the set.
    ///
    /// `base` supplies the on-disk container when the memtable has not touched
    /// this chunk yet, so the delta is a genuine read-modify-write rather than
    /// silently discarding what is already stored.
    pub fn insert(
        &mut self,
        key: u64,
        ordinal: u64,
        version: Version,
        base: impl FnOnce() -> Option<Container>,
    ) -> bool {
        let (prefix, low) = crate::split(ordinal);
        let ck = ChunkKey::new(key, prefix);
        let mut c = match self.chunks.get(&ck) {
            Some(_) => match self.latest(ck) {
                Some(c) => c.clone(),
                // A tombstone is a real answer: start fresh rather than
                // resurrecting whatever is on disk.
                None => Container::new_array(),
            },
            None => base().unwrap_or_else(Container::new_array),
        };
        let changed = c.insert(low);
        if changed {
            self.put(ck, version, Some(c));
        }
        changed
    }

    pub fn remove(
        &mut self,
        key: u64,
        ordinal: u64,
        version: Version,
        base: impl FnOnce() -> Option<Container>,
    ) -> bool {
        let (prefix, low) = crate::split(ordinal);
        let ck = ChunkKey::new(key, prefix);
        let mut c = match self.chunks.get(&ck) {
            Some(_) => match self.latest(ck) {
                Some(c) => c.clone(),
                None => return false, // already deleted
            },
            None => match base() {
                Some(c) => c,
                None => return false,
            },
        };
        let changed = c.remove(low);
        if changed {
            // An empty container is never stored; record a tombstone so the
            // deletion is visible over any on-disk value.
            let v = if c.is_empty() { None } else { Some(c) };
            self.put(ck, version, v);
        }
        changed
    }

    /// Apply sorted `vals` to one chunk in a single copy-on-write clone.
    ///
    /// The per-ordinal path pays a `ChunkKey` lookup, a version-chain walk and a
    /// clone **per value**; the whole point of grouping a commit's ordinals by
    /// chunk is that all of that is paid once. `vals` must be sorted and unique,
    /// which `plan_ops` guarantees.
    pub fn apply_values(
        &mut self,
        key: u64,
        prefix: Prefix48,
        vals: &[u16],
        remove: bool,
        version: Version,
        base: impl FnOnce() -> Option<Container>,
    ) -> u64 {
        debug_assert!(
            vals.windows(2).all(|w| w[0] < w[1]),
            "vals must be sorted and unique"
        );
        if vals.is_empty() {
            return 0;
        }
        let ck = ChunkKey::new(key, prefix);
        let existing = match self.chunks.get(&ck) {
            Some(_) => self.latest(ck).cloned(),
            None => base(),
        };
        let mut c = match existing {
            Some(c) => c,
            // Removing from nothing changes nothing; inserting starts fresh.
            None if remove => return 0,
            None => Container::new_array(),
        };
        let mut changed = 0u64;
        if remove {
            for &v in vals {
                changed += c.remove(v) as u64;
            }
        } else {
            for &v in vals {
                changed += c.insert(v) as u64;
            }
        }
        if changed > 0 {
            let v = if c.is_empty() { None } else { Some(c) };
            self.put(ck, version, v);
        }
        changed
    }

    /// Add `[lo, hi]` inclusive under `key`, one container call per chunk.
    ///
    /// The per-ordinal path costs a `ChunkKey` lookup, a version-chain walk and
    /// a copy-on-write clone **per ordinal**; this pays them once per chunk.
    /// A chunk the range covers completely does not need reading at all — the
    /// result is the full container regardless of what was there — which is
    /// what makes a bulk load independent of the prior contents.
    pub fn insert_range(
        &mut self,
        key: u64,
        lo: u64,
        hi: u64,
        version: Version,
        mut base: impl FnMut(Prefix48) -> Option<Container>,
    ) -> u64 {
        debug_assert!(lo <= hi);
        let mut changed = 0u64;
        let (mut o, end) = (lo, hi);
        while o <= end {
            let (prefix, low) = crate::split(o);
            // The last ordinal of this chunk, or the end of the range.
            let chunk_end = (prefix << crate::CHUNK_BITS) | u16::MAX as u64;
            let stop = chunk_end.min(end);
            let high = crate::split(stop).1;

            let ck = ChunkKey::new(key, prefix);
            let existing = match self.chunks.get(&ck) {
                Some(_) => self.latest(ck).cloned(),
                None => base(prefix),
            };
            let full_chunk = low == 0 && high == u16::MAX;
            let before_len = existing.as_ref().map(|c| c.len()).unwrap_or(0);
            let mut c = match existing {
                _ if full_chunk => Container::full(),
                Some(c) => c,
                None => Container::new_array(),
            };
            changed += if full_chunk {
                // `Container::full` is already the answer; count what it added.
                (crate::CHUNK_CARD - before_len) as u64
            } else {
                c.insert_range(low, high) as u64
            };
            self.put(ck, version, Some(c));
            // Test the ceiling *before* advancing. `stop + 1` overflows when
            // the range reaches `u64::MAX`, and a debug build panics on the add
            // even though the `break` below would have made the value unused.
            if stop == u64::MAX {
                break;
            }
            o = stop + 1;
        }
        changed
    }

    /// Drop `[lo, hi]` inclusive under `key`, one container call per chunk.
    pub fn remove_range(
        &mut self,
        key: u64,
        lo: u64,
        hi: u64,
        version: Version,
        mut base: impl FnMut(Prefix48) -> Option<Container>,
    ) -> u64 {
        debug_assert!(lo <= hi);
        let mut changed = 0u64;
        let (mut o, end) = (lo, hi);
        while o <= end {
            let (prefix, low) = crate::split(o);
            let chunk_end = (prefix << crate::CHUNK_BITS) | u16::MAX as u64;
            let stop = chunk_end.min(end);
            let high = crate::split(stop).1;

            let ck = ChunkKey::new(key, prefix);
            let existing = match self.chunks.get(&ck) {
                Some(_) => self.latest(ck).cloned(),
                None => base(prefix),
            };
            if let Some(mut c) = existing {
                let n = c.remove_range(low, high);
                if n > 0 {
                    changed += n as u64;
                    let v = if c.is_empty() { None } else { Some(c) };
                    self.put(ck, version, v);
                }
            }
            // Same ceiling guard as `insert_range`; see the note there.
            if stop == u64::MAX {
                break;
            }
            o = stop + 1;
        }
        changed
    }

    /// Replace a whole chunk, used by bulk load and by set-op writeback.
    pub fn put_chunk(&mut self, key: u64, prefix: Prefix48, c: Container, version: Version) {
        let ck = ChunkKey::new(key, prefix);
        let v = if c.is_empty() { None } else { Some(c) };
        self.put(ck, version, v);
    }

    /// Delete every chunk of `key` that the memtable knows about, and tombstone
    /// the ones named in `on_disk`.
    pub fn delete_key(
        &mut self,
        key: u64,
        version: Version,
        on_disk: impl IntoIterator<Item = Prefix48>,
    ) {
        let lo = ChunkKey::range_start(key);
        let hi = ChunkKey::range_end(key);
        let existing: Vec<ChunkKey> = self.chunks.range(lo..hi).map(|(k, _)| *k).collect();
        for ck in existing {
            self.put(ck, version, None);
        }
        for prefix in on_disk {
            self.put(ChunkKey::new(key, prefix), version, None);
        }
    }

    /// Chunks of one key visible at `at`, ascending. Tombstones are yielded as
    /// `None` so the caller can suppress the on-disk value.
    pub fn key_chunks(
        &self,
        key: u64,
        at: Version,
    ) -> impl Iterator<Item = (Prefix48, Option<&Container>)> + '_ {
        let lo = ChunkKey::range_start(key);
        let hi = ChunkKey::range_end(key);
        self.chunks.range(lo..hi).filter_map(move |(ck, chain)| {
            chain
                .iter()
                .find(|v| v.version <= at)
                .map(|v| (ck.prefix(), v.value.as_ref()))
        })
    }

    /// Every chunk visible at `at`, in `ChunkKey` order — the stream a
    /// checkpoint consumes.
    pub fn iter_at(
        &self,
        at: Version,
    ) -> impl Iterator<Item = (ChunkKey, Option<&Container>)> + '_ {
        self.chunks.iter().filter_map(move |(ck, chain)| {
            chain
                .iter()
                .find(|v| v.version <= at)
                .map(|v| (*ck, v.value.as_ref()))
        })
    }

    /// Drop versions no live reader can reach.
    ///
    /// Keeps the newest entry at or below `floor`, because that entry is still
    /// the answer for every reader at or above it. Returns how many were
    /// dropped.
    /// Drop chunks the **store** can now answer, and only those.
    ///
    /// # Why `prune` is not enough
    ///
    /// `prune` trims versions no reader can reach, but always keeps the newest
    /// one at or below the floor, because in general it is the only copy. That
    /// makes the memtable a permanent in-memory mirror of every chunk ever
    /// written: nothing ever leaves it, so every chunk stays *dirty* forever and
    /// every checkpoint rewrites the entire database. Touching one key in a
    /// 40-key shard superseded all 40.
    ///
    /// After a checkpoint the newest copy is no longer the only one — the store
    /// holds it — so the chain can go entirely.
    ///
    /// # The precondition, which the caller must guarantee
    ///
    /// `floor` must be at or below **both** the last checkpoint's watermark
    /// ( so the store really does hold this state ) and `safe_version` ( so no
    /// live reader needs an older version than the one being dropped ). Passing
    /// a floor above either silently serves the wrong data: a reader below it
    /// would fall through to a store copy that is *newer* than its snapshot.
    ///
    /// Never call this for a shard with no store. There is nothing to fall
    /// through to, and the data is simply lost.
    pub fn evict_durable(&mut self, floor: Version) -> usize {
        let mut evicted = 0usize;
        let mut freed = 0usize;
        self.chunks.retain(|_, chain| {
            // Only a chain reduced to a single durable version. Anything with
            // history still has a reader that might want the older entry, and
            // `prune` is what decides when that stops being true.
            let redundant =
                chain.len() == 1 && chain[0].version <= floor && chain[0].value.is_some();
            if redundant {
                freed += chain[0].value.as_ref().map_or(0, container_bytes);
                evicted += 1;
            }
            !redundant
        });
        self.bytes = self.bytes.saturating_sub(freed);
        evicted
    }

    pub fn prune(&mut self, floor: Version) -> usize {
        let mut dropped = 0usize;
        let mut freed = 0usize;
        self.chunks.retain(|_, chain| {
            // Index of the newest version at or below the floor; everything
            // after it is unreachable.
            if let Some(keep) = chain.iter().position(|v| v.version <= floor) {
                for v in chain.drain(keep + 1..) {
                    freed += v.value.as_ref().map_or(0, container_bytes);
                    dropped += 1;
                }
            }
            // A chain that has become a lone tombstone below the floor carries
            // no information any reader can use.
            !(chain.len() == 1 && chain[0].value.is_none() && chain[0].version <= floor)
        });
        self.bytes = self.bytes.saturating_sub(freed);
        dropped
    }
}

/// Rough resident cost of a container.
fn container_bytes(c: &Container) -> usize {
    match c.kind() {
        crate::ContainerKind::Bitmap => crate::BITMAP_BYTES,
        _ => c.payload_bytes(),
    }
    .max(1)
        + 32 // chain entry and map overhead
}

/// Cardinality of a key as of `at`, combining the memtable with a resolver for
/// chunks it has not touched.
/// Total cardinality of `key` at `at`, merging the memtable over per-chunk
/// counts read from the index.
///
/// `on_disk` must yield `( prefix, cardinality )` in **ascending prefix
/// order**, which an index range scan does by construction. Both sides being
/// sorted is what makes this a linear two-pointer merge.
///
/// It used to take a lookup closure plus a separate prefix list, and build a
/// `BTreeSet` of every memtable prefix to suppress the disk side — three
/// `O(n log n)` structures to merge two sorted streams. That is the same defect
/// as the one recorded on `Snapshot::merged_chunks`; see `load-is-superlinear`
/// in `JOURNAL.md`, 2026-08-26. Do not reintroduce a set or a map
/// here: `key_chunks` is already ordered, and so is the caller's scan.
/// Ordinals of `key` in the half-open range `[lo, hi)` at `at`, merging the
/// memtable over an ascending on-disk scan.
///
/// # The whole point is what it does *not* read
///
/// A chunk the range covers **wholly** contributes its `card_m1` — the number
/// already in the leaf entry — and its payload is never fetched. Only a chunk
/// the range covers *partially* needs one, and there are at most two of those
/// however wide the range is. A 1 M-row Parquet row group spans about sixteen
/// chunks; this reads two.
///
/// `load` is therefore called at most twice per query, and only for a disk chunk
/// with a partial window. Memtable chunks are already resident, so they are
/// counted directly whatever their window.
///
/// A tombstone contributes nothing **and** suppresses the on-disk count, which
/// is why absence and `Some(None)` cannot be conflated — the same reason
/// [`cardinality_at`] says so.
pub fn count_in_range_at(
    mem: &Memtable,
    key: u64,
    at: Version,
    lo: u64,
    hi: u64,
    on_disk: impl IntoIterator<Item = (Prefix48, u32)>,
    load: impl Fn(Prefix48) -> Option<Container>,
) -> u64 {
    if hi <= lo {
        return 0;
    }
    let mut total = 0u64;
    let mut overlay = mem.key_chunks(key, at).peekable();
    let mut last: Option<Prefix48> = None;

    // A memtable chunk is resident, so its window costs nothing to apply.
    let from_mem = |p: Prefix48, c: &Container| -> u64 {
        match crate::chunk_window(p, lo, hi) {
            Some((l, h)) => c.count_in_range(l, h) as u64,
            None => 0,
        }
    };

    for (prefix, n) in on_disk {
        debug_assert!(
            last.is_none_or(|p| p < prefix),
            "count_in_range_at requires an ascending on-disk scan"
        );
        last = Some(prefix);

        while overlay.peek().is_some_and(|&(p, _)| p < prefix) {
            if let (p, Some(c)) = overlay.next().unwrap() {
                total += from_mem(p, c);
            }
        }

        if overlay.peek().is_some_and(|&(p, _)| p == prefix) {
            if let (p, Some(c)) = overlay.next().unwrap() {
                total += from_mem(p, c);
            }
            continue;
        }

        let Some((l, h)) = crate::chunk_window(prefix, lo, hi) else {
            continue;
        };
        debug_assert!(n <= CHUNK_CARD);
        if l == 0 && h >= CHUNK_CARD {
            // Wholly covered: the leaf entry already says how many.
            total += n as u64;
        } else if let Some(c) = load(prefix) {
            total += c.count_in_range(l, h) as u64;
        }
    }

    for (p, c) in overlay {
        if let Some(c) = c {
            total += from_mem(p, c);
        }
    }
    total
}

pub fn cardinality_at(
    mem: &Memtable,
    key: u64,
    at: Version,
    on_disk: impl IntoIterator<Item = (Prefix48, u32)>,
) -> u64 {
    let mut total = 0u64;
    let mut overlay = mem.key_chunks(key, at).peekable();
    let mut last: Option<Prefix48> = None;

    for (prefix, n) in on_disk {
        debug_assert!(
            last.is_none_or(|p| p < prefix),
            "cardinality_at requires an ascending on-disk scan"
        );
        last = Some(prefix);

        // Memtable chunks below this prefix are uncontested.
        while overlay.peek().is_some_and(|&(p, _)| p < prefix) {
            if let (_, Some(c)) = overlay.next().unwrap() {
                total += c.len() as u64;
            }
        }

        // The memtable wins wherever it has an opinion. A tombstone contributes
        // nothing *and* suppresses the on-disk count, which is the whole reason
        // absence and `None` cannot be conflated here.
        if overlay.peek().is_some_and(|&(p, _)| p == prefix) {
            if let (_, Some(c)) = overlay.next().unwrap() {
                total += c.len() as u64;
            }
            continue;
        }

        debug_assert!(n <= CHUNK_CARD);
        total += n as u64;
    }

    for (_, c) in overlay {
        if let Some(c) = c {
            total += c.len() as u64;
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none() -> Option<Container> {
        None
    }

    #[test]
    fn insert_and_read_at_a_version() {
        let mut m = Memtable::new();
        assert!(m.insert(1, 5, 10, none));
        assert!(!m.insert(1, 5, 10, none), "re-inserting is not a change");

        let ck = ChunkKey::new(1, 0);
        assert!(m.get(ck, 10).unwrap().unwrap().contains(5));
        assert!(m.get(ck, 9).is_none(), "invisible to an older snapshot");
    }

    #[test]
    fn a_reader_at_an_older_version_sees_the_older_state() {
        let mut m = Memtable::new();
        m.insert(1, 5, 10, none);
        m.insert(1, 6, 20, none);
        let ck = ChunkKey::new(1, 0);

        let at10 = m.get(ck, 10).unwrap().unwrap();
        assert!(
            at10.contains(5) && !at10.contains(6),
            "v10 must not see v20's write"
        );

        let at20 = m.get(ck, 20).unwrap().unwrap();
        assert!(at20.contains(5) && at20.contains(6));

        // And a version in between resolves to the older one.
        assert!(!m.get(ck, 15).unwrap().unwrap().contains(6));
    }

    #[test]
    fn a_tombstone_is_distinct_from_absence() {
        let mut m = Memtable::new();
        // Chunk exists on disk with {7}; delete it.
        let base = || Some(Container::from_sorted(&[7]));
        assert!(m.remove(1, 7, 5, base));

        let ck = ChunkKey::new(1, 0);
        // The memtable positively knows it is gone...
        assert_eq!(m.get(ck, 5).map(|v| v.is_none()), Some(true));
        // ...which is different from having nothing to say.
        assert!(m.get(ChunkKey::new(2, 0), 5).is_none());
    }

    #[test]
    fn a_delete_is_visible_over_an_on_disk_value() {
        // The bug tombstones exist to prevent: without them the on-disk value
        // would show through to a reader newer than the delete.
        let mut m = Memtable::new();
        m.remove(1, 7, 5, || Some(Container::from_sorted(&[7])));
        let chunks: Vec<_> = m.key_chunks(1, 5).collect();
        assert_eq!(chunks.len(), 1);
        assert!(
            chunks[0].1.is_none(),
            "the tombstone must be yielded, not skipped"
        );
    }

    #[test]
    fn inserting_after_a_delete_does_not_resurrect_the_old_contents() {
        let mut m = Memtable::new();
        let base = || Some(Container::from_sorted(&[1, 2, 3]));
        m.remove(1, 1, 5, base);
        m.remove(1, 2, 5, || Some(Container::from_sorted(&[2, 3])));
        m.remove(1, 3, 5, || Some(Container::from_sorted(&[3])));
        assert!(m.get(ChunkKey::new(1, 0), 5).unwrap().is_none());

        // A later insert must start from the tombstone, not from disk.
        m.insert(1, 9, 6, || Some(Container::from_sorted(&[1, 2, 3])));
        let c = m.get(ChunkKey::new(1, 0), 6).unwrap().unwrap();
        assert_eq!(
            c.iter().collect::<Vec<_>>(),
            vec![9],
            "must not resurrect 1,2,3"
        );
    }

    #[test]
    fn removing_the_last_ordinal_leaves_a_tombstone_not_an_empty_container() {
        let mut m = Memtable::new();
        m.insert(1, 5, 10, none);
        assert!(m.remove(1, 5, 11, none));
        assert!(
            m.get(ChunkKey::new(1, 0), 11).unwrap().is_none(),
            "an empty container is never stored"
        );
    }

    #[test]
    fn repeated_writes_at_one_version_replace_rather_than_stack() {
        let mut m = Memtable::new();
        for i in 0..100u64 {
            m.insert(1, i, 7, none);
        }
        let c = m.get(ChunkKey::new(1, 0), 7).unwrap().unwrap();
        assert_eq!(c.len(), 100);
        // A batch touching one chunk repeatedly must not grow an unbounded chain.
        assert_eq!(m.chunk_count(), 1);
    }

    #[test]
    fn key_chunks_covers_only_the_requested_key() {
        let mut m = Memtable::new();
        m.insert(1, 5, 1, none);
        m.insert(1, 1 << 20, 1, none);
        m.insert(2, 5, 1, none);

        let got: Vec<Prefix48> = m.key_chunks(1, 1).map(|(p, _)| p).collect();
        assert_eq!(got, vec![0, 16]);
        assert_eq!(m.key_chunks(2, 1).count(), 1);
        assert_eq!(m.key_chunks(3, 1).count(), 0);
    }

    #[test]
    fn delete_key_tombstones_both_memtable_and_on_disk_chunks() {
        let mut m = Memtable::new();
        m.insert(1, 5, 1, none);
        // Chunk at prefix 99 exists only on disk.
        m.delete_key(1, 2, [99u64]);

        let chunks: Vec<_> = m.key_chunks(1, 2).collect();
        assert_eq!(chunks.len(), 2);
        assert!(
            chunks.iter().all(|(_, c)| c.is_none()),
            "all must be tombstones"
        );
        assert!(
            chunks.iter().any(|(p, _)| *p == 99),
            "the on-disk chunk needs one too"
        );
    }

    #[test]
    fn iteration_is_in_chunkkey_order() {
        let mut m = Memtable::new();
        for key in [5u64, 1, 3] {
            for prefix in [2u64, 0, 1] {
                m.put_chunk(key, prefix, Container::from_sorted(&[1]), 1);
            }
        }
        let keys: Vec<ChunkKey> = m.iter_at(1).map(|(k, _)| k).collect();
        assert!(
            keys.windows(2).all(|w| w[0] < w[1]),
            "checkpoint depends on this order"
        );
        assert_eq!(keys.len(), 9);
    }

    #[test]
    fn prune_keeps_the_newest_version_at_or_below_the_floor() {
        // The classic MVCC truncation bug: that entry is still the answer for
        // every reader at or above the floor.
        let mut m = Memtable::new();
        m.insert(1, 1, 10, none);
        m.insert(1, 2, 20, none);
        m.insert(1, 3, 30, none);

        let dropped = m.prune(20);
        assert_eq!(dropped, 1, "only v10 is unreachable");

        let ck = ChunkKey::new(1, 0);
        let at20 = m.get(ck, 20).unwrap().unwrap();
        assert!(at20.contains(1) && at20.contains(2) && !at20.contains(3));
        let at30 = m.get(ck, 30).unwrap().unwrap();
        assert!(at30.contains(3));
    }

    #[test]
    fn prune_below_every_version_keeps_the_chain_intact() {
        let mut m = Memtable::new();
        m.insert(1, 1, 10, none);
        m.insert(1, 2, 20, none);
        assert_eq!(
            m.prune(5),
            0,
            "nothing at or below the floor, nothing to drop"
        );
        assert!(m.get(ChunkKey::new(1, 0), 20).is_some());
    }

    #[test]
    fn prune_drops_a_chain_that_is_only_an_old_tombstone() {
        let mut m = Memtable::new();
        m.remove(1, 7, 5, || Some(Container::from_sorted(&[7])));
        assert_eq!(m.chunk_count(), 1);
        m.prune(10);
        assert_eq!(
            m.chunk_count(),
            0,
            "a tombstone below the floor tells nobody anything"
        );
    }

    #[test]
    fn dirty_bytes_grows_and_shrinks() {
        let mut m = Memtable::new();
        assert_eq!(m.dirty_bytes(), 0);
        for i in 0..1000u64 {
            m.insert(1, i * 1000, 1, none);
        }
        let peak = m.dirty_bytes();
        assert!(peak > 0);
        m.insert(1, 1, 2, none);
        m.prune(2);
        assert!(
            m.dirty_bytes() <= peak + 1024,
            "pruning must release accounting"
        );
    }

    #[test]
    fn cardinality_combines_memtable_and_disk_without_double_counting() {
        let mut m = Memtable::new();
        // Prefix 0 is modified in the memtable; prefix 1 exists only on disk.
        m.insert(1, 5, 10, || Some(Container::from_sorted(&[1, 2])));
        let n = cardinality_at(&m, 1, 10, [(0u64, 2u32), (1, 7)]);
        assert_eq!(
            n,
            3 + 7,
            "memtable wins for prefix 0; disk supplies prefix 1"
        );
    }

    #[test]
    fn cardinality_respects_tombstones() {
        let mut m = Memtable::new();
        m.remove(1, 5, 10, || Some(Container::from_sorted(&[5])));
        let n = cardinality_at(&m, 1, 10, [(0u64, 99u32)]);
        assert_eq!(n, 0, "a tombstone must suppress the on-disk count");
    }

    /// The merge has four positions relative to the disk scan — memtable-only
    /// below, contested, disk-only, memtable-only above — and the `BTreeSet`
    /// form it replaced could not get any of them wrong, because it did not
    /// have positions. Pin all four in one corpus.
    #[test]
    fn cardinality_merges_every_relative_position_of_the_two_streams() {
        let mut m = Memtable::new();
        // prefix 0: memtable only, below every on-disk chunk.
        m.put_chunk(1, 0, Container::from_sorted(&[1, 2, 3]), 10);
        // prefix 2: contested — the memtable's 1 must win over the disk's 50.
        m.put_chunk(1, 2, Container::from_sorted(&[9]), 10);
        // prefix 3: contested by a tombstone, which suppresses the disk's 50.
        m.remove(1, (3 << 16) | 7, 10, || Some(Container::from_sorted(&[7])));
        // prefix 9: memtable only, above every on-disk chunk.
        m.put_chunk(1, 9, Container::from_sorted(&[1, 2]), 10);

        // prefixes 1 and 4 are disk-only.
        let disk = [(1u64, 50u32), (2, 50), (3, 50), (4, 50)];
        // Written out per position rather than as one total, so a failure names
        // which of the five contributions moved. The tombstoned prefix is the
        // one that must contribute nothing *and* suppress the disk's 50.
        let expect: u64 = [
            3,  // prefix 0, memtable only, below the scan
            50, // prefix 1, disk only
            1,  // prefix 2, contested: the memtable's 1 beats the disk's 50
            50, // prefix 4, disk only
            2,  // prefix 9, memtable only, above the scan
        ]
        .iter()
        .sum(); // prefix 3 is tombstoned, so it has no row here at all
        assert_eq!(
            cardinality_at(&m, 1, 10, disk),
            expect,
            "memtable-below, disk-only, contested, tombstoned, memtable-above"
        );
    }
}
