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

/// An immutable tree rooted at a single page.
///
/// Cheap to copy: a snapshot is exactly this value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tree {
    pub root: PageId,
    pub height: u8,
    pub node_size: usize,
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

    /// Look up one chunk.
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
