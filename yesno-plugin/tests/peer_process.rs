//! The protocol across a **real process boundary**.
//!
//! # What this covers that nothing else does
//!
//! `yesno-server`'s channel tests use a real socket, a real `SCM_RIGHTS` handoff
//! and a real mapping -- I claimed otherwise on 2026-09-29 and it was wrong --
//! but they run inside one test binary. Three things only a second process can
//! show, and the third is the one the design rests on:
//!
//! * an arena **mapped by another process**, rather than by the one that created
//!   the `memfd`;
//! * a peer built and run as its own program, which is what a deployment actually
//!   does;
//! * a peer that is **killed**. Liveness here is "the socket closed", and a peer
//!   that exits cleanly proves almost nothing about that: its `Drop` runs, its
//!   buffers flush, it says goodbye by accident. `SIGKILL` runs no code at all,
//!   so what releases the snapshot is the kernel closing the descriptor -- which
//!   is the entire claim.
//!
//! The deleted C fixture used to prove a fourth property for the in-process ABI,
//! that a plugin cannot link `yesno-core`. Nothing proves that here: this peer is
//! a Rust binary in the same workspace and does link it. `ipc.rs` has **no**
//! imports at all, so a peer needs nothing but the protocol, and demonstrating
//! that properly means splitting that module into its own crate -- a file move
//! `ARCHITECTURE.md` already anticipates, and not this test's job.

use std::io::{BufRead, BufReader};
use std::sync::{Arc, RwLock};

use yesno_core::{Db, DbOptions};
use yesno_plugin::abi::Role;
use yesno_plugin::channel::{send_fd, serve_locked, Arena, Limits, Session};
use yesno_plugin::Host;

struct Clean(std::path::PathBuf);
impl Drop for Clean {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A database, a listening socket, and a thread serving one peer at a time.
///
/// Deliberately not `yesno-server`'s `Channel`: this crate owns the protocol and
/// should be able to demonstrate it without the server, which is the same reason
/// `Session::handle` is a function rather than a socket loop.
fn serve(tag: &str, keys: &[u64]) -> (Clean, std::path::PathBuf, Arc<Db>) {
    let mut dir = std::env::temp_dir();
    dir.push(format!("yesno-peerproc-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
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
    for c in [0u64, 1, 5] {
        for i in 0..3u64 {
            b.insert(10, c * 65536 + 7 + i);
        }
    }
    // A run with a non-zero start, so the peer's `[ start, end ]` decoding is
    // exercised rather than assumed: stored it is ( 1000, 4096 ).
    b.insert_range(30, 3 * 65536 + 1000, 3 * 65536 + 5096);
    b.commit().unwrap();
    db.checkpoint().unwrap();
    let db = Arc::new(db);

    let sock = dir.join("peer.sock");
    let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    let host = Host::new(Arc::new(RwLock::new(Some(db.clone()))), 1, Role::Leader);
    let limits = Limits {
        max_handles: 2,
        max_lanes: keys.len().max(1),
        max_blocks: 4,
        max_snapshots: 8,
    };
    std::thread::spawn(move || {
        for incoming in listener.incoming() {
            let Ok(stream) = incoming else { break };
            let host = host.clone();
            std::thread::spawn(move || {
                let Ok(arena) = Arena::new(limits.arena_bytes()) else {
                    return;
                };
                if send_fd(&stream, arena.as_fd()).is_err() {
                    return;
                }
                let mut session = Session::new(host, arena, limits);
                let writer = std::sync::Mutex::new(stream.try_clone().unwrap());
                let _ = serve_locked(&mut session, stream, &writer);
            });
        }
    });
    (clean, sock, db)
}

fn peer_binary() -> &'static str {
    env!("CARGO_BIN_EXE_yesno-channel-peer")
}

/// A peer in its own process maps the arena and reads the right answer.
#[test]
fn a_separate_process_scans_through_the_arena() {
    let (_c, sock, db) = serve("scan", &[10, 30]);
    let out = std::process::Command::new(peer_binary())
        .arg(&sock)
        .arg("10")
        .arg("30")
        .output()
        .expect("the peer binary must run");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "peer failed: {}{}",
        stdout,
        String::from_utf8_lossy(&out.stderr)
    );

    // 9 ordinals under key 10, and 4097 in the run under key 30.
    assert_eq!(
        stdout.trim(),
        "cardinality 4106",
        "the arena was mapped by another process and read wrong"
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while db.live_readers() > 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(db.live_readers(), 0, "and it released its snapshot on exit");
}

/// A peer that is **killed** still releases its snapshot.
///
/// The one that matters. `SIGKILL` runs no destructor, flushes no buffer and
/// sends no goodbye, so nothing the peer contains can be what releases the
/// snapshot: the kernel closes its descriptor, the server's read returns zero,
/// the session drops. That is the whole liveness argument, and until this test it
/// had never been made against a process that could not cooperate.
#[test]
fn a_killed_peer_releases_its_snapshot() {
    let (_c, sock, db) = serve("killed", &[10]);
    let mut child = std::process::Command::new(peer_binary())
        .arg("--hold")
        .arg(&sock)
        .arg("10")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("the peer binary must run");

    // Wait for it to say the snapshot is open, so the kill lands on a peer that
    // is actually holding something rather than one still starting.
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let holding = lines
        .next()
        .expect("the peer must announce that it is holding")
        .expect("readable");
    assert!(holding.starts_with("holding"), "got {holding:?}");
    assert_eq!(db.live_readers(), 1, "the peer's snapshot is registered");

    child.kill().expect("kill");
    child.wait().expect("reap");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while db.live_readers() > 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(
        db.live_readers(),
        0,
        "a killed peer ran no code, so only the kernel closing its socket can \
         have released this -- which is the whole design"
    );
}
