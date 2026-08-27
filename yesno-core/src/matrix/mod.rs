//! Packed bit-matrix algebra: an [`OrdSet`](crate::OrdSet) read as a series of
//! M×N boolean matrices.
//!
//! An ordinal set is a bit vector over `[0, ORDINAL_MAX]`. Fix a [`Layout`] and
//! that bit vector becomes a stack of dense M×N matrices, one of which every
//! operation names. A chunk is 65536 bits — 1024 `u64` — so an 8×8 matrix is one
//! word, 64×64 is 64 words, and 256×256 is exactly one container.
//!
//! # The seam is paid at the boundary, never in a kernel
//!
//! `M` and `N` are arbitrary and [`Layout`] carries independent strides for
//! lines and for matrices, so a row can begin at any bit offset and a matrix can
//! straddle the 65536-bit chunk boundary. If every kernel handled that, the
//! shift-and-carry path would live in the product, the transpose, the inverse
//! and every reduction at once.
//!
//! Instead the reader normalizes. [`OrdSet::read_matrix`](crate::OrdSet::read_matrix)
//! gathers an arbitrary-layout matrix into the **canonical form** — row-major,
//! each row padded to whole `u64` words — every kernel works only on that, and
//! [`MatrixSink`](crate::matrix::MatrixSink) scatters back. One shift path in,
//! one out, and the kernels in between are branch-free word loops.
//!
//! **That shift path is [`pack`](crate::pack) and is shared with
//! [`bignum`](crate::bignum).** [`Layout::packing`] drops [`Order`] — which
//! [`Layout::line_len`] and [`Layout::line_count`] have already normalized away
//! — and hands the result to one gather with one seeking arm behind it. An
//! integer is the same construction at `lines == 1`, and the two lenses used to
//! carry a copy of the walk each.
//!
//! `M * N < 65536` does **not** by itself keep a matrix inside one chunk. A
//! 100×100 matrix is 10 000 bits, so matrix 7 spans bits 70 000..80 000 and
//! crosses the boundary. Straddling is a property of `matrix_stride`, not of
//! matrix size, and any fast path must *test* for chunk containment rather than
//! infer it — [`Layout::straddles`] is that test and [`Layout::chunk_aligned`]
//! is the spelling that avoids the question.
//!
//! # The canonical form's padding tail is always zero
//!
//! `BitMatrix` derives `PartialEq`, which compares words. Bits at or above
//! `cols` in a row's last word are therefore not "don't care" — a dirty tail
//! makes two equal matrices compare unequal, and makes `count_ones` wrong. Every
//! operation that writes a row must mask it; [`BitMatrix::tail_is_clear`] is the
//! debug-time guard.

use crate::pack::Packing;
use crate::{CodecError, Result, ORDINAL_MAX};

mod elem;
mod gemm;
mod gf2;
mod lu;
mod query;
mod read;
mod reduce;
mod sink;
mod transpose;

pub use lu::BitLu;
pub use reduce::Counts;
pub use sink::MatrixSink;

/// Which algebra an operation folds in.
///
/// Closed rather than a trait, and closed at exactly two: on packed bits the
/// only additive monoids available are `|` and `^`. `&`'s identity is all-ones
/// and `\` is not associative, so neither can be a semiring's `+`. Follows
/// [`SetOp`](crate::ops::generic::SetOp), which is closed for the same reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Semiring {
    /// `+` is `|`, `*` is `&`.
    Boolean,
    /// `+` is `^`, `*` is `&`. The two-element field.
    Gf2,
}

/// Whether a *line* of the layout is a row or a column.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Order {
    /// A line is a row of `cols` bits; there are `rows` of them.
    RowMajor,
    /// A line is a column of `rows` bits; there are `cols` of them.
    ColMajor,
}

/// Where the bits of a matrix live in the ordinal space.
///
/// Element `(r, c)` of matrix `k` sits at ordinal
///
/// ```text
/// RowMajor:  k*matrix_stride + r*line_stride + c
/// ColMajor:  k*matrix_stride + c*line_stride + r
/// ```
///
/// The strides are separate knobs on purpose — the layouts worth naming are all
/// instances of this one struct:
///
/// | what you want | how to say it |
/// |---|---|
/// | densely packed, 1:1 with ordinals | [`Layout::dense`] |
/// | rows padded to a word, so every load is aligned | [`Layout::word_aligned`] |
/// | one matrix per chunk, so none ever straddles | `matrix_stride = 65536` |
///
/// # The dense layout has a property worth protecting
///
/// When `line_stride == line_len` and `matrix_stride == rows * cols`, the
/// ordinal set and the matrix stack are the *same object* — so `OrdSet::and` /
/// `or` / `xor` already **are** elementwise matrix algebra, for free. Padding
/// buys aligned loads and gives that up; both are legitimate and the choice is
/// the caller's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Layout {
    /// M.
    pub rows: u32,
    /// N.
    pub cols: u32,
    /// Bits between the start of consecutive lines. Must be `>= line_len()`.
    pub line_stride: u32,
    /// Bits between the start of consecutive matrices. Must be at least
    /// `line_count() * line_stride`.
    pub matrix_stride: u64,
    pub order: Order,
}

impl Layout {
    /// Densely packed and row-major: no padding anywhere, so the ordinal space
    /// and the matrix stack coincide exactly.
    pub fn dense(rows: u32, cols: u32) -> Layout {
        Layout {
            rows,
            cols,
            line_stride: cols,
            matrix_stride: rows as u64 * cols as u64,
            order: Order::RowMajor,
        }
    }

    /// Row-major with each row starting on a `u64` boundary.
    ///
    /// Wastes up to 63 bits per row and gives up the identity described on
    /// [`Layout`], in exchange for every row read being an aligned word load.
    pub fn word_aligned(rows: u32, cols: u32) -> Layout {
        let stride = cols.div_ceil(64).saturating_mul(64);
        Layout {
            rows,
            cols,
            line_stride: stride,
            matrix_stride: rows as u64 * stride as u64,
            order: Order::RowMajor,
        }
    }

    /// Bits in one line: `cols` under [`Order::RowMajor`], `rows` under
    /// [`Order::ColMajor`].
    #[inline]
    pub fn line_len(&self) -> u32 {
        match self.order {
            Order::RowMajor => self.cols,
            Order::ColMajor => self.rows,
        }
    }

    /// How many lines one matrix has.
    #[inline]
    pub fn line_count(&self) -> u32 {
        match self.order {
            Order::RowMajor => self.rows,
            Order::ColMajor => self.cols,
        }
    }

    /// Bits from the first to the last bit of one matrix, inclusive.
    ///
    /// Not `rows * cols`: a padded `line_stride` makes a matrix occupy more of
    /// the ordinal space than it has elements. The final line's padding is not
    /// counted, because nothing addresses it.
    #[inline]
    pub fn span_bits(&self) -> u64 {
        let lines = self.line_count() as u64;
        if lines == 0 {
            return 0;
        }
        (lines - 1) * self.line_stride as u64 + self.line_len() as u64
    }

    /// Is this layout self-consistent?
    ///
    /// Rejects zero dimensions, a `line_stride` that would overlap consecutive
    /// lines, and a `matrix_stride` that would overlap consecutive matrices. A
    /// deliberately over-large stride is fine — that is a caller choice, not an
    /// error.
    pub fn check(&self) -> Result<()> {
        if self.rows == 0 || self.cols == 0 {
            return Err(CodecError::Invariant("matrix layout has a zero dimension"));
        }
        if self.line_stride < self.line_len() {
            return Err(CodecError::Invariant(
                "matrix layout line_stride is shorter than a line",
            ));
        }
        if self.matrix_stride < self.line_count() as u64 * self.line_stride as u64 {
            return Err(CodecError::Invariant(
                "matrix layout matrix_stride overlaps consecutive matrices",
            ));
        }
        Ok(())
    }

    /// First ordinal of matrix `k`, or `None` if it is out of the universe.
    #[inline]
    pub fn base_of(&self, k: u64) -> Option<u64> {
        let base = k.checked_mul(self.matrix_stride)?;
        (base <= ORDINAL_MAX).then_some(base)
    }

    /// Ordinal holding element `(r, c)` of matrix `k`.
    ///
    /// `None` if the indices are out of range for the layout, or if the ordinal
    /// would exceed [`ORDINAL_MAX`] — `u64::MAX` is not an ordinal (invariant
    /// I8), so a matrix that would reach it cannot be addressed at all.
    #[inline]
    pub fn ordinal_at(&self, k: u64, r: u32, c: u32) -> Option<u64> {
        if r >= self.rows || c >= self.cols {
            return None;
        }
        let (line, within) = match self.order {
            Order::RowMajor => (r, c),
            Order::ColMajor => (c, r),
        };
        let off = (line as u64).checked_mul(self.line_stride as u64)?;
        let ord = self
            .base_of(k)?
            .checked_add(off)?
            .checked_add(within as u64)?;
        (ord <= ORDINAL_MAX).then_some(ord)
    }

    /// This layout as the shared [`Packing`](crate::pack::Packing) the transfer
    /// kernels are defined on.
    ///
    /// **[`Order`] does not survive the conversion, and must not.**
    /// [`Layout::line_len`] and [`Layout::line_count`] already normalize
    /// row-major and column-major to "lines of bits" — a `ColMajor` layout
    /// becomes a packing whose lines are columns — so the gather is spared the
    /// distinction entirely and `read_matrix` transposes afterwards. A `Packing`
    /// that carried an order would be describing the *reading*, which is this
    /// module's business and not the packing's.
    #[inline]
    pub fn packing(&self) -> Packing {
        Packing {
            line_bits: self.line_len(),
            lines: self.line_count(),
            line_stride: self.line_stride,
            object_stride: self.matrix_stride,
        }
    }

    /// Row-major with each matrix starting on a 65 536-bit chunk boundary, so
    /// **none ever straddles**.
    ///
    /// `None` if the matrix does not fit in a chunk, where the promise is
    /// unachievable. This is the spelling of the row the [`Layout`] table names,
    /// given a constructor because the hazard it avoids is the one the module
    /// header warns about — and a caller who has just read that warning should
    /// not have to re-derive the stride.
    pub fn chunk_aligned(rows: u32, cols: u32) -> Option<Layout> {
        let dense = Layout::dense(rows, cols);
        (dense.span_bits() <= crate::CHUNK_CARD as u64).then_some(Layout {
            matrix_stride: crate::CHUNK_CARD as u64,
            ..dense
        })
    }

    /// Does matrix `k` cross a 65 536-bit chunk boundary? `None` if `k` is not
    /// addressable at all.
    ///
    /// Exposed because the module header names straddling as *the* hazard, and a
    /// caller choosing a `matrix_stride` needs a way to check the claim rather
    /// than re-deriving the arithmetic. [`Layout::chunk_aligned`] is the answer
    /// for a caller who would rather not think about it.
    pub fn straddles(&self, k: u64) -> Option<bool> {
        self.packing().straddles(k)
    }
}

/// Words in one row of the canonical form.
#[inline]
pub(crate) const fn words_per_row(cols: u32) -> usize {
    (cols as usize).div_ceil(64)
}

/// Mask of the live bits in a row's last word.
#[inline]
pub(crate) const fn tail_mask(cols: u32) -> u64 {
    match cols % 64 {
        0 => u64::MAX,
        r => (1u64 << r) - 1,
    }
}

/// An owned, dense M×N bit matrix in the canonical form.
///
/// Row-major, each row padded to whole `u64` words, so `words.len()` is
/// `rows * words_per_row(cols)` and row `r` occupies
/// `words[r*W .. (r+1)*W]`. Bits at or above `cols` in a row's last word are
/// **always zero** — see the module header for why that is an invariant rather
/// than a convention.
/// # The population count is carried, not recomputed
///
/// `ones` is `Some` whenever the count is known and `None` when an operation
/// has invalidated it. That makes `count_ones`, `all`, `is_zero`, `any`, `none`
/// and `count_zeros` `O(1)` on a tracked matrix, and — the reason it exists —
/// makes [`Self::transpose_prefers_scatter`] **exact and free** instead of
/// sampled, retiring the estimator's one documented failure mode.
///
/// It is the same rule QG §2 states for `Container::len()`: a cardinality that
/// is recomputed by iterating is a regression even when it returns the right
/// number.
///
/// `None` is not a defect. Elimination XORs rows thousands of times and
/// maintaining an exact count there would popcount a row per row operation —
/// the same order as the operation itself. Those paths drop the count and
/// `count_ones()` recomputes on demand.
#[derive(Clone, Debug)]
pub struct BitMatrix {
    rows: u32,
    cols: u32,
    words: Vec<u64>,
    /// Set bits, when known. See the type doc.
    ones: Option<u64>,
}

/// **Hand-written, and it must stay that way.** A derived `PartialEq` would
/// compare `ones`, so two matrices with identical contents would report unequal
/// merely because one had been through an operation that dropped its count.
/// Equality is about the bits.
impl PartialEq for BitMatrix {
    fn eq(&self, other: &Self) -> bool {
        self.rows == other.rows && self.cols == other.cols && self.words == other.words
    }
}
impl Eq for BitMatrix {}

impl BitMatrix {
    /// An all-zero `rows` × `cols` matrix.
    pub fn zeros(rows: u32, cols: u32) -> BitMatrix {
        BitMatrix {
            rows,
            cols,
            words: vec![0u64; rows as usize * words_per_row(cols)],
            ones: Some(0),
        }
    }

    /// The population count if it is currently tracked, without computing it.
    ///
    /// `None` means an operation dropped it; [`Self::count_ones`] will compute.
    #[inline]
    pub(crate) fn known_ones(&self) -> Option<u64> {
        self.ones
    }

    /// Record a population count the caller already knows.
    ///
    /// # Panics
    /// In debug builds, if it disagrees with the truth. A wrong cached count is
    /// worse than none.
    #[inline]
    pub(crate) fn set_known_ones(&mut self, n: u64) {
        debug_assert_eq!(
            n,
            self.words
                .iter()
                .map(|w| w.count_ones() as u64)
                .sum::<u64>(),
            "a recorded population count must be the true one"
        );
        self.ones = Some(n);
    }

    /// The `n` × `n` identity.
    pub fn identity(n: u32) -> BitMatrix {
        let mut m = BitMatrix::zeros(n, n);
        for i in 0..n {
            m.set(i, i, true);
        }
        m
    }

    #[inline]
    pub fn rows(&self) -> u32 {
        self.rows
    }

    #[inline]
    pub fn cols(&self) -> u32 {
        self.cols
    }

    /// Words per row of the canonical form.
    #[inline]
    pub fn stride(&self) -> usize {
        words_per_row(self.cols)
    }

    /// # Panics
    /// If `r >= rows` or `c >= cols`.
    #[inline]
    pub fn get(&self, r: u32, c: u32) -> bool {
        assert!(r < self.rows && c < self.cols, "index out of range");
        let w = self.stride() * r as usize + (c as usize >> 6);
        self.words[w] >> (c & 63) & 1 == 1
    }

    /// # Panics
    /// If `r >= rows` or `c >= cols`.
    #[inline]
    pub fn set(&mut self, r: u32, c: u32, v: bool) {
        assert!(r < self.rows && c < self.cols, "index out of range");
        let w = self.stride() * r as usize + (c as usize >> 6);
        let bit = 1u64 << (c & 63);
        let was = self.words[w] & bit != 0;
        if v {
            self.words[w] |= bit;
        } else {
            self.words[w] &= !bit;
        }
        // One element changed by at most one, so the count follows exactly.
        if was != v {
            if let Some(n) = self.ones.as_mut() {
                if v {
                    *n += 1;
                } else {
                    *n -= 1;
                }
            }
        }
    }

    /// Row `r`'s words. Tail bits past `cols` are zero.
    ///
    /// # Panics
    /// If `r >= rows`.
    #[inline]
    pub fn row_words(&self, r: u32) -> &[u64] {
        let w = self.stride();
        &self.words[w * r as usize..w * (r as usize + 1)]
    }

    /// Hands out raw words, so it **drops the tracked population count** —
    /// there is no way to know what the caller will write. A caller that does
    /// know should call [`Self::set_known_ones`] afterwards.
    #[inline]
    pub(crate) fn row_words_mut(&mut self, r: u32) -> &mut [u64] {
        self.ones = None;
        let w = self.stride();
        &mut self.words[w * r as usize..w * (r as usize + 1)]
    }

    /// Every row's words at once, for a transfer that addresses rows itself.
    ///
    /// Drops the cached population count, exactly as
    /// [`BitMatrix::row_words_mut`] does. That is load-bearing rather than
    /// defensive: `zeros()` sets `ones` to `Some(0)`, so a gather that wrote
    /// through this without invalidating would leave a matrix that reports zero
    /// set bits and compares wrong in every derived quantity.
    pub(crate) fn all_words_mut(&mut self) -> &mut [u64] {
        self.ones = None;
        &mut self.words
    }

    /// `row(dst) ^= row(src)` — the elementary row operation over GF(2).
    ///
    /// Lives here rather than in `gf2.rs` because it needs `words` directly:
    /// borrowing one row mutably and another immutably out of the same matrix
    /// requires splitting the backing slice, which the accessors cannot express.
    ///
    /// # Panics
    /// If `dst == src`. That is always a caller mistake — the result would be a
    /// zero row — and silently allowing it would hide an off-by-one in a pivot
    /// loop.
    pub(crate) fn xor_row_into(&mut self, dst: u32, src: u32) {
        assert_ne!(dst, src, "a row cannot be XORed into itself");
        self.ones = None;
        let w = self.stride();
        let (d, s) = (dst as usize * w, src as usize * w);
        if d < s {
            let (lo, hi) = self.words.split_at_mut(s);
            for (a, b) in lo[d..d + w].iter_mut().zip(&hi[..w]) {
                *a ^= *b;
            }
        } else {
            let (lo, hi) = self.words.split_at_mut(d);
            for (a, b) in hi[..w].iter_mut().zip(&lo[s..s + w]) {
                *a ^= *b;
            }
        }
    }

    /// `row(dst)[from_col..] ^= row(src)[from_col..]` — the elementary row
    /// operation restricted to a suffix of the columns.
    ///
    /// A packed `LU` needs exactly this and cannot use
    /// [`Self::xor_row_into`]. Columns below `from_col` hold the multipliers of
    /// `L` that earlier steps already stored, and XOR-ing a whole row would
    /// corrupt them — silently, since the result is still a well-formed matrix.
    ///
    /// # Panics
    /// If `dst == src`.
    pub(crate) fn xor_row_suffix_into(&mut self, dst: u32, src: u32, from_col: u32) {
        assert_ne!(dst, src, "a row cannot be XORed into itself");
        self.ones = None;
        let w = self.stride();
        let fw = (from_col as usize) >> 6;
        if fw >= w {
            return;
        }
        // The first touched word is partial; everything after it is whole.
        let head = !0u64 << (from_col & 63);
        let (d, s) = (dst as usize * w, src as usize * w);
        if d < s {
            let (lo, hi) = self.words.split_at_mut(s);
            for (i, (a, b)) in lo[d + fw..d + w].iter_mut().zip(&hi[fw..w]).enumerate() {
                *a ^= *b & if i == 0 { head } else { !0 };
            }
        } else {
            let (lo, hi) = self.words.split_at_mut(d);
            for (i, (a, b)) in hi[fw..w].iter_mut().zip(&lo[s + fw..s + w]).enumerate() {
                *a ^= *b & if i == 0 { head } else { !0 };
            }
        }
    }

    /// Exchange two rows. A no-op when they are the same row.
    pub(crate) fn swap_rows(&mut self, a: u32, b: u32) {
        if a == b {
            return;
        }
        let w = self.stride();
        for i in 0..w {
            self.words.swap(a as usize * w + i, b as usize * w + i);
        }
    }

    /// Every set bit in the matrix.
    ///
    /// `O(1)` when the count is tracked, `O(words)` when an operation has
    /// dropped it. See the type doc.
    pub fn count_ones(&self) -> u64 {
        match self.ones {
            Some(n) => {
                debug_assert_eq!(
                    n,
                    self.words
                        .iter()
                        .map(|w| w.count_ones() as u64)
                        .sum::<u64>(),
                    "the tracked population count has drifted"
                );
                n
            }
            None => self.words.iter().map(|w| w.count_ones() as u64).sum(),
        }
    }

    /// Is the padding tail of every row zero?
    ///
    /// The canonical form's invariant. `PartialEq` compares words, so a dirty
    /// tail is a wrong answer and not merely untidy.
    pub fn tail_is_clear(&self) -> bool {
        let w = self.stride();
        if w == 0 {
            return true;
        }
        let mask = tail_mask(self.cols);
        (0..self.rows as usize).all(|r| self.words[r * w + w - 1] & !mask == 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dense_layout_packs_without_gaps() {
        let l = Layout::dense(3, 5);
        assert_eq!(l.line_stride, 5);
        assert_eq!(l.matrix_stride, 15);
        // Consecutive matrices abut exactly.
        assert_eq!(l.ordinal_at(0, 2, 4), Some(14));
        assert_eq!(l.ordinal_at(1, 0, 0), Some(15));
    }

    #[test]
    fn word_aligned_layout_rounds_each_row_up() {
        let l = Layout::word_aligned(3, 100);
        assert_eq!(l.line_stride, 128);
        assert_eq!(l.matrix_stride, 384);
        assert_eq!(l.ordinal_at(0, 1, 0), Some(128));
    }

    #[test]
    fn col_major_transposes_the_addressing() {
        let l = Layout {
            rows: 2,
            cols: 3,
            line_stride: 2,
            matrix_stride: 6,
            order: Order::ColMajor,
        };
        // Column 1 starts at bit 2; row 0 of it is bit 2, row 1 is bit 3.
        assert_eq!(l.ordinal_at(0, 0, 1), Some(2));
        assert_eq!(l.ordinal_at(0, 1, 1), Some(3));
    }

    #[test]
    fn check_rejects_overlapping_strides() {
        let bad = Layout {
            line_stride: 4,
            ..Layout::dense(2, 5)
        };
        assert!(bad.check().is_err(), "line_stride < cols must be rejected");

        let bad = Layout {
            matrix_stride: 5,
            ..Layout::dense(2, 5)
        };
        assert!(bad.check().is_err(), "matrices must not overlap");

        assert!(Layout::dense(0, 5).check().is_err());
        assert!(Layout::dense(5, 0).check().is_err());
        assert!(Layout::dense(2, 5).check().is_ok());
    }

    #[test]
    fn an_over_large_stride_is_a_choice_not_an_error() {
        let l = Layout {
            line_stride: 1000,
            matrix_stride: 1 << 20,
            ..Layout::dense(4, 8)
        };
        assert!(l.check().is_ok());
        assert_eq!(l.ordinal_at(2, 1, 3), Some(2 * (1 << 20) + 1000 + 3));
    }

    #[test]
    fn span_bits_does_not_count_the_final_padding() {
        // Four rows of 8 bits at a 64-bit stride: the last row ends at bit
        // 3*64+8 == 200, not at 4*64 == 256.
        let l = Layout::word_aligned(4, 8);
        assert_eq!(l.span_bits(), 3 * 64 + 8);
    }

    #[test]
    fn addressing_stops_at_the_ordinal_ceiling() {
        // `u64::MAX` is not an ordinal (I8), so a matrix reaching it is not
        // addressable rather than silently truncated.
        let l = Layout::dense(1, 2);
        let k = u64::MAX / 2;
        assert_eq!(l.ordinal_at(k, 0, 0), Some(u64::MAX - 1));
        assert_eq!(l.ordinal_at(k, 0, 1), None, "would be u64::MAX");
        // And the span check the reader makes says the same thing: the last bit
        // of the matrix lands on `u64::MAX`, which is above ORDINAL_MAX.
        let last = l.base_of(k).unwrap().checked_add(l.span_bits() - 1);
        assert_eq!(last, Some(u64::MAX));
        assert!(last.unwrap() > ORDINAL_MAX);
    }

    #[test]
    fn out_of_range_indices_are_none_not_wrapped() {
        let l = Layout::dense(2, 3);
        assert_eq!(l.ordinal_at(0, 2, 0), None);
        assert_eq!(l.ordinal_at(0, 0, 3), None);
    }

    #[test]
    fn identity_has_one_bit_per_row() {
        let m = BitMatrix::identity(70);
        assert_eq!(m.count_ones(), 70);
        assert!(m.get(69, 69) && !m.get(69, 68));
        assert!(m.tail_is_clear());
    }

    #[test]
    fn the_tail_is_clear_after_construction_and_writes() {
        let mut m = BitMatrix::zeros(3, 100);
        assert_eq!(m.stride(), 2);
        for r in 0..3 {
            for c in 0..100 {
                m.set(r, c, true);
            }
        }
        assert!(m.tail_is_clear(), "set() must not reach past cols");
        assert_eq!(m.count_ones(), 300);
    }

    #[test]
    fn tail_is_clear_notices_a_raw_word_write() {
        // The invariant checker must itself be tested, or it silently returns
        // `true` forever. A kernel writing whole words without masking is
        // exactly the failure it exists to catch.
        let mut m = BitMatrix::zeros(2, 100);
        assert!(m.tail_is_clear());
        m.row_words_mut(0)[1] = u64::MAX;
        assert!(!m.tail_is_clear());
        // And the reason it matters: the dirty bits are counted as elements.
        assert_eq!(m.count_ones(), 64, "36 live bits plus 28 of padding");
    }

    /// **The trap a derived `PartialEq` would have walked into.** Two
    /// matrices with identical bits must compare equal whether or not either
    /// happens to be carrying its population count — otherwise every operation
    /// that drops the count silently changes the meaning of `==`, and every
    /// differential test in this module is built on `==`.
    #[test]
    fn equality_ignores_whether_the_count_is_tracked() {
        let mut a = BitMatrix::identity(70);
        let mut b = BitMatrix::identity(70);
        assert_eq!(a.known_ones(), Some(70));
        assert_eq!(b.known_ones(), Some(70));

        // Invalidated the way production code does — by asking for raw words.
        // Nothing is written through them, so the bits are untouched.
        let _ = b.row_words_mut(0);
        assert_eq!(a.known_ones(), Some(70));
        assert_eq!(b.known_ones(), None, "one side must be untracked");
        assert_eq!(a, b, "tracking state must not affect equality");
        assert_eq!(b, a, "and it must be symmetric");
        assert_eq!(
            a.count_ones(),
            b.count_ones(),
            "both must still count right"
        );

        // And genuinely different matrices are still unequal.
        a.set(0, 1, true);
        assert_ne!(a, b);
    }

    #[test]
    fn the_count_follows_every_single_element_write() {
        let mut m = BitMatrix::zeros(9, 100);
        assert_eq!(m.known_ones(), Some(0));
        m.set(3, 70, true);
        assert_eq!(m.known_ones(), Some(1));
        // Setting a bit that is already set changes nothing.
        m.set(3, 70, true);
        assert_eq!(m.known_ones(), Some(1));
        m.set(3, 70, false);
        assert_eq!(m.known_ones(), Some(0));
        // ...and clearing a clear bit likewise.
        m.set(3, 70, false);
        assert_eq!(m.known_ones(), Some(0));
        // `flip` goes through `set`, so it follows too.
        m.flip(8, 99);
        assert_eq!(m.known_ones(), Some(1));
        assert_eq!(m.count_ones(), 1);
    }

    /// Handing out raw words has to drop the count, because there is no way
    /// to know what will be written through them. A stale count is worse than
    /// none — `count_ones` would return it, and `debug_assert` would only catch
    /// it in a debug build.
    #[test]
    fn raw_word_access_and_row_operations_drop_the_count() {
        let mut m = BitMatrix::identity(8);
        assert_eq!(m.known_ones(), Some(8));
        let _ = m.row_words_mut(0);
        assert_eq!(m.known_ones(), None, "row_words_mut must invalidate");

        let mut m = BitMatrix::identity(8);
        m.xor_row_into(1, 0);
        assert_eq!(m.known_ones(), None, "xor_row_into must invalidate");
        assert_eq!(m.count_ones(), 9, "and the recomputed answer is right");

        let mut m = BitMatrix::identity(8);
        m.xor_row_suffix_into(1, 0, 0);
        assert_eq!(m.known_ones(), None);

        // A row swap moves elements without changing how many there are, so it
        // is the one row operation that may keep the count.
        let mut m = BitMatrix::identity(8);
        m.swap_rows(0, 3);
        assert_eq!(m.known_ones(), Some(8), "swap_rows must preserve it");
        assert_eq!(m.count_ones(), 8);
    }

    #[test]
    fn the_count_survives_a_clone_and_a_transpose() {
        let m = BitMatrix::identity(65);
        assert_eq!(m.clone().known_ones(), Some(65));
        // A transpose permutes elements, so the count carries over.
        assert_eq!(m.transpose().known_ones(), Some(65));
        // A complement is the complement of the count.
        let c = m.complement();
        assert_eq!(c.known_ones(), Some(65 * 65 - 65));
        assert_eq!(c.count_ones(), 65 * 65 - 65);
    }

    #[test]
    fn tail_mask_is_full_on_a_word_boundary() {
        assert_eq!(tail_mask(64), u64::MAX);
        assert_eq!(tail_mask(128), u64::MAX);
        assert_eq!(tail_mask(1), 1);
        assert_eq!(tail_mask(63), (1u64 << 63) - 1);
        assert_eq!(words_per_row(0), 0);
        assert_eq!(words_per_row(1), 1);
        assert_eq!(words_per_row(64), 1);
        assert_eq!(words_per_row(65), 2);
    }
}
