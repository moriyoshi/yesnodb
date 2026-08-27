//! The database: sharded writers under a single global MVCC watermark.
//!
//! # Sharding
//!
//! Keys hash to one of 256 **virtual** shards, which map to physical shards.
//! The indirection means the physical shard count can change later without
//! rehashing chunk identity. All chunks of one key live in one shard ( I7 ), so
//! a single-key read never crosses a shard boundary.
//!
//! # The commit protocol
//!
//! 1. Take the participating shard locks **in ascending shard order**. Uniform
//!    ordering is what makes deadlock impossible with concurrent multi-shard
//!    batches.
//! 2. Assign the commit version **while holding them**. Late assignment gives
//!    I5 — each shard's log is version-monotonic, which is what makes recovery's
//!    discarded set a clean suffix rather than a scatter of holes.
//! 3. Publish the new versions, release the locks, then await durability.
//!
//! Publishing before durability is safe because readers snapshot the *visible*
//! watermark, never `next`: a pending version is invisible rather than
//! half-visible. That is precisely what lets the locks be released before the
//! fsync, which is what makes group commit worth having.
//!
//! # Observability
//!
//! The optional `tracing` feature emits spans at storage operation boundaries:
//! database open and recovery, commit, replica apply, checkpoint, and fsck.
//! These are deliberately outside container and chunk inner loops. A subscriber
//! therefore sees the duration and outcome of work that can block on I/O without
//! turning one operation into an event per ordinal. The typed [`crate::events`]
//! sink remains the transport-independent lifecycle contract.
//!
//! # Backup barrier
//!
//! [`Db::begin_backup`] excludes checkpoints only while a storage owner creates
//! an atomic filesystem snapshot. WAL commits continue because recovery already
//! defines how to consume their records from that snapshot. A commit that hits
//! mandatory checkpoint backpressure can still wait before returning. Holding
//! the lease through object upload would unnecessarily delay reclamation, so
//! the server releases it as soon as ZFS or Btrfs has created the immutable
//! view.

pub mod apply;
pub mod keystream;
pub mod manifest;
pub mod memtable;
pub(crate) mod readers;
pub mod store;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

pub use memtable::Memtable;

pub use store::ShardStore;

use crate::checkpoint::{self, DirtyChunk};
use crate::error::{CodecError, Result};
use crate::events::{self, CoreEvent, CoreEventSink, DatabaseMode, EventError};
use crate::index::tree::Tree;
use crate::mvcc::{Version, VersionOracle};
use crate::store::extent::ChunkKey;
use crate::wal::{record, GroupCommit, RecType, WalWriter};
use crate::{join, Container, OrdSet, Prefix48};

/// Virtual shards. Fixed, so key placement never changes.
pub const VSHARDS: u32 = 256;

/// Slabs evacuated per checkpoint, per shard. **Zero: off by default.**
///
/// Evacuation rewrites live chunks out of a sparse slab so it can be recycled
/// whole. It is implemented, tested, and — on every workload measured so far —
/// **does not pay for itself.** `e2e/scenarios/aged_state.py` A/Bs it at
/// 0 / 2 / 8 ( it was `examples/aged_state.rs` until 2026-08-26 ):
///
/// ```text
///  churn  evac   aged   live     amp  B/ordinal     moved     corpus
///   1.0%     0      6      -   1.20x      31.46         0     200 keys
///   1.0%     2      7      -   1.40x      36.70        78     200 keys
///   1.0%     0     18     14   2.57x      12.58         0   1 500 keys
///   1.0%     2     18     13   2.57x      12.58       759   1 500 keys
///   5.0%     0     17     14   2.43x      11.88         0   1 500 keys
///   5.0%     2     17     14   2.43x      11.88         0   1 500 keys
/// ```
///
/// On the small corpus it is a net *loss* — one extra slab for 78 relocations.
/// On the large one it reclaims **one live slab out of fourteen** for 759 chunks
/// rewritten at 1% churn, and nothing at all at 0.1% or 5%. The file's
/// high-water mark does not move either way.
///
/// The reason is that keeping active slabs across generations already fills
/// slabs densely and lets them drain naturally, so relocating out of a sparse
/// slab mostly rewrites data that was fine where it was. Evacuation was designed
/// against an allocator that abandoned a slab per class per checkpoint, and that
/// behaviour is gone.
///
/// Kept behind the knob rather than deleted because every measurement so far
/// uses uniform key sizes and round-robin churn; a skewed corpus may yet show a
/// case for it. Do not raise this default without a measurement that shows a
/// benefit — the sweep above is what a *non*-benefit looks like.
pub const EVACUATE_PER_CHECKPOINT: usize = 0;

/// Which virtual shard a key belongs to.
#[inline]
pub fn vshard_of(key: u64) -> u32 {
    // splitmix64 finalizer: cheap and avalanches well, so adjacent keys spread.
    let mut z = key.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    ((z ^ (z >> 31)) % VSHARDS as u64) as u32
}

struct Shard {
    mem: RwLock<Memtable>,
    /// Serializes writers. Held across version assignment, released before the
    /// durability wait.
    write: Mutex<()>,
    /// The persistent tier. `None` for an in-memory database.
    store: Option<Mutex<ShardStore>>,
    /// Serializes **checkpoints** on this shard.
    ///
    /// `Mutex<ShardStore>` used to do this implicitly, by being held across a
    /// checkpoint's whole body. It no longer is: the durability sequence runs
    /// with the store lock released so readers are not blocked behind three
    /// `fsync`s. That re-opens a window in which a second checkpoint could
    /// interleave with the first, and this closes it.
    ///
    /// Separate from `write` on purpose. Taking the writer lock here would
    /// block every commit for the duration of a checkpoint, which is the cost
    /// this whole change exists to remove.
    ckpt: Mutex<()>,
    /// The write-ahead log. `None` for an in-memory database.
    ///
    /// Separate from `store` rather than a field of it, because a commit
    /// appends here under the shard's write lock and fsyncs *after* releasing
    /// it — while the store is only touched by the checkpointer. Putting the log
    /// inside `ShardStore` would make every commit contend on the checkpoint
    /// lock.
    wal: Option<GroupCommit>,
}

impl Shard {
    /// A shard backed by a read-only store, with no log and an empty memtable.
    ///
    /// The empty memtable is the definition of checkpoint-visible reading:
    /// every read falls through to the index root the superblock published, and
    /// anything the writer has committed but not yet checkpointed is invisible
    /// here. Do not "fix" that by replaying the log — see `open_inner`.
    fn with_store_read_only(store: ShardStore) -> Self {
        Shard {
            mem: RwLock::new(Memtable::new()),
            write: Mutex::new(()),
            ckpt: Mutex::new(()),
            store: Some(Mutex::new(store)),
            wal: None,
        }
    }

    fn new() -> Self {
        Shard {
            mem: RwLock::new(Memtable::new()),
            write: Mutex::new(()),
            ckpt: Mutex::new(()),
            store: None,
            wal: None,
        }
    }

    fn with_store(store: ShardStore, wal: WalWriter) -> Result<Self> {
        Ok(Shard {
            mem: RwLock::new(Memtable::new()),
            write: Mutex::new(()),
            ckpt: Mutex::new(()),
            store: Some(Mutex::new(store)),
            wal: Some(GroupCommit::new(wal)?),
        })
    }

    /// The committed on-disk container for a chunk, if any.
    ///
    /// This is what makes a write a genuine read-modify-write. Without it, an
    /// insert into a chunk that lives only on disk starts from an empty
    /// container and the resulting memtable entry *shadows* the persisted data —
    /// silently losing every ordinal already stored there.
    fn disk_chunk(&self, key: u64, prefix: Prefix48) -> Option<Container> {
        let store = self.store.as_ref()?.lock().unwrap();
        let tree = store.tree()?;
        let ck = ChunkKey::new(key, prefix);
        let cref = tree.get(&*store, ck).ok()??;
        // Checked: a stale or corrupt reference must not decode as this key.
        store.read_container_for(ck, cref).ok()?
    }
}

/// What opening learned while replaying every shard's log.
struct RecoveryReport {
    recovered_version: Version,
    records_replayed: u64,
    discarded_versions: Vec<Version>,
    /// Highest commit time replayed, or 0 when the log carries no stamps.
    /// Feeds `VersionOracle::resume_at` so the clock stays monotone across a
    /// restart and across a promotion.
    max_time: crate::mvcc::Micros,
}

/// Replay every shard's log, returning what recovery observed.
///
/// The plan decides what is replayable: only versions in the consecutive
/// resolved run above the checkpoint, and never a commit marker. Anything above
/// `global_cv` is **truncated**, not merely skipped — by I5 the discarded
/// records form a clean suffix, and leaving them on disk would let a follower
/// see versions the leader has discarded.
fn replay_logs(
    shards: &[Shard],
    checkpoint_cv: Version,
    sink: &Arc<dyn CoreEventSink>,
    operation_id: events::OperationId,
) -> Result<RecoveryReport> {
    // The base travels with the bytes. A record's header carries the LSN it
    // was written at and `Record::decode` refuses one that disagrees with where
    // it was found — so a scanner started at 0 over a log whose base is not 0
    // rejects every record in it and recovery silently replays nothing. That is
    // not hypothetical: a checkpoint advances the base on every shard it cuts.
    let mut buffers: Vec<(u32, Vec<u8>, u64)> = Vec::new();
    for (i, sh) in shards.iter().enumerate() {
        if let Some(w) = sh.wal.as_ref() {
            let log = w.log();
            buffers.push((i as u32, log.read_all()?, log.base()));
        }
    }
    if buffers.iter().all(|(_, b, _)| b.is_empty()) {
        return Ok(RecoveryReport {
            recovered_version: checkpoint_cv,
            records_replayed: 0,
            discarded_versions: Vec::new(),
            max_time: 0,
        });
    }

    let logs: Vec<crate::wal::ShardLog<'_>> = buffers
        .iter()
        .map(|(shard, bytes, base_lsn)| crate::wal::ShardLog {
            shard: *shard,
            bytes,
            base_lsn: *base_lsn,
        })
        .collect();
    let plan = crate::wal::plan(&logs, checkpoint_cv)?;

    for (shard, rec) in &plan.replay {
        let sh = &shards[*shard as usize];
        let mut mem = sh.mem.write().unwrap();
        // Extracted, not inlined: a live replica applies the same records
        // through the same function. See `db::apply`.
        apply::apply_record(sh, &mut mem, rec)?;
    }

    for (shard, at) in &plan.truncate_at {
        if let Some(w) = shards[*shard as usize].wal.as_ref() {
            let old_end_lsn = w.log().end_lsn();
            if *at < old_end_lsn {
                events::emit(
                    sink,
                    CoreEvent::WalTailTruncated {
                        operation_id,
                        shard: *shard,
                        old_end_lsn,
                        new_end_lsn: *at,
                    },
                );
            }
            // Through the group, not the log directly: the durable mark has
            // to come down with the log or the first commits after this open
            // skip their fsync. See `GroupCommit::truncate`.
            w.truncate(*at)?;
        }
    }

    let max_time = plan.max_time();
    Ok(RecoveryReport {
        recovered_version: plan.global_cv.max(checkpoint_cv),
        records_replayed: plan.replay.len() as u64,
        discarded_versions: plan.discarded,
        max_time,
    })
}

/// How a directory is being opened.
///
/// Three modes, not a pair of booleans, because two of the three differences
/// are about **what must not happen**: a replica must not append an `EpochFence`
/// into the leader's LSN space, and a reader must not take the lock, open a log,
/// or replay one. A boolean pair admits a fourth combination nobody defined.
#[derive(Clone, Copy, PartialEq, Eq)]
enum OpenMode {
    Writer,
    Replica,
    Reader,
}

impl OpenMode {
    #[cfg(feature = "tracing")]
    fn as_str(self) -> &'static str {
        match self {
            OpenMode::Writer => "writer",
            OpenMode::Replica => "replica",
            OpenMode::Reader => "reader",
        }
    }

    fn event_mode(self) -> DatabaseMode {
        match self {
            OpenMode::Writer => DatabaseMode::Writer,
            OpenMode::Replica => DatabaseMode::Replica,
            OpenMode::Reader => DatabaseMode::Reader,
        }
    }
}

/// Emits the close fact after every storage-owning field in `DbInner` has
/// dropped. Struct fields drop in declaration order, so this guard stays last.
struct LifecycleGuard {
    sink: Arc<dyn CoreEventSink>,
    operation_id: Mutex<Option<events::OperationId>>,
    epoch: u64,
    term: u32,
}

impl Drop for LifecycleGuard {
    fn drop(&mut self) {
        let operation_id = *self.operation_id.lock().unwrap();
        events::emit(
            &self.sink,
            CoreEvent::DatabaseShutdownCompleted {
                operation_id,
                graceful: operation_id.is_some(),
                epoch: self.epoch,
                term: self.term,
            },
        );
    }
}

struct DbInner {
    /// Held for the lifetime of the `Db`; dropping it releases the file lock.
    ///
    /// The handle itself is the exclusion — `flock` is released when the last
    /// descriptor closes, and every `Db` clone shares this one `Arc<DbInner>`.
    /// It is never read from or written to after open.
    ///
    /// `None` for an in-memory database, which has no directory to guard and no
    /// second process that could reach it.
    _lock: Option<std::fs::File>,
    /// This open's fencing epoch, monotonically increasing per acquisition.
    /// Zero for an in-memory database, which has no durable state to fence.
    epoch: u64,
    shards: Vec<Shard>,
    /// LSNs whose containing WAL generation a checkpoint must retain, published
    /// by whatever is serving followers. Empty when nothing is.
    retention: crate::repl::RetentionFloor,
    /// `vshard -> shard`, read from the MANIFEST at open.
    ///
    /// Routing goes through this rather than `vshard_of(key) % shards.len()`,
    /// which is what made the shard count part of the routing function and let
    /// a reopen with a different count silently misroute every key.
    route: manifest::Manifest,
    policy: crate::checkpoint::CheckpointPolicy,
    evacuate_per_checkpoint: usize,
    oracle: VersionOracle,
    /// Live snapshot versions. Index into `readers`; `u64::MAX` means free.
    readers: Vec<AtomicU64>,
    /// For each live snapshot, the checkpoint watermark its pinned index roots
    /// came from. Parallel to `readers`.
    ///
    /// This is **not** the same as the snapshot's version, and conflating them
    /// is a data-loss bug. A snapshot pins the root of the *last checkpoint*
    /// while its version is the current `visible`, so anything committed in
    /// between exists only in the memtable. Evicting one of those on the
    /// strength of `safe_version` takes it out from under a reader whose root
    /// predates it — the key vanishes for that reader alone.
    reader_roots: Vec<AtomicU64>,
    /// Slots invalidated by [`SpaceAmpPolicy::AbortOldestReader`].
    ///
    /// **Marked, not freed.** Freeing would let a new `snapshot()` claim the
    /// slot while the old `ReaderSlot` is still alive, and its `Drop` would then
    /// clear a stranger's registration. The flag is cleared when the slot is
    /// genuinely re-claimed.
    reader_evicted: Vec<std::sync::atomic::AtomicBool>,
    /// Checkpoint sequence of the oldest root each reader captured.
    ///
    /// **Parallel to `reader_roots` rather than folded into it**, because
    /// that one holds a *version* — `evict_floor` needs a version for memtable
    /// eviction. This holds a checkpoint sequence, which is the only thing that
    /// answers "which roots can this reader reach through".
    ///
    /// `0` means "assume the oldest possible root", which is the conservative
    /// reading and is what an unpublished or just-claimed slot reads as.
    reader_ckpt_seqs: Vec<AtomicU64>,
    /// Wall-clock microseconds at which each slot was claimed, for age
    /// reporting. `0` in a free slot.
    ///
    /// Wall clock, not `Instant`: this is compared against nothing inside
    /// the engine and exists to be *reported* to an operator, who is reading a
    /// dashboard in wall-clock time. A clock step therefore misreports an age
    /// and cannot corrupt anything — which is the right trade here and would
    /// not be for a deadline that expires something.
    reader_started_micros: Vec<AtomicU64>,
    /// The oldest version [`Db::snapshot_at`] may still be granted.
    ///
    /// A checkpoint collapses each chunk's version chain to the newest entry at
    /// or below its floor, so *below* that floor the intermediate states no
    /// longer exist anywhere — not in the memtable, which dropped them, and not
    /// in the store, whose root holds only the checkpoint watermark's state.
    /// This records how far that has gone, monotonically.
    ///
    /// It is the **refusal** that makes the field worth its eight bytes.
    /// Without it a read below the floor still returns an answer, just the wrong
    /// one — the floor's state wearing the requested version's name. That is the
    /// failure this crate keeps finding and keeps writing down: a check that
    /// reports on something other than what it claims.
    ///
    /// Ordering with the reader registry is what makes this sound, and it is
    /// only sound in one direction. Claiming a slot at `v` drags `safe_version`
    /// down to `v`, so a checkpoint that starts *afterwards* cannot prune past
    /// it. Reading this field after publishing the slot therefore catches the
    /// only remaining case, a checkpoint that had already finished. Reading it
    /// first and then claiming would leave exactly the window it exists to
    /// close.
    read_floor: AtomicU64,
    /// What to do when deferred bytes reach live bytes.
    on_space_amp: SpaceAmpPolicy,
    space_amp_soft_permille: u32,
    snapshot_soft_age_secs: u64,
    snapshot_age_soft_breached: std::sync::atomic::AtomicBool,
    /// Latches the soft threshold so the event is **edge**-triggered. The
    /// natural evaluation point is every checkpoint, and a level-triggered
    /// event would fire once per checkpoint for as long as one reporting query
    /// runs — which is exactly the situation an operator is being told about.
    space_amp_soft_breached: std::sync::atomic::AtomicBool,
    /// True when this handle was opened by [`Db::open_replica`].
    ///
    /// A **runtime mode**, not a `DbOptions` field, and the distinction is
    /// load-bearing: `DbOptions` is `Copy` and passed by value at every open
    /// site, so a `read_only: bool` in it is a value a caller defaults away with
    /// `..Default::default()`. This crate has already been bitten by exactly
    /// that shape — see the `shards` note in `open_with` — and `retention_floor`
    /// was kept out of `DbOptions` for the same reason.
    replica: bool,
    /// Opened by [`Db::open_reader`]: no lock, no log, no writes.
    read_only: bool,
    /// This process's slot in the shared registry, when it is a foreign reader.
    /// Dropped with the `Db`, which releases the slot.
    _registration: Option<readers::ReaderRegistration>,
    /// The database directory, for consulting the registry. `None` in memory.
    dir: Option<std::path::PathBuf>,
    /// The commit table and cursors a replica carries between batches. `None`
    /// on a leader, which never applies foreign records.
    apply: Option<Mutex<apply::ApplyState>>,
    /// Checkpoints are exclusive; filesystem snapshot creation is shared.
    backup_barrier: Arc<BackupBarrier>,
    /// See `Db::set_checkpoint_hook`.
    #[allow(clippy::type_complexity)]
    checkpoint_hook: std::sync::Mutex<Option<std::sync::Arc<dyn Fn() + Send + Sync>>>,
    /// Operational observer, kept out of `DbOptions` so that type stays `Copy`.
    events: Arc<dyn CoreEventSink>,
    /// Must remain the last field; its `Drop` publishes after storage teardown.
    lifecycle: LifecycleGuard,
}

const FREE: u64 = u64::MAX;

#[derive(Default)]
struct BackupBarrierState {
    backups: usize,
    checkpoint: bool,
    waiting_checkpoints: usize,
}

#[derive(Default)]
struct BackupBarrier {
    state: Mutex<BackupBarrierState>,
    changed: std::sync::Condvar,
}

impl BackupBarrier {
    fn begin_backup(self: &Arc<Self>) -> BackupLease {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        while state.checkpoint || state.waiting_checkpoints > 0 {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(|error| error.into_inner());
        }
        state.backups += 1;
        BackupLease {
            barrier: self.clone(),
        }
    }

    fn begin_checkpoint(self: &Arc<Self>) -> CheckpointLease {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.waiting_checkpoints += 1;
        while state.checkpoint || state.backups > 0 {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(|error| error.into_inner());
        }
        state.waiting_checkpoints -= 1;
        state.checkpoint = true;
        CheckpointLease {
            barrier: self.clone(),
        }
    }
}

/// A short-lived exclusion against checkpoints while an atomic filesystem
/// snapshot is created. Dropping it immediately re-enables checkpoints.
pub struct BackupLease {
    barrier: Arc<BackupBarrier>,
}

impl Drop for BackupLease {
    fn drop(&mut self) {
        let mut state = self
            .barrier
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.backups -= 1;
        self.barrier.changed.notify_all();
    }
}

struct CheckpointLease {
    barrier: Arc<BackupBarrier>,
}

impl Drop for CheckpointLease {
    fn drop(&mut self) {
        let mut state = self
            .barrier
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.checkpoint = false;
        self.barrier.changed.notify_all();
    }
}

/// Take the database's exclusive lock and return it with this open's epoch.
///
/// The epoch lives in the lock file itself: read, incremented, written back
/// while the lock is held, so two processes cannot observe the same value even
/// if they contend. It is deliberately **not** in the superblock — a superblock
/// write is a checkpoint-commit operation, and an epoch bump must happen at
/// open, before any of that machinery is trusted.
pub(crate) fn acquire_lock(dir: &std::path::Path) -> Result<(std::fs::File, u64)> {
    let path = dir.join("LOCK");
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|_| CodecError::Invariant("cannot open the database lock file"))?;

    // Non-blocking: an already-open database is an error to report, not a
    // queue to join. Blocking here would hang a caller with no way to know why.
    file.try_lock()
        .map_err(|_| CodecError::AlreadyOpen(dir.display().to_string()))?;

    use std::io::{Read, Seek, Write};
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)
        .map_err(|_| CodecError::Invariant("cannot read the database lock file"))?;
    let prev = if buf.len() >= 8 {
        u64::from_le_bytes(buf[..8].try_into().unwrap())
    } else {
        0
    };
    let epoch = prev + 1;
    file.seek(std::io::SeekFrom::Start(0))
        .and_then(|_| file.write_all(&epoch.to_le_bytes()))
        .and_then(|_| file.sync_data())
        .map_err(|_| CodecError::Invariant("cannot record the fencing epoch"))?;
    Ok((file, epoch))
}

/// Publish a manifest, so that a crash cannot leave a database whose identity
/// and routing are only half on disk.
///
/// **One slot at a time, and never the live one.** This used to
/// `File::create` — which truncates — and then write the same new image to both
/// slots. A crash after the truncate and before the first copy landed left *no*
/// readable manifest and a database that would not open, which made the two
/// slots decorative: they held two copies of one generation rather than two
/// generations. [`promote_database`](crate::promote_database) is the caller most
/// exposed to it, since an operator runs it under failover pressure.
///
/// The update therefore targets the slot [`manifest::pick`] is not returning
/// ( [`manifest::next_slot_offset`] ), leaving the live slot byte-for-byte
/// intact. A partially written slot fails its CRC, so its higher `seq` is never
/// observable, and `pick` returns the old manifest until the new slot is whole
/// and the new one afterwards.
///
/// The `sync_all` is ordering, not belt-and-braces: the *next* update will
/// overwrite the slot this one left alone, so these bytes must be durable before
/// that is allowed to start, or one crash could lose both generations. It is
/// `sync_all` rather than `sync_data` because writing slot B for the first time
/// extends the file, and a length that is not durable is a slot that is not
/// there.
///
/// Writing both slots survives only for the case that cannot lose anything: a
/// file with no readable slot at all, which is a database being created.
pub(crate) fn write_manifest(path: &std::path::Path, m: &manifest::Manifest) -> Result<()> {
    use std::io::{Seek, Write};

    // A missing file reads as "nothing live", which is exactly right: creation.
    // Not `unwrap_or_default()` on the *decode* — a slot that checksums
    // correctly but this build cannot use is an error, and clobbering bytes the
    // writer demonstrably meant is not a recovery.
    let existing = std::fs::read(path).unwrap_or_default();
    let live = manifest::pick(&existing)?;
    let target = manifest::next_slot_offset(&existing)?;

    // A slot becomes authoritative purely by carrying the higher `seq`, so an
    // update that forgot to raise it would land on disk and be ignored — the
    // database would keep serving the old map with no error anywhere. That was
    // harmless while every write filled both slots and is not any more, so it is
    // checked rather than commented.
    if let Some(live) = &live {
        if m.seq <= live.seq {
            return Err(CodecError::Invariant(
                "a manifest update must raise seq, or the new slot would not become live",
            ));
        }
    }

    let Some(off) = target else {
        let bytes = manifest::initial_file(m);
        let mut f = std::fs::File::create(path)
            .map_err(|_| CodecError::Invariant("cannot write database manifest"))?;
        f.write_all(&bytes)
            .and_then(|_| f.sync_all())
            .map_err(|_| CodecError::Invariant("cannot write database manifest"))?;
        return Ok(());
    };

    // No `.truncate(true)` and no `File::create`: the whole point is that the
    // other slot keeps its bytes.
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|_| CodecError::Invariant("cannot write database manifest"))?;
    f.seek(std::io::SeekFrom::Start(off))
        .and_then(|_| f.write_all(&m.encode()))
        .and_then(|_| f.sync_all())
        .map_err(|_| CodecError::Invariant("cannot write database manifest"))?;
    Ok(())
}

/// A process- and time-derived identity. Not RFC 4122; it only needs to make
/// mixing shard files from different databases detectable.
fn fresh_uuid() -> [u8; 16] {
    let mut out = [0u8; 16];
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    out[..8].copy_from_slice(&nanos.to_le_bytes());
    out[8..].copy_from_slice(&(std::process::id() as u64).to_le_bytes());
    out
}

/// Wall-clock microseconds since the epoch, or `0` if the clock is before it.
fn now_micros() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

/// What to do when retention reaches its bound.
///
/// Space amplification is bounded at 2x by construction: once deferred bytes
/// reach live bytes, something has to give. This says what.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum SpaceAmpPolicy {
    /// Invalidate the oldest snapshot and retry. The design's default, because
    /// a reporting query should not be able to halt ingestion.
    ///
    /// This is what makes `Snapshot`'s readers return `Result`: enforcing it
    /// requires reads through an invalidated snapshot to fail.
    #[default]
    AbortOldestReader,
    /// Block the writer until readers release. No API cost, but it inverts the
    /// design's stated priority — a single long reader can stop ingestion.
    StallWriters,
}

/// Options for opening a database.
#[derive(Clone, Copy, Debug)]
pub struct DbOptions {
    pub shards: usize,
    /// When to checkpoint, and when to stall a writer.
    ///
    /// The stall is the **only** bound on memtable growth. Without it a
    /// sustained ingest that never checkpoints grows the memtable until the
    /// process dies, which is what happened for every milestone before
    /// 2026-08-25: `CheckpointPolicy` existed, was documented as mandatory, and
    /// was referenced from nowhere.
    pub policy: crate::checkpoint::CheckpointPolicy,
    /// Slabs evacuated per checkpoint, per shard. `0` disables evacuation.
    ///
    /// A knob rather than a constant so the aged-state harness can A/B it
    /// without a recompile — which matters because the one sweep run so far
    /// showed it making no useful difference, and a policy that never pays for
    /// itself should be measured before it is kept.
    pub evacuate_per_checkpoint: usize,
    pub max_readers: usize,
    /// What to do when deferred bytes reach live bytes.
    ///
    /// Space amplification, in parts per thousand, at which an **observation**
    /// event is emitted. `1250` is a 25 % overhead; `0` disables it.
    ///
    /// Observation only. Crossing this evicts nothing and stalls nobody — the
    /// hard bound in [`Db::enforce_space_amp`] remains the sole intervention.
    /// It exists so an operator sees retention growing *before* a reporting
    /// query is aborted for it, which the hard bound alone cannot tell them.
    pub space_amp_soft_permille: u32,

    /// Seconds a snapshot may stay open before an **observation** event names
    /// it. `0` disables it.
    ///
    /// Observation only, and there is deliberately no `snapshot_hard_age`: a
    /// hard age that ended a query would be a second way to kill one, and
    /// [`SpaceAmpPolicy::AbortOldestReader`] remains the only intervention.
    pub snapshot_soft_age_secs: u64,

    /// Defaults to [`SpaceAmpPolicy::AbortOldestReader`], which is the design's
    /// stated priority: a reporting query should not be able to halt ingestion.
    /// The cost is that every `Snapshot` read is fallible.
    pub on_space_amp: SpaceAmpPolicy,
    pub commit_ring: usize,
    /// Recompute allocator occupancy from the committed index at open.
    ///
    /// On by default, because off means leaking every extent that was awaiting
    /// reclamation at shutdown — once per restart, forever.
    ///
    /// The cost is one index walk per shard at open, `O(chunks)` and
    /// sequential. Turn it off only if that startup cost is unacceptable and
    /// the space is not, and know that `Db::verify` will then report the
    /// orphans as leaked rather than them being fixed.
    pub rebuild_alloc_on_open: bool,
}

impl Default for DbOptions {
    fn default() -> Self {
        DbOptions {
            shards: 8,
            policy: crate::checkpoint::CheckpointPolicy::default(),
            evacuate_per_checkpoint: EVACUATE_PER_CHECKPOINT,
            max_readers: 4096,
            space_amp_soft_permille: 1_250,
            snapshot_soft_age_secs: 300,
            on_space_amp: SpaceAmpPolicy::default(),
            commit_ring: 4096,
            rebuild_alloc_on_open: true,
        }
    }
}

/// An in-memory database. Cheap to clone; all clones share one store.
#[derive(Clone)]
pub struct Db {
    /// Chunks relocated by evacuation, for the aged-state harness.
    evacuated: Arc<AtomicU64>,
    /// Unix seconds of the last checkpoint, for the interval trigger.
    last_checkpoint: Arc<AtomicU64>,
    inner: Arc<DbInner>,
}

impl Db {
    pub fn new() -> Self {
        Self::with_options(DbOptions::default())
    }

    /// Exclude checkpoints while the caller creates an atomic filesystem
    /// snapshot. WAL commits may proceed, but a call whose backpressure requires
    /// a checkpoint can wait before returning.
    ///
    /// Keep this lease only through snapshot creation. Reading or uploading the
    /// resulting immutable snapshot does not need it.
    pub fn begin_backup(&self) -> BackupLease {
        self.inner.backup_barrier.begin_backup()
    }

    pub fn with_options(opts: DbOptions) -> Self {
        assert!(opts.shards > 0, "a database needs at least one shard");
        Db {
            evacuated: Arc::new(AtomicU64::new(0)),
            last_checkpoint: Arc::new(AtomicU64::new(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
            )),
            inner: Arc::new(DbInner {
                _lock: None,
                epoch: 0,
                shards: (0..opts.shards).map(|_| Shard::new()).collect(),
                // No directory to persist to, but routing must be the same
                // function it is on disk — otherwise the in-memory and durable
                // paths could disagree about which shard owns a key.
                route: manifest::Manifest::create([0u8; 16], opts.shards as u32),
                retention: Default::default(),
                policy: opts.policy,
                evacuate_per_checkpoint: opts.evacuate_per_checkpoint,
                oracle: VersionOracle::new(opts.commit_ring.next_power_of_two()),
                readers: (0..opts.max_readers)
                    .map(|_| AtomicU64::new(FREE))
                    .collect(),
                reader_roots: (0..opts.max_readers).map(|_| AtomicU64::new(0)).collect(),
                reader_evicted: (0..opts.max_readers)
                    .map(|_| std::sync::atomic::AtomicBool::new(false))
                    .collect(),
                reader_ckpt_seqs: (0..opts.max_readers).map(|_| AtomicU64::new(0)).collect(),
                reader_started_micros: (0..opts.max_readers).map(|_| AtomicU64::new(0)).collect(),
                read_floor: AtomicU64::new(0),
                on_space_amp: opts.on_space_amp,
                space_amp_soft_permille: opts.space_amp_soft_permille,
                snapshot_soft_age_secs: opts.snapshot_soft_age_secs,
                snapshot_age_soft_breached: std::sync::atomic::AtomicBool::new(false),
                space_amp_soft_breached: std::sync::atomic::AtomicBool::new(false),
                replica: false,
                read_only: false,
                _registration: None,
                dir: None,
                apply: None,
                backup_barrier: Arc::new(BackupBarrier::default()),
                checkpoint_hook: std::sync::Mutex::new(None),
                events: events::noop_sink(),
                lifecycle: LifecycleGuard {
                    sink: events::noop_sink(),
                    operation_id: Mutex::new(None),
                    epoch: 0,
                    term: 0,
                },
            }),
        }
    }

    /// Open a durable database, creating it if absent.
    ///
    /// One file per physical shard. Each carries its own superblock, so shards
    /// recover independently and a torn flip in one cannot affect another.
    pub fn open(dir: impl AsRef<std::path::Path>) -> Result<Self> {
        Self::open_with(dir, DbOptions::default())
    }

    pub fn open_with(dir: impl AsRef<std::path::Path>, opts: DbOptions) -> Result<Self> {
        Self::open_with_events(dir, opts, events::noop_sink())
    }

    /// Open a writer and publish its operational events to `sink`.
    pub fn open_with_events(
        dir: impl AsRef<std::path::Path>,
        opts: DbOptions,
        sink: Arc<dyn CoreEventSink>,
    ) -> Result<Self> {
        Self::open_observed(dir.as_ref(), opts, OpenMode::Writer, sink)
    }

    /// Open `dir` as a **read-only replica** of some leader.
    ///
    /// Reads are exactly a leader's; every local mutation is refused with
    /// [`CodecError::ReadOnlyReplica`]. What the caller gains is
    /// [`Db::apply_wal_batch`]: shipped leader frames can be applied into a
    /// database that is *open and serving*, rather than only into a directory
    /// nothing holds.
    ///
    /// **The one thing this changes on disk is that it writes nothing.** A
    /// normal open appends an `EpochFence` to every shard's log, and on a
    /// replica that record would be a **locally generated frame in an LSN space
    /// that belongs to the leader**: the next shipped frame would no longer land
    /// where its own header says, `Record::decode` would refuse it, the scan
    /// would stop, and the log would silently end there — the exact shape of the
    /// 2026-08-28 data-loss bug, arriving from a new direction. Suppressing it
    /// is a correctness requirement, not tidiness.
    ///
    /// A replica may still **checkpoint**, and should: it runs its own
    /// allocator and its own reclamation, and generation rollover preserves the
    /// next LSN the leader will carry. That is why a replica's own maintenance
    /// does not disturb offset identity.
    pub fn open_replica(dir: impl AsRef<std::path::Path>, opts: DbOptions) -> Result<Self> {
        Self::open_replica_with_events(dir, opts, events::noop_sink())
    }

    /// Open a replica and publish its operational events to `sink`.
    pub fn open_replica_with_events(
        dir: impl AsRef<std::path::Path>,
        opts: DbOptions,
        sink: Arc<dyn CoreEventSink>,
    ) -> Result<Self> {
        Self::open_observed(dir.as_ref(), opts, OpenMode::Replica, sink)
    }

    /// Open `dir` **read-only, without taking the database's exclusive lock**.
    ///
    /// # Why this exists
    ///
    /// [`Db::open`] takes a non-blocking exclusive `flock`, so a second process
    /// cannot open the same directory at all. That is right for writers — two
    /// of them is unbounded corruption — but it also excludes every *reader*,
    /// and PostgreSQL forks one backend per connection. A foreign reader is the
    /// only way such a deployment can read a database a `yesnod` is serving.
    ///
    /// # What it sees: **checkpoint-visible** state
    ///
    /// This reads only what the last checkpoint published through the
    /// superblock. It replays no log and has no memtable, so it lags the writer
    /// by up to one checkpoint. That is a statable semantic rather than a bug —
    /// and it is the same primitive a standby serving reads would need.
    ///
    /// It is **not** a substitute for [`Db::open`] on a directory nobody else
    /// holds: every mutation returns [`CodecError::ReadOnlyReplica`].
    ///
    /// # The safety obligation this creates
    ///
    /// Extent reclamation has three conditions, and condition 1 — no live
    /// snapshot can reach the extent — is enforced from `DbInner::readers`,
    /// which is **process-local memory**. A foreign reader is invisible to it.
    /// Registering in the shared `READERS` file is what makes the writer account
    /// for it; see [`crate::db::readers`].
    pub fn open_reader(dir: impl AsRef<std::path::Path>) -> Result<Self> {
        Self::open_reader_with(dir, DbOptions::default())
    }

    /// [`Db::open_reader`] with explicit options.
    ///
    /// `opts.shards` is ignored: the shard count comes from the MANIFEST, as
    /// it must — it is part of the routing function, and a reader guessing it
    /// would look in the wrong shard for most keys.
    pub fn open_reader_with(dir: impl AsRef<std::path::Path>, opts: DbOptions) -> Result<Self> {
        Self::open_reader_with_events(dir, opts, events::noop_sink())
    }

    /// Open a foreign reader and publish its operational events to `sink`.
    pub fn open_reader_with_events(
        dir: impl AsRef<std::path::Path>,
        opts: DbOptions,
        sink: Arc<dyn CoreEventSink>,
    ) -> Result<Self> {
        Self::open_observed(dir.as_ref(), opts, OpenMode::Reader, sink)
    }

    fn open_observed(
        dir: &std::path::Path,
        opts: DbOptions,
        mode: OpenMode,
        sink: Arc<dyn CoreEventSink>,
    ) -> Result<Self> {
        let operation_id = events::next_operation_id();
        #[cfg(feature = "tracing")]
        let span = tracing::info_span!(
            "yesno.db.open",
            operation_id,
            mode = mode.as_str(),
            path = %dir.display(),
        );
        #[cfg(feature = "tracing")]
        let _entered = span.enter();
        events::emit(
            &sink,
            CoreEvent::DatabaseOpenStarted {
                operation_id,
                mode: mode.event_mode(),
            },
        );
        match Self::open_inner(dir, opts, mode, sink.clone()) {
            Ok(db) => {
                events::emit(
                    &sink,
                    CoreEvent::DatabaseOpenCompleted {
                        operation_id,
                        mode: mode.event_mode(),
                        database_uuid: db.inner.route.uuid,
                        shards: db.inner.shards.len() as u32,
                        epoch: db.inner.epoch,
                        term: db.inner.route.term,
                        visible: db.inner.oracle.visible(),
                    },
                );
                #[cfg(feature = "tracing")]
                tracing::info!(
                    database_uuid = ?db.inner.route.uuid,
                    shards = db.inner.shards.len(),
                    epoch = db.inner.epoch,
                    term = db.inner.route.term,
                    visible = db.inner.oracle.visible(),
                    "database opened"
                );
                Ok(db)
            }
            Err(error) => {
                events::emit(
                    &sink,
                    CoreEvent::DatabaseOpenFailed {
                        operation_id,
                        mode: mode.event_mode(),
                        error: EventError::from_codec(&error),
                    },
                );
                #[cfg(feature = "tracing")]
                tracing::error!(error = %error, "database open failed");
                Err(error)
            }
        }
    }

    fn open_inner(
        dir: &std::path::Path,
        opts: DbOptions,
        mode: OpenMode,
        sink: Arc<dyn CoreEventSink>,
    ) -> Result<Self> {
        let replica = mode == OpenMode::Replica;
        let read_only = mode == OpenMode::Reader;
        std::fs::create_dir_all(dir)
            .map_err(|_| CodecError::Invariant("cannot create database directory"))?;

        // I1: little-endian only, enforced at open.
        //
        // Every zero-copy path in the crate reinterprets stored bytes as `u16`
        // or `u64` in host order — container payloads, index nodes, the slab
        // table. On a big-endian host those casts succeed and return
        // byte-swapped values, so the failure mode is **silently wrong answers**
        // rather than a crash or a checksum error. There is no byteswap path in
        // v1 and this is the check that keeps its absence honest.
        if cfg!(target_endian = "big") {
            return Err(CodecError::UnsupportedEndianness);
        }

        // Exclusive access, before anything else touches the directory.
        //
        // Two processes opening the same database is unbounded corruption, not a
        // race that resolves: both replay the WAL, both allocate extents from
        // their own view of the slab table, and both flip the superblock. There
        // is no invariant in this design that survives it. `flock` is released
        // by the kernel when the last descriptor closes, so a crashed process
        // never leaves a stale lock behind — which is why this is a lock file
        // and not a pid file.
        // A reader takes no lock, which is the whole point — but it also
        // means a reader cannot create or repair anything, because nothing
        // serialises it against the writer that owns the directory.
        let (lock, epoch) = if read_only {
            (None, 0)
        } else {
            let (l, e) = acquire_lock(dir)?;
            (Some(l), e)
        };

        // A stable identity so a shard file cannot be mixed into another
        // database, and the `vshard -> shard` map that routes every key.
        //
        // **The shard count is a property of the database, not of the open
        // call.** It used to come from `DbOptions` on every open and be
        // persisted nowhere, so reopening a 4-shard database with 8 routed every
        // key to a different file than the one holding it: measured at 31 of 64
        // keys readable, silently. Refusing a mismatch is what turns that into a
        // failure the caller can see. See `db::manifest`.
        let manifest_path = dir.join("MANIFEST");
        // Distinguish *absent* from *unreadable*. An absent manifest beside
        // existing shard files is a pre-MANIFEST database and is migrated below;
        // a present one whose slots are both torn must **not** be re-created,
        // because a fresh map would route keys to shards that do not hold them.
        let live = match std::fs::read(&manifest_path) {
            Ok(b) => match manifest::pick(&b)? {
                Some(m) => Some(m),
                None => return Err(CodecError::ManifestUnreadable),
            },
            Err(_) => None,
        };
        let manifest = match live {
            // **`DbOptions::shards` is a *creation* parameter.** On an
            // existing database the persisted count wins, and it must: the count
            // is part of the routing function, so there is no way to serve four
            // shards' data as eight. Refusing the mismatch was tried first and
            // is worse — it makes a perfectly good database unopenable by a
            // caller who passed a struct default, which `e2e/scenarios/lifecycle.py`
            // did immediately. Adopting cannot route a key wrongly; refusing
            // cannot either, but it can refuse work that was always fine.
            Some(m) => m,
            None => {
                // Carry a pre-MANIFEST database's identity forward rather than
                // minting a new one, which would orphan every shard file.
                let id = match std::fs::read(dir.join("UUID")) {
                    Ok(b) if b.len() == 16 => b.try_into().unwrap(),
                    _ => fresh_uuid(),
                };
                let m = manifest::Manifest::create(id, opts.shards as u32);
                write_manifest(&manifest_path, &m)?;
                m
            }
        };
        let db_uuid = manifest.uuid;

        let shard_count = manifest.shards as usize;
        let mut shards = Vec::with_capacity(shard_count);
        for i in 0..shard_count {
            let path = dir.join(format!("shard-{i:04}.yno"));
            // The store first, and the order is load-bearing. An empty active
            // WAL with no sealed generation carries no record to name its base;
            // `wal_replay_lsn` in this shard's superblock is the authority.
            if read_only {
                // No log, and therefore no replay. A reader that opened the
                // WAL would be reading a file the writer is actively appending
                // to, with no lock ordering to make that meaningful — and
                // replaying it would build a memtable the writer never agreed
                // to. Checkpoint-visible state is exactly what the superblock
                // already published.
                let store = ShardStore::open_read_only(&path, db_uuid, i as u32)?;
                shards.push(Shard::with_store_read_only(store));
                continue;
            }
            let store = ShardStore::open(&path, db_uuid, i as u32)?;
            let base_if_empty = store.superblock().wal_replay_lsn;
            let wal = WalWriter::open(dir.join(format!("shard-{i:04}.wal")), base_if_empty)?;
            shards.push(Shard::with_store(store, wal)?);
        }

        // The checkpoint watermark is the durable floor; the log carries
        // whatever was committed above it.
        let checkpoint_cv = shards
            .iter()
            .filter_map(|s| s.store.as_ref())
            .map(|s| s.lock().unwrap().superblock().checkpoint_cv)
            .max()
            .unwrap_or(0);

        // The commit clock's floor from the images, for the case the replayed
        // WAL cannot supply one: a restart right after a checkpoint replays no
        // stamped record at all. Zero from every shard means the images predate
        // the field, and the WAL below is the only source there is.
        let checkpoint_clock = shards
            .iter()
            .filter_map(|s| s.store.as_ref())
            .map(|s| s.lock().unwrap().superblock().commit_clock)
            .max()
            .unwrap_or(0);

        // A reader has no log to replay, so the checkpoint watermark *is* the
        // visible version.
        let recovery_operation = events::next_operation_id();
        events::emit(
            &sink,
            CoreEvent::RecoveryStarted {
                operation_id: recovery_operation,
                checkpoint_version: checkpoint_cv,
            },
        );
        let recovery = if read_only {
            RecoveryReport {
                recovered_version: checkpoint_cv,
                records_replayed: 0,
                discarded_versions: Vec::new(),
                // A reader opens no log and replays nothing, so it stamps
                // nothing either. It never assigns a version, so its clock is
                // never consulted.
                max_time: 0,
            }
        } else {
            #[cfg(feature = "tracing")]
            let span = tracing::info_span!(
                "yesno.db.recover",
                operation_id = recovery_operation,
                checkpoint_version = checkpoint_cv,
                shards = shards.len(),
            );
            #[cfg(feature = "tracing")]
            let _entered = span.enter();
            match replay_logs(&shards, checkpoint_cv, &sink, recovery_operation) {
                Ok(report) => report,
                Err(error) => {
                    events::emit(
                        &sink,
                        CoreEvent::RecoveryFailed {
                            operation_id: recovery_operation,
                            checkpoint_version: checkpoint_cv,
                            error: EventError::from_codec(&error),
                        },
                    );
                    #[cfg(feature = "tracing")]
                    tracing::error!(error = %error, "recovery failed");
                    return Err(error);
                }
            }
        };
        let resume = recovery.recovered_version;
        // Whichever floor is higher. Neither alone is enough: the WAL misses
        // everything already checkpointed away, and the image misses everything
        // committed since.
        let resume_time = recovery.max_time.max(checkpoint_clock);
        #[cfg(feature = "tracing")]
        tracing::info!(
            operation_id = recovery_operation,
            checkpoint_version = checkpoint_cv,
            recovered_version = recovery.recovered_version,
            records_replayed = recovery.records_replayed,
            discarded_versions = recovery.discarded_versions.len(),
            "recovery completed"
        );
        events::emit(
            &sink,
            CoreEvent::RecoveryCompleted {
                operation_id: recovery_operation,
                checkpoint_version: checkpoint_cv,
                recovered_version: recovery.recovered_version,
                records_replayed: recovery.records_replayed,
                discarded_versions: recovery.discarded_versions,
            },
        );

        // Registered **before** any read, and for the whole life of the
        // handle rather than per snapshot. The writer's reclamation floor is
        // computed from this file; a reader that registered lazily — on its
        // first `snapshot()` — would have a window in which the writer believed
        // nothing was reading, and the extents this handle is about to follow
        // could be reused inside it.
        //
        // The pinned checkpoint sequence is the oldest any shard published,
        // because a snapshot from this handle may read through any of them.
        let registration = if read_only {
            let ckpt_seq = shards
                .iter()
                .filter_map(|s| s.store.as_ref())
                .map(|s| s.lock().unwrap().superblock().checkpoint_seq)
                .min()
                .unwrap_or(0);
            Some(readers::ReaderRegistration::claim(dir, resume, ckpt_seq)?)
        } else {
            None
        };

        // Recompute allocator occupancy from the committed index.
        //
        // The deferred free list is in-memory ( I3 ), so anything superseded
        // but not yet past the reclamation conditions is queued nowhere durable
        // while its slot stays persisted as used. Without this it is orphaned
        // for good, once per restart. See
        // `ShardStore::rebuild_allocator_at_open` for why this is safe only
        // here, and why it refuses rather than guesses.
        // Never for a reader. `rebuild_allocator_at_open` writes the slab
        // table back, which is a mutation of a database this process does not
        // own — and the allocator it repairs is only consulted by paths a reader
        // never takes.
        if opts.rebuild_alloc_on_open && !read_only {
            for shard in &shards {
                if let Some(store) = shard.store.as_ref() {
                    store.lock().unwrap().rebuild_allocator_at_open()?;
                }
            }
        }

        // Mark where this open begins. A record written by a previous epoch and
        // a record written by this one are then distinguishable in the stream,
        // which is what a follower ( or a future Raft term change ) needs to
        // reject a stale writer's tail.
        //
        // Never on a replica. See `open_replica`: this log's LSN space
        // belongs to the leader, and one local frame in it makes every shipped
        // frame after it land at the wrong offset — silently.
        if !replica && !read_only {
            for shard in &shards {
                if let Some(wal) = shard.wal.as_ref() {
                    wal.append_and_sync(
                        RecType::EpochFence,
                        0,
                        manifest.term as u64,
                        epoch.to_le_bytes().to_vec(),
                    )?;
                }
            }
        }

        let oracle = VersionOracle::new(opts.commit_ring.next_power_of_two());
        oracle.resume_at(resume, resume_time);
        let manifest_term = manifest.term;

        Ok(Db {
            evacuated: Arc::new(AtomicU64::new(0)),
            last_checkpoint: Arc::new(AtomicU64::new(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
            )),
            inner: Arc::new(DbInner {
                _lock: lock,
                read_only,
                _registration: registration,
                dir: Some(dir.to_path_buf()),
                epoch,
                shards,
                route: manifest,
                retention: Default::default(),
                policy: opts.policy,
                evacuate_per_checkpoint: opts.evacuate_per_checkpoint,
                oracle,
                readers: (0..opts.max_readers)
                    .map(|_| AtomicU64::new(FREE))
                    .collect(),
                reader_roots: (0..opts.max_readers).map(|_| AtomicU64::new(0)).collect(),
                reader_evicted: (0..opts.max_readers)
                    .map(|_| std::sync::atomic::AtomicBool::new(false))
                    .collect(),
                reader_ckpt_seqs: (0..opts.max_readers).map(|_| AtomicU64::new(0)).collect(),
                reader_started_micros: (0..opts.max_readers).map(|_| AtomicU64::new(0)).collect(),
                read_floor: AtomicU64::new(0),
                on_space_amp: opts.on_space_amp,
                space_amp_soft_permille: opts.space_amp_soft_permille,
                snapshot_soft_age_secs: opts.snapshot_soft_age_secs,
                snapshot_age_soft_breached: std::sync::atomic::AtomicBool::new(false),
                space_amp_soft_breached: std::sync::atomic::AtomicBool::new(false),
                replica,
                apply: replica.then(|| Mutex::new(apply::ApplyState::default())),
                backup_barrier: Arc::new(BackupBarrier::default()),
                checkpoint_hook: std::sync::Mutex::new(None),
                events: sink.clone(),
                lifecycle: LifecycleGuard {
                    sink,
                    operation_id: Mutex::new(None),
                    epoch,
                    term: manifest_term,
                },
            }),
        })
    }

    #[inline]
    /// The leadership term this database is at, as the MANIFEST records it.
    ///
    /// **Not [`Db::epoch`]**, which is a per-directory lock counter that
    /// increments on every open. A term is inherited with the MANIFEST, so terms
    /// from different nodes are comparable; two nodes' epochs are unrelated
    /// numbers and comparing them would be meaningless.
    pub fn term(&self) -> u32 {
        self.inner.route.term
    }

    /// Mark an orderly shutdown before the last database owner is released.
    ///
    /// The matching completion fact is emitted only when the final `DbInner`
    /// owner disappears, which includes snapshots that outlive every `Db`
    /// handle and continue to hold the directory lock.
    pub fn begin_shutdown(&self, reason: events::ShutdownReason) -> events::OperationId {
        let mut current = self.inner.lifecycle.operation_id.lock().unwrap();
        if let Some(operation_id) = *current {
            return operation_id;
        }
        let operation_id = events::next_operation_id();
        *current = Some(operation_id);
        drop(current);
        events::emit(
            &self.inner.events,
            CoreEvent::DatabaseShutdownStarted {
                operation_id,
                reason,
            },
        );
        operation_id
    }

    /// The term, widened for the WAL record header.
    fn term64(&self) -> u64 {
        self.inner.route.term as u64
    }

    /// Apply one shipped batch of raw leader WAL frames.
    ///
    /// This is what makes a replica *live*: frames land in the database that is
    /// already open and serving, rather than in a directory nothing holds.
    ///
    /// # The order below is the safety argument
    ///
    /// 1. **Verify the batch.** Taking a [`crate::repl::WalBatch`] rather than a byte
    ///    slice is deliberate: the CRC lives on the batch, so checking it is
    ///    unskippable instead of something a caller must remember. This crate
    ///    already rejected the alternative once, in as many words — "a public
    ///    `verify_leader` an operator must remember to call is a rule that gets
    ///    forgotten".
    /// 2. **Append the frames at the offset their headers name**, through the
    ///    shard's own writer so `GroupCommit`'s byte counter stays exact and the
    ///    WAL-size checkpoint trigger keeps working.
    /// 3. **Decode with the same `Scanner` recovery uses**, and apply with the
    ///    same [`apply::apply_record`].
    /// 4. **Advance the watermark** only as far as the commit table says every
    ///    participant of a version has arrived — the leader's own rule, so a
    ///    multi-shard commit stays atomic across the wire.
    ///
    /// **`enforce_policy` runs after the locks are dropped**, and must. It can
    /// reach `checkpoint`, which takes the same shard locks; calling it while
    /// they are held is a `std::sync::Mutex` self-deadlock, which presents as a
    /// hang rather than a failure. `WriteBatch::commit` has the same shape for
    /// the same reason.
    ///
    /// A version whose participants never all arrive leaves the watermark
    /// where it is, permanently. That is the honest cost of applying as bytes
    /// arrive rather than with the whole log in hand, it is the same "safe stop"
    /// recovery computes, and it is observable: `records` climbs while `visible`
    /// does not.
    pub fn apply_wal_batch(&self, batch: &crate::repl::WalBatch) -> Result<apply::Applied> {
        #[cfg(feature = "tracing")]
        let span = tracing::debug_span!(
            "yesno.db.apply_wal_batch",
            shard = batch.shard,
            first_lsn = batch.first_lsn,
            bytes = batch.records.len(),
            heartbeat = batch.heartbeat,
        );
        #[cfg(feature = "tracing")]
        let _entered = span.enter();
        let Some(state) = self.inner.apply.as_ref() else {
            return Err(CodecError::Invariant(
                "apply_wal_batch is for a replica; open it with Db::open_replica",
            ));
        };
        batch.verify()?;

        let s = batch.shard as usize;
        let shard = self.inner.shards.get(s).ok_or(CodecError::Invariant(
            "batch names a shard that does not exist",
        ))?;
        let Some(wal) = shard.wal.as_ref() else {
            return Err(CodecError::Invariant(
                "this replica has no log for that shard",
            ));
        };

        let mut st = state.lock().unwrap();
        let visible;
        let mut records = 0u64;
        let next_lsn;
        {
            // The write lock, held across the whole batch: re-taking it per
            // record would let a reader observe half a commit.
            let _w = shard.write.lock().unwrap();

            if !batch.heartbeat && !batch.records.is_empty() {
                wal.append_with(|w| w.append_frames_at(batch.first_lsn, &batch.records))?;
            }
            next_lsn = wal.log().end_lsn();

            if !batch.heartbeat {
                let mut mem = shard.mem.write().unwrap();
                let mut scanner = crate::wal::Scanner::new(&batch.records, batch.first_lsn);
                for r in &mut scanner {
                    let r = r?;
                    st.table.observe(batch.shard, &r)?;
                    if apply::is_redo_material(&r) {
                        apply::apply_record(shard, &mut mem, &r)?;
                        records += 1;
                    }
                }
            }
            visible = st.table.resolved_through(self.inner.oracle.visible());
        }
        drop(st);

        self.inner.oracle.adopt_visible(visible);
        // Outside every lock. See the note above.
        self.enforce_policy()?;

        let applied = apply::Applied {
            shard: batch.shard,
            bytes: if batch.heartbeat {
                0
            } else {
                batch.records.len() as u64
            },
            records,
            next_lsn,
            visible: self.inner.oracle.visible(),
        };
        #[cfg(feature = "tracing")]
        tracing::debug!(
            records = applied.records,
            next_lsn = applied.next_lsn,
            visible = applied.visible,
            "WAL batch applied"
        );
        Ok(applied)
    }

    /// The LSN this shard's log expects next — for `Subscribe` and for `Ack`.
    pub fn apply_cursor(&self, shard: u32) -> Result<u64> {
        let sh = self
            .inner
            .shards
            .get(shard as usize)
            .ok_or(CodecError::Invariant("no such shard"))?;
        match sh.wal.as_ref() {
            Some(w) => Ok(w.log().end_lsn()),
            None => Err(CodecError::Invariant("this database has no log")),
        }
    }

    /// Whether this handle was opened by [`Db::open_replica`].
    pub fn is_replica(&self) -> bool {
        self.inner.replica
    }

    pub fn shard_count(&self) -> usize {
        self.inner.shards.len()
    }

    #[inline]
    pub fn is_durable(&self) -> bool {
        self.inner.shards.first().is_some_and(|s| s.store.is_some())
    }

    /// Physical shard for a key.
    #[inline]
    /// The retention floor this database's checkpoints consult.
    ///
    /// Hand a clone to whatever serves followers — `yesno-server`'s replication `Ack`
    /// handler publishes into it — and a checkpoint retains generations a
    /// follower still needs, up to `CheckpointPolicy::max_wal_bytes`.
    ///
    /// Deliberately **not** a `DbOptions` field: `DbOptions` is `Copy` and
    /// used as a value in every open call site, and an `Arc` in it would break
    /// that for something only a replicating deployment sets. The database owns
    /// one either way; this hands out a handle to it.
    pub fn retention_floor(&self) -> crate::repl::RetentionFloor {
        self.inner.retention.clone()
    }

    pub fn shard_of(&self, key: u64) -> usize {
        self.inner.route.shard_of_vshard(vshard_of(key))
    }

    /// Versions at or below this are readable.
    #[inline]
    pub fn visible(&self) -> Version {
        self.inner.oracle.visible()
    }

    /// Block until `version` is readable, or `timeout` elapses.
    ///
    /// `Ok( () )` means [`snapshot_at( version )`](Self::snapshot_at) will not
    /// fail with [`VersionNotVisible`](crate::error::CodecError::VersionNotVisible)
    /// — it may still fail with `VersionReclaimed` if a checkpoint overtakes the
    /// caller, which is a different problem and not one waiting can fix.
    ///
    /// This is the read-your-writes primitive: [`WriteBatch::commit`] returns a
    /// version before that version is necessarily visible, because the watermark
    /// advances over a consecutive prefix and an earlier commit may still be in
    /// its fsync. Passing the returned version here closes that window.
    ///
    /// **`commit` deliberately does not do this for you.** Making it wait
    /// would serialize every committer behind the slowest concurrent one, which
    /// is exactly what releasing the shard guards before the fsync exists to
    /// avoid. The cost belongs to the caller that needs recency.
    ///
    /// A timeout is not proof the version will never appear, and a version
    /// this database never assigned also reports a timeout — see
    /// [`VersionOracle::wait_visible`](crate::mvcc::VersionOracle::wait_visible)
    /// for why no cheaper distinction is available on a replica.
    ///
    /// [`WriteBatch::commit`]: crate::db::WriteBatch::commit
    pub fn wait_visible(&self, version: Version, timeout: std::time::Duration) -> Result<()> {
        if self.inner.oracle.wait_visible(version, timeout) {
            return Ok(());
        }
        Err(CodecError::VisibilityTimeout {
            requested: version,
            visible: self.inner.oracle.visible(),
            waited_micros: timeout.as_micros().min(u128::from(u64::MAX)) as u64,
        })
    }

    /// A consistent read view across every shard, at the newest visible version.
    pub fn snapshot(&self) -> Result<Snapshot> {
        self.claim_snapshot(self.inner.oracle.visible())
    }

    /// A consistent read view **at a version the caller names**.
    ///
    /// This is what makes a multi-endpoint read consistent. A coordinator that
    /// hands N readers disjoint slices of one query has to hand them all the
    /// same version, or the union it assembles is a set that never existed at
    /// any instant — which is precisely what a Flight ticket's version field is
    /// for, and precisely what it did not achieve while `do_get` opened a fresh
    /// snapshot and ignored it.
    ///
    /// # It refuses rather than approximates
    ///
    /// - Above [`visible`](Self::visible) → [`VersionNotVisible`]. Not a version
    ///   this database has assigned.
    /// - Below the reclamation floor → [`VersionReclaimed`]. The state existed
    ///   and a checkpoint has since collapsed it away.
    ///
    /// Both are `Err` on purpose, and the second is the one worth arguing
    /// about: the obvious alternative — answer from the floor — returns a
    /// *plausible* set that is not the one asked for, and the caller has no way
    /// to tell. A coordinator would silently assemble a torn union. An error it
    /// can retry at a fresh version is strictly better than a wrong answer it
    /// cannot detect.
    ///
    /// `VersionReclaimed` is never retryable by waiting: it names a version
    /// this database has already discarded, and the floor only rises.
    ///
    /// **`VersionNotVisible` is a different matter, and this comment said
    /// otherwise until 2026-09-12.** It claimed the variant "names a version
    /// this database will never assign". That is the *usual* cause and not the
    /// only one: `visible` advances over a consecutive prefix, so a commit that
    /// resolves while an earlier one is still pending holds a version that is
    /// assigned and durable and momentarily unreadable — demonstrated by
    /// `a_returned_commit_can_be_invisible_while_an_earlier_one_is_pending`,
    /// which observed `commit` return v20 with `visible` at 18. A caller racing
    /// its own commit should [`wait_visible`](Self::wait_visible) rather than
    /// conclude the version does not exist.
    ///
    /// [`VersionNotVisible`]: crate::error::CodecError::VersionNotVisible
    /// [`VersionReclaimed`]: crate::error::CodecError::VersionReclaimed
    pub fn snapshot_at(&self, version: Version) -> Result<Snapshot> {
        let visible = self.inner.oracle.visible();
        if version > visible {
            return Err(CodecError::VersionNotVisible {
                requested: version,
                visible,
            });
        }
        // Claim first, validate second, and the order is the whole proof.
        //
        // Publishing the slot at `version` pulls `safe_version` down to it, so
        // any checkpoint that reads the floor *after* this point is already
        // bounded by us and cannot prune what we are about to read. What remains
        // is a checkpoint that finished *before* the claim, and that is exactly
        // what `read_floor` records. Checking first would leave a window in
        // which a checkpoint runs between the check and the claim — the same
        // check-then-act shape that made the reader-root capture wrong until it
        // was taken under one lock hold.
        let snap = self.claim_snapshot(version)?;
        let floor = self.inner.read_floor.load(Ordering::Acquire);
        if version < floor {
            drop(snap);
            return Err(CodecError::VersionReclaimed {
                requested: version,
                floor,
            });
        }
        Ok(snap)
    }

    /// The oldest version [`snapshot_at`](Self::snapshot_at) can still serve.
    ///
    /// Monotonic, and raised by checkpoints rather than by writes: it is history
    /// being collapsed that costs readability, not new versions being assigned.
    pub fn read_floor(&self) -> Version {
        self.inner.read_floor.load(Ordering::Acquire)
    }

    fn claim_snapshot(&self, version: Version) -> Result<Snapshot> {
        for (i, slot) in self.inner.readers.iter().enumerate() {
            if slot
                .compare_exchange(FREE, version, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                // Capture the index root live *now*, together with the
                // watermark it corresponds to, under **one lock acquisition per
                // shard**.
                //
                // A later checkpoint installs a new root, but this one stays
                // readable because pages are never rewritten in place ( I2 )
                // and reclamation waits on `safe_version`. Without capturing,
                // a snapshot older than the next checkpoint would fall through
                // to that checkpoint's state for any chunk absent from its
                // memtable.
                //
                // The root and the watermark must come from the same lock
                // hold. They used to be two separate passes — `s.tree()` for
                // each shard, then `root_watermark()` for each shard again — and
                // a checkpoint landing between them recorded a watermark
                // *newer* than the root actually pinned. `evict_floor` then
                // returned a floor above what this reader needs, and
                // `evict_durable` dropped memtable versions that only this
                // reader's old root could not answer for. The key vanishes for
                // that snapshot alone, and for no one else.
                // **Cleared on claim, not merely on release.** The slot is
                // published ( the CAS above ) before its sequence is known, so
                // for the length of the capture below `reader_ckpt_floor` sees
                // an *active* slot. Whatever the previous occupant left must not
                // be what it reads: `0` is the conservative unpublished state
                // and blocks reclamation, any higher value permits it. `Drop`
                // also stores `0`, but relying on that alone makes this window's
                // safety a property of the *previous* reader's cleanup rather
                // than of this claim — which is exactly the kind of ordering
                // convention that holds until someone changes the other end.
                self.inner.reader_ckpt_seqs[i].store(0, Ordering::Release);

                let mut roots = Vec::with_capacity(self.inner.shards.len());
                let mut watermark = Version::MAX;
                // Taken under the **same lock hold** as the root, for the
                // same reason the watermark is: a checkpoint landing between
                // two reads would record a sequence that does not describe the
                // root actually captured, which is the whole defect this
                // guards. See `reader_ckpt_seqs`.
                let mut ckpt_seq = u64::MAX;
                for s in &self.inner.shards {
                    match s.store.as_ref() {
                        Some(st) => {
                            let g = st.lock().unwrap();
                            watermark = watermark.min(g.superblock().checkpoint_cv);
                            ckpt_seq = ckpt_seq.min(g.superblock().checkpoint_seq);
                            roots.push(g.tree());
                        }
                        None => roots.push(None),
                    }
                }
                if ckpt_seq == u64::MAX {
                    ckpt_seq = 0; // no store: nothing to reach through
                }
                self.inner.reader_ckpt_seqs[i].store(ckpt_seq, Ordering::Release);
                self.inner.reader_evicted[i].store(false, Ordering::Release);
                self.inner.reader_roots[i].store(watermark, Ordering::Release);
                self.inner.reader_started_micros[i].store(now_micros(), Ordering::Release);
                return Ok(Snapshot {
                    db: self.inner.clone(),
                    version,
                    _slot: Arc::new(ReaderSlot {
                        db: self.inner.clone(),
                        slot: i,
                    }),
                    roots,
                });
            }
        }
        // Refusing beats running unregistered: an unregistered reader makes
        // `safe_version` a lie and lets reclamation free data it is reading.
        Err(CodecError::Invariant("snapshot registry is full"))
    }

    /// Highest version the memtable may forget, given every live reader.
    ///
    /// Bounded by the **oldest pinned root**, not by `safe_version`: a reader's
    /// root can be arbitrarily older than its version, and it is the root that
    /// decides what the store can answer for it.
    ///
    /// There is deliberately no `root_watermark()` helper any more. Reading
    /// the watermark in a pass of its own is what let it disagree with the
    /// roots a snapshot had already captured; `Db::snapshot` now takes both
    /// under one lock hold per shard. Do not reintroduce a standalone
    /// watermark read for this purpose.
    fn evict_floor(&self) -> Version {
        let mut floor = Version::MAX;
        for (slot, r) in self.inner.readers.iter().enumerate() {
            // An evicted reader is entitled to nothing: that is what evicting it
            // means, and skipping it here is what actually returns the space.
            if r.load(Ordering::Acquire) != FREE
                && !self.inner.reader_evicted[slot].load(Ordering::Acquire)
            {
                floor = floor.min(self.inner.reader_roots[slot].load(Ordering::Acquire));
            }
        }
        floor
    }

    /// Oldest checkpoint whose root any live reader still holds.
    ///
    /// An extent superseded by checkpoint `k` is reachable from the roots of
    /// checkpoints below `k`, so it is free once this is `>= k`. `u64::MAX` when
    /// there are no live readers, which frees everything eligible.
    ///
    /// A slot that has been claimed but has not yet published its sequence
    /// reads as `0`, which blocks reclamation rather than permitting it. That is
    /// deliberate and is the fix: `Db::snapshot` publishes the slot *before* it
    /// captures roots, so this window is exactly when a reader is about to
    /// acquire a root nobody knows about yet.
    pub fn reader_ckpt_floor(&self) -> u64 {
        let mut floor = u64::MAX;
        for (slot, r) in self.inner.readers.iter().enumerate() {
            if r.load(Ordering::Acquire) != FREE
                && !self.inner.reader_evicted[slot].load(Ordering::Acquire)
            {
                floor = floor.min(self.inner.reader_ckpt_seqs[slot].load(Ordering::Acquire));
            }
        }
        // **Readers in other processes count too, and this is the whole
        // reason the registry exists.** The loop above sees only this process's
        // snapshots; a `Db::open_reader` handle elsewhere is mapping the same
        // extents and is invisible to it. Reclaiming on the strength of the
        // local floor alone would reuse a slot a foreign reader is still
        // following. Do not drop this because "readers are rare" — the cost
        // is one page walk, and the failure is a read that should have
        // succeeded returning a decode error.
        if let Some((_, foreign_ckpt)) = self.foreign_reader_floors() {
            floor = floor.min(foreign_ckpt);
        }
        floor
    }

    /// The floors imposed by readers in **other processes**, if any.
    ///
    /// `None` for an in-memory database, which no other process can reach.
    fn foreign_reader_floors(&self) -> Option<(Version, u64)> {
        readers::floors(self.inner.dir.as_deref()?)
    }

    /// Oldest version any live reader is entitled to.
    pub fn safe_version(&self) -> Version {
        let mut min = self.inner.oracle.visible();
        for (slot, s) in self.inner.readers.iter().enumerate() {
            if self.inner.reader_evicted[slot].load(Ordering::Acquire) {
                continue;
            }
            let v = s.load(Ordering::Acquire);
            if v != FREE && v < min {
                min = v;
            }
        }
        // Same reasoning as `reader_ckpt_floor`: a foreign reader pins a
        // version this process has no record of. Memtable eviction is driven
        // from here, so ignoring it would drop chunk versions a reader in
        // another process is entitled to see.
        if let Some((foreign_version, _)) = self.foreign_reader_floors() {
            min = min.min(foreign_version);
        }
        min
    }

    pub fn live_readers(&self) -> usize {
        self.inner
            .readers
            .iter()
            .filter(|s| s.load(Ordering::Acquire) != FREE)
            .count()
    }

    pub fn batch(&self) -> WriteBatch {
        WriteBatch {
            handle: self.clone(),
            ops: Vec::new(),
            err: None,
            keys_ascending: true,
            last_key: None,
        }
    }

    /// Autocommit one insert.
    pub fn insert(&self, key: u64, ordinal: u64) -> Result<bool> {
        let mut b = self.batch();
        b.insert(key, ordinal);
        Ok(b.commit()?.changed > 0)
    }

    pub fn remove(&self, key: u64, ordinal: u64) -> Result<bool> {
        let mut b = self.batch();
        b.remove(key, ordinal);
        Ok(b.commit()?.changed > 0)
    }

    /// Autocommit `[lo, hi]` inclusive under `key`. Returns ordinals added.
    pub fn insert_range(&self, key: u64, lo: u64, hi: u64) -> Result<u64> {
        let mut b = self.batch();
        b.insert_range(key, lo, hi);
        Ok(b.commit()?.changed)
    }

    /// Autocommit removal of `[lo, hi]` inclusive. Returns ordinals removed.
    pub fn remove_range(&self, key: u64, lo: u64, hi: u64) -> Result<u64> {
        let mut b = self.batch();
        b.remove_range(key, lo, hi);
        Ok(b.commit()?.changed)
    }

    /// Autocommit many ordinals under one key. Returns ordinals added.
    ///
    /// Contiguous runs are folded into range ops **here**, before an `Op` per
    /// ordinal is ever materialized: a million sorted ordinals would otherwise
    /// cost a million-entry vector and a sort of it before `plan_ops` could
    /// discover they were one range all along.
    ///
    /// The fold uses the same threshold `plan_ops` does, and that matters: a run
    /// too short to be worth a `SetRange` must stay an `Op::Insert` so the
    /// planner can still group it into a per-chunk `ChunkDelta`. Folding every
    /// run, including singletons, would turn a scattered batch back into one
    /// record per ordinal.
    /// This open's fencing epoch. Zero for an in-memory database.
    ///
    /// Strictly increases per acquisition of the database lock, so a record
    /// written by a previous open and one written by this one are
    /// distinguishable — the guard against a writer that lost its lease and
    /// resumed. Written to each shard's WAL as an `EpochFence` at open.
    pub fn epoch(&self) -> u64 {
        self.inner.epoch
    }

    pub fn insert_many(&self, key: u64, ordinals: &[u64]) -> Result<u64> {
        let mut b = self.batch();
        let mut sorted: Vec<u64> = ordinals.to_vec();
        sorted.sort_unstable();
        sorted.dedup();

        // Checked here rather than left to `commit`, because the run-folding
        // below computes `sorted[j] + 1` and `hi - lo + 1`. Both overflow on
        // `u64::MAX`, and a debug build would panic before the batch could
        // report the error. Sorted, so the last element is the only candidate.
        if let Some(&max) = sorted.last() {
            crate::check_ordinal(max)?;
        }

        let mut i = 0;
        while i < sorted.len() {
            let mut j = i;
            while j + 1 < sorted.len() && sorted[j + 1] == sorted[j] + 1 {
                j += 1;
            }
            let (lo, hi) = (sorted[i], sorted[j]);
            let crosses = crate::split(lo).0 != crate::split(hi).0;
            if crosses || hi - lo + 1 >= SETRANGE_MIN_RUN {
                b.insert_range(key, lo, hi);
            } else {
                for &o in &sorted[i..=j] {
                    b.insert(key, o);
                }
            }
            i = j + 1;
        }
        Ok(b.commit()?.changed)
    }

    /// Total dirty bytes across shards. Drives the checkpoint trigger.
    /// Checkpoint if the policy says to, and **stall** if it says we must.
    ///
    /// "Stall" here is a synchronous checkpoint on the writer's own thread. That
    /// is the honest form of back-pressure for an embedded database with no
    /// background thread: the writer waits exactly as long as it takes to make
    /// room, and cannot outrun the checkpointer because it *is* the
    /// checkpointer. A queue-and-return design would just move the unbounded
    /// growth somewhere less visible.
    ///
    /// Do not make this best-effort. `should_stall` is the only bound on
    /// memtable size; skipping it under load is precisely when it is needed.
    fn enforce_policy(&self) -> Result<()> {
        // In-memory databases have nowhere to checkpoint to, so the policy
        // cannot apply and the caller owns the bound.
        if !self.is_durable() {
            return Ok(());
        }
        let dirty = self.dirty_bytes();
        let policy = &self.inner.policy;
        // Not `0`, which is what this passed until 2026-08-28 — so
        // `CheckpointPolicy::wal_bytes`, a documented trigger with a 1 GiB
        // default, could never fire. Measured: a workload rewriting eight
        // ordinals held the memtable at nothing while the log reached 448 KB
        // against a 64 KB threshold, and **zero** checkpoints ran. The dirty
        // trigger keys on memtable size, so a small working set rewritten
        // forever is invisible to it, and only the 60 s interval bounded the
        // log at all.
        //
        // Cheap by construction: `GroupCommit::bytes` is an atomic mirrored
        // under the log lock, not a lock taken per shard per commit.
        let wal = self.wal_bytes();
        if policy.should_stall(dirty) || policy.should_checkpoint(dirty, wal, self.elapsed_secs()) {
            self.checkpoint()?;
        }
        Ok(())
    }

    /// Seconds since the last checkpoint.
    fn elapsed_secs(&self) -> u64 {
        let last = self.last_checkpoint.load(Ordering::Acquire);
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs().saturating_sub(last))
            .unwrap_or(0)
    }

    /// Extents superseded but not yet reclaimable, summed across shards.
    ///
    /// Diagnostic, and the only way a test can tell reclamation apart from doing
    /// nothing — both leave the data correct.
    /// Bytes retained by dead-but-unreclaimable extents, across all shards.
    ///
    /// See [`Db::space_amplification`]. Zero for an in-memory database.
    pub fn deferred_bytes(&self) -> u64 {
        self.inner
            .shards
            .iter()
            .filter_map(|s| s.store.as_ref())
            .map(|st| st.lock().unwrap().allocator().deferred_bytes())
            .sum()
    }

    /// Extents allocated across all shards — the fine-grained allocation count.
    ///
    /// `allocated_bytes` moves in 2 MiB slab steps, so it cannot witness I3
    /// ( "the write path allocates no file space" ) at extent granularity.
    pub fn used_extents(&self) -> u64 {
        self.inner
            .shards
            .iter()
            .filter_map(|s| s.store.as_ref())
            .map(|st| st.lock().unwrap().allocator().used_extents())
            .sum()
    }

    /// Slab capacity across all shards, in 2 MiB steps. Not extents in use.
    pub fn allocated_bytes(&self) -> u64 {
        self.inner
            .shards
            .iter()
            .filter_map(|s| s.store.as_ref())
            .map(|st| st.lock().unwrap().allocator().allocated_bytes())
            .sum()
    }

    /// Slab capacity over capacity-minus-deferred: 1.0 when nothing is retained.
    ///
    /// # What this measures, and why it is only a diagnostic
    ///
    /// A snapshot pins every extent it is *entitled* to read, so a reporting
    /// query held open across a churn workload keeps superseded extents alive
    /// for as long as it lives. [`Db::enforce_space_amp`] applies the configured
    /// policy once deferred bytes reach live bytes. `yesnod` invokes it before
    /// every periodic checkpoint; an embedded application has no background
    /// maintenance thread and must schedule enforcement itself if it wants the
    /// bound rather than this diagnostic alone.
    ///
    /// The denominator is derived from [`Db::allocated_bytes`], which is
    /// **slab capacity in 2 MiB steps**, not bytes of live extent. On sparsely
    /// filled slabs this therefore *understates* amplification. It is the right
    /// shape for watching retention grow under a long reader and the wrong
    /// number for a storage-efficiency claim; `e2e/scenarios/aged_state.py`
    /// is what measures the latter.
    pub fn space_amplification(&self) -> f64 {
        let total = self.allocated_bytes();
        let deferred = self.deferred_bytes();
        let live = total.saturating_sub(deferred);
        if live == 0 {
            return 1.0;
        }
        total as f64 / live as f64
    }

    /// Enforce the space-amplification bound, evicting readers if that is the
    /// policy.
    ///
    /// The design bounds space amplification at **2x by construction**: once
    /// deferred bytes reach live bytes, the oldest snapshot is invalidated and
    /// the check repeats. Returns how many readers were evicted.
    ///
    /// **The bound is only real because eviction is** — before this existed,
    /// `deferred_bytes` reported the retention and nothing acted on it, so a
    /// long reader retained space without limit. `Db::space_amplification` was
    /// "the number to watch", watched by nobody.
    ///
    /// Evicting frees no memory by itself and is not meant to. It stops the
    /// reader *pinning* retention ( `safe_version` and `evict_floor` skip
    /// evicted slots ), so the next checkpoint can reclaim. Already-materialized
    /// `Buffer`s stay sound regardless: that is reclamation condition 3, which
    /// is the refcount and is independent of condition 1.
    ///
    /// Under [`SpaceAmpPolicy::StallWriters`] this reports the breach and evicts
    /// nothing; the caller is expected to wait.
    /// Amplification in parts per thousand: `1000` when nothing is retained.
    ///
    /// The integer form of [`Self::space_amplification`], which is what the
    /// soft threshold compares against so that no float equality is involved.
    pub fn space_amplification_permille(&self) -> u32 {
        let total = self.allocated_bytes();
        let deferred = self.deferred_bytes();
        let live = total.saturating_sub(deferred);
        if live == 0 {
            return 1_000;
        }
        // Saturating: a pathological ratio must not wrap into "healthy".
        u32::try_from(total.saturating_mul(1_000) / live).unwrap_or(u32::MAX)
    }

    /// Whether amplification is currently above the configured soft threshold.
    ///
    /// For `/metrics`. Reads the latch rather than recomputing, so it agrees
    /// with the events that were emitted rather than with a ratio sampled at a
    /// different instant.
    pub fn space_amp_soft_breached(&self) -> bool {
        self.inner
            .space_amp_soft_breached
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Age in seconds of the longest-lived live snapshot; `0` when none.
    ///
    /// Skips evicted slots: an evicted reader no longer pins retention, so
    /// reporting its age would point an operator at a query that is not the
    /// problem.
    pub fn oldest_reader_age_secs(&self) -> u64 {
        let now = now_micros();
        let mut oldest = 0u64;
        for (i, slot) in self.inner.readers.iter().enumerate() {
            if slot.load(Ordering::Acquire) == FREE {
                continue;
            }
            if self.inner.reader_evicted[i].load(Ordering::Acquire) {
                continue;
            }
            let started = self.inner.reader_started_micros[i].load(Ordering::Acquire);
            if started == 0 {
                continue;
            }
            oldest = oldest.max(now.saturating_sub(started) / 1_000_000);
        }
        oldest
    }

    /// Whether a snapshot is currently older than the configured soft age.
    pub fn snapshot_age_soft_breached(&self) -> bool {
        self.inner
            .snapshot_age_soft_breached
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Evaluate the **snapshot soft age** and emit on a transition.
    ///
    /// Observation only: nothing is evicted. This answers a different
    /// question from [`Self::observe_space_amp`] — *which* reader is holding
    /// retention down, before the space it pins is large enough to notice.
    pub fn observe_snapshot_ages(&self) {
        let threshold = self.inner.snapshot_soft_age_secs;
        if threshold == 0 {
            return;
        }
        let oldest = self.oldest_reader_age_secs();
        let breached = oldest >= threshold;
        let was = self
            .inner
            .snapshot_age_soft_breached
            .swap(breached, std::sync::atomic::Ordering::Relaxed);
        if breached == was {
            return;
        }
        let operation_id = events::next_operation_id();
        if breached {
            events::emit(
                &self.inner.events,
                CoreEvent::SnapshotAgeSoftThreshold {
                    operation_id,
                    oldest_age_secs: oldest,
                    threshold_secs: threshold,
                    live_readers: self.live_readers() as u64,
                },
            );
        } else {
            events::emit(
                &self.inner.events,
                CoreEvent::SnapshotAgeSoftRecovered {
                    operation_id,
                    threshold_secs: threshold,
                },
            );
        }
    }

    /// Evaluate the **soft** threshold and emit on a transition.
    ///
    /// Observation only: this evicts nothing and stalls nobody. It is
    /// separate from [`Self::enforce_space_amp`] on purpose — the hard bound is
    /// an intervention and this is a report, and folding them together is how a
    /// warning turns into a second way to kill a query.
    pub fn observe_space_amp(&self) {
        let threshold = self.inner.space_amp_soft_permille;
        if threshold == 0 {
            return;
        }
        let amplification = self.space_amplification_permille();
        let breached = amplification >= threshold;
        let was = self
            .inner
            .space_amp_soft_breached
            .swap(breached, std::sync::atomic::Ordering::Relaxed);
        if breached == was {
            return;
        }
        let operation_id = events::next_operation_id();
        if breached {
            events::emit(
                &self.inner.events,
                CoreEvent::SpaceAmpSoftThreshold {
                    operation_id,
                    amplification_permille: amplification,
                    threshold_permille: threshold,
                    allocated_bytes: self.allocated_bytes(),
                    deferred_bytes: self.deferred_bytes(),
                },
            );
        } else {
            events::emit(
                &self.inner.events,
                CoreEvent::SpaceAmpSoftRecovered {
                    operation_id,
                    amplification_permille: amplification,
                    threshold_permille: threshold,
                },
            );
        }
    }

    pub fn enforce_space_amp(&self) -> usize {
        if self.inner.on_space_amp != SpaceAmpPolicy::AbortOldestReader {
            return 0;
        }
        let mut evicted = 0usize;
        // Bounded by the slot count: each pass evicts one reader and no reader
        // is evicted twice, so this cannot spin.
        for _ in 0..self.inner.readers.len() {
            let total = self.allocated_bytes();
            let deferred = self.deferred_bytes();
            if deferred == 0 || deferred < total.saturating_sub(deferred) {
                break;
            }
            if self.evict_oldest_reader() {
                evicted += 1;
            } else {
                // Nothing left to evict: the retention is not a reader's fault.
                break;
            }
        }
        evicted
    }

    /// Run `f` inside `checkpoint`, after the watermarks are sampled and before
    /// any shard is touched.
    ///
    /// **Testing seam, not an extension point.** It exists so that the
    /// reclamation hazard in `stale-root-blocks-idle-reclamation` has a
    /// *deterministic* regression instead of a threaded one that passes by
    /// luck. Nothing in production sets it.
    #[doc(hidden)]
    pub fn set_checkpoint_hook(&self, f: Option<std::sync::Arc<dyn Fn() + Send + Sync>>) {
        *self.inner.checkpoint_hook.lock().unwrap() = f;
    }

    /// Invalidate the oldest live snapshot. Returns whether one was found.
    ///
    /// The primitive under [`Self::enforce_space_amp`], public because it is
    /// also the operator control for "something is pinning space and I want it
    /// back now", and because a mechanism that can only be reached through a
    /// byte threshold cannot be tested where the threshold is not reachable.
    ///
    /// Does not free the slot. See [`Self::enforce_space_amp`].
    pub fn evict_oldest_reader(&self) -> bool {
        match self.oldest_live_reader() {
            Some(slot) => {
                self.inner.reader_evicted[slot].store(true, Ordering::Release);
                true
            }
            None => false,
        }
    }

    /// Slot of the oldest reader that is registered and not already evicted.
    fn oldest_live_reader(&self) -> Option<usize> {
        let mut best: Option<(usize, Version)> = None;
        for (slot, r) in self.inner.readers.iter().enumerate() {
            if self.inner.reader_evicted[slot].load(Ordering::Acquire) {
                continue;
            }
            let v = r.load(Ordering::Acquire);
            if v == FREE {
                continue;
            }
            if best.is_none_or(|(_, bv)| v < bv) {
                best = Some((slot, v));
            }
        }
        best.map(|(slot, _)| slot)
    }

    /// Readers currently invalidated. Diagnostics.
    pub fn evicted_reader_count(&self) -> usize {
        self.inner
            .reader_evicted
            .iter()
            .filter(|e| e.load(Ordering::Acquire))
            .count()
    }

    pub fn deferred_extents(&self) -> usize {
        self.inner
            .shards
            .iter()
            .filter_map(|s| s.store.as_ref())
            .map(|st| st.lock().unwrap().allocator().deferred_count())
            .sum()
    }

    /// Extents actually freed, summed across shards.
    ///
    /// The measure that tells a working reclaimer from an inert one — see
    /// `Allocator::freed_total` for why `deferred_extents` cannot.
    pub fn freed_extents(&self) -> u64 {
        self.inner
            .shards
            .iter()
            .filter_map(|s| s.store.as_ref())
            .map(|st| st.lock().unwrap().allocator().freed_total())
            .sum()
    }

    /// Index nodes written, summed across shards.
    ///
    /// Distinguishes a tree rebuilt whole from one that reused its unchanged
    /// leaves. Both produce identical, correct results.
    pub fn index_nodes_written(&self) -> u64 {
        self.inner
            .shards
            .iter()
            .filter_map(|s| s.store.as_ref())
            .map(|st| st.lock().unwrap().nodes_written())
            .sum()
    }

    /// Slabs allocated, summed across shards.
    ///
    /// The measure of whether space is actually recovered: with recycling, a
    /// churning workload plateaus; without it, the file grows for ever.
    pub fn slab_count(&self) -> usize {
        self.inner
            .shards
            .iter()
            .filter_map(|s| s.store.as_ref())
            .map(|st| st.lock().unwrap().allocator().slab_count())
            .sum()
    }

    /// Check every shard's allocator state against its index.
    ///
    /// **The escape hatch.** The design calls `fsck` M2-era rather than "later"
    /// precisely because it is the fallback for every other storage risk on the
    /// list — and it was written, tested, and called from nowhere until now.
    ///
    /// The index is the sole authority on liveness, so this derives what *should*
    /// be allocated by walking the tree and compares it with what the allocator
    /// believes. Two findings, and they are not equally bad:
    ///
    /// - **leaked**: a slot the allocator holds that no chunk references. Wasted
    ///   space, recoverable.
    /// - **dangling**: a slot a live chunk references that the allocator thinks
    ///   is free. That space can be handed out under a live chunk — corruption,
    ///   not waste.
    ///
    /// Slabs restored as [`SlabState::Opaque`] have no known geometry, so slots
    /// in them cannot be checked; they are reported through `errors` rather than
    /// silently passing.
    ///
    /// [`SlabState::Opaque`]: crate::store::alloc::SlabState::Opaque
    pub fn verify(&self) -> Result<Vec<crate::store::fsck::FsckReport>> {
        #[cfg(feature = "tracing")]
        let span = tracing::info_span!("yesno.db.verify", shards = self.inner.shards.len());
        #[cfg(feature = "tracing")]
        let _entered = span.enter();
        let mut out = Vec::new();
        for shard in &self.inner.shards {
            let Some(store_lock) = shard.store.as_ref() else {
                continue;
            };
            let mut store = store_lock.lock().unwrap();
            let Some(tree) = store.tree() else {
                out.push(crate::store::fsck::FsckReport::default());
                continue;
            };

            // The class comes from the slab table, which is what makes a slot
            // index computable at all. An Opaque slab has none.
            let classes: std::collections::BTreeMap<u32, u8> = (0..store.allocator().slab_count()
                as u32)
                .filter_map(|id| match store.allocator().slab(id).map(|s| s.state) {
                    Some(crate::store::alloc::SlabState::InUse { class, .. }) => Some((id, class)),
                    _ => None,
                })
                .collect();

            let (rebuilt, errors) = crate::store::fsck::rebuild(
                &tree,
                // Raw: this walk must be able to read a corrupt node to report
                // it, which `ShardStore`'s own reader refuses to do.
                &crate::db::store::RawNodes(&store),
                |slab| classes.get(&slab).copied(),
                |cell| {
                    let hdr = store.segment().read_at(cell, 2)?;
                    Ok(u16::from_le_bytes([hdr[0], hdr[1]]) as u32)
                },
            )?;
            // Invariant I8, the one check that needs both a `ChunkKey` and a
            // payload. `rebuild` has the first and not the second, `codec` has
            // the second and not the first, so neither can do it alone.
            let mut errors = errors;
            errors.extend(rebuilt.i8_violations(|ck, cref| store.read_container_for(ck, cref)));
            // Stored page checksums. Same shape as the I8 check and for the
            // same reason: `rebuild` has the references, the store has the
            // bytes. B+tree nodes are already done inside `rebuild`, which
            // holds every node through its `NodeReader`; this covers the other
            // two families, packed pages and standalone extent trailers.
            //
            // Deliberately not wired into `rebuild_allocator_at_open`: a
            // corrupt *payload* does not invalidate the liveness map, while a
            // corrupt *index node* does — and that one already flows into that
            // function's errors and blocks adoption, because adopting an
            // incomplete map does not fail to repair, it frees live data.
            errors
                .extend(rebuilt.checksum_violations(|off, len| store.segment().read_at(off, len)));
            out.push(crate::store::fsck::verify(
                &rebuilt,
                store.allocator(),
                errors,
            ));
        }
        #[cfg(feature = "tracing")]
        tracing::info!(reports = out.len(), "database verification completed");
        Ok(out)
    }

    /// Chunks relocated by slab evacuation. Diagnostics.
    pub fn evacuated_chunks(&self) -> u64 {
        self.evacuated.load(Ordering::Relaxed)
    }

    /// In-use slabs grouped by size class, ascending. Diagnostics.
    ///
    /// The question this answers is which *kind* of thing is retaining space.
    /// Index nodes all land in the class that fits `INDEX_NODE`, so a shard
    /// whose growth is index retention looks completely different from one whose
    /// growth is chunk extents, and the slab total alone cannot tell them apart.
    pub fn slabs_by_class(&self) -> Vec<(u8, usize)> {
        let mut m: std::collections::BTreeMap<u8, usize> = std::collections::BTreeMap::new();
        for sh in self.inner.shards.iter().filter_map(|s| s.store.as_ref()) {
            let mut st = sh.lock().unwrap();
            let n = st.allocator().slab_count();
            for i in 0..n as u32 {
                if let Some(crate::store::alloc::SlabState::InUse { class, .. }) =
                    st.allocator().slab(i).map(|s| s.state)
                {
                    *m.entry(class).or_insert(0) += 1;
                }
            }
        }
        m.into_iter().collect()
    }

    /// Live fraction of every in-use slab of `class`, ascending. Diagnostics.
    ///
    /// `evacuation_candidates` only offers slabs below `COMPACT_LIVE_FRACTION`,
    /// so when evacuation appears to do nothing the first question is whether
    /// any slab is actually below the threshold — a total slab count cannot
    /// distinguish "many sparse slabs, compactor not keeping up" from "no slab
    /// is sparse enough to qualify".
    pub fn live_fractions(&self, class: u8) -> Vec<(u32, u32)> {
        let mut v = Vec::new();
        for sh in self.inner.shards.iter().filter_map(|s| s.store.as_ref()) {
            let mut st = sh.lock().unwrap();
            let n = st.allocator().slab_count();
            for i in 0..n as u32 {
                if let Some(sl) = st.allocator().slab(i) {
                    if let crate::store::alloc::SlabState::InUse { class: c, .. } = sl.state {
                        if c == class {
                            v.push((sl.used_count(), sl.capacity()));
                        }
                    }
                }
            }
        }
        v.sort();
        v
    }

    /// Resolve a version that will never complete.
    ///
    /// **Mandatory, not defensive.** The visible watermark advances over a
    /// *consecutive* run of resolved versions, so one abandoned `Pending` slot
    /// stalls it for ever: no later commit becomes visible, and the next
    /// recovery treats everything above the hole as unresolved and discards it —
    /// including commits that were already acknowledged to their callers.
    ///
    /// The `Abort` record is fsynced before the slot is resolved, so a crash in
    /// the middle leaves the version unresolved on disk rather than resolved in
    /// memory and absent from the log. Best-effort on the log, because a failure
    /// here is already an error path: resolving the slot matters more than
    /// recording why, and leaving it `Pending` is the one outcome with no
    /// recovery.
    fn abort_version(&self, version: Version, participants: &[usize]) {
        // The version's *own* time, not the time of the abort. An `Abort`
        // resolves a version that was assigned earlier, and a wall-clock recovery
        // target reasons about when the version was taken. Stamping "now" here
        // would put a resolution out of order with the versions around it, which
        // is precisely the inversion I9 exists to prevent. `None` means the slot
        // has already been recycled, in which case an unstamped record is the
        // honest one.
        let time = self.inner.oracle.time_of(version);
        for &s in participants {
            let shard = &self.inner.shards[s];
            if let Some(wal) = shard.wal.as_ref() {
                let _ = match time {
                    Some(t) => {
                        wal.append_marker_and_sync(RecType::Abort, version, self.term64(), t)
                    }
                    None => wal.append_and_sync(RecType::Abort, version, self.term64(), Vec::new()),
                };
            }
        }
        self.inner.oracle.abort(version);
    }

    /// Checkpoints run so far. Test support: lets a durability test assert that
    /// it really did not checkpoint, rather than assuming the policy stayed
    /// quiet.
    pub fn checkpoint_count_for_test(&self) -> u64 {
        self.inner
            .shards
            .iter()
            .filter_map(|s| s.store.as_ref())
            .map(|st| st.lock().unwrap().superblock().checkpoint_seq)
            .max()
            .unwrap_or(0)
    }

    /// fsyncs issued to the logs, summed across shards. Diagnostics.
    pub fn wal_syncs(&self) -> u64 {
        self.inner
            .shards
            .iter()
            .filter_map(|s| s.wal.as_ref())
            .map(|w| w.log().syncs())
            .sum()
    }

    /// Total bytes currently held in the logs.
    ///
    /// On the write path, not only a diagnostic: `enforce_policy` reads it
    /// once per commit for the WAL-size trigger. That is why it goes through
    /// `GroupCommit::bytes` — an atomic mirrored under the log lock — rather
    /// than locking each shard's log to ask it directly.
    pub fn wal_bytes(&self) -> u64 {
        self.inner
            .shards
            .iter()
            .filter_map(|s| s.wal.as_ref())
            .map(|w| w.bytes())
            .sum()
    }

    /// Slab states, as `(free, in_use, opaque)`. Diagnostics.
    pub fn slab_states(&self) -> (usize, usize, usize) {
        let (mut f, mut u, mut o) = (0, 0, 0);
        for sh in self.inner.shards.iter().filter_map(|s| s.store.as_ref()) {
            let mut st = sh.lock().unwrap();
            let n = st.allocator().slab_count();
            for i in 0..n as u32 {
                match st.allocator().slab(i).map(|s| s.state) {
                    Some(crate::store::alloc::SlabState::Free) => f += 1,
                    Some(crate::store::alloc::SlabState::InUse { .. }) => u += 1,
                    _ => o += 1,
                }
            }
        }
        (f, u, o)
    }

    /// Superseded index pages queued for reclamation, summed across shards.
    pub fn index_nodes_freed(&self) -> u64 {
        self.inner
            .shards
            .iter()
            .filter_map(|s| s.store.as_ref())
            .map(|st| st.lock().unwrap().nodes_freed())
            .sum()
    }

    /// Slots currently pinned by a live Arrow `Buffer`, summed across shards.
    pub fn pinned_extents(&self) -> usize {
        self.inner
            .shards
            .iter()
            .filter_map(|s| s.store.as_ref())
            .map(|st| st.lock().unwrap().segment().pinned_count())
            .sum()
    }

    pub fn dirty_bytes(&self) -> usize {
        self.inner
            .shards
            .iter()
            .map(|s| s.mem.read().unwrap().dirty_bytes())
            .sum()
    }

    /// Persist every shard up to the current visible watermark.
    ///
    /// Returns the watermark that is now durable. A no-op for an in-memory
    /// database.
    pub fn checkpoint(&self) -> Result<Version> {
        let _checkpoint_lease = self.inner.backup_barrier.begin_checkpoint();
        let operation_id = events::next_operation_id();
        let watermark = self.inner.oracle.visible();
        let dirty_bytes = self.dirty_bytes() as u64;
        let wal_bytes = self.wal_bytes();
        #[cfg(feature = "tracing")]
        let span = tracing::info_span!(
            "yesno.db.checkpoint",
            operation_id,
            watermark,
            dirty_bytes,
            wal_bytes,
        );
        #[cfg(feature = "tracing")]
        let _entered = span.enter();
        events::emit(
            &self.inner.events,
            CoreEvent::CheckpointStarted {
                operation_id,
                watermark,
                dirty_bytes,
                wal_bytes,
            },
        );
        match self.checkpoint_inner(operation_id) {
            Ok(watermark) => {
                events::emit(
                    &self.inner.events,
                    CoreEvent::CheckpointCompleted {
                        operation_id,
                        watermark,
                    },
                );
                #[cfg(feature = "tracing")]
                tracing::info!(watermark, "checkpoint completed");
                Ok(watermark)
            }
            Err(error) => {
                let event_error = EventError::from_codec(&error);
                events::emit(
                    &self.inner.events,
                    CoreEvent::CheckpointFailed {
                        operation_id,
                        watermark,
                        error: event_error.clone(),
                    },
                );
                events::emit(
                    &self.inner.events,
                    CoreEvent::StorageOperationFailed {
                        operation_id,
                        operation: "checkpoint",
                        shard: None,
                        error: event_error,
                    },
                );
                #[cfg(feature = "tracing")]
                tracing::error!(error = %error, "checkpoint failed");
                Err(error)
            }
        }
    }

    fn checkpoint_inner(&self, operation_id: events::OperationId) -> Result<Version> {
        self.last_checkpoint.store(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            Ordering::Release,
        );
        let w = self.inner.oracle.visible();
        // Taken once, before touching any shard: the oldest version a live reader
        // can still reach. Sampling it per shard would let it advance mid-loop
        // and free an extent a reader registered in between still needs.
        let safe = self.safe_version();
        // Sampled with `safe`, before any shard lock, for the same reason.
        let reader_ckpt_floor = self.reader_ckpt_floor();

        // Test seam. Fires after `w` and `safe` are sampled and **before any
        // shard lock is taken**, which is precisely the window in which a
        // concurrently-created `Snapshot` can end up with a version above `w`
        // and a root from before this checkpoint's flip. Reclamation condition 1
        // is a test on *versions*, so such a reader passes it while still
        // reaching what this checkpoint supersedes.
        //
        // Not a general extension point: it exists so that hazard has a
        // deterministic regression rather than a threaded one. See
        // `stale-root-blocks-idle-reclamation` in `JOURNAL.md`.
        if let Some(hook) = self.inner.checkpoint_hook.lock().unwrap().clone() {
            hook();
        }

        for (shard_id, shard) in self.inner.shards.iter().enumerate() {
            let Some(store_lock) = shard.store.as_ref() else {
                continue;
            };

            // Snapshot the memtable at W. Holding the write lock keeps the set
            // of dirty chunks stable for the duration.
            let _w = shard.write.lock().unwrap();
            // A tombstone is a *value*, not an absence — see `memtable`. It has
            // to reach the carry-forward below, because a deletion is expressed
            // entirely by the chunk being missing from the rebuilt tree. Folding
            // tombstones away here ( `filter_map` over `Option<&Container>` )
            // meant a deleted chunk was simply not "touched", so the next
            // checkpoint read it back off disk and resurrected it.
            let (dirty, deleted): (Vec<DirtyChunk>, std::collections::BTreeSet<ChunkKey>) = {
                let mem = shard.mem.read().unwrap();
                let mut live = Vec::new();
                let mut gone = std::collections::BTreeSet::new();
                for (ck, c) in mem.iter_at(w) {
                    match c {
                        Some(c) => live.push(DirtyChunk {
                            key: ck,
                            container: c.clone(),
                            version: w,
                            previous: None,
                        }),
                        None => {
                            gone.insert(ck);
                        }
                    }
                }
                (live, gone)
            };

            // Two positions, deliberately. The active generation may be sealed
            // through its physical end, but recovery must resume at the first
            // record above `w`. They differ when a commit has appended and is
            // still waiting for its group fsync, hence is not visible yet.
            let (wal_end_at_snapshot, wal_replay_lsn) = shard
                .wal
                .as_ref()
                .map(|wal| {
                    let log = wal.log();
                    (log.end_lsn(), log.first_lsn_after(w))
                })
                .unwrap_or((0, 0));

            // **Held for this shard's whole checkpoint.** The store lock used to
            // provide this exclusion by being held throughout; it no longer is,
            // because the durability sequence runs with it released. Without
            // this, a second checkpoint could interleave in that window.
            let _ckpt = shard.ckpt.lock().unwrap();
            let mut store = store_lock.lock().unwrap();
            // Deletions alone are still work: the tree must be rebuilt without
            // them, even though they contribute nothing to write.
            if dirty.is_empty() && deleted.is_empty() && store.superblock().checkpoint_cv >= w {
                // **Nothing to write does not mean nothing to reclaim**, and
                // returning here is what made the space-amplification bound
                // lapse on an idle database: `enforce_space_amp` would evict the
                // oldest reader and the space would never come back, because
                // only a checkpoint runs `reclaim_deferred`.
                //
                // Reclaiming needs a superblock **flip**, not merely a call.
                // Condition 2 is the A/B rule: after checkpoint `k`, slot B
                // still names `root_{k-1}`, from which a superseded extent *is*
                // reachable — and that slot is durable, so a crash would recover
                // to it. So the flip below is not bookkeeping; it is the thing
                // that makes the extent unreachable from every durable root.
                // There is no way to return this space without writing.
                //
                // Gated on there being something queued, so a genuinely
                // quiescent database stays silent, and self-limiting: each flip
                // advances `checkpoint_seq`, two of them carry the queue past
                // `RECLAIM_CKPT_DELAY`, and then `deferred_count` reaches zero
                // and the flipping stops.
                if store.allocator().deferred_count() > 0 {
                    let mut sb = store.superblock().clone();
                    sb.seq += 1;
                    sb.checkpoint_seq += 1;
                    let seq = sb.checkpoint_seq;
                    // Prepare under the lock, sync without it, adopt under it
                    // again. See `SuperblockCommit::run`.
                    let commit = match store.prepare_superblock(sb) {
                        Ok(c) => c,
                        Err(e) => {
                            store.abandon_checkpoint();
                            return Err(e);
                        }
                    };
                    drop(store);
                    let synced = commit.run();
                    let mut store = store_lock.lock().unwrap();
                    let durable = match synced {
                        Ok(d) => d,
                        Err(e) => {
                            store.abandon_checkpoint();
                            return Err(e);
                        }
                    };
                    store.adopt_superblock(durable);
                    store.reclaim_deferred(reader_ckpt_floor, seq);
                }
                continue;
            }

            // Chunks the memtable never touched must be carried forward, or the
            // rebuilt index would lose them: the tree is rebuilt whole, not
            // patched.
            let mut all = dirty;
            let mut touched: std::collections::BTreeSet<ChunkKey> =
                all.iter().map(|d| d.key).collect();
            touched.extend(&deleted);

            let node_size = store.superblock().node_size as usize;
            let prev = store.superblock().clone();
            let ckpt_seq = prev.checkpoint_seq + 1;

            // The previous root, so unchanged leaves can be reused rather than
            // rewritten. `Tree` is two integers, so this is a copy.
            let prev_tree = store.tree();
            // Slabs sparse enough to be worth emptying. Capped per checkpoint:
            // evacuation is extra write volume, and the whole point of the
            // delta-sized checkpoint is not to reintroduce a whole-database
            // rewrite under another name.
            let evacuating: std::collections::HashSet<u32> = store
                .allocator()
                .evacuation_candidates()
                .into_iter()
                .take(self.inner.evacuate_per_checkpoint)
                .collect();

            let mut evacuated = 0u64;
            let mut carried_refs: Vec<(ChunkKey, crate::store::extent::ChunkRef)> = Vec::new();
            if let Some(t) = store.tree() {
                let carried: Vec<(ChunkKey, crate::store::extent::ChunkRef)> =
                    t.iter(&*store).collect::<Result<Vec<_>>>()?;
                // Where each key's *current* extent lives. A key being rewritten
                // or deleted supersedes its old extent, which is what makes that
                // space reclaimable — and until this map existed, `previous` was
                // always `None`, so nothing was ever queued and the deferred-free
                // path was unreachable in production.
                let mut superseded: Vec<crate::store::extent::ChunkRef> = Vec::new();
                for (ck, cref) in carried {
                    if touched.contains(&ck) {
                        superseded.push(cref);
                        continue;
                    }
                    // Untouched, but living in a slab worth emptying: rewrite it
                    // into the current generation so the old slab can be
                    // recycled whole. Setting `previous` is what queues the old
                    // slot, so the existing machinery does the rest.
                    if let Some(cell) = cref.cell() {
                        if evacuating.contains(&crate::store::alloc::slab_of(cell)) {
                            if let Some(c) = store.read_container_for(ck, cref)? {
                                evacuated += 1;
                                all.push(DirtyChunk {
                                    key: ck,
                                    container: c,
                                    version: w,
                                    previous: Some(cref),
                                });
                                continue;
                            }
                        }
                    }
                    // Untouched: keep the extent it already has. Reading and
                    // rewriting it here is what made a checkpoint cost the size
                    // of the database instead of the size of its delta.
                    carried_refs.push((ck, cref));
                }
                // Queue at `w`, not at "now": an older snapshot may still reach
                // this extent through the previous root, so it becomes free only
                // once no reader can be below the watermark *and* the A/B
                // superblock has cycled twice.
                for cref in superseded {
                    let Some(cell) = cref.cell() else { continue };
                    // A packed chunk owns no slot; its page is the unit, and it
                    // is returned only once every chunk in it is dead. Without
                    // this branch packed pages were allocated, never accounted
                    // and never freed — which is what the aged-state measurement
                    // finally caught.
                    if store.allocator().is_packed_cell(cell) {
                        // See the twin of this in `checkpoint.rs`: a run's
                        // length is a dependent read of its `nruns` prefix, and
                        // `payload_len(None).unwrap_or(0)` silently returned
                        // zero for every one of them.
                        let bytes = cref
                            .payload_len_with(|c| crate::checkpoint::run_nruns_at(&*store, c))?
                            as u32;
                        store
                            .allocator()
                            .supersede_packed_chunk(cell, bytes, ckpt_seq);
                        continue;
                    }
                    store.allocator().defer_free(cell, ckpt_seq);
                }
            }

            // The allocator stays inside the store. It used to be `mem::take`n
            // out and passed alongside, which left a default one behind for
            // `append_node` to allocate from — so index nodes were written over
            // the extents this same checkpoint had just written. See the
            // `AllocSource` docs.
            let res = checkpoint::run(
                w,
                all,
                &carried_refs,
                prev_tree,
                &mut *store,
                node_size,
                &prev,
                ckpt_seq,
                // The first record this image does not include. This may precede
                // the end being sealed when an in-flight commit is above `w`.
                wal_replay_lsn,
                // The clock floor the next open must not stamp below, once this
                // image makes the WAL carrying those stamps reclaimable.
                self.inner.oracle.commit_clock(),
            );
            // Every error path out of a checkpoint must discard the in-flight
            // node buffer. `?` here retained an owned copy of every index node
            // the failed attempt wrote, until the next successful checkpoint
            // cleared it; `abandon_checkpoint` was written for this and was
            // called from nowhere. Found by the unwired-`pub fn` sweep.
            let res = match res {
                Ok(r) => r,
                Err(e) => {
                    store.abandon_checkpoint();
                    return Err(e);
                }
            };
            let commit = match store.prepare_superblock(res.superblock) {
                Ok(c) => c,
                Err(e) => {
                    store.abandon_checkpoint();
                    return Err(e);
                }
            };
            // **The lock is released here and the three `fsync`s run without
            // it.** This is the window readers used to block in. `_ckpt` above
            // still excludes a second checkpoint; nothing else needs excluding,
            // because the live superblock does not change until `adopt` below.
            drop(store);
            let synced = commit.run();
            let mut store = store_lock.lock().unwrap();
            let durable = match synced {
                Ok(d) => d,
                Err(e) => {
                    store.abandon_checkpoint();
                    return Err(e);
                }
            };
            store.adopt_superblock(durable);

            // Only now, after the flip: an extent freed here must be unreachable
            // from *both* superblock slots, and the slot being replaced named the
            // previous root until this instant.
            //
            // `safe` is the oldest version any live reader can still see, which
            // is condition 1. Condition 3 — no live Arrow `Buffer` pointing into
            // the slot — is checked inside, because a `Buffer` is `'static` and
            // can outlive every structure that produced it.
            store.reclaim_deferred(reader_ckpt_floor, ckpt_seq);
            drop(store);

            // The store now holds every chunk at or below `w`, so memtable
            // entries reduced to a single durable version are redundant — and
            // keeping them is what made every chunk stay dirty forever, so that
            // every checkpoint rewrote the whole database.
            //
            // The floor is the lower of `w` and `safe`: above `w` the store does
            // not have it yet, and above `safe` a live reader may still need a
            // version older than the one being dropped.
            // Not `safe` — see `evict_floor`. A live reader's pinned root can
            // predate its version, and only the root bounds what the store can
            // answer on its behalf.
            self.evacuated.fetch_add(evacuated, Ordering::Relaxed);
            // The log below the checkpoint watermark is now redundant. Seal the
            // active generation at the LSN captured with the memtable snapshot,
            // then remove only whole generations no follower still needs.
            if let Some(wl) = shard.wal.as_ref() {
                // A follower may still need what this is about to remove.
                //
                // The floor is **consumed**, not peeked: a window in which no
                // follower acked yields `None` and the cut proceeds, so a
                // follower that dies stops holding the log at the next
                // checkpoint rather than until the process restarts. And it is
                // overridden past `max_wal_bytes`, because a follower that keeps
                // acking while falling further behind would otherwise fill the
                // disk — the same call the design makes for snapshot space.
                //
                let retention_floor = self.inner.retention.take(shard_id as u32);
                let bytes_before = wl.bytes();
                let held = retention_floor.is_some_and(|f| {
                    f < wal_replay_lsn && bytes_before < self.inner.policy.max_wal_bytes
                });
                let reclaim_through = if held {
                    retention_floor.unwrap_or(0)
                } else {
                    wal_replay_lsn
                };
                let rolled =
                    wl.rotate_and_reclaim_if_quiet(wal_end_at_snapshot, reclaim_through)?;
                if let Some(rolled) = rolled {
                    events::emit(
                        &self.inner.events,
                        CoreEvent::WalGenerationRotated {
                            operation_id,
                            shard: shard_id as u32,
                            old_base_lsn: rolled.old_base_lsn,
                            new_base_lsn: rolled.new_base_lsn,
                            sealed_through_lsn: wal_end_at_snapshot,
                            bytes_reclaimed: rolled.bytes_reclaimed,
                            forced_past_retention: !held
                                && retention_floor.is_some_and(|floor| floor < wal_replay_lsn),
                        },
                    );
                }
                if held {
                    events::emit(
                        &self.inner.events,
                        CoreEvent::WalReclamationDeferred {
                            operation_id,
                            shard: shard_id as u32,
                            end_lsn: wal_replay_lsn,
                            retention_floor_lsn: retention_floor.unwrap_or(0),
                        },
                    );
                }
            }

            let floor = w.min(self.evict_floor());
            // Both calls below destroy history, and `read_floor` is what stops a
            // later read from being answered out of what survives.
            //
            // `prune( safe )` keeps only the newest version at or below `safe`,
            // so a read *at* `safe` is still exact and a read below it is not.
            // `evict_durable( floor )` drops chains the store already holds,
            // after which a read below `floor` falls through to a root that
            // carries the checkpoint watermark's state rather than its own.
            // The higher of the two is therefore the oldest version still
            // reconstructible, and `fetch_max` because concurrent checkpoints
            // must not let it go backwards.
            self.inner
                .read_floor
                .fetch_max(safe.max(floor), Ordering::AcqRel);
            {
                let mut mem = shard.mem.write().unwrap();
                // Collapse history first. `evict_durable` only drops a chain
                // reduced to a single durable version, and without this nothing
                // ever reduces one: a key that was deleted and rewritten keeps
                // `[value, tombstone]` for ever, so it is never evicted, stays
                // permanently dirty, and is rewritten on **every** checkpoint.
                // `prune` was public and called from tests only.
                mem.prune(safe);
                mem.evict_durable(floor);
            }
        }
        Ok(w)
    }

    /// Drop memtable versions no live reader can reach.
    pub fn prune(&self) -> usize {
        let floor = self.safe_version();
        // Same reasoning as in `checkpoint`: this collapses chains, so it moves
        // the oldest version `snapshot_at` can still reconstruct. Easy to
        // miss because this is the *other* caller of `Memtable::prune` — the
        // floor is a property of the pruning, not of the checkpoint.
        self.inner.read_floor.fetch_max(floor, Ordering::AcqRel);
        self.inner
            .shards
            .iter()
            .map(|s| s.mem.write().unwrap().prune(floor))
            .sum()
    }
}

impl Default for Db {
    fn default() -> Self {
        Self::new()
    }
}

/// One pending mutation.
#[derive(Clone, Debug)]
enum Op {
    Insert(u64, u64),
    Remove(u64, u64),
    /// `key, lo, hi` inclusive. Kept whole rather than expanded into ordinals:
    /// it is one `SetRange` record and one container call per chunk, and
    /// expanding it here would discard both.
    InsertRange(u64, u64, u64),
    RemoveRange(u64, u64, u64),
    PutChunk(u64, Prefix48, crate::Container),
    DeleteKey(u64),
}

/// Round a record's on-disk footprint up to the 8-byte record alignment.
#[inline]
fn rec_cost(body_len: usize) -> usize {
    (record::HEADER + body_len).div_ceil(record::ALIGN) * record::ALIGN
}

/// A contiguous run of ordinals emitted as one `SetRange` is cheaper than
/// folding it into a `ChunkDelta` once `rec_cost(25) < 2 * len`.
const SETRANGE_MIN_RUN: u64 = 33;

/// `ChunkDelta` counts its value arrays in `u16`, so a body may not carry more
/// than 65535. Split well below that rather than trusting the argument that a
/// chunk cannot hold more — a later change to run-splitting must not be able to
/// turn this into a silent truncation.
const MAX_DELTA_VALUES: usize = 32768;

/// One commit's ops, regrouped into the shapes both the memtable and the WAL
/// want to work in.
///
/// # Why there is one plan and not two
///
/// The WAL wanted a batch's ordinals grouped into ranges and per-chunk value
/// lists so it could write `SetRange` and `ChunkDelta` instead of a record per
/// ordinal. The memtable wants **exactly the same grouping**, for the same
/// reason: applying a span costs one container call per chunk, where applying
/// it ordinal by ordinal costs a `ChunkKey` lookup, a version-chain walk and a
/// copy-on-write clone each. Computing the grouping twice would let the two
/// drift; computing it once and letting both consume it cannot.
#[derive(Debug)]
enum Planned {
    /// `[lo, hi]` inclusive under one key.
    Range {
        key: u64,
        lo: u64,
        hi: u64,
        remove: bool,
    },
    /// Scattered values inside a single chunk.
    Values {
        key: u64,
        prefix: Prefix48,
        vals: Vec<u16>,
        remove: bool,
    },
    /// Whole-chunk and whole-key ops, which are already their own shape.
    Whole(Op),
}

impl Planned {
    /// The log record for this planned unit.
    fn to_record(&self) -> (RecType, Vec<u8>) {
        match self {
            Planned::Range {
                key,
                lo,
                hi,
                remove,
            } => (
                RecType::SetRange,
                record::encode_set_range(*key, *lo, *hi, *remove),
            ),
            Planned::Values {
                key,
                prefix,
                vals,
                remove,
            } => {
                let (add, rem): (&[u16], &[u16]) = if *remove {
                    (&[], vals.as_slice())
                } else {
                    (vals.as_slice(), &[])
                };
                (
                    RecType::ChunkDelta,
                    record::encode_chunk_delta(*key, *prefix, add, rem),
                )
            }
            Planned::Whole(op) => op.to_record(),
        }
    }
}

/// Regroup one commit's ops for a single shard.
///
/// # The reordering rule
///
/// Only **consecutive** ops of the same key and the same direction are merged.
/// A batch may legally contain `insert(k, 5)` then `remove(k, 5)`, and sorting
/// those together would invert the outcome. Any change of key, of direction, or
/// any range / `PutChunk` / `DeleteKey` flushes the pending group, so the plan
/// preserves the batch's order exactly. Within a group every op is the same
/// operation on one key, so sorting is free of consequence.
///
/// # Choosing the shape per chunk
///
/// Contiguous runs that cross a chunk boundary or reach `SETRANGE_MIN_RUN`
/// become a `Range`, which describes any span in one record and one container
/// call per chunk. What is left is scattered inside single chunks and becomes
/// one `Values` per chunk, costing `rec_cost(24 + 2n)` against `n` runs' worth
/// of `SetRange` — so the cheaper of the two is chosen on the actual counts
/// rather than assumed.
fn plan_ops(ops: &[&Op]) -> Vec<Planned> {
    let mut out: Vec<Planned> = Vec::new();
    let mut pending: Vec<u64> = Vec::new();
    let mut group: Option<(u64, bool)> = None; // (key, is_remove)

    fn flush(key: u64, remove: bool, ords: &mut Vec<u64>, out: &mut Vec<Planned>) {
        if ords.is_empty() {
            return;
        }
        ords.sort_unstable();
        ords.dedup();

        // Maximal contiguous runs.
        let mut runs: Vec<(u64, u64)> = Vec::new();
        let mut it = ords.iter().copied();
        let first = it.next().expect("non-empty");
        let (mut lo, mut hi) = (first, first);
        for o in it {
            if o == hi + 1 {
                hi = o;
            } else {
                runs.push((lo, hi));
                (lo, hi) = (o, o);
            }
        }
        runs.push((lo, hi));

        // Long or chunk-crossing runs are always cheapest as a range.
        let mut scattered: std::collections::BTreeMap<u64, Vec<(u64, u64)>> = Default::default();
        for (lo, hi) in runs {
            let crosses = crate::split(lo).0 != crate::split(hi).0;
            if crosses || hi - lo + 1 >= SETRANGE_MIN_RUN {
                out.push(Planned::Range {
                    key,
                    lo,
                    hi,
                    remove,
                });
            } else {
                scattered
                    .entry(crate::split(lo).0)
                    .or_default()
                    .push((lo, hi));
            }
        }

        for (prefix, runs) in scattered {
            let count: usize = runs.iter().map(|(l, h)| (h - l + 1) as usize).sum();
            let as_ranges = runs.len() * rec_cost(25);
            let as_delta = rec_cost(24 + 2 * count);
            if as_delta >= as_ranges {
                for (lo, hi) in runs {
                    out.push(Planned::Range {
                        key,
                        lo,
                        hi,
                        remove,
                    });
                }
                continue;
            }
            let vals: Vec<u16> = runs
                .iter()
                .flat_map(|&(l, h)| (l..=h).map(|o| crate::split(o).1))
                .collect();
            for part in vals.chunks(MAX_DELTA_VALUES) {
                out.push(Planned::Values {
                    key,
                    prefix,
                    vals: part.to_vec(),
                    remove,
                });
            }
        }
        ords.clear();
    }

    for op in ops {
        match op {
            Op::Insert(k, o) | Op::Remove(k, o) => {
                let remove = matches!(op, Op::Remove(..));
                if group != Some((*k, remove)) {
                    if let Some((gk, gr)) = group {
                        flush(gk, gr, &mut pending, &mut out);
                    }
                    group = Some((*k, remove));
                }
                pending.push(*o);
            }
            other => {
                if let Some((gk, gr)) = group.take() {
                    flush(gk, gr, &mut pending, &mut out);
                }
                match other {
                    Op::InsertRange(k, lo, hi) => out.push(Planned::Range {
                        key: *k,
                        lo: *lo,
                        hi: *hi,
                        remove: false,
                    }),
                    Op::RemoveRange(k, lo, hi) => out.push(Planned::Range {
                        key: *k,
                        lo: *lo,
                        hi: *hi,
                        remove: true,
                    }),
                    _ => out.push(Planned::Whole((*other).clone())),
                }
            }
        }
    }
    if let Some((gk, gr)) = group {
        flush(gk, gr, &mut pending, &mut out);
    }
    out
}

impl Op {
    /// The log record for this operation.
    ///
    /// `SetRange` carries a single ordinal as a degenerate range, which is 32
    /// bytes for one insert — wasteful per-op, but it is the record type that
    /// makes a bulk range affordable, and collapsing runs of adjacent inserts
    /// into one belongs in the batch rather than here.
    ///
    /// `PutChunk` logs as **the chunk's ordinals**, with no container image:
    /// replay has no container decoder wired to it, and a whole-image record
    /// would be a second encoding path to keep in step with `codec`. It costs
    /// bytes on a rare operation and keeps replay to one shape.
    ///
    /// **This comment used to say "a delete plus the chunk's ordinals", and
    /// there is no delete here.** The distinction is the whole of why
    /// [`WriteBatch::merge_set`] does not reuse this op: a bare image record
    /// *unions* on replay while `Memtable::put_chunk` *replaces* live, and only
    /// the `DeleteKey` that [`WriteBatch::store_set`] emits separately keeps the
    /// two in agreement. See `db/apply.rs` at `RecType::ChunkImage`.
    fn to_record(&self) -> (RecType, Vec<u8>) {
        match self {
            Op::Insert(k, o) => (
                RecType::SetRange,
                record::encode_set_range(*k, *o, *o, false),
            ),
            Op::Remove(k, o) => (
                RecType::SetRange,
                record::encode_set_range(*k, *o, *o, true),
            ),
            Op::InsertRange(k, lo, hi) => (
                RecType::SetRange,
                record::encode_set_range(*k, *lo, *hi, false),
            ),
            Op::RemoveRange(k, lo, hi) => (
                RecType::SetRange,
                record::encode_set_range(*k, *lo, *hi, true),
            ),
            Op::DeleteKey(k) => (RecType::ChunkDelete, k.to_le_bytes().to_vec()),
            Op::PutChunk(k, prefix, c) => {
                let base = prefix << crate::CHUNK_BITS;
                let mut body = Vec::new();
                body.extend_from_slice(&k.to_le_bytes());
                for v in c.iter() {
                    body.extend_from_slice(&(base | v as u64).to_le_bytes());
                }
                (RecType::ChunkImage, body)
            }
        }
    }

    #[inline]
    fn key(&self) -> u64 {
        match self {
            Op::Insert(k, _)
            | Op::Remove(k, _)
            | Op::InsertRange(k, _, _)
            | Op::RemoveRange(k, _, _)
            | Op::PutChunk(k, _, _)
            | Op::DeleteKey(k) => *k,
        }
    }
}

/// Outcome of a commit.
#[derive(Debug, Clone, Copy)]
pub struct Committed {
    pub version: Version,
    /// Ordinals that actually changed state.
    pub changed: u64,
    pub shards: usize,
}

/// A batch of mutations, applied atomically.
pub struct WriteBatch {
    /// The whole handle, not just `DbInner`, so `commit` can enforce the
    /// checkpoint policy. Routing the check through `Db::insert` instead would
    /// leave `db.batch().commit()` — the documented path — unbounded.
    handle: Db,
    ops: Vec<Op>,
    /// First out-of-range ordinal seen while recording, surfaced by `commit`.
    ///
    /// The builder methods return `&mut Self` so they cannot report an error
    /// themselves, and deferring the check to `commit` alone is not enough:
    /// `Db::insert_many` folds contiguous runs with `sorted[j] + 1`, which
    /// overflows on `u64::MAX` in a debug build long before `commit` runs. So
    /// the check happens where the value is accepted, and the error is carried.
    err: Option<CodecError>,
    /// Are the recorded keys non-descending?
    ///
    /// Maintained in [`WriteBatch::push_op`] at `O( 1 )` per operation so that
    /// `commit` can skip both the sort **and** the scan that would detect a
    /// sorted batch. Measured: the scan alone cost key-major commit ~12% at
    /// 2 M operations, which is a real charge on the shape that was already
    /// fast, and the batch already knows the answer as the keys arrive.
    ///
    /// A **global** flag is sound for the per-shard sort because any
    /// subsequence of a non-descending sequence is non-descending, so
    /// `true` here implies every shard's slice is ordered too. It is only ever
    /// a *sufficient* condition: a `false` costs a sort that might have been
    /// unnecessary, never a missed one.
    keys_ascending: bool,
    /// Last key recorded, for the comparison above.
    last_key: Option<u64>,
}

impl WriteBatch {
    /// The only way an operation enters the batch.
    ///
    /// Every recording method routes through here so `keys_ascending` cannot go
    /// stale by someone adding a `self.ops.push` elsewhere. A missed site would
    /// leave the flag wrongly `true` and silently lose the commit-ordering
    /// optimization without failing a test, which is why there is one door.
    #[inline]
    fn push_op(&mut self, op: Op) {
        let k = op.key();
        if self.last_key.is_some_and(|prev| k < prev) {
            self.keys_ascending = false;
        }
        self.last_key = Some(k);
        self.ops.push(op);
    }

    /// Add every ordinal in `[lo, hi]` inclusive under `key`.
    ///
    /// One `SetRange` record and one container call per chunk, whatever the
    /// span. Inserting the same span ordinal by ordinal costs a WAL record and
    /// a copy-on-write clone **each**, which is what this exists to avoid.
    pub fn insert_range(&mut self, key: u64, lo: u64, hi: u64) -> &mut Self {
        // `hi` is *inclusive* here, so its ceiling is `ORDINAL_MAX` — whereas the
        // exclusive `hi` of `not_in_range` tops out at `u64::MAX`. Under I8 those
        // two ceilings denote the same last ordinal, which is the point of
        // reserving `u64::MAX`.
        if !self.reject_out_of_range(lo) || !self.reject_out_of_range(hi) {
            return self;
        }
        if lo <= hi {
            self.push_op(Op::InsertRange(key, lo, hi));
        }
        self
    }

    /// Drop every ordinal in `[lo, hi]` inclusive under `key`.
    pub fn remove_range(&mut self, key: u64, lo: u64, hi: u64) -> &mut Self {
        if !self.reject_out_of_range(lo) || !self.reject_out_of_range(hi) {
            return self;
        }
        if lo <= hi {
            self.push_op(Op::RemoveRange(key, lo, hi));
        }
        self
    }

    pub fn insert(&mut self, key: u64, ordinal: u64) -> &mut Self {
        if !self.reject_out_of_range(ordinal) {
            return self;
        }
        self.push_op(Op::Insert(key, ordinal));
        self
    }

    /// Removing an out-of-range ordinal is rejected rather than treated as the
    /// no-op it would be. I8 makes `u64::MAX` *not an ordinal*, so naming one is
    /// a caller bug in either direction, and accepting it on the read-ish path
    /// while rejecting it on the write path would make the contract conditional
    /// on which method you reached for.
    pub fn remove(&mut self, key: u64, ordinal: u64) -> &mut Self {
        if !self.reject_out_of_range(ordinal) {
            return self;
        }
        self.push_op(Op::Remove(key, ordinal));
        self
    }

    /// Store a whole set under `key`, replacing whatever is there.
    pub fn store_set(&mut self, key: u64, set: &OrdSet) -> &mut Self {
        // An `OrdSet` built through the infallible in-memory API could carry an
        // out-of-range ordinal ( I8 is a `debug_assert` there ). This is the
        // boundary where such a set would become durable, so it is checked.
        if let Some(max) = set.max() {
            if !self.reject_out_of_range(max) {
                return self;
            }
        }
        self.push_op(Op::DeleteKey(key));
        for (prefix, c) in set.chunks() {
            self.push_op(Op::PutChunk(key, prefix, c.clone()));
        }
        self
    }

    /// Union a whole set into `key`, keeping whatever is already there.
    ///
    /// The middle ground `WriteBatch` was missing: [`Self::store_set`] replaces
    /// a key wholesale and [`Self::insert`] adds one ordinal, so bulk ingest
    /// into an existing key was a read-modify-write — `load`, union, `store_set`
    /// — performed *outside* the batch, and therefore neither atomic with it nor
    /// safe against a concurrent writer.
    ///
    /// # Why this does not reuse `Op::PutChunk`
    ///
    /// `store_set` emits `DeleteKey` and then one `PutChunk` per chunk, so
    /// dropping the `DeleteKey` looks like exactly the union wanted here. **It
    /// is not, and the reason is a divergence worth knowing about.** `PutChunk`
    /// applies to the memtable through `Memtable::put_chunk`, which *replaces*
    /// the chunk's MVCC value; it logs as a bare `RecType::ChunkImage`, which
    /// replay applies with a per-ordinal `insert` and therefore *unions*. The
    /// two agree today only because `store_set`'s leading `DeleteKey` empties
    /// the key first, making replace and union the same operation. A `PutChunk`
    /// without that delete would commit one thing and replay another.
    ///
    /// So this lowers to the ordinary insert path instead, where the live apply
    /// and the replay apply are the same operation by construction.
    ///
    /// # Cost
    ///
    /// Runs are folded exactly as [`Db::insert_many`] folds them, so a
    /// contiguous span costs one 32-byte `SetRange` whatever its width, and
    /// what is left is scattered values coalesced per chunk by `plan_ops`. An
    /// `OrdSet` arrives sorted and deduplicated, so unlike `insert_many` this
    /// needs no intermediate copy of the ordinals.
    ///
    /// **The cost is driven by run count, not cardinality**, and the two differ
    /// by a lot in both directions. A dense set is a handful of long runs and
    /// costs almost nothing; a set of every *other* ordinal has one run per
    /// element and stages one `Op` per ordinal before `plan_ops` coalesces
    /// them, so a large scattered set is held in the batch at roughly 24 bytes
    /// an ordinal until `commit`. That is the same shape `insert_many` has —
    /// it is inherent to staging a batch rather than something this adds — but
    /// it is the case to watch, because the input here is a set rather than a
    /// slice the caller already sized.
    pub fn merge_set(&mut self, key: u64, set: &OrdSet) -> &mut Self {
        // Same boundary check as `store_set`, for the same reason: an `OrdSet`
        // built through the infallible in-memory API could carry an
        // out-of-range ordinal, and this is where it would become durable.
        // Checked before the fold, which computes `hi - lo + 1`.
        if let Some(max) = set.max() {
            if !self.reject_out_of_range(max) {
                return self;
            }
        }

        // Maximal runs, streamed. `set.iter()` is ascending and deduplicated,
        // which is what lets the fold be a single pass with no buffer.
        let mut run: Option<(u64, u64)> = None;
        for o in set.iter() {
            match run {
                Some((lo, hi)) if o == hi + 1 => run = Some((lo, o)),
                Some((lo, hi)) => {
                    self.push_run(key, lo, hi);
                    run = Some((o, o));
                }
                None => run = Some((o, o)),
            }
        }
        if let Some((lo, hi)) = run {
            self.push_run(key, lo, hi);
        }
        self
    }

    /// One maximal run, as the cheaper of a range record or scattered inserts.
    ///
    /// The same rule `Db::insert_many` applies: a run that crosses a chunk
    /// boundary or reaches `SETRANGE_MIN_RUN` is worth a `SetRange`, and a short
    /// one inside a single chunk is cheaper as values `plan_ops` will coalesce.
    fn push_run(&mut self, key: u64, lo: u64, hi: u64) {
        let crosses = crate::split(lo).0 != crate::split(hi).0;
        if crosses || hi - lo + 1 >= SETRANGE_MIN_RUN {
            self.push_op(Op::InsertRange(key, lo, hi));
        } else {
            for o in lo..=hi {
                self.push_op(Op::Insert(key, o));
            }
        }
    }

    /// Records the first out-of-range ordinal and reports whether to proceed.
    fn reject_out_of_range(&mut self, ordinal: u64) -> bool {
        if crate::is_valid_ordinal(ordinal) {
            return true;
        }
        self.err
            .get_or_insert(CodecError::OrdinalOutOfRange { ordinal });
        false
    }

    pub fn delete_key(&mut self, key: u64) -> &mut Self {
        self.push_op(Op::DeleteKey(key));
        self
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// Apply the batch atomically.
    pub fn commit(self) -> Result<Committed> {
        #[cfg(feature = "tracing")]
        let span = tracing::debug_span!(
            "yesno.db.commit",
            operations = self.ops.len(),
            durable = self.handle.inner.dir.is_some(),
        );
        #[cfg(feature = "tracing")]
        let _entered = span.enter();
        // **The single choke point, and that is why the check is here.**
        // Every mutator in this crate — `insert`, `remove`, `insert_range`,
        // `remove_range`, `insert_many`, and every `WriteBatch` method — funnels
        // through this one call, so one guard covers the whole write surface,
        // present and future. A mutator added later that did *not* pass through
        // here would be writing to the WAL by some other route, which is a much
        // larger review event than "did you remember the replica check".
        // Both modes, and a reader is the stricter of the two: a replica at
        // least owns its directory and may checkpoint, while a reader owns
        // nothing and must not write a byte.
        if self.handle.inner.replica || self.handle.inner.read_only {
            return Err(CodecError::ReadOnlyReplica);
        }
        // I8: nothing out of range is ever written, so no ordinal beyond
        // `ORDINAL_MAX` can reach the WAL, the memtable, or a container.
        if let Some(e) = self.err {
            return Err(e);
        }
        if self.ops.is_empty() {
            return Ok(Committed {
                version: self.handle.inner.oracle.visible(),
                changed: 0,
                shards: 0,
            });
        }
        // Group by physical shard, through the persisted map. Not
        // `vshard_of(key) % shards.len()`: this is the *write* half of the same
        // routing function `Snapshot::shard_index` reads through, and the two
        // disagreeing means a key is written to one shard and looked for in
        // another. They agree for a freshly created map, so nothing observable
        // separates them until the map is permuted — which is exactly what a
        // shard-count change would do.
        let mut by_shard: std::collections::BTreeMap<usize, Vec<&Op>> = Default::default();
        for op in &self.ops {
            let s = self.handle.inner.route.shard_of_vshard(vshard_of(op.key()));
            by_shard.entry(s).or_default().push(op);
        }
        // **Stable sort by key, so same-key operations are adjacent.**
        //
        // `plan_ops` coalesces a *consecutive* run of same-key ops into one
        // group and flushes whenever the key changes. Document-major input --
        // touch every key, then move to the next document, which is the natural
        // shape for ingesting records -- changes key on every op, so it flushed
        // once per op and staged one `Planned::Values` carrying a single
        // ordinal, with a `BTreeMap` allocation each. Measured on 256 keys:
        //
        //     inserts     key-major commit   document-major commit
        //      524 288             25.2 ms                369.3 ms   14.7x
        //    2 097 152             78.8 ms               1746.4 ms   22.2x
        //
        // Build time was identical in every run, so this was commit alone, and
        // the ratio **grows** with size: key-major commit is linear in the
        // operation count while document-major was not.
        //
        // **Sorting is unconditionally safe here, which is a property of `Op`
        // rather than of the caller.** Every variant is scoped to exactly one
        // key -- `Op::key()` matches all six exhaustively, with no wildcard, so
        // a future cross-key variant fails to compile rather than silently
        // breaking this -- therefore operations on different keys commute. The
        // sort is **stable**, so operations on the *same* key keep their
        // relative order, which is what `insert` then `remove` of one ordinal
        // depends on.
        //
        // **Sortedness is tracked as operations arrive, not detected here**, and
        // the difference was measured rather than assumed. An unconditional sort
        // cost key-major commit +18% at 2 M operations and +44% at 524 288 -- a
        // real charge on the shape that was already fast. Replacing it with an
        // `is_sorted_by_key` scan still cost **~12%** ( mean 80.4 -> 90.3 ms at
        // 2 M, ranges 76.0-86.4 against 84.8-93.8, so outside the run-to-run
        // spread ). `WriteBatch::push_op` maintains the flag at `O( 1 )` per
        // operation, so a key-major batch now pays nothing at all. The sort,
        // when it runs, moves `&Op` rather than operations.
        //
        // **Sorted through a `( key, index )` pair array, not by comparing
        // `&Op` directly.** `sort_by_key( |op| op.key() )` derefs a pointer per
        // comparison; the pairs are contiguous, so the comparison touches only
        // the array being sorted. Measured at 8 388 608 operations, one shard,
        // three runs per arm: document-major commit **693.5-700.9 ms** by
        // pointer against **591.1-606.7 ms** by pair -- disjoint ranges, ~14%.
        // It is better at every size measured ( 131 072 through 8.4 M ), so
        // there is no small-batch case paying for the extra array.
        //
        // **Stability is structural here rather than a property of the
        // algorithm.** Sorting `( key, original index )` lexicographically
        // orders by key and breaks ties by arrival, which *is* a stable sort by
        // key -- so `sort_unstable` is correct and the guarantee cannot be lost
        // by someone later swapping the sort for a faster one. Dropping the
        // index from the comparison reddens
        // `interleaving_order_does_not_change_what_a_batch_commits`.
        if !self.keys_ascending {
            for ops in by_shard.values_mut() {
                let mut idx: Vec<(u64, u32)> = ops
                    .iter()
                    .enumerate()
                    .map(|(i, op)| (op.key(), i as u32))
                    .collect();
                idx.sort_unstable();
                *ops = idx.iter().map(|&(_, i)| ops[i as usize]).collect();
            }
        }
        let participants: Vec<usize> = by_shard.keys().copied().collect();
        // One grouping, consumed by both the memtable apply below and the WAL
        // encoding further down. See `plan_ops`.
        let planned: std::collections::BTreeMap<usize, Vec<Planned>> = by_shard
            .iter()
            .map(|(&s, ops)| (s, plan_ops(ops)))
            .collect();

        // Ascending shard order: uniform ordering is what prevents deadlock
        // between concurrent multi-shard batches.
        let mut guards = Vec::with_capacity(participants.len());
        for &s in &participants {
            guards.push(self.handle.inner.shards[s].write.lock().unwrap());
        }

        // Late assignment, inside the locks. This is what gives I5.
        //
        // The commit time comes back from the *same* call, once, and is
        // stamped on every participant's `ShardCommit` below. Do not read the
        // clock inside the per-shard loop: participants of one commit would then
        // disagree, which recovery refuses outright ( I9 ), and a four-shard
        // commit would look like four events at four instants.
        let (version, commit_time) = self
            .handle
            .inner
            .oracle
            .begin(participants.len() as u32)
            .ok_or(CodecError::Invariant("commit ring is full"))?;

        let mut changed = 0u64;
        for &s in &participants {
            let mut mem = self.handle.inner.shards[s].mem.write().unwrap();
            for unit in &planned[&s] {
                let shard = &self.handle.inner.shards[s];
                let op = match unit {
                    Planned::Range {
                        key,
                        lo,
                        hi,
                        remove,
                    } => {
                        changed += if *remove {
                            mem.remove_range(*key, *lo, *hi, version, |p| shard.disk_chunk(*key, p))
                        } else {
                            mem.insert_range(*key, *lo, *hi, version, |p| shard.disk_chunk(*key, p))
                        };
                        continue;
                    }
                    Planned::Values {
                        key,
                        prefix,
                        vals,
                        remove,
                    } => {
                        changed += mem.apply_values(*key, *prefix, vals, *remove, version, || {
                            shard.disk_chunk(*key, *prefix)
                        });
                        continue;
                    }
                    Planned::Whole(op) => op,
                };
                match op {
                    Op::PutChunk(k, p, c) => {
                        changed += c.len() as u64;
                        mem.put_chunk(*k, *p, c.clone(), version);
                    }
                    Op::DeleteKey(k) => {
                        // On-disk chunks need tombstones of their own; without
                        // them the persisted value shows through the delete.
                        let on_disk = self.handle.inner.shards[s]
                            .store
                            .as_ref()
                            .map(|st| {
                                let st = st.lock().unwrap();
                                st.key_prefixes(*k).unwrap_or_default()
                            })
                            .unwrap_or_default();
                        mem.delete_key(*k, version, on_disk);
                    }
                    other => unreachable!("{other:?} is planned, not passed through"),
                }
            }
        }

        // Everything from here to `shard_durable` must resolve `version`, one way
        // or the other. A version that is assigned and then abandoned leaves its
        // ring slot `Pending` for ever: the visible watermark is a *prefix* rule,
        // so it stalls at the hole, no later commit can become visible, and the
        // next recovery discards every acknowledged commit above it. That is why
        // the design calls the abort path mandatory rather than defensive.
        let logged = (|| -> Result<()> {
            // Log before releasing the locks, so the record order within a shard
            // matches the version order ( I5 ). Nothing is durable yet — `append`
            // writes, `sync` is what makes it so, and that happens after the locks
            // are dropped.
            // Per shard, the LSN this commit's records end at. Durability is
            // then "get `synced` past this", which any thread may accomplish —
            // usually a concurrent committer's leader fsync, not this one.
            // Stamped on every record this leadership writes. Nothing reads
            // it back yet — recovery and the follower's apply path both ignore
            // it — but a log that records *which* leadership wrote each record
            // is what a divergence check needs, and adding the field later would
            // have been a format break. See `wal::Record`'s header.
            let term = self.handle.term64();
            let mut targets: Vec<(usize, u64)> = Vec::with_capacity(participants.len());
            for &s in &participants {
                let shard = &self.handle.inner.shards[s];
                let Some(wal) = shard.wal.as_ref() else {
                    continue;
                };
                let target = wal.append_with(|w| {
                    if participants.len() > 1 {
                        // Written to every participant, so each shard's stream is
                        // independently interpretable by a follower.
                        let ids: Vec<u32> = participants.iter().map(|p| *p as u32).collect();
                        w.append(
                            RecType::CommitIntent,
                            version,
                            term,
                            record::encode_commit_intent(&ids),
                        )?;
                    }
                    for unit in &planned[&s] {
                        let (rtype, body) = unit.to_record();
                        w.append(rtype, version, term, body)?;
                    }
                    // The marker recovery resolves a version against, and the
                    // only record carrying when the commit happened.
                    w.append_marker(RecType::ShardCommit, version, term, commit_time)?;
                    Ok(())
                })?;
                targets.push((s, target));
            }

            // Release before the durability wait, so a slow fsync does not hold the
            // shard lock. Safe because readers snapshot `visible`, not `next`.
            drop(guards);

            // Now make it durable, outside the locks — which is the whole reason
            // `append` and `sync` are separate. This often costs nothing: a
            // concurrent committer's leader fsync has already covered us.
            for &s in &participants {
                let shard = &self.handle.inner.shards[s];
                if let Some(wal) = shard.wal.as_ref() {
                    let target = targets
                        .iter()
                        .find(|(t, _)| *t == s)
                        .map(|(_, l)| *l)
                        .unwrap_or(0);
                    wal.sync_through(target)?;
                }
                self.handle.inner.oracle.shard_durable(version);
            }
            Ok(())
        })();

        if let Err(e) = logged {
            self.handle.abort_version(version, &participants);
            let operation_id = events::next_operation_id();
            events::emit(
                &self.handle.inner.events,
                CoreEvent::StorageOperationFailed {
                    operation_id,
                    operation: "wal_commit",
                    shard: None,
                    error: EventError::from_codec(&e),
                },
            );
            #[cfg(feature = "tracing")]
            tracing::warn!(error = %e, version, "WAL commit failed");
            return Err(e);
        }

        // Enforce the checkpoint policy before returning. A writer that never
        // checkpoints must not be able to grow the memtable without bound, and
        // this is the only place every write passes through.
        self.handle.enforce_policy()?;

        let committed = Committed {
            version,
            changed,
            shards: participants.len(),
        };
        #[cfg(feature = "tracing")]
        tracing::debug!(
            version = committed.version,
            changed = committed.changed,
            shards = committed.shards,
            "commit completed"
        );
        Ok(committed)
    }

    /// Discard without applying. No version is consumed, so nothing to abort.
    pub fn rollback(self) {}
}

/// The registry slot a snapshot occupies, released when the last clone drops.
///
/// Separate from `Snapshot` so that cloning one is refcounting rather than
/// duplicating a slot index. A `Clone` type whose `Drop` released a *shared*
/// slot would free it the first time any clone died, leaving the survivors
/// unregistered — and an unregistered reader makes `safe_version` a lie and
/// permits reclaiming data it is still reading.
struct ReaderSlot {
    db: Arc<DbInner>,
    slot: usize,
}

impl Drop for ReaderSlot {
    fn drop(&mut self) {
        // Clear the pinned-root watermark **before** releasing the slot.
        //
        // `evict_floor` reads `reader_roots[slot]` for any slot whose
        // `readers[slot]` is not `FREE`, and the next `snapshot` publishes
        // those two in the other order — it claims the slot by CAS, then
        // stores the watermark. Between those, `evict_floor` reads whatever the
        // previous occupant left. Zeroing here makes that window read 0, which
        // means "keep everything" and costs one checkpoint's eviction at worst.
        //
        // It happens to be harmless today without this, because
        // `root_watermark` only moves forward so a stale value is always *low*
        // — conservative. That is an accident of monotonicity, not a property
        // anyone stated, and a reader losing data is what it would cost to be
        // wrong about it.
        self.db.reader_roots[self.slot].store(0, Ordering::Release);
        // Cleared before the slot is released, for the same reason the root is:
        // the next occupant must not inherit an eviction it never suffered.
        self.db.reader_evicted[self.slot].store(false, Ordering::Release);
        // `0` is the conservative reading — "assume the oldest root" — which is
        // what a slot must read as between being claimed and publishing its
        // real sequence. Cleared before the slot is released, like the root.
        self.db.reader_ckpt_seqs[self.slot].store(0, Ordering::Release);
        self.db.readers[self.slot].store(FREE, Ordering::Release);
    }
}

/// A consistent read view: `Clone + Send + Sync + 'static`.
///
/// Cloning is refcounting the registry slot, not taking a second one, so the
/// version stays pinned until every clone is gone. That matters for a query
/// engine, where a plan node owns a snapshot and may be executed more than once,
/// and for anything that hands a `Buffer` onward — the data has to stay
/// unreclaimable for as long as *any* holder can reach it.
#[derive(Clone)]
pub struct Snapshot {
    db: Arc<DbInner>,
    version: Version,
    /// Dropped when the last clone goes, which is what releases the slot.
    _slot: Arc<ReaderSlot>,
    /// Per-shard index root as of snapshot creation.
    roots: Vec<Option<Tree>>,
}

/// One end of a container, chosen by the same flag the caller used.
///
/// Exists so `min` and `max` share one walk. Two copies of that merge would
/// be two chances to disagree with `load`, and no correctness test could see it.
fn endpoint_of(c: &Container, want_max: bool) -> Option<u16> {
    if want_max {
        c.max()
    } else {
        c.min()
    }
}

impl Snapshot {
    /// `Err(SnapshotTooOld)` once this view has been invalidated.
    ///
    /// Every read below opens with this. It gates **future** reads only —
    /// anything already materialized through this snapshot stays sound, because
    /// reclamation condition 3 is the `Buffer` refcount and is independent of
    /// condition 1. That separation is what makes eviction safe at all.
    #[inline]
    fn check_live(&self) -> Result<()> {
        if self.db.reader_evicted[self._slot.slot].load(Ordering::Acquire) {
            return Err(CodecError::SnapshotTooOld {
                version: self.version,
                last_key: None,
            });
        }
        Ok(())
    }

    /// Has this snapshot been invalidated? Diagnostics.
    #[inline]
    pub fn is_evicted(&self) -> bool {
        self.db.reader_evicted[self._slot.slot].load(Ordering::Acquire)
    }

    #[inline]
    pub fn version(&self) -> Version {
        self.version
    }

    fn shard_index(&self, key: u64) -> usize {
        // Through the map, for the same reason `WriteBatch::commit` is: a
        // reader recomputing a modulo would look in a different shard than the
        // writer used the moment the map stops being the identity permutation.
        self.db.route.shard_of_vshard(vshard_of(key))
    }

    fn shard_for(&self, key: u64) -> &Shard {
        &self.db.shards[self.shard_index(key)]
    }

    /// Every key with at least one live chunk, ascending.
    ///
    /// **This retires a position the codebase used to state as a fact.**
    /// `yesno-flight` answered `list_flights` with "the key space is a `u64`,
    /// not an enumerable catalogue", and that was true of the *space* and never
    /// of the *contents*: `ChunkKey` packs the key in its high bits, so one
    /// key's chunks are a contiguous B+tree range and the populated subset has
    /// always been walkable. What was missing was a reason to walk it. The
    /// PostgreSQL index access method supplies one — `ambulkdelete` must visit
    /// every key when VACUUM removes heap tuples, and a key it skips leaves
    /// dangling TIDs, which is a wrong answer rather than a maintenance gap.
    ///
    /// Cost is **O( distinct keys )**, not O( chunks ). Each key is found and
    /// then *seeked past* via `ChunkKey::range_end`, so a key holding 100 000
    /// chunks costs one descent rather than 100 000 steps. That property is
    /// invisible to a correctness test — a scan-through returns the identical
    /// list — so it is pinned by an allocation budget instead.
    ///
    /// A key present only as a tombstone is **absent**. `Memtable::get`
    /// returns `Option<Option<&Container>>` where the inner `None` is a delete
    /// rather than a miss, and treating a deleted key as present would resurrect
    /// it for every caller that enumerates.
    pub fn keys(&self) -> Result<Vec<u64>> {
        self.check_live()?;
        self.collect_keys(None)
    }

    /// The keys in `[lo, hi)`, ascending.
    ///
    /// Half-open, matching `len_in_range` and `Expr::Range` rather than
    /// `insert_range`'s inclusive form. The consequence worth stating: a key of
    /// `u64::MAX` cannot be named by any half-open upper bound, so
    /// [`Snapshot::keys`] is the only way to reach it. Unlike an ordinal — where
    /// `u64::MAX` is reserved and *cannot* be a member — a key of `u64::MAX` is
    /// perfectly legal, so this is a real restriction rather than a vacuous one.
    pub fn key_range(&self, lo: u64, hi: u64) -> Result<Vec<u64>> {
        self.check_live()?;
        if lo >= hi {
            return Ok(Vec::new());
        }
        self.collect_keys(Some((lo, hi)))
    }

    /// Shared body of [`Snapshot::keys`] and [`Snapshot::key_range`].
    fn collect_keys(&self, bounds: Option<(u64, u64)>) -> Result<Vec<u64>> {
        let mut out: Vec<u64> = Vec::new();
        for shard_ix in 0..self.db.shards.len() {
            self.shard_keys(shard_ix, bounds, &mut out)?;
        }
        // Sorted and deduped across shards rather than merged in order. By I7
        // all chunks of one key live in one shard, so a duplicate is impossible
        // and the dedup is insurance rather than logic — `tests/invariants.rs`
        // is what asserts I7 itself. Sorting is required regardless: shards are
        // visited in index order, which is unrelated to key order.
        out.sort_unstable();
        out.dedup();
        Ok(out)
    }

    /// Append one shard's live keys to `out`.
    fn shard_keys(
        &self,
        shard_ix: usize,
        bounds: Option<(u64, u64)>,
        out: &mut Vec<u64>,
    ) -> Result<()> {
        let shard = &self.db.shards[shard_ix];
        let in_bounds = |k: u64| bounds.is_none_or(|(lo, hi)| k >= lo && k < hi);

        // Candidate keys from the memtable, taken under its own lock and
        // released before the store's — the same discipline `merged_chunks`
        // follows, and for the same reason: nothing in the crate holds both.
        let mut candidates: Vec<u64> = {
            let mem = shard.mem.read().unwrap();
            mem.iter_at(self.version)
                .map(|(ck, _)| ck.key())
                .filter(|k| in_bounds(*k))
                .collect()
        };

        // Candidate keys from the index, one descent per key.
        if let (Some(tree), Some(store)) = (self.roots[shard_ix].as_ref(), shard.store.as_ref()) {
            let store = store.lock().unwrap();
            let hi = match bounds {
                Some((_, hi)) => ChunkKey::range_start(hi),
                None => ChunkKey(u128::MAX),
            };
            let mut at = match bounds {
                Some((lo, _)) => ChunkKey::range_start(lo),
                None => ChunkKey(0),
            };
            loop {
                let mut cursor = tree.range(&*store, at, hi);
                let Some(item) = cursor.next() else { break };
                let (ck, _) = item?;
                let key = ck.key();
                candidates.push(key);
                // The seek-past step, and the whole reason this is
                // O( distinct keys ). Do not replace it with `cursor.next()`
                // in a loop: that walks every chunk of every key and returns the
                // identical answer, so no correctness test would notice.
                if key == u64::MAX {
                    break;
                }
                at = ChunkKey::range_end(key);
            }
        }

        candidates.sort_unstable();
        candidates.dedup();

        for key in candidates {
            if self.key_is_live(shard_ix, key) {
                out.push(key);
            }
        }
        Ok(())
    }

    /// Whether `key` has at least one chunk visible at this snapshot.
    ///
    /// Not simply "the memtable or the tree mentions it". A chunk the
    /// memtable tombstones is *absent* even though the tree still holds it, so a
    /// key whose every chunk has been deleted must not be reported — that is the
    /// difference between enumerating keys and enumerating what was ever written.
    fn key_is_live(&self, shard_ix: usize, key: u64) -> bool {
        let shard = &self.db.shards[shard_ix];

        // The memtable's opinion, prefix by prefix. A single live chunk settles
        // it without touching the store at all.
        let overlay: Vec<(Prefix48, bool)> = {
            let mem = shard.mem.read().unwrap();
            mem.key_chunks(key, self.version)
                .map(|(p, c)| (p, c.is_some()))
                .collect()
        };
        if overlay.iter().any(|&(_, live)| live) {
            return true;
        }

        // Otherwise the key lives only if the index holds a chunk the memtable
        // has *not* tombstoned.
        let (Some(tree), Some(store)) = (self.roots[shard_ix].as_ref(), shard.store.as_ref())
        else {
            return false;
        };
        let store = store.lock().unwrap();
        let scan = tree.range(
            &*store,
            ChunkKey::range_start(key),
            ChunkKey::range_end(key),
        );
        for item in scan {
            let Ok((ck, _)) = item else { continue };
            let tombstoned = overlay.iter().any(|&(p, _)| p == ck.prefix());
            if !tombstoned {
                return true;
            }
        }
        false
    }

    /// The on-disk container for a chunk the memtable has nothing to say about.
    fn disk_container(&self, key: u64, prefix: Prefix48) -> Option<Container> {
        let i = self.shard_index(key);
        let tree = self.roots[i]?;
        let shard = &self.db.shards[i];
        let store = shard.store.as_ref()?.lock().unwrap();
        let ck = ChunkKey::new(key, prefix);
        let cref = tree.get(&*store, ck).ok()??;
        // Checked: a stale or corrupt reference must not decode as this key.
        store.read_container_for(ck, cref).ok()?
    }

    /// Every chunk of `key` visible here, merging the memtable over the
    /// captured index root. Tombstones suppress the on-disk value.
    ///
    /// Both sides arrive in `Prefix48` order already — the index scan because
    /// `ChunkKey` orders by prefix within a key, the memtable because
    /// [`Memtable::key_chunks`] is a `BTreeMap` range — so the merge is a
    /// two-pointer walk.
    ///
    /// **It used to be a `BTreeMap<Prefix48, Option<Container>>`**, filled
    /// from the disk scan and then overwritten by the memtable. That is
    /// `O(n log n)` over cache-hostile nodes to merge two sorted streams, and
    /// it was the whole of `load-is-superlinear`: `Snapshot::load` cost 38 ns
    /// per chunk at 100 chunks and 214 ns at 100 000, while
    /// [`OrdSet::from_chunks`] over the very same corpus stayed flat at
    /// 8-13 ns. Do not reintroduce an intermediate map here — nothing needs
    /// ordering that the inputs do not already supply. See JOURNAL, 2026-08-26.
    ///
    /// The merge also declines to decode a chunk the memtable supersedes. The
    /// map form read one container off the store for every overwritten prefix
    /// and immediately discarded it.
    /// # Errors are propagated, not dropped -- changed 2026-09-14
    ///
    /// This returned a bare `Vec` and swallowed both failure paths: a scan error
    /// via `let Ok( .. ) else { continue }` and a payload error via
    /// `if let Ok( Some( c ) ) = ..`. So `load` answered a corrupt key with a
    /// **short set presented as a complete one**, which `db/keystream.rs` already
    /// calls out as the worse failure and which `KeyStream` already refused to do.
    ///
    /// The checksum cache forced the question rather than raising it: with the
    /// stored CRCs recomputed on the read path, a corrupt node or payload now
    /// *produces* an error here, and swallowing it converted "silently correct"
    /// into "silently empty" -- strictly worse than not checking at all. A check
    /// whose error has nowhere to go is not a check.
    fn merged_chunks(&self, key: u64) -> Result<Vec<(Prefix48, Container)>> {
        let shard = self.shard_for(key);

        // Taken and released before the store lock. Nothing in the crate holds
        // both, and a merge streaming directly from each would be the first
        // thing to impose an order between them; a `Vec` of what is already a
        // snapshot at `self.version` costs one linear pass and keeps it that
        // way.
        let overlay: Vec<(Prefix48, Option<Container>)> = {
            let mem = shard.mem.read().unwrap();
            mem.key_chunks(key, self.version)
                .map(|(p, c)| (p, c.cloned()))
                .collect()
        };

        let Some((tree, store)) = self.roots[self.shard_index(key)].zip(shard.store.as_ref())
        else {
            return Ok(overlay
                .into_iter()
                .filter_map(|(p, c)| c.map(|c| (p, c)))
                .collect());
        };

        let store = store.lock().unwrap();
        let mut overlay = overlay.into_iter().peekable();
        let mut out: Vec<(Prefix48, Container)> = Vec::new();

        let scan = tree.range(
            &*store,
            ChunkKey::range_start(key),
            ChunkKey::range_end(key),
        );
        for item in scan {
            let (ck, cref) = item?;
            let prefix = ck.prefix();

            // Memtable chunks below this prefix are uncontested.
            while overlay.peek().is_some_and(|&(p, _)| p < prefix) {
                let (p, c) = overlay.next().unwrap();
                if let Some(c) = c {
                    out.push((p, c));
                }
            }

            // The memtable wins wherever it has an opinion, including
            // tombstones — and when it does, the payload is never decoded.
            if overlay.peek().is_some_and(|&(p, _)| p == prefix) {
                let (p, c) = overlay.next().unwrap();
                if let Some(c) = c {
                    out.push((p, c));
                }
                continue;
            }

            // Checked: the scan yields the key, so a reference that points
            // at a different chunk is detectable here rather than silently
            // decoding as this one.
            if let Some(c) = store.read_container_for(ck, cref)? {
                out.push((prefix, c));
            }
        }

        for (p, c) in overlay {
            if let Some(c) = c {
                out.push((p, c));
            }
        }
        Ok(out)
    }

    pub fn contains(&self, key: u64, ordinal: u64) -> Result<bool> {
        self.check_live()?;
        let (prefix, low) = crate::split(ordinal);
        let decided = {
            let mem = self.shard_for(key).mem.read().unwrap();
            // A tombstone positively answers "no" and must not fall through.
            mem.get(ChunkKey::new(key, prefix), self.version)
                .map(|c| c.is_some_and(|c| c.contains(low)))
        };
        Ok(match decided {
            Some(answer) => answer,
            None => self
                .disk_container(key, prefix)
                .is_some_and(|c| c.contains(low)),
        })
    }

    /// Ordinals under `key`, **without decoding a single payload**.
    ///
    /// `ChunkRef` carries `card_m1` for exactly this: one range scan of the
    /// pinned index root reads the per-chunk cardinality straight out of the
    /// leaf entries, and the memtable is merged over the top. The README,
    /// `ARCHITECTURE.md` and `ChunkRef::cardinality`'s own doc all call it a
    /// headline property of the format.
    ///
    /// It was not implemented. This summed `merged_chunks(key)`, which decodes
    /// every container first — 2 129 allocations for a 500-chunk key, against
    /// a handful for the index walk. No correctness test could see the
    /// difference, since both return the same number; the guard is
    /// `cardinality_is_answered_from_the_index_not_by_materializing` in
    /// `tests/allocation.rs`.
    pub fn cardinality(&self, key: u64) -> Result<u64> {
        self.check_live()?;
        let i = self.shard_index(key);
        let shard = &self.db.shards[i];

        // The scan is already in `ChunkKey` order, hence ascending in prefix,
        // which is the ordering `cardinality_at` merges against. Collecting it
        // into a map to be looked up again — and a second `Vec` of its keys —
        // was `load-is-superlinear` in its second location.
        let mut on_disk: Vec<(Prefix48, u32)> = Vec::new();
        if let (Some(tree), Some(store)) = (self.roots[i], shard.store.as_ref()) {
            let store = store.lock().unwrap();
            let scan = tree.range(
                &*store,
                ChunkKey::range_start(key),
                ChunkKey::range_end(key),
            );
            for item in scan {
                let Ok((ck, cref)) = item else { continue };
                on_disk.push((ck.prefix(), cref.cardinality()));
            }
        }
        let mem = shard.mem.read().unwrap();
        Ok(memtable::cardinality_at(&mem, key, self.version, on_disk))
    }

    #[inline]
    pub fn is_empty(&self, key: u64) -> Result<bool> {
        Ok(self.cardinality(key)? == 0)
    }

    /// Ordinals of `key` in the half-open range `[lo, hi)`, from the index.
    ///
    /// **Half-open.** `Db::insert_range` is inclusive `[lo, hi]`; this is not,
    /// because a Parquet row group is `[start, start + count)` and this exists to
    /// be asked once per row group.
    ///
    /// Reads `card_m1` out of the leaf entries for every chunk the range covers
    /// wholly and decodes a payload only for the at most **two** it covers
    /// partially — so the cost is one index range scan plus two container reads,
    /// whatever the range's width. The design calls this and
    /// [`Self::range_summary`] the pair that carries the pushdown story, and the
    /// reason is exactly that ratio: sixteen chunks per row group, two reads.
    pub fn len_in_range(&self, key: u64, lo: u64, hi: u64) -> Result<u64> {
        self.check_live()?;
        if hi <= lo {
            return Ok(0);
        }
        let i = self.shard_index(key);
        let shard = &self.db.shards[i];

        // Restricted to the prefixes the range touches, so a narrow window over
        // a huge key does not walk the key. `hi` is exclusive, so the last chunk
        // is the one holding `hi - 1`.
        let (p_lo, _) = crate::split(lo);
        let (p_hi, _) = crate::split(hi - 1);
        let mut on_disk: Vec<(Prefix48, u32)> = Vec::new();
        if let (Some(tree), Some(store)) = (self.roots[i], shard.store.as_ref()) {
            let store = store.lock().unwrap();
            // `ChunkKey::new` **masks** its prefix to 48 bits rather than
            // rejecting one that overflows, so `p_hi + 1` at the top chunk wraps
            // to prefix 0 — and an ascending scan from `p_lo` to `0` yields
            // nothing. Not an error: a silent zero, on exactly the query a
            // planner makes with no upper bound to offer. `range_end` is the
            // exclusive bound for that case and already exists.
            const TOP_PREFIX: Prefix48 = (1 << 48) - 1;
            let scan_end = if p_hi >= TOP_PREFIX {
                ChunkKey::range_end(key)
            } else {
                ChunkKey::new(key, p_hi + 1)
            };
            let scan = tree.range(&*store, ChunkKey::new(key, p_lo), scan_end);
            for item in scan {
                let Ok((ck, cref)) = item else { continue };
                on_disk.push((ck.prefix(), cref.cardinality()));
            }
        }
        let mem = shard.mem.read().unwrap();
        Ok(memtable::count_in_range_at(
            &mem,
            key,
            self.version,
            lo,
            hi,
            on_disk,
            |p| self.disk_container(key, p),
        ))
    }

    /// Whether `[lo, hi)` is wholly outside `key`, wholly inside it, or neither.
    ///
    /// The decision a scan planner makes per row group: `Empty` skips it without
    /// decompressing a page, `Full` scans it with no selection vector at all, and
    /// only `Partial` has to pay for one.
    pub fn range_summary(&self, key: u64, lo: u64, hi: u64) -> Result<crate::RangeSummary> {
        let width = hi.saturating_sub(lo);
        if width == 0 {
            return Ok(crate::RangeSummary::Empty);
        }
        use crate::RangeSummary;
        Ok(match self.len_in_range(key, lo, hi)? {
            0 => RangeSummary::Empty,
            n if n == width => RangeSummary::Full,
            _ => RangeSummary::Partial,
        })
    }

    /// Materialize a key as an [`OrdSet`].
    ///
    /// Decodes every container of the key up front. [`Self::key_stream`] is the
    /// chunk-at-a-time form, and is what a consumer that will not read the whole
    /// key should reach for.
    pub fn load(&self, key: u64) -> Result<OrdSet> {
        self.check_live()?;
        Ok(OrdSet::from_chunks(self.merged_chunks(key)?))
    }

    /// A key as a lazy [`ChunkStream`](crate::ChunkStream), decoding one chunk
    /// at a time.
    ///
    /// One index range scan resolves every visible chunk to a reference;
    /// payloads are read in `next_chunk`, and counting questions are answered
    /// from the index without reading any. See [`keystream`] for what is lazy,
    /// what is not, and the one way this differs from [`Self::load`].
    ///
    /// The returned stream holds this snapshot's reader slot, so it keeps the
    /// reclamation floor where it needs it and may outlive the `Snapshot` value
    /// it came from.
    pub fn key_stream(&self, key: u64) -> Result<keystream::KeyStream> {
        keystream::KeyStream::new(self, key)
    }

    /// A key as a **lazy leaf** in an expression.
    ///
    /// The difference from `Expr::set( snap.load( key )? )` is that nothing is
    /// decoded until the expression runs, and an operator that skips most of the
    /// key never decodes the part it skipped. The planner still gets the three
    /// statistics it reads off a resident set -- chunk count, prefix span and
    /// cardinality -- because [`keystream::KeySource`] answers them from the
    /// index. It is also the crate's only producer of
    /// [`Backing::Paged`](crate::stream::Backing::Paged), so a plan over one
    /// finally has a reason to prefer the other operand as its driver.
    ///
    /// Infallible: the expression is built without reading anything, and a
    /// snapshot too old to read reports that when the expression is *opened*,
    /// which is where every other read failure already surfaces.
    ///
    /// The returned expression holds this snapshot's reader slot, so like
    /// [`Self::key_stream`] it may outlive the `Snapshot` value it came from.
    pub fn key_expr(&self, key: u64) -> crate::Expr {
        crate::Expr::Source(std::sync::Arc::new(keystream::KeySource::new(
            self.clone(),
            key,
        )))
    }

    /// The lowest ordinal under `key`, **decoding one container**.
    ///
    /// Chunks are ordered by prefix, so the lowest ordinal lives in the first
    /// non-empty chunk and the highest in the last. This was `load( key )?.min()`,
    /// which decodes and clones **every** chunk of the posting list to read a
    /// value that lives in one of them -- 704x from 1 chunk to 10 000, where a
    /// first-chunk answer is roughly flat.
    ///
    /// The same rule as `is_empty` and [`Self::cardinality`]: *an answer must
    /// not cost more than the answer it is weaker than*. An endpoint is weaker
    /// than the whole set.
    ///
    /// Like `cardinality`, this is a **parallel implementation** of the
    /// materializing one and returns the same values, so no correctness test can
    /// tell them apart. The guards are
    /// `an_endpoint_is_not_answered_by_materializing` in `tests/allocation.rs`
    /// and the equivalence properties beside it.
    pub fn min(&self, key: u64) -> Result<Option<u64>> {
        self.endpoint(key, false)
    }

    /// The highest ordinal under `key`. See [`Self::min`].
    ///
    /// Costs one **index** walk to the end of the key's range, because the
    /// scan is ascending and there is no reverse traversal -- but still exactly
    /// one container decode, which is the part that dominated.
    pub fn max(&self, key: u64) -> Result<Option<u64>> {
        self.endpoint(key, true)
    }

    /// One end of a key's posting list.
    ///
    /// **Lock order is the same as `merged_chunks`**: the overlay is taken
    /// and released before the store lock, and nothing here holds both. That is
    /// also why the overlay is collected rather than streamed -- merging two live
    /// iterators would be the first thing in the crate to impose an order.
    fn endpoint(&self, key: u64, want_max: bool) -> Result<Option<u64>> {
        self.check_live()?;
        let i = self.shard_index(key);
        let shard = &self.db.shards[i];

        let overlay: Vec<(Prefix48, Option<Container>)> = {
            let mem = shard.mem.read().unwrap();
            mem.key_chunks(key, self.version)
                .map(|(p, c)| (p, c.cloned()))
                .collect()
        };

        // Where the winning chunk lives, once the merge has decided. A memtable
        // chunk is already in hand; a disk chunk is read afterwards, alone.
        enum Winner {
            Mem(Container),
            Disk(ChunkKey, crate::store::extent::ChunkRef),
        }
        let mut best: Option<Winner> = None;
        let mut prefix_of_best: Option<Prefix48> = None;
        // The first non-empty chunk in ascending order *is* the minimum, so
        // `min` stops here instead of walking the rest of the key.
        let stop_now = !want_max;

        let Some((tree, store)) = self.roots[i].zip(shard.store.as_ref()) else {
            for (p, c) in overlay {
                if let Some(c) = c.filter(|c| !c.is_empty()) {
                    best = Some(Winner::Mem(c));
                    prefix_of_best = Some(p);
                    if stop_now {
                        break;
                    }
                }
            }
            return Ok(match (best, prefix_of_best) {
                (Some(Winner::Mem(c)), Some(p)) => endpoint_of(&c, want_max).map(|l| join(p, l)),
                _ => None,
            });
        };

        let store = store.lock().unwrap();
        let mut overlay = overlay.into_iter().peekable();

        let scan = tree.range(
            &*store,
            ChunkKey::range_start(key),
            ChunkKey::range_end(key),
        );
        'scan: for item in scan {
            let Ok((ck, cref)) = item else { continue };
            let prefix = ck.prefix();

            while overlay.peek().is_some_and(|&(p, _)| p < prefix) {
                let (p, c) = overlay.next().unwrap();
                if let Some(c) = c.filter(|c| !c.is_empty()) {
                    best = Some(Winner::Mem(c));
                    prefix_of_best = Some(p);
                    if stop_now {
                        break 'scan;
                    }
                }
            }

            // The memtable wins wherever it has an opinion, tombstones
            // included — and when it does, the payload is never decoded.
            if overlay.peek().is_some_and(|&(p, _)| p == prefix) {
                let (p, c) = overlay.next().unwrap();
                if let Some(c) = c.filter(|c| !c.is_empty()) {
                    best = Some(Winner::Mem(c));
                    prefix_of_best = Some(p);
                    if stop_now {
                        break 'scan;
                    }
                }
                continue;
            }

            // `card_m1` answers "is this chunk empty" straight out of the
            // leaf entry, so locating the endpoint decodes nothing at all.
            if cref.cardinality() > 0 {
                best = Some(Winner::Disk(ck, cref));
                prefix_of_best = Some(prefix);
                if stop_now {
                    break 'scan;
                }
            }
        }

        // Overlay chunks above every on-disk chunk, which the scan cannot
        // reach. Skipped once `min` has already decided: `break 'scan` leaves
        // this loop live, and draining it here overwrote the settled minimum
        // with a *higher* chunk. Everything still in `overlay` sorts above the
        // winner by construction — the merge consumed the lower prefixes before
        // choosing — so for `min` there is nothing left worth looking at.
        if !stop_now || best.is_none() {
            for (p, c) in overlay {
                if let Some(c) = c.filter(|c| !c.is_empty()) {
                    best = Some(Winner::Mem(c));
                    prefix_of_best = Some(p);
                    if stop_now {
                        break;
                    }
                }
            }
        }

        let Some(best) = best else { return Ok(None) };
        let prefix = prefix_of_best.expect("a winner always records its prefix");
        Ok(match best {
            Winner::Mem(c) => endpoint_of(&c, want_max).map(|low| join(prefix, low)),
            Winner::Disk(ck, cref) => match store.read_container_for(ck, cref) {
                Ok(Some(c)) => endpoint_of(&c, want_max).map(|low| join(prefix, low)),
                _ => None,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Routing must follow the persisted map, not recompute a modulo.
    ///
    /// **This is the assertion the data-loss regression cannot make.** A
    /// freshly created map *is* `v % shards`, so once the shard count is
    /// persisted the map and the modulo agree on every key — sabotaging
    /// `shard_of` back to `vshard_of(key) % shards.len()` leaves
    /// `reopening_with_a_different_shard_count_keeps_every_key` green. Two
    /// separate deliverables were conflated in one item: **persisting the count**
    /// is what fixed the silent misrouting, and **the indirection** is what makes
    /// a future shard-count change possible without rehashing every key. Only a
    /// map that disagrees with the modulo can pin the second.
    #[test]
    fn routing_follows_the_persisted_map_rather_than_a_modulo() {
        let dir = tmpdir("route_by_map");
        let _ = std::fs::remove_dir_all(&dir);
        const N: usize = 4;
        {
            let db = Db::open_with(
                &dir,
                DbOptions {
                    shards: N,
                    ..Default::default()
                },
            )
            .unwrap();
            db.insert(1, 1).unwrap();
            db.checkpoint().unwrap();
        }

        // Reverse the map, so it disagrees with `v % N` wherever `v % N != N-1-(v % N)`.
        let path = dir.join("MANIFEST");
        let mut m = manifest::pick(&std::fs::read(&path).unwrap())
            .unwrap()
            .unwrap();
        let permuted: Vec<u16> = (0..VSHARDS)
            .map(|v| (N as u16 - 1) - (v % N as u32) as u16)
            .collect();
        m.map = permuted.clone();
        m.seq += 1;
        write_manifest(&path, &m).unwrap();

        let db = Db::open_with(
            &dir,
            DbOptions {
                shards: N,
                ..Default::default()
            },
        )
        .unwrap();
        let mut disagreements = 0;
        for k in 0..2_000u64 {
            let v = vshard_of(k);
            assert_eq!(
                db.shard_of(k),
                permuted[v as usize] as usize,
                "key {k} ( vshard {v} ) must route by the map"
            );
            if permuted[v as usize] as usize != v as usize % N {
                disagreements += 1;
            }
        }
        // Without this the loop would pass against a `shard_of` that ignored the
        // map entirely, on any permutation that happened to equal the modulo.
        assert!(
            disagreements > 0,
            "the permuted map must actually differ from the modulo somewhere"
        );

        // And `shard_of` is not the routing function the data takes. The
        // write path groups ops itself in `WriteBatch::commit` and the read
        // path resolves a shard in `Snapshot::shard_index`; both once
        // recomputed the modulo, so the accessor above agreed with the map
        // while every actual write and read disagreed with it. Two sites
        // computing the *same wrong* answer also round-trip perfectly, so a
        // round trip alone cannot see this — the shard the bytes landed in has
        // to be observed directly.
        let wal_len = |s: usize| {
            std::fs::metadata(dir.join(format!("shard-{s:04}.wal")))
                .map(|m| m.len())
                .unwrap_or(0)
        };
        let key = (0..2_000u64)
            .find(|&k| {
                let v = vshard_of(k) as usize;
                permuted[v] as usize != v % N
            })
            .expect("some key must route differently under the permutation");
        let want = permuted[vshard_of(key) as usize] as usize;
        let before: Vec<u64> = (0..N).map(wal_len).collect();
        db.insert(key, 7).unwrap();
        let grew: Vec<usize> = (0..N).filter(|&s| wal_len(s) > before[s]).collect();
        assert_eq!(
            grew,
            vec![want],
            "key {key} ( vshard {} ) must be logged to the shard the map names, \
             not to {} as the modulo would have it",
            vshard_of(key),
            vshard_of(key) as usize % N
        );

        // And the reader must look in that same shard.
        let s = db.snapshot().unwrap();
        assert!(
            s.contains(key, 7).unwrap(),
            "the read path resolved a different shard than the write path"
        );
    }

    #[test]
    fn insert_and_read_back() {
        let db = Db::new();
        assert!(db.insert(1, 5).unwrap());
        assert!(!db.insert(1, 5).unwrap(), "re-inserting is not a change");

        let s = db.snapshot().unwrap();
        assert!(s.contains(1, 5).unwrap());
        assert!(!s.contains(1, 6).unwrap());
        assert_eq!(s.cardinality(1).unwrap(), 1);
    }

    #[test]
    fn keys_spread_across_shards() {
        let db = Db::with_options(DbOptions {
            shards: 8,
            ..Default::default()
        });
        let used: std::collections::BTreeSet<usize> =
            (0..1000u64).map(|k| db.shard_of(k)).collect();
        assert_eq!(used.len(), 8, "hashing must use every shard");
    }

    #[test]
    fn all_chunks_of_a_key_share_one_shard() {
        // I7: a single-key read must never cross shards.
        let db = Db::with_options(DbOptions {
            shards: 8,
            ..Default::default()
        });
        for key in 0..100u64 {
            let s = db.shard_of(key);
            for prefix in [0u64, 1, 1 << 30] {
                assert_eq!(db.shard_of(key), s, "prefix {prefix} moved the key");
            }
        }
    }

    #[test]
    fn a_snapshot_does_not_see_later_writes() {
        let db = Db::new();
        db.insert(1, 5).unwrap();
        let s = db.snapshot().unwrap();

        db.insert(1, 6).unwrap();
        assert!(
            !s.contains(1, 6).unwrap(),
            "the snapshot is frozen at its version"
        );
        assert_eq!(s.cardinality(1).unwrap(), 1);

        let s2 = db.snapshot().unwrap();
        assert!(s2.contains(1, 6).unwrap());
        assert_eq!(s2.cardinality(1).unwrap(), 2);
    }

    #[test]
    fn a_multi_shard_batch_is_all_or_nothing() {
        let db = Db::with_options(DbOptions {
            shards: 8,
            ..Default::default()
        });
        // Pick keys that land on different shards.
        let mut keys = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for k in 0..1000u64 {
            let s = db.shard_of(k);
            if seen.insert(s) {
                keys.push(k);
            }
            if keys.len() == 4 {
                break;
            }
        }
        assert_eq!(keys.len(), 4);

        let before = db.snapshot().unwrap();
        let mut b = db.batch();
        for &k in &keys {
            b.insert(k, 42);
        }
        let c = b.commit().unwrap();
        assert_eq!(c.shards, 4, "the batch spans four shards");
        assert_eq!(c.changed, 4);

        // The older snapshot sees none of it; a new one sees all of it.
        for &k in &keys {
            assert!(!before.contains(k, 42).unwrap());
        }
        let after = db.snapshot().unwrap();
        for &k in &keys {
            assert!(
                after.contains(k, 42).unwrap(),
                "key {k} missing from the committed batch"
            );
        }
    }

    #[test]
    fn commit_versions_advance_and_are_visible() {
        let db = Db::new();
        assert_eq!(db.visible(), 0);
        let mut b = db.batch();
        b.insert(1, 1);
        let c1 = b.commit().unwrap();
        assert_eq!(c1.version, 1);
        assert_eq!(db.visible(), 1);
        let mut b = db.batch();
        b.insert(1, 2);
        let c2 = b.commit().unwrap();
        assert_eq!(c2.version, 2);
        assert_eq!(db.visible(), 2);
    }

    #[test]
    fn an_empty_batch_consumes_no_version() {
        let db = Db::new();
        db.insert(1, 1).unwrap();
        let before = db.visible();
        let c = db.batch().commit().unwrap();
        assert_eq!(c.shards, 0);
        assert_eq!(
            db.visible(),
            before,
            "an empty batch must not burn a version"
        );
    }

    #[test]
    fn rollback_applies_nothing() {
        let db = Db::new();
        let mut b = db.batch();
        b.insert(1, 5);
        b.rollback();
        assert_eq!(db.visible(), 0);
        assert!(!db.snapshot().unwrap().contains(1, 5).unwrap());
    }

    #[test]
    fn remove_is_visible_and_bounded_by_the_snapshot() {
        let db = Db::new();
        db.insert(1, 5).unwrap();
        let before = db.snapshot().unwrap();
        assert!(db.remove(1, 5).unwrap());

        assert!(
            before.contains(1, 5).unwrap(),
            "the older snapshot still sees it"
        );
        assert!(!db.snapshot().unwrap().contains(1, 5).unwrap());
        assert_eq!(db.snapshot().unwrap().cardinality(1).unwrap(), 0);
    }

    #[test]
    fn store_set_replaces_the_whole_key() {
        let db = Db::new();
        db.insert_many(1, &[1, 2, 3]).unwrap();
        let set = OrdSet::from_iter_unsorted([100u64, 200, 1 << 30]);
        let mut b = db.batch();
        b.store_set(1, &set);
        b.commit().unwrap();

        let s = db.snapshot().unwrap();
        assert_eq!(s.cardinality(1).unwrap(), 3);
        assert!(!s.contains(1, 1).unwrap(), "the old contents must be gone");
        assert!(s.contains(1, 200).unwrap());
        assert!(s.contains(1, 1 << 30).unwrap());
    }

    #[test]
    fn load_materializes_a_key_across_chunks() {
        let db = Db::new();
        let vals: Vec<u64> = vec![0, 5, 65_536, 65_540, 1 << 40];
        db.insert_many(1, &vals).unwrap();
        let s = db.snapshot().unwrap();
        assert_eq!(s.load(1).unwrap().iter().collect::<Vec<_>>(), vals);
        assert_eq!(s.min(1).unwrap(), Some(0));
        assert_eq!(s.max(1).unwrap(), Some(1 << 40));
    }

    /// `min` and `max` walk the index and decode one chunk; `load` decodes
    /// every chunk. They must not disagree, and only a test that asks both can
    /// say so — an endpoint that is merely *plausible* looks identical.
    ///
    /// The tombstone cases are the ones that bite. A deleted first chunk
    /// moves the minimum, and a memtable tombstone over a populated on-disk
    /// chunk is the only way the two sides can diverge without either being
    /// obviously wrong.
    #[test]
    fn an_endpoint_agrees_with_materializing_over_every_overlay_shape() {
        let dir = tmpdir("endpoint-agrees");
        let _clean = CleanDir(dir.clone());
        std::fs::create_dir_all(&dir).unwrap();

        // Chunks 0, 3, 7 and 40 on disk; the gaps matter because the merge
        // walks prefixes, not chunk indices.
        // **Every terminal chunk holds more than one ordinal, deliberately.**
        // With a single ordinal per chunk `min == max` inside it, and an
        // implementation that returned the wrong end of the right chunk would
        // pass. That is the shape of a check that cannot fail.
        // `at( chunk, low )` rather than `( c << 16 ) | l`, which clippy reads
        // as an identity operation at chunk 0 and which hides the intent anyway.
        let at = |chunk: u64, low: u64| (chunk << 16) | low;
        let on_disk: Vec<u64> = vec![
            at(0, 9),
            at(0, 11),
            at(0, 500),
            at(3, 5),
            at(3, 900),
            at(7, 1),
            at(40, 2),
            at(40, 65_535),
        ];
        let db = Db::open(&dir).unwrap();
        db.insert_many(1, &on_disk).unwrap();
        db.checkpoint().unwrap();

        let check = |db: &Db, label: &str| {
            let s = db.snapshot().unwrap();
            let want = s.load(1).unwrap();
            assert_eq!(s.min(1).unwrap(), want.min(), "min disagrees: {label}");
            assert_eq!(s.max(1).unwrap(), want.max(), "max disagrees: {label}");
        };

        check(&db, "purely on disk");

        // An overlay chunk *below* every on-disk chunk cannot exist here
        // (chunk 0 is taken), so extend the top instead: the new maximum is in
        // the memtable while the minimum is still on disk.
        db.insert_many(1, &[(99 << 16) | 3, (99 << 16) | 77])
            .unwrap();
        check(&db, "overlay above the on-disk maximum");

        // An overlay that contests an existing prefix: the memtable wins, and
        // it must win for the endpoint too.
        db.insert(1, at(3, 2)).unwrap();
        check(&db, "overlay contesting a populated prefix");

        // Tombstone the whole first chunk. The minimum must move to chunk 3.
        db.remove_range(1, 0, at(1, 0)).unwrap();
        check(&db, "first chunk tombstoned");

        // Tombstone the last chunk too. The maximum must fall back.
        db.remove_range(1, at(99, 0), at(100, 0)).unwrap();
        check(&db, "last chunk tombstoned");

        // Empty the key entirely; both ends must be `None`, not a stale value.
        db.remove_range(1, 0, at(200, 0)).unwrap();
        let s = db.snapshot().unwrap();
        assert_eq!(s.load(1).unwrap().min(), None, "the key should be empty");
        assert_eq!(s.min(1).unwrap(), None, "min on an emptied key");
        assert_eq!(s.max(1).unwrap(), None, "max on an emptied key");

        // A key that never existed.
        assert_eq!(s.min(12_345).unwrap(), None);
        assert_eq!(s.max(12_345).unwrap(), None);
    }

    /// **Is a returned commit guaranteed visible to the next read?**
    ///
    /// `snapshot()` reads `oracle.visible()`, and `visible` advances by a
    /// *prefix* rule over consecutive resolved versions. `commit` releases its
    /// shard guards before the durability wait and returns as soon as its own
    /// version is resolved — it never waits for `visible` to reach it. So a
    /// commit that resolves while an *earlier* version is still `Pending`
    /// returns a version the database will not yet show.
    ///
    /// This test exists to establish whether that window is **reachable**,
    /// not how often. It is a race by construction: the two committers use
    /// different shards so their fsyncs are independent, the slow one takes the
    /// lower version, and the fast one is released only once the slow one has
    /// taken its version. If the window is not reachable the loop simply never
    /// observes it and the test says so.
    #[test]
    fn a_returned_commit_can_be_invisible_while_an_earlier_one_is_pending() {
        let dir = tmpdir("visible-lag");
        let _clean = CleanDir(dir.clone());
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open_with(
            &dir,
            DbOptions {
                shards: 4,
                ..Default::default()
            },
        )
        .unwrap();

        // Two keys on different shards: independent WALs, independent fsyncs.
        let shard_of = |k: u64| db.inner.route.shard_of_vshard(vshard_of(k));
        let (mut slow_key, mut fast_key) = (0u64, 0u64);
        for k in 2u64..512 {
            if shard_of(k) != shard_of(1) {
                slow_key = 1;
                fast_key = k;
                break;
            }
        }
        assert!(fast_key != 0, "need two keys on different shards");

        let mut observed = None;
        for attempt in 0..64u64 {
            let base = attempt * 1_000_000;
            // Big enough that the slow committer's append and fsync dominate.
            let bulk: Vec<u64> = (0..60_000).map(|i| base + i).collect();

            let gate = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let before = db.inner.oracle.peek_next();

            let slow = {
                let db = db.clone();
                let gate = gate.clone();
                std::thread::spawn(move || {
                    let mut b = db.batch();
                    for &o in &bulk {
                        b.insert(slow_key, o);
                    }
                    gate.store(true, Ordering::Release);
                    b.commit().unwrap()
                })
            };

            // Wait until the slow committer has actually taken a version, so the
            // fast one is guaranteed to sit *above* it in the ring.
            while !gate.load(Ordering::Acquire) {
                std::hint::spin_loop();
            }
            while db.inner.oracle.peek_next() == before {
                std::hint::spin_loop();
            }

            let mut fb = db.batch();
            fb.insert(fast_key, base);
            let fast = fb.commit().unwrap();
            let visible_now = db.visible();
            // Sampled here, in the same window, not after the join: once the
            // earlier commit resolves, `visible` catches up and the read starts
            // succeeding. An assertion taken afterwards would pass for the wrong
            // reason and prove nothing.
            let read_back = db.snapshot_at(fast.version).err();
            // Also in the window, and the reason this test is the right home
            // for it: the wait can only be shown to *do* anything at the one
            // instant the thing it waits for has not happened. Outside this
            // window `wait_visible` returns immediately and proves nothing.
            let waited = read_back
                .is_some()
                .then(|| db.wait_visible(fast.version, std::time::Duration::from_secs(10)));
            let waited_read = waited
                .as_ref()
                .map(|_| db.snapshot_at(fast.version).map(|s| s.version()));
            let slow = slow.join().unwrap();

            if fast.version > visible_now {
                println!(
                    "attempt {attempt}: commit returned v{} while visible was {} \
                     (earlier pending commit held v{})",
                    fast.version, visible_now, slow.version
                );
                observed = Some((
                    fast.version,
                    visible_now,
                    slow.version,
                    read_back,
                    waited,
                    waited_read,
                ));
                break;
            }
        }

        let (returned, visible_then, earlier, read_back, waited, waited_read) = observed.expect(
            "never observed a committed version above `visible`; either the window \
             was missed on every attempt or commit does wait for visibility",
        );
        assert!(
            returned > visible_then,
            "commit returned version {returned} while visible was {visible_then} \
             ( the earlier, still-pending commit took version {earlier} )"
        );
        // The consequence a caller actually feels: the version `commit`
        // just handed back cannot be read at the moment it was handed back.
        assert!(
            matches!(read_back, Some(CodecError::VersionNotVisible { .. })),
            "expected reading back v{returned} to be refused as not-yet-visible, \
             got {read_back:?}"
        );

        // And the remedy, measured at the same instant. `wait_visible` is the
        // whole answer to the commit-visibility window: the window is real,
        // bounded by one fsync, and a caller that needs recency can now close it
        // without every other committer paying for it.
        assert!(
            matches!(waited, Some(Ok(()))),
            "waiting for v{returned} — refused a moment earlier — should succeed, \
             got {waited:?}"
        );
        assert!(
            matches!(waited_read, Some(Ok(v)) if v == returned),
            "after the wait, v{returned} must be readable at exactly that version, \
             got {waited_read:?}"
        );
    }

    /// The `Db` wrapper's two jobs beyond the oracle's: hand back `Ok` when the
    /// version is already readable, and name the right error when it is not.
    ///
    /// Deliberately does not construct the race — the test above owns that.
    /// This one pins that a timeout is **not** reported as `VersionNotVisible`,
    /// which is the distinction the whole variant exists for: one says retrying
    /// is pointless, the other says it is not.
    #[test]
    fn a_visibility_wait_reports_a_timeout_distinctly_from_a_missing_version() {
        let db = Db::new();
        db.insert(1, 7).unwrap();
        let v = db.visible();
        assert!(
            db.wait_visible(v, std::time::Duration::ZERO).is_ok(),
            "a single-shard commit is visible the moment it returns"
        );

        let err = db
            .wait_visible(v + 100, std::time::Duration::from_millis(20))
            .expect_err("a version this database never assigned cannot become visible");
        assert!(
            matches!(err, CodecError::VisibilityTimeout { requested, visible, .. }
                if requested == v + 100 && visible == v),
            "expected a VisibilityTimeout naming the current watermark, got {err:?}"
        );
        // The negative half. `snapshot_at` still reports the same version as
        // `VersionNotVisible`, and the two must not be confused: a caller that
        // waited and timed out may retry, a caller told `VersionNotVisible`
        // learns nothing about whether waiting would help.
        assert!(
            matches!(
                db.snapshot_at(v + 100),
                Err(CodecError::VersionNotVisible { .. })
            ),
            "snapshot_at keeps its own variant"
        );
    }

    #[test]
    fn safe_version_is_pinned_by_the_oldest_reader() {
        let db = Db::new();
        db.insert(1, 1).unwrap();
        let old = db.snapshot().unwrap();
        db.insert(1, 2).unwrap();
        db.insert(1, 3).unwrap();

        assert_eq!(
            db.safe_version(),
            old.version(),
            "the oldest reader pins the floor"
        );
        let v = old.version();
        drop(old);
        assert!(db.safe_version() > v, "releasing it lets the floor rise");
    }

    #[test]
    fn a_snapshot_releases_its_slot_on_drop() {
        let db = Db::with_options(DbOptions {
            max_readers: 2,
            ..Default::default()
        });
        let a = db.snapshot().unwrap();
        let b = db.snapshot().unwrap();
        assert_eq!(db.live_readers(), 2);
        assert!(
            db.snapshot().is_err(),
            "a full registry must refuse, not overflow"
        );
        drop(a);
        assert!(db.snapshot().is_ok(), "the slot is reusable");
        drop(b);
    }

    #[test]
    fn pruning_keeps_live_snapshots_readable() {
        let db = Db::new();
        db.insert_many(1, &[1, 2, 3]).unwrap();
        let old = db.snapshot().unwrap();
        for i in 4..20u64 {
            db.insert(1, i).unwrap();
        }
        db.prune();
        assert_eq!(
            old.cardinality(1).unwrap(),
            3,
            "pruning must not disturb a live reader"
        );
        drop(old);
        db.prune();
        assert_eq!(db.snapshot().unwrap().cardinality(1).unwrap(), 19);
    }

    #[test]
    fn concurrent_writers_across_shards_all_land() {
        let db = Db::with_options(DbOptions {
            shards: 8,
            ..Default::default()
        });
        let mut handles = Vec::new();
        for t in 0..8u64 {
            let db = db.clone();
            handles.push(std::thread::spawn(move || {
                for i in 0..100u64 {
                    db.insert(t * 1000 + i, i).unwrap();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let s = db.snapshot().unwrap();
        for t in 0..8u64 {
            for i in 0..100u64 {
                assert!(
                    s.contains(t * 1000 + i, i).unwrap(),
                    "lost write t={t} i={i}"
                );
            }
        }
    }

    #[test]
    fn concurrent_multi_shard_batches_do_not_deadlock() {
        // Ascending lock order is the reason this terminates.
        let db = Db::with_options(DbOptions {
            shards: 8,
            ..Default::default()
        });
        let mut handles = Vec::new();
        for t in 0..8u64 {
            let db = db.clone();
            handles.push(std::thread::spawn(move || {
                for i in 0..50u64 {
                    let mut b = db.batch();
                    // Deliberately reversed on odd threads, so the batches would
                    // acquire in opposing orders if the code did not sort.
                    let keys: Vec<u64> = if t % 2 == 0 {
                        (0..6).map(|k| k * 7 + i).collect()
                    } else {
                        (0..6).rev().map(|k| k * 7 + i).collect()
                    };
                    for k in keys {
                        b.insert(k, t);
                    }
                    b.commit().unwrap();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let s = db.snapshot().unwrap();
        assert!(s.cardinality(0).unwrap() > 0);
    }

    /// `store_set` must replay to exactly what it committed.
    ///
    /// **This is the only thing standing between a latent divergence and a real
    /// one.** `Op::PutChunk` replaces live ( `Memtable::put_chunk` ) and unions
    /// on replay ( `RecType::ChunkImage` ), and they agree solely because
    /// `store_set` emits a `DeleteKey` ahead of them. Remove that op and the
    /// committed state and the recovered state differ — silently, and only
    /// after a crash. No existing test covered the pair, because every other
    /// test either checkpoints first or never reopens.
    #[test]
    fn store_set_replays_to_what_it_committed() {
        let dir = tmpdir("store_set_replay");
        let _clean = CleanDir(dir.clone());

        // The key must be *populated first*, and with ordinals the replacement
        // does not contain: unioning into an empty key is indistinguishable
        // from replacing it, which is why this hole stayed open.
        let replacement = OrdSet::from_iter_unsorted([3u64, 4, 1 << 16]);
        let committed = {
            let db = Db::open_with(&dir, DbOptions::default()).unwrap();
            db.insert_many(1, &[1, 2, 3, 7 << 16]).unwrap();
            let mut b = db.batch();
            b.store_set(1, &replacement);
            b.commit().unwrap();
            let got = db.snapshot().unwrap().load(1).unwrap();
            assert_eq!(
                got.iter().collect::<Vec<_>>(),
                vec![3, 4, 1 << 16],
                "the committed state must be the replacement alone"
            );
            got
        };

        // Reopened without a checkpoint, so this is WAL replay and nothing else.
        let db = Db::open_with(&dir, DbOptions::default()).unwrap();
        let replayed = db.snapshot().unwrap().load(1).unwrap();
        assert_eq!(
            replayed, committed,
            "replay disagreed with the commit: PutChunk unions on replay and \
             replaces live, and only store_set's leading DeleteKey reconciles them"
        );
    }

    /// Interleaving order must not change what a batch commits.
    ///
    /// # Why an agreement test rather than a behaviour test
    ///
    /// `commit` reorders operations ( stable, by key ) so that `plan_ops` can
    /// coalesce same-key runs, which is worth **10x** on document-major input.
    /// A reordering optimization is exactly the kind that stays correct on every
    /// fixture written before it and breaks on the one nobody wrote, so what is
    /// pinned here is that the two orders **agree** -- not that either produces
    /// some expected set.
    ///
    /// The shape matters and is not incidental: keys **descend** in the
    /// document-major arm, so `keys_ascending` is false and the sort actually
    /// runs; and each key gets an insert *and* a later remove of the same
    /// ordinal, so a sort that were not **stable** would reverse them and change
    /// the answer. A fixture of inserts alone would pass against an unstable
    /// sort.
    #[test]
    fn interleaving_order_does_not_change_what_a_batch_commits() {
        fn build(dir: &std::path::Path, doc_major: bool) -> Vec<Vec<u64>> {
            let db = Db::open_with(
                dir,
                DbOptions {
                    shards: 4,
                    ..Default::default()
                },
            )
            .unwrap();
            let keys: Vec<u64> = (0..8u64).rev().collect(); // descending
            let mut b = db.batch();
            if doc_major {
                for i in 0..40u64 {
                    for &k in &keys {
                        b.insert(k, (k << 20) | i);
                        if i % 7 == 0 {
                            b.remove(k, (k << 20) | i);
                        }
                    }
                }
            } else {
                for &k in &keys {
                    for i in 0..40u64 {
                        b.insert(k, (k << 20) | i);
                        if i % 7 == 0 {
                            b.remove(k, (k << 20) | i);
                        }
                    }
                }
            }
            b.commit().unwrap();
            let snap = db.snapshot().unwrap();
            keys.iter()
                .map(|&k| snap.load(k).unwrap().iter().collect())
                .collect()
        }

        let d1 = tmpdir("order-doc");
        let d2 = tmpdir("order-key");
        let _c1 = CleanDir(d1.clone());
        let _c2 = CleanDir(d2.clone());

        let doc = build(&d1, true);
        let key = build(&d2, false);
        assert_eq!(doc, key, "commit must not depend on interleaving order");
        // Non-vacuity: the remove arm must actually have removed something.
        assert!(
            doc.iter().all(|v| !v.is_empty()) && doc.iter().any(|v| v.len() < 40),
            "fixture must exercise both insert and remove: {doc:?}"
        );
    }

    /// `merge_set` must union, where `store_set` replaces.
    ///
    /// The pair is tested together because the whole of the difference is one
    /// `DeleteKey` op, and a merge that accidentally kept it would look correct
    /// on an empty key and silently destroy data on a populated one.
    #[test]
    fn merge_set_unions_where_store_set_replaces() {
        let dir = tmpdir("merge_set");
        let _clean = CleanDir(dir.clone());
        let db = Db::open_with(&dir, DbOptions::default()).unwrap();

        db.insert_many(1, &[1, 2, 3]).unwrap();
        db.insert_many(2, &[1, 2, 3]).unwrap();

        // Overlapping and disjoint, and spanning a second chunk, so the
        // per-chunk loop is exercised rather than a single-chunk special case.
        let incoming = OrdSet::from_iter_unsorted([3u64, 4, 5, 1 << 16]);

        let mut b = db.batch();
        b.merge_set(1, &incoming);
        b.commit().unwrap();
        let mut b = db.batch();
        b.store_set(2, &incoming);
        b.commit().unwrap();

        let snap = db.snapshot().unwrap();
        assert_eq!(
            snap.load(1).unwrap().iter().collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5, 1 << 16],
            "merge_set must keep what was already there"
        );
        assert_eq!(
            snap.load(2).unwrap().iter().collect::<Vec<_>>(),
            vec![3, 4, 5, 1 << 16],
            "store_set must still replace"
        );
    }

    /// The union has to survive replay, not just the memtable.
    ///
    /// `RecType::ChunkImage` has always been applied with a per-ordinal insert,
    /// so this is the assertion that the record really carries union semantics
    /// rather than `merge_set` having merged in memory and logged a replace.
    #[test]
    fn a_merged_set_survives_a_reopen() {
        let dir = tmpdir("merge_set_reopen");
        let _clean = CleanDir(dir.clone());
        let want: Vec<u64> = vec![1, 2, 3, 4, 5, 1 << 16];
        {
            let db = Db::open_with(&dir, DbOptions::default()).unwrap();
            db.insert_many(1, &[1, 2, 3]).unwrap();
            let mut b = db.batch();
            b.merge_set(1, &OrdSet::from_iter_unsorted([3u64, 4, 5, 1 << 16]));
            b.commit().unwrap();
        }
        // Reopened without a checkpoint, so the answer comes from WAL replay.
        {
            let db = Db::open_with(&dir, DbOptions::default()).unwrap();
            let snap = db.snapshot().unwrap();
            assert_eq!(snap.load(1).unwrap().iter().collect::<Vec<_>>(), want);
            db.checkpoint().unwrap();
        }
        // And again from the checkpointed index rather than the log.
        {
            let db = Db::open_with(&dir, DbOptions::default()).unwrap();
            let snap = db.snapshot().unwrap();
            assert_eq!(snap.load(1).unwrap().iter().collect::<Vec<_>>(), want);
        }
    }

    /// It is one batch, so it is one atomic unit with everything beside it.
    ///
    /// This is what the read-modify-write it replaces could not offer: `load`,
    /// union and `store_set` from the caller leaves a window where a concurrent
    /// writer's insert is read and then overwritten.
    #[test]
    fn merge_set_composes_with_the_rest_of_its_batch() {
        let dir = tmpdir("merge_set_batch");
        let _clean = CleanDir(dir.clone());
        let db = Db::open_with(&dir, DbOptions::default()).unwrap();
        db.insert_many(7, &[10, 20]).unwrap();

        let before = db.snapshot().unwrap().load(7).unwrap();

        let mut b = db.batch();
        b.merge_set(7, &OrdSet::from_iter_unsorted([30u64]))
            .insert(7, 40)
            .remove(7, 10)
            .merge_set(8, &OrdSet::from_iter_unsorted([1u64, 2]));
        b.commit().unwrap();

        let snap = db.snapshot().unwrap();
        assert_eq!(
            snap.load(7).unwrap().iter().collect::<Vec<_>>(),
            vec![20, 30, 40]
        );
        assert_eq!(snap.load(8).unwrap().iter().collect::<Vec<_>>(), vec![1, 2]);
        // The earlier snapshot is unmoved, which is what "atomic" has to mean.
        assert_eq!(before.iter().collect::<Vec<_>>(), vec![10, 20]);
    }

    /// I8 is enforced at the same boundary `store_set` enforces it.
    #[test]
    fn merging_an_out_of_range_ordinal_is_rejected_rather_than_written() {
        let dir = tmpdir("merge_set_range");
        let _clean = CleanDir(dir.clone());
        let db = Db::open_with(&dir, DbOptions::default()).unwrap();
        db.insert(1, 5).unwrap();

        // `OrdSet::insert` debug-asserts I8, so the only way to hold a set
        // carrying `u64::MAX` is to assemble it from chunks — which is exactly
        // the hole the check in `merge_set` exists to close, since a release
        // build reaches it through the ordinary insert too.
        let top_prefix = (1u64 << 48) - 1;
        let bad = OrdSet::from_chunks(vec![(
            top_prefix,
            crate::Container::from_sorted(&[u16::MAX]),
        )]);
        assert_eq!(bad.max(), Some(u64::MAX));
        let mut b = db.batch();
        b.merge_set(1, &bad);
        let err = b.commit().unwrap_err();
        assert!(
            matches!(err, CodecError::OrdinalOutOfRange { .. }),
            "got {err:?}"
        );
        // Rejected means nothing was written, not that some of it was.
        let snap = db.snapshot().unwrap();
        assert_eq!(snap.load(1).unwrap().iter().collect::<Vec<_>>(), vec![5]);

        // An empty merge is a no-op rather than an error or a delete.
        let mut b = db.batch();
        b.merge_set(1, &OrdSet::new());
        b.commit().unwrap();
        let snap = db.snapshot().unwrap();
        assert_eq!(snap.load(1).unwrap().iter().collect::<Vec<_>>(), vec![5]);
    }

    fn tmpdir(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("yesno-db-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    struct CleanDir(std::path::PathBuf);
    impl Drop for CleanDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The property that separates "usable" from "a database".
    #[test]
    fn data_survives_a_checkpoint_and_reopen() {
        let dir = tmpdir("durable");
        let _c = CleanDir(dir.clone());
        let opts = DbOptions {
            shards: 4,
            ..Default::default()
        };

        let vals: Vec<u64> = vec![0, 5, 65_536, 65_540, 1 << 40, (1 << 40) + 9];
        {
            let db = Db::open_with(&dir, opts).unwrap();
            assert!(db.is_durable());
            for key in 0..20u64 {
                db.insert_many(key, &vals).unwrap();
            }
            let w = db.checkpoint().unwrap();
            assert!(w > 0, "the checkpoint must advance past the empty floor");
        }

        let db = Db::open_with(&dir, opts).unwrap();
        let s = db.snapshot().unwrap();
        for key in 0..20u64 {
            assert_eq!(
                s.cardinality(key).unwrap(),
                vals.len() as u64,
                "key {key} lost its chunks"
            );
            assert_eq!(
                s.load(key).unwrap().iter().collect::<Vec<_>>(),
                vals,
                "key {key} contents differ"
            );
            for &v in &vals {
                assert!(s.contains(key, v).unwrap(), "key {key} lost ordinal {v}");
            }
        }
    }

    /// The reopen test above passes with **inline** chunks only.
    ///
    /// Its `vals` spread six ordinals across three chunks, two each, and a chunk
    /// of two lives inside the index leaf. So it never stored an extent, never
    /// read one back, and could not see that every checkpoint was writing its
    /// index nodes on top of the extents it had just written ( see
    /// `checkpoint::AllocSource` ). Reading back a chunk that is **too large to
    /// inline** is the property that was actually missing.
    #[test]
    fn data_survives_a_reopen_for_chunks_too_large_to_inline() {
        let dir = tmpdir("durable-extent");
        let _c = CleanDir(dir.clone());
        let opts = DbOptions {
            shards: 2,
            ..Default::default()
        };

        // One case per storage path, since they allocate differently:
        // packed page, standalone extent, and bitmap.
        let packed: Vec<u64> = (0..40u64).map(|i| i * 3).collect();
        let standalone: Vec<u64> = (0..4_000u64).map(|i| 1 + i * 3).collect();
        let dense: Vec<u64> = (0..40_000u64).collect();

        {
            let db = Db::open_with(&dir, opts).unwrap();
            db.insert_many(1, &packed).unwrap();
            db.insert_many(2, &standalone).unwrap();
            db.insert_many(3, &dense).unwrap();
            db.checkpoint().unwrap();
        }

        let db = Db::open_with(&dir, opts).unwrap();
        let s = db.snapshot().unwrap();
        for (key, want) in [(1u64, &packed), (2, &standalone), (3, &dense)] {
            // Assert on *contents*. Cardinality comes from the index and was
            // correct throughout the bug, which is how it stayed hidden.
            assert_eq!(
                s.load(key).unwrap().iter().collect::<Vec<_>>(),
                *want,
                "key {key} came back with different contents after a reopen"
            );
            assert!(
                s.contains(key, want[want.len() / 2]).unwrap(),
                "key {key} lost a middle ordinal"
            );
        }
    }

    /// Extents and index nodes must never be handed the same cell.
    ///
    /// This is the invariant the `mem::take`n allocator violated. Asserting it
    /// directly means a future refactor that reintroduces a second allocator
    /// fails here, with a clear reason, rather than in a reopen test three
    /// layers away.
    #[test]
    fn a_checkpoint_never_allocates_a_node_and_an_extent_at_the_same_cell() {
        let dir = tmpdir("no-overlap");
        let _c = CleanDir(dir.clone());
        let db = Db::open_with(
            &dir,
            DbOptions {
                shards: 1,
                ..Default::default()
            },
        )
        .unwrap();

        for key in 0..40u64 {
            db.insert_many(
                key,
                &(0..200u64).map(|i| key * 1_000 + i * 3).collect::<Vec<_>>(),
            )
            .unwrap();
        }
        db.checkpoint().unwrap();

        // Every chunk must read back byte-for-byte after a reopen. Overlap
        // between the two allocation paths corrupts one or the other, so
        // full-fidelity readback is a sufficient witness.
        drop(db);
        let db = Db::open_with(
            &dir,
            DbOptions {
                shards: 1,
                ..Default::default()
            },
        )
        .unwrap();
        let s = db.snapshot().unwrap();
        for key in 0..40u64 {
            let want: Vec<u64> = (0..200u64).map(|i| key * 1_000 + i * 3).collect();
            assert_eq!(
                s.load(key).unwrap().iter().collect::<Vec<_>>(),
                want,
                "key {key} corrupted"
            );
        }
    }

    #[test]
    fn writes_after_reopen_continue_above_the_durable_floor() {
        let dir = tmpdir("resume");
        let _c = CleanDir(dir.clone());
        let opts = DbOptions {
            shards: 2,
            ..Default::default()
        };

        let floor = {
            let db = Db::open_with(&dir, opts).unwrap();
            db.insert(1, 100).unwrap();
            db.checkpoint().unwrap()
        };

        let db = Db::open_with(&dir, opts).unwrap();
        assert_eq!(
            db.visible(),
            floor,
            "the watermark resumes at the durable floor"
        );
        db.insert(1, 200).unwrap();
        assert!(db.visible() > floor, "new commits continue above it");

        let s = db.snapshot().unwrap();
        assert!(
            s.contains(1, 100).unwrap(),
            "the persisted ordinal is still there"
        );
        assert!(s.contains(1, 200).unwrap(), "and the new one too");
    }

    #[test]
    fn a_delete_survives_a_checkpoint() {
        // Tombstones must reach disk as an absence, not be forgotten and let the
        // old value reappear.
        let dir = tmpdir("delete");
        let _c = CleanDir(dir.clone());
        let opts = DbOptions {
            shards: 2,
            ..Default::default()
        };
        {
            let db = Db::open_with(&dir, opts).unwrap();
            db.insert_many(1, &[1, 2, 3]).unwrap();
            db.checkpoint().unwrap();
            assert!(db.remove(1, 2).unwrap());
            db.checkpoint().unwrap();
        }
        let db = Db::open_with(&dir, opts).unwrap();
        let s = db.snapshot().unwrap();
        assert!(s.contains(1, 1).unwrap());
        assert!(
            !s.contains(1, 2).unwrap(),
            "the deleted ordinal must not reappear"
        );
        assert!(s.contains(1, 3).unwrap());
        assert_eq!(s.cardinality(1).unwrap(), 2);
    }

    #[test]
    fn a_second_checkpoint_carries_untouched_keys_forward() {
        // The tree is rebuilt whole, not patched, so a key absent from the
        // memtable must still be carried into the new index.
        let dir = tmpdir("carry");
        let _c = CleanDir(dir.clone());
        let opts = DbOptions {
            shards: 2,
            ..Default::default()
        };
        {
            let db = Db::open_with(&dir, opts).unwrap();
            for key in 0..10u64 {
                db.insert_many(key, &[1, 2, 3]).unwrap();
            }
            db.checkpoint().unwrap();
            // Touch only one key, then checkpoint again.
            db.insert(0, 99).unwrap();
            db.checkpoint().unwrap();
        }
        let db = Db::open_with(&dir, opts).unwrap();
        let s = db.snapshot().unwrap();
        assert_eq!(
            s.cardinality(0).unwrap(),
            4,
            "the touched key gained its ordinal"
        );
        for key in 1..10u64 {
            assert_eq!(
                s.cardinality(key).unwrap(),
                3,
                "untouched key {key} was dropped"
            );
        }
    }

    /// A snapshot must not observe a checkpoint taken after it.
    #[test]
    fn a_snapshot_is_isolated_across_a_checkpoint() {
        let dir = tmpdir("isolate");
        let _c = CleanDir(dir.clone());
        let db = Db::open_with(
            &dir,
            DbOptions {
                shards: 2,
                ..Default::default()
            },
        )
        .unwrap();
        db.insert_many(1, &[1, 2, 3]).unwrap();
        db.checkpoint().unwrap();

        let old = db.snapshot().unwrap();
        db.insert(1, 4).unwrap();
        db.checkpoint().unwrap();

        assert_eq!(
            old.cardinality(1).unwrap(),
            3,
            "the older snapshot must not see the new write"
        );
        assert!(!old.contains(1, 4).unwrap());
        assert_eq!(db.snapshot().unwrap().cardinality(1).unwrap(), 4);
    }

    /// An abandoned commit version must be resolved, or nothing after it is
    /// ever visible again.
    ///
    /// The visible watermark advances over a **consecutive** run of resolved
    /// versions, so one `Pending` slot left behind by a failed commit is not a
    /// lost write — it is a permanently stalled database, and a recovery that
    /// discards every acknowledged commit above the hole.
    ///
    /// `mvcc::abort` has existed since M3 and was called from nowhere. This
    /// exercises the path `WriteBatch::commit` now takes when anything between
    /// version assignment and `shard_durable` fails.
    #[test]
    fn an_abandoned_version_is_aborted_rather_than_stalling_the_watermark() {
        let dir = tmpdir("abort");
        let _c = CleanDir(dir.clone());
        let db = Db::open_with(
            &dir,
            DbOptions {
                shards: 1,
                ..Default::default()
            },
        )
        .unwrap();

        db.insert(1, 10).unwrap();
        let before = db.visible();

        // Take a version and abandon it, exactly as a failed commit would.
        let orphan = db.inner.oracle.begin(1).expect("ring has room").0;
        assert_eq!(
            db.visible(),
            before,
            "an unresolved version must not be visible"
        );

        // A later commit cannot become visible while the hole is open — that is
        // the prefix rule, and it is why this must be resolved rather than left.
        db.insert(2, 20).unwrap();
        assert!(
            db.visible() < orphan,
            "the watermark must stall at the hole, not step over it"
        );

        db.abort_version(orphan, &[0]);

        assert!(
            db.visible() > orphan,
            "aborting must release the watermark past the hole"
        );
        let snap = db.snapshot().unwrap();
        assert!(snap.contains(1, 10).unwrap());
        assert!(
            snap.contains(2, 20).unwrap(),
            "the commit behind the hole must appear"
        );
    }

    /// The abort must be on disk, or recovery re-opens the hole.
    #[test]
    fn an_abort_is_logged_so_recovery_does_not_reopen_the_hole() {
        let dir = tmpdir("abort-logged");
        let _c = CleanDir(dir.clone());
        {
            let db = Db::open_with(
                &dir,
                DbOptions {
                    shards: 1,
                    ..Default::default()
                },
            )
            .unwrap();
            db.insert(1, 10).unwrap();
            let orphan = db.inner.oracle.begin(1).expect("ring has room").0;
            db.abort_version(orphan, &[0]);
            db.insert(2, 20).unwrap();
        }

        // Reopening replays the log. The aborted version has to be resolvable
        // from it, or `plan` stops at the hole and drops the commit above it.
        let db = Db::open_with(
            &dir,
            DbOptions {
                shards: 1,
                ..Default::default()
            },
        )
        .unwrap();
        let snap = db.snapshot().unwrap();
        assert!(
            snap.contains(1, 10).unwrap(),
            "the commit below the abort was lost"
        );
        assert!(
            snap.contains(2, 20).unwrap(),
            "the commit above the abort was discarded; the Abort record did not survive"
        );
    }

    /// The reader registry: the oldest live snapshot pins `safe_version`.
    ///
    /// These three properties used to be tested against `mvcc::SnapshotRegistry`,
    /// a second registry that `Db` never used and structurally could not — its
    /// lease borrows the registry, so it cannot back a `'static` `Snapshot`.
    /// Deleting the duplicate would have deleted its coverage, so the tests moved
    /// to the registry that actually runs.
    #[test]
    fn the_oldest_live_reader_pins_safe_version() {
        let db = Db::new();
        db.insert(1, 1).unwrap();
        let floor = db.safe_version();

        let a = db.snapshot().unwrap();
        db.insert(1, 2).unwrap();
        let b = db.snapshot().unwrap();
        assert_eq!(db.live_readers(), 2);
        assert!(
            db.safe_version() <= a.version(),
            "the oldest reader must pin the floor"
        );

        let a_version = a.version();
        drop(a);
        assert!(
            db.safe_version() >= a_version,
            "releasing the oldest reader must let the floor rise"
        );
        drop(b);
        assert_eq!(db.live_readers(), 0);
        assert!(db.safe_version() >= floor);
    }

    /// A full registry refuses rather than running a reader unregistered.
    ///
    /// An unregistered reader makes `safe_version` a lie, and reclamation then
    /// frees extents it is still reading — silently, because an unregistered
    /// reader is indistinguishable from no reader at all.
    #[test]
    fn a_full_registry_refuses_rather_than_running_a_reader_unregistered() {
        let db = Db::with_options(DbOptions {
            max_readers: 2,
            ..Default::default()
        });
        let _a = db.snapshot().unwrap();
        let _b = db.snapshot().unwrap();
        assert!(
            db.snapshot().is_err(),
            "the third reader must be refused, not run unregistered"
        );
    }

    /// `safe_version` never overtakes the oldest live reader, however far the
    /// database advances. This is the property extent reclamation rests on.
    #[test]
    fn safe_version_never_exceeds_the_oldest_reader() {
        let db = Db::new();
        db.insert(1, 1).unwrap();
        let held: Vec<_> = (0..5)
            .map(|i| {
                db.insert(1, 100 + i).unwrap();
                db.snapshot().unwrap()
            })
            .collect();
        let oldest = held[0].version();

        for i in 0..50u64 {
            db.insert(2, i).unwrap();
        }
        assert!(
            db.safe_version() <= oldest,
            "safe_version overtook the oldest live reader"
        );

        drop(held);
        assert!(db.safe_version() >= oldest);
    }

    #[test]
    fn an_in_memory_database_reports_itself_as_such() {
        let db = Db::new();
        assert!(!db.is_durable());
        assert_eq!(db.checkpoint().unwrap(), 0, "checkpointing is a no-op");
    }

    #[test]
    fn snapshot_is_send_and_static() {
        fn assert_send<T: Send + 'static>(_: T) {}
        let db = Db::new();
        db.insert(1, 1).unwrap();
        assert_send(db.snapshot().unwrap());
    }

    /// A record can be in the WAL before its commit version is visible because
    /// group fsync happens after the shard write lock is released. A checkpoint
    /// may seal that byte, but its image does not include it and must not reclaim
    /// it or advance `wal_replay_lsn` past it.
    #[test]
    fn checkpoint_retains_a_wal_record_above_its_visible_watermark() {
        let dir = tmpdir("checkpoint-pending-wal");
        let _cleanup = CleanDir(dir.clone());
        let db = Db::open_with(
            &dir,
            DbOptions {
                shards: 1,
                ..Default::default()
            },
        )
        .unwrap();
        db.insert(7, 11).unwrap();
        assert_eq!(db.visible(), 1);

        let wal = db.inner.shards[0].wal.as_ref().unwrap();
        let mut pending_lsn = 0;
        let pending_end = wal
            .append_with(|writer| {
                pending_lsn = writer.append(RecType::ChunkDelta, 2, db.term64(), vec![0; 24])?;
                Ok(())
            })
            .unwrap();
        wal.sync_through(pending_end).unwrap();

        db.checkpoint().unwrap();
        assert_eq!(
            db.inner.shards[0]
                .store
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .superblock()
                .wal_replay_lsn,
            pending_lsn,
            "the checkpoint skipped an uncheckpointed record"
        );
        assert!(wal.bytes() > 0, "the pending generation was reclaimed");

        drop(db);
        let reopened = Db::open(&dir).unwrap();
        assert!(reopened.snapshot().unwrap().contains(7, 11).unwrap());
    }
}
