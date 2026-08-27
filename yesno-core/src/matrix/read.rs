//! `OrdSet` -> [`BitMatrix`]: the matrix half of the gather.
//!
//! # The transfer itself lives in [`pack`](crate::pack)
//!
//! Where a matrix's bits sit in the ordinal space is a
//! [`Packing`](crate::pack::Packing), and moving them is
//! [`pack::gather`](crate::pack) with its seeking arm behind
//! `pack::try_gather`. Both are shared with [`bignum`](crate::bignum), because
//! an integer is the same construction at `lines == 1` and the two used to carry
//! a copy of this walk each.
//!
//! What is left here is the part that is genuinely about *matrices*: choosing
//! the destination shape, handling [`Order`], and taking the population count
//! from the chunk directory.
//!
//! # [`Order`] is normalized away before the transfer, not inside it
//!
//! [`Layout::line_len`] and [`Layout::line_count`] present a column-major layout
//! as lines of `rows` bits, so the gather fills the **transpose** and
//! [`OrdSet::read_matrix`] transposes afterwards. That is exact, reuses one
//! kernel, and gives a free identity worth testing: reading as `ColMajor` equals
//! reading as `RowMajor` and transposing.

use super::{BitMatrix, Layout, Order};
use crate::pack;
use crate::{split, OrdSet, CHUNK_CARD};

impl OrdSet {
    /// Gather matrix `k` into the canonical form.
    ///
    /// `None` if `layout` is not self-consistent ([`Layout::check`]) or if the
    /// matrix would reach past [`ORDINAL_MAX`](crate::ORDINAL_MAX) — `u64::MAX`
    /// is not an ordinal, so such a matrix is not addressable at all.
    ///
    /// A [`Order::ColMajor`] source is gathered line-by-line into the transpose
    /// and then transposed, which is exact and reuses one kernel. It gives a
    /// free identity worth testing: reading as `ColMajor` equals reading as
    /// `RowMajor` and transposing.
    pub fn read_matrix(&self, k: u64, layout: &Layout) -> Option<BitMatrix> {
        layout.check().ok()?;
        // The whole matrix must be addressable, which is exactly the condition
        // its last line's last bit imposes.
        let base = layout.base_of(k)?;
        let last = base.checked_add(layout.span_bits() - 1)?;
        if last > crate::ORDINAL_MAX {
            return None;
        }
        // Under ColMajor a line is a column, so scattering lines as rows builds
        // the transpose directly.
        let (dr, dc) = match layout.order {
            Order::RowMajor => (layout.rows, layout.cols),
            Order::ColMajor => (layout.cols, layout.rows),
        };
        let mut out = BitMatrix::zeros(dr, dc);
        let packing = layout.packing();
        let words = out.stride();
        let dst = out.all_words_mut();
        // The seeking arm decides before it writes, so a decline leaves `out`
        // untouched and the generic path starts from a clean zero matrix.
        if !pack::try_gather(self, base, &packing, dst, words) {
            pack::gather(self, base, &packing, dst, words);
        }
        debug_assert!(out.tail_is_clear());
        // The population count comes from the chunk directory, not from the
        // matrix — see `chunk_aligned_ones`. This is the whole payoff of the
        // set and the matrix being the same object: reading a matrix tells you
        // its density for free, and `transpose_prefers_scatter` then needs no
        // estimate at all.
        if let Some(n) = self.chunk_aligned_ones(base, layout) {
            out.set_known_ones(n);
        }
        Some(match layout.order {
            Order::RowMajor => out,
            Order::ColMajor => out.transpose(),
        })
    }

    /// Set bits of the matrix at `base`, summed from the **chunk directory**
    /// rather than from its contents — or `None` if the layout does not permit
    /// it.
    ///
    /// # Why this is free, and when
    ///
    /// `Container::len()` is `O(1)` on every representation, which the crate
    /// treats as load-bearing ( QG §2 ) precisely so that cardinality questions
    /// need not touch a payload. When a matrix covers **whole chunks exactly** —
    /// densely packed, starting on a chunk boundary, spanning a whole number of
    /// them — its population is the sum of those containers' lengths, and no
    /// payload is read.
    ///
    /// `Layout::dense(256, 256)` is exactly that case: 65 536 bits, one chunk,
    /// one container. It is the shape the module header names as the reason the
    /// chunk geometry cooperates, and this is where that pays.
    ///
    /// `None` for a padded stride, an unaligned base, or a partial chunk —
    /// there the count would have to be derived from the payload, which is what
    /// this exists to avoid.
    fn chunk_aligned_ones(&self, base: u64, layout: &Layout) -> Option<u64> {
        let span = layout.span_bits();
        let chunk = CHUNK_CARD as u64;
        if layout.line_stride != layout.line_len()
            || !base.is_multiple_of(chunk)
            || !span.is_multiple_of(chunk)
        {
            return None;
        }
        let (first, _) = split(base);
        let chunks = span / chunk;

        let n = self.chunk_count();
        let mut i = self.partition_point_in(0, n, first);
        let mut total = 0u64;
        while i < n {
            let Some((p, c)) = self.chunk_at(i) else {
                break;
            };
            if p >= first + chunks {
                break;
            }
            total += c.len() as u64;
            i += 1;
        }
        Some(total)
    }

    /// How many matrices this set's occupied span reaches into under `layout`.
    ///
    /// Counts addressable positions, not non-empty matrices: an all-zero matrix
    /// below the highest set ordinal is still counted, because a zero matrix is
    /// a legitimate value and absence cannot distinguish it from one.
    pub fn matrix_count(&self, layout: &Layout) -> u64 {
        layout.packing().count_below(self.max())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matrix::MatrixSink;

    fn set_of(v: &[u64]) -> OrdSet {
        OrdSet::from_iter_unsorted(v.iter().copied())
    }

    #[test]
    fn a_dense_row_major_matrix_reads_back_in_order() {
        // 2x3 dense: ordinals 0..6 laid out row by row.
        let l = Layout::dense(2, 3);
        let s = set_of(&[0, 2, 4]);
        let m = s.read_matrix(0, &l).unwrap();
        assert!(m.get(0, 0) && !m.get(0, 1) && m.get(0, 2));
        assert!(!m.get(1, 0) && m.get(1, 1) && !m.get(1, 2));
    }

    #[test]
    fn the_matrix_index_selects_by_stride() {
        let l = Layout::dense(2, 3);
        // Matrix 1 lives at ordinals 6..12.
        let s = set_of(&[6, 11]);
        let m0 = s.read_matrix(0, &l).unwrap();
        assert_eq!(m0.count_ones(), 0);
        let m1 = s.read_matrix(1, &l).unwrap();
        assert!(m1.get(0, 0) && m1.get(1, 2));
        assert_eq!(m1.count_ones(), 2);
    }

    #[test]
    fn a_padded_line_stride_skips_the_gap() {
        let l = Layout {
            line_stride: 8,
            matrix_stride: 16,
            ..Layout::dense(2, 3)
        };
        // Bits 3..8 are padding and must not appear anywhere in the matrix.
        let s = set_of(&[0, 3, 4, 7, 8]);
        let m = s.read_matrix(0, &l).unwrap();
        assert!(m.get(0, 0), "bit 0 is (0,0)");
        assert!(m.get(1, 0), "bit 8 is (1,0)");
        assert_eq!(m.count_ones(), 2, "the padding bits 3,4,7 are not elements");
    }

    #[test]
    fn col_major_reads_the_same_bits_the_other_way_round() {
        // Both layouts pack 15 bits densely; they disagree only on which index
        // moves fastest. RowMajor: o = r*5 + c. ColMajor: o = c*3 + r.
        let rm = Layout::dense(3, 5);
        let cm = Layout {
            order: Order::ColMajor,
            line_stride: 3,
            matrix_stride: 15,
            ..Layout::dense(3, 5)
        };
        let picks = [0u64, 1, 4, 7, 9, 14];
        let s = set_of(&picks);

        let a = s.read_matrix(0, &rm).unwrap();
        let b = s.read_matrix(0, &cm).unwrap();
        // Both yield a rows x cols matrix; only the addressing differs.
        assert_eq!((a.rows(), a.cols()), (3, 5));
        assert_eq!((b.rows(), b.cols()), (3, 5));

        let mut expect_rm = BitMatrix::zeros(3, 5);
        let mut expect_cm = BitMatrix::zeros(3, 5);
        for &o in &picks {
            expect_rm.set((o / 5) as u32, (o % 5) as u32, true);
            expect_cm.set((o % 3) as u32, (o / 3) as u32, true);
        }
        assert_eq!(a, expect_rm);
        assert_eq!(b, expect_cm);
    }

    #[test]
    fn a_matrix_straddling_a_chunk_boundary_reads_correctly() {
        // 100x100 dense = 10 000 bits, so matrix 6 spans 60 000..70 000 and
        // crosses 65 536. This is the case `M*N < 65536` does NOT protect
        // against: straddling follows from matrix_stride, not from matrix size.
        let l = Layout::dense(100, 100);
        let base = 6 * 10_000u64;
        assert!(base < 65_536 && base + 10_000 > 65_536, "must straddle");
        // Last ordinal of the low chunk, and the first of the high one.
        let s = set_of(&[base, 65_535, 65_536, base + 9_999]);
        let m = s.read_matrix(6, &l).unwrap();
        assert_eq!(m.count_ones(), 4);
        assert!(m.get(0, 0), "first bit of the matrix");
        assert!(m.get(99, 99), "last bit, in the next chunk");
        for o in [65_535u64, 65_536] {
            let off = o - base;
            assert!(
                m.get((off / 100) as u32, (off % 100) as u32),
                "the seam bit at {o}"
            );
        }
    }

    #[test]
    fn a_line_straddling_a_word_boundary_reads_correctly() {
        // 3 rows of 100 bits, densely packed: row 1 starts at bit 100, which is
        // neither a word nor a chunk boundary.
        let l = Layout::dense(3, 100);
        let s = set_of(&[100, 163, 164, 199]);
        let m = s.read_matrix(0, &l).unwrap();
        assert!(m.get(1, 0) && m.get(1, 63) && m.get(1, 64) && m.get(1, 99));
        assert_eq!(m.count_ones(), 4);
        assert!(m.tail_is_clear());
    }

    #[test]
    fn reading_past_the_ordinal_ceiling_is_none() {
        let l = Layout::dense(1, 2);
        let s = OrdSet::new();
        assert!(s.read_matrix(u64::MAX / 2, &l).is_none());
    }

    #[test]
    fn an_invalid_layout_is_none() {
        let s = OrdSet::new();
        let bad = Layout {
            line_stride: 1,
            ..Layout::dense(2, 5)
        };
        assert!(s.read_matrix(0, &bad).is_none());
    }

    #[test]
    fn an_absent_chunk_reads_as_the_zero_matrix() {
        let l = Layout::dense(8, 8);
        let s = set_of(&[0]);
        // Matrix 500 is far past anything stored.
        let m = s.read_matrix(500, &l).unwrap();
        assert_eq!(m.count_ones(), 0);
        assert_eq!(m.rows(), 8);
    }

    #[test]
    fn a_full_chunk_reads_as_the_all_ones_matrix() {
        // 256x256 is exactly one chunk, so a full container is an all-ones
        // matrix — the fast-path shape.
        let l = Layout::dense(256, 256);
        let s = OrdSet::from_iter_unsorted(0..65_536u64);
        let m = s.read_matrix(0, &l).unwrap();
        assert_eq!(m.count_ones(), 65_536);
        assert!(m.tail_is_clear());
    }

    #[test]
    fn every_container_kind_reaches_the_reader() {
        use crate::ContainerKind;
        // Array (sparse), bitmap (dense-ish), run (contiguous) in three chunks.
        let mut vals: Vec<u64> = (0..100u64).map(|i| i * 13).collect();
        vals.extend((1 << 16..(1 << 16) + 6000).map(|i| i * 2 % 65_536 + (1 << 16)));
        vals.extend((2 << 16)..(2 << 16) + 40_000);
        let mut s = OrdSet::from_iter_unsorted(vals.iter().copied());
        s.optimize();
        let kinds: Vec<ContainerKind> = s.chunks().map(|(_, c)| c.kind()).collect();
        assert!(kinds.contains(&ContainerKind::Array), "{kinds:?}");
        assert!(kinds.contains(&ContainerKind::Run), "{kinds:?}");

        // Read a matrix out of each chunk and check it against membership.
        let l = Layout::dense(256, 256);
        for k in 0..3u64 {
            let m = s.read_matrix(k, &l).unwrap();
            for (r, c) in [(0u32, 0u32), (7, 13), (255, 255), (128, 64)] {
                let o = l.ordinal_at(k, r, c).unwrap();
                assert_eq!(m.get(r, c), s.contains(o), "k={k} ({r},{c}) o={o}");
            }
            assert_eq!(
                m.count_ones(),
                (0..65_536u64)
                    .filter(|&i| s.contains(k * 65_536 + i))
                    .count() as u64
            );
        }
    }

    /// The payoff of the set and the matrix being the same object: reading a
    /// matrix tells you its density **from the chunk directory**, with no
    /// payload counted. `Layout::dense(256, 256)` is exactly one chunk.
    #[test]
    fn a_chunk_aligned_read_learns_its_density_from_the_directory() {
        let l = Layout::dense(256, 256);
        let mut s = OrdSet::from_iter_unsorted((0..65_536u64).step_by(3));
        s.optimize();
        let want = s.len();

        let m = s.read_matrix(0, &l).unwrap();
        assert_eq!(
            m.known_ones(),
            Some(want),
            "a chunk-aligned read must carry the count"
        );
        assert_eq!(m.count_ones(), want);

        // And the dispatch it feeds is then exact rather than sampled.
        assert_eq!(
            m.transpose_prefers_scatter(),
            want * 24 < 256 * 256,
            "the dispatch must use the exact count"
        );

        // An empty matrix in the same series, without reading a payload.
        let empty = s.read_matrix(5, &l).unwrap();
        assert_eq!(empty.known_ones(), Some(0));
    }

    /// And it declines where it would have to read the payload to know.
    #[test]
    fn an_unaligned_or_padded_layout_carries_no_count() {
        let mut s = OrdSet::from_iter_unsorted(0..200_000u64);
        s.optimize();

        // Padded stride: the padding is not part of the matrix, so the chunk's
        // length is not the matrix's population.
        let padded = Layout::word_aligned(3, 100);
        assert_eq!(s.read_matrix(0, &padded).unwrap().known_ones(), None);

        // A partial chunk: 100x100 is 10 000 bits, not a whole chunk.
        let small = Layout::dense(100, 100);
        assert_eq!(s.read_matrix(0, &small).unwrap().known_ones(), None);

        // But the answers are still right, computed on demand.
        assert_eq!(
            s.read_matrix(0, &small).unwrap().count_ones(),
            10_000,
            "a dense prefix fills the whole matrix"
        );
    }

    /// The directory sum must agree with counting the bits, on every container
    /// kind — an `Array`, a `Bitmap` and a `Run` all report `len()` differently
    /// underneath.
    #[test]
    fn the_directory_count_agrees_with_the_payload_count() {
        use crate::ContainerKind;
        let l = Layout::dense(256, 256);
        let mut vals: Vec<u64> = (0..900u64).map(|i| i * 71).collect();
        vals.extend((65_536..65_536 + 40_000u64).map(|i| i * 3 % 65_536 + 65_536));
        vals.extend(131_072..131_072 + 50_000);
        let mut s = OrdSet::from_iter_unsorted(vals);
        s.optimize();
        let kinds: Vec<ContainerKind> = s.chunks().map(|(_, c)| c.kind()).collect();
        assert!(kinds.len() >= 3, "need three chunks, got {kinds:?}");

        for k in 0..4u64 {
            let m = s.read_matrix(k, &l).unwrap();
            let tracked = m.known_ones().expect("chunk-aligned");
            let counted: u64 = (0..256)
                .flat_map(|r| (0..256).map(move |c| (r, c)))
                .filter(|&(r, c)| m.get(r, c))
                .count() as u64;
            assert_eq!(tracked, counted, "k={k} kinds={kinds:?}");
        }
    }

    #[test]
    fn matrix_count_reaches_the_highest_set_ordinal() {
        let l = Layout::dense(8, 8);
        assert_eq!(OrdSet::new().matrix_count(&l), 0);
        assert_eq!(set_of(&[0]).matrix_count(&l), 1);
        assert_eq!(set_of(&[63]).matrix_count(&l), 1);
        assert_eq!(set_of(&[64]).matrix_count(&l), 2);
        assert_eq!(set_of(&[0, 1000]).matrix_count(&l), 1000 / 64 + 1);
    }

    /// Does matrix `k` cross a 65536-bit chunk boundary under `l`?
    fn straddles(l: &Layout, k: u64) -> bool {
        let base = l.base_of(k).unwrap();
        split(base).0 != split(base + l.span_bits() - 1).0
    }

    #[test]
    fn round_trip_through_the_sink_is_the_identity() {
        // Word boundaries from both sides, both orders, dense and padded
        // strides, and a matrix that is not a whole number of words wide.
        let layouts = [
            Layout::dense(3, 5),
            Layout::dense(1, 1),
            Layout::dense(64, 64),
            Layout::dense(65, 63),
            Layout::dense(100, 100),
            Layout::dense(256, 256),
            Layout::word_aligned(3, 100),
            Layout::word_aligned(9, 8),
            Layout {
                line_stride: 200,
                matrix_stride: 1 << 17,
                ..Layout::dense(4, 70)
            },
            Layout {
                order: Order::ColMajor,
                line_stride: 4,
                matrix_stride: 20,
                ..Layout::dense(4, 5)
            },
            Layout {
                order: Order::ColMajor,
                line_stride: 100,
                matrix_stride: 10_000,
                ..Layout::dense(100, 100)
            },
        ];
        // A straddling `k` must actually occur, or the seam path is untested
        // while every assertion below still passes. Sabotaging the reader to
        // stop at the first chunk is what showed this was missing.
        let mut saw_straddle = false;

        for l in layouts {
            let mut src = BitMatrix::zeros(l.rows, l.cols);
            let mut n = 0u32;
            for r in 0..l.rows {
                for c in 0..l.cols {
                    // A deterministic, irregular pattern.
                    if (r as u64 * 7 + c as u64 * 5).is_multiple_of(3) {
                        src.set(r, c, true);
                        n += 1;
                    }
                }
            }
            for k in [0u64, 1, 6, 7, 13] {
                saw_straddle |= straddles(&l, k);
                let mut sink = MatrixSink::new(l);
                sink.place(k, &src).unwrap();
                let set = sink.build();
                assert_eq!(set.len(), n as u64, "layout {l:?} k={k}");
                let back = set.read_matrix(k, &l).unwrap();
                assert_eq!(back, src, "layout {l:?} k={k}");
                assert!(back.tail_is_clear(), "layout {l:?} k={k}");
                // Neighbours must be untouched: a scatter that overran would
                // otherwise be invisible here.
                assert_eq!(
                    set.read_matrix(k + 1, &l).unwrap().count_ones(),
                    0,
                    "layout {l:?} k={k} bled into k+1"
                );
            }
        }
        assert!(
            saw_straddle,
            "no case crossed a chunk boundary; the seam path went untested"
        );
    }
}
