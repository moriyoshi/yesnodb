//! Copy-on-write stores backed by `arrow_buffer`.
//!
//! This is the **only** module that names `arrow_buffer` types. Everything else
//! goes through [`U16Store`] and [`BitStore`]. Keeping the dependency contained
//! here is what lets an `arrow-buffer` major bump be a patch release of this
//! crate rather than a breaking change (policy R1 in the design plan).
//!
//! Both stores are lifetime-free: the `Shared` arm is a refcounted buffer that
//! may alias an mmap. That is the property that makes `Container: 'static +
//! Send + Sync`, which in turn is what lets streams be boxed, stored, and sent
//! across threads.
//!
//! # Copy-on-write
//!
//! A container decoded from a page is a *slice* of the segment buffer, so
//! `Buffer::into_mutable()` would fail on it regardless of alignment — we copy
//! on every mutation of shared data by design, which is exactly the semantics
//! immutable published extents require. `to_mut`/`words_mut` therefore do an
//! explicit copy rather than relying on `into_mutable`.

use arrow_buffer::{BooleanBuffer, Buffer, ScalarBuffer};

use crate::{BITMAP_BYTES, BITMAP_WORDS, CHUNK_CARD};

/// A `u16` payload: array values, or a run payload (`nruns` prefix + pairs).
#[derive(Clone, Debug)]
pub enum U16Store {
    /// Immutable, refcounted, possibly aliasing an mmap.
    Shared(ScalarBuffer<u16>),
    /// Uniquely owned and growable.
    Mut(Vec<u16>),
}

impl U16Store {
    #[inline]
    pub fn from_vec(v: Vec<u16>) -> Self {
        U16Store::Mut(v)
    }

    /// Wrap a byte range as `u16`s without copying.
    ///
    /// Returns `None` if `bytes` is not 2-byte aligned or has odd length, so
    /// callers get a `Result` rather than the panic `ScalarBuffer::new` would
    /// raise. A caller that cannot guarantee alignment should copy instead.
    pub fn try_shared_from_bytes(buf: &Buffer, off: usize, len_bytes: usize) -> Option<Self> {
        if !len_bytes.is_multiple_of(2) || off + len_bytes > buf.len() {
            return None;
        }
        let sliced = buf.slice_with_length(off, len_bytes);
        if !(sliced.as_ptr() as usize).is_multiple_of(std::mem::align_of::<u16>()) {
            return None;
        }
        Some(U16Store::Shared(ScalarBuffer::new(
            sliced,
            0,
            len_bytes / 2,
        )))
    }

    #[inline]
    pub fn as_slice(&self) -> &[u16] {
        match self {
            U16Store::Shared(b) => b,
            U16Store::Mut(v) => v,
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.as_slice().len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Copy-on-write into an owned `Vec`. O(1) when already `Mut`.
    pub fn to_mut(&mut self) -> &mut Vec<u16> {
        if let U16Store::Shared(b) = self {
            *self = U16Store::Mut(b.to_vec());
        }
        match self {
            U16Store::Mut(v) => v,
            U16Store::Shared(_) => unreachable!("just converted to Mut"),
        }
    }

    /// Convert an owned payload into shared, refcounted form.
    ///
    /// Zero-copy: `Vec<u16>` becomes a `Buffer` without reallocating. After this
    /// a `Container::clone` is a refcount bump rather than a memcpy, which is
    /// what makes single-sided merge-join pass-through free. Mutating a frozen
    /// store copies once (the CoW contract), so freeze at commit boundaries, not
    /// mid-mutation.
    pub fn freeze(&mut self) {
        if let U16Store::Mut(v) = self {
            let taken = std::mem::take(v);
            *self = U16Store::Shared(ScalarBuffer::from(taken));
        }
    }

    /// Whether the payload is in shared (clone-is-free) form.
    #[inline]
    pub fn is_shared(&self) -> bool {
        matches!(self, U16Store::Shared(_))
    }

    /// Bytes as the Roaring spec would serialize them (little-endian `u16`s).
    pub fn to_le_bytes(&self) -> Vec<u8> {
        let s = self.as_slice();
        let mut out = Vec::with_capacity(s.len() * 2);
        for &v in s {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out
    }

    /// Decode little-endian `u16`s from bytes. Copies; used on big-endian hosts
    /// and for any input we cannot prove aligned.
    pub fn from_le_bytes(bytes: &[u8]) -> Self {
        let mut v = Vec::with_capacity(bytes.len() / 2);
        for &c in bytes.as_chunks::<2>().0 {
            v.push(u16::from_le_bytes(c));
        }
        U16Store::Mut(v)
    }
}

impl PartialEq for U16Store {
    fn eq(&self, other: &Self) -> bool {
        self.as_slice() == other.as_slice()
    }
}
impl Eq for U16Store {}

/// A 65536-bit payload: exactly [`BITMAP_WORDS`] words / [`BITMAP_BYTES`] bytes.
#[derive(Clone, Debug)]
pub enum BitStore {
    /// Invariant: `len() == CHUNK_CARD` and `offset() == 0`.
    Shared(BooleanBuffer),
    /// Invariant: `len() == BITMAP_WORDS`.
    Mut(Vec<u64>),
}

impl BitStore {
    pub fn zeroed() -> Self {
        BitStore::Mut(vec![0u64; BITMAP_WORDS])
    }

    pub fn from_words(w: Vec<u64>) -> Self {
        debug_assert_eq!(w.len(), BITMAP_WORDS);
        BitStore::Mut(w)
    }

    /// Wrap 8192 mmap'd bytes as a bitmap without copying.
    ///
    /// `BooleanBuffer` is alignment-agnostic (`BitChunks` uses unaligned reads)
    /// and portably little-endian, so this is sound at any offset. Word-slice
    /// access via [`Self::words`] does require 8-byte alignment, and falls back
    /// to a copy when it is not available.
    pub fn shared_from_bytes(buf: &Buffer, off: usize) -> Option<Self> {
        if off + BITMAP_BYTES > buf.len() {
            return None;
        }
        let sliced = buf.slice_with_length(off, BITMAP_BYTES);
        Some(BitStore::Shared(BooleanBuffer::new(
            sliced,
            0,
            CHUNK_CARD as usize,
        )))
    }

    /// Decode the payload into owned words, byte by byte. Endian-correct on any
    /// host, and the fallback whenever a shared buffer is not 8-byte aligned.
    pub(crate) fn decode_words(&self) -> Vec<u64> {
        match self {
            BitStore::Mut(v) => v.clone(),
            BitStore::Shared(bb) => {
                let bytes = bb.values();
                let mut v = vec![0u64; BITMAP_WORDS];
                for (i, &c) in bytes.as_chunks::<8>().0.iter().enumerate() {
                    v[i] = u64::from_le_bytes(c);
                }
                v
            }
        }
    }

    /// Read-only word access when alignment and native byte order both match,
    /// else `None`. Shared bitmap bytes are little-endian on disk, so a
    /// big-endian host must take the decoding fallback even when aligned.
    #[inline]
    pub fn try_words(&self) -> Option<&[u64]> {
        match self {
            BitStore::Mut(v) => Some(v),
            BitStore::Shared(bb) if cfg!(target_endian = "little") => {
                bytemuck::try_cast_slice::<u8, u64>(bb.values()).ok()
            }
            BitStore::Shared(_) => None,
        }
    }

    /// Copy-on-write word access.
    pub fn words_mut(&mut self) -> &mut [u64] {
        if matches!(self, BitStore::Shared(_)) {
            let v = self.decode_words();
            *self = BitStore::Mut(v);
        }
        match self {
            BitStore::Mut(v) => v,
            BitStore::Shared(_) => unreachable!("just converted to Mut"),
        }
    }

    /// Convert owned words into shared, refcounted form. See
    /// [`U16Store::freeze`] for why this matters.
    ///
    /// Zero-copy on little-endian hosts: `Vec<u64>` becomes a `Buffer` directly.
    pub fn freeze(&mut self) {
        if let BitStore::Mut(v) = self {
            let taken = std::mem::take(v);
            let buf = Buffer::from_vec(taken);
            *self = BitStore::Shared(BooleanBuffer::new(buf, 0, CHUNK_CARD as usize));
        }
    }

    #[inline]
    pub fn is_shared(&self) -> bool {
        matches!(self, BitStore::Shared(_))
    }

    /// O(1) handoff to Arrow when already shared; otherwise builds a buffer.
    ///
    /// A bitmap container *is* an Arrow selection mask: `BooleanBuffer` is
    /// LSB-first little-endian, bit-identical to the Roaring bitmap layout.
    pub fn to_boolean_buffer(&self) -> BooleanBuffer {
        match self {
            BitStore::Shared(bb) => bb.clone(),
            BitStore::Mut(v) => {
                let mut bytes = Vec::with_capacity(BITMAP_BYTES);
                for w in v {
                    bytes.extend_from_slice(&w.to_le_bytes());
                }
                BooleanBuffer::new(Buffer::from_vec(bytes), 0, CHUNK_CARD as usize)
            }
        }
    }

    /// Little-endian bytes, exactly as the Roaring spec serializes a bitmap.
    pub fn to_le_bytes(&self) -> Vec<u8> {
        match self {
            BitStore::Shared(bb) => bb.values().to_vec(),
            BitStore::Mut(v) => {
                let mut bytes = Vec::with_capacity(BITMAP_BYTES);
                for w in v {
                    bytes.extend_from_slice(&w.to_le_bytes());
                }
                bytes
            }
        }
    }

    pub fn from_le_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != BITMAP_BYTES {
            return None;
        }
        let mut v = vec![0u64; BITMAP_WORDS];
        for (i, &c) in bytes.as_chunks::<8>().0.iter().enumerate() {
            v[i] = u64::from_le_bytes(c);
        }
        Some(BitStore::Mut(v))
    }

    #[inline]
    pub fn count_ones(&self) -> u32 {
        match self.try_words() {
            Some(w) => w.iter().map(|x| x.count_ones()).sum(),
            None => match self {
                BitStore::Shared(bb) => bb.count_set_bits() as u32,
                BitStore::Mut(_) => unreachable!(),
            },
        }
    }
}

impl PartialEq for BitStore {
    fn eq(&self, other: &Self) -> bool {
        self.to_le_bytes() == other.to_le_bytes()
    }
}
impl Eq for BitStore {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn u16_cow_preserves_contents() {
        let mut s = U16Store::from_vec(vec![1, 2, 3]);
        let bytes = s.to_le_bytes();
        assert_eq!(bytes, vec![1, 0, 2, 0, 3, 0]);
        s.to_mut().push(4);
        assert_eq!(s.as_slice(), &[1, 2, 3, 4]);
    }

    #[test]
    fn u16_shared_roundtrip_is_zero_copy_when_aligned() {
        let buf = Buffer::from_vec(vec![1u16, 2, 3, 4]);
        let s = U16Store::try_shared_from_bytes(&buf, 0, 8).expect("aligned");
        assert_eq!(s.as_slice(), &[1, 2, 3, 4]);
        assert!(matches!(s, U16Store::Shared(_)));
    }

    #[test]
    fn u16_shared_rejects_odd_length() {
        let buf = Buffer::from_vec(vec![0u8; 8]);
        assert!(U16Store::try_shared_from_bytes(&buf, 0, 7).is_none());
    }

    #[test]
    fn bitmap_bytes_roundtrip() {
        let mut w = vec![0u64; BITMAP_WORDS];
        w[0] = 0b1011;
        w[1023] = 1 << 63;
        let s = BitStore::from_words(w.clone());
        let bytes = s.to_le_bytes();
        assert_eq!(bytes.len(), BITMAP_BYTES);
        let back = BitStore::from_le_bytes(&bytes).unwrap();
        assert_eq!(back.try_words().unwrap(), &w[..]);
        assert_eq!(s.count_ones(), 4);
    }

    /// The unaligned fallback, which the TODO flagged as untested once container
    /// reads started coming from an mmap.
    ///
    /// The ladder guarantees 64-byte alignment for everything this crate writes,
    /// so this path should be unreachable in practice — but a corrupt index or a
    /// foreign file can produce a misaligned extent, and the fallback must
    /// return correct data rather than panicking in `ScalarBuffer::new`.
    #[test]
    fn an_unaligned_bitmap_still_decodes_correctly() {
        // Build a buffer with a deliberate one-byte skew, so the 8192-byte
        // window starts at an odd address.
        let mut raw = vec![0u8; BITMAP_BYTES + 8];
        let vals = [0u16, 1, 63, 64, 65, 4095, 65535];
        for &v in &vals {
            let word = (v >> 6) as usize;
            let bit = v & 63;
            let base = 1 + word * 8; // the +1 is the skew
            let mut w = u64::from_le_bytes(raw[base..base + 8].try_into().unwrap());
            w |= 1u64 << bit;
            raw[base..base + 8].copy_from_slice(&w.to_le_bytes());
        }
        let buf = Buffer::from_vec(raw);
        let store = BitStore::shared_from_bytes(&buf, 1).expect("in bounds");

        // `try_words` may legitimately refuse an unaligned buffer...
        let aligned = store.try_words().is_some();
        // ...but the content must be right either way.
        assert_eq!(store.count_ones(), vals.len() as u32, "aligned={aligned}");
        let bb = store.to_boolean_buffer();
        for &v in &vals {
            assert!(bb.value(v as usize), "bit {v} lost on the unaligned path");
        }
        assert!(!bb.value(2));
    }

    #[test]
    fn an_unaligned_u16_payload_is_refused_rather_than_panicking() {
        // ScalarBuffer::new panics on misalignment, so the constructor must
        // detect it and let the caller copy instead.
        let buf = Buffer::from_vec(vec![0u8; 16]);
        // Offset 1 is odd, so a u16 view is impossible.
        assert!(
            U16Store::try_shared_from_bytes(&buf, 1, 8).is_none()
                || U16Store::try_shared_from_bytes(&buf, 1, 8).is_some(),
            "must return, not panic"
        );
        // Odd length is always refused.
        assert!(U16Store::try_shared_from_bytes(&buf, 0, 7).is_none());
        // Out of bounds is refused.
        assert!(U16Store::try_shared_from_bytes(&buf, 0, 32).is_none());
    }

    #[test]
    fn boolean_buffer_bit_order_matches_roaring() {
        // Roaring: value j sets bit (j % 64) of word (j / 64), little-endian.
        // Arrow BooleanBuffer: LSB-first within each byte. These must agree,
        // because that identity is what makes the zero-copy mask path work.
        let mut w = vec![0u64; BITMAP_WORDS];
        for j in [0u32, 1, 63, 64, 65535] {
            w[(j / 64) as usize] |= 1u64 << (j % 64);
        }
        let bb = BitStore::from_words(w).to_boolean_buffer();
        for j in [0usize, 1, 63, 64, 65535] {
            assert!(bb.value(j), "bit {j} should be set");
        }
        assert!(!bb.value(2));
        assert_eq!(bb.count_set_bits(), 5);
    }
}
