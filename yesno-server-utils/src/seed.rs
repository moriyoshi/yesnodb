//! Seed an empty follower from a durable archive before live replication starts.
//!
//! The daemon's data directory may itself be a PVC mount point, so it cannot
//! be replaced with one rename. Restore into a child directory on that same
//! volume, then move verified files into the mount root with MANIFEST last.
//! A durable file list lets a restarted seed finish a partly published move.
//! No database process is running while this command executes.

use std::path::{Component, Path, PathBuf};

use clap::Args;
use yesno_server::replication::pb::{replication_client::ReplicationClient, StatusRequest};
use yesno_server::replication::FollowerClient;

use crate::archive::{validate_state, ArchiveError, ArchiveStore};
use crate::restore::{restore_to, RecoveryTarget, TargetAction};
use crate::transport::{self, ClientTls};

const STAGE: &str = ".yesno-archive-seed";
const LOCK: &str = ".yesno-archive-seed.lock";
const OWNER: &str = "OWNER";
const FILES: &str = "FILES";
const OWNER_BYTES: &[u8] = b"yesno-follower-archive-seed-v1\n";

#[derive(Args, Debug)]
pub struct SeedOptions {
    /// Object archive containing a base image and its WAL history.
    #[arg(long)]
    pub store: String,
    /// Empty follower data directory. An existing MANIFEST is left alone.
    #[arg(long)]
    pub target: PathBuf,
    /// Configured control journal, if it resides within the data directory.
    #[arg(long)]
    pub journal_dir: Option<PathBuf>,
    /// Current leader's shared replication endpoint.
    #[arg(long)]
    pub leader: String,
    #[arg(long)]
    pub ca: Option<PathBuf>,
    #[arg(long, requires = "key")]
    pub cert: Option<PathBuf>,
    #[arg(long, requires = "cert")]
    pub key: Option<PathBuf>,
    #[arg(long)]
    pub server_name: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SeedOutcome {
    Existing,
    NoArchive,
    ArchivedTermBehind,
    Seeded,
}

fn sync_dir(path: &Path) -> Result<(), ArchiveError> {
    std::fs::File::open(path)?.sync_all()?;
    Ok(())
}

fn stage_is_owned(stage: &Path) -> Result<bool, ArchiveError> {
    if !stage.exists() {
        return Ok(false);
    }
    if !stage.join(OWNER).exists() && std::fs::read_dir(stage)?.next().is_none() {
        // Interrupted between creating the reserved directory and writing its
        // marker. Nothing was downloaded or moved at that point.
        std::fs::remove_dir(stage)?;
        return Ok(false);
    }
    if std::fs::read(stage.join(OWNER))? != OWNER_BYTES {
        return Err(format!("unrecognized follower seed directory '{}'", stage.display()).into());
    }
    Ok(true)
}

fn empty_target(target: &Path, journal_dir: Option<&Path>) -> Result<(), ArchiveError> {
    let journal_entry = journal_dir
        .and_then(|path| path.strip_prefix(target).ok())
        .and_then(|path| path.components().next())
        .and_then(|component| match component {
            Component::Normal(name) => Some(name),
            _ => None,
        });
    for entry in std::fs::read_dir(target)? {
        let entry = entry?;
        let name = entry.file_name();
        let is_journal = journal_entry == Some(name.as_os_str()) && entry.file_type()?.is_dir();
        if name != STAGE && name != LOCK && name != "lost+found" && !is_journal {
            return Err(format!(
                "refusing to seed nonempty follower directory '{}': found '{}'",
                target.display(),
                entry.path().display()
            )
            .into());
        }
    }
    Ok(())
}

fn file_names(ready: &Path) -> Result<Vec<String>, ArchiveError> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(ready)? {
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| "restored database has a non-UTF-8 file name")?;
        if name.is_empty() || name.contains(['/', '\n', '\r']) || name == "." || name == ".." {
            return Err(format!("invalid restored database file name '{name}'").into());
        }
        if entry.file_type()?.is_symlink() {
            return Err(format!("restored database contains symlink '{name}'").into());
        }
        names.push(name);
    }
    if !names.iter().any(|name| name == "MANIFEST") {
        return Err("restored database has no MANIFEST".into());
    }
    names.sort();
    // MANIFEST is the publication marker. Every other rename and directory
    // sync completes before it appears at the final path.
    names.retain(|name| name != "MANIFEST");
    names.push("MANIFEST".into());
    Ok(names)
}

fn write_names(stage: &Path, names: &[String]) -> Result<(), ArchiveError> {
    let mut bytes = names.join("\n").into_bytes();
    bytes.push(b'\n');
    let partial = stage.join("FILES.partial");
    std::fs::write(&partial, bytes)?;
    std::fs::File::open(&partial)?.sync_all()?;
    std::fs::rename(partial, stage.join(FILES))?;
    sync_dir(stage)
}

fn read_names(stage: &Path) -> Result<Vec<String>, ArchiveError> {
    let text = std::fs::read_to_string(stage.join(FILES))?;
    let names: Vec<_> = text.lines().map(str::to_owned).collect();
    if names.last().map(String::as_str) != Some("MANIFEST")
        || names.iter().any(|name| {
            name.is_empty() || name == "." || name == ".." || name.contains(['/', '\r'])
        })
    {
        return Err("invalid follower seed file list".into());
    }
    Ok(names)
}

fn publish(target: &Path, stage: &Path) -> Result<(), ArchiveError> {
    let ready = stage.join("ready");
    let names = read_names(stage)?;
    for (index, name) in names.iter().enumerate() {
        let src = ready.join(name);
        let dst = target.join(name);
        if src.exists() {
            if dst.exists() {
                return Err(format!(
                    "follower seed destination '{}' already exists",
                    dst.display()
                )
                .into());
            }
            if name == "MANIFEST" {
                // All database files must be durable in the final directory
                // before the manifest makes the directory recognizable.
                sync_dir(&ready)?;
                sync_dir(target)?;
            }
            std::fs::rename(&src, &dst)?;
        } else if !dst.exists() {
            return Err(format!("follower seed lost '{}' during publication", name).into());
        }
        if index + 1 == names.len() {
            sync_dir(target)?;
        }
    }
    if let Err(error) = std::fs::remove_dir_all(stage) {
        // MANIFEST is already durable; a cleanup failure must not turn a
        // committed seed into a startup failure. The next start retries it.
        eprintln!("yesnoctl: follower seed cleanup: {error}");
    } else {
        sync_dir(target)?;
    }
    Ok(())
}

async fn leader_status(
    options: &SeedOptions,
) -> Result<yesno_server::replication::pb::StatusResponse, ArchiveError> {
    let channel = transport::connect(
        &options.leader,
        ClientTls {
            ca: options.ca.as_deref(),
            cert: options.cert.as_deref(),
            key: options.key.as_deref(),
            server_name: options.server_name.as_deref(),
        },
    )
    .await?;
    let mut client = ReplicationClient::new(channel);
    Ok(client.status(StatusRequest {}).await?.into_inner())
}

/// Seed an empty follower from the archive's durable tip. The ordinary daemon
/// follower loop then fetches any later frames from the live leader.
pub async fn seed_follower(options: &SeedOptions) -> Result<SeedOutcome, ArchiveError> {
    std::fs::create_dir_all(&options.target)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(options.target.join(LOCK))?;
    lock.try_lock().map_err(|error| {
        format!(
            "another follower seed owns '{}': {error}",
            options.target.display()
        )
    })?;
    let stage = options.target.join(STAGE);
    if options.target.join("MANIFEST").exists() {
        if stage_is_owned(&stage)? {
            std::fs::remove_dir_all(&stage)?;
        }
        return Ok(SeedOutcome::Existing);
    }
    if stage_is_owned(&stage)? {
        if stage.join(FILES).exists() {
            let status = leader_status(options).await?;
            let ready = stage.join("ready");
            let uuid = yesno_core::database_uuid(&ready)?;
            let term = yesno_core::database_term(&ready)?;
            if status.db_uuid != uuid || status.term != term {
                return Err(
                    "leader identity or term changed during follower seed publication".into(),
                );
            }
            publish(&options.target, &stage)?;
            return Ok(SeedOutcome::Seeded);
        }
        // No files have moved without FILES. An interrupted download can be
        // discarded and restarted from the archive's current durable tip.
        std::fs::remove_dir_all(&stage)?;
    }
    empty_target(&options.target, options.journal_dir.as_deref())?;

    let store = ArchiveStore::connect(&options.store)?;
    let Some(state) = store.load_state().await? else {
        return Ok(SeedOutcome::NoArchive);
    };
    validate_state(&state)?;
    if state.latest_base_manifest.is_empty() {
        return Ok(SeedOutcome::NoArchive);
    }
    let status = leader_status(options).await?;
    if state.database_uuid != status.db_uuid {
        return Err("archive and leader have different database identities".into());
    }
    if state.term < status.term {
        return Ok(SeedOutcome::ArchivedTermBehind);
    }
    if state.term > status.term {
        return Err("archive is ahead of the configured leader's leadership term".into());
    }

    std::fs::create_dir(&stage)?;
    std::fs::write(stage.join(OWNER), OWNER_BYTES)?;
    std::fs::File::open(stage.join(OWNER))?.sync_all()?;
    sync_dir(&stage)?;
    sync_dir(&options.target)?;
    let ready = stage.join("ready");
    let report = restore_to(store, &ready, RecoveryTarget::Tip, TargetAction::Publish).await?;
    let status = leader_status(options).await?;
    if status.db_uuid != report.database_uuid || report.term != status.term {
        return Err("leader identity or term changed while restoring the follower archive".into());
    }
    if report.shards != status.shard_count || status.end_lsn.len() != report.shards as usize {
        return Err("archive and leader have different shard topology".into());
    }
    let cursors = FollowerClient::resume_from_disk(&ready, report.shards)?;
    for shard in 0..report.shards {
        let cursor = cursors
            .cursor(shard)
            .ok_or("restored follower is missing a shard")?;
        if cursor.next_lsn > status.end_lsn[shard as usize] {
            return Err(format!(
                "archive shard {shard} is ahead of the leader: {} > {}",
                cursor.next_lsn, status.end_lsn[shard as usize]
            )
            .into());
        }
    }
    let names = file_names(&ready)?;
    write_names(&stage, &names)?;
    publish(&options.target, &stage)?;
    Ok(SeedOutcome::Seeded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publication_resumes_after_an_interrupted_file_move() {
        let volume = tempfile::tempdir().unwrap();
        let target = volume.path();
        let stage = target.join(STAGE);
        let ready = stage.join("ready");
        std::fs::create_dir_all(&ready).unwrap();
        std::fs::write(stage.join(OWNER), OWNER_BYTES).unwrap();
        std::fs::write(ready.join("MANIFEST"), b"identity").unwrap();
        std::fs::write(ready.join("shard-0000.yno"), b"image").unwrap();
        std::fs::write(ready.join("shard-0000.wal"), b"log").unwrap();
        let names = file_names(&ready).unwrap();
        write_names(&stage, &names).unwrap();

        // A process dies after moving one file. The durable file list still
        // names it, so a retry can finish without trusting directory contents
        // as a substitute for the expected database file set.
        std::fs::rename(ready.join("shard-0000.yno"), target.join("shard-0000.yno")).unwrap();
        assert!(!target.join("MANIFEST").exists());
        publish(target, &stage).unwrap();
        assert_eq!(std::fs::read(target.join("MANIFEST")).unwrap(), b"identity");
        assert_eq!(
            std::fs::read(target.join("shard-0000.yno")).unwrap(),
            b"image"
        );
        assert_eq!(
            std::fs::read(target.join("shard-0000.wal")).unwrap(),
            b"log"
        );
        assert!(!stage.exists());
    }

    #[test]
    fn publication_never_overwrites_an_existing_file() {
        let volume = tempfile::tempdir().unwrap();
        let target = volume.path();
        let stage = target.join(STAGE);
        let ready = stage.join("ready");
        std::fs::create_dir_all(&ready).unwrap();
        std::fs::write(ready.join("MANIFEST"), b"new").unwrap();
        std::fs::write(target.join("MANIFEST"), b"old").unwrap();
        write_names(&stage, &["MANIFEST".into()]).unwrap();
        assert!(publish(target, &stage).is_err());
        assert_eq!(std::fs::read(target.join("MANIFEST")).unwrap(), b"old");
    }

    #[tokio::test]
    async fn archive_without_a_published_base_uses_live_bootstrap() {
        let root = tempfile::tempdir().unwrap();
        let options = SeedOptions {
            store: format!("file://{}", root.path().join("objects").display()),
            target: root.path().join("follower"),
            journal_dir: Some(root.path().join("follower/control")),
            leader: "http://127.0.0.1:1".into(),
            ca: None,
            cert: None,
            key: None,
            server_name: None,
        };
        std::fs::create_dir_all(options.journal_dir.as_ref().unwrap()).unwrap();
        assert_eq!(
            seed_follower(&options).await.unwrap(),
            SeedOutcome::NoArchive
        );
        assert!(!options.target.join("MANIFEST").exists());
    }
}
