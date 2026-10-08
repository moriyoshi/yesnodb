//! The plugin-channel transport: a Unix socket to a running `yesnod`.
//!
//! The second [`Transport`] implementation, and the one the module header has
//! been describing as planned. It exists because of the problem recorded there:
//! an in-process transport cannot work while `Db::open` takes an exclusive
//! `flock` and PostgreSQL forks a backend per connection. A socket does not work
//! *around* that, it makes it moot -- one process owns the directory and N
//! backends connect to it.
//!
//! # It does not link the storage engine, and that is the point
//!
//! `yesno-pg` compiles its Flight client without the server feature so that a
//! PostgreSQL backend contains no storage engine. This transport holds to the
//! same rule by depending on `yesno-channel`, the peer half of the protocol,
//! which was split out of `yesno-plugin` for exactly this reason.
//!
//! # Filter pushdown, which this used to give up
//!
//! A `cmd` in this trait is either a bare 8-byte key or an encoded `yesno-wire`
//! expression, and **both are served**. Until 2026-10-09 only the first was: the
//! channel had no expression frame, so `plan_pushdown` declined to lower quals
//! for a channel server and PostgreSQL applied every filter itself.
//!
//! What closed it is `Frame::SnapshotEvalCardinality` and
//! `Frame::SnapshotEvalLoad`, which carry the encoded expression to the server
//! and are answered by the **same evaluator the Flight surface uses**. That
//! identity is the point and not an implementation detail: two transports
//! answering one filter differently would make a deployment choice into a
//! correctness difference, and the only way to be sure they agree is for there
//! to be one implementation rather than two.
//!
//! A server may still decline. `max_expr_bytes` is advertised in the greeting
//! and zero means it evaluates none, which is a deployment an operator can
//! choose -- so [`cmd_of`] reports rather than guessing, and the client refuses
//! locally before spending a round trip. The planner is not consulted about
//! that, because it cannot be: `plan_pushdown` runs before anything connects.
//! A channel server configured to evaluate nothing therefore fails a pushed-down
//! scan rather than falling back, which is the one rough edge left here and is
//! recorded as such.
use yesno_channel::client::{Client, Error as ClientError, Snapshot};
use yesno_channel::ipc::{Write, WriteOp};

use super::{OrdinalBatch, Transport, TransportError};
use crate::ordinal::ordinal_to_i64;

/// What the server logs this peer as.
const PEER_NAME: &str = "yesno-pg";

/// An open scan's position.
///
/// `after` is the last ordinal handed out, and the next page resumes **strictly
/// above** it. Continuation is therefore by value, with no server-side cursor to
/// leak or expire -- sound only because `snapshot` pins the version, which is
/// what makes resuming from a value yield a consistent sequence rather than a
/// smear of two states.
struct Scan {
    cmd: Cmd,
    after: Option<u64>,
    done: bool,
}

pub struct ChannelTransport {
    socket: String,
    batch_rows: usize,
    client: Option<Client>,
    /// The pinned read view. Held across statements so a ticket stays valid for
    /// the length of a transaction, which is the property `ticket_for` promises.
    pinned: Option<Snapshot>,
    scan: Option<Scan>,
}

/// What a descriptor is asking for.
///
/// The two forms the [`Transport`] trait defines, kept as a decoded value rather
/// than re-examined at each use so that a scan cannot classify its descriptor
/// one way when it opens and the other way when it pages.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Cmd {
    Key(u64),
    /// The encoded expression, carried rather than decoded: the server decodes
    /// it, and this crate holds no evaluator.
    Expr(Vec<u8>),
}

/// Classify a descriptor exactly as the Flight server does.
///
/// `SetExpr::looks_like_expr` and not a length test of this crate's own, because
/// **the two transports must disagree about nothing**, this included. It is also
/// the subtler check: a bare key is eight arbitrary bytes and keys are commonly
/// hashes, so one can begin with the `YSNX` magic by coincidence -- which is why
/// that function tests the length first and no valid expression is eight bytes.
fn cmd_of(cmd: &[u8]) -> Result<Cmd, TransportError> {
    if yesno_wire::SetExpr::looks_like_expr(cmd) {
        return Ok(Cmd::Expr(cmd.to_vec()));
    }
    if cmd.len() != 8 {
        return Err(TransportError::Schema(format!(
            "a descriptor is a bare 8-byte key or an encoded expression, and this is \
             {} bytes of neither",
            cmd.len()
        )));
    }
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(cmd);
    Ok(Cmd::Key(u64::from_le_bytes(bytes)))
}

impl ChannelTransport {
    pub fn new(socket: &str, batch_rows: usize) -> Result<Self, TransportError> {
        Ok(Self {
            socket: socket.to_owned(),
            batch_rows: batch_rows.max(1),
            client: None,
            pinned: None,
            scan: None,
        })
    }

    /// Connect on first use.
    ///
    /// Lazily, as the Flight transport does: `GetForeignRelSize` constructs a
    /// transport for a plan that may never execute, and a socket that is not
    /// there should fail the query that needs it rather than the planning of
    /// every query that mentions the table.
    fn client(&mut self) -> Result<&mut Client, TransportError> {
        if self.client.is_none() {
            let client = Client::connect(&self.socket, PEER_NAME).map_err(|why| {
                TransportError::Connect {
                    endpoint: self.socket.clone(),
                    why: why.to_string(),
                }
            })?;
            self.client = Some(client);
        }
        Ok(self.client.as_mut().expect("just assigned"))
    }

    /// Pin a fresh read view, replacing any held one.
    fn pin(&mut self) -> Result<&Snapshot, TransportError> {
        let snapshot = self.client()?.snapshot().map_err(rpc)?;
        self.pinned = Some(snapshot);
        Ok(self.pinned.as_ref().expect("just assigned"))
    }

    fn commit(&mut self, writes: Vec<Write>) -> Result<u64, TransportError> {
        let client = self.client()?;
        let allowed = client.limits().max_writes as usize;
        if writes.len() > allowed {
            // Refused rather than split, for the reason the trait gives: one
            // PostgreSQL transaction is meant to be one yesno version. Splitting
            // would make a half-applied transaction briefly visible, which is a
            // worse answer than a refused one.
            return Err(TransportError::Rpc(format!(
                "this transaction needs {} writes and the server accepts {} in one \
                 commit; splitting it would make a partial transaction visible",
                writes.len(),
                allowed
            )));
        }
        let (version, _changed) = client.apply(writes).map_err(rpc)?;
        Ok(version)
    }
}

/// Every non-connect client failure is an RPC failure to this layer.
///
/// The channel's classification -- retryable, stale, expired, wrong-role -- is
/// finer than [`TransportError`] can express, so it travels in the message.
/// Widening `TransportError` would be the way to act on it, exactly as widening
/// MySQL's `Backend` was.
fn rpc(error: ClientError) -> TransportError {
    TransportError::Rpc(error.to_string())
}

impl Transport for ChannelTransport {
    fn cardinality(&mut self, cmd: &[u8]) -> Result<u64, TransportError> {
        let cmd = cmd_of(cmd)?;
        let snapshot = self.pin()?;
        match &cmd {
            Cmd::Key(key) => snapshot.cardinality(*key),
            // Exact, not estimated, for the same reason the key form is: the
            // server counts container popcounts without materializing the set.
            Cmd::Expr(bytes) => snapshot.cardinality_expr(bytes),
        }
        .map_err(rpc)
    }

    fn open_scan(&mut self, cmd: &[u8]) -> Result<(), TransportError> {
        let cmd = cmd_of(cmd)?;
        self.pin()?;
        self.scan = Some(Scan {
            cmd,
            after: None,
            done: false,
        });
        Ok(())
    }

    /// Mint a ticket recording the version this transport has pinned.
    ///
    /// The channel has no frame for "open a snapshot at version V", so a ticket
    /// cannot be a self-contained reconstruction the way a Flight ticket is.
    /// Instead the pin is held here and the ticket names it: the version, then
    /// the descriptor verbatim, so that [`Transport::open_scan_with_ticket`] can
    /// verify it is being handed back the pin it was minted from rather than
    /// silently reading a newer one.
    ///
    /// **The descriptor rather than a key**, since 2026-10-09. It was a key when
    /// a key was all this transport served, and a fixed 16 bytes; carrying the
    /// `cmd` means a ticket replays the same scan whether it is a key or a
    /// pushed-down expression, and it is what `open_scan` already classifies, so
    /// the two paths cannot disagree about what a ticket meant.
    fn ticket_for(&mut self, cmd: &[u8]) -> Result<Vec<u8>, TransportError> {
        // Classified before minting, so a descriptor this transport cannot serve
        // fails here rather than at the first page of a replay.
        cmd_of(cmd)?;
        let version = self.pin()?.version();
        let mut ticket = Vec::with_capacity(8 + cmd.len());
        ticket.extend_from_slice(&version.to_le_bytes());
        ticket.extend_from_slice(cmd);
        Ok(ticket)
    }

    fn open_scan_with_ticket(&mut self, ticket: &[u8]) -> Result<(), TransportError> {
        if ticket.len() < 16 {
            return Err(TransportError::Schema(format!(
                "a channel ticket is a version and a descriptor, so at least 16 bytes, \
                 and this one is {}",
                ticket.len()
            )));
        }
        let mut version = [0u8; 8];
        version.copy_from_slice(&ticket[..8]);
        let cmd = cmd_of(&ticket[8..])?;
        let want = u64::from_le_bytes(version);

        // The pin is this transport's, so a ticket from a different connection
        // cannot be honoured. Reported rather than served at whatever version
        // happens to be current, because reading a different version than the
        // ticket names is the one failure a snapshot is supposed to prevent.
        match self.pinned.as_ref().map(Snapshot::version) {
            Some(have) if have == want => {}
            Some(have) => {
                return Err(TransportError::Rpc(format!(
                    "this ticket names version {want} and the pinned snapshot is {have}; \
                     the channel cannot reopen a version, so the transaction must restart"
                )))
            }
            None => {
                return Err(TransportError::Rpc(format!(
                    "this ticket names version {want} and no snapshot is pinned; the \
                     channel cannot reopen a version, so the transaction must restart"
                )))
            }
        }
        self.scan = Some(Scan {
            cmd,
            after: None,
            done: false,
        });
        Ok(())
    }

    fn next_batch(&mut self) -> Result<Option<OrdinalBatch>, TransportError> {
        let Some(scan) = self.scan.as_ref() else {
            return Ok(None);
        };
        if scan.done {
            return Ok(None);
        }
        let (cmd, after) = (scan.cmd.clone(), scan.after);
        let limit = self.batch_rows as u32;
        let snapshot = self
            .pinned
            .as_ref()
            .ok_or_else(|| TransportError::Rpc("the scan has no pinned snapshot".into()))?;
        let (ordinals, more) = match &cmd {
            Cmd::Key(key) => snapshot.load(*key, after, limit),
            // **Ask for the whole page every time.** Each page of an expression
            // costs the server an evaluation -- its answer is computed, so there
            // is nothing to seek into -- and `batch_rows` is what bounds that, so
            // a small one buys several evaluations for one filtered set.
            Cmd::Expr(bytes) => snapshot.load_expr(bytes, after, limit),
        }
        .map_err(rpc)?;

        let scan = self.scan.as_mut().expect("checked above");
        if let Some(last) = ordinals.last() {
            scan.after = Some(*last);
        }
        // A short page ends the scan whatever `more` says, and an empty page
        // ends it even if `more` is set -- believing a server that promises more
        // and sends none would spin forever.
        if !more || ordinals.is_empty() {
            scan.done = true;
        }
        if ordinals.is_empty() {
            return Ok(None);
        }
        Ok(Some(ordinals.into_iter().map(ordinal_to_i64).collect()))
    }

    fn close_scan(&mut self) {
        self.scan = None;
    }

    fn put(&mut self, key: u64, ordinals: &[u64], remove: bool) -> Result<u64, TransportError> {
        let op = if remove {
            WriteOp::Remove
        } else {
            WriteOp::Insert
        };
        let writes = ordinals
            .iter()
            .map(|ordinal| Write {
                key,
                lo: *ordinal,
                hi: *ordinal,
                op,
            })
            .collect();
        self.commit(writes)
    }

    fn apply(&mut self, ops: &[(u64, u64, bool)]) -> Result<u64, TransportError> {
        let writes = ops
            .iter()
            .map(|(key, ordinal, remove)| Write {
                key: *key,
                lo: *ordinal,
                hi: *ordinal,
                op: if *remove {
                    WriteOp::Remove
                } else {
                    WriteOp::Insert
                },
            })
            .collect();
        self.commit(writes)
    }

    /// One frame, because the protocol has had this exact question all along.
    ///
    /// `Frame::SnapshotContains` is a membership test at the snapshot, so the
    /// default's `And( Key, Range )` expression is not needed here even now that
    /// this transport can evaluate one -- and a one-ordinal probe through the
    /// evaluator would materialize a set to look at one element of it.
    fn contains(&mut self, key: u64, ordinal: u64) -> Result<bool, TransportError> {
        self.pin()?.contains(key, ordinal).map_err(rpc)
    }

    fn keys(&mut self) -> Result<Vec<u64>, TransportError> {
        let limit = self.batch_rows as u32;
        let snapshot = self.pin()?;
        let mut all = Vec::new();
        let mut lo = 0u64;
        loop {
            let (page, more) = snapshot.key_range(lo, u64::MAX, limit).map_err(rpc)?;
            let last = page.last().copied();
            all.extend_from_slice(&page);
            match last {
                // The range is inclusive, so the next page must start above the
                // last key. At `u64::MAX` there is no above, which is also the
                // only way this loop could fail to terminate.
                Some(k) if more && k < u64::MAX => lo = k + 1,
                _ => break,
            }
        }
        Ok(all)
    }
}
