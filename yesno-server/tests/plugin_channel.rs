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
        assert_eq!(version, 1, "the handoff byte carries the protocol version");
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

fn setup(tag: &str) -> (Clean, Config, Host, std::path::PathBuf) {
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
    let host = Host::new(slot, 1, Role::Leader);
    (clean, cfg, host, sock)
}

/// A peer connects, is handed the arena, and reads a scan out of it.
#[test]
fn a_peer_scans_through_the_socket_and_the_arena() {
    let (_c, cfg, host, sock) = setup("scan");
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
            protocol: 1,
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
                                .chunks_exact(2)
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

/// Notifications reach a connected peer without it asking.
///
/// A peer that had to poll for a replacement would serve answers from a database
/// that no longer exists between polls, which is why these are pushed.
#[test]
fn a_connected_peer_is_told_about_availability_without_asking() {
    let (_c, cfg, host, sock) = setup("notify");
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
    let (_c, cfg, host, sock) = setup("vanish");
    let channel = Channel::start(&cfg, host.clone()).unwrap().unwrap();
    let db = host.db().unwrap();
    assert_eq!(db.live_readers(), 0);

    let mut peer = Peer::connect(&sock);
    assert!(matches!(peer.read_frame(), Frame::ServerHello { .. }));
    peer.ask(Frame::ClientHello {
        protocol: 1,
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
    let (_c, cfg, host, sock) = setup("stop");
    let channel = Channel::start(&cfg, host.clone()).unwrap().unwrap();
    let db = host.db().unwrap();

    let mut peer = Peer::connect(&sock);
    assert!(matches!(peer.read_frame(), Frame::ServerHello { .. }));
    peer.ask(Frame::ClientHello {
        protocol: 1,
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
    let (_c, mut cfg, host, _sock) = setup("off");
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
    let (_c, mut cfg, host, sock) = setup("inline");
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
            protocol: 1,
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
                                .chunks_exact(2)
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
    let (_c, cfg, _host, sock) = setup("wire");
    assert!(!sock.exists(), "nothing is bound before wiring");

    // SAFETY: no library is configured, so nothing is loaded; only the channel starts.
    let wiring = unsafe { yesno_server::plugin::wire(&cfg, Role::Leader) }
        .expect("wiring a configured channel must succeed")
        .expect("a configured channel must produce wiring");
    assert!(
        wiring.facility.is_none(),
        "no library configured, so no in-process facility"
    );
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
    let (_c, mut cfg, _host, sock) = setup("wire-off");
    cfg.plugin.channel_socket = String::new();
    // SAFETY: nothing is configured, so nothing is loaded.
    let wiring = unsafe { yesno_server::plugin::wire(&cfg, Role::Leader) }.unwrap();
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
    let (_c, mut cfg, _host, sock) = setup("wire-cold");
    cfg.follower.serve_reads = false;
    // SAFETY: the call refuses before loading anything.
    let e = unsafe { yesno_server::plugin::wire(&cfg, Role::Follower) };
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
    let w = unsafe { yesno_server::plugin::wire(&cfg, Role::Follower) }
        .unwrap()
        .unwrap();
    assert!(sock.exists());
    w.channel.unwrap().stop();
}
