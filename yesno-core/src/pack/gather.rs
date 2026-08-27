//! `OrdSet` -> canonical words: the inbound half of the bit-layout seam.
//!
//! The outbound half is each lens's sink, over [`OrdinalSink`](super::OrdinalSink).
//!
//! A *line* is a contiguous run of `line_bits` ordinals, which is what makes
//! this a range gather rather than one membership probe per bit: the walk visits
//! the chunks overlapping an object's ordinal range and copies set bits into the
//! caller's destination. Cost is `O(set bits in the range + lines + chunks
//! spanned)`, so a sparse object is cheap even when it is nominally wide.
//!
//! # This is the oracle
//!
//! It handles any `line_stride`, any `object_stride`, a base at any bit offset,
//! and an object straddling the 65 536-bit chunk boundary. [`super::try_gather`]
//! is the specialized arm and is diffed against this one. It is never deleted
//! when that arm is faster — same contract as [`ops::generic`](crate::ops::generic).
//!
//! # What it still costs: the scan up to `base`
//!
//! A `Container` iterates from its first value and cannot seek, so gathering an
//! object that does **not** start on a chunk boundary discards every value in
//! that chunk below it. That is `O(values before base)` once per gather —
//! linear, not quadratic — but it is not free, and it is the entire reason
//! [`super::try_gather`] exists. Measured on two reads of the same 10 000 bits
//! of the same set:
//!
//! ```text
//!   contained,  object 0 at bit 0        19.6 us
//!   straddling, object 6 at bit 60 000  120.7 us   6.2x
//! ```
//!
//! The extra 100 µs is 60 000 values iterated and thrown away. So the prize for
//! a specialized arm is **skipping the prefix**, not the copy.

use super::Packing;
use crate::{chunk_base, split, OrdSet};

/// Scatter every source ordinal in the object's span into `out`, in one pass.
///
/// `out` is indexed by line: line `l` occupies
/// `out[l * out_line_words .. (l + 1) * out_line_words]`, and bit `b` of that
/// line is bit `b` of that slice. The caller must have zeroed it and must have
/// established that the object is addressable — see [`Packing::last_of`].
///
/// # One pass, because two would be quadratic
///
/// This was first written as "for each line, gather that line's ordinal
/// range", which is the obvious shape and is **`O(lines × container size)`**: a
/// container iterates from its first value, so gathering line `L` rescans and
/// discards everything below it. Measured on a 256×256 object over one full
/// chunk — 256 lines over 60 000 values — that was **5.5 ms to move 8 KiB**,
/// about 1.5 MB/s. Do not reintroduce it.
///
/// Both sequences ascend: the source ordinals, and the lines' ordinal ranges.
/// So they merge. The line cursor only ever moves forward, each source value is
/// examined once, and the cost is `O(set bits in the span + lines + chunks)`.
///
/// A value landing in the padding *between* two lines belongs to neither and is
/// dropped by the range test after the cursor has advanced past it.
pub(crate) fn gather(
    set: &OrdSet,
    base: u64,
    packing: &Packing,
    out: &mut [u64],
    out_line_words: usize,
) {
    let line_bits = packing.line_bits as u64;
    let stride = packing.line_stride as u64;
    let lines = packing.lines;
    if lines == 0 || line_bits == 0 {
        return;
    }
    debug_assert!(out_line_words >= packing.line_words());
    debug_assert!(out.len() >= lines as usize * out_line_words);
    // `base + span_bits() - 1 <= ORDINAL_MAX` is established by the caller, so
    // none of the arithmetic below can overflow.
    let last = base + (packing.span_bits() - 1);
    let (p_lo, _) = split(base);
    let (p_hi, _) = split(last);

    let mut line: u32 = 0;
    let mut line_lo = base;

    let n = set.chunk_count();
    let mut i = set.partition_point_in(0, n, p_lo);
    while i < n {
        let Some((p, c)) = set.chunk_at(i) else { break };
        if p > p_hi {
            break;
        }
        let cb = chunk_base(p);
        for v in c.iter() {
            let o = cb | v as u64;
            // Only the first chunk can hold values below `base`; the container
            // iterates ascending, so the upper bound can break outright.
            if o < base {
                continue;
            }
            if o > last {
                break;
            }
            while o >= line_lo + line_bits && line + 1 < lines {
                line += 1;
                line_lo = base + line as u64 * stride;
            }
            // Below the cursor means the padding gap between two lines: the loop
            // above advanced past the line this value would have belonged to,
            // and the next one has not started.
            //
            // The two halves are not equally load-bearing, and both sabotages
            // were run to find out which is which:
            //
            // - `o >= line_lo` is **required**. Without it a padding value gives
            //   a negative `bit` and underflows — a panic, not a wrong answer.
            // - `o < line_lo + line_bits` is **implied** by the loop above, which
            //   exits either on that condition or on the last line, where
            //   `o <= last` is exactly `line_lo + line_bits - 1`. Removing it
            //   fails nothing. It is kept because it costs one comparison and
            //   the alternative, should a future packing knob falsify that
            //   argument, is an out-of-range word index.
            //
            // Advancing the cursor on `stride` rather than `line_bits` is also
            // correct — it just moves which half is load-bearing. Do not "fix"
            // one without re-deriving the other.
            if o >= line_lo && o < line_lo + line_bits {
                let bit = (o - line_lo) as usize;
                debug_assert!(bit < line_bits as usize);
                let w = line as usize * out_line_words + (bit >> 6);
                out[w] |= 1u64 << (bit & 63);
            }
        }
        i += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set_of(ordinals: &[u64]) -> OrdSet {
        OrdSet::from_iter_unsorted(ordinals.iter().copied())
    }

    fn gather_object(set: &OrdSet, k: u64, p: &Packing) -> Vec<u64> {
        let words = p.line_words();
        let mut out = vec![0u64; p.lines as usize * words];
        let base = p.base_of(k).unwrap();
        p.last_of(k).expect("addressable");
        gather(set, base, p, &mut out, words);
        out
    }

    fn bit(out: &[u64], line_words: usize, l: u32, b: u32) -> bool {
        let i = l as usize * line_words + (b as usize >> 6);
        out[i] >> (b & 63) & 1 == 1
    }

    #[test]
    fn a_bit_at_base_plus_b_lands_on_line_zero_bit_b() {
        let p = Packing::dense(1, 64);
        // Object 1 spans ordinals 64..128; bits 0 and 3 of it are set.
        let out = gather_object(&set_of(&[64, 67]), 1, &p);
        assert_eq!(out, vec![0b1001]);
    }

    #[test]
    fn lines_are_addressed_independently() {
        let p = Packing::dense(3, 5);
        // Object 0 spans 0..15: line 0 is 0..5, line 1 is 5..10, line 2 is 10..15.
        let out = gather_object(&set_of(&[0, 6, 14]), 0, &p);
        assert!(bit(&out, 1, 0, 0));
        assert!(bit(&out, 1, 1, 1));
        assert!(bit(&out, 1, 2, 4));
        assert_eq!(out.iter().map(|w| w.count_ones()).sum::<u32>(), 3);
    }

    #[test]
    fn padding_between_lines_belongs_to_neither() {
        let p = Packing {
            line_stride: 16,
            object_stride: 32,
            ..Packing::dense(2, 8)
        };
        // Ordinal 8 is in the gap above line 0 and below line 1.
        let out = gather_object(&set_of(&[8]), 0, &p);
        assert_eq!(out.iter().map(|w| w.count_ones()).sum::<u32>(), 0);
        // And ordinal 16 is line 1 bit 0.
        let out = gather_object(&set_of(&[16]), 0, &p);
        assert!(bit(&out, 1, 1, 0));
    }

    /// The seam. `span_bits() < 65536` does not imply chunk containment, and
    /// this object is the one that proves it.
    #[test]
    fn an_object_straddling_a_chunk_boundary_gathers_correctly() {
        let p = Packing::dense(1, 10_000);
        // Object 6 spans bits 60 000..70 000, crossing 65 536.
        let base = 6 * 10_000;
        assert_eq!(p.straddles(6), Some(true));
        let s = set_of(&[base, base + 5_535, base + 5_536, base + 9_999]);
        let out = gather_object(&s, 6, &p);
        assert!(bit(&out, p.line_words(), 0, 0));
        assert!(
            bit(&out, p.line_words(), 0, 5_535),
            "last below the boundary"
        );
        assert!(bit(&out, p.line_words(), 0, 5_536), "first above it");
        assert!(bit(&out, p.line_words(), 0, 9_999));
        assert_eq!(out.iter().map(|w| w.count_ones()).sum::<u32>(), 4);
    }

    /// The word-level sharpening of the same hazard: 65 536 is a multiple of 64,
    /// so under a stride that is not, a single destination **word** can cross the
    /// boundary. A reader that copied whole words per chunk would be wrong for
    /// exactly one word, and every test whose `line_bits` is a multiple of 64
    /// would still pass.
    #[test]
    fn a_single_word_crossing_the_chunk_boundary_gathers_correctly() {
        let p = Packing::dense(1, 100);
        // Object 655 begins at bit 65 500; its first word spans 65 500..65 564.
        let base = 655 * 100;
        assert_eq!(base, 65_500);
        let s = set_of(&[base + 35, base + 36, base + 99]);
        let out = gather_object(&s, 655, &p);
        assert!(bit(&out, p.line_words(), 0, 35));
        assert!(bit(&out, p.line_words(), 0, 36));
        assert!(bit(&out, p.line_words(), 0, 99));
        assert_eq!(out.iter().map(|w| w.count_ones()).sum::<u32>(), 3);
    }

    #[test]
    fn an_unwritten_object_gathers_as_all_zero() {
        let p = Packing::dense(2, 32);
        let out = gather_object(&set_of(&[64]), 9_999, &p);
        assert!(out.iter().all(|w| *w == 0));
    }
}
