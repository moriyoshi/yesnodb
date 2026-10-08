//! How the extension reaches yesno.
//!
//! One trait, so the scan callbacks never learn which deployment they are in.
//! Two exist: Flight over gRPC, and the plugin channel over a Unix socket.
//!
//! # The channel is what `Local` was reaching for
//!
//! The note below explains why an in-process transport cannot work: one process
//! must hold the directory lock while PostgreSQL forks a backend per
//! connection. [`channel::ChannelTransport`] does not solve that, it sidesteps
//! it -- `yesnod` owns the directory and every backend is a peer on a socket --
//! and it needs no multi-process reader in the engine.
//!
//! It cost filter pushdown until 2026-10-09, because the channel served keys
//! rather than `yesno-wire` expressions. It no longer does: the protocol carries
//! an encoded expression and the host evaluates it with `yesno-eval`, **the same
//! crate behind the Flight surface**. The two transports therefore answer a
//! lowered qual identically by construction rather than by agreement, which is
//! the property that lets this trait hide which one a deployment chose.
//!
//! A `yesnod` can be configured to evaluate none ( `channel_max_expr_bytes = 0`
//! ), which it reports only in its greeting -- so the planner is told by the
//! `pushdown` server option rather than by a probe it has no connection to make.
//! See `options::ServerOptions::pushdown`.
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

pub mod channel;
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

    /// Whether one ordinal is in one key.
    ///
    /// A membership probe rather than a scan, because the table AM asks it per
    /// TID an index handed back: such a TID may name a row that has since been
    /// deleted, and synthesising the row from the TID alone would resurrect it.
    ///
    /// The default asks for the cardinality of `Key( key ) AND [ordinal,
    /// ordinal + 1 )`, which is how the table AM wrote it when Flight was the
    /// only transport.
    ///
    /// **It is a trait method because that spelling was once a wrong answer.**
    /// Built at the call site, it reached a channel transport that evaluated no
    /// expressions, which rejected the descriptor correctly -- and the caller
    /// read the rejection as "absent", so every live row vanished through a TID
    /// fetch. The channel evaluates expressions now, so the default would work
    /// there; it still overrides, because the protocol has a membership frame
    /// and running a one-ordinal probe through the evaluator would materialize a
    /// set to look at one element of it.
    ///
    /// A transport that cannot serve the default must override it. One that can
    /// should still override when it has something cheaper.
    fn contains(&mut self, key: u64, ordinal: u64) -> Result<bool, TransportError> {
        let expr = yesno_wire::SetExpr::And(vec![
            yesno_wire::SetExpr::Key(key),
            // Saturating because `u64::MAX` is reserved and unstorable, so the
            // empty range it produces is the right answer rather than a wrap.
            yesno_wire::SetExpr::Range(ordinal, ordinal.saturating_add(1)),
        ]);
        Ok(self.cardinality(&expr.encode())? > 0)
    }

    /// Every populated key, ascending.
    ///
    /// Needed by the index AM's `ambulkdelete`, which must visit every key
    /// when VACUUM removes heap tuples — there is no reverse map from an ordinal
    /// to the keys containing it. A key it skips leaves dangling TIDs.
    fn keys(&mut self) -> Result<Vec<u64>, TransportError>;
}

/// Open whichever transport a parsed [`crate::options::Transport`] names.
///
/// One place, because four call sites need one: the FDW from server options,
/// the index AM and the table AM from GUCs, and the pre-commit flush from a
/// buffer key. A fifth variant arriving must not be something three of them
/// can quietly fail to handle.
pub fn open(
    kind: &crate::options::Transport,
    batch_rows: usize,
) -> Result<Box<dyn Transport>, TransportError> {
    match kind {
        crate::options::Transport::Flight { endpoint } => {
            flight::FlightTransport::new(endpoint).map(|t| Box::new(t) as Box<dyn Transport>)
        }
        crate::options::Transport::Channel { socket } => {
            channel::ChannelTransport::new(socket, batch_rows)
                .map(|t| Box::new(t) as Box<dyn Transport>)
        }
        // Not "unimplemented" in the sense of work that only needs writing.
        // See this module's header: it needs a multi-process reader in the
        // engine first.
        crate::options::Transport::Local { data_dir } => Err(TransportError::Connect {
            endpoint: data_dir.clone(),
            why: "the \"data_dir\" transport is not available: it needs a multi-process \
                  read-only reader in yesno-core, which does not exist yet. Use \
                  \"endpoint\" or \"socket\" to reach a running yesnod"
                .into(),
        }),
    }
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
