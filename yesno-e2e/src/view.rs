//! `vw_*`: the packed-view verbs.
//!
//! A [`View`] says how several `OrdSet`s share one ordinal space. These verbs
//! build a packed set, pull a constituent back out, fold across them, and expand
//! the other way — the shape `yesno_core::view` is built around.
//!
//! # What these are for
//!
//! **Not** the oracle comparison. `yesno-core`'s own tests do that better:
//! they diff every specialised arm against its generic path over boundary-biased
//! sources, and assert the algebraic laws ( De Morgan, the one-sided
//! intersection, the Galois adjunction ) that a Python walk could only restate.
//!
//! The **operational sequence**, which no Rust test covers: pack constituents,
//! store the packed set in a real `Db`, checkpoint, **reopen**, load it back
//! through a `Snapshot`, and only then select and fold. A read on a live `Db` is
//! answered from the memtable and never reaches the store — the blind spot that
//! hid three bugs when `matrix/` was wired up the same way.
//!
//! **No verb name may collide with a Python builtin**, which is why every one
//! is prefixed. `vw_pack` rather than `vw_build` keeps it distinct from the
//! `sb_build` set-builder family, and `vw_fold` takes the reduction by name so a
//! scenario reads `vw_fold( v, p, "all" )` rather than carrying a magic integer.

use monty_types::{MontyException, MontyObject};
use yesno_core::view::{Reduce, View, ViewSink};

use crate::convert::{int_obj, value_err, Args};
use crate::world::{HandleKind, World};

pub const OWNS: &[&str] = &[
    // descriptors
    "vw_interleaved",
    "vw_blocked",
    "vw_sets",
    // the packing boundary
    "vw_pack",
    "vw_select",
    "vw_expand",
    // querying one constituent without unpacking it
    "vw_contains",
    "vw_cardinality",
    // across constituents
    "vw_fold",
];

impl World {
    pub(crate) fn call_view(
        &mut self,
        verb: &str,
        a: &Args,
    ) -> Result<MontyObject, MontyException> {
        match verb {
            "vw_interleaved" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let v = View::interleaved(a.u64(0)? as u32);
                v.check().map_err(|e| value_err(format!("{verb}(): {e}")))?;
                Ok(self.push_view(v))
            }
            "vw_blocked" => {
                a.exact(2)?;
                a.no_kwargs()?;
                let v = View::blocked(a.u64(0)? as u32, a.u64(1)?);
                v.check().map_err(|e| value_err(format!("{verb}(): {e}")))?;
                Ok(self.push_view(v))
            }
            "vw_sets" => {
                a.exact(1)?;
                a.no_kwargs()?;
                Ok(int_obj(self.view(a.handle(0)?, verb)?.sets() as u64))
            }

            // Packing is where a constituent's capacity is enforced, so an
            // out-of-range ordinal surfaces here as the caller's error rather
            // than as a silently short set later.
            "vw_pack" => {
                a.exact(2)?;
                a.no_kwargs()?;
                let v = *self.view(a.handle(0)?, verb)?;
                let parts = a.handle_list(1)?;
                let mut sink = ViewSink::new(v);
                for (i, h) in parts.iter().enumerate() {
                    let s = self.set(*h, verb)?.clone();
                    sink.place(i as u32, &s)
                        .map_err(|e| value_err(format!("{verb}(): constituent {i}: {e}")))?;
                }
                Ok(self.push_set(sink.build()))
            }
            "vw_select" => {
                a.exact(3)?;
                a.no_kwargs()?;
                let v = *self.view(a.handle(0)?, verb)?;
                let packed = self.set(a.handle(1)?, verb)?.clone();
                Ok(self.push_set(packed.view_select(&v, a.u64(2)? as u32)))
            }
            "vw_expand" => {
                a.exact(2)?;
                a.no_kwargs()?;
                let v = *self.view(a.handle(0)?, verb)?;
                let s = self.set(a.handle(1)?, verb)?.clone();
                Ok(self.push_set(s.view_expand(&v)))
            }

            "vw_contains" => {
                a.exact(4)?;
                a.no_kwargs()?;
                let v = *self.view(a.handle(0)?, verb)?;
                let packed = self.set(a.handle(1)?, verb)?;
                let (set, x) = (a.u64(2)? as u32, a.u64(3)?);
                Ok(MontyObject::Bool(packed.view_contains(&v, set, x)))
            }
            "vw_cardinality" => {
                a.exact(3)?;
                a.no_kwargs()?;
                let v = *self.view(a.handle(0)?, verb)?;
                let packed = self.set(a.handle(1)?, verb)?;
                Ok(int_obj(packed.view_cardinality(&v, a.u64(2)? as u32)))
            }

            // By name rather than by integer: a scenario asserting on
            // `vw_fold( v, p, "all" )` says what it means, and an unknown name is
            // a `ValueError` rather than a silently different reduction.
            "vw_fold" => {
                a.exact(3)?;
                a.no_kwargs()?;
                let v = *self.view(a.handle(0)?, verb)?;
                let packed = self.set(a.handle(1)?, verb)?.clone();
                let r = match a.str_at(2)? {
                    "any" => Reduce::Any,
                    "all" => Reduce::All,
                    "parity" => Reduce::Parity,
                    other => {
                        return Err(value_err(format!(
                            "{verb}(): unknown reduction {other:?}; expected \"any\", \"all\" or \"parity\""
                        )))
                    }
                };
                Ok(self.push_set(packed.view_fold(&v, r)))
            }

            _ => unreachable!("OWNS and this match must agree; {verb} is missing"),
        }
    }

    pub(crate) fn view(&self, h: usize, verb: &str) -> Result<&View, MontyException> {
        let i = self.slot(h, HandleKind::View, verb)?;
        Ok(&self.views[i])
    }

    pub(crate) fn push_view(&mut self, v: View) -> MontyObject {
        self.views.push(v);
        let idx = self.views.len() - 1;
        self.mint(HandleKind::View, idx)
    }
}
