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
