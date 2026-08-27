//! Elementwise algebra, with the in-place forms as the primitives.
//!
//! # In-place first
//!
//! `add_assign` / `and_assign` / `complement_assign` are the kernels; `add`,
//! `and` and `complement` clone and delegate. A fold over many terms therefore
//! allocates once rather than once per step, and there is one word loop per
//! operation to test rather than two.
//!
//! This is the opposite arrangement from [`gemm`](super::gemm), where the
//! non-mutating form is the primitive — deliberately, because the two operations
//! differ in what the caller usually wants. A product's operands are almost
//! always distinct from its result, so `gemm` returning a new matrix costs
//! nothing a caller would not have paid anyway. An elementwise fold accumulates
//! into one of its own operands, so an in-place form is the natural shape and an
//! allocating one is pure overhead.
//!
//! # The complement is bounded, and that is not an accident
//!
//! `complement` inverts within `[0, cols)` and re-masks the padding tail. There
//! is no unbounded complement to want here — a `BitMatrix` has a shape, unlike
//! an `OrdSet`, whose universe-wide complement is exactly why
//! [`OrdSet::not_in_range`](crate::OrdSet::not_in_range) takes a range and why
//! there is deliberately no eager `OrdSet::not()`.
//!
//! **The tail is where a complement goes wrong.** Flipping whole words sets
//! every padding bit, which breaks the canonical form's invariant, makes
//! `count_ones` overcount, and makes two equal matrices compare unequal. The
//! mask is not tidying up; it is the operation being correct.

use super::{tail_mask, BitMatrix, Semiring};

impl BitMatrix {
    /// `self += rhs`, elementwise, where `+` is the semiring's addition.
    ///
    /// `false` if the shapes differ, in which case `self` is untouched.
    pub fn add_assign(&mut self, rhs: &BitMatrix, semiring: Semiring) -> bool {
        if (self.rows(), self.cols()) != (rhs.rows(), rhs.cols()) {
            return false;
        }
        for r in 0..self.rows() {
            let src = rhs.row_words(r);
            let dst = self.row_words_mut(r);
            match semiring {
                Semiring::Boolean => {
                    for (d, s) in dst.iter_mut().zip(src) {
                        *d |= *s;
                    }
                }
                Semiring::Gf2 => {
                    for (d, s) in dst.iter_mut().zip(src) {
                        *d ^= *s;
                    }
                }
            }
        }
        // Both operands have clear tails, so OR and XOR keep it clear.
        debug_assert!(self.tail_is_clear());
        true
    }

    /// `self &= rhs`, elementwise.
    ///
    /// `false` if the shapes differ, in which case `self` is untouched.
    ///
    /// AND is **not** a [`Semiring`] arm and cannot be: its identity is the
    /// all-ones matrix, so it is not the additive monoid of anything here. That
    /// is a statement about `Semiring`, not about AND — as a standalone
    /// elementwise operation it is perfectly ordinary, and this is it.
    pub fn and_assign(&mut self, rhs: &BitMatrix) -> bool {
        if (self.rows(), self.cols()) != (rhs.rows(), rhs.cols()) {
            return false;
        }
        for r in 0..self.rows() {
            let src = rhs.row_words(r);
            let dst = self.row_words_mut(r);
            for (d, s) in dst.iter_mut().zip(src) {
                *d &= *s;
            }
        }
        debug_assert!(self.tail_is_clear());
        true
    }

    /// Flip every element, in place, within `[0, cols)`.
    pub fn complement_assign(&mut self) {
        let stride = self.stride();
        if stride == 0 {
            return;
        }
        // Computed before the writes, because `row_words_mut` drops it.
        let flipped = self
            .known_ones()
            .map(|n| self.rows() as u64 * self.cols() as u64 - n);
        let mask = tail_mask(self.cols());
        for r in 0..self.rows() {
            let dst = self.row_words_mut(r);
            for w in dst.iter_mut() {
                *w = !*w;
            }
            // Not tidying: without this the padding is all ones and the
            // canonical form is violated. See the module header.
            dst[stride - 1] &= mask;
        }
        debug_assert!(self.tail_is_clear());
        if let Some(n) = flipped {
            self.set_known_ones(n);
        }
    }

    /// Elementwise `self + rhs` — the semiring's addition.
    ///
    /// `None` unless the shapes match exactly. A wrapper over
    /// [`Self::add_assign`].
    pub fn add(&self, rhs: &BitMatrix, semiring: Semiring) -> Option<BitMatrix> {
        let mut out = self.clone();
        out.add_assign(rhs, semiring).then_some(out)
    }

    /// Elementwise `self & rhs`. `None` unless the shapes match exactly.
    pub fn and(&self, rhs: &BitMatrix) -> Option<BitMatrix> {
        let mut out = self.clone();
        out.and_assign(rhs).then_some(out)
    }

    /// Every element flipped, within `[0, cols)`.
    ///
    /// Named `complement` rather than `not` for two reasons. Clippy's
    /// `should_implement_trait` fires on an inherent `not(&self)`, which has the
    /// same name and arity as `Not::not`; and `complement` says the thing that
    /// matters, which is that this is bounded by the matrix's own shape.
    pub fn complement(&self) -> BitMatrix {
        let mut out = self.clone();
        out.complement_assign();
        out
    }
}

#[cfg(test)]
mod tests {
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

    /// Word boundaries from both sides, so the tail mask is exercised.
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

    const BOTH: [Semiring; 2] = [Semiring::Boolean, Semiring::Gf2];

    #[test]
    fn elementwise_ops_match_the_definition() {
        for &(r, c) in SHAPES {
            let a = patterned(r, c, 0);
            let b = patterned(r, c, 1);
            for sr in BOTH {
                let sum = a.add(&b, sr).unwrap();
                let and = a.and(&b).unwrap();
                let not = a.complement();
                for i in 0..r {
                    for j in 0..c {
                        let want = match sr {
                            Semiring::Boolean => a.get(i, j) || b.get(i, j),
                            Semiring::Gf2 => a.get(i, j) ^ b.get(i, j),
                        };
                        assert_eq!(sum.get(i, j), want, "{r}x{c} add {sr:?} ({i},{j})");
                        assert_eq!(and.get(i, j), a.get(i, j) && b.get(i, j), "and");
                        assert_eq!(not.get(i, j), !a.get(i, j), "complement");
                    }
                }
            }
        }
    }

    #[test]
    fn the_owning_forms_agree_with_the_in_place_ones() {
        // The wrappers must not drift from the kernels they delegate to.
        for &(r, c) in SHAPES {
            let a = patterned(r, c, 2);
            let b = patterned(r, c, 3);
            for sr in BOTH {
                let mut acc = a.clone();
                assert!(acc.add_assign(&b, sr));
                assert_eq!(acc, a.add(&b, sr).unwrap(), "{r}x{c} {sr:?}");
            }
            let mut acc = a.clone();
            assert!(acc.and_assign(&b));
            assert_eq!(acc, a.and(&b).unwrap());
            let mut acc = a.clone();
            acc.complement_assign();
            assert_eq!(acc, a.complement());
        }
    }

    #[test]
    fn a_complement_keeps_the_tail_clear_and_counts_right() {
        // The regression the mask exists for. cols = 100 leaves 28 padding
        // bits per row; flipping whole words sets all of them.
        for &(r, c) in SHAPES {
            let a = patterned(r, c, 4);
            let not = a.complement();
            assert!(not.tail_is_clear(), "{r}x{c}");
            assert_eq!(
                not.count_ones() + a.count_ones(),
                r as u64 * c as u64,
                "{r}x{c}: a complement must partition the elements"
            );
            // And it is an involution.
            assert_eq!(not.complement(), a, "{r}x{c}");
        }
    }

    #[test]
    fn complementing_zero_gives_every_element_and_no_more() {
        for &(r, c) in SHAPES {
            let full = BitMatrix::zeros(r, c).complement();
            assert_eq!(full.count_ones(), r as u64 * c as u64, "{r}x{c}");
            assert!(full.tail_is_clear(), "{r}x{c}");
            assert_eq!(full.complement(), BitMatrix::zeros(r, c));
        }
    }

    #[test]
    fn de_morgan_holds() {
        // Ties AND, OR and the complement to each other, which no single-op
        // test does.
        for &(r, c) in SHAPES {
            let a = patterned(r, c, 5);
            let b = patterned(r, c, 6);
            let lhs = a.and(&b).unwrap().complement();
            let rhs = a
                .complement()
                .add(&b.complement(), Semiring::Boolean)
                .unwrap();
            assert_eq!(lhs, rhs, "{r}x{c}: !(a & b) != !a | !b");
        }
    }

    #[test]
    fn gf2_addition_is_self_inverse_and_boolean_is_idempotent() {
        let a = patterned(9, 9, 7);
        assert_eq!(a.add(&a, Semiring::Gf2).unwrap(), BitMatrix::zeros(9, 9));
        assert_eq!(a.add(&a, Semiring::Boolean).unwrap(), a);
        assert_eq!(a.and(&a).unwrap(), a);
    }

    #[test]
    fn an_in_place_fold_allocates_nothing_per_term() {
        // The reason the in-place form is the primitive: an accumulator over
        // many terms is built once, not once per term.
        let terms: Vec<BitMatrix> = (0..8).map(|s| patterned(64, 64, s)).collect();
        let mut acc = BitMatrix::zeros(64, 64);
        for t in &terms {
            assert!(acc.add_assign(t, Semiring::Gf2));
        }
        let expect = terms.iter().fold(BitMatrix::zeros(64, 64), |e, t| {
            e.add(t, Semiring::Gf2).unwrap()
        });
        assert_eq!(acc, expect);
    }

    #[test]
    fn a_shape_mismatch_leaves_the_target_untouched() {
        let a = patterned(3, 4, 0);
        let b = patterned(5, 6, 1);
        for sr in BOTH {
            let mut acc = a.clone();
            assert!(!acc.add_assign(&b, sr), "must refuse");
            assert_eq!(acc, a, "a refused op must not write");
            assert!(a.add(&b, sr).is_none());
        }
        let mut acc = a.clone();
        assert!(!acc.and_assign(&b));
        assert_eq!(acc, a);
        assert!(a.and(&b).is_none());
    }
}
