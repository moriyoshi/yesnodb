//! Durable object-storage primitives for the `yesno-archive` sidecar.
//!
//! WAL generation events are deliberately absent from this module. A sealed
//! generation may be reclaimed before an event consumer opens its pathname, so
//! event delivery cannot be used as a file-lifetime protocol. The archive reads
//! WAL bytes from the replication stream and acknowledges an LSN only after the
//! corresponding object and protobuf cursor are durable.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
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

use crate::ctx;

/// Archive metadata schema emitted by this version of the sidecar.
pub const SCHEMA_VERSION: u32 = 2;

/// Bytes of a base file weighed for emptiness at a time, on both sides.
///
/// A shard image is a **sparse** file: the store grows it to a whole 1 GiB mmap
/// segment with `set_len`, so a database holding kilobytes has an apparent size
/// in gigabytes and occupies almost nothing -- measured, a live shard was 1 to
/// 2 MiB allocated against an apparent 1 GiB. An upload weighs each window and
/// ships only the ones that carry something; a restore sets the length and
/// writes only the windows it receives, so the holes survive the round trip
/// instead of being materialized at either end.
///
/// Measured on a four-shard database holding 6 401 keys, a `file://` store, a
/// release build: **4.00 GiB of base objects became 5.01 MiB**, 818x less, and
/// materializing the base on restore went from **3.11 s to 0.24 s**. What is
/// left of that stage is almost exactly the cost of hashing the holes the
/// restore did not receive -- see `crc_hole` in `restore.rs`.
///
/// The same decision, with the same reasoning, is already made on the
/// replication bootstrap path, which skips all-zero chunks on the wire and sets
/// the length first on the follower. One megabyte matches the chunk that path
/// weighs, and is large enough that a skip reliably leaves a hole rather than a
/// run of allocated zeros.
///
/// It is one constant and not two because it is one format decision: an extent
/// boundary written by an upload is a window boundary a restore seeks to.
///
/// **The holes here are no longer read**, as of 2026-10-16. Both this comment
/// and the replication path's used to say that `SEEK_DATA` was considered and
/// declined, on the grounds that the scan cost only memory bandwidth on a
/// one-off operation. That was measured and overturned: a shard image is
/// grown to a whole 1 GiB mapping segment at creation, so a four-shard
/// capture of a database holding three ordinals read 4.29 GB and took 17.3
/// seconds, none of it on account of anything the database contained.
///
/// What this constant decides is unchanged by that. The cut is still whole
/// windows measured from offset zero, because an extent boundary is a
/// boundary a restore seeks to; all the filesystem is asked is whether a
/// window is worth reading.
pub(crate) const SPARSE_WINDOW: usize = 1 << 20;

/// Bytes of a base file per content-addressed object.
///
/// **The store's own slab**, taken from `yesno-core` rather than restated, so
/// the cut cannot drift from the unit the allocator writes. That alignment is
/// the whole reason dedup works: a checkpoint that rewrites one key dirties
/// the slabs it touched and leaves the rest byte-identical, so generation
/// `N+1` renames almost every object generation `N` already stored.
/// `Slab::new` stamping the generation at creation and `alloc.rs` keeping one
/// checkpoint's allocations in a contiguous run are what make that true; cut
/// at any other boundary and an unrelated write shifts every later object.
///
/// **It coarsens hole detection from [`SPARSE_WINDOW`] to this**, because an
/// object is addressed by its whole contents and so cannot be half present. A
/// one-megabyte hole inside a two-megabyte slab is now stored. That is the
/// price of dedup and it is small against it: measured at 500 GiB over 16
/// shards, slab granularity uploads 25 GiB per generation at 5% churn where
/// one object per shard file uploads 500 GiB.
const SLAB_OBJECT_BYTES: usize = yesno_core::store::SLAB_SIZE as usize;

/// Slab objects uploaded at once.
///
/// A 500 GiB base is 256 000 objects, so this is the difference between a
/// capture that finishes and one that does not. Bounded rather than unbounded
/// because each task holds its slab in memory: this many times
/// [`SLAB_OBJECT_BYTES`] is the upload's working set.
const SLAB_UPLOAD_CONCURRENCY: usize = 16;
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
        Ok(self
            .list_relative_modified(prefix)
            .await?
            .into_iter()
            .map(|(key, _)| key)
            .collect())
    }

    /// The same listing, with each object's last-modified time in epoch
    /// microseconds, sorted by key.
    ///
    /// The time comes back from the listing that was happening anyway, which
    /// is the only reason it is worth having: it is the field an object-store
    /// lifecycle rule keys on, so it is the only thing in the archive that can
    /// say an expiry rule is about to delete something live. See
    /// [`crate::gc::oldest_live`].
    ///
    /// A store that reports a time before the epoch reads as zero rather than
    /// as an error. The value is advisory -- it is compared against an
    /// operator's lifecycle rule, not against anything this code decides -- and
    /// failing a reclamation pass over a nonsensical timestamp would be the
    /// worse outcome.
    pub(crate) async fn list_relative_modified(
        &self,
        prefix: &str,
    ) -> Result<Vec<(String, u64)>, ArchiveError> {
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
            let modified = object.last_modified.timestamp_micros().max(0) as u64;
            relative.push((value.to_owned(), modified));
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

        let uuid = uuid_hex(&manifest.database_uuid)?;
        let base = format!(
            "db/{uuid}/term/{:010}/base/{:020}",
            manifest.term, manifest.archive_generation
        );
        let known = self.slabs_of_active_base().await?;
        for (name, path) in database_files(source_dir)? {
            let file = self.put_slab_file(&path, &uuid, &known).await?;
            manifest.files.push(pb::ArchiveFile {
                name,
                // Empty: every extent names its own object. The per-writer
                // path this used to take is what made two generations of the
                // same bytes two objects.
                object_key: String::new(),
                size: file.size,
                crc32c: file.crc32c,
                sparse: file.sparse,
                extents: file.extents,
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

    /// Content addresses the active base already stores.
    ///
    /// A slab in this set needs neither an upload nor a `head` to prove it is
    /// there, which is what makes a capture cost the churn rather than the
    /// database. At 500 GiB that is the difference between 256 000 requests
    /// per capture and the twelve thousand or so that actually changed.
    ///
    /// **Why this set can be trusted.** Reclamation keeps every object named
    /// by a retained manifest, and the active base -- the one `state.pb`
    /// points at -- is never deletable; `the_active_base_is_never_deleted` is
    /// the test. So its objects exist. This adds no failure mode a restore
    /// does not already have: if the active base's objects could vanish,
    /// restoring from it was already broken.
    ///
    /// **Empty whenever anything is unknown rather than guessed** -- no state,
    /// no active base, a manifest that will not decode, or one written before
    /// slab objects existed. An empty set costs a `head` per slab and is
    /// always correct, so every failure here degrades to the previous
    /// behaviour instead of skipping an upload that was needed.
    async fn slabs_of_active_base(&self) -> Result<HashSet<[u8; 32]>, ArchiveError> {
        let mut known = HashSet::new();
        // Every failure below returns the empty set rather than propagating.
        // A capture must not fail because an optimisation could not read its
        // input, and `put_base` publishes `state.pb` afterwards anyway -- so
        // a state this cannot parse is reported there, by the code whose job
        // it is, instead of here.
        let Ok(Some(state)) = self.load_state().await else {
            return Ok(known);
        };
        if state.latest_base_manifest.is_empty() {
            return Ok(known);
        }
        let Ok(bytes) = self.get_bytes(&state.latest_base_manifest).await else {
            return Ok(known);
        };
        let Ok(manifest) = pb::BaseManifest::decode(bytes) else {
            return Ok(known);
        };
        for file in &manifest.files {
            for extent in &file.extents {
                if let Ok(hash) = <[u8; 32]>::try_from(extent.content_hash.as_slice()) {
                    known.insert(hash);
                }
            }
        }
        Ok(known)
    }

    /// Upload one base file as slab-sized, content-addressed objects.
    ///
    /// Three properties the caller depends on, in the order they are
    /// established:
    ///
    /// - **An all-zero slab produces no object and no extent.** The file's
    ///   holes survive the round trip, exactly as they did when a sparse file
    ///   was one object, and a restore writes only what it receives.
    /// - **`size` and `crc32c` are the whole file's, holes included**, so the
    ///   history anchor keeps committing to content rather than to a layout,
    ///   and a restore verifies the file it reconstructs rather than the bytes
    ///   it was sent.
    /// - **An object already in the store is not sent again.** The name is the
    ///   content, so a slab unchanged since the last generation already exists
    ///   under exactly this key. That is where a base generation stops costing
    ///   a whole database.
    ///
    /// The last of those three is why a capture may not overlap a reclamation
    /// pass. Naming an object this capture did not write makes the capture
    /// depend on whichever older generation did, and that generation may be
    /// the one a concurrent pass is dropping. The sidecar's `CaptureGate`
    /// holds the two apart; the set `known` carries is safe on its own terms,
    /// because the active base is never deletable, but the `head` below is
    /// not, because it will happily find an object nothing retained names.
    async fn put_slab_file(
        &self,
        source: &Path,
        uuid: &str,
        known: &HashSet<[u8; 32]>,
    ) -> Result<UploadedFile, ArchiveError> {
        use futures::stream::{FuturesUnordered, StreamExt};

        let size = tokio::fs::metadata(source)
            .await
            .map_err(|e| ctx("stat the upload source", source, e))?
            .len();
        let mut source_file = tokio::fs::File::open(source)
            .await
            .map_err(|e| ctx("open the upload source", source, e))?;
        // A second, synchronous handle for nothing but asking the filesystem
        // which parts of the image are there. `tokio::fs::File` keeps its own
        // position and does its reads on the blocking pool, so moving its
        // descriptor's offset from underneath it would be interfering with it
        // rather than using it.
        let probe =
            std::fs::File::open(source).map_err(|e| ctx("open the upload source", source, e))?;
        let mut run = yesno_server::sparse::next_allocated(&probe, 0, size);
        // Where the reader's cursor is. Skipping a window leaves it behind,
        // and a read after a skip has to be positioned rather than
        // sequential.
        let mut positioned = 0u64;

        let mut checksum = 0u32;
        let mut read_total = 0u64;
        let mut extents: Vec<pb::FileExtent> = Vec::new();
        let mut sent: HashSet<[u8; 32]> = HashSet::new();
        let mut inflight = FuturesUnordered::new();
        let mut slab = vec![0u8; SLAB_OBJECT_BYTES];

        while read_total < size {
            let at = read_total;
            let want = SLAB_OBJECT_BYTES.min((size - at) as usize);
            // Past the window, so look for the next run. The walk only ever
            // moves forward, which is what makes one probe per run rather
            // than one per window.
            while matches!(run, Some((_, end)) if end <= at) {
                let from = run.expect("matched above").1;
                run = yesno_server::sparse::next_allocated(&probe, from, size);
            }
            // **The cut does not move.** Slabs are whole windows measured
            // from offset zero, because an extent boundary written here is a
            // window boundary a restore seeks to, and because the dedup rests
            // on the boundary matching the allocator's. So an unallocated
            // window is skipped and every other window is read in full --
            // what the filesystem is asked is only whether to read it.
            if !matches!(run, Some((start, _)) if start < at + want as u64) {
                // Not read, and still hashed: the file's CRC covers its whole
                // length, and a hole is zeros for that purpose.
                checksum = crate::restore::crc_hole(checksum, want as u64);
                read_total = at + want as u64;
                continue;
            }
            if positioned != at {
                use tokio::io::AsyncSeekExt;
                source_file
                    .seek(std::io::SeekFrom::Start(at))
                    .await
                    .map_err(|e| ctx("seek the upload source", source, e))?;
            }
            // Whole slabs, so an object boundary is a slab boundary. A short
            // read mid-file is not end-of-file, and one taken for a slab would
            // shift every later object and defeat the dedup this is for.
            let count = fill(&mut source_file, &mut slab[..want]).await?;
            if count != want {
                return Err(format!(
                    "'{}' ended at {} while {size} bytes were expected",
                    source.display(),
                    at + count as u64
                )
                .into());
            }
            let filled = &slab[..count];
            checksum = crc32c::crc32c_append(checksum, filled);
            read_total = at + count as u64;
            positioned = read_total;
            // A run the filesystem has allocated may still be zeros. The two
            // filters compose: this one skips what was written and is zero,
            // the one above what was never written at all.
            if filled.iter().all(|byte| *byte == 0) {
                continue;
            }

            let hash = sha256(filled);
            extents.push(pb::FileExtent {
                offset: at,
                length: count as u64,
                content_hash: hash.to_vec(),
                crc32c: crc32c::crc32c(filled),
            });
            // A file may hold the same slab twice; one upload serves both.
            if !sent.insert(hash) {
                continue;
            }
            // Already in the active base, so already in the store under this
            // exact name. The common case at steady state, and the one that
            // costs nothing at all -- not even a round trip.
            if known.contains(&hash) {
                continue;
            }

            let body = Bytes::copy_from_slice(filled);
            let path = self.path(&slab_key(uuid, &hash));
            let store = Arc::clone(&self.inner);
            inflight.push(async move {
                // The name is the content, so an object that is already there
                // is already right. `head` costs one round trip against the
                // slab's worth of body it saves.
                if store.head(&path).await.is_ok() {
                    return Ok::<(), ArchiveError>(());
                }
                store.put(&path, PutPayload::from(body)).await?;
                Ok(())
            });
            while inflight.len() >= SLAB_UPLOAD_CONCURRENCY {
                inflight.next().await.expect("non-empty")?;
            }
        }
        while let Some(done) = inflight.next().await {
            done?;
        }

        if read_total != size {
            return Err(format!(
                "'{}' changed size while being uploaded: expected {size}, read {read_total}",
                source.display()
            )
            .into());
        }
        Ok(UploadedFile {
            size,
            crc32c: checksum,
            // Always, even for a file with no holes: the file is reconstructed
            // from its extents either way, and `sparse` is what tells a reader
            // not to look for one whole object.
            sparse: true,
            extents,
        })
    }
}

/// `db/<uuid>/slabs/<address>`, the object a content address names.
///
/// **Outside the term tree on purpose.** Every other key in this archive is
/// scoped by leadership term so a promoted timeline cannot overwrite an older
/// one's identically numbered object. A content address needs no such fence --
/// two writers producing this key produced the same bytes -- and scoping it by
/// term would defeat the sharing across generations that it exists for. It
/// stays inside the database prefix: dedup across tenants would mean one
/// database's bytes served out of another's object, which is a decision about
/// identity rather than about storage. See `tenant-and-identity-namespace-model`.
pub(crate) fn slab_key(uuid: &str, hash: &[u8]) -> String {
    format!("db/{uuid}/slabs/{}", hex(hash))
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

/// Read until `buf` is full or the file ends, reporting how much was filled.
async fn fill(file: &mut tokio::fs::File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match file.read(&mut buf[filled..]).await? {
            0 => break,
            count => filled += count,
        }
    }
    Ok(filled)
}

struct UploadedFile {
    size: u64,
    crc32c: u32,
    sparse: bool,
    extents: Vec<pb::FileExtent>,
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
        let mut file =
            std::fs::File::open(&image).map_err(|e| ctx("open the shard image", &image, e))?;
        let mut head = vec![0u8; 2 * yesno_core::store::PAGE];
        file.read_exact(&mut head)
            .map_err(|e| ctx("read the shard image superblock", &image, e))?;
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
    for file in &manifest.files {
        validate_extents(file)?;
    }
    Ok(())
}

/// Check that one file's extent list describes a reconstructible file.
///
/// The encoding is canonical, so one file has exactly one valid extent list
/// and a restore's arithmetic has one shape to handle. Runs ascend, carry
/// bytes and stay inside the file in both shapes; what differs is where their
/// bytes live and therefore what adjacency means.
///
/// **A file is one shape or the other, never both.** A mixed list would make
/// a reader decide per extent whether to trust the file's object key or the
/// extent's address, and the two cannot be checked against each other.
fn validate_extents(file: &pb::ArchiveFile) -> Result<(), ArchiveError> {
    let name = &file.name;
    if !file.sparse {
        if file.extents.is_empty() {
            return Ok(());
        }
        return Err(format!("base file '{name}' carries extents without the sparse flag").into());
    }
    if file.extents.is_empty() {
        // Entirely zero. There is nothing to address and nothing to fetch, so
        // neither shape applies and the object key is merely history: a writer
        // that cut the file into slabs produced none, an older one produced an
        // empty object.
        return Ok(());
    }
    let addressed = !file.extents[0].content_hash.is_empty();
    if addressed == !file.object_key.is_empty() {
        return Err(format!(
            "base file '{name}' must name one object or give every extent an address, not {}",
            if addressed { "both" } else { "neither" }
        )
        .into());
    }
    let mut end = 0u64;
    for (index, extent) in file.extents.iter().enumerate() {
        if extent.length == 0 {
            return Err(format!("base file '{name}' extent {index} is empty").into());
        }
        if extent.content_hash.is_empty() != !addressed {
            return Err(
                format!("base file '{name}' extent {index} mixes the two extent shapes").into(),
            );
        }
        if addressed {
            if extent.content_hash.len() != 32 {
                return Err(format!(
                    "base file '{name}' extent {index} has a {}-byte address, expected 32",
                    extent.content_hash.len()
                )
                .into());
            }
            // Slab-aligned, so an object is the same bytes wherever it
            // appears. An unaligned cut would still restore, and would
            // silently never dedup against anything.
            if extent.offset % SLAB_OBJECT_BYTES as u64 != 0 {
                return Err(
                    format!("base file '{name}' extent {index} does not start a slab").into(),
                );
            }
            if extent.length > SLAB_OBJECT_BYTES as u64 {
                return Err(
                    format!("base file '{name}' extent {index} spans several slabs").into(),
                );
            }
            // Adjacent runs are *expected* here -- two consecutive slabs that
            // both carry data are two objects at touching offsets -- so only
            // overlap is refused.
            if index > 0 && extent.offset < end {
                return Err(
                    format!("base file '{name}' extent {index} overlaps its predecessor").into(),
                );
            }
        } else if index > 0 && extent.offset <= end {
            // One object, so a boundary that is not a hole means the writer
            // failed to merge two runs and the list is not canonical.
            return Err(format!("base file '{name}' extent {index} does not follow a hole").into());
        }
        end = match extent.offset.checked_add(extent.length) {
            Some(end) if end <= file.size => end,
            _ => {
                return Err(format!("base file '{name}' extent {index} runs past its size").into())
            }
        };
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

pub(crate) fn uuid_hex(uuid: &[u8]) -> Result<String, ArchiveError> {
    validate_uuid(uuid)?;
    Ok(hex(uuid))
}

/// Lowercase hex, which is how every key in this archive spells bytes.
pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn database_files(dir: &Path) -> Result<Vec<(String, PathBuf)>, ArchiveError> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(|e| ctx("list the image directory", dir, e))? {
        let entry = entry?;
        if !entry
            .file_type()
            .map_err(|e| ctx("stat an image-directory entry", &entry.path(), e))?
            .is_file()
        {
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
        std::fs::create_dir_all(parent)
            .map_err(|e| ctx("create the state directory", parent, e))?;
        let name = path
            .file_name()
            .ok_or_else(|| format!("state path '{}' has no file name", path.display()))?;
        // **One temporary per call, not one per target.**
        //
        // This used to be `.{name}.partial`, derived from the file being
        // written. The sidecar spawns a `wal_loop` per shard plus `event_loop`,
        // `term_loop`, `writer_lease_loop` and `retention_loop` into one
        // `JoinSet`, and four of those publish state on the same path -- so
        // every concurrent writer opened, wrote and synced the *same*
        // temporary, and whichever renamed first took it away. The others'
        // `rename` then failed with a bare `ENOENT` on a file they had just
        // synced.
        //
        // That is `the-archive-sidecar-fails-with-enoent-under-load`, open from
        // 2026-09-28 and never reproduced in isolation, because the window is
        // the few microseconds between one writer's `sync_all` and its
        // `rename`. Load widened it; load was never the cause.
        //
        // The pid and counter follow `capture_target`'s naming for the same
        // reason: unique within a process, and distinguishable between two.
        let unique = TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(
            ".{}.{}.{unique}.partial",
            name.to_string_lossy(),
            std::process::id(),
        ));
        let write = (|| -> Result<(), ArchiveError> {
            let mut file = std::fs::File::create(&temporary)
                .map_err(|e| ctx("create the state temporary", &temporary, e))?;
            use std::io::Write as _;
            file.write_all(&bytes)
                .map_err(|e| ctx("write the state temporary", &temporary, e))?;
            file.sync_all()
                .map_err(|e| ctx("sync the state temporary", &temporary, e))?;
            std::fs::rename(&temporary, &path)
                .map_err(|e| ctx("rename the state temporary", &temporary, e))?;
            Ok(())
        })();
        if write.is_err() {
            // A per-call name cannot be reused, so a failed attempt would
            // otherwise leave its temporary in the work directory forever.
            let _ = std::fs::remove_file(&temporary);
        }
        write?;
        std::fs::File::open(parent)
            .map_err(|e| ctx("open the state directory to sync it", parent, e))?
            .sync_all()
            .map_err(|e| ctx("sync the state directory", parent, e))?;
        Ok(())
    })
    .await??;
    Ok(())
}

/// Distinguishes the temporaries of two concurrent [`write_local_state`] calls.
static TEMPORARY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

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

    /// Concurrent writers of the local state cache must not collide.
    ///
    /// The sidecar spawns a `wal_loop` **per shard** plus `event_loop`,
    /// `term_loop`, `writer_lease_loop` and `retention_loop` into one
    /// `JoinSet`, and four of those publish state on the same
    /// `runtime.local_state` path. `write_local_state` used to derive its
    /// temporary from the *target's* file name, so every concurrent writer
    /// used the same `.state.pb.partial`: whoever renamed first took it away,
    /// and the others' `rename` failed with a bare `ENOENT`.
    ///
    /// That was `the-archive-sidecar-fails-with-enoent-under-load`, open since
    /// 2026-09-28 and never reproduced in isolation because it needs two
    /// writers interleaved between one's `sync_all` and its `rename`. Load was
    /// the trigger and never the cause.
    ///
    /// Sixteen writers rather than two, because the window is a few
    /// microseconds wide and one pair almost never lands inside it.
    #[tokio::test]
    async fn concurrent_local_state_writers_do_not_share_a_temporary() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.pb");
        let mut tasks = tokio::task::JoinSet::new();
        for i in 0..16u8 {
            let path = path.clone();
            tasks.spawn(async move { write_local_state(&path, vec![i; 256]).await });
        }
        while let Some(joined) = tasks.join_next().await {
            joined
                .expect("no writer may panic")
                .expect("no writer may fail: a shared temporary makes the loser ENOENT");
        }
        // Whoever renamed last wins, and the file must be one writer's bytes
        // whole rather than a mixture -- which is what rename buys.
        let got = std::fs::read(&path).unwrap();
        assert_eq!(
            got.len(),
            256,
            "the published state is one writer's payload"
        );
        assert!(
            got.iter().all(|&b| b == got[0]),
            "the published state is torn: {:?}..",
            &got[..8]
        );
        // No temporary may survive a successful write.
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "state.pb")
            .collect();
        assert!(
            leftovers.is_empty(),
            "temporaries left behind: {leftovers:?}"
        );
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

    /// Build a file of `slabs` whole [`SLAB_OBJECT_BYTES`], filling only the
    /// indices named and leaving the rest as holes. A filled slab's contents
    /// are a function of `content`, so two indices sharing one are byte-equal.
    fn slab_file(dir: &Path, name: &str, slabs: usize, filled: &[(usize, u8)]) -> PathBuf {
        use std::io::{Seek, SeekFrom, Write};
        let path = dir.join(name);
        let mut file = std::fs::File::create(&path).unwrap();
        file.set_len((slabs * SLAB_OBJECT_BYTES) as u64).unwrap();
        for (index, content) in filled {
            file.seek(SeekFrom::Start((index * SLAB_OBJECT_BYTES) as u64))
                .unwrap();
            let body: Vec<u8> = (0..SLAB_OBJECT_BYTES)
                .map(|i| (i.wrapping_add(*content as usize) % 251) as u8 | 1)
                .collect();
            file.write_all(&body).unwrap();
        }
        file.sync_all().unwrap();
        path
    }

    /// A file whose length is not a whole number of slabs, with its only
    /// contents in the short window at the end.
    ///
    /// The uploader no longer reads what the filesystem says is not there, so
    /// the last window is now both the first one read and a partial one. Two
    /// things could go wrong only here: a read sized to the slab rather than
    /// to what is left, which would run past the end; and the holes before it
    /// going unaccounted, which the CRC catches because it covers the file's
    /// whole length.
    #[tokio::test]
    async fn a_short_final_window_after_nothing_but_holes_is_uploaded_whole() {
        use std::io::{Seek, SeekFrom, Write};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shard-0000.yno");
        let tail = 4096usize;
        let size = 2 * SLAB_OBJECT_BYTES + tail;
        let mut file = std::fs::File::create(&path).unwrap();
        file.set_len(size as u64).unwrap();
        file.seek(SeekFrom::Start(2 * SLAB_OBJECT_BYTES as u64))
            .unwrap();
        file.write_all(&vec![0x5au8; tail]).unwrap();
        file.sync_all().unwrap();
        drop(file);

        let store = ArchiveStore::new(Arc::new(InMemory::new()), ObjectPath::ROOT);
        let uuid = "cd".repeat(16);
        let uploaded = store
            .put_slab_file(&path, &uuid, &HashSet::new())
            .await
            .unwrap();

        assert_eq!(uploaded.size, size as u64);
        assert_eq!(
            uploaded.crc32c,
            crc32c::crc32c(&std::fs::read(&path).unwrap()),
            "the file's CRC, the two leading holes included"
        );
        assert_eq!(uploaded.extents.len(), 1, "{:?}", uploaded.extents);
        assert_eq!(uploaded.extents[0].offset, 2 * SLAB_OBJECT_BYTES as u64);
        assert_eq!(
            uploaded.extents[0].length, tail as u64,
            "the object is what is left of the file, not a whole slab"
        );
        let key = slab_key(&uuid, &uploaded.extents[0].content_hash);
        assert_eq!(store.get_bytes(&key).await.unwrap().len(), tail);
    }

    /// Only the slabs that carry something become objects, and the manifest
    /// records where each one belongs.
    ///
    /// Slabs 1 and 2 are adjacent on purpose. Under one object per file that
    /// was a non-canonical list; under one object per slab it is the ordinary
    /// case, and `validate_extents` has to accept it.
    #[tokio::test]
    async fn a_sparse_file_uploads_only_its_slabs() {
        let dir = tempfile::tempdir().unwrap();
        let path = slab_file(dir.path(), "shard-0000.yno", 8, &[(1, 1), (2, 2), (5, 3)]);
        let store = ArchiveStore::new(Arc::new(InMemory::new()), ObjectPath::ROOT);
        let uuid = "ab".repeat(16);
        let uploaded = store
            .put_slab_file(&path, &uuid, &HashSet::new())
            .await
            .unwrap();

        assert!(uploaded.sparse);
        assert_eq!(
            uploaded.size,
            8 * SLAB_OBJECT_BYTES as u64,
            "the file's length"
        );
        assert_eq!(
            uploaded.crc32c,
            crc32c::crc32c(&std::fs::read(&path).unwrap()),
            "the file's CRC, holes included"
        );
        assert_eq!(
            uploaded
                .extents
                .iter()
                .map(|e| e.offset / SLAB_OBJECT_BYTES as u64)
                .collect::<Vec<_>>(),
            vec![1, 2, 5],
            "one extent per filled slab, holes absent"
        );
        for extent in &uploaded.extents {
            assert_eq!(extent.length, SLAB_OBJECT_BYTES as u64);
            assert_eq!(extent.content_hash.len(), 32);
            let key = slab_key(&uuid, &extent.content_hash);
            let object = store.get_bytes(&key).await.unwrap();
            assert_eq!(object.len(), SLAB_OBJECT_BYTES, "the object is the slab");
            assert_eq!(crc32c::crc32c(&object), extent.crc32c);
        }
        validate_extents(&pb::ArchiveFile {
            name: "shard-0000.yno".into(),
            object_key: String::new(),
            size: uploaded.size,
            crc32c: uploaded.crc32c,
            sparse: true,
            extents: uploaded.extents,
        })
        .unwrap();
    }

    /// A file that is entirely holes transfers nothing at all, which is the
    /// case an empty extent list has to be able to mean.
    #[tokio::test]
    async fn an_entirely_empty_file_uploads_no_objects_and_still_has_a_length() {
        let dir = tempfile::tempdir().unwrap();
        let path = slab_file(dir.path(), "blank.yno", 4, &[]);
        let store = ArchiveStore::new(Arc::new(InMemory::new()), ObjectPath::ROOT);
        let uploaded = store
            .put_slab_file(&path, &"cd".repeat(16), &HashSet::new())
            .await
            .unwrap();
        assert!(uploaded.sparse);
        assert!(uploaded.extents.is_empty());
        assert_eq!(uploaded.size, 4 * SLAB_OBJECT_BYTES as u64);
        assert_eq!(
            uploaded.crc32c,
            crc32c::crc32c(&vec![0u8; uploaded.size as usize])
        );
        assert!(store.list_relative("db/").await.unwrap().is_empty());
    }

    /// Equal bytes are one object, which is the whole of why slabs are
    /// addressed by content.
    ///
    /// Within one file, and then across a second capture of an unchanged
    /// file: the second finds every object already present and sends none of
    /// them, which is what makes a base generation incremental.
    #[tokio::test]
    async fn identical_slabs_are_stored_and_sent_once() {
        let dir = tempfile::tempdir().unwrap();
        // Slabs 0 and 3 share content; 1 differs.
        let path = slab_file(dir.path(), "shard-0000.yno", 4, &[(0, 7), (1, 9), (3, 7)]);
        let store = ArchiveStore::new(Arc::new(InMemory::new()), ObjectPath::ROOT);
        let uuid = "ef".repeat(16);
        let first = store
            .put_slab_file(&path, &uuid, &HashSet::new())
            .await
            .unwrap();

        assert_eq!(first.extents.len(), 3, "three slabs carry bytes");
        assert_eq!(
            first.extents[0].content_hash, first.extents[2].content_hash,
            "equal bytes, equal address"
        );
        let objects = store.list_relative("db/").await.unwrap();
        assert_eq!(objects.len(), 2, "two distinct slabs, two objects");

        let again = store
            .put_slab_file(&path, &uuid, &HashSet::new())
            .await
            .unwrap();
        assert_eq!(again.extents, first.extents, "the same manifest");
        assert_eq!(
            store.list_relative("db/").await.unwrap(),
            objects,
            "an unchanged file adds nothing"
        );
    }

    /// A slab the active base already names is not sent, and not asked about.
    ///
    /// Run against an **empty** store, so the old behaviour -- `head`, miss,
    /// upload -- would have written every object. Nothing is written, which
    /// is only correct because the set is taken on trust: the active base is
    /// never deletable, so an object it names is in the store. The manifest
    /// is still complete, because a skipped upload must not mean a skipped
    /// extent.
    #[tokio::test]
    async fn a_slab_the_active_base_already_holds_is_never_sent() {
        let dir = tempfile::tempdir().unwrap();
        let path = slab_file(dir.path(), "shard-0000.yno", 4, &[(0, 7), (1, 9)]);
        let uuid = "ef".repeat(16);

        let survey = ArchiveStore::new(Arc::new(InMemory::new()), ObjectPath::ROOT);
        let full = survey
            .put_slab_file(&path, &uuid, &HashSet::new())
            .await
            .unwrap();
        let known: HashSet<[u8; 32]> = full
            .extents
            .iter()
            .map(|e| <[u8; 32]>::try_from(e.content_hash.as_slice()).unwrap())
            .collect();
        assert_eq!(known.len(), 2);

        let empty = ArchiveStore::new(Arc::new(InMemory::new()), ObjectPath::ROOT);
        let again = empty.put_slab_file(&path, &uuid, &known).await.unwrap();
        assert_eq!(again.extents, full.extents, "the manifest is unchanged");
        assert!(
            empty.list_relative("db/").await.unwrap().is_empty(),
            "nothing was uploaded and nothing was asked about"
        );
    }

    /// The skip set is empty whenever anything about the active base is
    /// unknown, because an empty set only costs a round trip while a wrong
    /// one loses data.
    #[tokio::test]
    async fn the_skip_set_is_empty_when_there_is_no_active_base() {
        let inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let store = ArchiveStore::new(Arc::clone(&inner), ObjectPath::ROOT);
        let write = |key: &str, bytes: Vec<u8>| {
            let inner = Arc::clone(&inner);
            let path = ObjectPath::from(key);
            async move { inner.put(&path, PutPayload::from(bytes)).await.unwrap() }
        };

        assert!(
            store.slabs_of_active_base().await.unwrap().is_empty(),
            "no state.pb at all"
        );

        write(STATE_OBJECT, b"not protobuf at all".to_vec()).await;
        assert!(
            store.slabs_of_active_base().await.unwrap().is_empty(),
            "a state.pb that will not load"
        );

        write(
            STATE_OBJECT,
            pb::ArchiveState {
                schema_version: SCHEMA_VERSION,
                ..Default::default()
            }
            .encode_to_vec(),
        )
        .await;
        assert!(
            store.slabs_of_active_base().await.unwrap().is_empty(),
            "a valid state naming no base"
        );

        let key = "db/ef/term/0000000000/base/0/manifest.pb";
        let state = pb::ArchiveState {
            schema_version: SCHEMA_VERSION,
            latest_base_manifest: key.into(),
            ..Default::default()
        };
        write(STATE_OBJECT, state.encode_to_vec()).await;
        assert!(
            store.slabs_of_active_base().await.unwrap().is_empty(),
            "a base object that is not there"
        );

        write(key, b"not protobuf at all".to_vec()).await;
        assert!(
            store.slabs_of_active_base().await.unwrap().is_empty(),
            "a manifest that will not decode"
        );
    }

    /// The extent encoding is canonical in both shapes, and each refusal is
    /// one way a writer could produce a list a restore would have to guess at.
    ///
    /// The shapes disagree about adjacency and the disagreement is the point:
    /// under one object per file, touching runs mean the writer failed to
    /// merge them; under one object per slab, touching runs are two slabs that
    /// both carry data.
    #[test]
    fn a_non_canonical_extent_list_is_refused() {
        let whole = |sparse: bool, extents: Vec<(u64, u64)>| pb::ArchiveFile {
            name: "shard-0000.yno".into(),
            object_key: "k".into(),
            size: 4096,
            crc32c: 0,
            sparse,
            extents: extents
                .into_iter()
                .map(|(offset, length)| pb::FileExtent {
                    offset,
                    length,
                    content_hash: Vec::new(),
                    crc32c: 0,
                })
                .collect(),
        };
        let slab = SLAB_OBJECT_BYTES as u64;
        let addressed = |extents: Vec<(u64, u64, usize)>| pb::ArchiveFile {
            name: "shard-0000.yno".into(),
            object_key: String::new(),
            size: slab * 8,
            crc32c: 0,
            sparse: true,
            extents: extents
                .into_iter()
                .map(|(offset, length, hash_len)| pb::FileExtent {
                    offset,
                    length,
                    content_hash: vec![7u8; hash_len],
                    crc32c: 0,
                })
                .collect(),
        };

        validate_extents(&whole(false, vec![])).unwrap();
        validate_extents(&whole(true, vec![])).unwrap();
        validate_extents(&whole(true, vec![(0, 1024), (2048, 1024)])).unwrap();
        validate_extents(&addressed(vec![(0, slab, 32), (slab, slab, 32)]))
            .expect("touching slabs are the ordinary case");
        validate_extents(&addressed(vec![(slab * 3, 16, 32)])).expect("a short last slab is legal");

        for (why, bad) in [
            ("extents without the flag", whole(false, vec![(0, 16)])),
            ("an empty run", whole(true, vec![(0, 0)])),
            (
                "runs that touch in one object",
                whole(true, vec![(0, 1024), (1024, 16)]),
            ),
            ("runs that overlap", whole(true, vec![(0, 1024), (512, 16)])),
            ("runs out of order", whole(true, vec![(2048, 16), (16, 16)])),
            ("a run past the end", whole(true, vec![(4000, 1024)])),
            ("a run that overflows", whole(true, vec![(u64::MAX, 2)])),
            ("a short address", addressed(vec![(0, slab, 16)])),
            ("an unaligned slab", addressed(vec![(64, slab, 32)])),
            ("a run spanning slabs", addressed(vec![(0, slab * 2, 32)])),
            (
                "addressed slabs that overlap",
                addressed(vec![(0, slab, 32), (0, slab, 32)]),
            ),
        ] {
            assert!(validate_extents(&bad).is_err(), "{why}");
        }

        // Naming one object *and* addressing every extent is the ambiguity
        // the shapes exist to avoid, as is doing neither.
        let mut both = addressed(vec![(0, slab, 32)]);
        both.object_key = "k".into();
        assert!(validate_extents(&both).is_err(), "both shapes at once");
        let mut neither = addressed(vec![(0, slab, 32)]);
        neither.extents[0].content_hash.clear();
        neither.object_key.clear();
        assert!(validate_extents(&neither).is_err(), "neither shape");
        // A list that starts addressed and stops is refused per extent.
        let mut mixed = addressed(vec![(0, slab, 32), (slab, slab, 0)]);
        mixed.object_key.clear();
        assert!(validate_extents(&mixed).is_err(), "a mixed list");
    }
}
