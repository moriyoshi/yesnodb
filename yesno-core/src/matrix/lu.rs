//! `PA = LU` over GF(2), and what it is for.
//!
//! # Why a factorisation object rather than another solver
//!
//! [`BitMatrix::solve_gf2`] eliminates from scratch for every right-hand side,
//! so solving `k` systems against one `A` costs `k` full eliminations. The whole
//! point of a factorisation is to pay `O(n³/64)` **once** and then answer each
//! right-hand side with two substitutions at `O(n²/64)`:
//!
//! ```text
//!   k solves, direct   k · O(n³/64)
//!   k solves, via LU     O(n³/64) + k · O(n²/64)
//! ```
//!
//! So `lu_gf2` is not a faster `solve_gf2` and does not replace it. For one
//! right-hand side the direct solver is the right call and does less work.
//!
//! # GF(2) makes this simpler than the textbook
//!
//! There is one non-zero scalar, so a multiplier is a bit rather than a value:
//! "is `A[s][c]` set" *is* the multiplier, there is nothing to divide by, and
//! `L`'s diagonal is all ones by construction. Partial pivoting is still
//! required — a zero pivot has to be swapped away — which is why this is
//! `PA = LU` and not `A = LU`.
//!
//! Singularity is exact, with no pivot tolerance: if no row at or below `c` has
//! a bit in column `c`, the matrix is singular and there is no factorisation to
//! return.
//!
//! # The packing, and the trap in it
//!
//! `L` and `U` share one matrix: strictly below the diagonal is `L`'s
//! multipliers, on and above it is `U`. That is why elimination uses
//! [`BitMatrix::xor_row_suffix_into`] and **not** the whole-row XOR — columns
//! below the pivot already hold `L` entries from earlier steps, and a full-row
//! XOR would corrupt them into a matrix that still looks perfectly well formed.

use super::BitMatrix;

/// A GF(2) `LU` factorisation with partial pivoting: `P·A = L·U`.
///
/// Produced by [`BitMatrix::lu_gf2`], which returns `None` for a matrix that is
/// not square or not invertible — so a `BitLu` that exists can always solve, and
/// [`BitLu::solve`] fails only on a malformed right-hand side.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BitLu {
    /// `L` strictly below the diagonal, `U` on and above it.
    lu: BitMatrix,
    /// `perm[i]` is the row of the original matrix now sitting at row `i`.
    perm: Vec<u32>,
}

impl BitMatrix {
    /// Factor `self` as `P·A = L·U` over GF(2).
    ///
    /// `None` if `self` is not square, or is singular. See the module header for
    /// when this is worth doing instead of [`Self::solve_gf2`].
    pub fn lu_gf2(&self) -> Option<BitLu> {
        let n = self.rows();
        if n != self.cols() {
            return None;
        }
        let mut lu = self.clone();
        let mut perm: Vec<u32> = (0..n).collect();

        for c in 0..n {
            let pivot = (c..n).find(|&r| lu.get(r, c))?;
            lu.swap_rows(c, pivot);
            perm.swap(c as usize, pivot as usize);

            for s in (c + 1)..n {
                if lu.get(s, c) {
                    // Eliminate columns `c..` only. The multiplier is 1, so the
                    // update is a plain XOR of the pivot row's suffix — and it
                    // clears `(s, c)`, which is then set again to *store* that
                    // multiplier as `L`.
                    lu.xor_row_suffix_into(s, c, c);
                    debug_assert!(!lu.get(s, c), "the pivot column must be cleared");
                    lu.set(s, c, true);
                }
            }
        }
        Some(BitLu { lu, perm })
    }
}

impl BitLu {
    /// Side of the square matrix that was factored.
    #[inline]
    pub fn size(&self) -> u32 {
        self.lu.rows()
    }

    /// `perm[i]` is the row of the original matrix now at row `i`.
    #[inline]
    pub fn row_order(&self) -> &[u32] {
        &self.perm
    }

    /// `L`: unit lower triangular.
    pub fn l(&self) -> BitMatrix {
        let mut l = self.lu.strictly_lower();
        l.set_diagonal();
        l
    }

    /// `U`: upper triangular, with an all-ones diagonal — over GF(2) a
    /// non-singular `U` has no other option.
    pub fn u(&self) -> BitMatrix {
        self.lu.upper()
    }

    /// `P`, as a permutation matrix, so `P·A == L·U` can be checked directly.
    pub fn permutation(&self) -> BitMatrix {
        let n = self.size();
        let mut p = BitMatrix::zeros(n, n);
        for (i, &src) in self.perm.iter().enumerate() {
            p.set(i as u32, src, true);
        }
        p
    }

    /// Solve `A·x = b`, where `b` and the returned `x` are `1 × n` rows.
    ///
    /// Forward substitution through `L`, then back substitution through `U`,
    /// each `O(n²/64)`. `None` unless `b` is `1 × size()`.
    ///
    /// # Why each substitution is a whole-row operation
    ///
    /// The obvious spelling masks each row to the part of it that has been
    /// solved so far. That mask is unnecessary: during forward substitution
    /// every `z[j]` for `j >= i` is still zero, so AND-ing against the *whole*
    /// packed row already contributes nothing outside the strict lower triangle
    /// — the `U` half and the diagonal are multiplied by zeros. Back
    /// substitution is the mirror image. So both are one AND-popcount per row
    /// with no masking at all.
    pub fn solve(&self, b: &BitMatrix) -> Option<BitMatrix> {
        let n = self.size();
        if b.rows() != 1 || b.cols() != n {
            return None;
        }
        // y = P·b, i.e. the rows of b reordered the way elimination reordered A.
        let mut y = BitMatrix::zeros(1, n);
        for (i, &src) in self.perm.iter().enumerate() {
            if b.get(0, src) {
                y.set(0, i as u32, true);
            }
        }

        // L·z = y. L is unit lower triangular, so z[i] = y[i] + <L_i, z>.
        let mut z = BitMatrix::zeros(1, n);
        for i in 0..n {
            if y.get(0, i) ^ Self::parity(self.lu.row_words(i), z.row_words(0)) {
                z.set(0, i, true);
            }
        }

        // U·x = z. U's diagonal is all ones, so x[i] = z[i] + <U_i, x>.
        let mut x = BitMatrix::zeros(1, n);
        for i in (0..n).rev() {
            if z.get(0, i) ^ Self::parity(self.lu.row_words(i), x.row_words(0)) {
                x.set(0, i, true);
            }
        }
        debug_assert!(x.tail_is_clear());
        Some(x)
    }

    /// Parity of the population count of `a & b`.
    #[inline]
    fn parity(a: &[u64], b: &[u64]) -> bool {
        let n: u32 = a.iter().zip(b).map(|(x, y)| (x & y).count_ones()).sum();
        n % 2 == 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matrix::Semiring;

    fn lcg(state: &mut u64) -> u64 {
        *state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        *state
    }

    /// Invertible by construction, as in `gf2.rs` — a random matrix is singular
    /// often enough that it would mostly test the `None` path.
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

    fn vec_of(n: u32, seed: u64) -> BitMatrix {
        let mut s = seed;
        let mut v = BitMatrix::zeros(1, n);
        for j in 0..n {
            if lcg(&mut s).is_multiple_of(3) {
                v.set(0, j, true);
            }
        }
        v
    }

    const SIDES: &[u32] = &[1, 2, 3, 8, 63, 64, 65, 100, 129];

    /// The defining identity, and the reason `permutation()` is public: a
    /// factorisation you cannot multiply back out is a factorisation you cannot
    /// check.
    #[test]
    fn p_times_a_equals_l_times_u() {
        for &n in SIDES {
            for seed in [1u64, 2] {
                let a = invertible(n, seed);
                let f = a.lu_gf2().expect("built invertible");
                let pa = f.permutation().mul(&a, Semiring::Gf2).unwrap();
                let lu = f.l().mul(&f.u(), Semiring::Gf2).unwrap();
                assert_eq!(pa, lu, "n={n} seed={seed}");
            }
        }
    }

    /// **This does not catch the packing trap, and that is worth knowing.**
    /// Replacing `xor_row_suffix_into` with a whole-row XOR corrupts `L`'s
    /// stored multipliers — and the result is *still* unit lower triangular, so
    /// every assertion here passes. Only `p_times_a_equals_l_times_u` and the
    /// solve test see it. A structural check is not a correctness check.
    #[test]
    fn l_is_unit_lower_and_u_is_upper() {
        for &n in SIDES {
            let f = invertible(n, 5).lu_gf2().unwrap();
            let (l, u) = (f.l(), f.u());
            // L: unit diagonal, nothing above it.
            assert_eq!(l.trace(), n, "n={n} L diagonal");
            assert!(l.strictly_upper().is_zero(), "n={n} L is not lower");
            // U: nothing below the diagonal, and over GF(2) a non-singular U
            // must have an all-ones diagonal.
            assert!(u.strictly_lower().is_zero(), "n={n} U is not upper");
            assert_eq!(u.trace(), n, "n={n} U diagonal");
            // Both are invertible, which is what makes the factorisation useful.
            assert_eq!(l.rank_gf2(), n, "n={n}");
            assert_eq!(u.rank_gf2(), n, "n={n}");
        }
    }

    /// The permutation is a permutation, not merely a matrix.
    #[test]
    fn the_permutation_is_a_bijection() {
        for &n in SIDES {
            let f = invertible(n, 7).lu_gf2().unwrap();
            let mut seen = f.row_order().to_vec();
            seen.sort_unstable();
            assert_eq!(seen, (0..n).collect::<Vec<_>>(), "n={n}");
            let p = f.permutation();
            assert_eq!(p.count_ones(), n as u64, "n={n}");
            assert_eq!(
                p.mul(&p.transpose(), Semiring::Gf2).unwrap(),
                BitMatrix::identity(n),
                "n={n}: P·Pᵀ"
            );
        }
    }

    /// The oracle is `gf2::tests::solve_direct`, the Gauss-Jordan solver —
    /// **not** `solve_gf2`, which now delegates here and would make this
    /// circular. Two independent implementations of one function is the
    /// arrangement `ops::generic` has with the specialized kernels, and losing
    /// it by accident is exactly what delegating a public method can do.
    #[test]
    fn lu_solve_agrees_with_the_direct_solver() {
        for &n in SIDES {
            let a = invertible(n, 11);
            let f = a.lu_gf2().unwrap();
            for seed in [1u64, 2, 3] {
                let b = vec_of(n, seed);
                let x = f.solve(&b).expect("well-formed rhs");
                assert_eq!(
                    x,
                    crate::matrix::gf2::tests::solve_direct(&a, &b).unwrap(),
                    "n={n} seed={seed}"
                );
                // And it really is a solution.
                assert_eq!(a.mul_vec(&x, Semiring::Gf2).unwrap(), b, "n={n} A·x != b");
            }
        }
    }

    #[test]
    fn one_factorisation_answers_many_right_hand_sides() {
        // The property the type exists for: the factorisation is independent of
        // `b`, so it is computed once and reused.
        let n = 64u32;
        let a = invertible(n, 13);
        let f = a.lu_gf2().unwrap();
        for seed in 0..16u64 {
            let b = vec_of(n, seed + 100);
            assert_eq!(a.mul_vec(&f.solve(&b).unwrap(), Semiring::Gf2).unwrap(), b);
        }
    }

    #[test]
    fn solving_the_identity_returns_the_right_hand_side() {
        for &n in SIDES {
            let f = BitMatrix::identity(n).lu_gf2().unwrap();
            let b = vec_of(n, 21);
            assert_eq!(f.solve(&b).unwrap(), b, "n={n}");
            // The identity needs no pivoting.
            assert_eq!(f.row_order(), (0..n).collect::<Vec<_>>(), "n={n}");
        }
    }

    /// A matrix whose first pivot is missing forces a row swap, which is the
    /// only thing that makes `P` non-trivial — a corpus of matrices that never
    /// pivot leaves the permutation untested.
    #[test]
    fn a_zero_leading_entry_forces_a_pivot_swap() {
        // [[0,1],[1,0]] must swap rows 0 and 1.
        let mut a = BitMatrix::zeros(2, 2);
        a.set(0, 1, true);
        a.set(1, 0, true);
        let f = a.lu_gf2().unwrap();
        assert_eq!(f.row_order(), &[1, 0], "the pivot search must have swapped");
        assert_ne!(f.permutation(), BitMatrix::identity(2));
        assert_eq!(
            f.permutation().mul(&a, Semiring::Gf2).unwrap(),
            f.l().mul(&f.u(), Semiring::Gf2).unwrap()
        );
        // And the solve still works through the permutation.
        let mut b = BitMatrix::zeros(1, 2);
        b.set(0, 0, true);
        let x = f.solve(&b).unwrap();
        assert_eq!(a.mul_vec(&x, Semiring::Gf2).unwrap(), b);
    }

    #[test]
    fn some_case_in_the_corpus_actually_pivots() {
        // Guards the guard: if no fixture ever needed a swap, every assertion
        // about `P` above would hold vacuously.
        let pivoted = SIDES
            .iter()
            .filter(|&&n| n > 1)
            .flat_map(|&n| (0..4u64).map(move |s| (n, s)))
            .filter(|&(n, s)| {
                invertible(n, s)
                    .lu_gf2()
                    .is_some_and(|f| f.row_order() != (0..n).collect::<Vec<_>>())
            })
            .count();
        assert!(pivoted > 0, "no fixture ever required a row swap");
    }

    #[test]
    fn a_singular_or_non_square_matrix_has_no_factorisation() {
        for &n in &[2u32, 8, 65] {
            let mut a = invertible(n, 17);
            for c in 0..n {
                a.set(1, c, false);
            }
            assert!(a.lu_gf2().is_none(), "n={n} zero row");

            let mut dup = invertible(n, 19);
            for c in 0..n {
                let v = dup.get(0, c);
                dup.set(1, c, v);
            }
            assert!(dup.lu_gf2().is_none(), "n={n} duplicated row");
            assert!(BitMatrix::zeros(n, n).lu_gf2().is_none(), "n={n} zero");
        }
        assert!(BitMatrix::zeros(3, 4).lu_gf2().is_none(), "non-square");
        // And the invertible direction, so this is not passing vacuously.
        assert!(invertible(8, 23).lu_gf2().is_some());
    }

    #[test]
    fn solve_rejects_a_malformed_right_hand_side() {
        let f = invertible(8, 29).lu_gf2().unwrap();
        assert!(f.solve(&BitMatrix::zeros(1, 7)).is_none());
        assert!(f.solve(&BitMatrix::zeros(2, 8)).is_none());
        assert!(f.solve(&BitMatrix::zeros(1, 8)).is_some());
        assert_eq!(f.size(), 8);
    }
}
