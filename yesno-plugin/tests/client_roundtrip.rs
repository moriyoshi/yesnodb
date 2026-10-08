//! The client against the server, over a real Unix socket.
//!
//! `channel_session.rs` drives [`Session::handle`] frame by frame with no socket,
//! which is the right shape for the protocol logic. This covers what that cannot:
//! the handshake's *ordering* -- greeting first, `ClientHello` second, `Done` third
//! -- the framing over a stream, and the client's own block walk.
//!
//! **Both payload modes**, and the same assertions over each. `serve_blocking` sends
//! the greeting but not a descriptor, so the arena cases do what `yesno-server` does
//! on accept: `send_fd( &stream, arena.as_fd() )` first, then serve.
//!
//! Running one walk through both modes is the point rather than tidiness. Inline
//! payloads are packed and addressed by a running sum; arena payloads are a fixed
//! stride via [`batched_lane_offset`]. Those are two separate pieces of arithmetic
//! for the same bytes, and `ipc.rs` says what the hazard is -- "the two sides getting
//! it separately right is the failure this layout exists to prevent." A multi-block
//! batch is required to see it at all: with one block the stride term is multiplied
//! by zero and any wrong `max_lanes` factor disappears.
use std::os::unix::net::UnixListener;
use std::sync::{Arc, RwLock};

use yesno_core::{Db, DbOptions};
use yesno_plugin::abi::Role;
use yesno_plugin::channel::{send_fd, serve_blocking, Arena, Limits, Session};
use yesno_plugin::client::Client;
use yesno_plugin::ipc::LaneKind;
use yesno_plugin::Host;

struct Clean(std::path::PathBuf);
impl Drop for Clean {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `max_blocks` is deliberately larger than any key here needs, so a batch comes
/// back **short** and the "fewer than asked for means the scan ended" rule is the
/// thing under test. With `max_blocks: 1` a short batch is indistinguishable from a
/// full one and the rule would go unexercised.
fn limits() -> Limits {
    Limits {
        max_handles: 2,
        max_lanes: 4,
        max_blocks: 4,
        max_snapshots: 8,
        max_writes: yesno_plugin::ipc::MAX_WRITES,
    }
}

fn setup(tag: &str) -> (Clean, Host, std::path::PathBuf) {
    let mut dir = std::env::temp_dir();
    dir.push(format!("yesno-client-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let clean = Clean(dir.clone());

    let db = Db::open_with(
        dir.join("db"),
        DbOptions {
            shards: 1,
            ..Default::default()
        },
    )
    .unwrap();
    let mut b = db.batch();
    // Key 1: three ordinals in chunk 0, so one block with one array lane whose
    // payload is the three lows. Key 2: one ordinal, also chunk 0.
    for o in [5u64, 6, 7] {
        b.insert(1, o);
    }
    b.insert(2, 100);
    // Key 3 spans three chunks, so a single `BlockAdvanceMany` returns three blocks
    // and the arena stride is exercised at block 1 and block 2 rather than only at
    // block 0, where it is multiplied by zero.
    for o in [1u64, 2] {
        b.insert(3, o);
    }
    for o in [65536u64 + 10, 65536 + 11] {
        b.insert(3, o);
    }
    b.insert(3, 131072 + 20);
    b.commit().unwrap();
    db.checkpoint().unwrap();

    let slot = Arc::new(RwLock::new(Some(Arc::new(db))));
    let host = Host::new(slot, 1, Role::Leader);
    (clean, host, dir.join("sock"))
}

/// Serve one connection in inline mode: greeting, then frames carrying payloads.
fn serve_inline(listener: UnixListener, host: Host) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut session = Session::new_inline(host, limits());
        let _ = serve_blocking(&mut session, stream);
    })
}

/// Serve one connection in arena mode, exactly as `yesno-server` does on accept: the
/// descriptor goes first, carrying the protocol version as its one byte of payload,
/// and only then does the greeting follow.
fn serve_arena(listener: UnixListener, host: Host) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let arena = Arena::new(limits().arena_bytes()).expect("memfd arena");
        send_fd(&stream, arena.as_fd()).expect("hand over the arena descriptor");
        let mut session = Session::new(host, arena, limits());
        let _ = serve_blocking(&mut session, stream);
    })
}

/// Walk key 3's three blocks and check every payload byte.
///
/// Identical expectations in both modes, which is what makes this a differential test
/// of the two offset schemes rather than two restatements of one.
fn walk_key_three(client: &Client) {
    let snap = client.snapshot().unwrap();
    let mut cursor = snap.lanes(vec![3]).unwrap();

    // Chunk-local lows, little-endian u16, one array lane per block.
    let expected: [(u64, &[u8]); 3] = [(0, &[1, 0, 2, 0]), (1, &[10, 0, 11, 0]), (2, &[20, 0])];
    for (prefix, payload) in expected {
        assert!(cursor.advance().unwrap(), "block {prefix} should be there");
        assert_eq!(cursor.prefix(), Some(prefix));
        assert_eq!(cursor.lane_count(), 1);
        let (lane, bytes) = cursor.lane(0).unwrap();
        assert_eq!(lane.kind, LaneKind::Array);
        assert_eq!(bytes, payload, "payload of block {prefix}");
    }
    // Three blocks against max_blocks of 4 is a short batch, so the scan is over and
    // no further round trip is made.
    assert!(!cursor.advance().unwrap());
}

/// The three-block walk over inline payloads.
#[test]
fn inline_payloads_walk_three_blocks() {
    let (_clean, host, sock) = setup("inline3");
    let listener = UnixListener::bind(&sock).unwrap();
    let server = serve_inline(listener, host);
    {
        let client = Client::connect(&sock, "inline3").unwrap();
        assert!(!client.is_arena());
        walk_key_three(&client);
    }
    server.join().unwrap();
}

/// The same walk over the shared arena, which is the zero-copy path.
///
/// Covers what no other test here does: the `SCM_RIGHTS` receive in
/// `Client::connect`, the version byte arriving *before* `ServerHello`, mapping the
/// descriptor, and the batched stride at a non-zero block index.
#[test]
fn arena_payloads_walk_the_same_three_blocks() {
    let (_clean, host, sock) = setup("arena3");
    let listener = UnixListener::bind(&sock).unwrap();
    let server = serve_arena(listener, host);
    {
        let client = Client::connect(&sock, "arena3").unwrap();
        assert!(client.is_arena(), "the server handed over a descriptor");
        assert!(client.limits().arena_bytes > 0);
        walk_key_three(&client);
    }
    server.join().unwrap();
}

/// One server, one client, over a socket: handshake, point reads, and a block walk.
#[test]
fn a_client_completes_the_handshake_and_reads_what_the_server_holds() {
    let (_clean, host, sock) = setup("roundtrip");
    let listener = UnixListener::bind(&sock).unwrap();

    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut session = Session::new_inline(host, limits());
        // Returns when the peer closes, which the client's drop does below.
        let _ = serve_blocking(&mut session, stream);
    });

    {
        let client = Client::connect(&sock, "roundtrip-test").unwrap();

        // The greeting's limits reach the caller rather than being rediscovered.
        assert!(
            !client.is_arena(),
            "serve_blocking offers no arena descriptor"
        );
        assert_eq!(client.limits().max_blocks, 4);
        assert_eq!(client.limits().max_lanes, 4);

        let snap = client.snapshot().unwrap();

        assert_eq!(snap.cardinality(1).unwrap(), 3);
        assert_eq!(snap.cardinality(2).unwrap(), 1);
        // A key that was never written is not an error, it is empty.
        assert_eq!(snap.cardinality(999).unwrap(), 0);

        assert!(snap.contains(1, 6).unwrap());
        assert!(!snap.contains(1, 8).unwrap());
        assert!(!snap.contains(999, 6).unwrap());

        assert_eq!(snap.max(1).unwrap(), Some(7));
        assert_eq!(snap.max(999).unwrap(), None);

        // Keys 1, 2 and 3 are populated by `setup`; the range enumerates exactly the
        // populated subset, not the span.
        let (keys, more) = snap.key_range(0, 10, 16).unwrap();
        assert_eq!(keys, vec![1, 2, 3]);
        assert!(!more);
        // A limit below the number available reports that more remain.
        let (first, more) = snap.key_range(0, 10, 2).unwrap();
        assert_eq!(first, vec![1, 2]);
        assert!(more, "two of three keys means more remain");

        // The block walk. Key 1 has one chunk, so one block of one array lane, and
        // the batch is short of `max_blocks` -- so the walk must end without a
        // second round trip.
        let mut cursor = snap.lanes(vec![1]).unwrap();
        assert!(cursor.advance().unwrap(), "the first block should be there");
        assert_eq!(cursor.prefix(), Some(0));
        assert_eq!(cursor.lane_count(), 1);
        let (lane, payload) = cursor.lane(0).unwrap();
        assert_eq!(lane.kind, LaneKind::Array);
        assert_eq!(lane.count, 3);
        // An array lane carries the chunk-local lows as little-endian `u16`.
        assert_eq!(payload, &[5u8, 0, 6, 0, 7, 0]);
        assert!(
            !cursor.advance().unwrap(),
            "a batch shorter than max_blocks ends the scan"
        );

        // Asking past the end stays false rather than erroring or looping.
        assert!(!cursor.advance().unwrap());
    }

    server.join().unwrap();
}

/// A key with no ordinals yields no blocks at all, and the walk says so immediately.
#[test]
fn an_empty_key_yields_an_empty_walk() {
    let (_clean, host, sock) = setup("empty");
    let listener = UnixListener::bind(&sock).unwrap();
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut session = Session::new_inline(host, limits());
        let _ = serve_blocking(&mut session, stream);
    });

    {
        let client = Client::connect(&sock, "empty-test").unwrap();
        let snap = client.snapshot().unwrap();
        let mut cursor = snap.lanes(vec![999]).unwrap();
        assert!(!cursor.advance().unwrap());
        assert_eq!(cursor.prefix(), None);
        assert_eq!(cursor.lane_count(), 0);
        // No current block, so a lane request is a protocol error from the caller's
        // side rather than a panic or an empty slice.
        assert!(cursor.lane(0).is_err());
    }

    server.join().unwrap();
}
