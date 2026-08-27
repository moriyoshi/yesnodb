//! B+tree node format with prefix-compressed leaf keys.
//!
//! # Truncation, not delta encoding
//!
//! The obvious way to compress sorted keys is delta encoding, and it is a trap:
//! comparing entry *i* would require decoding entries `0..i`, so binary search
//! degrades to a linear scan unless block skip pointers are added — and at a
//! 2-byte suffix width those pointers cost more than the deltas save.
//!
//! **Fixed-width truncated suffixes** get essentially the same compression on
//! this key distribution with O(1) random access, because a [`ChunkKey`] is
//! `key(64) || prefix48(48)` and within one leaf the `key` half is usually
//! constant while the `prefix48` values are dense.
//!
//! # Suffixes are big-endian
//!
//! The one deliberately big-endian part of the format. It makes byte-array
//! comparison identical to integer comparison at any width, so a probe is one
//! `from_be_bytes` plus one integer compare. This does not conflict with I1,
//! which governs *payload* endianness and zero-copy typed access — index keys
//! are never handed to Arrow.
//!
//! # Why `ksuf_len` has a value for "uncompressed"
//!
//! `ksuf_len == 14` *is* the uncompressed form, so compressed and uncompressed
//! leaves mix in one tree with no separate flag and no migration. The widths 10
//! and 12 are not padding of the set: a leaf spanning several user keys needs
//! the 6 bytes of `prefix48` plus the bytes separating adjacent keys, which is
//! 11-12 for realistic key counts. Omitting them would charge 14 bytes to the
//! majority of entries in a many-small-keys corpus.

use crate::error::{CodecError, Result};
use crate::store::checksum::crc32c_append;
use crate::store::extent::{ChunkKey, ChunkRef, CHUNKKEY_BYTES};

pub const NODE_LEAF: u8 = 1;
pub const NODE_INTERNAL: u8 = 2;
pub const VERSION: u8 = 1;

/// Bytes of node header before the key array.
pub const HEADER: usize = 32;

/// Legal truncated-suffix widths.
///
/// Odd widths are excluded to keep the ladder short: the gain from 9 over 10 is
/// one byte per entry, against a wider set of cases every reader has to handle.
///
/// **This comment used to claim that "every value except 14 fits the `<= 8` fast
/// path of a single zero-extended integer load", and that is arithmetic nobody
/// checked** — 10 and 12 are in this array and neither fits in 8 bytes. The
/// module header twenty lines above says plainly that both are needed for a
/// leaf spanning several user keys, so the file contradicted itself, and the
/// reader believed *this* half: [`LeafRef::search`] fell through to a `u64`
/// suffix load for every width below [`CHUNKKEY_BYTES`], which panicked at 10
/// and 12 and would have compared truncated keys had it not. Fixed 2026-09-14.
/// A width here is legal, reachable and must be handled to its full byte count;
/// there is no `<= 8` fast path and there never was one.
pub const KSUF_WIDTHS: [u8; 7] = [2, 4, 6, 8, 10, 12, 14];

const OFF_TYPE: usize = 0;
const OFF_VER: usize = 1;
const OFF_NKEYS: usize = 2;
const OFF_KSUF: usize = 4;
/// Offset of the stored CRC32C. Leaf and internal nodes share this framing,
/// which is what lets one checksum routine serve both.
pub const OFF_CRC: usize = 8;
const OFF_COMMON: usize = 12; // 14 bytes of full ChunkKey for entry 0
const OFF_KEYS: usize = HEADER;

/// Entry size for a leaf at a given suffix width: suffix plus an 8-byte value.
#[inline]
pub const fn leaf_entry_size(ksuf_len: u8) -> usize {
    ksuf_len as usize + 8
}

/// How many entries a leaf of `node_size` holds at `ksuf_len`.
#[inline]
pub const fn leaf_capacity(node_size: usize, ksuf_len: u8) -> usize {
    (node_size - HEADER) / leaf_entry_size(ksuf_len)
}

/// Narrowest legal width that distinguishes every key in `keys` from the first.
///
/// Returns `None` for an empty slice.
pub fn choose_ksuf(keys: &[ChunkKey]) -> Option<u8> {
    let first = *keys.first()?;
    let last = *keys.last()?;
    // The suffix must cover every byte in which any key differs from the first,
    // so find the highest-order differing byte across the range. Keys are sorted,
    // so comparing first against last is sufficient.
    let a = first.to_be_bytes();
    let b = last.to_be_bytes();
    let mut differ_at = CHUNKKEY_BYTES; // index of the first differing byte
    for i in 0..CHUNKKEY_BYTES {
        if a[i] != b[i] {
            differ_at = i;
            break;
        }
    }
    let need = CHUNKKEY_BYTES - differ_at; // bytes that must be retained
    KSUF_WIDTHS.iter().copied().find(|&w| w as usize >= need)
}

/// A leaf node laid out in a byte buffer.
pub struct LeafBuilder {
    node_size: usize,
    ksuf_len: u8,
    common: ChunkKey,
    keys: Vec<ChunkKey>,
    vals: Vec<ChunkRef>,
}

impl LeafBuilder {
    pub fn new(node_size: usize, ksuf_len: u8) -> Result<Self> {
        if !KSUF_WIDTHS.contains(&ksuf_len) {
            return Err(CodecError::Invariant("illegal ksuf_len"));
        }
        Ok(LeafBuilder {
            node_size,
            ksuf_len,
            common: ChunkKey(0),
            keys: Vec::new(),
            vals: Vec::new(),
        })
    }

    #[inline]
    pub fn capacity(&self) -> usize {
        leaf_capacity(self.node_size, self.ksuf_len)
    }

    /// Append an entry. Returns `false` if the leaf is full or the key would no
    /// longer fit the chosen suffix width — the caller seals and opens a new leaf.
    pub fn push(&mut self, key: ChunkKey, val: ChunkRef) -> Result<bool> {
        if let Some(&last) = self.keys.last() {
            if key <= last {
                return Err(CodecError::Invariant("leaf keys must strictly ascend"));
            }
        }
        if self.keys.len() >= self.capacity() {
            return Ok(false);
        }
        let first = *self.keys.first().unwrap_or(&key);
        if !fits_suffix(first, key, self.ksuf_len) {
            return Ok(false);
        }
        if self.keys.is_empty() {
            self.common = key;
        }
        self.keys.push(key);
        self.vals.push(val);
        Ok(true)
    }

    /// Serialize. The tail is zero-filled so the checksum is deterministic.
    pub fn seal(&self) -> Result<Vec<u8>> {
        if self.keys.is_empty() {
            return Err(CodecError::Invariant("cannot seal an empty leaf"));
        }
        let mut buf = vec![0u8; self.node_size];
        buf[OFF_TYPE] = NODE_LEAF;
        buf[OFF_VER] = VERSION;
        buf[OFF_NKEYS..OFF_NKEYS + 2].copy_from_slice(&(self.keys.len() as u16).to_le_bytes());
        buf[OFF_KSUF] = self.ksuf_len;
        buf[OFF_COMMON..OFF_COMMON + CHUNKKEY_BYTES].copy_from_slice(&self.common.to_be_bytes());

        let s = self.ksuf_len as usize;
        for (i, k) in self.keys.iter().enumerate() {
            let be = k.to_be_bytes();
            let off = OFF_KEYS + i * s;
            buf[off..off + s].copy_from_slice(&be[CHUNKKEY_BYTES - s..]);
        }
        let vals_off = OFF_KEYS + self.keys.len() * s;
        for (i, v) in self.vals.iter().enumerate() {
            let off = vals_off + i * 8;
            buf[off..off + 8].copy_from_slice(&v.to_bits().to_le_bytes());
        }

        let crc = checksum(&buf);
        buf[OFF_CRC..OFF_CRC + 4].copy_from_slice(&crc.to_le_bytes());
        Ok(buf)
    }
}

/// Can `key` be represented in a leaf whose first key is `first`, at width `s`?
#[inline]
fn fits_suffix(first: ChunkKey, key: ChunkKey, s: u8) -> bool {
    if s as usize >= CHUNKKEY_BYTES {
        return true;
    }
    let shift = 8 * s as u32;
    (first.0 >> shift) == (key.0 >> shift)
}

/// CRC32C over a whole node with its own checksum field read as zero.
///
/// Leaf and internal nodes are checksummed identically — same [`OFF_CRC`], same
/// whole-node extent — so `fsck` can verify a node without first deciding which
/// kind it is, which matters because deciding that means trusting the very
/// bytes under suspicion.
///
/// `node` must be at least [`HEADER`] bytes. [`verify_checksum`] is the entry
/// point that enforces it; this one is called from the builders, which own the
/// buffer they hand in.
pub fn checksum(node: &[u8]) -> u32 {
    let c = crc32c_append(0, &node[..OFF_CRC]);
    let c = crc32c_append(c, &[0, 0, 0, 0]);
    crc32c_append(c, &node[OFF_CRC + 4..])
}

/// Recompute a node's stored CRC32C and compare it.
///
/// # Why this is a report and not a repair
///
/// `slabmeta` documents the store's policy for a failed checksum: that region
/// is a **cache of derivable state**, so a mismatch falls back to recomputing
/// it from the index. An index node is the opposite end of that argument — it
/// *is* the authority, and the format deliberately carries no extent header to
/// derive it back from ( see `store::extent` ). So a node that fails here is
/// unrecoverable by any walk, and the only correct behaviour is to report it
/// and refuse to act on what it says. In particular `fsck` must not let a
/// rebuild that read a bad node be adopted, because adopting an incomplete
/// liveness map frees live data rather than merely failing to repair.
///
/// Never panics, including on a truncated buffer: a node too short to hold
/// its own header is an error, not a slice out of range.
pub fn verify_checksum(node: &[u8]) -> Result<()> {
    if node.len() < HEADER {
        return Err(CodecError::Truncated {
            expected: HEADER,
            found: node.len(),
        });
    }
    let stored = u32::from_le_bytes(node[OFF_CRC..OFF_CRC + 4].try_into().unwrap());
    if checksum(node) != stored {
        return Err(CodecError::Invariant("index node checksum mismatch"));
    }
    Ok(())
}

/// A parsed, borrowed leaf node.
pub struct LeafRef<'a> {
    buf: &'a [u8],
    nkeys: usize,
    ksuf_len: u8,
    common: ChunkKey,
}

impl<'a> LeafRef<'a> {
    /// Parse without verifying the checksum. Never panics on hostile input.
    pub fn parse(buf: &'a [u8]) -> Result<Self> {
        if buf.len() < HEADER {
            return Err(CodecError::Truncated {
                expected: HEADER,
                found: buf.len(),
            });
        }
        if buf[OFF_TYPE] != NODE_LEAF {
            return Err(CodecError::Invariant("not a leaf node"));
        }
        if buf[OFF_VER] != VERSION {
            return Err(CodecError::UnsupportedEncoding);
        }
        let ksuf_len = buf[OFF_KSUF];
        if !KSUF_WIDTHS.contains(&ksuf_len) {
            return Err(CodecError::Invariant("illegal ksuf_len"));
        }
        let nkeys = u16::from_le_bytes([buf[OFF_NKEYS], buf[OFF_NKEYS + 1]]) as usize;
        let need = OFF_KEYS + nkeys * leaf_entry_size(ksuf_len);
        if need > buf.len() {
            return Err(CodecError::Truncated {
                expected: need,
                found: buf.len(),
            });
        }
        let common = ChunkKey::from_be_bytes(
            buf[OFF_COMMON..OFF_COMMON + CHUNKKEY_BYTES]
                .try_into()
                .unwrap(),
        );
        Ok(LeafRef {
            buf,
            nkeys,
            ksuf_len,
            common,
        })
    }

    /// Parse **and** check the stored CRC32C.
    ///
    /// This used to say the production checksum check lived on the page-read
    /// path. It did not — nothing recomputed a node checksum anywhere, which is
    /// the gap `stored-page-crcs-are-not-verified` names. The integrity scan is
    /// now the caller that makes the claim true; see
    /// [`verify_checksum`], which is the kind-agnostic form `fsck` uses because
    /// it must check internal nodes too.
    #[cfg(test)]
    pub fn verify(buf: &'a [u8]) -> Result<Self> {
        let l = Self::parse(buf)?;
        verify_checksum(buf)?;
        Ok(l)
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.nkeys
    }

    /// Suffix at `i`, zero-extended into a `u128`.
    ///
    /// **The accumulator is `u128` because a suffix is not bounded by 64 bits.**
    /// [`KSUF_WIDTHS`] runs to [`CHUNKKEY_BYTES`] = 14, so widths 10 and 12 are
    /// legal, reachable and unrepresentable in a `u64`. This was a `u64` until
    /// 2026-09-14 and panicked at both of them — `8 - s` underflows in debug and
    /// wraps into an out-of-range slice index in release. See
    /// `leaf-search-truncated-suffixes-above-eight-bytes` in `JOURNAL.md`.
    #[inline]
    fn suffix_u128(&self, i: usize) -> u128 {
        let s = self.ksuf_len as usize;
        let off = OFF_KEYS + i * s;
        let mut b = [0u8; 16];
        b[16 - s..].copy_from_slice(&self.buf[off..off + s]);
        u128::from_be_bytes(b)
    }

    /// Full key at `i`, reassembled from the common prefix and the suffix.
    pub fn key_at(&self, i: usize) -> Option<ChunkKey> {
        if i >= self.nkeys {
            return None;
        }
        let s = self.ksuf_len as usize;
        let off = OFF_KEYS + i * s;
        let mut be = self.common.to_be_bytes();
        be[CHUNKKEY_BYTES - s..].copy_from_slice(&self.buf[off..off + s]);
        Some(ChunkKey::from_be_bytes(be))
    }

    pub fn value_at(&self, i: usize) -> Option<ChunkRef> {
        if i >= self.nkeys {
            return None;
        }
        let vals_off = OFF_KEYS + self.nkeys * self.ksuf_len as usize;
        let off = vals_off + i * 8;
        let bits = u64::from_le_bytes(self.buf[off..off + 8].try_into().unwrap());
        Some(ChunkRef::from_bits(bits))
    }

    /// Binary search **without decompressing the leaf**.
    ///
    /// The first step is what makes truncation correct: compare the *shared high
    /// part* before looking at any suffix. A target outside the leaf's prefix is
    /// resolved in two branches with no per-entry work — and, critically, its
    /// truncated suffix would otherwise compare as though it were inside.
    pub fn search(&self, target: ChunkKey) -> std::result::Result<usize, usize> {
        let s = self.ksuf_len as u32;
        if s as usize >= CHUNKKEY_BYTES {
            // Uncompressed: compare whole keys.
            return binary_search_by(self.nkeys, |i| self.key_at(i).unwrap().cmp(&target));
        }
        let shift = 8 * s;
        let t_hi = target.0 >> shift;
        let c_hi = self.common.0 >> shift;
        if t_hi < c_hi {
            return Err(0);
        }
        if t_hi > c_hi {
            return Err(self.nkeys);
        }
        // Masked and compared as `u128`. Narrowing to `u64` here would truncate
        // an 80- or 96-bit suffix at widths 10 and 12 — a silently wrong binary
        // search rather than a panic, which is the worse of the two failures and
        // the reason fixing only the underflow above would not have been a fix.
        let t_lo = target.0 & ((1u128 << shift) - 1);
        binary_search_by(self.nkeys, |i| self.suffix_u128(i).cmp(&t_lo))
    }

    pub fn iter(&self) -> impl Iterator<Item = (ChunkKey, ChunkRef)> + '_ {
        (0..self.nkeys).map(move |i| (self.key_at(i).unwrap(), self.value_at(i).unwrap()))
    }
}

/// `slice::binary_search_by` over an index range rather than a slice.
fn binary_search_by(
    n: usize,
    mut cmp: impl FnMut(usize) -> std::cmp::Ordering,
) -> std::result::Result<usize, usize> {
    let (mut lo, mut hi) = (0usize, n);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        match cmp(mid) {
            std::cmp::Ordering::Less => lo = mid + 1,
            std::cmp::Ordering::Greater => hi = mid,
            std::cmp::Ordering::Equal => return Ok(mid),
        }
    }
    Err(lo)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::INDEX_NODE;
    use crate::ContainerKind;

    fn val(i: u64) -> ChunkRef {
        ChunkRef::extent(i * 64, ContainerKind::Array, (i % 4000 + 1) as u32).unwrap()
    }

    fn build(keys: &[ChunkKey], ksuf: u8) -> Vec<u8> {
        let mut b = LeafBuilder::new(INDEX_NODE, ksuf).unwrap();
        for (i, &k) in keys.iter().enumerate() {
            assert!(b.push(k, val(i as u64)).unwrap(), "key {i} did not fit");
        }
        b.seal().unwrap()
    }

    #[test]
    fn roundtrip_at_every_legal_width() {
        for &s in &KSUF_WIDTHS {
            // Keys that differ only in the low `s` bytes.
            let keys: Vec<ChunkKey> = (0..20u64).map(|i| ChunkKey::new(7, i)).collect();
            let node = build(&keys, s);
            let leaf = LeafRef::verify(&node).unwrap();
            assert_eq!(leaf.len(), keys.len(), "width {s}");
            for (i, &k) in keys.iter().enumerate() {
                assert_eq!(leaf.key_at(i), Some(k), "width {s} entry {i}");
                assert_eq!(leaf.value_at(i), Some(val(i as u64)));
            }
        }
    }

    /// `search` at **every** legal width, not just the narrow ones.
    ///
    /// # Why this is a separate test from `roundtrip_at_every_legal_width`
    ///
    /// That test sweeps every width and calls only `key_at` / `value_at`;
    /// `search_finds_every_present_key_and_brackets_absent_ones` calls `search`
    /// and only at width 2. Neither crosses the other's axis, so the cell that
    /// actually broke — `search` at widths 10 and 12 — was covered by neither
    /// while looking thoroughly covered by both. `suffix_u128` was a `u64` until
    /// 2026-09-14 and this test panics against that version at the first width
    /// above 8, in debug by subtraction overflow and in release by a slice index.
    ///
    /// # Where the discriminating bits have to go, and why it is not obvious
    ///
    /// The keys must differ in the **top byte of the suffix window**, not merely
    /// somewhere inside it. A first version of this test spread them over the
    /// low 48 bits, which catches the underflow — any width above 8 panics
    /// before comparing anything — but **passes against a comparison truncated
    /// to 64 bits**, because keys that small are unchanged by the truncation.
    /// The silent half of the defect needs keys differing *above bit 64* while
    /// still sharing everything above `8*s`, which is the window bytes 8..`s`
    /// occupy and exactly where the peer report's `(3 << 56) | (0x10 << 20)`
    /// lands. `high` supplies a non-zero common prefix above the window so the
    /// `t_hi` / `c_hi` early-out is exercised rather than skipped.
    #[test]
    fn search_finds_every_present_key_at_every_legal_width() {
        for &s in &KSUF_WIDTHS {
            let bits = 8 * s as u32;
            // Step by the top byte of the window, so 20 keys differ across
            // `[bits - 8, bits)` -- above 64 for every width past 8.
            let step = 1u128 << (bits - 8);
            // A non-zero common prefix above the window, where one exists.
            let high = if bits < 8 * CHUNKKEY_BYTES as u32 {
                0xA5u128 << bits
            } else {
                0
            };
            let keys: Vec<ChunkKey> = (0..20u128).map(|i| ChunkKey(high | (i * step))).collect();
            let node = build(&keys, s);
            let leaf = LeafRef::verify(&node).unwrap();

            for (i, &k) in keys.iter().enumerate() {
                assert_eq!(leaf.search(k), Ok(i), "width {s}, present key {k:?}");
            }
            // An absent key between two present ones brackets correctly, which
            // a truncating comparison gets wrong without panicking.
            for (i, k) in keys.iter().enumerate().take(keys.len() - 1) {
                let missing = ChunkKey(k.0 + 1);
                assert_eq!(leaf.search(missing), Err(i + 1), "width {s}, absent {i}");
            }
        }
    }

    #[test]
    fn search_finds_every_present_key_and_brackets_absent_ones() {
        let keys: Vec<ChunkKey> = (0..40u64).map(|i| ChunkKey::new(3, i * 2)).collect();
        let node = build(&keys, 2);
        let leaf = LeafRef::verify(&node).unwrap();

        for (i, &k) in keys.iter().enumerate() {
            assert_eq!(leaf.search(k), Ok(i), "present key {k:?}");
        }
        // Absent keys land at the correct insertion point.
        for i in 0..40usize {
            let missing = ChunkKey::new(3, (i as u64) * 2 + 1);
            assert_eq!(leaf.search(missing), Err(i + 1));
        }
        assert_eq!(leaf.search(ChunkKey::new(3, 1000)), Err(keys.len()));
    }

    /// The step that makes truncation correct at all.
    #[test]
    fn keys_outside_the_common_prefix_resolve_without_false_matches() {
        // Leaf holds key=5 chunks only, compressed to 2-byte suffixes.
        let keys: Vec<ChunkKey> = (0..10u64).map(|i| ChunkKey::new(5, 100 + i)).collect();
        let node = build(&keys, 2);
        let leaf = LeafRef::verify(&node).unwrap();

        // A target under a *different* user key shares low bytes with entries
        // here. Without the high-part check it would false-match.
        let other = ChunkKey::new(4, 105);
        assert_eq!(
            leaf.search(other),
            Err(0),
            "lower key must sort before the leaf"
        );
        let higher = ChunkKey::new(6, 105);
        assert_eq!(
            leaf.search(higher),
            Err(leaf.len()),
            "higher key must sort after"
        );

        // And it must not be reported as found.
        assert!(leaf.search(other).is_err());
        assert!(leaf.search(higher).is_err());
    }

    #[test]
    fn push_refuses_a_key_that_does_not_fit_the_width() {
        let mut b = LeafBuilder::new(INDEX_NODE, 2).unwrap();
        assert!(b.push(ChunkKey::new(9, 1), val(0)).unwrap());
        // Same user key, close prefix: fits in 2 bytes.
        assert!(b.push(ChunkKey::new(9, 2), val(1)).unwrap());
        // Different user key: cannot be represented at width 2, so the builder
        // reports full rather than silently truncating.
        assert!(!b.push(ChunkKey::new(10, 2), val(2)).unwrap());
    }

    #[test]
    fn push_rejects_non_ascending_keys() {
        let mut b = LeafBuilder::new(INDEX_NODE, 6).unwrap();
        b.push(ChunkKey::new(1, 5), val(0)).unwrap();
        assert!(b.push(ChunkKey::new(1, 5), val(1)).is_err(), "duplicate");
        assert!(b.push(ChunkKey::new(1, 4), val(1)).is_err(), "descending");
    }

    #[test]
    fn choose_ksuf_picks_the_narrowest_that_works() {
        let dense: Vec<ChunkKey> = (0..50u64).map(|i| ChunkKey::new(7, i)).collect();
        assert_eq!(choose_ksuf(&dense), Some(2), "dense prefixes need 2 bytes");

        let wide: Vec<ChunkKey> = (0..50u64).map(|i| ChunkKey::new(7, i << 20)).collect();
        assert_eq!(choose_ksuf(&wide), Some(4));

        // Spanning user keys needs the prefix48 plus key-separating bytes; this
        // is the case the widths 10 and 12 exist for.
        let cross: Vec<ChunkKey> = (0..50u64).map(|i| ChunkKey::new(i, 3)).collect();
        let w = choose_ksuf(&cross).unwrap();
        assert!(w >= 8, "cross-key leaf needs a wide suffix, got {w}");
        assert!(KSUF_WIDTHS.contains(&w));

        assert_eq!(choose_ksuf(&[]), None);
    }

    #[test]
    fn chosen_width_always_admits_every_key() {
        // Whatever choose_ksuf returns must let the whole batch into one leaf,
        // or the bulk builder would loop.
        let batches: Vec<Vec<ChunkKey>> = vec![
            (0..30u64).map(|i| ChunkKey::new(7, i)).collect(),
            (0..30u64).map(|i| ChunkKey::new(7, i * 65_536)).collect(),
            (0..30u64).map(|i| ChunkKey::new(i, 0)).collect(),
            vec![ChunkKey::new(0, 0), ChunkKey::new(u64::MAX, (1 << 48) - 1)],
        ];
        for keys in batches {
            let s = choose_ksuf(&keys).unwrap();
            let mut b = LeafBuilder::new(4096, s).unwrap();
            for (i, &k) in keys.iter().enumerate() {
                assert!(
                    b.push(k, val(i as u64)).unwrap(),
                    "width {s} rejected key {i} of a batch it was chosen for"
                );
            }
        }
    }

    #[test]
    fn narrower_suffixes_raise_fanout() {
        // The reason prefix compression is in v1 at all.
        let cap2 = leaf_capacity(INDEX_NODE, 2);
        let cap6 = leaf_capacity(INDEX_NODE, 6);
        let cap14 = leaf_capacity(INDEX_NODE, 14);
        assert!(cap2 > cap6 && cap6 > cap14);
        // At 1 KiB nodes, a 2-byte suffix should roughly double the fanout over
        // uncompressed.
        assert!(
            cap2 as f64 / cap14 as f64 > 1.8,
            "cap2={cap2} cap14={cap14}"
        );
    }

    #[test]
    fn uncompressed_width_is_just_another_width() {
        // ksuf_len == 14 IS the uncompressed form, so mixed trees need no flag.
        let keys: Vec<ChunkKey> = vec![
            ChunkKey::new(1, 1),
            ChunkKey::new(500, 7),
            ChunkKey::new(u64::MAX, 9),
        ];
        let node = build(&keys, 14);
        let leaf = LeafRef::verify(&node).unwrap();
        for (i, &k) in keys.iter().enumerate() {
            assert_eq!(leaf.search(k), Ok(i));
        }
    }

    #[test]
    fn parse_rejects_garbage_without_panicking() {
        assert!(LeafRef::parse(&[]).is_err());
        assert!(LeafRef::parse(&[0u8; HEADER]).is_err(), "wrong node type");

        let keys: Vec<ChunkKey> = (0..5u64).map(|i| ChunkKey::new(1, i)).collect();
        let node = build(&keys, 2);

        let mut bad_ver = node.clone();
        bad_ver[OFF_VER] = 9;
        assert!(LeafRef::parse(&bad_ver).is_err());

        let mut bad_ksuf = node.clone();
        bad_ksuf[OFF_KSUF] = 3; // not in KSUF_WIDTHS
        assert!(LeafRef::parse(&bad_ksuf).is_err());

        // An nkeys that overruns the node must be caught, not indexed.
        let mut huge = node.clone();
        huge[OFF_NKEYS..OFF_NKEYS + 2].copy_from_slice(&u16::MAX.to_le_bytes());
        assert!(LeafRef::parse(&huge).is_err());

        for cut in 0..node.len() {
            let _ = LeafRef::parse(&node[..cut]);
        }
    }

    #[test]
    fn checksum_detects_corruption() {
        let keys: Vec<ChunkKey> = (0..8u64).map(|i| ChunkKey::new(2, i)).collect();
        let node = build(&keys, 2);
        LeafRef::verify(&node).unwrap();
        for &i in &[0usize, 12, HEADER, HEADER + 5, INDEX_NODE - 1] {
            let mut bad = node.clone();
            bad[i] ^= 0xFF;
            assert!(
                LeafRef::verify(&bad).is_err(),
                "corruption at {i} undetected"
            );
        }
    }

    #[test]
    fn sealing_an_empty_leaf_is_refused() {
        assert!(LeafBuilder::new(INDEX_NODE, 2).unwrap().seal().is_err());
    }

    #[test]
    fn illegal_width_is_rejected_at_construction() {
        for bad in [0u8, 1, 3, 9, 11, 13, 15, 16] {
            assert!(
                LeafBuilder::new(INDEX_NODE, bad).is_err(),
                "width {bad} must be illegal"
            );
        }
    }
}
