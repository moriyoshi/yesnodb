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
//! yesno-channel-peer <socket> <key>...          # scan, print the cardinality
//! yesno-channel-peer --hold <socket> <key>...   # scan, then hold and wait
//! ```
//!
//! `--hold` keeps the snapshot open and blocks for ever, so a caller can kill the
//! process and observe what the server does about it. It prints `holding` first,
//! so the caller knows the snapshot exists before it pulls the trigger.

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
        if version != 1 {
            return Err(std::io::Error::other(format!(
                "server speaks protocol {version}, this peer speaks 1"
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

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (hold, rest) = match args.split_first() {
        Some((first, rest)) if first == "--hold" => (true, rest),
        _ => (false, &args[..]),
    };
    let Some((path, keys)) = rest.split_first() else {
        eprintln!("usage: yesno-channel-peer [--hold] <socket> <key>...");
        return std::process::ExitCode::from(2);
    };
    let keys: Vec<u64> = match keys.iter().map(|k| k.parse()).collect() {
        Ok(k) => k,
        Err(e) => {
            eprintln!("keys must be integers: {e}");
            return std::process::ExitCode::from(2);
        }
    };

    match run(path, &keys, hold) {
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

fn run(path: &str, keys: &[u64], hold: bool) -> std::io::Result<u64> {
    let mut peer = Peer::connect(path)?;
    match peer.recv()? {
        Frame::ServerHello { protocol: 1, .. } => {}
        other => {
            return Err(std::io::Error::other(format!(
                "unexpected greeting {other:?}"
            )))
        }
    }
    peer.ask(Frame::ClientHello {
        protocol: 1,
        name: "yesno-channel-peer".into(),
    })?;

    let snapshot = match peer.ask(Frame::SnapshotOpen)? {
        Frame::SnapshotOpened { snapshot, .. } => snapshot,
        // Not an error to retry on here, but the distinction is the point: a
        // real peer waits and asks again rather than exiting, because this is
        // what every request answers while the server is starting or rebuilding.
        Frame::Fault { status, message } => {
            return Err(std::io::Error::other(format!(
                "snapshot refused ( status {status} ): {message}"
            )))
        }
        other => return Err(std::io::Error::other(format!("unexpected {other:?}"))),
    };

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

    if hold {
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
    Ok(total)
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
            .chunks_exact(8)
            .map(|w| u64::from_le_bytes(w.try_into().expect("8 bytes")).count_ones() as u64)
            .sum(),
        LaneKind::Run => bytes
            .chunks_exact(4)
            .map(|p| {
                let start = u16::from_le_bytes([p[0], p[1]]);
                let end = u16::from_le_bytes([p[2], p[3]]);
                (end as u64) - (start as u64) + 1
            })
            .sum(),
    }
}
