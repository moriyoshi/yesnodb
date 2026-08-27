//! Copy-on-write B+tree over [`ChunkKey`].
//!
//! # Bottom-up bulk build, not incremental insert
//!
//! Nodes are immutable once written ( I2 ), so there is no in-place insert to
//! implement. A checkpoint merges the in-memory delta with the previous tree
//! into a sorted stream and rebuilds the touched path bottom-up. That is the
//! only writer, which keeps the shape of this module small: fill leaves, collect
//! `(first_key, page_id)`, repeat one level up until a single root remains.
//!
//! A snapshot is therefore just `(root, height)` — two integers — and taking one
//! costs nothing.
//!
//! # Internal keys are uncompressed
//!
//! Leaves carry prefix-compressed suffixes because leaf entries outnumber
//! internal entries by the fanout, so that is where the bytes are. Full 14-byte
//! separators here.
//!
//! **The number that used to be in this paragraph was wrong by 7.3x.** It said
//! internal levels add "roughly `1/fanout` of the total, so compressing them
//! buys ~0.25%". The formula is right and the fanout is the wrong one: the
//! ratio depends on the **internal** fanout, not the leaf fanout. Internal
//! entries are full-width, so `INTERNAL_ENTRY` = 18 B gives 55 per 1 KiB node,
//! and `1/55` = **1.82%**. Computed over 1e6 index entries it is **1.83%**, and
//! **flat across every legal leaf suffix width** ( 1.828% to 1.837% for ksuf 2
//! through 14 ) -- the leaf fanout cancels out of the ratio entirely, which is
//! why a leaf-side intuition gets it wrong in the first place.
//!
//! **The conclusion survives, by a route the old number did not give.** Halving
//! the separator would roughly double internal fanout and save ~1% of *index*
//! bytes -- 4x the payoff previously recorded, and still not worth the subtlety,
//! because index nodes are themselves a few percent of the file ( see
//! `db/store.rs` ). So the saving is ~0.05% of the database, and the case rests
//! on that rather than on the internal share being negligible. It is not.
//! Measured 2026-09-14; see `internal-key-compression` in TODO.md.
//!
//! # No sibling pointers
//!
//! A leaf-to-leaf link would be invalidated by the sibling's own copy-on-write,
//! forcing a cascade of rewrites. Range scans use a cursor stack instead;
//! crossing a leaf boundary costs one step up and back down, amortized
//! `1/fanout` per entry.

use arrow_buffer::Buffer;

use crate::error::{CodecError, Result};
use crate::index::node::{
    checksum, choose_ksuf, LeafBuilder, LeafRef, HEADER, NODE_INTERNAL, NODE_LEAF, OFF_CRC, VERSION,
};
use crate::store::extent::{ChunkKey, ChunkRef, CHUNKKEY_BYTES};

/// A page identifier within the index region.
pub type PageId = u32;

/// Bytes per internal entry: a full separator key plus a child pointer.
pub const INTERNAL_ENTRY: usize = CHUNKKEY_BYTES + 4;

const OFF_TYPE: usize = 0;
const OFF_VER: usize = 1;
const OFF_NKEYS: usize = 2;

/// Children an internal node of `node_size` can address.
#[inline]
pub const fn internal_capacity(node_size: usize) -> usize {
    // n separators + (n+1) children must fit.
    (node_size - HEADER - 4) / INTERNAL_ENTRY + 1
}

/// One index page, opaque over what keeps it alive.
///
/// Owns its backing rather than borrowing it, so the same type serves both
/// tiers: an in-memory store hands back a refcount bump, and an mmap-backed one
/// hands back a window into the mapping with no copy at all. A `&[u8]` return
/// could not express the second without tying the borrow to a lock guard.
///
/// **The field is private on purpose — this is policy R1.** "No Arrow types
/// in `yesno-core`'s public API. None, including `Buffer`", whose payoff is that
/// an `arrow-buffer` major bump is a *patch* release of core. `NodeReader::node`
/// returned a `Buffer` directly, which put that type in the signature of a
/// `pub trait` that `checkpoint::CheckpointSink` requires — so a bump changed a
/// signature every implementor had to match. Wrapping it costs nothing: every
/// consumer only ever wanted [`Page::as_slice`].
#[derive(Clone)]
pub struct Page {
    inner: Buffer,
}

impl Page {
    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        &self.inner
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

impl Page {
    /// `pub(crate)`, which is what keeps the Arrow type out of the public
    /// API. Every implementor of [`NodeReader`] is in this workspace, so no
    /// external one needs to build a `Page`.
    #[inline]
    pub(crate) fn from_buffer(inner: Buffer) -> Self {
        Page { inner }
    }
}

/// Somewhere to read index pages from.
pub trait NodeReader {
    fn node(&self, id: PageId) -> Result<Page>;
}

/// Somewhere to append index pages. Append-only: nodes are never rewritten.
pub trait NodeWriter {
    fn append_node(&mut self, bytes: &[u8]) -> Result<PageId>;
}

/// An in-memory page store.
///
/// This said "for tests and for the memtable-side tree" until 2026-08-28. The
/// second half is not true: with `index` no longer a `pub mod`, the compiler
/// reports it constructed only from `#[cfg(test)]` code. If a memtable-side tree
/// ever wants it, lift the `cfg` then rather than leaving the claim standing.
#[cfg(test)]
#[derive(Default)]
pub struct VecNodes {
    pages: Vec<Buffer>,
}

#[cfg(test)]
impl VecNodes {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn is_empty(&self) -> bool {
        self.pages.is_empty()
    }
    /// Total bytes occupied by index pages — the term that dominates total size
    /// for sparse data, so it is worth being able to measure directly.
    pub fn bytes(&self) -> usize {
        self.pages.iter().map(|p| p.len()).sum()
    }
}

#[cfg(test)]
impl NodeReader for VecNodes {
    fn node(&self, id: PageId) -> Result<Page> {
        self.pages
            .get(id as usize)
            .cloned()
            .map(Page::from_buffer)
            .ok_or(CodecError::Invariant("no such index page"))
    }
}

#[cfg(test)]
impl NodeWriter for VecNodes {
    fn append_node(&mut self, bytes: &[u8]) -> Result<PageId> {
        let id = self.pages.len() as PageId;
        self.pages.push(Buffer::from_vec(bytes.to_vec()));
        Ok(id)
    }
}

/// Serialize an internal node from `(first_key, child)` pairs.
///
/// `entries[i].0` is the smallest key reachable through `entries[i].1`. The
/// first entry's key is implied by the parent, so only `entries[1..]` are stored
/// as separators.
fn build_internal(node_size: usize, entries: &[(ChunkKey, PageId)]) -> Result<Vec<u8>> {
    if entries.is_empty() {
        return Err(CodecError::Invariant("cannot build an empty internal node"));
    }
    if entries.len() > internal_capacity(node_size) {
        return Err(CodecError::Invariant(
            "too many children for one internal node",
        ));
    }
    let nsep = entries.len() - 1;
    let mut buf = vec![0u8; node_size];
    buf[OFF_TYPE] = NODE_INTERNAL;
    buf[OFF_VER] = VERSION;
    buf[OFF_NKEYS..OFF_NKEYS + 2].copy_from_slice(&(nsep as u16).to_le_bytes());

    for (i, (k, _)) in entries.iter().enumerate().skip(1) {
        let off = HEADER + (i - 1) * CHUNKKEY_BYTES;
        buf[off..off + CHUNKKEY_BYTES].copy_from_slice(&k.to_be_bytes());
    }
    let ch_off = HEADER + nsep * CHUNKKEY_BYTES;
    for (i, (_, c)) in entries.iter().enumerate() {
        let off = ch_off + i * 4;
        buf[off..off + 4].copy_from_slice(&c.to_le_bytes());
    }
    let crc = checksum(&buf);
    buf[OFF_CRC..OFF_CRC + 4].copy_from_slice(&crc.to_le_bytes());
    Ok(buf)
}

/// A parsed internal node.
struct InternalRef<'a> {
    buf: &'a [u8],
    nsep: usize,
}

impl<'a> InternalRef<'a> {
    fn parse(buf: &'a [u8]) -> Result<Self> {
        if buf.len() < HEADER + 4 {
            return Err(CodecError::Truncated {
                expected: HEADER + 4,
                found: buf.len(),
            });
        }
        if buf[OFF_TYPE] != NODE_INTERNAL {
            return Err(CodecError::Invariant("not an internal node"));
        }
        if buf[OFF_VER] != VERSION {
            return Err(CodecError::UnsupportedEncoding);
        }
        let nsep = u16::from_le_bytes([buf[OFF_NKEYS], buf[OFF_NKEYS + 1]]) as usize;
        let need = HEADER + nsep * CHUNKKEY_BYTES + (nsep + 1) * 4;
        if need > buf.len() {
            return Err(CodecError::Truncated {
                expected: need,
                found: buf.len(),
            });
        }
        Ok(InternalRef { buf, nsep })
    }

    #[inline]
    fn separator(&self, i: usize) -> ChunkKey {
        let off = HEADER + i * CHUNKKEY_BYTES;
        ChunkKey::from_be_bytes(self.buf[off..off + CHUNKKEY_BYTES].try_into().unwrap())
    }

    #[inline]
    fn child(&self, i: usize) -> PageId {
        let off = HEADER + self.nsep * CHUNKKEY_BYTES + i * 4;
        u32::from_le_bytes(self.buf[off..off + 4].try_into().unwrap())
    }

    #[inline]
    fn child_count(&self) -> usize {
        self.nsep + 1
    }

    /// Index of the child that may contain `target`.
    fn child_for(&self, target: ChunkKey) -> usize {
        // separator[i] is the first key of child[i+1], so descend into the last
        // child whose separator is <= target.
        let mut lo = 0usize;
        let mut hi = self.nsep;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.separator(mid) <= target {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }
}

/// One leaf as it exists on disk: its page and the entries it holds.
///
/// The entries are what `build_reusing` compares against, so both halves are
/// needed — a key whose `ChunkRef` moved must not reuse the leaf that still
/// names its old extent.
type LeafSnapshot = (PageId, Vec<(ChunkKey, ChunkRef)>);

/// One previous leaf, identified and bounded but not decoded.
///
/// `last` is the leaf's own final key; the range it *owns* runs to the next
/// leaf's `first`, which is where a key inserted between two leaves belongs.
struct LeafSpan {
    id: PageId,
    first: ChunkKey,
    last: ChunkKey,
    len: usize,
}

/// An immutable tree rooted at a single page.
///
/// Cheap to copy: a snapshot is exactly this value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tree {
    pub root: PageId,
    pub height: u8,
    pub node_size: usize,
}

/// What [`Tree::build_updating`] produces besides the new root.
///
/// `superseded` carries the old refs of every dropped entry, gathered during the
/// merge rather than by the caller looking each one up: the merge visits every
/// previous entry regardless, so they cost nothing there and ~0.7 us each as a
/// point lookup.
pub struct TreeUpdate {
    pub tree: Option<Tree>,
    /// Previous entries that survived into the new tree.
    pub carried: u64,
    /// Refs of entries the new tree does not carry, for reclamation.
    pub superseded: Vec<ChunkRef>,
}

impl Tree {
    /// Build from a sorted, deduplicated stream of entries.
    ///
    /// Returns `None` for an empty stream — an empty tree has no root, and
    /// inventing one would mean writing a page that says nothing.
    pub fn build(
        w: &mut impl NodeWriter,
        node_size: usize,
        entries: &[(ChunkKey, ChunkRef)],
    ) -> Result<Option<Tree>> {
        if entries.is_empty() {
            return Ok(None);
        }
        if entries.windows(2).any(|p| p[0].0 >= p[1].0) {
            return Err(CodecError::Invariant("tree input must strictly ascend"));
        }

        // Level 0: pack leaves, choosing the narrowest suffix width per leaf.
        let mut level: Vec<(ChunkKey, PageId)> = Vec::new();
        let mut i = 0usize;
        while i < entries.len() {
            let (bytes, consumed) = Self::pack_leaf(node_size, &entries[i..])?;
            let id = w.append_node(&bytes)?;
            level.push((entries[i].0, id));
            i += consumed;
        }

        // Levels 1..: pack children until a single root remains.
        let mut height = 1u8;
        let cap = internal_capacity(node_size);
        while level.len() > 1 {
            let mut next = Vec::with_capacity(level.len().div_ceil(cap));
            for group in level.chunks(cap) {
                let bytes = build_internal(node_size, group)?;
                let id = w.append_node(&bytes)?;
                next.push((group[0].0, id));
            }
            level = next;
            height += 1;
        }
        Ok(Some(Tree {
            root: level[0].1,
            height,
            node_size,
        }))
    }

    /// Fill one leaf greedily, returning the page bytes and how many entries fit.
    ///
    /// The suffix width is chosen from the widest run that would fit at the
    /// narrowest width, then verified — `choose_ksuf` guarantees the batch it was
    /// derived from is admissible, which is what stops this from looping.
    fn pack_leaf(node_size: usize, rest: &[(ChunkKey, ChunkRef)]) -> Result<(Vec<u8>, usize)> {
        // Try the narrowest width that admits at least the first entry, then let
        // `push` reject keys that no longer fit and seal there.
        let probe_len = rest
            .len()
            .min(crate::index::node::leaf_capacity(node_size, 2));
        let probe: Vec<ChunkKey> = rest[..probe_len.max(1)].iter().map(|e| e.0).collect();
        let mut s = choose_ksuf(&probe).unwrap_or(CHUNKKEY_BYTES as u8);

        loop {
            let mut b = LeafBuilder::new(node_size, s)?;
            let mut n = 0usize;
            for (k, v) in rest {
                if !b.push(*k, *v)? {
                    break;
                }
                n += 1;
            }
            if n > 0 {
                return Ok((b.seal()?, n));
            }
            // Not even one entry fit: widen. Terminates because width 14 always
            // admits any single key.
            let widths = crate::index::node::KSUF_WIDTHS;
            match widths.iter().copied().find(|&w| w > s) {
                Some(next) => s = next,
                None => return Err(CodecError::Invariant("no suffix width admits this key")),
            }
        }
    }

    /// Build, reusing leaves whose contents are byte-for-byte unchanged.
    ///
    /// # Why this is not an optimization
    ///
    /// [`Tree::build`] writes a fresh node for every leaf, so the index write is
    /// `O(total chunks)` per checkpoint no matter how few keys changed. With
    /// payload writes now proportional to the delta, that made the index the
    /// dominant write cost — the ~800x amplification `ARCHITECTURE.md` names,
    /// and the reason the node size is 1 KiB rather than a page.
    ///
    /// A leaf whose entries are exactly what they were needs no new node: its
    /// page is immutable ( I2 ) and index nodes are never reclaimed, so the old
    /// `PageId` stays valid indefinitely. Internal levels are still rebuilt —
    /// they are roughly `1/fanout` of the leaf bytes, so reusing leaves alone
    /// removes almost all of the volume.
    ///
    /// # Resynchronisation
    ///
    /// Leaf boundaries shift when entries are inserted or removed, so a rebuilt
    /// leaf can consume a different number of entries than the one it replaces.
    /// The scan re-syncs by skipping previous leaves that start before the
    /// current position, so a local edit costs a few rebuilt leaves rather than
    /// the whole tree. In the worst case nothing matches and this degrades
    /// exactly to `build`.
    pub fn build_reusing<S: NodeWriter + NodeReader>(
        sink: &mut S,
        node_size: usize,
        prev: Option<Tree>,
        entries: &[(ChunkKey, ChunkRef)],
        reused_out: &mut Vec<PageId>,
    ) -> Result<Option<Tree>> {
        if entries.is_empty() {
            return Ok(None);
        }
        if entries.windows(2).any(|p| p[0].0 >= p[1].0) {
            return Err(CodecError::Invariant("tree input must strictly ascend"));
        }

        // A different node size means a different geometry; nothing is reusable.
        // Read the old layout first, then write. One `&mut` sink serves both
        // roles, so the borrows have to be sequential rather than overlapping.
        let prev_leaves = match prev {
            Some(t) if t.node_size == node_size => t.leaves(&*sink)?,
            _ => Vec::new(),
        };
        Self::build_from(sink, node_size, &prev_leaves, entries, reused_out)
    }

    /// Rebuild from an already-loaded previous layout.
    ///
    /// Split out so [`Tree::build_updating`] can reuse the previous leaves it
    /// must load anyway, instead of the caller materializing the same entries a
    /// second time.
    fn build_from<S: NodeWriter + NodeReader>(
        sink: &mut S,
        node_size: usize,
        prev_leaves: &[LeafSnapshot],
        entries: &[(ChunkKey, ChunkRef)],
        reused_out: &mut Vec<PageId>,
    ) -> Result<Option<Tree>> {
        if entries.is_empty() {
            return Ok(None);
        }
        let mut level: Vec<(ChunkKey, PageId)> = Vec::new();
        let mut i = 0usize;
        let mut pj = 0usize;
        while i < entries.len() {
            // Skip previous leaves that begin before where we are now.
            while pj < prev_leaves.len()
                && prev_leaves[pj]
                    .1
                    .first()
                    .is_none_or(|(k, _)| *k < entries[i].0)
            {
                pj += 1;
            }

            if let Some((pid, pents)) = prev_leaves.get(pj) {
                let n = pents.len();
                // Exact match on keys *and* references. A key whose ChunkRef
                // moved must not reuse the leaf that still names the old extent.
                if n > 0 && i + n <= entries.len() && entries[i..i + n] == pents[..] {
                    level.push((entries[i].0, *pid));
                    reused_out.push(*pid);
                    i += n;
                    pj += 1;
                    continue;
                }
            }

            // Rebuild, but stop at the next previous-leaf boundary.
            //
            // Without this cap the rebuilt leaf packs greedily, ends somewhere
            // the old layout never had a boundary, and every leaf after it is
            // therefore misaligned and rebuilt too — one changed key cascades
            // through the whole remainder of the tree. Capping restores
            // alignment on the very next leaf, at the cost of leaving this one
            // slightly under-full.
            let limit = prev_leaves
                .get(pj + 1)
                .and_then(|(_, e)| e.first())
                .map(|(boundary, _)| entries[i..].partition_point(|(k, _)| k < boundary).max(1))
                .unwrap_or(entries.len() - i);
            let upto = (i + limit).min(entries.len());

            // Rebuild `[i, upto)` as however many leaves it takes, ending
            // exactly on the old boundary. Emitting a single greedy leaf is not
            // enough: an *insertion* makes the range one entry longer than the
            // leaf it replaces, and a nearly full leaf then spills, misaligning
            // every boundary after it. Splitting inside the range keeps the
            // damage local — which is the whole point.
            while i < upto {
                let (bytes, consumed) = Self::pack_leaf(node_size, &entries[i..upto])?;
                let id = sink.append_node(&bytes)?;
                level.push((entries[i].0, id));
                i += consumed;
            }
        }

        let mut height = 1u8;
        let cap = internal_capacity(node_size);
        while level.len() > 1 {
            let mut next = Vec::with_capacity(level.len().div_ceil(cap));
            for group in level.chunks(cap) {
                let bytes = build_internal(node_size, group)?;
                let id = sink.append_node(&bytes)?;
                next.push((group[0].0, id));
            }
            level = next;
            height += 1;
        }
        Ok(Some(Tree {
            root: level[0].1,
            height,
            node_size,
        }))
    }

    /// Rebuild the index from the previous tree plus this checkpoint's delta,
    /// without the caller materializing the whole key space.
    ///
    /// # Why this exists
    ///
    /// [`Tree::build_reusing`] needs a complete, sorted slice of every entry
    /// the new tree will hold, so its caller used to produce one by iterating
    /// the previous tree and collecting it. That made **two** full passes over
    /// the key space per checkpoint -- the caller's `iter().collect()` and this
    /// module's own `leaves()` -- and both ran inside the checkpoint's exclusive
    /// region. Measured 2026-09-15 at 80 000 resident entries, the caller's pass
    /// and the classification loop that followed it were **76%** of the
    /// exclusive hold, against **17%** for building the tree.
    ///
    /// The previous leaves already hold every carried entry, so the caller's
    /// copy was redundant. This function walks the previous leaves itself and
    /// **decodes only the ones a key falls in**: [`Tree::leaf_spans`] gets each
    /// leaf's key range from its header ( `key_at` is `O( 1 )` ), a leaf with no
    /// superseded key and no changed key in range is reused by page id with its
    /// entries never parsed, and a touched leaf is decoded, merged and repacked
    /// within its own range.
    ///
    /// # What it costs, which depends on write layout rather than write volume
    ///
    /// `O( leaves + entries in touched leaves )`. **That degrades to
    /// `O( total entries )` when every leaf is touched** -- which is what a
    /// write set strided across the key space does, and is why the measured
    /// gain moves with the *shape* of the writes and not their number: at
    /// 80 000 resident entries and 1 000 dirty, contiguous writes took the
    /// exclusive hold from 7.09-9.60 ms to 0.99-1.74 ms, while the same count
    /// strided across the space barely moved it.
    ///
    /// Leaf packing stays bottom-up and full, so node occupancy and the
    /// `( root, height )` snapshot shape are exactly as before. Making the cost
    /// `O( dirty )` in the *leaf count* as well needs path-copying with
    /// untouched **subtree** reuse, which trades leaf occupancy for it and is
    /// tracked separately.
    ///
    /// `changed` must strictly ascend. `superseded` names keys the new tree must
    /// not carry: those rewritten this checkpoint ( which reappear via `changed`
    /// ) and those deleted ( which do not ).
    pub fn build_updating<S: NodeWriter + NodeReader>(
        sink: &mut S,
        node_size: usize,
        prev: Option<Tree>,
        changed: &[(ChunkKey, ChunkRef)],
        superseded: &std::collections::BTreeSet<ChunkKey>,
        reused_out: &mut Vec<PageId>,
    ) -> Result<TreeUpdate> {
        if changed.windows(2).any(|p| p[0].0 >= p[1].0) {
            return Err(CodecError::Invariant("tree input must strictly ascend"));
        }
        let spans = match prev {
            Some(t) if t.node_size == node_size => t.leaf_spans(&*sink)?,
            _ => Vec::new(),
        };

        let mut level: Vec<(ChunkKey, PageId)> = Vec::new();
        let mut carried = 0u64;
        // Refs of dropped entries, collected where the merge already visits
        // them. Obtaining these with one `Tree::get` per touched key instead
        // cost ~0.7 us apiece -- 62% of the checkpoint's exclusive hold at
        // 10 000 touched keys, which made the whole path slower than the scan
        // it replaced past roughly 1 300.
        let mut superseded_refs: Vec<ChunkRef> = Vec::new();
        let mut ci = 0usize;

        for span in spans.iter() {
            // A leaf owns keys up to and including its own last key. A key
            // landing in the **gap** between two leaves therefore goes with the
            // *following* one, and a key past the final leaf goes to the
            // overflow below rather than being absorbed into it.
            //
            // Both of those match what the positional builder does, and neither
            // is arbitrary: absorbing an appended key into the last leaf would
            // rewrite a leaf that did not change, losing its page for no gain.
            let cstart = ci;
            while ci < changed.len() && changed[ci].0 <= span.last {
                ci += 1;
            }
            let cslice = &changed[cstart..ci];

            // Cheap necessary condition. `superseded` may name keys this leaf
            // does not hold -- a delete of a key that was never written -- so a
            // hit here means "decode and look", not "rebuild".
            let maybe_dropped = superseded.range(span.first..=span.last).next().is_some();

            if cslice.is_empty() && !maybe_dropped {
                // Untouched: reused by page id, entries never decoded. This is
                // the case the whole `leaf_spans` split exists for.
                level.push((span.first, span.id));
                reused_out.push(span.id);
                carried += span.len as u64;
                continue;
            }

            let pents = Self::leaf_entries(&*sink, span.id)?;
            let mut merged: Vec<(ChunkKey, ChunkRef)> =
                Vec::with_capacity(pents.len() + cslice.len());
            let mut dropped_here = 0usize;
            let mut cj = 0usize;
            for (k, r) in pents.iter().copied() {
                if superseded.contains(&k) {
                    superseded_refs.push(r);
                    dropped_here += 1;
                    continue;
                }
                while cj < cslice.len() && cslice[cj].0 < k {
                    merged.push(cslice[cj]);
                    cj += 1;
                }
                merged.push((k, r));
            }
            while cj < cslice.len() {
                merged.push(cslice[cj]);
                cj += 1;
            }

            // The necessary condition fired but nothing actually moved, so this
            // leaf is what a rebuild would emit. Reusing it keeps page reuse
            // **exactly** that of `build_reusing`, which compares content rather
            // than inferring from key ranges -- and page reuse is one of the
            // things the equivalence test pins.
            if cslice.is_empty() && dropped_here == 0 {
                level.push((span.first, span.id));
                reused_out.push(span.id);
                carried += span.len as u64;
                continue;
            }
            carried += (pents.len() - dropped_here) as u64;

            // Repack within this leaf's own range, so the boundary the next leaf
            // starts at is preserved and one changed key cannot cascade through
            // the rest of the tree.
            let mut i = 0usize;
            while i < merged.len() {
                let (bytes, consumed) = Self::pack_leaf(node_size, &merged[i..])?;
                let id = sink.append_node(&bytes)?;
                level.push((merged[i].0, id));
                i += consumed;
            }
        }

        // Keys beyond every existing leaf.
        let rest = &changed[ci..];
        let mut i = 0usize;
        while i < rest.len() {
            let (bytes, consumed) = Self::pack_leaf(node_size, &rest[i..])?;
            let id = sink.append_node(&bytes)?;
            level.push((rest[i].0, id));
            i += consumed;
        }

        if level.is_empty() {
            return Ok(TreeUpdate {
                tree: None,
                carried,
                superseded: superseded_refs,
            });
        }

        let mut height = 1u8;
        let cap = internal_capacity(node_size);
        while level.len() > 1 {
            let mut next = Vec::with_capacity(level.len().div_ceil(cap));
            for group in level.chunks(cap) {
                let bytes = build_internal(node_size, group)?;
                let id = sink.append_node(&bytes)?;
                next.push((group[0].0, id));
            }
            level = next;
            height += 1;
        }
        Ok(TreeUpdate {
            tree: Some(Tree {
                root: level[0].1,
                height,
                node_size,
            }),
            carried,
            superseded: superseded_refs,
        })
    }

    /// Every node in the tree: leaves and internal alike.
    ///
    /// The input to reclamation. A node not carried into the new tree is
    /// unreachable from the new root, so it can be queued for reuse — but the
    /// set to free is `old_nodes - reused_nodes`, **never** `old_nodes`, since
    /// `build_reusing` deliberately keeps some of the old pages alive.
    pub fn node_ids(&self, r: &impl NodeReader) -> Result<Vec<PageId>> {
        let mut out = Vec::new();
        Self::collect_nodes(r, self.root, &mut out)?;
        Ok(out)
    }

    fn collect_nodes(r: &impl NodeReader, id: PageId, out: &mut Vec<PageId>) -> Result<()> {
        out.push(id);
        let buf = r.node(id)?;
        if buf.as_slice().first() == Some(&NODE_LEAF) {
            return Ok(());
        }
        let node = InternalRef::parse(buf.as_slice())?;
        for ci in 0..node.child_count() {
            Self::collect_nodes(r, node.child(ci), out)?;
        }
        Ok(())
    }

    /// Each leaf's identity and key range, **without decoding its entries**.
    ///
    /// [`Tree::leaves`] parses every entry of every leaf into a `Vec`. That used
    /// to be the dominant cost of a checkpoint's exclusive region -- measured
    /// 2026-09-16 at 70-82% of the hold -- and almost all of it was thrown away,
    /// because a leaf no key touches is reused by page id and its entries are
    /// never read. `leaves` is no longer on the checkpoint path at all; it
    /// remains only for `build_reusing`, which is the differential oracle.
    ///
    /// A leaf's first key is in its header and `key_at` is `O( 1 )`, so a range
    /// costs two indexed reads rather than `nkeys` parses. The page itself is
    /// still read -- it must be, to learn the range -- but it is normally
    /// resident and its checksum already verified.
    fn leaf_spans(&self, r: &impl NodeReader) -> Result<Vec<LeafSpan>> {
        let mut ids = Vec::new();
        Self::collect_leaves(r, self.root, &mut ids)?;
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            let buf = r.node(id)?;
            let leaf = LeafRef::parse(buf.as_slice())?;
            let n = leaf.len();
            let (Some(first), Some(last)) = (leaf.key_at(0), leaf.key_at(n.saturating_sub(1)))
            else {
                // `pack_leaf` never emits an empty leaf; skip rather than
                // invent a range for one.
                continue;
            };
            out.push(LeafSpan {
                id,
                first,
                last,
                len: n,
            });
        }
        Ok(out)
    }

    /// The entries of one leaf, decoded on demand.
    fn leaf_entries(r: &impl NodeReader, id: PageId) -> Result<Vec<(ChunkKey, ChunkRef)>> {
        let buf = r.node(id)?;
        Ok(LeafRef::parse(buf.as_slice())?.iter().collect())
    }

    /// Every leaf in key order, with its contents.
    fn leaves(&self, r: &impl NodeReader) -> Result<Vec<LeafSnapshot>> {
        let mut ids = Vec::new();
        Self::collect_leaves(r, self.root, &mut ids)?;
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            let buf = r.node(id)?;
            out.push((id, LeafRef::parse(buf.as_slice())?.iter().collect()));
        }
        Ok(out)
    }

    /// Depth-first leaf ids, left to right. Depth is bounded by the tree height.
    fn collect_leaves(r: &impl NodeReader, id: PageId, out: &mut Vec<PageId>) -> Result<()> {
        let buf = r.node(id)?;
        if buf.as_slice().first() == Some(&NODE_LEAF) {
            out.push(id);
            return Ok(());
        }
        let node = InternalRef::parse(buf.as_slice())?;
        for ci in 0..node.child_count() {
            Self::collect_leaves(r, node.child(ci), out)?;
        }
        Ok(())
    }

    /// Look up one chunk.
    pub fn get(&self, r: &impl NodeReader, key: ChunkKey) -> Result<Option<ChunkRef>> {
        let leaf_id = self.descend(r, key)?;
        let buf = r.node(leaf_id)?;
        let leaf = LeafRef::parse(buf.as_slice())?;
        Ok(match leaf.search(key) {
            Ok(i) => leaf.value_at(i),
            Err(_) => None,
        })
    }

    /// Page id of the leaf that would hold `key`.
    fn descend(&self, r: &impl NodeReader, key: ChunkKey) -> Result<PageId> {
        let mut id = self.root;
        for _ in 1..self.height {
            let buf = r.node(id)?;
            let node = InternalRef::parse(buf.as_slice())?;
            id = node.child(node.child_for(key));
        }
        Ok(id)
    }

    /// Ordered scan of `[lo, hi)`.
    ///
    /// The hot path: all chunks of one key is `[key << 48, (key+1) << 48)`.
    pub fn range<'a, R: NodeReader>(&self, r: &'a R, lo: ChunkKey, hi: ChunkKey) -> Cursor<'a, R> {
        Cursor {
            r,
            tree: *self,
            hi,
            stack: Vec::new(),
            leaf: None,
            idx: 0,
            seek: Some(lo),
            done: false,
            cached: None,
        }
    }

    /// Every entry, ascending.
    pub fn iter<'a, R: NodeReader>(&self, r: &'a R) -> Cursor<'a, R> {
        self.range(r, ChunkKey(0), ChunkKey(u128::MAX))
    }
}

/// A range cursor over the tree.
///
/// # Why a stack and not a re-descend
///
/// With no sibling pointers, the tempting way to leave an exhausted leaf is to
/// descend again for `last_key + 1`. **That is wrong**, and silently so: if the
/// next leaf's first key is not exactly `last_key + 1` — which it never is when
/// the key space has gaps — the descent lands back in the leaf just finished and
/// the scan stops early, returning a prefix of the range with no error.
///
/// The cursor therefore keeps the path from the root as `(node, child_index)`
/// pairs. Advancing pops until some ancestor has an unvisited child, then
/// descends leftmost from it. Amortized one step per leaf, which is `1/fanout`
/// per entry.
pub struct Cursor<'a, R: NodeReader> {
    r: &'a R,
    tree: Tree,
    hi: ChunkKey,
    /// Path from the root: the internal node and which child was taken.
    stack: Vec<(PageId, usize)>,
    leaf: Option<PageId>,
    idx: usize,
    seek: Option<ChunkKey>,
    done: bool,
    /// The current leaf's bytes, held so a scan reads each leaf **once**.
    ///
    /// `step` used to call `NodeReader::node` for every entry, so a range scan
    /// re-fetched and re-parsed the same leaf once per item — at fanout 127
    /// that is 127 reads of one page. On the store that is an Arrow `Buffer`
    /// construction ( and an `ExtentGuard` `Arc` ) per entry, which put the
    /// design's own hot path — "one descent plus a cursor walk, zero read
    /// amplification" — at two allocations per chunk.
    cached: Option<(PageId, Page)>,
}

impl<R: NodeReader> Cursor<'_, R> {
    /// Position at the first entry `>= key`, recording the path.
    fn seek_to(&mut self, key: ChunkKey) -> Result<()> {
        self.stack.clear();
        let mut id = self.tree.root;
        loop {
            let buf = self.r.node(id)?;
            if buf.as_slice().first() == Some(&NODE_LEAF) {
                let leaf = LeafRef::parse(buf.as_slice())?;
                self.idx = match leaf.search(key) {
                    Ok(i) => i,
                    Err(i) => i,
                };
                self.leaf = Some(id);
                return Ok(());
            }
            let node = InternalRef::parse(buf.as_slice())?;
            let ci = node.child_for(key);
            self.stack.push((id, ci));
            id = node.child(ci);
        }
    }

    /// Move to the leftmost leaf after the current one. `false` when exhausted.
    fn next_leaf(&mut self) -> Result<bool> {
        while let Some((id, ci)) = self.stack.pop() {
            let nbuf = self.r.node(id)?;
            let node = InternalRef::parse(nbuf.as_slice())?;
            if ci + 1 < node.child_count() {
                self.stack.push((id, ci + 1));
                let mut child = node.child(ci + 1);
                loop {
                    let buf = self.r.node(child)?;
                    if buf.as_slice().first() == Some(&NODE_LEAF) {
                        self.leaf = Some(child);
                        self.idx = 0;
                        return Ok(true);
                    }
                    let n = InternalRef::parse(buf.as_slice())?;
                    self.stack.push((child, 0));
                    child = n.child(0);
                }
            }
        }
        Ok(false)
    }

    /// The bytes of leaf `id`, reading it only when it is not the cached one.
    fn leaf_bytes(&mut self, id: PageId) -> Result<Page> {
        if let Some((cached_id, buf)) = &self.cached {
            if *cached_id == id {
                return Ok(buf.clone());
            }
        }
        let buf = self.r.node(id)?;
        self.cached = Some((id, buf.clone()));
        Ok(buf)
    }

    fn step(&mut self) -> Result<Option<(ChunkKey, ChunkRef)>> {
        if self.done {
            return Ok(None);
        }
        if let Some(k) = self.seek.take() {
            self.seek_to(k)?;
        }
        loop {
            let Some(id) = self.leaf else {
                self.done = true;
                return Ok(None);
            };
            let lbuf = self.leaf_bytes(id)?;
            let leaf = LeafRef::parse(lbuf.as_slice())?;
            if self.idx < leaf.len() {
                let k = leaf.key_at(self.idx).expect("in range");
                if k >= self.hi {
                    self.done = true;
                    return Ok(None);
                }
                let v = leaf.value_at(self.idx).expect("in range");
                self.idx += 1;
                return Ok(Some((k, v)));
            }
            if !self.next_leaf()? {
                self.done = true;
                return Ok(None);
            }
        }
    }
}

impl<R: NodeReader> Iterator for Cursor<'_, R> {
    type Item = Result<(ChunkKey, ChunkRef)>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.step() {
            Ok(Some(x)) => Some(Ok(x)),
            Ok(None) => None,
            Err(e) => {
                self.done = true;
                Some(Err(e))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::INDEX_NODE;
    use crate::ContainerKind;

    fn val(i: u64) -> ChunkRef {
        ChunkRef::extent((i % 1000) * 64, ContainerKind::Array, (i % 4000 + 1) as u32).unwrap()
    }

    fn entries(keys: impl IntoIterator<Item = ChunkKey>) -> Vec<(ChunkKey, ChunkRef)> {
        keys.into_iter()
            .enumerate()
            .map(|(i, k)| (k, val(i as u64)))
            .collect()
    }

    fn build(e: &[(ChunkKey, ChunkRef)]) -> (VecNodes, Tree) {
        let mut n = VecNodes::new();
        let t = Tree::build(&mut n, INDEX_NODE, e)
            .unwrap()
            .expect("non-empty");
        (n, t)
    }

    /// `build_updating` must produce exactly what `build_reusing` produces
    /// from the equivalent fully-materialized input.
    ///
    /// # Why equivalence rather than assertions about the result
    ///
    /// `build_updating` exists to stop the caller materializing the whole key
    /// space, not to change what is built. Its merge is therefore the only new
    /// logic, and the sharpest statement about a merge is that it agrees with
    /// the concatenate-and-sort it replaces -- on the same **tree**, the same
    /// **height**, the same per-key lookups, and the same set of **reused**
    /// pages, since leaf reuse is what the whole exercise is protecting.
    ///
    /// The cases are chosen so a naive merge fails at least one: an update in
    /// place ( key present before and after ), a pure insert ( key absent
    /// before ), a pure delete ( superseded with no replacement ), and a delete
    /// of the **first** and **last** keys, which is where an off-by-one in the
    /// merge lands without disturbing anything else.
    #[test]
    fn build_updating_agrees_with_build_reusing() {
        let base: Vec<(ChunkKey, ChunkRef)> =
            entries((0..5_000u64).map(|i| ChunkKey::new(1, i * 2)));

        let cases: Vec<(&str, Vec<u64>, Vec<u64>)> = vec![
            ("update in place", vec![10, 400, 4000], vec![]),
            ("pure insert", vec![], vec![11, 401, 4001]),
            ("pure delete", vec![], vec![]),
            ("delete first and last", vec![], vec![]),
            ("mixed", vec![100, 102], vec![101, 103]),
            // Keys beyond every existing one. Without this the merge's trailing
            // drain is never reached, and deleting that loop entirely leaves
            // this test green -- which it did, on the first version.
            ("append past the end", vec![], vec![5_000, 5_001]),
            // A dense run of inserts, long enough to cross several leaf
            // boundaries. This is the case the leaf-ownership rule is about: a
            // key landing in the gap between two leaves must go with the
            // following one, and nothing shorter than a run is guaranteed to
            // produce a gap key at all.
            (
                "insert spanning leaf boundaries",
                vec![],
                (1_000..1_200).collect(),
            ),
            // Deleting a key that was never written. Its position falls
            // inside an existing leaf, so that leaf's range intersects
            // `superseded` and the cheap test says "maybe dropped" -- but
            // the leaf holds no such key, so nothing moves and it must
            // still be reused by page id.
            //
            // **No test in the tree reached that branch before**, measured
            // with a counter: 0. A fallback whose quiet state is the only
            // one ever observed is untested by construction.
            ("delete of a key never written", vec![], vec![]),
        ];

        for (i, (name, updates, inserts)) in cases.iter().enumerate() {
            // Deletions: case 2 drops a middle key, case 3 drops the extremes.
            let deletes: Vec<ChunkKey> = match i {
                2 => vec![ChunkKey::new(1, 200)],
                3 => vec![base[0].0, base[base.len() - 1].0],
                // Odd, so absent from a base of even keys, and low enough
                // to land inside a leaf rather than past the end.
                5 => vec![ChunkKey::new(1, 101)],
                _ => vec![],
            };

            let mut changed: Vec<(ChunkKey, ChunkRef)> = Vec::new();
            for &u in updates {
                changed.push((ChunkKey::new(1, u * 2), val(90_000 + u)));
            }
            for &s in inserts {
                changed.push((ChunkKey::new(1, s * 2 + 1), val(80_000 + s)));
            }
            changed.sort_by_key(|(k, _)| *k);

            let mut superseded: std::collections::BTreeSet<ChunkKey> =
                changed.iter().map(|(k, _)| *k).collect();
            superseded.extend(deletes.iter().copied());

            // The materialized equivalent: survivors merged with the delta.
            let mut merged: Vec<(ChunkKey, ChunkRef)> = base
                .iter()
                .filter(|(k, _)| !superseded.contains(k))
                .copied()
                .collect();
            merged.extend(changed.iter().copied());
            merged.sort_by_key(|(k, _)| *k);

            // `build` is deterministic, so two fresh base trees are identical
            // and their page ids are comparable between the two sides.
            let (mut na, prev_a) = build(&base);
            let mut ra = Vec::new();
            let a =
                Tree::build_reusing(&mut na, INDEX_NODE, Some(prev_a), &merged, &mut ra).unwrap();

            let (mut nb, prev_b) = build(&base);
            let mut rb = Vec::new();
            let upd = Tree::build_updating(
                &mut nb,
                INDEX_NODE,
                Some(prev_b),
                &changed,
                &superseded,
                &mut rb,
            )
            .unwrap();
            let (b, carried) = (upd.tree, upd.carried);

            // The dropped entries' old refs, which the caller queues for
            // reclamation. Gathered in the merge rather than by point lookup,
            // so they need their own assertion: losing one silently leaks an
            // extent, and nothing else in this test would notice.
            let mut want_superseded: Vec<ChunkRef> = base
                .iter()
                .filter(|(k, _)| superseded.contains(k))
                .map(|(_, r)| *r)
                .collect();
            let mut got_superseded = upd.superseded.clone();
            want_superseded.sort_unstable_by_key(|r| r.cell());
            got_superseded.sort_unstable_by_key(|r| r.cell());
            assert_eq!(
                got_superseded, want_superseded,
                "{name}: superseded refs must be exactly the dropped entries"
            );

            let (a, b) = (a.expect(name), b.expect(name));
            assert_eq!(a.height, b.height, "{name}: height");
            assert_eq!(
                carried as usize,
                base.len()
                    - superseded
                        .iter()
                        .filter(|k| base.iter().any(|(bk, _)| bk == *k))
                        .count(),
                "{name}: carried count"
            );
            // Reuse is asserted as a **superset**, not equality, and the
            // reason is a real difference rather than a concession.
            //
            // `build_reusing` finds unchanged leaves by comparing a positional
            // slice of the merged array against a previous leaf's entries. A
            // deletion shifts every later position by one, so its cursor skips
            // the leaf the deletion fell in and then mismatches the *next* leaf
            // against a range that begins inside the previous one -- rebuilding
            // a leaf that did not change. `build_updating` decides by key range,
            // which a shift does not perturb, so it keeps that leaf.
            //
            // Content equality is still asserted exactly, below and above: same
            // tree, height, per-key values, carried count and superseded refs.
            // Only *which pages were recycled* may differ, and only ever in the
            // direction of recycling more. Reusing a page that should have been
            // rebuilt would corrupt a lookup, and the per-key assertions are
            // what stand against that.
            ra.sort_unstable();
            rb.sort_unstable();
            // Record the direction as well, so a future change that silently
            // degrades to `build_reusing`'s positional matching is visible.
            if rb.len() > ra.len() {
                println!(
                    "  {name}: build_updating reused {} pages, build_reusing {}",
                    rb.len(),
                    ra.len()
                );
            }
            for id in &ra {
                assert!(
                    rb.contains(id),
                    "{name}: page {id} was reused by build_reusing but not by \
                     build_updating -- reuse must never be lost"
                );
            }

            for (k, v) in &merged {
                assert_eq!(b.get(&nb, *k).unwrap(), Some(*v), "{name}: key {k:?}");
            }
            for k in &deletes {
                assert_eq!(b.get(&nb, *k).unwrap(), None, "{name}: deleted {k:?}");
            }
        }
    }

    #[test]
    fn empty_input_yields_no_tree() {
        let mut n = VecNodes::new();
        assert!(Tree::build(&mut n, INDEX_NODE, &[]).unwrap().is_none());
        assert!(n.is_empty(), "an empty tree must not write a page");
    }

    #[test]
    fn single_leaf_roundtrip() {
        let e = entries((0..10u64).map(|i| ChunkKey::new(1, i)));
        let (n, t) = build(&e);
        assert_eq!(t.height, 1);
        for (k, v) in &e {
            assert_eq!(t.get(&n, *k).unwrap(), Some(*v));
        }
        assert_eq!(t.get(&n, ChunkKey::new(1, 999)).unwrap(), None);
        assert_eq!(t.get(&n, ChunkKey::new(2, 0)).unwrap(), None);
    }

    #[test]
    fn multi_level_tree_finds_every_key() {
        // Enough entries to force several internal levels at 1 KiB nodes.
        let e = entries((0..20_000u64).map(|i| ChunkKey::new(1, i)));
        let (n, t) = build(&e);
        assert!(
            t.height >= 3,
            "expected a multi-level tree, got height {}",
            t.height
        );

        for (k, v) in &e {
            assert_eq!(t.get(&n, *k).unwrap(), Some(*v), "missing {k:?}");
        }
        // And absent keys resolve to None rather than a neighbour.
        for i in 0..100u64 {
            assert_eq!(t.get(&n, ChunkKey::new(1, 20_000 + i)).unwrap(), None);
            assert_eq!(t.get(&n, ChunkKey::new(0, i)).unwrap(), None);
        }
    }

    #[test]
    fn iteration_returns_everything_in_order() {
        let e = entries((0..5_000u64).map(|i| ChunkKey::new(3, i * 7)));
        let (n, t) = build(&e);
        let got: Vec<(ChunkKey, ChunkRef)> = t.iter(&n).collect::<Result<Vec<_>>>().unwrap();
        assert_eq!(got, e, "full scan must reproduce the input exactly");
    }

    /// The hot path the whole index shape is chosen for.
    #[test]
    fn range_scan_covers_exactly_one_key() {
        let mut all = Vec::new();
        for key in 0..5u64 {
            for p in 0..500u64 {
                all.push(ChunkKey::new(key, p));
            }
        }
        let e = entries(all);
        let (n, t) = build(&e);

        for key in 0..5u64 {
            let got: Vec<ChunkKey> = t
                .range(&n, ChunkKey::range_start(key), ChunkKey::range_end(key))
                .map(|r| r.unwrap().0)
                .collect();
            assert_eq!(got.len(), 500, "key {key} scan returned {}", got.len());
            assert!(
                got.iter().all(|k| k.key() == key),
                "scan leaked into another key"
            );
            assert!(got.windows(2).all(|w| w[0] < w[1]), "scan out of order");
        }
    }

    #[test]
    fn range_scan_crosses_leaf_boundaries() {
        // Far more entries than one leaf holds, so the cursor must re-descend.
        let e = entries((0..3_000u64).map(|i| ChunkKey::new(9, i)));
        let (n, t) = build(&e);
        let got: Vec<ChunkKey> = t
            .range(&n, ChunkKey::new(9, 10), ChunkKey::new(9, 2_990))
            .map(|r| r.unwrap().0)
            .collect();
        assert_eq!(got.len(), 2_980);
        assert_eq!(got.first(), Some(&ChunkKey::new(9, 10)));
        assert_eq!(got.last(), Some(&ChunkKey::new(9, 2_989)));
    }

    /// Regression: a cursor that re-descends for `last_key + 1` instead of
    /// keeping a path stack silently truncates the scan.
    ///
    /// With gaps in the key space, `last_key + 1` is never the next leaf's first
    /// key, so the descent lands back in the leaf just finished and iteration
    /// stops — returning a *prefix* of the range with no error. Wide gaps here
    /// so every leaf boundary triggers it.
    #[test]
    fn scan_crosses_leaf_boundaries_when_keys_are_sparse() {
        let stride = 1_000_000u64;
        let n_keys = 4_000u64;
        let e = entries((0..n_keys).map(|i| ChunkKey::new(2, i * stride)));
        let (n, t) = build(&e);
        assert!(t.height >= 2, "test needs more than one leaf");

        let got: Vec<ChunkKey> = t.iter(&n).map(|r| r.unwrap().0).collect();
        assert_eq!(
            got.len(),
            n_keys as usize,
            "scan truncated at a leaf boundary: got {} of {n_keys}",
            got.len()
        );
        assert_eq!(got.first(), Some(&ChunkKey::new(2, 0)));
        assert_eq!(got.last(), Some(&ChunkKey::new(2, (n_keys - 1) * stride)));
        assert!(got.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn range_bounds_are_half_open() {
        let e = entries((0..100u64).map(|i| ChunkKey::new(1, i)));
        let (n, t) = build(&e);
        let got: Vec<ChunkKey> = t
            .range(&n, ChunkKey::new(1, 10), ChunkKey::new(1, 20))
            .map(|r| r.unwrap().0)
            .collect();
        assert_eq!(got.len(), 10);
        assert_eq!(got.first(), Some(&ChunkKey::new(1, 10)), "lo is inclusive");
        assert_eq!(got.last(), Some(&ChunkKey::new(1, 19)), "hi is exclusive");
    }

    #[test]
    fn empty_range_yields_nothing() {
        let e = entries((0..100u64).map(|i| ChunkKey::new(1, i)));
        let (n, t) = build(&e);
        assert_eq!(
            t.range(&n, ChunkKey::new(1, 50), ChunkKey::new(1, 50))
                .count(),
            0
        );
        assert_eq!(
            t.range(&n, ChunkKey::new(5, 0), ChunkKey::new(5, 99))
                .count(),
            0
        );
    }

    #[test]
    fn keys_spanning_many_user_keys_still_resolve() {
        // Forces wide suffix widths and cross-key leaves, the case widths 10/12
        // exist for.
        let e = entries((0..4_000u64).map(|i| ChunkKey::new(i, i % 7)));
        let (n, t) = build(&e);
        for (k, v) in &e {
            assert_eq!(t.get(&n, *k).unwrap(), Some(*v), "missing {k:?}");
        }
    }

    #[test]
    fn extreme_keys_are_representable() {
        let e = entries([
            ChunkKey::new(0, 0),
            ChunkKey::new(0, 1),
            ChunkKey::new(u64::MAX / 2, 12345),
            ChunkKey::new(u64::MAX, (1 << 48) - 2),
            ChunkKey::new(u64::MAX, (1 << 48) - 1),
        ]);
        let (n, t) = build(&e);
        for (k, v) in &e {
            assert_eq!(t.get(&n, *k).unwrap(), Some(*v));
        }
        let got: Vec<ChunkKey> = t.iter(&n).map(|r| r.unwrap().0).collect();
        assert_eq!(
            got.len(),
            e.len(),
            "cursor must terminate at the maximum key"
        );
    }

    #[test]
    fn build_rejects_unsorted_input() {
        let mut n = VecNodes::new();
        let bad = entries([ChunkKey::new(1, 5), ChunkKey::new(1, 5)]);
        assert!(
            Tree::build(&mut n, INDEX_NODE, &bad).is_err(),
            "duplicate keys"
        );
        let bad = entries([ChunkKey::new(1, 5), ChunkKey::new(1, 4)]);
        assert!(
            Tree::build(&mut n, INDEX_NODE, &bad).is_err(),
            "descending keys"
        );
    }

    #[test]
    fn cow_rebuild_leaves_the_old_root_intact() {
        // The property a snapshot depends on: building a new version must not
        // disturb pages the previous root reaches.
        let e1 = entries((0..2_000u64).map(|i| ChunkKey::new(1, i)));
        let (mut n, t1) = build(&e1);

        let e2 = entries((0..3_000u64).map(|i| ChunkKey::new(1, i)));
        let t2 = Tree::build(&mut n, INDEX_NODE, &e2).unwrap().unwrap();
        assert_ne!(t1.root, t2.root);

        // The old snapshot still reads its own data.
        for (k, v) in &e1 {
            assert_eq!(t1.get(&n, *k).unwrap(), Some(*v), "old root lost {k:?}");
        }
        assert_eq!(t1.iter(&n).count(), 2_000);
        assert_eq!(t2.iter(&n).count(), 3_000);
    }

    #[test]
    fn index_bytes_per_chunk_is_bounded() {
        // The cost model says the index dominates for sparse data, so keep an
        // eye on it: dense prefixes under one key should compress well.
        let e = entries((0..50_000u64).map(|i| ChunkKey::new(1, i)));
        let (n, _t) = build(&e);
        let per_chunk = n.bytes() as f64 / e.len() as f64;
        assert!(
            per_chunk < 16.0,
            "index costs {per_chunk:.2} B/chunk; prefix compression is not working"
        );
    }

    #[test]
    fn corrupt_page_id_errors_rather_than_panicking() {
        let e = entries((0..100u64).map(|i| ChunkKey::new(1, i)));
        let (n, mut t) = build(&e);
        t.root = 9999;
        assert!(t.get(&n, ChunkKey::new(1, 5)).is_err());
        assert!(t.iter(&n).next().unwrap().is_err());
    }

    #[test]
    fn internal_capacity_is_sane() {
        let cap = internal_capacity(INDEX_NODE);
        assert!(
            cap >= 50,
            "internal fanout {cap} is too low for a 1 KiB node"
        );
        // Height needed for 10^8 chunks must stay small.
        let leaf_cap = crate::index::node::leaf_capacity(INDEX_NODE, 2);
        let mut n = 100_000_000f64 / leaf_cap as f64;
        let mut h = 1;
        while n > 1.0 {
            n /= cap as f64;
            h += 1;
        }
        assert!(h <= 6, "height {h} for 10^8 chunks is too deep");
    }
}
