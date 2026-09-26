//! The sorts other than `Set`, over a real Flight connection.
//!
//! **This is the gap that existed from the day the language became
//! multi-sorted until 2026-09-25.** `QueryRequest` and `Ticket` both carried a
//! `SetExpr`, so a facet histogram -- the acceptance case the sorted language
//! was built for -- parsed, evaluated in process, and could not be asked for
//! over the wire. Nothing failed; there was simply no path. A test that only
//! called the evaluator would have kept passing forever, which is why this one
//! goes through a socket.

use std::sync::Arc;

use arrow_array::{Array, BinaryArray, BooleanArray, UInt64Array};
use arrow_flight::flight_service_client::FlightServiceClient;
use arrow_flight::flight_service_server::FlightServiceServer;
use arrow_flight::FlightDescriptor;
use futures::StreamExt;
use tonic::transport::{Channel, Server};
use yesno_core::bignum::BigUint;
use yesno_core::{Db, DbOptions, OrdSet};
use yesno_flight::{
    AnyExpr, BigBinOp, BigExpr, BigLit, IntExpr, QueryRequest, SetExpr, VecBigExpr, VecIntExpr,
    VecSetExpr, ViewSpec, YesnoFlightService,
};

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

/// Ask for `e` and return the one batch it answers with, plus `total_records`.
async fn fetch(
    c: &mut FlightServiceClient<Channel>,
    e: AnyExpr,
) -> (arrow_array::RecordBatch, i64) {
    let d = FlightDescriptor::new_cmd(QueryRequest::current(e).encode());
    let info = c.get_flight_info(d).await.unwrap().into_inner();
    let total = info.total_records;
    let ticket = info.endpoint[0].ticket.clone().unwrap();
    let stream = c.do_get(ticket).await.unwrap().into_inner();
    let mut batches: Vec<_> = arrow_flight::decode::FlightRecordBatchStream::new_from_flight_data(
        stream.map(|r| r.map_err(|e| e.into())),
    )
    .collect()
    .await;
    assert_eq!(batches.len(), 1, "these sorts answer in one batch");
    (batches.pop().unwrap().unwrap(), total)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_facet_histogram_travels_and_returns_one_count_per_constituent() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        Db::open_with(
            dir.path(),
            DbOptions {
                shards: 2,
                ..Default::default()
            },
        )
        .unwrap(),
    );

    // Three constituents interleaved under key 9, filtered by key 7.
    let cohorts: [Vec<u64>; 3] = [vec![0, 1, 2, 3], vec![0, 2, 4], vec![5]];
    let packed: Vec<u64> = cohorts
        .iter()
        .enumerate()
        .flat_map(|(i, xs)| xs.iter().map(move |x| x * 3 + i as u64))
        .collect();
    db.insert_many(9, &packed).unwrap();
    db.insert_many(7, &[0, 2, 4, 9]).unwrap();

    let (url, stop) = serve(db.clone()).await;
    let mut c =
        FlightServiceClient::new(Channel::from_shared(url).unwrap().connect().await.unwrap());

    let facet = AnyExpr::VecInt(VecIntExpr::Map(
        Box::new(VecSetExpr::View(
            Box::new(SetExpr::Key(9)),
            ViewSpec::interleaved(3),
        )),
        Box::new(IntExpr::Cardinality(Box::new(SetExpr::And(vec![
            SetExpr::Hole,
            SetExpr::Key(7),
        ])))),
    ));

    let (batch, total) = fetch(&mut c, facet).await;
    // One row per constituent, and `total_records` says so before the read.
    assert_eq!(total, 3);
    assert_eq!(batch.num_rows(), 3);
    let got = batch
        .column(0)
        .as_any()
        .downcast_ref::<UInt64Array>()
        .expect("the declared column type");

    // Against Python's-set-style reasoning done by hand: constituent i's
    // members intersected with { 0, 2, 4, 9 }.
    let filter = [0u64, 2, 4, 9];
    for (i, cohort) in cohorts.iter().enumerate() {
        let want = cohort.iter().filter(|x| filter.contains(x)).count() as u64;
        assert_eq!(got.value(i), want, "constituent {i}");
    }

    let _ = stop.send(());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_arbitrary_precision_value_travels_and_returns_sign_and_magnitude() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        Db::open_with(
            dir.path(),
            DbOptions {
                shards: 2,
                ..Default::default()
            },
        )
        .unwrap(),
    );

    // A stored value wider than a machine word, then arithmetic on it.
    let stored = BigUint::from_limbs_le(vec![0xDEAD_BEEF_CAFE_BABE, 0x1234_5678]);
    let ordinals: Vec<u64> = OrdSet::from_int(&stored).unwrap().iter().collect();
    db.insert_many(4, &ordinals).unwrap();

    let (url, stop) = serve(db.clone()).await;
    let mut c =
        FlightServiceClient::new(Channel::from_shared(url).unwrap().connect().await.unwrap());

    // read(key 4, 128) * -3
    let e = AnyExpr::Big(BigExpr::Mul(
        Box::new(BigExpr::Read(Box::new(SetExpr::Key(4)), 128)),
        Box::new(BigExpr::Lit(BigLit::from_i64(-3))),
    ));
    let (batch, total) = fetch(&mut c, e).await;
    assert_eq!(total, 1);
    assert_eq!(batch.num_rows(), 1);

    let negative = batch
        .column(0)
        .as_any()
        .downcast_ref::<BooleanArray>()
        .expect("the declared column type");
    let magnitude = batch
        .column(1)
        .as_any()
        .downcast_ref::<BinaryArray>()
        .expect("the declared column type");

    assert!(negative.value(0), "a positive times -3 is negative");
    let want = stored.mul(&BigUint::from_u64(3));
    assert_eq!(magnitude.value(0), want.to_le_bytes().as_slice());
    // Canonical on the wire: no trailing zero byte.
    assert_ne!(magnitude.value(0).last(), Some(&0));

    let _ = stop.send(());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_vector_of_big_integers_travels_and_returns_one_value_per_constituent() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        Db::open_with(
            dir.path(),
            DbOptions {
                shards: 2,
                ..Default::default()
            },
        )
        .unwrap(),
    );

    // Three integers interleaved into one ordinal space, as view constituents.
    // This is the shape the sort exists for: a facet histogram whose per-cohort
    // quantity is a stored integer rather than a count.
    let values = [
        BigUint::from_u64(0b1011),
        BigUint::from_u64(0),
        BigUint::from_limbs_le(vec![0xDEAD_BEEF, 0x99]),
    ];
    let sets = 3u32;
    let mut packed: Vec<u64> = Vec::new();
    for (i, v) in values.iter().enumerate() {
        for x in 0..v.bit_len() {
            if v.bit(x) {
                packed.push(x * u64::from(sets) + i as u64);
            }
        }
    }
    packed.sort_unstable();
    db.insert_many(31, &packed).unwrap();

    let (url, stop) = serve(db.clone()).await;
    let mut c =
        FlightServiceClient::new(Channel::from_shared(url).unwrap().connect().await.unwrap());

    let e = AnyExpr::VecBig(yesno_flight::VecBigExpr::Map(
        Box::new(VecSetExpr::View(
            Box::new(SetExpr::Key(31)),
            ViewSpec::interleaved(sets),
        )),
        Box::new(BigExpr::Read(Box::new(SetExpr::Hole), 128)),
    ));

    let (batch, total) = fetch(&mut c, e).await;
    assert_eq!(total, i64::from(sets));
    assert_eq!(batch.num_rows(), sets as usize);

    let negative = batch
        .column(0)
        .as_any()
        .downcast_ref::<BooleanArray>()
        .expect("the declared column type");
    let magnitude = batch
        .column(1)
        .as_any()
        .downcast_ref::<BinaryArray>()
        .expect("the declared column type");

    for (i, want) in values.iter().enumerate() {
        assert!(!negative.value(i), "a magnitude read is never negative");
        assert_eq!(
            magnitude.value(i),
            want.to_le_bytes().as_slice(),
            "value {i}"
        );
    }

    let _ = stop.send(());
}

/// A zip and a scale cross Flight and are applied position by position.
///
/// `Map` was the only `VecBig` shape with transport coverage, and it is the one
/// whose result comes straight from the reader. These two *compute* per
/// element, so they are where a per-position misalignment would show up -- and
/// a misalignment is invisible to a scalar test, which has only one position.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_zip_and_a_scale_travel_and_are_applied_position_by_position() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        Db::open_with(
            dir.path(),
            DbOptions {
                shards: 2,
                ..Default::default()
            },
        )
        .unwrap(),
    );

    // Deliberately distinct per constituent, so swapping two positions changes
    // the answer. Equal values would make an off-by-one invisible.
    let values = [
        BigUint::from_u64(6),
        BigUint::from_u64(10),
        BigUint::from_u64(15),
    ];
    let sets = 3u32;
    let mut packed: Vec<u64> = Vec::new();
    for (i, v) in values.iter().enumerate() {
        for x in 0..v.bit_len() {
            if v.bit(x) {
                packed.push(x * u64::from(sets) + i as u64);
            }
        }
    }
    packed.sort_unstable();
    db.insert_many(17, &packed).unwrap();

    let (url, stop) = serve(db.clone()).await;
    let mut c =
        FlightServiceClient::new(Channel::from_shared(url).unwrap().connect().await.unwrap());

    let stored = || {
        VecBigExpr::Map(
            Box::new(VecSetExpr::View(
                Box::new(SetExpr::Key(17)),
                ViewSpec::interleaved(sets),
            )),
            Box::new(BigExpr::Read(Box::new(SetExpr::Hole), 64)),
        )
    };
    let literals = VecBigExpr::List(vec![
        BigExpr::Lit(BigLit::from_i64(100)),
        BigExpr::Lit(BigLit::from_i64(200)),
        BigExpr::Lit(BigLit::from_i64(300)),
    ]);

    // zip: [ 6, 10, 15 ] * [ 100, 200, 300 ]
    let zip = AnyExpr::VecBig(VecBigExpr::Zip(
        Box::new(stored()),
        Box::new(literals),
        BigBinOp::Mul,
    ));
    let (batch, total) = fetch(&mut c, zip).await;
    assert_eq!(total, i64::from(sets));
    let magnitude = batch
        .column(1)
        .as_any()
        .downcast_ref::<BinaryArray>()
        .expect("the declared column type");
    for (i, want) in [600u64, 2000, 4500].iter().enumerate() {
        assert_eq!(
            magnitude.value(i),
            BigUint::from_u64(*want).to_le_bytes().as_slice(),
            "position {i}"
        );
    }

    // scale: [ 6, 10, 15 ] - 4, the asymmetric operator, so the scalar being
    // taken as the left operand would be visible.
    let scale = AnyExpr::VecBig(VecBigExpr::Scale(
        Box::new(stored()),
        Box::new(BigExpr::Lit(BigLit::from_i64(4))),
        BigBinOp::Sub,
    ));
    let (batch, _) = fetch(&mut c, scale).await;
    let negative = batch
        .column(0)
        .as_any()
        .downcast_ref::<BooleanArray>()
        .expect("the declared column type");
    let magnitude = batch
        .column(1)
        .as_any()
        .downcast_ref::<BinaryArray>()
        .expect("the declared column type");
    for (i, want) in [2u64, 6, 11].iter().enumerate() {
        assert!(!negative.value(i), "position {i} is not negative");
        assert_eq!(
            magnitude.value(i),
            BigUint::from_u64(*want).to_le_bytes().as_slice(),
            "position {i}"
        );
    }

    let _ = stop.send(());
}
