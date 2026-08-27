//! The seeking arm: the specialized alternative to [`super::gather`].
//!
//! # What the benchmark actually asked for
//!
//! The generic gather merges the source ordinals against the lines' ranges in
//! one pass, which is `O(nnz + lines + chunks)` and correct for every packing.
//! But a `Container` iterates from its first value and cannot seek, so an object
//! that does not start on a chunk boundary discards every value below it.
//! Measured on the same 10 000 bits read out of the same set:
//!
//! ```text
//!   contained,  object 0 at bit 0        19.6 us
//!   straddling, object 6 at bit 60 000  120.7 us   6.2x
//! ```
//!
//! **So the prize is skipping the prefix, not a `memcpy`** — which is the
//! opposite of what "add an aligned fast path" suggests, and is why this arm
//! seeks per line rather than special-casing one aligned packing. Seeking costs
//! `O(log)` per line for an array or run and `O(1)` for a bitmap, against
//! `O(values below the line)` for the generic walk.
//!
//! The second win comes free once the source is addressed rather than iterated:
//! a bitmap line becomes a **bit-block transfer** and a run line becomes a
//! **range fill**, both independent of how populated the container is, where the
//! generic path pays per set bit.
//!
//! # This declines rather than lies
//!
//! [`BitStore::try_words`](crate::buffer::BitStore::try_words) returns `None`
//! for a buffer that is not 8-byte aligned, which an mmap-backed container
//! legitimately is. There is no bit-addressed fallback here: the arm returns
//! `false` and the caller uses [`super::gather`], which is always correct.
//! The generic path is never deleted because this one is faster — same
//! contract as [`ops::generic`](crate::ops::generic).

use super::Packing;
use crate::container::Container;
use crate::{chunk_base, split, OrdSet, CHUNK_CARD};

/// Copy `nbits` from `src` starting at bit `src_start` into `dst` starting at
/// bit `dst_start`, OR-ing into whatever is there.
///
/// A bit-block transfer: each step moves as many bits as fit in both the source
/// and destination words, so it costs about two iterations per output word
/// regardless of how the two offsets line up.
fn blit(src: &[u64], src_start: usize, dst: &mut [u64], dst_start: usize, nbits: usize) {
    let mut i = 0usize;
    while i < nbits {
        let (s, d) = (src_start + i, dst_start + i);
        let (sw, sb) = (s >> 6, s & 63);
        let (dw, db) = (d >> 6, d & 63);
        let take = (64 - sb).min(64 - db).min(nbits - i);
        let mask = if take == 64 {
            u64::MAX
        } else {
            (1u64 << take) - 1
        };
        dst[dw] |= ((src[sw] >> sb) & mask) << db;
        i += take;
    }
}

/// Set `len` bits of `dst` starting at bit `start`.
fn set_range(dst: &mut [u64], start: usize, len: usize) {
    let mut i = 0usize;
    while i < len {
        let d = start + i;
        let (dw, db) = (d >> 6, d & 63);
        let take = (64 - db).min(len - i);
        let mask = if take == 64 {
            u64::MAX
        } else {
            ((1u64 << take) - 1) << db
        };
        dst[dw] |= mask;
        i += take;
    }
}

/// Copy the part of `c` covering chunk-local `[lo, hi]` into `dst`, placing
/// local value `lo` at `dst` bit `dst_base`.
///
/// `cursor` is a position in an array container's value slice that the caller
/// carries across lines.
///
/// `false` if this container cannot be addressed — see the module header.
fn copy_chunk_span(
    c: &Container,
    lo: u16,
    hi: u16,
    dst_base: usize,
    dst: &mut [u64],
    cursor: &mut usize,
) -> bool {
    debug_assert!(lo <= hi);
    match c {
        Container::Bitmap(b) => {
            let Some(words) = b.bits.try_words() else {
                return false;
            };
            blit(
                words,
                lo as usize,
                dst,
                dst_base,
                hi as usize - lo as usize + 1,
            );
            true
        }
        Container::Array(a) => {
            let vals = a.as_slice();
            // Advance the carried cursor rather than searching from the front.
            //
            // This is what a per-line `partition_point` cost: `O(lines·log V)`
            // against the generic merge's `O(V)`, which lost 2.3x on a 256x256
            // read out of a 1 000-value array. A *linear* advance from where the
            // previous line stopped is `O(V)` in total across every line, so the
            // seek is amortized to nothing and arrays stop being a special case.
            //
            // The caller resets the cursor whenever the chunk changes, which is
            // exactly when `lo` may go backwards — see `try_gather`.
            let mut i = *cursor;
            while i < vals.len() && vals[i] < lo {
                i += 1;
            }
            *cursor = i;
            while i < vals.len() {
                let v = vals[i];
                if v > hi {
                    break;
                }
                let bit = dst_base + (v - lo) as usize;
                dst[bit >> 6] |= 1u64 << (bit & 63);
                i += 1;
            }
            true
        }
        Container::Run(r) => {
            let n = r.nruns();
            // Intervals are sorted and disjoint, so seek to the first one whose
            // end reaches `lo`.
            //
            // The search is on `end`, not on `start`. An interval that begins
            // before `lo` and ends after it still covers part of the span, and
            // searching on `start` would skip exactly that one — the interval
            // most likely to matter, since it is the one straddling the seek
            // point.
            let (mut a, mut b) = (0u32, n);
            while a < b {
                let mid = a + (b - a) / 2;
                if r.end(mid) < lo {
                    a = mid + 1;
                } else {
                    b = mid;
                }
            }
            let mut i = a;
            while i < n {
                let (s, e) = (r.start(i), r.end(i));
                if s > hi {
                    break;
                }
                let s = s.max(lo);
                let e = e.min(hi);
                if s <= e {
                    // A whole interval at once: this is where a run container
                    // stops costing one step per value.
                    set_range(dst, dst_base + (s - lo) as usize, (e - s) as usize + 1);
                }
                i += 1;
            }
            true
        }
    }
}

/// Can `c` be addressed at all?
///
/// Only one thing cannot: a bitmap whose buffer is not 8-byte aligned, which an
/// mmap-backed container legitimately is. There is no bit-addressed word view,
/// and inventing one here would duplicate the generic path.
///
/// **Array containers used to decline too, and removing that exception is
/// worth more than it looks.** With a per-line `partition_point` an array paid
/// `O(lines · log V)` against the generic merge's `O(V)`, and reading a 256×256
/// object out of a 1 000-value array **regressed 2.23 µs → 5.03 µs**. The
/// carried cursor in `copy_chunk_span` made that seek `O(V)` in total.
///
/// **But on a pure-array read the cursor only draws with the generic merge**
/// ( 1.85 µs against 1.66–2.23 µs across runs — inside the noise ). Measuring
/// only that would have said the change bought nothing. The case it is actually
/// for is a set whose chunks are **different kinds**, because a decline is
/// all-or-nothing for the whole read: one array chunk in the span used to send
/// the bitmap chunk to the generic path as well.
///
/// ```text
///   512x256 over one array chunk and one bitmap chunk
///     arrays decline   71.74 us
///     array cursor      5.08 us    14.1x
/// ```
///
/// So the exception is gone, and with it a rule that would have had to be kept
/// in step with the kernel.
fn can_seek(c: &Container) -> bool {
    match c {
        Container::Bitmap(b) => b.bits.try_words().is_some(),
        Container::Array(_) | Container::Run(_) => true,
    }
}

/// Fill `out` line by line, seeking into each container rather than walking it.
///
/// Same destination convention as [`super::gather`]: line `l` occupies
/// `out[l * out_line_words .. (l + 1) * out_line_words]`.
///
/// `false` if any container in the span declines, in which case nothing has been
/// written and the caller uses the generic path.
pub(crate) fn try_gather(
    set: &OrdSet,
    base: u64,
    packing: &Packing,
    out: &mut [u64],
    out_line_words: usize,
) -> bool {
    let line_bits = packing.line_bits;
    let lines = packing.lines;
    if lines == 0 || line_bits == 0 {
        return true;
    }
    debug_assert!(out_line_words >= packing.line_words());
    debug_assert!(out.len() >= lines as usize * out_line_words);
    let n = set.chunk_count();

    // Decide before writing anything. The alternative — discovering a declining
    // container half way through — throws away work already done and makes the
    // caller re-zero the destination.
    let last = base + (packing.span_bits() - 1);
    let (first_p, _) = split(base);
    let (last_p, _) = split(last);
    let mut i = set.partition_point_in(0, n, first_p);
    while i < n {
        let Some((p, c)) = set.chunk_at(i) else { break };
        if p > last_p {
            break;
        }
        if !can_seek(c) {
            return false;
        }
        i += 1;
    }

    // Carried across lines so an array is scanned once in total rather than
    // once per line. Valid because the chunk index is globally non-decreasing —
    // each line's first chunk is at or after the previous line's — and within
    // one chunk successive lines ask for a non-decreasing `lo`. It is reset the
    // moment the chunk changes, which is the only point either could go
    // backwards.
    let mut cursor_chunk = usize::MAX;
    let mut cursor = 0usize;

    for line in 0..lines {
        let lo = base + line as u64 * packing.line_stride as u64;
        let hi = lo + line_bits as u64 - 1;
        let (p_lo, l_lo) = split(lo);
        let (p_hi, l_hi) = split(hi);
        let dst = &mut out[line as usize * out_line_words..][..out_line_words];

        let mut i = set.partition_point_in(0, n, p_lo);
        while i < n {
            let Some((p, c)) = set.chunk_at(i) else { break };
            if p > p_hi {
                break;
            }
            // Clip the line to this chunk. Only the first and last chunk of a
            // line are partial; a line spanning three chunks covers the middle
            // one entirely.
            let s = if p == p_lo { l_lo } else { 0 };
            let e = if p == p_hi {
                l_hi
            } else {
                (CHUNK_CARD - 1) as u16
            };
            if i != cursor_chunk {
                cursor_chunk = i;
                cursor = 0;
            }
            let dst_base = (chunk_base(p) + s as u64 - lo) as usize;
            if !copy_chunk_span(c, s, e, dst_base, dst, &mut cursor) {
                return false;
            }
            i += 1;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pack::gather;
    use crate::ContainerKind;

    /// Boundary-biased sources, chosen so every container kind is reached.
    fn sources() -> Vec<(&'static str, OrdSet)> {
        let mut v: Vec<(&'static str, OrdSet)> = vec![
            ("empty", OrdSet::new()),
            (
                "sparse_array",
                OrdSet::from_iter_unsorted((0..900u64).map(|i| i * 71)),
            ),
            (
                "clustered",
                OrdSet::from_iter_unsorted((0..5000u64).map(|i| (i / 10) * 97 + i % 10)),
            ),
            ("one_full_chunk", OrdSet::from_iter_unsorted(0..65_536u64)),
            ("three_chunks", OrdSet::from_iter_unsorted(0..200_000u64)),
            (
                "runny",
                OrdSet::from_iter_unsorted((0..300u64).flat_map(|i| (i * 500)..(i * 500 + 137))),
            ),
            (
                "at_the_ceiling",
                OrdSet::from_iter_unsorted([0u64, 1, 65_535, 65_536, 131_071, crate::ORDINAL_MAX]),
            ),
            (
                "dense_half",
                OrdSet::from_iter_unsorted((0..200_000u64).filter(|i| i.is_multiple_of(2))),
            ),
        ];
        for (_, s) in v.iter_mut() {
            s.optimize();
        }
        v
    }

    /// Deliberately mixes contained and **straddling** packings. A generator
    /// that only produced chunk-contained objects would leave the seam untested
    /// while every assertion below still passed.
    fn packings() -> Vec<Packing> {
        vec![
            Packing::dense(1, 1),
            Packing::dense(1, 64),
            Packing::dense(1, 100),
            Packing::dense(1, 10_000),
            Packing::dense(3, 5),
            Packing::dense(8, 8),
            Packing::dense(64, 64),
            Packing::dense(65, 63),
            Packing::dense(100, 100),
            Packing::dense(256, 256),
            Packing::word_aligned(3, 100),
            Packing::word_aligned(9, 8),
            Packing::word_aligned(1, 100),
            Packing::chunk_aligned(4, 70).unwrap(),
            Packing {
                line_stride: 200,
                object_stride: 1 << 17,
                ..Packing::dense(4, 70)
            },
            Packing {
                line_stride: 71,
                object_stride: 65_536,
                ..Packing::dense(7, 70)
            },
        ]
    }

    /// The test this arm exists to pass, and the reason the generic path is
    /// never deleted: two total functions over the same domain, diffed.
    #[test]
    fn the_seeking_arm_agrees_with_the_generic_gather() {
        let mut compared = 0u32;
        let mut nonempty = 0u32;
        let mut straddling = 0u32;
        let mut declined = 0u32;
        for (sname, set) in sources() {
            for p in packings() {
                for k in [0u64, 1, 6, 7, 13, 40, 655] {
                    if p.last_of(k).is_none() {
                        continue;
                    }
                    let base = p.base_of(k).unwrap();
                    let words = p.line_words();
                    let n = p.lines as usize * words;

                    let mut generic = vec![0u64; n];
                    gather(&set, base, &p, &mut generic, words);

                    let mut seeking = vec![0u64; n];
                    if !try_gather(&set, base, &p, &mut seeking, words) {
                        declined += 1;
                        // A decline must leave the destination untouched, or
                        // the caller's fallback would start from dirty state.
                        assert!(seeking.iter().all(|w| *w == 0), "{sname} k={k}");
                        continue;
                    }
                    assert_eq!(
                        generic, seeking,
                        "{sname} k={k} lines={} bits={} stride={}",
                        p.lines, p.line_bits, p.object_stride
                    );
                    compared += 1;
                    if generic.iter().any(|w| *w != 0) {
                        nonempty += 1;
                    }
                    if p.straddles(k) == Some(true) {
                        straddling += 1;
                    }
                }
            }
        }
        // A comparison of two all-zero buffers proves nothing, and a suite
        // that never reached a straddling object would pass while the seam was
        // broken. Assert the coverage rather than hoping for it.
        assert!(compared > 100, "compared only {compared}");
        assert!(nonempty > 50, "only {nonempty} non-empty comparisons");
        assert!(straddling > 10, "only {straddling} straddling objects");
        // No in-memory container declines any more, now that arrays seek.
        // The decline path needs an unaligned shared buffer and is covered by
        // `an_unaligned_bitmap_declines_and_the_generic_path_answers`.
        assert_eq!(declined, 0, "an in-memory container declined unexpectedly");
    }

    #[test]
    fn every_container_kind_is_actually_reached() {
        // The three arms of `copy_chunk_span` are the point of this module; a
        // corpus that never produces a run container tests two thirds of it.
        let mut seen = Vec::new();
        for (_, s) in sources() {
            for (_, c) in s.chunks() {
                if !seen.contains(&c.kind()) {
                    seen.push(c.kind());
                }
            }
        }
        for k in [
            ContainerKind::Array,
            ContainerKind::Bitmap,
            ContainerKind::Run,
        ] {
            assert!(
                seen.contains(&k),
                "no {k:?} container in the corpus: {seen:?}"
            );
        }
    }

    #[test]
    fn blit_moves_bits_at_every_offset_pairing() {
        // The shift path, exercised directly at both aligned and misaligned
        // source and destination offsets.
        let src: Vec<u64> = vec![0xDEAD_BEEF_1234_5678, 0x0F0F_0F0F_F0F0_F0F0, u64::MAX];
        for src_start in [0usize, 1, 7, 63, 64, 65, 100] {
            for dst_start in [0usize, 1, 7, 63, 64, 65] {
                for nbits in [1usize, 7, 63, 64, 65, 80] {
                    if src_start + nbits > 192 {
                        continue;
                    }
                    let mut dst = vec![0u64; 8];
                    blit(&src, src_start, &mut dst, dst_start, nbits);
                    for i in 0..nbits {
                        let s = src_start + i;
                        let want = src[s >> 6] >> (s & 63) & 1;
                        let d = dst_start + i;
                        let got = dst[d >> 6] >> (d & 63) & 1;
                        assert_eq!(got, want, "s={src_start} d={dst_start} n={nbits} i={i}");
                    }
                    // Nothing outside the window may be touched.
                    for i in 0..dst_start {
                        assert_eq!(dst[i >> 6] >> (i & 63) & 1, 0, "wrote below the window");
                    }
                }
            }
        }
    }

    #[test]
    fn set_range_fills_exactly_its_window() {
        for start in [0usize, 1, 63, 64, 65, 130] {
            for len in [1usize, 2, 63, 64, 65, 129] {
                let mut dst = vec![0u64; 8];
                set_range(&mut dst, start, len);
                let total: u32 = dst.iter().map(|w| w.count_ones()).sum();
                assert_eq!(total as usize, len, "start={start} len={len}");
                for i in start..start + len {
                    assert_eq!(
                        dst[i >> 6] >> (i & 63) & 1,
                        1,
                        "start={start} len={len} i={i}"
                    );
                }
            }
        }
    }

    /// The one thing that still declines, built rather than assumed.
    ///
    /// **Both this arm's decline and the generic fallback behind it are
    /// unreachable through the page store**, and that is a property of the
    /// format rather than luck: every slot in every size class is 64-byte
    /// aligned ( asserted in the store's size-class ladder ), so a decoded
    /// bitmap always has words. This test therefore builds the buffer by hand.
    ///
    /// **Writing this test is what found `bitmap-iter-is-silently-empty-when-
    /// unaligned`.** `BitmapContainer::iter` used to obtain its words with
    /// `try_words().unwrap_or(&[])`, so the generic fallback answered *empty*
    /// for exactly the container this arm declines — the decline was correct and
    /// the safety net behind it was not. That is fixed ( `BitmapIter` now holds
    /// a `Cow` and decodes ), so this asserts the whole path end to end rather
    /// than only the half this module owns.
    #[test]
    fn an_unaligned_bitmap_declines_and_the_generic_path_answers() {
        use crate::buffer::BitStore;
        use crate::container::BitmapContainer;
        use crate::{Container, BITMAP_BYTES};

        // Every 8th bit set, laid out one byte past an aligned allocation.
        let mut bytes = vec![0u8; BITMAP_BYTES + 8];
        let mut n = 0u32;
        for v in (0u64..65_536).step_by(8) {
            bytes[1 + (v as usize >> 3)] |= 1 << (v & 7);
            n += 1;
        }
        let buf = arrow_buffer::Buffer::from_vec(bytes);
        let bits = BitStore::shared_from_bytes(&buf, 1).expect("room for a bitmap");
        assert!(
            bits.try_words().is_none(),
            "offset 1 must be unaligned, or this test proves nothing"
        );
        let c = Container::Bitmap(BitmapContainer { bits, len: n });
        assert!(!can_seek(&c), "an unaligned bitmap must decline");

        let set = OrdSet::from_chunks(vec![(0, c)]);
        let p = Packing::dense(256, 256);
        let words = p.line_words();
        let len = p.lines as usize * words;

        // The arm declines and writes nothing...
        let mut got = vec![0u64; len];
        assert!(!try_gather(&set, 0, &p, &mut got, words));
        assert!(got.iter().all(|w| *w == 0), "a decline must not write");

        // ...and the generic path behind it returns the right answer.
        let mut want = vec![0u64; len];
        gather(&set, 0, &p, &mut want, words);
        let total: u32 = want.iter().map(|w| w.count_ones()).sum();
        assert_eq!(total, n, "the fallback lost the payload");
        for v in (0u64..65_536).step_by(8) {
            let (l, b) = (v / 256, v % 256);
            let i = l as usize * words + (b as usize >> 6);
            assert_eq!(want[i] >> (b & 63) & 1, 1, "lost {v}");
        }
    }

    #[test]
    fn a_run_straddling_the_seek_point_is_not_skipped() {
        // The interval that begins before `lo` and ends after it is the one a
        // search on `start` would drop — and it is the likeliest to matter.
        let mut s = OrdSet::from_iter_unsorted(0..1000u64);
        s.optimize();
        assert_eq!(s.chunks().next().unwrap().1.kind(), ContainerKind::Run);
        // A line covering [500, 600) sits entirely inside one interval [0, 999].
        let p = Packing::dense(1, 100);
        let words = p.line_words();
        let mut got = vec![0u64; words];
        assert!(try_gather(&s, p.base_of(5).unwrap(), &p, &mut got, words));
        let total: u32 = got.iter().map(|w| w.count_ones()).sum();
        assert_eq!(total, 100, "the straddling interval was skipped");
    }
}
