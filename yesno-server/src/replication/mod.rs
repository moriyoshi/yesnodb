//! Async single-leader WAL shipping over plain tonic.
//!
//! # Why not Arrow Flight
//!
//! Flight moves `RecordBatch`es; a WAL is an opaque self-framed binary log.
//! Wrapping it costs an IPC schema message, per-batch FlatBuffer headers and an
//! extra copy on both ends, for no benefit — and batching fights commit latency.
//!
//! The decisive argument is narrower than any of that. **If the leader ships
//! raw on-disk WAL frames, the follower's apply path is byte-identical to crash
//! recovery**: one framing, one decoder, one fuzz target. Flight forces a second
//! framing that can drift from the first, and the two would drift silently.
//!
//! Flight is still the right answer for *query results*, where the payload
//! really is columnar. That is `yesno-flight`, not this crate.
//!
//! # What the leader is
//!
//! A shard directory. The service reads the retained
//! `shard-NNNN.wal.<base-lsn>` generations plus `shard-NNNN.wal`, and slices
//! their logical concatenation on record boundaries with [`WalPublisher`]. It
//! does not need a live `Db` handle and deliberately does not take one, so
//! shipping cannot perturb the writer.
//!
//! # Determinism is at the level of set *contents*, not container *encoding*
//!
//! Records are logical and carry no extent addresses. The follower runs its own
//! allocator, checkpointer and compaction, so it may legitimately hold a Bitmap
//! where the leader holds an Array for the same chunk. `fsck --compare` means
//! set equality, not byte equality — stating this removes a whole category of
//! false divergence alarms.

use std::path::{Path, PathBuf};

use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

use yesno_core::repl::{WalBatch, WalPublisher};

pub mod follower;
pub use follower::{CaughtUp, FollowerClient, FollowerError};

/// The follower's name, as **the transport** established it, placed in a
/// request's extensions by whatever authenticated the call.
///
/// # Why an extension and not a proto field
///
/// `AckRequest` deliberately carries no `follower_id`, and adding one would be
/// worse than leaving the gap: a self-reported identity is a *claim*, and the
/// whole value of per-follower retention is that one follower cannot move
/// another's floor. An identity the transport established is a fact.
///
/// # Why this crate defines it rather than importing it
///
/// This crate must stay usable with no authentication at all — its own tests
/// serve a `LeaderService` on a bare socket, and `--insecure-replication` is a
/// supported deployment. So it declares the *shape* of the identity it can use
/// and never requires one; the party that knows what a principal is inserts it.
/// Same seam as `GuardedFlight`: policy lives where policy is configured, and
/// the service stays honest about having no handshake of its own.
///
/// Absent, every caller collapses to one anonymous entry — see
/// [`RetentionFloor`](yesno_core::repl::RetentionFloor), which then reads acks
/// pessimistically because it must.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FollowerIdentity(pub String);

pub mod pb {
    //! Generated types for `yesno.replication.v1`.
    tonic::include_proto!("yesno.replication.v1");
}

use pb::replication_server::Replication;

/// How much of a shard's log one `WalBatch` may carry, when the client asks for
/// nothing specific. Large enough to amortize the round trip, small enough that
/// a follower starting from zero streams rather than materializes.
pub const DEFAULT_BATCH_BYTES: usize = 256 * 1024;

/// Bytes per `FetchBaseSnapshot` chunk.
const SNAPSHOT_CHUNK: usize = 1 << 20;

/// Ship one shard image, holes and all -- by not shipping the holes.
///
/// # Why this is not a plain file copy
///
/// A shard image is a **sparse** file. The store grows it to a whole 1 GiB mmap
/// segment with `set_len`, so a database holding kilobytes reports gigabytes
/// and occupies almost nothing; the segment comment says so and it is the right
/// design for local storage. Copying it byte for byte was not: it cost the
/// *apparent* size three times over -- `tokio::fs::read` of the whole image
/// into a `Vec<u8>` here, every zero on the wire, and every hole materialized
/// on the follower's disk.
///
/// The leader's memory was the dangerous one. A two-shard bootstrap
/// allocated 2 GiB for a database measured in kilobytes, so a replica asking to
/// join could OOM-kill the leader it was joining. Measured live on
/// 2026-09-05, where the follower's 1 GiB volume filled during its first
/// bootstrap.
///
/// # What it does instead
///
/// Streams a chunk at a time and sends only the chunks that are not entirely
/// zero. That is deliberately *not* the same as "sends only the allocated
/// extents": a run of real, allocated zeros is skipped too, which is correct
/// because the follower sets the file's length first and an unwritten byte
/// reads as zero. The copy is content-identical; only its allocation differs,
/// and the follower's is the better one.
///
/// `SEEK_DATA` would let the leader skip *reading* the holes as well, and is
/// not used: it needs a `libc` call and the unsafe rules that come with it, to
/// save a scan whose cost is memory bandwidth on a one-off operation. Revisit
/// only if a leader's CPU during bootstrap is ever shown to matter.
async fn stream_base_snapshot(
    shard: u32,
    path: &std::path::Path,
    tx: &mpsc::Sender<Result<pb::SnapshotChunk, Status>>,
) -> Result<(), Status> {
    use tokio::io::AsyncReadExt;

    // The image is consistent by construction ( I4 ): a checkpoint persists
    // only state at or below the visible watermark, so there is no hot-backup
    // protocol and no torn-page problem here.
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|e| Status::unavailable(e.to_string()))?;
    let total_len = file
        .metadata()
        .await
        .map_err(|e| Status::unavailable(e.to_string()))?
        .len();

    let mut buffer = vec![0u8; SNAPSHOT_CHUNK];
    let mut offset = 0u64;
    let mut data_len = 0u64;
    let mut replay_off = None;
    while offset < total_len {
        let want = SNAPSHOT_CHUNK.min((total_len - offset) as usize);
        // `read_exact`, because a short read in the middle of a file is not
        // end-of-file and a chunk assembled from one would ship a shortened
        // image at a truncated offset.
        file.read_exact(&mut buffer[..want])
            .await
            .map_err(|e| Status::unavailable(e.to_string()))?;
        let chunk = &buffer[..want];

        if replay_off.is_none() {
            // Was read from the whole file. The superblock is in the first
            // two pages and nothing past them is consulted, so reading a
            // gigabyte to look at eight kilobytes was the whole cost.
            //
            // And not `metadata(wal).len()` — the log's length *now*. A
            // checkpoint changes generation boundaries, so every commit made
            // between that checkpoint and this copy lies below the current end;
            // a follower told to resume there skips all of them, silently, and
            // ends up a replica missing a window's worth of writes with nothing
            // reporting a gap. The image's own superblock is the only thing
            // that knows which records its contents already include.
            replay_off = Some(yesno_core::wal_replay_offset(chunk).map_err(|e| {
                Status::internal(format!(
                    "shard {shard} image carries no usable replay offset: {e:?}"
                ))
            })?);
        }

        if chunk.iter().any(|byte| *byte != 0) {
            data_len += want as u64;
            let message = pb::SnapshotChunk {
                shard,
                offset,
                data: chunk.to_vec(),
                wal_replay_off: 0,
                last: false,
                total_len,
                data_len: 0,
            };
            if tx.send(Ok(message)).await.is_err() {
                return Ok(());
            }
        }
        offset += want as u64;
    }

    let replay_off = replay_off.ok_or_else(|| {
        Status::internal(format!(
            "shard {shard} image is empty and cannot be shipped"
        ))
    })?;
    // The marker. Sending it rather than flagging the last data chunk is what
    // lets the data go out as it is read: with holes skipped, the final chunk
    // carrying bytes can be followed by gigabytes of nothing, and finding that
    // out first would mean holding it until the scan reached the end.
    let _ = tx
        .send(Ok(pb::SnapshotChunk {
            shard,
            offset: total_len,
            data: Vec::new(),
            wal_replay_off: replay_off,
            last: true,
            total_len,
            data_len,
        }))
        .await;
    Ok(())
}

/// Serves one leader's shard directory.
#[derive(Clone, Debug)]
pub struct LeaderService {
    dir: PathBuf,
    shards: usize,
    /// Where `Ack` publishes what its followers still need, so the local
    /// database's checkpoints do not cut it away. `None` when the service has
    /// not been given one — in which case acks report lag and nothing else,
    /// which is what this did for everybody until 2026-08-28.
    retention: Option<yesno_core::repl::RetentionFloor>,
}

impl LeaderService {
    pub fn new(dir: impl AsRef<Path>, shards: usize) -> Self {
        LeaderService {
            dir: dir.as_ref().to_path_buf(),
            shards,
            retention: None,
        }
    }

    /// Serve, and hold log back for the followers that ack.
    ///
    /// Take the floor from the `Db` that owns this directory
    /// ( `Db::retention_floor()` ). It must be *that* database's: the floor is
    /// consulted by its checkpoints, and one belonging to anything else is a
    /// value nothing reads.
    ///
    /// Still not a `Db` handle. The service reads files and takes no lock, so
    /// shipping cannot perturb the writer; a floor is one `Arc<Mutex<BTreeMap>>`
    /// written on `Ack` and drained per checkpoint, which is the narrowest
    /// channel that carries the fact.
    pub fn with_retention(
        dir: impl AsRef<Path>,
        shards: usize,
        floor: yesno_core::repl::RetentionFloor,
    ) -> Self {
        LeaderService {
            dir: dir.as_ref().to_path_buf(),
            shards,
            retention: Some(floor),
        }
    }

    fn wal_path(&self, shard: u32) -> PathBuf {
        self.dir.join(format!("shard-{shard:04}.wal"))
    }

    fn data_path(&self, shard: u32) -> PathBuf {
        self.dir.join(format!("shard-{shard:04}.yno"))
    }

    fn check_shard(&self, shard: u32) -> Result<(), Status> {
        if shard as usize >= self.shards {
            return Err(Status::out_of_range(format!(
                "shard {shard} does not exist; this leader has {}",
                self.shards
            )));
        }
        Ok(())
    }

    /// The LSN of byte 0 of a shard's log, for the case where the log itself
    /// cannot say — which is an empty one, and is exactly what a checkpoint
    /// leaves behind.
    ///
    /// Read from the image's superblock, and only the first two pages of it: a
    /// shard file is megabytes and this is eight kilobytes of it.
    async fn base_if_empty(&self, shard: u32) -> u64 {
        use tokio::io::AsyncReadExt;
        let Ok(mut f) = tokio::fs::File::open(self.data_path(shard)).await else {
            return 0;
        };
        let mut head = vec![0u8; 2 * yesno_core::store::PAGE];
        if f.read_exact(&mut head).await.is_err() {
            return 0;
        }
        yesno_core::wal_replay_offset(&head).unwrap_or(0)
    }

    /// How long a shard's log is, in **LSNs**, without reading it.
    ///
    /// Not `metadata(active).len()`: the logical end follows every sealed
    /// generation and the active generation's own base.
    ///
    /// The *frame*, not a fixed-size prefix of it. `peek_lsn` verifies a CRC
    /// over the whole record and answers `None` on a buffer that stops short, so
    /// a 40-byte probe silently reports base 0 for any log whose first record is
    /// larger — which is most of them, and cost an hour here.
    async fn log_end_lsn(&self, shard: u32) -> Result<u64, Status> {
        Ok(self.log_base_and_end(shard).await?.1)
    }

    /// The LSN of byte 0 of a shard's log, and one past its last byte.
    ///
    /// The pair, because `subscribe` needs both and they come from the same two
    /// cheap probes — `metadata` for the length and the first frame for the
    /// base. Deriving them separately would read that frame twice per poll.
    async fn log_base_and_end(&self, shard: u32) -> Result<(u64, u64), Status> {
        let path = self.wal_path(shard);
        let fallback = self.base_if_empty(shard).await;
        tokio::task::spawn_blocking(move || yesno_core::wal::log_bounds(path, fallback))
            .await
            .map_err(|_| Status::unavailable("WAL catalog task failed"))?
            .map_err(|error| Status::unavailable(error.to_string()))
    }

    /// `want` bytes of a shard's logical log starting at global `lsn`.
    ///
    /// This is the whole point of not reading the file. `subscribe` polls
    /// every 50 ms per shard per follower, and `CheckpointPolicy::wal_bytes`
    /// lets retained generations reach a gigabyte — so a whole-log read here
    /// is tens of gigabytes per second of page-cache traffic for a follower
    /// that is merely *idle*.
    ///
    /// Short reads are normal and not an error: the file is being appended to
    /// concurrently, so the caller gets what was there and asks again.
    async fn read_window(
        path: PathBuf,
        base_if_empty: u64,
        lsn: u64,
        want: usize,
    ) -> yesno_core::Result<Vec<u8>> {
        tokio::task::spawn_blocking(move || {
            yesno_core::wal::read_log_range(path, base_if_empty, lsn, want)
        })
        .await
        .map_err(|_| yesno_core::CodecError::Invariant("WAL read task failed"))?
    }
}

/// Convert a core `WalBatch` to the wire form. The record bytes pass verbatim.
fn to_wire(b: WalBatch) -> pb::WalBatch {
    pb::WalBatch {
        shard: b.shard,
        first_lsn: b.first_lsn,
        last_lsn: b.end_lsn(),
        crc32c: b.crc32c,
        is_heartbeat: b.heartbeat,
        records: b.records,
    }
}

/// And back, so a follower hands the core exactly what the leader sliced.
pub fn from_wire(b: pb::WalBatch) -> WalBatch {
    WalBatch {
        shard: b.shard,
        first_lsn: b.first_lsn,
        crc32c: b.crc32c,
        heartbeat: b.is_heartbeat,
        records: b.records,
    }
}

#[tonic::async_trait]
impl Replication for LeaderService {
    async fn status(
        &self,
        _req: Request<pb::StatusRequest>,
    ) -> Result<Response<pb::StatusResponse>, Status> {
        let mut end_lsn = Vec::with_capacity(self.shards);
        for s in 0..self.shards as u32 {
            end_lsn.push(self.log_end_lsn(s).await?);
        }
        // Was `fs::read(dir/UUID).unwrap_or_default()`, which reached past
        // the crate's API *and* turned a missing identity into sixteen zero
        // bytes — so two databases that both lacked one compared equal. The
        // identity moved into the MANIFEST on 2026-08-28; this asks for it.
        let db_uuid = yesno_core::database_uuid(&self.dir)
            .map(|u| u.to_vec())
            .unwrap_or_default();
        Ok(Response::new(pb::StatusResponse {
            // The leader's visible version is derivable by the follower from the
            // records it receives, using the same watermark rule. Reporting it
            // here would be a second source of truth for the same fact.
            visible_version: 0,
            shard_count: self.shards as u32,
            end_lsn,
            db_uuid,
            // The leadership term, and the case it exists for is the one
            // identity cannot catch: a promoted standby is a *copy*, so it
            // carries the same `db_uuid` as the node it replaced. A follower
            // keeps the highest term it has seen and refuses anything below it,
            // which is what stops a revived old leader being followed back onto
            // a timeline that no longer exists.
            //
            // Read from the MANIFEST per call rather than captured at
            // construction: `promote_database` raises it while nothing has the
            // database open, and a service built before a promotion would
            // otherwise keep serving the superseded number.
            term: yesno_core::database_term(&self.dir).unwrap_or(0),
        }))
    }

    type SubscribeStream = ReceiverStream<Result<pb::WalBatch, Status>>;

    async fn subscribe(
        &self,
        req: Request<pb::SubscribeRequest>,
    ) -> Result<Response<Self::SubscribeStream>, Status> {
        let req = req.into_inner();
        self.check_shard(req.shard)?;
        let path = self.wal_path(req.shard);
        let max = if req.max_batch_bytes == 0 {
            DEFAULT_BATCH_BYTES
        } else {
            req.max_batch_bytes as usize
        };

        let svc = self.clone();
        let (tx, rx) = mpsc::channel(8);
        tokio::spawn(async move {
            let mut next = req.after_lsn;
            loop {
                // The retained base and logical end span every generation. A
                // single file length is neither quantity after the first roll.
                let (base, end) = match svc.log_base_and_end(req.shard).await {
                    Ok(bounds) => bounds,
                    Err(error) => {
                        let _ = tx.send(Err(error)).await;
                        return;
                    }
                };

                // The log only grows, so a follower ahead of it is a follower
                // talking to the wrong leader — report rather than wrap around.
                if next > end {
                    let _ = tx
                        .send(Err(Status::out_of_range(format!(
                            "follower is at {next}, past this leader's {end}"
                        ))))
                        .await;
                    return;
                }
                // Below the base is the other end of the same mistake, and it is
                // the *recoverable* one: the leader reclaimed a generation this
                // follower still wanted. `FailedPrecondition` names the remedy.
                if next < base {
                    let _ = tx
                        .send(Err(Status::failed_precondition(format!(
                            "shard {} cursor {next} is below this leader's retained log, \
                             which now starts at {base}; the follower must bootstrap again",
                            req.shard,
                        ))))
                        .await;
                    return;
                }

                // Only the window the batch could possibly need.
                let window = match Self::read_window(path.clone(), base, next, max).await {
                    Ok(w) => w,
                    Err(e) => {
                        let _ = tx.send(Err(Status::unavailable(e.to_string()))).await;
                        return;
                    }
                };
                // One regrow, for the record that is bigger than the budget.
                // `batch_from` ships such a record whole rather than stalling
                // ( a budget below one record still makes progress ), and it can
                // only do that if the record is actually in front of it.
                let window = match yesno_core::wal::Record::peek_framed_len(&window) {
                    Some(total) if total > window.len() => {
                        match Self::read_window(path.clone(), base, next, total).await {
                            Ok(w) => w,
                            Err(e) => {
                                let _ = tx.send(Err(Status::unavailable(e.to_string()))).await;
                                return;
                            }
                        }
                    }
                    _ => window,
                };

                // The window starts exactly at `next`, so that is its base.
                let batch = match WalPublisher::new(req.shard, &window, next).batch_from(next, max)
                {
                    Ok(b) => b,
                    Err(e) => {
                        let _ = tx.send(Err(Status::internal(format!("{e:?}")))).await;
                        return;
                    }
                };

                // A heartbeat with log still ahead of us is **ambiguous**, and
                // over a window it is undecidable: `names_a_record` walks from
                // the publisher's base, and this publisher's base *is* `next`, so
                // it answers "yes" for free and every cursor looks like a
                // boundary. The two cases it must separate are:
                //
                //   * the record at `next` is half-written — a leader mid-append
                //     leaves that behind and it heals on the next poll;
                //   * `next` is not a boundary at all — the cursor came from a
                //     corrupt or foreign history, and answering "you have
                //     everything I have" leaves it permanently, silently
                //     behind.
                //
                // Only a walk from the log's true base tells them apart, so pay
                // for the whole retained log here — on the path a healthy
                // follower takes only when it has genuinely caught up, where
                // `next == end` and this is skipped.
                let batch = if batch.heartbeat && next < end {
                    let bytes =
                        match Self::read_window(path.clone(), base, base, (end - base) as usize)
                            .await
                        {
                            Ok(b) => b,
                            Err(e) => {
                                let _ = tx.send(Err(Status::unavailable(e.to_string()))).await;
                                return;
                            }
                        };
                    match WalPublisher::over_log(req.shard, &bytes, base).batch_from(next, max) {
                        Ok(b) => b,
                        // A cursor inside the log but off a record boundary is a
                        // *stale* cursor, not a broken leader. `FailedPrecondition`
                        // is the operator-facing difference between "re-bootstrap
                        // this follower" and "the leader is sick".
                        Err(yesno_core::CodecError::WalCursorNotOnRecordBoundary(at)) => {
                            let _ = tx
                                .send(Err(Status::failed_precondition(format!(
                                    "shard {} cursor {at} does not name a record in this log; \
                                     the log was cut by a checkpoint and the follower must \
                                     bootstrap again",
                                    req.shard
                                ))))
                                .await;
                            return;
                        }
                        Err(e) => {
                            let _ = tx.send(Err(Status::internal(format!("{e:?}")))).await;
                            return;
                        }
                    }
                } else {
                    batch
                };
                let caught_up = batch.heartbeat;
                next = batch.end_lsn();
                if tx.send(Ok(to_wire(batch))).await.is_err() {
                    return; // follower hung up
                }
                if caught_up {
                    // Nothing new; wait before looking again rather than
                    // spinning on the file.
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            }
        });
        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn ack(
        &self,
        req: Request<Streaming<pb::AckRequest>>,
    ) -> Result<Response<pb::AckResponse>, Status> {
        // Read **before** `into_inner` consumes the request. The extensions
        // go with it, and an identity read afterwards would silently be `None`
        // for every call — which fails soft, as anonymous, and would look
        // exactly like a correctly unauthenticated deployment.
        let who = req
            .extensions()
            .get::<FollowerIdentity>()
            .map(|i| i.0.clone())
            .unwrap_or_default();
        let mut stream = req.into_inner();
        let mut behind = 0u64;
        while let Some(a) = stream.message().await? {
            self.check_shard(a.shard)?;
            // Published *before* the lag is computed and regardless of it.
            // The lag is what the follower learns; this is what the leader does
            // about it, and until 2026-08-28 the handler did only the first —
            // the design names "applied-LSN acks -> lag + retention floor" and
            // the floor was nowhere.
            if let Some(r) = self.retention.as_ref() {
                r.observe_from(&who, a.shard, a.applied_lsn);
            }
            let end = self.log_end_lsn(a.shard).await?;
            behind = behind.max(end.saturating_sub(a.applied_lsn));
        }
        // Reported in bytes of un-applied log. Versions would need the leader to
        // decode its own log to answer, which is work the follower already did.
        Ok(Response::new(pb::AckResponse {
            lag_versions: behind,
        }))
    }

    type FetchBaseSnapshotStream = ReceiverStream<Result<pb::SnapshotChunk, Status>>;

    async fn fetch_base_snapshot(
        &self,
        req: Request<pb::SnapshotRequest>,
    ) -> Result<Response<Self::FetchBaseSnapshotStream>, Status> {
        let req = req.into_inner();
        self.check_shard(req.shard)?;
        let data = self.data_path(req.shard);

        let (tx, rx) = mpsc::channel(4);
        tokio::spawn(async move {
            if let Err(status) = stream_base_snapshot(req.shard, &data, &tx).await {
                let _ = tx.send(Err(status)).await;
            }
        });
        Ok(Response::new(ReceiverStream::new(rx)))
    }

    /// Hand over the MANIFEST so a follower can route keys the way this leader
    /// does.
    ///
    /// Without this a follower cannot be bootstrapped across a network at
    /// all, and the failure is silent. `FetchBaseSnapshot` ships shard images
    /// and `Status` carries the identity and the shard count — but the
    /// `vshard -> shard` map is in neither, and a follower given the identity
    /// without the map routes keys to shards that do not hold them. Every
    /// replication test in this tree passed for months because both ends shared
    /// a disk and the seeding step was a `std::fs::copy` in a helper.
    ///
    /// Reconstructing the map from `uuid + shard_count` happens to work while
    /// it is `v % shards`, which is exactly the property `db/manifest.rs` exists
    /// to stop depending on. The recorded cost of getting this wrong is **31 of
    /// 64 keys readable, silently**.
    ///
    /// The **file**, both slots, verbatim — not a decoded shape. The reader
    /// picks the valid slot with the higher `seq` exactly as a local open does,
    /// so there is one rule for choosing rather than two that can drift.
    async fn fetch_manifest(
        &self,
        _req: Request<pb::ManifestRequest>,
    ) -> Result<Response<pb::ManifestResponse>, Status> {
        let manifest = tokio::fs::read(self.dir.join("MANIFEST"))
            .await
            .map_err(|e| {
                // `failed_precondition`, not `internal`: a leader with no
                // MANIFEST is a directory that has never been opened as a
                // database, and the fix is on the leader rather than in a retry.
                Status::failed_precondition(format!(
                    "this leader has no MANIFEST to hand over ( {e} ). It has not been \
                     opened as a database, so it has no routing map to replicate."
                ))
            })?;
        Ok(Response::new(pb::ManifestResponse { manifest }))
    }
}
