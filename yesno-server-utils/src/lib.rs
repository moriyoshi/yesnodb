//! Operational backup and object-archive utilities for `yesnod`.
//!
//! This crate depends on the daemon's public control client and replication
//! protocol; `yesno-server` never depends back on it as a crate. The `yesnod`
//! binary can invoke the sibling `yesnoctl` binary before a follower starts to
//! seed an empty directory from an archive. Keeping the utilities on the client
//! side prevents object-storage and backup workflow policy from becoming daemon
//! library dependencies. Filesystem snapshot creation stays with the daemon;
//! this crate only consumes its leases.

pub mod archive;
pub mod basebackup;
mod deferred;
/// One archive on disk, shared by the tests that need a real store.
#[cfg(test)]
mod fixture;
pub mod gc;
mod history;
pub mod lease;
pub mod metrics;
pub mod restore;
pub mod seed;
pub mod sidecar;
pub mod transport;

/// Attach the path to an `std::io::Error` before it is boxed into an error type.
///
/// **`std::io::Error` carries no path.** `File::open( p )` failing yields
/// exactly "No such file or directory ( os error 2 )", and this crate's error
/// aliases are boxed `dyn Error`, so the `?` that boxes one is where the only
/// identifying information is lost. The archive sidecar is a detached task
/// whose error surfaces through a metric wait many seconds later and in another
/// process, so that string is the entire report.
///
/// This is not hypothetical. The sidecar died with a bare `os error 2` twice --
/// `pitr_retention.py` on 2026-09-28 and `archive.py` on 2026-09-29 -- and
/// neither occurrence could say **which** file was missing, so neither could be
/// diagnosed. See `the-archive-sidecar-fails-with-enoent-under-load`.
///
/// It lives here rather than in any one module because four of them need it and
/// none owns it; `transport` is used *by* `archive`, so importing it from there
/// would invert the dependency.
pub(crate) fn ctx(
    doing: &str,
    path: &std::path::Path,
    error: std::io::Error,
) -> Box<dyn std::error::Error + Send + Sync> {
    format!("cannot {doing} '{}': {error}", path.display()).into()
}

/// What a published file claims to be, and what it costs: `( length, allocated )`.
///
/// A shard image is grown to a whole 1 GiB mapping segment when it is created,
/// so a database holding kilobytes has a file reporting gigabytes -- and every
/// path that copies one now writes those holes as holes rather than as zeros.
/// The length alone therefore stopped answering the question an operator asks
/// of a backup report, which is how much room the copy needs: it over-reports
/// by whatever fraction of the image is empty, which on a small database is
/// very nearly all of it.
///
/// Both numbers are reported, neither in place of the other. The length is what
/// a reader of the directory sees, and what copying it without hole detection
/// would cost. The allocated size is what this filesystem charges for it.
///
/// `st_blocks` counts 512-byte units by POSIX, whatever the filesystem's own
/// block size is, so the multiplier is fixed rather than queried. It measures
/// the copy on the filesystem holding it and not a property of the data: a
/// store with no support for holes reports the two numbers equal, and one that
/// compresses or shares extents can report less than the data occupies.
///
/// This lives here for the same reason [`ctx`] does. Both the base backup and
/// the archive restore need it, neither owns the other, and the two reports
/// have to mean the same thing by the same rule rather than by two readings of
/// the same `stat`.
pub(crate) fn occupancy(meta: &std::fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt as _;
    (meta.len(), meta.blocks().saturating_mul(512))
}

#[cfg(test)]
mod occupancy_tests {
    use std::io::Write as _;

    fn measure(file: &std::fs::File) -> (u64, u64) {
        file.sync_all().unwrap();
        super::occupancy(&file.metadata().unwrap())
    }

    /// A declared length costs nothing until something is written into it.
    ///
    /// This is the whole reason the second number exists: both reports would
    /// otherwise say a mebibyte about a file holding one page.
    #[test]
    fn a_declared_length_is_not_an_allocation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sparse");
        let mut file = std::fs::File::create(&path).unwrap();
        file.set_len(1 << 20).unwrap();
        let (len, empty) = measure(&file);
        assert_eq!(len, 1 << 20);
        assert_eq!(empty, 0, "a file of nothing but hole occupies nothing");

        file.write_all(&[7u8; 4096]).unwrap();
        let (len, held) = measure(&file);
        assert_eq!(len, 1 << 20, "writing inside the length does not extend it");
        assert!(held > 0, "a written page occupies something");
        assert!(
            held < len / 16,
            "{held} of {len} is not a mostly-empty file"
        );
    }

    /// A file with no holes occupies about its length, and that brackets the
    /// multiplier.
    ///
    /// `st_blocks` counts 512-byte units whatever the filesystem's block size
    /// is. Reading it as blocks of 4096 -- the obvious wrong guess, and the
    /// number `stat` prints beside it as `IO Block` -- would report eight
    /// times the length here, so the upper bound is the test. The lower bound
    /// is the one that fails if the units were taken as bytes.
    #[test]
    fn a_file_with_no_holes_occupies_about_its_length() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dense");
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(&[3u8; 1 << 20]).unwrap();
        let (len, held) = measure(&file);
        assert_eq!(len, 1 << 20);
        assert!(held >= len, "{held} is less than the {len} bytes written");
        assert!(held < 2 * len, "{held} is implausible for {len} bytes");
    }
}
