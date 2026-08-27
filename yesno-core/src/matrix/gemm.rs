//! `A · B + C` over a semiring, and the two operations that fall out of it.
//!
//! # One kernel, driven by A's rows
//!
//! ```text
//! C := A·B + C,  A is M×K, B is K×N, C is M×N
//! for i in 0..M:
//!     acc = C.row(i)
//!     for each set bit k of A.row(i):
//!         acc |= B.row(k)        // Semiring::Boolean
//!         acc ^= B.row(k)        // Semiring::Gf2
//!     C.row(i) = acc
//! ```
//!
//! Driving from A's rows is what makes `Bᵀ` unnecessary: the inner step is a
//! whole-row word loop against `B` in its natural layout. The two semirings
//! differ by one operator, which is why "semiring-selectable" costs nothing
//! here.
//!
//! Cost is `O(nnz(A) · ceil(N/64))` against `O(M·K·N)` bit by bit — the work
//! tracks A's set bits, not its shape.
//!
//! # Fused accumulation is free, so GEMM is the primitive
//!
//! The accumulator is seeded from `C` rather than from zero. That is the entire
//! difference from a plain product: no extra pass, no extra memory, the same
//! instruction count. So there is one kernel to write, test and specialize, and
//! [`BitMatrix::mul`] is it over a zeroed accumulator.
//!
//! `gemm` returns a new matrix rather than accumulating in place, so a sum of
//! products allocates one matrix per term — against two per term for `mul`
//! followed by `add`, which materializes `Aᵢ·Bᵢ` and then the running sum
//! separately.
//!
//! # The tail invariant is load-bearing twice
//!
//! Set bits of `A.row(i)` are used directly as row indices into `B`. They are in
//! range only because A's padding tail is zero — `a.cols() == b.rows()` bounds
//! the live bits, and a dirty tail would index past `B`. And because every
//! `B.row(k)` has a zero tail, OR-ing or XOR-ing them into a zero-tailed
//! accumulator keeps it zero, so no masking is needed on the way out.

use super::{BitMatrix, Semiring};

/// `out += a · b`, where `+=` is `op`. Shapes are the caller's responsibility.
fn accumulate(out: &mut BitMatrix, a: &BitMatrix, b: &BitMatrix, op: impl Fn(&mut u64, u64)) {
    debug_assert_eq!(a.cols(), b.rows());
    debug_assert_eq!((out.rows(), out.cols()), (a.rows(), b.cols()));
    debug_assert!(a.tail_is_clear(), "a dirty tail in A would index past B");

    for i in 0..a.rows() {
        for wi in 0..a.stride() {
            let mut w = a.row_words(i)[wi];
            while w != 0 {
                let k = (wi * 64) as u32 + w.trailing_zeros();
                w &= w - 1;
                debug_assert!(k < b.rows());
                // `b` and `out` are distinct matrices, which the borrow checker
                // enforces at every call site: `out` arrives as `&mut` and `b`
                // as `&`, so they cannot alias.
                let src = b.row_words(k);
                let dst = out.row_words_mut(i);
                for (d, s) in dst.iter_mut().zip(src) {
                    op(d, *s);
                }
            }
        }
    }
    debug_assert!(out.tail_is_clear());
}

/// `out += a · b` when `N <= 64`, so the accumulator is a single word.
///
/// # Why this arm exists, and why it is about *small* matrices
///
/// `accumulate` does a read-modify-write against `out`'s row for every set bit
/// of A. When `N <= 64` that row is one word, so it can live in a register for
/// the whole row and be stored once — the memory traffic collapses from one
/// round trip per contributing `k` to one store per output row.
///
/// The benchmark is what pointed here, and it pointed somewhere unintuitive.
/// Cost per unit of `nnz(A)·ceil(N/64)` falls from **12.9 ns at n=8 to 0.163 ns
/// at n=1024**, a 79× spread: the fixed cost of extracting a bit index and
/// bounds-checking a row dominates when the inner word loop is short, and
/// amortizes away when it is long. So the specialization opportunity is at the
/// *small* end, which is the opposite of where one looks first.
///
/// Measured before / after, `benches/bitmatrix.rs` group `gemm`:
///
/// ```text
///   n     density   generic    narrow   speedup
///     8   1/2        51.6 ns   33.2 ns    1.55x
///     8   1/64       21.2 ns   20.4 ns    1.04x
///    64   1/2        2.65 us   1.40 us    1.89x
///    64   1/64      103.5 ns   94.7 ns    1.09x
///   256   1/2       76.12 us  76.19 us    1.00x   ( generic; unchanged )
///  1024   1/2        1.37 ms   1.37 ms    1.00x   ( generic; unchanged )
/// ```
///
/// The gain is much larger at high density, which is the mechanism confirming
/// itself: what is saved is one round trip to `out`'s row per contributing `k`,
/// and a sparse row has few of those. Sizes above the threshold are unchanged to
/// within noise, which is what says the dispatch is not costing anything.
fn accumulate_narrow(
    out: &mut BitMatrix,
    a: &BitMatrix,
    b: &BitMatrix,
    op: impl Fn(u64, u64) -> u64,
) {
    debug_assert_eq!(b.stride(), 1, "this arm is only valid for N <= 64");
    for i in 0..a.rows() {
        let mut acc = out.row_words(i)[0];
        for wi in 0..a.stride() {
            let mut w = a.row_words(i)[wi];
            while w != 0 {
                let k = (wi * 64) as u32 + w.trailing_zeros();
                w &= w - 1;
                acc = op(acc, b.row_words(k)[0]);
            }
        }
        out.row_words_mut(i)[0] = acc;
    }
    debug_assert!(out.tail_is_clear());
}

/// `out += a · b` by the method of four Russians.
///
/// # The idea
///
/// The generic kernel does one row operation per set bit of A. Group B's rows
/// into eights and precompute, for each group, the combination selected by every
/// one of the 256 possible bytes; then one byte of A's row costs **one table
/// lookup** instead of up to eight row operations.
///
/// The table is built incrementally — `T[m] = T[m & (m-1)] op B.row(low bit of
/// m)` — so each of the 256 entries costs one row operation, not eight.
///
/// # This is a large-M arm, and the cost model says so before any measurement
///
/// Per group of 8 rows of B the table costs `256·W` word operations and is used
/// by all `M` rows of A at `W` each. With `K/8` groups:
///
/// ```text
///   four Russians   M·K·W/8 + 32·K·W
///   generic         M·K·d·W                ( d = density of A )
///   4R wins when    d > 1/8 + 32/M
/// ```
///
/// So the table build is amortized over `M`, and the arm needs **both** a dense
/// `A` and a tall one. At `M = 1024` it should win above ~16% fill; at
/// `M = 64` the break-even is ~62%, which is above the densities that occur.
/// Do not enable it by size alone.
///
/// The scratch is one group's table, reused: `256·W` words, which is 32 KiB at
/// `N = 1024` and stays in L2.
///
/// # Measured
///
/// ```text
///   n     density   generic   four Russians   speedup
///     8   1/2        33.2 ns        33.4 ns     1.00x   ( declined: M < 37 )
///    64   1/2        1.40 us        1.40 us     1.00x   ( declined: M < 37 )
///   256   1/2       76.19 us       33.16 us     2.30x
///   256   1/64       1.80 us        1.81 us     0.99x   ( declined: too sparse )
///  1024   1/2        1.37 ms      465.78 us     2.94x
///  1024   1/64      47.49 us       46.30 us     1.03x   ( declined: too sparse )
/// ```
///
/// The cost model predicted a win above `d > 1/8 + 32/M`, so ~16% fill at
/// `M = 1024` and ~62% at `M = 64` — which is why 64 is declined at half fill
/// and 256 is not. Predicted 3.2x at `n = 1024`; measured 2.94x.
///
/// **Getting the gate cheap took three attempts, and the middle one was
/// misdiagnosed.** An early version regressed the declined cases 5-13%. It was
/// briefly concluded that the arm's mere *presence* was perturbing codegen —
/// based on an experiment where the arm was present but never taken, which still
/// measured slow. That was wrong: the experiment shared a build with the
/// expensive row-walking estimator, so it did not isolate what it claimed to.
/// The real cost was the gate reading `A` through a per-row sampler with integer
/// division. A contiguous window plus float-scaled indexing removed it, and the
/// declined cases are now within 1-2% of the generic kernel.
fn accumulate_four_russians(
    out: &mut BitMatrix,
    a: &BitMatrix,
    b: &BitMatrix,
    op: impl Fn(&mut u64, u64),
) {
    let w = b.stride();
    let k = b.rows() as usize;
    let mut table = vec![0u64; 256 * w];

    for g in 0..k.div_ceil(8) {
        let base_row = g * 8;
        let rows_here = (k - base_row).min(8);

        // T[0] is the identity of the fold and stays zero; T[m] extends the
        // entry with m's lowest bit removed.
        table[..w].fill(0);
        for m in 1usize..(1 << rows_here) {
            let low = m.trailing_zeros() as usize;
            let prev = m & (m - 1);
            let src_row = base_row + low;
            let (dst_lo, dst_hi) = (m * w, m * w + w);
            // Copy the shorter predecessor, then fold in the one new row.
            let (head, tail) = table.split_at_mut(dst_lo);
            tail[..w].copy_from_slice(&head[prev * w..prev * w + w]);
            for (d, s) in tail[..w].iter_mut().zip(b.row_words(src_row as u32)) {
                op(d, *s);
            }
            debug_assert_eq!(dst_hi - dst_lo, w);
        }

        // One byte of A's row selects one table entry.
        let byte_index = base_row / 64;
        let byte_shift = base_row % 64;
        for i in 0..a.rows() {
            let word = a.row_words(i)[byte_index];
            let m = ((word >> byte_shift) & 0xFF) as usize;
            if m == 0 {
                continue;
            }
            let src = &table[m * w..m * w + w];
            let dst = out.row_words_mut(i);
            for (d, s) in dst.iter_mut().zip(src) {
                op(d, *s);
            }
        }
    }
    debug_assert!(out.tail_is_clear());
}

/// Dispatch: the narrow arm where it applies, the generic kernel otherwise.
///
/// The generic kernel stays reachable and stays the differential oracle, per
/// QG §4. It is not deleted because this arm is faster.
/// Is the four-Russians table worth building for this pair?
///
/// Straight from the cost model on [`accumulate_four_russians`]:
/// `nnz(A) > K · (M/8 + 32)`, which is `d > 1/8 + 32/M` rearranged to avoid a
/// division. The `32·K` term is the table build, so a short `A` cannot amortize
/// it however dense it is.
///
/// **A first version called the estimator unconditionally and cost 5–9% on
/// the cases it then declined.** Two fixes, both derived rather than tuned:
///
/// 1. **A structural bound first, which touches no data.** `nnz(A) <= M·K`
///    always, so the test can only pass if `M·K > K·(M/8 + 32)`, i.e. `7M > 256`,
///    i.e. `M >= 37`. Below that height the table can never be amortized however
///    dense `A` is, and no count is needed to know it. This removed the 8×8 and
///    64×64 regressions outright.
/// 2. **A crude, contiguous, 64-word sample** — [`BitMatrix::crude_ones`] rather
///    than the transpose's row-walking estimator, which measured 230 ns against
///    a 1.84 µs multiply. 64 words is ample because the decision is nowhere near
///    marginal: at 256×256 half fill `nnz` clears the threshold by 2×, at 1/64
///    fill it misses by 16×.
///
/// **The margin is not symmetry — the two errors cost very differently.** A
/// wrong "no" falls back to the generic kernel and forgoes at most the ~3× this
/// arm wins. A wrong "yes" builds tables for a sparse `A` and costs
/// `M·K·W/8 + 32·K·W` against `nnz·W` — about **16× worse** at 256×256, 1/64
/// fill. So the estimate must clear the threshold by half again before the table
/// is built, and ties go to the cheap answer. That asymmetry is also why a
/// crude, always-under-estimating sampler is the *right* one here, and why the
/// transpose — whose errors are bounded near 1.3× either way — uses a careful
/// one instead.
fn prefers_four_russians(a: &BitMatrix, b: &BitMatrix) -> bool {
    const GATE_SAMPLE_WORDS: usize = 64;
    /// `7M > 256` — the smallest `M` for which a fully dense `A` could pay for
    /// the table. Derived from the cost model, not measured.
    const MIN_ROWS: u64 = 37;

    let (m, k) = (a.rows() as u64, b.rows() as u64);
    if m < MIN_ROWS || k == 0 {
        return false;
    }
    debug_assert!(
        m * k > k * (m / 8 + 32),
        "MIN_ROWS must make the test feasible"
    );
    let threshold = k * (m / 8 + 32);
    // `est > 1.5 * threshold`, without the division.
    a.crude_ones(GATE_SAMPLE_WORDS) * 2 > threshold * 3
}

fn dispatch(out: &mut BitMatrix, a: &BitMatrix, b: &BitMatrix, semiring: Semiring) {
    if prefers_four_russians(a, b) {
        match semiring {
            Semiring::Boolean => accumulate_four_russians(out, a, b, |d, s| *d |= s),
            Semiring::Gf2 => accumulate_four_russians(out, a, b, |d, s| *d ^= s),
        }
    } else if b.stride() == 1 && b.cols() > 0 {
        match semiring {
            Semiring::Boolean => accumulate_narrow(out, a, b, |d, s| d | s),
            Semiring::Gf2 => accumulate_narrow(out, a, b, |d, s| d ^ s),
        }
    } else {
        match semiring {
            Semiring::Boolean => accumulate(out, a, b, |d, s| *d |= s),
            Semiring::Gf2 => accumulate(out, a, b, |d, s| *d ^= s),
        }
    }
}

/// Do the shapes of `A (M×K)`, `B (K×N)` and `C (M×N)` agree?
fn shapes_agree(a: &BitMatrix, b: &BitMatrix, c: &BitMatrix) -> bool {
    a.cols() == b.rows() && c.rows() == a.rows() && c.cols() == b.cols()
}

impl BitMatrix {
    /// `self · rhs + addend`, fused.
    ///
    /// `+` is the semiring's addition: `|` for [`Semiring::Boolean`], `^` for
    /// [`Semiring::Gf2`]. `None` if the shapes do not agree — `self` must be
    /// `M×K`, `rhs` `K×N`, and `addend` `M×N`.
    ///
    /// Seeding the accumulator from `addend` costs nothing over a plain product,
    /// so this is the primitive and [`Self::mul`] is the special case. A sum of
    /// products written as repeated `gemm` allocates one matrix per term, where
    /// `mul` then `add` allocates two.
    pub fn gemm(
        &self,
        rhs: &BitMatrix,
        addend: &BitMatrix,
        semiring: Semiring,
    ) -> Option<BitMatrix> {
        if !shapes_agree(self, rhs, addend) {
            return None;
        }
        let mut out = addend.clone();
        dispatch(&mut out, self, rhs, semiring);
        Some(out)
    }

    /// `self · rhs`. [`Self::gemm`] over a zeroed accumulator.
    ///
    /// `None` if `self.cols() != rhs.rows()`.
    pub fn mul(&self, rhs: &BitMatrix, semiring: Semiring) -> Option<BitMatrix> {
        if self.cols() != rhs.rows() {
            return None;
        }
        let mut out = BitMatrix::zeros(self.rows(), rhs.cols());
        dispatch(&mut out, self, rhs, semiring);
        Some(out)
    }
}

impl BitMatrix {
    /// `self · v`, where `v` is a **column** vector of `cols()` bits carried as
    /// a `1 × cols()` row. The result is `1 × rows()`.
    ///
    /// `y[i]` is the fold over `k` of `self[i][k] & v[k]`: "does row `i` meet
    /// `v`" for [`Semiring::Boolean`], "does it meet it an odd number of times"
    /// for [`Semiring::Gf2`].
    ///
    /// # Why this is not just `mul` with a thin operand
    ///
    /// A column vector as a matrix is `N × 1`, and [`Self::mul`] is driven by
    /// A's set bits with a `ceil(1/64) = 1`-word inner loop — so it costs
    /// `O(nnz(self))`, one word operation per set bit. Holding the same vector
    /// as a single padded **row** instead lets each output bit be one
    /// `popcount(row_i & v)` over `W` words, which is `O(rows · W)`. On a dense
    /// matrix that is `M·N` against `M·N/64` — **64× fewer operations**.
    ///
    /// The other direction needs nothing new: `v · self` for a `1 × rows()` row
    /// vector is exactly `v.mul(self, semiring)`, which the row-driven kernel
    /// already computes optimally. Do not add a second spelling of it.
    ///
    /// `None` unless `v` is `1 × self.cols()`.
    pub fn mul_vec(&self, v: &BitMatrix, semiring: Semiring) -> Option<BitMatrix> {
        if v.rows() != 1 || v.cols() != self.cols() {
            return None;
        }
        let vw = v.row_words(0);
        let mut out = BitMatrix::zeros(1, self.rows());
        for i in 0..self.rows() {
            let hits: u32 = self
                .row_words(i)
                .iter()
                .zip(vw)
                .map(|(a, b)| (a & b).count_ones())
                .sum();
            let bit = match semiring {
                Semiring::Boolean => hits > 0,
                Semiring::Gf2 => hits % 2 == 1,
            };
            if bit {
                out.set(0, i, true);
            }
        }
        debug_assert!(out.tail_is_clear());
        Some(out)
    }

    /// `self^n` by square-and-multiply. Square matrices only.
    ///
    /// `pow(0)` is the identity, which is why this requires squareness even for
    /// `n == 1` — a non-square matrix has no identity to fall back to and the
    /// signature would otherwise be honest for some `n` and not others.
    ///
    /// `None` if `self` is not square.
    pub fn pow(&self, n: u32, semiring: Semiring) -> Option<BitMatrix> {
        if self.rows() != self.cols() {
            return None;
        }
        let side = self.rows();
        let mut result = BitMatrix::identity(side);
        let mut base = self.clone();
        let mut n = n;
        while n > 0 {
            if n & 1 == 1 {
                result = result.mul(&base, semiring)?;
            }
            n >>= 1;
            if n > 0 {
                base = base.mul(&base, semiring)?;
            }
        }
        Some(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The oracle: the definition, over `Vec<Vec<bool>>`. Shares no code with
    /// the kernel — no words, no strides, no packing.
    fn naive(a: &BitMatrix, b: &BitMatrix, c: &BitMatrix, semiring: Semiring) -> BitMatrix {
        let mut out = BitMatrix::zeros(a.rows(), b.cols());
        for i in 0..a.rows() {
            for j in 0..b.cols() {
                let mut acc = c.get(i, j);
                for k in 0..a.cols() {
                    let term = a.get(i, k) && b.get(k, j);
                    acc = match semiring {
                        Semiring::Boolean => acc || term,
                        Semiring::Gf2 => acc ^ term,
                    };
                }
                out.set(i, j, acc);
            }
        }
        out
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

    /// (M, K, N), straddling the 64-bit word from both sides.
    const SHAPES: &[(u32, u32, u32)] = &[
        (1, 1, 1),
        (1, 5, 1),
        (3, 3, 3),
        (8, 8, 8),
        (63, 63, 63),
        (64, 64, 64),
        (65, 65, 65),
        (63, 64, 65),
        (65, 64, 63),
        (1, 128, 1),
        (100, 3, 70),
        (7, 130, 129),
    ];

    const BOTH: [Semiring; 2] = [Semiring::Boolean, Semiring::Gf2];

    #[test]
    fn gemm_agrees_with_the_naive_oracle() {
        for &(m, k, n) in SHAPES {
            let a = patterned(m, k, 0);
            let b = patterned(k, n, 1);
            let c = patterned(m, n, 2);
            for sr in BOTH {
                let got = a.gemm(&b, &c, sr).unwrap();
                assert_eq!(got, naive(&a, &b, &c, sr), "{m}x{k}x{n} {sr:?}");
                assert!(got.tail_is_clear(), "{m}x{k}x{n} {sr:?}");
            }
        }
    }

    #[test]
    fn mul_is_gemm_over_a_zero_addend() {
        for &(m, k, n) in SHAPES {
            let a = patterned(m, k, 0);
            let b = patterned(k, n, 1);
            let zero = BitMatrix::zeros(m, n);
            for sr in BOTH {
                let via_mul = a.mul(&b, sr).unwrap();
                let via_gemm = a.gemm(&b, &zero, sr).unwrap();
                assert_eq!(via_mul, via_gemm, "{m}x{k}x{n} {sr:?}");
            }
        }
    }

    #[test]
    fn gemm_is_the_product_plus_the_addend() {
        for &(m, k, n) in SHAPES {
            let a = patterned(m, k, 0);
            let b = patterned(k, n, 1);
            let c = patterned(m, n, 2);
            for sr in BOTH {
                let fused = a.gemm(&b, &c, sr).unwrap();
                let split = a.mul(&b, sr).unwrap().add(&c, sr).unwrap();
                assert_eq!(fused, split, "{m}x{k}x{n} {sr:?}");
            }
        }
    }

    #[test]
    fn accumulating_a_sum_of_products_sums_them() {
        // Composition: `gemm` chained over several terms must sum them. What
        // this adds over `gemm_is_the_product_plus_the_addend` is the *repeated*
        // application — an implementation right for one call but which corrupts
        // or re-reads its accumulator across calls passes that and fails this.
        //
        // It is not the only guard on the addend. Seeding the accumulator
        // from zero instead of from `addend` fails this **and** four others,
        // because the single-call tests use a non-zero addend. Only
        // `mul_is_gemm_over_a_zero_addend` survives that sabotage, which is
        // exactly what its name says it checks.
        for &(m, k, n) in SHAPES {
            let terms: Vec<(BitMatrix, BitMatrix)> = (0..4)
                .map(|s| (patterned(m, k, s), patterned(k, n, s + 10)))
                .collect();
            for sr in BOTH {
                let mut acc = BitMatrix::zeros(m, n);
                for (a, b) in &terms {
                    acc = a.gemm(b, &acc, sr).unwrap();
                }
                let expect = terms.iter().fold(BitMatrix::zeros(m, n), |e, (a, b)| {
                    a.mul(b, sr).unwrap().add(&e, sr).unwrap()
                });
                assert_eq!(acc, expect, "{m}x{k}x{n} {sr:?}");
            }
        }
    }

    #[test]
    fn multiplication_is_associative() {
        // Catches state leaking between rows — a reused accumulator that is not
        // reset, or a cursor that is not rewound.
        for &(m, k, n) in &[(3u32, 4u32, 5u32), (65, 63, 64), (8, 8, 8)] {
            let a = patterned(m, k, 0);
            let b = patterned(k, n, 1);
            let c = patterned(n, m, 2);
            for sr in BOTH {
                let left = a.mul(&b, sr).unwrap().mul(&c, sr).unwrap();
                let right = a.mul(&b.mul(&c, sr).unwrap(), sr).unwrap();
                assert_eq!(left, right, "{m}x{k}x{n} {sr:?}");
            }
        }
    }

    #[test]
    fn the_identity_is_a_two_sided_unit() {
        for &(m, n) in &[(1u32, 1u32), (3, 5), (64, 64), (65, 63), (100, 130)] {
            let a = patterned(m, n, 0);
            for sr in BOTH {
                assert_eq!(a.mul(&BitMatrix::identity(n), sr).unwrap(), a, "right");
                assert_eq!(BitMatrix::identity(m).mul(&a, sr).unwrap(), a, "left");
            }
        }
    }

    #[test]
    fn a_zero_operand_annihilates() {
        for sr in BOTH {
            let a = patterned(5, 7, 0);
            let z = BitMatrix::zeros(7, 3);
            assert_eq!(a.mul(&z, sr).unwrap(), BitMatrix::zeros(5, 3));
            let z = BitMatrix::zeros(4, 5);
            assert_eq!(z.mul(&a, sr).unwrap(), BitMatrix::zeros(4, 7));
        }
    }

    #[test]
    fn the_two_semirings_differ_exactly_where_the_path_count_is_even() {
        // Boolean says "some path exists"; GF(2) says "an odd number do". So
        // they disagree on exactly the cells with an even, non-zero count.
        let (m, k, n) = (12u32, 11u32, 13u32);
        let a = patterned(m, k, 0);
        let b = patterned(k, n, 1);
        let bool_c = a.mul(&b, Semiring::Boolean).unwrap();
        let gf2_c = a.mul(&b, Semiring::Gf2).unwrap();

        let mut disagreed = 0u32;
        for i in 0..m {
            for j in 0..n {
                let paths = (0..k).filter(|&x| a.get(i, x) && b.get(x, j)).count();
                assert_eq!(bool_c.get(i, j), paths > 0, "boolean ({i},{j})");
                assert_eq!(gf2_c.get(i, j), paths % 2 == 1, "gf2 ({i},{j})");
                if bool_c.get(i, j) != gf2_c.get(i, j) {
                    assert!(paths > 0 && paths % 2 == 0);
                    disagreed += 1;
                }
            }
        }
        assert!(disagreed > 0, "the two semirings never diverged; vacuous");
    }

    #[test]
    fn gf2_addition_is_self_inverse() {
        let a = patterned(9, 9, 0);
        let sum = a.add(&a, Semiring::Gf2).unwrap();
        assert_eq!(sum, BitMatrix::zeros(9, 9));
        // While boolean addition is idempotent.
        assert_eq!(a.add(&a, Semiring::Boolean).unwrap(), a);
    }

    #[test]
    fn a_shape_mismatch_is_none() {
        let a = patterned(3, 4, 0);
        let b = patterned(5, 6, 1);
        for sr in BOTH {
            assert!(a.mul(&b, sr).is_none(), "4 != 5");
            assert!(a.add(&b, sr).is_none());
            // Right shapes for the product, wrong addend.
            let b = patterned(4, 6, 1);
            assert!(a.mul(&b, sr).is_some());
            assert!(a.gemm(&b, &BitMatrix::zeros(3, 6), sr).is_some());
            assert!(a.gemm(&b, &BitMatrix::zeros(3, 5), sr).is_none());
            assert!(a.gemm(&b, &BitMatrix::zeros(2, 6), sr).is_none());
        }
    }

    #[test]
    fn a_full_addend_keeps_the_tail_clear() {
        // cols = 100 leaves 28 padding bits. A full addend is where an
        // unmasked accumulate would surface.
        let (m, k, n) = (5u32, 6u32, 100u32);
        let a = patterned(m, k, 0);
        let b = patterned(k, n, 1);
        let mut full = BitMatrix::zeros(m, n);
        for i in 0..m {
            for j in 0..n {
                full.set(i, j, true);
            }
        }
        for sr in BOTH {
            let got = a.gemm(&b, &full, sr).unwrap();
            assert!(got.tail_is_clear(), "{sr:?}");
            assert_eq!(got, naive(&a, &b, &full, sr), "{sr:?}");
        }
    }

    #[test]
    fn a_permutation_times_its_transpose_is_the_identity() {
        // Rows are disjoint singletons, so Boolean and GF(2) must agree.
        let n = 70u32;
        // The multiplier must be coprime to `n` or this is a function and not
        // a bijection, and `P·Pᵀ` is then not the identity. `i*7 % 70` was the
        // first thing written here and is not one — gcd(7, 70) = 7.
        let images: Vec<u32> = (0..n).map(|i| (i * 3 + 11) % n).collect();
        let mut distinct = images.clone();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(distinct.len(), n as usize, "not a bijection");

        let mut p = BitMatrix::zeros(n, n);
        for (i, &c) in images.iter().enumerate() {
            p.set(i as u32, c, true);
        }
        for sr in BOTH {
            assert_eq!(p.mul(&p.transpose(), sr).unwrap(), BitMatrix::identity(n));
            assert_eq!(p.transpose().mul(&p, sr).unwrap(), BitMatrix::identity(n));
        }
    }

    #[test]
    fn the_operands_may_be_the_same_matrix() {
        // `A·A + A` must work: `gemm` clones the addend first, so nothing
        // aliases the accumulator.
        let a = patterned(16, 16, 0);
        for sr in BOTH {
            let got = a.gemm(&a, &a, sr).unwrap();
            assert_eq!(got, naive(&a, &a, &a, sr), "{sr:?}");
        }
    }
}

#[cfg(test)]
mod arm_tests {
    use super::*;

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

    /// QG §4: a specialized arm is differential-tested against the generic
    /// kernel, which stays reachable. Both are called directly, because
    /// `mul` only ever runs whichever the dispatch chose.
    #[test]
    fn the_narrow_arm_agrees_with_the_generic_kernel() {
        let mut checked = 0u32;
        for &(m, k, n) in &[
            (1u32, 1u32, 1u32),
            (1, 1, 64),
            (3, 5, 7),
            (8, 8, 8),
            (64, 64, 64),
            (65, 129, 63),
            (100, 3, 1),
            (7, 130, 64),
        ] {
            assert!(n <= 64, "the narrow arm requires N <= 64");
            let a = patterned(m, k, 0);
            let b = patterned(k, n, 1);
            for addend in [BitMatrix::zeros(m, n), patterned(m, n, 2)] {
                for sr in [Semiring::Boolean, Semiring::Gf2] {
                    let mut want = addend.clone();
                    match sr {
                        Semiring::Boolean => accumulate(&mut want, &a, &b, |d, s| *d |= s),
                        Semiring::Gf2 => accumulate(&mut want, &a, &b, |d, s| *d ^= s),
                    }
                    let mut got = addend.clone();
                    match sr {
                        Semiring::Boolean => accumulate_narrow(&mut got, &a, &b, |d, s| d | s),
                        Semiring::Gf2 => accumulate_narrow(&mut got, &a, &b, |d, s| d ^ s),
                    }
                    assert_eq!(got, want, "{m}x{k}x{n} {sr:?}");
                    assert!(got.tail_is_clear(), "{m}x{k}x{n} {sr:?}");
                    checked += 1;
                }
            }
        }
        assert!(checked >= 32, "only {checked} comparisons");
    }

    /// QG §4 for the four-Russians arm: diffed against the generic kernel over
    /// shapes and densities, including the ones the cost model would reject —
    /// the arm must be *correct* everywhere even where it is not *chosen*.
    /// A row-shaped vector of `cols` bits, from a bit list.
    #[allow(dead_code)]
    fn vec_of(cols: u32, bits: &[u32]) -> BitMatrix {
        let mut v = BitMatrix::zeros(1, cols);
        for &b in bits {
            v.set(0, b, true);
        }
        v
    }

    #[test]
    fn mul_vec_matches_the_definition() {
        for &(m, n) in &[(1u32, 1u32), (3, 5), (8, 8), (63, 65), (65, 63), (70, 130)] {
            let a = patterned(m, n, 0);
            let v = patterned(1, n, 1);
            for sr in [Semiring::Boolean, Semiring::Gf2] {
                let y = a.mul_vec(&v, sr).unwrap();
                assert_eq!((y.rows(), y.cols()), (1, m), "{m}x{n} shape");
                for i in 0..m {
                    let hits = (0..n).filter(|&k| a.get(i, k) && v.get(0, k)).count();
                    let want = match sr {
                        Semiring::Boolean => hits > 0,
                        Semiring::Gf2 => hits % 2 == 1,
                    };
                    assert_eq!(y.get(0, i), want, "{m}x{n} {sr:?} row {i}");
                }
                assert!(y.tail_is_clear());
            }
        }
    }

    /// The claim in `mul_vec`'s doc, checked rather than asserted in prose:
    /// it is the same answer `mul` gives with the vector shaped as a column.
    #[test]
    fn mul_vec_agrees_with_the_general_product() {
        for &(m, n) in &[(3u32, 5u32), (8, 8), (65, 63)] {
            let a = patterned(m, n, 2);
            let v = patterned(1, n, 3);
            // The same vector as an n x 1 column.
            let mut col = BitMatrix::zeros(n, 1);
            for k in 0..n {
                col.set(k, 0, v.get(0, k));
            }
            for sr in [Semiring::Boolean, Semiring::Gf2] {
                let via_vec = a.mul_vec(&v, sr).unwrap();
                let via_mul = a.mul(&col, sr).unwrap();
                assert_eq!((via_mul.rows(), via_mul.cols()), (m, 1));
                for i in 0..m {
                    assert_eq!(via_vec.get(0, i), via_mul.get(i, 0), "{m}x{n} {sr:?} {i}");
                }
            }
        }
    }

    #[test]
    fn the_identity_maps_a_vector_to_itself() {
        for n in [1u32, 8, 63, 64, 65] {
            let v = patterned(1, n, 4);
            for sr in [Semiring::Boolean, Semiring::Gf2] {
                assert_eq!(BitMatrix::identity(n).mul_vec(&v, sr).unwrap(), v, "n={n}");
            }
        }
    }

    #[test]
    fn mul_vec_rejects_a_shape_that_is_not_a_row_of_cols() {
        let a = patterned(4, 6, 0);
        assert!(a
            .mul_vec(&BitMatrix::zeros(1, 5), Semiring::Boolean)
            .is_none());
        assert!(a
            .mul_vec(&BitMatrix::zeros(2, 6), Semiring::Boolean)
            .is_none());
        assert!(a
            .mul_vec(&BitMatrix::zeros(6, 1), Semiring::Boolean)
            .is_none());
        assert!(a
            .mul_vec(&BitMatrix::zeros(1, 6), Semiring::Boolean)
            .is_some());
    }

    #[test]
    fn pow_agrees_with_repeated_multiplication() {
        for n in [1u32, 3, 8, 65] {
            let a = patterned(n, n, 5);
            for sr in [Semiring::Boolean, Semiring::Gf2] {
                let mut want = BitMatrix::identity(n);
                for e in 0..6u32 {
                    assert_eq!(a.pow(e, sr).unwrap(), want, "n={n} {sr:?} ^{e}");
                    want = want.mul(&a, sr).unwrap();
                }
            }
        }
    }

    #[test]
    fn pow_of_a_permutation_cycles_back_to_the_identity() {
        // An n-cycle has order n, so P^n is the identity and P^k is not for
        // 0 < k < n. A strong check that square-and-multiply is not off by one.
        let n = 12u32;
        let mut p = BitMatrix::zeros(n, n);
        for i in 0..n {
            p.set(i, (i + 1) % n, true);
        }
        for sr in [Semiring::Boolean, Semiring::Gf2] {
            assert_eq!(p.pow(n, sr).unwrap(), BitMatrix::identity(n), "{sr:?}");
            for k in 1..n {
                assert_ne!(p.pow(k, sr).unwrap(), BitMatrix::identity(n), "{sr:?} ^{k}");
            }
        }
        // And a large exponent, where square-and-multiply actually squares.
        assert_eq!(p.pow(n * 7, Semiring::Gf2).unwrap(), BitMatrix::identity(n));
    }

    #[test]
    fn pow_requires_a_square_matrix() {
        assert!(patterned(3, 4, 0).pow(2, Semiring::Boolean).is_none());
        assert!(patterned(3, 4, 0).pow(0, Semiring::Boolean).is_none());
        assert!(patterned(4, 4, 0).pow(0, Semiring::Boolean).is_some());
    }

    #[test]
    fn the_four_russians_arm_agrees_with_the_generic_kernel() {
        let mut checked = 0u32;
        for &(m, k, n) in &[
            (1u32, 1u32, 1u32),
            (1, 8, 1),
            (3, 7, 5),
            (8, 8, 8),
            (9, 17, 9),
            (64, 64, 64),
            (65, 129, 63),
            (128, 65, 200),
            (7, 130, 129),
            (200, 8, 200),
        ] {
            for seed in [0u64, 4] {
                let a = patterned(m, k, seed);
                let b = patterned(k, n, seed + 1);
                for addend in [BitMatrix::zeros(m, n), patterned(m, n, seed + 2)] {
                    for sr in [Semiring::Boolean, Semiring::Gf2] {
                        let mut want = addend.clone();
                        match sr {
                            Semiring::Boolean => accumulate(&mut want, &a, &b, |d, s| *d |= s),
                            Semiring::Gf2 => accumulate(&mut want, &a, &b, |d, s| *d ^= s),
                        }
                        let mut got = addend.clone();
                        match sr {
                            Semiring::Boolean => {
                                accumulate_four_russians(&mut got, &a, &b, |d, s| *d |= s)
                            }
                            Semiring::Gf2 => {
                                accumulate_four_russians(&mut got, &a, &b, |d, s| *d ^= s)
                            }
                        }
                        assert_eq!(got, want, "{m}x{k}x{n} seed={seed} {sr:?}");
                        assert!(got.tail_is_clear());
                        checked += 1;
                    }
                }
            }
        }
        assert!(checked >= 80, "only {checked} comparisons");
    }

    /// `K` not being a multiple of 8 is the edge the table build has to get
    /// right: the last group is short, so only `2^rows_here` entries are valid
    /// and A's byte must not select past them.
    #[test]
    fn a_partial_last_group_selects_no_stale_table_entry() {
        for k in [1u32, 7, 8, 9, 15, 16, 17, 63, 65] {
            let a = patterned(16, k, 0);
            let b = patterned(k, 16, 1);
            let mut want = BitMatrix::zeros(16, 16);
            accumulate(&mut want, &a, &b, |d, s| *d |= s);
            let mut got = BitMatrix::zeros(16, 16);
            accumulate_four_russians(&mut got, &a, &b, |d, s| *d |= s);
            assert_eq!(got, want, "K={k}");
        }
    }

    #[test]
    fn the_cost_model_gates_on_both_density_and_height() {
        // Tall and dense: the table amortizes.
        let tall = patterned(1024, 256, 0);
        let b = patterned(256, 256, 1);
        assert!(
            prefers_four_russians(&tall, &b),
            "1024 rows at ~1/3 fill should build the table"
        );
        // Same density, short: 32·K cannot be amortized over 16 rows.
        let short = patterned(16, 256, 0);
        assert!(
            !prefers_four_russians(&short, &b),
            "16 rows cannot amortize the table however dense"
        );
        // Tall but sparse.
        let sparse = BitMatrix::zeros(1024, 256);
        assert!(!prefers_four_russians(&sparse, &b), "empty A");
    }

    #[test]
    fn the_dispatch_picks_the_narrow_arm_exactly_when_n_fits_a_word() {
        // The condition itself, asserted rather than inferred from timings.
        for n in [1u32, 63, 64] {
            assert_eq!(BitMatrix::zeros(4, n).stride(), 1, "N={n} should be narrow");
        }
        for n in [65u32, 128, 129] {
            assert!(BitMatrix::zeros(4, n).stride() > 1, "N={n} should be wide");
        }
    }

    #[test]
    fn the_generic_kernel_is_still_reachable() {
        // N > 64 must not silently route to an arm that cannot handle it.
        let a = patterned(4, 4, 0);
        let b = patterned(4, 200, 1);
        let got = a.mul(&b, Semiring::Boolean).unwrap();
        let mut want = BitMatrix::zeros(4, 200);
        accumulate(&mut want, &a, &b, |d, s| *d |= s);
        assert_eq!(got, want);
    }
}
