//! Packed pages: many small array and run payloads sharing one slot.
//!
//! Sparse data is the common case, not the tail. A term with 100k ordinals over
//! a 10^9 space produces ~15 259 chunks averaging ~6.5 ordinals — **13 bytes of
//! real payload each**. Giving every one of those its own extent means per-chunk
//! overhead dominates the data by an order of magnitude.
//!
//! # No per-chunk directory
//!
//! [`crate::store::extent::ChunkRef::cell`] points *directly* at a payload, so a point read is one
//! dereference with no group scan, and a payload's length is derivable from the
//! reference alone ( arrays and bitmaps outright; runs from their own
//! spec-mandated `nruns` prefix ). Nothing in the page describes its contents —
//! consistent with the rule that the index is the sole authority. Rebuild scans
//! the index range `[first_key, last_key]` the header records.
//!
//! # PA1: every packed payload length is even
//!
//! Array payloads are `2·card` and run payloads are `2 + 4·nruns`, so all are
//! even, and the payload region starts at an even offset. Every packed payload
//! is therefore **2-byte aligned by construction with zero padding bytes** —
//! which is all `ScalarBuffer<u16>` requires. An 8-byte rule would cost ~3 bytes
//! per chunk, 23% amplification on a 13-byte payload, and buy nothing.
//!
//! # The page is immutable after write
//!
//! There is deliberately no `live_bytes` counter in the header. An earlier
//! design decremented one as chunks were superseded — an in-place write to a
//! published, mapped page that live snapshots hold `Buffer`s into, which is
//! exactly the I2 violation this crate treats as its worst failure mode. Live
//! bytes are tracked in RAM by the allocator and recomputed by an index scan,
//! so the page is *structurally* incapable of violating I2 rather than merely
//! careful about it.

use super::checksum::crc32c_append;
use super::extent::{ChunkKey, CHUNKKEY_BYTES};
use crate::error::{CodecError, Result};

/// Bytes of header preceding the payload region.
pub const HEADER: usize = 40;

pub const MAGIC: u16 = 0x4E59; // "YN" little-endian
pub const VERSION: u8 = 1;

const OFF_MAGIC: usize = 0;
const OFF_VER: usize = 2;
const OFF_FLAGS: usize = 3;
const OFF_CRC: usize = 4;
const OFF_FIRST: usize = 8;
const OFF_LAST: usize = OFF_FIRST + CHUNKKEY_BYTES; // 22
const OFF_RESERVED: usize = OFF_LAST + CHUNKKEY_BYTES; // 36

/// Payload capacity of a page of `page_size` bytes.
#[inline]
pub const fn capacity(page_size: usize) -> usize {
    page_size - HEADER
}

/// Builds one packed page, reporting each payload's offset within it.
///
/// Payloads are appended in ascending [`ChunkKey`] order — the order the
/// checkpointer already writes in, which is also what makes the header's
/// `[first, last]` range a valid index-scan bound.
pub struct PackedPageBuilder {
    page_size: usize,
    buf: Vec<u8>,
    first: Option<ChunkKey>,
    last: Option<ChunkKey>,
}

impl PackedPageBuilder {
    pub fn new(page_size: usize) -> Self {
        assert!(page_size > HEADER, "page too small for a header");
        PackedPageBuilder {
            page_size,
            buf: vec![0u8; HEADER],
            first: None,
            last: None,
        }
    }

    /// Bytes still available for payloads.
    #[inline]
    pub fn remaining(&self) -> usize {
        self.page_size - self.buf.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.first.is_none()
    }

    /// Append a payload, returning its byte offset within the page.
    ///
    /// Fails if the payload has odd length ( violating PA1 ), if keys are not
    /// ascending, or if it does not fit — the caller seals the page and opens a
    /// new one on `None`.
    pub fn push(&mut self, key: ChunkKey, payload: &[u8]) -> Result<Option<usize>> {
        if !payload.len().is_multiple_of(2) {
            return Err(CodecError::Invariant(
                "PA1: packed payload length must be even",
            ));
        }
        if let Some(last) = self.last {
            if key <= last {
                return Err(CodecError::Invariant(
                    "packed page keys must strictly ascend",
                ));
            }
        }
        if payload.len() > self.remaining() {
            return Ok(None);
        }
        let off = self.buf.len();
        self.buf.extend_from_slice(payload);
        if self.first.is_none() {
            self.first = Some(key);
        }
        self.last = Some(key);
        Ok(Some(off))
    }

    /// Finish the page: zero-fill the tail, stamp the header, checksum.
    ///
    /// The tail is zeroed so the checksum is deterministic, and as a free
    /// canary: a stale pointer into free space decodes as an all-zero payload,
    /// which violates the sorted-unique invariant and surfaces as an error
    /// rather than as silent garbage.
    pub fn seal(mut self) -> Result<Vec<u8>> {
        let (Some(first), Some(last)) = (self.first, self.last) else {
            return Err(CodecError::Invariant("cannot seal an empty packed page"));
        };
        self.buf.resize(self.page_size, 0);

        self.buf[OFF_MAGIC..OFF_MAGIC + 2].copy_from_slice(&MAGIC.to_le_bytes());
        self.buf[OFF_VER] = VERSION;
        self.buf[OFF_FLAGS] = 0;
        self.buf[OFF_FIRST..OFF_FIRST + CHUNKKEY_BYTES].copy_from_slice(&first.to_be_bytes());
        self.buf[OFF_LAST..OFF_LAST + CHUNKKEY_BYTES].copy_from_slice(&last.to_be_bytes());
        self.buf[OFF_RESERVED..OFF_RESERVED + 4].fill(0);

        let crc = compute_crc(&self.buf);
        self.buf[OFF_CRC..OFF_CRC + 4].copy_from_slice(&crc.to_le_bytes());
        Ok(self.buf)
    }
}

/// Checksum over the whole page with the CRC field itself read as zero.
fn compute_crc(page: &[u8]) -> u32 {
    let c = crc32c_append(0, &page[..OFF_CRC]);
    let c = crc32c_append(c, &[0, 0, 0, 0]);
    crc32c_append(c, &page[OFF_CRC + 4..])
}

/// A parsed packed-page header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PackedHeader {
    pub version: u8,
    pub flags: u8,
    pub crc32c: u32,
    pub first: ChunkKey,
    pub last: ChunkKey,
}

impl PackedHeader {
    /// Parse and validate. Never panics on hostile input.
    pub fn parse(page: &[u8]) -> Result<Self> {
        if page.len() < HEADER {
            return Err(CodecError::Truncated {
                expected: HEADER,
                found: page.len(),
            });
        }
        let magic = u16::from_le_bytes([page[OFF_MAGIC], page[OFF_MAGIC + 1]]);
        if magic != MAGIC {
            return Err(CodecError::BadCookie(magic as u32));
        }
        let version = page[OFF_VER];
        if version != VERSION {
            return Err(CodecError::UnsupportedEncoding);
        }
        // A v1 reader cannot safely ignore an unknown flag.
        let flags = page[OFF_FLAGS];
        if flags != 0 {
            return Err(CodecError::UnsupportedEncoding);
        }
        let crc32c = u32::from_le_bytes(page[OFF_CRC..OFF_CRC + 4].try_into().unwrap());
        let first = ChunkKey::from_be_bytes(
            page[OFF_FIRST..OFF_FIRST + CHUNKKEY_BYTES]
                .try_into()
                .unwrap(),
        );
        let last = ChunkKey::from_be_bytes(
            page[OFF_LAST..OFF_LAST + CHUNKKEY_BYTES]
                .try_into()
                .unwrap(),
        );
        if last < first {
            return Err(CodecError::Invariant("packed page range is inverted"));
        }
        Ok(PackedHeader {
            version,
            flags,
            crc32c,
            first,
            last,
        })
    }

    /// Parse **and** recompute the stored page checksum.
    ///
    /// This said "called once per fault-in and memoized, not per read". That
    /// described an intent, not a caller: nothing recomputed this CRC anywhere,
    /// which is the gap `stored-page-crcs-are-not-verified` names. Its caller
    /// today is the integrity scan ( `fsck` ), which verifies **one CRC per
    /// page** rather than one per chunk in it — a page shared by 300 sparse
    /// chunks costs a single pass.
    ///
    /// The online read path still calls [`PackedHeader::parse`] and not this:
    /// a CRC over the whole page on every chunk read would roughly double the
    /// cost of a bitmap intersection. Making that affordable needs a
    /// once-per-faulted-page memo, and a memo needs a fault boundary to hang
    /// itself on — `SegmentedMmap` is the only thing that materializes file
    /// bytes, and its `write_at` is the only thing that changes them, so that
    /// is where such a cache belongs and what would invalidate it. Do not
    /// add the memo here: a per-page cache in a parsed header value has nowhere
    /// to live across reads.
    pub fn verify(page: &[u8]) -> Result<Self> {
        let h = Self::parse(page)?;
        if compute_crc(page) != h.crc32c {
            return Err(CodecError::Invariant("packed page checksum mismatch"));
        }
        Ok(h)
    }

    /// Does this page possibly hold `key`? An O(1) range check on the same
    /// cache line as the rest of the header, used to catch a reference landing
    /// in the wrong page.
    #[inline]
    pub fn may_contain(&self, key: ChunkKey) -> bool {
        key >= self.first && key <= self.last
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::PAGE;

    fn key(k: u64, p: u64) -> ChunkKey {
        ChunkKey::new(k, p)
    }

    #[test]
    fn build_seal_and_read_back() {
        let mut b = PackedPageBuilder::new(PAGE);
        let a_payload: Vec<u8> = vec![1, 0, 2, 0, 3, 0];
        let b_payload: Vec<u8> = vec![9, 0, 8, 0];

        let off_a = b.push(key(1, 10), &a_payload).unwrap().unwrap();
        let off_b = b.push(key(1, 11), &b_payload).unwrap().unwrap();
        assert_eq!(
            off_a, HEADER,
            "first payload sits immediately after the header"
        );
        assert_eq!(off_b, HEADER + a_payload.len());

        let page = b.seal().unwrap();
        assert_eq!(page.len(), PAGE);

        let h = PackedHeader::verify(&page).unwrap();
        assert_eq!(h.first, key(1, 10));
        assert_eq!(h.last, key(1, 11));
        // Sliced directly, as the read path does: `ChunkRef.cell` points at the
        // payload, so there is no per-chunk framing to go through.
        assert_eq!(&page[off_a..off_a + a_payload.len()], &a_payload[..]);
        assert_eq!(&page[off_b..off_b + b_payload.len()], &b_payload[..]);
    }

    #[test]
    fn pa1_every_payload_is_two_byte_aligned_with_no_padding() {
        let mut b = PackedPageBuilder::new(PAGE);
        let mut offsets = Vec::new();
        let mut expected = HEADER;
        for i in 0..50u64 {
            // Array payloads (2*card) and run payloads (2 + 4*nruns) are always even.
            let payload = vec![0xABu8; 2 * (i as usize % 7 + 1)];
            let off = b.push(key(1, i), &payload).unwrap().unwrap();
            assert_eq!(off % 2, 0, "payload at {off} is not 2-byte aligned");
            assert_eq!(off, expected, "padding was inserted between payloads");
            expected += payload.len();
            offsets.push(off);
        }
        assert!(!offsets.is_empty());
    }

    #[test]
    fn odd_length_payload_is_rejected() {
        let mut b = PackedPageBuilder::new(PAGE);
        let err = b.push(key(1, 0), &[1, 2, 3]).unwrap_err();
        assert!(
            matches!(err, CodecError::Invariant(_)),
            "PA1 must be enforced"
        );
    }

    #[test]
    fn keys_must_strictly_ascend() {
        let mut b = PackedPageBuilder::new(PAGE);
        b.push(key(1, 5), &[0, 0]).unwrap().unwrap();
        assert!(b.push(key(1, 5), &[0, 0]).is_err(), "duplicate key");
        assert!(b.push(key(1, 4), &[0, 0]).is_err(), "descending key");
        assert!(b.push(key(1, 6), &[0, 0]).is_ok());
    }

    #[test]
    fn push_reports_none_when_full_rather_than_erroring() {
        let mut b = PackedPageBuilder::new(PAGE);
        let big = vec![0u8; capacity(PAGE)];
        assert!(b.push(key(1, 0), &big).unwrap().is_some());
        // Caller seals and opens a new page on None.
        assert!(b.push(key(1, 1), &[0, 0]).unwrap().is_none());
    }

    #[test]
    fn capacity_matches_the_documented_figure() {
        // 4096 - 40 = 4056, the number the PACK_MAX analysis is built on.
        assert_eq!(capacity(PAGE), 4056);
    }

    #[test]
    fn tail_is_zero_filled_so_stale_pointers_decode_as_invalid() {
        let mut b = PackedPageBuilder::new(PAGE);
        b.push(key(1, 0), &[1, 0, 2, 0]).unwrap().unwrap();
        let page = b.seal().unwrap();
        assert!(
            page[HEADER + 4..].iter().all(|&x| x == 0),
            "unused bytes must be zero for a deterministic checksum and as a canary"
        );
    }

    #[test]
    fn checksum_detects_corruption_anywhere_in_the_page() {
        let mut b = PackedPageBuilder::new(PAGE);
        b.push(key(7, 3), &[1, 0, 2, 0, 3, 0]).unwrap().unwrap();
        let page = b.seal().unwrap();
        PackedHeader::verify(&page).unwrap();

        for &i in &[0usize, 8, 21, 39, HEADER, HEADER + 3, PAGE - 1] {
            let mut bad = page.clone();
            bad[i] ^= 0xFF;
            assert!(
                PackedHeader::verify(&bad).is_err(),
                "corruption at byte {i} went undetected"
            );
        }
    }

    #[test]
    fn range_check_catches_a_reference_to_the_wrong_page() {
        let mut b = PackedPageBuilder::new(PAGE);
        b.push(key(5, 10), &[0, 0]).unwrap().unwrap();
        b.push(key(5, 20), &[0, 0]).unwrap().unwrap();
        let page = b.seal().unwrap();
        let h = PackedHeader::verify(&page).unwrap();

        assert!(h.may_contain(key(5, 10)));
        assert!(h.may_contain(key(5, 15)));
        assert!(h.may_contain(key(5, 20)));
        assert!(!h.may_contain(key(5, 9)));
        assert!(!h.may_contain(key(5, 21)));
        assert!(!h.may_contain(key(4, 15)));
    }

    #[test]
    fn parse_rejects_garbage_without_panicking() {
        assert!(PackedHeader::parse(&[]).is_err());
        assert!(PackedHeader::parse(&[0u8; HEADER]).is_err(), "bad magic");

        let mut b = PackedPageBuilder::new(PAGE);
        b.push(key(1, 0), &[0, 0]).unwrap().unwrap();
        let page = b.seal().unwrap();

        let mut wrong_ver = page.clone();
        wrong_ver[OFF_VER] = 99;
        assert!(PackedHeader::parse(&wrong_ver).is_err());

        // An unknown flag must be refused, not ignored: a reader that skipped it
        // could return wrong data rather than an error.
        let mut wrong_flags = page.clone();
        wrong_flags[OFF_FLAGS] = 1;
        assert!(PackedHeader::parse(&wrong_flags).is_err());

        // Truncation at every prefix length must be handled.
        for cut in 0..HEADER {
            let _ = PackedHeader::parse(&page[..cut]);
        }
    }

    #[test]
    fn sealing_an_empty_page_is_refused() {
        assert!(PackedPageBuilder::new(PAGE).seal().is_err());
    }

    #[test]
    fn a_full_page_holds_hundreds_of_sparse_chunks() {
        // The whole point: ~13-byte payloads should amortize the 40-byte header
        // to well under a byte per chunk.
        let mut b = PackedPageBuilder::new(PAGE);
        let mut n = 0u64;
        while b.push(key(1, n), &[0u8; 14]).unwrap().is_some() {
            n += 1;
        }
        assert!(n > 250, "expected 250+ chunks per page, packed {n}");
        let overhead = HEADER as f64 / n as f64;
        assert!(
            overhead < 0.2,
            "header overhead {overhead:.3} B/chunk is too high"
        );
    }
}
