//! One archive on disk, for the tests that need a real store rather than a
//! hand-built plan.
//!
//! It lives here rather than in either module that uses it because neither
//! owns it: `metrics` asserts what a pass *publishes*, `gc` asserts what a
//! pass *decides*, and both need the same smallest archive a real
//! `ArchiveStore` will accept. Hand-built manifests are enough for `gc::plan`,
//! which is pure, and not enough for anything that goes through
//! `validate_base_manifest` -- the uuid has to be sixteen bytes and the
//! history anchor has to match the contents it commits to.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use prost::Message;

use crate::archive::{pb, SCHEMA_VERSION, STATE_OBJECT};

const UUID: [u8; 16] = [0x11; 16];
pub(crate) const HEX: &str = "11111111111111111111111111111111";

pub(crate) fn write(root: &Path, key: &str, bytes: &[u8]) {
    let path = root.join(key);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

/// An archive with one live base, plus whatever `junk` keys the caller
/// wants the pass to fail to classify.
pub(crate) fn archive(root: &Path, junk: &[&str]) {
    let dir = format!("db/{HEX}/term/0000000000/base/{:020}", 1);
    let manifest_key = format!("{dir}/manifest.pb");
    let file_key = format!("{dir}/MANIFEST");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_micros() as u64;

    let mut manifest = pb::BaseManifest {
        schema_version: SCHEMA_VERSION,
        database_uuid: UUID.to_vec(),
        term: 0,
        archive_generation: 1,
        checkpoint_version: 10,
        checkpoint_time: now,
        files: vec![pb::ArchiveFile {
            name: "MANIFEST".into(),
            object_key: file_key.clone(),
            size: 1,
            crc32c: 0,
            sparse: false,
            extents: Vec::new(),
        }],
        wal_cursors: vec![pb::WalCursor {
            shard: 0,
            archived_lsn: 0,
            history_fingerprint: Vec::new(),
        }],
        ..Default::default()
    };
    manifest.history_anchor = crate::history::base_anchor(&manifest);
    for cursor in &mut manifest.wal_cursors {
        cursor.history_fingerprint = crate::history::base_cursor_fingerprint(
            &manifest.history_anchor,
            cursor.shard,
            cursor.archived_lsn,
        );
    }

    let state = pb::ArchiveState {
        schema_version: SCHEMA_VERSION,
        database_uuid: UUID.to_vec(),
        latest_base_manifest: manifest_key.clone(),
        wal_cursors: vec![pb::WalCursor {
            shard: 0,
            archived_lsn: 0,
            history_fingerprint: vec![0; 32],
        }],
        ..Default::default()
    };

    write(root, STATE_OBJECT, &state.encode_to_vec());
    write(root, &manifest_key, &manifest.encode_to_vec());
    write(root, &file_key, b"x");
    for key in junk {
        write(root, key, b"who put this here");
    }
}
