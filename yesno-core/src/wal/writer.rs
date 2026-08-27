//! The log writer: immutable generations plus one active file per shard.
//!
//! # Why this did not exist until now
//!
//! `record` could frame and scan, `recover` could plan a replay, and
//! `crash_matrix` tested both hard — but nothing owned a file or appended to
//! one, so `Db` never wrote a record and a commit was durable only once a
//! checkpoint had run. The gap was invisible to every test because they all
//! checkpoint explicitly and none crashes.
//!
//! # `lsn` is a byte offset — in the shard's whole history, not in one file
//!
//! Not a counter. The offset a record starts at *is* its LSN, so a cursor is a
//! seek and a byte range shipped to a follower is offset-identical on both
//! sides. That last property is what makes the replication design work without
//! re-framing, and it is why `append` returns an offset rather than a sequence
//! number.
//!
//! A checkpoint seals `shard-NNNN.wal` by renaming it to
//! `shard-NNNN.wal.<base-lsn>` and creates a new active file. Sealed generations
//! are immutable and are reclaimed only as whole files. The filename is an
//! index; the first record still carries the generation's base.
//!
//! # Where the base comes from, and why the log answers for itself
//!
//! From the log's **own first record**, whose header carries the LSN it was
//! written at. A log is therefore self-describing and a reopen cannot disagree
//! with it.
//!
//! The one case that has no first record is an empty active generation. Its base
//! is the end of the newest sealed generation, or the checkpoint's
//! `wal_replay_lsn` when no retained generation remains.
//!
//! # Durability is explicit
//!
//! `append` writes; only `sync` makes it durable. They are separate so a batch
//! spanning several shards can append to all of them under their locks and then
//! fsync once per shard *after* the locks are released — which is what makes
//! group commit possible at all. A writer that synced inside `append` would
//! hold every shard lock across every fsync.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek};
use std::path::{Path, PathBuf};

use crate::error::{CodecError, Result};

/// The crate's error type predates `io`; map rather than widen it here, exactly
/// as `db::store` does.
pub(crate) fn io_err(e: std::io::Error) -> CodecError {
    CodecError::Invariant(match e.kind() {
        std::io::ErrorKind::NotFound => "write-ahead log not found",
        std::io::ErrorKind::PermissionDenied => "permission denied on the log",
        _ => "write-ahead log I/O error",
    })
}
use crate::mvcc::Version;
use crate::wal::record::{RecType, Record};

#[derive(Clone, Debug, PartialEq, Eq)]
struct Generation {
    path: PathBuf,
    base: u64,
    len: u64,
}

impl Generation {
    fn end(&self) -> u64 {
        self.base + self.len
    }
}

/// Immutable sealed generations followed by one append-only active file.
pub struct WalWriter {
    file: File,
    path: PathBuf,
    sealed: Vec<Generation>,
    /// The LSN of byte 0 of the active file.
    active_base: u64,
    active_len: u64,
    /// First record LSN for each commit version still retained, in file order.
    version_starts: Vec<(Version, u64)>,
    /// Durable **through this LSN** as of the last sync.
    ///
    /// An LSN rather than a byte count, because every target it is compared
    /// against comes from [`Self::end_lsn`]. Mixing the two would make a log
    /// with a reclaimed prefix look durable past its own end.
    synced: u64,
    /// fsyncs actually issued.
    ///
    /// The measure of whether group commit is doing anything: the design has a
    /// leader batching concurrent waiters into one `fdatasync`, and without it N
    /// concurrent commits cost N syncs. Wall-clock cannot distinguish those on a
    /// machine with a fast device; a count can.
    syncs: u64,
}

impl WalWriter {
    /// Open, creating if absent. Appends continue from the existing length.
    ///
    /// `base_if_empty` is used **only** when the file has no readable first
    /// record; otherwise the log's own first record names the base. See the
    /// module note for why the file outranks the caller here.
    pub fn open(path: impl AsRef<Path>, base_if_empty: u64) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let sealed = discover_generations(&path)?;
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(io_err)?;
        let active_len = file.metadata().map_err(io_err)?.len();

        // Exactly the first frame — a log can be gigabytes, and reading a fixed
        // prefix instead gets the wrong answer whenever that frame is bigger
        // than the prefix, because `peek_lsn` verifies a CRC over the whole
        // thing and answers `None` when it stops short. The frame says how long
        // it is; ask it.
        let active_base = if active_len == 0 {
            sealed.last().map_or(base_if_empty, Generation::end)
        } else {
            read_first_frame(&mut file, active_len)?
                .as_deref()
                .and_then(Record::peek_lsn)
                .unwrap_or(base_if_empty)
        };

        if sealed
            .last()
            .is_some_and(|generation| generation.end() != active_base)
        {
            return Err(CodecError::Invariant("WAL generations are not contiguous"));
        }

        let mut writer = WalWriter {
            file,
            path,
            sealed,
            active_base,
            active_len,
            version_starts: Vec::new(),
            synced: active_base + active_len,
            syncs: 0,
        };
        writer.rebuild_version_starts()?;
        Ok(writer)
    }

    /// The first retained LSN across all generations.
    #[inline]
    pub fn base(&self) -> u64 {
        self.sealed
            .first()
            .map_or(self.active_base, |generation| generation.base)
    }

    /// Bytes retained across sealed and active generations.
    ///
    /// Not the next LSN — see [`Self::end_lsn`]. This is the size metric
    /// ( checkpoint policy, diagnostics ); the two agree only in a log that has
    /// never been cut, which is what made mixing them up invisible.
    #[inline]
    pub fn len(&self) -> u64 {
        self.sealed
            .iter()
            .map(|generation| generation.len)
            .sum::<u64>()
            + self.active_len
    }

    /// The LSN the next record will occupy.
    #[inline]
    pub fn end_lsn(&self) -> u64 {
        self.active_base + self.active_len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[inline]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// First retained record above `version`, or the logical end when none is.
    ///
    /// A checkpoint may seal through [`Self::end_lsn`] but may reclaim only
    /// through this position. They differ when a commit has appended while its
    /// group fsync is still in flight and therefore is not visible yet.
    pub fn first_lsn_after(&self, version: Version) -> u64 {
        self.version_starts
            .iter()
            .find_map(|&(commit_version, lsn)| (commit_version > version).then_some(lsn))
            .unwrap_or_else(|| self.end_lsn())
    }

    /// Frame and append one record, returning the LSN it was written at.
    ///
    /// Not durable until [`sync`]. The record's own `lsn` field is set to the
    /// offset it lands at, redundantly with its position, so a scan that has
    /// lost framing can detect that rather than confidently decoding rubbish.
    ///
    /// [`sync`]: WalWriter::sync
    pub fn append(
        &mut self,
        rtype: RecType,
        commit_version: Version,
        term: u64,
        body: Vec<u8>,
    ) -> Result<u64> {
        let lsn = self.end_lsn();
        let rec = Record::new(rtype, lsn, commit_version, term, body);
        let bytes = rec.encode();
        write_all_at(&self.file, self.active_len, &bytes)?;
        self.active_len += bytes.len() as u64;
        self.note_version_start(commit_version, lsn);
        Ok(lsn)
    }

    /// Frame and append a *stamped* commit marker — `ShardCommit` or `Abort`.
    ///
    /// Separate from [`WalWriter::append`] so the flag and the body stay in one
    /// place: a caller that set [`FLAG_COMMIT_TIME`] but forgot the body, or the
    /// reverse, would produce a frame that decodes as garbage or as unstamped.
    ///
    /// [`FLAG_COMMIT_TIME`]: crate::wal::record::FLAG_COMMIT_TIME
    pub fn append_marker(
        &mut self,
        rtype: RecType,
        commit_version: Version,
        term: u64,
        time: u64,
    ) -> Result<u64> {
        let lsn = self.end_lsn();
        let rec = Record::commit_marker(rtype, lsn, commit_version, term, time);
        let bytes = rec.encode();
        write_all_at(&self.file, self.active_len, &bytes)?;
        self.active_len += bytes.len() as u64;
        self.note_version_start(commit_version, lsn);
        Ok(lsn)
    }

    /// Append bytes that are **already framed** and already carry their LSNs.
    ///
    /// This is the replica's write path: a leader ships raw on-disk frames, and
    /// they must land where their own headers say they do.
    ///
    /// Not [`WalWriter::append`], which assigns `lsn = end_lsn()` and frames
    /// the body itself. These frames were framed by the leader.
    ///
    /// **The offset check is not a courtesy.** `Record::decode` refuses a
    /// record whose header LSN disagrees with the position it was found at —
    /// that redundancy is how a scan detects lost framing — so writing a shipped
    /// frame anywhere but at `first_lsn - base` makes the entire tail from that
    /// point undecodable. And silently: the scan simply *stops*, recovery
    /// replays a prefix, and the replica comes up missing everything after it
    /// with no error anywhere. Refusing here turns that into a caller's problem
    /// at the moment it is made.
    pub fn append_frames_at(&mut self, first_lsn: u64, frames: &[u8]) -> Result<()> {
        if first_lsn != self.end_lsn() {
            return Err(CodecError::WalCursorNotOnRecordBoundary(first_lsn));
        }
        if frames.is_empty() {
            return Ok(());
        }
        let starts = version_starts(frames, first_lsn)?;
        write_all_at(&self.file, self.active_len, frames)?;
        self.active_len += frames.len() as u64;
        for (version, lsn) in starts {
            self.note_version_start(version, lsn);
        }
        Ok(())
    }

    /// Make everything appended so far durable.
    ///
    /// Cheap to call redundantly: a sync with nothing new outstanding returns
    /// without touching the device, which is what lets every participant in a
    /// batch call it unconditionally.
    pub fn sync(&mut self) -> Result<()> {
        if self.synced >= self.end_lsn() {
            return Ok(());
        }
        self.file.sync_data().map_err(io_err)?;
        self.syncs += 1;
        self.synced = self.end_lsn();
        Ok(())
    }

    /// fsyncs issued over this writer's life.
    #[inline]
    pub fn syncs(&self) -> u64 {
        self.syncs
    }

    /// A second descriptor for the same file.
    ///
    /// So a group-commit leader can fsync **without** holding the log's mutex.
    /// Holding it across the sync would serialize appends behind the device and
    /// exclude from the batch exactly the writers a batch exists to absorb.
    pub fn dup_for_sync(&self) -> Result<File> {
        self.file.try_clone().map_err(io_err)
    }

    /// Record that everything up to `to` is durable.
    ///
    /// Called by the group-commit leader, which does the fsync itself through a
    /// duplicate descriptor and so has to publish the result back here.
    pub fn note_synced(&mut self, to: u64) {
        if to > self.synced {
            self.synced = to;
            self.syncs += 1;
        }
    }

    /// The whole log, for recovery.
    pub fn read_all(&self) -> Result<Vec<u8>> {
        let mut buf = Vec::with_capacity(self.len() as usize);
        for generation in &self.sealed {
            File::open(&generation.path)
                .map_err(io_err)?
                .read_to_end(&mut buf)
                .map_err(io_err)?;
        }
        let mut active = self.file.try_clone().map_err(io_err)?;
        active.rewind().map_err(io_err)?;
        active.read_to_end(&mut buf).map_err(io_err)?;
        Ok(buf)
    }

    /// Drop everything at or above `lsn` — recovery's cut.
    ///
    /// A **correctness** step: by I5 the discarded records form a clean suffix,
    /// and leaving them would let a follower see versions the leader discarded.
    /// The base does not move, because the records below the cut are still here.
    pub fn truncate_to(&mut self, lsn: u64) -> Result<()> {
        if lsn >= self.end_lsn() {
            return Ok(());
        }
        if lsn < self.base() {
            return Err(CodecError::Invariant(
                "WAL suffix cut precedes retained history",
            ));
        }

        if lsn >= self.active_base {
            let keep = lsn - self.active_base;
            self.file.set_len(keep).map_err(io_err)?;
            self.active_len = keep;
        } else {
            self.file.set_len(0).map_err(io_err)?;
            self.active_len = 0;
            self.active_base = lsn;

            let crossing = self
                .sealed
                .iter()
                .position(|generation| generation.base < lsn && lsn < generation.end());
            let keep_count = crossing.map_or_else(
                || {
                    self.sealed
                        .iter()
                        .take_while(|generation| generation.end() <= lsn)
                        .count()
                },
                |index| index + 1,
            );
            for generation in self.sealed[keep_count..].iter().rev() {
                std::fs::remove_file(&generation.path).map_err(io_err)?;
            }
            self.sealed.truncate(keep_count);

            if crossing.is_some() {
                let generation = self.sealed.last_mut().unwrap();
                let keep = lsn - generation.base;
                let file = OpenOptions::new()
                    .write(true)
                    .open(&generation.path)
                    .map_err(io_err)?;
                file.set_len(keep).map_err(io_err)?;
                file.sync_all().map_err(io_err)?;
                generation.len = keep;
            }
        }
        self.file.sync_all().map_err(io_err)?;
        let end_lsn = self.end_lsn();
        self.synced = self.synced.min(end_lsn);
        self.version_starts
            .retain(|&(_, record_lsn)| record_lsn < end_lsn);
        sync_parent(&self.path)?;
        Ok(())
    }

    /// Seal the active file and start an empty generation at its end.
    pub fn rotate(&mut self) -> Result<()> {
        if self.active_len == 0 {
            return Ok(());
        }
        self.file.sync_all().map_err(io_err)?;
        let end = self.end_lsn();
        let sealed_path = generation_path(&self.path, self.active_base);
        if sealed_path.exists() {
            return Err(CodecError::Invariant("WAL generation already exists"));
        }
        std::fs::rename(&self.path, &sealed_path).map_err(io_err)?;
        let rename_sync = sync_parent(&self.path);
        let new_file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&self.path)
        {
            Ok(file) => file,
            Err(error) => {
                let _ = std::fs::rename(&sealed_path, &self.path);
                let _ = sync_parent(&self.path);
                return Err(io_err(error));
            }
        };
        let file_sync = new_file.sync_all().map_err(io_err);
        let create_sync = sync_parent(&self.path);

        self.sealed.push(Generation {
            path: sealed_path,
            base: self.active_base,
            len: self.active_len,
        });
        self.file = new_file;
        self.active_base = end;
        self.active_len = 0;
        self.synced = end;
        rename_sync?;
        file_sync?;
        create_sync?;
        Ok(())
    }

    /// Reclaim sealed generations ending at or below `lsn`.
    pub fn reclaim_through(&mut self, lsn: u64) -> Result<u64> {
        let first_kept = self
            .sealed
            .iter()
            .position(|generation| generation.end() > lsn)
            .unwrap_or(self.sealed.len());
        let mut reclaimed = 0u64;
        for generation in &self.sealed[..first_kept] {
            std::fs::remove_file(&generation.path).map_err(io_err)?;
            reclaimed += generation.len;
        }
        self.sealed.drain(..first_kept);
        let base = self.base();
        self.version_starts
            .retain(|&(_, record_lsn)| record_lsn >= base);
        if reclaimed != 0 {
            sync_parent(&self.path)?;
        }
        Ok(reclaimed)
    }

    fn note_version_start(&mut self, version: Version, lsn: u64) {
        if self
            .version_starts
            .last()
            .is_none_or(|&(last, _)| last != version)
        {
            self.version_starts.push((version, lsn));
        }
    }

    fn rebuild_version_starts(&mut self) -> Result<()> {
        self.version_starts = version_starts(&self.read_all()?, self.base())?;
        Ok(())
    }
}

fn version_starts(bytes: &[u8], base: u64) -> Result<Vec<(Version, u64)>> {
    let mut starts = Vec::new();
    for record in crate::wal::Scanner::new(bytes, base) {
        let record = record?;
        if starts
            .last()
            .is_none_or(|&(version, _)| version != record.commit_version)
        {
            starts.push((record.commit_version, record.lsn));
        }
    }
    Ok(starts)
}

fn generation_path(path: &Path, base: u64) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{base:020}"));
    path.with_file_name(name)
}

fn generation_base(path: &Path, candidate: &Path) -> Option<u64> {
    let active = path.file_name()?.to_str()?;
    let name = candidate.file_name()?.to_str()?;
    let suffix = name.strip_prefix(active)?.strip_prefix('.')?;
    if suffix.len() != 20 || !suffix.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    suffix.parse().ok()
}

fn discover_generations(path: &Path) -> Result<Vec<Generation>> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut generations = Vec::new();
    for entry in std::fs::read_dir(parent).map_err(io_err)? {
        let entry = entry.map_err(io_err)?;
        let entry_path = entry.path();
        let Some(base) = generation_base(path, &entry_path) else {
            continue;
        };
        generations.push(Generation {
            len: entry.metadata().map_err(io_err)?.len(),
            path: entry_path,
            base,
        });
    }
    generations.sort_by_key(|generation| generation.base);
    if generations
        .windows(2)
        .any(|pair| pair[0].end() != pair[1].base)
    {
        return Err(CodecError::Invariant("WAL generations are not contiguous"));
    }
    Ok(generations)
}

fn sync_parent(path: &Path) -> Result<()> {
    File::open(path.parent().unwrap_or_else(|| Path::new(".")))
        .and_then(|dir| dir.sync_all())
        .map_err(io_err)
}

fn catalog(path: &Path, base_if_empty: u64) -> Result<(Vec<Generation>, u64, u64)> {
    // Rollover is a durable rename followed by active-file creation, not one
    // atomic directory snapshot. Read the sealed list on both sides of opening
    // the active file; if it changed, the descriptor and list describe
    // different instants and must not be combined into bogus bounds.
    for _ in 0..8 {
        let sealed = discover_generations(path)?;
        let mut active = match File::open(path) {
            Ok(file) => Some(file),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(io_err(error)),
        };
        let active_len = match active.as_ref() {
            Some(file) => file.metadata().map_err(io_err)?.len(),
            None => 0,
        };
        let active_base = if active_len == 0 {
            sealed.last().map_or(base_if_empty, Generation::end)
        } else {
            read_first_frame(active.as_mut().unwrap(), active_len)?
                .as_deref()
                .and_then(Record::peek_lsn)
                .unwrap_or(base_if_empty)
        };
        if discover_generations(path)? != sealed {
            continue;
        }
        if sealed
            .last()
            .is_some_and(|generation| generation.end() != active_base)
        {
            return Err(CodecError::Invariant("WAL generations are not contiguous"));
        }
        return Ok((sealed, active_base, active_len));
    }
    Err(CodecError::Invariant(
        "WAL generations changed continuously while inspected",
    ))
}

/// Retained LSN bounds across every generation of `path`.
pub fn log_bounds(path: impl AsRef<Path>, base_if_empty: u64) -> Result<(u64, u64)> {
    let (sealed, active_base, active_len) = catalog(path.as_ref(), base_if_empty)?;
    Ok((
        sealed
            .first()
            .map_or(active_base, |generation| generation.base),
        active_base + active_len,
    ))
}

/// Read up to `want` bytes beginning at global `lsn`, crossing generations.
pub fn read_log_range(
    path: impl AsRef<Path>,
    base_if_empty: u64,
    lsn: u64,
    want: usize,
) -> Result<Vec<u8>> {
    let path = path.as_ref();
    let (sealed, active_base, active_len) = catalog(path, base_if_empty)?;
    let base = sealed
        .first()
        .map_or(active_base, |generation| generation.base);
    let end = active_base + active_len;
    if lsn < base || lsn > end {
        return Err(CodecError::WalCursorNotOnRecordBoundary(lsn));
    }

    let mut out = Vec::with_capacity(want.min((end - lsn) as usize));
    let mut at = lsn;
    for generation in sealed {
        if at >= generation.end() || out.len() == want {
            continue;
        }
        if at < generation.base {
            return Err(CodecError::Invariant("WAL generations are not contiguous"));
        }
        let wanted = (want - out.len()).min((generation.end() - at) as usize);
        let got = read_file_range(&generation.path, at - generation.base, wanted, &mut out)?;
        at += got as u64;
        if got < wanted {
            return Ok(out);
        }
    }
    if out.len() < want && at < end {
        let wanted = (want - out.len()).min((end - at) as usize);
        read_file_range(path, at - active_base, wanted, &mut out)?;
    }
    Ok(out)
}

/// Remove every sealed generation belonging to `path`.
pub fn remove_log_generations(path: impl AsRef<Path>) -> Result<()> {
    let path = path.as_ref();
    let generations = discover_generations(path)?;
    for generation in generations {
        std::fs::remove_file(generation.path).map_err(io_err)?;
    }
    sync_parent(path)
}

fn read_file_range(path: &Path, off: u64, want: usize, out: &mut Vec<u8>) -> Result<usize> {
    let mut file = File::open(path).map_err(io_err)?;
    file.seek(std::io::SeekFrom::Start(off)).map_err(io_err)?;
    let old_len = out.len();
    out.resize(old_len + want, 0);
    let mut got = 0usize;
    while got < want {
        let read = file.read(&mut out[old_len + got..]).map_err(io_err)?;
        if read == 0 {
            break;
        }
        got += read;
    }
    out.truncate(old_len + got);
    Ok(got)
}

/// The bytes of a log's first frame, or `None` if it does not have one.
///
/// Two reads: four bytes for the length the frame claims, then the frame. The
/// claim is bounded by the file's own length before anything is allocated,
/// because the field is a `u32` and a torn or zero-filled head is a valid one.
///
/// Shared by `WalWriter::open` and — through the same rule, not a second copy of
/// it — by the replication client, so a follower and the `Db` that replays its
/// bytes cannot disagree about where a log starts.
pub fn read_first_frame(file: &mut File, len: u64) -> Result<Option<Vec<u8>>> {
    use std::io::{Read, Seek};
    if len < 4 {
        return Ok(None);
    }
    file.rewind().map_err(io_err)?;
    let mut head = [0u8; 4];
    file.read_exact(&mut head).map_err(io_err)?;
    let Some(total) = Record::peek_framed_len(&head) else {
        return Ok(None);
    };
    if total as u64 > len {
        return Ok(None);
    }
    let mut frame = vec![0u8; total];
    frame[..4].copy_from_slice(&head);
    file.read_exact(&mut frame[4..]).map_err(io_err)?;
    Ok(Some(frame))
}

#[cfg(unix)]
fn write_all_at(file: &File, off: u64, data: &[u8]) -> Result<()> {
    use std::os::unix::fs::FileExt;
    file.write_all_at(data, off).map_err(io_err)
}

#[cfg(not(unix))]
fn write_all_at(_file: &File, _off: u64, _data: &[u8]) -> Result<()> {
    Err(CodecError::Invariant(
        "the write-ahead log requires a unix target",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wal::record::Scanner;

    fn tmp(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("yesno-wal-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = remove_log_generations(&self.0);
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn an_appended_record_scans_back_at_the_lsn_it_reported() {
        let p = tmp("roundtrip");
        let _c = Cleanup(p.clone());
        let mut w = WalWriter::open(&p, 0).unwrap();

        let a = w
            .append(RecType::ChunkDelta, 1, 0, vec![1, 2, 3, 4])
            .unwrap();
        let b = w.append(RecType::ShardCommit, 1, 0, Vec::new()).unwrap();
        assert_eq!(a, 0, "the first record starts at zero");
        assert!(b > a, "the second starts past the first");
        w.sync().unwrap();

        let bytes = w.read_all().unwrap();
        let got: Vec<Record> = Scanner::new(&bytes, 0).map(|r| r.unwrap()).collect();
        assert_eq!(got.len(), 2);
        // The lsn in the header must equal the offset the record occupies —
        // that redundancy is how a scan detects lost framing.
        assert_eq!(got[0].lsn, a);
        assert_eq!(got[1].lsn, b);
    }

    #[test]
    fn reopening_continues_from_the_existing_length() {
        let p = tmp("reopen");
        let _c = Cleanup(p.clone());
        let first_end = {
            let mut w = WalWriter::open(&p, 0).unwrap();
            w.append(RecType::ChunkDelta, 1, 0, vec![9; 16]).unwrap();
            w.sync().unwrap();
            w.len()
        };

        let mut w = WalWriter::open(&p, 0).unwrap();
        assert_eq!(w.len(), first_end, "reopen must not restart at zero");
        let lsn = w.append(RecType::ChunkDelta, 2, 0, vec![7; 8]).unwrap();
        assert_eq!(
            lsn, first_end,
            "the next record appends, it does not overwrite"
        );
        w.sync().unwrap();

        let bytes = w.read_all().unwrap();
        assert_eq!(Scanner::new(&bytes, 0).count(), 2);
    }

    #[test]
    fn truncation_drops_the_suffix_and_keeps_the_prefix() {
        let p = tmp("truncate");
        let _c = Cleanup(p.clone());
        let mut w = WalWriter::open(&p, 0).unwrap();
        w.append(RecType::ChunkDelta, 1, 0, vec![1; 8]).unwrap();
        let second = w.append(RecType::ChunkDelta, 2, 0, vec![2; 8]).unwrap();
        w.sync().unwrap();

        // Recovery truncates at the first record above `global_cv`.
        w.truncate_to(second).unwrap();
        assert_eq!(w.len(), second);

        let bytes = w.read_all().unwrap();
        let got: Vec<Record> = Scanner::new(&bytes, 0).map(|r| r.unwrap()).collect();
        assert_eq!(got.len(), 1, "only the record below the cut survives");
        assert_eq!(got[0].commit_version, 1);

        // And the next append lands at the cut, not past the old end.
        assert_eq!(
            w.append(RecType::ChunkDelta, 3, 0, vec![3; 8]).unwrap(),
            second
        );
    }

    /// Generation rollover carries the shard's LSN sequence across files.
    ///
    /// Both halves matter and only the second is obvious. If the base lived
    /// solely in memory, every reopen would restart LSNs at zero and the cut
    /// would be undone by the next `open` — so the first record of the next
    /// generation is what a reopen reads, and this asserts a reopen with a
    /// **wrong** fallback still gets the right answer from the file.
    #[test]
    fn generation_rollover_carries_the_base_and_the_log_remembers_it() {
        let p = tmp("prefix-cut");
        let _c = Cleanup(p.clone());

        let (cut_at, second) = {
            let mut w = WalWriter::open(&p, 0).unwrap();
            w.append(RecType::ChunkDelta, 1, 0, vec![1; 64]).unwrap();
            w.append(RecType::ChunkDelta, 2, 0, vec![2; 64]).unwrap();
            w.sync().unwrap();
            let end = w.end_lsn();
            assert_eq!(w.base(), 0, "a fresh log starts at the origin");

            w.rotate().unwrap();
            w.reclaim_through(end).unwrap();
            assert_eq!(w.base(), end, "the cut must advance the base");
            assert_eq!(w.len(), 0, "and leave no bytes");
            assert_eq!(w.end_lsn(), end, "so the next lsn is unchanged by it");

            // Not zero: the sequence continues.
            let second = w.append(RecType::ChunkDelta, 3, 0, vec![3; 32]).unwrap();
            assert_eq!(second, end);
            w.sync().unwrap();
            (end, second)
        };

        // Reopened with a deliberately wrong fallback. The file outranks it.
        let w = WalWriter::open(&p, 999_999).unwrap();
        assert_eq!(
            w.base(),
            cut_at,
            "a reopen must take the base from the log's own first record"
        );
        assert_eq!(w.end_lsn(), second + Record::framed_len(32) as u64);

        let bytes = w.read_all().unwrap();
        let got: Vec<Record> = Scanner::new(&bytes, w.base()).map(|r| r.unwrap()).collect();
        assert_eq!(got.len(), 1, "only the post-cut record is still here");
        assert_eq!(
            got[0].lsn, second,
            "and it scans back at the lsn it was written at"
        );
    }

    #[test]
    fn a_logical_read_crosses_from_a_sealed_generation_into_the_active_one() {
        let p = tmp("generation-range");
        let _c = Cleanup(p.clone());
        let mut w = WalWriter::open(&p, 0).unwrap();
        w.append(RecType::ChunkDelta, 1, 0, vec![1; 24]).unwrap();
        let boundary = w.end_lsn();
        w.rotate().unwrap();
        w.append(RecType::ChunkDelta, 2, 0, vec![2; 24]).unwrap();
        w.sync().unwrap();

        assert!(generation_path(&p, 0).exists());
        assert_eq!(std::fs::metadata(&p).unwrap().len(), w.end_lsn() - boundary);
        let bytes = read_log_range(&p, 0, 0, w.end_lsn() as usize).unwrap();
        let got: Vec<Record> = Scanner::new(&bytes, 0).map(|r| r.unwrap()).collect();
        assert_eq!(
            got.iter()
                .map(|record| record.commit_version)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
    }

    #[test]
    fn open_repairs_the_crash_window_between_sealing_and_creating_active() {
        let p = tmp("generation-crash-window");
        let _c = Cleanup(p.clone());
        let end = {
            let mut w = WalWriter::open(&p, 0).unwrap();
            w.append(RecType::ChunkDelta, 1, 0, vec![1; 24]).unwrap();
            w.sync().unwrap();
            w.end_lsn()
        };
        std::fs::rename(&p, generation_path(&p, 0)).unwrap();

        let w = WalWriter::open(&p, 999_999).unwrap();
        assert!(p.exists(), "open did not recreate the active generation");
        assert_eq!(w.base(), 0);
        assert_eq!(w.end_lsn(), end);
        assert_eq!(Scanner::new(&w.read_all().unwrap(), 0).count(), 1);
    }

    #[test]
    fn recovery_can_cut_a_suffix_that_starts_inside_a_sealed_generation() {
        let p = tmp("generation-recovery-cut");
        let _c = Cleanup(p.clone());
        let mut w = WalWriter::open(&p, 0).unwrap();
        w.append(RecType::ChunkDelta, 1, 0, vec![1; 24]).unwrap();
        w.rotate().unwrap();
        w.append(RecType::ChunkDelta, 2, 0, vec![2; 24]).unwrap();
        let cut = w.end_lsn();
        w.append(RecType::ChunkDelta, 3, 0, vec![3; 24]).unwrap();
        w.rotate().unwrap();
        w.append(RecType::ChunkDelta, 4, 0, vec![4; 24]).unwrap();

        w.truncate_to(cut).unwrap();
        let bytes = w.read_all().unwrap();
        let got: Vec<Record> = Scanner::new(&bytes, 0).map(|r| r.unwrap()).collect();
        assert_eq!(
            got.iter()
                .map(|record| record.commit_version)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        drop(w);
        assert_eq!(WalWriter::open(&p, cut).unwrap().end_lsn(), cut);
    }

    /// An empty log cannot answer for itself, which is exactly what a cut leaves
    /// behind — so the fallback is what stops a checkpointed shard restarting.
    #[test]
    fn an_emptied_log_takes_its_base_from_the_fallback() {
        let p = tmp("empty-base");
        let _c = Cleanup(p.clone());
        {
            let mut w = WalWriter::open(&p, 0).unwrap();
            w.append(RecType::ChunkDelta, 1, 0, vec![1; 64]).unwrap();
            let end = w.end_lsn();
            w.rotate().unwrap();
            w.reclaim_through(end).unwrap();
        }
        let mut w = WalWriter::open(&p, 4_096).unwrap();
        assert_eq!(w.base(), 4_096);
        assert_eq!(
            w.append(RecType::ChunkDelta, 2, 0, vec![2; 8]).unwrap(),
            4_096,
            "the first record after a cut continues the sequence"
        );
    }

    #[test]
    fn truncating_past_the_end_is_a_no_op() {
        let p = tmp("truncate-noop");
        let _c = Cleanup(p.clone());
        let mut w = WalWriter::open(&p, 0).unwrap();
        w.append(RecType::ChunkDelta, 1, 0, vec![1; 8]).unwrap();
        let len = w.len();
        w.truncate_to(len + 1_000).unwrap();
        assert_eq!(w.len(), len);
    }

    #[test]
    fn a_redundant_sync_costs_nothing() {
        let p = tmp("resync");
        let _c = Cleanup(p.clone());
        let mut w = WalWriter::open(&p, 0).unwrap();
        w.append(RecType::ChunkDelta, 1, 0, vec![1; 8]).unwrap();
        w.sync().unwrap();
        // Every participant in a batch calls this unconditionally, so it has to
        // be free when there is nothing outstanding.
        w.sync().unwrap();
        w.sync().unwrap();
        assert_eq!(w.len(), w.synced);
    }
}
