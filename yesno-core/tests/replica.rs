//! A live read replica: open, serving, and applying shipped frames.
//!
//! **Entirely inside `yesno-core`, with no socket and no runtime**, and that
//! is deliberate rather than convenient. `WalPublisher` and `WalBatch` are core
//! types, so a leader's log can be sliced and handed to a replica in-process —
//! which means a defect in the apply path fails *here*, where it can be
//! diagnosed, rather than in a satellite crate where it looks like a networking
//! problem. `tests/crash_matrix.rs` is the same argument already carried to its
//! conclusion: it exercises partial multi-shard commits in core rather than
//! against a running server, in `a_multi_shard_batch_is_all_or_nothing_at_every_step`
//! and `truncating_one_shard_of_a_multi_shard_batch_blocks_the_whole_batch`.
//!
//! This sentence used to cite a `TODO.md` entry named
//! `crash-matrix-has-no-partial-multi-shard-commit` as though the gap were still
//! open. That entry does not exist and the gap is closed -- it was cited, in
//! support of a design choice, for eleven days after the coverage it asked for
//! had landed. Corrected 2026-09-14; see `dangling-backlog-citations`.

use std::collections::BTreeSet;
use std::path::Path;

use std::path::PathBuf;

use yesno_core::repl::{WalBatch, WalPublisher};
use yesno_core::{CodecError, Db, DbOptions};

/// No `tempfile`. `yesno-core`'s dev-dependencies are `roaring`, `num-bigint`,
/// `proptest` and `criterion`, and every other test here rolls its own temporary
/// directory rather than adding a fifth — the crate's whole thesis is a small
/// dependency tree, and the CI guard counts it. Same shape as `durability.rs`.
struct CleanDir(PathBuf);

impl CleanDir {
    fn new(tag: &str) -> CleanDir {
        let mut p = std::env::temp_dir();
        p.push(format!("yesno-replica-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        CleanDir(p)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for CleanDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn opts(shards: usize) -> DbOptions {
    DbOptions {
        shards,
        ..Default::default()
    }
}

fn set_of(db: &Db, key: u64) -> BTreeSet<u64> {
    db.snapshot().unwrap().load(key).unwrap().iter().collect()
}

/// Everything a leader's log holds for one shard, as the wire would carry it.
fn ship(leader_dir: &Path, shard: u32, from: u64) -> WalBatch {
    let path = leader_dir.join(format!("shard-{shard:04}.wal"));
    let bytes = std::fs::read(&path).unwrap_or_default();
    // `over_log` takes the base from the log's own first frame, which is the
    // only thing that knows it after a checkpoint has cut and renumbered.
    let pubr = WalPublisher::over_log(shard, &bytes, 0);
    pubr.batch_from(from, 1 << 30).unwrap()
}

/// Ship every shard's log into an open replica.
fn ship_all(leader_dir: &Path, replica: &Db, shards: u32) {
    for s in 0..shards {
        let from = replica.apply_cursor(s).unwrap();
        let batch = ship(leader_dir, s, from);
        replica.apply_wal_batch(&batch).unwrap();
    }
}

/// The whole point: a replica that is **open** takes shipped frames and serves
/// them, without ever closing.
#[test]
fn an_open_replica_applies_shipped_frames_and_serves_them() {
    let leader_dir = CleanDir::new("leaderdir1");
    let replica_dir = CleanDir::new("replicadir2");

    let leader = Db::open_with(leader_dir.path(), opts(2)).unwrap();
    leader.insert_range(1, 0, 4_000).unwrap();
    leader.checkpoint().unwrap();

    // Seed the replica from the leader's checkpointed images, exactly as a
    // bootstrap does.
    for s in 0..2 {
        std::fs::copy(
            leader_dir.path().join(format!("shard-{s:04}.yno")),
            replica_dir.path().join(format!("shard-{s:04}.yno")),
        )
        .unwrap();
    }
    std::fs::copy(
        leader_dir.path().join("MANIFEST"),
        replica_dir.path().join("MANIFEST"),
    )
    .unwrap();

    let replica = Db::open_replica(replica_dir.path(), opts(2)).unwrap();
    assert!(replica.is_replica());
    assert_eq!(
        set_of(&replica, 1).len(),
        4_001,
        "the base image did not come through"
    );

    // ---- now ship, while the replica stays open
    leader.insert_range(2, 10_000, 12_000).unwrap();
    leader.insert(3, 7).unwrap();
    ship_all(leader_dir.path(), &replica, 2);

    assert_eq!(set_of(&replica, 2), set_of(&leader, 2), "key 2 diverged");
    assert_eq!(set_of(&replica, 3), set_of(&leader, 3), "key 3 diverged");

    // ---- and again, into the same open handle
    leader.insert_range(4, 0, 500).unwrap();
    ship_all(leader_dir.path(), &replica, 2);
    assert_eq!(set_of(&replica, 4), set_of(&leader, 4));

    // A snapshot taken *before* the second batch must not see it — the
    // replica's watermark is what makes applied bytes visible, not the write.
    let before = replica.snapshot().unwrap();
    leader.insert_range(5, 0, 100).unwrap();
    ship_all(leader_dir.path(), &replica, 2);
    assert_eq!(
        before.cardinality(5).unwrap(),
        0,
        "an older snapshot saw records applied after it was taken"
    );
    assert_eq!(set_of(&replica, 5), set_of(&leader, 5));
}

/// Every local mutation is refused, through the one choke point.
#[test]
fn a_replica_refuses_every_local_write() {
    let dir = CleanDir::new("dir3");
    {
        let db = Db::open_with(dir.path(), opts(1)).unwrap();
        db.insert_range(1, 0, 100).unwrap();
        db.checkpoint().unwrap();
    }
    let r = Db::open_replica(dir.path(), opts(1)).unwrap();

    // Reads work exactly as on a leader.
    assert_eq!(r.snapshot().unwrap().cardinality(1).unwrap(), 101);

    // Every mutator, because the guard's whole claim is that they all funnel
    // through `WriteBatch::commit`. Testing one would not establish that.
    let refusals: Vec<yesno_core::Result<()>> = vec![
        r.insert(9, 1).map(|_| ()),
        r.remove(1, 0).map(|_| ()),
        r.insert_range(9, 0, 10).map(|_| ()),
        r.remove_range(1, 0, 10).map(|_| ()),
        r.insert_many(9, &[1, 2, 3]).map(|_| ()),
        {
            let mut b = r.batch();
            b.insert(9, 1);
            b.commit().map(|_| ())
        },
        {
            let mut b = r.batch();
            b.delete_key(1);
            b.commit().map(|_| ())
        },
    ];
    for (i, e) in refusals.into_iter().enumerate() {
        match e {
            Err(CodecError::ReadOnlyReplica) => {}
            other => panic!("mutator {i} was not refused: {other:?}"),
        }
    }

    // And nothing changed.
    assert_eq!(r.snapshot().unwrap().cardinality(1).unwrap(), 101);
}

/// A replica writes **nothing of its own** into the log it is being shipped.
///
/// This is the one that would have been a silent disaster. A normal open
/// appends an `EpochFence`; on a replica that is a locally generated frame in an
/// LSN space belonging to the leader, so the *next* shipped frame no longer
/// lands where its own header says. `Record::decode` refuses it, the scan stops,
/// and the log silently ends there — `Ok`, checksums fine, every later record
/// gone.
#[test]
fn opening_a_replica_leaves_its_log_byte_identical() {
    let leader_dir = CleanDir::new("leaderdir4");
    let replica_dir = CleanDir::new("replicadir5");

    let leader = Db::open_with(leader_dir.path(), opts(1)).unwrap();
    leader.insert_range(1, 0, 200).unwrap();
    leader.checkpoint().unwrap();
    std::fs::copy(
        leader_dir.path().join("shard-0000.yno"),
        replica_dir.path().join("shard-0000.yno"),
    )
    .unwrap();
    std::fs::copy(
        leader_dir.path().join("MANIFEST"),
        replica_dir.path().join("MANIFEST"),
    )
    .unwrap();

    let wal = replica_dir.path().join("shard-0000.wal");
    let before = std::fs::read(&wal).unwrap_or_default();

    let replica = Db::open_replica(replica_dir.path(), opts(1)).unwrap();
    let after = std::fs::read(&wal).unwrap_or_default();
    assert_eq!(
        before,
        after,
        "opening a replica wrote {} bytes into a log whose LSN space belongs to the leader",
        after.len() - before.len()
    );

    // And the offset identity that buys: shipped frames land and decode.
    leader.insert_range(2, 0, 300).unwrap();
    ship_all(leader_dir.path(), &replica, 1);
    assert_eq!(set_of(&replica, 2), set_of(&leader, 2));

    // A **leader** open on the same directory does write one, which is what
    // makes the suppression above a difference rather than a coincidence.
    drop(replica);
    let n_before = std::fs::read(&wal).unwrap().len();
    let promoted = Db::open_with(replica_dir.path(), opts(1)).unwrap();
    assert!(
        std::fs::read(&wal).unwrap().len() > n_before,
        "a leader open wrote no epoch fence, so this test proves nothing"
    );
    drop(promoted);
}

/// A multi-shard commit stays atomic across the wire.
///
/// The watermark, **and** the memtable. `yesno-core`'s own unit tests cover
/// the watermark rule; only a `Db`-level test can see whether a half-arrived
/// commit has become *readable*, which is the failure a reader would notice.
#[test]
fn a_multi_shard_commit_is_invisible_until_every_shard_arrives() {
    let leader_dir = CleanDir::new("leaderdir6");
    let replica_dir = CleanDir::new("replicadir7");

    let leader = Db::open_with(leader_dir.path(), opts(4)).unwrap();
    // One key per shard, so the batch below genuinely spans them.
    let mut keys = Vec::new();
    for k in 0u64..2_000 {
        if keys.len() == 4 {
            break;
        }
        let s = leader.shard_of(k);
        if !keys.iter().any(|&(_, sh)| sh == s) {
            keys.push((k, s));
        }
    }
    assert_eq!(keys.len(), 4, "could not find a key on each shard");
    leader.checkpoint().unwrap();

    for s in 0..4 {
        std::fs::copy(
            leader_dir.path().join(format!("shard-{s:04}.yno")),
            replica_dir.path().join(format!("shard-{s:04}.yno")),
        )
        .unwrap();
    }
    std::fs::copy(
        leader_dir.path().join("MANIFEST"),
        replica_dir.path().join("MANIFEST"),
    )
    .unwrap();
    let replica = Db::open_replica(replica_dir.path(), opts(4)).unwrap();

    // One batch spanning every shard.
    let mut wb = leader.batch();
    for (k, _) in &keys {
        wb.insert(*k, 42);
    }
    wb.commit().unwrap();

    // Ship all but the last shard.
    let visible_before = replica.snapshot().unwrap().version();
    for s in 0..3u32 {
        let from = replica.apply_cursor(s).unwrap();
        replica
            .apply_wal_batch(&ship(leader_dir.path(), s, from))
            .unwrap();
    }
    assert_eq!(
        replica.snapshot().unwrap().version(),
        visible_before,
        "the watermark advanced with a participant still missing"
    );
    for (k, _) in &keys {
        assert!(
            !replica.snapshot().unwrap().contains(*k, 42).unwrap(),
            "a half-arrived multi-shard commit was readable on key {k}"
        );
    }

    // The last participant resolves it.
    let from = replica.apply_cursor(3).unwrap();
    replica
        .apply_wal_batch(&ship(leader_dir.path(), 3, from))
        .unwrap();
    for (k, _) in &keys {
        assert!(
            replica.snapshot().unwrap().contains(*k, 42).unwrap(),
            "key {k} did not become visible once every shard had arrived"
        );
    }
}

/// A batch at the wrong offset is refused, not written.
#[test]
fn a_batch_that_does_not_continue_the_log_is_refused() {
    let leader_dir = CleanDir::new("leaderdir8");
    let replica_dir = CleanDir::new("replicadir9");
    let leader = Db::open_with(leader_dir.path(), opts(1)).unwrap();
    leader.insert_range(1, 0, 100).unwrap();
    leader.checkpoint().unwrap();
    std::fs::copy(
        leader_dir.path().join("shard-0000.yno"),
        replica_dir.path().join("shard-0000.yno"),
    )
    .unwrap();
    std::fs::copy(
        leader_dir.path().join("MANIFEST"),
        replica_dir.path().join("MANIFEST"),
    )
    .unwrap();
    let replica = Db::open_replica(replica_dir.path(), opts(1)).unwrap();

    leader.insert_range(2, 0, 100).unwrap();
    let good = ship(leader_dir.path(), 0, replica.apply_cursor(0).unwrap());

    // Shifted by one record's worth. Writing this would put every following
    // frame at an offset its header contradicts, and the scan would stop there
    // silently — which is precisely what `append_frames_at` refuses.
    let bad = WalBatch::new(good.shard, good.first_lsn + 8, good.records.clone());
    let before = std::fs::read(replica_dir.path().join("shard-0000.wal")).unwrap_or_default();
    assert!(
        replica.apply_wal_batch(&bad).is_err(),
        "a batch at the wrong offset was accepted"
    );
    assert_eq!(
        std::fs::read(replica_dir.path().join("shard-0000.wal")).unwrap_or_default(),
        before,
        "a refused batch still wrote to the log"
    );

    // The correctly placed one still works afterwards.
    replica.apply_wal_batch(&good).unwrap();
    assert_eq!(set_of(&replica, 2), set_of(&leader, 2));
}

/// `apply_wal_batch` is for a replica, and says so on a leader.
#[test]
fn a_leader_refuses_to_apply_foreign_frames() {
    let dir = CleanDir::new("dir10");
    let db = Db::open_with(dir.path(), opts(1)).unwrap();
    db.insert(1, 1).unwrap();
    let batch = ship(dir.path(), 0, 0);
    // Refused rather than tolerated: a leader adopting foreign versions would
    // collide with the ones its own oracle is handing out.
    assert!(db.apply_wal_batch(&batch).is_err());
}

/// **A follower answers a stale read successfully and says nothing about it.**
///
/// This is the guarantee a follower actually provides, stated as assertions: a
/// *consistent, atomic, monotonically advancing snapshot of a committed prefix*
/// of the leader's history — and nothing about recency. Replication is
/// asynchronous, so an acknowledged leader write may be entirely absent, the lag
/// is unbounded, and the read that misses it is not an error.
///
/// The danger is not the staleness, which is inherent and documented. It is
/// that a stale read is **indistinguishable from a current one** at the call
/// site: `contains` returns `Ok( false )`, not "I am behind". A client can only
/// detect it by comparing `visible()` against the leader's, which requires
/// knowing the leader's version through some other channel.
#[test]
fn a_follower_serves_a_stale_but_consistent_prefix_and_does_not_say_so() {
    let leader_dir = CleanDir::new("stale-leader");
    let replica_dir = CleanDir::new("stale-replica");

    let leader = Db::open_with(leader_dir.path(), opts(1)).unwrap();
    leader.insert_many(7, &[1, 2, 3]).unwrap();
    leader.checkpoint().unwrap();

    std::fs::copy(
        leader_dir.path().join("shard-0000.yno"),
        replica_dir.path().join("shard-0000.yno"),
    )
    .unwrap();
    std::fs::copy(
        leader_dir.path().join("MANIFEST"),
        replica_dir.path().join("MANIFEST"),
    )
    .unwrap();

    let replica = Db::open_replica(replica_dir.path(), opts(1)).unwrap();
    // No shipping yet: the checkpoint cut the leader's log, so the bootstrap
    // images *are* the follower's starting state. Shipping from a cursor the
    // cut log no longer contains is refused, which is itself the right
    // behaviour and is covered elsewhere.
    let caught_up = BTreeSet::from([1, 2, 3]);
    assert_eq!(
        set_of(&replica, 7),
        caught_up,
        "the follower starts current"
    );

    // ---- 1. An acknowledged leader write is simply absent from the follower.
    assert!(
        leader.insert(7, 99).unwrap(),
        "the leader acknowledges the write"
    );
    assert!(
        leader.snapshot().unwrap().contains(7, 99).unwrap(),
        "and serves it immediately itself"
    );
    assert_eq!(
        set_of(&replica, 7),
        caught_up,
        "the follower does not have an acknowledged write"
    );
    assert!(
        replica.visible() < leader.visible(),
        "and its watermark is behind: {} < {}",
        replica.visible(),
        leader.visible()
    );

    // ---- 2. The stale read *succeeds*. That is the hazard.
    assert!(
        !replica.snapshot().unwrap().contains(7, 99).unwrap(),
        "a stale follower answers `false`, not an error — a caller cannot tell \
         this from an ordinal that was never written"
    );

    // ---- 3. The lag is unbounded and the leader neither blocks nor objects.
    let mut marks = vec![replica.visible()];
    for i in 0..500u64 {
        leader.insert(7, 1_000 + i).unwrap();
        if i % 100 == 0 {
            marks.push(replica.visible());
        }
    }
    assert_eq!(
        set_of(&replica, 7),
        caught_up,
        "500 further acknowledged commits and the follower still shows the old prefix"
    );
    assert!(
        leader.visible() - replica.visible() >= 500,
        "nothing bounds how far behind a follower may fall"
    );

    // ---- 4. What it *does* guarantee: the prefix is whole, never torn.
    //
    // The follower shows exactly the state at its own watermark — all three of
    // the original ordinals, none of the 501 later ones. A partial view would
    // show some of the later writes, which is the failure this rules out.
    assert_eq!(
        set_of(&replica, 7),
        caught_up,
        "a follower shows a complete earlier state, not a mixture"
    );

    // ---- 5. Catch up, including a second ship that carries nothing.
    ship_all(leader_dir.path(), &replica, 1);
    marks.push(replica.visible());
    ship_all(leader_dir.path(), &replica, 1);
    marks.push(replica.visible());

    // **This loop is a weak check and is labelled as one.** Follower
    // monotonicity is *structural*, not something this test establishes:
    // `apply_wal_batch` computes `resolved_through( oracle.visible() )`, which
    // starts at the current watermark and only walks forward, so the value
    // handed to `adopt_visible` is already monotone and its `fetch_max` is
    // belt-and-braces. Verified by sabotage: replacing that `fetch_max` with a
    // plain `store` leaves this test green, and storing `v - 1` is caught by
    // the watermark equality below rather than here.
    //
    // Do not read a pass here as evidence that monotonicity is enforced.
    for pair in marks.windows(2) {
        assert!(
            pair[1] >= pair[0],
            "the follower watermark regressed across a ship: {} then {}",
            pair[0],
            pair[1]
        );
    }

    // ---- 6. Once frames arrive it has everything, including the first write.
    assert!(
        replica.snapshot().unwrap().contains(7, 99).unwrap(),
        "after catching up the acknowledged write is present"
    );
    assert_eq!(
        replica.visible(),
        leader.visible(),
        "and the watermarks agree once the log is drained"
    );
}
