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
//! # A ticket outlives the transport that minted it
//!
//! A Flight ticket is self-describing -- it carries the version, so a later
//! statement replays it against a connection opened fresh. This channel has no
//! frame for "open a snapshot at version V", so **the pin is the snapshot
//! handle** and the version lasts exactly as long as something holds it.
//!
//! PostgreSQL builds a transport per statement, so until 2026-10-09 a ticket was
//! good for the statement that minted it and no longer: the second read of a
//! `REPEATABLE READ` transaction failed with "the transaction must restart".
//! `PINS` holds the pin for the session instead, and
//! [`release_transaction_pins`] drops it on the same transaction callback that
//! clears the tickets.
//!
//! # A server may still decline, and the operator declares that
//!
//! `max_expr_bytes` is advertised in the greeting and zero means this `yesnod`
//! evaluates none, which is a deployment an operator can choose. The greeting
//! is read at connect time, and `plan_pushdown` holds no connection -- so the
//! planner is told by a **server option**, `pushdown 'off'`, which is the other
//! half of `channel_max_expr_bytes = 0` on the daemon.
//!
//! Probing instead was rejected: the planner would open a connection per planned
//! scan to learn a fact that cannot change between statements, and planning
//! would begin to fail when the server was merely unreachable.
//!
//! The two settings disagreeing is a **query that fails naming the option**, by
//! [`ChannelTransport::refuse_if_not_evaluated`], and not a wrong answer. The
//! client refuses it as well, before spending a round trip; this layer repeats
//! the check because it is the one that knows the message is read from a SQL
//! prompt and that the remedy is spelled `ALTER SERVER`.
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use yesno_channel::client::{Client, Error as ClientError, Snapshot};
use yesno_channel::ipc::{Write, WriteOp};

use super::{OrdinalBatch, Transport, TransportError};
use crate::ordinal::ordinal_to_i64;

/// What the server logs this peer as.
const PEER_NAME: &str = "yesno-pg";

thread_local! {
    /// Snapshots this transaction has pinned, by socket and version.
    ///
    /// # A ticket is a promise that its version is still pinned somewhere
    ///
    /// A Flight ticket is self-describing: it carries the version, so any later
    /// statement can replay it against a connection opened fresh. The channel
    /// has no frame for "open a snapshot at version V" -- **the pin is the
    /// snapshot handle**, and the version lives exactly as long as something
    /// holds it.
    ///
    /// Until 2026-10-09 the only holder was the `ChannelTransport` that minted
    /// the ticket, and PostgreSQL builds one of those per statement. So a ticket
    /// was good for the statement that minted it and no longer: the second read
    /// of a `REPEATABLE READ` transaction failed with "no snapshot is pinned;
    /// the transaction must restart". Holding the pin here instead makes it
    /// last as long as the ticket does.
    ///
    /// A thread-local is the right scope and not a convenience: a PostgreSQL
    /// backend is one process serving one session, so "this thread" and "this
    /// session" are the same thing, which is the same reasoning
    /// `fdw::modify`'s buffer already rests on.
    ///
    /// **Cleared by [`release_transaction_pins`] on the transaction callback
    /// that clears the tickets**, and the two must be one moment. A ticket
    /// outliving its pin is a recoverable error; a pin outliving its ticket
    /// holds a reader slot out of the database's 4096 and pins the reclamation
    /// floor, which nothing in this backend would ever notice.
    static PINS: RefCell<HashMap<(String, u64), Rc<Snapshot>>> =
        RefCell::new(HashMap::new());
}

/// Drop every snapshot this transaction pinned.
///
/// Called from `fdw::modify`'s transaction callback, beside the line that clears
/// the ticket map, because a ticket and its pin have to end together. Dropping a
/// [`Snapshot`] sends `SnapshotClose`, so this is where the server learns the
/// version may be collapsed; a failure to deliver that is ignored, exactly as
/// `Snapshot::drop` ignores it, because there is nothing a committing
/// transaction could do about it.
pub fn release_transaction_pins() {
    PINS.with(|pins| pins.borrow_mut().clear());
}

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
    /// The read view this transport is using.
    ///
    /// `Rc` because a pin minted by [`Transport::ticket_for`] is **also** held
    /// in `PINS`, which is what lets a later statement reach it. This field is
    /// then a cache: the statement that minted the pin need not go through the
    /// map to use it.
    pinned: Option<Rc<Snapshot>>,
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
///
/// # A bare-key expression is a key, and spending an evaluation on it is waste
///
/// `lower_all` returns `SetExpr::Key( k )` when **no** qual lowered, so an
/// unfiltered scan arrives here as an encoded expression rather than as eight
/// bytes. Served as an expression that costs `collect_set` -- the whole key
/// materialized in the server, per page -- where the key form is a streaming
/// chunk walk with a seek.
///
/// It is the same set either way, so this is a normalization and not a policy:
/// the planner is free to describe an unfiltered scan as `Key( k )`, and the
/// transport is free to notice that is not a filter. Caught the day pushdown was
/// enabled for this transport, because every unfiltered channel scan had been
/// taking the bare-key path by accident while `plan_pushdown` refused.
fn cmd_of(cmd: &[u8]) -> Result<Cmd, TransportError> {
    if yesno_wire::SetExpr::looks_like_expr(cmd) {
        if let Ok(yesno_wire::SetExpr::Key(key)) = yesno_wire::SetExpr::decode(cmd) {
            return Ok(Cmd::Key(key));
        }
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

    /// Pin a fresh read view, replacing any this transport was using.
    ///
    /// Not published to `PINS`: a pin only has to outlive its statement when a
    /// *ticket* names it, and publishing every read's snapshot would hold a
    /// reader slot per scan until the transaction ended.
    fn pin(&mut self) -> Result<Rc<Snapshot>, TransportError> {
        let snapshot = Rc::new(self.client()?.snapshot().map_err(rpc)?);
        self.pinned = Some(Rc::clone(&snapshot));
        Ok(snapshot)
    }

    /// Refuse an expression this server will not evaluate, naming the remedy.
    ///
    /// The client refuses it too, and says so accurately -- "it advertised
    /// max_expr_bytes 0" -- but that names a *daemon* setting to someone holding
    /// a SQL prompt. Reaching here means the planner lowered a qual because the
    /// `SERVER` says `pushdown` is on while the daemon says it evaluates
    /// nothing, and the fix is on the side the message is read from.
    fn refuse_if_not_evaluated(&mut self) -> Result<(), TransportError> {
        if self.client()?.limits().evaluates_expressions() {
            return Ok(());
        }
        Err(TransportError::Rpc(format!(
            "this yesnod evaluates no set expressions ( it advertised max_expr_bytes 0 ), \
             but a filter was pushed down to it. Set channel_max_expr_bytes on the \
             daemon, or ALTER SERVER ... OPTIONS ( ADD pushdown 'off' ) so the planner \
             filters in PostgreSQL instead. The socket is {}",
            self.socket
        )))
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
        if matches!(cmd, Cmd::Expr(_)) {
            self.refuse_if_not_evaluated()?;
        }
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
        if matches!(cmd, Cmd::Expr(_)) {
            self.refuse_if_not_evaluated()?;
        }
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
    fn ticket_for(
        &mut self,
        cmd: &[u8],
        at: Option<u64>,
    ) -> Result<(Vec<u8>, u64), TransportError> {
        // Classified before minting, so a descriptor this transport cannot serve
        // fails here rather than at the first page of a replay -- including the
        // server-evaluates-nothing case, which a ticket would otherwise carry
        // all the way to `next_batch`.
        if matches!(cmd_of(cmd)?, Cmd::Expr(_)) {
            self.refuse_if_not_evaluated()?;
        }
        let version = match at {
            // A later target in a scope that has already fixed its version.
            // **No new pin is taken**: the one the first target published is the
            // version, and this descriptor simply reads at it. That is why the
            // registry is keyed by version rather than by target.
            Some(want) => {
                let held =
                    PINS.with(|pins| pins.borrow().get(&(self.socket.clone(), want)).cloned());
                let Some(snapshot) = held else {
                    return Err(TransportError::Rpc(format!(
                        "this scope fixed version {want} and nothing in this session \
                         still pins it; the channel cannot reopen a version, so the \
                         transaction must restart. The socket is {}",
                        self.socket
                    )));
                };
                self.pinned = Some(snapshot);
                want
            }
            None => {
                let snapshot = self.pin()?;
                let version = snapshot.version();
                // **Published, which is the whole of what makes a ticket
                // replayable.** This transport is gone by the next statement;
                // the pin has to be somewhere that is not.
                PINS.with(|pins| {
                    pins.borrow_mut()
                        .insert((self.socket.clone(), version), snapshot);
                });
                version
            }
        };
        let mut ticket = Vec::with_capacity(8 + cmd.len());
        ticket.extend_from_slice(&version.to_le_bytes());
        ticket.extend_from_slice(cmd);
        Ok((ticket, version))
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

        // This transport's own pin first -- the statement that minted the ticket
        // is holding it -- then the transaction's registry, which is where a
        // *later* statement finds it. Served at the named version or not at all:
        // reading a different version than the ticket names is the one failure a
        // snapshot exists to prevent, so a missing pin is reported rather than
        // answered from whatever is current.
        let found = match self.pinned.as_ref() {
            Some(held) if held.version() == want => Some(Rc::clone(held)),
            _ => PINS.with(|pins| pins.borrow().get(&(self.socket.clone(), want)).cloned()),
        };
        let Some(snapshot) = found else {
            return Err(TransportError::Rpc(format!(
                "this ticket names version {want} and nothing in this session still pins \
                 it; the channel cannot reopen a version, so the transaction must \
                 restart. The socket is {}",
                self.socket
            )));
        };
        self.pinned = Some(snapshot);
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
        // Cloned rather than borrowed: the handle is counted, so this costs an
        // increment and leaves `self` free for the cursor update below.
        let snapshot = self
            .pinned
            .as_ref()
            .map(Rc::clone)
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
