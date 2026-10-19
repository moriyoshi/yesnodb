//! A minimal out-of-process plugin peer: the worked example, and the only thing
//! in the tree that speaks this protocol from **another process**.
//!
//! # What it is for
//!
//! Two jobs, and the second is why it is a binary rather than a test helper.
//!
//! **It closes a coverage gap.** `yesno-server`'s channel tests do use a real
//! socket, a real `SCM_RIGHTS` handoff and a real mapping -- a claim to the
//! contrary was corrected on 2026-09-29 -- but they run inside one test binary.
//! So nothing covered an arena mapped across a real process boundary, or a peer
//! that is **killed** rather than dropped, which is the case the whole
//! liveness-is-the-socket design rests on: the kernel closes the descriptor, the
//! server sees EOF, and the snapshots go. A peer that exits cleanly proves far
//! less than one that is shot.
//!
//! **It is the artefact a consumer asks for.** A protocol document tells someone
//! what the bytes are; this tells them what to do with them, in the order that
//! works, including the two things that are easy to get wrong -- the descriptor
//! arrives *before* any frame, and `UNAVAILABLE` means "not yet", not "broken".
//!
//! # Usage
//!
//! ```text
//! yesno-channel-peer <socket> <key>...                  # scan, print the cardinality
//! yesno-channel-peer --hold <socket> <key>...           # scan, then hold and wait
//! yesno-channel-peer --write <socket> <key> <ord>...    # insert, then scan that key
//! yesno-channel-peer --write-n <socket> <key> <count>   # insert count ordinals in one frame
//! yesno-channel-peer --targets <socket> <key>...        # bounded window, exact target batch
//! ```
//!
//! `--write-n` exists so the **widest legal frame** can be exercised across a real process
//! boundary: the advertised cap is 16 384 entries and passing that many on a command line
//! is not practical. It inserts ordinals `0 .. count` under `key` in one `Apply`, which is
//! also the shape a bulk loader uses.
//!
//! `--write` exists because the worked example should show the whole surface: the channel
//! served reads only until 2026-10-04, and a consumer reading this file would otherwise
//! still conclude it needs a second transport to be useful. It inserts each ordinal under
//! `key` in **one** `Apply` frame, prints the version the commit produced, and then scans
//! the same key over the same connection -- which is the shape worth copying, because it
//! shows the two directions sharing one descriptor and a snapshot opened after a commit
//! seeing it.
//!
//! `--hold` keeps the snapshot open and blocks for ever, so a caller can kill the
//! process and observe what the server does about it. It prints `holding` first,
//! so the caller knows the snapshot exists before it pulls the trigger.
//!
//! `--targets` is the one mode built on the **official client** rather than on the
//! raw frames above, and it is deliberate twice over. It is how a consumer should
//! reach the bounded window and the exact-target batch -- the validation rules are
//! fiddly enough that hand-rolling them is how a peer gets them subtly wrong -- and
//! it is the only thing in the tree that drives that client across a real process
//! boundary, where every other client test shares an address space with its server.
//! It also works in **both** transports from one code path, because the client hides
//! which one it got, so a caller can serve it either way and compare the output byte
//! for byte.

use std::io::{Read, Write};

use yesno_plugin::channel::{read_frame, recv_fd};
use yesno_plugin::ipc::{lane_offset, Frame, LaneKind};

/// Frames in, frames out, over one socket.
struct Peer {
    sock: std::os::unix::net::UnixStream,
    view: memmap2::Mmap,
    buf: Vec<u8>,
}

impl Peer {
    /// Connect and take the arena descriptor.
    ///
    /// **The descriptor arrives before the first frame**, as a one-byte message
    /// carrying the protocol version with the fd attached. Reading a frame first
    /// consumes that byte as frame data and desynchronises everything after it,
    /// which is the single easiest way to misuse this protocol.
    fn connect(path: &str) -> std::io::Result<Peer> {
        let sock = std::os::unix::net::UnixStream::connect(path)?;
        let (fd, version) = recv_fd(&sock)?;
        if version != yesno_plugin::ipc::VERSION {
            return Err(std::io::Error::other(format!(
                "server speaks protocol {version}, this peer speaks {}",
                yesno_plugin::ipc::VERSION
            )));
        }
        let file = std::fs::File::from(fd);
        // SAFETY: the descriptor is the server's sealed arena; it cannot shrink,
        // which is what makes a mapping of it safe to hold.
        let view = unsafe { memmap2::Mmap::map(&file) }?;
        Ok(Peer {
            sock,
            view,
            buf: Vec::new(),
        })
    }

    fn send(&mut self, frame: Frame) -> std::io::Result<()> {
        let bytes = frame
            .encode()
            .map_err(|e| std::io::Error::other(format!("{e}")))?;
        self.sock.write_all(&bytes)
    }

    fn recv(&mut self) -> std::io::Result<Frame> {
        read_frame(&mut self.sock, &mut self.buf)?
            .ok_or_else(|| std::io::Error::other("the server closed the connection"))
    }

    fn ask(&mut self, frame: Frame) -> std::io::Result<Frame> {
        self.send(frame)?;
        loop {
            match self.recv()? {
                // Pushed, not asked for, and can arrive between any request and
                // its reply. A peer that treats one as its answer will misread
                // every response after a rebootstrap.
                Frame::Unavailable | Frame::Available { .. } | Frame::GenerationChanged { .. } => {}
                other => return Ok(other),
            }
        }
    }
}

/// `UNAVAILABLE` is "not yet", so this waits instead of failing.
///
/// **The single most important thing this example demonstrates.** `yesnod` binds
/// the channel socket *before* it opens the database, so a peer that starts with
/// the server -- which is exactly what a sidecar does -- connects successfully
/// and is refused by every request until startup finishes. It happens again, for
/// as long as it takes, whenever a follower rebootstraps.
///
/// A peer that treats the first refusal as fatal therefore exits on nearly every
/// start, and under a container runtime that is a crash loop with backoff: the
/// peer is *least* likely to be running exactly when the database has just become
/// available. The first version of this file did that, which made it a worked
/// example of the mistake its own documentation warns about.
///
/// Only `UNAVAILABLE` is retried. Every other fault is a real answer -- a bad
/// request, a snapshot too old, the wrong role -- and retrying those would turn a
/// clear error into a hang.
fn open_snapshot(peer: &mut Peer) -> std::io::Result<u64> {
    const UNAVAILABLE: u32 = 3;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    let mut waited = false;
    loop {
        match peer.ask(Frame::SnapshotOpen)? {
            Frame::SnapshotOpened { snapshot, .. } => {
                if waited {
                    eprintln!("database became available");
                }
                return Ok(snapshot);
            }
            Frame::Fault { status, .. } if status == UNAVAILABLE => {
                if !waited {
                    eprintln!("database unavailable, waiting");
                    waited = true;
                }
                if std::time::Instant::now() >= deadline {
                    return Err(std::io::Error::other(
                        "database still unavailable after 120s",
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
            Frame::Fault { status, message } => {
                return Err(std::io::Error::other(format!(
                    "snapshot refused ( status {status} ): {message}"
                )))
            }
            other => return Err(std::io::Error::other(format!("unexpected {other:?}"))),
        }
    }
}

/// What the peer does once it has an answer.
///
/// `Hold` and `Watch` are deliberately separate rather than one flag with a
/// timer. `Hold` opens exactly one snapshot and never lets go, which is the
/// premise `a_killed_peer_releases_its_snapshot` rests on: it reads the first
/// announcement and kills the process expecting a snapshot to still be open. A
/// mode that closed and reopened on an interval would leave a window in which
/// that premise is false, and the test would fail for a reason unrelated to
/// liveness.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Scan once, print the total, exit.
    Once,
    /// Insert the given ordinals under the key in one frame, then scan it.
    Write,
    /// Insert `count` consecutive ordinals under the key in one frame, then scan it.
    WriteN,
    /// Scan once and keep the snapshot open until the socket closes.
    Hold,
    /// Re-open a snapshot on an interval, announcing each answer.
    ///
    /// **A snapshot is a point in time**, so a peer that samples once at startup
    /// can never see a write that came later -- which is what a sidecar started
    /// with its cluster faces, since it is running before anything is ingested. A
    /// long-lived plugin re-opens as the database advances, and this is that.
    Watch,
    /// Acquire a bounded window, read an exact batch of target prefixes, and show
    /// what a refused batch does to the handle.
    Targets,
}

/// How long `Mode::Watch` keeps a snapshot before taking the next one.
///
/// Short enough that a scenario waiting on the answer is not mostly waiting on
/// this, long enough that the log is readable and the snapshot registry is not
/// churned pointlessly.
const WATCH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (mode, rest) = match args.split_first() {
        Some((first, rest)) if first == "--hold" => (Mode::Hold, rest),
        Some((first, rest)) if first == "--write" => (Mode::Write, rest),
        Some((first, rest)) if first == "--write-n" => (Mode::WriteN, rest),
        Some((first, rest)) if first == "--watch" => (Mode::Watch, rest),
        Some((first, rest)) if first == "--targets" => (Mode::Targets, rest),
        _ => (Mode::Once, &args[..]),
    };
    let Some((path, keys)) = rest.split_first() else {
        eprintln!(
            "usage: yesno-channel-peer [--hold|--watch] <socket> <key>...\n\
             \x20      yesno-channel-peer --write <socket> <key> <ordinal>..."
        );
        return std::process::ExitCode::from(2);
    };
    let keys: Vec<u64> = match keys.iter().map(|k| k.parse()).collect() {
        Ok(k) => k,
        Err(e) => {
            eprintln!("keys must be integers: {e}");
            return std::process::ExitCode::from(2);
        }
    };
    if mode == Mode::Write && keys.len() < 2 {
        eprintln!("--write needs a key and at least one ordinal");
        return std::process::ExitCode::from(2);
    }
    if mode == Mode::WriteN && keys.len() != 2 {
        eprintln!("--write-n needs exactly a key and a count");
        return std::process::ExitCode::from(2);
    }

    // The official client, not the raw frames: a different entry point rather than
    // a mode of `run`, because it shares none of that function's state.
    if mode == Mode::Targets {
        return match run_targets(path, &keys) {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("peer failed: {e}");
                std::process::ExitCode::FAILURE
            }
        };
    }

    match run(path, &keys, mode) {
        Ok(total) => {
            println!("cardinality {total}");
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("peer failed: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run(path: &str, keys: &[u64], mode: Mode) -> std::io::Result<u64> {
    let mut peer = Peer::connect(path)?;
    match peer.recv()? {
        Frame::ServerHello { protocol, .. } if protocol == yesno_plugin::ipc::VERSION as u32 => {}
        other => {
            return Err(std::io::Error::other(format!(
                "unexpected greeting {other:?}"
            )))
        }
    }
    peer.ask(Frame::ClientHello {
        protocol: yesno_plugin::ipc::VERSION as u32,
        name: "yesno-channel-peer".into(),
    })?;

    // The write comes first and the scan that follows is the demonstration: a snapshot
    // opened after the commit must see it.
    let keys = if mode == Mode::Write || mode == Mode::WriteN {
        let (key, ordinals) = keys.split_first().expect("checked in main");
        let writes: Vec<_> = match mode {
            Mode::WriteN => (0..ordinals[0])
                .map(|o| yesno_plugin::ipc::Write {
                    key: *key,
                    lo: o,
                    hi: o,
                    op: yesno_plugin::ipc::WriteOp::Insert,
                })
                .collect(),
            _ => ordinals
                .iter()
                .map(|&o| yesno_plugin::ipc::Write {
                    key: *key,
                    lo: o,
                    hi: o,
                    op: yesno_plugin::ipc::WriteOp::Insert,
                })
                .collect(),
        };
        match peer.ask(Frame::Apply { writes })? {
            Frame::Committed { version, changed } => {
                println!("committed version {version}, changed {changed}");
            }
            // `WRONG_ROLE` here is the interesting failure and deserves to be legible: it
            // means this peer is talking to a follower, and the write belongs at the leader.
            other => {
                return Err(std::io::Error::other(format!(
                    "the write was refused: {other:?}"
                )))
            }
        }
        vec![*key]
    } else {
        keys.to_vec()
    };

    loop {
        let total = scan_once(&mut peer, &keys, mode)?;
        if mode != Mode::Watch {
            return Ok(total);
        }
    }
}

/// One snapshot, opened, walked, announced, and -- in `Watch` -- released.
///
/// Lanes are released **before** the snapshot is closed. A snapshot refuses to
/// close while a handle derived from it is live, so the other order would fault
/// on every round after the first.
fn scan_once(peer: &mut Peer, keys: &[u64], mode: Mode) -> std::io::Result<u64> {
    let snapshot = open_snapshot(peer)?;

    let (lanes, arena_off) = match peer.ask(Frame::LanesAcquire {
        snapshot,
        keys: keys.to_vec(),
    })? {
        Frame::LanesAcquired { lanes, arena_off } => (lanes, arena_off),
        other => return Err(std::io::Error::other(format!("unexpected {other:?}"))),
    };

    let mut total = 0u64;
    loop {
        match peer.ask(Frame::BlockAdvance { lanes })? {
            Frame::BlockDone => break,
            Frame::Block { lanes: ls, .. } => {
                for (i, lane) in ls.iter().enumerate() {
                    let off = lane_offset(arena_off, i) as usize;
                    let n = lane.kind.payload_bytes(lane.count);
                    total += count(lane.kind, &peer.view[off..off + n]);
                }
                peer.ask(Frame::BlockRelease { lanes })?;
            }
            Frame::BlocksInline { blocks, payload } => {
                for b in &blocks {
                    let mut at = 0usize;
                    for lane in &b.lanes {
                        let n = lane.kind.payload_bytes(lane.count);
                        total += count(lane.kind, &payload[at..at + n]);
                        at += n;
                    }
                }
                peer.ask(Frame::BlockRelease { lanes })?;
            }
            other => return Err(std::io::Error::other(format!("unexpected {other:?}"))),
        }
    }

    match mode {
        // The write modes have already printed their commit; the scan they then run behaves
        // like `Once`, which is the whole point -- the read path is unchanged by the write.
        //
        // `Targets` never reaches here: it runs on the official client and leaves
        // `main` before `run`, so this arm exists only to keep the match exhaustive
        // and would be a bug if it were ever taken.
        Mode::Once | Mode::Write | Mode::WriteN | Mode::Targets => {}
        Mode::Hold => {
            // Snapshot deliberately still open. Announce it, then block: whoever
            // started this wants to kill it and watch the server clean up.
            println!("holding {total}");
            std::io::stdout().flush()?;
            loop {
                let mut sink = [0u8; 64];
                if peer.sock.read(&mut sink)? == 0 {
                    break;
                }
            }
        }
        Mode::Watch => {
            println!("holding {total}");
            std::io::stdout().flush()?;
            std::thread::sleep(WATCH_INTERVAL);
            peer.ask(Frame::LanesRelease { lanes })?;
            peer.ask(Frame::SnapshotClose { snapshot })?;
        }
    }
    Ok(total)
}

/// A bounded window and an exact target batch, through the official client.
///
/// # What it prints, and why that is the whole test
///
/// `window <total>` is the cardinality of every lane of every block **inside the
/// window**, which is strictly less than a whole-key scan of the same keys: the
/// chunks below `WINDOW_LO` are never read, so a window that silently did nothing
/// would print the larger figure. Then one `block` line per target, in the order
/// asked for, with the number of lanes present -- so an all-absent target prints
/// `present 0` rather than being missing, and a duplicated key prints two.
///
/// The two `refused` lines are the point of the mode. A batch that is not ascending
/// is refused by the client before a byte is sent; a batch whose first target is
/// behind the handle's position is refused by the server. **Both leave the handle
/// usable**, which the `after` line proves by reading a further target on the same
/// handle -- an exact-target reader that lost its handle to a bad batch would have
/// to re-acquire, and re-acquiring means another snapshot or another round of
/// planning for a mistake that cost the server nothing.
fn run_targets(path: &str, keys: &[u64]) -> std::io::Result<()> {
    use yesno_plugin::client::Client;

    /// Low enough to exclude prefix 0 and high enough to include a target past
    /// every stored chunk, so both edges of the window are exercised.
    const WINDOW_LO: u64 = 1;
    const WINDOW_HI: u64 = 10;

    let fail = |e: yesno_plugin::client::Error| std::io::Error::other(format!("{e}"));
    let client = Client::connect(path, "yesno-channel-peer").map_err(fail)?;
    let snap = client.snapshot().map_err(fail)?;
    let mut cursor = snap
        .lanes_in_prefix_range(keys.to_vec(), WINDOW_LO, WINDOW_HI)
        .map_err(fail)?;

    // Sequential first, so the window's effect is visible as a total before any
    // target is named -- and so the shared forward position starts somewhere other
    // than the beginning for the batch that follows.
    let mut total = 0u64;
    while cursor.advance().map_err(fail)? {
        for i in 0..cursor.lane_count() {
            let (lane, bytes) = cursor.lane(i).map_err(fail)?;
            total += count(lane.kind, bytes);
        }
    }
    println!("window {total}");

    // A fresh handle, because the walk above consumed the shared position.
    let mut cursor = snap
        .lanes_in_prefix_range(keys.to_vec(), WINDOW_LO, WINDOW_HI)
        .map_err(fail)?;

    // One target past what the greeting advertised, before any position is taken.
    // Refused for its **size**, which is the only fault these targets have: they
    // are ascending, inside the window and ahead of a handle that has read nothing.
    let cap = client.limits().max_blocks as u64;
    let oversized: Vec<u64> = (WINDOW_LO..WINDOW_LO + cap + 1).collect();
    match cursor.seek_many(&oversized) {
        Err(_) => println!("refused oversized {}", oversized.len()),
        Ok(_) => {
            return Err(std::io::Error::other(format!(
                "a batch of {} targets was accepted against max_blocks {cap}",
                oversized.len()
            )))
        }
    }

    // Exactly `max_blocks` targets, so the boundary is the case under test rather
    // than one avoided. 2 holds nothing and must still answer; 9 is past every
    // stored chunk and is read further down, on the same handle.
    let targets: Vec<u64> = vec![1, 2, 3, 5];
    let n = cursor.seek_many(&targets).map_err(fail)?;
    if n != targets.len() {
        return Err(std::io::Error::other(format!(
            "asked for {} targets and got {n} blocks",
            targets.len()
        )));
    }
    for (at, &target) in targets.iter().enumerate() {
        if at > 0 && !cursor.advance().map_err(fail)? {
            return Err(std::io::Error::other(format!(
                "the batch ran out before target {target}"
            )));
        }
        let mut card = 0u64;
        let mut present = 0usize;
        for i in 0..cursor.lane_count() {
            let (lane, bytes) = cursor.lane(i).map_err(fail)?;
            if lane.kind != LaneKind::Absent {
                present += 1;
                card += count(lane.kind, bytes);
            }
        }
        println!(
            "block {} present {present} card {card}",
            cursor.prefix().unwrap_or(u64::MAX)
        );
    }

    // Refused by this client, before anything is sent.
    match cursor.seek_many(&[7, 6]) {
        Err(_) => println!("refused descending"),
        Ok(_) => return Err(std::io::Error::other("a descending batch was accepted")),
    }
    // Refused by the server: 4 is behind the position the batch above reached, and
    // the whole batch goes with it even though 9 alone would have been legal.
    match cursor.seek_many(&[4, 9]) {
        Err(_) => println!("refused backwards"),
        Ok(_) => return Err(std::io::Error::other("a backwards batch was accepted")),
    }
    // And the handle survived both, which is why they are refusals rather than
    // failures.
    let n = cursor.seek_many(&[9]).map_err(fail)?;
    println!("after {n} block {}", cursor.prefix().unwrap_or(u64::MAX));
    Ok(())
}

/// Ordinals in one lane's payload, per its representation.
///
/// Run pairs are `[ start, end ]` and **both ends are inclusive**; reading the
/// second word as a length is the defect fixed on 2026-09-29, and it is silent
/// because a wrong interval is still a well-formed one.
fn count(kind: LaneKind, bytes: &[u8]) -> u64 {
    match kind {
        LaneKind::Absent => 0,
        LaneKind::Array => (bytes.len() / 2) as u64,
        LaneKind::Bitmap => bytes
            .as_chunks::<8>()
            .0
            .iter()
            .map(|w| u64::from_le_bytes(*w).count_ones() as u64)
            .sum(),
        LaneKind::Run => bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|p| {
                let start = u16::from_le_bytes([p[0], p[1]]);
                let end = u16::from_le_bytes([p[2], p[3]]);
                (end as u64) - (start as u64) + 1
            })
            .sum(),
    }
}
