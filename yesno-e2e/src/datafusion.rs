//! `df_*`: SQL over a real database, through DataFusion.
//!
//! # Why this is the only shape worth having here
//!
//! `pushdown::lower` is a pure `Expr -> LoweredFilter`, and `lowering_oracle.rs`
//! already tests it against a row-level oracle. Reaching it from a scenario
//! would mean building DataFusion `Expr` trees out of Python — an expression DSL
//! in the scenario language, which is precisely what TESTING §4 forbids and what
//! got `and_shape.py` moved out of `scenarios/`.
//!
//! What no Rust test covered until now is the thing an operator actually
//! does: **write to a database, then query it in SQL**. That path did not exist.
//! `PostingSource` had exactly one implementation, `MapSource`, whose own doc
//! says "for tests and small fixtures" — so `yesno_lookup` could be pointed at a
//! `HashMap` filled by hand and at nothing else, and the trait's promise that "a
//! caller can back the function with a snapshot" was true of nothing.
//! `SnapshotSource` is that missing half, and these verbs are its first caller.
//!
//! The term encoder is a **hash**, and deliberately not injective — see
//! `HashEncoder`. So a scenario cannot guess which key `yesno_lookup( 'rust' )`
//! reads; `df_key` answers that, and writing under any other key would make
//! every assertion here vacuous.

use std::sync::Arc;

use arrow_array::{Array, UInt64Array};
use datafusion::common::ScalarValue;
use datafusion::prelude::SessionContext;
use monty_types::{MontyException, MontyObject};
use yesno_datafusion::{HashEncoder, SnapshotSource, TermEncoder, YesnoLookup};

use crate::convert::{db_err, int_obj, value_err, Args};
use crate::world::World;

pub const OWNS: &[&str] = &["df_key", "df_sql"];

impl World {
    pub(crate) fn call_datafusion(
        &mut self,
        verb: &str,
        a: &Args<'_>,
    ) -> Result<MontyObject, MontyException> {
        match verb {
            // The key `yesno_lookup( term )` will read.
            //
            // Not a convenience. The encoder hashes, so this is the only way a
            // scenario can write data the query will find — and it is the
            // *shipped* encoder answering, so a change to the hash moves both
            // sides together rather than reddening for the wrong reason.
            "df_key" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let term = a.str_at(0)?;
                HashEncoder
                    .encode(&ScalarValue::Utf8(Some(term.to_string())))
                    .map(int_obj)
                    .ok_or_else(|| {
                        value_err(format!("{verb}(): {term:?} is not encodable as a key"))
                    })
            }

            // Run SQL against a snapshot, returning the single `u64` column.
            //
            // One `SnapshotSource` per call, holding **this** snapshot: every
            // `yesno_lookup` inside one statement therefore answers from one
            // instant, which is what makes a query over several terms mean
            // anything. A source rebuilt per lookup would assemble a union that
            // never existed and nothing downstream could tell.
            "df_sql" => {
                a.exact(2)?;
                a.no_kwargs()?;
                let (s, sql) = (a.handle(0)?, a.str_at(1)?);
                // Cloned out before the runtime borrow: `snap` borrows `self`,
                // and `rt_handle` needs it mutably.
                let snap = self.snap(s, verb)?.clone();
                let rt = self.rt_handle(verb)?;

                let ctx = SessionContext::new();
                ctx.register_udtf(
                    "yesno_lookup",
                    Arc::new(YesnoLookup::new(Arc::new(SnapshotSource::new(snap)))),
                );

                let out: Result<Vec<u64>, MontyException> = rt.block_on(async {
                    let df = ctx.sql(sql).await.map_err(|e| db_err(verb, e))?;
                    let batches = df.collect().await.map_err(|e| db_err(verb, e))?;
                    let mut ords = Vec::new();
                    for b in &batches {
                        if b.num_columns() != 1 {
                            return Err(value_err(format!(
                                "{verb}(): the query returned {} columns; these verbs read one \
                                 u64 column, so project it in SQL",
                                b.num_columns()
                            )));
                        }
                        let col = b
                            .column(0)
                            .as_any()
                            .downcast_ref::<UInt64Array>()
                            .ok_or_else(|| {
                                value_err(format!(
                                    "{verb}(): column `{}` is {:?}, not UInt64",
                                    b.schema().field(0).name(),
                                    b.schema().field(0).data_type()
                                ))
                            })?;
                        ords.extend((0..col.len()).map(|i| col.value(i)));
                    }
                    Ok(ords)
                });
                Ok(MontyObject::List(out?.into_iter().map(int_obj).collect()))
            }

            _ => Err(value_err(format!("{verb}(): not a datafusion verb"))),
        }
    }
}
