//! Shared filesystem contract for local and Fargate snapshot staging.
//!
//! A worker copies only the bounded database-file set recorded by the snapshot
//! lease. It writes a sibling partial directory and renames it only after every
//! file and the directory have been synchronized, so `yesnod` can never publish
//! a half-populated Fargate result.

use std::io::Read as _;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SnapshotFile {
    pub name: String,
    pub size: u64,
}

pub(crate) fn database_files(dir: &Path) -> Result<Vec<SnapshotFile>, std::io::Error> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let database_file = name == "MANIFEST"
            || name == "UUID"
            || (name.starts_with("shard-") && (name.ends_with(".yno") || name.contains(".wal")));
        if database_file {
            files.push(SnapshotFile {
                name,
                size: entry.metadata()?.len(),
            });
        }
    }
    files.sort_unstable_by(|left, right| left.name.cmp(&right.name));
    Ok(files)
}

#[allow(
    dead_code,
    reason = "used by the separately compiled yesno-snapshot-stage binary"
)]
pub(crate) fn copy_database_snapshot(source: &Path, target: &Path) -> Result<(), std::io::Error> {
    let files = database_files(source)?;
    if files.is_empty() {
        return Err(std::io::Error::other(
            "mounted snapshot contains no database files",
        ));
    }
    if target.try_exists()? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!(
                "snapshot staging target '{}' already exists",
                target.display()
            ),
        ));
    }
    let parent = target.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "snapshot staging target '{}' has no parent",
                target.display()
            ),
        )
    })?;
    let target_name = target.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "snapshot staging target has no file name",
        )
    })?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let partial = parent.join(format!(
        ".{}.partial-{}-{nonce:032x}",
        target_name.to_string_lossy(),
        std::process::id()
    ));
    std::fs::create_dir(&partial)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&partial, std::fs::Permissions::from_mode(0o700))?;
    }
    let copy = (|| {
        for file in &files {
            let mut input = std::fs::File::open(source.join(&file.name))?.take(file.size);
            let mut output = std::fs::File::create(partial.join(&file.name))?;
            let copied = std::io::copy(&mut input, &mut output)?;
            if copied != file.size {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    format!(
                        "database file '{}' ended at {copied} bytes while its snapshot size was {}",
                        file.name, file.size
                    ),
                ));
            }
            output.sync_all()?;
        }
        std::fs::File::open(&partial)?.sync_all()?;
        std::fs::rename(&partial, target)?;
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if copy.is_err() {
        let _ = std::fs::remove_dir_all(&partial);
    }
    copy
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staging_copies_only_database_files_and_publishes_atomically() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let target = temp.path().join("target");
        std::fs::create_dir(&source).unwrap();
        for (name, contents) in [
            ("MANIFEST", b"manifest".as_slice()),
            ("UUID", b"uuid".as_slice()),
            ("shard-0.yno", b"data".as_slice()),
            ("shard-0.wal.0001", b"wal".as_slice()),
            ("operator-notes", b"do not copy".as_slice()),
        ] {
            std::fs::write(source.join(name), contents).unwrap();
        }

        copy_database_snapshot(&source, &target).unwrap();

        assert_eq!(std::fs::read(target.join("MANIFEST")).unwrap(), b"manifest");
        assert_eq!(std::fs::read(target.join("UUID")).unwrap(), b"uuid");
        assert_eq!(std::fs::read(target.join("shard-0.yno")).unwrap(), b"data");
        assert_eq!(
            std::fs::read(target.join("shard-0.wal.0001")).unwrap(),
            b"wal"
        );
        assert!(!target.join("operator-notes").exists());
        assert!(copy_database_snapshot(&source, &target).is_err());
        assert!(std::fs::read_dir(temp.path()).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains(".partial-")));
    }
}
