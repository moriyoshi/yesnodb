//! `MontyObject` <-> yesno value conversion, and argument checking for verbs.
//!
//! One asymmetry drives this whole module. yesno addresses keys and ordinals as
//! `u64`; monty's `int` is an `i64` with a `BigInt` arm above it. So every
//! ordinal above `i64::MAX` — half the address space, and precisely the half a
//! 48-bit-prefix design is most likely to get wrong — reaches the host as
//! `MontyObject::BigInt`, and has to be **accepted** rather than rejected as
//! "not an integer". [`int_obj`] is the mirror image: an ordinal that does not
//! fit an `i64` must go back as a `BigInt`, or a scenario comparing
//! `snap_load(...)` against a Python list would fail on a value the database
//! stored perfectly well.
//!
//! Every failure here is a Python exception rather than a Rust `Err`, so a
//! mistyped scenario argument surfaces as a traceback at the calling line
//! instead of a harness-level panic with no source position.

use monty_types::{ExcType, MontyException, MontyObject};
use num_bigint::BigInt;
use yesno_core::bignum::BigUint;

/// A `ValueError` — the argument had the right type but an impossible value.
pub fn value_err(msg: impl std::fmt::Display) -> MontyException {
    MontyException::new(ExcType::ValueError, Some(msg.to_string()))
}

/// A `TypeError` — the argument was the wrong shape entirely.
pub fn type_err(msg: impl std::fmt::Display) -> MontyException {
    MontyException::new(ExcType::TypeError, Some(msg.to_string()))
}

/// A yesno error surfaced into Python. Kept distinct from `value_err` so a
/// scenario can tell "the harness rejected my argument" from "the database
/// refused the operation".
pub fn db_err(verb: &str, e: impl std::fmt::Display) -> MontyException {
    MontyException::new(
        ExcType::RuntimeError,
        Some(format!("{verb}(): yesno returned an error: {e}")),
    )
}

/// `u64` -> Python int, widening to `BigInt` above `i64::MAX`.
///
/// The widening is not a nicety. A scenario that stores `u64::MAX - 1` and
/// reads it back must get the same number, and truncating to `i64` would hand
/// it a negative one instead.
#[must_use]
pub fn int_obj(v: u64) -> MontyObject {
    match i64::try_from(v) {
        Ok(i) => MontyObject::Int(i),
        Err(_) => MontyObject::BigInt(BigInt::from(v)),
    }
}

/// `BigUint` -> Python int, at any magnitude.
///
/// The mirror of [`Args::bignum`], and the reason a scenario can write
/// `assert bn_mul(a, b) == a * b` with Python's own `int` as the oracle.
#[must_use]
pub fn bignum_obj(v: &BigUint) -> MontyObject {
    let b = BigInt::from_bytes_le(num_bigint::Sign::Plus, &v.to_le_bytes());
    match i64::try_from(b.clone()) {
        Ok(i) => MontyObject::Int(i),
        Err(_) => MontyObject::BigInt(b),
    }
}

#[must_use]
pub fn opt_int_obj(v: Option<u64>) -> MontyObject {
    v.map_or(MontyObject::None, int_obj)
}

/// A count or index -> Python int.
///
/// Distinct from [`int_obj`] only in taking a `usize`; it goes through the same
/// widening, because a `usize` count on a 64-bit host can in principle exceed
/// `i64::MAX` and truncating it would report a negative length.
#[must_use]
pub fn whole_obj(v: usize) -> MontyObject {
    int_obj(v as u64)
}

/// A handle, as the small non-negative int the harness mints.
///
/// Separate from [`int_obj`] on purpose: a handle is an index into one of the
/// world's tables and can never need the `BigInt` arm, so widening it would
/// only hide a table that had grown to 2^63 entries.
#[must_use]
pub fn handle_obj(h: usize) -> MontyObject {
    MontyObject::Int(h as i64)
}

/// `Option<handle>` -> Python handle or `None`.
#[must_use]
pub fn opt_handle_obj(h: Option<usize>) -> MontyObject {
    h.map_or(MontyObject::None, handle_obj)
}

/// Build a Python tuple. Used for the `( prefix, payload )` pairs the chunk
/// cursors yield, so a scenario can unpack them the way Python expects.
#[must_use]
pub fn tuple(items: Vec<MontyObject>) -> MontyObject {
    MontyObject::Tuple(items)
}

/// Build a Python dict. Ordered, because monty dicts are insertion-ordered and
/// a stable order makes a printed `db_stats()` diffable between runs.
#[must_use]
pub fn dict(pairs: Vec<(&str, MontyObject)>) -> MontyObject {
    MontyObject::Dict(
        pairs
            .into_iter()
            .map(|(k, v)| (MontyObject::String(k.to_owned()), v))
            .collect::<Vec<_>>()
            .into(),
    )
}

/// The positional and keyword arguments of one verb call.
pub struct Args<'a> {
    pub verb: &'a str,
    pub pos: &'a [MontyObject],
    pub kw: &'a [(MontyObject, MontyObject)],
}

impl<'a> Args<'a> {
    pub fn new(
        verb: &'a str,
        pos: &'a [MontyObject],
        kw: &'a [(MontyObject, MontyObject)],
    ) -> Self {
        Args { verb, pos, kw }
    }

    /// Require exactly `n` positional arguments.
    pub fn exact(&self, n: usize) -> Result<(), MontyException> {
        if self.pos.len() == n {
            Ok(())
        } else {
            Err(type_err(format!(
                "{}() takes exactly {} argument{}, got {}",
                self.verb,
                n,
                if n == 1 { "" } else { "s" },
                self.pos.len()
            )))
        }
    }

    /// Require between `lo` and `hi` positional arguments.
    pub fn between(&self, lo: usize, hi: usize) -> Result<(), MontyException> {
        if (lo..=hi).contains(&self.pos.len()) {
            Ok(())
        } else {
            Err(type_err(format!(
                "{}() takes {lo} to {hi} arguments, got {}",
                self.verb,
                self.pos.len()
            )))
        }
    }

    fn at(&self, i: usize) -> Result<&MontyObject, MontyException> {
        self.pos
            .get(i)
            .ok_or_else(|| type_err(format!("{}() is missing argument {}", self.verb, i + 1)))
    }

    /// A `u64` key or ordinal, from either monty int arm.
    pub fn u64(&self, i: usize) -> Result<u64, MontyException> {
        u64_of(self.verb, i, self.at(i)?)
    }

    /// A handle index. Handles are small non-negative ints minted by the
    /// harness; anything else is a scenario bug, not a database result.
    pub fn handle(&self, i: usize) -> Result<usize, MontyException> {
        match self.at(i)? {
            MontyObject::Int(v) if *v >= 0 => Ok(*v as usize),
            other => Err(type_err(format!(
                "{}() argument {} must be a handle returned by the harness, got {}",
                self.verb,
                i + 1,
                other.type_name()
            ))),
        }
    }

    /// An arbitrary-magnitude non-negative integer, for the `bn_*` verbs.
    ///
    /// [`Args::u64`] deliberately refuses anything above `u64::MAX`, which is
    /// right for a key or an ordinal and wrong for a bignum: the whole point of
    /// the module is that Python's `int` and `BigUint` have the same range, so
    /// the bridge must carry it. Negatives are refused — `BigUint` is unsigned by
    /// contract, and silently taking a magnitude would make `bn_sub`'s error case
    /// untestable from a scenario.
    pub fn bignum(&self, i: usize) -> Result<BigUint, MontyException> {
        let obj = self.at(i)?;
        let sign_err = || {
            value_err(format!(
                "{}() argument {} must not be negative; bignum is unsigned",
                self.verb,
                i + 1
            ))
        };
        match obj {
            MontyObject::Int(v) => {
                let u = u64::try_from(*v).map_err(|_| sign_err())?;
                Ok(BigUint::from_u64(u))
            }
            MontyObject::BigInt(b) => {
                let (sign, bytes) = b.to_bytes_le();
                if sign == num_bigint::Sign::Minus {
                    return Err(sign_err());
                }
                Ok(BigUint::from_le_bytes(&bytes))
            }
            other => Err(type_err(format!(
                "{}() argument {} must be an int, got {}",
                self.verb,
                i + 1,
                other.type_name()
            ))),
        }
    }

    /// A Python list of arbitrary-magnitude ints.
    pub fn bignum_list(&self, i: usize) -> Result<Vec<BigUint>, MontyException> {
        let items = match self.at(i)? {
            MontyObject::List(items) => items.clone(),
            MontyObject::Tuple(items) => items.to_vec(),
            other => {
                return Err(type_err(format!(
                    "{}() argument {} must be a list of ints, got {}",
                    self.verb,
                    i + 1,
                    other.type_name()
                )))
            }
        };
        let inner = Args::new(self.verb, &items, &[]);
        (0..items.len()).map(|j| inner.bignum(j)).collect()
    }

    pub fn u64_list(&self, i: usize) -> Result<Vec<u64>, MontyException> {
        let obj = self.at(i)?;
        let items = match obj {
            MontyObject::List(v)
            | MontyObject::Tuple(v)
            | MontyObject::Set(v)
            | MontyObject::FrozenSet(v) => v,
            other => {
                return Err(type_err(format!(
                    "{}() argument {} must be a list, tuple or set of integers, got {}",
                    self.verb,
                    i + 1,
                    other.type_name()
                )))
            }
        };
        items
            .iter()
            .map(|x| u64_of(self.verb, i, x))
            .collect::<Result<Vec<_>, _>>()
    }

    /// A list of handles, for the n-ary verbs.
    pub fn handle_list(&self, i: usize) -> Result<Vec<usize>, MontyException> {
        let obj = self.at(i)?;
        let items = match obj {
            MontyObject::List(v) | MontyObject::Tuple(v) => v,
            other => {
                return Err(type_err(format!(
                    "{}() argument {} must be a list or tuple of handles, got {}",
                    self.verb,
                    i + 1,
                    other.type_name()
                )))
            }
        };
        items
            .iter()
            .map(|x| match x {
                MontyObject::Int(v) if *v >= 0 => Ok(*v as usize),
                other => Err(type_err(format!(
                    "{}() argument {}: {} is not a handle returned by the harness",
                    self.verb,
                    i + 1,
                    other.type_name()
                ))),
            })
            .collect()
    }

    /// A required positional string, used for the enumerated selectors
    /// ( `q_time`'s terminal, `yn_const`'s name ).
    pub fn str_at(&self, i: usize) -> Result<&str, MontyException> {
        match self.at(i)? {
            MontyObject::String(s) => Ok(s),
            other => Err(type_err(format!(
                "{}() argument {} must be a string, got {}",
                self.verb,
                i + 1,
                other.type_name()
            ))),
        }
    }

    /// A Python list or tuple containing only strings.
    pub fn string_list(&self, i: usize) -> Result<Vec<String>, MontyException> {
        let items = match self.at(i)? {
            MontyObject::List(items) | MontyObject::Tuple(items) => items,
            other => {
                return Err(type_err(format!(
                    "{}() argument {} must be a list or tuple of strings, got {}",
                    self.verb,
                    i + 1,
                    other.type_name()
                )))
            }
        };
        items
            .iter()
            .map(|item| match item {
                MontyObject::String(value) => Ok(value.clone()),
                other => Err(type_err(format!(
                    "{}() argument {} contains {}, expected only strings",
                    self.verb,
                    i + 1,
                    other.type_name()
                ))),
            })
            .collect()
    }

    /// A `usize` count. Unlike [`Args::u64`] this is for repetition counts and
    /// array indices rather than ordinals, so it is capped at `usize`.
    pub fn usize_at(&self, i: usize) -> Result<usize, MontyException> {
        usize::try_from(self.u64(i)?).map_err(|_| {
            value_err(format!(
                "{}() argument {} does not fit a machine word",
                self.verb,
                i + 1
            ))
        })
    }

    /// An optional positional string, used for database names.
    pub fn opt_str(&self, i: usize) -> Result<Option<String>, MontyException> {
        match self.pos.get(i) {
            None | Some(MontyObject::None) => Ok(None),
            Some(MontyObject::String(s)) => Ok(Some(s.clone())),
            Some(other) => Err(type_err(format!(
                "{}() argument {} must be a string name, got {}",
                self.verb,
                i + 1,
                other.type_name()
            ))),
        }
    }

    /// Reject every keyword not in `allowed`.
    ///
    /// An **unrecognised** keyword is an error rather than being ignored:
    /// `db_open("x", shard=4)` silently opening 8 shards would make a
    /// shard-count scenario prove nothing. Call this once per verb that takes
    /// keywords, *before* reading any of them — reading only the names you
    /// expect would let a typo pass unseen.
    pub fn kw_allowed(&self, allowed: &[&str]) -> Result<(), MontyException> {
        for (k, _) in self.kw {
            let MontyObject::String(k) = k else {
                return Err(type_err(format!(
                    "{}() keyword names must be strings",
                    self.verb
                )));
            };
            if !allowed.contains(&k.as_str()) {
                return Err(type_err(format!(
                    "{}() got an unexpected keyword argument '{k}'; it takes {}",
                    self.verb,
                    if allowed.is_empty() {
                        "none".to_owned()
                    } else {
                        allowed.join(", ")
                    }
                )));
            }
        }
        Ok(())
    }

    /// A keyword argument holding a count of at least `min`, e.g. `shards=4`.
    ///
    /// `min` is a parameter rather than a fixed `1` because `evacuate=0` is
    /// a **meaningful** setting — it is how the aged-state fixture A/Bs the
    /// evacuation path away — and rejecting it would make that column
    /// unreachable from a scenario.
    pub fn kw_usize_min(
        &self,
        name: &str,
        default: usize,
        min: usize,
    ) -> Result<usize, MontyException> {
        let mut found = None;
        for (k, v) in self.kw {
            if matches!(k, MontyObject::String(k) if k == name) {
                found = Some(v);
            }
        }
        match found {
            None => Ok(default),
            // A bool is an int in Python; accepting `True` as 1 here would be
            // the same silent-wrong-argument trap `u64_of` refuses.
            Some(MontyObject::Int(v)) if *v >= 0 && (*v as usize) >= min => Ok(*v as usize),
            Some(other) => Err(value_err(format!(
                "{}() argument '{name}' must be an integer >= {min}, got {}",
                self.verb,
                other.type_name()
            ))),
        }
    }

    /// A keyword argument holding a positive count. Shorthand for the common
    /// single-keyword verb: checks the name set and reads it in one call.
    pub fn kw_usize(&self, name: &str, default: usize) -> Result<usize, MontyException> {
        self.kw_allowed(&[name])?;
        self.kw_usize_min(name, default, 1)
    }

    /// A keyword argument holding a flag, e.g. `isolate_mounts=True`.
    ///
    /// Only a real bool is accepted. `isolate_mounts=1` is refused for the
    /// same reason `kw_usize_min` refuses `True`: a scenario that means one
    /// thing and writes another must fail loudly rather than silently take the
    /// default and then assert against a topology it never asked for.
    pub fn kw_bool(&self, name: &str, default: bool) -> Result<bool, MontyException> {
        let mut found = None;
        for (k, v) in self.kw {
            if matches!(k, MontyObject::String(k) if k == name) {
                found = Some(v);
            }
        }
        match found {
            None => Ok(default),
            Some(MontyObject::Bool(v)) => Ok(*v),
            Some(other) => Err(value_err(format!(
                "{}() argument '{name}' must be True or False, got {}",
                self.verb,
                other.type_name()
            ))),
        }
    }

    /// Reject keyword arguments entirely, for verbs that accept none.
    pub fn no_kwargs(&self) -> Result<(), MontyException> {
        if let Some((k, _)) = self.kw.first() {
            return Err(type_err(format!(
                "{}() takes no keyword arguments, got '{k}'",
                self.verb
            )));
        }
        Ok(())
    }
}

fn u64_of(verb: &str, i: usize, o: &MontyObject) -> Result<u64, MontyException> {
    let out_of_range = || {
        value_err(format!(
            "{verb}() argument {}: {} is outside the u64 range yesno addresses",
            i + 1,
            o
        ))
    };
    match o {
        MontyObject::Int(v) => u64::try_from(*v).map_err(|_| out_of_range()),
        MontyObject::BigInt(b) => u64::try_from(b).map_err(|_| out_of_range()),
        MontyObject::Bool(_) => Err(type_err(format!(
            "{verb}() argument {}: a bool is not a key or ordinal",
            i + 1
        ))),
        other => Err(type_err(format!(
            "{verb}() argument {} must be an integer, got {}",
            i + 1,
            other.type_name()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The u64/i64 boundary in both directions. If `int_obj` truncated, the
    /// round trip would come back negative.
    #[test]
    fn values_above_i64_max_survive_the_round_trip() {
        for v in [0u64, 1, i64::MAX as u64, i64::MAX as u64 + 1, u64::MAX] {
            let obj = int_obj(v);
            let args = Args::new("t", std::slice::from_ref(&obj), &[]);
            assert_eq!(args.u64(0).unwrap(), v, "round trip failed for {v}");
        }
        assert!(matches!(int_obj(u64::MAX), MontyObject::BigInt(_)));
        assert!(matches!(int_obj(7), MontyObject::Int(7)));
    }

    #[test]
    fn negative_and_oversized_integers_are_refused() {
        let neg = MontyObject::Int(-1);
        assert!(Args::new("t", std::slice::from_ref(&neg), &[])
            .u64(0)
            .is_err());

        let huge = MontyObject::BigInt(BigInt::from(u64::MAX) + 1);
        assert!(Args::new("t", std::slice::from_ref(&huge), &[])
            .u64(0)
            .is_err());
    }

    /// A bool is an int in Python. Accepting `True` as ordinal 1 would let a
    /// scenario pass an obviously wrong argument and get a plausible answer.
    #[test]
    fn a_bool_is_not_an_ordinal() {
        let b = MontyObject::Bool(true);
        assert!(Args::new("t", std::slice::from_ref(&b), &[])
            .u64(0)
            .is_err());
    }

    #[test]
    fn an_unknown_keyword_is_an_error_not_a_default() {
        let kw = [(MontyObject::String("shard".to_owned()), MontyObject::Int(4))];
        let args = Args::new("db_open", &[], &kw);
        assert!(args.kw_usize("shards", 8).is_err());
    }
}
