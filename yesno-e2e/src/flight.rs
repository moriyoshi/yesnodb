//! `flight_*`: the shipped Arrow Flight service, over a real loopback socket.
//!
//! # What these verbs are for
//!
//! `yesno-flight/tests/roundtrip.rs` already pins the protocol's shape. What it
//! cannot express cheaply is the *sequence an operator or a client library
//! performs* — open, ingest over the wire, ask for the count without fetching,
//! fetch, force a checkpoint, stop, reopen and check it survived — because each
//! variation of it is a recompile.
//!
//! **And the comparison is the payoff.** `flight_get` hands back a plain
//! Python list, so the expectation is `sorted( set(...) )` of an expression the
//! scenario wrote itself. That is an oracle. A Rust test comparing what came off
//! the socket against what `Snapshot::load` says is comparing yesno against
//! yesno.
//!
//! # The rule these verbs obey
//!
//! They drive the **shipped** `YesnoFlightService` and a real
//! `FlightServiceClient` and nothing else. No verb hands a scenario decoded
//! `FlightData` to reassemble itself — that would be a second Flight client
//! written in the scenario language, which is the TESTING §4 failure exactly.
//!
//! # A `Db` clone, and why the harness counts them
//!
//! `YesnoFlightService::new` takes an `Arc<Db>`, and `Db` is `Clone` over a
//! shared `Arc<DbInner>` — so the service holds the *same* store as the handle
//! it was served from, and therefore the same exclusive directory lock.
//! Closing the database underneath a live service would leave the next
//! `db_open` of that directory failing with a lock error several statements away
//! from the cause, which is precisely what the `live_snaps` guard already exists
//! to prevent. `flight_serve` increments the same kind of counter and
//! `flight_stop` releases it.

use std::sync::Arc;

use arrow_array::{Array, UInt64Array};
use arrow_flight::flight_service_client::FlightServiceClient;
use arrow_flight::flight_service_server::FlightServiceServer;
use arrow_flight::{Action, Empty, FlightDescriptor};
use futures::StreamExt;
use monty_types::{MontyException, MontyObject};
use tonic::transport::{Channel, Server};
use yesno_flight::{ServerStats, Ticket, YesnoFlightService};

use crate::convert::{db_err, dict, int_obj, value_err, Args};
use crate::world::{stale_handle, HandleKind, World};

/// The verbs this module dispatches. Merged into [`crate::world::all_names`].
pub const OWNS: &[&str] = &[
    "flight_serve",
    "flight_location",
    "flight_stop",
    "flight_info",
    "flight_get",
    "flight_view_select",
    "flight_view_fold",
    "flight_ticket",
    "flight_fetch",
    "flight_put",
    "flight_action",
    "flight_actions",
    "flight_expect_term",
    "flight_term",
];

/// A Flight service on loopback, plus a client connected to it.
pub(crate) struct Endpoint {
    pub(crate) client: FlightServiceClient<Channel>,
    /// Reusable only for a service created by `flight_serve`.
    location: Option<String>,
    /// The lowest leadership term this client will accept, sent on every call.
    ///
    /// The client-facing half of split-brain fencing: a superseded leader does
    /// not know it has been replaced, so the caller is the only party that can
    /// hold the newer number.
    pub(crate) expect_term: Option<u32>,
    /// Sent as `Authorization: Bearer` on every call.
    ///
    /// Held here and injected per request rather than baked into an
    /// interceptor, because `with_interceptor` changes the client's **type** —
    /// and then an endpoint with a token and one without could not share a
    /// handle table, which is exactly what a scenario comparing them needs.
    pub(crate) token: Option<String>,
    /// Present only for a service **this module started**. An endpoint handed
    /// over by `srv_flight` points at a daemon that owns its own socket, and
    /// stopping that is `srv_stop`'s job, not `flight_stop`'s.
    owned: Option<Owned>,
    /// The database handle this was served from, so `flight_stop` can release
    /// the hold that keeps `db_close` refusing. `None` for a daemon-backed
    /// endpoint, which has no harness database handle behind it.
    db_handle: Option<usize>,
}

impl Endpoint {
    /// Wrap a value in a request carrying this endpoint's credential.
    fn req<T>(&self, v: T) -> tonic::Request<T> {
        let mut r = tonic::Request::new(v);
        if let Some(t) = &self.token {
            r.metadata_mut()
                .insert("authorization", format!("Bearer {t}").parse().unwrap());
        }
        if let Some(n) = self.expect_term {
            r.metadata_mut().insert(
                yesno_server::guard::EXPECT_TERM,
                n.to_string().parse().expect("a number is valid ASCII"),
            );
        }
        r
    }
}

/// The half of an endpoint that exists only when the harness is the server.
struct Owned {
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    /// The serving task, so `flight_stop` can **wait** for it rather than
    /// merely ask. See the note there — this is the difference between a
    /// deterministic teardown and a sleep.
    served: Option<tokio::task::JoinHandle<()>>,
}

/// Everything the `flight_*` verbs own, kept out of `World`'s own fields so
/// that `world.rs` does not have to know Arrow Flight exists.
#[derive(Default)]
pub struct FlightState {
    pub(crate) endpoints: Vec<Option<Endpoint>>,
}

/// A gRPC failure, formatted so a scenario can assert on the **code**.
///
/// `Status`'s own `Display` prints the code's *description* — "The caller does
/// not have permission to execute the specified operation" — and not its name.
/// A scenario asserting on prose is a scenario that breaks when tonic rewords a
/// sentence, so the name goes in front: `PermissionDenied`, `Unauthenticated`,
/// `InvalidArgument`. That distinction is the one a client acts on.
///
/// A `RuntimeError`, not a `ValueError`. In this harness `ValueError` means
/// *the scenario* passed something wrong and `RuntimeError` means yesno refused
/// the operation — and a server declining a call is squarely the second. Getting
/// it backwards makes a scenario catch the wrong exception and then pass for the
/// wrong reason.
fn grpc_err(verb: &str, st: tonic::Status) -> MontyException {
    db_err(verb, format!("{:?}: {}", st.code(), st.message()))
}

/// The descriptor form the service's `key_of` accepts without reparsing: an
/// 8-byte little-endian key in `cmd`.
fn descriptor(key: u64) -> FlightDescriptor {
    FlightDescriptor::new_cmd(key.to_le_bytes().to_vec())
}

fn view_spec(view: yesno_core::view::View) -> yesno_flight::ViewSpec {
    match view.layout() {
        yesno_core::view::ViewLayout::Interleaved => {
            yesno_flight::ViewSpec::interleaved(view.sets())
        }
        yesno_core::view::ViewLayout::Blocked { stride } => {
            yesno_flight::ViewSpec::blocked(view.sets(), stride)
        }
    }
}

async fn query_descriptor(
    ep: &mut Endpoint,
    descriptor: FlightDescriptor,
    verb: &str,
) -> Result<(u64, Vec<u64>), MontyException> {
    let info = ep
        .client
        .get_flight_info(ep.req(descriptor))
        .await
        .map_err(|e| grpc_err(verb, e))?
        .into_inner();
    let ticket = info
        .endpoint
        .first()
        .and_then(|endpoint| endpoint.ticket.clone())
        .ok_or_else(|| value_err(format!("{verb}(): no ticket to fetch with")))?;
    let stream = ep
        .client
        .do_get(ep.req(ticket))
        .await
        .map_err(|e| grpc_err(verb, e))?
        .into_inner()
        .map(|r| r.map_err(|e| arrow_flight::error::FlightError::Tonic(Box::new(e))));
    let mut batches = arrow_flight::decode::FlightRecordBatchStream::new_from_flight_data(stream);
    let mut ords = Vec::new();
    while let Some(batch) = batches.next().await {
        let batch = batch.map_err(|e| db_err(verb, e))?;
        let col = batch
            .column_by_name("ordinal")
            .and_then(|column| column.as_any().downcast_ref::<UInt64Array>())
            .ok_or_else(|| value_err(format!("{verb}(): no UInt64 `ordinal` column")))?;
        ords.extend((0..col.len()).map(|i| col.value(i)));
    }
    Ok((info.total_records as u64, ords))
}

fn query_result(total_records: u64, rows: Vec<u64>) -> MontyObject {
    dict(vec![
        ("total_records", int_obj(total_records)),
        (
            "rows",
            MontyObject::List(rows.into_iter().map(int_obj).collect()),
        ),
    ])
}

impl World {
    fn endpoint(&mut self, h: usize, verb: &str) -> Result<&mut Endpoint, MontyException> {
        let i = self.slot(h, HandleKind::Flight, verb)?;
        self.flight.endpoints[i]
            .as_mut()
            .ok_or_else(|| stale_handle(verb, "flight endpoint", h))
    }

    pub(crate) fn call_flight(
        &mut self,
        verb: &str,
        a: &Args<'_>,
    ) -> Result<MontyObject, MontyException> {
        match verb {
            // Serve an already-open database. The service takes a `Db` clone,
            // so it reads the same store the scenario is writing through — which
            // is what lets a scenario write with `db_insert` and read back over
            // the wire.
            "flight_serve" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let h = a.handle(0)?;
                let db = self.db_clone(h, verb)?;
                let rt = self.rt_handle(verb)?;

                let (client, location, shutdown, served) = rt
                    .block_on(async move {
                        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
                        let addr = listener.local_addr()?;
                        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
                        let served = tokio::spawn(async move {
                            let _ = Server::builder()
                                .add_service(FlightServiceServer::new(YesnoFlightService::new(
                                    Arc::new(db),
                                )))
                                .serve_with_incoming_shutdown(
                                    tokio_stream::wrappers::TcpListenerStream::new(listener),
                                    async {
                                        let _ = rx.await;
                                    },
                                )
                                .await;
                        });
                        let ch = Channel::from_shared(format!("http://{addr}"))
                            .map_err(std::io::Error::other)?
                            .connect()
                            .await
                            .map_err(std::io::Error::other)?;
                        Ok::<_, std::io::Error>((
                            FlightServiceClient::new(ch),
                            format!("grpc+tcp://{addr}"),
                            tx,
                            served,
                        ))
                    })
                    .map_err(|e| db_err(verb, e))?;

                self.hold_db_for_flight(h, verb)?;
                self.flight.endpoints.push(Some(Endpoint {
                    client,
                    location: Some(location),
                    token: None,
                    expect_term: None,
                    owned: Some(Owned {
                        shutdown: Some(shutdown),
                        served: Some(served),
                    }),
                    db_handle: Some(h),
                }));
                let idx = self.flight.endpoints.len() - 1;
                Ok(self.mint(HandleKind::Flight, idx))
            }

            "flight_location" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let location = self
                    .endpoint(a.handle(0)?, verb)?
                    .location
                    .clone()
                    .ok_or_else(|| {
                        value_err(format!(
                            "{verb}(): this endpoint has no reusable loopback location"
                        ))
                    })?;
                Ok(MontyObject::String(location))
            }

            // Stop serving. Invalidating rather than merely stopping, so a
            // scenario that keeps using the handle says so loudly.
            "flight_stop" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let h = a.handle(0)?;
                let rt = self.rt_handle(verb)?;
                let i = self.slot(h, HandleKind::Flight, verb)?;
                match self.flight.endpoints[i].take() {
                    None => Err(stale_handle(verb, "flight endpoint", h)),
                    Some(e) => {
                        // **Signal, drop, then wait — and the waiting is the
                        // part that matters.** The serving task owns the
                        // `Arc<Db>`, and a `Db` holds the directory's exclusive
                        // lock, so a `flight_stop` that only *asks* leaves the
                        // lock held for however long the task takes to unwind.
                        // A scenario that then closes and reopens the directory
                        // fails with `AlreadyOpen` — nondeterministically, which
                        // is the worst way to fail.
                        //
                        // The client is dropped in between because
                        // `serve_with_incoming_shutdown` waits for live
                        // connections as well as for the signal; holding the
                        // channel open would hang the join.
                        let Endpoint {
                            client,
                            location: _,
                            owned,
                            db_handle,
                            token: _,
                            expect_term: _,
                        } = e;
                        match owned {
                            Some(mut o) => {
                                if let Some(tx) = o.shutdown.take() {
                                    let _ = tx.send(());
                                }
                                drop(client);
                                if let Some(served) = o.served.take() {
                                    let _ = rt.block_on(served);
                                }
                            }
                            // A daemon's endpoint: only the client is ours.
                            None => drop(client),
                        }
                        if let Some(h) = db_handle {
                            self.release_db_for_flight(h);
                        }
                        Ok(MontyObject::None)
                    }
                }
            }

            // `get_flight_info`, whose `total_records` is the design's headline:
            // exact, and taken from container popcounts in the index without
            // materializing an ordinal or reading a payload extent.
            "flight_info" => {
                a.exact(2)?;
                a.no_kwargs()?;
                let (h, key) = (a.handle(0)?, a.u64(1)?);
                let rt = self.rt_handle(verb)?;
                let ep = self.endpoint(h, verb)?;
                let info = rt
                    .block_on(ep.client.get_flight_info(ep.req(descriptor(key))))
                    .map_err(|e| grpc_err(verb, e))?
                    .into_inner();

                let endpoint = info.endpoint.first().ok_or_else(|| {
                    value_err(format!("{verb}(): the server returned no endpoint"))
                })?;
                let raw = endpoint.ticket.as_ref().ok_or_else(|| {
                    value_err(format!("{verb}(): the endpoint carries no ticket"))
                })?;
                let t = Ticket::decode(&raw.ticket)
                    .ok_or_else(|| value_err(format!("{verb}(): the ticket did not decode")))?;

                Ok(dict(vec![
                    ("total_records", int_obj(info.total_records as u64)),
                    // The ticket's snapshot version, and since 2026-08-29 the
                    // version a subsequent `do_get` actually answers from — see
                    // `flight_ticket` / `flight_fetch`, which exist so a
                    // scenario can put a write between the two calls and hold
                    // the service to it.
                    ("version", int_obj(t.version)),
                    ("key", int_obj(t.key)),
                    ("ordered", MontyObject::Bool(info.ordered)),
                ]))
            }

            // `get_flight_info`, keeping the ticket. The split from
            // `flight_get` is the entire point: `flight_get` mints and fetches
            // in one host call, so **nothing can happen in between** and it can
            // never observe whether the version field is honoured. A scenario
            // that writes between these two verbs can.
            //
            // The ticket comes back as a list of ints rather than a handle
            // because it is a value, not a resource — 40 opaque bytes a scenario
            // stores, passes around and may deliberately present late or to the
            // wrong server. A handle would imply a lifetime it does not have.
            "flight_ticket" => {
                a.exact(2)?;
                a.no_kwargs()?;
                let (h, key) = (a.handle(0)?, a.u64(1)?);
                let rt = self.rt_handle(verb)?;
                let ep = self.endpoint(h, verb)?;
                let info = rt
                    .block_on(ep.client.get_flight_info(ep.req(descriptor(key))))
                    .map_err(|e| grpc_err(verb, e))?
                    .into_inner();
                let raw = info
                    .endpoint
                    .first()
                    .and_then(|e| e.ticket.as_ref())
                    .ok_or_else(|| value_err(format!("{verb}(): the endpoint carries no ticket")))?
                    .ticket
                    .clone();
                let t = Ticket::decode(&raw)
                    .ok_or_else(|| value_err(format!("{verb}(): the ticket did not decode")))?;
                Ok(dict(vec![
                    ("total_records", int_obj(info.total_records as u64)),
                    ("version", int_obj(t.version)),
                    ("key", int_obj(t.key)),
                    (
                        "raw",
                        MontyObject::List(raw.iter().map(|&b| int_obj(b as u64)).collect()),
                    ),
                ]))
            }

            // `do_get` with a ticket the scenario supplies, which may be one
            // minted arbitrarily long ago.
            "flight_fetch" => {
                a.exact(2)?;
                a.no_kwargs()?;
                let h = a.handle(0)?;
                let raw: Vec<u8> = a
                    .u64_list(1)?
                    .into_iter()
                    .map(|b| {
                        u8::try_from(b).map_err(|_| {
                            value_err(format!(
                                "{verb}(): {b} is not a byte; pass a ticket's `raw`"
                            ))
                        })
                    })
                    .collect::<Result<_, _>>()?;
                let rt = self.rt_handle(verb)?;
                let ep = self.endpoint(h, verb)?;

                let out: Result<Vec<u64>, MontyException> = rt.block_on(async {
                    let mut stream = ep
                        .client
                        .do_get(ep.req(arrow_flight::Ticket::new(raw)))
                        .await
                        .map_err(|e| grpc_err(verb, e))?
                        .into_inner();
                    // Drained to completion **before** decoding, because
                    // `FlightRecordBatchStream` rewraps whatever its inner
                    // stream yields as `ExternalError` — which turns every
                    // server refusal into `Internal` with the real code buried
                    // in the message. A scenario asserting that a stale ticket
                    // is `FailedPrecondition` would then be asserting on prose.
                    let mut data = Vec::new();
                    while let Some(d) = stream.message().await.map_err(|e| grpc_err(verb, e))? {
                        data.push(Ok(d));
                    }
                    let mut batches =
                        arrow_flight::decode::FlightRecordBatchStream::new_from_flight_data(
                            futures::stream::iter(data),
                        );
                    let mut ords = Vec::new();
                    while let Some(b) = batches.next().await {
                        let b = b.map_err(|e| db_err(verb, e))?;
                        let col = b
                            .column_by_name("ordinal")
                            .and_then(|c| c.as_any().downcast_ref::<UInt64Array>())
                            .ok_or_else(|| {
                                value_err(format!("{verb}(): no UInt64 `ordinal` column"))
                            })?;
                        ords.extend((0..col.len()).map(|i| col.value(i)));
                    }
                    Ok(ords)
                });
                Ok(MontyObject::List(out?.into_iter().map(int_obj).collect()))
            }

            // The whole round trip: ticket from `get_flight_info`, then
            // `do_get`, decoded by the shipped Arrow decoder.
            "flight_get" => {
                a.exact(2)?;
                a.no_kwargs()?;
                let (h, key) = (a.handle(0)?, a.u64(1)?);
                let rt = self.rt_handle(verb)?;
                let ep = self.endpoint(h, verb)?;

                let out: Result<Vec<u64>, MontyException> = rt.block_on(async {
                    let info = ep
                        .client
                        .get_flight_info(ep.req(descriptor(key)))
                        .await
                        .map_err(|e| grpc_err(verb, e))?
                        .into_inner();
                    let ticket = info
                        .endpoint
                        .first()
                        .and_then(|e| e.ticket.clone())
                        .ok_or_else(|| value_err(format!("{verb}(): no ticket to fetch with")))?;

                    let stream = ep
                        .client
                        .do_get(ep.req(ticket))
                        .await
                        .map_err(|e| grpc_err(verb, e))?
                        .into_inner()
                        .map(|r| {
                            r.map_err(|e| arrow_flight::error::FlightError::Tonic(Box::new(e)))
                        });
                    let mut batches =
                        arrow_flight::decode::FlightRecordBatchStream::new_from_flight_data(stream);

                    let mut ords = Vec::new();
                    while let Some(b) = batches.next().await {
                        let b = b.map_err(|e| db_err(verb, e))?;
                        let col = b
                            .column_by_name("ordinal")
                            .and_then(|c| c.as_any().downcast_ref::<UInt64Array>())
                            .ok_or_else(|| {
                                value_err(format!("{verb}(): no UInt64 `ordinal` column"))
                            })?;
                        ords.extend((0..col.len()).map(|i| col.value(i)));
                    }
                    Ok(ords)
                });

                Ok(MontyObject::List(out?.into_iter().map(int_obj).collect()))
            }

            "flight_view_select" => {
                a.exact(4)?;
                a.no_kwargs()?;
                let h = a.handle(0)?;
                let key = a.u64(1)?;
                let view = view_spec(*self.view(a.handle(2)?, verb)?);
                let set = u32::try_from(a.u64(3)?)
                    .map_err(|_| value_err(format!("{verb}(): constituent must fit in u32")))?;
                if set >= view.sets {
                    return Err(value_err(format!(
                        "{verb}(): constituent {set} is outside 0..{}",
                        view.sets
                    )));
                }
                let expression = yesno_flight::SetExpr::At(
                    Box::new(yesno_flight::VecSetExpr::View(
                        Box::new(yesno_flight::SetExpr::Key(key)),
                        view,
                    )),
                    set,
                );
                let rt = self.rt_handle(verb)?;
                let ep = self.endpoint(h, verb)?;
                let (total, rows) = rt.block_on(query_descriptor(
                    ep,
                    FlightDescriptor::new_cmd(expression.encode()),
                    verb,
                ))?;
                Ok(query_result(total, rows))
            }

            "flight_view_fold" => {
                a.exact(4)?;
                a.no_kwargs()?;
                let h = a.handle(0)?;
                let key = a.u64(1)?;
                let view = view_spec(*self.view(a.handle(2)?, verb)?);
                // The operator is named for the Boolean operation being
                // folded, matching the wire and the CLI. The old spelling was
                // `any` / `all` / `parity`.
                let op = match a.str_at(3)? {
                    "or" => yesno_flight::FoldOp::Or,
                    "and" => yesno_flight::FoldOp::And,
                    "xor" => yesno_flight::FoldOp::Xor,
                    other => {
                        return Err(value_err(format!(
                            "{verb}(): unknown fold operator {other:?}; expected \"or\", \"and\" or \"xor\""
                        )))
                    }
                };
                let expression = yesno_flight::SetExpr::Fold(
                    Box::new(yesno_flight::VecSetExpr::View(
                        Box::new(yesno_flight::SetExpr::Key(key)),
                        view,
                    )),
                    op,
                );
                let rt = self.rt_handle(verb)?;
                let ep = self.endpoint(h, verb)?;
                let (total, rows) = rt.block_on(query_descriptor(
                    ep,
                    FlightDescriptor::new_cmd(expression.encode()),
                    verb,
                ))?;
                Ok(query_result(total, rows))
            }

            // Bulk ingest as `( key, ordinal )` pairs, which is `do_put`'s only
            // shape. Two parallel lists rather than a list of tuples: the
            // harness already converts `u64` lists and a tuple list would be a
            // second conversion for no expressive gain.
            "flight_put" => {
                a.exact(3)?;
                a.no_kwargs()?;
                let h = a.handle(0)?;
                let keys = a.u64_list(1)?;
                let ords = a.u64_list(2)?;
                if keys.len() != ords.len() {
                    return Err(value_err(format!(
                        "{verb}(): {} keys and {} ordinals; do_put takes pairs, so the lists \
                         must be the same length",
                        keys.len(),
                        ords.len()
                    )));
                }
                if keys.is_empty() {
                    return Err(value_err(format!("{verb}(): nothing to ingest")));
                }
                let n = keys.len();
                let rt = self.rt_handle(verb)?;
                let ep = self.endpoint(h, verb)?;

                let schema = yesno_flight::pairs_schema();
                let batch = arrow_array::RecordBatch::try_new(
                    schema.clone(),
                    vec![
                        Arc::new(UInt64Array::new(keys.into(), None)),
                        Arc::new(UInt64Array::new(ords.into(), None)),
                    ],
                )
                .map_err(|e| db_err(verb, e))?;
                let input = arrow_flight::encode::FlightDataEncoderBuilder::new()
                    .with_schema(schema)
                    .build(futures::stream::iter(vec![Ok(batch)]))
                    .map(|r| r.expect("a locally built batch encodes"));

                let rows = rt.block_on(async {
                    let mut acked = ep
                        .client
                        .do_put(ep.req(input))
                        .await
                        .map_err(|e| grpc_err(verb, e))?
                        .into_inner();
                    let mut rows = 0u64;
                    while let Some(r) = acked.next().await {
                        let r = r.map_err(|e| grpc_err(verb, e))?;
                        // The row count is the **leading** `u64`; the field
                        // also carries the commit version since 2026-09-12. An
                        // earlier `if len == 8 { .. }` with no `else` reported
                        // `rows = 0` for any other width, so widening the field
                        // surfaced as "the server acknowledged 0" rather than as
                        // a decoding error — 11 scenarios failed with a message
                        // that pointed at the wrong thing entirely.
                        let md = r.app_metadata.as_ref();
                        if !matches!(md.len(), 8 | 16) {
                            return Err(value_err(format!(
                                "{verb}(): ingest acknowledgement has {} bytes, expected 8 or 16",
                                md.len()
                            )));
                        }
                        rows = u64::from_le_bytes(md[..8].try_into().unwrap());
                    }
                    Ok::<_, MontyException>(rows)
                })?;

                if rows as usize != n {
                    return Err(value_err(format!(
                        "{verb}(): sent {n} pairs and the server acknowledged {rows}"
                    )));
                }
                Ok(int_obj(rows))
            }

            // `do_action` results keep their wire contract here: protobuf stats
            // become a typed dict. Scenarios should never interpret raw
            // protocol bytes.
            "flight_action" => {
                a.exact(2)?;
                a.no_kwargs()?;
                let h = a.handle(0)?;
                let name = a.str_at(1)?.to_owned();
                let stats_action = name == "stats";
                let rt = self.rt_handle(verb)?;
                let ep = self.endpoint(h, verb)?;

                let body = rt
                    .block_on(async {
                        let mut s = ep
                            .client
                            .do_action(ep.req(Action {
                                r#type: name,
                                body: Default::default(),
                            }))
                            .await?
                            .into_inner();
                        let mut out = Vec::new();
                        while let Some(r) = s.next().await {
                            out.extend_from_slice(&r?.body);
                        }
                        Ok::<_, tonic::Status>(out)
                    })
                    .map_err(|e| grpc_err(verb, e))?;

                if stats_action {
                    let stats = ServerStats::decode_protobuf(&body)
                        .map_err(|e| value_err(format!("{verb}(): invalid stats protobuf: {e}")))?;
                    return Ok(dict(vec![
                        ("allocated_bytes", int_obj(stats.allocated_bytes)),
                        ("deferred_bytes", int_obj(stats.deferred_bytes)),
                        ("wal_bytes", int_obj(stats.wal_bytes)),
                        ("live_readers", int_obj(stats.live_readers)),
                        ("shards", int_obj(stats.shards)),
                    ]));
                }

                Ok(MontyObject::String(
                    String::from_utf8_lossy(&body).into_owned(),
                ))
            }

            "flight_actions" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let h = a.handle(0)?;
                let rt = self.rt_handle(verb)?;
                let ep = self.endpoint(h, verb)?;

                let names = rt
                    .block_on(async {
                        let mut s = ep.client.list_actions(ep.req(Empty {})).await?.into_inner();
                        let mut out: Vec<String> = Vec::new();
                        while let Some(r) = s.next().await {
                            out.push(r?.r#type);
                        }
                        Ok::<_, tonic::Status>(out)
                    })
                    .map_err(|e| grpc_err(verb, e))?;

                Ok(MontyObject::List(
                    names.into_iter().map(MontyObject::String).collect(),
                ))
            }

            // Pin the lowest leadership this client will accept.
            "flight_expect_term" => {
                a.exact(2)?;
                a.no_kwargs()?;
                let (h, n) = (a.handle(0)?, a.u64(1)?);
                let n = u32::try_from(n)
                    .map_err(|_| value_err(format!("{verb}(): a term is a u32")))?;
                self.endpoint(h, verb)?.expect_term = Some(n);
                Ok(MontyObject::None)
            }

            // The term the server reports, taken from the response metadata of
            // an ordinary call.
            //
            // Read off a **normal** response rather than from a bespoke RPC,
            // because that is how a real client learns it: the header is on every
            // response precisely so that a client which only ever reads still
            // builds the memory that protects it.
            "flight_term" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let h = a.handle(0)?;
                let rt = self.rt_handle(verb)?;
                let ep = self.endpoint(h, verb)?;
                let resp = rt
                    .block_on(ep.client.list_actions(ep.req(Empty {})))
                    .map_err(|e| grpc_err(verb, e))?;
                match resp.metadata().get(yesno_server::guard::TERM) {
                    Some(v) => {
                        let t: u64 =
                            v.to_str()
                                .ok()
                                .and_then(|s| s.parse().ok())
                                .ok_or_else(|| {
                                    value_err(format!("{verb}(): unreadable term header"))
                                })?;
                        Ok(int_obj(t))
                    }
                    None => Ok(MontyObject::None),
                }
            }

            _ => Err(value_err(format!("{verb}(): not a flight verb"))),
        }
    }
}

impl World {
    /// Register a client against a socket somebody else is serving — the shape
    /// `srv_flight` needs, so that every `flight_*` verb works unchanged against
    /// a real `yesnod`.
    pub(crate) fn adopt_flight_endpoint(
        &mut self,
        client: FlightServiceClient<Channel>,
        token: Option<String>,
    ) -> MontyObject {
        self.flight.endpoints.push(Some(Endpoint {
            client,
            location: None,
            token,
            expect_term: None,
            owned: None,
            db_handle: None,
        }));
        let idx = self.flight.endpoints.len() - 1;
        self.mint(HandleKind::Flight, idx)
    }
}
