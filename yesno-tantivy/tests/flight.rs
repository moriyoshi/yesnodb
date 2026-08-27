#![cfg(feature = "flight")]

use std::sync::Arc;

use arrow_flight::flight_service_server::FlightServiceServer;
use tantivy::collector::Count;
use tantivy::schema::{Schema, FAST, INDEXED};
use tantivy::{doc, Index};
use tonic::transport::Server;
use yesno_core::Db;
use yesno_flight::{SetExpr, YesnoClient, YesnoFlightService};
use yesno_tantivy::flight::{FlightQueryPreparer, RemotePrepareError};
use yesno_tantivy::FastFieldOrdinalResolver;

async fn serve(
    db: Arc<Db>,
) -> (
    String,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        Server::builder()
            .add_service(FlightServiceServer::new(YesnoFlightService::new(db)))
            .serve_with_incoming_shutdown(
                tokio_stream::wrappers::TcpListenerStream::new(listener),
                async {
                    let _ = stopped.await;
                },
            )
            .await
            .unwrap();
    });
    (format!("http://{address}"), stop, task)
}

fn tantivy_searcher() -> (tantivy::IndexReader, tantivy::Searcher) {
    let mut schema = Schema::builder();
    let stable_id = schema.add_u64_field("stable_id", FAST | INDEXED);
    let index = Index::create_in_ram(schema.build());
    let reader = index.reader().unwrap();
    let mut writer = index.writer(50_000_000).unwrap();
    for ordinal in [10u64, 20, 30] {
        writer.add_document(doc!(stable_id => ordinal)).unwrap();
    }
    writer.commit().unwrap();
    reader.reload().unwrap();
    let searcher = reader.searcher();
    (reader, searcher)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_query_supports_current_and_strict_pinned_versions() {
    let directory = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::open(directory.path()).unwrap());
    db.insert_many(7, &[10, 30]).unwrap();

    let (url, stop, task) = serve(db.clone()).await;
    let mut client = YesnoClient::connect(url).await.unwrap();
    let (_reader, searcher) = tantivy_searcher();
    let resolver = FastFieldOrdinalResolver::build(&searcher, "stable_id").unwrap();

    let first = FlightQueryPreparer::new(SetExpr::Key(7))
        .prepare(&mut client, &searcher, &resolver)
        .await
        .unwrap();
    let pinned_version = first.yesno_version().unwrap();
    assert_eq!(first.matched_docs(), 2);
    assert_eq!(searcher.search(&first, &Count).unwrap(), 2);

    db.insert(7, 20).unwrap();

    let pinned = FlightQueryPreparer::new(SetExpr::Key(7))
        .pinned(pinned_version)
        .prepare(&mut client, &searcher, &resolver)
        .await
        .unwrap();
    assert_eq!(pinned.yesno_version(), Some(pinned_version));
    assert_eq!(searcher.search(&pinned, &Count).unwrap(), 2);

    let current = FlightQueryPreparer::new(SetExpr::Key(7))
        .prepare(&mut client, &searcher, &resolver)
        .await
        .unwrap();
    assert!(current.yesno_version().unwrap() > pinned_version);
    assert_eq!(searcher.search(&current, &Count).unwrap(), 3);

    let limited = FlightQueryPreparer::new(SetExpr::Key(7))
        .max_matches(2)
        .prepare(&mut client, &searcher, &resolver)
        .await;
    assert!(matches!(
        limited,
        Err(RemotePrepareError::TooManyMatches {
            promised: 3,
            limit: 2
        })
    ));

    stop.send(()).unwrap();
    task.await.unwrap();
}
