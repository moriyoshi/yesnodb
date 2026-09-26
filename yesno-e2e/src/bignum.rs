//! `bn_*`: arbitrary-precision integers, and the `OrdSet` boundary they live at.
//!
//! An `OrdSet` **is** an unsigned big integer: ordinal `j` carries the `2^j`
//! term. These verbs move one across that boundary, operate on it, and put it
//! back — the shape
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
use yesno_core::bignum::{BigInt as SignedBig, BigUint};
use yesno_core::OrdSet;

use crate::convert::{bignum_obj, int_obj, signed_obj, tuple, value_err, Args};
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
    "bn_saturate",
    // Signed. A separate family rather than widening `bn_*`, because
    // `Args::bignum` refuses negatives on purpose -- that is what keeps
    // `bn_sub`'s absent answer testable from a scenario.
    "bi_add",
    "bi_sub",
    "bi_mul",
    "bi_divmod",
    "bi_div_euclid",
    "bi_neg",
    "bi_abs",
    "bi_cmp",
    "bi_truncate",
    "bi_saturate",
    "bi_of_set",
    // inspection
    "bn_bit_len",
    "bn_limb_len",
    "bn_hex",
    // the OrdSet boundary
    "bn_to_set",
    "bn_of_set",
];

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
            "bn_saturate" => {
                a.exact(2)?;
                Ok(bignum_obj(&a.bignum(0)?.saturate(a.u64(1)?)))
            }

            // --- signed ---
            "bi_add" => {
                a.exact(2)?;
                Ok(signed_obj(&a.signed(0)?.add(&a.signed(1)?)))
            }
            "bi_sub" => {
                a.exact(2)?;
                Ok(signed_obj(&a.signed(0)?.sub(&a.signed(1)?)))
            }
            "bi_mul" => {
                a.exact(2)?;
                Ok(signed_obj(&a.signed(0)?.mul(&a.signed(1)?)))
            }
            // Truncating toward zero, so the remainder carries the sign of the
            // dividend. Python's own `divmod` is *floored*, which is why a
            // scenario has to spell the oracle out rather than compare to it.
            "bi_divmod" => {
                a.exact(2)?;
                Ok(match a.signed(0)?.divrem(&a.signed(1)?) {
                    Some((q, r)) => tuple(vec![signed_obj(&q), signed_obj(&r)]),
                    None => MontyObject::None,
                })
            }
            // The non-negative remainder, which *is* Python's `a % abs( b )`.
            "bi_div_euclid" => {
                a.exact(2)?;
                Ok(match a.signed(0)?.div_euclid_rem(&a.signed(1)?) {
                    Some((q, r)) => tuple(vec![signed_obj(&q), signed_obj(&r)]),
                    None => MontyObject::None,
                })
            }
            "bi_neg" => {
                a.exact(1)?;
                Ok(signed_obj(&a.signed(0)?.neg()))
            }
            "bi_abs" => {
                a.exact(1)?;
                Ok(signed_obj(&a.signed(0)?.abs()))
            }
            // -1, 0 or 1, so a scenario can compare against Python's own
            // ordering without a second spelling of it.
            "bi_cmp" => {
                a.exact(2)?;
                let ordering = a.signed(0)?.cmp(&a.signed(1)?);
                Ok(MontyObject::Int(match ordering {
                    std::cmp::Ordering::Less => -1,
                    std::cmp::Ordering::Equal => 0,
                    std::cmp::Ordering::Greater => 1,
                }))
            }
            // The wrap and the clamp, into the *same* two's-complement field.
            "bi_truncate" => {
                a.exact(2)?;
                Ok(signed_obj(&a.signed(0)?.truncate(a.u64(1)?)))
            }
            "bi_saturate" => {
                a.exact(2)?;
                Ok(signed_obj(&a.signed(0)?.saturate(a.u64(1)?)))
            }
            // The same stored bits as `bn_of_set`, read as two's complement.
            "bi_of_set" => {
                a.exact(2)?;
                let set = self.set(a.handle(0)?, verb)?.clone();
                let width = a.u64(1)?;
                if width == 0 {
                    return Err(value_err(format!("{verb}(): width must not be zero")));
                }
                let raw = set.read_int(width);
                let value = if raw.bit(width - 1) {
                    let modulus = BigUint::one().shl(width);
                    SignedBig::from_magnitude(
                        true,
                        modulus.sub(&raw).expect("a read value is below 2^width"),
                    )
                } else {
                    SignedBig::from_uint(raw)
                };
                Ok(signed_obj(&value))
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
            // One value in, one set out. Returns an ordinary set handle, so
            // the database verbs carry it.
            "bn_to_set" => {
                a.exact(1)?;
                let set = OrdSet::from_int(&a.bignum(0)?)
                    .map_err(|e| value_err(format!("{verb}(): {e}")))?;
                Ok(self.push_set(set))
            }
            // The width is explicit on the line that reads, rather than carried
            // on the handle, so a scenario reading at a narrower width than it
            // wrote says so where it does it -- which is the `x mod 2^width`
            // identity this boundary exists to expose.
            "bn_of_set" => {
                a.exact(2)?;
                let set = self.set(a.handle(0)?, verb)?.clone();
                Ok(bignum_obj(&set.read_int(a.u64(1)?)))
            }
            _ => Err(value_err(format!("{verb}() is not a bignum verb"))),
        }
    }
}
