//! Bit transpose: blocked 64×64 delta swap.
//!
//! # Why a delta swap
//!
//! Transposing bit by bit costs one test and one masked write per element —
//! 4096 of each for a 64×64 block. The delta swap does the same work in six
//! passes of masked XOR-swaps over 64 words, about 120 word operations, because
//! each pass exchanges two whole `2^k`-sized sub-blocks at once. It is
//! Hacker's Delight §7-3.
//!
//! # Edge masking is free here, and that is the invariant paying off
//!
//! A partial block would normally need masks in both directions. It needs none:
//! a source row's padding tail is zero by the canonical form's invariant, so
//! loading a short block zero-fills the missing columns, and rows past `rows`
//! are zero-filled explicitly. Every bit that lands outside the destination was
//! therefore already zero. That means this kernel *depends* on the tail
//! invariant rather than merely preserving it — a dirty tail anywhere upstream
//! becomes wrong bits here, not just an untidy one.
//!
//! # The naive form stays as the oracle
//!
//! `tests::naive_transpose` is the differential reference and is not deleted
//! because the shipped arms are faster. It lives under `#[cfg(test)]` because it
//! is not a runtime fallback — both real arms are portable — unlike
//! `ops::run`'s scalar merge, which is one.
//!
//! **It is not unconditionally faster, and the crossover has now been
//! measured.** This kernel costs `O(rows * cols / 4096)` blocks whatever the
//! contents; walking set bits and scattering them costs `O(nnz)`. So density
//! decides, and `benches/bitmatrix.rs` locates where:
//!
//! ```text
//!   scatter / delta_swap, by fill density        ( <1.0 means scatter wins )
//!   density    n=64    n=256   n=1024
//!   1/2        8.52x   10.67x   13.59x
//!   1/8        2.49x    2.53x    2.50x
//!   1/16       1.27x    1.27x    1.26x
//!   1/32       0.95x    0.70x    0.66x     <- crossover
//!   1/64       0.64x    0.43x    0.39x
//!   1/1024     0.52x    0.22x    0.16x
//! ```
//!
//! **The crossover sits between 1/16 and 1/32 fill — about 4% — and it is the
//! same at every size.** That size-independence is the interesting part: it says
//! the ratio is a property of the two algorithms rather than of cache behaviour,
//! so a density-driven dispatch would be sound rather than machine-tuned.
//!
//! **Both arms ship, dispatched on density — exactly when the matrix knows
//! its own population, and on a sample when it does not.**
//!
//! The original obstacle was that `count_ones` over the whole matrix is
//! `O(rows*cols/64)`, about a sixth of the delta swap's cost, so "count then
//! choose" handed most of the saving back. That is no longer true: `BitMatrix`
//! carries its population count, and a matrix read out of a chunk-aligned layout
//! gets that count **from the chunk directory** — `Container::len()` is `O(1)`
//! on every representation, so reading a matrix tells you its density with no
//! payload counted. `Layout::dense(256, 256)` is one chunk, one container.
//!
//! The sampled estimator remains as the fallback for matrices whose count was
//! dropped — a product's output, or anything out of elimination — where the
//! payoff is lopsided enough for a rough answer: a wrong call near the threshold
//! costs at most ~1.3x, a right call far from it gains up to 6x.
//!
//! **Correctness does not rest on the estimate.** Both arms compute the same
//! function and are differential-tested against each other and against a naive
//! reference over every shape; only speed is at stake in the choice.
//!
//! What the dispatch buys, `n = 1024`, against the block kernel alone:
//!
//! ```text
//!   density   block only   dispatched   gain
//!   1/2         37.62 us     37.87 us   0.99x
//!   1/8         37.60 us     37.87 us   0.99x
//!   1/16        37.62 us     37.88 us   0.99x
//!   1/32        37.64 us     24.10 us   1.56x
//!   1/64        37.67 us     14.37 us   2.62x
//!   1/1024      37.72 us      5.93 us   6.36x
//! ```
//!
//! **Carrying the count was not free until the scatter arm stopped using
//! `set`.** Tracking made `set` do a load, a compare and a branch per element,
//! and `transpose_scatter` calls it once per set bit — which cost **25.2 us ->
//! 34.4 us at 1/32 fill** until that loop was changed to write words directly
//! and accumulate the count itself. A cheaper decision bought with a more
//! expensive primitive is not a saving.
//!
//! For scale, the bit-by-bit `O(rows*cols)` form — visiting every cell rather
//! than every set bit — is 10–14× slower than the delta swap at 1/2 fill and
//! never wins at any density. It is not the interesting rival and comparing only
//! against it would have answered the wrong question.

use super::BitMatrix;

/// Side of one square block, in bits.
const BLOCK: usize = 64;

/// Transpose a 64×64 bit matrix in place, one row per word.
///
/// Bit `c` of `a[r]` is element `(r, c)` on the way in and element `(c, r)` on
/// the way out — the same LSB-first convention `BitMatrix` uses, which is what
/// lets a block be loaded and stored with no shuffling.
fn transpose64(a: &mut [u64; BLOCK]) {
    let mut j = 32usize;
    let mut m = 0x0000_0000_FFFF_FFFFu64;
    while j != 0 {
        let mut k = 0usize;
        while k < BLOCK {
            // Exchange the off-diagonal quadrants of a 2j x 2j sub-block:
            // rows [k, k+j) columns [j, 2j)  <->  rows [k+j, k+2j) columns [0, j).
            //
            // Hacker's Delight writes this the other way round — `(a[k] ^
            // (a[k+j] >> j))`, swapping a[k]'s LOW half with a[k+j]'s HIGH half.
            // That is the same algorithm under the opposite bit convention
            // ( column 0 in the most significant bit ). `BitMatrix` puts column
            // 0 in the least significant bit, so the shifts move the other way.
            // Transcribing it unchanged compiles, runs, and transposes the wrong
            // quadrants.
            let t = ((a[k] >> j) ^ a[k + j]) & m;
            a[k + j] ^= t;
            a[k] ^= t << j;
            k = (k + j + 1) & !j;
        }
        j >>= 1;
        m ^= m << j;
    }
}

/// Ceiling on words read by the density estimate, and how they are chosen.
///
/// **Sampled by row, not by word position.** A first attempt took 16
/// contiguous blocks spread across the flat buffer, and it failed on the obvious
/// clustering: a 512×512 matrix with all its bits in the first 8 rows put block
/// zero entirely inside the cluster and **over-estimated 4×**, choosing the
/// block kernel where the scatter is 2.6× faster. The natural unit of clustering
/// in a matrix is a row, so that is the unit to sample — and sampling rows
/// cannot alias the row stride the way a fixed word stride can.
///
/// Within a very wide row the window is contiguous and its offset rotates across
/// sampled rows, so column-clustered data is covered without a stride that could
/// resonate with it.
const MAX_SAMPLE_WORDS: usize = 1024;
const MAX_SAMPLE_ROWS: usize = 64;

/// Density below which the scatter arm is chosen, as a reciprocal.
///
/// The measured crossover is between 1/16 fill ( scatter 1.26–1.27× slower at
/// every size ) and 1/32 ( scatter 0.66–0.95× ), so the true break-even sits
/// near 1/20–1/26 depending on `n`. One threshold in the middle is enough
/// **because the payoff is lopsided**: a wrong call near the boundary costs at
/// most ~1.3×, while a right call far from it gains up to 6×. That asymmetry is
/// what makes an estimate acceptable here at all.
const SCATTER_BELOW_ONE_IN: u64 = 24;

impl BitMatrix {
    /// Estimated set bits, from a sample rather than a full count.
    ///
    /// **A full `count_ones` costs `O(rows·cols/64)`, about a sixth of the
    /// delta swap it would be choosing for** — so counting everything and then
    /// choosing hands most of the saving straight back. This reads at most
    /// [`MAX_SAMPLE_WORDS`], which is 6% of a 1024×1024 matrix.
    ///
    /// Exact whenever the whole buffer fits in that budget, so the sizes where a
    /// misestimate would be cheapest to make are the ones that cannot have one.
    pub(super) fn estimated_ones(&self) -> u64 {
        self.estimated_ones_within(MAX_SAMPLE_WORDS)
    }

    /// [`Self::estimated_ones`] with an explicit word budget.
    ///
    /// The transpose can afford 1024 words against a kernel that costs many
    /// thousands; a caller whose own work is smaller needs a smaller sample, and
    /// picking the budget at the call site is cheaper than making every caller
    /// pay the largest one.
    pub(super) fn estimated_ones_within(&self, budget: usize) -> u64 {
        let total = self.words.len();
        if total <= budget {
            return self.words.iter().map(|x| x.count_ones() as u64).sum();
        }
        let w = self.stride();
        let rows = self.rows() as usize;
        debug_assert!(
            w > 0 && rows > 0,
            "a non-empty buffer has rows and a stride"
        );

        let sample_rows = rows.min(MAX_SAMPLE_ROWS);
        let per_row = (budget / sample_rows).clamp(1, w);

        // **No integer division inside the loop, and no Bresenham either.**
        // The obvious spelling computes `i * (rows-1) / (sample_rows-1)` per
        // sampled row; an integer division is 20-40 cycles, and 64 of them are
        // real money against a gate that has to be cheap.
        //
        // The first replacement was a Bresenham accumulator, which was a
        // mistake twice over: on a modern CPU an `f64` multiply is ~4 cycles
        // latency at 0.5 throughput and beats the integer add-and-conditional-
        // subtract loop, *and* the accumulator advanced past the final row when
        // stepped after the last sample. Scaling in floating point is both
        // faster and harder to get wrong.
        let row_scale = (rows - 1) as f64 / (sample_rows - 1).max(1) as f64;

        let rotates = per_row < w;
        let modulus = w - per_row + 1;
        let mut off = 0usize;

        let mut seen = 0u64;
        for i in 0..sample_rows {
            // Clamped: `(sample_rows-1) * ((rows-1)/(sample_rows-1))` is
            // exact in real arithmetic and can land an ulp low in `f64`, and an
            // index is not the place to find that out.
            let r = ((i as f64 * row_scale) as usize).min(rows - 1);
            let base = r * w + off;
            for &x in &self.words[base..base + per_row] {
                seen += x.count_ones() as u64;
            }
            if rotates {
                off += per_row;
                if off >= modulus {
                    off -= modulus;
                }
            }
        }
        seen * total as u64 / (sample_rows * per_row) as u64
    }

    /// A deliberately crude density estimate: one contiguous window of words
    /// from the middle of the buffer, scaled up.
    ///
    /// **Crude on purpose, and only sound where the two errors cost
    /// differently.** It reads a contiguous run, so it vectorizes and needs no
    /// per-row arithmetic — `estimated_ones_within`'s row walk measured **230 ns
    /// on a 1.84 us multiply, 12%**, which is far too much for a decision. The
    /// price is that data clustered outside the window is missed, which always
    /// *under*-estimates.
    ///
    /// That makes it right for [`gemm`](super::gemm)'s four-Russians gate, where
    /// a wrong "no" costs at most the speedup forgone and a wrong "yes" costs
    /// ~16x — so an estimator that errs toward "no" is exactly what is wanted.
    /// It is **not** right for the transpose dispatch, whose errors are
    /// symmetric and whose failure mode is precisely row-clustered data.
    pub(super) fn crude_ones(&self, budget: usize) -> u64 {
        let total = self.words.len();
        if total <= budget {
            return self.words.iter().map(|x| x.count_ones() as u64).sum();
        }
        let start = (total - budget) / 2;
        let seen: u64 = self.words[start..start + budget]
            .iter()
            .map(|x| x.count_ones() as u64)
            .sum();
        seen * total as u64 / budget as u64
    }

    /// Which arm the dispatch would take. Public for the same reason
    /// `Container::kind` is: a test that cannot see the decision can only infer
    /// it from a clock.
    pub fn transpose_prefers_scatter(&self) -> bool {
        let cells = self.rows() as u64 * self.cols() as u64;
        // Exact when the count is carried, which is the common case: a matrix
        // just read out of a set, transposed, complemented or built from zeros
        // knows its own population. Sampling is the fallback for matrices that
        // came out of elimination or a product, where maintaining the count
        // would have cost more than it saves.
        let ones = self.known_ones().unwrap_or_else(|| self.estimated_ones());
        ones * SCATTER_BELOW_ONE_IN < cells
    }

    /// `self` with rows and columns exchanged: a `rows × cols` matrix becomes
    /// `cols × rows`.
    ///
    /// Dispatches on estimated density — see [`Self::transpose_prefers_scatter`].
    /// Both arms stay reachable and are differential-tested against each other
    /// and against a naive reference, per QG §4.
    pub fn transpose(&self) -> BitMatrix {
        if self.transpose_prefers_scatter() {
            self.transpose_scatter()
        } else {
            self.transpose_blocked()
        }
    }

    /// `O(nnz)`: walk the set bits and scatter them.
    ///
    /// Wins below roughly 1/24 fill, where the delta swap's fixed
    /// `O(rows·cols/4096)` block cost exceeds the cost of touching each set bit.
    pub(crate) fn transpose_scatter(&self) -> BitMatrix {
        let mut out = BitMatrix::zeros(self.cols(), self.rows());
        let stride = out.stride();
        let mut n = 0u64;
        for r in 0..self.rows() {
            for (wi, &word) in self.row_words(r).iter().enumerate() {
                let mut w = word;
                while w != 0 {
                    let c = (wi * 64) as u32 + w.trailing_zeros();
                    debug_assert!(c < self.cols());
                    // A direct word write, not `set`. Tracking the population
                    // count made `set` do a load, a compare and a branch per
                    // element, and this loop runs once per set bit — measured
                    // **25.2 us -> 34.4 us at 1/32 fill, a 27% regression**,
                    // until the count was accumulated here instead.
                    out.words[stride * c as usize + (r as usize >> 6)] |= 1u64 << (r & 63);
                    n += 1;
                    w &= w - 1;
                }
            }
        }
        debug_assert!(out.tail_is_clear());
        // Writing `words` directly bypasses the tracking, so the count must be
        // restored here — `zeros()` left it at `Some(0)`, which is now stale.
        out.set_known_ones(n);
        debug_assert!(self.known_ones().is_none_or(|k| k == n));
        out
    }

    /// The blocked 64×64 delta swap. Content-independent.
    pub(crate) fn transpose_blocked(&self) -> BitMatrix {
        let (m, n) = (self.rows() as usize, self.cols() as usize);
        let mut out = BitMatrix::zeros(self.cols(), self.rows());
        let mut blk = [0u64; BLOCK];

        for bi in 0..m.div_ceil(BLOCK) {
            let rlo = bi * BLOCK;
            let rhi = (rlo + BLOCK).min(m);
            for bj in 0..n.div_ceil(BLOCK) {
                let clo = bj * BLOCK;
                let chi = (clo + BLOCK).min(n);

                // Rows past `rows` stay zero; columns past `cols` are already
                // zero in the source row's tail. Hence no masks.
                blk.fill(0);
                for (i, r) in (rlo..rhi).enumerate() {
                    blk[i] = self.row_words(r as u32)[bj];
                }
                transpose64(&mut blk);
                for (i, c) in (clo..chi).enumerate() {
                    out.row_words_mut(c as u32)[bi] = blk[i];
                }
            }
        }
        debug_assert!(
            out.tail_is_clear(),
            "a source tail bit leaked into the transpose"
        );
        // A transpose is a permutation of the elements, so the count carries
        // over unchanged — and the destination was written through
        // `row_words_mut`, which dropped it.
        if let Some(n) = self.known_ones() {
            out.set_known_ones(n);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The oracle: read every element, write it to the swapped position.
    /// Deliberately shares no code with the kernel above.
    fn naive_transpose(m: &BitMatrix) -> BitMatrix {
        let mut out = BitMatrix::zeros(m.cols(), m.rows());
        for r in 0..m.rows() {
            for c in 0..m.cols() {
                if m.get(r, c) {
                    out.set(c, r, true);
                }
            }
        }
        out
    }

    fn patterned(rows: u32, cols: u32) -> BitMatrix {
        let mut m = BitMatrix::zeros(rows, cols);
        for r in 0..rows {
            for c in 0..cols {
                if (r as u64 * 7 + c as u64 * 5).is_multiple_of(3) {
                    m.set(r, c, true);
                }
            }
        }
        m
    }

    /// Shapes that straddle the 64-bit block from both sides, in both
    /// rectangular directions, plus multi-block cases.
    const SHAPES: &[(u32, u32)] = &[
        (1, 1),
        (1, 7),
        (7, 1),
        (1, 64),
        (64, 1),
        (8, 8),
        (63, 63),
        (63, 64),
        (64, 63),
        (64, 64),
        (65, 64),
        (64, 65),
        (65, 65),
        (100, 3),
        (3, 100),
        (129, 65),
        (200, 200),
    ];

    /// QG §4: the two arms are two implementations of one function, so they
    /// are diffed against each other *and* against the naive reference. Called
    /// directly, because `transpose` only ever runs whichever the estimate chose.
    #[test]
    fn both_arms_agree_with_each_other_and_with_the_oracle() {
        let mut checked = 0u32;
        for &(r, c) in SHAPES {
            for one_in in [1u64, 2, 8, 32, 1024] {
                let mut m = BitMatrix::zeros(r, c);
                for i in 0..r {
                    for j in 0..c {
                        if (i as u64 * 7 + j as u64 * 5).is_multiple_of(one_in) {
                            m.set(i, j, true);
                        }
                    }
                }
                let want = naive_transpose(&m);
                assert_eq!(m.transpose_blocked(), want, "{r}x{c} 1/{one_in} blocked");
                assert_eq!(m.transpose_scatter(), want, "{r}x{c} 1/{one_in} scatter");
                // And the dispatch, whichever way it went.
                assert_eq!(m.transpose(), want, "{r}x{c} 1/{one_in} dispatched");
                assert!(m.transpose_scatter().tail_is_clear());
                checked += 1;
            }
        }
        assert!(checked > 60, "only {checked} comparisons");
    }

    /// The rotating within-row window, which only engages when a row is wider
    /// than the per-row budget.
    ///
    /// **This is where the estimate is genuinely approximate**, and the test
    /// asserts the *decision* rather than the number: a window covers part of a
    /// row, so column-clustered bits are seen only by the windows that happen to
    /// overlap them. A wrong decision is slow, never wrong, and the threshold has
    /// an order of magnitude of slack on both sides here.
    #[test]
    fn a_row_wider_than_the_sample_budget_still_decides_correctly() {
        let (rows, cols) = (128u32, 2048u32);
        let m = BitMatrix::zeros(rows, cols);
        assert!(
            m.stride() * MAX_SAMPLE_ROWS > MAX_SAMPLE_WORDS,
            "the fixture must exceed the per-row budget, or it tests nothing"
        );

        // Sparse, bits confined to the first 8 columns: 1/256 fill.
        let mut sparse = BitMatrix::zeros(rows, cols);
        for i in 0..rows {
            for j in 0..8 {
                sparse.set(i, j, true);
            }
        }
        assert!(
            sparse.transpose_prefers_scatter(),
            "1/256 fill wants scatter"
        );

        // Dense, bits confined to the first quarter of the columns: 1/4 fill.
        let mut dense = BitMatrix::zeros(rows, cols);
        for i in 0..rows {
            for j in 0..cols / 4 {
                dense.set(i, j, true);
            }
        }
        assert!(
            !dense.transpose_prefers_scatter(),
            "1/4 fill wants the block"
        );

        // And both still transpose correctly, whichever arm was chosen.
        for m in [&sparse, &dense] {
            assert_eq!(m.transpose(), naive_transpose(m));
        }
    }

    #[test]
    fn the_estimate_is_exact_for_a_small_matrix() {
        // Below the sampling budget it reads everything, so there is no error
        // to reason about at the sizes where a mistake would be cheapest to make.
        for &(r, c) in &[(8u32, 8u32), (64, 64), (16, 256)] {
            let m = patterned(r, c);
            assert_eq!(m.estimated_ones(), m.count_ones(), "{r}x{c}");
        }
    }

    #[test]
    fn the_dispatch_follows_density() {
        // Uniform fill, well clear of the threshold in both directions.
        let n = 512u32;
        let mut dense = BitMatrix::zeros(n, n);
        let mut sparse = BitMatrix::zeros(n, n);
        for i in 0..n {
            for j in 0..n {
                if (i as u64 * 7 + j as u64 * 5).is_multiple_of(2) {
                    dense.set(i, j, true);
                }
                if (i as u64 * 7 + j as u64 * 5).is_multiple_of(256) {
                    sparse.set(i, j, true);
                }
            }
        }
        assert!(
            !dense.transpose_prefers_scatter(),
            "1/2 fill wants the block"
        );
        assert!(
            sparse.transpose_prefers_scatter(),
            "1/256 fill wants the scatter"
        );
        assert!(BitMatrix::zeros(n, n).transpose_prefers_scatter(), "empty");
    }

    /// The sampler's failure mode: bits clustered somewhere the sample misses.
    ///
    /// A fixed stride would alias the row stride and read the same column offset
    /// in every row, which is why the sample is contiguous blocks instead. Both
    /// clusterings are checked, and the assertion is on the *decision*, not on a
    /// clock — a wrong decision is only slow, so the bound is generous and what
    /// matters is that it is not wildly wrong.
    #[test]
    fn the_estimate_survives_clustered_layouts() {
        let n = 512u32;
        let cells = n as u64 * n as u64;

        // All bits in the first 8 rows: 8/512 of the matrix, fully dense there.
        let mut by_row = BitMatrix::zeros(n, n);
        for i in 0..8 {
            for j in 0..n {
                by_row.set(i, j, true);
            }
        }
        // All bits in the first 8 columns.
        let mut by_col = BitMatrix::zeros(n, n);
        for i in 0..n {
            for j in 0..8 {
                by_col.set(i, j, true);
            }
        }
        for (name, m) in [("rows", &by_row), ("cols", &by_col)] {
            let exact = m.count_ones();
            let est = m.estimated_ones();
            assert_eq!(exact, 8 * n as u64, "{name}: fixture is wrong");
            // Row sampling makes both of these exact: a row is the unit of
            // clustering, and a sampled row is counted in full. The earlier
            // word-block sampler over-estimated the row-clustered case 4x.
            assert_eq!(est, exact, "{name}: estimated {est} of {cells}");
            assert!(
                m.transpose_prefers_scatter(),
                "{name}: 1/64 fill must choose the scatter"
            );
        }
    }

    #[test]
    fn the_block_kernel_agrees_with_the_naive_oracle() {
        for &(r, c) in SHAPES {
            let m = patterned(r, c);
            let t = m.transpose();
            assert_eq!(t, naive_transpose(&m), "{r}x{c}");
            assert_eq!((t.rows(), t.cols()), (c, r), "{r}x{c}");
            assert!(t.tail_is_clear(), "{r}x{c}");
        }
    }

    #[test]
    fn a_single_bit_lands_correctly_at_every_block_corner() {
        // One bit at a time is the strongest positional check there is: a
        // pattern can mask an index error that a lone bit cannot.
        let (rows, cols) = (130u32, 130u32);
        for &(r, c) in &[
            (0u32, 0u32),
            (0, 63),
            (63, 0),
            (63, 63),
            (64, 64),
            (0, 64),
            (64, 0),
            (129, 129),
            (1, 128),
            (128, 1),
        ] {
            let mut m = BitMatrix::zeros(rows, cols);
            m.set(r, c, true);
            let t = m.transpose();
            assert_eq!(t.count_ones(), 1, "({r},{c}) lost or duplicated a bit");
            assert!(t.get(c, r), "({r},{c}) landed in the wrong place");
        }
    }

    #[test]
    fn transposing_a_full_matrix_keeps_the_tail_clear() {
        // cols == 100 means 28 padding bits per row, and the block kernel writes
        // whole words — this is where an unmasked write would show up.
        for &(r, c) in &[(70u32, 100u32), (100, 70), (65, 65), (200, 130)] {
            let mut m = BitMatrix::zeros(r, c);
            for i in 0..r {
                for j in 0..c {
                    m.set(i, j, true);
                }
            }
            let t = m.transpose();
            assert!(t.tail_is_clear(), "{r}x{c}");
            assert_eq!(t.count_ones(), r as u64 * c as u64, "{r}x{c}");
            assert_eq!((t.rows(), t.cols()), (c, r), "{r}x{c}");
        }
    }

    #[test]
    fn transpose_is_an_involution() {
        // Weak on its own — any self-inverse index mangling survives it — but
        // free, and it does catch dropped entries.
        for &(r, c) in SHAPES {
            let m = patterned(r, c);
            assert_eq!(m.transpose().transpose(), m, "{r}x{c}");
        }
    }

    #[test]
    fn transpose_conserves_the_bit_count() {
        for &(r, c) in SHAPES {
            let m = patterned(r, c);
            assert_eq!(m.transpose().count_ones(), m.count_ones(), "{r}x{c}");
        }
    }

    #[test]
    fn the_identity_is_its_own_transpose() {
        for n in [1u32, 8, 63, 64, 65, 100, 129] {
            assert_eq!(BitMatrix::identity(n).transpose(), BitMatrix::identity(n));
        }
    }

    #[test]
    fn transpose64_moves_a_row_to_a_column() {
        // The bit convention, checked directly on the primitive: row 0 all ones
        // must become bit 0 of every word.
        let mut a = [0u64; 64];
        a[0] = u64::MAX;
        transpose64(&mut a);
        assert!(a.iter().all(|&w| w == 1), "row 0 should become column 0");

        // And the reverse direction.
        let mut a = [1u64; 64];
        transpose64(&mut a);
        assert_eq!(a[0], u64::MAX);
        assert!(a[1..].iter().all(|&w| w == 0));
    }

    #[test]
    fn transpose64_is_an_involution_on_an_irregular_block() {
        let mut a = [0u64; 64];
        for (i, w) in a.iter_mut().enumerate() {
            *w = (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        }
        let original = a;
        transpose64(&mut a);
        assert_ne!(a, original, "the block was not actually transposed");
        transpose64(&mut a);
        assert_eq!(a, original);
    }

    #[test]
    fn an_empty_matrix_transposes_to_an_empty_one() {
        for &(r, c) in &[(1u32, 1u32), (64, 64), (100, 3)] {
            let t = BitMatrix::zeros(r, c).transpose();
            assert_eq!(t.count_ones(), 0);
            assert_eq!((t.rows(), t.cols()), (c, r));
        }
    }
}
