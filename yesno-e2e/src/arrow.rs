//! `ar_*`: the Arrow surface, over data that came out of a real database.
//!
//! # What these are for
//!
//! **Not** re-checking that a mask selects the right ordinals. `yesno-arrow`'s
//! own unit tests do that against a hand-built `OrdSet`, and a Python walk over a
//! mask would be a second implementation of `BooleanBuffer` — the `and_shape.py`
//! mistake.
//!
//! The claim that **cannot** be made from a hand-built set: *zero copy is a
//! property of the container's kind, and the kind is a property of the write
//! history*. `is_zero_copy` is true only for a bitmap, and whether a chunk is a
//! bitmap depends on how many ordinals were written into it and in what pattern
//! — decided by size class at promotion time, and decided **again** by the store
//! codec when the chunk is read back after a reopen.
//!
//! Those are two different implementations of the same decision, and only an
//! operational sequence crosses both. A Rust test that builds an `OrdSet` in
//! memory and asserts `is_zero_copy` never touches the second one at all.
//!
//! Every verb takes a **snapshot** handle rather than a set handle, for
//! exactly that reason: reading through a snapshot after a checkpoint and reopen
//! is what makes the store's answer observable. A set handle would be the
//! in-memory path wearing a different name.

use arrow_array::{Array, UInt64Array};
use monty_types::{MontyException, MontyObject};
// `masks::is_zero_copy` by its full path: it is `pub` in a `pub mod` and so
// reachable, but it is **not** in `lib.rs`'s re-export list — and until these
// verbs landed it had no caller anywhere in the workspace outside its own unit
// test, which is the unwired-`pub fn` signature this repo sweeps for.
use yesno_arrow::masks::is_zero_copy;
use yesno_arrow::{ContainerBatchBuilder, MaskStream, OrdinalBatchReader};
use yesno_core::stream::SetStream;

use crate::convert::{db_err, dict, int_obj, value_err, Args};
use crate::world::World;

pub const OWNS: &[&str] = &[
    "ar_kinds",
    "ar_masks",
    "ar_mask_ordinals",
    "ar_batches",
    "ar_batch_ordinals",
    "ar_containers",
];

/// Container kind as a word a scenario can assert on.
///
/// Spelled out rather than returned as an index. A scenario asserting
/// `kinds[0] == 1` is a scenario nobody can review, and the mapping would be a
/// second place the enum is written down.
fn kind_name(c: &yesno_core::Container) -> &'static str {
    match c.kind() {
        yesno_core::ContainerKind::Array => "array",
        yesno_core::ContainerKind::Bitmap => "bitmap",
        yesno_core::ContainerKind::Run => "run",
    }
}

/// `"sparse"` ( only chunks the set touches ) or `"dense"` ( every chunk in the
/// set's span, including empty ones ).
///
/// Spelled out rather than a boolean, for the reason `mx_*` gives about its
/// semiring: `ar_masks( s, 7, True )` does not say what is true.
fn coverage_of(verb: &str, s: Option<&str>) -> Result<bool, MontyException> {
    match s {
        None | Some("sparse") => Ok(false),
        Some("dense") => Ok(true),
        Some(other) => Err(value_err(format!(
            "{verb}(): coverage must be \"sparse\" or \"dense\", not {other:?}"
        ))),
    }
}

impl World {
    pub(crate) fn call_arrow(
        &mut self,
        verb: &str,
        a: &Args<'_>,
    ) -> Result<MontyObject, MontyException> {
        match verb {
            // The shape of a key, chunk by chunk: which representation each
            // chunk is in and whether it can lend its bits.
            //
            // `zero_copy` is `is_zero_copy` itself and not a re-derivation
            // from `kind`. The two agreeing is the point — a scenario that
            // computed it from the kind would pass even if the predicate were
            // wired to a different container.
            "ar_kinds" => {
                a.exact(2)?;
                a.no_kwargs()?;
                let (s, key) = (a.handle(0)?, a.u64(1)?);
                let set = self.snap(s, verb)?.load(key).map_err(|e| db_err(verb, e))?;
                Ok(MontyObject::List(
                    set.chunks()
                        .map(|(prefix, c)| {
                            dict(vec![
                                ("prefix", int_obj(prefix)),
                                ("kind", MontyObject::String(kind_name(c).to_owned())),
                                ("zero_copy", MontyObject::Bool(is_zero_copy(c))),
                                ("cardinality", int_obj(c.len() as u64)),
                            ])
                        })
                        .collect(),
                ))
            }

            // One mask per chunk the set touches, as the shipped `MaskStream`
            // produces them. `dense=True` emits a mask for every chunk in the
            // span, including empty ones — the shape a scan over a contiguous
            // ordinal space wants.
            "ar_masks" => {
                a.between(2, 3)?;
                a.no_kwargs()?;
                let (s, key) = (a.handle(0)?, a.u64(1)?);
                let dense = coverage_of(verb, a.opt_str(2)?.as_deref())?;
                let set = self.snap(s, verb)?.load(key).map_err(|e| db_err(verb, e))?;
                let masks = mask_chunks(verb, &set, dense)?;
                Ok(MontyObject::List(
                    masks
                        .into_iter()
                        .map(|m| {
                            dict(vec![
                                ("base", int_obj(m.base_ordinal)),
                                ("selected", int_obj(m.selected() as u64)),
                                ("bits", int_obj(m.mask.len() as u64)),
                            ])
                        })
                        .collect(),
                ))
            }

            // Every ordinal the mask path selects, reassembled from the bits.
            //
            // This is the one place a scenario reads the buffer itself, and it
            // is deliberate: the oracle is Python's own `set`, so the comparison
            // is against something written independently of yesno rather than
            // against another yesno path. It is **not** a general-purpose
            // mask API for scenarios to build on — the assertion it exists for
            // is "the mask selects exactly the set".
            "ar_mask_ordinals" => {
                a.between(2, 3)?;
                a.no_kwargs()?;
                let (s, key) = (a.handle(0)?, a.u64(1)?);
                let dense = coverage_of(verb, a.opt_str(2)?.as_deref())?;
                let set = self.snap(s, verb)?.load(key).map_err(|e| db_err(verb, e))?;
                let mut out = Vec::new();
                for m in mask_chunks(verb, &set, dense)? {
                    for (i, bit) in m.mask.iter().enumerate() {
                        if bit {
                            out.push(int_obj(m.base_ordinal + i as u64));
                        }
                    }
                }
                Ok(MontyObject::List(out))
            }

            // Row counts of the batches `OrdinalBatchReader` emits. The interest
            // is the *shape* — a scenario asserts that a large key arrives as
            // several batches rather than one allocation.
            "ar_batches" => {
                a.exact(2)?;
                a.no_kwargs()?;
                let (s, key) = (a.handle(0)?, a.u64(1)?);
                let set = self.snap(s, verb)?.load(key).map_err(|e| db_err(verb, e))?;
                let mut rows = Vec::new();
                for b in OrdinalBatchReader::new(SetStream::new(std::sync::Arc::new(set.clone()))) {
                    let b = b.map_err(|e| db_err(verb, e))?;
                    rows.push(int_obj(b.num_rows() as u64));
                }
                Ok(MontyObject::List(rows))
            }

            "ar_batch_ordinals" => {
                a.exact(2)?;
                a.no_kwargs()?;
                let (s, key) = (a.handle(0)?, a.u64(1)?);
                let set = self.snap(s, verb)?.load(key).map_err(|e| db_err(verb, e))?;
                let mut out = Vec::new();
                for b in OrdinalBatchReader::new(SetStream::new(std::sync::Arc::new(set.clone()))) {
                    let b = b.map_err(|e| db_err(verb, e))?;
                    let col = b
                        .column(0)
                        .as_any()
                        .downcast_ref::<UInt64Array>()
                        .ok_or_else(|| {
                            value_err(format!("{verb}(): the batch is not a UInt64 column"))
                        })?;
                    out.extend((0..col.len()).map(|i| int_obj(col.value(i))));
                }
                Ok(MontyObject::List(out))
            }

            // The container wire path: encode a key's chunks into a
            // `RecordBatch` and read them straight back.
            //
            // A **round trip**, so the assertion is set equality against
            // Python's oracle rather than "the encoder produced some bytes".
            // This is the format `yesno-flight` and `yesno-pg` both speak, and
            // the one whose whole argument is that a container crosses it
            // without being re-encoded.
            "ar_containers" => {
                a.exact(2)?;
                a.no_kwargs()?;
                let (s, key) = (a.handle(0)?, a.u64(1)?);
                let set = self.snap(s, verb)?.load(key).map_err(|e| db_err(verb, e))?;

                let mut b = ContainerBatchBuilder::new();
                b.push_set(key, &set);
                let batch = match b.finish().map_err(|e| db_err(verb, e))? {
                    Some(batch) => batch,
                    // An empty key encodes to nothing, which is not an error and
                    // must not be reported as one.
                    None => {
                        return Ok(dict(vec![
                            ("chunks", int_obj(0)),
                            ("ordinals", MontyObject::List(vec![])),
                        ]))
                    }
                };
                let chunks = yesno_arrow::read_containers(&batch).map_err(|e| db_err(verb, e))?;
                let mut out = Vec::new();
                for (k, prefix, c) in &chunks {
                    if *k != key {
                        return Err(value_err(format!(
                            "{verb}(): a chunk came back under key {k}, not {key}"
                        )));
                    }
                    let base = prefix << yesno_core::CHUNK_BITS;
                    out.extend(c.iter().map(|v| int_obj(base | v as u64)));
                }
                Ok(dict(vec![
                    ("chunks", int_obj(chunks.len() as u64)),
                    ("ordinals", MontyObject::List(out)),
                ]))
            }

            _ => Err(value_err(format!("{verb}(): not an arrow verb"))),
        }
    }
}

/// Drain a `MaskStream` over one set.
fn mask_chunks(
    verb: &str,
    set: &yesno_core::OrdSet,
    dense: bool,
) -> Result<Vec<yesno_arrow::MaskChunk>, MontyException> {
    let stream = SetStream::new(std::sync::Arc::new(set.clone()));
    let masks: yesno_core::Result<Vec<_>> = if dense {
        // The dense span is the set's own, so an empty set yields nothing
        // rather than a mask over the whole 48-bit prefix space.
        match (set.min(), set.max()) {
            (Some(lo), Some(hi)) => MaskStream::dense(
                stream,
                lo >> yesno_core::CHUNK_BITS,
                hi >> yesno_core::CHUNK_BITS,
            )
            .collect(),
            _ => Ok(Vec::new()),
        }
    } else {
        MaskStream::new(stream).collect()
    };
    masks.map_err(|e| db_err(verb, e))
}
