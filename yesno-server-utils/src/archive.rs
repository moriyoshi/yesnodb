//! Durable object-storage primitives for the `yesno-archive` sidecar.
//!
//! WAL generation events are deliberately absent from this module. A sealed
//! generation may be reclaimed before an event consumer opens its pathname, so
//! event delivery cannot be used as a file-lifetime protocol. The archive reads
//! WAL bytes from the replication stream and acknowledges an LSN only after the
//! corresponding object and protobuf cursor are durable.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use futures::TryStreamExt;
use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore, ObjectStoreExt, PutMode, PutOptions, PutPayload, UpdateVersion};
use prost::Message;
use tokio::io::AsyncReadExt;
use url::Url;
use yesno_core::wal::Scanner;

pub mod pb {
    //! Protobuf messages constituting the on-object-store archive format.
    include!(concat!(env!("OUT_DIR"), "/yesno.archive.v1.rs"));
}

/// One sidecar failure, safe to move across its asynchronous worker tasks.
pub type ArchiveError = Box<dyn std::error::Error + Send + Sync>;

/// Archive metadata schema emitted by this version of the sidecar.
pub const SCHEMA_VERSION: u32 = 2;
const MULTIPART_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const STATE_OBJECT: &str = "state.pb";
pub(crate) const WRITER_OBJECT: &str = "writer.pb";

/// Canonical history advance produced by one replication batch.
///
/// Objects are normalized to individual WAL frames, so these endpoints do not
/// depend on how the transport happened to group those frames.
#[derive(Debug)]
pub struct WalUpload {
    pub first_lsn: u64,
    pub last_lsn: u64,
    pub previous_fingerprint: Vec<u8>,
    pub history_fingerprint: Vec<u8>,
    pub frames: Vec<pb::WalObject>,
}

/// An object store rooted at the prefix named by the configured URL.
#[derive(Clone)]
pub struct ArchiveStore {
    pub(crate) inner: Arc<dyn ObjectStore>,
    pub(crate) prefix: ObjectPath,
    pub(crate) local_prefix: Option<PathBuf>,
    pub(crate) state_version: Arc<Mutex<Option<UpdateVersion>>>,
    pub(crate) lease_version: Arc<Mutex<Option<UpdateVersion>>>,
    pub(crate) lease_ttl_secs: Arc<Mutex<u64>>,
    pub(crate) owner_id: Arc<Mutex<Vec<u8>>>,
}

impl std::fmt::Debug for ArchiveStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArchiveStore")
            .field("store", &self.inner.to_string())
            .field("prefix", &self.prefix)
            .finish()
    }
}

impl ArchiveStore {
    /// Connect to `file://`, `s3://`, or an S3-compatible endpoint configured
    /// through the object-store crate's standard environment options.
    pub fn connect(value: &str) -> Result<Self, ArchiveError> {
        let url = Url::parse(value)?;
        let local_prefix = if url.scheme() == "file" {
            Some(
                url.to_file_path()
                    .map_err(|()| format!("invalid file archive URL {value}"))?,
            )
        } else {
            None
        };
        let (inner, prefix) = object_store::parse_url_opts(&url, std::env::vars())?;
        Ok(Self {
            inner: Arc::from(inner),
            prefix,
            local_prefix,
            state_version: Arc::new(Mutex::new(None)),
            lease_version: Arc::new(Mutex::new(None)),
            lease_ttl_secs: Arc::new(Mutex::new(0)),
            owner_id: Arc::new(Mutex::new(Vec::new())),
        })
    }

    #[cfg(test)]
    pub(crate) fn new(inner: Arc<dyn ObjectStore>, prefix: ObjectPath) -> Self {
        Self {
            inner,
            prefix,
            local_prefix: None,
            state_version: Arc::new(Mutex::new(None)),
            lease_version: Arc::new(Mutex::new(None)),
            lease_ttl_secs: Arc::new(Mutex::new(0)),
            owner_id: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub(crate) fn path(&self, relative: &str) -> ObjectPath {
        let path = if self.prefix.is_root() {
            relative.to_owned()
        } else {
            format!("{}/{relative}", self.prefix)
        };
        // Every component is generated from decimal/hex identifiers or fixed
        // ASCII names. Parse the complete key so '/' remains a delimiter;
        // `Path::from`/`join` takes one PathPart and would encode it as `%2F`.
        ObjectPath::parse(path).expect("generated archive object key is valid")
    }

    /// Load the authoritative remote state and remember the version needed for
    /// the next compare-and-swap publication. Missing state starts a new archive.
    pub async fn load_state(&self) -> Result<Option<pb::ArchiveState>, ArchiveError> {
        let path = self.path(STATE_OBJECT);
        let result = match self.inner.get(&path).await {
            Ok(result) => result,
            Err(object_store::Error::NotFound { .. }) => {
                *self.state_version.lock().unwrap() = None;
                return Ok(None);
            }
            Err(error) => return Err(error.into()),
        };
        let version = UpdateVersion {
            e_tag: result.meta.e_tag.clone(),
            version: result.meta.version.clone(),
        };
        let bytes = result.bytes().await?;
        let state = pb::ArchiveState::decode(bytes)?;
        validate_state(&state)?;
        if self.local_prefix.is_none() {
            *self.state_version.lock().unwrap() = Some(version);
        }
        Ok(Some(state))
    }

    /// Publish state remotely with compare-and-swap, then atomically refresh
    /// its local cache. Callers must not ACK a cursor until this succeeds.
    pub async fn publish_state(
        &self,
        local_path: &Path,
        state: &pb::ArchiveState,
    ) -> Result<(), ArchiveError> {
        validate_state(state)?;
        let owner = self.owner_id.lock().unwrap().clone();
        if owner.is_empty() || state.writer_id != owner {
            return Err("archive state is not owned by this writer lease".into());
        }
        let bytes = state.encode_to_vec();
        let path = self.path(STATE_OBJECT);
        let result = if self.local_prefix.is_some() {
            self.inner.put(&path, PutPayload::from(bytes.clone())).await
        } else {
            let ttl_secs = *self.lease_ttl_secs.lock().unwrap();
            if ttl_secs == 0 {
                return Err("archive writer lease has no renewal lifetime".into());
            }
            // Prove and extend lease ownership before publishing state. This
            // closes the interval between another writer taking an expired
            // lease and that writer claiming state.pb.
            self.renew_writer_lease(ttl_secs).await?;
            let mode = self
                .state_version
                .lock()
                .unwrap()
                .clone()
                .map_or(PutMode::Create, PutMode::Update);
            self.inner
                .put_opts(
                    &path,
                    PutPayload::from(bytes.clone()),
                    PutOptions::from(mode),
                )
                .await
        };
        let result = match result {
            Ok(result) => result,
            Err(object_store::Error::AlreadyExists { .. })
            | Err(object_store::Error::Precondition { .. }) => {
                return Err("archive writer was fenced by a concurrent state update".into());
            }
            Err(error) => return Err(error.into()),
        };
        if self.local_prefix.is_none() {
            *self.state_version.lock().unwrap() = Some(result.into());
        }
        write_local_state(local_path, bytes).await
    }

    pub(crate) async fn get_bytes(&self, key: &str) -> Result<Bytes, ArchiveError> {
        Ok(self.inner.get(&self.path(key)).await?.bytes().await?)
    }

    pub(crate) async fn put_immutable(
        &self,
        key: &str,
        bytes: Vec<u8>,
    ) -> Result<(), ArchiveError> {
        match self
            .inner
            .put_opts(
                &self.path(key),
                PutPayload::from(bytes.clone()),
                PutOptions::from(PutMode::Create),
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(object_store::Error::AlreadyExists { .. }) => {
                if self.get_bytes(key).await?.as_ref() == bytes.as_slice() {
                    Ok(())
                } else {
                    Err(format!(
                        "immutable archive object '{key}' already exists with different bytes"
                    )
                    .into())
                }
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Permanently remove one object.
    ///
    /// The only deleting call in this crate, and it is deliberately the whole
    /// interface: reclamation decides *what* elsewhere, under the writer lease,
    /// and this does not second-guess it. A missing object is not an error —
    /// reclamation is idempotent, so a retry after a partial pass must succeed.
    pub(crate) async fn delete_object(&self, key: &str) -> Result<(), ArchiveError> {
        match self.inner.delete(&self.path(key)).await {
            Ok(()) => Ok(()),
            Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) async fn list_relative(&self, prefix: &str) -> Result<Vec<String>, ArchiveError> {
        let objects = self
            .inner
            .list(Some(&self.path(prefix)))
            .try_collect::<Vec<_>>()
            .await?;
        let root = self.prefix.as_ref();
        let mut relative = Vec::with_capacity(objects.len());
        for object in objects {
            let full = object.location.as_ref();
            let value = if root.is_empty() {
                full
            } else {
                full.strip_prefix(root)
                    .and_then(|tail| tail.strip_prefix('/'))
                    .ok_or("object-store listing escaped the configured archive prefix")?
            };
            relative.push(value.to_owned());
        }
        relative.sort();
        Ok(relative)
    }

    /// Store a replication batch as canonical per-frame immutable objects.
    pub async fn put_wal(
        &self,
        database_uuid: &[u8],
        term: u32,
        shard: u32,
        lsn_range: std::ops::Range<u64>,
        previous_fingerprint: &[u8],
        records: Vec<u8>,
    ) -> Result<WalUpload, ArchiveError> {
        let first_lsn = lsn_range.start;
        let last_lsn = lsn_range.end;
        crate::history::validate_fingerprint(previous_fingerprint, "previous WAL fingerprint")?;
        if first_lsn.checked_add(records.len() as u64) != Some(last_lsn) {
            return Err("WAL object LSN range does not match its byte length".into());
        }
        let mut scanner = Scanner::new(&records, first_lsn);
        let mut frame_start = first_lsn;
        let mut frames = Vec::new();
        while let Some(record) = scanner.next() {
            let record = record?;
            let frame_end = scanner.stopped_at();
            // `None` for an unstamped record, and for every record that is not
            // a commit marker. Carried as a hint so a wall-clock restore can
            // narrow which objects to fetch; the frames stay the authority.
            let time = record.commit_time()?.unwrap_or(0);
            frames.push((frame_start, frame_end, record.commit_version, time));
            frame_start = frame_end;
        }
        if scanner.stopped_at() != last_lsn || frames.is_empty() {
            return Err(format!(
                "WAL batch {first_lsn}..{last_lsn} is not a complete non-empty frame range"
            )
            .into());
        }

        let database = uuid_hex(database_uuid)?;
        let mut fingerprint = previous_fingerprint.to_vec();
        let mut descriptors = Vec::with_capacity(frames.len());
        for (frame_start, frame_end, version, time) in frames {
            let start = usize::try_from(frame_start - first_lsn)?;
            let end = usize::try_from(frame_end - first_lsn)?;
            let frame = &records[start..end];
            let key = format!(
                "db/{database}/term/{term:010}/wal/{shard:04}/{frame_start:020}-{frame_end:020}.wal"
            );
            let next = crate::history::wal_fingerprint(
                &fingerprint,
                database_uuid,
                term,
                shard,
                frame_start,
                frame_end,
                frame,
            );
            let descriptor = pb::WalObject {
                schema_version: SCHEMA_VERSION,
                database_uuid: database_uuid.to_vec(),
                term,
                shard,
                first_lsn: frame_start,
                last_lsn: frame_end,
                object_key: key.clone(),
                size: frame.len() as u64,
                crc32c: crc32c::crc32c(frame),
                previous_fingerprint: fingerprint,
                history_fingerprint: next.clone(),
                first_version: version,
                last_version: version,
                first_time: time,
                last_time: time,
            };
            self.put_immutable(&key, frame.to_vec()).await?;
            self.put_immutable(&format!("{key}.pb"), descriptor.encode_to_vec())
                .await?;
            fingerprint = next;
            descriptors.push(descriptor);
        }
        Ok(WalUpload {
            first_lsn,
            last_lsn,
            previous_fingerprint: previous_fingerprint.to_vec(),
            history_fingerprint: fingerprint,
            frames: descriptors,
        })
    }

    /// Upload a complete base directory and publish its manifest last. Only
    /// regular database files are included; lock and reader-registry files are
    /// process-local state and are intentionally excluded.
    pub async fn put_base(
        &self,
        source_dir: &Path,
        mut manifest: pb::BaseManifest,
    ) -> Result<(String, pb::BaseManifest), ArchiveError> {
        validate_uuid(&manifest.database_uuid)?;
        let owner = self.owner_id.lock().unwrap().clone();
        if owner.is_empty() {
            return Err("base publication requires a writer lease".into());
        }
        manifest.schema_version = SCHEMA_VERSION;
        manifest.files.clear();
        manifest.history_anchor.clear();
        manifest.wal_cursors.sort_by_key(|cursor| cursor.shard);
        for cursor in &mut manifest.wal_cursors {
            cursor.history_fingerprint.clear();
        }

        let base = format!(
            "db/{}/term/{:010}/base/{:020}",
            uuid_hex(&manifest.database_uuid)?,
            manifest.term,
            manifest.archive_generation
        );
        let writer = uuid_hex(&owner)?;
        for (name, path) in database_files(source_dir)? {
            let key = format!("{base}/writers/{writer}/files/{name}");
            let file = self.put_file(&path, &key).await?;
            manifest.files.push(pb::ArchiveFile {
                name,
                object_key: key,
                size: file.size,
                crc32c: file.crc32c,
            });
        }
        if manifest.files.is_empty() {
            return Err(format!(
                "base directory '{}' contains no database files",
                source_dir.display()
            )
            .into());
        }

        let anchor = crate::history::base_anchor(&manifest);
        manifest.history_anchor = anchor.clone();
        for cursor in &mut manifest.wal_cursors {
            cursor.history_fingerprint =
                crate::history::base_cursor_fingerprint(&anchor, cursor.shard, cursor.archived_lsn);
        }
        validate_base_manifest(&manifest)?;
        let manifest_key = format!("{base}/manifest.pb");
        self.put_immutable(&manifest_key, manifest.encode_to_vec())
            .await?;
        Ok((manifest_key, manifest))
    }

    async fn put_file(&self, source: &Path, key: &str) -> Result<UploadedFile, ArchiveError> {
        let size = tokio::fs::metadata(source).await?.len();
        if size == 0 {
            self.inner
                .put(&self.path(key), PutPayload::from(Vec::<u8>::new()))
                .await?;
            return Ok(UploadedFile { size, crc32c: 0 });
        }

        let mut source_file = tokio::fs::File::open(source).await?;
        let mut upload = self.inner.put_multipart(&self.path(key)).await?;
        let mut checksum = 0u32;
        let mut transferred = 0u64;
        loop {
            let mut part = vec![0u8; MULTIPART_BYTES];
            let count = match source_file.read(&mut part).await {
                Ok(count) => count,
                Err(error) => {
                    let _ = upload.abort().await;
                    return Err(error.into());
                }
            };
            if count == 0 {
                break;
            }
            part.truncate(count);
            checksum = crc32c::crc32c_append(checksum, &part);
            transferred = transferred.saturating_add(count as u64);
            if let Err(error) = upload.put_part(Bytes::from(part).into()).await {
                let _ = upload.abort().await;
                return Err(error.into());
            }
        }
        if let Err(error) = upload.complete().await {
            let _ = upload.abort().await;
            return Err(error.into());
        }
        if transferred != size {
            return Err(format!(
                "'{}' changed size while being uploaded: expected {size}, read {transferred}",
                source.display()
            )
            .into());
        }
        Ok(UploadedFile {
            size,
            crc32c: checksum,
        })
    }
}

struct UploadedFile {
    size: u64,
    crc32c: u32,
}

/// Metadata and per-shard WAL ends derived from one complete database copy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BaseInspection {
    /// Identity persisted in the database manifest.
    pub database_uuid: [u8; 16],
    /// Leadership timeline persisted in the database manifest.
    pub term: u32,
    /// Number of contiguous shard images found.
    pub shards: u32,
    /// Common checkpoint watermark of every shard image.
    pub checkpoint_version: u64,
    /// Logical end of the WAL included for every shard.
    pub wal_cursors: Vec<pb::WalCursor>,
    /// Upper bound on the commit time of everything at or below
    /// `checkpoint_version`, taken from the images' persisted commit clock.
    /// Zero when the images predate commit-time stamping.
    pub commit_clock: u64,
}

/// Inspect a base directory without opening it as a database. Opening a
/// read-only filesystem snapshot would attempt recovery writes; the two
/// superblock pages and the WAL catalog contain everything the archiver needs.
pub fn inspect_base(dir: &Path) -> Result<BaseInspection, ArchiveError> {
    use std::io::Read as _;

    let database_uuid = yesno_core::database_uuid(dir)?;
    let term = yesno_core::database_term(dir)?;
    let mut shard_images = database_files(dir)?
        .into_iter()
        .filter_map(|(name, path)| {
            name.strip_prefix("shard-")
                .and_then(|value| value.strip_suffix(".yno"))
                .and_then(|value| value.parse::<u32>().ok())
                .map(|shard| (shard, path))
        })
        .collect::<Vec<_>>();
    shard_images.sort_by_key(|(shard, _)| *shard);
    if shard_images.is_empty() {
        return Err(format!("'{}' contains no shard images", dir.display()).into());
    }
    for (expected, (shard, _)) in shard_images.iter().enumerate() {
        if *shard != expected as u32 {
            return Err(format!(
                "'{}' has non-contiguous shard image {shard}, expected {expected}",
                dir.display()
            )
            .into());
        }
    }

    let mut checkpoint_version = None;
    let mut commit_clock = 0u64;
    let mut wal_cursors = Vec::with_capacity(shard_images.len());
    for (shard, image) in shard_images {
        let mut file = std::fs::File::open(&image)?;
        let mut head = vec![0u8; 2 * yesno_core::store::PAGE];
        file.read_exact(&mut head)?;
        let page = yesno_core::store::PAGE;
        let superblock = yesno_core::store::superblock::pick(&head[..page], &head[page..])?
            .ok_or_else(|| format!("'{}' has no readable superblock", image.display()))?;
        if checkpoint_version
            .replace(superblock.checkpoint_cv)
            .is_some_and(|previous| previous != superblock.checkpoint_cv)
        {
            return Err(format!(
                "'{}' contains shard images from different checkpoint watermarks",
                dir.display()
            )
            .into());
        }
        // The maximum, unlike `checkpoint_cv` above, which must agree across
        // every image. This is a bound, and shards bound it independently.
        commit_clock = commit_clock.max(superblock.commit_clock);
        let (_, end) = yesno_core::wal::log_bounds(
            dir.join(format!("shard-{shard:04}.wal")),
            superblock.wal_replay_lsn,
        )?;
        wal_cursors.push(pb::WalCursor {
            shard,
            archived_lsn: end,
            history_fingerprint: Vec::new(),
        });
    }

    Ok(BaseInspection {
        database_uuid,
        term,
        shards: wal_cursors.len() as u32,
        checkpoint_version: checkpoint_version.unwrap_or(0),
        wal_cursors,
        commit_clock,
    })
}

/// Construct sorted, versioned archive state for a newly observed database.
pub fn new_state(
    database_uuid: Vec<u8>,
    term: u32,
    cursors: Vec<pb::WalCursor>,
) -> pb::ArchiveState {
    let mut state = pb::ArchiveState {
        schema_version: SCHEMA_VERSION,
        database_uuid,
        event_sequence: 0,
        latest_base_manifest: String::new(),
        wal_cursors: cursors,
        term,
        base_generation: 0,
        writer_id: Vec::new(),
    };
    state.wal_cursors.sort_by_key(|cursor| cursor.shard);
    state
}

/// Return the durable archived LSN for `shard`.
pub fn cursor(state: &pb::ArchiveState, shard: u32) -> Option<u64> {
    state
        .wal_cursors
        .iter()
        .find(|cursor| cursor.shard == shard)
        .map(|cursor| cursor.archived_lsn)
}

/// Insert or replace one shard cursor while preserving canonical shard order.
pub fn set_cursor(state: &mut pb::ArchiveState, shard: u32, archived_lsn: u64) {
    match state
        .wal_cursors
        .iter_mut()
        .find(|cursor| cursor.shard == shard)
    {
        Some(cursor) => cursor.archived_lsn = archived_lsn,
        None => state.wal_cursors.push(pb::WalCursor {
            shard,
            archived_lsn,
            history_fingerprint: Vec::new(),
        }),
    }
    state.wal_cursors.sort_by_key(|cursor| cursor.shard);
}

/// Return the durable history-chain tip for `shard`.
pub fn cursor_fingerprint(state: &pb::ArchiveState, shard: u32) -> Option<&[u8]> {
    state
        .wal_cursors
        .iter()
        .find(|cursor| cursor.shard == shard)
        .map(|cursor| cursor.history_fingerprint.as_slice())
}

/// Advance one shard cursor and its history commitment together.
pub fn set_cursor_history(
    state: &mut pb::ArchiveState,
    shard: u32,
    archived_lsn: u64,
    history_fingerprint: Vec<u8>,
) {
    match state
        .wal_cursors
        .iter_mut()
        .find(|cursor| cursor.shard == shard)
    {
        Some(cursor) => {
            cursor.archived_lsn = archived_lsn;
            cursor.history_fingerprint = history_fingerprint;
        }
        None => state.wal_cursors.push(pb::WalCursor {
            shard,
            archived_lsn,
            history_fingerprint,
        }),
    }
    state.wal_cursors.sort_by_key(|cursor| cursor.shard);
}

/// Validate archive identity, schema version, ownership, and cursor history.
pub fn validate_state(state: &pb::ArchiveState) -> Result<(), ArchiveError> {
    if state.schema_version != SCHEMA_VERSION {
        return Err(format!(
            "unsupported archive-state schema {}, expected {SCHEMA_VERSION}",
            state.schema_version
        )
        .into());
    }
    validate_uuid(&state.database_uuid)?;
    if !state.writer_id.is_empty() {
        validate_uuid(&state.writer_id)?;
    }
    let mut previous = None;
    for cursor in &state.wal_cursors {
        if previous.is_some_and(|shard| shard >= cursor.shard) {
            return Err("archive-state WAL cursors are not strictly shard-sorted".into());
        }
        if !state.latest_base_manifest.is_empty() {
            crate::history::validate_fingerprint(
                &cursor.history_fingerprint,
                "archive-state WAL fingerprint",
            )?;
        }
        previous = Some(cursor.shard);
    }
    Ok(())
}

/// Validate one immutable base manifest and its derived history anchor.
pub fn validate_base_manifest(manifest: &pb::BaseManifest) -> Result<(), ArchiveError> {
    if manifest.schema_version != SCHEMA_VERSION {
        return Err(format!(
            "unsupported base-manifest schema {}, expected {SCHEMA_VERSION}",
            manifest.schema_version
        )
        .into());
    }
    validate_uuid(&manifest.database_uuid)?;
    crate::history::validate_fingerprint(&manifest.history_anchor, "base history anchor")?;
    if crate::history::base_anchor(manifest) != manifest.history_anchor {
        return Err("base manifest history anchor does not match its contents".into());
    }
    let mut previous_shard = None;
    for cursor in &manifest.wal_cursors {
        if previous_shard.is_some_and(|shard| shard >= cursor.shard) {
            return Err("base-manifest WAL cursors are not strictly shard-sorted".into());
        }
        let expected = crate::history::base_cursor_fingerprint(
            &manifest.history_anchor,
            cursor.shard,
            cursor.archived_lsn,
        );
        if cursor.history_fingerprint != expected {
            return Err(format!(
                "base-manifest shard {} has a bad history fingerprint",
                cursor.shard
            )
            .into());
        }
        previous_shard = Some(cursor.shard);
    }
    if manifest.files.is_empty() || manifest.wal_cursors.is_empty() {
        return Err("base manifest has no files or no WAL cursors".into());
    }
    Ok(())
}

/// Validate one WAL descriptor against its raw byte-identical object.
pub fn validate_wal_object(object: &pb::WalObject, records: &[u8]) -> Result<(), ArchiveError> {
    if object.schema_version != SCHEMA_VERSION {
        return Err(format!(
            "unsupported WAL-object schema {}, expected {SCHEMA_VERSION}",
            object.schema_version
        )
        .into());
    }
    validate_uuid(&object.database_uuid)?;
    crate::history::validate_fingerprint(&object.previous_fingerprint, "previous WAL fingerprint")?;
    crate::history::validate_fingerprint(&object.history_fingerprint, "WAL history fingerprint")?;
    if object.size != records.len() as u64
        || object.first_lsn.checked_add(object.size) != Some(object.last_lsn)
        || object.crc32c != crc32c::crc32c(records)
    {
        return Err("WAL descriptor size, range, or CRC does not match its raw object".into());
    }
    let versions = crate::history::wal_versions(records, object.first_lsn, object.last_lsn)?;
    if versions != (object.first_version, object.last_version) {
        return Err("WAL descriptor version range does not match its records".into());
    }
    let expected = crate::history::wal_fingerprint(
        &object.previous_fingerprint,
        &object.database_uuid,
        object.term,
        object.shard,
        object.first_lsn,
        object.last_lsn,
        records,
    );
    if object.history_fingerprint != expected {
        return Err("WAL history fingerprint does not match its descriptor and bytes".into());
    }
    Ok(())
}

fn validate_uuid(uuid: &[u8]) -> Result<(), ArchiveError> {
    if uuid.len() != 16 {
        return Err(format!("database UUID is {} bytes, expected 16", uuid.len()).into());
    }
    Ok(())
}

fn uuid_hex(uuid: &[u8]) -> Result<String, ArchiveError> {
    validate_uuid(uuid)?;
    Ok(uuid.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn database_files(dir: &Path) -> Result<Vec<(String, PathBuf)>, ArchiveError> {
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
            files.push((name, entry.path()));
        }
    }
    files.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(files)
}

async fn write_local_state(path: &Path, bytes: Vec<u8>) -> Result<(), ArchiveError> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<(), ArchiveError> {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent)?;
        let name = path
            .file_name()
            .ok_or_else(|| format!("state path '{}' has no file name", path.display()))?;
        let temporary = parent.join(format!(".{}.partial", name.to_string_lossy()));
        let mut file = std::fs::File::create(&temporary)?;
        use std::io::Write as _;
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(&temporary, &path)?;
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    })
    .await??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use object_store::memory::InMemory;
    use yesno_core::wal::{RecType, Record};

    fn cursor(shard: u32, archived_lsn: u64) -> pb::WalCursor {
        pb::WalCursor {
            shard,
            archived_lsn,
            history_fingerprint: Vec::new(),
        }
    }

    #[test]
    fn cursors_are_unique_and_sorted() {
        let mut state = new_state(vec![7; 16], 3, vec![cursor(2, 20), cursor(0, 10)]);
        set_cursor(&mut state, 1, 15);
        set_cursor(&mut state, 2, 25);
        assert_eq!(
            state
                .wal_cursors
                .iter()
                .map(|cursor| (cursor.shard, cursor.archived_lsn))
                .collect::<Vec<_>>(),
            vec![(0, 10), (1, 15), (2, 25)]
        );
        validate_state(&state).unwrap();
    }

    #[tokio::test]
    async fn remote_state_round_trips_as_protobuf_under_a_writer_lease() {
        let store = ArchiveStore::new(Arc::new(InMemory::new()), ObjectPath::ROOT);
        let lease = store.acquire_writer(vec![3; 16], 30).await.unwrap();
        let mut state = new_state(vec![9; 16], 4, vec![cursor(0, 42)]);
        let tmp = tempfile::tempdir().unwrap();
        let local = tmp.path().join("state.pb");
        store.claim_state(&local, &mut state).await.unwrap();
        assert_eq!(store.load_state().await.unwrap(), Some(state.clone()));
        assert_eq!(
            pb::ArchiveState::decode(std::fs::read(local).unwrap().as_slice()).unwrap(),
            state
        );
        lease.release().await.unwrap();
    }

    #[tokio::test]
    async fn wal_keys_are_deterministic_and_immutable() {
        let store = ArchiveStore::new(Arc::new(InMemory::new()), ObjectPath::ROOT);
        let records = Record::new(RecType::ShardCommit, 10, 5, 7, Vec::new()).encode();
        let last = 10 + records.len() as u64;
        store
            .put_wal(&[1; 16], 7, 3, 10..last, &[8; 32], records.clone())
            .await
            .unwrap();
        let key = format!(
            "db/01010101010101010101010101010101/term/0000000007/wal/0003/{:020}-{:020}.wal",
            10, last
        );
        let bytes = store
            .inner
            .get(&ObjectPath::parse(&key).unwrap())
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert_eq!(bytes.as_ref(), records);

        let different = Record::new(RecType::ShardCommit, 10, 6, 7, Vec::new()).encode();
        let error = store
            .put_wal(&[1; 16], 7, 3, 10..last, &[8; 32], different)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("different bytes"));
    }

    #[tokio::test]
    async fn wal_history_is_independent_of_replication_batch_partition() {
        let first = Record::new(RecType::ShardCommit, 10, 5, 7, Vec::new()).encode();
        let second_lsn = 10 + first.len() as u64;
        let second = Record::new(RecType::ShardCommit, second_lsn, 6, 7, Vec::new()).encode();
        let end = second_lsn + second.len() as u64;
        let mut combined = first.clone();
        combined.extend_from_slice(&second);

        let combined_store = ArchiveStore::new(Arc::new(InMemory::new()), ObjectPath::ROOT);
        let combined_tip = combined_store
            .put_wal(&[1; 16], 7, 3, 10..end, &[8; 32], combined)
            .await
            .unwrap();

        let split_store = ArchiveStore::new(Arc::new(InMemory::new()), ObjectPath::ROOT);
        let first_tip = split_store
            .put_wal(&[1; 16], 7, 3, 10..second_lsn, &[8; 32], first)
            .await
            .unwrap();
        let split_tip = split_store
            .put_wal(
                &[1; 16],
                7,
                3,
                second_lsn..end,
                &first_tip.history_fingerprint,
                second,
            )
            .await
            .unwrap();

        assert_eq!(
            combined_tip.history_fingerprint,
            split_tip.history_fingerprint
        );
        assert_eq!(
            combined_store.list_relative("db/").await.unwrap(),
            split_store.list_relative("db/").await.unwrap()
        );
    }
}
