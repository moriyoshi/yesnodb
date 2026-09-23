//! How the extension reaches yesno.
//!
//! One trait, so the scan callbacks never learn which deployment they are in.
//! Two implementations were planned; only one exists.
//!
//! # Why `Local` is absent
//!
//! `Db::open` takes a **non-blocking exclusive `flock`** on the database
//! directory and yesno has no shared or read-only open. PostgreSQL forks one
//! backend per connection, so N backends attempting an in-process open means
//! N-1 failures, and a running `yesnod` makes it N. An in-process transport
//! therefore needs a genuine multi-process reader in `yesno-core` — including a
//! cross-process reader registry, because extent reclamation is enforced from
//! process-local state and a foreign reader is invisible to it. Tracked as
//! `multiprocess-read-only-reader` in `TODO.md`.
//!
//! Do not implement `Transport` for a `data_dir` server by calling
//! `Db::open`. It works for exactly one backend and then fails in a way that
//! looks like a locking bug rather than a design gap.

pub mod flight;

/// A batch of ordinals, already mapped to the `bigint` PostgreSQL will see.
///
/// Ordinals rather than rows: the column is the only one, so a batch is a
/// contiguous `Vec` and the scan hands them out one at a time. Mapping happens
/// at the transport boundary so the executor path never sees a `u64`.
pub type OrdinalBatch = Vec<i64>;

/// A source of ordinals for one key.
pub trait Transport {
    /// Exact row count, without fetching rows.
    ///
    /// Exact, not estimated. `Snapshot::cardinality` sums container popcounts
    /// from the B+tree leaves and decodes no payload extent, so this costs no
    /// I/O over the ordinals themselves. It is what lets the planner be given a
    /// true row count — unusual among foreign data wrappers, and the reason
    /// `count(*)` pushdown is worth building in phase 3.
    /// `cmd` is the Flight descriptor payload: a bare 8-byte little-endian key,
    /// or an encoded `yesno-wire` expression. Opaque here on purpose — the
    /// transport must not care which, so that adding an expression form does not
    /// touch this layer.
    fn cardinality(&mut self, cmd: &[u8]) -> Result<u64, TransportError>;

    /// Begin streaming a key's ordinals. Subsequent batches come from
    /// [`Transport::next_batch`].
    fn open_scan(&mut self, cmd: &[u8]) -> Result<(), TransportError>;

    /// Mint a ticket for `cmd` without reading it.
    ///
    /// A ticket records **the version it was minted at**, and the server
    /// answers at that version rather than at whatever is current. That is what
    /// makes a stable snapshot expressible: hold one ticket for the length of a
    /// transaction and every scan through it sees the same state.
    fn ticket_for(&mut self, cmd: &[u8]) -> Result<Vec<u8>, TransportError>;

    /// Begin streaming from a ticket obtained earlier, possibly by an earlier
    /// statement in the same transaction.
    fn open_scan_with_ticket(&mut self, ticket: &[u8]) -> Result<(), TransportError>;

    /// The next batch, or `None` at end of stream.
    fn next_batch(&mut self) -> Result<Option<OrdinalBatch>, TransportError>;

    /// Abandon an open scan. Idempotent.
    fn close_scan(&mut self);

    /// Apply `( key, ordinal )` pairs, inserting or removing.
    ///
    /// One call, one batch, one server-side commit. The caller accumulates
    /// and flushes once, because a commit appends to the WAL and fsyncs — doing
    /// that per row would make an ordinary `INSERT … SELECT` unusable.
    fn put(&mut self, key: u64, ordinals: &[u64], remove: bool) -> Result<u64, TransportError>;

    /// Apply every buffered change as **one** server-side commit.
    ///
    /// `ops` is `( key, ordinal, remove )` and may span keys. One PostgreSQL
    /// transaction is one yesno version, which is what [`Transport::put`]
    /// could not express: it takes one key and one direction, so a transaction
    /// touching *n* keys with both insertions and removals became `2n`
    /// commits, each briefly visible to readers.
    ///
    /// Order does not matter here and the buffer guarantees it: pending writes
    /// are keyed by ordinal, so an ordinal is either inserted or removed, never
    /// both.
    fn apply(&mut self, ops: &[(u64, u64, bool)]) -> Result<u64, TransportError>;

    /// Every populated key, ascending.
    ///
    /// Needed by the index AM's `ambulkdelete`, which must visit every key
    /// when VACUUM removes heap tuples — there is no reverse map from an ordinal
    /// to the keys containing it. A key it skips leaves dangling TIDs.
    fn keys(&mut self) -> Result<Vec<u64>, TransportError>;
}

#[derive(Debug)]
pub enum TransportError {
    Connect {
        endpoint: String,
        why: String,
    },
    Rpc(String),
    /// The server sent something that does not match the agreed schema.
    ///
    /// Its own variant rather than folded into `Rpc` because the two mean
    /// different things to an operator: an `Rpc` failure is usually a network or
    /// a permissions problem, while this one means the client and server
    /// disagree about the wire format — a version skew between `yesno-pg` and
    /// `yesnod`, which no amount of retrying fixes.
    Schema(String),
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransportError::Connect { endpoint, why } => {
                write!(f, "cannot reach yesnod at {endpoint}: {why}")
            }
            TransportError::Rpc(m) => write!(f, "yesnod rpc failed: {m}"),
            TransportError::Schema(m) => write!(
                f,
                "yesnod returned an unexpected schema ({m}); \
                 this usually means yesno-pg and yesnod are different versions"
            ),
        }
    }
}
