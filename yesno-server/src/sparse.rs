//! Which parts of a sparse file are actually there.
//!
//! A shard image is grown to a whole mapping segment at creation, so a
//! database holding kilobytes lives in a file that reports gigabytes. Two
//! places copy those images -- the replication leader shipping a base
//! snapshot, and the archive capture cutting one into content-addressed
//! slabs -- and until 2026-10-16 both read every byte to find out which parts
//! were holes. Measured on a four-shard database holding three ordinals, that
//! was 15.5 seconds for a bootstrap and 17.3 for a capture, in neither case
//! because of anything the database contained.
//!
//! This module is public for the second of those callers. `yesno-server-utils`
//! copies the same images for the archive and depends on this crate already,
//! the same way it takes the daemon's metric writers rather than restating
//! them. Nothing about the answer is specific to replication, and two copies
//! of a `libc` call with its own safety argument is one too many.

/// The next allocated run at or after `from`, as `(start, end)`.
///
/// `None` when the filesystem says nothing past `from` is allocated, which is
/// the answer that makes the caller stop rather than read a gigabyte of
/// holes.
///
/// **Degrades to the whole remainder on any refusal.** A filesystem without
/// hole support answers `EINVAL`, an old kernel `ENOSYS`; reporting one run
/// covering everything left is then both correct and exactly what this
/// function replaced. The only answer treated as "nothing left" is the one
/// that means it: `ENXIO` from `SEEK_DATA`.
pub fn next_allocated(file: &std::fs::File, from: u64, total_len: u64) -> Option<(u64, u64)> {
    use std::os::fd::AsRawFd;

    if from >= total_len {
        return None;
    }
    let whole = Some((from, total_len));
    let fd = file.as_raw_fd();
    // SAFETY: `fd` is borrowed from a `File` the caller holds open for the
    // whole call, and `lseek` reads and writes no memory -- it moves that one
    // descriptor's offset, which nothing else uses. The descriptor exists for
    // this probing and is never read from.
    let start = unsafe { libc::lseek(fd, from as libc::off_t, libc::SEEK_DATA) };
    if start < 0 {
        return match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::ENXIO) => None,
            _ => whole,
        };
    }
    // SAFETY: as above. The end of the file counts as a hole, so this call
    // terminates every run.
    let end = unsafe { libc::lseek(fd, start, libc::SEEK_HOLE) };
    if end < 0 {
        return whole;
    }
    // The kernel is not supposed to answer outside the file or behind the
    // offset asked for. Clamped rather than trusted, because the whole point
    // of the pair is to decide which bytes are read, and an answer past the
    // end would read past it.
    let start = (start as u64).clamp(from, total_len);
    let end = (end as u64).clamp(start, total_len);
    match start < end {
        true => Some((start, end)),
        false => None,
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Seek, SeekFrom, Write};

    use super::next_allocated;

    /// The runs `next_allocated` reports, walked to the end of the file.
    fn runs(file: &std::fs::File, total_len: u64) -> Vec<(u64, u64)> {
        let mut found = Vec::new();
        let mut at = 0u64;
        while let Some((start, end)) = next_allocated(file, at, total_len) {
            assert!(
                start >= at,
                "a run behind the offset asked for: {start} < {at}"
            );
            assert!(end <= total_len, "a run past the end: {end} > {total_len}");
            assert!(start < end, "an empty run at {start}");
            found.push((start, end));
            at = end;
        }
        found
    }

    /// The property the whole optimization rests on: **nothing written may be
    /// left out of a run.** A run that is missed is a region the leader never
    /// reads and never ships, and the follower's image is then quietly short
    /// in the middle -- the failure this file's comments record paying for
    /// twice already.
    ///
    /// Deliberately not asserted here: that the holes are *excluded*. A
    /// filesystem without hole support answers that the whole file is one
    /// run, which is correct and is the behaviour this replaced, so an
    /// assertion about exclusion would be an assertion about the filesystem
    /// the gate happens to run on. What exclusion buys was measured instead,
    /// end to end: see the journal for 2026-10-16.
    #[test]
    fn every_written_byte_falls_inside_a_reported_run() {
        const MIB: u64 = 1 << 20;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sparse.bin");
        let written = [(0u64, 4096u64), (MIB, MIB + 4096), (3 * MIB, 3 * MIB + 17)];

        let mut file = std::fs::File::create(&path).unwrap();
        file.set_len(4 * MIB).unwrap();
        for (start, end) in written {
            file.seek(SeekFrom::Start(start)).unwrap();
            file.write_all(&vec![0xab; (end - start) as usize]).unwrap();
        }
        file.sync_all().unwrap();
        drop(file);

        let file = std::fs::File::open(&path).unwrap();
        let found = runs(&file, 4 * MIB);
        assert!(!found.is_empty(), "a file with data reported no runs");
        for (start, end) in written {
            assert!(
                found.iter().any(|(a, b)| *a <= start && end <= *b),
                "{start}..{end} is in no run of {found:?}"
            );
        }
    }

    /// A file with nothing in it must terminate the walk rather than report a
    /// run the caller would then read.
    #[test]
    fn a_file_of_nothing_but_holes_reports_at_most_itself() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.bin");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(1 << 20).unwrap();
        file.sync_all().unwrap();
        drop(file);

        let file = std::fs::File::open(&path).unwrap();
        // Either none -- the filesystem knows it is all hole -- or the one
        // run the fallback reports. Never more, and never past the end.
        assert!(runs(&file, 1 << 20).len() <= 1);
        assert_eq!(
            next_allocated(&file, 1 << 20, 1 << 20),
            None,
            "past the end"
        );
    }
}
