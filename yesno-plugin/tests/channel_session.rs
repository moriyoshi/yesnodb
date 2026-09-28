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
