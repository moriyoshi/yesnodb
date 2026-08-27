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
    assert_eq!(ticket.expr, Some(expr), "the ticket must carry the view");

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
