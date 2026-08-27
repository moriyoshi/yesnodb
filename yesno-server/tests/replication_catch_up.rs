//! The M7 gate: follower catch-up from a physical bootstrap, then set equality.
//!
//! The follower here does **not** have a bespoke apply path. It writes the
//! frames it receives into its own `shard-NNNN.wal` and opens a `Db`, which
//! replays them through ordinary crash recovery. That is the whole argument for
//! shipping raw WAL bytes rather than wrapping them in Flight: there is one
//! framing and one decoder, and this test exercises the same one a crash does.
//!
//! Bootstrap is physical. The leader's `.yno` is copied as-is and is
//! **consistent by construction** ( I4: a checkpoint persists only state at or
//! below the visible watermark ), so there is no hot-backup protocol and no
//! torn-page problem — which is what makes a follower queryable in milliseconds
//! instead of after a full parse-and-rebuild.

use std::collections::BTreeSet;

use tonic::transport::Server;
use yesno_core::{Db, DbOptions};
use yesno_server::replication::pb::replication_client::ReplicationClient;
use yesno_server::replication::pb::replication_server::ReplicationServer;
use yesno_server::replication::{pb, FollowerClient, LeaderService};

const SHARDS: usize = 1;

fn opts() -> DbOptions {
    DbOptions {
        shards: SHARDS,
        ..Default::default()
    }
}

fn set_of(db: &Db, key: u64) -> BTreeSet<u64> {
    db.snapshot().unwrap().load(key).unwrap().iter().collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_follower_bootstraps_physically_then_catches_up_to_set_equality() {
    let leader_dir = tempfile::tempdir().unwrap();
    let follower_dir = tempfile::tempdir().unwrap();

    // ---- leader: some checkpointed state, then commits that live only in the WAL
    let (want_a, want_b, uuid) = {
        let db = Db::open_with(leader_dir.path(), opts()).unwrap();
        db.insert_many(1, &(0..4_000u64).map(|i| i * 3).collect::<Vec<_>>())
            .unwrap();
        db.checkpoint().unwrap();

        // Deliberately after the checkpoint: these exist only as log records, so
        // the test fails if the follower bootstraps but never catches up.
        db.insert_many(1, &(0..4_000u64).map(|i| i * 3 + 1).collect::<Vec<_>>())
            .unwrap();
        db.insert_range(2, 100_000, 160_000).unwrap();

        // The MANIFEST, not a bare UUID: since 2026-08-28 it carries the shard
        // count and the `vshard -> shard` map as well as the identity, and a
        // follower that had the identity but not the map would route keys to
        // shards that do not hold them.
        let manifest = std::fs::read(leader_dir.path().join("MANIFEST")).unwrap();
        (set_of(&db, 1), set_of(&db, 2), manifest)
        // Two different things now, and conflating them is a real mistake:
        // the *manifest* is what the follower must be given, and the *uuid* is
        // the 16 bytes inside it that the status handshake reports.
        // The leader's Db is dropped here, releasing its lock. The service reads
        // files, so it would have worked either way.
    };
    assert!(
        !want_a.is_empty() && !want_b.is_empty(),
        "the leader wrote nothing"
    );

    // ---- serve
    let svc = LeaderService::new(leader_dir.path(), SHARDS);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let shutdown = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        Server::builder()
            .add_service(ReplicationServer::new(svc))
            .serve_with_incoming_shutdown(
                tokio_stream::wrappers::TcpListenerStream::new(listener),
                async {
                    let _ = shutdown.1.await;
                },
            )
            .await
            .unwrap();
    });

    let mut client = ReplicationClient::connect(format!("http://{addr}"))
        .await
        .expect("the leader must be reachable");

    // ---- status
    let st = client
        .status(pb::StatusRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(st.shard_count, SHARDS as u32);
    assert_eq!(
        st.db_uuid,
        yesno_core::database_uuid(leader_dir.path())
            .unwrap()
            .to_vec(),
        "the follower must be able to tell leaders apart"
    );
    assert_eq!(st.db_uuid.len(), 16, "an identity, not the whole manifest");
    assert!(st.end_lsn[0] > 0, "the leader reports an empty log");

    // ---- bootstrap and catch up through the **shipped** follower
    //
    // This used to be sixty lines of hand-rolled client here in the test: fetch
    // the snapshot, write the image, accumulate frames, write the log. The gate
    // therefore proved that the *test's* follower could catch up, which is not
    // the claim — the replication module shipped only the leader half, and
    // `yesno_core::repl::Follower` had no caller at all.
    std::fs::write(follower_dir.path().join("MANIFEST"), &uuid).unwrap();
    let mut follower = FollowerClient::new(follower_dir.path(), 0, []);

    let replay_off = follower.bootstrap_shard(&mut client, 0).await.unwrap();
    // This asserted `replay_off > 0` until 2026-08-28, and that assertion was
    // pinning a bug rather than a property: the field reported the log's current
    // *end*, so a steady-state follower resuming there skipped every commit made
    // since the checkpoint. What is true, and what the leader has to get right,
    // is that the offset names a point the log still holds.
    assert!(
        replay_off <= st.end_lsn[0],
        "the replay offset is past the log it refers to"
    );

    // From `replay_off`, which is what a follower actually does.
    //
    // It used to be `0`, on the argument that the image already covers
    // everything below its checkpoint so re-shipping is idempotent. That
    // argument was sound and its *spelling* stopped being: once LSNs became
    // global ( since 2026-08-28 ) zero is not "the start of
    // the log" but a point the leader cut away generations ago, and asking for
    // it now earns a `FailedPrecondition` telling this follower to bootstrap
    // again — correctly. The base of the retained log is `replay_off`, and
    // resuming there ships exactly the records the image does not have.
    let moved = follower
        .catch_up_shard(&mut client, 0, replay_off, 64 * 1024)
        .await
        .unwrap();
    assert_eq!(
        moved.bytes,
        st.end_lsn[0] - replay_off,
        "did not receive the whole retained log"
    );
    assert!(moved.records > 0, "the catch-up applied no records");
    assert_eq!(
        moved.next_lsn, st.end_lsn[0],
        "the resume cursor must sit at the end of what was shipped"
    );

    // The follower's watermark is the leader's own rule, not a byte count: a
    // multi-shard commit counts only once every participant has arrived.
    assert_eq!(
        follower.visible(),
        st.visible_version,
        "the follower's watermark did not reach the leader's"
    );
    assert_eq!(follower.lag_versions(st.visible_version), 0);

    // And the leader is told, which is what drives its retention floor.
    let lag = follower.ack(&mut client, 0).await.unwrap();
    assert_eq!(lag, 0, "the leader still thinks this follower is behind");

    // ---- the follower applies by *recovering*, not by a second decoder
    let follower = Db::open_with(follower_dir.path(), opts()).unwrap();
    assert_eq!(
        set_of(&follower, 1),
        want_a,
        "key 1 diverged after catch-up"
    );
    assert_eq!(
        set_of(&follower, 2),
        want_b,
        "key 2 diverged after catch-up"
    );

    // Set equality, not byte equality: the follower runs its own allocator and
    // checkpointer, so it may hold a different container encoding for the same
    // chunk. That is legitimate and is why `fsck --compare` compares contents.
    let _ = shutdown.0.send(());
}

/// A follower asking for a shard the leader does not have must be told so.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_shard_is_rejected_rather_than_served_empty() {
    let dir = tempfile::tempdir().unwrap();
    {
        Db::open_with(dir.path(), opts())
            .unwrap()
            .insert(1, 1)
            .unwrap();
    }

    let svc = LeaderService::new(dir.path(), SHARDS);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        Server::builder()
            .add_service(ReplicationServer::new(svc))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
            .unwrap();
    });

    let mut client = ReplicationClient::connect(format!("http://{addr}"))
        .await
        .unwrap();
    let err = client
        .subscribe(pb::SubscribeRequest {
            shard: 99,
            after_lsn: 0,
            max_batch_bytes: 0,
        })
        .await
        .expect_err("shard 99 does not exist");
    assert_eq!(
        err.code(),
        tonic::Code::OutOfRange,
        "wrong status for a bad shard"
    );
}

/// A follower bootstrapped **entirely over the wire** — no `std::fs::copy`.
///
/// This is the gap `FetchManifest` closed, and it was invisible because every
/// other test in this tree seeds the follower's MANIFEST by copying the leader's
/// file. That works because both ends share a disk *in a test*; across a network
/// there was no RPC that could hand it over, so "leader/follower deployment" was
/// not something an operator could actually perform.
///
/// The assertion that matters is not that the bytes arrive — it is that the
/// follower **routes keys the way the leader does** afterwards. A follower with
/// the identity but not the `vshard -> shard` map opens fine and answers for the
/// wrong shards.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_follower_bootstraps_over_the_wire_with_no_shared_filesystem() {
    const SHARDS: usize = 4;

    let leader_dir = tempfile::tempdir().unwrap();
    let follower_dir = tempfile::tempdir().unwrap();

    // Keys chosen so they land on different shards: the routing map is the
    // thing under test, so a single-shard corpus would prove nothing.
    let keys: Vec<u64> = (0..64u64).collect();
    let (want, leader_uuid) = {
        let db = Db::open_with(
            leader_dir.path(),
            DbOptions {
                shards: SHARDS,
                ..Default::default()
            },
        )
        .unwrap();
        for k in &keys {
            db.insert_range(*k, k * 100, k * 100 + 40).unwrap();
        }
        db.checkpoint().unwrap();
        let snap = db.snapshot().unwrap();
        let want: Vec<BTreeSet<u64>> = keys
            .iter()
            .map(|k| snap.load(*k).unwrap().iter().collect())
            .collect();
        drop(snap);
        (want, yesno_core::database_uuid(leader_dir.path()).unwrap())
    };

    let svc = LeaderService::new(leader_dir.path(), SHARDS);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        Server::builder()
            .add_service(ReplicationServer::new(svc))
            .serve_with_incoming_shutdown(
                tokio_stream::wrappers::TcpListenerStream::new(listener),
                async {
                    let _ = stop_rx.await;
                },
            )
            .await
            .unwrap();
    });
    let mut client = ReplicationClient::connect(format!("http://{addr}"))
        .await
        .unwrap();

    // ---- everything the follower learns, it learns from the socket
    let mut follower = FollowerClient::new(follower_dir.path(), 0, []);
    let got_uuid = follower.fetch_manifest(&mut client).await.unwrap();
    assert_eq!(
        got_uuid, leader_uuid,
        "the follower adopted a different identity"
    );

    let st = client
        .status(pb::StatusRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(st.shard_count as usize, SHARDS);

    for shard in 0..SHARDS as u32 {
        let off = follower.bootstrap_shard(&mut client, shard).await.unwrap();
        follower
            .catch_up_shard(&mut client, shard, off, 64 * 1024)
            .await
            .unwrap();
    }

    // ---- and the payoff: same contents, and same routing
    let replica = Db::open_with(
        follower_dir.path(),
        DbOptions {
            // Deliberately **wrong**, and it must not matter: the persisted
            // MANIFEST's count wins. Passing the right number here would hide a
            // follower that had invented its own map.
            shards: 1,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        replica.shard_count(),
        SHARDS,
        "the follower did not adopt the leader's shard count"
    );

    let snap = replica.snapshot().unwrap();
    for (i, k) in keys.iter().enumerate() {
        let got: BTreeSet<u64> = snap.load(*k).unwrap().iter().collect();
        assert_eq!(&got, &want[i], "key {k} diverged");
    }
    // Routing itself, not just contents: every key must live where the leader
    // put it.
    let leader = Db::open_with(
        leader_dir.path(),
        DbOptions {
            shards: SHARDS,
            ..Default::default()
        },
    );
    if let Ok(leader) = leader {
        for k in &keys {
            assert_eq!(
                replica.shard_of(*k),
                leader.shard_of(*k),
                "key {k} routes to a different shard on the follower"
            );
        }
    }

    let _ = stop_tx.send(());
}

/// Seeding twice is a check, not a second fetch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetching_a_manifest_twice_keeps_the_local_one() {
    let leader_dir = tempfile::tempdir().unwrap();
    let follower_dir = tempfile::tempdir().unwrap();
    {
        let db = Db::open_with(leader_dir.path(), opts()).unwrap();
        db.insert(1, 1).unwrap();
    }

    let svc = LeaderService::new(leader_dir.path(), 1);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        Server::builder()
            .add_service(ReplicationServer::new(svc))
            .serve_with_incoming_shutdown(
                tokio_stream::wrappers::TcpListenerStream::new(listener),
                async {
                    let _ = stop_rx.await;
                },
            )
            .await
            .unwrap();
    });
    let mut client = ReplicationClient::connect(format!("http://{addr}"))
        .await
        .unwrap();

    let mut f = FollowerClient::new(follower_dir.path(), 0, []);
    let first = f.fetch_manifest(&mut client).await.unwrap();
    let bytes = std::fs::read(follower_dir.path().join("MANIFEST")).unwrap();
    let second = f.fetch_manifest(&mut client).await.unwrap();
    assert_eq!(first, second);
    assert_eq!(
        bytes,
        std::fs::read(follower_dir.path().join("MANIFEST")).unwrap(),
        "the second fetch rewrote the local MANIFEST"
    );
    // And no temporary is left behind, because the next `Db::open` would not
    // know what to make of it.
    assert!(!follower_dir.path().join("MANIFEST.tmp").exists());

    let _ = stop_tx.send(());
}
