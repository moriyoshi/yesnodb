//! Multi-shard atomicity, end to end over the wire.
//!
//! # Why this is a separate gate from `catch_up.rs`
//!
//! `catch_up.rs` runs at `SHARDS = 1`, where every claim about the follower's
//! watermark is vacuous: with one participant per commit the prefix rule and
//! "count the bytes" agree on every input, so a follower that simply reported
//! how much log it had received would pass it. The design's actual claim —
//! *a multi-shard commit becomes visible on the follower only once every
//! participant's records have arrived* — has no test that can fail unless a
//! commit genuinely spans shards and the shards arrive at different times.
//!
//! `yesno_core::repl`'s unit tests do make that claim, against a hand-built
//! `Log` of synthetic 24-byte records. What they cannot reach is whether a real
//! `WriteBatch` writes a `CommitIntent` naming the right participants into every
//! participant's log, whether the leader slices those logs on boundaries that
//! keep the intent and its marker together, and whether the follower still
//! resolves the prefix after the bytes have been through gRPC. That is three
//! separate places the participant set could be lost, and none of them is the
//! one `repl.rs` exercises.
//!
//! # Why the leader does not checkpoint here
//!
//! A checkpoint can reclaim generations below its replay position, so a follower
//! starting at version 0 never sees those versions and its prefix watermark stalls at 0 for ever —
//! correctly, since its base image already covers them. That makes
//! `follower.visible()` structurally zero in a bootstrap test and any assertion
//! about it unfalsifiable. Here the whole log is present from version 1, so the
//! watermark is a live quantity. See `steady_state.rs` for the bootstrap side.

mod replication_common;

use replication_common::{one_key_per_shard, opts, seed_follower_manifest, serve, set_of};
use std::collections::BTreeSet;
use yesno_core::Db;
use yesno_server::replication::FollowerClient;

const SHARDS: usize = 4;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_multi_shard_commit_is_invisible_until_every_participant_arrives() {
    let leader_dir = tempfile::tempdir().unwrap();
    let follower_dir = tempfile::tempdir().unwrap();

    let db = Db::open_with(leader_dir.path(), opts(SHARDS)).unwrap();
    let keys = one_key_per_shard(&db, SHARDS);

    // Version 1: one batch touching every shard.
    let mut b = db.batch();
    for (i, &k) in keys.iter().enumerate() {
        b.insert_range(k, i as u64 * 100_000, i as u64 * 100_000 + 5_000);
    }
    let spanning = b.commit().unwrap();
    assert_eq!(
        spanning.shards, SHARDS,
        "the batch must genuinely span every shard, or this test is about nothing"
    );
    assert_eq!(spanning.version, 1);

    // Version 2: a single-shard commit *after* it, on a shard that will be
    // caught up first. It must stay invisible too — the watermark is a prefix
    // rule, so a hole below stops everything above it.
    let mut b = db.batch();
    b.insert(keys[0], 999_999);
    let later = b.commit().unwrap();
    assert_eq!(later.shards, 1);
    assert_eq!(later.version, 2);

    let want: Vec<BTreeSet<u64>> = keys.iter().map(|&k| set_of(&db, k)).collect();

    let mut leader = serve(leader_dir.path(), SHARDS).await;
    seed_follower_manifest(leader_dir.path(), follower_dir.path());
    let mut follower = FollowerClient::new(follower_dir.path(), 0, []);

    // ---- shards arrive one at a time
    for s in 0..SHARDS as u32 {
        let moved = follower
            .catch_up_shard(&mut leader.client, s, 0, 64 * 1024)
            .await
            .unwrap();
        assert!(
            moved.records > 0,
            "shard {s} shipped nothing, so it is not a participant after all"
        );

        if (s as usize) < SHARDS - 1 {
            assert_eq!(
                follower.visible(),
                0,
                "version 1 is missing {} participants, so nothing may be visible \
                 — including version 2, which is complete but sits above the hole",
                SHARDS - 1 - s as usize
            );
        }
    }

    // ---- and only once the last one does
    assert_eq!(
        follower.visible(),
        2,
        "with every participant in hand, the prefix resolves through the later \
         single-shard commit as well"
    );
    assert_eq!(follower.lag_versions(2), 0);

    // ---- the follower applies by recovering, and agrees on contents
    drop(leader);
    let replica = Db::open_with(follower_dir.path(), opts(SHARDS)).unwrap();
    assert_eq!(replica.shard_count(), SHARDS);
    for (i, &k) in keys.iter().enumerate() {
        assert!(!want[i].is_empty(), "the leader wrote nothing for key {k}");
        assert_eq!(set_of(&replica, k), want[i], "key {k} diverged");
    }
}

/// The participant set has to survive the *wire*, not just the log.
///
/// A `CommitIntent` is written into every participant's stream so each is
/// independently interpretable. If the leader's batching dropped one, or the
/// follower's decode ignored it, the version would resolve on the first shard
/// alone — which is the failure the test above detects only by the order it
/// happens to catch shards up in. This one asks the question directly, by
/// catching up *only* the shard the later single-shard commit lives on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_participant_alone_never_resolves_a_spanning_version() {
    let leader_dir = tempfile::tempdir().unwrap();
    let follower_dir = tempfile::tempdir().unwrap();

    let db = Db::open_with(leader_dir.path(), opts(SHARDS)).unwrap();
    let keys = one_key_per_shard(&db, SHARDS);

    let mut b = db.batch();
    for &k in &keys {
        b.insert(k, 1);
    }
    assert_eq!(b.commit().unwrap().shards, SHARDS);

    let mut leader = serve(leader_dir.path(), SHARDS).await;
    seed_follower_manifest(leader_dir.path(), follower_dir.path());
    let mut follower = FollowerClient::new(follower_dir.path(), 0, []);

    let moved = follower
        .catch_up_shard(&mut leader.client, 0, 0, 64 * 1024)
        .await
        .unwrap();
    assert!(
        moved.records > 0,
        "shard 0 is a participant and shipped nothing"
    );
    assert_eq!(
        follower.visible(),
        0,
        "a version naming four participants resolved on one — the intent was \
         lost between the leader's log and the follower's watermark"
    );
    assert_eq!(
        follower.lag_versions(1),
        1,
        "and the follower must report itself a version behind"
    );
}

/// `Ack` drives the leader's retention floor, so a follower that has fallen
/// behind must be *reported* behind. Only the zero case was covered, and zero is
/// what a stub returning a constant would also produce.
///
/// The lag here is created the way it is created in production — the leader
/// keeps committing while the follower is idle — rather than by stopping a
/// catch-up early, which `catch_up_shard` deliberately will not do.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ack_reports_a_follower_the_leader_has_run_ahead_of() {
    let leader_dir = tempfile::tempdir().unwrap();
    let follower_dir = tempfile::tempdir().unwrap();

    let db = Db::open_with(leader_dir.path(), opts(1)).unwrap();
    for i in 0..20u64 {
        db.insert_range(1, i * 1_000, i * 1_000 + 500).unwrap();
    }

    let mut leader = serve(leader_dir.path(), 1).await;
    seed_follower_manifest(leader_dir.path(), follower_dir.path());
    let mut follower = FollowerClient::new(follower_dir.path(), 0, []);

    let first = follower
        .catch_up_shard(&mut leader.client, 0, 0, 0)
        .await
        .unwrap();
    assert!(first.records > 0);
    assert_eq!(
        follower.ack(&mut leader.client, 0).await.unwrap(),
        0,
        "a fully caught-up follower must not be reported behind"
    );

    // The leader moves on while the follower sits still.
    let before = first.next_lsn;
    for i in 20..40u64 {
        db.insert_range(1, i * 1_000, i * 1_000 + 500).unwrap();
    }
    let end = leader
        .client
        .status(yesno_server::replication::pb::StatusRequest {})
        .await
        .unwrap()
        .into_inner()
        .end_lsn[0];
    assert!(end > before, "the leader's log did not grow");

    assert_eq!(
        follower.ack(&mut leader.client, 0).await.unwrap(),
        end - before,
        "the leader must report exactly the un-applied bytes"
    );

    // ---- and resuming from the cursor, not from zero, drains it
    let second = follower
        .catch_up_shard(&mut leader.client, 0, before, 0)
        .await
        .unwrap();
    assert_eq!(second.next_lsn, end, "the resume did not reach the end");
    assert_eq!(
        second.bytes,
        end - before,
        "a resume must ship the tail, not the whole log again"
    );
    assert_eq!(follower.ack(&mut leader.client, 0).await.unwrap(), 0);

    let want = set_of(&db, 1);
    drop(leader);
    let replica = Db::open_with(follower_dir.path(), opts(1)).unwrap();
    assert_eq!(
        set_of(&replica, 1),
        want,
        "the replica diverged after a resume"
    );
}
