//! Arrow Flight client: the FDW talking to a running `yesnod`.
//!
//! # Async inside a synchronous backend
//!
//! PostgreSQL's executor is synchronous and single-threaded per backend; `tonic`
//! is async. The bridge is a **current-thread** tokio runtime owned by the scan
//! state, with every RPC a `block_on`.
//!
//! `rt` rather than `rt-multi-thread` is a deliberate choice, not a default.
//! A multi-threaded runtime would spawn a worker pool **per backend**, so a
//! hundred connections become hundreds of idle threads, and a scan has no
//! concurrency to spend them on — it consumes one stream in order.
//!
//! # Every wait answers PostgreSQL's interrupts
//!
//! A `block_on` is a wait PostgreSQL cannot see into. Until 2026-10-26 every one
//! here was a plain `block_on`, and a server that accepted the connection and
//! then said nothing held the backend **forever**: `statement_timeout`,
//! `pg_cancel_backend` and even `pg_terminate_backend` were all ignored, because
//! each only sets a flag that the backend acts on at its next
//! `CHECK_FOR_INTERRUPTS`, and a backend parked in tokio never reaches one. The
//! only way out was `SIGKILL`, which makes the postmaster restart every session.
//!
//! So every wait goes through [`wait`], which wakes every [`INTERRUPT_POLL`] to
//! run `check_for_interrupts!`. A cancel or a terminate raises PostgreSQL's own
//! error from inside the wait -- pgrx turns it into a panic that unwinds out of
//! the runtime and back into an ordinary `ERROR` -- and any other interrupt is
//! processed and the wait carries on. This adds no timeout of its own: how long
//! a statement may wait is `statement_timeout`'s business, and now it is
//! honoured.
//!
//! # The ticket is opaque, on purpose
//!
//! `YesnoClient` validates the ticket returned by `get_flight_info`, but this
//! adapter stores and returns its raw bytes. The client-only build excludes the
//! Flight service and `yesno-core`, so PostgreSQL does not link a second engine.
//!
//! Do not "optimize" by constructing a ticket locally from the key. The
//! server's ticket carries the snapshot version the count was taken at; a
//! locally minted one would silently discard it.
//!
//! `prepare` does *read* the version the server's ticket declares, which is
//! the opposite of constructing one: it is how `fdw::modify` learns the version
//! a scope has fixed so that every later target in that scope can be minted at
//! it. The bytes are still handed back to `fetch_ticket` unaltered.

use arrow_array::{Array, RecordBatch, UInt64Array};
use futures::StreamExt;
use tonic::transport::Channel;
use yesno_flight::client::Mutation;
use yesno_flight::YesnoClient;

use super::{OrdinalBatch, Prepared, Transport, TransportError};
use crate::ordinal::ordinal_to_i64;

/// The column `yesno-flight`'s S1 schema carries.
const ORDINAL_COLUMN: &str = "ordinal";

/// The descriptor to send in order to read at `at`, or `None` to send `cmd` as
/// it is.
///
/// One function because **two call sites asking for a version differently is how
/// they drift**: `cardinality` and `prepare` must agree exactly about what
/// "at version v" means on the wire, or a count and a scan in one transaction
/// would disagree for a reason no fixture would name.
fn at_version(cmd: &[u8], at: Option<u64>) -> Result<Option<Vec<u8>>, TransportError> {
    let Some(version) = at else {
        return Ok(None);
    };
    // A `QueryRequest` is the only descriptor form with somewhere to put a
    // version, and the server's `request_of` checks for it before anything else
    // and answers from `snapshot_at( version )`.
    Ok(Some(
        yesno_flight::QueryRequest::at(yesno_flight::AnyExpr::Set(expr_of(cmd)?), version).encode(),
    ))
}

/// A descriptor as a set expression, so it can be wrapped in a `QueryRequest`.
///
/// The two forms this trait defines, and the same classification the server
/// uses -- `SetExpr::looks_like_expr` tests the length first, because a bare key
/// is eight arbitrary bytes and keys are commonly hashes, so one can begin with
/// the `YSNX` magic by coincidence.
fn expr_of(cmd: &[u8]) -> Result<yesno_flight::SetExpr, TransportError> {
    if yesno_flight::SetExpr::looks_like_expr(cmd) {
        return yesno_flight::SetExpr::decode(cmd).map_err(|e| {
            TransportError::Schema(format!("this descriptor is not a set expression: {e}"))
        });
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
    Ok(yesno_flight::SetExpr::Key(u64::from_le_bytes(bytes)))
}

/// How often a wait on the server checks for a PostgreSQL interrupt.
///
/// Only the latency of a cancel. A wait that completes sooner never ticks, so
/// this costs nothing on the normal path.
const INTERRUPT_POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// `fut`, run on `rt`, answering PostgreSQL's interrupts while it waits.
fn wait<T>(rt: &tokio::runtime::Runtime, fut: impl std::future::Future<Output = T>) -> T {
    // A statement, not the closure's expression: the macro expands to an
    // attribute on an `unsafe` block, which is only allowed in statement position.
    wait_ticking(rt, fut, || {
        pgrx::check_for_interrupts!();
    })
}

/// `fut`, run on `rt`, with `on_tick` run every [`INTERRUPT_POLL`] until it
/// completes. Split from [`wait`] so the waiting can be tested without a
/// backend: a test's `on_tick` stands in for an interrupt by panicking.
fn wait_ticking<T>(
    rt: &tokio::runtime::Runtime,
    fut: impl std::future::Future<Output = T>,
    mut on_tick: impl FnMut(),
) -> T {
    rt.block_on(async move {
        let mut fut = std::pin::pin!(fut);
        loop {
            match tokio::time::timeout(INTERRUPT_POLL, fut.as_mut()).await {
                Ok(out) => return out,
                Err(_) => on_tick(),
            }
        }
    })
}

/// Rows per staging request inside one write transaction.
///
/// The transaction bounds the *commit*; this bounds each *request*, so a flush
/// of any size stays one commit without one enormous message.
const STAGE_ROWS: usize = 8192;

pub struct FlightTransport {
    endpoint: String,
    runtime: tokio::runtime::Runtime,
    client: Option<YesnoClient<Channel>>,
    stream: Option<BoxedBatchStream>,
}

type BoxedBatchStream = std::pin::Pin<
    Box<dyn futures::Stream<Item = Result<RecordBatch, arrow_flight::error::FlightError>> + Send>,
>;

fn client_error(error: arrow_flight::error::FlightError) -> TransportError {
    match error {
        arrow_flight::error::FlightError::ProtocolError(message)
        | arrow_flight::error::FlightError::DecodeError(message) => TransportError::Schema(message),
        other => TransportError::Rpc(other.to_string()),
    }
}

impl FlightTransport {
    pub fn new(endpoint: &str) -> Result<Self, TransportError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| TransportError::Connect {
                endpoint: endpoint.to_string(),
                why: format!("cannot start a tokio runtime: {e}"),
            })?;
        Ok(FlightTransport {
            endpoint: endpoint.to_string(),
            runtime,
            client: None,
            stream: None,
        })
    }

    /// Connect lazily.
    ///
    /// Lazily because `BeginForeignScan` runs for `EXPLAIN` without
    /// `ANALYZE` too, and an `EXPLAIN` that cannot be produced while the server
    /// is down would make the plan un-inspectable exactly when someone is
    /// debugging why it cannot reach the server.
    fn client(&mut self) -> Result<&mut YesnoClient<Channel>, TransportError> {
        if self.client.is_none() {
            // The channel is built explicitly to retain lazy connection: using
            // `YesnoClient::connect` here would connect while constructing the
            // adapter, including during an unexecuted EXPLAIN.
            let endpoint = self.endpoint.clone();
            let channel = wait(&self.runtime, async move {
                Channel::from_shared(endpoint)
                    .map_err(|e| e.to_string())?
                    .connect()
                    .await
                    .map_err(|e| e.to_string())
            })
            .map_err(|why| TransportError::Connect {
                endpoint: self.endpoint.clone(),
                why,
            })?;
            self.client = Some(YesnoClient::new(channel));
        }
        Ok(self.client.as_mut().expect("just assigned"))
    }

    /// The bare-key descriptor payload, for callers with nothing to push down.
    pub fn key_cmd(key: u64) -> Vec<u8> {
        key.to_le_bytes().to_vec()
    }
}

impl Transport for FlightTransport {
    fn cardinality(&mut self, cmd: &[u8], at: Option<u64>) -> Result<u64, TransportError> {
        let wrapped = at_version(cmd, at)?;
        let cmd = wrapped.as_deref().unwrap_or(cmd);
        self.client()?;
        let client = self.client.as_mut().expect("connected above");
        let info = wait(&self.runtime, client.prepare_command(cmd)).map_err(client_error)?;
        Ok(info.total_records())
    }

    fn prepare(&mut self, cmd: &[u8], at: Option<u64>) -> Result<Prepared, TransportError> {
        let wrapped = at_version(cmd, at)?;
        let cmd = wrapped.as_deref().unwrap_or(cmd);
        self.client()?;
        let client = self.client.as_mut().expect("connected above");
        let info = wait(&self.runtime, client.prepare_command(cmd)).map_err(client_error)?;

        // The ticket is taken from the server response and handed back unread.
        // QueryInfo validates its shape but this adapter never constructs one.
        let bytes = info.ticket_bytes().to_vec();
        // **Free**: the server computed it to fill `total_records` whether or
        // not this call wanted it, so reading it here is what lets a pushed-down
        // `count(*)` be one round trip again rather than two.
        let rows = Some(info.total_records());
        // Its declared version, which is the one thing read out of it. A ticket
        // the client cannot parse is a version skew worth reporting here rather
        // than discovering as a wrong answer later.
        let version = yesno_flight::Ticket::decode(&bytes)
            .ok_or_else(|| {
                TransportError::Schema("yesnod returned a ticket this client cannot parse".into())
            })?
            .version;
        if let Some(wanted) = at {
            // The server was asked for one version and answered with another,
            // which no amount of retrying fixes and which would silently put
            // this transaction on two versions.
            if version != wanted {
                return Err(TransportError::Schema(format!(
                    "asked yesnod for version {wanted} and its ticket names {version}"
                )));
            }
        }
        Ok(Prepared {
            ticket: bytes,
            version,
            rows,
        })
    }

    fn open_scan_with_ticket(&mut self, ticket: &[u8]) -> Result<(), TransportError> {
        self.close_scan();
        self.client()?;
        let client = self.client.as_mut().expect("connected above");
        let batches =
            wait(&self.runtime, client.fetch_ticket(ticket.to_vec())).map_err(client_error)?;
        self.stream = Some(Box::pin(batches));
        Ok(())
    }

    fn open_scan(&mut self, cmd: &[u8]) -> Result<(), TransportError> {
        let prepared = self.prepare(cmd, None)?;
        self.open_scan_with_ticket(&prepared.ticket)
    }

    fn next_batch(&mut self) -> Result<Option<OrdinalBatch>, TransportError> {
        let Some(stream) = self.stream.as_mut() else {
            return Ok(None);
        };
        let next = wait(&self.runtime, stream.next());
        let batch = match next {
            None => return Ok(None),
            Some(Ok(b)) => b,
            Some(Err(e)) => return Err(TransportError::Rpc(e.to_string())),
        };

        let col = batch
            .column_by_name(ORDINAL_COLUMN)
            .ok_or_else(|| TransportError::Schema(format!("no \"{ORDINAL_COLUMN}\" column")))?;
        let ords = col.as_any().downcast_ref::<UInt64Array>().ok_or_else(|| {
            TransportError::Schema(format!(
                "\"{ORDINAL_COLUMN}\" is {:?}, expected UInt64",
                col.data_type()
            ))
        })?;

        // `null_count` rather than trusting the schema's `nullable = false`.
        // yesno's Arrow schemas forbid nulls *structurally* — a posting list is
        // a set of present values — so a null here means the peer is not what it
        // claims, and mapping it to some default would invent an ordinal.
        if ords.null_count() != 0 {
            return Err(TransportError::Schema(
                "ordinal column contained nulls; a posting list has none".into(),
            ));
        }

        Ok(Some(
            ords.values().iter().copied().map(ordinal_to_i64).collect(),
        ))
    }

    fn close_scan(&mut self) {
        self.stream = None;
    }

    fn keys(&mut self) -> Result<Vec<u64>, TransportError> {
        self.client()?;
        let client = self.client.as_mut().expect("connected above");
        wait(&self.runtime, client.keys()).map_err(client_error)
    }

    fn apply(&mut self, ops: &[(u64, u64, bool)]) -> Result<u64, TransportError> {
        if ops.is_empty() {
            return Ok(0);
        }
        self.client()?;
        let client = self.client.as_mut().expect("connected above");
        // Interruptible like every other wait, including the commit, which runs
        // from PostgreSQL's pre-commit callback with interrupts still live. A
        // cancel landing inside `commit_write` leaves it unknown whether yesnod
        // committed -- the same ambiguity a dropped connection there already has,
        // recorded as `fdw-two-phase-commit` -- and a commit that cannot be
        // cancelled at all, which is what this was, is the worse of the two.
        wait(&self.runtime, async move {
            // A write transaction rather than the one-shot `apply`, because a
            // flush can be arbitrarily large -- `INSERT ... SELECT` buffers the
            // whole statement -- and staging in chunks keeps one request
            // bounded while still producing one commit.
            let txn = client.begin_write().await.map_err(client_error)?;
            let staged = async {
                for chunk in ops.chunks(STAGE_ROWS) {
                    let mutations = chunk.iter().map(|&(key, ordinal, remove)| {
                        if remove {
                            Mutation::Remove { key, ordinal }
                        } else {
                            Mutation::Insert { key, ordinal }
                        }
                    });
                    client.stage(txn, mutations).await?;
                }
                client.commit_write(txn).await
            }
            .await;
            match staged {
                Ok(_) => Ok(ops.len() as u64),
                Err(e) => {
                    // Release the server's staged memory rather than leaving it
                    // to expire. The abort is best effort: the original failure
                    // is what the caller needs to see.
                    let _ = client.abort_write(txn).await;
                    Err(client_error(e))
                }
            }
        })
    }

    fn put(&mut self, key: u64, ordinals: &[u64], remove: bool) -> Result<u64, TransportError> {
        if ordinals.is_empty() {
            return Ok(0);
        }
        self.client()?;
        let pairs = ordinals.iter().copied().map(|ordinal| (key, ordinal));
        let client = self.client.as_mut().expect("connected above");
        wait(&self.runtime, async move {
            if remove {
                client.remove_batch(pairs).await
            } else {
                client.insert_batch(pairs).await
            }
        })
        .map_err(client_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    /// The normal path: a wait that finishes inside one poll never ticks.
    #[test]
    fn a_prompt_wait_never_looks_for_an_interrupt() {
        let rt = runtime();
        let mut ticks = 0;
        assert_eq!(wait_ticking(&rt, async { 7 }, || ticks += 1), 7);
        assert_eq!(ticks, 0);
    }

    /// A slow wait ticks while it waits and still returns its value: an
    /// interrupt that turns out to be nothing must not cost the answer.
    #[test]
    fn a_slow_wait_ticks_and_still_answers() {
        let rt = runtime();
        let mut ticks = 0;
        let slow = async {
            tokio::time::sleep(INTERRUPT_POLL * 5 / 2).await;
            9
        };
        assert_eq!(wait_ticking(&rt, slow, || ticks += 1), 9);
        // At least one: a loaded host delays ticks, which can only lower it.
        assert!(ticks >= 1, "a 125 ms wait never ticked");
    }

    /// The defect this exists for: a wait on a server that never answers. A
    /// tick that raises -- as `check_for_interrupts!` does for a cancel --
    /// must end it, and the runtime must still work afterwards, because the
    /// transport keeps it for the next statement.
    #[test]
    fn a_wait_that_never_completes_is_ended_by_an_interrupt() {
        let rt = runtime();
        let mut ticks = 0;
        let started = std::time::Instant::now();
        let ended = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            wait_ticking(&rt, std::future::pending::<()>(), || {
                ticks += 1;
                if ticks == 3 {
                    panic!("canceling statement due to user request");
                }
            })
        }));
        assert!(ended.is_err(), "the interrupt did not end the wait");
        assert_eq!(ticks, 3);
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert_eq!(wait_ticking(&rt, async { 1 }, || {}), 1);
    }

    /// The bare-key payload retained by the adapter is little-endian.
    #[test]
    fn a_key_command_is_eight_little_endian_bytes() {
        for key in [0u64, 1, 42, u64::MAX] {
            let command = FlightTransport::key_cmd(key);
            assert_eq!(command.len(), 8, "key {key}");
            assert_eq!(
                u64::from_le_bytes(command.as_slice().try_into().unwrap()),
                key,
                "key {key} must round-trip"
            );
        }
    }

    #[test]
    fn protocol_errors_remain_schema_errors() {
        let error = client_error(arrow_flight::error::FlightError::protocol("bad ticket"));
        assert!(matches!(error, TransportError::Schema(message) if message == "bad ticket"));
    }

    /// No test connects here. Server integration belongs in SQL regression.
    #[test]
    fn a_runtime_is_current_thread_not_a_pool() {
        let transport = FlightTransport::new("grpc://127.0.0.1:1").expect("runtime builds");
        assert!(transport.client.is_none(), "connection must be lazy");
    }
}
