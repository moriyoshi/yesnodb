//! The out-of-process channel, end to end against a running yesnod.
//!
//! A real Unix socket, a real descriptor handoff, a real arena mapping. The session
//! tests in `yesno-plugin` drive the protocol as function calls; what can only fail
//! here is the wiring -- the socket bound, the descriptor sent before any frame, the
//! notifications reaching a peer, and the connections closing before teardown waits
//! on readers.

use std::io::{Read, Write};
use std::sync::{Arc, RwLock};

use yesno_core::{Db, DbOptions};
use yesno_plugin::abi::Role;
use yesno_plugin::channel::recv_fd;
use yesno_plugin::client::Client;
use yesno_plugin::ipc::{lane_offset, Frame, LaneKind};
use yesno_plugin::Host;
use yesno_server::config::Config;
use yesno_server::plugin::Channel;

struct Clean(std::path::PathBuf);
impl Drop for Clean {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A peer, written the way a third party would have to write one.
struct Peer {
    sock: std::os::unix::net::UnixStream,
    view: memmap2::Mmap,
    buf: Vec<u8>,
}

impl Peer {
    fn connect(path: &std::path::Path) -> Peer {
        let sock = std::os::unix::net::UnixStream::connect(path).unwrap();
        // The descriptor arrives first, before any frame.
        let (fd, version) = recv_fd(&sock).unwrap();
        assert_eq!(
            version,
            yesno_plugin::ipc::VERSION,
            "the handoff byte carries the protocol version"
        );
        let f = std::fs::File::from(fd);
        let view = unsafe { memmap2::Mmap::map(&f) }.unwrap();
        Peer {
            sock,
            view,
            buf: Vec::new(),
        }
    }

    fn read_frame(&mut self) -> Frame {
        loop {
            match Frame::decode(&self.buf) {
                Ok((f, used)) => {
                    self.buf.drain(..used);
                    return f;
                }
                Err(yesno_plugin::ipc::IpcError::Truncated) => {}
                Err(e) => panic!("{e}"),
            }
            let mut chunk = [0u8; 8192];
            let n = self.sock.read(&mut chunk).unwrap();
            assert!(n > 0, "the server closed unexpectedly");
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }

    fn ask(&mut self, f: Frame) -> Frame {
        self.sock.write_all(&f.encode().unwrap()).unwrap();
        self.read_frame()
    }
}

fn setup(
    tag: &str,
) -> (
    Clean,
    Config,
    Host,
    std::path::PathBuf,
    yesno_server::guard::DbSlot,
) {
    let mut dir = std::env::temp_dir();
    dir.push(format!("yesno-chanwire-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let clean = Clean(dir.clone());
    std::fs::create_dir_all(&dir).unwrap();

    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 1,
            ..Default::default()
        },
    )
    .unwrap();
    let mut b = db.batch();
    for c in [0u64, 1, 5] {
        for i in 0..3u64 {
            b.insert(10, c * 65536 + 7 + i);
        }
    }
    b.insert_range(30, 3 * 65536, 3 * 65536 + 4096);
    b.commit().unwrap();
    db.checkpoint().unwrap();

    let sock = dir.join("plugin.sock");
    let mut cfg = Config::default();
    cfg.server.data_dir = Some(dir.clone());
    cfg.plugin.channel_socket = sock.to_string_lossy().into_owned();
    cfg.plugin.channel_max_lanes = 4;
    cfg.plugin.channel_max_handles = 2;
    cfg.plugin.channel_max_blocks = 4;

    let slot = Arc::new(RwLock::new(Some(Arc::new(db))));
    let host = Host::new(slot.clone(), 1, Role::Leader);
    (clean, cfg, host, sock, slot)
}

/// A peer connects, is handed the arena, and reads a scan out of it.
#[test]
fn a_peer_scans_through_the_socket_and_the_arena() {
    let (_c, cfg, host, sock, _slot) = setup("scan");
    let channel = Channel::start(&cfg, host.clone()).unwrap().unwrap();

    let mut peer = Peer::connect(&sock);
    let (arena_bytes, max_blocks) = match peer.read_frame() {
        Frame::ServerHello {
            arena_bytes,
            max_blocks,
            role,
            ..
        } => {
            assert_eq!(role, yesno_plugin::ipc::Role::Leader);
            (arena_bytes, max_blocks)
        }
        other => panic!("the greeting must come first, got {other:?}"),
    };
    assert_eq!(
        arena_bytes as usize,
        peer.view.len(),
        "the advertised size must match the region actually handed over"
    );
    assert_eq!(max_blocks, 4);

    assert_eq!(
        peer.ask(Frame::ClientHello {
            protocol: yesno_plugin::ipc::VERSION as u32,
            name: "test-peer".into()
        }),
        Frame::Done
    );
    let snapshot = match peer.ask(Frame::SnapshotOpen) {
        Frame::SnapshotOpened { snapshot, .. } => snapshot,
        other => panic!("{other:?}"),
    };
    let (lanes, arena_off) = match peer.ask(Frame::LanesAcquire {
        snapshot,
        keys: vec![10, 30],
    }) {
        Frame::LanesAcquired { lanes, arena_off } => (lanes, arena_off),
        other => panic!("{other:?}"),
    };

    let mut seen = Vec::new();
    let mut first_array: Option<Vec<u16>> = None;
    loop {
        match peer.ask(Frame::BlockAdvance { lanes }) {
            Frame::BlockDone => break,
            Frame::Block { prefix, lanes: ls } => {
                for (i, l) in ls.iter().enumerate() {
                    if l.kind == LaneKind::Array && first_array.is_none() {
                        let off = lane_offset(arena_off, i) as usize;
                        let n = l.kind.payload_bytes(l.count);
                        first_array = Some(
                            peer.view[off..off + n]
                                .as_chunks::<2>()
                                .0
                                .iter()
                                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                                .collect(),
                        );
                    }
                }
                seen.push((prefix, ls.iter().map(|l| l.kind).collect::<Vec<_>>()));
                peer.ask(Frame::BlockRelease { lanes });
            }
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(
        seen,
        vec![
            (0, vec![LaneKind::Array, LaneKind::Absent]),
            (1, vec![LaneKind::Array, LaneKind::Absent]),
            (3, vec![LaneKind::Absent, LaneKind::Run]),
            (5, vec![LaneKind::Array, LaneKind::Absent]),
        ]
    );
    assert_eq!(
        first_array,
        Some(vec![7, 8, 9]),
        "read out of the shared region the server wrote, through the socket's descriptor"
    );

    assert_eq!(channel.peers(), 1);
    channel.stop();
}

/// `yesno_plugin::client` against a running channel, not against a test harness.
///
/// The `Peer` above is a deliberately low-level harness: it exists so these tests can
/// drive one frame at a time and poke at edges a real client validates away. It is
/// **not** the client anyone ships, so until now the shipped client had never spoken
/// to a live server -- every test of it drove `serve_blocking` in-process. That is
/// the gap this closes, and it is the one that matters before the client is public.
///
/// What is live here and nowhere else: `Channel::start` binds the socket and spawns a
/// serving thread per connection, the server creates a real `memfd` arena and sends
/// its descriptor with `SCM_RIGHTS` before any frame, and the client receives it,
/// maps it, and reads payloads out of the server's own shared region.
///
/// `channel_max_blocks` is 4 and key 10 spans three chunks, so one batch comes back
/// **short and multi-block** -- which exercises the batched arena stride at block 1
/// and block 2 against memory the server actually wrote. At block 0 the stride term
/// is multiplied by zero, so a single-block check would pass against a wrong
/// `max_lanes` factor.
#[test]
fn the_shipped_client_reads_through_a_live_channel_and_its_real_arena() {
    let (_c, cfg, host, sock, _slot) = setup("client");
    let channel = Channel::start(&cfg, host).unwrap().unwrap();

    let client = Client::connect(&sock, "integration").unwrap();
    assert!(
        client.is_arena(),
        "a configured channel hands over a descriptor, so this is the zero-copy path"
    );
    assert!(client.limits().arena_bytes > 0);
    assert_eq!(client.limits().max_blocks, 4);

    let snap = client.snapshot().unwrap();

    // Key 10 is three chunks of three ordinals. Key 30 is one `insert_range`, which
    // is **inclusive of both endpoints** -- `3 * 65536 ..= 3 * 65536 + 4096` is 4097
    // ordinals, not 4096. Spelled out because it is an off-by-one that reads as
    // correct either way.
    assert_eq!(snap.cardinality(10).unwrap(), 9);
    assert_eq!(snap.cardinality(30).unwrap(), 4097);
    assert!(snap.contains(10, 7).unwrap());
    assert!(!snap.contains(10, 6).unwrap());
    assert_eq!(snap.max(10).unwrap(), Some(5 * 65536 + 9));

    let (ordinals, more) = snap.load(10, None, 64).unwrap();
    assert_eq!(
        ordinals,
        vec![7, 8, 9, 65543, 65544, 65545, 327687, 327688, 327689]
    );
    assert!(!more);

    // The block walk, out of the server's arena.
    let mut cursor = snap.lanes(vec![10]).unwrap();
    let mut prefixes = Vec::new();
    while cursor.advance().unwrap() {
        let prefix = cursor.prefix().expect("a current block while advancing");
        assert_eq!(cursor.lane_count(), 1, "one key, so one lane a block");
        let (lane, bytes) = cursor.lane(0).unwrap();
        // The declared geometry and the bytes delivered have to agree; this is the
        // arithmetic both sides compute separately.
        assert_eq!(bytes.len(), lane.kind.payload_bytes(lane.count));
        assert_eq!(lane.kind, LaneKind::Array);
        assert_eq!(lane.count, 3);
        let lows: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        assert_eq!(lows, vec![7, 8, 9], "chunk-local lows of block {prefix}");
        prefixes.push(prefix);
    }
    assert_eq!(
        prefixes,
        vec![0, 1, 5],
        "three chunks, ascending, one batch"
    );

    // A second handle over a different shape: 4096 contiguous ordinals in chunk 3.
    let mut run = snap.lanes(vec![30]).unwrap();
    assert!(run.advance().unwrap());
    assert_eq!(run.prefix(), Some(3));
    let (lane, bytes) = run.lane(0).unwrap();
    assert_eq!(bytes.len(), lane.kind.payload_bytes(lane.count));
    assert!(!run.advance().unwrap());

    assert_eq!(channel.peers(), 1, "the client is one peer, not two");
    // Stopped with the client still alive on purpose: its handles then drop against a
    // closed socket, which is the path where a release must fail silently rather than
    // panic in a destructor.
    channel.stop();
}

/// Notifications reach a connected peer without it asking.
///
/// A peer that had to poll for a replacement would serve answers from a database
/// that no longer exists between polls, which is why these are pushed.
#[test]
fn a_connected_peer_is_told_about_availability_without_asking() {
    let (_c, cfg, host, sock, _slot) = setup("notify");
    let channel = Channel::start(&cfg, host).unwrap().unwrap();
    let mut peer = Peer::connect(&sock);
    assert!(matches!(peer.read_frame(), Frame::ServerHello { .. }));

    channel.notify(&Frame::Unavailable);
    assert_eq!(peer.read_frame(), Frame::Unavailable);

    channel.notify(&Frame::GenerationChanged { old: 1, new: 2 });
    channel.notify(&Frame::Available { generation: 2 });
    assert_eq!(
        peer.read_frame(),
        Frame::GenerationChanged { old: 1, new: 2 }
    );
    assert_eq!(peer.read_frame(), Frame::Available { generation: 2 });

    channel.stop();
}

/// A peer that vanishes mid-scan has its snapshots released, with no cooperation.
///
/// **This is the property the whole shape is chosen for.** The peer is dropped
/// without closing anything; the kernel closes the socket, the serving thread's read
/// returns zero, and the session drops. Nothing polls and no pid is consulted.
#[test]
fn a_vanished_peer_releases_its_snapshots() {
    let (_c, cfg, host, sock, _slot) = setup("vanish");
    let channel = Channel::start(&cfg, host.clone()).unwrap().unwrap();
    let db = host.db().unwrap();
    assert_eq!(db.live_readers(), 0);

    let mut peer = Peer::connect(&sock);
    assert!(matches!(peer.read_frame(), Frame::ServerHello { .. }));
    peer.ask(Frame::ClientHello {
        protocol: yesno_plugin::ipc::VERSION as u32,
        name: "doomed".into(),
    });
    let snapshot = match peer.ask(Frame::SnapshotOpen) {
        Frame::SnapshotOpened { snapshot, .. } => snapshot,
        other => panic!("{other:?}"),
    };
    peer.ask(Frame::LanesAcquire {
        snapshot,
        keys: vec![10],
    });
    assert_eq!(db.live_readers(), 1, "the peer's snapshot is registered");

    // No close, no goodbye: simply gone.
    drop(peer);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while db.live_readers() > 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(
        db.live_readers(),
        0,
        "the socket closing is what releases the snapshot; nothing else was asked to"
    );
    channel.stop();
}

/// Stopping the channel closes live connections, which is what lets the database go.
#[test]
fn stopping_the_channel_releases_a_well_behaved_peer_too() {
    let (_c, cfg, host, sock, _slot) = setup("stop");
    let channel = Channel::start(&cfg, host.clone()).unwrap().unwrap();
    let db = host.db().unwrap();

    let mut peer = Peer::connect(&sock);
    assert!(matches!(peer.read_frame(), Frame::ServerHello { .. }));
    peer.ask(Frame::ClientHello {
        protocol: yesno_plugin::ipc::VERSION as u32,
        name: "polite".into(),
    });
    peer.ask(Frame::SnapshotOpen);
    assert_eq!(db.live_readers(), 1);

    channel.stop();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while db.live_readers() > 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(
        db.live_readers(),
        0,
        "teardown must not depend on a peer choosing to disconnect"
    );
    assert!(!sock.exists(), "and the socket is removed behind it");
}

/// An unconfigured channel binds nothing.
#[test]
fn an_unconfigured_channel_is_absent() {
    let (_c, mut cfg, host, _sock, _slot) = setup("off");
    cfg.plugin.channel_socket = String::new();
    assert!(Channel::start(&cfg, host).unwrap().is_none());
}

/// A peer served inline: no descriptor, payloads in the frames, same answers.
///
/// # What this closes
///
/// `Session::new_inline` made the *protocol* portable, and until `serve_one` used it
/// the running server still called `Arena::new` unconditionally and refused the
/// connection when it failed. A deployment document would have read the protocol
/// property as a server property. This drives the wired inline path over a real
/// socket, so the two claims are the same claim.
///
/// It is reachable on Linux through configuration rather than only on a host without
/// `memfd`, which is what makes it testable at all.
#[test]
fn a_peer_is_served_inline_when_the_host_has_no_arena() {
    let (_c, mut cfg, host, sock, _slot) = setup("inline");
    cfg.plugin.channel_inline = true;
    // Configured wider than an inline frame can carry, deliberately: the server must
    // advertise what it can actually encode rather than what the file says, and the
    // greeting is checked below. Left at the default before, this test could not see
    // the difference.
    cfg.plugin.channel_max_lanes = 1024;
    let channel = Channel::start(&cfg, host).unwrap().unwrap();

    // Connect *without* expecting a descriptor. A peer that blocked on recv_fd here
    // would hang, which is why arena_bytes = 0 is in the greeting.
    let mut sock_raw = std::os::unix::net::UnixStream::connect(&sock).unwrap();
    let mut buf = Vec::new();
    let read_frame = |s: &mut std::os::unix::net::UnixStream, buf: &mut Vec<u8>| -> Frame {
        loop {
            match Frame::decode(buf) {
                Ok((f, used)) => {
                    buf.drain(..used);
                    return f;
                }
                Err(yesno_plugin::ipc::IpcError::Truncated) => {}
                Err(e) => panic!("{e}"),
            }
            let mut chunk = [0u8; 8192];
            let n = s.read(&mut chunk).unwrap();
            assert!(n > 0, "the server closed unexpectedly");
            buf.extend_from_slice(&chunk[..n]);
        }
    };
    let ask = |s: &mut std::os::unix::net::UnixStream, buf: &mut Vec<u8>, f: Frame| -> Frame {
        s.write_all(&f.encode().unwrap()).unwrap();
        read_frame(s, buf)
    };

    match read_frame(&mut sock_raw, &mut buf) {
        Frame::ServerHello {
            arena_bytes,
            max_lanes,
            max_blocks,
            ..
        } => {
            assert_eq!(
                arena_bytes, 0,
                "zero is how a peer learns there is no descriptor coming"
            );
            // The configuration asked for 1024. An inline frame cannot hold that
            // many dense lanes, so the greeting must say what it can encode.
            assert!(
                max_lanes as usize * max_blocks as usize * 8192 <= 1024 * 1024,
                "advertised width times depth must fit an inline frame, got \
                 {max_lanes} lanes by {max_blocks} blocks"
            );
        }
        other => panic!("{other:?}"),
    }
    ask(
        &mut sock_raw,
        &mut buf,
        Frame::ClientHello {
            protocol: yesno_plugin::ipc::VERSION as u32,
            name: "inline-peer".into(),
        },
    );
    let snapshot = match ask(&mut sock_raw, &mut buf, Frame::SnapshotOpen) {
        Frame::SnapshotOpened { snapshot, .. } => snapshot,
        other => panic!("{other:?}"),
    };
    let lanes = match ask(
        &mut sock_raw,
        &mut buf,
        Frame::LanesAcquire {
            snapshot,
            keys: vec![10, 30],
        },
    ) {
        Frame::LanesAcquired { lanes, .. } => lanes,
        other => panic!("{other:?}"),
    };

    let mut seen = Vec::new();
    let mut first_array: Option<Vec<u16>> = None;
    loop {
        match ask(&mut sock_raw, &mut buf, Frame::BlockAdvance { lanes }) {
            Frame::BlockDone => break,
            Frame::BlocksInline { blocks, payload } => {
                assert_eq!(blocks.len(), 1);
                let b = &blocks[0];
                let mut at = 0usize;
                for l in &b.lanes {
                    let n = l.kind.payload_bytes(l.count);
                    if l.kind == LaneKind::Array && first_array.is_none() && n > 0 {
                        first_array = Some(
                            payload[at..at + n]
                                .as_chunks::<2>()
                                .0
                                .iter()
                                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                                .collect(),
                        );
                    }
                    at += n;
                }
                assert_eq!(at, payload.len(), "descriptors account for every byte");
                seen.push((b.prefix, b.lanes.iter().map(|l| l.kind).collect::<Vec<_>>()));
                ask(&mut sock_raw, &mut buf, Frame::BlockRelease { lanes });
            }
            other => panic!("expected an inline block, got {other:?}"),
        }
    }
    assert_eq!(
        seen,
        vec![
            (0, vec![LaneKind::Array, LaneKind::Absent]),
            (1, vec![LaneKind::Array, LaneKind::Absent]),
            (3, vec![LaneKind::Absent, LaneKind::Run]),
            (5, vec![LaneKind::Array, LaneKind::Absent]),
        ],
        "the inline transport must report exactly what the arena transport reports"
    );
    assert_eq!(
        first_array,
        Some(vec![7, 8, 9]),
        "and the payload arrived, read out of the frame"
    );

    channel.stop();
}

/// `plugin::wire` is what the daemon calls, and it must bind the configured socket.
///
/// # What this closes
///
/// Every piece of the channel worked and nothing called it: `Channel::start` and
/// `Facility::load` were reachable only from `start_with_plugin`, which only tests
/// used. An operator setting `plugin.channel_socket` got no listener, and a peer
/// connecting got `ENOENT` -- with the commit log saying the channel was "wired into
/// yesnod", which was true of the lifecycle functions and false of the daemon.
///
/// So this drives the same entry point `main.rs` does, and connects to prove the
/// socket is real rather than merely configured.
#[test]
fn the_daemon_wiring_binds_the_configured_socket() {
    let (_c, cfg, _host, sock, _slot) = setup("wire");
    assert!(!sock.exists(), "nothing is bound before wiring");

    let wiring = yesno_server::plugin::wire(&cfg, Role::Leader)
        .expect("wiring a configured channel must succeed")
        .expect("a configured channel must produce wiring");
    let channel = wiring.channel.expect("the channel must be started");
    assert!(sock.exists(), "the configured socket must be bound");

    // The slot it handed back is the one a startup path must fill. Empty for now,
    // which is exactly why a peer would see UNAVAILABLE until the server fills it.
    assert!(
        wiring.slot.read().unwrap().is_none(),
        "the slot is the caller's to fill"
    );

    // A peer can reach it, which is the property the gap denied.
    let mut peer = Peer::connect(&sock);
    match peer.read_frame() {
        Frame::ServerHello { .. } => {}
        other => panic!("{other:?}"),
    }
    assert_eq!(channel.peers(), 1);
    channel.stop();
    assert!(!sock.exists(), "and stopping removes it");
}

/// Neither shape configured means no wiring and no socket.
#[test]
fn the_daemon_wiring_is_absent_when_nothing_is_configured() {
    let (_c, mut cfg, _host, sock, _slot) = setup("wire-off");
    cfg.plugin.channel_socket = String::new();
    // SAFETY: nothing is configured, so nothing is loaded.
    let wiring = yesno_server::plugin::wire(&cfg, Role::Leader).unwrap();
    assert!(wiring.is_none(), "an unconfigured node wires nothing");
    assert!(!sock.exists());
}

/// A plugin on a cold standby is refused rather than started.
///
/// A standby without `follower.serve_reads` never opens a database, so a channel on
/// one would bind, accept peers, and answer `UNAVAILABLE` to every request for the
/// life of the process -- which reads as a broken peer rather than a
/// misconfiguration. Refusing at startup puts the error where the mistake is.
#[test]
fn a_plugin_on_a_cold_standby_is_refused() {
    let (_c, mut cfg, _host, sock, _slot) = setup("wire-cold");
    cfg.follower.serve_reads = false;
    // SAFETY: the call refuses before loading anything.
    let e = yesno_server::plugin::wire(&cfg, Role::Follower);
    let msg = match e {
        Err(m) => m,
        Ok(_) => panic!("a plugin on a cold standby must be refused"),
    };
    assert!(
        msg.contains("serve_reads"),
        "the message must name what to change: {msg}"
    );
    assert!(!sock.exists(), "and nothing is bound on the way out");

    // With reads enabled, the same configuration wires.
    cfg.follower.serve_reads = true;
    // SAFETY: as above.
    let w = yesno_server::plugin::wire(&cfg, Role::Follower)
        .unwrap()
        .unwrap();
    assert!(sock.exists());
    w.channel.unwrap().stop();
}

/// A rebootstrap takes a peer's snapshots back, rather than asking for them.
///
/// **This is the bug a security review found on 2026-09-29** ( item 1 ).
/// `Listeners::before_close` announced `Unavailable` and stopped there, so a peer
/// holding a `Snapshot` -- which holds `Arc<DbInner>`, which holds the directory
/// lock -- kept that lock alive for as long as it felt like. The follower then
/// dropped its `Arc<Db>` and `open_if_needed` failed `AlreadyOpen`, surfacing
/// seconds later as an error naming nothing.
///
/// The peer here is deliberately **silent**: it opens a snapshot and then never
/// reads another byte, which is exactly the case an announcement cannot reach. The
/// listener must stay up throughout, because the contract is that a peer
/// reconnects on its own and learns the new generation from its greeting.
#[test]
fn a_rebootstrap_disconnects_a_peer_that_ignores_the_announcement() {
    let (_c, cfg, host, sock, _slot) = setup("revoke");
    let channel = Channel::start(&cfg, host.clone()).unwrap().unwrap();
    let db = host.db().unwrap();

    let mut peer = Peer::connect(&sock);
    assert!(matches!(peer.read_frame(), Frame::ServerHello { .. }));
    peer.ask(Frame::ClientHello {
        protocol: yesno_plugin::ipc::VERSION as u32,
        name: "silent".into(),
    });
    peer.ask(Frame::SnapshotOpen);
    assert_eq!(db.live_readers(), 1, "the peer's snapshot is registered");

    // What a follower does before dropping its database to rebuild it.
    yesno_server::plugin::Listeners {
        channel: Some(&channel),
    }
    .before_close();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while db.live_readers() > 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(
        db.live_readers(),
        0,
        "announcing a close does not release a snapshot; disconnecting does, and a \
         reopen cannot wait on a peer's goodwill"
    );
    assert!(
        sock.exists(),
        "the listener stays up: a peer must be able to reconnect after the rebuild"
    );
    // And it can.
    let mut again = Peer::connect(&sock);
    assert!(matches!(again.read_frame(), Frame::ServerHello { .. }));

    drop(peer);
    channel.stop();
}

/// A live socket is never clobbered, and a non-socket path is refused outright.
///
/// **Item 5 of the 2026-09-29 security review.** `start` unconditionally unlinked
/// its configured path, justified by "the database lock already proves no other
/// yesnod holds this directory" -- an argument that does not cover the case that
/// matters: two instances with *different* data directories and the same socket
/// path. Each holds its own lock, so neither is refused, and the second silently
/// redirects new peers to a different database while the first's existing peers
/// carry on. Nobody observes the split, because both sides look healthy.
#[test]
fn a_second_channel_refuses_to_take_over_a_live_socket() {
    let (_c, cfg, host, sock, _slot) = setup("clobber");
    let first = Channel::start(&cfg, host.clone()).unwrap().unwrap();
    assert!(sock.exists());

    let e = match Channel::start(&cfg, host.clone()) {
        Err(e) => e,
        Ok(_) => panic!("binding over a live socket must be refused, not silently taken"),
    };
    assert_eq!(e.kind(), std::io::ErrorKind::AddrInUse);
    assert!(
        format!("{e}").contains("already accepting"),
        "the message must say why: {e}"
    );

    // The first channel is untouched and still serving.
    let mut peer = Peer::connect(&sock);
    assert!(matches!(peer.read_frame(), Frame::ServerHello { .. }));
    drop(peer);
    first.stop();

    // A leftover socket from a dead process is not a live one, and is reclaimed.
    let stale = Channel::start(&cfg, host.clone()).unwrap().unwrap();
    stale.stop();

    // A path that exists and is not a socket is refused rather than deleted.
    std::fs::write(&sock, b"not a socket").unwrap();
    let e = match Channel::start(&cfg, host) {
        Err(e) => e,
        Ok(_) => panic!("a regular file must not be unlinked"),
    };
    assert_eq!(e.kind(), std::io::ErrorKind::AlreadyExists);
    assert!(
        sock.exists(),
        "and it must still be there: refusing means not deleting"
    );
    let _ = std::fs::remove_file(&sock);
}

/// The socket takes the configured mode.
///
/// Item 4's filesystem half. Without it the socket lands at the umask, which on a
/// group-writable mount admits more than the deployment intends -- and the socket
/// carries no authentication of its own, so whoever the mode admits can read the
/// whole database.
#[test]
fn the_channel_socket_takes_the_configured_mode() {
    use std::os::unix::fs::PermissionsExt as _;
    let (_c, mut cfg, host, sock, _slot) = setup("mode");
    cfg.plugin.channel_socket_mode = "0600".into();
    let channel = Channel::start(&cfg, host).unwrap().unwrap();
    let mode = std::fs::metadata(&sock).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "the socket must carry the configured mode");
    channel.stop();
}

/// Connections past the cap are refused, and refusing one frees room for the next.
///
/// Item 3's admission half. Each peer costs a thread and its own arena, so an
/// uncapped accept loop lets one peer multiply those by reconnecting.
#[test]
fn peers_past_the_configured_limit_are_refused() {
    let (_c, mut cfg, host, sock, _slot) = setup("cap");
    cfg.plugin.channel_max_peers = 2;
    let channel = Channel::start(&cfg, host).unwrap().unwrap();

    let mut admitted = Vec::new();
    for _ in 0..2 {
        let mut p = Peer::connect(&sock);
        assert!(matches!(p.read_frame(), Frame::ServerHello { .. }));
        admitted.push(p);
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while channel.peers() < 2 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(channel.peers(), 2, "both peers are being served");
    assert_eq!(channel.admitted(), 2, "and both hold an admission slot");

    // The third connects -- the listener still accepts -- but is closed without a
    // greeting, which is what a refusal looks like from the peer's side.
    let mut refused = std::os::unix::net::UnixStream::connect(&sock).unwrap();
    refused
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    let mut byte = [0u8; 1];
    assert_eq!(
        std::io::Read::read(&mut refused, &mut byte).unwrap(),
        0,
        "a refused peer must see EOF at once, not a connection that never answers"
    );
    assert_eq!(channel.admitted(), 2, "and it was never counted");

    // Room reopens when one leaves. Waited on the **admission** count, not the
    // registry: a connection leaves the registry just before its thread ends and
    // releases its slot, so waiting on `peers` can race ahead of the capacity.
    admitted.pop();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while channel.admitted() > 1 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(channel.admitted(), 1, "the slot came back");
    let mut p = Peer::connect(&sock);
    assert!(
        matches!(p.read_frame(), Frame::ServerHello { .. }),
        "the cap is a limit on concurrency, not a permanent budget"
    );
    drop(p);
    drop(admitted);
    channel.stop();
}

/// A peer that arrives during the cutover cannot reach the old database.
///
/// **Item 1 of the re-review.** Disconnecting the registered peers and *then*
/// taking the slot leaves a gap, because the listener stays up by design: a
/// connection arriving in between is admitted, finds the database still in the
/// slot, opens a snapshot and holds its lock -- reintroducing the `AlreadyOpen`
/// the disconnect exists to prevent, on a peer that arrived microseconds early.
///
/// **This pins the invariant the fix rests on, not the call order itself**, and
/// the distinction is worth stating: it asserts that once the slot is empty every
/// peer is refused a snapshot, whether already connected or just arrived. Nothing
/// here would catch someone reversing the two statements in `close_for_rebuild`
/// again, because that function is private to the follower and driving it needs a
/// replication harness. Reading it is the only check on the order -- so the
/// comment there carries the argument, and this carries the property that makes
/// the argument sound.
#[test]
fn a_peer_arriving_during_a_cutover_cannot_open_a_snapshot() {
    let (_c, cfg, host, sock, slot) = setup("cutover");
    let channel = Channel::start(&cfg, host.clone()).unwrap().unwrap();

    // What `close_for_rebuild` now does first.
    let taken = slot.write().unwrap().take();
    assert!(taken.is_some(), "the fixture had a database to take");

    let mut late = Peer::connect(&sock);
    assert!(matches!(late.read_frame(), Frame::ServerHello { .. }));
    late.ask(Frame::ClientHello {
        protocol: yesno_plugin::ipc::VERSION as u32,
        name: "late".into(),
    });
    match late.ask(Frame::SnapshotOpen) {
        Frame::Fault { status, .. } => assert_eq!(
            status,
            yesno_plugin::abi::Status::Unavailable as u32,
            "a peer admitted after the cutover must be told the database is gone"
        ),
        other => panic!("it must not get a snapshot against the old database: {other:?}"),
    }
    drop(late);
    channel.stop();
    drop(taken);
}

/// The cap holds when peers arrive together, not only one at a time.
///
/// **Item 3 of the re-review, and the reason it names the old test.** Admission
/// read `peers.len()`, which a connection joins only after its arena is built --
/// so simultaneous connects all saw room and all allocated. A test that waits for
/// each peer to register before opening the next cannot reach that, which is
/// exactly what the existing one does. This one races them deliberately.
#[test]
fn simultaneous_connects_cannot_exceed_the_peer_cap() {
    let (_c, mut cfg, host, sock, _slot) = setup("caprace");
    cfg.plugin.channel_max_peers = 3;
    let channel = Channel::start(&cfg, host).unwrap().unwrap();

    let gate = Arc::new(std::sync::Barrier::new(12));
    let threads: Vec<_> = (0..12)
        .map(|_| {
            let sock = sock.clone();
            let gate = gate.clone();
            std::thread::spawn(move || {
                gate.wait();
                std::os::unix::net::UnixStream::connect(&sock).ok()
            })
        })
        .collect();
    let held: Vec<_> = threads
        .into_iter()
        .filter_map(|t| t.join().unwrap())
        .collect();

    // Whatever the interleaving, the channel never admitted more than the cap.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut worst = 0usize;
    while std::time::Instant::now() < deadline {
        worst = worst.max(channel.admitted());
        assert!(
            channel.admitted() <= 3,
            "admitted {} with a cap of 3: the check and the spawn are not atomic",
            channel.admitted()
        );
        if channel.peers() == 3 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(worst <= 3, "peak admitted was {worst}");
    drop(held);
    channel.stop();
}

/// A peer writes and reads over **one real socket**, which is the point of the change.
///
/// The session tests call `Session::handle` directly and so prove the semantics; this proves
/// the wiring. It matters separately because the frame has to survive encoding, the socket's
/// bounded writer, and the generic serving loop -- and because the claim being made is that
/// a consumer can bind this socket and need nothing else, which is a statement about the
/// socket rather than about the session object.
///
/// The read-back goes through the same connection, so a passing run demonstrates both
/// directions on one descriptor.
#[test]
fn a_peer_writes_and_reads_over_one_socket() {
    let (_c, cfg, host, sock, _slot) = setup("rw");
    let channel = Channel::start(&cfg, host.clone()).unwrap().unwrap();

    let mut peer = Peer::connect(&sock);
    assert!(matches!(peer.read_frame(), Frame::ServerHello { .. }));
    peer.ask(Frame::ClientHello {
        protocol: yesno_plugin::ipc::VERSION as u32,
        name: "writer".into(),
    });

    use yesno_plugin::ipc::{Write, WriteOp};
    let version = match peer.ask(Frame::Apply {
        writes: vec![
            Write {
                key: 77,
                lo: 1,
                hi: 1,
                op: WriteOp::Insert,
            },
            Write {
                key: 77,
                lo: 1_000,
                hi: 1_010,
                op: WriteOp::InsertRange,
            },
        ],
    }) {
        Frame::Committed { version, changed } => {
            assert_eq!(changed, 12, "one ordinal plus eleven in the range");
            version
        }
        other => panic!("expected Committed, got {other:?}"),
    };

    // A snapshot opened *after* the commit must see it, and its version must not predate
    // the write -- which is what makes the two directions usable together rather than
    // merely both present.
    let snapshot = match peer.ask(Frame::SnapshotOpen) {
        Frame::SnapshotOpened {
            snapshot,
            version: v,
        } => {
            assert!(
                v >= version,
                "a snapshot taken after the commit must not predate it: {v} < {version}"
            );
            snapshot
        }
        other => panic!("{other:?}"),
    };
    assert_eq!(
        peer.ask(Frame::SnapshotCardinality { snapshot, key: 77 }),
        Frame::Count { value: 12 },
        "the write is visible to a read on the same socket"
    );

    // And it is durable in the host's own database, not merely in the session's view.
    assert_eq!(
        host.db()
            .unwrap()
            .snapshot()
            .unwrap()
            .cardinality(77)
            .unwrap(),
        12
    );

    channel.stop();
}

/// The widest legal `Apply` crosses a real socket, and one over is refused before mutating.
///
/// # Why a socket test as well as a codec test
///
/// The codec test proves the frame encodes and decodes at `MAX_WRITES`. This proves the
/// whole path carries it: a peer's encoder, the socket, `read_frame`'s incremental buffer --
/// which grows to the frame's real size rather than a declared one -- the server's
/// admission against the advertised cap, and one commit at the far end. The defect a
/// consumer reported was visible only at the limit, and only end to end.
///
/// The over-limit half asserts the database is untouched, because "refused" has to mean
/// nothing happened and not merely that an error came back.
#[test]
fn the_widest_legal_apply_crosses_the_socket_and_one_over_is_refused() {
    use yesno_plugin::ipc::{Write, WriteOp, MAX_WRITES};

    let (_c, cfg, host, sock, _slot) = setup("widest");
    let channel = Channel::start(&cfg, host.clone()).unwrap().unwrap();

    let mut peer = Peer::connect(&sock);
    let advertised = match peer.read_frame() {
        Frame::ServerHello { max_writes, .. } => {
            assert_eq!(
                max_writes as usize, MAX_WRITES,
                "the greeting must advertise the cap this build enforces"
            );
            max_writes as usize
        }
        other => panic!("{other:?}"),
    };
    peer.ask(Frame::ClientHello {
        protocol: yesno_plugin::ipc::VERSION as u32,
        name: "widest".into(),
    });

    // Distinct ordinals, so the committed cardinality is the entry count exactly and a
    // batch that silently dropped or duplicated entries would be visible.
    let writes: Vec<Write> = (0..advertised as u64)
        .map(|i| Write {
            key: 300,
            lo: i * 3,
            hi: i * 3,
            op: WriteOp::Insert,
        })
        .collect();
    match peer.ask(Frame::Apply {
        writes: writes.clone(),
    }) {
        Frame::Committed { changed, .. } => assert_eq!(
            changed as usize, advertised,
            "every entry of the widest legal frame must have been applied"
        ),
        other => panic!("the widest legal Apply must be accepted, got {other:?}"),
    }
    assert_eq!(
        host.db()
            .unwrap()
            .snapshot()
            .unwrap()
            .cardinality(300)
            .unwrap() as usize,
        advertised,
        "one commit, every ordinal"
    );

    // One over. The peer's own encoder refuses it, which is the first line of defence and
    // the one the consumer actually hit -- so assert on that rather than only on the server.
    let mut over = writes;
    over.push(Write {
        key: 300,
        lo: u32::MAX as u64,
        hi: u32::MAX as u64,
        op: WriteOp::Insert,
    });
    assert_eq!(
        Frame::Apply { writes: over }.encode(),
        Err(yesno_plugin::ipc::IpcError::TooLarge),
        "a frame past the cap must fail at encode, before the socket"
    );
    assert_eq!(
        host.db()
            .unwrap()
            .snapshot()
            .unwrap()
            .cardinality(300)
            .unwrap() as usize,
        advertised,
        "and the database must be untouched by the attempt"
    );

    channel.stop();
}

/// A server configured below the protocol ceiling advertises and enforces *its* number.
///
/// The bug was an advertised limit that was not the enforced one. The fix has to hold in
/// both directions, so this configures a small cap and checks three things agree: what the
/// greeting says, what is accepted at that figure, and what is refused one past it. Without
/// this, enforcing `MAX_WRITES` while advertising a lower number would pass every other
/// test here.
#[test]
fn a_configured_write_cap_is_the_one_advertised_and_enforced() {
    use yesno_plugin::ipc::{Write, WriteOp};

    let (_c, mut cfg, host, sock, _slot) = setup("writecap");
    cfg.plugin.channel_max_writes = 64;
    let channel = Channel::start(&cfg, host.clone()).unwrap().unwrap();

    let mut peer = Peer::connect(&sock);
    match peer.read_frame() {
        Frame::ServerHello { max_writes, .. } => assert_eq!(max_writes, 64),
        other => panic!("{other:?}"),
    }
    peer.ask(Frame::ClientHello {
        protocol: yesno_plugin::ipc::VERSION as u32,
        name: "capped".into(),
    });

    let batch = |n: u64| Frame::Apply {
        writes: (0..n)
            .map(|i| Write {
                key: 301,
                lo: i,
                hi: i,
                op: WriteOp::Insert,
            })
            .collect(),
    };
    match peer.ask(batch(64)) {
        Frame::Committed { changed, .. } => assert_eq!(changed, 64),
        other => panic!("the advertised figure must be accepted, got {other:?}"),
    }
    // One past it is refused by the server -- the frame is well under the protocol cap, so
    // nothing but the configured limit can reject it.
    match peer.ask(batch(65)) {
        Frame::Fault { status, message } => {
            assert_eq!(status, yesno_plugin::abi::Status::InvalidArgument as u32);
            assert!(
                message.contains("65") && message.contains("64"),
                "the refusal must name both figures, got {message:?}"
            );
        }
        other => panic!("a batch past the configured cap must be refused, got {other:?}"),
    }
    assert_eq!(
        host.db()
            .unwrap()
            .snapshot()
            .unwrap()
            .cardinality(301)
            .unwrap(),
        64,
        "the refused batch staged nothing"
    );

    channel.stop();
}
