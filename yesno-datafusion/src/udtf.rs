//! `yesno_lookup('term')` — a posting list as a table.
//!
//! # Why this lands before the indexed-table pushdown
//!
//! It composes with *any* table, on day one, with no assumptions about how rows
//! map to ordinals:
//!
//! ```sql
//! SELECT d.* FROM docs d JOIN yesno_lookup('rust') o ON d.id = o.ordinal
//! ```
//!
//! The `ParquetAccessPlan` pushdown is far more powerful but requires a stable,
//! dense, order-defined row→ordinal mapping. Any table mutation, file reorder or
//! compaction silently corrupts that mapping — **wrong results, no error**. This
//! function has none of that exposure, which is why it is the risk-free 80% and
//! goes first.

use std::collections::HashMap;
use std::sync::Arc;

use arrow_array::RecordBatch;
use arrow_schema::SchemaRef;
use datafusion::catalog::{Session, TableFunctionArgs, TableFunctionImpl, TableProvider};
use datafusion::common::{plan_err, DataFusionError, Result as DfResult, ScalarValue};
use datafusion::datasource::memory::MemTable;
use datafusion::logical_expr::{Expr, TableType};
use datafusion::physical_plan::ExecutionPlan;
use yesno_arrow::{ordinals_schema, OrdinalBatchReader};
use yesno_core::OrdSet;

use crate::pushdown::{HashEncoder, TermEncoder};

/// Somewhere to fetch a posting list by key.
///
/// A trait rather than a concrete `Db` so a caller can back the function with a
/// snapshot, a fixture, or a remote store without this module knowing.
pub trait PostingSource: Send + Sync + std::fmt::Debug {
    /// The posting list for `key`, or `None` if nothing is indexed under it.
    ///
    /// **`None` and `Err` are different answers and must stay so.** A term
    /// nobody indexed is a legitimate query returning no rows; a read that
    /// *failed* is not. Collapsing the second into the first makes a query
    /// succeed with zero rows when it could not read its data -- wrong in the
    /// safe direction, but a query engine cannot retry what it was not told
    /// about.
    fn posting_list(&self, key: u64) -> DfResult<Option<Arc<OrdSet>>>;
}

/// An in-memory source, for tests and small fixtures.
#[derive(Debug, Default)]
pub struct MapSource {
    lists: HashMap<u64, Arc<OrdSet>>,
}

impl MapSource {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, key: u64, set: OrdSet) -> &mut Self {
        self.lists.insert(key, Arc::new(set));
        self
    }
}

impl PostingSource for MapSource {
    fn posting_list(&self, key: u64) -> DfResult<Option<Arc<OrdSet>>> {
        Ok(self.lists.get(&key).cloned())
    }
}

/// A posting source backed by a **real database**, at one snapshot.
///
/// Until this landed, `MapSource` was the only implementation of
/// `PostingSource` anywhere in the workspace — and its own doc says "for tests
/// and small fixtures". So the trait's promise, "a caller can back the function
/// with a snapshot", was true of nothing: `yesno_lookup` could be pointed at a
/// `HashMap` you filled by hand and at nothing else. That is the same shape this
/// project keeps finding — `Follower` with no caller while the gate hand-rolled
/// its own, a `db_uuid` written everywhere and compared nowhere — and it is why
/// the trait comment was not evidence.
///
/// # One snapshot, held for the source's life
///
/// Deliberate, and it is what makes a multi-key query *mean* something: every
/// lookup inside one query answers from the same instant, so a plan touching
/// three keys cannot assemble a union that never existed. Re-opening a snapshot
/// per lookup would be cheaper to write and wrong in a way nothing downstream
/// could detect — the same argument the Flight ticket's version field makes.
///
/// And it has a cost worth stating: a live `Snapshot` holds `Arc<DbInner>`,
/// so it pins the database's file lock and holds the reclamation floor down for
/// as long as it exists. Do not park one in a long-lived session context and
/// forget it; build one per query, or accept that space amplification is bounded
/// by how long your longest query runs.
pub struct SnapshotSource {
    snap: yesno_core::Snapshot,
}

// Hand-written because `Snapshot` is not `Debug`, and reporting only the
// version is the right amount: `PostingSource: Debug` exists so a plan can be
// printed, and a plan naming the instant it reads from is exactly what a reader
// of an `EXPLAIN` wants. Not the contents — a posting source can hold
// millions of ordinals.
impl std::fmt::Debug for SnapshotSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SnapshotSource")
            .field("version", &self.snap.version())
            .finish()
    }
}

impl SnapshotSource {
    pub fn new(snap: yesno_core::Snapshot) -> Self {
        SnapshotSource { snap }
    }

    /// The version every lookup through this source answers from.
    pub fn version(&self) -> u64 {
        self.snap.version()
    }
}

impl PostingSource for SnapshotSource {
    /// An absent key is `Ok( None )`; a failed read is `Err`.
    ///
    /// Until 2026-09-12 both were `None`, so a snapshot evicted under
    /// `AbortOldestReader` -- which reports `SnapshotTooOld` -- became an empty
    /// posting list and the query **succeeded with zero rows**. That was the
    /// safe direction and still a wrong answer, because nothing downstream
    /// could tell it apart from a term nobody indexed.
    ///
    /// The entry recording this called it a design item pending "a decision
    /// about what a DataFusion operator should do with the error". The decision
    /// was already made by the only caller: `provider_for` returns `DfResult`
    /// and the failure surfaces while *planning* the table provider, exactly
    /// where DataFusion already reports an unencodable term.
    fn posting_list(&self, key: u64) -> DfResult<Option<Arc<OrdSet>>> {
        match self.snap.load(key) {
            Ok(s) if s.is_empty() => Ok(None),
            Ok(s) => Ok(Some(Arc::new(s))),
            Err(error) => Err(DataFusionError::External(Box::new(error))),
        }
    }
}

/// The `yesno_lookup` table function.
#[derive(Debug)]
pub struct YesnoLookup {
    source: Arc<dyn PostingSource>,
    encoder: Arc<dyn TermEncoder>,
}

impl YesnoLookup {
    pub fn new(source: Arc<dyn PostingSource>) -> Self {
        YesnoLookup {
            source,
            encoder: Arc::new(HashEncoder),
        }
    }

    pub fn with_encoder(source: Arc<dyn PostingSource>, encoder: Arc<dyn TermEncoder>) -> Self {
        YesnoLookup { source, encoder }
    }

    fn provider_for(&self, term: &ScalarValue) -> DfResult<Arc<dyn TableProvider>> {
        let schema = ordinals_schema();
        let Some(key) = self.encoder.encode(term) else {
            return plan_err!("yesno_lookup: cannot encode {term:?} as a key");
        };
        let batches = match self.source.posting_list(key)? {
            // An absent key is an empty table, not an error: a term nobody
            // indexed is a legitimate query returning no rows. A *failed*
            // read is not absent and propagates above via `?`.
            None => Vec::new(),
            Some(set) => OrdinalBatchReader::new(set.stream())
                .collect::<std::result::Result<Vec<RecordBatch>, _>>()
                .map_err(|e| DataFusionError::ArrowError(Box::new(e), None))?,
        };
        Ok(Arc::new(PostingTable { schema, batches }))
    }
}

impl TableFunctionImpl for YesnoLookup {
    /// `call_with_args`, not the deprecated `call`.
    ///
    /// `call` has been deprecated since DataFusion 53; the default
    /// `call_with_args` forwards to it, so implementing the old one still works
    /// but ties this crate to a method scheduled for removal.
    fn call_with_args(&self, args: TableFunctionArgs) -> DfResult<Arc<dyn TableProvider>> {
        let [Expr::Literal(term, _)] = args.exprs() else {
            return plan_err!("yesno_lookup expects exactly one literal argument");
        };
        self.provider_for(term)
    }
}

/// A materialized posting list exposed as a table.
///
/// Batches are built eagerly. That is a real limitation for a very large
/// posting list and the streaming form is the obvious follow-up — but a custom
/// `ExecutionPlan` is DataFusion's least stable extension point, so reusing the
/// memory source is the right first landing.
#[derive(Debug)]
struct PostingTable {
    schema: SchemaRef,
    batches: Vec<RecordBatch>,
}

#[async_trait::async_trait]
impl TableProvider for PostingTable {
    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    fn table_type(&self) -> TableType {
        TableType::Base
    }

    async fn scan(
        &self,
        state: &dyn Session,
        projection: Option<&Vec<usize>>,
        filters: &[Expr],
        limit: Option<usize>,
    ) -> DfResult<Arc<dyn ExecutionPlan>> {
        let mem = MemTable::try_new(self.schema.clone(), vec![self.batches.clone()])?;
        mem.scan(state, projection, filters, limit).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use datafusion::prelude::SessionContext;

    fn source() -> Arc<MapSource> {
        let mut m = MapSource::new();
        let enc = HashEncoder;
        let rust = enc.encode(&ScalarValue::Utf8(Some("rust".into()))).unwrap();
        let go = enc.encode(&ScalarValue::Utf8(Some("go".into()))).unwrap();
        m.insert(rust, OrdSet::from_iter_unsorted([1u64, 3, 5, 7, 65_540]));
        m.insert(go, OrdSet::from_iter_unsorted([2u64, 3, 4]));
        Arc::new(m)
    }

    async fn query(sql: &str) -> Vec<u64> {
        let ctx = SessionContext::new();
        ctx.register_udtf("yesno_lookup", Arc::new(YesnoLookup::new(source())));
        let df = ctx.sql(sql).await.unwrap();
        let batches = df.collect().await.unwrap();
        let mut out = Vec::new();
        for b in batches {
            let a = b
                .column(0)
                .as_any()
                .downcast_ref::<arrow_array::UInt64Array>()
                .unwrap();
            out.extend(a.values().iter().copied());
        }
        out.sort_unstable();
        out
    }

    #[tokio::test]
    async fn a_lookup_returns_its_posting_list() {
        let got = query("SELECT ordinal FROM yesno_lookup('rust')").await;
        assert_eq!(got, vec![1, 3, 5, 7, 65_540]);
    }

    #[tokio::test]
    async fn an_unknown_term_is_an_empty_table_not_an_error() {
        // A term nobody indexed is a legitimate query with no rows.
        let got = query("SELECT ordinal FROM yesno_lookup('nonexistent')").await;
        assert!(got.is_empty());
    }

    /// A source whose read fails, to separate "nothing indexed" from "could not
    /// read". `MapSource` cannot express the second, which is why the gap
    /// went unnoticed: every implementation in the workspace could only succeed.
    #[derive(Debug)]
    struct FailingSource;

    impl PostingSource for FailingSource {
        fn posting_list(&self, _key: u64) -> DfResult<Option<Arc<OrdSet>>> {
            Err(DataFusionError::External("snapshot too old".into()))
        }
    }

    /// The bug this pins: a failed read must not look like an unindexed term.
    ///
    /// Both answered zero rows until `PostingSource` became fallible, so a
    /// snapshot evicted under `AbortOldestReader` produced a *successful* query
    /// over no data. Pairing it with `an_unknown_term_is_an_empty_table_not_an_error`
    /// is the point -- either assertion alone is satisfied by the broken
    /// behaviour, and only the two together separate the cases.
    #[tokio::test]
    async fn a_failed_read_is_an_error_and_not_an_empty_table() {
        let ctx = SessionContext::new();
        ctx.register_udtf(
            "yesno_lookup",
            Arc::new(YesnoLookup::new(Arc::new(FailingSource))),
        );
        let result = ctx.sql("SELECT ordinal FROM yesno_lookup('rust')").await;
        let error = match result {
            Err(error) => error,
            Ok(frame) => match frame.collect().await {
                Err(error) => error,
                Ok(batches) => panic!(
                    "a failed read answered {} batch(es) instead of erroring",
                    batches.len()
                ),
            },
        };
        let text = error.to_string();
        assert!(text.contains("snapshot too old"), "{text}");
    }

    #[tokio::test]
    async fn a_lookup_joins_against_a_real_table() {
        // The whole point: it composes with any table, with no assumptions about
        // how that table's rows map to ordinals.
        let ctx = SessionContext::new();
        ctx.register_udtf("yesno_lookup", Arc::new(YesnoLookup::new(source())));
        ctx.sql(
            "CREATE TABLE docs AS
             SELECT * FROM (VALUES (1, 'a'), (2, 'b'), (3, 'c'), (4, 'd')) AS t(id, body)",
        )
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();

        let df = ctx
            .sql(
                "SELECT d.id FROM docs d
                 JOIN yesno_lookup('rust') o ON CAST(d.id AS BIGINT UNSIGNED) = o.ordinal
                 ORDER BY d.id",
            )
            .await
            .unwrap();
        let batches = df.collect().await.unwrap();
        let mut ids = Vec::new();
        for b in batches {
            let a = b
                .column(0)
                .as_any()
                .downcast_ref::<arrow_array::Int64Array>()
                .unwrap();
            ids.extend(a.values().iter().copied());
        }
        assert_eq!(
            ids,
            vec![1, 3],
            "only docs present in the posting list survive"
        );
    }

    #[tokio::test]
    async fn aggregates_work_over_a_lookup() {
        let ctx = SessionContext::new();
        ctx.register_udtf("yesno_lookup", Arc::new(YesnoLookup::new(source())));
        let df = ctx
            .sql("SELECT COUNT(*) AS n FROM yesno_lookup('go')")
            .await
            .unwrap();
        let batches = df.collect().await.unwrap();
        let a = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<arrow_array::Int64Array>()
            .unwrap();
        assert_eq!(a.value(0), 3);
    }

    #[tokio::test]
    async fn the_schema_is_a_single_non_nullable_ordinal_column() {
        let p = YesnoLookup::new(source())
            .provider_for(&ScalarValue::Utf8(Some("rust".into())))
            .unwrap();
        let s = p.schema();
        assert_eq!(s.fields().len(), 1);
        assert_eq!(s.field(0).name(), "ordinal");
        assert!(!s.field(0).is_nullable());
    }

    #[tokio::test]
    async fn a_non_literal_argument_is_a_plan_error() {
        let ctx = SessionContext::new();
        ctx.register_udtf("yesno_lookup", Arc::new(YesnoLookup::new(source())));
        // A column reference is not a literal term.
        let err = ctx
            .sql("SELECT ordinal FROM yesno_lookup(ordinal)")
            .await
            .expect_err("a non-literal argument must be rejected at plan time");
        let msg = err.to_string();
        assert!(
            msg.contains("literal") || msg.contains("not found") || msg.contains("Schema"),
            "unexpected error: {msg}"
        );
    }
}
