//! The channel's protocol logic, driven frame by frame with no socket.
//!
//! Every case here is a function call because [`Session::handle`] takes a frame and
//! answers one. The socket loop is thin enough that what is worth testing is all in
//! this layer: unknown handles, the block protocol, a snapshot closed under live
//! lanes, and the arena arithmetic the two sides have to agree on.

use std::sync::{Arc, RwLock};

use yesno_core::{Db, DbOptions};
use yesno_plugin::abi::{Role, Status};
use yesno_plugin::channel::{Arena, Limits, Session};
use yesno_plugin::ipc::{lane_offset, Frame, LaneKind, LANE_BYTES};
use yesno_plugin::Host;

struct Clean(std::path::PathBuf);
impl Drop for Clean {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Small limits, so a test's arena is a few hundred kilobytes rather than megabytes.
fn limits() -> Limits {
    Limits {
        max_handles: 2,
        max_lanes: 4,
        max_blocks: 1,
    }
}

fn setup(tag: &str) -> (Clean, Host, Session) {
    let mut dir = std::env::temp_dir();
    dir.push(format!("yesno-chan-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let clean = Clean(dir.clone());

    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 1,
            ..Default::default()
        },
    )
    .unwrap();
    let mut b = db.batch();
    // key 10: chunks 0, 1, 5 as arrays from 7. key 20: chunk 1 scattered past
    // RUN_MAX_INTERVALS so it is a bitmap. key 30: chunk 3 as one run.
    for c in [0u64, 1, 5] {
        for i in 0..3u64 {
            b.insert(10, c * 65536 + 7 + i);
        }
    }
    for i in 0..5000u64 {
        b.insert(20, 65536 + i * 3);
    }
    b.insert_range(30, 3 * 65536, 3 * 65536 + 4096);
    b.commit().unwrap();
    db.checkpoint().unwrap();

    let slot = Arc::new(RwLock::new(Some(Arc::new(db))));
    let host = Host::new(slot, 1, Role::Leader);
    let l = limits();
    let arena = Arena::new(l.arena_bytes()).expect("memfd arena");
    let session = Session::new(host.clone(), arena, l);
    (clean, host, session)
}

fn greet(s: &mut Session) {
    assert_eq!(
        s.handle(Frame::ClientHello {
            protocol: 1,
            name: "t".into()
        }),
        Frame::Done
    );
}

fn fault_status(f: &Frame) -> u32 {
    match f {
        Frame::Fault { status, .. } => *status,
        other => panic!("expected a fault, got {other:?}"),
    }
}

/// The greeting states the server's own limits, so a peer learns them rather than
/// discovering them by refusal.
#[test]
fn the_greeting_declares_the_arena_and_the_limits() {
    let (_c, _h, s) = setup("hello");
    match s.hello() {
        Frame::ServerHello {
            protocol,
            generation,
            role,
            shards,
            arena_bytes,
            max_lanes,
            max_handles,
            max_blocks,
        } => {
            assert_eq!(protocol, 1);
            assert_eq!(generation, 1);
            assert_eq!(role, yesno_plugin::ipc::Role::Leader);
            assert_eq!(shards, 1);
            assert_eq!(max_lanes, 4);
            assert_eq!(max_handles, 2);
            assert_eq!(max_blocks, 1, "the batch cap is advertised, not guessed");
            assert_eq!(arena_bytes as usize, 2 * 4 * LANE_BYTES);
        }
        other => panic!("expected ServerHello, got {other:?}"),
    }
}

/// Nothing is served before the greeting, so a peer that skipped it is told rather
/// than served a version it never agreed.
#[test]
fn a_request_before_the_greeting_is_refused() {
    let (_c, _h, mut s) = setup("ungreeted");
    let f = s.handle(Frame::SnapshotOpen);
    assert_eq!(fault_status(&f), Status::InvalidArgument as u32);
    greet(&mut s);
    assert!(matches!(
        s.handle(Frame::SnapshotOpen),
        Frame::SnapshotOpened { .. }
    ));
}

/// A whole scan: every block, every lane, absence in place, and the arena holding
/// the bytes each descriptor claims.
#[test]
fn a_scan_reports_every_block_and_writes_the_payloads_into_the_arena() {
    let (_c, _h, mut s) = setup("scan");
    greet(&mut s);
    let snapshot = match s.handle(Frame::SnapshotOpen) {
        Frame::SnapshotOpened { snapshot, .. } => snapshot,
        other => panic!("{other:?}"),
    };
    let (lanes, arena_off) = match s.handle(Frame::LanesAcquire {
        snapshot,
        keys: vec![10, 20, 30],
    }) {
        Frame::LanesAcquired { lanes, arena_off } => (lanes, arena_off),
        other => panic!("{other:?}"),
    };
    assert_eq!(arena_off, 0, "the first handle takes the first slot");

    let mut seen = Vec::new();
    loop {
        match s.handle(Frame::BlockAdvance { lanes }) {
            Frame::BlockDone => break,
            Frame::Block { prefix, lanes: ls } => {
                assert_eq!(ls.len(), 3);
                seen.push((
                    prefix,
                    ls.iter().map(|l| (l.kind, l.count)).collect::<Vec<_>>(),
                ));
                assert_eq!(s.handle(Frame::BlockRelease { lanes }), Frame::Done);
            }
            other => panic!("{other:?}"),
        }
    }
    // Map the arena exactly as a peer would -- through the descriptor, read-only --
    // and check the bytes, not just the descriptors. Without this the test's name is
    // a claim it does not check.
    let peer_view = {
        let f = std::fs::File::from(s.arena_fd().unwrap().try_clone_to_owned().unwrap());
        // SAFETY: the descriptor is the server's memfd, sized to the arena.
        unsafe { memmap2::Mmap::map(&f) }.unwrap()
    };

    let absent = (LaneKind::Absent, 0u32);
    assert_eq!(
        seen,
        vec![
            (0, vec![(LaneKind::Array, 3), absent, absent]),
            (
                1,
                vec![(LaneKind::Array, 3), (LaneKind::Bitmap, 1024), absent]
            ),
            (3, vec![absent, absent, (LaneKind::Run, 1)]),
            (5, vec![(LaneKind::Array, 3), absent, absent]),
        ]
    );

    // Lane 0 at the last block held key 10's three ordinals, whose low sixteen bits
    // are 7, 8 and 9. Read them from where the peer's arithmetic says they are.
    let off = lane_offset(arena_off, 0) as usize;
    let vals: Vec<u16> = peer_view[off..off + 6]
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    assert_eq!(
        vals,
        vec![7, 8, 9],
        "the arena must hold the payload the descriptor described, at the offset the \
         index implies"
    );

    assert_eq!(s.handle(Frame::LanesRelease { lanes }), Frame::Done);
    assert_eq!(s.handle(Frame::SnapshotClose { snapshot }), Frame::Done);
    assert_eq!(s.outstanding(), (0, 0));
}

/// Advancing with a block open is refused, because its arena slice is still lent
/// and overwriting it would change bytes the peer is reading.
#[test]
fn advancing_an_open_block_is_refused() {
    let (_c, _h, mut s) = setup("proto");
    greet(&mut s);
    let snapshot = match s.handle(Frame::SnapshotOpen) {
        Frame::SnapshotOpened { snapshot, .. } => snapshot,
        other => panic!("{other:?}"),
    };
    let lanes = match s.handle(Frame::LanesAcquire {
        snapshot,
        keys: vec![10],
    }) {
        Frame::LanesAcquired { lanes, .. } => lanes,
        other => panic!("{other:?}"),
    };
    assert!(matches!(
        s.handle(Frame::BlockAdvance { lanes }),
        Frame::Block { .. }
    ));
    let f = s.handle(Frame::BlockAdvance { lanes });
    assert_eq!(fault_status(&f), Status::BlockState as u32);
    assert_eq!(s.handle(Frame::BlockRelease { lanes }), Frame::Done);
    assert!(matches!(
        s.handle(Frame::BlockAdvance { lanes }),
        Frame::Block { .. }
    ));
}

/// Closing a snapshot whose lanes are still open is refused.
///
/// The in-process ABI allows it, because there a derived handle holds its own
/// `Snapshot` clone. Here the **session** owns the snapshot, so allowing the close
/// would leave a handle reading a version nothing pins.
#[test]
fn a_snapshot_with_live_lanes_cannot_be_closed() {
    let (_c, _h, mut s) = setup("liveclose");
    greet(&mut s);
    let snapshot = match s.handle(Frame::SnapshotOpen) {
        Frame::SnapshotOpened { snapshot, .. } => snapshot,
        other => panic!("{other:?}"),
    };
    let lanes = match s.handle(Frame::LanesAcquire {
        snapshot,
        keys: vec![10],
    }) {
        Frame::LanesAcquired { lanes, .. } => lanes,
        other => panic!("{other:?}"),
    };
    let f = s.handle(Frame::SnapshotClose { snapshot });
    assert_eq!(fault_status(&f), Status::InvalidArgument as u32);
    assert_eq!(s.handle(Frame::LanesRelease { lanes }), Frame::Done);
    assert_eq!(s.handle(Frame::SnapshotClose { snapshot }), Frame::Done);
}

/// Arena slots are reused, so a long-lived session does not exhaust them.
#[test]
fn releasing_a_handle_returns_its_arena_slot() {
    let (_c, _h, mut s) = setup("slots");
    greet(&mut s);
    let snapshot = match s.handle(Frame::SnapshotOpen) {
        Frame::SnapshotOpened { snapshot, .. } => snapshot,
        other => panic!("{other:?}"),
    };
    let mut offs = Vec::new();
    let mut ids = Vec::new();
    for _ in 0..2 {
        match s.handle(Frame::LanesAcquire {
            snapshot,
            keys: vec![10],
        }) {
            Frame::LanesAcquired { lanes, arena_off } => {
                ids.push(lanes);
                offs.push(arena_off);
            }
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(offs, vec![0, (4 * LANE_BYTES) as u64], "distinct slots");

    // A third exceeds max_handles.
    let f = s.handle(Frame::LanesAcquire {
        snapshot,
        keys: vec![10],
    });
    assert_eq!(fault_status(&f), Status::InvalidArgument as u32);

    // Releasing one frees its slot for the next acquire.
    assert_eq!(s.handle(Frame::LanesRelease { lanes: ids[0] }), Frame::Done);
    match s.handle(Frame::LanesAcquire {
        snapshot,
        keys: vec![10],
    }) {
        Frame::LanesAcquired { arena_off, .. } => assert_eq!(arena_off, 0, "slot 0 came back"),
        other => panic!("{other:?}"),
    }
}

/// More lanes than the server allows is a refusal, not a larger arena.
#[test]
fn more_lanes_than_the_limit_is_refused() {
    let (_c, _h, mut s) = setup("wide");
    greet(&mut s);
    let snapshot = match s.handle(Frame::SnapshotOpen) {
        Frame::SnapshotOpened { snapshot, .. } => snapshot,
        other => panic!("{other:?}"),
    };
    let f = s.handle(Frame::LanesAcquire {
        snapshot,
        keys: vec![1, 2, 3, 4, 5],
    });
    assert_eq!(fault_status(&f), Status::InvalidArgument as u32);
}

/// Unknown ids are refused rather than addressing something.
#[test]
fn unknown_handles_are_refused() {
    let (_c, _h, mut s) = setup("unknown");
    greet(&mut s);
    for f in [
        Frame::SnapshotClose { snapshot: 999 },
        Frame::LanesAcquire {
            snapshot: 999,
            keys: vec![1],
        },
        Frame::LanesRelease { lanes: 999 },
        Frame::BlockAdvance { lanes: 999 },
        Frame::BlockRelease { lanes: 999 },
    ] {
        let kind = f.kind();
        let r = s.handle(f);
        assert_eq!(
            fault_status(&r),
            Status::InvalidArgument as u32,
            "{kind:?} with an unknown id must be refused"
        );
    }
    // Zero is never a valid id, so an uninitialised peer field fails immediately.
    let r = s.handle(Frame::BlockAdvance { lanes: 0 });
    assert_eq!(fault_status(&r), Status::InvalidArgument as u32);
}

/// With no database, a peer is told to retry rather than served an error it cannot
/// interpret.
#[test]
fn an_absent_database_reports_unavailable() {
    let mut dir = std::env::temp_dir();
    dir.push(format!("yesno-chan-unavail-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let _c = Clean(dir.clone());
    let slot: Arc<RwLock<Option<Arc<Db>>>> = Arc::new(RwLock::new(None));
    let host = Host::new(slot, 1, Role::Follower);
    let l = limits();
    let arena = Arena::new(l.arena_bytes()).unwrap();
    let mut s = Session::new(host, arena, l);
    greet(&mut s);
    let f = s.handle(Frame::SnapshotOpen);
    assert_eq!(fault_status(&f), Status::Unavailable as u32);
}

/// A response or notification frame arriving where a request belongs is refused by
/// its kind, which is what the three discriminant ranges are for.
#[test]
fn a_frame_travelling_the_wrong_way_is_refused_by_its_kind() {
    let (_c, _h, mut s) = setup("wrongway");
    greet(&mut s);
    for f in [Frame::BlockDone, Frame::Unavailable, Frame::Done] {
        let kind = f.kind();
        let r = s.handle(f);
        assert_eq!(
            fault_status(&r),
            Status::InvalidArgument as u32,
            "{kind:?} is not a request and must be refused as one"
        );
    }
}

/// Dropping the session drops its snapshots, which is the reclamation story: the
/// socket closing is what drops the session, so a dead peer needs no cooperation.
#[test]
fn dropping_a_session_releases_everything_it_held() {
    let (_c, host, mut s) = setup("drop");
    greet(&mut s);
    let snapshot = match s.handle(Frame::SnapshotOpen) {
        Frame::SnapshotOpened { snapshot, .. } => snapshot,
        other => panic!("{other:?}"),
    };
    let _ = s.handle(Frame::LanesAcquire {
        snapshot,
        keys: vec![10, 20],
    });
    assert_eq!(s.outstanding(), (1, 1));
    let db = host.db().unwrap();
    assert_eq!(db.live_readers(), 1, "the session's snapshot is registered");

    drop(s);
    assert_eq!(
        db.live_readers(),
        0,
        "a dropped session releases its readers with no cooperation from any peer"
    );
}

/// The arena offset a peer computes must be the one the server wrote to. Asserted
/// through the shared helper, since the whole point of the layout is that neither
/// side carries the arithmetic separately.
#[test]
fn both_sides_agree_where_a_lane_lives() {
    assert_eq!(lane_offset(0, 0), 0);
    assert_eq!(
        lane_offset(4 * LANE_BYTES as u64, 3),
        (4 + 3) * LANE_BYTES as u64
    );
}

/// A batched scan returns the same blocks as the unbatched one, and says it is
/// finished by returning fewer than asked for.
#[test]
fn a_batched_scan_sees_exactly_what_the_unbatched_one_sees() {
    // One scan the old way, for the oracle.
    let (_c, _h, mut s) = setup("batch-ref");
    greet(&mut s);
    let snapshot = match s.handle(Frame::SnapshotOpen) {
        Frame::SnapshotOpened { snapshot, .. } => snapshot,
        other => panic!("{other:?}"),
    };
    let lanes = match s.handle(Frame::LanesAcquire {
        snapshot,
        keys: vec![10, 20, 30],
    }) {
        Frame::LanesAcquired { lanes, .. } => lanes,
        other => panic!("{other:?}"),
    };
    let mut one_at_a_time = Vec::new();
    loop {
        match s.handle(Frame::BlockAdvance { lanes }) {
            Frame::BlockDone => break,
            Frame::Block { prefix, lanes: ls } => {
                one_at_a_time.push((prefix, ls));
                s.handle(Frame::BlockRelease { lanes });
            }
            other => panic!("{other:?}"),
        }
    }

    // And the same scan in batches of three.
    let l = Limits {
        max_handles: 1,
        max_lanes: 4,
        max_blocks: 3,
    };
    let (_c2, _h2, _s2) = setup("batch-unused");
    let (_c3, host, _drop) = setup("batch-many");
    let arena = Arena::new(l.arena_bytes()).unwrap();
    let mut b = Session::new(host, arena, l);
    greet(&mut b);
    let snapshot = match b.handle(Frame::SnapshotOpen) {
        Frame::SnapshotOpened { snapshot, .. } => snapshot,
        other => panic!("{other:?}"),
    };
    let lanes = match b.handle(Frame::LanesAcquire {
        snapshot,
        keys: vec![10, 20, 30],
    }) {
        Frame::LanesAcquired { lanes, .. } => lanes,
        other => panic!("{other:?}"),
    };
    let mut batched = Vec::new();
    let mut requests = 0;
    loop {
        requests += 1;
        match b.handle(Frame::BlockAdvanceMany {
            lanes,
            max_blocks: 3,
        }) {
            Frame::Blocks { blocks } => {
                let short = blocks.len() < 3;
                for blk in blocks {
                    batched.push((blk.prefix, blk.lanes));
                }
                if short {
                    break;
                }
            }
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(
        batched, one_at_a_time,
        "batching must not change what a scan sees"
    );
    assert_eq!(
        requests, 2,
        "four blocks in batches of three is two requests, against eight the other way"
    );
}

/// A batch never returns more than the server's own limit, whatever a peer asks.
#[test]
fn a_batch_is_capped_by_the_server_not_the_request() {
    let l = Limits {
        max_handles: 1,
        max_lanes: 4,
        max_blocks: 2,
    };
    let (_c, host, _drop) = setup("batch-cap");
    let arena = Arena::new(l.arena_bytes()).unwrap();
    let mut s = Session::new(host, arena, l);
    greet(&mut s);
    let snapshot = match s.handle(Frame::SnapshotOpen) {
        Frame::SnapshotOpened { snapshot, .. } => snapshot,
        other => panic!("{other:?}"),
    };
    let lanes = match s.handle(Frame::LanesAcquire {
        snapshot,
        keys: vec![10],
    }) {
        Frame::LanesAcquired { lanes, .. } => lanes,
        other => panic!("{other:?}"),
    };
    match s.handle(Frame::BlockAdvanceMany {
        lanes,
        max_blocks: 1000,
    }) {
        Frame::Blocks { blocks } => assert_eq!(
            blocks.len(),
            2,
            "the server's own limit bounds the arena it reserved, so it wins"
        ),
        other => panic!("{other:?}"),
    }
}

/// The arena is sealed against resize, so neither side can produce a `SIGBUS`.
///
/// A mapping whose file is truncated underneath it faults on the next touch, and
/// that is not catchable as a `Result` -- the same hazard invariant I6 states for
/// the shard files, reached here by a different route. `F_SEAL_SHRINK` makes the
/// size immutable for **both** ends, so this asserts the seal is actually applied
/// rather than trusting that nothing will try.
#[test]
#[cfg(target_os = "linux")]
fn the_arena_cannot_be_resized_by_either_side() {
    let l = limits();
    let arena = Arena::new(l.arena_bytes()).unwrap();
    let f = std::fs::File::from(arena.as_fd().try_clone_to_owned().unwrap());

    assert!(
        f.set_len(0).is_err(),
        "truncating the arena must be refused; a live mapping over it would SIGBUS"
    );
    assert!(
        f.set_len(l.arena_bytes() as u64 * 2).is_err(),
        "growing it must be refused too, or a peer's mapping stops covering it"
    );

    // And the seals are themselves sealed, so nothing can add F_SEAL_WRITE later
    // and break the host's own writes.
    let rc = unsafe {
        libc::fcntl(
            std::os::fd::AsRawFd::as_raw_fd(&f),
            libc::F_ADD_SEALS,
            libc::F_SEAL_WRITE,
        )
    };
    assert_eq!(rc, -1, "adding a seal after F_SEAL_SEAL must fail");

    // The arena still works: sealing fixed the size, not the contents.
    assert_eq!(f.metadata().unwrap().len() as usize, l.arena_bytes());
}

/// A session with no shared region serves the same scan, payloads in the frames.
///
/// This is the portable path: the protocol makes no system calls and a byte stream
/// exists everywhere, so **only the shared region is platform-specific**. A host
/// that cannot make one still answers, at the cost of a second copy and a batch
/// bounded by frame size rather than by the arena.
#[test]
fn a_session_without_an_arena_serves_payloads_in_the_frames() {
    // The arena-backed scan, for the oracle.
    let (_c, _h, mut s) = setup("inline-ref");
    greet(&mut s);
    let snapshot = match s.handle(Frame::SnapshotOpen) {
        Frame::SnapshotOpened { snapshot, .. } => snapshot,
        other => panic!("{other:?}"),
    };
    let lanes = match s.handle(Frame::LanesAcquire {
        snapshot,
        keys: vec![10, 20, 30],
    }) {
        Frame::LanesAcquired { lanes, .. } => lanes,
        other => panic!("{other:?}"),
    };
    let mut shared = Vec::new();
    loop {
        match s.handle(Frame::BlockAdvance { lanes }) {
            Frame::BlockDone => break,
            Frame::Block { prefix, lanes: ls } => {
                shared.push((prefix, ls));
                s.handle(Frame::BlockRelease { lanes });
            }
            other => panic!("{other:?}"),
        }
    }

    // And with no arena at all.
    let (_c2, host, _drop) = setup("inline");
    let mut n = Session::new_inline(host, limits());
    assert!(!n.has_arena());
    assert!(n.arena_fd().is_none(), "there is no descriptor to send");
    match n.hello() {
        Frame::ServerHello { arena_bytes, .. } => assert_eq!(
            arena_bytes, 0,
            "zero is how a peer learns to expect inline payloads"
        ),
        other => panic!("{other:?}"),
    }
    greet(&mut n);
    let snapshot = match n.handle(Frame::SnapshotOpen) {
        Frame::SnapshotOpened { snapshot, .. } => snapshot,
        other => panic!("{other:?}"),
    };
    let lanes = match n.handle(Frame::LanesAcquire {
        snapshot,
        keys: vec![10, 20, 30],
    }) {
        Frame::LanesAcquired { lanes, .. } => lanes,
        other => panic!("{other:?}"),
    };
    let mut inline = Vec::new();
    let mut first_array: Option<Vec<u16>> = None;
    loop {
        match n.handle(Frame::BlockAdvance { lanes }) {
            Frame::BlockDone => break,
            Frame::BlocksInline { blocks, payload } => {
                assert_eq!(blocks.len(), 1, "the unbatched path sends one block");
                let b = &blocks[0];
                // Walk the payload the way a peer must: each lane's length follows
                // from its own kind and count, in order, with no offsets.
                let mut at = 0usize;
                for l in &b.lanes {
                    let n = l.kind.payload_bytes(l.count);
                    if l.kind == LaneKind::Array && first_array.is_none() && n > 0 {
                        first_array = Some(
                            payload[at..at + n]
                                .chunks_exact(2)
                                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                                .collect(),
                        );
                    }
                    at += n;
                }
                assert_eq!(at, payload.len(), "the descriptors account for every byte");
                inline.push((b.prefix, b.lanes.clone()));
                n.handle(Frame::BlockRelease { lanes });
            }
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(
        inline, shared,
        "the transport must not change what a scan reports"
    );
    assert_eq!(
        first_array,
        Some(vec![7, 8, 9]),
        "and the bytes are the real payload, read out of the frame"
    );
}

/// The arena descriptor survives the socket, and what arrives is the same memory.
///
/// Asserted by writing through the sender's mapping and reading through a mapping
/// made from the *received* descriptor. A test that only checked the descriptor
/// arrived would pass for a descriptor to the wrong thing.
#[test]
#[cfg(target_os = "linux")]
fn the_arena_descriptor_crosses_the_socket_and_names_the_same_memory() {
    use yesno_plugin::channel::{recv_fd, send_fd};

    let l = limits();
    let mut arena = Arena::new(l.arena_bytes()).unwrap();
    let (tx, rx) = std::os::unix::net::UnixStream::pair().unwrap();

    send_fd(&tx, arena.as_fd()).unwrap();
    let (got, version) = recv_fd(&rx).unwrap();
    assert_eq!(version, 1, "the byte carries the protocol version");

    // Write through the sender's mapping after the descriptor was sent.
    arena.write_at_for_test(0, &[0xAB, 0xCD]);

    let f = std::fs::File::from(got);
    let view = unsafe { memmap2::Mmap::map(&f) }.unwrap();
    assert_eq!(view.len(), l.arena_bytes(), "the whole region arrived");
    assert_eq!(
        &view[..2],
        &[0xAB, 0xCD],
        "the received descriptor must name the sender's memory, not a copy"
    );

    // And the seal travelled with it: a peer cannot resize what it received.
    assert!(
        f.set_len(0).is_err(),
        "seals belong to the file, so they hold on the receiving side too"
    );
}

/// A query wider than one handle splits across handles on **one** snapshot.
///
/// The shape is a consumer's: 256 query dimensions plus nine planes is 265 lanes in
/// one logical open, which no single handle need hold. Splitting is safe only if
/// both properties survive it -- every handle reads the one pinned version, and all
/// of them coexist so every lane is readable at once rather than in passes. Opening
/// a second *snapshot* for the overflow is the mistake the single-acquire frame
/// exists to prevent, and this asserts the safe alternative actually works.
#[test]
fn a_wide_query_splits_across_handles_without_splitting_the_snapshot() {
    let mut dir = std::env::temp_dir();
    // Deliberately not "yesno-chan-wide-...": `setup( "wide" )` already builds that
    // path, and two tests opening one directory is an `AlreadyOpen` that looks like
    // a channel bug.
    dir.push(format!("yesno-chan-widesplit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let _c = Clean(dir.clone());
    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 1,
            ..Default::default()
        },
    )
    .unwrap();
    // 265 keys, each with one chunk, so every lane is present in one block.
    let mut b = db.batch();
    for k in 0..265u64 {
        b.insert(k, k);
    }
    b.commit().unwrap();
    db.checkpoint().unwrap();

    let slot = Arc::new(RwLock::new(Some(Arc::new(db))));
    let host = Host::new(slot, 1, Role::Leader);
    // Deliberately narrower than the query, so the split is exercised.
    let l = Limits {
        max_handles: 3,
        max_lanes: 128,
        max_blocks: 1,
    };
    let arena = Arena::new(l.arena_bytes()).unwrap();
    let mut s = Session::new(host, arena, l);
    greet(&mut s);

    let snapshot = match s.handle(Frame::SnapshotOpen) {
        Frame::SnapshotOpened { snapshot, .. } => snapshot,
        other => panic!("{other:?}"),
    };

    // 265 lanes over three handles of at most 128, all on the one snapshot.
    let groups: Vec<Vec<u64>> = (0..265u64)
        .collect::<Vec<_>>()
        .chunks(128)
        .map(|c| c.to_vec())
        .collect();
    assert_eq!(groups.len(), 3, "265 lanes needs three handles of 128");

    let mut handles = Vec::new();
    let mut offsets = Vec::new();
    for g in &groups {
        match s.handle(Frame::LanesAcquire {
            snapshot,
            keys: g.clone(),
        }) {
            Frame::LanesAcquired { lanes, arena_off } => {
                handles.push(lanes);
                offsets.push(arena_off);
            }
            other => panic!("a split acquire must succeed, got {other:?}"),
        }
    }
    assert_eq!(
        offsets.len(),
        3,
        "all three coexist, so every lane is readable at once"
    );
    let mut sorted = offsets.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), 3, "and their arena slices do not alias");

    // Every handle advances over the same pinned version.
    let mut total_present = 0usize;
    for h in &handles {
        loop {
            match s.handle(Frame::BlockAdvance { lanes: *h }) {
                Frame::BlockDone => break,
                Frame::Block { lanes: ls, .. } => {
                    total_present += ls.iter().filter(|l| l.kind != LaneKind::Absent).count();
                    s.handle(Frame::BlockRelease { lanes: *h });
                }
                other => panic!("{other:?}"),
            }
        }
    }
    assert_eq!(
        total_present, 265,
        "every requested lane produced its chunk, across the split"
    );

    // One snapshot the whole way: one registry slot, not three.
    assert_eq!(s.outstanding().0, 1, "the split used a single snapshot");
    for h in handles {
        s.handle(Frame::LanesRelease { lanes: h });
    }
    s.handle(Frame::SnapshotClose { snapshot });
}

/// A single acquire of a consumer-sized query is within the protocol's own cap.
///
/// The regression this pins: `MAX_LANES` was 256, so 265 lanes could not be
/// expressed at all -- a policy number written into a structural cap.
#[test]
fn a_consumer_sized_lane_count_is_expressible() {
    let keys: Vec<u64> = (0..265).collect();
    assert!(
        keys.len() <= yesno_plugin::ipc::MAX_LANES,
        "256 dimensions plus nine planes must fit the protocol cap"
    );
    let f = Frame::LanesAcquire { snapshot: 1, keys };
    let bytes = f.encode().expect("a real query must be encodable");
    assert_eq!(Frame::decode(&bytes).unwrap().0, f);
}

/// The five SetSnapshot reads, against `yesno-core` itself as the oracle.
///
/// Each answer is compared with what the same snapshot returns directly, so the
/// channel cannot be self-consistently wrong.
#[test]
fn the_snapshot_reads_agree_with_the_engine() {
    let (_c, host, mut s) = setup("reads");
    greet(&mut s);
    let db = host.db().unwrap();
    let oracle = db.snapshot().unwrap();

    let snapshot = match s.handle(Frame::SnapshotOpen) {
        Frame::SnapshotOpened { snapshot, .. } => snapshot,
        other => panic!("{other:?}"),
    };

    // cardinality
    for key in [10u64, 20, 30, 999] {
        let want = oracle.cardinality(key).unwrap();
        assert_eq!(
            s.handle(Frame::SnapshotCardinality { snapshot, key }),
            Frame::Count { value: want },
            "cardinality of {key}"
        );
    }

    // contains, both answers and a key that holds nothing
    for (key, ordinal) in [(10u64, 7u64), (10, 8), (10, 6), (999, 0)] {
        let want = oracle.contains(key, ordinal).unwrap();
        assert_eq!(
            s.handle(Frame::SnapshotContains {
                snapshot,
                key,
                ordinal
            }),
            Frame::Bool {
                value: u8::from(want)
            },
            "contains({key}, {ordinal})"
        );
    }

    // max, including the absent case
    for key in [10u64, 20, 30, 999] {
        let want = oracle.max(key).unwrap();
        assert_eq!(
            s.handle(Frame::SnapshotMax { snapshot, key }),
            Frame::Ordinal {
                present: u8::from(want.is_some()),
                value: want.unwrap_or(0)
            },
            "max of {key}"
        );
    }

    // key_range over the whole space
    let want = oracle.key_range(0, u64::MAX).unwrap();
    assert_eq!(
        s.handle(Frame::SnapshotKeyRange {
            snapshot,
            lo: 0,
            hi: u64::MAX,
            limit: 1000
        }),
        Frame::Keys {
            values: want.clone(),
            more: 0
        }
    );
    assert_eq!(want, vec![10, 20, 30], "the fixture's three keys");

    s.handle(Frame::SnapshotClose { snapshot });
}

/// Paging `load` reassembles exactly the set, at every page size.
///
/// Continuation is by value rather than by cursor, so the property to pin is that
/// stitching pages together equals one materialized read -- no gaps at a chunk
/// boundary, no duplicate at a resume point, and `more` telling the truth.
#[test]
fn paging_a_load_reassembles_the_whole_set() {
    let (_c, host, mut s) = setup("load-page");
    greet(&mut s);
    let db = host.db().unwrap();
    // key 20 spans one chunk with 5000 scattered ordinals; key 10 spans three
    // chunks with three each. Both boundaries matter: within a chunk and across.
    let oracle: Vec<u64> = db.snapshot().unwrap().load(20).unwrap().iter().collect();
    let across: Vec<u64> = db.snapshot().unwrap().load(10).unwrap().iter().collect();
    assert_eq!(oracle.len(), 5000);
    assert_eq!(across.len(), 9, "three chunks of three");

    let snapshot = match s.handle(Frame::SnapshotOpen) {
        Frame::SnapshotOpened { snapshot, .. } => snapshot,
        other => panic!("{other:?}"),
    };

    for (key, want) in [(20u64, &oracle), (10, &across)] {
        for page in [1usize, 2, 7, 100, 4096] {
            let mut got: Vec<u64> = Vec::new();
            let mut after: Option<u64> = None;
            let mut rounds = 0;
            loop {
                rounds += 1;
                assert!(rounds < 20_000, "paging did not terminate");
                let f = s.handle(Frame::SnapshotLoad {
                    snapshot,
                    key,
                    after: after.unwrap_or(0),
                    has_after: u8::from(after.is_some()),
                    limit: page as u32,
                });
                match f {
                    Frame::Ordinals { values, more } => {
                        assert!(values.len() <= page, "a page must not exceed its limit");
                        if let Some(&last) = values.last() {
                            after = Some(last);
                        }
                        let empty = values.is_empty();
                        got.extend(values);
                        if more == 0 {
                            assert!(!empty || got.is_empty(), "more=0 ends the walk");
                            break;
                        }
                        assert!(!empty, "more=1 with an empty page would never terminate");
                    }
                    other => panic!("{other:?}"),
                }
            }
            assert_eq!(
                &got, want,
                "key {key} paged at {page} must reassemble to the whole set"
            );
        }
    }
    s.handle(Frame::SnapshotClose { snapshot });
}

/// A page is capped by the server, and `more` reports honestly at the boundary.
#[test]
fn a_page_is_capped_and_more_is_honest() {
    let (_c, _h, mut s) = setup("page-cap");
    greet(&mut s);
    let snapshot = match s.handle(Frame::SnapshotOpen) {
        Frame::SnapshotOpened { snapshot, .. } => snapshot,
        other => panic!("{other:?}"),
    };
    // Asking for more than MAX_PAGE gets MAX_PAGE, not a fault: the cap is the
    // server's and a peer should not have to know it to make progress.
    match s.handle(Frame::SnapshotLoad {
        snapshot,
        key: 20,
        after: 0,
        has_after: 0,
        limit: u32::MAX,
    }) {
        Frame::Ordinals { values, more } => {
            assert_eq!(values.len(), 4096, "clamped to MAX_PAGE");
            assert_eq!(more, 1, "5000 ordinals do not fit one page");
        }
        other => panic!("{other:?}"),
    }
    // And a key that holds nothing is an empty page with more = 0, not a fault.
    assert_eq!(
        s.handle(Frame::SnapshotLoad {
            snapshot,
            key: 999,
            after: 0,
            has_after: 0,
            limit: 10
        }),
        Frame::Ordinals {
            values: vec![],
            more: 0
        }
    );
    s.handle(Frame::SnapshotClose { snapshot });
}

/// A read against an unknown snapshot is refused, for every one of the five.
#[test]
fn every_read_refuses_an_unknown_snapshot() {
    let (_c, _h, mut s) = setup("reads-unknown");
    greet(&mut s);
    for f in [
        Frame::SnapshotCardinality {
            snapshot: 99,
            key: 1,
        },
        Frame::SnapshotContains {
            snapshot: 99,
            key: 1,
            ordinal: 1,
        },
        Frame::SnapshotMax {
            snapshot: 99,
            key: 1,
        },
        Frame::SnapshotLoad {
            snapshot: 99,
            key: 1,
            after: 0,
            has_after: 0,
            limit: 10,
        },
        Frame::SnapshotKeyRange {
            snapshot: 99,
            lo: 0,
            hi: 10,
            limit: 10,
        },
    ] {
        let kind = f.kind();
        let r = s.handle(f);
        assert_eq!(
            fault_status(&r),
            Status::InvalidArgument as u32,
            "{kind:?} must refuse an unknown snapshot"
        );
    }
}

/// A wide block of **dense** lanes, in both transports.
///
/// # What the earlier inline test missed
///
/// It used two sparse lanes, so its frames were a few hundred bytes and it never
/// approached `MAX_INLINE_PAYLOAD`. That is a test passing for a reason unrelated to
/// the property: inline mode advertised 1024 lanes while one block of bitmap lanes
/// at that width is 8 MiB against a 1 MiB cap, so `Frame::encode` refused and the
/// connection died -- for a configuration the server itself called legal. Payload
/// size only bites when the lanes are **dense**, so the fixture has to make them so.
///
/// The arena path is unaffected and is checked alongside, because a `Block` frame
/// carries only descriptors: the contrast is the argument for the arena.
#[test]
fn a_wide_block_of_dense_lanes_is_served_in_both_transports() {
    let mut dir = std::env::temp_dir();
    dir.push(format!("yesno-chan-dense-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let _c = Clean(dir.clone());
    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 1,
            ..Default::default()
        },
    )
    .unwrap();
    // 200 keys, each a bitmap in chunk 0: scattered past RUN_MAX_INTERVALS so the
    // container is 8 KiB, which is the worst case the frame has to hold.
    const KEYS: u64 = 200;
    let mut b = db.batch();
    for k in 0..KEYS {
        for i in 0..5000u64 {
            b.insert(k, i * 3);
        }
    }
    b.commit().unwrap();
    db.checkpoint().unwrap();
    let keys: Vec<u64> = (0..KEYS).collect();

    let slot = Arc::new(RwLock::new(Some(Arc::new(db))));
    let host = Host::new(slot, 1, Role::Leader);
    // Deliberately configured wider than inline can carry, which is the case that
    // produced a dead connection.
    let wide = Limits {
        max_handles: 4,
        max_lanes: 1024,
        max_blocks: 16,
    };

    // --- inline ---
    let mut inline = Session::new_inline(host.clone(), wide);
    let advertised = match inline.hello() {
        Frame::ServerHello {
            max_lanes,
            max_blocks,
            arena_bytes,
            ..
        } => {
            assert_eq!(arena_bytes, 0);
            assert!(
                max_lanes as usize <= Session::INLINE_MAX_LANES,
                "inline must not advertise more lanes than one frame can hold: {max_lanes}"
            );
            assert!(max_blocks >= 1);
            assert!(
                max_lanes as usize * max_blocks as usize * 8192 <= 1024 * 1024,
                "the advertised width and depth together must fit MAX_INLINE_PAYLOAD"
            );
            max_lanes as usize
        }
        other => panic!("{other:?}"),
    };
    greet(&mut inline);
    let snapshot = match inline.handle(Frame::SnapshotOpen) {
        Frame::SnapshotOpened { snapshot, .. } => snapshot,
        other => panic!("{other:?}"),
    };
    // A request at the advertised width must be served, not refused and not fatal.
    let lanes = match inline.handle(Frame::LanesAcquire {
        snapshot,
        keys: keys[..advertised].to_vec(),
    }) {
        Frame::LanesAcquired { lanes, .. } => lanes,
        other => panic!("a request at the advertised width must succeed: {other:?}"),
    };
    match inline.handle(Frame::BlockAdvance { lanes }) {
        Frame::BlocksInline { blocks, payload } => {
            assert_eq!(blocks[0].lanes.len(), advertised);
            assert_eq!(
                payload.len(),
                advertised * 8192,
                "every lane is a full bitmap, which is the worst case"
            );
            // And it round trips, which is what the connection actually needs.
            let f = Frame::BlocksInline { blocks, payload };
            let bytes = f
                .encode()
                .expect("a block at the advertised width must encode");
            assert_eq!(Frame::decode(&bytes).unwrap().0, f);
        }
        other => panic!("{other:?}"),
    }
    // Wider than advertised is a refusal the peer can read, never a dead socket.
    let too_wide = inline.handle(Frame::LanesAcquire {
        snapshot,
        keys: keys.clone(),
    });
    assert_eq!(fault_status(&too_wide), Status::InvalidArgument as u32);

    // --- arena, same width, unaffected ---
    let arena = Arena::new(wide.arena_bytes()).unwrap();
    let mut shared = Session::new(host, arena, wide);
    greet(&mut shared);
    let snapshot = match shared.handle(Frame::SnapshotOpen) {
        Frame::SnapshotOpened { snapshot, .. } => snapshot,
        other => panic!("{other:?}"),
    };
    let lanes = match shared.handle(Frame::LanesAcquire {
        snapshot,
        keys: keys.clone(),
    }) {
        Frame::LanesAcquired { lanes, .. } => lanes,
        other => panic!("the arena carries the full width: {other:?}"),
    };
    match shared.handle(Frame::BlockAdvance { lanes }) {
        Frame::Block { lanes: ls, .. } => {
            assert_eq!(ls.len(), KEYS as usize, "all 200 lanes in one block");
            let bytes = Frame::Block {
                prefix: 0,
                lanes: ls,
            }
            .encode()
            .expect("descriptors are small whatever the payloads are");
            assert!(
                bytes.len() < 2048,
                "a 200-lane Block frame is descriptors only: {} bytes",
                bytes.len()
            );
        }
        other => panic!("{other:?}"),
    }
}
