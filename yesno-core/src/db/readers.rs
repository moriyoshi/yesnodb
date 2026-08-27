//! The cross-process reader registry.
//!
//! # What this exists to prevent
//!
//! Extent reclamation has three conditions, and **condition 1** — no live
//! snapshot can reach the extent through any retained root — is enforced from
//! `DbInner::readers`, which is process-local memory. A reader in another
//! process is invisible to it, so the writer would compute a reclamation floor
//! as if that reader did not exist and reuse an extent it is still mapping.
//!
//! Condition 3 does **not** cover this. That condition is the live
//! `arrow_buffer::Buffer` refcount, which is also process-local; the foreign
//! process's own mapping keeps its *file pages* alive but does nothing to stop
//! the writer reusing the slot underneath them. The two conditions guard
//! different things and neither implies the other.
//!
//! # What the failure would look like
//!
//! Not corruption. `ShardStore::read_container_for` is the *checked* read — it
//! validates the `ChunkKey` a reference claims — so a reader that followed a
//! stale reference into a reused slot gets a decode error rather than another
//! key's data. That makes this a liveness and correctness-of-error problem
//! rather than a silent-wrong-answer one, which is the only reason a reader was
//! worth building before the registry existed. It is still not acceptable:
//! an error on a read that should have succeeded is a wrong answer to the
//! caller.
//!
//! # Shape
//!
//! A fixed array of slots in a file beside `LOCK`, each holding a pid, the
//! version a reader is pinned to, and the checkpoint sequence its roots came
//! from. Fixed-size because the file is written by processes that do not
//! coordinate: a slot is claimed with a compare-and-swap on its pid word, and
//! nothing ever resizes or compacts.
//!
//! **Staleness is resolved by pid liveness, not by a heartbeat.** A reader
//! that crashes leaves its slot populated for ever otherwise, and a heartbeat
//! would make correctness depend on a timer — a reader merely *slow* would be
//! declared dead and have its extents reclaimed underneath it. `kill(pid, 0)`
//! answers the first half of the question that matters.
//!
//! # Pid reuse, and why a pid alone is not an identity
//!
//! A pid is recycled, so a slot left by a crashed reader can name a live
//! process that never registered. That direction costs space rather than
//! correctness — a phantom reader holds the reclamation floor back — but it is
//! still wrong, and it is closed here by recording the reader's **process start
//! time** beside its pid and comparing both. On Linux the start time is field
//! 22 of `/proc/<pid>/stat`, in clock ticks since boot; it is fixed for the
//! life of a process, so `( pid, start time )` names one process and a recycled
//! pid cannot impersonate it.
//!
//! **Every ambiguity resolves toward "still live"**, because the unsafe
//! direction is declaring a live reader dead and reclaiming extents underneath
//! it. A slot is treated as dead only when the identity region positively
//! contradicts it: the recorded pid matches, both start times are known, and
//! they differ. An absent identity region, an entry that does not match the
//! slot's pid, a start time of zero, or a platform with no readable `/proc` —
//! all of these fall back to today's pid-only behaviour, which is conservative.
//! There is no fallback that says "dead".
//!
//! Non-Linux, or a Linux with `/proc` unmounted, therefore keeps exactly the
//! old semantics: the registry still works, it is merely still vulnerable to
//! the space leak a recycled pid causes. The reader is never wrong, only
//! wasteful.
//!
//! # File layout, and why the identity region is appended rather than inlined
//!
//! The file has **no header and no version word** in the region a slot lives
//! in, and one cannot be retrofitted: a build that predates this change reads
//! 24-byte slots from offset 0 unconditionally and has no way to be told not
//! to. Widening the slot would therefore make such a build read the words at
//! the wrong stride, and — the fatal part — it could then *claim a slot a live
//! reader holds*, erasing that reader from the floor. That is the unsafe
//! direction.
//!
//! So the slot array is left byte-for-byte as it was and the identities are
//! appended **after** it, behind their own magic and version:
//!
//! ```text
//! 0                     : MAX_READERS x 24-byte slots  ( pid, version, ckpt_seq )
//! IDENT_OFF             : magic, format version, entry stride, entry count
//! IDENT_ENTRIES_OFF     : MAX_READERS x 16-byte entries ( pid, start time )
//! ```
//!
//! An older build maps the larger file, reads only the prefix, and behaves
//! exactly as it did — correctly, and conservatively. A newer build opening an
//! old short file finds no identity region, treats every slot's start time as
//! unknown, and extends the file so later claims record one.
//!
//! **A slot claimed by an older build is safe against a newer one**, and the
//! argument is worth keeping: such a slot's identity entry is whatever the
//! previous occupant left, so its recorded pid differs from the slot's pid and
//! the entry is ignored. It cannot accidentally *match*, because a slot whose
//! pid is live is never claimed — so no claimant can ever write its own pid
//! into a slot whose stale entry already names that same pid.

use std::path::Path;

use crate::error::{CodecError, Result};

/// Slots in the registry. Fixed, and part of the file format: changing it
/// changes every offset, so a mixed-version pair of processes would read each
/// other's slots at the wrong place. The slot array still carries no version
/// word and cannot be given one now — see the layout note above, which is why
/// the identity region was appended instead of the slot being widened. The
/// identity region *does* record this count, so a build that disagrees about it
/// ignores that region rather than misreading it.
pub const MAX_READERS: usize = 4096;

/// `pid`, `version`, `ckpt_seq`, each a little-endian `u64`.
const SLOT_BYTES: usize = 24;
/// The v1 file, and still the prefix of every later one. Do not fold the
/// identity region into this constant: an older build compares the file length
/// against its own copy of it, and every byte below it must keep the meaning it
/// had.
const FILE_BYTES: usize = MAX_READERS * SLOT_BYTES;

/// Start of the identity region.
const IDENT_OFF: usize = FILE_BYTES;
/// `"YNRIDENT"`, little-endian. Present only in files written by a build that
/// records start times; its absence is a valid state, not an error.
const IDENT_MAGIC: u64 = u64::from_le_bytes(*b"YNRIDENT");
/// The identity region's own version, independent of the slot array's ( which
/// has none ). A region that fails **any** of magic, version, stride or
/// count is ignored wholesale rather than partially trusted: the fallback is
/// pid-only liveness, which is conservative, so refusing to guess costs
/// retained space and never a reclaimed live extent.
const IDENT_VERSION: u64 = 1;
/// `pid`, `start_time`, each a little-endian `u64`.
const IDENT_ENTRY_BYTES: usize = 16;
/// magic, version, stride, count.
const IDENT_HEADER_BYTES: usize = 32;
const IDENT_ENTRIES_OFF: usize = IDENT_OFF + IDENT_HEADER_BYTES;
/// The whole file as this build writes it.
const TOTAL_BYTES: usize = IDENT_ENTRIES_OFF + MAX_READERS * IDENT_ENTRY_BYTES;

/// A registered foreign reader. Releases its slot on drop.
pub struct ReaderRegistration {
    map: memmap2::MmapMut,
    slot: usize,
}

impl ReaderRegistration {
    /// Claim a slot for this process.
    ///
    /// Publishes `( version, ckpt_seq )` and the identity entry **before**
    /// the pid, and the order is load-bearing: a writer that saw the pid first
    /// could read a zero `ckpt_seq`, and zero means "assume the oldest possible
    /// root", which blocks reclamation rather than permitting it. Getting the
    /// order backwards is safe in that direction and would still be wrong to
    /// rely on. The identity entry rides the same rule for a weaker reason —
    /// a writer that saw the pid first would find the entry not yet matching
    /// and fall back to pid-only liveness, which is merely conservative.
    pub fn claim(dir: &Path, version: u64, ckpt_seq: u64) -> Result<Self> {
        let path = dir.join("READERS");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|_| CodecError::Invariant("cannot open the reader registry"))?;
        // Grown, never shrunk — an older build's mapping of the same file
        // must keep addressing the same slots, and a shorter file would be one
        // it re-extends with zeros over live readers.
        if file
            .metadata()
            .map_err(|_| CodecError::Invariant("cannot stat the reader registry"))?
            .len()
            < TOTAL_BYTES as u64
        {
            file.set_len(TOTAL_BYTES as u64)
                .map_err(|_| CodecError::Invariant("cannot size the reader registry"))?;
        }
        // SAFETY: a file mapped shared that is never truncated — the size is
        // set above and only ever grows ( an older build's `FILE_BYTES`, this
        // one's `TOTAL_BYTES` ), so the SIGBUS hazard that applies to the shard
        // files does not arise.
        let mut map = unsafe { memmap2::MmapMut::map_mut(&file) }
            .map_err(|_| CodecError::Invariant("cannot map the reader registry"))?;

        // Stamp the identity header if this is an old file, or a fresh one.
        // Idempotent on purpose: two processes racing here write identical
        // bytes, and neither touches an existing entry.
        if !ident_ok(&map) {
            write_u64(&mut map, IDENT_OFF, IDENT_MAGIC);
            write_u64(&mut map, IDENT_OFF + 8, IDENT_VERSION);
            write_u64(&mut map, IDENT_OFF + 16, IDENT_ENTRY_BYTES as u64);
            write_u64(&mut map, IDENT_OFF + 24, MAX_READERS as u64);
        }

        let me = std::process::id() as u64;
        let my_start = proc_start_time(me).unwrap_or(0);
        for slot in 0..MAX_READERS {
            let off = slot * SLOT_BYTES;
            // Free, or held by a process that is gone — including one whose pid
            // has since been recycled by an unrelated process.
            if slot_is_live(&map, slot) {
                continue;
            }
            write_u64(&mut map, off + 8, version);
            write_u64(&mut map, off + 16, ckpt_seq);
            let ident = IDENT_ENTRIES_OFF + slot * IDENT_ENTRY_BYTES;
            write_u64(&mut map, ident, me);
            write_u64(&mut map, ident + 8, my_start);
            write_u64(&mut map, off, me);
            return Ok(ReaderRegistration { map, slot });
        }
        Err(CodecError::Invariant(
            "the reader registry is full; no slot is free",
        ))
    }
}

impl Drop for ReaderRegistration {
    fn drop(&mut self) {
        let off = self.slot * SLOT_BYTES;
        // Identity first, pid second, and the order is the reverse of
        // `claim`'s for a reason: once the pid word is zero another process may
        // claim the slot and write its own identity entry, and a zero written
        // after that would erase it. Zeroing the identity while the pid is
        // still set only degrades the slot to pid-only liveness, which is
        // conservative.
        let ident = IDENT_ENTRIES_OFF + self.slot * IDENT_ENTRY_BYTES;
        if ident + IDENT_ENTRY_BYTES <= self.map.len() {
            write_u64(&mut self.map, ident, 0);
            write_u64(&mut self.map, ident + 8, 0);
        }
        write_u64(&mut self.map, off, 0);
        // Best effort: the mapping is shared, so the zero is visible to the
        // writer as soon as it lands. A crash skips this, which is exactly what
        // the pid-liveness check exists to clean up.
        let _ = self.map.flush_async();
    }
}

/// What foreign readers require the writer to retain.
///
/// Returns `( min_version, min_ckpt_seq )` over live slots, or `None` when there
/// are none.
///
/// Called on every reclamation decision, so it must stay cheap: a linear scan
/// of 4096 slots is one 96 KiB page walk and no syscalls except the liveness
/// checks, which only run for non-zero pids. The `/proc` read that confirms a
/// pid's identity runs only for a slot that got past `kill(pid, 0)` — one open
/// and one read per *live* foreign reader, of which there are normally a
/// handful. Do not cache it: the whole point is that the answer can change.
pub fn floors(dir: &Path) -> Option<(u64, u64)> {
    let path = dir.join("READERS");
    let file = std::fs::OpenOptions::new().read(true).open(&path).ok()?;
    if file.metadata().ok()?.len() < FILE_BYTES as u64 {
        return None;
    }
    // SAFETY: read-only mapping of a file that is never truncated.
    let map = unsafe { memmap2::Mmap::map(&file) }.ok()?;

    let mut min_version = u64::MAX;
    let mut min_ckpt = u64::MAX;
    let mut any = false;
    for slot in 0..MAX_READERS {
        let off = slot * SLOT_BYTES;
        if !slot_is_live(&map, slot) {
            continue;
        }
        any = true;
        min_version = min_version.min(read_u64(&map, off + 8));
        min_ckpt = min_ckpt.min(read_u64(&map, off + 16));
    }
    any.then_some((min_version, min_ckpt))
}

/// Whether a slot names a reader that is still running.
///
/// The conservative predicate. `false` — which permits reclamation — is
/// returned only for a free slot, a pid that no longer exists, or a pid whose
/// recorded and actual start times are both known, both nonzero, and different.
/// Everything else is `true`.
fn slot_is_live(map: &[u8], slot: usize) -> bool {
    let pid = read_u64(map, slot * SLOT_BYTES);
    if pid == 0 || !pid_is_live(pid) {
        return false;
    }
    !identity_refutes(recorded_start(map, slot, pid), proc_start_time(pid))
}

/// Whether the two start times **prove** the slot's reader is gone.
///
/// A separate function because it is the whole safety argument, and as a
/// truth table it can be tested exhaustively — including the rows no Linux test
/// host can reach, such as a live pid whose `/proc` entry cannot be read.
///
/// | recorded | actual | verdict |
/// |---|---|---|
/// | unknown | anything | live — nothing to compare |
/// | known | unknown | live — cannot disprove it |
/// | known | equal | live — the same process |
/// | known | different | **dead** — the pid was recycled |
fn identity_refutes(recorded: Option<u64>, actual: Option<u64>) -> bool {
    match (recorded, actual) {
        (Some(r), Some(a)) => r != a,
        _ => false,
    }
}

/// The start time recorded for `slot`, if the identity region is present and
/// its entry describes `pid`.
///
/// The `pid` match is what makes a stale entry harmless — including one left
/// behind for a slot since claimed by a build that does not write entries.
fn recorded_start(map: &[u8], slot: usize, pid: u64) -> Option<u64> {
    if !ident_ok(map) {
        return None;
    }
    let off = IDENT_ENTRIES_OFF + slot * IDENT_ENTRY_BYTES;
    if read_u64(map, off) != pid {
        return None;
    }
    // Zero is "unknown", written by a claim that could not read `/proc`.
    match read_u64(map, off + 8) {
        0 => None,
        start => Some(start),
    }
}

/// Whether the file carries an identity region this build understands.
fn ident_ok(map: &[u8]) -> bool {
    map.len() >= TOTAL_BYTES
        && read_u64(map, IDENT_OFF) == IDENT_MAGIC
        && read_u64(map, IDENT_OFF + 8) == IDENT_VERSION
        && read_u64(map, IDENT_OFF + 16) == IDENT_ENTRY_BYTES as u64
        && read_u64(map, IDENT_OFF + 24) == MAX_READERS as u64
}

/// A process's start time, in clock ticks since boot.
///
/// Linux only, and `None` everywhere else — including a Linux with `/proc`
/// unmounted, a pid in another namespace, or a process that vanished between
/// the liveness check and this read. Every one of those falls back to pid-only
/// liveness. Do not turn a `None` here into "dead".
fn proc_start_time(pid: u64) -> Option<u64> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_start_time(&stat)
}

/// Field 22 of a `/proc/<pid>/stat` line.
///
/// Parsed from the **last** `)` rather than by splitting on whitespace, and
/// this is not defensive programming: field 2 is the executable name in
/// parentheses, it is not escaped, and it can contain spaces *and* parentheses
/// — `(evil) 1 2 3)` is a legal `comm`. Counting tokens from the start of the
/// line reads a different field for such a process, and the field it lands on
/// is an attacker-chosen number.
fn parse_start_time(stat: &str) -> Option<u64> {
    let close = stat.rfind(')')?;
    // Fields 3 onward follow the `)`; field 22 is 19 further along.
    stat.get(close + 1..)?
        .split_ascii_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

/// Whether a pid names a live process.
///
/// `kill(pid, 0)` rather than a `/proc` lookup: it is one syscall, it is
/// portable across the platforms this crate supports, and `EPERM` — a process
/// this user may not signal — correctly reads as *alive*.
fn pid_is_live(pid: u64) -> bool {
    if pid == 0 || pid > i32::MAX as u64 {
        return false;
    }
    // SAFETY: `kill` with signal 0 performs error checking only and sends
    // nothing. It cannot affect the target process.
    let rc = unsafe { libc_kill(pid as i32, 0) };
    rc == 0 || is_eperm()
}

// Declared here rather than taking a `libc` dependency. `yesno-core` has five
// direct dependencies and a CI job that fails if it gains a sixth; `kill` is two
// lines of `extern "C"` and this is the only place the crate needs it.
extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
    #[link_name = "__errno_location"]
    fn errno_location() -> *mut i32;
}

fn is_eperm() -> bool {
    // EPERM is 1 on every platform this crate targets.
    // SAFETY: `__errno_location` returns a pointer to this thread's errno.
    unsafe { *errno_location() == 1 }
}

fn read_u64(map: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(map[off..off + 8].try_into().expect("8 bytes in range"))
}

fn write_u64(map: &mut [u8], off: usize, v: u64) {
    map[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "yesno-readers-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn an_empty_registry_imposes_no_floor() {
        let d = tmp("empty");
        assert_eq!(floors(&d), None, "no file at all");
        let r = ReaderRegistration::claim(&d, 5, 2).unwrap();
        assert_eq!(floors(&d), Some((5, 2)));
        drop(r);
        assert_eq!(floors(&d), None, "a released slot imposes nothing");
    }

    #[test]
    fn the_floor_is_the_minimum_over_live_readers() {
        let d = tmp("min");
        let a = ReaderRegistration::claim(&d, 9, 4).unwrap();
        let b = ReaderRegistration::claim(&d, 3, 7).unwrap();
        // The two minima are taken independently. They are different
        // quantities — a version and a checkpoint sequence — and the reader with
        // the older version is not necessarily the one with the older root.
        assert_eq!(floors(&d), Some((3, 4)));
        drop(b);
        assert_eq!(floors(&d), Some((9, 4)));
        drop(a);
        assert_eq!(floors(&d), None);
    }

    /// The property the whole file exists for: a reader that **crashed**
    /// must not pin retention for ever. Simulated by writing a pid that cannot
    /// be running.
    #[test]
    fn a_dead_readers_slot_is_ignored_and_reclaimed() {
        let d = tmp("dead");
        {
            let _live = ReaderRegistration::claim(&d, 1, 1).unwrap();
        }
        // Forge a slot held by a pid that does not exist. Not pid 1, which is
        // always alive.
        let path = d.join("READERS");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let mut map = unsafe { memmap2::MmapMut::map_mut(&file) }.unwrap();
        write_u64(&mut map, 0, i32::MAX as u64 - 1);
        write_u64(&mut map, 8, 42);
        write_u64(&mut map, 16, 42);
        map.flush().unwrap();
        drop(map);

        assert_eq!(
            floors(&d),
            None,
            "a slot held by a dead pid must impose no floor"
        );

        // And the slot is reusable rather than leaked.
        let r = ReaderRegistration::claim(&d, 7, 7).unwrap();
        assert_eq!(r.slot, 0, "the dead slot should be the one claimed");
        assert_eq!(floors(&d), Some((7, 7)));
    }

    #[test]
    fn this_process_is_live_and_a_zero_pid_is_not() {
        assert!(pid_is_live(std::process::id() as u64));
        assert!(!pid_is_live(0));
        assert!(!pid_is_live(u64::MAX));
    }

    /// Map the registry for forging. Only sound in tests, where the file is
    /// this test's own and nothing else is mapping it.
    fn forge(dir: &std::path::Path) -> memmap2::MmapMut {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(dir.join("READERS"))
            .unwrap();
        unsafe { memmap2::MmapMut::map_mut(&file) }.unwrap()
    }

    fn ident_entry(map: &[u8], slot: usize) -> (u64, u64) {
        let off = IDENT_ENTRIES_OFF + slot * IDENT_ENTRY_BYTES;
        (read_u64(map, off), read_u64(map, off + 8))
    }

    /// The property this file's pid-reuse fix exists for. A slot left by a
    /// crashed reader whose pid has since been **recycled** by an unrelated
    /// live process must not pin retention. Simulated exactly: the pid is this
    /// process, so `kill(pid, 0)` says live, and only the start time separates
    /// the phantom from a real registration.
    #[test]
    fn a_live_pid_with_a_foreign_start_time_imposes_no_floor() {
        let d = tmp("recycled");
        let me = std::process::id() as u64;
        {
            let _r = ReaderRegistration::claim(&d, 1, 1).unwrap();
        }
        let Some(real) = proc_start_time(me) else {
            // No `/proc`: the fallback is pid-only liveness and there is
            // nothing to assert. Documented as the non-Linux behaviour.
            return;
        };

        let mut map = forge(&d);
        write_u64(&mut map, 0, me);
        write_u64(&mut map, 8, 42);
        write_u64(&mut map, 16, 42);
        write_u64(&mut map, IDENT_ENTRIES_OFF, me);
        write_u64(&mut map, IDENT_ENTRIES_OFF + 8, real.wrapping_add(1));
        map.flush().unwrap();
        drop(map);

        assert_eq!(
            floors(&d),
            None,
            "a recycled pid must not make a dead reader's slot look live"
        );
        // And the slot is reusable rather than pinned for ever.
        let r = ReaderRegistration::claim(&d, 7, 7).unwrap();
        assert_eq!(r.slot, 0, "the phantom's slot should be the one claimed");
        assert_eq!(floors(&d), Some((7, 7)));
    }

    /// The other half: a real registration by this process, whose start time
    /// does match, still imposes a floor. A fix that declared everything
    /// dead would pass the test above and fail this one.
    #[test]
    fn this_process_with_its_recorded_start_time_imposes_a_floor() {
        let d = tmp("selfident");
        let me = std::process::id() as u64;
        let r = ReaderRegistration::claim(&d, 5, 2).unwrap();
        assert_eq!(floors(&d), Some((5, 2)));

        let map = forge(&d);
        assert!(ident_ok(&map), "claim must stamp the identity header");
        let (pid, start) = ident_entry(&map, r.slot);
        assert_eq!(pid, me, "the entry must name the claiming process");
        assert_eq!(
            start,
            proc_start_time(me).unwrap_or(0),
            "the entry must record this process's real start time"
        );
        let slot = r.slot;
        drop(map);

        drop(r);
        assert_eq!(floors(&d), None);
        // Released, not merely unreferenced: a stale entry left behind would
        // be inherited by the next claimant of this slot if it were ever to
        // name the same pid.
        assert_eq!(ident_entry(&forge(&d), slot), (0, 0));
    }

    /// A slot whose identity entry names a **different** pid says nothing about
    /// the slot's occupant — that is what a slot claimed by a build predating
    /// the identity region looks like. It must read as live: the ambiguous
    /// direction is the conservative one.
    #[test]
    fn an_identity_entry_for_another_pid_is_ignored_not_believed() {
        let d = tmp("mixed");
        let me = std::process::id() as u64;
        {
            let _r = ReaderRegistration::claim(&d, 1, 1).unwrap();
        }
        let mut map = forge(&d);
        write_u64(&mut map, 0, me);
        write_u64(&mut map, 8, 11);
        write_u64(&mut map, 16, 12);
        // A stale entry from some previous occupant, plus a start time that
        // would mismatch if it were believed.
        write_u64(&mut map, IDENT_ENTRIES_OFF, me + 1);
        write_u64(&mut map, IDENT_ENTRIES_OFF + 8, 999_999);
        map.flush().unwrap();
        drop(map);

        assert_eq!(
            floors(&d),
            Some((11, 12)),
            "an entry that does not name the slot's pid must not condemn it"
        );
    }

    /// The only row that may reclaim is `( known, known, different )`.
    /// Everything else is live, because the unsafe direction is declaring a
    /// live reader dead. Exhaustive over the shape of the inputs, which is what
    /// covers the rows a Linux test host cannot stage — a pid that is live but
    /// has no readable `/proc` entry.
    #[test]
    fn only_two_known_and_different_start_times_refute_a_slot() {
        assert!(identity_refutes(Some(7), Some(9)));
        assert!(!identity_refutes(Some(7), Some(7)));
        assert!(!identity_refutes(Some(7), None), "unreadable /proc is live");
        assert!(!identity_refutes(None, Some(9)), "nothing recorded is live");
        assert!(!identity_refutes(None, None));
    }

    /// Zero is the "unknown start time" sentinel, written by a claim on a
    /// platform with no readable `/proc`. It must not be compared against a
    /// real start time and found different — that is the unsafe direction.
    #[test]
    fn a_zero_start_time_means_unknown_not_mismatched() {
        let d = tmp("unknown");
        let me = std::process::id() as u64;
        {
            let _r = ReaderRegistration::claim(&d, 1, 1).unwrap();
        }
        let mut map = forge(&d);
        write_u64(&mut map, 0, me);
        write_u64(&mut map, 8, 21);
        write_u64(&mut map, 16, 22);
        write_u64(&mut map, IDENT_ENTRIES_OFF, me);
        write_u64(&mut map, IDENT_ENTRIES_OFF + 8, 0);
        map.flush().unwrap();
        drop(map);

        assert_eq!(floors(&d), Some((21, 22)));
    }

    /// An identity region this build cannot vouch for is ignored **wholesale**,
    /// not partially trusted. Both directions of that matter: it must not
    /// condemn a slot, and it must not be read at a stride it was not written
    /// with.
    #[test]
    fn an_unrecognized_identity_header_is_ignored_wholesale() {
        let d = tmp("badhdr");
        let me = std::process::id() as u64;
        for (label, off, bad) in [
            ("magic", 0u64, 0u64),
            ("version", 8, IDENT_VERSION + 1),
            ("stride", 16, 24),
            ("count", 24, MAX_READERS as u64 + 1),
        ] {
            let d = d.join(label);
            std::fs::create_dir_all(&d).unwrap();
            {
                let _r = ReaderRegistration::claim(&d, 1, 1).unwrap();
            }
            let mut map = forge(&d);
            write_u64(&mut map, 0, me);
            write_u64(&mut map, 8, 31);
            write_u64(&mut map, 16, 32);
            write_u64(&mut map, IDENT_ENTRIES_OFF, me);
            write_u64(&mut map, IDENT_ENTRIES_OFF + 8, 999_999);
            write_u64(&mut map, IDENT_OFF + off as usize, bad);
            map.flush().unwrap();
            drop(map);

            assert!(!ident_ok(&forge(&d)), "{label}");
            assert_eq!(
                floors(&d),
                Some((31, 32)),
                "a {label} this build does not recognize must not condemn a slot"
            );
        }
    }

    /// A registry written by a build with no identity region at all: exactly
    /// `FILE_BYTES` long, one live slot. It must keep imposing its floor,
    /// and a claim must extend the file rather than steal the slot.
    #[test]
    fn a_legacy_file_without_an_identity_region_still_imposes_its_floor() {
        let d = tmp("legacy");
        let path = d.join("READERS");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .unwrap();
        file.set_len(FILE_BYTES as u64).unwrap();
        let mut map = unsafe { memmap2::MmapMut::map_mut(&file) }.unwrap();
        write_u64(&mut map, 0, std::process::id() as u64);
        write_u64(&mut map, 8, 3);
        write_u64(&mut map, 16, 4);
        map.flush().unwrap();
        drop(map);
        drop(file);

        assert!(!ident_ok(&forge(&d)));
        assert_eq!(
            floors(&d),
            Some((3, 4)),
            "an unknown start time must read as live, not as dead"
        );

        let r = ReaderRegistration::claim(&d, 9, 9).unwrap();
        assert_ne!(r.slot, 0, "a live legacy slot must not be stolen");
        assert_eq!(floors(&d), Some((3, 4)));
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            TOTAL_BYTES as u64,
            "the claim should have extended the file"
        );
    }

    /// `comm` is unescaped and may contain spaces **and** parentheses, so
    /// field 22 can only be found by scanning back from the last `)`. Counting
    /// tokens from the start of the line reads an attacker-chosen number
    /// instead. Constructed directly: no real process is needed, and relying on
    /// finding one would make the test flaky rather than thorough.
    #[test]
    fn start_time_is_parsed_from_the_last_paren_not_by_splitting() {
        // Fields 3..=52 of a real line, with field 22 = 540885717.
        let tail = "R 1 2 3 0 -1 4194304 106 0 0 0 0 0 0 0 20 0 1 0 540885717 \
                    19525632 312 18446744073709551615 1 2 3 0 0 0 0 0 0 0 0 0 \
                    17 11 0 0 0 0 0 1 2 3 4 5 6";
        let tail: String = tail.split_whitespace().collect::<Vec<_>>().join(" ");

        assert_eq!(
            parse_start_time(&format!("42 (cat) {tail}")),
            Some(540_885_717),
            "the ordinary case"
        );
        // A `comm` that contains `) `, spaces, and digits that would be read as
        // field 22 by a naive split.
        assert_eq!(
            parse_start_time(&format!("42 (ev)  il 1 2 3) {tail}")),
            Some(540_885_717),
            "a comm containing `) ` must not shift the field index"
        );
        assert_eq!(
            parse_start_time(&format!("42 (a b c) {tail}")),
            Some(540_885_717),
            "a comm containing spaces"
        );
        // Malformed input is `None`, never a wrong number and never a panic.
        assert_eq!(parse_start_time(""), None);
        assert_eq!(parse_start_time("42 (cat) R 1 2"), None);
        assert_eq!(parse_start_time("no parens at all"), None);
    }
}
