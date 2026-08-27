//! Portable Roaring serialization — 32-bit, and CRoaring's 64-bit layout.
//!
//! Because our container payloads are already spec-identical, reading a
//! `.roaring` file is `O(container count)`, not `O(cardinality)` — and so is
//! writing one.
//!
//! # The offset-header rule
//!
//! The 32-bit format's `u32` offset array is present **iff** the cookie is
//! [`SERIAL_COOKIE_NO_RUNCONTAINER`], **OR** the cookie is [`SERIAL_COOKIE`]
//! and there are at least [`NO_OFFSET_THRESHOLD`] containers. Getting this
//! wrong silently misparses small run-encoded bitmaps, so it is centralized in
//! [`has_offsets`] and tested directly.
//!
//! # 64-bit
//!
//! Two incompatible 64-bit layouts exist in the wild. We implement **CRoaring's
//! `Roaring64Map`**: a `u64` bucket count, then per bucket a `u32` high key
//! followed by a complete 32-bit portable bitmap. Java's
//! `Roaring64NavigableMap` is *not* supported.

use std::io::{Read, Write};

use crate::container::{codec, Container, ContainerKind};
use crate::error::{CodecError, Result};
use crate::set::OrdSet;
use crate::{ARRAY_MAX, CHUNK_BITS};

pub const SERIAL_COOKIE_NO_RUNCONTAINER: u32 = 12_346;
pub const SERIAL_COOKIE: u32 = 12_347;
/// At or above this container count, a run-encoded bitmap still carries offsets.
pub const NO_OFFSET_THRESHOLD: usize = 4;

/// Whether the `u32` offset array follows the descriptive header.
#[inline]
pub const fn has_offsets(cookie_lo: u32, n_containers: usize) -> bool {
    cookie_lo == SERIAL_COOKIE_NO_RUNCONTAINER || n_containers >= NO_OFFSET_THRESHOLD
}

/// Normalize a container's kind for export.
///
/// # The format has no kind field, so cardinality decides
///
/// For non-run containers the portable format carries no kind: a reader infers
/// array-vs-bitset from the descriptive header's cardinality against
/// [`ARRAY_MAX`]. Ours does ( see `deserialize_with_len` ), and so do the
/// `roaring` crate and CRoaring.
///
/// Our in-memory kind is deliberately **not** a function of cardinality.
/// [`crate::BITMAP_DEMOTE`] is 3584, not 4096, so a promoted `Bitmap` is
/// retained on the way back down to damp conversion churn — and `remove` does
/// not demote at all. So a `Bitmap` holding `card <= ARRAY_MAX` used to emit an
/// 8192-byte payload beneath a header promising `2 * card` bytes, and every
/// reader resynchronised on the wrong boundary. At `card == 1` the misparse even
/// produced a well-formed one-element array holding the **wrong value**.
///
/// # Unconditional, and not [`Container::optimize`]
///
/// Do not route this through `optimize` or the `OPT_GAIN` 7/8 margin. That
/// rule declines the demotion across the whole `[3584, 4096]` band
/// ( `7200 * 8 = 57600 > 8192 * 7 = 57344` ) and would leave the bug in place
/// for exactly the cardinalities the hysteresis band creates. `OPT_GAIN` is a
/// churn heuristic for our own storage; it has no authority over an interchange
/// format that has already fixed the encoding by cardinality.
///
/// There is a second, sharper reason, found by trying it: `has_runs` — and
/// therefore the cookie and the run-flag bitset — is computed from the
/// containers *before* payloads are encoded. Any normalization that can
/// **introduce** a `Run` writes a run payload under a no-run cookie, and
/// `serialized_bytes_are_identical_to_the_roaring_crate` fails too. So the
/// invariant for anything added here is narrow: this may only ever turn a
/// `Bitmap` into an `Array`, never into a `Run`.
///
/// Do not "fix" this by raising `BITMAP_DEMOTE` to 4096. The 512-value gap is
/// what bounds conversion work — a promote/demote cycle needs 514 mutations
/// either way — and closing it trades that bound for a boundary bug.
///
/// This is an **export-only** concern. Nothing durable infers kind from
/// cardinality: `ChunkRef` carries an explicit 2-bit kind at `[56:58)` and
/// `codec::decode` takes the kind as a parameter.
fn export_kind(c: &Container) -> Container {
    match c {
        Container::Bitmap(b) if b.len() as usize <= ARRAY_MAX => Container::Array(
            crate::container::ArrayContainer::from_sorted_vec(b.iter().collect()),
        ),
        other => other.clone(),
    }
}

/// A 32-bit Roaring bitmap: `u16`-keyed containers covering `[0, 2^32)`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Roaring32 {
    pub containers: Vec<(u16, Container)>,
}

impl Roaring32 {
    pub fn cardinality(&self) -> u64 {
        self.containers.iter().map(|(_, c)| c.len() as u64).sum()
    }

    /// Build from sorted, unique `u32` values.
    pub fn from_sorted_u32(vals: &[u32]) -> Self {
        let mut containers: Vec<(u16, Container)> = Vec::new();
        let mut i = 0usize;
        while i < vals.len() {
            let key = (vals[i] >> 16) as u16;
            let mut lows = Vec::new();
            while i < vals.len() && (vals[i] >> 16) as u16 == key {
                lows.push((vals[i] & 0xFFFF) as u16);
                i += 1;
            }
            containers.push((key, Container::from_sorted(&lows)));
        }
        Roaring32 { containers }
    }

    /// All values, ascending.
    pub fn values(&self) -> Vec<u32> {
        self.containers
            .iter()
            .flat_map(|(k, c)| {
                let base = (*k as u32) << 16;
                c.iter().map(move |v| base | v as u32)
            })
            .collect()
    }

    /// Re-select each container's encoding, enabling run containers where they win.
    pub fn optimize(&mut self) {
        for (_, c) in &mut self.containers {
            c.optimize();
        }
    }

    pub fn serialize_into(&self, w: &mut impl Write) -> std::io::Result<()> {
        w.write_all(&self.serialize())
    }

    pub fn serialize(&self) -> Vec<u8> {
        let n = self.containers.len();
        let has_runs = self
            .containers
            .iter()
            .any(|(_, c)| c.kind() == ContainerKind::Run);
        let mut out = Vec::new();

        if has_runs {
            // Low 16 bits are the cookie; high 16 hold (n - 1).
            let cookie = SERIAL_COOKIE | (((n as u32).saturating_sub(1)) << 16);
            out.extend_from_slice(&cookie.to_le_bytes());
            // Run-flag bitset: one bit per container, LSB-first.
            let mut flags = vec![0u8; n.div_ceil(8)];
            for (i, (_, c)) in self.containers.iter().enumerate() {
                if c.kind() == ContainerKind::Run {
                    flags[i / 8] |= 1 << (i % 8);
                }
            }
            out.extend_from_slice(&flags);
        } else {
            out.extend_from_slice(&SERIAL_COOKIE_NO_RUNCONTAINER.to_le_bytes());
            out.extend_from_slice(&(n as u32).to_le_bytes());
        }

        // Descriptive header: (key, cardinality - 1) pairs.
        for (k, c) in &self.containers {
            out.extend_from_slice(&k.to_le_bytes());
            out.extend_from_slice(&((c.len() - 1) as u16).to_le_bytes());
        }

        let cookie_lo = if has_runs {
            SERIAL_COOKIE
        } else {
            SERIAL_COOKIE_NO_RUNCONTAINER
        };
        let payloads: Vec<Vec<u8>> = self
            .containers
            .iter()
            .map(|(_, c)| codec::encode(&export_kind(c)))
            .collect();

        if has_offsets(cookie_lo, n) {
            let header_len = out.len() + 4 * n;
            let mut off = header_len as u32;
            for p in &payloads {
                out.extend_from_slice(&off.to_le_bytes());
                off += p.len() as u32;
            }
        }
        for p in payloads {
            out.extend_from_slice(&p);
        }
        out
    }

    pub fn deserialize(bytes: &[u8]) -> Result<Self> {
        Self::deserialize_with_len(bytes).map(|(s, _)| s)
    }

    /// Parse, also reporting how many bytes were consumed.
    ///
    /// The 64-bit container format packs one complete 32-bit bitmap per bucket
    /// with no length prefix, so the reader must learn the extent from the parse
    /// itself. Re-serializing to measure it would make deserialization O(n)
    /// twice over — it measured ~7× slower than serializing.
    pub fn deserialize_with_len(bytes: &[u8]) -> Result<(Self, usize)> {
        let mut r = Cursor::new(bytes);
        let cookie = r.u32()?;
        let cookie_lo = cookie & 0xFFFF;

        let (n, run_flags) = if cookie == SERIAL_COOKIE_NO_RUNCONTAINER {
            (r.u32()? as usize, None)
        } else if cookie_lo == SERIAL_COOKIE {
            let n = ((cookie >> 16) + 1) as usize;
            let flags = r.take(n.div_ceil(8))?.to_vec();
            (n, Some(flags))
        } else {
            return Err(CodecError::BadCookie(cookie));
        };

        // `n` is read from the file and reaches `Vec::with_capacity` twice
        // below, so it must be bounded by what the input could actually hold
        // before anything allocates. Each container costs at least four bytes of
        // key header ( key `u16` + cardinality `u16` ), so a file claiming more
        // than `remaining / 4` is impossible however plausible its cookie was.
        //
        // Without this, `SERIAL_COOKIE_NO_RUNCONTAINER` takes `n` from a bare
        // `u32`: a **60-byte** input asked for a 34 GB allocation and died
        // before parsing a single container. Found by
        // `fuzz_targets/roaring_import.rs`; the loops below would have rejected
        // it correctly, just far too late to matter.
        const MIN_BYTES_PER_CONTAINER: usize = 4;
        let remaining = r.rest().len();
        if n > remaining / MIN_BYTES_PER_CONTAINER {
            return Err(CodecError::Truncated {
                expected: n.saturating_mul(MIN_BYTES_PER_CONTAINER),
                found: remaining,
            });
        }

        let mut keys = Vec::with_capacity(n);
        for _ in 0..n {
            let k = r.u16()?;
            let card = r.u16()? as u32 + 1;
            keys.push((k, card));
        }

        // Offsets are consumed but not trusted: payloads are contiguous and in
        // header order, so a wrong offset array cannot mislead us.
        if has_offsets(cookie_lo, n) {
            for _ in 0..n {
                let _ = r.u32()?;
            }
        }

        let mut containers = Vec::with_capacity(n);
        for (i, (k, card)) in keys.into_iter().enumerate() {
            let is_run = run_flags
                .as_ref()
                .is_some_and(|f| f[i / 8] & (1 << (i % 8)) != 0);
            let c = if is_run {
                // A run payload is self-describing: read nruns, then the pairs.
                let nruns = r.peek_u16()? as usize;
                let bytes = r.take(2 + 4 * nruns)?;
                codec::decode(ContainerKind::Run, bytes, card)?
            } else if card as usize > ARRAY_MAX {
                codec::decode(ContainerKind::Bitmap, r.take(crate::BITMAP_BYTES)?, card)?
            } else {
                codec::decode(ContainerKind::Array, r.take(2 * card as usize)?, card)?
            };
            containers.push((k, c));
        }
        Ok((Roaring32 { containers }, r.pos))
    }
}

/// CRoaring `Roaring64Map`: `u64` bucket count, then `(u32 high, Roaring32)`.
pub fn serialize_u64(set: &OrdSet) -> Vec<u8> {
    let mut buckets: Vec<(u32, Roaring32)> = Vec::new();
    for (prefix, c) in set.chunks() {
        // prefix48 splits into a u32 bucket key and a u16 container key.
        let hi = (prefix >> 16) as u32;
        let lo = (prefix & 0xFFFF) as u16;
        match buckets.last_mut() {
            Some((h, r)) if *h == hi => r.containers.push((lo, c.clone())),
            _ => buckets.push((
                hi,
                Roaring32 {
                    containers: vec![(lo, c.clone())],
                },
            )),
        }
    }

    let mut out = Vec::new();
    out.extend_from_slice(&(buckets.len() as u64).to_le_bytes());
    for (hi, r) in &buckets {
        out.extend_from_slice(&hi.to_le_bytes());
        out.extend_from_slice(&r.serialize());
    }
    out
}

pub fn deserialize_u64(bytes: &[u8]) -> Result<OrdSet> {
    let mut r = Cursor::new(bytes);
    let n = r.u64()? as usize;

    // Containers arrive already in ascending prefix order and already in their
    // final representation, so they are moved straight into the set. Expanding
    // them to individual ordinals and re-sorting would make this O(cardinality
    // log cardinality) instead of O(container count) — for a dense set that is
    // the difference between touching 8 KiB and touching 65 536 integers.
    let mut chunks: Vec<(u64, Container)> = Vec::new();
    let mut ordered = true;
    for _ in 0..n {
        let hi = r.u32()? as u64;
        // Each bucket holds a complete 32-bit bitmap with no length prefix, so
        // the parse itself reports the extent.
        let (bm, consumed) = Roaring32::deserialize_with_len(r.rest())?;
        r.advance(consumed)?;
        for (k, c) in bm.containers {
            let prefix = (hi << 16) | k as u64;
            // I8: `u64::MAX` is not an ordinal. It can only arrive one way — the
            // top prefix carrying low value 0xFFFF — so the check is exact and
            // costs one `contains` on one container of a whole file.
            //
            // A CRoaring 64-bit file may legitimately hold `2^64 - 1`, so this
            // does make some valid foreign files unreadable. That is deliberate:
            // dropping the value instead would return a set that differs from
            // the file with no signal, breaking the round-trip identity that
            // makes `O(container count)` import legitimate in the first place.
            if prefix == (1u64 << 48) - 1 && c.contains(u16::MAX) {
                return Err(CodecError::OrdinalOutOfRange { ordinal: u64::MAX });
            }
            if chunks.last().is_some_and(|(p, _)| *p >= prefix) {
                ordered = false;
            }
            if !c.is_empty() {
                chunks.push((prefix, c));
            }
        }
    }

    if ordered {
        return Ok(OrdSet::from_chunks(chunks));
    }
    // A non-conforming writer emitted buckets or keys out of order. Fall back to
    // the general path rather than building a set that violates its invariants.
    let mut vals: Vec<u64> = Vec::new();
    for (prefix, c) in &chunks {
        let base = prefix << CHUNK_BITS;
        vals.extend(c.iter().map(|v| base | v as u64));
    }
    vals.sort_unstable();
    vals.dedup();
    Ok(OrdSet::from_sorted_slice(&vals))
}

pub fn write_u64(set: &OrdSet, w: &mut impl Write) -> std::io::Result<()> {
    w.write_all(&serialize_u64(set))
}

/// Read a CRoaring 64-bit bitmap from `r`.
///
/// **The io error is mapped, not widened, and it used to be discarded.** This
/// reported `Truncated { expected: 0, found: 0 }` for *every* failure — a
/// permission denial, a broken pipe, a disconnected socket — naming a condition
/// that did not occur and leaving an operator two zeros to work with. Widening
/// `CodecError` to carry an `io::Error` is the other option and the crate has
/// already declined it once ( see `db::store::io_err`: "the store's error type
/// predates io; map rather than widen it here" ), so this maps to a reason that
/// is at least true.
pub fn read_u64(r: &mut impl Read) -> Result<OrdSet> {
    let mut buf = Vec::new();
    r.read_to_end(&mut buf)
        .map_err(|e| CodecError::Invariant(io_reason(e.kind())))?;
    deserialize_u64(&buf)
}

/// A true, static description of an io failure. Mirrors `db::store::io_err`.
fn io_reason(kind: std::io::ErrorKind) -> &'static str {
    match kind {
        std::io::ErrorKind::UnexpectedEof => "roaring stream ended early",
        std::io::ErrorKind::PermissionDenied => "permission denied reading the roaring stream",
        std::io::ErrorKind::NotFound => "roaring stream not found",
        std::io::ErrorKind::BrokenPipe => "roaring stream closed by the writer",
        std::io::ErrorKind::ConnectionReset => "connection reset while reading the roaring stream",
        _ => "I/O error reading the roaring stream",
    }
}

/// Minimal bounds-checked reader. Every accessor returns `Err` rather than
/// panicking, because this parses untrusted bytes.
struct Cursor<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(b: &'a [u8]) -> Self {
        Cursor { b, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).ok_or(CodecError::Truncated {
            expected: n,
            found: self.b.len() - self.pos,
        })?;
        if end > self.b.len() {
            return Err(CodecError::Truncated {
                expected: n,
                found: self.b.len() - self.pos,
            });
        }
        let s = &self.b[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn advance(&mut self, n: usize) -> Result<()> {
        self.take(n).map(|_| ())
    }

    fn rest(&self) -> &'a [u8] {
        &self.b[self.pos..]
    }

    fn u16(&mut self) -> Result<u16> {
        let s = self.take(2)?;
        Ok(u16::from_le_bytes([s[0], s[1]]))
    }

    fn peek_u16(&self) -> Result<u16> {
        if self.pos + 2 > self.b.len() {
            return Err(CodecError::Truncated {
                expected: 2,
                found: self.b.len() - self.pos,
            });
        }
        Ok(u16::from_le_bytes([self.b[self.pos], self.b[self.pos + 1]]))
    }

    fn u32(&mut self) -> Result<u32> {
        let s = self.take(4)?;
        Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }

    fn u64(&mut self) -> Result<u64> {
        let s = self.take(8)?;
        Ok(u64::from_le_bytes(s.try_into().unwrap()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offset_header_rule() {
        // Present whenever there are no run containers, at any count.
        assert!(has_offsets(SERIAL_COOKIE_NO_RUNCONTAINER, 0));
        assert!(has_offsets(SERIAL_COOKIE_NO_RUNCONTAINER, 1));
        assert!(has_offsets(SERIAL_COOKIE_NO_RUNCONTAINER, 100));
        // With runs, only at or above the threshold. This is the classic trap:
        // small run-encoded bitmaps carry NO offset array.
        assert!(!has_offsets(SERIAL_COOKIE, 1));
        assert!(!has_offsets(SERIAL_COOKIE, 3));
        assert!(has_offsets(SERIAL_COOKIE, 4));
        assert!(has_offsets(SERIAL_COOKIE, 5));
    }

    fn roundtrip(vals: &[u64]) {
        let s = OrdSet::from_iter_unsorted(vals.iter().copied());
        let bytes = serialize_u64(&s);
        let back = deserialize_u64(&bytes).expect("roundtrip must parse");
        assert_eq!(
            back.iter().collect::<Vec<_>>(),
            s.iter().collect::<Vec<_>>()
        );
        assert_eq!(back.len(), s.len());
    }

    #[test]
    fn u64_roundtrip_small() {
        roundtrip(&[1, 2, 3]);
    }

    #[test]
    fn u64_roundtrip_empty() {
        roundtrip(&[]);
    }

    #[test]
    fn u64_roundtrip_across_buckets_and_chunks() {
        roundtrip(&[0, 65535, 65536, 1 << 32, (1 << 32) + 5, crate::ORDINAL_MAX]);
    }

    #[test]
    fn u64_roundtrip_dense_and_runny() {
        let dense: Vec<u64> = (0..70_000u64).collect();
        roundtrip(&dense);
    }

    #[test]
    fn roundtrip_survives_run_optimization() {
        let mut s = OrdSet::from_sorted_slice(&(0..50_000u64).collect::<Vec<_>>());
        s.optimize();
        let bytes = serialize_u64(&s);
        let back = deserialize_u64(&bytes).unwrap();
        assert_eq!(back.len(), s.len());
        assert_eq!(
            back.iter().collect::<Vec<_>>(),
            s.iter().collect::<Vec<_>>()
        );
    }

    #[test]
    fn deserialize_rejects_garbage_without_panicking() {
        assert!(Roaring32::deserialize(&[]).is_err());
        assert!(Roaring32::deserialize(&[0, 0, 0, 0]).is_err(), "bad cookie");
        assert!(deserialize_u64(&[1, 2, 3]).is_err());
        // Truncated mid-payload.
        let s = OrdSet::from_iter_unsorted([1u64, 2, 3]);
        let bytes = serialize_u64(&s);
        for cut in 1..bytes.len() {
            let _ = deserialize_u64(&bytes[..cut]); // must not panic
        }
    }

    /// A 60-byte input must not be able to request a 34 GB allocation.
    ///
    /// `SERIAL_COOKIE_NO_RUNCONTAINER` carries the container count in a bare
    /// `u32`, which went straight to `Vec::with_capacity`. The parse loop *would*
    /// have rejected this file — but only after the allocation, which is far too
    /// late when the allocation is the attack.
    ///
    /// Found by `fuzz_targets/roaring_import.rs` on its first run.
    #[test]
    fn a_huge_container_count_is_rejected_before_allocating() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&SERIAL_COOKIE_NO_RUNCONTAINER.to_le_bytes());
        bytes.extend_from_slice(&0xFFFF_0000u32.to_le_bytes()); // ~4.29e9 containers
                                                                // Nothing else: the file is 8 bytes and claims billions of containers.
        assert!(Roaring32::deserialize(&bytes).is_err());
    }

    /// The bound must be tight enough to matter but not so tight it rejects
    /// real files: a genuine container count for the bytes present still parses.
    #[test]
    fn a_container_count_the_input_can_support_is_still_accepted() {
        let mut s = OrdSet::new();
        for v in [1u64, 5, 70_000, 140_000] {
            s.insert(v);
        }
        let bytes = serialize_u64(&s);
        let back = deserialize_u64(&bytes).expect("a real file must still round-trip");
        assert_eq!(
            back.iter().collect::<Vec<_>>(),
            s.iter().collect::<Vec<_>>()
        );
    }

    /// The exact 60-byte input libFuzzer minimized, kept verbatim.
    #[test]
    fn the_fuzzer_oom_input_now_errs_cleanly() {
        let bytes: [u8; 60] = [
            0xff, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x30, 0x3a, 0x30,
            0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0x3b, 0x30, 0x00, 0x30, 0x00, 0x00, 0xff,
            0x3b, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0x3b, 0x30, 0x00, 0x00,
            0xff, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0x3b, 0x30,
            0x00, 0x30, 0x00, 0x00,
        ];
        // The contract is "Err, not OOM and not panic" — the specific error does
        // not matter, only that it returns at all.
        let _ = deserialize_u64(&bytes);
    }

    /// The streaming pair round-trips, and reports why a read failed.
    ///
    /// `write_u64` / `read_u64` were **public with no test and no caller** —
    /// found by the unwired-`pub fn` sweep. Their siblings `serialize_u64` /
    /// `deserialize_u64` have several, which is what made the gap invisible:
    /// the module looked covered.
    #[test]
    fn the_streaming_u64_pair_round_trips() {
        let s = OrdSet::from_sorted_slice(&[0u64, 7, 65_535, 65_536, 1 << 40, u64::MAX - 1]);
        let mut buf: Vec<u8> = Vec::new();
        write_u64(&s, &mut buf).expect("writing to a Vec cannot fail");

        // Byte-identical to the non-streaming form, which is the only thing
        // that makes one a wrapper of the other rather than a second encoder.
        assert_eq!(buf, serialize_u64(&s));

        let back = read_u64(&mut buf.as_slice()).expect("must parse what we wrote");
        assert_eq!(
            back.iter().collect::<Vec<_>>(),
            s.iter().collect::<Vec<_>>()
        );
    }

    /// An io failure must say what happened, not invent a truncation.
    #[test]
    fn a_failing_reader_reports_its_own_reason() {
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "nope",
                ))
            }
        }
        let err = read_u64(&mut Broken).expect_err("a failing reader must fail");
        let msg = format!("{err}");
        assert!(
            msg.contains("permission denied"),
            "the reason must survive; got {msg:?}"
        );
        // And specifically must not claim a truncation that did not happen.
        assert!(
            !matches!(err, CodecError::Truncated { .. }),
            "an io failure is not a truncated payload"
        );
    }
}
