//! Container payload encode/decode, byte-identical to the portable Roaring spec.
//!
//! - Array: `2 * card` bytes, little-endian `u16` values, sorted and unique.
//! - Bitmap: exactly 8192 bytes, LSB-first little-endian.
//! - Run: `2 + 4 * nruns` bytes — a leading `nruns` LE `u16`, then
//!   `(start, len_minus_1)` LE `u16` pairs.
//!
//! Matching the spec byte for byte is what buys the differential test against
//! the `roaring` crate and `O(container count)` import of `.roaring` files.
//!
//! # The two decode paths do not have the same contract
//!
//! [`decode`] is the **untrusted-input boundary** — it is what
//! `roaring_format` calls on a foreign `.roaring` file — and it is a fuzz
//! target: for *any* input it must return `Err` or a container satisfying its
//! invariants, and must never panic. That obliges it to check *content*, not
//! just lengths. A payload can be structurally immaculate ( right length,
//! in-range cardinality ) and still describe a container that cannot exist: a
//! descending array, a bitmap whose stated cardinality is not its popcount,
//! overlapping runs, or a run reaching past the end of the chunk. All four
//! decoded to `Ok` until 2026-08-25, and the last one *panicked* on first
//! access, because [`RunContainer::end`] adds `start + len_minus_1` in `u16`.
//!
//! [`decode_buffer`] is the **internal mmap path**, reached only through
//! `db::store` on a page whose CRC has already been verified. Its job is to
//! avoid touching bytes at all, so it deliberately does *not* re-derive an
//! array's ordering or a bitmap's popcount — [`validate`] says why that would
//! defeat the point. It does perform the run checks, because that arm has to
//! walk every run to total the lengths regardless, making them free.
//!
//! Keep that asymmetry deliberate. If a future caller feeds untrusted bytes to
//! [`decode_buffer`], the checks it skips have to move, not be assumed.
//!
//! [`RunContainer::end`]: crate::container::RunContainer::end

use crate::buffer::{BitStore, U16Store};
use crate::container::{ArrayContainer, BitmapContainer, Container, ContainerKind, RunContainer};
use crate::error::{CodecError, Result};
use crate::{ARRAY_MAX, BITMAP_BYTES, CHUNK_CARD, RUN_DECODE_MAX};

/// Serialize a container's payload in Roaring spec form.
pub fn encode(c: &Container) -> Vec<u8> {
    match c {
        Container::Array(a) => a.vals.to_le_bytes(),
        Container::Bitmap(b) => b.bits.to_le_bytes(),
        Container::Run(r) => {
            let n = r.nruns();
            let mut out = Vec::with_capacity(2 + 4 * n as usize);
            out.extend_from_slice(&(n as u16).to_le_bytes());
            out.extend_from_slice(&r.runs.to_le_bytes());
            out
        }
    }
}

/// Decode a payload of a known kind.
///
/// `card` is the cardinality carried alongside in the index (`card_m1 + 1`); for
/// runs it is used only to cross-check the decoded intervals.
pub fn decode(kind: ContainerKind, bytes: &[u8], card: u32) -> Result<Container> {
    match kind {
        ContainerKind::Array => decode_array(bytes, card),
        ContainerKind::Bitmap => decode_bitmap(bytes, card),
        ContainerKind::Run => decode_run(bytes),
    }
}

fn decode_array(bytes: &[u8], card: u32) -> Result<Container> {
    if card == 0 || card > ARRAY_MAX as u32 {
        return Err(CodecError::BadCardinality(card));
    }
    let want = 2 * card as usize;
    if bytes.len() != want {
        return Err(CodecError::BadLength {
            kind: "array",
            len: bytes.len(),
            card,
        });
    }
    let store = U16Store::from_le_bytes(bytes);
    // Content check, not merely a length check — see the module note on the
    // untrusted-input boundary. Every kernel binary-searches an array, so a
    // descending or duplicated payload silently returns wrong answers.
    if store.as_slice().windows(2).any(|w| w[0] >= w[1]) {
        return Err(CodecError::Invariant("array not strictly ascending"));
    }
    Ok(Container::Array(ArrayContainer::from_store(store)))
}

fn decode_bitmap(bytes: &[u8], card: u32) -> Result<Container> {
    if card == 0 || card > CHUNK_CARD {
        return Err(CodecError::BadCardinality(card));
    }
    if bytes.len() != BITMAP_BYTES {
        return Err(CodecError::BadLength {
            kind: "bitmap",
            len: bytes.len(),
            card,
        });
    }
    let store = BitStore::from_le_bytes(bytes).ok_or(CodecError::BadLength {
        kind: "bitmap",
        len: bytes.len(),
        card,
    })?;
    // `card` arrives from the file, not from the payload. If it disagrees with
    // the popcount, every cardinality identity in `ops::card` silently returns a
    // wrong number, because they all trust the cached length.
    if store.count_ones() != card {
        return Err(CodecError::Invariant(
            "bitmap cardinality disagrees with popcount",
        ));
    }
    Ok(Container::Bitmap(BitmapContainer::from_store(store, card)))
}

fn decode_run(bytes: &[u8]) -> Result<Container> {
    if bytes.len() < 2 {
        return Err(CodecError::Truncated {
            expected: 2,
            found: bytes.len(),
        });
    }
    let nruns = u16::from_le_bytes([bytes[0], bytes[1]]) as u32;
    // Our writer caps at RUN_MAX_INTERVALS, but that is a size-class choice, not
    // a spec limit. Accept anything a conforming writer could emit so foreign
    // CRoaring files stay readable; `optimize()` normalizes later.
    if nruns == 0 || nruns > RUN_DECODE_MAX {
        return Err(CodecError::BadRunCount(nruns));
    }
    let want = 2 + 4 * nruns as usize;
    if bytes.len() != want {
        return Err(CodecError::BadLength {
            kind: "run",
            len: bytes.len(),
            card: nruns,
        });
    }

    let flat = U16Store::from_le_bytes(&bytes[2..]);
    let s = flat.as_slice();

    // Content validation. Summing the lengths is not enough on its own: a
    // payload can be perfectly well-formed structurally and still describe a
    // container that cannot exist.
    let mut len: u32 = 0;
    let mut prev_end: Option<u32> = None;
    let mut adjacent = false;
    for i in 0..nruns as usize {
        let start = s[i * 2] as u32;
        let end = start + s[i * 2 + 1] as u32;
        // `RunContainer::end` computes `start + len_minus_1` in `u16`, so a run
        // reaching past the chunk does not merely violate an invariant — it
        // panics on the first access in a debug build and wraps in release.
        if end > u16::MAX as u32 {
            return Err(CodecError::Invariant(
                "run extends past the end of the chunk",
            ));
        }
        if let Some(pe) = prev_end {
            if start <= pe {
                return Err(CodecError::Invariant("runs overlap or do not ascend"));
            }
            adjacent |= start == pe + 1;
        }
        prev_end = Some(end);
        len += end - start + 1;
    }
    if len > CHUNK_CARD {
        return Err(CodecError::Invariant("run cardinality exceeds chunk"));
    }

    if !adjacent {
        return Ok(Container::Run(RunContainer::from_store(flat, len)));
    }
    // Adjacency is the one defect worth repairing rather than rejecting. The
    // Roaring spec forbids *overlap*, not touching runs, so `[0,3],[4,7]` is
    // legal in a foreign file — but it is never legal state here, because every
    // run kernel assumes maximal runs. Rejecting would make conforming files
    // unreadable, which is exactly what this decoder's leniency policy exists to
    // avoid, so coalesce instead.
    let mut merged: Vec<u16> = Vec::with_capacity(s.len());
    for i in 0..nruns as usize {
        let (start, lm1) = (s[i * 2], s[i * 2 + 1]);
        if let Some(k) = merged.len().checked_sub(2) {
            let prev = merged[k] as u32 + merged[k + 1] as u32;
            if start as u32 == prev + 1 {
                // Bounded by the `end > u16::MAX` check above, so this fits.
                merged[k + 1] = (start as u32 + lm1 as u32 - merged[k] as u32) as u16;
                continue;
            }
        }
        merged.push(start);
        merged.push(lm1);
    }
    Ok(Container::Run(RunContainer::from_store(
        U16Store::from_vec(merged),
        len,
    )))
}

/// Decode directly from a buffer, **sharing its bytes where possible**.
///
/// This is the path that makes an mmap'd read zero-copy: the returned container
/// aliases `buf` rather than owning a copy, and the Arrow `Buffer` keeps the
/// mapping alive for as long as the container lives.
///
/// Falls back to copying when alignment does not permit sharing. That cannot
/// happen for extents this crate writes ( every slot is 64-byte aligned by
/// ladder invariant ) but can for a corrupt or foreign file, and returning a
/// correct copy beats panicking in `ScalarBuffer::new`.
pub fn decode_buffer(
    kind: ContainerKind,
    buf: &arrow_buffer::Buffer,
    off: usize,
    len: usize,
    card: u32,
) -> Result<Container> {
    if off + len > buf.len() {
        return Err(CodecError::OutOfBounds {
            off,
            len,
            buf_len: buf.len(),
        });
    }
    match kind {
        ContainerKind::Bitmap => {
            if card == 0 || card > CHUNK_CARD {
                return Err(CodecError::BadCardinality(card));
            }
            if len != BITMAP_BYTES {
                return Err(CodecError::BadLength {
                    kind: "bitmap",
                    len,
                    card,
                });
            }
            match BitStore::shared_from_bytes(buf, off) {
                Some(bits) => Ok(Container::Bitmap(BitmapContainer::from_store(bits, card))),
                None => decode(kind, &buf.as_slice()[off..off + len], card),
            }
        }
        ContainerKind::Array => {
            if card == 0 || card > ARRAY_MAX as u32 {
                return Err(CodecError::BadCardinality(card));
            }
            if len != 2 * card as usize {
                return Err(CodecError::BadLength {
                    kind: "array",
                    len,
                    card,
                });
            }
            match U16Store::try_shared_from_bytes(buf, off, len) {
                Some(vals) => Ok(Container::Array(ArrayContainer::from_store(vals))),
                None => decode(kind, &buf.as_slice()[off..off + len], card),
            }
        }
        ContainerKind::Run => {
            if len < 2 {
                return Err(CodecError::Truncated {
                    expected: 2,
                    found: len,
                });
            }
            let bytes = &buf.as_slice()[off..off + len];
            let nruns = u16::from_le_bytes([bytes[0], bytes[1]]) as u32;
            if nruns == 0 || nruns > RUN_DECODE_MAX || len != 2 + 4 * nruns as usize {
                return decode(kind, bytes, card);
            }
            // Share the pair array, skipping the spec-mandated nruns prefix.
            match U16Store::try_shared_from_bytes(buf, off + 2, len - 2) {
                Some(flat) => {
                    let s = flat.as_slice();
                    // The ordering and overflow checks ride along in the loop
                    // that has to run anyway to total the lengths, so they are
                    // free here — unlike the array and bitmap arms above, where
                    // an equivalent check would mean an extra O(n) pass over
                    // bytes this function exists to *avoid* touching.
                    let mut total = 0u32;
                    let mut prev_end: Option<u32> = None;
                    for i in 0..nruns as usize {
                        let start = s[i * 2] as u32;
                        let end = start + s[i * 2 + 1] as u32;
                        if end > u16::MAX as u32 {
                            return Err(CodecError::Invariant(
                                "run extends past the end of the chunk",
                            ));
                        }
                        if let Some(pe) = prev_end {
                            if start <= pe {
                                return Err(CodecError::Invariant("runs overlap or do not ascend"));
                            }
                            if start == pe + 1 {
                                // Coalescing needs an owned buffer, so hand off
                                // to the copying path rather than duplicating it.
                                return decode(kind, bytes, card);
                            }
                        }
                        prev_end = Some(end);
                        total += end - start + 1;
                    }
                    if total > CHUNK_CARD {
                        return Err(CodecError::Invariant("run cardinality exceeds chunk"));
                    }
                    Ok(Container::Run(RunContainer::from_store(flat, total)))
                }
                None => decode(kind, bytes, card),
            }
        }
    }
}

/// Full O(n) structural validation.
///
/// Deliberately *not* on the hot read path — validating every container on
/// every read would defeat zero-copy. Page-level checksums cover corruption;
/// this runs in debug builds, fuzz targets, and `Db::verify()`.
pub fn validate(c: &Container) -> Result<()> {
    match c {
        Container::Array(a) => {
            let s = a.as_slice();
            if s.is_empty() {
                return Err(CodecError::Invariant("empty container must not be stored"));
            }
            if s.len() > ARRAY_MAX {
                return Err(CodecError::Invariant("array exceeds ARRAY_MAX"));
            }
            if s.windows(2).any(|w| w[0] >= w[1]) {
                return Err(CodecError::Invariant("array not strictly ascending"));
            }
            Ok(())
        }
        Container::Bitmap(b) => {
            if b.is_empty() {
                return Err(CodecError::Invariant("empty container must not be stored"));
            }
            if b.len() != b.bits.count_ones() {
                return Err(CodecError::Invariant("bitmap cached len != popcount"));
            }
            Ok(())
        }
        Container::Run(r) => {
            let n = r.nruns();
            if n == 0 {
                return Err(CodecError::Invariant("empty container must not be stored"));
            }
            let mut total: u32 = 0;
            let mut prev_end: Option<u32> = None;
            for i in 0..n {
                let (s, e) = (r.start(i) as u32, r.end(i) as u32);
                if e < s {
                    return Err(CodecError::Invariant("run end before start"));
                }
                if let Some(pe) = prev_end {
                    // Non-adjacency: [0,3],[4,7] must have been coalesced.
                    if s <= pe + 1 {
                        return Err(CodecError::Invariant("runs overlap or are adjacent"));
                    }
                }
                prev_end = Some(e);
                total += e - s + 1;
            }
            if total != r.len() {
                return Err(CodecError::Invariant("run cached len != sum of intervals"));
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A run container whose `nruns` prefix is impossible is refused.
    ///
    /// # Why this needed writing
    ///
    /// `decode` is a **fuzz target by contract**: for any input it must return
    /// an error or a container satisfying its invariants, and never panic. A
    /// 2026-09-16 sweep over `CodecError` -- a bounded enum, so every variant
    /// is enumerable -- found `BadRunCount` had **never been constructed by any
    /// test**, here or in `tests/` or in `fuzz/`. The refusal that keeps the
    /// contract was itself unexercised.
    ///
    /// Both bounds are asserted beside the largest **accepted** value, because
    /// a test that only asserts rejection passes equally against a decoder that
    /// rejects everything.
    #[test]
    fn a_run_container_with_an_impossible_run_count_is_refused() {
        let hdr = |nruns: u16| -> Vec<u8> {
            let mut v = nruns.to_le_bytes().to_vec();
            v.extend(std::iter::repeat_n(0u8, 4 * nruns as usize));
            v
        };

        // Zero runs: a run container always holds at least one interval.
        assert!(matches!(
            decode_run(&hdr(0)),
            Err(CodecError::BadRunCount(0))
        ));

        // Above the decode ceiling.
        let over = (RUN_DECODE_MAX + 1) as u16;
        assert!(matches!(
            decode_run(&over.to_le_bytes()),
            Err(CodecError::BadRunCount(n)) if n == over as u32
        ));

        // The largest accepted count is not refused for *this* reason -- it
        // fails on length instead, which is the next check and a different one.
        let at_max = RUN_DECODE_MAX as u16;
        assert!(!matches!(
            decode_run(&at_max.to_le_bytes()),
            Err(CodecError::BadRunCount(_))
        ));

        // And one real interval decodes.
        assert!(decode_run(&hdr(1)).is_ok());
    }
    use crate::container::BitmapContainer;

    #[test]
    fn array_roundtrip() {
        let c = Container::from_sorted(&[1u16, 5, 9]);
        let bytes = encode(&c);
        assert_eq!(bytes, vec![1, 0, 5, 0, 9, 0]);
        let back = decode(ContainerKind::Array, &bytes, 3).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn bitmap_roundtrip() {
        let c = Container::Bitmap(BitmapContainer::from_sorted(&[0u16, 63, 64, 65535]));
        let bytes = encode(&c);
        assert_eq!(bytes.len(), BITMAP_BYTES);
        let back = decode(ContainerKind::Bitmap, &bytes, 4).unwrap();
        assert_eq!(back.len(), 4);
        assert_eq!(back.iter().collect::<Vec<_>>(), vec![0, 63, 64, 65535]);
    }

    #[test]
    fn run_payload_has_the_spec_mandated_nruns_prefix() {
        let c = Container::Run(RunContainer::from_pairs(&[(10, 20), (30, 30)]));
        let bytes = encode(&c);
        // 2 bytes nruns + 2 runs * 4 bytes
        assert_eq!(bytes.len(), 2 + 8);
        assert_eq!(&bytes[0..2], &2u16.to_le_bytes());
        // (start=10, len_minus_1=10), (start=30, len_minus_1=0)
        assert_eq!(&bytes[2..4], &10u16.to_le_bytes());
        assert_eq!(&bytes[4..6], &10u16.to_le_bytes());
        assert_eq!(&bytes[6..8], &30u16.to_le_bytes());
        assert_eq!(&bytes[8..10], &0u16.to_le_bytes());

        let back = decode(ContainerKind::Run, &bytes, 12).unwrap();
        assert_eq!(back, c);
        assert_eq!(back.len(), 12);
    }

    #[test]
    fn decode_rejects_bad_lengths_without_panicking() {
        assert!(decode(ContainerKind::Array, &[1, 0, 5], 3).is_err());
        assert!(decode(ContainerKind::Bitmap, &[0u8; 10], 1).is_err());
        assert!(decode(ContainerKind::Run, &[], 0).is_err());
        assert!(
            decode(ContainerKind::Run, &[5, 0], 0).is_err(),
            "nruns=5 with no payload"
        );
        assert!(
            decode(ContainerKind::Array, &[], 0).is_err(),
            "cardinality 0"
        );
    }

    #[test]
    fn decode_rejects_oversized_array() {
        let card = ARRAY_MAX as u32 + 1;
        let bytes = vec![0u8; 2 * card as usize];
        assert!(matches!(
            decode(ContainerKind::Array, &bytes, card),
            Err(CodecError::BadCardinality(_))
        ));
    }

    #[test]
    fn decode_accepts_foreign_run_counts_above_our_writer_cap() {
        // 3000 > RUN_MAX_INTERVALS (2032) but is legal in the spec.
        let n = 3000u32;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(n as u16).to_le_bytes());
        for i in 0..n {
            bytes.extend_from_slice(&((i as u16) * 20).to_le_bytes());
            bytes.extend_from_slice(&0u16.to_le_bytes());
        }
        let c = decode(ContainerKind::Run, &bytes, n).expect("foreign file must stay readable");
        assert_eq!(c.len(), n);
    }

    /// A misaligned extent must fall back to copying, not panic.
    ///
    /// Unreachable for extents this crate writes — every slot is 64-byte aligned
    /// by ladder invariant — but a corrupt index or a foreign file can point
    /// anywhere, and `ScalarBuffer::new` panics rather than erroring.
    #[test]
    fn decode_buffer_handles_a_misaligned_payload() {
        use arrow_buffer::Buffer;

        let vals: Vec<u16> = (0..300u16).map(|i| i * 7).collect();
        let c = Container::from_sorted(&vals);
        let payload = encode(&c);

        // Place the payload at an odd offset so a u16 view is impossible.
        let mut raw = vec![0u8; payload.len() + 1];
        raw[1..].copy_from_slice(&payload);
        let buf = Buffer::from_vec(raw);

        let back = decode_buffer(ContainerKind::Array, &buf, 1, payload.len(), 300)
            .expect("a misaligned payload must decode, not panic");
        assert_eq!(back.iter().collect::<Vec<_>>(), vals);
    }

    #[test]
    fn decode_buffer_shares_bytes_when_alignment_allows() {
        use arrow_buffer::Buffer;

        let vals: Vec<u16> = (0..5000u16).map(|i| i * 2).collect();
        let c = Container::Bitmap(BitmapContainer::from_sorted(&vals));
        let buf = Buffer::from_vec(encode(&c));

        let a = decode_buffer(ContainerKind::Bitmap, &buf, 0, BITMAP_BYTES, c.len()).unwrap();
        let b = decode_buffer(ContainerKind::Bitmap, &buf, 0, BITMAP_BYTES, c.len()).unwrap();
        // Two decodes of one aligned buffer must alias it, not copy it.
        let (Container::Bitmap(x), Container::Bitmap(y)) = (&a, &b) else {
            panic!("expected bitmaps");
        };
        assert_eq!(
            x.bits().to_boolean_buffer().values().as_ptr(),
            y.bits().to_boolean_buffer().values().as_ptr(),
            "aligned decodes must share the buffer"
        );
    }

    #[test]
    fn decode_buffer_rejects_out_of_bounds() {
        use arrow_buffer::Buffer;
        let buf = Buffer::from_vec(vec![0u8; 64]);
        assert!(decode_buffer(ContainerKind::Array, &buf, 0, 128, 64).is_err());
        assert!(decode_buffer(ContainerKind::Bitmap, &buf, 60, BITMAP_BYTES, 1).is_err());
    }

    #[test]
    fn validate_catches_structural_violations() {
        // Unsorted array.
        let bad = Container::Array(ArrayContainer::from_store(U16Store::from_vec(vec![5, 1])));
        assert!(validate(&bad).is_err());

        // Adjacent runs that should have been coalesced.
        let bad = Container::Run(RunContainer::from_pairs(&[(0, 3), (4, 7)]));
        assert!(validate(&bad).is_err());

        // Well-formed cases pass.
        assert!(validate(&Container::from_sorted(&[1, 2, 3])).is_ok());
        assert!(validate(&Container::Run(RunContainer::from_pairs(&[(0, 3), (5, 7)]))).is_ok());
    }

    #[test]
    fn validate_rejects_empty_containers() {
        assert!(validate(&Container::new_array()).is_err());
        assert!(validate(&Container::Run(RunContainer::new())).is_err());
    }

    /// `decode` is the untrusted-input boundary, and these four inputs are all
    /// structurally well-formed — correct lengths, in-range cardinalities — while
    /// describing containers that cannot exist. Before this was checked, every
    /// one of them decoded to `Ok`.
    ///
    /// The contract is stated in `CLAUDE.md`: for **any** input, `decode` must
    /// return `Err` or a container satisfying its invariants, and must never
    /// panic. `validate` is the definition of "satisfying its invariants", so
    /// `decode` returning something `validate` rejects is a contract violation
    /// however plausible the bytes look.
    #[test]
    fn decode_rejects_a_descending_array() {
        let mut b = Vec::new();
        for v in [9u16, 5, 1] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        // Every kernel binary-searches an array; a descending one returns wrong
        // answers rather than failing.
        assert!(decode(ContainerKind::Array, &b, 3).is_err());
    }

    #[test]
    fn decode_rejects_an_array_with_duplicates() {
        let mut b = Vec::new();
        for v in [1u16, 5, 5] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        assert!(
            decode(ContainerKind::Array, &b, 3).is_err(),
            "a set cannot hold a duplicate"
        );
    }

    #[test]
    fn decode_rejects_a_bitmap_whose_cardinality_disagrees_with_its_popcount() {
        let mut bytes = vec![0u8; BITMAP_BYTES];
        bytes[0] = 0xFF; // 8 bits set, but the file claims 5
        assert!(decode(ContainerKind::Bitmap, &bytes, 5).is_err());
    }

    #[test]
    fn decode_rejects_overlapping_runs() {
        // [10,15] then [12,15]: iteration would yield 12..15 twice, and the
        // cached length would count them twice.
        let b = run_bytes(&[(10, 5), (12, 3)]);
        assert!(decode(ContainerKind::Run, &b, 0).is_err());
    }

    #[test]
    fn decode_rejects_runs_that_do_not_ascend() {
        let b = run_bytes(&[(100, 0), (10, 0)]);
        assert!(decode(ContainerKind::Run, &b, 0).is_err());
    }

    /// The one that panicked rather than merely producing a bad container.
    ///
    /// `RunContainer::end` computes `start + len_minus_1` in `u16`, so a run
    /// reaching past the chunk overflowed on first access — a panic in debug,
    /// a silent wrap in release, reachable from any foreign `.roaring` file.
    #[test]
    fn decode_rejects_a_run_extending_past_the_chunk() {
        let b = run_bytes(&[(60_000, 10_000)]);
        let decoded = decode(ContainerKind::Run, &b, 0);
        assert!(decoded.is_err(), "60000 + 10000 > u16::MAX must not decode");
    }

    /// Leniency must survive the new strictness.
    ///
    /// The spec forbids overlapping runs, not *touching* ones, so `[0,3],[4,7]`
    /// is legal input from a conforming writer. It is still never legal state
    /// here, so it is coalesced rather than rejected.
    #[test]
    fn decode_coalesces_adjacent_runs_rather_than_rejecting_them() {
        let b = run_bytes(&[(0, 3), (4, 3)]);
        let c = decode(ContainerKind::Run, &b, 0).expect("adjacent runs are legal input");
        validate(&c).expect("but must not stay adjacent");
        assert_eq!(c.len(), 8);
        match &c {
            Container::Run(r) => assert_eq!(r.nruns(), 1, "the two runs must have merged"),
            other => panic!("expected a run, got {:?}", other.kind()),
        }
        assert_eq!(
            c.iter().collect::<Vec<u16>>(),
            (0u16..8).collect::<Vec<_>>()
        );
    }

    #[test]
    fn decode_coalesces_a_whole_chain_of_adjacent_runs() {
        let b = run_bytes(&[(0, 0), (1, 0), (2, 0), (3, 0)]);
        let c = decode(ContainerKind::Run, &b, 0).unwrap();
        validate(&c).unwrap();
        assert_eq!(c.len(), 4);
        assert_eq!(c.iter().collect::<Vec<u16>>(), vec![0, 1, 2, 3]);
    }

    /// A run ending exactly at the last ordinal is legal and must still decode.
    #[test]
    fn a_run_reaching_the_final_ordinal_is_accepted() {
        let b = run_bytes(&[(65_535, 0)]);
        let c = decode(ContainerKind::Run, &b, 0).expect("65535 is in range");
        validate(&c).unwrap();
        assert_eq!(c.iter().collect::<Vec<u16>>(), vec![65_535]);
    }

    /// Build a run payload: the spec-mandated `nruns` prefix, then
    /// `(start, len_minus_1)` pairs.
    fn run_bytes(runs: &[(u16, u16)]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&(runs.len() as u16).to_le_bytes());
        for &(s, l) in runs {
            b.extend_from_slice(&s.to_le_bytes());
            b.extend_from_slice(&l.to_le_bytes());
        }
        b
    }
}
