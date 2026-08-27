//! WAL record framing.
//!
//! # `lsn` is the record's global byte offset
//!
//! Not a counter. That makes a cursor trivially seekable, and — more
//! importantly — it makes a byte range shipped to a follower *offset-identical*
//! to the leader's, which is what a future Raft log-matching rule requires. The
//! field is stored redundantly in the header so a scan can detect that it has
//! lost framing rather than confidently decoding rubbish.
//!
//! # `term` holds the leadership that wrote the record
//!
//! Reserved from day one on the argument that eight bytes per record is the
//! cheapest option available on the path to consensus and that adding the field
//! later would be a format break. Since 2026-08-29 it is populated: every append
//! carries the term from the database's MANIFEST, which a promotion raises.
//!
//! **Nothing reads it back here.** Recovery and the follower's apply path both
//! ignore it, and the fence that uses the term operates on `StatusResponse`
//! rather than on records. What this field buys is that a log records *which*
//! leadership wrote each record — the input a divergence check needs, and the
//! log-matching property consensus would later require. It is deliberately inert
//! today rather than absent.
//!
//! The same argument applies to [`RecType::EpochFence`], which is still written
//! and still read by nothing.
//!
//! # `flags` is load-bearing, and its unknown bits are an error
//!
//! Byte 9 was slack until commit-time stamping needed somewhere to live. It now
//! carries [`FLAG_COMMIT_TIME`], which says that a `ShardCommit` or `Abort` body
//! holds eight bytes of UNIX-epoch microseconds.
//!
//! The commit time is in a *body* while `term` is in the *header*, and that
//! asymmetry is deliberate rather than an oversight to tidy up. `term` is stamped
//! on every record, so a header field is the only place it can go. A commit time
//! belongs to a commit, not to a record: every commit necessarily writes a
//! `ShardCommit` ( or an `Abort` ), both of which had empty bodies, so the field
//! costs eight bytes per commit per participating shard instead of eight bytes
//! per record. Widening the header would have put it on every `SetRange` in a
//! bulk load — the one record type this format is shaped around — to carry a
//! value identical across the whole batch.
//!
//! The other half of that bargain is that a reader must not *ignore* a flag it
//! does not know: [`Record::decode`] rejects any bit outside [`KNOWN_FLAGS`],
//! because a future flag silently skipped is a body misread as something else.
//! A record with no flags set and an empty body is an ordinary pre-stamping
//! record and stays valid forever.
//!
//! # The framing contract
//!
//! A scan stops — cleanly, without error — at the first record that is not
//! well-formed: a CRC mismatch, a zero header, an `lsn` that disagrees with the
//! position, or a truncated tail. All four are normal at the end of a log that
//! was being written when the process died. Anything past that point is
//! discarded, which is safe because recovery only replays a prefix.

use crate::error::{CodecError, Result};
use crate::store::checksum::crc32c;

/// Bytes of framing before a record body.
pub const HEADER: usize = 40;

/// Records are padded so every header starts 8-byte aligned.
pub const ALIGN: usize = 8;

/// The record's body carries an 8-byte commit time. Set only on the commit
/// markers `ShardCommit` and `Abort`; see this module's header.
pub const FLAG_COMMIT_TIME: u8 = 0x01;

/// Every flag bit this version understands.
///
/// A bit outside this mask is refused by [`Record::decode`] rather than
/// ignored. Ignoring one would let a later writer's body be read as an earlier
/// writer's, which is exactly the confident-garbage failure the redundant `lsn`
/// exists to prevent elsewhere.
pub const KNOWN_FLAGS: u8 = FLAG_COMMIT_TIME;

const OFF_LEN: usize = 0;
const OFF_CRC: usize = 4;
const OFF_TYPE: usize = 8;
const OFF_FLAGS: usize = 9;
const OFF_BODY_LEN: usize = 12;
const OFF_LSN: usize = 16;
const OFF_CV: usize = 24;
const OFF_TERM: usize = 32;

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecType {
    /// Filler to the end of a segment. Skipped on replay.
    Pad = 0,
    /// Logical add/remove of values within one chunk. Kind-agnostic.
    ChunkDelta = 1,
    /// A whole container image, for promotions and large rewrites.
    ChunkImage = 2,
    ChunkDelete = 3,
    /// An arbitrarily large ordinal range in 32 bytes. The single biggest
    /// WAL-size lever, and what makes bulk load affordable.
    SetRange = 4,
    /// Names every shard participating in a commit. Written redundantly to all
    /// of them so each stream is independently interpretable by a follower.
    CommitIntent = 5,
    ShardCommit = 6,
    /// Resolves a commit version that will never complete.
    ///
    /// Without this, an abort after a commit version has been assigned stalls
    /// the visible watermark forever, and the next recovery silently discards
    /// every acknowledged commit above the hole.
    Abort = 7,
    CheckpointBegin = 8,
    CheckpointEnd = 9,
    EpochFence = 10,
}

impl RecType {
    pub fn from_u8(v: u8) -> Option<RecType> {
        Some(match v {
            0 => RecType::Pad,
            1 => RecType::ChunkDelta,
            2 => RecType::ChunkImage,
            3 => RecType::ChunkDelete,
            4 => RecType::SetRange,
            5 => RecType::CommitIntent,
            6 => RecType::ShardCommit,
            7 => RecType::Abort,
            8 => RecType::CheckpointBegin,
            9 => RecType::CheckpointEnd,
            10 => RecType::EpochFence,
            _ => return None,
        })
    }

    /// Does this record carry a commit version that recovery must resolve?
    #[inline]
    pub fn is_commit_marker(self) -> bool {
        matches!(
            self,
            RecType::CommitIntent | RecType::ShardCommit | RecType::Abort
        )
    }
}

/// A framed record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub rtype: RecType,
    pub flags: u8,
    pub lsn: u64,
    pub commit_version: u64,
    pub term: u64,
    pub body: Vec<u8>,
}

impl Record {
    pub fn new(rtype: RecType, lsn: u64, commit_version: u64, term: u64, body: Vec<u8>) -> Self {
        Record {
            rtype,
            flags: 0,
            lsn,
            commit_version,
            term,
            body,
        }
    }

    /// A commit marker carrying its commit time.
    ///
    /// `rtype` must be a resolving marker — `ShardCommit` or `Abort`. The intent
    /// record is not stamped: it names participants, and a commit that writes one
    /// also writes a `ShardCommit` to every shard it names.
    pub fn commit_marker(
        rtype: RecType,
        lsn: u64,
        commit_version: u64,
        term: u64,
        time: u64,
    ) -> Self {
        debug_assert!(
            matches!(rtype, RecType::ShardCommit | RecType::Abort),
            "only a resolving commit marker carries a commit time"
        );
        Record {
            rtype,
            flags: FLAG_COMMIT_TIME,
            lsn,
            commit_version,
            term,
            body: encode_commit_time(time),
        }
    }

    /// The commit time this record carries, if it carries one.
    ///
    /// `None` for every record written before commit-time stamping existed, which
    /// is why a time-targeted restore must refuse rather than round: an absent
    /// stamp is genuinely unknown, not zero.
    pub fn commit_time(&self) -> Result<Option<u64>> {
        if self.flags & FLAG_COMMIT_TIME == 0 {
            return Ok(None);
        }
        decode_commit_time(&self.body).map(Some)
    }

    /// Total framed size, including padding to [`ALIGN`].
    #[inline]
    pub fn framed_len(body_len: usize) -> usize {
        (HEADER + body_len).div_ceil(ALIGN) * ALIGN
    }

    /// Serialize at `lsn`, which must equal the byte offset this will occupy.
    pub fn encode(&self) -> Vec<u8> {
        let total = Self::framed_len(self.body.len());
        let mut buf = vec![0u8; total];
        buf[OFF_LEN..OFF_LEN + 4].copy_from_slice(&(total as u32).to_le_bytes());
        buf[OFF_TYPE] = self.rtype as u8;
        buf[OFF_FLAGS] = self.flags;
        buf[OFF_BODY_LEN..OFF_BODY_LEN + 4]
            .copy_from_slice(&(self.body.len() as u32).to_le_bytes());
        buf[OFF_LSN..OFF_LSN + 8].copy_from_slice(&self.lsn.to_le_bytes());
        buf[OFF_CV..OFF_CV + 8].copy_from_slice(&self.commit_version.to_le_bytes());
        buf[OFF_TERM..OFF_TERM + 8].copy_from_slice(&self.term.to_le_bytes());
        buf[HEADER..HEADER + self.body.len()].copy_from_slice(&self.body);
        // CRC covers everything after the CRC field itself, so a corrupted
        // header is caught along with a corrupted body.
        let crc = crc32c(&buf[OFF_TYPE..]);
        buf[OFF_CRC..OFF_CRC + 4].copy_from_slice(&crc.to_le_bytes());
        buf
    }

    /// The framed length a record at `buf[0]` claims, from its first four bytes.
    ///
    /// Only the *claim*. Nothing is verified, because verification needs the
    /// whole frame and the entire point of this is to learn how much of it to
    /// read — [`Self::peek_lsn`] answers `None` on a buffer that stops short, so
    /// probing a log's base with a fixed-size read silently gets the wrong
    /// answer whenever the first record happens to be larger than the probe.
    /// That is a bug this cost an hour of; the fix is to read what the frame
    /// says it is.
    ///
    /// Callers must bound the result against what the file actually holds before
    /// allocating: the field is a `u32` and garbage is a valid `u32`.
    pub fn peek_framed_len(buf: &[u8]) -> Option<usize> {
        if buf.len() < 4 {
            return None;
        }
        let total = u32::from_le_bytes(buf[..4].try_into().ok()?) as usize;
        (total >= HEADER && total.is_multiple_of(ALIGN)).then_some(total)
    }

    /// The LSN a well-formed frame at `buf[0]` claims, without being told where
    /// it is.
    ///
    /// This is the one check [`Self::decode`] will not skip: it refuses a
    /// frame whose `lsn` disagrees with the offset it was found at, and that
    /// redundancy is how a scan detects lost framing. But a log's **base** is
    /// precisely the quantity that offset is measured from, so deriving it has
    /// to read the field rather than verify it.
    ///
    /// Everything else is still checked, so a torn or garbage first frame
    /// answers `None` rather than a plausible number: the base is what every
    /// later offset is computed against, and a wrong one silently invalidates a
    /// whole log rather than one record.
    pub fn peek_lsn(buf: &[u8]) -> Option<u64> {
        if buf.len() < HEADER {
            return None;
        }
        let total = u32::from_le_bytes(buf[OFF_LEN..OFF_LEN + 4].try_into().ok()?) as usize;
        if total < HEADER || !total.is_multiple_of(ALIGN) || total > buf.len() {
            return None;
        }
        let stored_crc = u32::from_le_bytes(buf[OFF_CRC..OFF_CRC + 4].try_into().ok()?);
        if crc32c(&buf[OFF_TYPE..total]) != stored_crc {
            return None;
        }
        Some(u64::from_le_bytes(
            buf[OFF_LSN..OFF_LSN + 8].try_into().ok()?,
        ))
    }

    /// Decode the record starting at `buf[0]`, whose byte offset is `at`.
    ///
    /// Returns `Ok(None)` for the four *normal* end-of-log conditions rather
    /// than an error: a zero header, a truncated tail, a CRC mismatch, or an
    /// `lsn` disagreeing with `at`. Each means "the log ends here", which is
    /// exactly what a crash mid-append leaves behind.
    pub fn decode(buf: &[u8], at: u64) -> Result<Option<(Record, usize)>> {
        if buf.len() < HEADER {
            return Ok(None);
        }
        let total = u32::from_le_bytes(buf[OFF_LEN..OFF_LEN + 4].try_into().unwrap()) as usize;
        if total == 0 {
            return Ok(None); // never-written tail
        }
        if total < HEADER || !total.is_multiple_of(ALIGN) || total > buf.len() {
            return Ok(None); // truncated or nonsense length
        }
        let stored_crc = u32::from_le_bytes(buf[OFF_CRC..OFF_CRC + 4].try_into().unwrap());
        if crc32c(&buf[OFF_TYPE..total]) != stored_crc {
            return Ok(None); // torn write
        }
        let Some(rtype) = RecType::from_u8(buf[OFF_TYPE]) else {
            // A valid CRC over an unknown type means a newer writer produced it.
            // That is not a torn tail, so it is an error rather than a stop.
            return Err(CodecError::UnsupportedEncoding);
        };
        let flags = buf[OFF_FLAGS];
        if flags & !KNOWN_FLAGS != 0 {
            // Same reasoning as the unknown `RecType` above: the CRC held, so a
            // newer writer meant this. Refusing beats guessing at the body.
            return Err(CodecError::UnsupportedEncoding);
        }
        let body_len =
            u32::from_le_bytes(buf[OFF_BODY_LEN..OFF_BODY_LEN + 4].try_into().unwrap()) as usize;
        if HEADER + body_len > total {
            return Err(CodecError::Invariant("record body overruns its frame"));
        }
        let lsn = u64::from_le_bytes(buf[OFF_LSN..OFF_LSN + 8].try_into().unwrap());
        if lsn != at {
            return Ok(None); // lost framing
        }
        let rec = Record {
            rtype,
            flags,
            lsn,
            commit_version: u64::from_le_bytes(buf[OFF_CV..OFF_CV + 8].try_into().unwrap()),
            term: u64::from_le_bytes(buf[OFF_TERM..OFF_TERM + 8].try_into().unwrap()),
            body: buf[HEADER..HEADER + body_len].to_vec(),
        };
        Ok(Some((rec, total)))
    }
}

/// Sequential scan over a WAL byte range.
///
/// Yields records until the first ill-formed one, then stops. `stopped_at`
/// reports where, which is the offset recovery truncates to.
pub struct Scanner<'a> {
    buf: &'a [u8],
    base: u64,
    pos: usize,
    stopped: bool,
}

impl<'a> Scanner<'a> {
    /// `base` is the byte offset of `buf[0]` in the global address space.
    pub fn new(buf: &'a [u8], base: u64) -> Self {
        Scanner {
            buf,
            base,
            pos: 0,
            stopped: false,
        }
    }

    /// Offset just past the last well-formed record.
    #[inline]
    pub fn stopped_at(&self) -> u64 {
        self.base + self.pos as u64
    }
}

impl Iterator for Scanner<'_> {
    type Item = Result<Record>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.stopped || self.pos >= self.buf.len() {
            return None;
        }
        let at = self.base + self.pos as u64;
        match Record::decode(&self.buf[self.pos..], at) {
            Ok(Some((rec, n))) => {
                self.pos += n;
                Some(Ok(rec))
            }
            Ok(None) => {
                self.stopped = true;
                None
            }
            Err(e) => {
                self.stopped = true;
                Some(Err(e))
            }
        }
    }
}

// ------------------------------------------------------------ record bodies

/// `CommitIntent` body: the shards participating in one commit.
pub fn encode_commit_intent(shards: &[u32]) -> Vec<u8> {
    let mut b = Vec::with_capacity(2 + shards.len() * 4);
    b.extend_from_slice(&(shards.len() as u16).to_le_bytes());
    for s in shards {
        b.extend_from_slice(&s.to_le_bytes());
    }
    b
}

pub fn decode_commit_intent(body: &[u8]) -> Result<Vec<u32>> {
    if body.len() < 2 {
        return Err(CodecError::Truncated {
            expected: 2,
            found: body.len(),
        });
    }
    let n = u16::from_le_bytes([body[0], body[1]]) as usize;
    if body.len() != 2 + n * 4 {
        return Err(CodecError::Truncated {
            expected: 2 + n * 4,
            found: body.len(),
        });
    }
    Ok((0..n)
        .map(|i| u32::from_le_bytes(body[2 + i * 4..6 + i * 4].try_into().unwrap()))
        .collect())
}

/// Commit-marker body: the wall-clock time the commit version was assigned.
///
/// Written on `ShardCommit` and `Abort` when [`FLAG_COMMIT_TIME`] is set. UNIX
/// epoch microseconds, taken once per commit under the version oracle's lock, so
/// it is non-decreasing in commit version by construction ( invariant **I9** ).
pub fn encode_commit_time(micros: u64) -> Vec<u8> {
    micros.to_le_bytes().to_vec()
}

pub fn decode_commit_time(body: &[u8]) -> Result<u64> {
    if body.len() != 8 {
        return Err(CodecError::Truncated {
            expected: 8,
            found: body.len(),
        });
    }
    Ok(u64::from_le_bytes(body[..8].try_into().unwrap()))
}

/// `SetRange` body: an inclusive ordinal range under one key, in 25 bytes.
pub fn encode_set_range(key: u64, lo: u64, hi: u64, remove: bool) -> Vec<u8> {
    let mut b = Vec::with_capacity(25);
    b.extend_from_slice(&key.to_le_bytes());
    b.extend_from_slice(&lo.to_le_bytes());
    b.extend_from_slice(&hi.to_le_bytes());
    b.push(remove as u8);
    b
}

pub fn decode_set_range(body: &[u8]) -> Result<(u64, u64, u64, bool)> {
    if body.len() != 25 {
        return Err(CodecError::Truncated {
            expected: 25,
            found: body.len(),
        });
    }
    let g = |i: usize| u64::from_le_bytes(body[i..i + 8].try_into().unwrap());
    let (key, lo, hi) = (g(0), g(8), g(16));
    if hi < lo {
        return Err(CodecError::Invariant("SetRange bounds inverted"));
    }
    Ok((key, lo, hi, body[24] != 0))
}

/// `ChunkDelta` body: sorted values added and removed within one chunk.
pub fn encode_chunk_delta(key: u64, prefix: u64, add: &[u16], rem: &[u16]) -> Vec<u8> {
    let mut b = Vec::with_capacity(24 + 2 * (add.len() + rem.len()));
    b.extend_from_slice(&key.to_le_bytes());
    b.extend_from_slice(&prefix.to_le_bytes());
    b.extend_from_slice(&(add.len() as u16).to_le_bytes());
    b.extend_from_slice(&(rem.len() as u16).to_le_bytes());
    b.extend_from_slice(&[0u8; 4]); // pad to 24 so the value arrays are aligned
    for v in add.iter().chain(rem) {
        b.extend_from_slice(&v.to_le_bytes());
    }
    b
}

pub fn decode_chunk_delta(body: &[u8]) -> Result<(u64, u64, Vec<u16>, Vec<u16>)> {
    if body.len() < 24 {
        return Err(CodecError::Truncated {
            expected: 24,
            found: body.len(),
        });
    }
    let key = u64::from_le_bytes(body[0..8].try_into().unwrap());
    let prefix = u64::from_le_bytes(body[8..16].try_into().unwrap());
    let n_add = u16::from_le_bytes(body[16..18].try_into().unwrap()) as usize;
    let n_rem = u16::from_le_bytes(body[18..20].try_into().unwrap()) as usize;
    let want = 24 + 2 * (n_add + n_rem);
    if body.len() != want {
        return Err(CodecError::Truncated {
            expected: want,
            found: body.len(),
        });
    }
    let vals: Vec<u16> = body[24..]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&c| u16::from_le_bytes(c))
        .collect();
    let (add, rem) = vals.split_at(n_add);
    Ok((key, prefix, add.to_vec(), rem.to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(t: RecType, lsn: u64, cv: u64, body: Vec<u8>) -> Record {
        Record::new(t, lsn, cv, 1, body)
    }

    #[test]
    fn header_is_forty_bytes_and_frames_are_aligned() {
        assert_eq!(HEADER, 40);
        for body in [0usize, 1, 7, 8, 25, 1000] {
            let n = Record::framed_len(body);
            assert!(n >= HEADER + body);
            assert_eq!(n % ALIGN, 0, "frame of body {body} is not 8-byte aligned");
        }
    }

    #[test]
    fn roundtrip_preserves_every_field() {
        let r = rec(RecType::ChunkDelta, 4096, 77, vec![1, 2, 3, 4, 5]);
        let bytes = r.encode();
        let (back, n) = Record::decode(&bytes, 4096).unwrap().unwrap();
        assert_eq!(back, r);
        assert_eq!(n, bytes.len());
        assert_eq!(back.term, 1, "term must survive the roundtrip");
    }

    #[test]
    fn lsn_is_the_byte_offset() {
        // Records encoded at successive offsets must decode only at those offsets.
        let mut buf = Vec::new();
        let mut lsn = 0u64;
        let mut expect = Vec::new();
        for i in 0..10u64 {
            let r = rec(RecType::ShardCommit, lsn, i, vec![i as u8; i as usize]);
            let e = r.encode();
            lsn += e.len() as u64;
            expect.push(r);
            buf.extend_from_slice(&e);
        }
        let got: Vec<Record> = Scanner::new(&buf, 0).map(|r| r.unwrap()).collect();
        assert_eq!(got, expect);
        assert_eq!(got.last().unwrap().lsn + Record::framed_len(9) as u64, lsn);
    }

    #[test]
    fn decoding_at_the_wrong_offset_reports_lost_framing() {
        let r = rec(RecType::Pad, 512, 0, vec![]);
        let bytes = r.encode();
        assert!(Record::decode(&bytes, 512).unwrap().is_some());
        assert!(
            Record::decode(&bytes, 513).unwrap().is_none(),
            "an lsn disagreeing with the position means framing was lost"
        );
    }

    #[test]
    fn scan_stops_cleanly_at_a_torn_tail() {
        let mut buf = Vec::new();
        let mut lsn = 0u64;
        for i in 0..5u64 {
            let r = rec(RecType::ChunkDelta, lsn, i, vec![7; 16]);
            let e = r.encode();
            lsn += e.len() as u64;
            buf.extend_from_slice(&e);
        }
        let good_end = buf.len();
        // A half-written record, as a crash mid-append leaves.
        let r = rec(RecType::ChunkDelta, lsn, 99, vec![7; 16]);
        let e = r.encode();
        buf.extend_from_slice(&e[..e.len() / 2]);

        let mut s = Scanner::new(&buf, 0);
        let got: Vec<Record> = (&mut s).map(|r| r.unwrap()).collect();
        assert_eq!(got.len(), 5, "must yield only the intact prefix");
        assert_eq!(s.stopped_at(), good_end as u64, "truncation point");
    }

    #[test]
    fn scan_stops_at_a_corrupted_record_without_losing_the_prefix() {
        let mut buf = Vec::new();
        let mut lsn = 0u64;
        let mut offsets = Vec::new();
        for i in 0..5u64 {
            offsets.push(buf.len());
            let r = rec(RecType::ChunkDelta, lsn, i, vec![3; 24]);
            let e = r.encode();
            lsn += e.len() as u64;
            buf.extend_from_slice(&e);
        }
        // Corrupt the body of record 3.
        buf[offsets[3] + HEADER + 2] ^= 0xFF;

        let mut s = Scanner::new(&buf, 0);
        let got: Vec<Record> = (&mut s).map(|r| r.unwrap()).collect();
        assert_eq!(got.len(), 3, "records before the corruption survive");
        assert_eq!(s.stopped_at(), offsets[3] as u64);
    }

    #[test]
    fn a_corrupted_header_is_caught_too() {
        // The CRC deliberately covers the header from `rtype` onward, so a flip
        // in commit_version or term cannot pass.
        for off in [OFF_TYPE, OFF_BODY_LEN, OFF_LSN, OFF_CV, OFF_TERM] {
            let r = rec(RecType::ShardCommit, 0, 5, vec![1, 2, 3]);
            let mut bytes = r.encode();
            bytes[off] ^= 0x01;
            let decoded = Record::decode(&bytes, 0);
            assert!(
                matches!(decoded, Ok(None)) || decoded.is_err(),
                "corruption at header offset {off} was not detected"
            );
        }
    }

    #[test]
    fn zero_filled_tail_ends_the_scan() {
        let mut buf = rec(RecType::Pad, 0, 0, vec![]).encode();
        buf.resize(4096, 0);
        let mut s = Scanner::new(&buf, 0);
        assert_eq!((&mut s).count(), 1);
        assert_eq!(s.stopped_at(), HEADER as u64);
    }

    #[test]
    fn unknown_record_type_with_a_valid_crc_is_an_error_not_a_stop() {
        // A newer writer produced this. Treating it as a torn tail would
        // silently discard everything after it.
        let mut r = rec(RecType::ShardCommit, 0, 1, vec![]);
        r.flags = 0;
        let mut bytes = r.encode();
        bytes[OFF_TYPE] = 200;
        let crc = crc32c(&bytes[OFF_TYPE..]);
        bytes[OFF_CRC..OFF_CRC + 4].copy_from_slice(&crc.to_le_bytes());
        assert!(matches!(
            Record::decode(&bytes, 0),
            Err(CodecError::UnsupportedEncoding)
        ));
    }

    #[test]
    fn an_unknown_flag_bit_with_a_valid_crc_is_an_error_not_a_stop() {
        // Same argument as the unknown record type: the checksum held, so a
        // newer writer meant these bytes. Skipping the flag would mean reading
        // its body as an older writer's, which is worse than refusing.
        let mut r = rec(RecType::ShardCommit, 0, 1, vec![]);
        r.flags = 0;
        let mut bytes = r.encode();
        bytes[OFF_FLAGS] = 0x80;
        let crc = crc32c(&bytes[OFF_TYPE..]);
        bytes[OFF_CRC..OFF_CRC + 4].copy_from_slice(&crc.to_le_bytes());
        assert!(matches!(
            Record::decode(&bytes, 0),
            Err(CodecError::UnsupportedEncoding)
        ));
    }

    #[test]
    fn a_stamped_marker_round_trips_and_an_unstamped_one_reports_none() {
        let stamped = Record::commit_marker(RecType::ShardCommit, 0, 7, 3, 1_700_000_000_000_001);
        let bytes = stamped.encode();
        let (back, _) = Record::decode(&bytes, 0).unwrap().unwrap();
        assert_eq!(back, stamped);
        assert_eq!(back.commit_time().unwrap(), Some(1_700_000_000_000_001));

        // The pre-stamping shape, which stays valid forever.
        let plain = rec(RecType::ShardCommit, 0, 7, vec![]);
        let bytes = plain.encode();
        let (back, _) = Record::decode(&bytes, 0).unwrap().unwrap();
        assert_eq!(back.flags, 0);
        assert_eq!(
            back.commit_time().unwrap(),
            None,
            "an unstamped marker must read as unknown, never as the epoch"
        );
    }

    /// The flag promises eight bytes. A frame that sets it over a shorter body
    /// is malformed, not "a zero time".
    #[test]
    fn a_stamped_marker_with_a_short_body_is_an_error() {
        let mut r = rec(RecType::ShardCommit, 0, 1, vec![0u8; 4]);
        r.flags = FLAG_COMMIT_TIME;
        let bytes = r.encode();
        let (back, _) = Record::decode(&bytes, 0).unwrap().unwrap();
        assert!(back.commit_time().is_err());
    }

    #[test]
    fn decode_never_panics_on_arbitrary_bytes() {
        let mut seed = 0x12345678u64;
        for _ in 0..2000 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let n = (seed % 200) as usize;
            let bytes: Vec<u8> = (0..n).map(|i| (seed >> (i % 56)) as u8).collect();
            let _ = Record::decode(&bytes, seed % 1024);
            let _ = Scanner::new(&bytes, 0).count();
        }
    }

    #[test]
    fn commit_intent_body_roundtrip() {
        for shards in [vec![], vec![0u32], vec![1, 5, 9, 200]] {
            let b = encode_commit_intent(&shards);
            assert_eq!(decode_commit_intent(&b).unwrap(), shards);
        }
        assert!(decode_commit_intent(&[]).is_err());
        assert!(
            decode_commit_intent(&[9, 0]).is_err(),
            "claims 9 shards, has none"
        );
    }

    #[test]
    fn set_range_is_twenty_five_bytes_regardless_of_span() {
        let small = encode_set_range(1, 5, 6, false);
        let huge = encode_set_range(1, 0, u64::MAX, false);
        assert_eq!(small.len(), 25);
        assert_eq!(
            huge.len(),
            25,
            "a range's cost must not scale with its size"
        );
        assert_eq!(decode_set_range(&huge).unwrap(), (1, 0, u64::MAX, false));
        assert_eq!(decode_set_range(&small).unwrap(), (1, 5, 6, false));
    }

    #[test]
    fn set_range_rejects_inverted_bounds() {
        let mut b = encode_set_range(1, 10, 20, false);
        b[8..16].copy_from_slice(&30u64.to_le_bytes());
        assert!(decode_set_range(&b).is_err());
    }

    #[test]
    fn chunk_delta_body_roundtrip() {
        let add = vec![1u16, 5, 9];
        let rem = vec![2u16, 7];
        let b = encode_chunk_delta(42, 1234, &add, &rem);
        assert_eq!(decode_chunk_delta(&b).unwrap(), (42, 1234, add, rem));

        // Empty deltas are legal.
        let b = encode_chunk_delta(1, 2, &[], &[]);
        assert_eq!(decode_chunk_delta(&b).unwrap(), (1, 2, vec![], vec![]));
        // A truncated body must not index out of bounds.
        assert!(decode_chunk_delta(&b[..20]).is_err());
    }

    #[test]
    fn a_delta_beats_an_image_for_a_bitmap_chunk() {
        // The reason the delta record type exists: a few values against 8 KiB.
        let delta = encode_chunk_delta(1, 2, &[7, 9, 11], &[]);
        assert!(
            delta.len() < crate::BITMAP_BYTES / 100,
            "delta is {} bytes, which is not decisively smaller than an image",
            delta.len()
        );
    }
}
