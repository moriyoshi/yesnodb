//! argmin / argmax over a matrix, in the four senses that are meaningful when
//! the entries are bits, plus the counted product.
//!
//! | sense | what it is here |
//! |---|---|
//! | by weight | which row holds the most (or fewest) set bits |
//! | ranked | the same, as a top-k rather than a single extremum |
//! | per row, numpy `axis=1` | the index of a row's max (or min) entry |
//! | over a counted product | which cell of `A·B` has the most contributing paths |
//!
//! # The per-row sense is not degenerate, but it is asymmetric
//!
//! A row is a 0/1 vector, so its **argmax is the first set bit** — every set bit
//! attains the maximum and the convention is to report the first. Its
//! **argmin is the first clear bit**, by the same convention.
//!
//! **That asymmetry is where the bug lives.** The canonical form pads each
//! row to whole words with zeros, and a zero reads as "clear" — so an unmasked
//! search for the first clear bit reports one in the padding, and a completely
//! full row returns `cols` (or beyond) instead of `None`. The tail must be
//! *set* before complementing. `argmax` has no such problem, because the padding
//! is zero and it is looking for a one.
//!
//! # Ties
//!
//! Every reduction here breaks ties toward the **smallest index**, and that is
//! asserted rather than left to fall out of the iteration order — an unstated
//! tie-break is how a reduction becomes non-deterministic across a refactor.

use super::{tail_mask, BitMatrix};

impl BitMatrix {
    /// Set bits in each row.
    pub fn row_weights(&self) -> Vec<u32> {
        (0..self.rows())
            .map(|r| self.row_words(r).iter().map(|w| w.count_ones()).sum())
            .collect()
    }

    /// Set bits in each column.
    ///
    /// Via [`Self::transpose`], because a column is not contiguous in the
    /// canonical form and the transpose kernel is much cheaper than a
    /// per-element walk.
    pub fn col_weights(&self) -> Vec<u32> {
        self.transpose().row_weights()
    }

    /// The heaviest row and its weight. Ties go to the smallest row index.
    ///
    /// `None` only when there are no rows — an all-zero matrix still has a
    /// heaviest row, of weight zero.
    ///
    /// Allocates nothing, which is why it exists alongside
    /// [`Self::top_k_by_weight`].
    pub fn argmax_weight(&self) -> Option<(u32, u32)> {
        (0..self.rows())
            .map(|r| (r, self.row_words(r).iter().map(|w| w.count_ones()).sum()))
            .reduce(|best, cur| if cur.1 > best.1 { cur } else { best })
    }

    /// The lightest row and its weight. Ties go to the smallest row index.
    pub fn argmin_weight(&self) -> Option<(u32, u32)> {
        (0..self.rows())
            .map(|r| (r, self.row_words(r).iter().map(|w| w.count_ones()).sum()))
            .reduce(|best, cur| if cur.1 < best.1 { cur } else { best })
    }

    /// The `k` heaviest rows as `(row, weight)`, heaviest first.
    ///
    /// Ties go to the smallest row index. Returns `min(k, rows)` entries.
    ///
    /// This is a full sort, not a `k`-selection: it is `O(rows log rows)`
    /// whatever `k` is. That is deliberate for now — `rows` is bounded by the
    /// matrix, and a partial selection is worth adding only against a
    /// measurement. [`Self::argmax_weight`] is the allocation-free path for
    /// `k == 1`.
    pub fn top_k_by_weight(&self, k: usize) -> Vec<(u32, u32)> {
        let mut v: Vec<(u32, u32)> = self
            .row_weights()
            .into_iter()
            .enumerate()
            .map(|(r, w)| (r as u32, w))
            .collect();
        // Descending by weight, then ascending by row.
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        v.truncate(k);
        v
    }

    /// The `k` lightest rows as `(row, weight)`, lightest first.
    ///
    /// Ties go to the smallest row index. Returns `min(k, rows)` entries.
    pub fn bottom_k_by_weight(&self, k: usize) -> Vec<(u32, u32)> {
        let mut v: Vec<(u32, u32)> = self
            .row_weights()
            .into_iter()
            .enumerate()
            .map(|(r, w)| (r as u32, w))
            .collect();
        v.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));
        v.truncate(k);
        v
    }

    /// For each row, the column of its maximum entry — the **first set** bit.
    ///
    /// `None` for an all-zero row, which has no maximum to point at.
    pub fn argmax_by_row(&self) -> Vec<Option<u32>> {
        (0..self.rows())
            .map(|r| {
                // The padding tail is zero, so no set bit is ever out of range
                // and no masking is needed here.
                self.row_words(r)
                    .iter()
                    .enumerate()
                    .find(|(_, &w)| w != 0)
                    .map(|(wi, &w)| (wi * 64) as u32 + w.trailing_zeros())
            })
            .collect()
    }

    /// For each row, the column of its minimum entry — the **first clear** bit.
    ///
    /// `None` for a completely full row, which has no minimum to point at.
    ///
    /// The padding tail is zero and therefore reads as clear. It is forced to
    /// ones before the search, or a full row whose `cols` is not a multiple of
    /// 64 would report the first padding bit instead of `None`.
    pub fn argmin_by_row(&self) -> Vec<Option<u32>> {
        let last = self.stride().saturating_sub(1);
        let pad = !tail_mask(self.cols());
        (0..self.rows())
            .map(|r| {
                for (wi, &word) in self.row_words(r).iter().enumerate() {
                    let word = if wi == last { word | pad } else { word };
                    if word != u64::MAX {
                        return Some((wi * 64) as u32 + (!word).trailing_zeros());
                    }
                }
                None
            })
            .collect()
    }

    /// `counts[i][j] = |row i of self ∩ column j of rhs|` — the number of
    /// `i -> k -> j` paths, i.e. the integer matrix product.
    ///
    /// `None` if `self.cols() != rhs.rows()`.
    ///
    /// It needs `rhs` transposed so a column is contiguous, which is why this
    /// lives downstream of the transpose arm.
    ///
    /// **This is not "nearly free next to the boolean product", which is what
    /// this comment claimed until it was measured.** The two have different
    /// orders: `mul` is `O(nnz(A) · N/64)` because it skips A's zero bits, while
    /// this is `O(M · N · K/64)` because every cell needs a full word-AND over
    /// `K` bits whatever A contains. So the gap *is* A's density. Measured at
    /// 1/8 fill, where the model predicts 8×:
    ///
    /// ```text
    ///   n      counted_mul   boolean mul    ratio
    ///    64        3.98 us      596.3 ns     6.7x
    ///   128       21.47 us       2.88 us     7.5x
    ///   256      136.03 us      15.53 us     8.8x
    /// ```
    ///
    /// Carrying multiplicities costs the density factor. That is inherent —
    /// a count cannot be skipped the way a zero row can — and it is the reason
    /// to reach for `mul` when only reachability is wanted.
    pub fn counted_mul(&self, rhs: &BitMatrix) -> Option<Counts> {
        if self.cols() != rhs.rows() {
            return None;
        }
        let (m, n) = (self.rows(), rhs.cols());
        // Row j of `bt` is column j of `rhs`.
        let bt = rhs.transpose();
        let mut v = vec![0u32; m as usize * n as usize];
        for i in 0..m {
            let arow = self.row_words(i);
            for j in 0..n {
                let sum = arow
                    .iter()
                    .zip(bt.row_words(j))
                    .map(|(a, b)| (a & b).count_ones())
                    .sum();
                v[i as usize * n as usize + j as usize] = sum;
            }
        }
        Some(Counts {
            rows: m,
            cols: n,
            v,
        })
    }
}

/// The integer product of two bit matrices: how many paths reach each cell.
///
/// Bounded by the matrix rather than by the ordinal universe — `rows * cols`
/// `u32`s — which is what makes materializing it reasonable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Counts {
    rows: u32,
    cols: u32,
    v: Vec<u32>,
}

impl Counts {
    #[inline]
    pub fn rows(&self) -> u32 {
        self.rows
    }

    #[inline]
    pub fn cols(&self) -> u32 {
        self.cols
    }

    /// # Panics
    /// If `r >= rows` or `c >= cols`.
    #[inline]
    pub fn get(&self, r: u32, c: u32) -> u32 {
        assert!(r < self.rows && c < self.cols, "index out of range");
        self.v[r as usize * self.cols as usize + c as usize]
    }

    /// Row `r`'s counts, borrowed, in column order.
    ///
    /// [`Self::get`] asserts per cell, so reading a whole row through it pays a
    /// bounds check per element and cannot hand the row to anything that takes a
    /// slice. One check covers the row here.
    ///
    /// # Panics
    /// If `r >= rows`.
    #[inline]
    pub fn row(&self, r: u32) -> &[u32] {
        assert!(r < self.rows, "row index out of range");
        let n = self.cols as usize;
        let start = r as usize * n;
        &self.v[start..start + n]
    }

    /// Every row in order. `rows()` slices of `cols()` counts each.
    #[inline]
    pub fn rows_iter(&self) -> impl Iterator<Item = &[u32]> + '_ {
        self.v.chunks_exact(self.cols.max(1) as usize)
    }

    /// The cell with the most paths, as `(row, col, count)`.
    ///
    /// Ties go to the smallest row, then the smallest column. `None` only when
    /// there are no cells.
    pub fn argmax(&self) -> Option<(u32, u32, u32)> {
        self.cells()
            .reduce(|best, cur| if cur.2 > best.2 { cur } else { best })
    }

    /// The cell of row `r` with the most paths, as `(col, count)`.
    ///
    /// Ties go to the smallest column. `None` if `r` is out of range.
    pub fn argmax_in_row(&self, r: u32) -> Option<(u32, u32)> {
        if r >= self.rows {
            return None;
        }
        // Through `row`, not `get`: one bounds check for the row rather than
        // one per column. This was the in-tree instance of the complaint that
        // `row` exists to answer.
        self.row(r)
            .iter()
            .enumerate()
            .map(|(c, &n)| (c as u32, n))
            .reduce(|best, cur| if cur.1 > best.1 { cur } else { best })
    }

    /// The `k` cells with the most paths, as `(row, col, count)`, most first.
    ///
    /// Ties go to the smallest row, then the smallest column. Returns
    /// `min(k, rows * cols)` entries.
    pub fn top_k(&self, k: usize) -> Vec<(u32, u32, u32)> {
        let mut v: Vec<(u32, u32, u32)> = self.cells().collect();
        v.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)).then(a.1.cmp(&b.1)));
        v.truncate(k);
        v
    }

    /// Every cell as `(row, col, count)`, in row-major order.
    fn cells(&self) -> impl Iterator<Item = (u32, u32, u32)> + '_ {
        (0..self.rows).flat_map(move |r| (0..self.cols).map(move |c| (r, c, self.get(r, c))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matrix::Semiring;

    /// `row` must be exactly what `get` reports, cell for cell.
    ///
    /// The two are a parallel implementation of the same indexing — one asserts
    /// per cell and one asserts per row — so nothing but a differential check
    /// stops a transposed index in either.
    #[test]
    fn a_row_borrow_agrees_with_per_cell_indexing() {
        let a = patterned(7, 11, 1);
        let b = patterned(11, 5, 2);
        let counts = a.counted_mul(&b).unwrap();
        assert_eq!((counts.rows(), counts.cols()), (7, 5));

        for r in 0..counts.rows() {
            let row = counts.row(r);
            assert_eq!(row.len(), counts.cols() as usize);
            for c in 0..counts.cols() {
                assert_eq!(row[c as usize], counts.get(r, c), "cell ({r}, {c})");
            }
        }

        // Non-square on purpose: a `rows`/`cols` swap inside `row` survives a
        // square fixture and dies here.
        let by_iter: Vec<&[u32]> = counts.rows_iter().collect();
        assert_eq!(by_iter.len(), counts.rows() as usize);
        for (r, row) in by_iter.iter().enumerate() {
            assert_eq!(*row, counts.row(r as u32));
        }

        // And the reader that used to pay a check per cell still agrees.
        for r in 0..counts.rows() {
            let want = (0..counts.cols())
                .map(|c| (c, counts.get(r, c)))
                .reduce(|best, cur| if cur.1 > best.1 { cur } else { best });
            assert_eq!(counts.argmax_in_row(r), want, "argmax_in_row({r})");
        }
    }

    #[test]
    #[should_panic(expected = "row index out of range")]
    fn a_row_past_the_end_panics_rather_than_slicing_the_next_one() {
        let counts = patterned(3, 4, 1).counted_mul(&patterned(4, 3, 2)).unwrap();
        let _ = counts.row(3);
    }

    fn patterned(rows: u32, cols: u32, seed: u64) -> BitMatrix {
        let mut m = BitMatrix::zeros(rows, cols);
        for r in 0..rows {
            for c in 0..cols {
                if (r as u64 * 7 + c as u64 * 5 + seed * 11).is_multiple_of(3) {
                    m.set(r, c, true);
                }
            }
        }
        m
    }

    fn full(rows: u32, cols: u32) -> BitMatrix {
        let mut m = BitMatrix::zeros(rows, cols);
        for r in 0..rows {
            for c in 0..cols {
                m.set(r, c, true);
            }
        }
        m
    }

    const SHAPES: &[(u32, u32)] = &[
        (1, 1),
        (3, 5),
        (8, 8),
        (63, 63),
        (64, 64),
        (65, 65),
        (5, 100),
        (100, 5),
        (70, 130),
    ];

    #[test]
    fn row_weights_count_every_row() {
        for &(r, c) in SHAPES {
            let m = patterned(r, c, 0);
            let w = m.row_weights();
            assert_eq!(w.len(), r as usize, "{r}x{c}");
            // Against the element-by-element definition.
            for i in 0..r {
                let want = (0..c).filter(|&j| m.get(i, j)).count() as u32;
                assert_eq!(w[i as usize], want, "{r}x{c} row {i}");
            }
            assert_eq!(w.iter().map(|&x| x as u64).sum::<u64>(), m.count_ones());
        }
    }

    #[test]
    fn col_weights_count_every_column() {
        for &(r, c) in SHAPES {
            let m = patterned(r, c, 0);
            let w = m.col_weights();
            assert_eq!(w.len(), c as usize, "{r}x{c}");
            for j in 0..c {
                let want = (0..r).filter(|&i| m.get(i, j)).count() as u32;
                assert_eq!(w[j as usize], want, "{r}x{c} col {j}");
            }
        }
    }

    #[test]
    fn a_full_matrix_weighs_cols_per_row() {
        // The padding tail must not be counted.
        for &(r, c) in SHAPES {
            assert!(full(r, c).row_weights().iter().all(|&w| w == c), "{r}x{c}");
        }
    }

    #[test]
    fn argmax_and_argmin_by_weight_agree_with_the_weights() {
        for &(r, c) in SHAPES {
            let m = patterned(r, c, 0);
            let w = m.row_weights();
            let hi = *w.iter().max().unwrap();
            let lo = *w.iter().min().unwrap();
            assert_eq!(
                m.argmax_weight(),
                Some((w.iter().position(|&x| x == hi).unwrap() as u32, hi))
            );
            assert_eq!(
                m.argmin_weight(),
                Some((w.iter().position(|&x| x == lo).unwrap() as u32, lo))
            );
        }
    }

    #[test]
    fn weight_ties_go_to_the_smallest_row() {
        // Every row identical, so every row ties. The answer must be row 0, and
        // `position` above would not distinguish a last-wins implementation on a
        // patterned matrix where the extremum happens to be unique.
        let m = full(9, 7);
        assert_eq!(m.argmax_weight(), Some((0, 7)));
        assert_eq!(m.argmin_weight(), Some((0, 7)));
        assert_eq!(m.top_k_by_weight(3), vec![(0, 7), (1, 7), (2, 7)]);
        assert_eq!(m.bottom_k_by_weight(3), vec![(0, 7), (1, 7), (2, 7)]);
    }

    #[test]
    fn an_all_zero_matrix_still_has_an_extremum() {
        let m = BitMatrix::zeros(4, 5);
        assert_eq!(m.argmax_weight(), Some((0, 0)));
        assert_eq!(m.argmin_weight(), Some((0, 0)));
    }

    #[test]
    fn a_matrix_with_no_rows_has_none() {
        let m = BitMatrix::zeros(0, 5);
        assert_eq!(m.argmax_weight(), None);
        assert_eq!(m.argmin_weight(), None);
        assert!(m.top_k_by_weight(3).is_empty());
        assert!(m.argmax_by_row().is_empty());
    }

    #[test]
    fn top_k_is_the_sorted_order_and_argmax_is_its_head() {
        for &(r, c) in SHAPES {
            let m = patterned(r, c, 3);
            let top = m.top_k_by_weight(r as usize);
            assert_eq!(top.len(), r as usize, "{r}x{c}");
            // Descending, ties ascending by row.
            for pair in top.windows(2) {
                assert!(
                    pair[0].1 > pair[1].1 || (pair[0].1 == pair[1].1 && pair[0].0 < pair[1].0),
                    "{r}x{c} not ordered: {pair:?}"
                );
            }
            assert_eq!(Some(top[0]), m.argmax_weight(), "{r}x{c}");
            let bottom = m.bottom_k_by_weight(r as usize);
            assert_eq!(Some(bottom[0]), m.argmin_weight(), "{r}x{c}");
            // The two are reverses of each other up to the tie-break, so they
            // must at least agree on the multiset of weights.
            let mut a: Vec<u32> = top.iter().map(|x| x.1).collect();
            let mut b: Vec<u32> = bottom.iter().map(|x| x.1).collect();
            a.sort_unstable();
            b.sort_unstable();
            assert_eq!(a, b, "{r}x{c}");
        }
    }

    #[test]
    fn top_k_saturates_at_the_row_count() {
        let m = patterned(4, 5, 0);
        assert_eq!(m.top_k_by_weight(100).len(), 4);
        assert_eq!(m.top_k_by_weight(0).len(), 0);
    }

    #[test]
    fn argmax_by_row_is_the_first_set_bit() {
        for &(r, c) in SHAPES {
            let m = patterned(r, c, 1);
            let got = m.argmax_by_row();
            assert_eq!(got.len(), r as usize, "{r}x{c}");
            for i in 0..r {
                let want = (0..c).find(|&j| m.get(i, j));
                assert_eq!(got[i as usize], want, "{r}x{c} row {i}");
            }
        }
    }

    #[test]
    fn argmin_by_row_is_the_first_clear_bit() {
        for &(r, c) in SHAPES {
            let m = patterned(r, c, 1);
            let got = m.argmin_by_row();
            assert_eq!(got.len(), r as usize, "{r}x{c}");
            for i in 0..r {
                let want = (0..c).find(|&j| !m.get(i, j));
                assert_eq!(got[i as usize], want, "{r}x{c} row {i}");
            }
        }
    }

    #[test]
    fn a_full_row_has_no_argmin_even_when_cols_is_not_a_word() {
        // The regression this whole masking dance exists for. With cols = 100
        // a row occupies two words and 28 padding bits; unmasked, `!word` finds
        // a clear bit at column 100 and reports it as the minimum.
        //
        // Verified by sabotage: dropping the mask fails **only this test**.
        // `argmin_by_row_is_the_first_clear_bit` is blind to it, because a
        // patterned matrix has no completely full row and the padding is never
        // the first clear bit. A general property is not a substitute here.
        for c in [1u32, 5, 63, 64, 65, 100, 128, 129] {
            let m = full(3, c);
            assert_eq!(
                m.argmin_by_row(),
                vec![None, None, None],
                "a full row of {c} columns must have no clear bit"
            );
            // And argmax is still the first column.
            assert_eq!(
                m.argmax_by_row(),
                vec![Some(0), Some(0), Some(0)],
                "cols={c}"
            );
        }
    }

    #[test]
    fn an_empty_row_has_no_argmax_but_argmin_is_zero() {
        for c in [1u32, 63, 64, 100] {
            let m = BitMatrix::zeros(2, c);
            assert_eq!(m.argmax_by_row(), vec![None, None], "cols={c}");
            assert_eq!(m.argmin_by_row(), vec![Some(0), Some(0)], "cols={c}");
        }
    }

    #[test]
    fn a_row_full_except_its_last_column_finds_that_column() {
        // The clear bit sits in the padding word, adjacent to the padding — the
        // place a mask that is one bit wrong would miss it.
        for c in [65u32, 100, 128, 129] {
            let mut m = full(1, c);
            m.set(0, c - 1, false);
            assert_eq!(m.argmin_by_row(), vec![Some(c - 1)], "cols={c}");
        }
    }

    #[test]
    fn counted_mul_agrees_with_the_definition() {
        for &(m, k, n) in &[
            (1u32, 1u32, 1u32),
            (3, 4, 5),
            (8, 8, 8),
            (65, 63, 70),
            (5, 130, 3),
        ] {
            let a = patterned(m, k, 0);
            let b = patterned(k, n, 1);
            let counts = a.counted_mul(&b).unwrap();
            assert_eq!((counts.rows(), counts.cols()), (m, n));
            for i in 0..m {
                for j in 0..n {
                    let want = (0..k).filter(|&x| a.get(i, x) && b.get(x, j)).count() as u32;
                    assert_eq!(counts.get(i, j), want, "{m}x{k}x{n} ({i},{j})");
                }
            }
        }
    }

    #[test]
    fn a_positive_count_is_exactly_the_boolean_product() {
        // Ties the counter to the boolean kernel: they are two implementations
        // of "is there a path", and only this compares them.
        for &(m, k, n) in &[(3u32, 4u32, 5u32), (8, 8, 8), (65, 63, 70)] {
            let a = patterned(m, k, 0);
            let b = patterned(k, n, 1);
            let counts = a.counted_mul(&b).unwrap();
            let boolean = a.mul(&b, Semiring::Boolean).unwrap();
            let mut positives = 0u32;
            for i in 0..m {
                for j in 0..n {
                    assert_eq!(counts.get(i, j) > 0, boolean.get(i, j), "({i},{j})");
                    positives += u32::from(counts.get(i, j) > 0);
                }
            }
            assert!(positives > 0, "{m}x{k}x{n}: vacuous, nothing was reachable");
        }
    }

    #[test]
    fn the_parity_of_a_count_is_the_gf2_product() {
        // The third leg of the same triangle.
        let (m, k, n) = (12u32, 11u32, 13u32);
        let a = patterned(m, k, 0);
        let b = patterned(k, n, 1);
        let counts = a.counted_mul(&b).unwrap();
        let gf2 = a.mul(&b, Semiring::Gf2).unwrap();
        for i in 0..m {
            for j in 0..n {
                assert_eq!(counts.get(i, j) % 2 == 1, gf2.get(i, j), "({i},{j})");
            }
        }
    }

    #[test]
    fn counted_mul_by_the_identity_is_zero_or_one() {
        let a = patterned(9, 9, 0);
        let counts = a.counted_mul(&BitMatrix::identity(9)).unwrap();
        for i in 0..9 {
            for j in 0..9 {
                assert_eq!(counts.get(i, j), u32::from(a.get(i, j)));
            }
        }
    }

    #[test]
    fn counted_mul_rejects_a_shape_mismatch() {
        assert!(patterned(3, 4, 0)
            .counted_mul(&patterned(5, 6, 0))
            .is_none());
    }

    #[test]
    fn counts_argmax_finds_the_largest_and_breaks_ties_low() {
        let (m, k, n) = (7u32, 9u32, 6u32);
        let a = patterned(m, k, 0);
        let b = patterned(k, n, 1);
        let counts = a.counted_mul(&b).unwrap();

        let (br, bc, bv) = counts.argmax().unwrap();
        let want = (0..m)
            .flat_map(|i| (0..n).map(move |j| (i, j)))
            .map(|(i, j)| counts.get(i, j))
            .max()
            .unwrap();
        assert_eq!(bv, want);
        // Smallest (row, col) attaining it.
        let first = (0..m)
            .flat_map(|i| (0..n).map(move |j| (i, j)))
            .find(|&(i, j)| counts.get(i, j) == want)
            .unwrap();
        assert_eq!((br, bc), first);

        assert_eq!(counts.top_k(1), vec![(br, bc, bv)]);
        assert_eq!(counts.top_k(0).len(), 0);
        assert_eq!(counts.top_k(1000).len(), (m * n) as usize);
    }

    #[test]
    fn counts_top_k_is_descending() {
        let a = patterned(7, 9, 0);
        let b = patterned(9, 6, 1);
        let counts = a.counted_mul(&b).unwrap();
        let all = counts.top_k(usize::MAX);
        for pair in all.windows(2) {
            assert!(pair[0].2 >= pair[1].2, "not descending: {pair:?}");
        }
    }

    #[test]
    fn counts_argmax_in_row_matches_a_scan() {
        let a = patterned(7, 9, 0);
        let b = patterned(9, 6, 1);
        let counts = a.counted_mul(&b).unwrap();
        for i in 0..counts.rows() {
            let (c, v) = counts.argmax_in_row(i).unwrap();
            let want = (0..counts.cols()).map(|j| counts.get(i, j)).max().unwrap();
            assert_eq!(v, want, "row {i}");
            let first = (0..counts.cols())
                .find(|&j| counts.get(i, j) == want)
                .unwrap();
            assert_eq!(c, first, "row {i} tie-break");
        }
        assert_eq!(counts.argmax_in_row(counts.rows()), None);
    }
}
