//! Shared plumbing for the leader/follower tests.
//!
//! Standing a `LeaderService` on a loopback port is ten lines of tonic
//! boilerplate that says nothing about replication, and it was already
//! duplicated once. It lives here so a test file is only the sequence it pins.
//!
//! Each integration-test binary compiles this module whole and uses a
//! subset, so unused helpers are normal here and `dead_code` is silenced for
//! that reason and no other — a helper with no caller anywhere is still a
//! helper to delete.

#![allow(dead_code)]

use std::collections::BTreeSet;
use std::path::Path;

use tonic::transport::Channel;
use tonic::transport::Server;
use yesno_core::{Db, DbOptions};
use yesno_server::replication::pb::replication_client::ReplicationClient;
use yesno_server::replication::pb::replication_server::ReplicationServer;
use yesno_server::replication::{FollowerClient, LeaderService};

pub fn opts(shards: usize) -> DbOptions {
    DbOptions {
        shards,
        ..Default::default()
    }
}

pub fn set_of(db: &Db, key: u64) -> BTreeSet<u64> {
    db.snapshot().unwrap().load(key).unwrap().iter().collect()
}

/// One key routed to each shard, in shard order.
///
/// Asserted rather than assumed. `vshard_of` is a hash, so "keys 0..n spread
/// over n shards" is a property of the hash rather than of the loop, and a
/// multi-shard test whose batch quietly landed on two shards would still pass
/// most of its assertions.
pub fn one_key_per_shard(db: &Db, shards: usize) -> Vec<u64> {
    let mut found = vec![None; shards];
    for k in 0u64.. {
        if k > 100_000 {
            break;
        }
        let s = db.shard_of(k);
        if found[s].is_none() {
            found[s] = Some(k);
        }
        if found.iter().all(Option::is_some) {
            break;
        }
    }
    found
        .into_iter()
        .enumerate()
        .map(|(s, k)| k.unwrap_or_else(|| panic!("no key routes to shard {s}")))
        .collect()
}

/// The follower must be given the leader's MANIFEST, not just its identity: it
/// carries the shard count and the `vshard -> shard` map, and a follower with
/// the identity but not the map would route keys to shards that do not hold
/// them.
pub fn seed_follower_manifest(leader: &Path, follower: &Path) {
    std::fs::copy(leader.join("MANIFEST"), follower.join("MANIFEST"))
        .expect("the leader has no MANIFEST to hand over");
}

/// A `LeaderService` running on loopback. Dropping it shuts the server down.
pub struct Leader {
    pub client: ReplicationClient<Channel>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for Leader {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

pub async fn serve(dir: &Path, shards: usize) -> Leader {
    serve_svc(LeaderService::new(dir, shards)).await
}

/// The same, for a leader that holds log back for the followers that ack.
pub async fn serve_retaining(
    dir: &Path,
    shards: usize,
    floor: yesno_core::repl::RetentionFloor,
) -> Leader {
    serve_svc(LeaderService::with_retention(dir, shards, floor)).await
}

async fn serve_svc(svc: LeaderService) -> Leader {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        Server::builder()
            .add_service(ReplicationServer::new(svc))
            .serve_with_incoming_shutdown(
                tokio_stream::wrappers::TcpListenerStream::new(listener),
                async {
                    let _ = rx.await;
                },
            )
            .await
            .unwrap();
    });
    let client = ReplicationClient::connect(format!("http://{addr}"))
        .await
        .expect("the leader must be reachable");
    Leader {
        client,
        shutdown: Some(tx),
    }
}

/// Where a shard's log ends, **in LSNs**, as the leader reports it.
///
/// Tests used to read `metadata(shard-NNNN.wal).len()` for this, and it was
/// right only until a leader checkpointed: the file restarts at byte 0 while the
/// shard's LSN sequence carries on, so the byte length undercounts by everything
/// ever reclaimed. Ask the leader, which knows its own base.
pub async fn end_lsn(client: &mut ReplicationClient<Channel>, shard: usize) -> u64 {
    client
        .status(yesno_server::replication::pb::StatusRequest {})
        .await
        .unwrap()
        .into_inner()
        .end_lsn[shard]
}

/// Catch every shard up from the start of its log, returning bytes per shard.
pub async fn catch_up_all(
    follower: &mut FollowerClient,
    client: &mut ReplicationClient<Channel>,
    shards: usize,
    max_batch_bytes: u32,
) -> Vec<u64> {
    let mut out = Vec::with_capacity(shards);
    for s in 0..shards as u32 {
        out.push(
            follower
                .catch_up_shard(client, s, 0, max_batch_bytes)
                .await
                .unwrap()
                .bytes,
        );
    }
    out
}
