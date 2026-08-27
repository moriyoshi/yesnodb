//! The bootstrap seam and the transport's edges.
//!
//! `catch_up.rs` proves a follower can be stood up from a physical image and
//! reach set equality **when it re-ships the whole log from zero**. That is the
//! easy direction, and it is idempotent by construction: replay skips records at
//! or below the version the image already carries, so shipping too much is free.
//!
//! Shipping too *little* is not free, and it is the direction a steady-state
//! follower actually takes — it resumes at the offset the bootstrap handed it.
//! `catch_up.rs` says in as many words that this "would also be correct and is
//! what a steady-state follower does", and never runs it. It was not correct.
//! See `a_bootstrapped_follower_resumes_from_the_offset_the_image_carries`.

mod replication_common;

use replication_common::{end_lsn, opts, seed_follower_manifest, serve, serve_retaining, set_of};
use yesno_core::{Db, DbOptions};
use yesno_server::replication::{pb, FollowerClient, FollowerError};

fn wal_len(dir: &std::path::Path, shard: u32) -> u64 {
    let (base, end) =
        yesno_core::wal::log_bounds(dir.join(format!("shard-{shard:04}.wal")), 0).unwrap();
    end - base
}

/// The regression for a silent data-loss bug in the documented bootstrap path.
///
/// A checkpoint records the first logical LSN its image does not contain.
/// `FetchBaseSnapshot` reported `metadata(active_wal).len()` instead — one
/// file's byte length rather than that replay position — so every
/// commit made between the leader's last checkpoint and the follower's bootstrap
/// fell below the offset the follower was told to resume at and was skipped.
///
/// Nothing reported a gap. The cursor was valid, the checksums matched, the
/// catch-up returned `Ok` having moved zero bytes, and the replica came up with
/// a window of writes simply absent. The only observable is set equality on a
/// key written in that window, which is why this test writes one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bootstrapped_follower_resumes_from_the_offset_the_image_carries() {
    let leader_dir = tempfile::tempdir().unwrap();
    let follower_dir = tempfile::tempdir().unwrap();

    let db = Db::open_with(leader_dir.path(), opts(1)).unwrap();
    db.insert_range(1, 0, 10_000).unwrap();
    db.checkpoint().unwrap();

    // Written *after* the checkpoint, so it lives only in the log — and at an
    // offset below the log's current end, which is exactly what the old
    // `replay_off` pointed past.
    db.insert_range(2, 100_000, 110_000).unwrap();
    assert!(
        wal_len(leader_dir.path(), 0) > 0,
        "the post-checkpoint commit did not reach the log"
    );

    let (want1, want2) = (set_of(&db, 1), set_of(&db, 2));

    let mut leader = serve(leader_dir.path(), 1).await;
    // The LSN the log ends at, not its byte length. Since the checkpoint above
    // carries the base forward, the two differ by everything it reclaimed — and
    // comparing `replay_off` against the byte length would call a perfectly good
    // offset "past the end".
    let log_end = end_lsn(&mut leader.client, 0).await;
    seed_follower_manifest(leader_dir.path(), follower_dir.path());
    let mut follower = FollowerClient::new(follower_dir.path(), 0, []);

    let replay_off = follower
        .bootstrap_shard(&mut leader.client, 0)
        .await
        .unwrap();
    assert!(
        replay_off < log_end,
        "the replay offset ({replay_off}) is at or past the log's end ({log_end}), \
         so it is reporting where the leader is now rather than what the image covers"
    );

    // The steady-state resume, from the offset rather than from zero.
    let moved = follower
        .catch_up_shard(&mut leader.client, 0, replay_off, 0)
        .await
        .unwrap();
    assert!(
        moved.records > 0,
        "the resume shipped nothing, so the post-checkpoint commits were skipped"
    );
    assert_eq!(moved.next_lsn, log_end);

    drop(leader);
    let replica = Db::open_with(follower_dir.path(), opts(1)).unwrap();
    assert_eq!(
        set_of(&replica, 1),
        want1,
        "key 1 came from the base image and diverged anyway"
    );
    assert_eq!(
        set_of(&replica, 2),
        want2,
        "key 2 was committed after the checkpoint and never reached the replica"
    );
}

/// A base image bigger than one wire chunk must arrive whole and in order —
/// and without its holes.
///
/// # What changed, and what did not
///
/// This test used to assert **contiguity**: every chunk beginning where the
/// last one ended. That was the *means*, not the end. A shard image is sparse
/// — the store grows it to a whole 1 GiB mmap segment — and shipping the holes
/// cost the apparent size in the leader's memory, on the wire and on the
/// follower's disk, so the leader now skips runs that are entirely zero and
/// `offset` jumps.
///
/// The **end** is unchanged and is what this still pins: a silent gap
/// produces a file that *looks* complete and only decodes as corruption much
/// later, somewhere unrelated. Three things replace contiguity and all three
/// are checked below — the declared length, the ordering and bounds of every
/// chunk, and the leader's own count of the bytes it sent. Do not relax
/// these back into "the offsets look plausible": with holes skipped, a dropped
/// chunk is indistinguishable from a hole, and the byte count is the only thing
/// that can tell them apart.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_multi_chunk_base_image_arrives_whole_and_without_its_holes() {
    let leader_dir = tempfile::tempdir().unwrap();
    let follower_dir = tempfile::tempdir().unwrap();

    let db = Db::open_with(leader_dir.path(), opts(1)).unwrap();
    db.insert_range(1, 0, 200_000).unwrap();
    db.checkpoint().unwrap();
    let image = leader_dir.path().join("shard-0000.yno");
    let size = std::fs::metadata(&image).unwrap().len();

    let mut leader = serve(leader_dir.path(), 1).await;

    // Count the chunks directly, so "more than one" is a fact rather than an
    // inference from the file size and a constant this test does not own.
    let mut stream = leader
        .client
        .fetch_base_snapshot(pb::SnapshotRequest { shard: 0 })
        .await
        .unwrap()
        .into_inner();
    let (mut chunks, mut shipped, mut lasts) = (0u32, 0u64, 0u32);
    let (mut next, mut claimed) = (0u64, 0u64);
    while let Some(c) = stream.message().await.unwrap() {
        assert_eq!(c.shard, 0);
        assert_eq!(
            c.total_len, size,
            "every chunk must declare the image's length, or the follower \
             cannot turn the skipped runs back into holes"
        );
        assert!(
            c.offset >= next,
            "chunk {chunks} went backwards, to {} from {next}",
            c.offset
        );
        assert!(
            c.offset + c.data.len() as u64 <= size,
            "chunk {chunks} runs past the end of a {size}-byte image"
        );
        next = c.offset + c.data.len() as u64;
        shipped += c.data.len() as u64;
        if c.last {
            lasts += 1;
            claimed = c.data_len;
            assert!(c.data.is_empty(), "the final chunk carries no data");
        } else {
            assert_eq!(
                c.wal_replay_off, 0,
                "only the final chunk may carry the replay offset"
            );
            assert_eq!(c.data_len, 0, "only the final chunk may carry the count");
        }
        chunks += 1;
    }
    assert!(
        chunks > 2,
        "a {size}-byte image arrived in {chunks} chunk(s); this test no longer covers the split"
    );
    assert_eq!(lasts, 1, "exactly one chunk must be marked last");
    // What replaces `at == size`. The stream no longer carries the whole
    // image and must not: it carries every byte that is not a hole, and says
    // how many that was.
    assert_eq!(
        shipped, claimed,
        "the leader's count disagrees with what it sent"
    );
    // The saving, asserted rather than assumed. Without this the test passes
    // against a leader that ships every zero, which is the implementation it
    // exists to have replaced.
    assert!(
        shipped < size / 2,
        "the leader shipped {shipped} of a {size}-byte image; the holes are still on the wire"
    );

    // And the shipped follower lands the same bytes.
    seed_follower_manifest(leader_dir.path(), follower_dir.path());
    let mut follower = FollowerClient::new(follower_dir.path(), 0, []);
    follower
        .bootstrap_shard(&mut leader.client, 0)
        .await
        .unwrap();
    // Byte-identical, holes and all. The copy differs from its original only
    // in allocation, which is the whole claim.
    assert_eq!(
        std::fs::read(follower_dir.path().join("shard-0000.yno")).unwrap(),
        std::fs::read(&image).unwrap(),
        "the bootstrapped image is not byte-identical to the leader's"
    );
}

/// A follower past the leader's log is talking to the wrong leader, or to one
/// that was rebuilt behind it. The log only grows within a generation, so this
/// must be reported rather than wrapped around into arbitrary bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_follower_ahead_of_the_leader_is_told_rather_than_served() {
    let leader_dir = tempfile::tempdir().unwrap();
    let db = Db::open_with(leader_dir.path(), opts(1)).unwrap();
    db.insert_range(1, 0, 1_000).unwrap();

    let mut leader = serve(leader_dir.path(), 1).await;
    let end = end_lsn(&mut leader.client, 0).await;
    assert!(end > 0);
    let mut stream = leader
        .client
        .subscribe(pb::SubscribeRequest {
            shard: 0,
            after_lsn: end + 1,
            max_batch_bytes: 0,
        })
        .await
        .unwrap()
        .into_inner();
    let err = stream
        .message()
        .await
        .expect_err("a cursor past the log must not be served");
    assert_eq!(err.code(), tonic::Code::OutOfRange, "{err}");

    // And exactly at the end is *not* an error — that is a caught-up
    // follower, which must get a heartbeat. Without this the check above would
    // pass just as well against an off-by-one that refused the normal case.
    let mut ok = leader
        .client
        .subscribe(pb::SubscribeRequest {
            shard: 0,
            after_lsn: end,
            max_batch_bytes: 0,
        })
        .await
        .unwrap()
        .into_inner();
    let hb = ok
        .message()
        .await
        .unwrap()
        .expect("a heartbeat, not an error");
    assert!(hb.is_heartbeat && hb.records.is_empty());
}

/// The record-boundary cut, against a *real* log rather than a synthetic one.
///
/// `repl.rs` proves the publisher cuts on boundaries over a log of uniform
/// 24-byte bodies. A real WAL is not that: `SetRange` is tiny, a `ChunkImage` is
/// up to 8 KiB, and a batch budget smaller than a single record is a case the
/// uniform log cannot even express. If the cut ever landed mid-frame the
/// follower would reject the batch as a gap — or, worse, accept a prefix and
/// diverge — so the assertion is set equality, not a byte count.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_batch_budget_below_one_record_still_ships_the_whole_log() {
    let leader_dir = tempfile::tempdir().unwrap();
    let follower_dir = tempfile::tempdir().unwrap();

    let db = Db::open_with(leader_dir.path(), opts(1)).unwrap();
    // A mix of shapes, so the log holds records of very different sizes.
    db.insert_range(1, 0, 50_000).unwrap();
    db.insert_many(2, &(0..3_000u64).map(|i| i * 7).collect::<Vec<_>>())
        .unwrap();
    for i in 0..30u64 {
        db.insert(3, i * 1_000).unwrap();
    }
    let want: Vec<_> = (1..=3u64).map(|k| set_of(&db, k)).collect();

    let mut leader = serve(leader_dir.path(), 1).await;
    let end = end_lsn(&mut leader.client, 0).await;
    let bytes_in_log = wal_len(leader_dir.path(), 0);
    seed_follower_manifest(leader_dir.path(), follower_dir.path());
    let mut follower = FollowerClient::new(follower_dir.path(), 0, []);

    // 16 bytes is below the framed size of every record in that log, so the
    // publisher's "at least one record, budget or not" branch runs every time.
    // This leader has never checkpointed, so its base is 0 and the two spellings
    // of "the whole log" coincide — asserted, so the day that stops being true
    // the test says which one it meant.
    assert_eq!(
        end, bytes_in_log,
        "an uncut log's end lsn is its byte length"
    );
    let moved = follower
        .catch_up_shard(&mut leader.client, 0, 0, 16)
        .await
        .unwrap();
    assert_eq!(moved.bytes, bytes_in_log, "the whole log did not arrive");
    assert_eq!(moved.next_lsn, end);

    drop(leader);
    let replica = Db::open_with(follower_dir.path(), opts(1)).unwrap();
    for (i, w) in want.iter().enumerate() {
        let k = i as u64 + 1;
        assert!(!w.is_empty());
        assert_eq!(set_of(&replica, k), *w, "key {k} diverged");
    }
}

/// A follower keeps its place across its leader's checkpoint.
///
/// # What this used to be
///
/// Before 2026-08-28 a checkpoint cut the shard's one log file and later records
/// were written from **file offset zero again**, so every LSN a follower held stopped
/// meaning anything the moment its leader checkpointed — which is the steady
/// state, not an edge case, since checkpoints fire on a timer. The leader
/// answered the stale cursor with a *heartbeat*, and the follower sat there
/// permanently and silently behind:
///
/// ```text
/// gen 1 log = 160          follower catches up, cursor = 160
/// after ckpt log = 0
/// gen 2 log = 22400        resume -> Ok(bytes: 0, records: 0), next_lsn: 160
/// key 2: 0 of 1000200 ordinals on the replica
/// ```
///
/// Making that *loud* was the first repair; making LSNs **global** was the real
/// one. An LSN is now an offset in the shard's whole history rather than in the
/// current file, so a cut advances the log's base instead of rewinding it and a
/// cursor stays meaningful across any number of checkpoints. There is nothing to
/// re-bootstrap.
///
/// The assertion that matters is `records > 0` together with set equality on
/// **key 2**, which exists only in the generation written after the cut. A
/// follower that silently stopped would still hold key 1 and still return `Ok`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_follower_keeps_its_place_across_a_leader_checkpoint() {
    let leader_dir = tempfile::tempdir().unwrap();
    let follower_dir = tempfile::tempdir().unwrap();

    let db = Db::open_with(leader_dir.path(), opts(1)).unwrap();
    db.insert_range(1, 0, 5_000).unwrap();

    let mut leader = serve(leader_dir.path(), 1).await;
    seed_follower_manifest(leader_dir.path(), follower_dir.path());
    let mut follower = FollowerClient::new(follower_dir.path(), 0, []);
    let first = follower
        .catch_up_shard(&mut leader.client, 0, 0, 0)
        .await
        .unwrap();
    assert_eq!(
        first.next_lsn,
        end_lsn(&mut leader.client, 0).await,
        "the follower did not reach the end"
    );

    // The leader checkpoints — reclaiming the now-redundant generation — and
    // then writes a second generation longer than the first, so a cursor
    // interpreted as a file offset would land inside it. That was the silent
    // case.
    db.checkpoint().unwrap();
    assert_eq!(
        wal_len(leader_dir.path(), 0),
        0,
        "the checkpoint did not reclaim the redundant WAL, so this test is no longer about anything"
    );
    // The active file is empty and the logical log is not back at zero. The
    // next active generation begins at the prior end.
    assert_eq!(
        end_lsn(&mut leader.client, 0).await,
        first.next_lsn,
        "rollover must carry the shard's lsn sequence across it, not rewind it"
    );

    for i in 0..200u64 {
        db.insert_range(2, i * 10_000, i * 10_000 + 5_000).unwrap();
    }
    let after = end_lsn(&mut leader.client, 0).await;
    assert!(
        after > first.next_lsn,
        "the second generation wrote nothing"
    );

    // ---- the ordinary steady-state resume, with no re-bootstrap
    let moved = follower
        .catch_up_shard(&mut leader.client, 0, first.next_lsn, 0)
        .await
        .expect("a cursor from before a checkpoint must still be usable");
    assert!(
        moved.records > 0,
        "the resume shipped nothing — the cursor stopped meaning anything across the cut"
    );
    assert_eq!(moved.next_lsn, after);
    assert_eq!(follower.ack(&mut leader.client, 0).await.unwrap(), 0);

    let (want1, want2) = (set_of(&db, 1), set_of(&db, 2));
    drop(leader);
    let replica = Db::open_with(follower_dir.path(), opts(1)).unwrap();
    assert_eq!(set_of(&replica, 1), want1, "key 1 diverged across the cut");
    assert_eq!(
        set_of(&replica, 2),
        want2,
        "key 2 was written after the cut and never reached the replica"
    );
}

/// The other end of the same cursor check, and the one that is still not
/// survivable: a follower so far behind that the leader has already reclaimed
/// the log it wants.
///
/// LSNs being global makes a cursor *comparable* across a cut; it does not make
/// the bytes come back. A cursor below the retained log therefore has one
/// correct answer — bootstrap again — and the leader gives it, with the remedy
/// named. Nothing consumes `Ack`'s retention floor yet, so a leader will do
/// this to a follower that has stopped acking entirely.
///
/// **This comment said "nothing consumes `Ack`'s retention floor yet" until
/// 2026-09-14. It was wired on 2026-08-28** -- seventeen days earlier, by the
/// same change whose comment in `replication/mod.rs` records that "the floor
/// was nowhere" before it. The loop is closed end to end: the leader's ack
/// handler calls `RetentionFloor::observe_from`, and `Db::checkpoint` calls
/// `take` and sets `reclaim_through` to the floor rather than to
/// `wal_replay_lsn` whenever one is held.
///
/// It is bounded on both sides, which is why this still reaches the remedy. The
/// floor is **consumed, not peeked**, so a follower that dies stops holding the
/// log at the next checkpoint rather than until restart; and it is overridden
/// past `max_wal_bytes`, with a `forced_past_retention` event, so a follower
/// that keeps acking while falling further behind cannot fill the disk. This
/// test exercises the case past both: a cursor below the retained log.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cursor_below_the_retained_log_names_its_remedy() {
    let leader_dir = tempfile::tempdir().unwrap();
    let db = Db::open_with(leader_dir.path(), opts(1)).unwrap();
    db.insert_range(1, 0, 5_000).unwrap();
    db.checkpoint().unwrap();
    db.insert_range(2, 0, 5_000).unwrap();

    let mut leader = serve(leader_dir.path(), 1).await;
    let base = end_lsn(&mut leader.client, 0).await - wal_len(leader_dir.path(), 0);
    assert!(base > 0, "the checkpoint did not advance the base");

    let mut stream = leader
        .client
        .subscribe(pb::SubscribeRequest {
            shard: 0,
            after_lsn: 0,
            max_batch_bytes: 0,
        })
        .await
        .unwrap()
        .into_inner();
    let err = stream
        .message()
        .await
        .expect_err("a cursor below the retained log must not be served");
    assert_eq!(err.code(), tonic::Code::FailedPrecondition, "{err}");
    assert!(
        err.message().contains("bootstrap again"),
        "the error must name the remedy: {}",
        err.message()
    );

    // And exactly at the base is fine — that is a follower with nothing missing.
    let mut ok = leader
        .client
        .subscribe(pb::SubscribeRequest {
            shard: 0,
            after_lsn: base,
            max_batch_bytes: 0,
        })
        .await
        .unwrap()
        .into_inner();
    let b = ok.message().await.unwrap().expect("the base is servable");
    assert!(!b.is_heartbeat && !b.records.is_empty());
}

/// Pointing a follower at the wrong leader is refused **before a byte lands**.
///
/// # What this used to say
///
/// `Status` has reported `db_uuid` since M7 "so the follower must be able to
/// tell leaders apart", and nothing compared it. This test existed and asserted
/// the gap: `bootstrap_shard` succeeded — with the comment "the transport itself
/// has no way to know, and does not pretend to" — and the mistake surfaced only
/// at `Db::open`, where `ShardStore::open` checks the superblock against the
/// MANIFEST. A real safety net, and the last one, by which point a foreign
/// multi-megabyte image is already on disk.
///
/// The assertion that carries this is that the follower's `.yno` **does not
/// exist**. "The call returned `Err`" is satisfied by a client that writes the
/// whole image and then notices.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_follower_pointed_at_the_wrong_leader_writes_nothing() {
    let leader_dir = tempfile::tempdir().unwrap();
    let other_dir = tempfile::tempdir().unwrap();

    for d in [leader_dir.path(), other_dir.path()] {
        let db = Db::open_with(d, opts(1)).unwrap();
        db.insert_range(1, 0, 1_000).unwrap();
        db.checkpoint().unwrap();
    }
    assert_ne!(
        yesno_core::database_uuid(leader_dir.path()).unwrap(),
        yesno_core::database_uuid(other_dir.path()).unwrap(),
        "two databases must not share an identity"
    );

    // A follower that belongs to `other`, pointed at `leader`.
    let follower_dir = tempfile::tempdir().unwrap();
    seed_follower_manifest(other_dir.path(), follower_dir.path());

    let mut leader = serve(leader_dir.path(), 1).await;
    let mut follower = FollowerClient::new(follower_dir.path(), 0, []);

    let err = follower
        .bootstrap_shard(&mut leader.client, 0)
        .await
        .expect_err("a foreign leader must be refused");
    assert!(matches!(err, FollowerError::WrongLeader { .. }), "{err}");
    assert!(
        !follower_dir.path().join("shard-0000.yno").exists(),
        "the image was written before the identity was checked"
    );

    // Catching up is refused for the same reason and just as early.
    let err = follower
        .catch_up_shard(&mut leader.client, 0, 0, 0)
        .await
        .expect_err("shipping a log to the wrong database must be refused too");
    assert!(matches!(err, FollowerError::WrongLeader { .. }), "{err}");
    assert!(!follower_dir.path().join("shard-0000.wal").exists());
}

/// A follower with no identity yet adopts the first leader it reaches, and is
/// then bound to it.
///
/// This is the case that made folding the check into `bootstrap_shard` look
/// impossible — a genuinely empty directory is where a bootstrap legitimately
/// starts, and it has no MANIFEST to check against. Refusing it would have
/// forced the check back out into a `verify_leader` an operator must remember to
/// call, which is the same forgettable shape as the identity that was written
/// everywhere and read nowhere. Adopting resolves it without leaving the check
/// optional.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unseeded_follower_adopts_one_leader_and_then_refuses_another() {
    let a_dir = tempfile::tempdir().unwrap();
    let b_dir = tempfile::tempdir().unwrap();
    for d in [a_dir.path(), b_dir.path()] {
        let db = Db::open_with(d, opts(1)).unwrap();
        db.insert_range(1, 0, 1_000).unwrap();
        db.checkpoint().unwrap();
    }

    let mut a = serve(a_dir.path(), 1).await;
    let mut b = serve(b_dir.path(), 1).await;

    // No MANIFEST: nothing says which database this directory belongs to.
    let follower_dir = tempfile::tempdir().unwrap();
    let mut follower = FollowerClient::new(follower_dir.path(), 0, []);

    follower
        .bootstrap_shard(&mut a.client, 0)
        .await
        .expect("an unseeded follower must be able to start somewhere");

    let err = follower
        .bootstrap_shard(&mut b.client, 0)
        .await
        .expect_err("having adopted A, this client must not then follow B");
    assert!(matches!(err, FollowerError::WrongLeader { .. }), "{err}");
}

/// The last net, tested without the client — because it is the one that catches
/// a mistake made outside it.
///
/// The transport refuses a foreign leader now, but an operator copying shard
/// files by hand does not go through the transport. `ShardStore::open` compares
/// the superblock's `db_uuid` against the MANIFEST's, and that is what makes the
/// field worth writing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_shard_image_copied_in_by_hand_is_still_refused_at_open() {
    let leader_dir = tempfile::tempdir().unwrap();
    let other_dir = tempfile::tempdir().unwrap();
    for d in [leader_dir.path(), other_dir.path()] {
        let db = Db::open_with(d, opts(1)).unwrap();
        db.insert_range(1, 0, 1_000).unwrap();
        db.checkpoint().unwrap();
    }

    let follower_dir = tempfile::tempdir().unwrap();
    seed_follower_manifest(other_dir.path(), follower_dir.path());
    std::fs::copy(
        leader_dir.path().join("shard-0000.yno"),
        follower_dir.path().join("shard-0000.yno"),
    )
    .unwrap();

    match Db::open_with(follower_dir.path(), opts(1)).map(|_| ()) {
        Err(yesno_core::CodecError::DatabaseIdentityMismatch) => {}
        other => panic!("a foreign shard image opened as if it belonged here: {other:?}"),
    }
}

/// A follower that acks keeps its leader from reclaiming a needed generation.
///
/// # What the two halves are
///
/// `Ack` has carried the follower's applied LSN since M7 and the leader answered
/// with a lag it then **discarded**. The design names "applied-LSN acks -> lag +
/// retention floor"; only the lag existed, so a checkpoint reclaimed on its timer
/// regardless of who was still reading, and a follower one interval behind paid
/// a whole base-image copy to recover.
///
/// The control matters as much as the case. A leader served *without* a floor
/// must still reclaim — otherwise this test would pass against a checkpoint that had
/// simply stopped working — so the same sequence runs both ways and the two
/// outcomes are asserted against each other.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_acking_follower_holds_the_log_and_an_absent_one_does_not() {
    // Returns the log's byte length after a checkpoint taken while a follower
    // sits one round behind, with and without a retention floor in play.
    async fn round(retaining: bool) -> u64 {
        let leader_dir = tempfile::tempdir().unwrap();
        let follower_dir = tempfile::tempdir().unwrap();
        let db = Db::open_with(leader_dir.path(), opts(1)).unwrap();
        db.insert_range(1, 0, 5_000).unwrap();

        let mut leader = if retaining {
            serve_retaining(leader_dir.path(), 1, db.retention_floor()).await
        } else {
            serve(leader_dir.path(), 1).await
        };
        seed_follower_manifest(leader_dir.path(), follower_dir.path());
        let mut follower = FollowerClient::new(follower_dir.path(), 0, []);

        let caught = follower
            .catch_up_shard(&mut leader.client, 0, 0, 0)
            .await
            .unwrap();
        // The ack is what publishes the floor. Nothing else does.
        follower.ack(&mut leader.client, 0).await.unwrap();

        // The leader writes on, then checkpoints — the moment the log would be
        // reclaimed away from a follower that is now behind.
        for i in 0..40u64 {
            db.insert_range(2, i * 1_000, i * 1_000 + 100).unwrap();
        }
        assert!(
            wal_len(leader_dir.path(), 0) > 0,
            "nothing was written to reclaim"
        );
        db.checkpoint().unwrap();

        let after = wal_len(leader_dir.path(), 0);
        if retaining {
            assert!(
                std::fs::read_dir(leader_dir.path()).unwrap().any(|entry| {
                    entry
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .starts_with("shard-0000.wal.")
                }),
                "the checkpoint did not seal a WAL generation"
            );
            db.insert_range(3, 90_000, 90_100).unwrap();
            // Held, so the follower's cursor is still servable and the ordinary
            // resume crosses from the sealed generation into the active one.
            let moved = follower
                .catch_up_shard(&mut leader.client, 0, caught.next_lsn, 0)
                .await
                .expect("the floor was published and its generation was reclaimed anyway");
            assert!(moved.records > 0);
            let want = set_of(&db, 2);
            drop(leader);
            let replica = Db::open_with(follower_dir.path(), opts(1)).unwrap();
            assert_eq!(set_of(&replica, 2), want, "the replica diverged");
            assert_eq!(set_of(&replica, 3), set_of(&db, 3));
        }
        after
    }

    let held = round(true).await;
    let reclaimed = round(false).await;

    assert_eq!(
        reclaimed, 0,
        "without a floor a checkpoint must still reclaim; this test's control is broken"
    );
    assert!(
        held > 0,
        "the floor was published and the checkpoint reclaimed its generation anyway"
    );
}

/// The floor is a courtesy with a hard edge: past `max_wal_bytes` generations
/// are reclaimed whatever the followers say.
///
/// Without this a follower that keeps acking while falling further behind
/// holds the log open until the disk fills — the failure the design forecloses
/// for snapshot space with `AbortOldestReader`, "a reporting query should not be
/// able to halt ingestion", applied to the log.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_retention_floor_yields_to_the_hard_bound() {
    let leader_dir = tempfile::tempdir().unwrap();
    let follower_dir = tempfile::tempdir().unwrap();

    // A bound low enough that ordinary ingest crosses it, and no other trigger.
    let policy = yesno_core::checkpoint::CheckpointPolicy {
        dirty_bytes: usize::MAX,
        max_dirty_bytes: usize::MAX,
        wal_bytes: u64::MAX,
        interval_secs: u64::MAX,
        max_wal_bytes: 8 << 10,
    };
    let db = Db::open_with(
        leader_dir.path(),
        DbOptions {
            shards: 1,
            policy,
            ..Default::default()
        },
    )
    .unwrap();
    db.insert_range(1, 0, 100).unwrap();

    let mut leader = serve_retaining(leader_dir.path(), 1, db.retention_floor()).await;
    seed_follower_manifest(leader_dir.path(), follower_dir.path());
    let mut follower = FollowerClient::new(follower_dir.path(), 0, []);
    follower
        .catch_up_shard(&mut leader.client, 0, 0, 0)
        .await
        .unwrap();
    follower.ack(&mut leader.client, 0).await.unwrap();

    // Well past the bound, with the follower's floor published and stale.
    for i in 0..200u64 {
        db.insert_range(2, i * 1_000, i * 1_000 + 100).unwrap();
    }
    assert!(
        wal_len(leader_dir.path(), 0) > 8 << 10,
        "the log did not reach the bound, so nothing was tested"
    );
    db.checkpoint().unwrap();

    assert_eq!(
        wal_len(leader_dir.path(), 0),
        0,
        "the log grew past max_wal_bytes and a lagging follower still held it"
    );
}

/// A revived leader that has been replaced must be refused.
///
/// **This is the case identity cannot catch.** A promoted standby is a *copy*
/// of the same database, so it carries the same `db_uuid` as the node it
/// replaced — `check_leader`'s existing test passes for both. A follower pointed
/// back at the old leader would therefore accept it and apply records from a
/// timeline that has been abandoned, silently.
///
/// The term is what tells two leaderships of one database apart, and it lives in
/// the follower's own MANIFEST so that the fence survives a restart — which is
/// precisely the case that matters, since the zombie *is* a process coming back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_leader_that_has_been_superseded_is_refused() {
    let old_dir = tempfile::tempdir().unwrap();
    let follower_dir = tempfile::tempdir().unwrap();

    // ---- the original leadership, at term 0
    {
        let db = Db::open_with(old_dir.path(), opts(1)).unwrap();
        db.insert_range(1, 0, 500).unwrap();
        assert_eq!(db.term(), 0, "a fresh database starts at term 0");
    }

    let mut old = serve(old_dir.path(), 1).await;
    seed_follower_manifest(old_dir.path(), follower_dir.path());
    let mut f = FollowerClient::new(follower_dir.path(), 0, []);
    let off = f.bootstrap_shard(&mut old.client, 0).await.unwrap();
    f.catch_up_shard(&mut old.client, 0, off, 64 * 1024)
        .await
        .unwrap();
    assert_eq!(f.term(), 0);

    // ---- a failover happens: some node is promoted to term 1, and this
    // follower learns of it. In a real deployment that is another standby; here
    // the point is only that the follower has *seen* a higher term.
    yesno_core::promote_database(follower_dir.path(), 1).unwrap();
    assert_eq!(yesno_core::database_term(follower_dir.path()).unwrap(), 1);

    // ---- the old leader comes back, still at term 0, and **keeps writing**.
    //
    // This is what makes the test about the fence rather than about luck. If
    // the zombie had nothing new to offer, or offered it at a cursor that did
    // not line up, the follower would refuse it for an incidental reason — a
    // batch that does not continue the cursor — and the test would pass with the
    // fence deleted. Here the records are exactly what this follower would
    // legitimately accept, so the *only* thing that can refuse them is the term.
    {
        let db = Db::open_with(old_dir.path(), opts(1)).unwrap();
        db.insert_range(3, 0, 400).unwrap();
        assert_eq!(
            db.term(),
            0,
            "the revived leader is still the old leadership"
        );
    }
    assert_eq!(
        yesno_core::database_uuid(old_dir.path()).unwrap(),
        yesno_core::database_uuid(follower_dir.path()).unwrap(),
        "the two must be the same database, or this test is not about zombies"
    );

    let cursor = f.cursor(0).map_or(off, |c| c.next_lsn);
    let before = wal_len(follower_dir.path(), 0);

    let err = f
        .catch_up_shard(&mut old.client, 0, cursor, 64 * 1024)
        .await
        .expect_err("a superseded leader was followed");
    match err {
        FollowerError::StaleLeader { seen, offered } => {
            assert_eq!(seen, 1);
            assert_eq!(offered, 0);
        }
        other => panic!("expected StaleLeader, got {other}"),
    }

    // And nothing was written. A fence that refuses *after* applying is not a
    // fence — `check_leader` runs before a byte is written, which is the whole
    // reason it is where it is.
    assert_eq!(
        wal_len(follower_dir.path(), 0),
        before,
        "the follower wrote log shipped by a superseded leader"
    );
    // The zombie's key must not be here at all.
    let replica = Db::open_with(follower_dir.path(), opts(1)).unwrap();
    assert!(
        set_of(&replica, 3).is_empty(),
        "a record from the abandoned timeline reached the follower"
    );
    drop(replica);

    drop(old);
}

/// A term must not walk backwards, and the API must say so rather than doing it.
#[test]
fn a_promotion_must_raise_the_term() {
    let dir = tempfile::tempdir().unwrap();
    {
        let _db = Db::open_with(dir.path(), opts(1)).unwrap();
    }
    assert_eq!(yesno_core::database_term(dir.path()).unwrap(), 0);

    yesno_core::promote_database(dir.path(), 3).unwrap();
    assert_eq!(yesno_core::database_term(dir.path()).unwrap(), 3);

    // Equal is refused too. "Promote to the term I am already at" is a caller
    // that has misread something, and letting it succeed would make a retry look
    // like a failover.
    assert!(yesno_core::promote_database(dir.path(), 3).is_err());
    assert!(yesno_core::promote_database(dir.path(), 2).is_err());
    assert_eq!(yesno_core::database_term(dir.path()).unwrap(), 3);

    // And not while the database is open: the open `Db` holds the manifest it
    // read and would keep stamping the old term onto everything it writes.
    let db = Db::open_with(dir.path(), opts(1)).unwrap();
    assert_eq!(db.term(), 3, "the open database must see the promoted term");
    assert!(
        yesno_core::promote_database(dir.path(), 4).is_err(),
        "a promotion succeeded while the database was open"
    );
}
