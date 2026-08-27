//! The lazy half: expression trees, the planner, and raw chunk cursors.
//!
//! # What the `q_*` verbs gained
//!
//! They could build an expression and take its cardinality, and nothing else.
//! That is enough to check the M1 identities and not enough to migrate a single
//! measurement fixture — the eight that were `yesno-core/examples/*.rs` until
//! 2026-08-26 — all of which turn on three things the
//! surface had no word for:
//!
//! * **The planner as an object of study.** `e2e/scenarios/rule_economics.py` asks
//!   whether a rewrite pays for itself, which means holding the planned and the
//!   unplanned form of one expression *at the same time* and running both.
//!   `q_plan` returns a second handle rather than mutating
//!   the first, so a scenario can.
//! * **Structural introspection.** `q_repr` gives the `Debug` string, which is
//!   what the Rust original compared to decide whether a rewrite fired — a fine
//!   tripwire and a poor assertion. `q_kind` / `q_arity` / `q_child` let a
//!   scenario say *what* the planner produced, so "`Xor(disjoint)` lowered to a
//!   concatenation" is a claim a test can make instead of "the string changed".
//! * **The cursor underneath the operators.** `scenarios/and_shape.py` is four
//!   hand-written k-way intersections, and every one of them is built from
//!   `peek_prefix`, `seek` and `next_chunk`. Those are the `ChunkStream`
//!   contract; a scenario that cannot call them cannot express the algorithms
//!   the fixture exists to compare.
//!
//! # `st_open` does not plan
//!
//! [`Expr::open`] plans and then lowers; `Expr::open_planned` only lowers.
//! `st_open` is the second, deliberately, because the fixtures need the
//! unplanned form as a *control* — the whole measurement in `rule_economics` is
//! planned against unplanned. Composing is explicit: `st_open( q_plan( e ) )`
//! is the planned column, `st_open( e )` is the raw one. A verb that quietly
//! planned would make the two columns identical and the fixture vacuous.
//!
//! # Why `q_time` runs its loop in the host
//!
//! Several fixtures report nanoseconds — `rule_economics` measures a planner
//! pass at 88 ns against an execution at 49 ns — and a repetition loop written
//! in Python would be timing monty, not yesno. One host call per iteration
//! costs more than the entire quantity under measurement. So the repetition
//! lives on this side of the boundary: `q_time` takes an iteration count and a
//! named terminal, warms up, and reports nanoseconds per iteration.
//!
//! It bounds itself by wall clock as well as by count, and **reports the
//! iterations it actually ran**, because a scenario asking for 200 000
//! repetitions of a 5 ms query would otherwise sit inside one host call for
//! twenty minutes — past the runner's time limit, which only bounds the VM.

use std::time::{Duration, Instant};

use monty_types::{MontyException, MontyObject};
use yesno_core::{ChunkStream, ChunkStreamExt, Expr, OrdSet};

use crate::convert::{db_err, dict, int_obj, opt_int_obj, tuple, type_err, value_err, Args};
use crate::world::{stale_handle, HandleKind, World};

pub const OWNS: &[&str] = &[
    // construction
    "q_set",
    "q_not_in",
    // planning and introspection
    "q_plan",
    "q_repr",
    "q_kind",
    "q_arity",
    "q_child",
    "q_leaf_set",
    "q_bounds",
    // terminals
    "q_collect_set",
    "q_time",
    // cursors
    "st_open",
    "st_peek_prefix",
    "st_seek",
    "st_next_chunk",
    "st_next_cardinality",
    "st_cardinality",
    "st_release",
];

/// How long one `q_time` call may spend before it stops early.
///
/// The runner's [`crate::DEFAULT_TIME_LIMIT`] is enforced by monty's resource
/// tracker, which sees VM time — a host call that loops is invisible to it. So
/// the timing verb carries its own bound.
pub const TIME_BUDGET: Duration = Duration::from_secs(5);

/// Warm-up iterations before the clock starts, matching the fixtures being
/// migrated ( they use three to five ). Doubles as the calibration run that
/// sizes the deadline-check batch.
const WARMUP: u32 = 5;

/// How coarsely the wall-clock deadline may be enforced. One clock read per
/// this much work is negligible; one per iteration is not.
const CLOCK_GRANULARITY: Duration = Duration::from_millis(20);

/// Ceiling on the batch, so a nanosecond-scale operation still checks the
/// deadline occasionally rather than running `iters` to completion.
const MAX_BATCH: usize = 1 << 16;

impl World {
    pub(crate) fn call_lazy(
        &mut self,
        verb: &str,
        a: &Args<'_>,
    ) -> Result<MontyObject, MontyException> {
        match verb {
            "q_set" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let s = self.set(a.handle(0)?, verb)?.clone();
                Ok(self.push_expr(Expr::Set(s)))
            }
            // Complement within `[lo, hi)`, the half-open convention `q_range`
            // already uses. A distinct variant rather than sugar for
            // `AndNot( Range, x )`: the desugared form counts by stepping the
            // range, which turns a complement over a wide window into a hang.
            "q_not_in" => {
                a.exact(3)?;
                a.no_kwargs()?;
                let (h, lo, hi) = (a.handle(0)?, a.u64(1)?, a.u64(2)?);
                let e = self.expr(h, verb)?;
                Ok(self.push_expr(e.not_in(lo, hi)))
            }

            "q_plan" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let planned = self.expr(a.handle(0)?, verb)?.plan();
                Ok(self.push_expr(planned))
            }
            "q_repr" => {
                a.exact(1)?;
                a.no_kwargs()?;
                Ok(MontyObject::String(format!(
                    "{:?}",
                    self.expr(a.handle(0)?, verb)?
                )))
            }
            "q_kind" => {
                a.exact(1)?;
                a.no_kwargs()?;
                Ok(MontyObject::String(kind(&self.expr(a.handle(0)?, verb)?)))
            }
            "q_arity" => {
                a.exact(1)?;
                a.no_kwargs()?;
                Ok(int_obj(arity(&self.expr(a.handle(0)?, verb)?) as u64))
            }
            // Out of range raises rather than returning `None`: a walk that
            // indexed past the arity would otherwise treat a bug as a leaf.
            "q_child" => {
                a.exact(2)?;
                a.no_kwargs()?;
                let (h, i) = (a.handle(0)?, a.usize_at(1)?);
                let e = self.expr(h, verb)?;
                let n = arity(&e);
                if i >= n {
                    return Err(value_err(format!(
                        "{verb}(): a {} node has {n} child(ren), so index {i} does not exist",
                        kind(&e)
                    )));
                }
                let child = match (&e, i) {
                    (
                        Expr::And(x, _) | Expr::Or(x, _) | Expr::Xor(x, _) | Expr::AndNot(x, _),
                        0,
                    )
                    | (
                        Expr::And(_, x) | Expr::Or(_, x) | Expr::Xor(_, x) | Expr::AndNot(_, x),
                        1,
                    )
                    | (Expr::Not(x, _, _), 0) => (**x).clone(),
                    _ => unreachable!("arity() and this match must agree"),
                };
                Ok(self.push_expr(child))
            }
            "q_leaf_set" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let e = self.expr(a.handle(0)?, verb)?;
                let Expr::Set(s) = e else {
                    return Err(value_err(format!(
                        "{verb}(): this is a {} node, not a set leaf",
                        kind(&e)
                    )));
                };
                Ok(self.push_set_arc(s))
            }
            // Half-open `[lo, hi)` for both variants that carry a window.
            "q_bounds" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let e = self.expr(a.handle(0)?, verb)?;
                match e {
                    Expr::Range(lo, hi) | Expr::Not(_, lo, hi) => {
                        Ok(tuple(vec![int_obj(lo), int_obj(hi)]))
                    }
                    other => Err(value_err(format!(
                        "{verb}(): a {} node carries no [lo, hi) window",
                        kind(&other)
                    ))),
                }
            }

            // The materializing terminal, as a set handle rather than a list.
            // `q_collect` already returns a list; handing back a set as well is
            // what lets a scenario compare a lazy result against an eager one
            // without moving ten million integers through the VM.
            "q_collect_set" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let e = self.expr(a.handle(0)?, verb)?;
                let set: OrdSet = e.collect_set().map_err(|err| db_err(verb, err))?;
                Ok(self.push_set(set))
            }
            "q_time" => self.q_time(a),

            "st_open" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let e = self.expr(a.handle(0)?, verb)?;
                self.streams.push(Some(e.open_planned()));
                let idx = self.streams.len() - 1;
                Ok(self.mint(HandleKind::Stream, idx))
            }
            "st_peek_prefix" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let s = self.stream(a.handle(0)?, verb)?;
                Ok(opt_int_obj(s.peek_prefix().map_err(|e| db_err(verb, e))?))
            }
            "st_seek" => {
                a.exact(2)?;
                a.no_kwargs()?;
                let (h, p) = (a.handle(0)?, a.u64(1)?);
                self.stream(h, verb)?.seek(p).map_err(|e| db_err(verb, e))?;
                Ok(MontyObject::None)
            }
            "st_next_chunk" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let got = self
                    .stream(a.handle(0)?, verb)?
                    .next_chunk()
                    .map_err(|e| db_err(verb, e))?;
                let Some((p, c)) = got else {
                    return Ok(MontyObject::None);
                };
                let ch = self.push_container(c);
                Ok(tuple(vec![int_obj(p), ch]))
            }
            // The counting advance. Its whole reason to exist is that it does
            // *not* hand back a payload, so a scenario measuring which question
            // an operator asks has to be able to ask both.
            "st_next_cardinality" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let got = self
                    .stream(a.handle(0)?, verb)?
                    .next_cardinality()
                    .map_err(|e| db_err(verb, e))?;
                Ok(match got {
                    None => MontyObject::None,
                    Some((p, n)) => tuple(vec![int_obj(p), int_obj(n)]),
                })
            }
            "st_cardinality" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let n = self
                    .stream(a.handle(0)?, verb)?
                    .cardinality_dyn()
                    .map_err(|e| db_err(verb, e))?;
                Ok(int_obj(n))
            }
            "st_release" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let h = a.handle(0)?;
                let i = self.slot(h, HandleKind::Stream, verb)?;
                if self.streams[i].take().is_none() {
                    return Err(stale_handle(verb, "stream", h));
                }
                Ok(MontyObject::None)
            }

            _ => Err(type_err(format!("{verb}() is not a harness verb"))),
        }
    }

    fn q_time(&mut self, a: &Args<'_>) -> Result<MontyObject, MontyException> {
        const VERB: &str = "q_time";
        a.exact(3)?;
        a.no_kwargs()?;
        let (h, iters) = (a.handle(0)?, a.usize_at(1)?);
        let terminal = a.str_at(2)?.to_owned();
        if iters == 0 {
            return Err(value_err(format!("{VERB}(): iters must be at least 1")));
        }
        let e = self.expr(h, VERB)?;

        // Resolved **before** the loop, not inside it. Matching the terminal
        // name per iteration added a flat ~29 ns to every measurement — three
        // times what `And(disjoint)`'s planned execution costs — and it showed
        // up as a constant offset against the Rust fixture on every small row
        // while the large ones agreed. A microbenchmark harness that charges
        // for its own dispatch is measuring itself.
        let terminal = Terminal::parse(&terminal)?;

        let once = |e: &Expr| -> Result<(), MontyException> {
            match terminal {
                Terminal::Plan => {
                    std::hint::black_box(e.plan());
                }
                Terminal::Cardinality => {
                    std::hint::black_box(
                        e.open_planned()
                            .cardinality_dyn()
                            .map_err(|err| db_err(VERB, err))?,
                    );
                }
                // `open_planned`, like every other terminal here — `Expr::collect_set`
                // plans first, and a `collect` column that silently planned
                // could not be the unplanned control.
                Terminal::Collect => {
                    std::hint::black_box(
                        e.open_planned()
                            .collect_set()
                            .map_err(|err| db_err(VERB, err))?
                            .len(),
                    );
                }
                // `drain` and `collect` differ in what they keep, not in what
                // they touch; `drain` is here under its own name because
                // `scenarios/and_shape.py` names it and explains why it is the
                // fair terminal against a hand-written walk: the tree operators
                // override `cardinality_dyn` and a hand-written k-way walk has
                // no equivalent, so counting would compare different amounts of
                // work.
                Terminal::Drain => {
                    let mut s = e.open_planned();
                    let mut n = 0u64;
                    while let Some((_, c)) = s.next_chunk().map_err(|err| db_err(VERB, err))? {
                        n += u64::from(c.len());
                    }
                    std::hint::black_box(n);
                }
            }
            Ok(())
        };

        // The warm-up is also the calibration, and it has to be, because the
        // deadline check is not free. Reading the clock once per iteration cost
        // a flat ~25 ns — which is *ten times* what `And(disjoint)`'s planned
        // execution costs, and showed up as a constant offset on every small
        // row against the Rust fixture while the large rows agreed. So the
        // clock is read once per batch instead, and the batch is sized from
        // what one iteration turned out to cost.
        let warm = Instant::now();
        for _ in 0..WARMUP {
            once(&e)?;
        }
        let per_iter = warm.elapsed() / WARMUP;
        let batch = if per_iter.is_zero() {
            MAX_BATCH
        } else {
            let n = (CLOCK_GRANULARITY.as_nanos() / per_iter.as_nanos().max(1)) as usize;
            n.clamp(1, MAX_BATCH)
        };

        let started = Instant::now();
        let mut ran = 0usize;
        'outer: while ran < iters {
            for _ in 0..batch.min(iters - ran) {
                once(&e)?;
                ran += 1;
            }
            if started.elapsed() >= TIME_BUDGET {
                break 'outer;
            }
        }
        let elapsed = started.elapsed();
        Ok(dict(vec![
            (
                "ns",
                MontyObject::Float(elapsed.as_secs_f64() * 1e9 / ran as f64),
            ),
            ("iters", int_obj(ran as u64)),
            ("truncated", MontyObject::Bool(ran < iters)),
        ]))
    }

    fn stream(
        &mut self,
        h: usize,
        verb: &str,
    ) -> Result<&mut yesno_core::stream::BoxedStream, MontyException> {
        let i = self.slot(h, HandleKind::Stream, verb)?;
        self.streams[i]
            .as_mut()
            .ok_or_else(|| stale_handle(verb, "stream", h))
    }
}

/// What `q_time` repeats. Resolved from its name once, so the timed loop
/// dispatches on a `Copy` enum rather than on a string.
#[derive(Clone, Copy)]
enum Terminal {
    Plan,
    Cardinality,
    Collect,
    Drain,
}

impl Terminal {
    fn parse(name: &str) -> Result<Terminal, MontyException> {
        match name {
            "plan" => Ok(Terminal::Plan),
            "cardinality" => Ok(Terminal::Cardinality),
            "collect" => Ok(Terminal::Collect),
            "drain" => Ok(Terminal::Drain),
            other => Err(value_err(format!(
                "q_time(): '{other}' is not a terminal; use plan, cardinality, collect or drain"
            ))),
        }
    }
}

/// The variant name, lower-cased, as `q_kind` reports it.
///
/// `Expr` is `#[non_exhaustive]`, so the catch-all is not dead code — a new
/// variant must show up as an unknown kind rather than being silently folded
/// into one of these.
fn kind(e: &Expr) -> String {
    match e {
        Expr::Set(_) => "set",
        Expr::Range(_, _) => "range",
        Expr::Empty => "empty",
        Expr::And(_, _) => "and",
        Expr::Or(_, _) => "or",
        Expr::Xor(_, _) => "xor",
        Expr::AndNot(_, _) => "andnot",
        Expr::Not(_, _, _) => "not",
        _ => "unknown",
    }
    .to_owned()
}

fn arity(e: &Expr) -> usize {
    match e {
        Expr::And(_, _) | Expr::Or(_, _) | Expr::Xor(_, _) | Expr::AndNot(_, _) => 2,
        Expr::Not(_, _, _) => 1,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use monty_types::MontyObject as M;

    fn w() -> World {
        World::temporary().unwrap()
    }

    fn set_of(w: &mut World, vals: &[i64]) -> MontyObject {
        let list = M::List(vals.iter().map(|v| M::Int(*v)).collect());
        w.call("set_of", &[list], &[]).unwrap()
    }

    fn leaf(w: &mut World, vals: &[i64]) -> MontyObject {
        let s = set_of(w, vals);
        w.call("q_set", &[s], &[]).unwrap()
    }

    /// The planner must be inspectable structurally, not only as a string.
    /// `And` of two span-disjoint operands is the rewrite `rule_economics`
    /// exists to price, and a scenario has to be able to name the result.
    #[test]
    fn planning_a_disjoint_and_yields_an_empty_node() {
        let mut w = w();
        let a = leaf(&mut w, &[1, 2, 3]);
        let b = leaf(&mut w, &[1_000_000, 1_000_001]);
        let e = w.call("q_and", &[a, b], &[]).unwrap();
        assert_eq!(
            w.call("q_kind", std::slice::from_ref(&e), &[]).unwrap(),
            M::String("and".into())
        );
        let p = w.call("q_plan", std::slice::from_ref(&e), &[]).unwrap();
        assert_eq!(
            w.call("q_kind", std::slice::from_ref(&p), &[]).unwrap(),
            M::String("empty".into()),
            "a span-disjoint And must plan to Empty"
        );
        // And the unplanned form must still be usable — the fixture needs both.
        assert_eq!(w.call("q_cardinality", &[e], &[]).unwrap(), M::Int(0));
    }

    /// Indexing past a node's arity is a scenario bug and must say so.
    #[test]
    fn a_child_index_past_the_arity_is_refused() {
        let mut w = w();
        let a = leaf(&mut w, &[1]);
        let err = w.call("q_child", &[a, M::Int(0)], &[]).unwrap_err();
        assert!(err.summary().contains("0 child"), "{}", err.summary());
    }

    /// The cursor verbs must expose the `ChunkStream` contract as it is:
    /// `peek_prefix` reports without consuming, `next_chunk` consumes.
    #[test]
    fn peeking_does_not_consume_and_next_chunk_does() {
        let mut w = w();
        let e = leaf(&mut w, &[1, 65_537]);
        let st = w.call("st_open", &[e], &[]).unwrap();
        assert_eq!(
            w.call("st_peek_prefix", std::slice::from_ref(&st), &[])
                .unwrap(),
            M::Int(0)
        );
        assert_eq!(
            w.call("st_peek_prefix", std::slice::from_ref(&st), &[])
                .unwrap(),
            M::Int(0),
            "peek_prefix must not advance"
        );
        let M::Tuple(first) = w
            .call("st_next_chunk", std::slice::from_ref(&st), &[])
            .unwrap()
        else {
            panic!("expected a ( prefix, container ) pair")
        };
        assert_eq!(first[0], M::Int(0));
        assert_eq!(
            w.call("st_peek_prefix", std::slice::from_ref(&st), &[])
                .unwrap(),
            M::Int(1),
            "next_chunk must advance"
        );
    }

    /// Seeking past everything must end the stream rather than wrapping.
    #[test]
    fn seeking_past_the_end_exhausts_the_stream() {
        let mut w = w();
        let e = leaf(&mut w, &[1, 2, 3]);
        let st = w.call("st_open", &[e], &[]).unwrap();
        w.call("st_seek", &[st.clone(), M::Int(1 << 20)], &[])
            .unwrap();
        assert_eq!(
            w.call("st_next_chunk", std::slice::from_ref(&st), &[])
                .unwrap(),
            M::None
        );
    }

    /// `st_next_cardinality` must agree with `st_next_chunk` on the count —
    /// they are two implementations of one advance, which is exactly the shape
    /// this project keeps getting wrong.
    #[test]
    fn the_counting_advance_agrees_with_the_materializing_one() {
        let mut w = w();
        let vals: Vec<i64> = (0..300).map(|i| i * 7).collect();
        let a = leaf(&mut w, &vals);
        let b = leaf(&mut w, &vals);
        let sa = w.call("st_open", &[a], &[]).unwrap();
        let sb = w.call("st_open", &[b], &[]).unwrap();
        loop {
            let x = w
                .call("st_next_chunk", std::slice::from_ref(&sa), &[])
                .unwrap();
            let y = w
                .call("st_next_cardinality", std::slice::from_ref(&sb), &[])
                .unwrap();
            match (x, y) {
                (M::None, M::None) => break,
                (M::Tuple(x), M::Tuple(y)) => {
                    assert_eq!(x[0], y[0], "prefixes must match");
                    let M::Int(h) = x[1] else {
                        panic!("container handle")
                    };
                    let len = w.call("ct_len", &[M::Int(h)], &[]).unwrap();
                    assert_eq!(len, y[1], "cardinalities must match");
                }
                (x, y) => panic!("streams disagreed on termination: {x:?} vs {y:?}"),
            }
        }
    }

    /// A released stream must raise on next use rather than resurrecting.
    #[test]
    fn a_released_stream_is_refused() {
        let mut w = w();
        let e = leaf(&mut w, &[1]);
        let st = w.call("st_open", &[e], &[]).unwrap();
        w.call("st_release", std::slice::from_ref(&st), &[])
            .unwrap();
        let err = w
            .call("st_peek_prefix", std::slice::from_ref(&st), &[])
            .unwrap_err();
        assert!(
            err.summary().contains("already been closed or released"),
            "{}",
            err.summary()
        );
    }

    /// The timing verb must report the iterations it ran, not the ones it was
    /// asked for — otherwise a truncated run reads as a complete one.
    #[test]
    fn timing_reports_what_it_actually_ran() {
        let mut w = w();
        let e = leaf(&mut w, &[1, 2, 3]);
        let out = w
            .call(
                "q_time",
                &[e, M::Int(50), M::String("cardinality".into())],
                &[],
            )
            .unwrap();
        let M::Dict(pairs) = out else {
            panic!("q_time must return a dict")
        };
        let got: Vec<_> = (&pairs).into_iter().collect();
        assert!(
            got.iter()
                .any(|(k, v)| matches!(k, M::String(s) if s == "iters") && *v == M::Int(50)),
            "{got:?}"
        );
    }

    #[test]
    fn an_unknown_terminal_is_refused() {
        let mut w = w();
        let e = leaf(&mut w, &[1]);
        let err = w
            .call(
                "q_time",
                &[e, M::Int(1), M::String("free lunch".into())],
                &[],
            )
            .unwrap_err();
        assert!(
            err.summary().contains("not a terminal"),
            "{}",
            err.summary()
        );
    }
}
