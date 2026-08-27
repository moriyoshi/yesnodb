//! The public convenience client must cover yesno's whole Flight vocabulary:
//! plan/fetch, exact count-only queries, expression queries, pair ingest/removal,
//! and administrative actions.

use std::sync::Arc;

use arrow_flight::flight_service_server::FlightServiceServer;
use futures::TryStreamExt;
use tonic::transport::Server;
use yesno_core::Db;
use yesno_flight::{SetExpr, YesnoClient, YesnoFlightService};

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_covers_queries_mutations_and_actions() {
    let directory = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::open(directory.path()).unwrap());
    let (url, stop, task) = serve(db).await;
    let mut client = YesnoClient::connect(url).await.unwrap();

    let key_seven: Vec<_> = (0..9_000).map(|ordinal| (7, ordinal)).collect();
    assert_eq!(client.insert(key_seven).await.unwrap(), 9_000);
    assert_eq!(
        client
            .insert(std::iter::empty::<(u64, u64)>())
            .await
            .unwrap(),
        0
    );
    assert_eq!(client.insert_batch([(9, 1), (9, 3)]).await.unwrap(), 2);
    assert_eq!(client.remove_batch([(9, 1)]).await.unwrap(), 1);
    assert!(client.insert_one(9, 5).await.unwrap());
    assert!(!client.insert_one(9, 5).await.unwrap());
    assert!(client.remove_one(9, 5).await.unwrap());
    assert!(!client.remove_one(9, 5).await.unwrap());
    assert_eq!(client.keys().await.unwrap(), vec![7, 9]);

    assert_eq!(client.cardinality(7).await.unwrap(), 9_000);

    let planned = client.prepare_key(7).await.unwrap();
    assert_eq!(planned.total_records(), 9_000);
    assert_eq!(planned.ticket().key, 7);
    assert!(planned.version() > 0);

    // The query can be fetched through the raw batch surface without rebuilding
    // its descriptor or extracting its opaque Flight ticket.
    let raw_rows: usize = client
        .fetch_ticket(planned.ticket_bytes().to_vec())
        .await
        .unwrap()
        .map_ok(|batch| batch.num_rows())
        .try_fold(0, |total, rows| async move { Ok(total + rows) })
        .await
        .unwrap();
    assert_eq!(raw_rows, 9_000);

    let key_eight: Vec<_> = (0..9_000).step_by(2).map(|ordinal| (8, ordinal)).collect();
    assert_eq!(client.insert(key_eight).await.unwrap(), 4_500);

    let expression = SetExpr::And(vec![SetExpr::Key(7), SetExpr::Key(8)]);
    assert_eq!(client.query_cardinality(&expression).await.unwrap(), 4_500);
    let result = client.query(&expression).await.unwrap();
    assert_eq!(result.info().total_records(), 4_500);
    assert_eq!(
        result.collect_ordinals().await.unwrap(),
        (0..9_000).step_by(2).collect::<Vec<_>>()
    );

    let literal = SetExpr::And(vec![
        SetExpr::Key(7),
        SetExpr::literal([9, 0, 9, 65_536, 5]).unwrap(),
    ]);
    assert_eq!(client.query_cardinality(&literal).await.unwrap(), 3);
    assert_eq!(
        client
            .query(&literal)
            .await
            .unwrap()
            .collect_ordinals()
            .await
            .unwrap(),
        vec![0, 5, 9]
    );

    assert_eq!(client.remove([(7, 0), (7, 2)]).await.unwrap(), 2);
    assert_eq!(client.cardinality(7).await.unwrap(), 8_998);
    assert!(client.contains(7, 1).await.unwrap());
    assert!(!client.contains(7, 2).await.unwrap());

    let rows = client.get(7).await.unwrap();
    assert_eq!(rows.info().total_records(), 8_998);
    let rows = rows.collect_ordinals().await.unwrap();
    assert_eq!(rows.len(), 8_998);
    assert_eq!(rows.first(), Some(&1));
    assert!(!rows.contains(&2));

    let stats = client.stats().await.unwrap();
    assert!(stats.shards > 0);
    assert!(stats.wal_bytes > 0);
    assert!(client.clear(9).await.unwrap() > 0);
    assert!(client.clear(9).await.unwrap() > 0);
    assert_eq!(client.cardinality(9).await.unwrap(), 0);

    stop.send(()).unwrap();
    task.await.unwrap();
}

/// **Does a Flight client ever see half a multi-shard commit?** No — provided
/// it asks in **one request**.
///
/// `replica.rs` establishes multi-shard atomicity for the engine and for a
/// follower. This is the same property seen from the wire, and the Flight-side
/// qualifier is the interesting part: a single `SetExpr` is evaluated against a
/// single snapshot, so a union spanning two shards is atomic. **Two separate
/// requests are two snapshots**, and nothing on this surface lets a client read
/// two keys at one instant except by putting them in one expression. That is not
/// a defect — it is the boundary, and a client that reads key A and then key B
/// has straddled whatever commits landed in between.
///
/// The invariant is chosen so a torn read is *arithmetically* visible. Each
/// round commits one ordinal to key 1 and one to key 2 in a single batch, so the
/// union's cardinality is always **even**. An odd count is half a commit, and no
/// amount of lag or reordering can produce one legitimately.
///
/// **This test has teeth, and it takes a two-part sabotage to prove it.**
/// Making `Db::snapshot` read one version *above* the resolved prefix — breaking
/// the "readers snapshot `visible`, not `next`" rule the commit path relies on —
/// is **not enough on its own**: the window between the two shards' memtable
/// writes is too narrow to hit in 400 rounds. Widening that window as well
/// ( a sleep between iterations of the per-shard write loop ) produces the tear
/// immediately: `a Flight query observed 1 ordinals`. So a single sabotage
/// passing here would have meant the sabotage was too weak, not that the test
/// was.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_flight_query_never_observes_half_a_multi_shard_commit() {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    let directory = tempfile::tempdir().unwrap();
    let db = Arc::new(
        Db::open_with(
            directory.path(),
            yesno_core::DbOptions {
                shards: 4,
                ..Default::default()
            },
        )
        .unwrap(),
    );
    let (url, stop, task) = serve(db.clone()).await;
    let mut client = YesnoClient::connect(url).await.unwrap();

    const ROUNDS: u64 = 400;
    let done = Arc::new(AtomicBool::new(false));
    let multi_shard = Arc::new(AtomicU64::new(0));

    let writer = {
        let db = db.clone();
        let done = done.clone();
        let multi_shard = multi_shard.clone();
        std::thread::spawn(move || {
            for r in 0..ROUNDS {
                // One batch, two keys. `Committed::shards` is what proves this
                // test is exercising the multi-shard path at all rather than
                // quietly degenerating to one shard.
                let mut b = db.batch();
                b.insert(1, r);
                b.insert(2, 1_000_000 + r);
                let committed = b.commit().unwrap();
                if committed.shards > 1 {
                    multi_shard.fetch_add(1, Ordering::Relaxed);
                }
            }
            done.store(true, Ordering::Release);
        })
    };

    let union = SetExpr::Or(vec![SetExpr::Key(1), SetExpr::Key(2)]);
    let mut samples = 0u64;
    let mut mid_flight = 0u64;
    let mut highest = 0u64;
    while !done.load(Ordering::Acquire) {
        let n = client.query_cardinality(&union).await.unwrap();
        assert_eq!(
            n % 2,
            0,
            "a Flight query observed {n} ordinals — an odd count is half of a \
             two-shard commit"
        );
        samples += 1;
        highest = highest.max(n);
        if n > 0 && n < 2 * ROUNDS {
            mid_flight += 1;
        }
    }
    writer.join().unwrap();

    // ---- Non-vacuity, asserted rather than hoped for.
    assert!(
        multi_shard.load(Ordering::Relaxed) > 0,
        "no commit touched more than one shard; the test never exercised the \
         property it claims to check"
    );
    assert!(
        samples > 0,
        "the reader never managed a single query while the writer ran"
    );
    assert!(
        mid_flight > 0,
        "every sample landed either before the first commit or after the last \
         ({samples} samples, highest {highest}); nothing was observed *during* \
         the writes, so an odd count could never have appeared"
    );

    // ---- And the final state is whole.
    let total = client.query_cardinality(&union).await.unwrap();
    assert_eq!(
        total,
        2 * ROUNDS,
        "every committed ordinal is present at the end"
    );

    let _ = stop.send(());
    let _ = task.await;
}
