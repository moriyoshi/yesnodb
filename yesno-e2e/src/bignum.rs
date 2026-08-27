//! `bn_*`: arbitrary-precision integers, and the `OrdSet` boundary they live at.
//!
//! An `OrdSet` under an `IntLayout` is a series of unsigned big integers. These
//! verbs read one out, operate on it, and put it back — the shape
//! `yesno_core::bignum` is built around, so a scenario exercises the real
//! boundary rather than a convenience wrapper.
//!
//! # What these are for
//!
//! **Python's `int` is arbitrary-precision, so it is exactly this type.** That
//! makes it a free, exact, independently-implemented oracle, and a scenario can
//! write `assert bn_mul(a, b) == a * b` with no reference implementation of its
//! own. Same argument that puts the set scenarios here because Python's `set` is
//! the oracle.
//!
//! The **operational sequence**, which no Rust test covers: build a series,
//! store it in a real `Db`, checkpoint, **reopen**, read it back through a
//! `Snapshot`, do arithmetic, store the result, reopen again. A read on a live
//! `Db` is answered from the memtable and never reaches the store — the blind
//! spot that hid three bugs in `mx_*`.
//!
//! **Not the oracle comparison for the kernels.** `yesno-core`'s own
//! `tests/bignum_oracle.rs` does that better: randomized, boundary-biased, and
//! against `num-bigint`. Reimplementing Karatsuba in Python to compare against
//! another Python walk is what got `and_shape.py` moved out of `scenarios/`.
//!
//! **No `bn_store` / `bn_load`.** `bn_series_build` returns an ordinary set
//! handle, so `batch_store_set` and `snap_load_set` already carry it to and from
//! a database. Adding a `bn_`-prefixed pair would duplicate two shipped verbs
//! and give the family two more names to keep alive — `TESTING.md` records
//! thirty-four verbs orphaned when two scenario files moved out.
//!
//! # Every name a caller would reach for first is a Python builtin
//!
//! `pow`, `divmod`, `int`, `abs`, `min`, `max`, `len` — monty resolves builtins
//! **without ever asking the host**, so a verb named `pow` would silently compute
//! Python's `pow( a, e, m )`. That is *precisely the oracle these verbs are
//! compared against*, so the scenario would pass while testing nothing at all.
//! This is the sharpest instance of the collision rule in the harness, and the
//! `bn_` prefix is what makes it a non-issue.

use monty_types::{MontyException, MontyObject};
use yesno_core::bignum::{IntLayout, IntSink};

use crate::convert::{bignum_obj, int_obj, tuple, value_err, Args};
use crate::world::World;

pub const OWNS: &[&str] = &[
    // arithmetic
    "bn_add",
    "bn_sub",
    "bn_mul",
    "bn_divmod",
    "bn_pow_mod",
    "bn_shl",
    "bn_shr",
    "bn_truncate",
    // inspection
    "bn_bit_len",
    "bn_limb_len",
    "bn_hex",
    // the OrdSet boundary
    "bn_series_build",
    "bn_series_get",
    "bn_series_count",
];

/// The layout every series verb interprets its index under.
///
/// Spelled from two explicit arguments rather than carried on a handle, so a
/// scenario that reads at a different width than it wrote says so on the line
/// that does it — which is the `x mod 2^width` identity `read_int` exists to
/// expose.
fn layout_of(verb: &str, width: u64, stride: u64) -> Result<IntLayout, MontyException> {
    let width_bits = u32::try_from(width)
        .map_err(|_| value_err(format!("{verb}(): width_bits must fit a u32")))?;
    let l = IntLayout { width_bits, stride };
    l.check().map_err(|e| value_err(format!("{verb}(): {e}")))?;
    Ok(l)
}

impl World {
    pub(crate) fn call_bignum(
        &mut self,
        verb: &str,
        a: &Args,
    ) -> Result<MontyObject, MontyException> {
        a.no_kwargs()?;
        match verb {
            "bn_add" => {
                a.exact(2)?;
                Ok(bignum_obj(&a.bignum(0)?.add(&a.bignum(1)?)))
            }
            // `None` rather than an exception on underflow, because that is
            // what `BigUint::sub` returns and the point is that it is neither a
            // wrap nor a clamp. A scenario asserts `bn_sub(b, a) is None`.
            "bn_sub" => {
                a.exact(2)?;
                Ok(match a.bignum(0)?.sub(&a.bignum(1)?) {
                    Some(d) => bignum_obj(&d),
                    None => MontyObject::None,
                })
            }
            "bn_mul" => {
                a.exact(2)?;
                Ok(bignum_obj(&a.bignum(0)?.mul(&a.bignum(1)?)))
            }
            // `None` for a zero divisor, matching `divrem`. Not a raise: a
            // zero read out of a sparse series is ordinary data.
            "bn_divmod" => {
                a.exact(2)?;
                Ok(match a.bignum(0)?.divrem(&a.bignum(1)?) {
                    Some((q, r)) => tuple(vec![bignum_obj(&q), bignum_obj(&r)]),
                    None => MontyObject::None,
                })
            }
            "bn_pow_mod" => {
                a.exact(3)?;
                Ok(match a.bignum(0)?.pow_mod(&a.bignum(1)?, &a.bignum(2)?) {
                    Some(v) => bignum_obj(&v),
                    None => MontyObject::None,
                })
            }
            "bn_shl" => {
                a.exact(2)?;
                Ok(bignum_obj(&a.bignum(0)?.shl(a.u64(1)?)))
            }
            "bn_shr" => {
                a.exact(2)?;
                Ok(bignum_obj(&a.bignum(0)?.shr(a.u64(1)?)))
            }
            "bn_truncate" => {
                a.exact(2)?;
                Ok(bignum_obj(&a.bignum(0)?.truncate(a.u64(1)?)))
            }
            "bn_bit_len" => {
                a.exact(1)?;
                Ok(int_obj(a.bignum(0)?.bit_len()))
            }
            "bn_limb_len" => {
                a.exact(1)?;
                Ok(int_obj(a.bignum(0)?.limbs().len() as u64))
            }
            "bn_hex" => {
                a.exact(1)?;
                Ok(MontyObject::String(a.bignum(0)?.to_hex_string()))
            }

            // --- the OrdSet boundary ---
            // Returns an ordinary set handle, so the database verbs carry it.
            "bn_series_build" => {
                a.exact(3)?;
                let layout = layout_of(verb, a.u64(1)?, a.u64(2)?)?;
                let values = a.bignum_list(0)?;
                let mut sink = IntSink::new(layout);
                for (k, v) in values.iter().enumerate() {
                    sink.place(k as u64, v)
                        .map_err(|e| value_err(format!("{verb}(): index {k}: {e}")))?;
                }
                Ok(self.push_set(sink.build()))
            }
            "bn_series_get" => {
                a.exact(4)?;
                let set = self.set(a.handle(0)?, verb)?.clone();
                let layout = layout_of(verb, a.u64(2)?, a.u64(3)?)?;
                Ok(match set.read_int(a.u64(1)?, &layout) {
                    Some(v) => bignum_obj(&v),
                    // Not addressable at all, which is a statement about the
                    // layout rather than an empty answer.
                    None => MontyObject::None,
                })
            }
            "bn_series_count" => {
                a.exact(3)?;
                let set = self.set(a.handle(0)?, verb)?.clone();
                let layout = layout_of(verb, a.u64(1)?, a.u64(2)?)?;
                Ok(int_obj(set.int_count(&layout)))
            }
            _ => Err(value_err(format!("{verb}() is not a bignum verb"))),
        }
    }
}
