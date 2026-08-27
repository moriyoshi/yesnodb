//! `mx_*`: the packed bit-matrix verbs.
//!
//! An `OrdSet` under a [`Layout`] is a series of dense M×N boolean matrices.
//! These verbs read one out, operate on it as a dense value, and put it back —
//! which is the shape `yesno_core::matrix` is built around, so a scenario
//! exercises the real chaining rather than a convenience wrapper.
//!
//! # What these are for
//!
//! **Not** the oracle comparison. `yesno-core`'s own proptests do that better:
//! randomized, boundary-biased, and against a naive `Vec<Vec<bool>>`. Writing a
//! matrix product in Python and comparing it to another Python walk is what got
//! `and_shape.py` moved out of `scenarios/`.
//!
//! The **operational sequence**, which no Rust test covers: build a matrix in
//! a real `Db`, checkpoint, **reopen**, read it back through a `Snapshot`,
//! multiply, store the product, reopen again, and check it survived. A read on a
//! live `Db` is answered from the memtable and never reaches the store — that
//! blind spot hid three bugs.
//!
//! **No verb name may collide with a Python builtin**, which is why every one
//! is prefixed. `mx_put` rather than `mx_set` additionally avoids reading as the
//! `set_*` family, and `mx_identity` / `mx_invert` rather than `mx_id` / `mx_inv`
//! avoid the abbreviations that make a scenario ambiguous.

use monty_types::{MontyException, MontyObject};
use yesno_core::matrix::{BitMatrix, Layout, Order, Semiring};

use crate::convert::{int_obj, type_err, value_err, Args};
use crate::world::{HandleKind, World};

pub const OWNS: &[&str] = &[
    // construction and inspection
    "mx_zeros",
    "mx_identity",
    "mx_put",
    "mx_get",
    "mx_shape",
    "mx_ones",
    "mx_row_bits",
    // the OrdSet boundary
    "mx_read",
    "mx_read_at",
    "mx_to_set",
    // algebra
    "mx_gemm",
    "mx_mul",
    "mx_add",
    "mx_transpose",
    "mx_invert",
    "mx_rank",
    // reductions
    "mx_row_weights",
    "mx_argmax_weight",
];

/// `"bool"` or `"gf2"`, spelled out rather than a flag so a scenario says which
/// algebra it means.
fn semiring_of(verb: &str, s: &str) -> Result<Semiring, MontyException> {
    match s {
        "bool" => Ok(Semiring::Boolean),
        "gf2" => Ok(Semiring::Gf2),
        _ => Err(value_err(format!(
            "{verb}(): semiring must be \"bool\" or \"gf2\", not {s:?}"
        ))),
    }
}

impl World {
    pub(crate) fn call_matrix(
        &mut self,
        verb: &str,
        a: &Args<'_>,
    ) -> Result<MontyObject, MontyException> {
        match verb {
            "mx_zeros" => {
                a.exact(2)?;
                a.no_kwargs()?;
                let (r, c) = (a.u64(0)? as u32, a.u64(1)? as u32);
                Ok(self.push_matrix(BitMatrix::zeros(r, c)))
            }
            "mx_identity" => {
                a.exact(1)?;
                a.no_kwargs()?;
                Ok(self.push_matrix(BitMatrix::identity(a.u64(0)? as u32)))
            }
            // Returns a *new* matrix: values here are immutable, matching the
            // dense-value-in / dense-value-out shape of the Rust API.
            "mx_put" => {
                a.exact(4)?;
                a.no_kwargs()?;
                let h = a.handle(0)?;
                let (r, c) = (a.u64(1)? as u32, a.u64(2)? as u32);
                let v = a.u64(3)? != 0;
                let mut m = self.matrix(h, verb)?.clone();
                if r >= m.rows() || c >= m.cols() {
                    return Err(value_err(format!(
                        "{verb}(): ({r},{c}) is outside a {}x{} matrix",
                        m.rows(),
                        m.cols()
                    )));
                }
                m.set(r, c, v);
                Ok(self.push_matrix(m))
            }
            "mx_get" => {
                a.exact(3)?;
                a.no_kwargs()?;
                let m = self.matrix(a.handle(0)?, verb)?;
                let (r, c) = (a.u64(1)? as u32, a.u64(2)? as u32);
                if r >= m.rows() || c >= m.cols() {
                    return Err(value_err(format!(
                        "{verb}(): ({r},{c}) is outside a {}x{} matrix",
                        m.rows(),
                        m.cols()
                    )));
                }
                Ok(MontyObject::Bool(m.get(r, c)))
            }
            "mx_shape" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let m = self.matrix(a.handle(0)?, verb)?;
                Ok(MontyObject::List(vec![
                    int_obj(m.rows() as u64),
                    int_obj(m.cols() as u64),
                ]))
            }
            "mx_ones" => {
                a.exact(1)?;
                a.no_kwargs()?;
                Ok(int_obj(self.matrix(a.handle(0)?, verb)?.count_ones()))
            }
            // The columns set in one row, as a Python list — so a scenario can
            // compare against a Python `set` without a nested loop.
            "mx_row_bits" => {
                a.exact(2)?;
                a.no_kwargs()?;
                let m = self.matrix(a.handle(0)?, verb)?;
                let r = a.u64(1)? as u32;
                if r >= m.rows() {
                    return Err(value_err(format!(
                        "{verb}(): row {r} is outside a {}-row matrix",
                        m.rows()
                    )));
                }
                let bits: Vec<MontyObject> = (0..m.cols())
                    .filter(|&c| m.get(r, c))
                    .map(|c| int_obj(c as u64))
                    .collect();
                Ok(MontyObject::List(bits))
            }
            "mx_read" => {
                a.exact(4)?;
                a.no_kwargs()?;
                let s = a.handle(0)?;
                let k = a.u64(1)?;
                let l = Layout::dense(a.u64(2)? as u32, a.u64(3)? as u32);
                self.read_into_handle(verb, s, k, l)
            }
            // The full layout, so a scenario can reach a padded stride, a
            // column-major source, and a matrix that straddles a chunk.
            "mx_read_at" => {
                a.exact(7)?;
                a.no_kwargs()?;
                let s = a.handle(0)?;
                let k = a.u64(1)?;
                let l = Layout {
                    rows: a.u64(2)? as u32,
                    cols: a.u64(3)? as u32,
                    line_stride: a.u64(4)? as u32,
                    matrix_stride: a.u64(5)?,
                    order: if a.u64(6)? != 0 {
                        Order::ColMajor
                    } else {
                        Order::RowMajor
                    },
                };
                self.read_into_handle(verb, s, k, l)
            }
            "mx_to_set" => {
                a.exact(2)?;
                a.no_kwargs()?;
                let m = self.matrix(a.handle(0)?, verb)?.clone();
                let k = a.u64(1)?;
                let l = Layout::dense(m.rows(), m.cols());
                let mut sink = yesno_core::matrix::MatrixSink::new(l);
                sink.place(k, &m)
                    .map_err(|e| value_err(format!("{verb}(): {e}")))?;
                Ok(self.push_set(sink.build()))
            }
            "mx_gemm" | "mx_mul" | "mx_add" => {
                let want = if verb == "mx_gemm" { 4 } else { 3 };
                a.exact(want)?;
                a.no_kwargs()?;
                let x = self.matrix(a.handle(0)?, verb)?.clone();
                let y = self.matrix(a.handle(1)?, verb)?.clone();
                let (c, sr_at) = if verb == "mx_gemm" {
                    (Some(self.matrix(a.handle(2)?, verb)?.clone()), 3)
                } else {
                    (None, 2)
                };
                let sr = semiring_of(verb, a.str_at(sr_at)?)?;
                let out = match (verb, c) {
                    ("mx_gemm", Some(c)) => x.gemm(&y, &c, sr),
                    ("mx_mul", _) => x.mul(&y, sr),
                    ("mx_add", _) => x.add(&y, sr),
                    _ => unreachable!("verb list and match arms agree"),
                };
                match out {
                    Some(m) => Ok(self.push_matrix(m)),
                    None => Err(value_err(format!("{verb}(): the shapes do not agree"))),
                }
            }
            "mx_transpose" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let t = self.matrix(a.handle(0)?, verb)?.transpose();
                Ok(self.push_matrix(t))
            }
            // `None` rather than an error: singular is an answer, not a fault.
            "mx_invert" => {
                a.exact(1)?;
                a.no_kwargs()?;
                match self.matrix(a.handle(0)?, verb)?.invert_gf2() {
                    Some(m) => Ok(self.push_matrix(m)),
                    None => Ok(MontyObject::None),
                }
            }
            "mx_rank" => {
                a.exact(1)?;
                a.no_kwargs()?;
                Ok(int_obj(self.matrix(a.handle(0)?, verb)?.rank_gf2() as u64))
            }
            "mx_row_weights" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let w = self.matrix(a.handle(0)?, verb)?.row_weights();
                Ok(MontyObject::List(
                    w.into_iter().map(|x| int_obj(x as u64)).collect(),
                ))
            }
            "mx_argmax_weight" => {
                a.exact(1)?;
                a.no_kwargs()?;
                match self.matrix(a.handle(0)?, verb)?.argmax_weight() {
                    Some((r, w)) => Ok(MontyObject::List(vec![
                        int_obj(r as u64),
                        int_obj(w as u64),
                    ])),
                    None => Ok(MontyObject::None),
                }
            }
            _ => Err(type_err(format!("{verb}() is not a harness verb"))),
        }
    }

    fn read_into_handle(
        &mut self,
        verb: &str,
        set: usize,
        k: u64,
        l: Layout,
    ) -> Result<MontyObject, MontyException> {
        let s = self.set(set, verb)?.clone();
        match s.read_matrix(k, &l) {
            Some(m) => Ok(self.push_matrix(m)),
            // The layout is invalid, or the matrix would reach past
            // ORDINAL_MAX. Both are the caller's mistake, not an empty answer.
            None => Err(value_err(format!(
                "{verb}(): {l:?} at k={k} is not addressable"
            ))),
        }
    }

    pub(crate) fn matrix(&self, h: usize, verb: &str) -> Result<&BitMatrix, MontyException> {
        let i = self.slot(h, HandleKind::Matrix, verb)?;
        Ok(&self.matrices[i])
    }

    pub(crate) fn push_matrix(&mut self, m: BitMatrix) -> MontyObject {
        self.matrices.push(m);
        let idx = self.matrices.len() - 1;
        self.mint(HandleKind::Matrix, idx)
    }
}
