//! A yesno-shaped client over Apache Flight's mid-level client.
//!
//! [`arrow_flight::FlightClient`] deliberately knows nothing about yesno's
//! descriptors, exact cardinalities, versioned tickets, pair schema, or ingest
//! acknowledgements. [`YesnoClient`] supplies those pieces while retaining
//! access to the underlying Flight client for transport metadata and protocol
//! operations that are not yesno-specific.
//!
//! Planning and fetching are separate operations as well as one-shot helpers.
//! This is important rather than decorative: a [`QueryInfo`] owns the versioned
//! ticket returned alongside its exact cardinality, so callers can cache and
//! reuse that ticket when they need repeatable reads.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use arrow_array::{Array, RecordBatch, UInt64Array};
use arrow_flight::decode::FlightRecordBatchStream;
use arrow_flight::encode::FlightDataEncoderBuilder;
use arrow_flight::error::{FlightError, Result};
use arrow_flight::{Action, FlightClient, FlightDescriptor, FlightInfo, Ticket as FlightTicket};
use futures::{Stream, TryStreamExt};
use tonic::codegen::{Body, Bytes, StdError};
use tonic::transport::{Channel, Endpoint};

use crate::mutations_schema;
use crate::{
    pairs_schema, QueryRequest, ServerStats, SetExpr, Ticket, ACTION_ABORT_WRITE,
    ACTION_BEGIN_WRITE, ACTION_CLEAR, ACTION_COMMIT_WRITE, ACTION_CONTAINS, ACTION_INSERT_ONE,
    ACTION_REMOVE_ONE, BATCH_ROWS, OP_DELETE_KEY, OP_INSERT, OP_INSERT_RANGE, OP_REMOVE,
    OP_REMOVE_RANGE, PUT_APPLY, PUT_INSERT, PUT_REMOVE, PUT_TXN_PREFIX,
};

/// An open write transaction.
///
/// Opaque and deliberately *not* a [`crate::Ticket`]. A read ticket names an
/// immutable version and prefix range and is safe to cache, copy and replay; a
/// write handle owns mutable staged state with an expiry and a single
/// resolution. Sharing an encoding would not make their lifecycles the same,
/// and would turn every cached ticket into a potential mutation capability.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WriteTxn(pub u64);

/// One operation in a mixed batch.
///
/// Ranges are inclusive and are carried as ranges rather than expanded into
/// ordinals: the engine writes one record and one container call per chunk for
/// a range, and expanding client-side throws both away.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mutation {
    Insert {
        key: u64,
        ordinal: u64,
    },
    Remove {
        key: u64,
        ordinal: u64,
    },
    InsertRange {
        key: u64,
        lo: u64,
        hi: u64,
    },
    RemoveRange {
        key: u64,
        lo: u64,
        hi: u64,
    },
    /// Drop every ordinal under `key`.
    DeleteKey {
        key: u64,
    },
}

impl Mutation {
    /// `( key, lo, hi, op )` as `mutations_schema` carries it.
    fn columns(self) -> (u64, u64, u64, u8) {
        match self {
            Mutation::Insert { key, ordinal } => (key, ordinal, ordinal, OP_INSERT),
            Mutation::Remove { key, ordinal } => (key, ordinal, ordinal, OP_REMOVE),
            Mutation::InsertRange { key, lo, hi } => (key, lo, hi, OP_INSERT_RANGE),
            Mutation::RemoveRange { key, lo, hi } => (key, lo, hi, OP_REMOVE_RANGE),
            Mutation::DeleteKey { key } => (key, 0, 0, OP_DELETE_KEY),
        }
    }
}

/// What the server acknowledged for one ingest call.
///
/// # The version is `Option`, and the reason is wire compatibility
///
/// `do_put`'s `app_metadata` carried **8 bytes** — the row count alone — until
/// 2026-09-12, and a server that predates the change still sends 8. Decoding
/// accepts both widths and reports `None` for the short form, so a new client
/// against an old server degrades to "I cannot tell you the version" rather
/// than to a protocol error.
///
/// `Some( 0 )` never occurs: version 0 is the empty database and no commit is
/// assigned it, so a server that committed nothing ( an empty stream ) also
/// reports `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ack {
    /// Pairs the server accepted and committed.
    pub rows: u64,
    /// The database version at which those rows are present, when the server
    /// reports one.
    pub version: Option<u64>,
}

impl Ack {
    fn decode(metadata: &[u8]) -> Result<Self> {
        let rows = metadata
            .get(..8)
            .and_then(|b| <[u8; 8]>::try_from(b).ok())
            .map(u64::from_le_bytes)
            .ok_or_else(|| {
                FlightError::protocol(format!(
                    "yesno ingest acknowledgement has {} bytes, fewer than the 8 a row count needs",
                    metadata.len()
                ))
            })?;
        let version = match metadata.len() {
            8 => None,
            16 => {
                let v = u64::from_le_bytes(metadata[8..16].try_into().unwrap());
                (v != 0).then_some(v)
            }
            other => {
                return Err(FlightError::protocol(format!(
                    "yesno ingest acknowledgement has {other} bytes, expected 8 or 16"
                )))
            }
        };
        Ok(Ack { rows, version })
    }
}

/// The result of planning a yesno query.
///
/// `total_records` and `ticket.version` describe the same database snapshot.
/// A ticket is not a lease: if the server reclaims that version before
/// [`YesnoClient::fetch`], the returned stream fails with
/// [`tonic::Code::FailedPrecondition`] and the caller must plan again.
#[derive(Clone, Debug)]
pub struct QueryInfo {
    info: FlightInfo,
    ticket: FlightTicket,
    decoded_ticket: Ticket,
    total_records: u64,
}

impl QueryInfo {
    fn try_new(info: FlightInfo) -> Result<Self> {
        let total_records = u64::try_from(info.total_records).map_err(|_| {
            FlightError::protocol(format!(
                "yesno returned a negative total_records value ({})",
                info.total_records
            ))
        })?;
        let [endpoint] = info.endpoint.as_slice() else {
            return Err(FlightError::protocol(format!(
                "yesno returned {} endpoints instead of 1",
                info.endpoint.len()
            )));
        };
        let ticket = endpoint
            .ticket
            .clone()
            .ok_or_else(|| FlightError::protocol("yesno returned no query ticket"))?;
        let decoded_ticket = Ticket::decode(&ticket.ticket)
            .ok_or_else(|| FlightError::protocol("yesno returned a malformed query ticket"))?;
        Ok(Self {
            info,
            ticket,
            decoded_ticket,
            total_records,
        })
    }

    /// The exact number of ordinals in this result.
    pub fn total_records(&self) -> u64 {
        self.total_records
    }

    /// The database version shared by the count and eventual row stream.
    pub fn version(&self) -> u64 {
        self.decoded_ticket.version
    }

    /// The decoded yesno ticket, including its key and prefix bounds.
    pub fn ticket(&self) -> &Ticket {
        &self.decoded_ticket
    }

    /// The opaque Flight ticket bytes, for storage by transaction adapters.
    pub fn ticket_bytes(&self) -> &[u8] {
        self.ticket.ticket.as_ref()
    }

    /// The complete Flight metadata returned by the server.
    pub fn flight_info(&self) -> &FlightInfo {
        &self.info
    }

    /// Consume this value and return its complete Flight metadata.
    pub fn into_flight_info(self) -> FlightInfo {
        self.info
    }
}

/// A query's exact metadata and decoded stream of Arrow record batches.
///
/// This implements [`Stream`] directly. Call [`Self::collect_ordinals`] only
/// when materializing the complete result is appropriate.
#[derive(Debug)]
pub struct QueryStream {
    info: QueryInfo,
    batches: FlightRecordBatchStream,
}

impl QueryStream {
    /// The metadata and versioned ticket that produced this stream.
    pub fn info(&self) -> &QueryInfo {
        &self.info
    }

    /// Consume the stream and return its metadata and raw batch stream.
    pub fn into_parts(self) -> (QueryInfo, FlightRecordBatchStream) {
        (self.info, self.batches)
    }

    /// Materialize every ordinal in this result.
    pub async fn collect_ordinals(mut self) -> Result<Vec<u64>> {
        let capacity = usize::try_from(self.info.total_records()).unwrap_or(0);
        let mut ordinals = Vec::with_capacity(capacity);
        while let Some(batch) = self.batches.try_next().await? {
            let column = batch
                .column_by_name("ordinal")
                .and_then(|column| column.as_any().downcast_ref::<UInt64Array>())
                .ok_or_else(|| {
                    FlightError::protocol("yesno returned no UInt64 `ordinal` column")
                })?;
            if column.null_count() != 0 {
                return Err(FlightError::protocol(
                    "yesno returned nulls in its non-null `ordinal` column",
                ));
            }
            ordinals.extend(column.values().iter().copied());
        }
        let received = u64::try_from(ordinals.len())
            .map_err(|_| FlightError::protocol("ordinal result length does not fit in u64"))?;
        if received != self.info.total_records() {
            return Err(FlightError::protocol(format!(
                "yesno returned {received} ordinals after promising {}",
                self.info.total_records()
            )));
        }
        Ok(ordinals)
    }
}

impl Stream for QueryStream {
    type Item = Result<RecordBatch>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.batches).poll_next(cx)
    }
}

/// A yesno-shaped convenience layer over [`FlightClient`].
#[derive(Debug)]
pub struct YesnoClient<T = Channel> {
    flight: FlightClient<T>,
}

impl YesnoClient<Channel> {
    /// Connect to a Flight endpoint using tonic's default channel settings.
    ///
    /// For TLS or other channel customization, build a [`Channel`] first and
    /// pass it to [`YesnoClient::new`].
    pub async fn connect<D>(dst: D) -> std::result::Result<Self, tonic::transport::Error>
    where
        D: TryInto<Endpoint>,
        D::Error: Into<StdError>,
    {
        let channel = Endpoint::new(dst)?.connect().await?;
        Ok(Self::new(channel))
    }
}

impl<T> YesnoClient<T> {
    /// Wrap an existing Apache Flight client.
    pub fn new_from_inner(flight: FlightClient<T>) -> Self {
        Self { flight }
    }

    /// Access the underlying Apache Flight client.
    pub fn inner(&self) -> &FlightClient<T> {
        &self.flight
    }

    /// Mutably access the underlying client, for example to add auth metadata.
    pub fn inner_mut(&mut self) -> &mut FlightClient<T> {
        &mut self.flight
    }

    /// Consume this wrapper and return the underlying Apache Flight client.
    pub fn into_inner(self) -> FlightClient<T> {
        self.flight
    }
}

impl<T> YesnoClient<T>
where
    T: tonic::client::GrpcService<tonic::body::Body>,
    T::Error: Into<StdError>,
    T::ResponseBody: Body<Data = Bytes> + Send + 'static,
    <T::ResponseBody as Body>::Error: Into<StdError> + Send,
{
    /// Build a yesno client from an existing gRPC transport.
    pub fn new(transport: T) -> Self {
        Self::new_from_inner(FlightClient::new(transport))
    }

    /// Return every populated key in ascending order.
    pub async fn keys(&mut self) -> Result<Vec<u64>> {
        let mut flights = self.flight.list_flights(Vec::new()).await?;
        let mut keys = Vec::new();
        while let Some(info) = flights.try_next().await? {
            let descriptor = info
                .flight_descriptor
                .ok_or_else(|| FlightError::protocol("yesno key listing returned no descriptor"))?;
            let bytes: [u8; 8] = descriptor.cmd.as_ref().try_into().map_err(|_| {
                FlightError::protocol(format!(
                    "yesno key descriptor has {} bytes instead of 8",
                    descriptor.cmd.len()
                ))
            })?;
            keys.push(u64::from_le_bytes(bytes));
        }
        Ok(keys)
    }

    /// Plan a key lookup without fetching any ordinals.
    pub async fn prepare_key(&mut self, key: u64) -> Result<QueryInfo> {
        self.prepare(FlightDescriptor::new_cmd(key.to_le_bytes().to_vec()))
            .await
    }

    /// Plan an expression without fetching any ordinals.
    pub async fn prepare_query(&mut self, expression: &SetExpr) -> Result<QueryInfo> {
        self.prepare(FlightDescriptor::new_cmd(expression.encode()))
            .await
    }

    /// Plan an expression at a caller-selected database version.
    ///
    /// This is strict: a reclaimed or unknown version is returned as a server
    /// error rather than replaced with the current snapshot.
    ///
    /// Strict, but no longer impatient. The server waits briefly for a version
    /// that is merely *not visible yet* — the window a commit's own version sits
    /// in while an earlier commit finishes its fsync — and only then refuses. So
    /// [`Ack::version`] can be passed here directly without the caller having to
    /// retry a `VersionNotVisible` that was only ever about timing. The version
    /// is still honoured **exactly**; the wait does not silently read something
    /// newer.
    pub async fn prepare_query_at(
        &mut self,
        expression: &SetExpr,
        version: u64,
    ) -> Result<QueryInfo> {
        self.prepare_command(&QueryRequest::at(expression.clone(), version).encode())
            .await
    }

    /// Plan an already-encoded yesno descriptor command.
    ///
    /// This is intended for adapters that persist descriptors opaquely, such as
    /// the PostgreSQL FDW. Prefer [`Self::prepare_key`] or
    /// [`Self::prepare_query`] when constructing a new request.
    pub async fn prepare_command(&mut self, command: &[u8]) -> Result<QueryInfo> {
        self.prepare(FlightDescriptor::new_cmd(command.to_vec()))
            .await
    }

    /// Return a key's exact cardinality without fetching any ordinals.
    pub async fn cardinality(&mut self, key: u64) -> Result<u64> {
        Ok(self.prepare_key(key).await?.total_records())
    }

    /// Return an expression's exact cardinality without fetching any ordinals.
    pub async fn query_cardinality(&mut self, expression: &SetExpr) -> Result<u64> {
        Ok(self.prepare_query(expression).await?.total_records())
    }

    /// Fetch a previously planned query at the version named by its ticket.
    pub async fn fetch(&mut self, query: &QueryInfo) -> Result<FlightRecordBatchStream> {
        self.flight.do_get(query.ticket.clone()).await
    }

    /// Fetch opaque ticket bytes previously returned by this yesno server.
    pub async fn fetch_ticket(
        &mut self,
        ticket: impl Into<Bytes>,
    ) -> Result<FlightRecordBatchStream> {
        self.flight.do_get(FlightTicket::new(ticket)).await
    }

    /// Plan and fetch one key.
    pub async fn get(&mut self, key: u64) -> Result<QueryStream> {
        let info = self.prepare_key(key).await?;
        self.fetch_info(info).await
    }

    /// Plan and fetch one Boolean expression.
    pub async fn query(&mut self, expression: &SetExpr) -> Result<QueryStream> {
        let info = self.prepare_query(expression).await?;
        self.fetch_info(info).await
    }

    /// Insert `(key, ordinal)` pairs, returning the number acknowledged.
    pub async fn insert(&mut self, pairs: impl IntoIterator<Item = (u64, u64)>) -> Result<u64> {
        self.insert_acked(pairs).await.map(|ack| ack.rows)
    }

    /// Remove `(key, ordinal)` pairs, returning the number acknowledged.
    pub async fn remove(&mut self, pairs: impl IntoIterator<Item = (u64, u64)>) -> Result<u64> {
        self.remove_acked(pairs).await.map(|ack| ack.rows)
    }

    /// [`Self::insert`], reporting the **commit version** alongside the count.
    ///
    /// This is the read-your-writes primitive on this surface: pass
    /// [`Ack::version`] to [`Self::prepare_query_at`] and the read is bound to a
    /// database state that contains the write. Without it a client can only
    /// query "now", and "now" on this surface is whatever the watermark had
    /// reached when the request arrived — which is not necessarily its own
    /// commit, because the watermark advances over a consecutive prefix.
    pub async fn insert_acked(
        &mut self,
        pairs: impl IntoIterator<Item = (u64, u64)>,
    ) -> Result<Ack> {
        self.put(pairs, PUT_INSERT).await
    }

    /// [`Self::remove`], reporting the commit version alongside the count.
    pub async fn remove_acked(
        &mut self,
        pairs: impl IntoIterator<Item = (u64, u64)>,
    ) -> Result<Ack> {
        self.put(pairs, PUT_REMOVE).await
    }

    /// Insert all pairs in one Arrow batch and one server-side commit.
    ///
    /// Transaction adapters use this to preserve their flush boundary. General
    /// callers should prefer [`Self::insert`], which bounds batch memory.
    pub async fn insert_batch(
        &mut self,
        pairs: impl IntoIterator<Item = (u64, u64)>,
    ) -> Result<u64> {
        self.insert_batch_acked(pairs).await.map(|ack| ack.rows)
    }

    /// [`Self::insert_batch`], reporting the commit version alongside the count.
    ///
    /// This is the one whose version is unambiguous: one batch is one commit,
    /// so the version names exactly this call's write. The streaming forms commit
    /// per batch and report the **last** version, which is a state containing
    /// every row sent but not a single atomic instant for them.
    pub async fn insert_batch_acked(
        &mut self,
        pairs: impl IntoIterator<Item = (u64, u64)>,
    ) -> Result<Ack> {
        self.put_one_batch(pairs, PUT_INSERT).await
    }

    /// Remove all pairs in one Arrow batch and one server-side commit.
    ///
    /// Transaction adapters use this to preserve their flush boundary. General
    /// callers should prefer [`Self::remove`], which bounds batch memory.
    pub async fn remove_batch(
        &mut self,
        pairs: impl IntoIterator<Item = (u64, u64)>,
    ) -> Result<u64> {
        self.remove_batch_acked(pairs).await.map(|ack| ack.rows)
    }

    /// [`Self::remove_batch`], reporting the commit version alongside the count.
    pub async fn remove_batch_acked(
        &mut self,
        pairs: impl IntoIterator<Item = (u64, u64)>,
    ) -> Result<Ack> {
        self.put_one_batch(pairs, PUT_REMOVE).await
    }

    /// Return the server's typed space and reader counters.
    pub async fn stats(&mut self) -> Result<ServerStats> {
        ServerStats::decode_protobuf(self.action("stats", Vec::new()).await?).map_err(|error| {
            FlightError::protocol(format!("yesno stats returned invalid protobuf: {error}"))
        })
    }

    /// Whether the server advertises every bit in `features`.
    ///
    /// **Check this before sending a mixed batch or a transaction to a server
    /// you did not start.** A server older than these features treats an
    /// unrecognised `do_put` command as *insert*, so a mixed batch's removals
    /// would be applied as insertions with no error anywhere. The bit is
    /// absent from an old server's `ServerStats` and protobuf decodes that as
    /// zero, which is the right answer.
    pub async fn supports(&mut self, features: u64) -> Result<bool> {
        Ok(self.stats().await?.features & features == features)
    }

    /// Open a write transaction.
    ///
    /// Stage with [`Self::stage`] and publish with [`Self::commit_write`].
    /// Nothing staged is visible to any reader until the commit, which
    /// publishes all of it at one version.
    pub async fn begin_write(&mut self) -> Result<WriteTxn> {
        Ok(WriteTxn(
            self.u64_action(ACTION_BEGIN_WRITE, Vec::new()).await?,
        ))
    }

    /// Append operations to an open transaction, returning the rows staged.
    ///
    /// Operations take effect in the order they are staged, across calls, so
    /// `DeleteKey` followed by inserts means replacement. **One stream at a
    /// time per transaction**: the server refuses a concurrent one rather than
    /// interleave two streams into an order the caller cannot predict.
    pub async fn stage(
        &mut self,
        txn: WriteTxn,
        mutations: impl IntoIterator<Item = Mutation>,
    ) -> Result<u64> {
        let mut command = PUT_TXN_PREFIX.to_vec();
        command.extend_from_slice(&txn.0.to_le_bytes());
        Ok(self.put_mutations(mutations, &command).await?.rows)
    }

    /// Publish a transaction, returning the one version its work became
    /// visible at.
    ///
    /// **Idempotent per handle.** A retry after a lost response returns the
    /// original version rather than applying the work twice, which is what
    /// lets a change-data-capture pipe resume from an ambiguous failure.
    pub async fn commit_write(&mut self, txn: WriteTxn) -> Result<u64> {
        self.u64_action(ACTION_COMMIT_WRITE, txn.0.to_le_bytes().to_vec())
            .await
    }

    /// Discard a transaction's staged work.
    ///
    /// Aborting one that already committed is an error: a version exists that
    /// says otherwise. Aborting one already gone -- aborted before, or expired
    /// -- succeeds, because the caller's intent already holds.
    pub async fn abort_write(&mut self, txn: WriteTxn) -> Result<()> {
        self.u64_action(ACTION_ABORT_WRITE, txn.0.to_le_bytes().to_vec())
            .await
            .map(|_| ())
    }

    /// Apply mixed operations as **one** commit, without a transaction.
    ///
    /// The one-shot form: everything fits in one request, so there is no
    /// handle to begin, abort or expire. Use [`Self::begin_write`] when the
    /// work spans several requests.
    pub async fn apply(&mut self, mutations: impl IntoIterator<Item = Mutation>) -> Result<Ack> {
        self.put_mutations(mutations, PUT_APPLY).await
    }

    async fn put_mutations(
        &mut self,
        mutations: impl IntoIterator<Item = Mutation>,
        command: &[u8],
    ) -> Result<Ack> {
        let (mut keys, mut los, mut his, mut ops) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for m in mutations {
            let (key, lo, hi, op) = m.columns();
            keys.push(key);
            los.push(lo);
            his.push(hi);
            ops.push(op);
        }
        let sent = u64::try_from(keys.len())
            .map_err(|_| FlightError::protocol("mutation count does not fit in u64"))?;
        let batches = if keys.is_empty() {
            Vec::new()
        } else {
            vec![RecordBatch::try_new(
                mutations_schema(),
                vec![
                    Arc::new(UInt64Array::new(keys.into(), None)),
                    Arc::new(UInt64Array::new(los.into(), None)),
                    Arc::new(UInt64Array::new(his.into(), None)),
                    Arc::new(arrow_array::UInt8Array::new(ops.into(), None)),
                ],
            )
            .map_err(FlightError::from)?]
        };
        self.put_batches_with(batches, sent, command, mutations_schema())
            .await
    }

    /// Atomically remove every ordinal under `key`, returning its commit version.
    pub async fn clear(&mut self, key: u64) -> Result<u64> {
        self.u64_action(ACTION_CLEAR, key.to_le_bytes().to_vec())
            .await
    }

    /// Test one `(key, ordinal)` pair without fetching the key's posting list.
    pub async fn contains(&mut self, key: u64, ordinal: u64) -> Result<bool> {
        let mut body = Vec::with_capacity(16);
        body.extend_from_slice(&key.to_le_bytes());
        body.extend_from_slice(&ordinal.to_le_bytes());
        match self.u64_action(ACTION_CONTAINS, body).await? {
            0 => Ok(false),
            1 => Ok(true),
            value => Err(FlightError::protocol(format!(
                "yesno contains returned {value} instead of zero or one"
            ))),
        }
    }

    /// Atomically insert one pair, returning whether the set changed.
    pub async fn insert_one(&mut self, key: u64, ordinal: u64) -> Result<bool> {
        self.bool_pair_action(ACTION_INSERT_ONE, key, ordinal).await
    }

    /// Atomically remove one pair, returning whether the set changed.
    pub async fn remove_one(&mut self, key: u64, ordinal: u64) -> Result<bool> {
        self.bool_pair_action(ACTION_REMOVE_ONE, key, ordinal).await
    }

    async fn prepare(&mut self, descriptor: FlightDescriptor) -> Result<QueryInfo> {
        QueryInfo::try_new(self.flight.get_flight_info(descriptor).await?)
    }

    async fn fetch_info(&mut self, info: QueryInfo) -> Result<QueryStream> {
        let batches = self.fetch(&info).await?;
        Ok(QueryStream { info, batches })
    }

    async fn put(
        &mut self,
        pairs: impl IntoIterator<Item = (u64, u64)>,
        command: &[u8],
    ) -> Result<Ack> {
        let mut batches = Vec::new();
        let mut keys = Vec::with_capacity(BATCH_ROWS);
        let mut ordinals = Vec::with_capacity(BATCH_ROWS);
        let mut sent = 0u64;

        for (key, ordinal) in pairs {
            keys.push(key);
            ordinals.push(ordinal);
            sent += 1;
            if keys.len() == BATCH_ROWS {
                batches.push(pair_batch(&mut keys, &mut ordinals)?);
            }
        }
        if !keys.is_empty() {
            batches.push(pair_batch(&mut keys, &mut ordinals)?);
        }

        self.put_batches(batches, sent, command).await
    }

    async fn put_one_batch(
        &mut self,
        pairs: impl IntoIterator<Item = (u64, u64)>,
        command: &[u8],
    ) -> Result<Ack> {
        let (keys, ordinals): (Vec<_>, Vec<_>) = pairs.into_iter().unzip();
        let sent = u64::try_from(keys.len())
            .map_err(|_| FlightError::protocol("pair count does not fit in u64"))?;
        let batches = if keys.is_empty() {
            Vec::new()
        } else {
            vec![RecordBatch::try_new(
                pairs_schema(),
                vec![
                    Arc::new(UInt64Array::new(keys.into(), None)),
                    Arc::new(UInt64Array::new(ordinals.into(), None)),
                ],
            )
            .map_err(FlightError::from)?]
        };
        self.put_batches(batches, sent, command).await
    }

    async fn put_batches(
        &mut self,
        batches: Vec<RecordBatch>,
        sent: u64,
        command: &[u8],
    ) -> Result<Ack> {
        self.put_batches_with(batches, sent, command, pairs_schema())
            .await
    }

    async fn put_batches_with(
        &mut self,
        batches: Vec<RecordBatch>,
        sent: u64,
        command: &[u8],
        schema: arrow_schema::SchemaRef,
    ) -> Result<Ack> {
        let descriptor = FlightDescriptor::new_cmd(command.to_vec());
        let input = FlightDataEncoderBuilder::new()
            .with_schema(schema)
            .with_flight_descriptor(Some(descriptor))
            .build(futures::stream::iter(batches.into_iter().map(Ok)));
        let mut acknowledgements = self.flight.do_put(input).await?;
        let mut acknowledged = None;
        while let Some(result) = acknowledgements.try_next().await? {
            acknowledged = Some(Ack::decode(result.app_metadata.as_ref())?);
        }
        let acknowledged = acknowledged
            .ok_or_else(|| FlightError::protocol("yesno returned no ingest acknowledgement"))?;
        if acknowledged.rows != sent {
            return Err(FlightError::protocol(format!(
                "yesno acknowledged {} of {sent} ingest pairs",
                acknowledged.rows
            )));
        }
        Ok(acknowledged)
    }

    async fn u64_action(&mut self, name: &str, request: Vec<u8>) -> Result<u64> {
        let body = self.action(name, request).await?;
        let bytes: [u8; 8] = body.try_into().map_err(|body: Vec<u8>| {
            FlightError::protocol(format!(
                "yesno {name} returned {} bytes instead of 8",
                body.len()
            ))
        })?;
        Ok(u64::from_le_bytes(bytes))
    }

    async fn bool_pair_action(&mut self, name: &str, key: u64, ordinal: u64) -> Result<bool> {
        let mut body = Vec::with_capacity(16);
        body.extend_from_slice(&key.to_le_bytes());
        body.extend_from_slice(&ordinal.to_le_bytes());
        match self.u64_action(name, body).await? {
            0 => Ok(false),
            1 => Ok(true),
            value => Err(FlightError::protocol(format!(
                "yesno {name} returned {value} instead of zero or one"
            ))),
        }
    }

    async fn action(&mut self, name: &str, request: Vec<u8>) -> Result<Vec<u8>> {
        let mut chunks = self.flight.do_action(Action::new(name, request)).await?;
        let mut body = Vec::new();
        while let Some(chunk) = chunks.try_next().await? {
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }
}

fn pair_batch(keys: &mut Vec<u64>, ordinals: &mut Vec<u64>) -> Result<RecordBatch> {
    let keys = std::mem::replace(keys, Vec::with_capacity(BATCH_ROWS));
    let ordinals = std::mem::replace(ordinals, Vec::with_capacity(BATCH_ROWS));
    RecordBatch::try_new(
        pairs_schema(),
        vec![
            Arc::new(UInt64Array::new(keys.into(), None)),
            Arc::new(UInt64Array::new(ordinals.into(), None)),
        ],
    )
    .map_err(FlightError::from)
}

#[cfg(test)]
mod tests {
    use super::Ack;

    /// The 8-byte case is the one that matters and is easy to forget: a client
    /// built after 2026-09-12 talking to a server built before it. It must lose
    /// the version, not the call.
    #[test]
    fn an_acknowledgement_decodes_both_the_old_and_the_new_width() {
        let old = 5_000u64.to_le_bytes().to_vec();
        assert_eq!(
            Ack::decode(&old).unwrap(),
            Ack {
                rows: 5_000,
                version: None
            },
            "an 8-byte acknowledgement is an older server, not an error"
        );

        let mut new = 5_000u64.to_le_bytes().to_vec();
        new.extend_from_slice(&42u64.to_le_bytes());
        assert_eq!(
            Ack::decode(&new).unwrap(),
            Ack {
                rows: 5_000,
                version: Some(42)
            }
        );
    }

    /// Version 0 is the empty database and no commit is ever assigned it, so a
    /// zero in the version slot means "committed nothing" and must not be handed
    /// to a caller as a version it can read at — `prepare_query_at( .., 0 )`
    /// would be a different question entirely.
    #[test]
    fn a_zero_version_is_reported_as_absent_rather_than_as_version_zero() {
        let mut empty = 0u64.to_le_bytes().to_vec();
        empty.extend_from_slice(&0u64.to_le_bytes());
        assert_eq!(
            Ack::decode(&empty).unwrap(),
            Ack {
                rows: 0,
                version: None
            }
        );
    }

    /// Widths that are neither are a protocol error, not a silent truncation.
    /// Including 24: a future server that widens this again must be *detected*
    /// by an older client rather than have its extra field ignored.
    #[test]
    fn any_other_width_is_refused() {
        for len in [0usize, 4, 9, 15, 24] {
            let bytes = vec![0u8; len];
            assert!(
                Ack::decode(&bytes).is_err(),
                "a {len}-byte acknowledgement must be refused"
            );
        }
    }
}
