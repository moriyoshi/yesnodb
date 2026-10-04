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
        max_writes: yesno_plugin::ipc::MAX_WRITES,
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

/// The peer waits through `UNAVAILABLE` instead of exiting.
///
/// **This is the behaviour the channel's own documentation demands of every peer
/// and the example did not have.** `yesnod` binds the socket before it opens the
/// database, so a peer started alongside it -- a sidecar, which is the whole
/// point -- connects and is refused by every request until startup completes.
/// Exiting on that is a crash loop with backoff, leaving the peer least likely to
/// be running at the moment the database becomes usable.
///
/// The server here starts with an **empty slot**, which is exactly what `Host`
/// reports while a database is closed or rebuilding, and fills it only after the
/// peer has already connected and been refused at least once.
#[test]
fn a_peer_waits_for_a_database_that_is_not_open_yet() {
    let mut dir = std::env::temp_dir();
    dir.push(format!("yesno-peerwait-{}", std::process::id()));
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
    for i in 0..3u64 {
        b.insert(10, 7 + i);
    }
    b.commit().unwrap();
    db.checkpoint().unwrap();
    let db = Arc::new(db);

    // Deliberately empty: every request will answer UNAVAILABLE until it is
    // filled, which is what a peer meets on a cold start.
    let slot: Arc<RwLock<Option<Arc<Db>>>> = Arc::new(RwLock::new(None));
    let sock = dir.join("peer.sock");
    let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    let host = Host::new(slot.clone(), 1, Role::Leader);
    let limits = Limits {
        max_handles: 2,
        max_lanes: 2,
        max_blocks: 4,
        max_snapshots: 8,
        max_writes: yesno_plugin::ipc::MAX_WRITES,
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

    let child = std::process::Command::new(peer_binary())
        .arg(&sock)
        .arg("10")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("the peer binary must run");

    // Let it connect and be refused before the database appears. A peer that
    // exits on the first refusal is already gone by now.
    std::thread::sleep(std::time::Duration::from_millis(600));
    *slot.write().unwrap() = Some(db);

    let out = child.wait_with_output().expect("reap");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "the peer must survive a cold start: {stdout}"
    );
    assert_eq!(
        stdout.trim(),
        "cardinality 3",
        "and then read the database that arrived while it waited"
    );
    drop(clean);
}

/// A watching peer sees a write that landed after it started.
///
/// **The defect this pins cost a full Kubernetes gate run.** A snapshot is a point
/// in time, so a peer that samples once at startup reports the database as it was
/// before anything was ingested -- and a sidecar is started *with* its cluster, so
/// that is always before. The operator fixture asserted a cardinality such a peer
/// could never reach, and read as correct because zero is also what an empty
/// database returns. Nothing about that needs Kubernetes to demonstrate.
///
/// It also proves the round closes what it opened: `max_snapshots` is 8 here, so a
/// `Watch` that leaked a snapshot per round would start faulting within seconds.
#[test]
fn a_watching_peer_sees_a_write_that_came_after_it_started() {
    let (_c, sock, db) = serve("watch", &[10]);
    let mut child = std::process::Command::new(peer_binary())
        .arg("--watch")
        .arg(&sock)
        .arg("10")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("the peer binary must run");
    // Read on a thread and receive with a timeout. Reading inline would block in
    // `next()` forever once the peer stopped announcing, so the very failure this
    // test exists to catch would arrive as a hung gate rather than as a red test.
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let out = child.stdout.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(out).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    fn announced(rx: &std::sync::mpsc::Receiver<String>) -> String {
        rx.recv_timeout(std::time::Duration::from_secs(30))
            .expect("the peer must keep announcing")
    }

    // The first announcement predates the write. Asserting it exactly is what makes
    // the second assertion mean something: if the peer already reported 14 here, the
    // test would pass without ever re-opening anything.
    let first = announced(&rx);
    assert_eq!(
        first, "holding 9",
        "the nine ordinals `serve` wrote under key 10"
    );

    let mut b = db.batch();
    for i in 1..=5u64 {
        b.insert(10, 9 * 65536 + i);
    }
    b.commit().unwrap();
    db.checkpoint().unwrap();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut last = first;
    while std::time::Instant::now() < deadline && last != "holding 14" {
        last = announced(&rx);
    }
    child.kill().expect("kill");
    child.wait().expect("reap");
    assert_eq!(
        last, "holding 14",
        "a watching peer must re-open a snapshot and see the later write"
    );
}

/// A peer in its own process **writes**, and the host's database has the ordinals.
///
/// # Why this is worth a second process
///
/// `yesno-server`'s `a_peer_writes_and_reads_over_one_socket` already proves the frame
/// survives a real socket. This proves the *worked example* does, which is a different
/// claim: `yesno-channel-peer` is the artefact a consumer is pointed at, and until
/// 2026-10-04 it could only scan -- so anyone modelling a peer on it would have concluded
/// the socket cannot write and reached for a second transport. A broken example is worse
/// than none, because it is copied.
///
/// The assertion is against the **host's** database rather than the peer's output, so a peer
/// that printed a plausible version without committing anything would fail.
#[test]
fn a_separate_process_writes_and_the_host_sees_it() {
    let (_c, sock, db) = serve("write", &[10]);
    let before = db.snapshot().unwrap().cardinality(55).unwrap();
    assert_eq!(before, 0, "key 55 is untouched by the fixture");

    let out = std::process::Command::new(peer_binary())
        .arg("--write")
        .arg(&sock)
        .arg("55")
        .args(["7", "11", "13"])
        .output()
        .expect("the peer binary must run");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "peer failed: {}{}",
        stdout,
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("committed version") && stdout.contains("changed 3"),
        "the peer must report what it committed, got {stdout:?}"
    );
    // The scan it runs afterwards is over the key it just wrote, on the same connection.
    assert!(
        stdout.contains("cardinality 3"),
        "a snapshot opened after the commit must see it, got {stdout:?}"
    );

    assert_eq!(
        db.snapshot().unwrap().cardinality(55).unwrap(),
        3,
        "the host's database must hold what another process wrote"
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while db.live_readers() > 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(db.live_readers(), 0, "and it released its snapshot on exit");
}

/// The widest legal `Apply` crosses a **real process boundary** in one commit.
///
/// The server-side test proves the socket carries it; this proves a separate program can
/// build and send it, which is what the consumer was actually doing when it hit the old
/// cap. Their failure was in the peer's own encoder before a byte was sent, so the half of
/// the path that broke lives on this side of the socket and only a separate process
/// exercises it as deployed.
#[test]
fn a_separate_process_sends_the_widest_legal_apply() {
    let (_c, sock, db) = serve("widest", &[10]);
    let n = yesno_plugin::ipc::MAX_WRITES;

    let out = std::process::Command::new(peer_binary())
        .arg("--write-n")
        .arg(&sock)
        .arg("900")
        .arg(n.to_string())
        .output()
        .expect("the peer binary must run");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "peer failed: {}{}",
        stdout,
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains(&format!("changed {n}")),
        "every entry must be committed, got {stdout:?}"
    );
    assert_eq!(
        db.snapshot().unwrap().cardinality(900).unwrap() as usize,
        n,
        "the host must hold all {n} ordinals a separate process sent in one frame"
    );
}
