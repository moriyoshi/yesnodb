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
//! # The ticket is opaque, on purpose
//!
//! `YesnoClient` validates the ticket returned by `get_flight_info`, but this
//! adapter stores and returns its raw bytes. The client-only build excludes the
//! Flight service and `yesno-core`, so PostgreSQL does not link a second engine.
//!
//! Do not "optimize" by constructing a ticket locally from the key. The
//! server's ticket carries the snapshot version the count was taken at; a
//! locally minted one would silently discard it.

use arrow_array::{Array, RecordBatch, UInt64Array};
use futures::StreamExt;
use tonic::transport::Channel;
use yesno_flight::YesnoClient;

use super::{OrdinalBatch, Transport, TransportError};
use crate::ordinal::ordinal_to_i64;

/// The column `yesno-flight`'s S1 schema carries.
const ORDINAL_COLUMN: &str = "ordinal";

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
            let channel = self
                .runtime
                .block_on(async move {
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
    fn cardinality(&mut self, cmd: &[u8]) -> Result<u64, TransportError> {
        self.client()?;
        let client = self.client.as_mut().expect("connected above");
        let info = self
            .runtime
            .block_on(client.prepare_command(cmd))
            .map_err(client_error)?;
        Ok(info.total_records())
    }

    fn ticket_for(&mut self, cmd: &[u8]) -> Result<Vec<u8>, TransportError> {
        self.client()?;
        let client = self.client.as_mut().expect("connected above");
        let info = self
            .runtime
            .block_on(client.prepare_command(cmd))
            .map_err(client_error)?;

        // The ticket is taken from the server response and handed back unread.
        // QueryInfo validates its shape but this adapter never constructs one.
        Ok(info.ticket_bytes().to_vec())
    }

    fn open_scan_with_ticket(&mut self, ticket: &[u8]) -> Result<(), TransportError> {
        self.close_scan();
        self.client()?;
        let client = self.client.as_mut().expect("connected above");
        let batches = self
            .runtime
            .block_on(client.fetch_ticket(ticket.to_vec()))
            .map_err(client_error)?;
        self.stream = Some(Box::pin(batches));
        Ok(())
    }

    fn open_scan(&mut self, cmd: &[u8]) -> Result<(), TransportError> {
        let ticket = self.ticket_for(cmd)?;
        self.open_scan_with_ticket(&ticket)
    }

    fn next_batch(&mut self) -> Result<Option<OrdinalBatch>, TransportError> {
        let Some(stream) = self.stream.as_mut() else {
            return Ok(None);
        };
        let next = self.runtime.block_on(stream.next());
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
        self.runtime.block_on(client.keys()).map_err(client_error)
    }

    fn put(&mut self, key: u64, ordinals: &[u64], remove: bool) -> Result<u64, TransportError> {
        if ordinals.is_empty() {
            return Ok(0);
        }
        self.client()?;
        let pairs = ordinals.iter().copied().map(|ordinal| (key, ordinal));
        let client = self.client.as_mut().expect("connected above");
        self.runtime
            .block_on(async move {
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
