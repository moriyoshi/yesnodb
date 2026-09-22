//! The capture point fires on a real Flight query, as `tracing` events.
//!
//! # Why this exists as an integration test
//!
//! The unit tests in `src/hotspot.rs` call the recorders directly, so they
//! prove the recorders work and prove nothing about whether anything calls
//! them. That gap is not hypothetical: an earlier version of the hook was
//! placed on the lowered expression, where every leaf is a freshly allocated
//! `KeySource`, and it captured nothing at all. A unit test cannot see that,
//! because a unit test supplies the expression itself.
//!
//! So this drives the whole path -- server, wire encoding, `get_flight_info`,
//! `expr::cardinality` -- with a corpus dense enough to be offload material,
//! and asserts on the events. It is the only test here that would notice the
//! hook being deleted.
//!
//! One test in its own binary, because it installs a *global* subscriber: the
//! server answers on tokio worker threads, which a thread-local dispatcher
//! would not reach.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use arrow_flight::flight_service_client::FlightServiceClient;
use arrow_flight::flight_service_server::FlightServiceServer;
use arrow_flight::FlightDescriptor;
use tonic::transport::{Channel, Server};
use tracing::field::{Field, Visit};
use yesno_core::{Db, DbOptions};
use yesno_flight::{SetExpr, YesnoFlightService};

const TARGET: &str = "yesno::hotspot";

#[derive(Default)]
struct Fields(HashMap<String, String>);

impl Visit for Fields {
    fn record_debug(&mut self, f: &Field, v: &dyn std::fmt::Debug) {
        self.0.insert(f.name().to_string(), format!("{v:?}"));
    }
    fn record_u64(&mut self, f: &Field, v: u64) {
        self.0.insert(f.name().to_string(), v.to_string());
    }
    fn record_str(&mut self, f: &Field, v: &str) {
        self.0.insert(f.name().to_string(), v.to_string());
    }
}

struct Collector(Arc<Mutex<Vec<HashMap<String, String>>>>);

impl tracing::Subscriber for Collector {
    fn enabled(&self, m: &tracing::Metadata<'_>) -> bool {
        m.target() == TARGET
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, e: &tracing::Event<'_>) {
        let mut f = Fields::default();
        e.record(&mut f);
        self.0.lock().expect("lock").push(f.0);
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

/// Dense enough that every chunk of the posting list is a bitmap container,
/// which is what `all_bitmap_chunks` admits and the GPU kernel was measured on.
fn dense(db: &Db, key: u64, chunks: u64) {
    let mut v = Vec::new();
    for prefix in 0..chunks {
        for i in 0..6000u64 {
            v.push((prefix << 16) | ((i * 7) % 65536));
        }
    }
    v.sort_unstable();
    v.dedup();
    db.insert_many(key, &v).expect("insert");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_live_flight_query_is_captured_as_tracing_events() {
    let events = Arc::new(Mutex::new(Vec::new()));
    tracing::subscriber::set_global_default(Collector(Arc::clone(&events)))
        .expect("one subscriber, one test binary");

    let dir = tempfile::tempdir().expect("tempdir");
    let db = Arc::new(
        Db::open_with(
            dir.path(),
            DbOptions {
                shards: 2,
                ..Default::default()
            },
        )
        .expect("open"),
    );
    dense(&db, 7, 4);
    dense(&db, 8, 3);
    db.checkpoint().expect("checkpoint");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
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
            .expect("serve");
    });

    let ch = Channel::from_shared(format!("http://{addr}"))
        .expect("url")
        .connect()
        .await
        .expect("connect");
    let mut client = FlightServiceClient::new(ch);

    let expr = SetExpr::And(vec![SetExpr::Key(7), SetExpr::Key(8)]);
    let info = client
        .get_flight_info(FlightDescriptor::new_cmd(expr.encode()))
        .await
        .expect("get_flight_info")
        .into_inner();
    assert!(
        info.total_records >= 0,
        "the count is exact, not a sentinel"
    );

    // Same query again: the point is that a repeat produces the *same* keys,
    // which is what makes reuse visible to the observer.
    client
        .get_flight_info(FlightDescriptor::new_cmd(expr.encode()))
        .await
        .expect("second get_flight_info");
    let _ = tx.send(());

    let seen = events.lock().expect("lock").clone();
    let queries: Vec<&HashMap<String, String>> =
        seen.iter().filter(|e| e.contains_key("keys")).collect();
    let described: Vec<&HashMap<String, String>> =
        seen.iter().filter(|e| e.contains_key("key")).collect();
    let shapes: Vec<&HashMap<String, String>> =
        seen.iter().filter(|e| e.contains_key("shape")).collect();

    assert_eq!(queries.len(), 2, "two queries, two query events");
    assert_eq!(
        queries[0].get("keys").map(String::as_str),
        Some("7,8"),
        "the posting lists the query named"
    );
    assert_eq!(
        queries[0], queries[1],
        "an identical query must yield identical keys, or the trace reports \
         zero reuse regardless of the workload"
    );

    // Dimensions are emitted once per posting list, not once per query. That
    // is what takes the `key_expr` plan build off the per-query path.
    assert_eq!(
        described.len(),
        2,
        "two posting lists across two queries, described once each"
    );
    let mut dims: Vec<(&str, &str)> = described
        .iter()
        .map(|e| {
            (
                e.get("key").expect("key").as_str(),
                e.get("chunks").expect("chunks").as_str(),
            )
        })
        .collect();
    dims.sort_unstable();
    assert_eq!(
        dims,
        vec![("7", "4"), ("8", "3")],
        "four chunks under key 7, three under key 8"
    );

    assert_eq!(shapes.len(), 2, "two queries, two shape events");
    assert_eq!(
        shapes[0].get("shape"),
        shapes[1].get("shape"),
        "the same query plans to the same shape"
    );
    for s in &shapes {
        s.get("thread")
            .expect("every shape event names its thread")
            .parse::<u64>()
            .expect("a decimal u64");
    }
}
