//! Canonical SHA-256 commitments for archive bases and WAL chains.

use sha2::{Digest, Sha256};
use yesno_core::wal::Scanner;

use crate::archive::{pb, ArchiveError, SCHEMA_VERSION};

const FINGERPRINT_DOMAIN: &[u8] = b"yesno-archive-history-v1";

fn field(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn finish(hasher: Sha256) -> Vec<u8> {
    hasher.finalize().to_vec()
}

pub(crate) fn base_anchor(manifest: &pb::BaseManifest) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(FINGERPRINT_DOMAIN);
    hasher.update(b"base");
    hasher.update(SCHEMA_VERSION.to_le_bytes());
    field(&mut hasher, &manifest.database_uuid);
    hasher.update(manifest.term.to_le_bytes());
    hasher.update(manifest.archive_generation.to_le_bytes());
    hasher.update(manifest.checkpoint_version.to_le_bytes());
    hasher.update(manifest.recovered_version.to_le_bytes());
    for file in &manifest.files {
        field(&mut hasher, file.name.as_bytes());
        field(&mut hasher, file.object_key.as_bytes());
        hasher.update(file.size.to_le_bytes());
        hasher.update(file.crc32c.to_le_bytes());
    }
    for cursor in &manifest.wal_cursors {
        hasher.update(cursor.shard.to_le_bytes());
        hasher.update(cursor.archived_lsn.to_le_bytes());
    }
    finish(hasher)
}

pub(crate) fn base_cursor_fingerprint(anchor: &[u8], shard: u32, lsn: u64) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(FINGERPRINT_DOMAIN);
    hasher.update(b"cursor");
    field(&mut hasher, anchor);
    hasher.update(shard.to_le_bytes());
    hasher.update(lsn.to_le_bytes());
    finish(hasher)
}

pub(crate) fn wal_fingerprint(
    previous: &[u8],
    database_uuid: &[u8],
    term: u32,
    shard: u32,
    first_lsn: u64,
    last_lsn: u64,
    records: &[u8],
) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(FINGERPRINT_DOMAIN);
    hasher.update(b"wal");
    field(&mut hasher, previous);
    field(&mut hasher, database_uuid);
    hasher.update(term.to_le_bytes());
    hasher.update(shard.to_le_bytes());
    hasher.update(first_lsn.to_le_bytes());
    hasher.update(last_lsn.to_le_bytes());
    field(&mut hasher, records);
    finish(hasher)
}

pub(crate) fn wal_versions(
    records: &[u8],
    first_lsn: u64,
    last_lsn: u64,
) -> Result<(u64, u64), ArchiveError> {
    let mut scanner = Scanner::new(records, first_lsn);
    let mut first = None;
    let mut last = None;
    for record in &mut scanner {
        let record = record?;
        first.get_or_insert(record.commit_version);
        last = Some(record.commit_version);
    }
    if scanner.stopped_at() != last_lsn || first.is_none() {
        return Err(format!(
            "WAL object {first_lsn}..{last_lsn} is not a complete non-empty frame range"
        )
        .into());
    }
    Ok((first.unwrap(), last.unwrap()))
}

pub(crate) fn validate_fingerprint(value: &[u8], what: &str) -> Result<(), ArchiveError> {
    if value.len() != 32 {
        return Err(format!("{what} is {} bytes, expected a SHA-256 value", value.len()).into());
    }
    Ok(())
}
