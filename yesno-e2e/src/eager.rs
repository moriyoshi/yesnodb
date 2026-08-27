//! The eager half of the verb surface: building `OrdSet`s, taking them apart,
//! and running the container kernels by hand.
//!
//! # Why a scenario needs sets it did not store
//!
//! Every verb in [`crate::world`] proper reaches the library *through a
//! database*, which is right for operational sequences and wrong for the
//! measurement fixtures in `e2e/scenarios/`. Those measure set algebra,
//! chunk-level kernels and stream operators on synthetic operands — a hundred
//! thousand chunks of a chosen shape — and a database is not in the picture at
//! all. Without a way to build an `OrdSet` directly, migrating them would mean
//! ingesting ten million ordinals through the WAL to get an operand that
//! `OrdSet::from_sorted_slice` produces in one pass.
//!
//! # Why a builder rather than `set_of([...])`
//!
//! `set_of` exists and is the right verb for a dozen ordinals. It is the wrong
//! one for the fixtures, and not by a constant factor: `scenarios/aligned_eval.py`
//! builds an operand of ~8.6 million ordinals, and materializing that as a
//! Python list inside a bytecode VM costs more than every other part of the
//! scenario together — assuming it fits at all.
//!
//! So the builder takes **arithmetic progressions**, not values:
//! `sb_stride( sb, base, step, count )` is one host call whatever `count` is.
//! That is not a fixture-shaped convenience; it is the shape every one of those
//! operands actually has. `(p << 16) | l` over a prefix range is a stride of
//! `65536`; a contiguous range is a stride of `1`; the `band` helper in
//! `aligned_eval` is one stride per prefix. A scenario composes progressions and
//! explicit values into one builder and pays for the sort once.
//!
//! Deliberately **no `set_range` verb**. yesno already spells ranges two ways
//! — `db_insert_range` is inclusive, `q_range` is half-open — and a third
//! spelling at the set level would be a trap rather than a convenience.
//! `sb_stride( sb, lo, 1, hi - lo )` says which one it means.
//!
//! # Container handles are a real handle kind
//!
//! `ops::and` and friends return `Option<Container>`, and `None` — "the result
//! is empty" — is load-bearing in every k-way walk in `scenarios/and_shape.py`:
//! it is what the early-out tests. So `ops_and` returns a handle or Python
//! `None`, and a scenario branches on `is None` exactly where the Rust branches
//! on the `Option`. Containers are frozen and cloning one is a refcount bump,
//! so handing them out by handle costs nothing.

use std::sync::Arc;

use monty_types::{MontyException, MontyObject};
use yesno_core::{ops, Container, ContainerKind, OrdSet};

use crate::convert::{int_obj, opt_int_obj, tuple, type_err, value_err, whole_obj, Args};
use crate::world::{stale_handle, HandleKind, World};

/// The verbs this module dispatches. Merged into [`crate::world::NAMES`],
/// which stays the single source of truth for name resolution.
pub const OWNS: &[&str] = &[
    // builders
    "sb_new",
    "sb_stride",
    "sb_values",
    "sb_build",
    // sets
    "set_of",
    "set_len",
    "set_is_empty",
    "set_min",
    "set_max",
    "set_contains",
    "set_rank",
    "set_select",
    "set_to_list",
    "set_chunk_count",
    "set_prefix_at",
    "set_chunk_at",
    "set_partition_point_in",
    "set_and",
    "set_or",
    "set_xor",
    "set_andnot",
    "set_union_all",
    // containers
    "ct_full",
    "ct_len",
    "ct_is_full",
    "ct_kind",
    "ct_run_count",
    "ops_and",
    "ops_or",
    "ops_xor",
    "ops_andnot",
];

/// Most ordinals one builder may accumulate before it is built.
///
/// A bound rather than trust, because `sb_stride` takes a count and a scenario
/// asking for `2^40` ordinals would be an out-of-memory kill with no line
/// number rather than a failure that names the file. The value clears the
/// largest operand any migrated fixture builds ( ~8.6 M, in `aligned_eval` ) by
/// a factor of two and costs 128 MiB at the limit.
pub const BUILDER_MAX_ORDINALS: usize = 16 << 20;

impl World {
    pub(crate) fn call_eager(
        &mut self,
        verb: &str,
        a: &Args<'_>,
    ) -> Result<MontyObject, MontyException> {
        match verb {
            "sb_new" => {
                a.exact(0)?;
                a.no_kwargs()?;
                self.builders.push(Some(Vec::new()));
                let idx = self.builders.len() - 1;
                Ok(self.mint(HandleKind::Builder, idx))
            }
            // The workhorse. `base + i * step` for `i` in `0..count`, appended
            // in one host call however large `count` is.
            "sb_stride" => {
                a.exact(4)?;
                a.no_kwargs()?;
                let (h, base, step) = (a.handle(0)?, a.u64(1)?, a.u64(2)?);
                let count = a.usize_at(3)?;
                let vals = stride(verb, base, step, count)?;
                self.append(h, verb, vals)
            }
            "sb_values" => {
                a.exact(2)?;
                a.no_kwargs()?;
                let (h, vals) = (a.handle(0)?, a.u64_list(1)?);
                self.append(h, verb, vals)
            }
            // Consumes the builder: a second `sb_build` on the same handle is a
            // stale-handle error, not a second identical set. Building twice is
            // always a scenario bug, and silently succeeding would hide it.
            "sb_build" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let h = a.handle(0)?;
                let vals = self.take_builder(h, verb)?;
                Ok(self.push_set(OrdSet::from_iter_unsorted(vals)))
            }

            "set_of" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let vals = a.u64_list(0)?;
                check_budget(verb, vals.len())?;
                Ok(self.push_set(OrdSet::from_iter_unsorted(vals)))
            }
            "set_len" => {
                a.exact(1)?;
                a.no_kwargs()?;
                Ok(int_obj(self.set(a.handle(0)?, verb)?.len()))
            }
            "set_is_empty" => {
                a.exact(1)?;
                a.no_kwargs()?;
                Ok(MontyObject::Bool(self.set(a.handle(0)?, verb)?.is_empty()))
            }
            "set_min" => {
                a.exact(1)?;
                a.no_kwargs()?;
                Ok(opt_int_obj(self.set(a.handle(0)?, verb)?.min()))
            }
            "set_max" => {
                a.exact(1)?;
                a.no_kwargs()?;
                Ok(opt_int_obj(self.set(a.handle(0)?, verb)?.max()))
            }
            "set_contains" => {
                a.exact(2)?;
                a.no_kwargs()?;
                let (h, ord) = (a.handle(0)?, a.u64(1)?);
                Ok(MontyObject::Bool(self.set(h, verb)?.contains(ord)))
            }
            // `rank` is strictly-less-than and `select` is zero-based, exactly
            // as `snap_rank` / `snap_select` mirror them. Not adjusted here
            // either — the two inverting is the property worth being able to
            // assert.
            "set_rank" => {
                a.exact(2)?;
                a.no_kwargs()?;
                let (h, ord) = (a.handle(0)?, a.u64(1)?);
                Ok(int_obj(self.set(h, verb)?.rank(ord)))
            }
            "set_select" => {
                a.exact(2)?;
                a.no_kwargs()?;
                let (h, n) = (a.handle(0)?, a.u64(1)?);
                Ok(opt_int_obj(self.set(h, verb)?.select(n)))
            }
            "set_to_list" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let s = self.set(a.handle(0)?, verb)?;
                Ok(MontyObject::List(s.iter().map(int_obj).collect()))
            }

            "set_chunk_count" => {
                a.exact(1)?;
                a.no_kwargs()?;
                Ok(whole_obj(self.set(a.handle(0)?, verb)?.chunk_count()))
            }
            "set_prefix_at" => {
                a.exact(2)?;
                a.no_kwargs()?;
                let (h, i) = (a.handle(0)?, a.usize_at(1)?);
                Ok(opt_int_obj(self.set(h, verb)?.prefix_at(i)))
            }
            // `( prefix, container )`, or `None` past the end — the shape
            // `OrdSet::chunk_at` returns, so a scenario walking chunks reads the
            // same way the Rust does.
            "set_chunk_at" => {
                a.exact(2)?;
                a.no_kwargs()?;
                let (h, i) = (a.handle(0)?, a.usize_at(1)?);
                let Some((p, c)) = self.set(h, verb)?.chunk_at(i).map(|(p, c)| (p, c.clone()))
                else {
                    return Ok(MontyObject::None);
                };
                let ch = self.push_container(c);
                Ok(tuple(vec![int_obj(p), ch]))
            }
            "set_partition_point_in" => {
                a.exact(4)?;
                a.no_kwargs()?;
                let (h, lo, hi, p) = (a.handle(0)?, a.usize_at(1)?, a.usize_at(2)?, a.u64(3)?);
                let s = self.set(h, verb)?;
                if lo > hi || hi > s.chunk_count() {
                    return Err(value_err(format!(
                        "{verb}(): [{lo}, {hi}) is not a sub-range of this set's {} chunks",
                        s.chunk_count()
                    )));
                }
                Ok(whole_obj(s.partition_point_in(lo, hi, p)))
            }

            "set_and" | "set_or" | "set_xor" | "set_andnot" => {
                a.exact(2)?;
                a.no_kwargs()?;
                let (l, r) = (a.handle(0)?, a.handle(1)?);
                let (ls, rs) = (self.set(l, verb)?.clone(), self.set(r, verb)?.clone());
                let out = match verb {
                    "set_and" => ls.and(&rs),
                    "set_or" => ls.or(&rs),
                    "set_xor" => ls.xor(&rs),
                    _ => ls.and_not(&rs),
                };
                Ok(self.push_set(out))
            }
            "set_union_all" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let handles = a.handle_list(0)?;
                let owned: Vec<Arc<OrdSet>> = handles
                    .iter()
                    .map(|h| self.set(*h, verb).cloned())
                    .collect::<Result<_, _>>()?;
                let refs: Vec<&OrdSet> = owned.iter().map(AsRef::as_ref).collect();
                Ok(self.push_set(OrdSet::union_all(&refs)))
            }

            "ct_full" => {
                a.exact(0)?;
                a.no_kwargs()?;
                Ok(self.push_container(Container::full()))
            }
            "ct_len" => {
                a.exact(1)?;
                a.no_kwargs()?;
                Ok(int_obj(u64::from(
                    self.container(a.handle(0)?, verb)?.len(),
                )))
            }
            // `O(1)`, and that is the point: it is what makes the three-valued
            // classification in `scenarios/aligned_eval.py` exact rather than a
            // conservative approximation.
            "ct_is_full" => {
                a.exact(1)?;
                a.no_kwargs()?;
                Ok(MontyObject::Bool(
                    self.container(a.handle(0)?, verb)?.is_full(),
                ))
            }
            "ct_kind" => {
                a.exact(1)?;
                a.no_kwargs()?;
                Ok(MontyObject::String(
                    match self.container(a.handle(0)?, verb)?.kind() {
                        ContainerKind::Array => "array",
                        ContainerKind::Bitmap => "bitmap",
                        ContainerKind::Run => "run",
                    }
                    .to_owned(),
                ))
            }

            // Maximal runs of consecutive ordinals — the second axis of the
            // `( m, r )` plane, and the one no verb previously exposed. A
            // scenario could report the container *kind* mix and the cardinality
            // but not whether a `Run` holds two intervals or two thousand, which
            // is the difference between the fastest arm in the crate and its
            // slowest.
            //
            // **The cost is asymmetric and the fast case is the misleading
            // one.** On the run arm this reads the stored `nruns` prefix and is
            // `O(1)`; on an array it scans up to `ARRAY_MAX` `u16`s and on a
            // bitmap it walks 1024 words. So it is free exactly where it has
            // least to say, and a payload walk on the two kinds holding nearly
            // all the bytes. Do not call it per chunk over a whole database
            // and describe the result as an index scan — that is the mistake
            // `Snapshot::cardinality` made once, where the materializing and
            // non-materializing forms return the same number and no correctness
            // test can tell them apart.
            "ct_run_count" => {
                a.exact(1)?;
                a.no_kwargs()?;
                Ok(int_obj(u64::from(
                    self.container(a.handle(0)?, verb)?.run_count(),
                )))
            }

            // `None` means the result is empty, mirroring `Option<Container>`.
            // A scenario testing `is None` is testing the same condition the
            // engine's early-outs test.
            "ops_and" | "ops_or" | "ops_xor" | "ops_andnot" => {
                a.exact(2)?;
                a.no_kwargs()?;
                let (l, r) = (a.handle(0)?, a.handle(1)?);
                let (lc, rc) = (
                    self.container(l, verb)?.clone(),
                    self.container(r, verb)?.clone(),
                );
                let out = match verb {
                    "ops_and" => ops::and(&lc, &rc),
                    "ops_or" => ops::or(&lc, &rc),
                    "ops_xor" => ops::xor(&lc, &rc),
                    _ => ops::and_not(&lc, &rc),
                };
                Ok(self.push_opt_container(out))
            }

            _ => Err(type_err(format!("{verb}() is not a harness verb"))),
        }
    }

    fn append(
        &mut self,
        h: usize,
        verb: &str,
        mut vals: Vec<u64>,
    ) -> Result<MontyObject, MontyException> {
        let i = self.slot(h, HandleKind::Builder, verb)?;
        match self.builders[i] {
            None => Err(stale_handle(verb, "set builder", h)),
            Some(ref mut buf) => {
                check_budget(verb, buf.len() + vals.len())?;
                buf.append(&mut vals);
                Ok(MontyObject::None)
            }
        }
    }

    fn take_builder(&mut self, h: usize, verb: &str) -> Result<Vec<u64>, MontyException> {
        let i = self.slot(h, HandleKind::Builder, verb)?;
        self.builders[i]
            .take()
            .ok_or_else(|| stale_handle(verb, "set builder", h))
    }
}

/// `base + i * step` for `i` in `0..count`, refusing anything that would leave
/// the ordinal space rather than wrapping into it.
fn stride(verb: &str, base: u64, step: u64, count: usize) -> Result<Vec<u64>, MontyException> {
    check_budget(verb, count)?;
    if count == 0 {
        return Ok(Vec::new());
    }
    // Checked up front, not per element: the failure a scenario wants named is
    // "your stride runs off the end", and reporting it after building 8 million
    // ordinals would be both slower and less clear.
    let span = (count as u64 - 1)
        .checked_mul(step)
        .and_then(|s| base.checked_add(s))
        .ok_or_else(|| {
            value_err(format!(
                "{verb}(): base {base} + {} x step {step} overflows u64",
                count - 1
            ))
        })?;
    if !yesno_core::is_valid_ordinal(span) {
        return Err(value_err(format!(
            "{verb}(): the stride ends at {span}, above ORDINAL_MAX ({})",
            yesno_core::ORDINAL_MAX
        )));
    }
    Ok((0..count as u64).map(|i| base + i * step).collect())
}

fn check_budget(verb: &str, n: usize) -> Result<(), MontyException> {
    if n > BUILDER_MAX_ORDINALS {
        return Err(value_err(format!(
            "{verb}(): {n} ordinals exceeds the {BUILDER_MAX_ORDINALS}-ordinal builder budget; \
             build the operand in pieces and combine them with set_union_all()"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use monty_types::MontyObject as M;

    fn w() -> World {
        World::temporary().unwrap()
    }

    fn ints(v: &[i64]) -> MontyObject {
        M::List(v.iter().map(|x| M::Int(*x)).collect())
    }

    /// A stride must land exactly where the arithmetic says, including the
    /// `p << 16` shape every migrated fixture is built from.
    #[test]
    fn a_stride_is_an_arithmetic_progression() {
        let mut w = w();
        let b = w.call("sb_new", &[], &[]).unwrap();
        w.call(
            "sb_stride",
            &[b.clone(), M::Int(0), M::Int(1 << 16), M::Int(4)],
            &[],
        )
        .unwrap();
        let s = w.call("sb_build", std::slice::from_ref(&b), &[]).unwrap();
        let got = w
            .call("set_to_list", std::slice::from_ref(&s), &[])
            .unwrap();
        assert_eq!(got, ints(&[0, 65536, 131_072, 196_608]));
    }

    /// The overflow guard has to fire, or a stride that wraps would silently
    /// produce a set nobody asked for.
    #[test]
    fn a_stride_that_leaves_the_ordinal_space_is_refused() {
        let mut w = w();
        let b = w.call("sb_new", &[], &[]).unwrap();
        let err = w
            .call(
                "sb_stride",
                &[b, M::Int(i64::MAX), M::Int(i64::MAX), M::Int(4)],
                &[],
            )
            .unwrap_err();
        assert!(err.summary().contains("overflow"), "{}", err.summary());
    }

    #[test]
    fn a_builder_may_not_be_built_twice() {
        let mut w = w();
        let b = w.call("sb_new", &[], &[]).unwrap();
        w.call("sb_values", &[b.clone(), ints(&[1, 2, 3])], &[])
            .unwrap();
        w.call("sb_build", std::slice::from_ref(&b), &[]).unwrap();
        let err = w
            .call("sb_build", std::slice::from_ref(&b), &[])
            .unwrap_err();
        assert!(
            err.summary().contains("already been closed or released"),
            "{}",
            err.summary()
        );
    }

    /// The budget must be enforced on the accumulated total, not per call —
    /// otherwise a loop of legal strides walks straight past it.
    #[test]
    fn the_builder_budget_counts_the_accumulated_total() {
        let mut w = w();
        let b = w.call("sb_new", &[], &[]).unwrap();
        let big = (BUILDER_MAX_ORDINALS / 2 + 1) as i64;
        w.call(
            "sb_stride",
            &[b.clone(), M::Int(0), M::Int(1), M::Int(big)],
            &[],
        )
        .unwrap();
        let err = w
            .call("sb_stride", &[b, M::Int(0), M::Int(1), M::Int(big)], &[])
            .unwrap_err();
        assert!(err.summary().contains("budget"), "{}", err.summary());
    }

    /// `ops_*` must report an empty result as `None` rather than as an empty
    /// container — every k-way early-out in `scenarios/and_shape.py` branches on
    /// exactly this.
    #[test]
    fn an_empty_intersection_is_none_not_an_empty_container() {
        let mut w = w();
        let a = build(&mut w, &[1, 2, 3]);
        let b = build(&mut w, &[4, 5, 6]);
        let ca = chunk0(&mut w, &a);
        let cb = chunk0(&mut w, &b);
        assert_eq!(
            w.call("ops_and", &[ca.clone(), cb.clone()], &[]).unwrap(),
            M::None
        );
        assert_ne!(w.call("ops_or", &[ca, cb], &[]).unwrap(), M::None);
    }

    #[test]
    fn a_full_container_reports_itself_full() {
        let mut w = w();
        let c = w.call("ct_full", &[], &[]).unwrap();
        assert_eq!(
            w.call("ct_is_full", std::slice::from_ref(&c), &[]).unwrap(),
            M::Bool(true)
        );
        assert_eq!(
            w.call("ct_len", std::slice::from_ref(&c), &[]).unwrap(),
            M::Int(65536)
        );
    }

    /// The run count must distinguish shapes that `ct_kind` and `ct_len` cannot.
    ///
    /// The assertion that matters is the **third** one: two containers with
    /// the same kind and the same cardinality, differing only in how their
    /// ordinals are grouped. A verb that returned the cardinality, or `1`, or
    /// the interval count of the first run would satisfy the first two cases
    /// and fail here.
    #[test]
    fn the_run_count_separates_shapes_the_other_container_verbs_cannot() {
        let mut w = w();
        let rc = |w: &mut World, c: &M| {
            w.call("ct_run_count", std::slice::from_ref(c), &[])
                .unwrap()
        };

        // One contiguous stretch is one run, whatever its length.
        let contiguous = build(&mut w, &(0..600i64).collect::<Vec<_>>());
        let c = chunk0(&mut w, &contiguous);
        assert_eq!(rc(&mut w, &c), M::Int(1));

        // A full container is the degenerate single run.
        let full = w.call("ct_full", &[], &[]).unwrap();
        assert_eq!(rc(&mut w, &full), M::Int(1));

        // Same kind, same cardinality, different grouping: 300 ordinals as 300
        // singletons against 300 ordinals as one stretch.
        let scattered = build(&mut w, &(0..300i64).map(|v| v * 3).collect::<Vec<_>>());
        let packed = build(&mut w, &(0..300i64).collect::<Vec<_>>());
        let (cs, cp) = (chunk0(&mut w, &scattered), chunk0(&mut w, &packed));
        assert_eq!(
            w.call("ct_len", std::slice::from_ref(&cs), &[]).unwrap(),
            w.call("ct_len", std::slice::from_ref(&cp), &[]).unwrap(),
            "the two shapes must be indistinguishable by cardinality"
        );
        assert_eq!(
            w.call("ct_kind", std::slice::from_ref(&cs), &[]).unwrap(),
            w.call("ct_kind", std::slice::from_ref(&cp), &[]).unwrap(),
            "and indistinguishable by kind, or this proves nothing"
        );
        assert_eq!(rc(&mut w, &cs), M::Int(300));
        assert_eq!(rc(&mut w, &cp), M::Int(1));
    }

    /// Set algebra through the harness must agree with Python's own `set`,
    /// which is what every scenario uses as its oracle.
    #[test]
    fn set_algebra_agrees_with_the_obvious_answer() {
        let mut w = w();
        let a = build(&mut w, &[1, 2, 3, 4, 5]);
        let b = build(&mut w, &[4, 5, 6]);
        let and = w.call("set_and", &[a.clone(), b.clone()], &[]).unwrap();
        assert_eq!(
            w.call("set_to_list", std::slice::from_ref(&and), &[])
                .unwrap(),
            ints(&[4, 5])
        );
        let all = w
            .call("set_union_all", &[M::List(vec![a, b])], &[])
            .unwrap();
        assert_eq!(
            w.call("set_to_list", std::slice::from_ref(&all), &[])
                .unwrap(),
            ints(&[1, 2, 3, 4, 5, 6])
        );
    }

    fn build(w: &mut World, vals: &[i64]) -> MontyObject {
        w.call("set_of", &[ints(vals)], &[]).unwrap()
    }

    fn chunk0(w: &mut World, s: &MontyObject) -> MontyObject {
        let M::Tuple(pair) = w
            .call("set_chunk_at", &[s.clone(), M::Int(0)], &[])
            .unwrap()
        else {
            panic!("set_chunk_at must return a ( prefix, container ) pair");
        };
        pair[1].clone()
    }
}
