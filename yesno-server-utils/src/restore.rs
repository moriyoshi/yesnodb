//! Point-in-time reconstruction from a completed archive base and WAL chain.
//!
//! A restore is built in a sibling staging directory, verified and recovered by
//! the ordinary replica path, then made visible with one rename.
//!
//! # Two kinds of target, one resolution
//!
//! A target names either an exact logical commit version or a wall clock. The
//! wall clock is answerable because every commit marker carries the time its
//! version was assigned, non-decreasing in commit version by invariant **I9** —
//! so "everything at or before T" is genuinely a prefix, and a time resolves to
//! the highest committed version at or before it. Everything after that point is
//! the version path, unchanged: one cut, ordinary replica recovery, one rename.
//!
//! Object upload time is still not commit time and is still not used. An
//! upload may be retried or delayed long after the commit it carries, so the
//! only defensible source is the stamp inside the frame.
//!
//! # Absence is refused, not rounded
//!
//! A log written before commit-time stamping carries no times, and this module
//! reads that as *unknown* rather than as the epoch. A wall-clock target whose
//! range includes an unstamped version fails and names it. The alternative —
//! treating an unstamped commit as 1970 — would silently place the entire
//! pre-stamping history before every target an operator could type, which is the
//! precise-looking wrong answer this module exists to avoid.
//!
//! # Where the times are trusted from
//!
//! The archive's Protobuf descriptors carry commit times so a restore can narrow
//! which base and which objects to fetch. They are **hints**: they are
//! deliberately outside the SHA-256 history commitments, because adding a field
//! to those hashes would change the fingerprint of every existing history and
//! break chain continuity at the sidecar's next reconnect. The times a cut is
//! made on are read back out of the verified frames.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use clap::Args;
use futures::TryStreamExt;
use object_store::ObjectStoreExt;
use prost::Message;
use tokio::io::AsyncWriteExt;
use yesno_core::wal::{Scanner, ShardLog, WalWriter};
use yesno_core::{Db, DbOptions};

use crate::archive::{
    inspect_base, pb, validate_base_manifest, validate_wal_object, ArchiveError, ArchiveStore,
};

static STAGING_ID: AtomicU64 = AtomicU64::new(0);

/// Where a restore is asked to stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum RecoveryTarget {
    /// The archive's complete durable prefix.
    #[default]
    Tip,
    /// An exact committed logical version. Fails if that version is not a
    /// complete commit, rather than rounding to a neighbour.
    Version(u64),
    /// A wall clock, in UNIX epoch microseconds.
    ///
    /// Resolves to the highest committed version whose commit time satisfies the
    /// bound. `inclusive` decides whether a commit stamped exactly at the target
    /// is kept — the difference matters precisely when an operator has the
    /// timestamp of the transaction they are recovering *away* from.
    Time { micros: i64, inclusive: bool },
}

impl RecoveryTarget {
    fn is_time(self) -> bool {
        matches!(self, RecoveryTarget::Time { .. })
    }
}

/// What a restore does with the recovered staging directory.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum TargetAction {
    /// Rename the staging directory onto the target. The default.
    #[default]
    Publish,
    /// Verify and recover, then stop, leaving the staging directory in place and
    /// naming it in the report. For inspecting what a target would produce
    /// before committing to it.
    Pause,
    /// Raise the leadership term before publishing, so the restored copy is a
    /// *new timeline* rather than a second copy of the original's.
    ///
    /// This is the option to use when the restored database will be written
    /// to. Without it a restored copy carries the original's term, and an
    /// archiver or follower pointed at it cannot tell the two histories apart —
    /// they share a database UUID, which is exactly the case UUID alone cannot
    /// fence.
    Promote,
}

/// Command-line options for one archive restore.
#[derive(Args, Debug)]
pub struct RestoreOptions {
    /// Object-store URL: file:///path, s3://bucket/prefix, or compatible S3.
    #[arg(long, env = "YESNO_RESTORE_STORE")]
    store: String,

    /// New directory to publish. It must not already exist.
    ///
    /// Not required with --inspect, which creates no directory.
    #[arg(
        short = 'D',
        long,
        value_name = "DIR",
        required_unless_present = "inspect"
    )]
    target: Option<PathBuf>,

    /// Logical commit version to recover through. Omit for the durable tip.
    #[arg(long, conflicts_with = "target_time")]
    target_version: Option<u64>,

    /// Wall-clock instant to recover through, as RFC 3339
    /// ( e.g. 2026-09-06T14:02:00Z ). Omit for the durable tip.
    #[arg(long, conflicts_with = "target_version")]
    target_time: Option<String>,

    /// Exclude a commit stamped exactly at --target-time. Default is to include
    /// it. Meaningless without --target-time.
    #[arg(long, requires = "target_time")]
    target_exclusive: bool,

    /// What to do with the recovered directory.
    #[arg(long, value_enum, default_value_t = TargetAction::Publish)]
    target_action: TargetAction,

    /// Report the recovery windows this archive offers and exit. Downloads no
    /// base files and creates no staging directory.
    #[arg(long)]
    inspect: bool,
}

impl RestoreOptions {
    /// The requested target, or an error if the wall clock does not parse.
    pub fn recovery_target(&self) -> Result<RecoveryTarget, ArchiveError> {
        match (&self.target_version, &self.target_time) {
            (Some(_), Some(_)) => {
                Err("--target-version and --target-time are mutually exclusive".into())
            }
            (Some(v), None) => Ok(RecoveryTarget::Version(*v)),
            (None, Some(text)) => Ok(RecoveryTarget::Time {
                micros: parse_rfc3339_micros(text)?,
                inclusive: !self.target_exclusive,
            }),
            (None, None) => Ok(RecoveryTarget::Tip),
        }
    }
}

/// Parse an RFC 3339 instant into UNIX epoch microseconds.
///
/// An offset is required. A bare `2026-09-06T14:02:00` is ambiguous by up to
/// a day across deployments, and guessing UTC for an operator who meant local
/// time is a silent wrong answer of exactly the kind a recovery target must not
/// produce — so it is refused with the fix in the message.
pub fn parse_rfc3339_micros(text: &str) -> Result<i64, ArchiveError> {
    let parsed = chrono::DateTime::parse_from_rfc3339(text).map_err(|error| {
        format!(
            "'{text}' is not an RFC 3339 instant ({error}); it must carry an offset, \
             for example 2026-09-06T14:02:00Z or 2026-09-06T23:02:00+09:00"
        )
    })?;
    Ok(parsed.timestamp_micros())
}

/// Render UNIX epoch microseconds as RFC 3339, for reports and messages.
pub fn format_micros(micros: u64) -> String {
    chrono::DateTime::from_timestamp_micros(micros as i64)
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Micros, true))
        .unwrap_or_else(|| format!("{micros}us"))
}

/// Facts about one durably published restored directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoreReport {
    /// Final restored directory.
    pub directory: PathBuf,
    /// Database identity selected from archive state.
    pub database_uuid: [u8; 16],
    /// Leadership timeline containing the selected base.
    pub term: u32,
    /// Base generation used for reconstruction.
    pub base_generation: u64,
    /// Base checkpoint watermark.
    pub checkpoint_version: u64,
    /// Complete logical version recovered after the requested cut.
    pub recovered_version: u64,
    /// Commit time of `recovered_version`, when the history carries one.
    ///
    /// `None` for a version written before commit-time stamping, and for a
    /// restore that stopped exactly at the base checkpoint ( whose stamp lives
    /// in the image, not in the replayed log ).
    pub recovered_time: Option<u64>,
    /// The target this restore was asked for, echoed back.
    pub target: RecoveryTarget,
    /// Physical shard count.
    pub shards: u32,
    /// Bytes in the published directory.
    pub bytes: u64,
    /// What was done with the recovered directory.
    pub action: TargetAction,
    /// Set when `action` was [`TargetAction::Pause`]: the staging directory that
    /// was left in place instead of being published.
    pub paused_at: Option<PathBuf>,
}

struct StagingDir {
    path: PathBuf,
    published: bool,
}

impl Drop for StagingDir {
    fn drop(&mut self) {
        if !self.published {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

fn target_parts(target: &Path) -> Result<(&Path, &std::ffi::OsStr), ArchiveError> {
    let name = target.file_name().ok_or_else(|| {
        format!(
            "restore target '{}' has no directory name",
            target.display()
        )
    })?;
    let parent = target
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    Ok((parent, name))
}

fn create_staging(target: &Path) -> Result<StagingDir, ArchiveError> {
    let (parent, name) = target_parts(target)?;
    std::fs::create_dir_all(parent)?;
    for _ in 0..100 {
        let id = STAGING_ID.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(
            ".{}.yesno-restore.{}.{}.partial",
            name.to_string_lossy(),
            std::process::id(),
            id
        ));
        match std::fs::create_dir(&path) {
            Ok(()) => {
                return Ok(StagingDir {
                    path,
                    published: false,
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err("could not allocate a unique restore staging directory".into())
}

fn uuid_hex(uuid: &[u8]) -> String {
    uuid.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Is this base at or below the target, and therefore a legal starting point?
///
/// For a wall clock the comparison is against the manifest's `checkpoint_time`,
/// an upper bound on every version at or below the base's watermark — so a base
/// that passes is guaranteed not to sit above the target. A base publishing no
/// bound ( zero ) predates commit-time stamping and cannot answer a wall-clock
/// target; it is skipped here and reported as unusable if nothing else matches.
fn base_is_at_or_below(manifest: &pb::BaseManifest, target: RecoveryTarget) -> bool {
    match target {
        RecoveryTarget::Tip => true,
        RecoveryTarget::Version(v) => manifest.checkpoint_version <= v,
        RecoveryTarget::Time { micros, .. } => {
            manifest.checkpoint_time != 0 && manifest.checkpoint_time <= micros as u64
        }
    }
}

async fn load_base(
    store: &ArchiveStore,
    state: &pb::ArchiveState,
    target: RecoveryTarget,
) -> Result<(String, pb::BaseManifest), ArchiveError> {
    if target == RecoveryTarget::Tip {
        if state.latest_base_manifest.is_empty() {
            return Err("archive has no completed base manifest".into());
        }
        let manifest =
            pb::BaseManifest::decode(store.get_bytes(&state.latest_base_manifest).await?)?;
        validate_base_manifest(&manifest)?;
        return Ok((state.latest_base_manifest.clone(), manifest));
    }

    let mut unstamped = 0usize;
    let mut selected: Option<(String, pb::BaseManifest)> = None;
    for (key, manifest) in list_bases(store, &state.database_uuid).await? {
        if target.is_time() && manifest.checkpoint_time == 0 {
            unstamped += 1;
        }
        if !base_is_at_or_below(&manifest, target) {
            continue;
        }
        let replace = selected
            .as_ref()
            .is_none_or(|(_, current)| manifest.archive_generation > current.archive_generation);
        if replace {
            selected = Some((key, manifest));
        }
    }
    selected.ok_or_else(|| match target {
        RecoveryTarget::Version(v) => {
            format!("archive has no base checkpoint at or below target version {v}").into()
        }
        RecoveryTarget::Time { micros, .. } if unstamped > 0 => format!(
            "archive has no base checkpoint at or below {}; {unstamped} base(s) publish no \
             commit time, so they predate commit-time stamping and cannot answer a wall-clock \
             target. Use --target-version against those.",
            format_micros(micros as u64)
        )
        .into(),
        RecoveryTarget::Time { micros, .. } => format!(
            "archive has no base checkpoint at or below {}",
            format_micros(micros as u64)
        )
        .into(),
        RecoveryTarget::Tip => "archive has no completed base manifest".into(),
    })
}

/// Every completed base manifest for this database, with its object key.
async fn list_bases(
    store: &ArchiveStore,
    database_uuid: &[u8],
) -> Result<Vec<(String, pb::BaseManifest)>, ArchiveError> {
    let prefix = format!("db/{}/term/", uuid_hex(database_uuid));
    let mut out = Vec::new();
    for key in store.list_relative(&prefix).await? {
        if !key.ends_with("/manifest.pb") || !key.contains("/base/") {
            continue;
        }
        let manifest = pb::BaseManifest::decode(store.get_bytes(&key).await?)?;
        validate_base_manifest(&manifest)?;
        if manifest.database_uuid != database_uuid {
            continue;
        }
        out.push((key, manifest));
    }
    Ok(out)
}

async fn download_base_file(
    store: &ArchiveStore,
    file: &pb::ArchiveFile,
    target: &Path,
) -> Result<(), ArchiveError> {
    let relative = Path::new(&file.name);
    if relative.components().count() != 1
        || relative
            .file_name()
            .is_none_or(|name| name != file.name.as_str())
    {
        return Err(format!("base manifest file name '{}' is not flat", file.name).into());
    }
    let result = store.inner.get(&store.path(&file.object_key)).await?;
    if result.meta.size != file.size {
        return Err(format!(
            "base object '{}' is {} bytes, manifest says {}",
            file.object_key, result.meta.size, file.size
        )
        .into());
    }
    let mut stream = result.into_stream();
    let mut output = tokio::fs::File::create(target.join(&file.name)).await?;
    let mut size = 0u64;
    let mut checksum = 0u32;
    while let Some(chunk) = stream.try_next().await? {
        size = size.saturating_add(chunk.len() as u64);
        checksum = crc32c::crc32c_append(checksum, &chunk);
        output.write_all(&chunk).await?;
    }
    output.sync_all().await?;
    if size != file.size || checksum != file.crc32c {
        return Err(format!(
            "base object '{}' failed size or CRC verification",
            file.object_key
        )
        .into());
    }
    Ok(())
}

async fn materialize_base(
    store: &ArchiveStore,
    manifest: &pb::BaseManifest,
    target: &Path,
) -> Result<(), ArchiveError> {
    for file in &manifest.files {
        download_base_file(store, file, target).await?;
    }
    tokio::fs::File::open(target).await?.sync_all().await?;
    let inspection = inspect_base(target)?;
    if inspection.database_uuid.as_slice() != manifest.database_uuid
        || inspection.term != manifest.term
        || inspection.checkpoint_version != manifest.checkpoint_version
        || inspection.wal_cursors.len() != manifest.wal_cursors.len()
    {
        return Err("downloaded base does not match its manifest identity or checkpoint".into());
    }
    for (actual, expected) in inspection.wal_cursors.iter().zip(&manifest.wal_cursors) {
        if actual.shard != expected.shard || actual.archived_lsn != expected.archived_lsn {
            return Err(format!(
                "downloaded base shard {} ends at {}, manifest says {}",
                actual.shard, actual.archived_lsn, expected.archived_lsn
            )
            .into());
        }
    }
    Ok(())
}

async fn load_wal_descriptors(
    store: &ArchiveStore,
    manifest: &pb::BaseManifest,
) -> Result<BTreeMap<u32, Vec<pb::WalObject>>, ArchiveError> {
    let prefix = format!(
        "db/{}/term/{:010}/wal/",
        uuid_hex(&manifest.database_uuid),
        manifest.term
    );
    let mut by_shard: BTreeMap<u32, Vec<pb::WalObject>> = BTreeMap::new();
    for key in store.list_relative(&prefix).await? {
        if !key.ends_with(".wal.pb") {
            continue;
        }
        let descriptor = pb::WalObject::decode(store.get_bytes(&key).await?)?;
        if descriptor.database_uuid != manifest.database_uuid || descriptor.term != manifest.term {
            return Err(
                format!("WAL descriptor '{key}' escaped its database or term prefix").into(),
            );
        }
        by_shard
            .entry(descriptor.shard)
            .or_default()
            .push(descriptor);
    }
    Ok(by_shard)
}

async fn assemble_wal(
    store: &ArchiveStore,
    state: &pb::ArchiveState,
    manifest_key: &str,
    manifest: &pb::BaseManifest,
    target: &Path,
) -> Result<(), ArchiveError> {
    let descriptors = load_wal_descriptors(store, manifest).await?;
    let state_tip = (manifest_key == state.latest_base_manifest && manifest.term == state.term)
        .then_some(&state.wal_cursors);

    for base_cursor in &manifest.wal_cursors {
        let path = target.join(format!("shard-{:04}.wal", base_cursor.shard));
        let mut writer = WalWriter::open(&path, base_cursor.archived_lsn)?;
        if writer.end_lsn() != base_cursor.archived_lsn {
            return Err(format!(
                "base shard {} WAL ends at {}, manifest says {}",
                base_cursor.shard,
                writer.end_lsn(),
                base_cursor.archived_lsn
            )
            .into());
        }
        let expected_tip =
            state_tip.and_then(|tips| tips.iter().find(|cursor| cursor.shard == base_cursor.shard));
        let mut lsn = base_cursor.archived_lsn;
        let mut fingerprint = base_cursor.history_fingerprint.clone();
        loop {
            if expected_tip.is_some_and(|tip| {
                tip.archived_lsn == lsn && tip.history_fingerprint == fingerprint
            }) {
                break;
            }
            let children = descriptors
                .get(&base_cursor.shard)
                .into_iter()
                .flatten()
                .filter(|object| {
                    object.first_lsn == lsn && object.previous_fingerprint == fingerprint
                })
                .collect::<Vec<_>>();
            match children.as_slice() {
                [] if expected_tip.is_none() => break,
                [] => {
                    return Err(format!(
                        "WAL history for shard {} ends at {lsn} before the durable state tip",
                        base_cursor.shard
                    )
                    .into());
                }
                [object] => {
                    if expected_tip.is_some_and(|tip| object.last_lsn > tip.archived_lsn) {
                        return Err(format!(
                            "WAL object for shard {} runs past the durable state tip",
                            base_cursor.shard
                        )
                        .into());
                    }
                    let records = store.get_bytes(&object.object_key).await?;
                    validate_wal_object(object, &records)?;
                    writer.append_frames_at(object.first_lsn, &records)?;
                    lsn = object.last_lsn;
                    fingerprint = object.history_fingerprint.clone();
                }
                _ => {
                    return Err(format!(
                        "WAL history for shard {} forks at LSN {lsn}",
                        base_cursor.shard
                    )
                    .into());
                }
            }
        }
        writer.sync()?;
    }
    Ok(())
}

/// Whether a listed WAL object lies past the durable tip a restore will walk to.
///
/// `stage_wal` stops at `state.wal_cursors` for the manifest it restores
/// from, so a window that counts objects beyond it promises a version the
/// restore will not reach. An object can be uploaded and listable before the
/// cursor advances: the sidecar publishes the object, then `set_cursor_history`,
/// then `publish_state`.
///
/// `None` means the manifest is not the one the durable tip describes -- a
/// superseded base -- and `stage_wal` then walks its chain to the end, so
/// nothing is out of bounds.
fn beyond_durable_tip(shard: u32, last_lsn: u64, tip: Option<&Vec<pb::WalCursor>>) -> bool {
    tip.is_some_and(|cursors| {
        cursors
            .iter()
            .any(|c| c.shard == shard && last_lsn > c.archived_lsn)
    })
}

/// Resolve a wall clock to the version a cut should be made at.
///
/// `select = max { v in (checkpoint, global_cv] : time(v) <= T }`, or `< T` when
/// the target is exclusive. Falling back to the base checkpoint when nothing in
/// the range qualifies is correct: the base is by construction at or below the
/// target, so "everything at or before T" is exactly the base.
///
/// Every version in the range must be stamped, and the scan checks the whole
/// range rather than stopping at the first match. A gap above the chosen version
/// still matters: the times are what prove `select` is the *highest* qualifying
/// version, and an unknown one could have qualified.
fn resolve_time(
    plan: &yesno_core::wal::RecoveryPlan,
    checkpoint_version: u64,
    micros: i64,
    inclusive: bool,
) -> Result<u64, ArchiveError> {
    if micros < 0 {
        return Err("a recovery target before the UNIX epoch selects nothing".into());
    }
    let bound = micros as u64;
    let mut selected = checkpoint_version;
    let mut version = checkpoint_version + 1;
    while version <= plan.global_cv {
        let Some(time) = plan.times.get(&version).copied() else {
            return Err(format!(
                "commit version {version} carries no commit time, so a wall-clock target \
                 cannot be honoured over this history; it was written before commit-time \
                 stamping existed. Use --target-version instead."
            )
            .into());
        };
        let qualifies = if inclusive {
            time <= bound
        } else {
            time < bound
        };
        if qualifies {
            selected = version;
        }
        version += 1;
    }
    Ok(selected)
}

fn plan_and_cut(
    target: &Path,
    manifest: &pb::BaseManifest,
    recovery_target: RecoveryTarget,
) -> Result<(u64, Option<u64>), ArchiveError> {
    let mut owned = Vec::with_capacity(manifest.wal_cursors.len());
    for cursor in &manifest.wal_cursors {
        let writer = WalWriter::open(
            target.join(format!("shard-{:04}.wal", cursor.shard)),
            cursor.archived_lsn,
        )?;
        owned.push((cursor.shard, writer.base(), writer.read_all()?));
    }
    let logs = owned
        .iter()
        .map(|(shard, base_lsn, bytes)| ShardLog {
            shard: *shard,
            bytes,
            base_lsn: *base_lsn,
        })
        .collect::<Vec<_>>();
    let plan = yesno_core::wal::plan(&logs, manifest.checkpoint_version)?;
    let selected = match recovery_target {
        RecoveryTarget::Tip => plan.global_cv,
        RecoveryTarget::Version(v) => v,
        RecoveryTarget::Time { micros, inclusive } => {
            resolve_time(&plan, manifest.checkpoint_version, micros, inclusive)?
        }
    };
    if selected < manifest.checkpoint_version {
        return Err(format!(
            "target version {selected} precedes base checkpoint {}",
            manifest.checkpoint_version
        )
        .into());
    }
    if selected > plan.global_cv {
        return Err(format!(
            "target version {selected} is beyond the archive's complete prefix {}",
            plan.global_cv
        )
        .into());
    }

    if selected < plan.global_cv {
        for (shard, base_lsn, bytes) in &owned {
            let mut scanner = Scanner::new(bytes, *base_lsn);
            let mut cut = None;
            for record in &mut scanner {
                let record = record?;
                if record.commit_version > selected {
                    cut = Some(record.lsn);
                    break;
                }
            }
            let cut = cut.unwrap_or_else(|| scanner.stopped_at());
            let mut writer =
                WalWriter::open(target.join(format!("shard-{shard:04}.wal")), *base_lsn)?;
            writer.truncate_to(cut)?;
        }
    }
    // `None` when the cut landed on the base checkpoint itself: that version's
    // stamp lives in the image, not in the replayed log.
    Ok((selected, plan.times.get(&selected).copied()))
}

fn sync_tree(dir: &Path) -> Result<u64, ArchiveError> {
    let mut bytes = 0u64;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            let file = std::fs::File::open(entry.path())?;
            bytes = bytes.saturating_add(file.metadata()?.len());
            file.sync_all()?;
        }
    }
    std::fs::File::open(dir)?.sync_all()?;
    Ok(bytes)
}

/// Reconstruct and atomically publish one archived database directory.
///
/// Kept for callers that only ever want a version or the tip.
pub async fn restore(
    store: ArchiveStore,
    target: impl AsRef<Path>,
    target_version: Option<u64>,
) -> Result<RestoreReport, ArchiveError> {
    let recovery_target = match target_version {
        Some(v) => RecoveryTarget::Version(v),
        None => RecoveryTarget::Tip,
    };
    restore_to(store, target, recovery_target, TargetAction::Publish).await
}

/// Reconstruct one archived database directory at `recovery_target`.
pub async fn restore_to(
    store: ArchiveStore,
    target: impl AsRef<Path>,
    recovery_target: RecoveryTarget,
    action: TargetAction,
) -> Result<RestoreReport, ArchiveError> {
    let target = target.as_ref();
    if target.exists() {
        return Err(format!("restore target '{}' already exists", target.display()).into());
    }
    let state = store.load_state().await?.ok_or("archive has no state.pb")?;
    let (manifest_key, manifest) = load_base(&store, &state, recovery_target).await?;
    let mut staging = create_staging(target)?;
    materialize_base(&store, &manifest, &staging.path).await?;
    assemble_wal(&store, &state, &manifest_key, &manifest, &staging.path).await?;
    let (selected, recovered_time) = plan_and_cut(&staging.path, &manifest, recovery_target)?;

    let db = Db::open_replica(&staging.path, DbOptions::default())?;
    let recovered_version = db.visible();
    let shards = db.shard_count() as u32;
    drop(db);
    if recovered_version != selected {
        return Err(format!(
            "ordinary recovery reached version {recovered_version}, expected {selected}"
        )
        .into());
    }
    if shards != manifest.wal_cursors.len() as u32 {
        return Err("restored MANIFEST shard topology differs from the base manifest".into());
    }

    // Before the directory is published, never after. A term raised on a
    // directory an operator has already started serving from is a second
    // timeline with no fence between it and the first.
    let mut term = manifest.term;
    if action == TargetAction::Promote {
        term = yesno_core::promote_database(&staging.path, term.saturating_add(1))?;
    }

    let bytes = sync_tree(&staging.path)?;
    let mut database_uuid = [0u8; 16];
    database_uuid.copy_from_slice(&manifest.database_uuid);
    let mut report = RestoreReport {
        directory: target.to_path_buf(),
        database_uuid,
        term,
        base_generation: manifest.archive_generation,
        checkpoint_version: manifest.checkpoint_version,
        recovered_version,
        recovered_time,
        target: recovery_target,
        shards,
        bytes,
        action,
        paused_at: None,
    };

    if action == TargetAction::Pause {
        // Verified and recovered, deliberately unpublished. `StagingDir::drop`
        // would remove it, so ownership is released here rather than marking it
        // published — the directory outlives this call by design.
        report.directory = staging.path.clone();
        report.paused_at = Some(staging.path.clone());
        staging.published = true;
        return Ok(report);
    }

    let (parent, _) = target_parts(target)?;
    if target.exists() {
        return Err(format!(
            "restore target '{}' appeared during recovery",
            target.display()
        )
        .into());
    }
    std::fs::rename(&staging.path, target)?;
    std::fs::File::open(parent)?.sync_all()?;
    staging.published = true;
    Ok(report)
}

/// One base generation, and the range of targets it can answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryWindow {
    /// Object key of the base manifest that roots this window.
    pub manifest_key: String,
    /// Leadership timeline the base belongs to.
    pub term: u32,
    /// Base generation.
    pub base_generation: u64,
    /// Lowest version this window can restore to.
    pub checkpoint_version: u64,
    /// Highest version this window can restore to.
    pub end_version: u64,
    /// Upper bound on the commit time at `checkpoint_version`, or `None` when
    /// the base predates commit-time stamping.
    pub checkpoint_time: Option<u64>,
    /// Commit time at `end_version`, when the archived descriptors carry one.
    pub end_time: Option<u64>,
    /// Whether every archived WAL object in this window carries a commit time.
    ///
    /// A wall-clock target can only be answered inside a window where this is
    /// true. It is reported rather than inferred so an operator finds out from
    /// `--inspect` rather than from a failed recovery.
    pub fully_stamped: bool,
    /// This is the window `state.pb` currently designates as the active root.
    pub active: bool,
}

/// Enumerate the recovery windows this archive offers.
///
/// Reads manifests and WAL descriptors only. It downloads no base files, writes
/// nothing, and creates no staging directory — so it is safe to run against a
/// production archive while the sidecar is live.
///
/// The bounds come from the descriptors, which are *hints* outside the
/// history commitments. They are right for choosing a target to type; the cut a
/// restore actually makes is resolved from verified frames. A window will
/// therefore never mislead about *whether* a target is answerable, only ever by
/// a frame at its edge about exactly where the edge is.
pub async fn inspect(store: &ArchiveStore) -> Result<Vec<RecoveryWindow>, ArchiveError> {
    let state = store.load_state().await?.ok_or("archive has no state.pb")?;
    let mut windows = Vec::new();
    for (manifest_key, manifest) in list_bases(store, &state.database_uuid).await? {
        let prefix = format!(
            "db/{}/term/{:010}/wal/",
            uuid_hex(&manifest.database_uuid),
            manifest.term
        );
        let base_version = manifest.recovered_version.max(manifest.checkpoint_version);
        // The durable tip bounds this window only for the manifest a restore
        // would take it from -- the same condition `stage_wal` applies.
        let active_tip = (manifest_key == state.latest_base_manifest
            && manifest.term == state.term)
            .then_some(&state.wal_cursors);
        // version -> whether any object for it carries a stamp.
        //
        // Per *version*, not per object. Objects are normalized to individual
        // frames and only the commit markers are stamped, so a version's
        // `SetRange` frames legitimately report zero. Reading "unstamped" off an
        // individual object would mark every window as unanswerable by wall
        // clock, including the ones that are entirely stamped.
        let mut stamped: BTreeMap<u64, Option<u64>> = BTreeMap::new();
        let mut saw_object = false;
        for key in store.list_relative(&prefix).await? {
            if !key.ends_with(".wal.pb") {
                continue;
            }
            let object = pb::WalObject::decode(store.get_bytes(&key).await?)?;
            if object.database_uuid != manifest.database_uuid || object.term != manifest.term {
                continue;
            }
            // Objects below the base's own cursor belong to an earlier window.
            if manifest
                .wal_cursors
                .iter()
                .any(|c| c.shard == object.shard && object.last_lsn <= c.archived_lsn)
            {
                continue;
            }
            // **A window must promise only what a restore will actually
            // stage.** `stage_wal` walks the descriptor chain up to the durable
            // `state.wal_cursors` tip and stops there, deliberately: the tip is
            // what the archive has *committed to*, and an object can be
            // uploaded and listable before the cursor advances -- the sidecar
            // publishes the object, then `set_cursor_history`, then
            // `publish_state`. Stopping the sidecar inside that gap leaves the
            // object present and the cursor behind it.
            //
            // Reading `end_version` off the listed objects alone therefore
            // advertised a version the restore would not walk to. The symptom
            // was a window reporting `end_version: 4` whose restore returned
            // `recovered: 3, recovered_time: None` and an empty key -- see
            // `pitr_retention.py`. Do not "fix" that by letting `stage_wal`
            // run past the tip; the tip is a durability boundary, not a hint.
            if beyond_durable_tip(object.shard, object.last_lsn, active_tip) {
                continue;
            }
            saw_object = true;
            let slot = stamped.entry(object.last_version).or_default();
            if object.last_time != 0 {
                *slot = Some(slot.unwrap_or(0).max(object.last_time));
            }
        }
        let end_version = stamped.keys().next_back().copied().unwrap_or(base_version);
        let mut end_time = stamped.get(&end_version).copied().flatten();
        let mut fully_stamped = stamped.values().all(|t| t.is_some());
        if !saw_object {
            end_time = (manifest.checkpoint_time != 0).then_some(manifest.checkpoint_time);
            fully_stamped = true;
        }
        windows.push(RecoveryWindow {
            active: manifest_key == state.latest_base_manifest,
            manifest_key,
            term: manifest.term,
            base_generation: manifest.archive_generation,
            checkpoint_version: manifest.checkpoint_version,
            end_version,
            checkpoint_time: (manifest.checkpoint_time != 0).then_some(manifest.checkpoint_time),
            end_time,
            fully_stamped: fully_stamped && manifest.checkpoint_time != 0,
        });
    }
    windows.sort_by_key(|w| (w.term, w.base_generation));
    Ok(windows)
}

/// Stable human-facing line for one recovery window.
pub fn window_line(w: &RecoveryWindow) -> String {
    let time = |t: Option<u64>| t.map(format_micros).unwrap_or_else(|| "unstamped".into());
    format!(
        "window: term={} base={} versions={}..{} times={}..{} wall_clock={} active={}",
        w.term,
        w.base_generation,
        w.checkpoint_version,
        w.end_version,
        time(w.checkpoint_time),
        time(w.end_time),
        if w.fully_stamped { "yes" } else { "no" },
        w.active,
    )
}

/// Connect to the configured archive and run one restore.
pub async fn run(options: RestoreOptions) -> Result<Option<RestoreReport>, ArchiveError> {
    let store = ArchiveStore::connect(&options.store)?;
    let recovery_target = options.recovery_target()?;
    if options.inspect {
        let windows = inspect(&store).await?;
        if windows.is_empty() {
            println!("archive offers no completed recovery window");
        }
        for w in &windows {
            println!("{}", window_line(w));
        }
        return Ok(None);
    }
    let target = options
        .target
        .ok_or("--target is required unless --inspect is given")?;
    restore_to(store, target, recovery_target, options.target_action)
        .await
        .map(Some)
}

/// Stable human-facing completion line.
///
/// Fields are appended, never reordered or removed: operators grep this.
pub fn completion_line(report: &RestoreReport) -> String {
    let time = report
        .recovered_time
        .map(format_micros)
        .unwrap_or_else(|| "unstamped".to_string());
    let verb = match report.action {
        TargetAction::Pause => "restore staged (unpublished)",
        _ => "restore complete",
    };
    format!(
        "{verb}: target={} uuid={} term={} base={} checkpoint={} recovered={} shards={} bytes={} recovered_time={} action={:?}",
        report.directory.display(),
        uuid_hex(&report.database_uuid),
        report.term,
        report.base_generation,
        report.checkpoint_version,
        report.recovered_version,
        report.shards,
        report.bytes,
        time,
        report.action,
    )
}

#[cfg(test)]
mod tests {
    fn cursors(pairs: &[(u32, u64)]) -> Vec<pb::WalCursor> {
        pairs
            .iter()
            .map(|(shard, archived_lsn)| pb::WalCursor {
                shard: *shard,
                archived_lsn: *archived_lsn,
                history_fingerprint: vec![0; 32],
            })
            .collect()
    }

    /// The window must promise only what `stage_wal` will actually stage.
    ///
    /// Measured 2026-09-10 on a failing `pitr_retention.py`: the archive held
    /// WAL objects chaining `864336 -> 864408 -> 864456`, the base cursor was
    /// `864336`, and `state.wal_cursors` was **also** `864336` because the
    /// sidecar was stopped between publishing the object and advancing the
    /// cursor. The window advertised `end_version: 4`; the restore walked to the
    /// tip, found it equal to the base, staged nothing, and reported
    /// `recovered: 3, recovered_time: None` with the key empty.
    #[test]
    fn a_window_does_not_count_wal_beyond_the_durable_tip() {
        let tip = cursors(&[(0, 864_336), (1, 500)]);

        assert!(
            beyond_durable_tip(0, 864_408, Some(&tip)),
            "an object past the tip is not restorable and must not be counted"
        );
        assert!(
            beyond_durable_tip(0, 864_456, Some(&tip)),
            "nor is one further past it"
        );
        assert!(
            !beyond_durable_tip(0, 864_336, Some(&tip)),
            "the tip itself is reachable"
        );
        assert!(
            !beyond_durable_tip(0, 864_000, Some(&tip)),
            "and so is everything below it"
        );

        // Per shard, not across shards: shard 1's tip must not bound shard 0.
        assert!(
            !beyond_durable_tip(1, 400, Some(&tip)),
            "shard 1 is judged by its own cursor"
        );
        assert!(
            beyond_durable_tip(1, 600, Some(&tip)),
            "and is bounded by it"
        );

        // A superseded base carries no tip; `stage_wal` walks its chain to the
        // end, so nothing is out of bounds.
        assert!(!beyond_durable_tip(0, u64::MAX, None));

        // A shard the tip does not mention has no bound to compare against.
        assert!(!beyond_durable_tip(7, u64::MAX, Some(&tip)));
    }

    use super::*;
    use std::collections::BTreeMap;

    /// A plan with the given stamped versions above `checkpoint`.
    fn plan_with(checkpoint: u64, stamps: &[(u64, u64)]) -> yesno_core::wal::RecoveryPlan {
        let mut times = BTreeMap::new();
        let mut global = checkpoint;
        for (v, t) in stamps {
            times.insert(*v, *t);
            global = global.max(*v);
        }
        yesno_core::wal::RecoveryPlan {
            global_cv: global,
            truncate_at: BTreeMap::new(),
            replay: Vec::new(),
            discarded: Vec::new(),
            times,
        }
    }

    #[test]
    fn an_inclusive_target_keeps_a_commit_stamped_exactly_at_it() {
        let plan = plan_with(0, &[(1, 100), (2, 200), (3, 300)]);
        assert_eq!(resolve_time(&plan, 0, 200, true).unwrap(), 2);
    }

    #[test]
    fn an_exclusive_target_drops_a_commit_stamped_exactly_at_it() {
        let plan = plan_with(0, &[(1, 100), (2, 200), (3, 300)]);
        assert_eq!(resolve_time(&plan, 0, 200, false).unwrap(), 1);
    }

    /// The *highest* qualifying version, not the first.
    ///
    /// Stopping at the first match would restore a prefix that stops short of
    /// the target while reporting success, which is the quiet wrong answer a
    /// recovery target must never give.
    #[test]
    fn a_target_selects_the_highest_qualifying_version() {
        let plan = plan_with(0, &[(1, 100), (2, 110), (3, 120), (4, 500)]);
        assert_eq!(resolve_time(&plan, 0, 300, true).unwrap(), 3);
    }

    /// A target above everything archived is the tip, not an error: the
    /// operator asked for "everything up to then", and everything is up to then.
    #[test]
    fn a_target_after_the_last_commit_selects_the_tip() {
        let plan = plan_with(0, &[(1, 100), (2, 200)]);
        assert_eq!(resolve_time(&plan, 0, 9_999, true).unwrap(), 2);
    }

    /// A target below every commit above the base falls back to the base
    /// checkpoint, which is by construction already at or below the target.
    #[test]
    fn a_target_before_every_logged_commit_selects_the_base_checkpoint() {
        let plan = plan_with(7, &[(8, 800), (9, 900)]);
        assert_eq!(resolve_time(&plan, 7, 10, true).unwrap(), 7);
    }

    /// The whole range is checked, not just up to the first match.
    ///
    /// An unstamped version *above* the chosen one still invalidates the answer:
    /// the times are what prove the choice is the highest qualifying version, so
    /// an unknown one could have qualified and been missed.
    #[test]
    fn an_unstamped_version_anywhere_in_the_range_is_refused() {
        let plan = plan_with(0, &[(1, 100), (3, 300)]);
        let err = resolve_time(&plan, 0, 150, true).unwrap_err();
        let text = format!("{err}");
        assert!(text.contains("commit version 2"), "{text}");
        assert!(text.contains("--target-version"), "{text}");
    }

    /// The operator-facing strings must read as sentences, not as source layout.
    ///
    /// Rustfmt collapses a `\`-continued literal onto one line and keeps the
    /// indentation that followed it, so a message that looks fine in the source
    /// can reach a terminal with a run of spaces in the middle. Nothing else in
    /// the suite reads these, and an operator reads every one of them.
    #[test]
    fn operator_facing_messages_carry_no_stray_whitespace() {
        let mut messages = vec![
            format!(
                "{}",
                parse_rfc3339_micros("2026-09-06T14:02:00").unwrap_err()
            ),
            format!(
                "{}",
                resolve_time(&plan_with(0, &[(1, 100), (3, 300)]), 0, 150, true).unwrap_err()
            ),
            window_line(&RecoveryWindow {
                manifest_key: "k".into(),
                term: 3,
                base_generation: 7,
                checkpoint_version: 10,
                end_version: 42,
                checkpoint_time: Some(1_000_000),
                end_time: None,
                fully_stamped: false,
                active: true,
            }),
        ];
        messages.push(completion_line(&RestoreReport {
            directory: PathBuf::from("/var/lib/yesno-restored"),
            database_uuid: [0u8; 16],
            term: 3,
            base_generation: 7,
            checkpoint_version: 10,
            recovered_version: 42,
            recovered_time: Some(1_000_000),
            target: RecoveryTarget::Time {
                micros: 1_000_000,
                inclusive: true,
            },
            shards: 2,
            bytes: 4096,
            action: TargetAction::Publish,
            paused_at: None,
        }));
        for m in &messages {
            assert!(!m.contains("  "), "double space in operator message: {m:?}");
            assert!(!m.contains('\n'), "newline in operator message: {m:?}");
        }
        // And the window line must actually say the useful things.
        assert!(messages[2].contains("wall_clock=no"), "{}", messages[2]);
        assert!(messages[2].contains("unstamped"), "{}", messages[2]);
        assert!(
            messages[3].contains("1970-01-01T00:00:01"),
            "{}",
            messages[3]
        );
    }

    #[test]
    fn a_target_before_the_epoch_is_refused_rather_than_wrapped() {
        let plan = plan_with(0, &[(1, 100)]);
        assert!(resolve_time(&plan, 0, -1, true).is_err());
    }

    #[test]
    fn rfc3339_requires_an_offset() {
        assert_eq!(
            parse_rfc3339_micros("1970-01-01T00:00:01Z").unwrap(),
            1_000_000
        );
        // Same instant, written with an offset instead of Z.
        assert_eq!(
            parse_rfc3339_micros("1970-01-01T09:00:01+09:00").unwrap(),
            1_000_000
        );
        let err = format!(
            "{}",
            parse_rfc3339_micros("2026-09-06T14:02:00").unwrap_err()
        );
        assert!(err.contains("must carry an offset"), "{err}");
    }

    /// A base that publishes no commit time cannot answer a wall clock, and is
    /// skipped rather than treated as sitting at the epoch.
    #[test]
    fn base_selection_skips_a_base_with_no_commit_time_for_a_wall_clock() {
        let unstamped = pb::BaseManifest {
            checkpoint_version: 5,
            checkpoint_time: 0,
            ..Default::default()
        };
        let stamped = pb::BaseManifest {
            checkpoint_version: 5,
            checkpoint_time: 1_000,
            ..Default::default()
        };
        let time = RecoveryTarget::Time {
            micros: 2_000,
            inclusive: true,
        };
        assert!(!base_is_at_or_below(&unstamped, time));
        assert!(base_is_at_or_below(&stamped, time));
        // A version target does not care about times at all.
        assert!(base_is_at_or_below(&unstamped, RecoveryTarget::Version(5)));
        assert!(!base_is_at_or_below(&unstamped, RecoveryTarget::Version(4)));
        assert!(base_is_at_or_below(&unstamped, RecoveryTarget::Tip));
    }
}
