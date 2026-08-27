//! Predicates, structural queries, and the cheap constructions.
//!
//! Nothing here is subtle; it is here because a caller writing each of these
//! inline would write the `stride`/tail arithmetic inline with it, and that is
//! the part that is easy to get wrong. Every one of them either reads whole
//! words — in which case the padding tail being zero is what makes the answer
//! right — or writes whole words, in which case it must re-mask.

use super::BitMatrix;

impl BitMatrix {
    /// No rows or no columns.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.rows() == 0 || self.cols() == 0
    }

    /// Same number of rows as columns. A `0 × 0` matrix is square.
    #[inline]
    pub fn is_square(&self) -> bool {
        self.rows() == self.cols()
    }

    /// Are all elements zero?
    ///
    /// Reads whole words, which is only correct because the padding tail is
    /// zero — a dirty tail would report a zero matrix as non-zero.
    pub fn is_zero(&self) -> bool {
        (0..self.rows()).all(|r| self.row_words(r).iter().all(|&w| w == 0))
    }

    /// Is any element set? The complement of [`Self::is_zero`].
    #[inline]
    pub fn any(&self) -> bool {
        !self.is_zero()
    }

    /// Are all elements set?
    #[inline]
    pub fn all(&self) -> bool {
        self.count_ones() == self.rows() as u64 * self.cols() as u64
    }

    /// Are no elements set? A synonym for [`Self::is_zero`], present because
    /// `any` / `all` / `none` are read as a set.
    #[inline]
    pub fn none(&self) -> bool {
        self.is_zero()
    }

    /// Elements that are clear. `rows * cols - count_ones()`.
    #[inline]
    pub fn count_zeros(&self) -> u64 {
        self.rows() as u64 * self.cols() as u64 - self.count_ones()
    }

    /// Is this the identity? `false` for a non-square matrix.
    pub fn is_identity(&self) -> bool {
        self.is_square() && self.count_ones() == self.rows() as u64 && self.trace() == self.rows()
    }

    /// Set elements on the main diagonal. Runs to `min(rows, cols)`, so a
    /// non-square matrix has a trace rather than an error.
    pub fn trace(&self) -> u32 {
        (0..self.rows().min(self.cols()))
            .filter(|&i| self.get(i, i))
            .count() as u32
    }

    /// Is `self == selfᵀ`? `false` for a non-square matrix.
    ///
    /// Checks only the strict upper triangle against the lower — comparing both
    /// halves would do every test twice and, worse, would pass for a matrix that
    /// disagrees with itself in a way the loop bounds hid.
    pub fn is_symmetric(&self) -> bool {
        if !self.is_square() {
            return false;
        }
        (0..self.rows()).all(|r| (0..r).all(|c| self.get(r, c) == self.get(c, r)))
    }

    /// Flip one element.
    ///
    /// # Panics
    /// If `r >= rows` or `c >= cols`.
    #[inline]
    pub fn flip(&mut self, r: u32, c: u32) {
        let v = self.get(r, c);
        self.set(r, c, !v);
    }

    /// Exchange two rows.
    ///
    /// # Panics
    /// If either index is out of range.
    pub fn swap_row_pair(&mut self, a: u32, b: u32) {
        assert!(a < self.rows() && b < self.rows(), "row index out of range");
        self.swap_rows(a, b);
    }

    /// Exchange two columns.
    ///
    /// Costs `O(rows)` bit operations, not the `O(stride)` word swap
    /// [`Self::swap_row_pair`] costs — a column is not contiguous in the
    /// canonical form. Transposing, swapping rows, and transposing back is
    /// cheaper for many columns at once.
    ///
    /// # Panics
    /// If either index is out of range.
    pub fn swap_col_pair(&mut self, a: u32, b: u32) {
        assert!(
            a < self.cols() && b < self.cols(),
            "column index out of range"
        );
        if a == b {
            return;
        }
        for r in 0..self.rows() {
            let (x, y) = (self.get(r, a), self.get(r, b));
            if x != y {
                self.set(r, a, y);
                self.set(r, b, x);
            }
        }
    }

    /// A copy of the `rows × cols` block whose top-left corner is `(top, left)`.
    ///
    /// A **copy**, not a view. A view would have to carry a bit offset into
    /// every row, which is exactly the seam the canonical form exists to keep
    /// out of the kernels — see the module header on `matrix`.
    ///
    /// `None` if the block does not fit.
    pub fn sub_matrix(&self, top: u32, left: u32, rows: u32, cols: u32) -> Option<BitMatrix> {
        if top.checked_add(rows)? > self.rows() || left.checked_add(cols)? > self.cols() {
            return None;
        }
        let mut out = BitMatrix::zeros(rows, cols);
        for r in 0..rows {
            for c in 0..cols {
                if self.get(top + r, left + c) {
                    out.set(r, c, true);
                }
            }
        }
        Some(out)
    }

    /// The lower triangle, including the diagonal; everything above is cleared.
    pub fn lower(&self) -> BitMatrix {
        self.triangle(true, true)
    }

    /// The lower triangle, excluding the diagonal.
    pub fn strictly_lower(&self) -> BitMatrix {
        self.triangle(true, false)
    }

    /// The upper triangle, including the diagonal.
    pub fn upper(&self) -> BitMatrix {
        self.triangle(false, true)
    }

    /// The upper triangle, excluding the diagonal.
    pub fn strictly_upper(&self) -> BitMatrix {
        self.triangle(false, false)
    }

    /// The strict lower triangle with the diagonal forced to ones — unit lower
    /// triangular, the `L` of an `LU` factorisation.
    pub fn unit_lower(&self) -> BitMatrix {
        let mut m = self.strictly_lower();
        m.set_diagonal();
        m
    }

    /// The strict upper triangle with the diagonal forced to ones.
    pub fn unit_upper(&self) -> BitMatrix {
        let mut m = self.strictly_upper();
        m.set_diagonal();
        m
    }

    /// Set every element of the main diagonal.
    pub fn set_diagonal(&mut self) {
        for i in 0..self.rows().min(self.cols()) {
            self.set(i, i, true);
        }
    }

    fn triangle(&self, lower: bool, diagonal: bool) -> BitMatrix {
        let mut out = BitMatrix::zeros(self.rows(), self.cols());
        for r in 0..self.rows() {
            for c in 0..self.cols() {
                let keep = match (lower, diagonal) {
                    (true, true) => c <= r,
                    (true, false) => c < r,
                    (false, true) => c >= r,
                    (false, false) => c > r,
                };
                if keep && self.get(r, c) {
                    out.set(r, c, true);
                }
            }
        }
        out
    }

    /// The rank-1 matrix `aᵀ · b`, where `a` and `b` are `1 × m` and `1 × n`
    /// rows: element `(i, j)` is set iff both `a[i]` and `b[j]` are.
    ///
    /// `None` unless both operands are single rows.
    pub fn from_outer_product(a: &BitMatrix, b: &BitMatrix) -> Option<BitMatrix> {
        if a.rows() != 1 || b.rows() != 1 {
            return None;
        }
        let mut out = BitMatrix::zeros(a.cols(), b.cols());
        for i in 0..a.cols() {
            if !a.get(0, i) {
                continue;
            }
            // Every set row is a copy of `b`, so this is a word copy per row
            // rather than a bit loop.
            out.row_words_mut(i).copy_from_slice(b.row_words(0));
        }
        debug_assert!(out.tail_is_clear());
        Some(out)
    }

    /// One line per row, `'1'` and `'.'`, rows separated by newlines.
    ///
    /// `.` rather than `0` because a bit matrix is read for its *shape*, and a
    /// field of zeros is much easier to see through than a field of `0`s.
    pub fn to_pretty_string(&self) -> String {
        let mut s = String::with_capacity((self.cols() as usize + 1) * self.rows() as usize);
        for r in 0..self.rows() {
            for c in 0..self.cols() {
                s.push(if self.get(r, c) { '1' } else { '.' });
            }
            s.push('\n');
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matrix::Semiring;

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

    const SHAPES: &[(u32, u32)] = &[
        (1, 1),
        (3, 5),
        (5, 3),
        (8, 8),
        (64, 64),
        (65, 63),
        (70, 130),
    ];

    #[test]
    fn the_predicates_agree_with_counting() {
        for &(r, c) in SHAPES {
            let m = patterned(r, c, 0);
            let cells = r as u64 * c as u64;
            assert_eq!(m.count_zeros() + m.count_ones(), cells, "{r}x{c}");
            assert_eq!(m.is_zero(), m.count_ones() == 0, "{r}x{c}");
            assert_eq!(m.any(), !m.is_zero(), "{r}x{c}");
            assert_eq!(m.none(), m.is_zero(), "{r}x{c}");
            assert_eq!(m.all(), m.count_ones() == cells, "{r}x{c}");
            assert_eq!(m.is_square(), r == c, "{r}x{c}");
            assert!(!m.is_empty(), "{r}x{c}");
        }
        assert!(BitMatrix::zeros(0, 5).is_empty());
        assert!(BitMatrix::zeros(5, 0).is_empty());
        assert!(BitMatrix::zeros(4, 4).is_zero() && BitMatrix::zeros(4, 4).none());
    }

    #[test]
    fn a_full_matrix_is_all_and_a_complement_of_it_is_none() {
        // Catches `all` reading the padding tail: cols = 130 leaves 62 bits
        // of padding, so a whole-word count would never reach `rows * cols`.
        for &(r, c) in SHAPES {
            let full = BitMatrix::zeros(r, c).complement();
            assert!(full.all(), "{r}x{c}");
            assert!(!full.is_zero(), "{r}x{c}");
            assert_eq!(full.count_zeros(), 0, "{r}x{c}");
            assert!(full.complement().none(), "{r}x{c}");
        }
    }

    #[test]
    fn trace_and_identity() {
        for n in [1u32, 8, 63, 64, 65] {
            let id = BitMatrix::identity(n);
            assert!(id.is_identity(), "n={n}");
            assert_eq!(id.trace(), n, "n={n}");
            assert!(id.is_symmetric(), "n={n}");

            // One bit off the diagonal is enough to stop being the identity,
            // even though the count still matches.
            let mut nearly = BitMatrix::identity(n);
            if n > 1 {
                nearly.set(0, 0, false);
                nearly.set(0, 1, true);
                assert_eq!(nearly.count_ones(), n as u64, "count is unchanged");
                assert!(!nearly.is_identity(), "n={n}: count alone is not enough");
            }
        }
        // Non-square never is.
        assert!(!patterned(3, 4, 0).is_identity());
        assert!(!patterned(3, 4, 0).is_symmetric());
        // A non-square matrix still has a trace, over min(rows, cols).
        let m = patterned(3, 5, 0);
        assert_eq!(m.trace(), (0..3).filter(|&i| m.get(i, i)).count() as u32);
    }

    #[test]
    fn symmetry_agrees_with_the_transpose() {
        // Two independent routes to the same predicate.
        for n in [1u32, 8, 65] {
            let a = patterned(n, n, 1);
            assert_eq!(a.is_symmetric(), a == a.transpose(), "n={n} patterned");
            // `A | Aᵀ` is symmetric by construction.
            let sym = a.add(&a.transpose(), Semiring::Boolean).unwrap();
            assert!(sym.is_symmetric(), "n={n} constructed");
            assert_eq!(sym, sym.transpose(), "n={n}");
        }
    }

    #[test]
    fn flip_and_the_swaps() {
        let mut m = patterned(6, 7, 2);
        let before = m.get(2, 3);
        m.flip(2, 3);
        assert_eq!(m.get(2, 3), !before);
        m.flip(2, 3);
        assert_eq!(m.get(2, 3), before);

        // A row swap is a permutation of rows; a column swap of columns.
        //
        // The indices are checked to differ first. `patterned` repeats with
        // period 3 in both axes, so rows 1 and 4 are *identical* and swapping
        // them is a no-op — a fixture that would have made `assert_ne!` fail and
        // every other assertion here vacuous.
        let orig = m.clone();
        let (ra, rb) = (1u32, 2u32);
        assert!(
            (0..7).any(|c| orig.get(ra, c) != orig.get(rb, c)),
            "rows {ra} and {rb} must differ, or this test proves nothing"
        );
        m.swap_row_pair(ra, rb);
        assert_ne!(m, orig);
        for c in 0..7 {
            assert_eq!(m.get(ra, c), orig.get(rb, c), "col {c}");
            assert_eq!(m.get(rb, c), orig.get(ra, c), "col {c}");
        }
        m.swap_row_pair(ra, rb);
        assert_eq!(m, orig, "swapping twice is the identity");

        let (ca, cb) = (0u32, 1u32);
        assert!(
            (0..6).any(|r| orig.get(r, ca) != orig.get(r, cb)),
            "columns {ca} and {cb} must differ"
        );
        m.swap_col_pair(ca, cb);
        assert_ne!(m, orig);
        for r in 0..6 {
            assert_eq!(m.get(r, ca), orig.get(r, cb), "row {r}");
            assert_eq!(m.get(r, cb), orig.get(r, ca), "row {r}");
        }
        m.swap_col_pair(ca, cb);
        assert_eq!(m, orig);
        // Self-swap is a no-op, not a clear.
        m.swap_col_pair(3, 3);
        m.swap_row_pair(3, 3);
        assert_eq!(m, orig);
    }

    #[test]
    fn sub_matrix_copies_the_right_block() {
        let m = patterned(8, 9, 3);
        let b = m.sub_matrix(2, 3, 4, 5).unwrap();
        assert_eq!((b.rows(), b.cols()), (4, 5));
        for r in 0..4 {
            for c in 0..5 {
                assert_eq!(b.get(r, c), m.get(2 + r, 3 + c), "({r},{c})");
            }
        }
        assert!(b.tail_is_clear());
        // The whole matrix, and blocks that do not fit.
        assert_eq!(m.sub_matrix(0, 0, 8, 9).unwrap(), m);
        assert!(m.sub_matrix(1, 0, 8, 9).is_none());
        assert!(m.sub_matrix(0, 1, 8, 9).is_none());
        assert!(m.sub_matrix(0, 0, 0, 0).unwrap().is_empty());
    }

    #[test]
    fn the_triangles_partition_the_matrix() {
        for &(r, c) in SHAPES {
            let m = patterned(r, c, 4);
            // Strict lower + diagonal + strict upper == the whole thing, with
            // no element counted twice.
            let diag = m.lower().and(&m.upper()).expect("same shape");
            let recombined = m
                .strictly_lower()
                .add(&m.strictly_upper(), Semiring::Boolean)
                .unwrap()
                .add(&diag, Semiring::Boolean)
                .unwrap();
            assert_eq!(recombined, m, "{r}x{c}");
            assert_eq!(
                m.strictly_lower().count_ones()
                    + m.strictly_upper().count_ones()
                    + diag.count_ones(),
                m.count_ones(),
                "{r}x{c}: the three parts must be disjoint"
            );
            // And the strict forms hold nothing on the diagonal.
            assert_eq!(m.strictly_lower().trace(), 0, "{r}x{c}");
            assert_eq!(m.strictly_upper().trace(), 0, "{r}x{c}");
        }
    }

    #[test]
    fn the_unit_triangles_have_a_full_diagonal() {
        for &(r, c) in SHAPES {
            let m = patterned(r, c, 5);
            let n = r.min(c);
            assert_eq!(m.unit_lower().trace(), n, "{r}x{c} lower");
            assert_eq!(m.unit_upper().trace(), n, "{r}x{c} upper");
            // Off the diagonal they agree with the strict forms.
            assert_eq!(
                m.unit_lower().strictly_lower(),
                m.strictly_lower(),
                "{r}x{c}"
            );
        }
        // A unit lower triangular matrix is invertible over GF(2) — it has a
        // full diagonal, so every pivot exists.
        assert_eq!(patterned(9, 9, 6).unit_lower().rank_gf2(), 9);
    }

    #[test]
    fn the_outer_product_is_rank_one_and_a_cross_product_of_supports() {
        let a = {
            let mut v = BitMatrix::zeros(1, 6);
            for i in [0u32, 3, 5] {
                v.set(0, i, true);
            }
            v
        };
        let b = {
            let mut v = BitMatrix::zeros(1, 70);
            for j in [1u32, 64, 69] {
                v.set(0, j, true);
            }
            v
        };
        let m = BitMatrix::from_outer_product(&a, &b).unwrap();
        assert_eq!((m.rows(), m.cols()), (6, 70));
        assert_eq!(m.count_ones(), 3 * 3);
        for i in 0..6 {
            for j in 0..70 {
                assert_eq!(m.get(i, j), a.get(0, i) && b.get(0, j), "({i},{j})");
            }
        }
        assert!(m.tail_is_clear());
        // Rank one, by the module's own elimination.
        assert_eq!(m.rank_gf2(), 1);
        // A zero operand gives the zero matrix, still of the right shape.
        let z = BitMatrix::from_outer_product(&BitMatrix::zeros(1, 6), &b).unwrap();
        assert!(z.is_zero() && z.rows() == 6 && z.cols() == 70);
        // And it needs two row vectors.
        assert!(BitMatrix::from_outer_product(&patterned(2, 3, 0), &b).is_none());
    }

    #[test]
    fn pretty_printing_round_trips_by_eye() {
        let mut m = BitMatrix::zeros(3, 4);
        m.set(0, 0, true);
        m.set(1, 2, true);
        m.set(2, 3, true);
        assert_eq!(m.to_pretty_string(), "1...\n..1.\n...1\n");
        assert_eq!(BitMatrix::zeros(2, 2).to_pretty_string(), "..\n..\n");
        assert_eq!(BitMatrix::identity(2).to_pretty_string(), "1.\n.1\n");
        // One line per row, and the padding never shows.
        let wide = patterned(3, 130, 0);
        for line in wide.to_pretty_string().lines() {
            assert_eq!(line.len(), 130);
        }
    }
}
