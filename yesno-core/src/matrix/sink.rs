//! [`BitMatrix`] -> `OrdSet`: the scatter, and the other half of the seam.
//!
//! # Why a sink rather than a per-operation encode
//!
//! Every result of the dense algebra is dense by construction, and turning one
//! into container bytes means picking an array / bitmap / run representation and
//! running [`Container::optimize`](crate::Container::optimize), which re-selects
//! the encoding by serialized size. Doing that per operation would pay it for
//! intermediates nothing ever reads.
//!
//! So chained work — read, multiply, transpose, invert — stays in
//! [`BitMatrix`], and the sink encodes **once**, at [`MatrixSink::build`].

use super::{BitMatrix, Layout};
use crate::pack::OrdinalSink;
use crate::{CodecError, OrdSet, Result};

/// Accumulates matrix placements and encodes them into an `OrdSet` in one pass.
///
/// ```
/// use yesno_core::matrix::{BitMatrix, Layout, MatrixSink};
///
/// let layout = Layout::dense(8, 8);
/// let mut sink = MatrixSink::new(layout);
/// sink.place(0, &BitMatrix::identity(8)).unwrap();
/// let set = sink.build();
/// assert_eq!(set.len(), 8);
/// assert_eq!(set.read_matrix(0, &layout).unwrap(), BitMatrix::identity(8));
/// ```
#[derive(Clone, Debug)]
pub struct MatrixSink {
    layout: Layout,
    ordinals: OrdinalSink,
}

impl MatrixSink {
    pub fn new(layout: Layout) -> MatrixSink {
        MatrixSink {
            layout,
            ordinals: OrdinalSink::new(),
        }
    }

    /// Record matrix `m` at index `k`.
    ///
    /// # Errors
    ///
    /// - the layout is not self-consistent — see [`Layout::check`];
    /// - `m`'s shape does not match the layout;
    /// - the matrix would reach above [`ORDINAL_MAX`](crate::ORDINAL_MAX),
    ///   which is [`CodecError::OrdinalOutOfRange`]. `u64::MAX` is not an
    ///   ordinal (invariant I8), so such a matrix cannot be stored at all rather
    ///   than being silently truncated. The reported ordinal saturates at
    ///   `u64::MAX`: an absurd `k` can put the true value past what a `u64`
    ///   holds, and that value is out of range either way.
    ///
    /// The check is made **before** anything is written, so a failed `place`
    /// leaves the sink exactly as it was. It is one comparison, not one per bit:
    /// the highest ordinal a matrix occupies is always its last element, in both
    /// orders, because the offset grows monotonically in each index.
    ///
    /// Placing the same `k` twice unions the two, because the underlying set has
    /// no notion of clearing a bit that a later placement leaves unset. Callers
    /// that mean to replace should build a fresh sink.
    pub fn place(&mut self, k: u64, m: &BitMatrix) -> Result<()> {
        self.layout.check()?;
        let l = self.layout;
        if m.rows() != l.rows || m.cols() != l.cols {
            return Err(CodecError::Invariant(
                "matrix shape does not match the sink's layout",
            ));
        }
        if l.ordinal_at(k, l.rows - 1, l.cols - 1).is_none() {
            // Computed in u128 so the report survives an overflowing `k`.
            let top = k as u128 * l.matrix_stride as u128
                + (l.line_count() as u128 - 1) * l.line_stride as u128
                + (l.line_len() as u128 - 1);
            return Err(CodecError::OrdinalOutOfRange {
                ordinal: top.min(u64::MAX as u128) as u64,
            });
        }
        for r in 0..m.rows() {
            for (wi, &word) in m.row_words(r).iter().enumerate() {
                let mut w = word;
                while w != 0 {
                    let c = (wi * 64) as u32 + w.trailing_zeros();
                    w &= w - 1;
                    let o = l
                        .ordinal_at(k, r, c)
                        .expect("the last element was just checked, and it is the largest");
                    self.ordinals.push(o);
                }
            }
        }
        Ok(())
    }

    /// Encode every placement into an `OrdSet`, choosing each container's
    /// representation once.
    pub fn build(self) -> OrdSet {
        self.ordinals.build()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matrix::Order;

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

    #[test]
    fn placing_then_reading_is_the_identity() {
        let l = Layout::dense(9, 7);
        let m = patterned(9, 7);
        let mut sink = MatrixSink::new(l);
        sink.place(3, &m).unwrap();
        let set = sink.build();
        assert_eq!(set.read_matrix(3, &l).unwrap(), m);
        assert_eq!(set.read_matrix(2, &l).unwrap().count_ones(), 0);
    }

    #[test]
    fn several_matrices_coexist_without_bleeding() {
        let l = Layout::dense(8, 8);
        let a = patterned(8, 8);
        let b = BitMatrix::identity(8);
        let mut sink = MatrixSink::new(l);
        sink.place(0, &a).unwrap();
        sink.place(1, &b).unwrap();
        sink.place(9, &a).unwrap();
        let set = sink.build();
        assert_eq!(set.read_matrix(0, &l).unwrap(), a);
        assert_eq!(set.read_matrix(1, &l).unwrap(), b);
        assert_eq!(set.read_matrix(9, &l).unwrap(), a);
        assert_eq!(set.read_matrix(2, &l).unwrap().count_ones(), 0);
    }

    #[test]
    fn a_shape_mismatch_is_rejected() {
        let l = Layout::dense(4, 4);
        let mut sink = MatrixSink::new(l);
        assert!(sink.place(0, &BitMatrix::zeros(4, 5)).is_err());
        assert!(sink.place(0, &BitMatrix::zeros(5, 4)).is_err());
        assert!(sink.place(0, &BitMatrix::zeros(4, 4)).is_ok());
    }

    #[test]
    fn an_invalid_layout_is_rejected() {
        let bad = Layout {
            line_stride: 1,
            ..Layout::dense(2, 5)
        };
        let mut sink = MatrixSink::new(bad);
        assert!(sink.place(0, &BitMatrix::zeros(2, 5)).is_err());
    }

    #[test]
    fn a_placement_past_the_ceiling_fails_and_changes_nothing() {
        let l = Layout::dense(1, 2);
        let mut full = BitMatrix::zeros(1, 2);
        full.set(0, 0, true);
        full.set(0, 1, true);

        let mut sink = MatrixSink::new(l);
        sink.place(0, &full).unwrap();
        let before = sink.ordinals.len();
        assert_eq!(before, 2);

        // k = u64::MAX/2 puts element (0,1) exactly at u64::MAX, which is not an
        // ordinal (I8) — so the whole placement is refused.
        let k = u64::MAX / 2;
        assert_eq!(l.ordinal_at(k, 0, 0), Some(u64::MAX - 1));
        match sink.place(k, &full) {
            Err(CodecError::OrdinalOutOfRange { ordinal }) => assert_eq!(ordinal, u64::MAX),
            other => panic!("expected OrdinalOutOfRange, got {other:?}"),
        }
        assert_eq!(sink.ordinals.len(), before, "a failed place must not write");
    }

    #[test]
    fn a_matrix_ending_exactly_at_ordinal_max_is_storable() {
        // The boundary from the other side: ORDINAL_MAX itself is a legal
        // element, and only u64::MAX is not.
        let l = Layout::dense(1, 1);
        let k = crate::ORDINAL_MAX;
        let mut one = BitMatrix::zeros(1, 1);
        one.set(0, 0, true);
        let mut sink = MatrixSink::new(l);
        sink.place(k, &one).unwrap();
        let set = sink.build();
        assert!(set.contains(crate::ORDINAL_MAX));
        assert_eq!(set.read_matrix(k, &l).unwrap(), one);
    }

    #[test]
    fn a_col_major_layout_round_trips() {
        let l = Layout {
            order: Order::ColMajor,
            line_stride: 5,
            matrix_stride: 35,
            ..Layout::dense(5, 7)
        };
        let m = patterned(5, 7);
        let mut sink = MatrixSink::new(l);
        sink.place(2, &m).unwrap();
        let set = sink.build();
        assert_eq!(set.read_matrix(2, &l).unwrap(), m);
    }

    #[test]
    fn a_padded_layout_leaves_the_padding_unset() {
        let l = Layout::word_aligned(3, 5);
        let m = patterned(3, 5);
        let mut sink = MatrixSink::new(l);
        sink.place(0, &m).unwrap();
        let set = sink.build();
        assert_eq!(set.len(), m.count_ones(), "padding must not be written");
        // Every set ordinal is within a row's live 5 bits.
        for o in set.iter() {
            assert!(o % 64 < 5, "ordinal {o} landed in the padding");
        }
        assert_eq!(set.read_matrix(0, &l).unwrap(), m);
    }

    #[test]
    fn an_empty_matrix_writes_nothing() {
        let l = Layout::dense(8, 8);
        let mut sink = MatrixSink::new(l);
        sink.place(0, &BitMatrix::zeros(8, 8)).unwrap();
        assert_eq!(sink.build().len(), 0);
    }

    #[test]
    fn build_optimizes_a_run_shaped_result() {
        use crate::ContainerKind;
        // 256x256 is exactly one chunk; a full matrix should encode as a run.
        let l = Layout::dense(256, 256);
        let mut full = BitMatrix::zeros(256, 256);
        for r in 0..256 {
            for c in 0..256 {
                full.set(r, c, true);
            }
        }
        let mut sink = MatrixSink::new(l);
        sink.place(0, &full).unwrap();
        let set = sink.build();
        assert_eq!(set.len(), 65_536);
        let kinds: Vec<ContainerKind> = set.chunks().map(|(_, c)| c.kind()).collect();
        assert_eq!(
            kinds,
            vec![ContainerKind::Run],
            "build() must optimize once"
        );
    }
}
