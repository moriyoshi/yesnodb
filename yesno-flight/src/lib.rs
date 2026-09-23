//! Arrow Flight for yesno **query results**, not for the WAL.
//!
//! The split is deliberate and the two services reached it from opposite ends.
//! WAL shipping is `yesno-server::replication` over plain tonic, because a log is an
//! opaque self-framed binary stream and wrapping it in Flight buys nothing. Here
//! the payload genuinely is columnar, so Flight is the right answer: a client
//! gets `RecordBatch`es over a standard protocol with no bespoke decoder.
//!
//! # Two things this server does that most Flight servers do not
//!
//! 1. **`FlightInfo.total_records` is exact.** For keys, ordinary Boolean
//!    expressions, and top-level view selection, the count comes from indexed
//!    container summaries without building the result. View folds and expansion
//!    remain eager transform boundaries and are counted after evaluation.
//! 2. **The ticket carries the snapshot version.** That is what makes a future
//!    multi-endpoint fetch *consistent* rather than merely parallel. See
//!    [`ticket`].
//!
//! # Flight SQL is cut, not deferred
//!
//! yesno is not SQL. Anyone wanting SQL over the wire runs DataFusion's
//! `FlightSqlService` on top of `yesno-datafusion`'s table provider, which is a
//! composition rather than a reimplementation. `do_exchange` is cut for the same
//! reason: there is no bidirectional use case that `do_get` plus `do_put` does
//! not already cover.
//!
//! # Page faults must not run on the reactor
//!
//! A yesno read can take a major page fault on the mmap, parking a reactor
//! thread for 100 us to 10 ms. Every read here goes through `spawn_blocking`
//! onto a bounded channel, which also supplies backpressure.

use std::sync::Arc;

#[cfg(feature = "server")]
use arrow_array::{RecordBatch, UInt64Array};
#[cfg(feature = "server")]
use arrow_flight::encode::FlightDataEncoderBuilder;
#[cfg(feature = "server")]
use arrow_flight::flight_service_server::FlightService;
#[cfg(feature = "server")]
use arrow_flight::{
    Action, ActionType, Criteria, Empty, FlightData, FlightDescriptor, FlightEndpoint, FlightInfo,
    HandshakeRequest, HandshakeResponse, PollInfo, PutResult, SchemaAsIpc, SchemaResult,
    Ticket as FlightTicket,
};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
#[cfg(feature = "server")]
use futures::{stream::BoxStream, StreamExt, TryStreamExt};
#[cfg(feature = "server")]
use tonic::{Request, Response, Status, Streaming};
#[cfg(feature = "server")]
use yesno_core::{CodecError, Db};

/// Map an engine error to a status a client can act on.
///
/// **`internal` is the wrong answer for a refusal.** A client that writes to a
/// read-only replica by mistake and is told "internal error" will retry, page
/// somebody, and look for a bug in the server — when what it needs to do is send
/// the write to the leader. `FailedPrecondition` is gRPC's "the system is not in
/// a state required for this operation", which is exactly the situation, and it
/// is the same code the leadership-term fence answers with for the same reason.
#[cfg(feature = "server")]
fn engine_status(e: CodecError) -> Status {
    match e {
        CodecError::ReadOnlyReplica => Status::failed_precondition(
            "this node is a read-only replica; send writes to its leader",
        ),
        // `failed_precondition`, and the gRPC spec's own gloss is the reason:
        // "the client should not retry until the system state has been
        // explicitly fixed". Fixing it here means asking for a new ticket. A
        // `unavailable` or `internal` would put the client in a retry loop
        // against a version that only recedes further.
        e @ CodecError::VersionReclaimed { .. } => Status::failed_precondition(format!(
            "{e}. This ticket was minted against a version the server has since \
             collapsed; call GetFlightInfo again."
        )),
        // Not the server's fault and not survivable by retrying: a version
        // this database never assigned. The likeliest cause is a ticket minted
        // against a *different* server — the case a fan-out coordinator creates
        // by construction — so name it rather than reporting a bare error.
        e @ CodecError::VersionNotVisible { .. } => Status::invalid_argument(format!(
            "{e}. A ticket is only valid against the server that minted it."
        )),
        // `aborted`, which gRPC glosses as a concurrency conflict the client
        // should retry at a higher level — and that is exactly right here: the
        // read was cut short to bound space amplification, and retrying at a
        // *newer* snapshot succeeds. The error carries the last key it reached
        // so the retry can resume rather than restart, so it is repeated in the
        // message: a status code cannot carry it and the client cannot
        // reconstruct it.
        e @ CodecError::SnapshotTooOld { .. } => Status::aborted(format!(
            "{e}. Retry at a fresh snapshot; a long scan can resume from the key named here."
        )),
        other => Status::internal(format!("{other:?}")),
    }
}

pub mod client;
#[cfg(feature = "server")]
pub mod expr;
pub mod ticket;

/// What a `FlightDescriptor` resolves to.
///
/// Named with a trailing digit to avoid colliding with `tonic::Request`, which
/// this module already imports.
#[cfg(feature = "server")]
enum Request2 {
    Key(u64),
    Expr(SetExpr, Option<u64>),
}
pub use client::{Ack, QueryInfo, QueryStream, YesnoClient};
pub use ticket::Ticket;
pub use yesno_wire::{
    AnyExpr, BoolExpr, FoldOp, IntExpr, QueryRequest, SetExpr, VecIntExpr, VecSetExpr, ViewLayout,
    ViewSpec,
};

/// Space and reader counters returned by the `stats` Flight action.
///
/// This is the protobuf message defined by `proto/stats.proto`. Field numbers
/// are part of the public Flight wire contract and must never be reused.
#[derive(Clone, Copy, PartialEq, Eq, prost::Message)]
#[non_exhaustive]
pub struct ServerStats {
    #[prost(uint64, tag = "1")]
    pub allocated_bytes: u64,
    #[prost(uint64, tag = "2")]
    pub deferred_bytes: u64,
    #[prost(uint64, tag = "3")]
    pub wal_bytes: u64,
    #[prost(uint64, tag = "4")]
    pub live_readers: u64,
    #[prost(uint64, tag = "5")]
    pub shards: u64,
    /// Capability bits: see [`FEATURE_MIXED_PUT`].
    ///
    /// Tag 6, added after the first release. An older server does not send it
    /// and prost decodes the absence as zero, which is exactly the right
    /// answer: it supports none of the features the bits name.
    #[prost(uint64, tag = "6")]
    pub features: u64,
}

impl ServerStats {
    /// Decode a `ServerStats` protobuf action result.
    pub fn decode_protobuf(bytes: impl AsRef<[u8]>) -> Result<Self, prost::DecodeError> {
        prost::Message::decode(bytes.as_ref())
    }
}
/// S1: a stream of ordinals for one key.
pub fn ordinals_schema() -> SchemaRef {
    // No nulls, structurally: a posting list is a set of *present* values, and
    // absence is a closed-world fact already encoded by the ordinal's absence.
    Arc::new(Schema::new(vec![Field::new(
        "ordinal",
        DataType::UInt64,
        false,
    )]))
}

/// S2: `(key, ordinal)` pairs, the bulk-ingest and export shape.
pub fn pairs_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("key", DataType::UInt64, false),
        Field::new("ordinal", DataType::UInt64, false),
    ]))
}

/// S3: `( key, lo, hi, op )` for a mixed-operation ingest.
///
/// One row is one `WriteBatch` operation, and the operation set is the core's:
/// point insert and remove, **inclusive range** insert and remove, and whole-key
/// delete. `yesno-core`'s `WriteBatch` has always accepted all of these mixed
/// across arbitrary keys in one commit; only the wire could not say so.
///
/// # Why ranges are on the wire rather than expanded by the client
///
/// A range is one WAL record and one container call per chunk. Expanding it
/// into ordinals client-side discards both, which is the difference between
/// clearing a contiguous encoded row and writing 65 536 WAL operations to say
/// the same thing. A surface advertised as *the* remote atomic-batch API has to
/// carry the cheap form.
///
/// # Why three integer columns rather than two
///
/// `lo` and `hi` are the inclusive bounds for range operations. Point
/// operations set `hi == lo`, which is checked rather than ignored: a row with
/// `hi != lo` under [`OP_INSERT`] almost certainly meant a range, and silently
/// dropping `hi` would apply a fraction of what the caller asked for.
/// [`OP_DELETE_KEY`] names a whole key, so both bounds must be zero.
pub fn mutations_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("key", DataType::UInt64, false),
        Field::new("lo", DataType::UInt64, false),
        Field::new("hi", DataType::UInt64, false),
        Field::new("op", DataType::UInt8, false),
    ]))
}

/// Row operations for [`mutations_schema`].
///
/// Numbered explicitly and never reordered: these go on the wire.
pub const OP_INSERT: u8 = 0;
pub const OP_REMOVE: u8 = 1;
pub const OP_INSERT_RANGE: u8 = 2;
pub const OP_REMOVE_RANGE: u8 = 3;
pub const OP_DELETE_KEY: u8 = 4;

/// `do_put` descriptor commands. **One is required.**
///
/// An absent or unrecognised command used to mean insert. That made every
/// future command a trap -- a client sending [`PUT_APPLY`] to a server that did
/// not know it would have had its *removals applied as insertions*, with no
/// error anywhere -- and it made the wire not self-describing: a stream's
/// meaning depended on what the server happened to recognise.
///
/// Both now fail. The two callers that relied on the default, the `yesno put`
/// CLI and the e2e harness, send [`PUT_INSERT`] explicitly instead; the Go and
/// C++ clients always did. Nothing is guessed, so no future command can be
/// mistaken for insert.
pub const PUT_INSERT: &[u8] = b"insert";
pub const PUT_REMOVE: &[u8] = b"remove";

/// Mixed inserts and removals, published as **one** commit.
///
/// Carries [`mutations_schema`]. Every record batch in the stream accumulates
/// into a single `WriteBatch` which commits once at end of stream, so the
/// returned version is the single instant at which every row became visible.
/// This is the difference from [`PUT_INSERT`], which commits per record batch
/// and can only report the last of several versions.
pub const PUT_APPLY: &[u8] = b"apply";

/// Prefix of a `do_put` command naming an open write transaction.
///
/// The full command is this prefix followed by the transaction's eight
/// little-endian bytes. Flight's `DoPut` carries a descriptor rather than a
/// `Ticket`, so the handle has to ride there; putting it in the command keeps
/// it on the *first* message, which is the only one the server inspects before
/// handing the stream to the decoder.
pub const PUT_TXN_PREFIX: &[u8] = b"txn:";

/// Point and key-level actions used by remote storage adapters.
///
/// Payloads are little-endian and fixed width: `clear` carries one key, while
/// the three point operations carry `(key, ordinal)`. Results are encoded as
/// one little-endian `u64`: the committed version for `clear`, and zero or one
/// for the point operations.
pub const ACTION_CLEAR: &str = "clear";
pub const ACTION_CONTAINS: &str = "contains";
pub const ACTION_INSERT_ONE: &str = "insert_one";
pub const ACTION_REMOVE_ONE: &str = "remove_one";

/// Write-transaction actions.
///
/// `begin` returns eight little-endian bytes naming the transaction. `commit`
/// and `abort` take those bytes back; `commit` returns the version every
/// staged row became visible at.
///
/// **`commit` is idempotent within one live service, and only there.** A
/// committed transaction's outcome is remembered for [`COMMIT_MEMORY`]
/// entries, so a *prompt* retry after a lost response returns the original
/// version instead of failing or committing twice.
///
/// That memory is in process memory and bounded. A restart loses every
/// outcome, and later traffic evicts older ones. So this covers a retry
/// against the same running server before eviction, and **not** the case a
/// change-data pipe actually has to survive: losing the response and then
/// finding the server restarted. Such a caller cannot recover the version its
/// transaction committed at, and must not assume replaying under the old
/// handle is safe -- handles do not recur, so the replay fails closed rather
/// than resolving something else, but the original outcome is simply gone.
///
/// Durable idempotency needs a client-supplied identity derived from the
/// source transaction, persisted with the commit, or an equivalent durable
/// outcome query. Neither exists; see `durable-write-transaction-idempotency`
/// in `TODO.md`.
///
/// **What an exclusive writer can do instead**, stated because it is a weaker
/// contract and should not be mistaken for the one above: these operations are
/// replay-idempotent -- inserting a present ordinal or removing an absent one
/// changes nothing -- so replaying a whole source transaction after a
/// `NotFound` converges on the same final set. That recovers *state*, not
/// *identity*: it may publish a second version for one source transaction, and
/// readers may observe the committed state at a version for which the pipe
/// never recorded a source-position mapping. There is no partial application
/// at any point, because the original commit and the replay are each atomic --
/// the exposure is an unrecorded mapping, not a torn state. Sound only for a
/// single writer whose source transactions are replayable in full.
pub const ACTION_BEGIN_WRITE: &str = "begin_write";
pub const ACTION_COMMIT_WRITE: &str = "commit_write";
pub const ACTION_ABORT_WRITE: &str = "abort_write";

/// Capability bits reported by [`ServerStats::features`].
///
/// A server that predates the field reports zero, because protobuf decodes an
/// absent field as its default. **A client must check before sending a new
/// `do_put` command**: an older server treats an unknown command as insert,
/// which turns a mixed batch's removals into insertions with no error
/// anywhere.
pub const FEATURE_MIXED_PUT: u64 = 1 << 0;
pub const FEATURE_WRITE_TRANSACTIONS: u64 = 1 << 1;

/// Everything this build implements.
pub const FEATURES: u64 = FEATURE_MIXED_PUT | FEATURE_WRITE_TRANSACTIONS;

/// Live write transactions a server will hold at once.
pub const MAX_OPEN_TRANSACTIONS: usize = 256;

/// Rows a single write transaction may stage.
///
/// Staged work is held in memory until commit, so this is a real bound rather
/// than a formality. Exceeding it fails the `do_put` that crossed it and
/// leaves the transaction open to be aborted: silently splitting into two
/// commits would break the one guarantee the caller asked for.
pub const MAX_TRANSACTION_ROWS: u64 = 16 * 1024 * 1024;

/// Committed transaction outcomes remembered for idempotent retry.
///
/// In memory, bounded, and lost on restart. This is the whole extent of the
/// idempotency guarantee; see [`ACTION_COMMIT_WRITE`] for what that does and
/// does not cover.
pub const COMMIT_MEMORY: usize = 1024;

/// Rows per `RecordBatch`. 64 KiB of `u64`, which fits L2 and matches
/// DataFusion's default `batch_size`.
const BATCH_ROWS: usize = 8192;

#[cfg(feature = "server")]
#[derive(Clone)]
pub struct YesnoFlightService {
    db: Arc<Db>,
    /// Snapshots held open between `GetFlightInfo` and `DoGet`, by version.
    ///
    /// **A ticket names a version and nothing held it open.** A checkpoint
    /// between the two calls moved the reclamation floor past it and `DoGet`
    /// refused — correctly, rather than answering from a different instant, but
    /// a coordinator fanning one query across endpoints then had to re-mint.
    ///
    /// `Snapshot` clones by **refcounting its registry slot**, so a clone
    /// parked here pins the version for exactly as long as it lives. A lease is
    /// therefore a registered reader with a deadline, and needs no new
    /// retention mechanism.
    leases: Arc<std::sync::Mutex<std::collections::HashMap<u64, TicketLease>>>,
    lease_ttl: std::time::Duration,
    /// How long `GetFlightInfo` will wait for a requested version to become
    /// readable before refusing.
    ///
    /// This exists because `commit` returns a version that is not necessarily
    /// visible yet: the watermark advances over a consecutive prefix, so a
    /// commit that resolves while an earlier one is still in its fsync holds a
    /// version the database will not yet show. A client that reads back its own
    /// write hit that as an intermittent `VersionNotVisible`.
    ///
    /// Deliberately short. The window it closes is **one fsync**; a gap
    /// larger than that is replication lag or a version this database never
    /// assigned, and neither should be hidden behind a server-side block that
    /// holds a `spawn_blocking` thread.
    ///
    /// **A ticket minted by a different leader now costs this wait before it
    /// is rejected**, where it used to be refused instantly — that case is
    /// indistinguishable here from a version about to arrive, which is the whole
    /// reason the wait is bounded and short. A *reclaimed* version is not
    /// affected: it sits **below** the watermark, so the wait returns at once and
    /// `snapshot_at` reports `VersionReclaimed` with no added latency.
    visibility_wait: std::time::Duration,
    /// Open write transactions and the outcomes of recently committed ones.
    writes: Arc<std::sync::Mutex<WriteTransactions>>,
    /// How long an untouched write transaction survives.
    ///
    /// Staged work is memory the client is holding on the server, so an
    /// abandoned transaction has to be reclaimable without the client. It is a
    /// *deadline* rather than a lease renewed by use: a transaction that keeps
    /// streaming still expires, which bounds the worst case at one deadline's
    /// worth of memory per open transaction rather than an unbounded one.
    write_ttl: std::time::Duration,
}

#[cfg(feature = "server")]
struct TicketLease {
    snap: yesno_core::Snapshot,
    expires: std::time::Instant,
}

#[cfg(feature = "server")]
/// What a `do_put` stream is doing, decided from the first descriptor.
#[cfg(feature = "server")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PutMode {
    Insert,
    Remove,
    Apply,
    Txn(u64),
}

#[cfg(feature = "server")]
impl PutMode {
    fn name(self) -> &'static str {
        match self {
            PutMode::Insert => "insert",
            PutMode::Remove => "remove",
            PutMode::Apply => "apply",
            PutMode::Txn(_) => "transaction",
        }
    }
}

/// Decide the mode. Every stream must name one.
///
/// Absent, empty and unrecognised are all errors. Nothing here guesses, which
/// is what stops a command this server does not know from being applied as
/// some other operation the caller did not ask for.
#[cfg(feature = "server")]
fn put_mode(cmd: Option<&[u8]>) -> Result<PutMode, Status> {
    match cmd {
        None | Some([]) => Err(Status::invalid_argument(
            "do_put needs a descriptor command: insert, remove, apply, or a transaction \
             handle. It used to default to insert, which meant a command this server did \
             not recognise silently inserted rows a client meant to remove",
        )),
        Some(c) if c == PUT_INSERT => Ok(PutMode::Insert),
        Some(c) if c == PUT_REMOVE => Ok(PutMode::Remove),
        Some(c) if c == PUT_APPLY => Ok(PutMode::Apply),
        Some(c) if c.starts_with(PUT_TXN_PREFIX) => {
            let rest = &c[PUT_TXN_PREFIX.len()..];
            let bytes: [u8; 8] = rest.try_into().map_err(|_| {
                Status::invalid_argument(
                    "a transaction do_put command is the prefix plus eight little-endian bytes",
                )
            })?;
            Ok(PutMode::Txn(u64::from_le_bytes(bytes)))
        }
        Some(other) => Err(Status::invalid_argument(format!(
            "unknown do_put command {:?}; this server understands insert, remove, apply \
             and a transaction handle. Check ServerStats::features before sending a \
             command an older server would silently treat as insert",
            String::from_utf8_lossy(other)
        ))),
    }
}

/// The `( key, ordinal )` columns of [`pairs_schema`].
#[cfg(feature = "server")]
fn pair_columns(b: &RecordBatch) -> Result<(&UInt64Array, &UInt64Array), Status> {
    let keys = b
        .column_by_name("key")
        .and_then(|c| c.as_any().downcast_ref::<UInt64Array>())
        .ok_or_else(|| Status::invalid_argument("expected a UInt64 `key` column"))?;
    let ords = b
        .column_by_name("ordinal")
        .and_then(|c| c.as_any().downcast_ref::<UInt64Array>())
        .ok_or_else(|| Status::invalid_argument("expected a UInt64 `ordinal` column"))?;
    Ok((keys, ords))
}

/// Record a homogeneous batch, returning the rows staged.
#[cfg(feature = "server")]
fn stage_pairs(
    wb: &mut yesno_core::WriteBatch,
    b: &RecordBatch,
    remove: bool,
) -> Result<u64, Status> {
    let (keys, ords) = pair_columns(b)?;
    for i in 0..b.num_rows() {
        if remove {
            wb.remove(keys.value(i), ords.value(i));
        } else {
            wb.insert(keys.value(i), ords.value(i));
        }
    }
    Ok(b.num_rows() as u64)
}

/// Record a mixed batch carrying [`mutations_schema`].
///
/// **Rows are recorded in arrival order and never grouped by kind.** That is
/// the whole correctness argument for mixing: `WriteBatch` sorts by key through
/// a `( key, arrival index )` pair, so operations on one key keep the order
/// they were recorded in. `delete_key` followed by inserts therefore means
/// replacement, and `remove` followed by `insert` of the same membership means
/// present. Regrouping by operation would still look atomic and would silently
/// invert both.
///
/// An unrecognised `op` is rejected rather than defaulted, for the reason
/// [`put_mode`] gives one level up -- and here it would be worse, because a
/// per-row default turns *part* of a batch into the wrong operation, which is
/// harder to notice than a whole stream going the wrong way.
#[cfg(feature = "server")]
/// One validated row, ready to apply and unable to fail.
///
/// Decoding is separated from application so that a batch is **all or
/// nothing**. It used to apply row by row as it validated, so an invalid row
/// left every earlier row of the same batch already in the resident
/// `WriteBatch` -- the staging call reported failure and the transaction could
/// still be committed with the partial work. A caller was told its write did
/// not happen and could then publish it.
#[cfg(feature = "server")]
enum Staged {
    Insert(u64, u64),
    Remove(u64, u64),
    InsertRange(u64, u64, u64),
    RemoveRange(u64, u64, u64),
    DeleteKey(u64),
}

#[cfg(feature = "server")]
fn apply_staged(wb: &mut yesno_core::WriteBatch, ops: Vec<Staged>) {
    for op in ops {
        match op {
            Staged::Insert(key, ordinal) => {
                wb.insert(key, ordinal);
            }
            Staged::Remove(key, ordinal) => {
                wb.remove(key, ordinal);
            }
            Staged::InsertRange(key, lo, hi) => {
                wb.insert_range(key, lo, hi);
            }
            Staged::RemoveRange(key, lo, hi) => {
                wb.remove_range(key, lo, hi);
            }
            Staged::DeleteKey(key) => {
                wb.delete_key(key);
            }
        }
    }
}

#[cfg(feature = "server")]
fn decode_mutations(b: &RecordBatch) -> Result<Vec<Staged>, Status> {
    let col = |name: &str| -> Result<&UInt64Array, Status> {
        b.column_by_name(name)
            .and_then(|c| c.as_any().downcast_ref::<UInt64Array>())
            .ok_or_else(|| {
                Status::invalid_argument(format!(
                    "expected a UInt64 `{name}` column; a mixed batch uses mutations_schema()"
                ))
            })
    };
    let keys = col("key")?;
    let los = col("lo")?;
    let his = col("hi")?;
    let ops = b
        .column_by_name("op")
        .and_then(|c| c.as_any().downcast_ref::<arrow_array::UInt8Array>())
        .ok_or_else(|| {
            Status::invalid_argument(
                "expected a UInt8 `op` column; a mixed batch uses mutations_schema()",
            )
        })?;

    let mut staged = Vec::with_capacity(b.num_rows());
    for i in 0..b.num_rows() {
        let (key, lo, hi) = (keys.value(i), los.value(i), his.value(i));
        let point = |what: &str| -> Result<(), Status> {
            if hi == lo {
                Ok(())
            } else {
                Err(Status::invalid_argument(format!(
                    "row {i} is a point {what} with lo {lo} and hi {hi}; set hi == lo, \
                     or use the range operation if a range was meant"
                )))
            }
        };
        staged.push(match ops.value(i) {
            OP_INSERT => {
                point("insert")?;
                Staged::Insert(key, lo)
            }
            OP_REMOVE => {
                point("remove")?;
                Staged::Remove(key, lo)
            }
            OP_INSERT_RANGE => {
                range_bounds(i, lo, hi)?;
                Staged::InsertRange(key, lo, hi)
            }
            OP_REMOVE_RANGE => {
                range_bounds(i, lo, hi)?;
                Staged::RemoveRange(key, lo, hi)
            }
            OP_DELETE_KEY => {
                if lo != 0 || hi != 0 {
                    return Err(Status::invalid_argument(format!(
                        "row {i} deletes key {key} but carries bounds {lo}..={hi}; \
                         a whole-key delete names no range"
                    )));
                }
                Staged::DeleteKey(key)
            }
            other => {
                return Err(Status::invalid_argument(format!(
                    "row {i} has op {other}; expected {OP_INSERT} insert, {OP_REMOVE} \
                     remove, {OP_INSERT_RANGE} insert_range, {OP_REMOVE_RANGE} \
                     remove_range, or {OP_DELETE_KEY} delete_key"
                )))
            }
        });
    }
    Ok(staged)
}

/// Inclusive bounds must not be inverted.
///
/// The core would treat `lo > hi` as an empty range and do nothing, which is a
/// silent no-op for what is almost always a transposed pair of arguments.
#[cfg(feature = "server")]
fn range_bounds(row: usize, lo: u64, hi: u64) -> Result<(), Status> {
    if lo > hi {
        return Err(Status::invalid_argument(format!(
            "row {row} has an inverted range {lo}..={hi}; bounds are inclusive and ascending"
        )));
    }
    Ok(())
}

/// One transaction's staged work.
///
/// Holds a live `WriteBatch` rather than a list of tuples, because
/// `WriteBatch` owns a refcounted `Db` handle and is therefore `'static` --
/// accumulating straight into it means commit is one call with no second
/// representation to keep in step.
#[cfg(feature = "server")]
struct PendingWrite {
    batch: yesno_core::WriteBatch,
    rows: u64,
    deadline: std::time::Instant,
    /// Why this transaction may no longer be committed, if it may not.
    ///
    /// **A staging call is atomic per record batch, not per call**, and that
    /// distinction is not one a caller can act on. The clients split a `stage`
    /// into batches of [`BATCH_ROWS`], and each batch is decoded, bound-checked
    /// and applied before the next arrives -- so a failure in a later batch,
    /// whether validation, the row bound, a decode error or the transport
    /// dropping mid-stream, leaves the earlier batches of that same call
    /// already staged. They cannot be withdrawn: the core `WriteBatch` has no
    /// rollback, and buffering a whole call before applying it would mean
    /// holding up to [`MAX_TRANSACTION_ROWS`] rows twice.
    ///
    /// So the transaction fails closed instead. Any staging call that does not
    /// complete cleanly poisons it, `commit_write` refuses a poisoned
    /// transaction, and `abort_write` is the only way out. The alternative --
    /// documenting that a client *should* abort -- puts the correctness of
    /// every caller in a comment, and the same audit that found this also
    /// found a caller could already commit work the server had refused.
    poisoned: Option<String>,
    /// A `do_put` stream is currently appending to this transaction.
    ///
    /// **Concurrent staging is refused rather than interleaved.** Operations on
    /// one key take effect in the order they were recorded, so the order has to
    /// be one the client can predict -- and arrival order across two HTTP/2
    /// streams is not. Refusing is the simplest rule that keeps "stage clear,
    /// then stage the replacement" meaning what it says; the alternative is
    /// per-stream sequence numbers and a commit that rejects gaps.
    staging: bool,
}

/// Clears [`PendingWrite::staging`] however the stream ends.
///
/// A guard rather than a clear at the end of the loop: `do_put` returns early
/// on a decode error, an expired deadline and a row-bound breach, and a leaked
/// flag would make the transaction permanently unstageable while still
/// appearing open.
#[cfg(feature = "server")]
struct StagingGuard {
    writes: Arc<std::sync::Mutex<WriteTransactions>>,
    id: u64,
    /// Set once the stream has been consumed without error.
    ///
    /// Everything else -- a validation failure, the row bound, a decode error,
    /// the client vanishing mid-stream -- leaves this false and poisons the
    /// transaction on drop. Putting it here rather than at each error site is
    /// deliberate: an error path added later is poisoned by default, and the
    /// transport failures have no error site to annotate.
    clean: bool,
}

#[cfg(feature = "server")]
impl StagingGuard {
    fn finished(&mut self) {
        self.clean = true;
    }
}

#[cfg(feature = "server")]
impl Drop for StagingGuard {
    fn drop(&mut self) {
        if let Ok(mut w) = self.writes.lock() {
            if let Some(tx) = w.open.get_mut(&self.id) {
                tx.staging = false;
                if !self.clean && tx.poisoned.is_none() {
                    tx.poisoned = Some(
                        "a staging call failed part-way through; rows from its earlier \
                         record batches are staged and cannot be withdrawn"
                            .into(),
                    );
                }
            }
        }
    }
}

/// Open transactions, plus a bounded memory of committed ones.
#[cfg(feature = "server")]
#[derive(Default)]
struct WriteTransactions {
    open: std::collections::HashMap<u64, PendingWrite>,
    /// `( transaction, version )` for recently committed transactions, oldest
    /// first. Makes `commit` idempotent **within one live service**: a prompt
    /// retry after a lost response finds its own outcome instead of a missing
    /// transaction. See [`COMMIT_MEMORY`] for what this does not promise.
    committed: std::collections::VecDeque<(u64, u64)>,
    counter: u64,
    /// Randomly seeded per service, which is what keeps handles from repeating
    /// across a restart.
    ///
    /// Handles used to be a plain counter from zero. A server restarted over
    /// the same database therefore issued handle 1 again, and a delayed
    /// `commit_write` from *before* the restart would resolve whatever
    /// transaction now held that number -- publishing another caller's staged
    /// work under the retrying caller's identity. An audit reproduced it with
    /// no hostile client: two sequential servers and one stale retry.
    ///
    /// `RandomState` is seeded from the OS per process, so hashing the counter
    /// through it yields handles that are unique within a service and do not
    /// recur after one. A dependency-free source is deliberate -- adding one
    /// here would mean a `crate_universe` repin for eight bytes of entropy.
    ///
    /// Non-reuse is a **correctness** property, separate from the ownership
    /// and fencing this surface still lacks: it makes a stale handle fail
    /// closed rather than succeed against the wrong transaction.
    ids: std::collections::hash_map::RandomState,
}

#[cfg(feature = "server")]
impl WriteTransactions {
    /// A handle no live transaction holds and no remembered outcome names.
    fn mint(&mut self) -> u64 {
        use std::hash::{BuildHasher, Hasher};
        loop {
            self.counter += 1;
            let mut h = self.ids.build_hasher();
            h.write_u64(self.counter);
            let id = h.finish();
            // Zero is reserved so that a client's zeroed handle cannot name a
            // real transaction, and a collision -- astronomically unlikely, but
            // free to exclude -- simply draws again.
            if id != 0
                && !self.open.contains_key(&id)
                && !self.committed.iter().any(|&(t, _)| t == id)
            {
                return id;
            }
        }
    }
}

#[cfg(feature = "server")]
impl YesnoFlightService {
    /// A lease long enough to cross a `GetFlightInfo` / `DoGet` round trip and
    /// short enough that an abandoned one is not a retention leak.
    pub const DEFAULT_TICKET_LEASE: std::time::Duration = std::time::Duration::from_secs(30);

    /// Long enough to cover one fsync, short enough that a version which will
    /// never arrive is reported quickly. See [`Self::with_visibility_wait`].
    pub const DEFAULT_VISIBILITY_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

    /// Default deadline for an open write transaction. See
    /// [`YesnoFlightService::with_write_ttl`].
    pub const DEFAULT_WRITE_TTL: std::time::Duration = std::time::Duration::from_secs(60);

    /// Override how long an open write transaction survives.
    pub fn with_write_ttl(mut self, write_ttl: std::time::Duration) -> Self {
        self.write_ttl = write_ttl;
        self
    }

    /// Take exclusive staging rights on an open transaction.
    fn claim_staging(&self, id: u64) -> Result<StagingGuard, Status> {
        let mut w = self
            .writes
            .lock()
            .map_err(|_| Status::internal("write transaction table poisoned"))?;
        let tx = w
            .open
            .get_mut(&id)
            .ok_or_else(|| Status::not_found(format!("write transaction {id} is not open")))?;
        if std::time::Instant::now() > tx.deadline {
            w.open.remove(&id);
            return Err(Status::deadline_exceeded(format!(
                "write transaction {id} expired"
            )));
        }
        if tx.staging {
            return Err(Status::failed_precondition(format!(
                "write transaction {id} already has a do_put stream; stage one at a time \
                 so the order operations take effect in is the order you sent them"
            )));
        }
        tx.staging = true;
        Ok(StagingGuard {
            writes: Arc::clone(&self.writes),
            id,
            clean: false,
        })
    }

    pub fn new(db: Arc<Db>) -> Self {
        Self::with_ticket_lease(db, Self::DEFAULT_TICKET_LEASE)
    }

    /// `Duration::ZERO` restores the pre-wait behaviour: a requested version
    /// that is not yet visible is refused immediately.
    pub fn with_visibility_wait(mut self, visibility_wait: std::time::Duration) -> Self {
        self.visibility_wait = visibility_wait;
        self
    }

    /// `Duration::ZERO` disables leasing, restoring the pre-lease behaviour
    /// where `DoGet` re-opens by version and may be refused.
    pub fn with_ticket_lease(db: Arc<Db>, lease_ttl: std::time::Duration) -> Self {
        YesnoFlightService {
            db,
            leases: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            lease_ttl,
            visibility_wait: Self::DEFAULT_VISIBILITY_WAIT,
            writes: Arc::new(std::sync::Mutex::new(WriteTransactions::default())),
            write_ttl: Self::DEFAULT_WRITE_TTL,
        }
    }

    /// Park a clone of `snap` so the version it names survives to `DoGet`.
    ///
    /// Sweeps expired leases on the way in, so no background task is needed:
    /// the only thing that creates leases is the only thing that must retire
    /// them. An expired lease is *released*, never an error — under the
    /// observe-only space policy a lease reports through `live_readers` and the
    /// soft space-amplification threshold, and is not itself an intervention.
    fn hold(&self, snap: &yesno_core::Snapshot) {
        if self.lease_ttl.is_zero() {
            return;
        }
        let now = std::time::Instant::now();
        let mut leases = self.leases.lock().unwrap_or_else(|e| e.into_inner());
        leases.retain(|_, lease| lease.expires > now);
        // `checked_add`, because `Instant + Duration` **panics** on overflow and
        // `with_ticket_lease( db, Duration::MAX )` — the obvious spelling of "never
        // expire" — is a plausible argument to a public constructor. Saturating to
        // a year is indistinguishable from never for a lease held in process
        // memory, and it is a value the clock can actually represent.
        let expires = now
            .checked_add(self.lease_ttl)
            .unwrap_or_else(|| now + std::time::Duration::from_secs(365 * 24 * 60 * 60));
        leases.insert(
            snap.version(),
            TicketLease {
                snap: snap.clone(),
                expires,
            },
        );
    }

    /// A still-live lease for `version`, if one was taken and has not expired.
    fn leased(&self, version: u64) -> Option<yesno_core::Snapshot> {
        let now = std::time::Instant::now();
        let leases = self.leases.lock().unwrap_or_else(|e| e.into_inner());
        leases
            .get(&version)
            .filter(|lease| lease.expires > now)
            .map(|lease| lease.snap.clone())
    }

    /// The key a descriptor names, from `cmd` or a single-element path.
    fn key_of(d: &FlightDescriptor) -> Result<u64, Status> {
        if d.cmd.len() == 8 {
            return Ok(u64::from_le_bytes(d.cmd.as_ref().try_into().unwrap()));
        }
        if let [one] = d.path.as_slice() {
            return one
                .parse::<u64>()
                .map_err(|_| Status::invalid_argument("path must be a u64 key"));
        }
        Err(Status::invalid_argument(
            "descriptor must carry an 8-byte LE key in cmd, or one numeric path element",
        ))
    }

    /// What a descriptor is asking for.
    ///
    /// The expression form is checked **first**, and it is distinguishable
    /// from a bare key by length as well as magic — a key is exactly 8 bytes and
    /// its contents are arbitrary, so one can begin with `YSNX` by coincidence.
    /// See `SetExpr::looks_like_expr`.
    fn request_of(d: &FlightDescriptor) -> Result<Request2, Status> {
        if QueryRequest::looks_like_request(&d.cmd) {
            let q = QueryRequest::decode(&d.cmd)
                .map_err(|e| Status::invalid_argument(format!("bad query request: {e}")))?;
            return Ok(Request2::Expr(q.expression, q.version));
        }
        if SetExpr::looks_like_expr(&d.cmd) {
            let e = SetExpr::decode(&d.cmd)
                .map_err(|e| Status::invalid_argument(format!("bad expression: {e}")))?;
            return Ok(Request2::Expr(e, None));
        }
        Self::key_of(d).map(Request2::Key)
    }
}

#[cfg(feature = "server")]
#[tonic::async_trait]
impl FlightService for YesnoFlightService {
    type HandshakeStream = BoxStream<'static, Result<HandshakeResponse, Status>>;
    type ListFlightsStream = BoxStream<'static, Result<FlightInfo, Status>>;
    type DoGetStream = BoxStream<'static, Result<FlightData, Status>>;
    type DoPutStream = BoxStream<'static, Result<PutResult, Status>>;
    type DoExchangeStream = BoxStream<'static, Result<FlightData, Status>>;
    type DoActionStream = BoxStream<'static, Result<arrow_flight::Result, Status>>;
    type ListActionsStream = BoxStream<'static, Result<ActionType, Status>>;

    async fn handshake(
        &self,
        _r: Request<Streaming<HandshakeRequest>>,
    ) -> Result<Response<Self::HandshakeStream>, Status> {
        Err(Status::unimplemented("no authentication handshake in v1"))
    }

    /// One `FlightInfo` per populated key.
    ///
    /// This replaced `Status::unimplemented` with the reason "the key space
    /// is a u64, not an enumerable catalogue". That sentence was true of the
    /// *space* and never of the *contents*: `ChunkKey` packs the key in its high
    /// bits, so the populated subset has always been a walkable B+tree range —
    /// what was missing was `Snapshot::keys`, which the PostgreSQL index access
    /// method's `ambulkdelete` finally required.
    ///
    /// `total_records` is **not** filled in per key. Doing so would mean one
    /// `cardinality` call per key, turning a catalogue listing into a scan of
    /// the whole index; a caller that wants a count asks `get_flight_info` for
    /// the key it cares about. Do not "improve" this by counting eagerly.
    #[tracing::instrument(name = "yesno.flight.list_flights", skip_all, err)]
    async fn list_flights(
        &self,
        _r: Request<Criteria>,
    ) -> Result<Response<Self::ListFlightsStream>, Status> {
        let db = self.db.clone();
        let span = tracing::Span::current();
        // `spawn_blocking`: walking the index can fault the mmap, and parking a
        // reactor thread on a major fault is how an async server stalls
        // invisibly — the same rule `do_get` follows.
        let keys = tokio::task::spawn_blocking(move || {
            span.in_scope(move || -> Result<Vec<(u64, u64)>, Status> {
                let snap = db
                    .snapshot()
                    .map_err(|e| Status::internal(format!("{e:?}")))?;
                let version = snap.version();
                let keys = snap.keys().map_err(|e| Status::internal(e.to_string()))?;
                Ok(keys.into_iter().map(|k| (k, version)).collect())
            })
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))??;

        let schema = ordinals_schema();
        let infos: Vec<Result<FlightInfo, Status>> = keys
            .into_iter()
            .map(|(key, version)| {
                let t = Ticket::whole_key(version, key);
                FlightInfo::new()
                    .try_with_schema(&schema)
                    .map_err(|e| Status::internal(e.to_string()))
                    .map(|i| {
                        i.with_descriptor(FlightDescriptor::new_cmd(key.to_le_bytes().to_vec()))
                            .with_endpoint(
                                FlightEndpoint::new().with_ticket(FlightTicket::new(t.encode())),
                            )
                            .with_ordered(true)
                    })
            })
            .collect();

        Ok(Response::new(futures::stream::iter(infos).boxed()))
    }

    #[tracing::instrument(
        name = "yesno.flight.get_flight_info",
        skip_all,
        fields(query.kind = tracing::field::Empty, query.version = tracing::field::Empty, result.records = tracing::field::Empty),
        err
    )]
    async fn get_flight_info(
        &self,
        r: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        let d = r.into_inner();
        let request = Self::request_of(&d)?;
        let query_kind = match &request {
            Request2::Key(_) => "key",
            Request2::Expr(_, _) => "expression",
        };
        tracing::Span::current().record("query.kind", query_kind);
        let requested_version = match &request {
            Request2::Key(_) => None,
            Request2::Expr(_, version) => *version,
        };
        let snap = match requested_version {
            Some(version) => {
                // `spawn_blocking`, because `wait_visible` sleeps. Running it
                // inline would park a reactor thread for the duration, which is
                // the failure this module's header describes for reads.
                //
                // Still `snapshot_at` and not "something at least this new".
                // The version is honoured **exactly**, because that is what a
                // coordinator fanning one query across endpoints depends on.
                // The wait only removes a refusal that was purely about timing.
                let db = self.db.clone();
                let wait = self.visibility_wait;
                tokio::task::spawn_blocking(move || {
                    if !wait.is_zero() {
                        // A timeout is not itself an error here: `snapshot_at`
                        // is about to produce the *precise* diagnosis, and it
                        // distinguishes "never assigned" from "reclaimed",
                        // which this wait cannot.
                        let _ = db.wait_visible(version, wait);
                    }
                    db.snapshot_at(version)
                })
                .await
                .map_err(|e| Status::internal(e.to_string()))?
            }
            None => self.db.snapshot(),
        }
        .map_err(engine_status)?;
        tracing::Span::current().record("query.version", snap.version());
        // Hold the version open for the `DoGet` this info is minted for.
        self.hold(&snap);

        let (total, t) = match request {
            // Exact, from the index alone: `card_m1` per chunk, no payload
            // touched.
            Request2::Key(key) => {
                let total = snap
                    .cardinality(key)
                    .map_err(|e| Status::internal(e.to_string()))?;
                (total, Ticket::whole_key(snap.version(), key))
            }
            // Exact without materializing an ordinary Boolean result. A top-level
            // view selection likewise uses its dedicated count; folds and expands
            // are explicit eager transform boundaries in the current core API.
            Request2::Expr(e, _) => {
                let total = expr::cardinality(&e, &snap)
                    .map_err(|err| Status::internal(err.to_string()))?;
                let mut keys = Vec::new();
                e.keys(&mut keys);
                // `key` names the primary posting list so a coordinator can
                // route without decoding the expression; the expression remains
                // the authority on what to return.
                let primary = keys.first().copied().unwrap_or(0);
                (total, Ticket::with_expr(snap.version(), primary, e))
            }
        };
        tracing::Span::current().record("result.records", total);

        let info = FlightInfo::new()
            .try_with_schema(&ordinals_schema())
            .map_err(|e| Status::internal(e.to_string()))?
            .with_descriptor(d)
            .with_endpoint(FlightEndpoint::new().with_ticket(FlightTicket::new(t.encode())))
            .with_total_records(total as i64)
            // Bytes are not known without reading, and a guess would be worse
            // than the honest -1 the builder defaults to.
            .with_ordered(true);
        Ok(Response::new(info))
    }

    async fn poll_flight_info(
        &self,
        _r: Request<FlightDescriptor>,
    ) -> Result<Response<PollInfo>, Status> {
        Err(Status::unimplemented("queries here are not long-running"))
    }

    async fn get_schema(
        &self,
        _r: Request<FlightDescriptor>,
    ) -> Result<Response<SchemaResult>, Status> {
        let opts = arrow_flight::IpcMessage::try_from(SchemaAsIpc::new(
            &ordinals_schema(),
            &arrow_ipc::writer::IpcWriteOptions::default(),
        ))
        .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(SchemaResult { schema: opts.0 }))
    }

    #[tracing::instrument(
        name = "yesno.flight.do_get",
        skip_all,
        fields(query.version = tracing::field::Empty, query.expression = tracing::field::Empty),
        err
    )]
    async fn do_get(
        &self,
        r: Request<FlightTicket>,
    ) -> Result<Response<Self::DoGetStream>, Status> {
        let raw = r.into_inner().ticket;
        let t = Ticket::decode(&raw).ok_or_else(|| Status::invalid_argument("malformed ticket"))?;
        let db = self.db.clone();
        // Resolved here, not inside the worker: the closure is `move` and
        // owns `db` alone, and the lease map belongs to the service.
        let leased = self.leased(t.version);
        tracing::Span::current().record("query.version", t.version);
        tracing::Span::current().record("query.expression", t.expr.is_some());

        let (tx, rx) = tokio::sync::mpsc::channel::<Result<RecordBatch, Status>>(4);
        let worker_span = tracing::info_span!(
            "yesno.flight.do_get.read",
            query.version = t.version,
            query.expression = t.expr.is_some(),
        );
        // `spawn_blocking`: reading may fault the mmap, and parking a reactor
        // thread on a major fault is how an async server stalls invisibly.
        tokio::task::spawn_blocking(move || {
            worker_span.in_scope(move || {
                // At **the ticket's** version, not at whatever is current.
                //
                // This is the field's entire purpose, and until 2026-08-29 nothing
                // read it: `do_get` opened a fresh snapshot, so a client that called
                // `get_flight_info` and then `do_get` across a write was handed a
                // row count and then a different number of rows. Worse for the case
                // the field exists for — a coordinator fanning one query across N
                // endpoints — each endpoint answered from its own instant and the
                // union was a set that never existed.
                //
                // Version `0` means "whatever is current" and is not a ticket this
                // service mints; it is what a hand-built or truncated ticket
                // carries, and honouring it as a version would refuse every such
                // caller with `VersionReclaimed` the moment the database checkpoints.
                let snap = match t.version {
                    0 => db.snapshot(),
                    // The lease first: it is the same snapshot `GetFlightInfo`
                    // answered from, so a checkpoint in between cannot refuse it.
                    // Falling back to `snapshot_at` is deliberate rather than
                    // an error — a ticket may outlive its lease, or be minted by
                    // another process, and refusing those would be a regression.
                    v => match leased {
                        Some(snap) => Ok(snap),
                        None => db.snapshot_at(v),
                    },
                };
                let snap = match snap {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!(error = %e, "Flight read could not open its snapshot");
                        let _ = tx.blocking_send(Err(engine_status(e)));
                        return;
                    }
                };
                // Same shape as the `snapshot()` match above: this closure returns
                // `()`, so a failed read is reported on the channel rather than
                // propagated. A snapshot evicted under `AbortOldestReader` lands
                // here.
                // When the ticket carries a filter it is the authority, not
                // `t.key`. Falling back to the bare key here would return a
                // **superset** — every row the client asked to exclude — and
                // nothing downstream would notice.
                let loaded = match &t.expr {
                    None => snap.load(t.key),
                    Some(e) => expr::lower(e, &snap).and_then(|lowered| lowered.collect_set()),
                };
                let set = match loaded {
                    Ok(s) => s,
                    // Through `engine_status`, not `Status::internal(format!(…))`.
                    // A snapshot evicted under `AbortOldestReader` lands here, and
                    // it is the one read failure a client can genuinely recover from
                    // — the error even carries the last key it reached so a long
                    // scan can resume. Reporting it as an internal fault throws that
                    // away.
                    Err(e) => {
                        tracing::warn!(error = %e, "Flight read evaluation failed");
                        let _ = tx.blocking_send(Err(engine_status(e)));
                        return;
                    }
                };
                let schema = ordinals_schema();
                let mut buf: Vec<u64> = Vec::with_capacity(BATCH_ROWS);
                let mut rows = 0u64;
                let mut batches = 0u64;
                for o in set.iter() {
                    let prefix = o >> 16;
                    if prefix < t.prefix_lo || prefix >= t.prefix_hi {
                        continue;
                    }
                    buf.push(o);
                    rows += 1;
                    if buf.len() == BATCH_ROWS {
                        let b = std::mem::replace(&mut buf, Vec::with_capacity(BATCH_ROWS));
                        // `UInt64Array::new(.., None)` and not `from(vec)`: the
                        // latter builds a validity buffer this schema forbids.
                        let arr = UInt64Array::new(b.into(), None);
                        let rb = RecordBatch::try_new(schema.clone(), vec![Arc::new(arr)]).unwrap();
                        if tx.blocking_send(Ok(rb)).is_err() {
                            tracing::debug!(rows, batches, "Flight read receiver closed early");
                            return;
                        }
                        batches += 1;
                    }
                }
                if !buf.is_empty() {
                    let arr = UInt64Array::new(buf.into(), None);
                    let rb = RecordBatch::try_new(schema, vec![Arc::new(arr)]).unwrap();
                    if tx.blocking_send(Ok(rb)).is_err() {
                        tracing::debug!(rows, batches, "Flight read receiver closed early");
                        return;
                    }
                    batches += 1;
                }
                tracing::info!(rows, batches, "Flight read produced");
            })
        });

        // `FlightError::Tonic` in and back out again, so the status a read
        // failed with is the status the client sees.
        //
        // This used to be `ExternalError(Box::new(e))` in and
        // `Status::internal(e.to_string())` out, and the pair silently collapsed
        // **every** read-path failure to `Internal` — the code survived only as
        // text inside the message, as `External error: code: '…'`. A client
        // switching on `code()` therefore saw one value for a stale ticket, an
        // evicted snapshot and a genuine server fault alike, and the first two
        // are things it can act on: ask for a new ticket, retry at a newer
        // snapshot. Found while asserting on the codes rather than on the
        // messages, which is the only way this shows up.
        let batches = tokio_stream::wrappers::ReceiverStream::new(rx)
            .map_err(|e| arrow_flight::error::FlightError::Tonic(Box::new(e)));
        let stream = FlightDataEncoderBuilder::new()
            .with_schema(ordinals_schema())
            .build(batches)
            .map_err(|e| match e {
                arrow_flight::error::FlightError::Tonic(s) => *s,
                // Encoding failed rather than reading — that really is ours.
                other => Status::internal(other.to_string()),
            });
        Ok(Response::new(stream.boxed()))
    }

    #[tracing::instrument(
        name = "yesno.flight.do_put",
        skip_all,
        fields(operation = tracing::field::Empty, batches = tracing::field::Empty, rows = tracing::field::Empty),
        err
    )]
    async fn do_put(
        &self,
        r: Request<Streaming<FlightData>>,
    ) -> Result<Response<Self::DoPutStream>, Status> {
        // The mode rides on the **first message's** descriptor, and reading it
        // means peeking the stream before handing it to the decoder — a
        // `FlightRecordBatchStream` consumes the descriptor without exposing it.
        // The peeked message is chained back so no data is lost.
        let mut inner = r.into_inner();
        let first = inner.next().await.transpose().map_err(|e| {
            Status::invalid_argument(format!("do_put stream failed immediately: {e}"))
        })?;
        let mode = put_mode(
            first
                .as_ref()
                .and_then(|d| d.flight_descriptor.as_ref())
                .map(|d| d.cmd.as_ref()),
        )?;
        let operation = mode.name();
        tracing::Span::current().record("operation", operation);

        let head = futures::stream::iter(first.map(Ok));
        let stream = head
            .chain(inner)
            .map_err(|e| arrow_flight::error::FlightError::ExternalError(Box::new(e)));
        let mut decoded =
            arrow_flight::decode::FlightRecordBatchStream::new_from_flight_data(stream);

        let mut rows = 0u64;
        let mut batch_count = 0u64;
        let mut version = 0u64;

        // One accumulating batch for the whole stream, for the modes whose
        // point is that every row becomes visible at the same instant.
        // `Insert` and `Remove` keep their per-record-batch commit: changing
        // that would alter what an existing client's version means.
        let mut pending = match mode {
            PutMode::Apply => Some(self.db.batch()),
            _ => None,
        };

        // Claimed for the whole stream, released by the guard however it ends.
        let mut _staging = match mode {
            PutMode::Txn(id) => Some(self.claim_staging(id)?),
            _ => None,
        };

        while let Some(b) = decoded.next().await {
            let b = b.map_err(|e| Status::invalid_argument(e.to_string()))?;
            let batch_rows = b.num_rows() as u64;

            match mode {
                PutMode::Insert | PutMode::Remove => {
                    let db = self.db.clone();
                    let remove = matches!(mode, PutMode::Remove);

                    // `spawn_blocking`, for the same reason `do_get` uses it and
                    // a stronger one. A commit appends to the WAL and **fsyncs**,
                    // and it may synchronously trigger a whole checkpoint —
                    // serializing dirty chunks, rebuilding the index and syncing
                    // again, which is seconds of work under a sustained ingest.
                    //
                    // Awaited before the next batch is pulled, so "one batch, one
                    // commit" still holds and batches still commit in the order
                    // they arrived.
                    let commit_span = tracing::debug_span!(
                        "yesno.flight.do_put.commit",
                        batch = batch_count + 1,
                        rows = batch_rows,
                        operation,
                    );
                    let n = tokio::task::spawn_blocking(move || {
                        commit_span.in_scope(move || -> Result<(u64, u64), Status> {
                            let mut wb = db.batch();
                            let staged = stage_pairs(&mut wb, &b, remove)?;
                            let committed = wb.commit().map_err(engine_status)?;
                            tracing::debug!(
                                version = committed.version,
                                changed = committed.changed,
                                "Flight ingest batch committed"
                            );
                            Ok((staged, committed.version))
                        })
                    })
                    .await
                    .map_err(|e| Status::internal(e.to_string()))??;
                    rows += n.0;
                    // The **last** batch's version, which is the highest.
                    version = version.max(n.1);
                }
                PutMode::Apply => {
                    let staged = decode_mutations(&b)?;
                    // Checked before applying. `apply` discards its batch on
                    // any error, so the ordering matters less here than in a
                    // transaction, but the two paths should not differ in when
                    // a bound is enforced.
                    if rows + staged.len() as u64 > MAX_TRANSACTION_ROWS {
                        return Err(Status::resource_exhausted(format!(
                            "apply exceeded {MAX_TRANSACTION_ROWS} staged rows; \
                             split the work or use a write transaction"
                        )));
                    }
                    rows += staged.len() as u64;
                    apply_staged(pending.as_mut().expect("created for this mode"), staged);
                }
                PutMode::Txn(id) => {
                    // Staged under the lock so the row bound is enforced
                    // against the transaction's running total rather than this
                    // stream's, which is what a caller streaming in several
                    // `do_put` calls is actually accumulating.
                    let mut open = self
                        .writes
                        .lock()
                        .map_err(|_| Status::internal("write transaction table poisoned"))?;
                    let tx = open.open.get_mut(&id).ok_or_else(|| {
                        Status::not_found(format!("write transaction {id} is not open"))
                    })?;
                    if std::time::Instant::now() > tx.deadline {
                        open.open.remove(&id);
                        return Err(Status::deadline_exceeded(format!(
                            "write transaction {id} expired before this batch"
                        )));
                    }
                    // **Decoded, validated and bound-checked before a single
                    // row reaches `tx.batch`.** Both halves of this used to run
                    // the other way round: rows were applied as they were
                    // validated, and the row total was incremented before the
                    // limit was tested. Either error therefore left the
                    // rejected work in the resident batch, and `commit_write`
                    // re-checks neither -- so a caller could commit a
                    // transaction the server had told it was invalid or
                    // over-limit, and an audit demonstrated exactly that.
                    let staged = decode_mutations(&b)?;
                    let staged_rows = staged.len() as u64;
                    if tx.rows + staged_rows > MAX_TRANSACTION_ROWS {
                        // Left open rather than aborted: the client asked for
                        // atomicity, so silently discarding half of it is worse
                        // than telling it to abort. Nothing from this batch was
                        // staged, so what is open is exactly what was accepted.
                        return Err(Status::resource_exhausted(format!(
                            "write transaction {id} would exceed {MAX_TRANSACTION_ROWS} \
                             staged rows; nothing from this batch was staged, so it can \
                             be committed as it stands or aborted"
                        )));
                    }
                    apply_staged(&mut tx.batch, staged);
                    tx.rows += staged_rows;
                    rows += staged_rows;
                }
            }
            batch_count += 1;
        }

        // The stream was consumed without error, so this call staged all of
        // itself or none of it. Anything that returned early above leaves the
        // guard unclean and poisons the transaction on drop.
        if let Some(g) = _staging.as_mut() {
            g.finished();
        }

        // `Apply`'s single commit: every row in the stream becomes visible at
        // one version, which is the whole difference from `Insert`.
        if let Some(wb) = pending {
            let commit_span = tracing::debug_span!(
                "yesno.flight.do_put.commit",
                batches = batch_count,
                rows,
                operation,
            );
            version = tokio::task::spawn_blocking(move || {
                commit_span.in_scope(move || -> Result<u64, Status> {
                    Ok(wb.commit().map_err(engine_status)?.version)
                })
            })
            .await
            .map_err(|e| Status::internal(e.to_string()))??;
        }

        tracing::Span::current().record("batches", batch_count);
        tracing::Span::current().record("rows", rows);
        tracing::info!(
            operation,
            batches = batch_count,
            rows,
            "Flight ingest completed"
        );

        // **Sixteen bytes, not eight**, and the widening is the whole content
        // of `no-session-guarantees-on-the-flight-surface`: the engine knew the
        // commit version, this handler logged it, and then threw it away — so a
        // remote client could not name the version its write landed at, could
        // not wait for it, and could not bind a read to it. Rows stay first so
        // the field is read the same way it always was; the version is appended.
        //
        // A `version` of 0 means no batch committed — an empty stream, or a
        // transaction-bound stream whose rows are staged and not yet visible —
        // and is not a version any commit is ever assigned.
        let mut metadata = Vec::with_capacity(16);
        metadata.extend_from_slice(&rows.to_le_bytes());
        metadata.extend_from_slice(&version.to_le_bytes());
        let out = futures::stream::once(async move {
            Ok(PutResult {
                app_metadata: metadata.into(),
            })
        });
        Ok(Response::new(out.boxed()))
    }

    async fn do_exchange(
        &self,
        _r: Request<Streaming<FlightData>>,
    ) -> Result<Response<Self::DoExchangeStream>, Status> {
        Err(Status::unimplemented(
            "do_exchange is cut: do_get and do_put cover every case here",
        ))
    }

    #[tracing::instrument(
        name = "yesno.flight.do_action",
        skip_all,
        fields(action = tracing::field::Empty),
        err
    )]
    async fn do_action(
        &self,
        r: Request<Action>,
    ) -> Result<Response<Self::DoActionStream>, Status> {
        let a = r.into_inner();
        tracing::Span::current().record("action", a.r#type.as_str());
        let body: Vec<u8> = match a.r#type.as_str() {
            "stats" => prost::Message::encode_to_vec(&ServerStats {
                allocated_bytes: self.db.allocated_bytes(),
                deferred_bytes: self.db.deferred_bytes(),
                wal_bytes: self.db.wal_bytes(),
                live_readers: self.db.live_readers() as u64,
                shards: self.db.shard_count() as u64,
                features: FEATURES,
            }),
            ACTION_BEGIN_WRITE => {
                let mut w = self
                    .writes
                    .lock()
                    .map_err(|_| Status::internal("write transaction table poisoned"))?;
                // Swept here rather than on a timer: this crate spawns no
                // tasks of its own, and `begin` is the moment the answer to
                // "is there room" is about to be needed.
                let now = std::time::Instant::now();
                w.open.retain(|_, tx| tx.deadline > now);
                if w.open.len() >= MAX_OPEN_TRANSACTIONS {
                    return Err(Status::resource_exhausted(format!(
                        "{MAX_OPEN_TRANSACTIONS} write transactions are already open"
                    )));
                }
                let id = w.mint();
                w.open.insert(
                    id,
                    PendingWrite {
                        batch: self.db.batch(),
                        rows: 0,
                        poisoned: None,
                        deadline: now + self.write_ttl,
                        staging: false,
                    },
                );
                tracing::debug!(transaction = id, "write transaction begun");
                id.to_le_bytes().to_vec()
            }
            ACTION_COMMIT_WRITE => {
                let id = u64::from_le_bytes(a.body.as_ref().try_into().map_err(|_| {
                    Status::invalid_argument("commit_write body must be one 8-byte LE transaction")
                })?);
                let taken = {
                    let mut w = self
                        .writes
                        .lock()
                        .map_err(|_| Status::internal("write transaction table poisoned"))?;
                    // **Idempotent by identity.** A retry after a lost response
                    // finds its own outcome and returns the original version
                    // rather than committing a second time. This is the
                    // property a CDC pipe needs to resume after an ambiguous
                    // network failure without risking double application.
                    if let Some(&(_, version)) = w.committed.iter().find(|&&(tx, _)| tx == id) {
                        tracing::debug!(
                            transaction = id,
                            version,
                            "write transaction commit replayed from memory"
                        );
                        return Ok(Response::new(
                            futures::stream::once(async move {
                                Ok(arrow_flight::Result {
                                    body: version.to_le_bytes().to_vec().into(),
                                })
                            })
                            .boxed(),
                        ));
                    }
                    let tx = w.open.get(&id).ok_or_else(|| {
                        Status::not_found(format!(
                            "write transaction {id} is not open; it was never begun, was \
                             aborted, or expired"
                        ))
                    })?;
                    if let Some(why) = &tx.poisoned {
                        return Err(Status::failed_precondition(format!(
                            "write transaction {id} cannot be committed: {why}. Abort it \
                             and stage the work again."
                        )));
                    }
                    if tx.staging {
                        return Err(Status::failed_precondition(format!(
                            "write transaction {id} still has a do_put stream in flight"
                        )));
                    }
                    if std::time::Instant::now() > tx.deadline {
                        w.open.remove(&id);
                        return Err(Status::deadline_exceeded(format!(
                            "write transaction {id} expired before commit; nothing was applied"
                        )));
                    }
                    w.open.remove(&id).expect("checked above")
                };

                let rows = taken.rows;
                let batch = taken.batch;
                let span = tracing::Span::current();
                let version = tokio::task::spawn_blocking(move || {
                    span.in_scope(move || batch.commit().map(|r| r.version))
                })
                .await
                .map_err(|e| Status::internal(e.to_string()))?
                .map_err(engine_status)?;

                {
                    let mut w = self
                        .writes
                        .lock()
                        .map_err(|_| Status::internal("write transaction table poisoned"))?;
                    w.committed.push_back((id, version));
                    while w.committed.len() > COMMIT_MEMORY {
                        w.committed.pop_front();
                    }
                }
                tracing::info!(
                    transaction = id,
                    version,
                    rows,
                    "write transaction committed"
                );
                version.to_le_bytes().to_vec()
            }
            ACTION_ABORT_WRITE => {
                let id = u64::from_le_bytes(a.body.as_ref().try_into().map_err(|_| {
                    Status::invalid_argument("abort_write body must be one 8-byte LE transaction")
                })?);
                let mut w = self
                    .writes
                    .lock()
                    .map_err(|_| Status::internal("write transaction table poisoned"))?;
                // Aborting something already committed is an error, not a
                // no-op: the caller believes nothing was applied, and a
                // version exists that says otherwise.
                if let Some(&(_, version)) = w.committed.iter().find(|&&(tx, _)| tx == id) {
                    return Err(Status::failed_precondition(format!(
                        "write transaction {id} already committed at version {version}"
                    )));
                }
                // An unknown transaction is *not* an error. It was aborted, or
                // it expired, and either way the caller's intent -- that none of
                // it is visible -- already holds.
                let dropped = w.open.remove(&id).is_some();
                tracing::debug!(transaction = id, dropped, "write transaction aborted");
                0u64.to_le_bytes().to_vec()
            }
            ACTION_CLEAR => {
                let bytes: [u8; 8] = a.body.as_ref().try_into().map_err(|_| {
                    Status::invalid_argument("clear action body must be one 8-byte LE key")
                })?;
                let key = u64::from_le_bytes(bytes);
                let db = self.db.clone();
                let span = tracing::Span::current();
                tokio::task::spawn_blocking(move || {
                    span.in_scope(move || {
                        let mut batch = db.batch();
                        batch.delete_key(key);
                        batch.commit().map(|result| result.version)
                    })
                })
                .await
                .map_err(|e| Status::internal(e.to_string()))?
                .map_err(engine_status)?
                .to_le_bytes()
                .to_vec()
            }
            ACTION_CONTAINS => {
                let bytes: [u8; 16] = a.body.as_ref().try_into().map_err(|_| {
                    Status::invalid_argument(
                        "contains action body must be an 8-byte LE key followed by an 8-byte LE ordinal",
                    )
                })?;
                let key = u64::from_le_bytes(bytes[..8].try_into().unwrap());
                let ordinal = u64::from_le_bytes(bytes[8..].try_into().unwrap());
                let db = self.db.clone();
                let span = tracing::Span::current();
                let present = tokio::task::spawn_blocking(move || {
                    span.in_scope(move || db.snapshot()?.contains(key, ordinal))
                })
                .await
                .map_err(|e| Status::internal(e.to_string()))?
                .map_err(engine_status)?;
                u64::from(present).to_le_bytes().to_vec()
            }
            ACTION_INSERT_ONE | ACTION_REMOVE_ONE => {
                let bytes: [u8; 16] = a.body.as_ref().try_into().map_err(|_| {
                    Status::invalid_argument(
                        "point mutation action body must be an 8-byte LE key followed by an 8-byte LE ordinal",
                    )
                })?;
                let key = u64::from_le_bytes(bytes[..8].try_into().unwrap());
                let ordinal = u64::from_le_bytes(bytes[8..].try_into().unwrap());
                let insert = a.r#type == ACTION_INSERT_ONE;
                let db = self.db.clone();
                let span = tracing::Span::current();
                let changed = tokio::task::spawn_blocking(move || {
                    span.in_scope(move || {
                        if insert {
                            db.insert(key, ordinal)
                        } else {
                            db.remove(key, ordinal)
                        }
                    })
                })
                .await
                .map_err(|e| Status::internal(e.to_string()))?
                .map_err(engine_status)?;
                u64::from(changed).to_le_bytes().to_vec()
            }
            other => {
                return Err(Status::invalid_argument(format!(
                    "unknown action `{other}`"
                )));
            }
        };
        let out =
            futures::stream::once(async move { Ok(arrow_flight::Result { body: body.into() }) });
        Ok(Response::new(out.boxed()))
    }

    async fn list_actions(
        &self,
        _r: Request<Empty>,
    ) -> Result<Response<Self::ListActionsStream>, Status> {
        let actions = vec![
            ActionType {
                r#type: "stats".into(),
                description: "space and reader counters, as protobuf".into(),
            },
            ActionType {
                r#type: ACTION_CLEAR.into(),
                description: "atomically clear one key; returns committed version".into(),
            },
            ActionType {
                r#type: ACTION_BEGIN_WRITE.into(),
                description: "open a write transaction; returns its 8-byte LE handle".into(),
            },
            ActionType {
                r#type: ACTION_COMMIT_WRITE.into(),
                description: "publish a write transaction as one version; idempotent per \
                              handle, returns the version"
                    .into(),
            },
            ActionType {
                r#type: ACTION_ABORT_WRITE.into(),
                description: "discard a write transaction's staged work".into(),
            },
            ActionType {
                r#type: ACTION_CONTAINS.into(),
                description: "test one (key, ordinal) pair; returns zero or one".into(),
            },
            ActionType {
                r#type: ACTION_INSERT_ONE.into(),
                description: "atomically insert one pair; returns whether it changed".into(),
            },
            ActionType {
                r#type: ACTION_REMOVE_ONE.into(),
                description: "atomically remove one pair; returns whether it changed".into(),
            },
        ];
        Ok(Response::new(
            futures::stream::iter(actions.into_iter().map(Ok)).boxed(),
        ))
    }
}
