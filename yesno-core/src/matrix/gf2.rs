//! Gauss-Jordan elimination over GF(2): inversion and rank.
//!
//! GF(2) is the two-element field, so every elementary row operation is one
//! word-wise XOR — there are no scalars to multiply by and no pivot scaling.
//! Cost is `O(n²)` row operations of `ceil(n/64)` words each, the textbook
//! `O(n³/64)` bitset bound.
//!
//! # Singularity is a decision, not an estimate
//!
//! Over the reals a pivot search needs a tolerance and "singular" is a judgement
//! about conditioning. Here "no row at or below `c` has a bit in column `c`" is
//! exact, and so is the rank that falls out of the same elimination. That is why
//! [`BitMatrix::rank_gf2`] is worth exposing: it is the answer when the inverse
//! does not exist, and it costs nothing extra to produce.
//!
//! # The pivot search is a linear scan, and an index would be wrong
//!
//! Finding "rows with a bit in column `c`" by scanning is `O(n)` per pivot,
//! `O(n²)` overall — the same order as the XORs that are mandatory anyway, so it
//! is not the bottleneck. A transposed index would be faster to query and
//! **goes stale after every single XOR**, since each elementary operation
//! changes which rows hold which columns. Maintaining it costs more than it
//! saves, and a stale one is a wrong answer rather than a slow one.
//!
//! # Density
//!
//! The inverse of a sparse GF(2) matrix is generically **dense**, and unlike
//! a boolean closure there is no representation that saves you — an `n × n`
//! inverse really is `n²` bits. This is a completeness operation, not a scaling
//! path, and the usable `n` is small.

use super::BitMatrix;

impl BitMatrix {
    /// `self⁻¹` over GF(2), by Gauss-Jordan on `[self | I]`.
    ///
    /// `None` if `self` is not square, or is singular. Both are exact: there is
    /// no tolerance to tune.
    pub fn invert_gf2(&self) -> Option<BitMatrix> {
        if self.rows() != self.cols() {
            return None;
        }
        let n = self.rows();
        let mut a = self.clone();
        let mut inv = BitMatrix::identity(n);

        for c in 0..n {
            // No pivot at or below the diagonal means the columns are dependent.
            let pivot = (c..n).find(|&r| a.get(r, c))?;
            a.swap_rows(c, pivot);
            inv.swap_rows(c, pivot);
            // Clear the column everywhere else — Jordan, not just Gauss, so `a`
            // finishes as the identity and `inv` as the answer.
            for s in 0..n {
                if s != c && a.get(s, c) {
                    a.xor_row_into(s, c);
                    inv.xor_row_into(s, c);
                }
            }
        }
        debug_assert_eq!(a, BitMatrix::identity(n), "elimination did not converge");
        Some(inv)
    }

    /// Rank over GF(2), for any shape.
    ///
    /// The row-echelon half of the same elimination: the number of pivots found.
    /// A square matrix is invertible exactly when this equals its side.
    pub fn rank_gf2(&self) -> u32 {
        self.eliminate(false).1
    }
}

impl BitMatrix {
    /// Solve `self · x = b` over GF(2), where `b` and the returned `x` are
    /// `1 × n` rows holding column vectors.
    ///
    /// `None` if `self` is not square, if `b` is not `1 × n`, or if the system
    /// has no unique solution — singular is exact here, with no pivot tolerance.
    ///
    /// # This delegates to `LU`, and the measurement is why
    ///
    /// It was originally Gauss-Jordan on `[A | b]`, which is a perfectly good
    /// algorithm and **2.8× slower than factoring and substituting even for a
    /// single right-hand side** — Jordan clears above *and* below every pivot,
    /// roughly twice the row operations, and then the augmented column is
    /// touched a bit at a time inside the inner loop.
    ///
    /// ```text
    ///   n=128, one rhs   Gauss-Jordan 23.15 us   via LU  8.34 us   2.78x
    ///   n=512, one rhs   Gauss-Jordan 941.9 us   via LU 249.5 us   3.78x
    /// ```
    ///
    /// **Those numbers describe code that no longer ships**, and the current
    /// benchmark cannot reproduce them: with this method delegating, both arms
    /// of `bitmatrix/gf2/*/solve_*` are the same implementation and measure
    /// 1.00x at one right-hand side, as they must. They are recorded here
    /// because they are the reason for the delegation, not a claim about it.
    ///
    /// So there is no regime where the direct form is the right thing to ship,
    /// and leaving it as the obvious call would have been a performance trap in
    /// the API. It survives as `tests::solve_direct`, which is the differential
    /// oracle both this and [`BitLu::solve`](crate::matrix::BitLu::solve) are
    /// checked against — and is not deleted for being slower.
    ///
    /// For many right-hand sides against one `A`, factor once with
    /// [`Self::lu_gf2`] and reuse it; this call factors every time.
    pub fn solve_gf2(&self, b: &BitMatrix) -> Option<BitMatrix> {
        self.lu_gf2()?.solve(b)
    }

    /// Row echelon form over GF(2): the pivots staircase down and to the right,
    /// with zeros below each one.
    ///
    /// This is the half of the elimination [`Self::rank_gf2`] already runs;
    /// exposing it means a caller wanting a rank *certificate*, a null-space
    /// basis, or the pivot columns does not have to re-derive it.
    pub fn echelon_gf2(&self) -> BitMatrix {
        self.eliminate(false).0
    }

    /// Reduced row echelon form: echelon, and each pivot column cleared above as
    /// well as below, so every pivot is the only set bit in its column.
    pub fn reduced_echelon_gf2(&self) -> BitMatrix {
        self.eliminate(true).0
    }

    /// The shared elimination. Returns the reduced matrix and its pivot count,
    /// which is the rank.
    ///
    /// One implementation, three callers — `rank_gf2`, `echelon_gf2` and
    /// `reduced_echelon_gf2` differ only in `full` and in what they keep. A
    /// second copy of Gauss elimination is exactly the kind of parallel
    /// implementation that drifts.
    fn eliminate(&self, full: bool) -> (BitMatrix, u32) {
        let (rows, cols) = (self.rows(), self.cols());
        let mut a = self.clone();
        let mut pivots = 0u32;
        for c in 0..cols {
            if pivots >= rows {
                break;
            }
            let Some(pivot) = (pivots..rows).find(|&r| a.get(r, c)) else {
                continue;
            };
            a.swap_rows(pivots, pivot);
            let lo = if full { 0 } else { pivots + 1 };
            for s in lo..rows {
                if s != pivots && a.get(s, c) {
                    a.xor_row_into(s, pivots);
                }
            }
            pivots += 1;
        }
        (a, pivots)
    }
}

#[cfg(test)]
pub(in crate::matrix) mod tests {
    use super::*;
    use crate::matrix::Semiring;

    /// Deterministic bit source. No RNG dev-dependency, matching the crate's
    /// convention in `benches/setops.rs` and `tests/differential.rs`.
    fn lcg(state: &mut u64) -> u64 {
        *state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        *state
    }

    /// Invertible **by construction**: the identity with random elementary row
    /// operations applied, each of which preserves invertibility.
    ///
    /// This is what makes the singular-case tests non-vacuous — a suite that
    /// only ever feeds singular matrices passes for a function that always
    /// answers `None`.
    fn invertible(n: u32, seed: u64) -> BitMatrix {
        let mut s = seed;
        let mut m = BitMatrix::identity(n);
        for _ in 0..(4 * n) {
            let a = (lcg(&mut s) % n as u64) as u32;
            let b = (lcg(&mut s) % n as u64) as u32;
            if a != b {
                m.xor_row_into(a, b);
            }
        }
        m
    }

    /// The oracle: the same elimination over `Vec<Vec<bool>>`.
    ///
    /// It shares the *algorithm* and differs only in the representation, so
    /// it catches packing, masking and row-operation bugs — which is where the
    /// risk is — and not a shared misunderstanding of Gauss-Jordan. The
    /// independent checks are `A·A⁻¹ == I` through the separately-verified `mul`,
    /// and `rank_gf2 == n`.
    fn naive_invert(m: &BitMatrix) -> Option<BitMatrix> {
        if m.rows() != m.cols() {
            return None;
        }
        let n = m.rows() as usize;
        let mut a: Vec<Vec<bool>> = (0..n)
            .map(|i| (0..n).map(|j| m.get(i as u32, j as u32)).collect())
            .collect();
        let mut inv: Vec<Vec<bool>> = (0..n).map(|i| (0..n).map(|j| i == j).collect()).collect();
        for c in 0..n {
            let p = (c..n).find(|&r| a[r][c])?;
            a.swap(c, p);
            inv.swap(c, p);
            for s in 0..n {
                if s != c && a[s][c] {
                    for j in 0..n {
                        a[s][j] ^= a[c][j];
                        inv[s][j] ^= inv[c][j];
                    }
                }
            }
        }
        let mut out = BitMatrix::zeros(n as u32, n as u32);
        for (i, row) in inv.iter().enumerate() {
            for (j, &b) in row.iter().enumerate() {
                if b {
                    out.set(i as u32, j as u32, true);
                }
            }
        }
        Some(out)
    }

    /// Rank oracle: row echelon over `Vec<Vec<bool>>`.
    fn naive_rank(m: &BitMatrix) -> u32 {
        let (rows, cols) = (m.rows() as usize, m.cols() as usize);
        let mut a: Vec<Vec<bool>> = (0..rows)
            .map(|i| (0..cols).map(|j| m.get(i as u32, j as u32)).collect())
            .collect();
        let mut pivots = 0usize;
        for c in 0..cols {
            if pivots >= rows {
                break;
            }
            let Some(p) = (pivots..rows).find(|&r| a[r][c]) else {
                continue;
            };
            a.swap(pivots, p);
            // Cloned so the pivot row can be read while another row is written.
            // An oracle is allowed to be slow; it is not allowed to be subtle.
            let pivot_row = a[pivots].clone();
            for row in a.iter_mut().skip(pivots + 1) {
                if row[c] {
                    for (x, &p) in row.iter_mut().zip(&pivot_row) {
                        *x ^= p;
                    }
                }
            }
            pivots += 1;
        }
        pivots as u32
    }

    /// The original Gauss-Jordan solver, kept as the differential oracle for
    /// `solve_gf2` and `BitLu::solve`.
    ///
    /// `pub(in crate::matrix)` so `lu.rs` can diff against it too — two
    /// independent implementations of one function is the arrangement that makes
    /// either trustworthy, and both live tests would otherwise be circular.
    pub(in crate::matrix) fn solve_direct(a: &BitMatrix, b: &BitMatrix) -> Option<BitMatrix> {
        let n = a.rows();
        if n != a.cols() || b.rows() != 1 || b.cols() != n {
            return None;
        }
        let mut a = a.clone();
        let mut x = b.clone();
        for c in 0..n {
            let pivot = (c..n).find(|&r| a.get(r, c))?;
            a.swap_rows(c, pivot);
            if pivot != c {
                let (p, q) = (x.get(0, pivot), x.get(0, c));
                x.set(0, c, p);
                x.set(0, pivot, q);
            }
            for s in 0..n {
                if s != c && a.get(s, c) {
                    a.xor_row_into(s, c);
                    let v = x.get(0, s) ^ x.get(0, c);
                    x.set(0, s, v);
                }
            }
        }
        debug_assert_eq!(a, BitMatrix::identity(n));
        Some(x)
    }

    /// Word boundaries from both sides.
    const SIDES: &[u32] = &[1, 2, 3, 8, 63, 64, 65, 100, 129];

    #[test]
    fn solve_agrees_with_multiplying_by_the_inverse() {
        // Two independent routes to x: the cheap one and the expensive one.
        for &n in SIDES {
            let a = invertible(n, 31);
            let inv = a.invert_gf2().unwrap();
            for seed in [1u64, 2] {
                let mut b = BitMatrix::zeros(1, n);
                let mut s = seed;
                for j in 0..n {
                    if lcg(&mut s).is_multiple_of(3) {
                        b.set(0, j, true);
                    }
                }
                let x = a.solve_gf2(&b).expect("invertible");
                // A·x == b, checked through the product.
                assert_eq!(a.mul_vec(&x, Semiring::Gf2).unwrap(), b, "n={n} A·x != b");
                // And x == A⁻¹·b, the route solve exists to avoid.
                assert_eq!(inv.mul_vec(&b, Semiring::Gf2).unwrap(), x, "n={n}");
                // And it agrees with the independent Gauss-Jordan oracle.
                assert_eq!(solve_direct(&a, &b).unwrap(), x, "n={n} seed={seed}");
            }
        }
    }

    #[test]
    fn solve_refuses_a_singular_system_and_a_bad_shape() {
        let n = 8u32;
        let mut singular = invertible(n, 3);
        for c in 0..n {
            singular.set(1, c, false);
        }
        let b = BitMatrix::zeros(1, n);
        assert!(singular.solve_gf2(&b).is_none(), "singular");
        assert!(invertible(n, 3)
            .solve_gf2(&BitMatrix::zeros(1, n + 1))
            .is_none());
        assert!(invertible(n, 3)
            .solve_gf2(&BitMatrix::zeros(2, n))
            .is_none());
        // Non-square.
        assert!(BitMatrix::zeros(3, 4)
            .solve_gf2(&BitMatrix::zeros(1, 4))
            .is_none());
    }

    #[test]
    fn the_echelon_forms_preserve_rank_and_reduce_further() {
        let mut s = 77u64;
        for &(r, c) in &[
            (1u32, 1u32),
            (3, 5),
            (5, 3),
            (8, 8),
            (63, 65),
            (65, 63),
            (70, 130),
        ] {
            for keep in [2u64, 8] {
                let mut m = BitMatrix::zeros(r, c);
                for i in 0..r {
                    for j in 0..c {
                        if lcg(&mut s).is_multiple_of(keep) {
                            m.set(i, j, true);
                        }
                    }
                }
                let rank = m.rank_gf2();
                let ech = m.echelon_gf2();
                let red = m.reduced_echelon_gf2();
                // Elementary row operations preserve rank.
                assert_eq!(ech.rank_gf2(), rank, "{r}x{c} echelon rank");
                assert_eq!(red.rank_gf2(), rank, "{r}x{c} reduced rank");
                // The reduced form is at most as heavy: clearing above a pivot
                // only ever removes bits from that column.
                assert!(red.count_ones() <= ech.count_ones(), "{r}x{c}");
                // Both are idempotent.
                assert_eq!(red.reduced_echelon_gf2(), red, "{r}x{c} not idempotent");
            }
        }
    }

    #[test]
    fn the_reduced_echelon_form_of_an_invertible_matrix_is_the_identity() {
        for &n in SIDES {
            let a = invertible(n, 19);
            assert_eq!(a.reduced_echelon_gf2(), BitMatrix::identity(n), "n={n}");
        }
    }

    #[test]
    fn a_pivot_is_alone_in_its_column_only_in_the_reduced_form() {
        // The property that distinguishes the two, and a guard against
        // `eliminate(true)` and `eliminate(false)` having become the same thing.
        // The fixture has to put a set bit *above* a pivot, or clearing
        // below is already enough and the two forms coincide. Row 0 carries
        // column 1, whose pivot is row 1:
        //   1 1 0        1 1 0  ( echelon: nothing below to clear )
        //   0 1 0   ->   0 1 0
        //   0 0 1        0 0 1
        let mut m = BitMatrix::zeros(3, 3);
        for (r, c) in [(0, 0), (0, 1), (1, 1), (2, 2)] {
            m.set(r, c, true);
        }
        let ech = m.echelon_gf2();
        let red = m.reduced_echelon_gf2();
        assert_ne!(ech, red, "the two forms must differ on this input");
        assert_eq!(red, BitMatrix::identity(3));
    }

    #[test]
    fn the_inverse_agrees_with_the_naive_oracle() {
        for &n in SIDES {
            for seed in [1u64, 2, 3] {
                let a = invertible(n, seed);
                let got = a.invert_gf2().expect("built invertible");
                assert_eq!(got, naive_invert(&a).unwrap(), "n={n} seed={seed}");
                assert!(got.tail_is_clear(), "n={n}");
            }
        }
    }

    #[test]
    fn a_matrix_times_its_inverse_is_the_identity() {
        // Circular on its own — it uses this module's own `mul`. It is here
        // because it is independent of the *oracle*, not of the crate.
        for &n in SIDES {
            let a = invertible(n, 7);
            let inv = a.invert_gf2().unwrap();
            let id = BitMatrix::identity(n);
            assert_eq!(a.mul(&inv, Semiring::Gf2).unwrap(), id, "A·A⁻¹, n={n}");
            assert_eq!(inv.mul(&a, Semiring::Gf2).unwrap(), id, "A⁻¹·A, n={n}");
        }
    }

    #[test]
    fn inversion_is_an_involution() {
        for &n in SIDES {
            let a = invertible(n, 11);
            let back = a.invert_gf2().unwrap().invert_gf2().unwrap();
            assert_eq!(back, a, "n={n}");
        }
    }

    #[test]
    fn the_identity_is_its_own_inverse() {
        for &n in SIDES {
            let id = BitMatrix::identity(n);
            assert_eq!(id.invert_gf2().unwrap(), id, "n={n}");
        }
    }

    #[test]
    fn a_permutation_inverts_to_its_transpose() {
        let n = 70u32;
        let images: Vec<u32> = (0..n).map(|i| (i * 3 + 11) % n).collect();
        let mut distinct = images.clone();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(distinct.len(), n as usize, "not a bijection");
        let mut p = BitMatrix::zeros(n, n);
        for (i, &c) in images.iter().enumerate() {
            p.set(i as u32, c, true);
        }
        assert_eq!(p.invert_gf2().unwrap(), p.transpose());
    }

    #[test]
    fn a_zero_row_makes_it_singular() {
        for &n in &[2u32, 8, 65] {
            let mut a = invertible(n, 5);
            // Clear row 1 entirely.
            for c in 0..n {
                a.set(1, c, false);
            }
            assert_eq!(a.invert_gf2(), None, "n={n}");
            assert_eq!(a.rank_gf2(), n - 1, "n={n}");
        }
    }

    #[test]
    fn a_duplicated_row_makes_it_singular() {
        for &n in &[2u32, 8, 65] {
            let mut a = invertible(n, 6);
            for c in 0..n {
                let v = a.get(0, c);
                a.set(1, c, v);
            }
            assert_eq!(a.invert_gf2(), None, "n={n}");
            assert_eq!(a.rank_gf2(), n - 1, "n={n}");
        }
    }

    #[test]
    fn the_zero_matrix_is_singular_and_has_rank_zero() {
        for &n in &[1u32, 8, 65] {
            let z = BitMatrix::zeros(n, n);
            assert_eq!(z.invert_gf2(), None, "n={n}");
            assert_eq!(z.rank_gf2(), 0, "n={n}");
        }
    }

    #[test]
    fn a_non_square_matrix_has_no_inverse_but_has_a_rank() {
        let a = BitMatrix::identity(5);
        assert!(a.invert_gf2().is_some());
        for &(r, c) in &[(3u32, 5u32), (5, 3), (1, 64), (64, 1)] {
            let mut m = BitMatrix::zeros(r, c);
            for i in 0..r.min(c) {
                m.set(i, i, true);
            }
            assert_eq!(m.invert_gf2(), None, "{r}x{c} is not square");
            assert_eq!(m.rank_gf2(), r.min(c), "{r}x{c}");
        }
    }

    #[test]
    fn rank_agrees_with_the_naive_oracle() {
        let mut s = 99u64;
        for &(r, c) in &[
            (1u32, 1u32),
            (3, 5),
            (5, 3),
            (8, 8),
            (63, 65),
            (65, 63),
            (64, 64),
            (70, 130),
        ] {
            // A few densities, including one that is almost certainly full rank
            // and one that is almost certainly not.
            for keep in [1u64, 2, 8] {
                let mut m = BitMatrix::zeros(r, c);
                for i in 0..r {
                    for j in 0..c {
                        if lcg(&mut s).is_multiple_of(keep.max(2)) {
                            m.set(i, j, true);
                        }
                    }
                }
                assert_eq!(m.rank_gf2(), naive_rank(&m), "{r}x{c} keep={keep}");
                assert!(m.rank_gf2() <= r.min(c), "rank exceeds the smaller side");
            }
        }
    }

    #[test]
    fn rank_equals_the_side_exactly_when_invertible() {
        for &n in SIDES {
            let a = invertible(n, 13);
            assert_eq!(a.rank_gf2(), n, "invertible n={n}");
            assert!(a.invert_gf2().is_some());

            let mut singular = a.clone();
            for c in 0..n {
                singular.set(0, c, false);
            }
            assert!(singular.rank_gf2() < n, "singular n={n}");
            assert!(singular.invert_gf2().is_none());
        }
    }

    #[test]
    fn rank_is_unchanged_by_elementary_row_operations() {
        // The defining property of rank, and independent of the elimination
        // order the implementation happens to use.
        let mut s = 4242u64;
        for &n in &[8u32, 65] {
            let mut m = BitMatrix::zeros(n, n);
            for i in 0..n {
                for j in 0..n {
                    if lcg(&mut s).is_multiple_of(3) {
                        m.set(i, j, true);
                    }
                }
            }
            let before = m.rank_gf2();
            for _ in 0..20 {
                let a = (lcg(&mut s) % n as u64) as u32;
                let b = (lcg(&mut s) % n as u64) as u32;
                if a != b {
                    m.xor_row_into(a, b);
                }
            }
            assert_eq!(m.rank_gf2(), before, "n={n}");
        }
    }

    #[test]
    fn a_singular_matrix_is_not_reported_invertible_for_the_wrong_reason() {
        // Guards against a `rank_gf2` that always returns `rows`, and against an
        // `invert_gf2` that always returns `None`: both directions must hold on
        // the same shapes.
        let mut some = 0u32;
        let mut none = 0u32;
        for &n in SIDES {
            if invertible(n, 21).invert_gf2().is_some() {
                some += 1;
            }
            if BitMatrix::zeros(n, n).invert_gf2().is_none() {
                none += 1;
            }
        }
        assert_eq!(some, SIDES.len() as u32, "some invertible case failed");
        assert_eq!(none, SIDES.len() as u32, "some singular case was accepted");
    }
}
