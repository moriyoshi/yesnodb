//! The generic fallback kernel: a sorted merge over two container iterators.
//!
//! This ships **first**, deliberately. It is simultaneously
//!
//! 1. the initial implementation for all 36 (kind × kind × op) arms,
//! 2. the differential-test oracle that specialized kernels are checked against,
//! 3. the safety net for arms that never justify specialization.
//!
//! Specialize an arm only when a benchmark demands it, and keep this kernel as
//! the oracle forever. It is the antidote to "36 kernels, half of them
//! undertested".

use crate::container::{Container, ContainerIter};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetOp {
    And,
    Or,
    Xor,
    AndNot,
}

impl SetOp {
    /// Should a value present only on the left survive?
    #[inline]
    const fn keep_left(self) -> bool {
        matches!(self, SetOp::Or | SetOp::Xor | SetOp::AndNot)
    }
    /// Should a value present only on the right survive?
    #[inline]
    const fn keep_right(self) -> bool {
        matches!(self, SetOp::Or | SetOp::Xor)
    }
    /// Should a value present on both sides survive?
    #[inline]
    const fn keep_both(self) -> bool {
        matches!(self, SetOp::And | SetOp::Or)
    }
}

/// Merge two sorted `u16` streams under `op`, collecting the surviving values.
///
/// `cap` pre-sizes the output. Growing by doubling instead costs several
/// reallocations per chunk, which is the dominant allocation in a streaming
/// pipeline that builds one intermediate container per shared prefix.
///
/// **There is deliberately no capacity-less variant.** A `merge_values( op,
/// a, b )` wrapper passing `cap = 0` existed until 2026-08-27 with no caller
/// anywhere in the workspace. Dead public API is bad; this was worse, because
/// the warning against using it lived on *this* function rather than on the one
/// a caller would reach for. Found by the unwired-`pub fn` sweep.
pub fn merge_values_with_capacity(
    op: SetOp,
    a: ContainerIter<'_>,
    b: ContainerIter<'_>,
    cap: usize,
) -> Vec<u16> {
    let mut out = Vec::with_capacity(cap);
    let mut a = a.peekable();
    let mut b = b.peekable();
    loop {
        match (a.peek().copied(), b.peek().copied()) {
            (Some(x), Some(y)) => {
                if x < y {
                    a.next();
                    if op.keep_left() {
                        out.push(x);
                    }
                } else if y < x {
                    b.next();
                    if op.keep_right() {
                        out.push(y);
                    }
                } else {
                    a.next();
                    b.next();
                    if op.keep_both() {
                        out.push(x);
                    }
                }
            }
            (Some(x), None) => {
                a.next();
                if op.keep_left() {
                    out.push(x);
                }
            }
            (None, Some(y)) => {
                b.next();
                if op.keep_right() {
                    out.push(y);
                }
            }
            (None, None) => break,
        }
    }
    out
}

/// Apply `op` to two containers. Returns `None` when the result is empty —
/// an empty container is never stored.
pub fn apply(op: SetOp, a: &Container, b: &Container) -> Option<Container> {
    // Short-circuits that skip the merge entirely. `is_full` is cheap (a cached
    // length compare) and these cases are common in practice.
    match op {
        SetOp::And if a.is_full() => return non_empty(b.clone()),
        SetOp::And if b.is_full() => return non_empty(a.clone()),
        SetOp::Or if a.is_full() || b.is_full() => {
            return non_empty(if a.is_full() { a.clone() } else { b.clone() })
        }
        SetOp::AndNot if b.is_full() => return None,
        SetOp::AndNot if b.is_empty() => return non_empty(a.clone()),
        SetOp::Or if a.is_empty() => return non_empty(b.clone()),
        SetOp::Or if b.is_empty() => return non_empty(a.clone()),
        SetOp::And if a.is_empty() || b.is_empty() => return None,
        _ => {}
    }

    // Exact upper bound on the result size, so the output Vec never reallocates.
    let cap = match op {
        SetOp::And => a.len().min(b.len()),
        SetOp::Or | SetOp::Xor => a.len() + b.len(),
        SetOp::AndNot => a.len(),
    } as usize;
    let vals =
        merge_values_with_capacity(op, a.iter(), b.iter(), cap.min(crate::CHUNK_CARD as usize));
    if vals.is_empty() {
        return None;
    }
    Some(Container::from_sorted_vec(vals))
}

#[inline]
fn non_empty(c: Container) -> Option<Container> {
    (!c.is_empty()).then_some(c)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container::{BitmapContainer, RunContainer};
    use std::collections::BTreeSet;

    fn oracle(op: SetOp, a: &[u16], b: &[u16]) -> Vec<u16> {
        let sa: BTreeSet<u16> = a.iter().copied().collect();
        let sb: BTreeSet<u16> = b.iter().copied().collect();
        let mut v: Vec<u16> = match op {
            SetOp::And => sa.intersection(&sb).copied().collect(),
            SetOp::Or => sa.union(&sb).copied().collect(),
            SetOp::Xor => sa.symmetric_difference(&sb).copied().collect(),
            SetOp::AndNot => sa.difference(&sb).copied().collect(),
        };
        v.sort_unstable();
        v
    }

    fn got(op: SetOp, a: &Container, b: &Container) -> Vec<u16> {
        apply(op, a, b)
            .map(|c| c.iter().collect())
            .unwrap_or_default()
    }

    const OPS: [SetOp; 4] = [SetOp::And, SetOp::Or, SetOp::Xor, SetOp::AndNot];

    #[test]
    fn matches_btreeset_oracle_across_all_kind_pairs() {
        let a_vals: Vec<u16> = (0..500u16).map(|i| i * 5).collect();
        let b_vals: Vec<u16> = (0..500u16).map(|i| i * 7).collect();

        // Build each operand in all three representations; results must agree
        // regardless of how the same set happens to be encoded.
        let build = |v: &[u16]| {
            vec![
                Container::from_sorted(v),
                Container::Bitmap(BitmapContainer::from_sorted(v)),
                Container::Run(RunContainer::from_sorted_values(v.iter().copied())),
            ]
        };

        for a in build(&a_vals) {
            for b in build(&b_vals) {
                for op in OPS {
                    assert_eq!(
                        got(op, &a, &b),
                        oracle(op, &a_vals, &b_vals),
                        "{op:?} failed for {:?} vs {:?}",
                        a.kind(),
                        b.kind()
                    );
                }
            }
        }
    }

    #[test]
    fn disjoint_and_identical_operands() {
        let a = Container::from_sorted(&[1, 2, 3]);
        let b = Container::from_sorted(&[10, 11]);
        assert_eq!(got(SetOp::And, &a, &b), Vec::<u16>::new());
        assert_eq!(got(SetOp::Or, &a, &b), vec![1, 2, 3, 10, 11]);
        assert_eq!(got(SetOp::Xor, &a, &b), vec![1, 2, 3, 10, 11]);
        assert_eq!(got(SetOp::AndNot, &a, &b), vec![1, 2, 3]);

        assert_eq!(got(SetOp::And, &a, &a), vec![1, 2, 3]);
        assert_eq!(got(SetOp::Xor, &a, &a), Vec::<u16>::new());
        assert_eq!(got(SetOp::AndNot, &a, &a), Vec::<u16>::new());
    }

    #[test]
    fn full_container_short_circuits_are_correct() {
        let full = Container::Run(RunContainer::from_pairs(&[(0, 65535)]));
        let a = Container::from_sorted(&[5, 9]);
        assert!(full.is_full());

        assert_eq!(got(SetOp::And, &full, &a), vec![5, 9]);
        assert_eq!(got(SetOp::And, &a, &full), vec![5, 9]);
        assert_eq!(apply(SetOp::Or, &a, &full).unwrap().len(), 65536);
        assert_eq!(got(SetOp::AndNot, &a, &full), Vec::<u16>::new());
    }

    #[test]
    fn result_promotes_when_union_exceeds_array_max() {
        let a = Container::from_sorted(&(0..3000u16).collect::<Vec<_>>());
        let b = Container::from_sorted(&(3000..6000u16).collect::<Vec<_>>());
        let r = apply(SetOp::Or, &a, &b).unwrap();
        assert_eq!(r.len(), 6000);
        assert_eq!(r.kind(), crate::ContainerKind::Bitmap, "6000 > ARRAY_MAX");
    }

    #[test]
    fn empty_result_is_none_not_an_empty_container() {
        let a = Container::from_sorted(&[1, 2]);
        assert!(apply(SetOp::And, &a, &Container::from_sorted(&[3])).is_none());
        assert!(apply(SetOp::Xor, &a, &a).is_none());
    }
}
