//! End-to-end Flight: `get_flight_info` -> `do_get` -> the same set back.
//!
//! The two assertions that matter are the design's two differentiators, because
//! everything else here is protocol plumbing a client would notice immediately:
//!
//! 1. **`total_records` is exact** before a single ordinal is materialized. Most
//!    Flight servers return -1 because counting means executing; here it comes
//!    from `card_m1` in the index.
//! 2. **The ticket carries the snapshot version**, which is what would make a
//!    multi-endpoint fetch consistent rather than merely parallel.

use std::collections::BTreeSet;
use std::sync::Arc;

use arrow_array::{Array, UInt64Array};
use arrow_flight::flight_service_client::FlightServiceClient;
use arrow_flight::flight_service_server::FlightServiceServer;
use arrow_flight::FlightDescriptor;
use futures::StreamExt;
use tonic::transport::{Channel, Server};
use yesno_core::{Db, DbOptions};
use yesno_flight::{Ticket, YesnoFlightService};

/// Connect a client to `url`. The generated client takes a channel rather than
/// a URL, so the connection is built explicitly.
async fn client_for(url: String) -> FlightServiceClient<Channel> {
    let ch = Channel::from_shared(url).unwrap().connect().await.unwrap();
    FlightServiceClient::new(ch)
}

/// Serve `db`, returning the URL and a handle that stops the server.
///
/// The shutdown handle is not ceremony. The service owns an `Arc<Db>`, and a
/// live `Db` holds the database's **exclusive file lock** — so a test that
/// reopens the directory to check durability fails with `AlreadyOpen` against
/// its own server unless the server is stopped and its `Arc` dropped first.
/// That is the lock behaving correctly, and it is worth an operator knowing.
async fn serve(db: Arc<Db>) -> (String, tokio::sync::oneshot::Sender<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        Server::builder()
            .add_service(FlightServiceServer::new(YesnoFlightService::new(db)))
            .serve_with_incoming_shutdown(
                tokio_stream::wrappers::TcpListenerStream::new(listener),
                async {
                    let _ = rx.await;
                },
            )
            .await
            .unwrap();
    });
    (format!("http://{addr}"), tx)
}

fn open(dir: &std::path::Path) -> Arc<Db> {
    Arc::new(
        Db::open_with(
            dir,
            DbOptions {
                shards: 2,
                ..Default::default()
            },
        )
        .unwrap(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_key_round_trips_through_flight_with_an_exact_row_count() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());

    // Spans many chunks, so the count cannot come from one container by luck.
    let want: BTreeSet<u64> = (0..30_000u64).map(|i| i * 977).collect();
    db.insert_many(7, &want.iter().copied().collect::<Vec<_>>())
        .unwrap();
    db.checkpoint().unwrap();

    let (url, _stop) = serve(db.clone()).await;
    let mut client = client_for(url).await;

    // --- get_flight_info: the count must be exact, and free.
    let d = FlightDescriptor::new_cmd(7u64.to_le_bytes().to_vec());
    let info = client.get_flight_info(d).await.unwrap().into_inner();
    assert_eq!(
        info.total_records,
        want.len() as i64,
        "total_records must be the exact cardinality, not -1"
    );
    assert_eq!(info.endpoint.len(), 1, "v1 ships exactly one endpoint");

    // --- the ticket must pin a snapshot, which is the consistency guarantee.
    let raw = info.endpoint[0].ticket.as_ref().unwrap().ticket.clone();
    let t = Ticket::decode(&raw).expect("the server must issue a well-formed ticket");
    assert_eq!(t.key, 7);
    assert!(
        t.version > 0,
        "a ticket without a version cannot be consistent"
    );

    // --- do_get: the ordinals themselves.
    let stream = client
        .do_get(arrow_flight::Ticket::new(raw))
        .await
        .unwrap()
        .into_inner();
    let mut decoded = arrow_flight::decode::FlightRecordBatchStream::new_from_flight_data(
        stream.map(|r| r.map_err(|e| arrow_flight::error::FlightError::ExternalError(Box::new(e)))),
    );

    let mut got: BTreeSet<u64> = BTreeSet::new();
    let mut batches = 0;
    while let Some(b) = decoded.next().await {
        let b = b.unwrap();
        assert_eq!(b.num_columns(), 1);
        let col = b.column(0).as_any().downcast_ref::<UInt64Array>().unwrap();
        assert_eq!(
            col.null_count(),
            0,
            "a posting list has no nulls, structurally"
        );
        got.extend((0..col.len()).map(|i| col.value(i)));
        batches += 1;
    }
    assert!(
        batches > 1,
        "30k ordinals must arrive as several batches, not one"
    );
    assert_eq!(
        got, want,
        "the set that came back is not the set that went in"
    );
}

/// A view descriptor crosses Flight intact and is evaluated against its packed key.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_view_query_round_trips_with_an_exact_logical_count() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    let constituents = [
        BTreeSet::from([1, 8, 55]),
        BTreeSet::from([2, 8, 89]),
        BTreeSet::from([3, 8, 144]),
    ];
    let packed: Vec<u64> = constituents
        .iter()
        .enumerate()
        .flat_map(|(set, xs)| xs.iter().map(move |x| x * 3 + set as u64))
        .collect();
    db.insert_many(9, &packed).unwrap();

    let expr = yesno_flight::SetExpr::At(
        Box::new(yesno_flight::VecSetExpr::View(
            Box::new(yesno_flight::SetExpr::Key(9)),
            yesno_flight::ViewSpec::interleaved(3),
        )),
        2,
    );
    let (url, _stop) = serve(db).await;
    let mut client = client_for(url).await;
    let info = client
        .get_flight_info(FlightDescriptor::new_cmd(expr.encode()))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(info.total_records, constituents[2].len() as i64);

    let raw = info.endpoint[0].ticket.as_ref().unwrap().ticket.to_vec();
    let ticket = Ticket::decode(&raw).expect("server view ticket must decode");
    assert_eq!(ticket.key, 9, "the physical key remains the routing key");
    assert_eq!(
        ticket.expr,
        Some(yesno_flight::AnyExpr::Set(expr)),
        "the ticket must carry the view"
    );

    let stream = client
        .do_get(arrow_flight::Ticket::new(raw))
        .await
        .unwrap()
        .into_inner();
    let mut decoded = arrow_flight::decode::FlightRecordBatchStream::new_from_flight_data(
        stream.map(|r| r.map_err(|e| arrow_flight::error::FlightError::ExternalError(Box::new(e)))),
    );
    let mut got = BTreeSet::new();
    while let Some(batch) = decoded.next().await {
        let batch = batch.unwrap();
        let col = batch
            .column(0)
            .as_any()
            .downcast_ref::<UInt64Array>()
            .unwrap();
        got.extend((0..col.len()).map(|i| col.value(i)));
    }
    assert_eq!(got, constituents[2]);
}

/// `do_put` must ingest S2 pairs and make them readable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn do_put_ingests_pairs_and_they_survive_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let pairs: Vec<(u64, u64)> = (0..5_000u64).map(|i| (i % 4, i * 13)).collect();

    let stop = {
        let db = open(dir.path());
        let (url, stop) = serve(db.clone()).await;
        let mut client = client_for(url).await;

        let schema = yesno_flight::pairs_schema();
        let keys = UInt64Array::new(pairs.iter().map(|p| p.0).collect::<Vec<_>>().into(), None);
        let ords = UInt64Array::new(pairs.iter().map(|p| p.1).collect::<Vec<_>>().into(), None);
        let batch =
            arrow_array::RecordBatch::try_new(schema.clone(), vec![Arc::new(keys), Arc::new(ords)])
                .unwrap();

        let input = arrow_flight::encode::FlightDataEncoderBuilder::new()
            .with_schema(schema)
            // `do_put` requires an explicit command; it no longer defaults
            // to insert, so that an unrecognised one cannot be applied as one.
            .with_flight_descriptor(Some(arrow_flight::FlightDescriptor::new_cmd(
                yesno_flight::PUT_INSERT.to_vec(),
            )))
            .build(futures::stream::iter(vec![Ok(batch)]))
            .map(|r| r.unwrap());
        let acked = client.do_put(input).await.unwrap().into_inner();
        let results: Vec<_> = acked.collect().await;
        assert!(!results.is_empty(), "do_put acknowledged nothing");

        // The bare Flight service has no administrative checkpoint surface.
        db.checkpoint().unwrap();
        stop
    };

    // Stop the server so it drops its `Arc<Db>` and releases the file lock.
    // Without this the reopen below fails with `AlreadyOpen` — the exclusive
    // lock working exactly as intended, against this test's own server.
    let _ = stop.send(());
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Reopened: the ingest is durable, not just visible in the memtable.
    let db = open(dir.path());
    let snap = db.snapshot().unwrap();
    for k in 0..4u64 {
        let want: BTreeSet<u64> = pairs.iter().filter(|p| p.0 == k).map(|p| p.1).collect();
        let got: BTreeSet<u64> = snap.load(k).unwrap().iter().collect();
        assert_eq!(got, want, "key {k} did not survive the round trip");
    }
}

/// A malformed ticket must be refused, not answered with an empty result.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_malformed_ticket_is_an_error_not_an_empty_answer() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    db.insert(1, 1).unwrap();
    let (url, _stop) = serve(db).await;
    let mut client = client_for(url).await;

    let err = client
        .do_get(arrow_flight::Ticket::new(vec![1, 2, 3]))
        .await
        .expect_err("a 3-byte ticket is not valid");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
}

/// `do_exchange` and Flight SQL are cut; saying so beats a confusing failure.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cut_surfaces_report_unimplemented() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    let (url, _stop) = serve(db).await;
    let mut client = client_for(url).await;

    let err = client
        .do_exchange(futures::stream::iter(Vec::<arrow_flight::FlightData>::new()))
        .await
        .expect_err("do_exchange is cut");
    assert_eq!(err.code(), tonic::Code::Unimplemented);
}

/// Container payloads and ordinals must decode to the same set, and be far smaller.
///
/// **The ordinal path is the oracle**, which is what makes this a differential test
/// rather than a test of the dense encoder against itself: the same key, the same
/// pinned version, two representations, one expected answer. A wrong `kind`, a wrong
/// prefix or a mis-encoded payload shows up as a set mismatch here rather than as a
/// plausible-looking batch.
///
/// The size assertion is the reason the representation exists at all, so it is
/// stated as a ratio and left loose: dense chunks are 8 KiB of payload against
/// 512 KiB of ordinals, so a factor of four is eight times under the ordinal floor
/// while surviving a legitimate change to framing or to `codec`.
#[tokio::test]
async fn container_payloads_decode_to_the_same_set_as_ordinals() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());

    // Deliberately mixed: four dense chunks that must become bitmaps, a contiguous
    // run, and a sparse scattering that must stay an array. One representation has
    // to serve all three, which is the whole argument for shipping containers.
    let mut want: BTreeSet<u64> = (0..4 * 65_536u64).collect();
    want.extend((10 << 16)..(10 << 16) + 5_000);
    want.extend((0..200u64).map(|i| (20 << 16) + i * 300));
    db.insert_many(7, &want.iter().copied().collect::<Vec<_>>())
        .unwrap();
    db.checkpoint().unwrap();

    let (url, _stop) = serve(db.clone()).await;
    let mut client = client_for(url).await;

    let d = FlightDescriptor::new_cmd(7u64.to_le_bytes().to_vec());
    let info = client.get_flight_info(d).await.unwrap().into_inner();
    let raw = info.endpoint[0].ticket.as_ref().unwrap().ticket.clone();
    let t = Ticket::decode(&raw).expect("the server must issue a well-formed ticket");
    assert_eq!(
        t.wire,
        yesno_flight::SetWire::Ordinals,
        "a minted ticket must default to ordinals; density must never switch it"
    );

    // --- the oracle: ordinals, at this exact ticket.
    let mut ordinal_bytes = 0usize;
    let mut from_ordinals: BTreeSet<u64> = BTreeSet::new();
    {
        let stream = client
            .do_get(arrow_flight::Ticket::new(raw.clone()))
            .await
            .unwrap()
            .into_inner();
        let mut decoded =
            arrow_flight::decode::FlightRecordBatchStream::new_from_flight_data(stream.map(|r| {
                r.map_err(|e| arrow_flight::error::FlightError::ExternalError(Box::new(e)))
            }));
        while let Some(b) = decoded.next().await {
            let b = b.unwrap();
            let col = b.column(0).as_any().downcast_ref::<UInt64Array>().unwrap();
            ordinal_bytes += col.len() * 8;
            from_ordinals.extend((0..col.len()).map(|i| col.value(i)));
        }
    }
    assert_eq!(from_ordinals, want, "the oracle itself must be right first");

    // --- the same ticket, asking for containers.
    let dense = t.with_wire(yesno_flight::SetWire::Containers);
    let stream = client
        .do_get(arrow_flight::Ticket::new(dense.encode()))
        .await
        .unwrap()
        .into_inner();
    let mut decoded = arrow_flight::decode::FlightRecordBatchStream::new_from_flight_data(
        stream.map(|r| r.map_err(|e| arrow_flight::error::FlightError::ExternalError(Box::new(e)))),
    );

    let mut from_containers: BTreeSet<u64> = BTreeSet::new();
    let mut payload_bytes = 0usize;
    let mut kinds: BTreeSet<u8> = BTreeSet::new();
    while let Some(b) = decoded.next().await {
        let b = b.unwrap();
        assert_eq!(
            b.schema(),
            yesno_arrow::containers_schema(),
            "the dense stream must carry the container schema it declared"
        );
        for (key, prefix, c) in yesno_arrow::read_containers(&b).unwrap() {
            assert_eq!(key, 7, "every row belongs to the key that was asked for");
            kinds.insert(c.kind() as u8);
            payload_bytes += yesno_core::container::codec::encode(&c).len();
            from_containers.extend(c.iter().map(|v| (prefix << 16) | v as u64));
        }
    }

    assert_eq!(
        from_containers, want,
        "containers decoded to a different set than ordinals did"
    );
    assert!(
        kinds.len() > 1,
        "this fixture must exercise more than one container kind, got {kinds:?}"
    );
    assert!(
        payload_bytes * 4 < ordinal_bytes,
        "container payloads were {payload_bytes} bytes against {ordinal_bytes} of \
         ordinals; the dense representation has stopped paying for itself"
    );
}

/// A materialized bitvector agrees with the ordinals, and an unbounded one is refused.
///
/// Two properties in one fixture because they are the same design point seen from
/// either side: a bitvector is sized by its **prefix window** rather than by the
/// set's cardinality, which is what makes it both the easiest representation to
/// consume and the only one a server must refuse outright.
///
/// The agreement half uses the ordinal path as oracle. Gaps matter here in a way
/// they do not for containers: the stream carries a bit for every position in the
/// window, so a chunk the set never touched must arrive as 65 536 zeros rather than
/// be skipped -- otherwise row `n` stops meaning ordinal `base + n` and every later
/// bit is shifted.
#[tokio::test]
async fn a_materialized_bitvector_agrees_with_ordinals_and_refuses_an_unbounded_window() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());

    // Chunks 0 and 3 populated, 1 and 2 deliberately empty, so the window contains
    // gaps that have to be materialized rather than skipped.
    let mut want: BTreeSet<u64> = (0..65_536u64).filter(|o| o % 7 == 0).collect();
    want.extend((3 << 16)..(3 << 16) + 1_000);
    db.insert_many(7, &want.iter().copied().collect::<Vec<_>>())
        .unwrap();
    db.checkpoint().unwrap();

    let (url, _stop) = serve(db.clone()).await;
    let mut client = client_for(url).await;

    let d = FlightDescriptor::new_cmd(7u64.to_le_bytes().to_vec());
    let info = client.get_flight_info(d).await.unwrap().into_inner();
    let raw = info.endpoint[0].ticket.as_ref().unwrap().ticket.clone();
    let base = Ticket::decode(&raw).expect("the server must issue a well-formed ticket");

    // --- an unbounded window must be refused, not attempted.
    let unbounded = base.clone().with_wire(yesno_flight::SetWire::Bitvector);
    assert_eq!(
        unbounded.prefix_hi - unbounded.prefix_lo,
        1 << 48,
        "a whole-key ticket must span everything, or this half proves nothing"
    );
    let err = {
        let stream = client
            .do_get(arrow_flight::Ticket::new(unbounded.encode()))
            .await
            .unwrap()
            .into_inner();
        let mut decoded =
            arrow_flight::decode::FlightRecordBatchStream::new_from_flight_data(stream.map(|r| {
                r.map_err(|e| arrow_flight::error::FlightError::ExternalError(Box::new(e)))
            }));
        let mut seen = None;
        while let Some(b) = decoded.next().await {
            if let Err(e) = b {
                seen = Some(e.to_string());
                break;
            }
        }
        seen.expect("2^48 chunks of bits must be refused rather than streamed")
    };
    assert!(
        err.contains("bitvector") && err.contains("narrow"),
        "the refusal must say what to do about it, got {err:?}"
    );

    // --- bounded: four chunks, and the bits must be the set.
    let mut windowed = base.with_wire(yesno_flight::SetWire::Bitvector);
    windowed.prefix_lo = 0;
    windowed.prefix_hi = 4;
    let stream = client
        .do_get(arrow_flight::Ticket::new(windowed.encode()))
        .await
        .unwrap()
        .into_inner();
    let mut decoded = arrow_flight::decode::FlightRecordBatchStream::new_from_flight_data(
        stream.map(|r| r.map_err(|e| arrow_flight::error::FlightError::ExternalError(Box::new(e)))),
    );

    // **Decoded as a consumer with no Roaring would**: each value is bit-packed
    // little-endian bits in ordinal order, so bit `j` of value `i` is ordinal
    // `base + i * bits_per_value + j`. Both figures come from the schema metadata rather
    // than being assumed, which is what makes the stream self-describing.
    // Taken from the first batch, because a Flight stream's schema is not known until its
    // first message has arrived -- `FlightRecordBatchStream::schema()` is `None` before
    // then, which the first version of this test assumed otherwise.
    let mut base = u64::MAX;
    let mut per_value = 0u64;
    let mut from_bits: BTreeSet<u64> = BTreeSet::new();
    let mut values = 0u64;
    while let Some(b) = decoded.next().await {
        let b = b.unwrap();
        if per_value == 0 {
            let meta = b.schema().field(0).metadata().clone();
            base = meta
                .get(yesno_arrow::schema::META_BASE_ORDINAL)
                .expect("the base ordinal rides in the field metadata")
                .parse()
                .unwrap();
            per_value = meta
                .get(yesno_arrow::META_BITS_PER_VALUE)
                .expect("the bits each value advances ride there too")
                .parse()
                .unwrap();
            assert_eq!(base, 0, "this window starts at prefix 0");
            assert_eq!(per_value, 65_536);
        }
        let col = b
            .column(0)
            .as_any()
            .downcast_ref::<arrow_array::BinaryArray>()
            .unwrap();
        for i in 0..col.len() {
            let bytes = col.value(i);
            assert_eq!(
                bytes.len() as u64,
                per_value / 8,
                "every value must be a whole chunk of bits, including the empty ones"
            );
            let origin = base + values * per_value;
            for (byte, &v) in bytes.iter().enumerate() {
                for bit in 0..8u32 {
                    if v & (1 << bit) != 0 {
                        from_bits.insert(origin + byte as u64 * 8 + bit as u64);
                    }
                }
            }
            values += 1;
        }
    }
    assert_eq!(
        values * per_value,
        4 * 65_536,
        "a bitvector must carry a bit for every position in its window, gaps included"
    );
    // Batched rather than one value per batch, which is what the measured plateau asks for.
    assert!(values > 1, "the fixture must span several chunks");
    assert_eq!(
        from_bits, want,
        "the bitvector decoded to a different set than the ordinals did"
    );
}

/// A fully dense window goes down the **borrowed** path and still agrees with ordinals.
///
/// The test above deliberately has gaps, so it exercises the staging walk. This one has
/// none, so `dense_span` vouches for every batch and the server builds them over lent
/// buffers instead of copying -- a different code path, with its own offset arithmetic, and
/// therefore its own chance to be wrong. That `dense_span` lends rather than copies is
/// asserted in `yesno-core`'s `tests/dense_span.rs` by address; what this asserts is that
/// the bytes it hands to Arrow describe the same set the ordinal path does.
#[tokio::test]
async fn a_borrowed_bitvector_agrees_with_ordinals() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());

    // Four chunks, every one dense enough to be a bitmap and none empty.
    let want: BTreeSet<u64> = (0..4 * 65_536u64).filter(|o| o % 2 == 0).collect();
    db.insert_many(7, &want.iter().copied().collect::<Vec<_>>())
        .unwrap();
    db.checkpoint().unwrap();

    let (url, _stop) = serve(db.clone()).await;
    let mut client = client_for(url).await;
    let d = FlightDescriptor::new_cmd(7u64.to_le_bytes().to_vec());
    let info = client.get_flight_info(d).await.unwrap().into_inner();
    let raw = info.endpoint[0].ticket.as_ref().unwrap().ticket.clone();

    let mut t = Ticket::decode(&raw)
        .unwrap()
        .with_wire(yesno_flight::SetWire::Bitvector);
    t.prefix_lo = 0;
    t.prefix_hi = 4;
    let stream = client
        .do_get(arrow_flight::Ticket::new(t.encode()))
        .await
        .unwrap()
        .into_inner();
    let mut decoded = arrow_flight::decode::FlightRecordBatchStream::new_from_flight_data(
        stream.map(|r| r.map_err(|e| arrow_flight::error::FlightError::ExternalError(Box::new(e)))),
    );

    let mut got: BTreeSet<u64> = BTreeSet::new();
    let mut values = 0u64;
    while let Some(b) = decoded.next().await {
        let b = b.unwrap();
        let col = b
            .column(0)
            .as_any()
            .downcast_ref::<arrow_array::BinaryArray>()
            .unwrap();
        for i in 0..col.len() {
            let bytes = col.value(i);
            assert_eq!(bytes.len(), 8192, "each value is one chunk of bits");
            let origin = values * 65_536;
            for (byte, &v) in bytes.iter().enumerate() {
                for bit in 0..8u32 {
                    if v & (1 << bit) != 0 {
                        got.insert(origin + byte as u64 * 8 + bit as u64);
                    }
                }
            }
            values += 1;
        }
    }
    assert_eq!(
        values, 4,
        "every chunk in the window must arrive exactly once"
    );
    assert_eq!(
        got, want,
        "the borrowed bitvector decoded to a different set than the ordinals did"
    );
}
