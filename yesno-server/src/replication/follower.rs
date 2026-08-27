//! The follower half: bootstrap physically, then catch up by **recovering**.
//!
//! # Why there is no apply loop here
//!
//! A follower does not decode records and reapply them. It writes the frames it
//! receives into its own `shard-NNNN.wal`, byte for byte, and opens a `Db` — at
//! which point ordinary crash recovery replays them. That is the entire
//! argument for shipping raw WAL bytes instead of wrapping them in Flight:
//! **one framing, one decoder, one fuzz target**. A second decoder is a second
//! thing that can drift, and drift here is a replica that silently disagrees
//! with its leader.
//!
//! So what is left for this module is transport and bookkeeping: put the bytes
//! in the right place, refuse a gap, and track how far along the follower is.
//!
//! # What the bookkeeping is for
//!
//! [`yesno_core::repl::Follower`] runs **the leader's own watermark
//! algorithm** — the shared commit table and the same consecutive-prefix rule —
//! so a multi-shard commit becomes visible on the follower only once every
//! participant's records have arrived. That is what preserves multi-shard
//! atomicity end to end rather than re-deriving it, and it is what an `Ack`
//! reports.
//!
//! Until this module existed, that type had no caller: the crate shipped only
//! [`super::LeaderService`], and the M7 gate hand-rolled a follower inside the
//! test. The gate therefore exercised the test's follower rather than any
//! shipped one — the same "the subject is test code" shape this project has hit
//! repeatedly.
//!
//! # Bootstrap is physical
//!
//! The leader's `.yno` is copied as-is, byte for byte — but **not hole for
//! hole**. It is **consistent by construction** ( I4: a checkpoint persists
//! only state at or below the visible watermark ), so there is no hot-backup
//! protocol and no torn-page problem, and the follower is queryable as soon as
//! the bytes land rather than after a parse-and-rebuild proportional to the
//! dataset.
//!
//! The image is a **sparse** file: the store grows it to a whole 1 GiB mmap
//! segment with `set_len`, so a database holding kilobytes reports gigabytes.
//! The leader skips the runs that are entirely zero and reports the apparent
//! length; this end sets the file to that length before writing, so the skipped
//! runs come back as holes. The result is content-identical and differently
//! allocated. Skipping them is not an optimization for the follower's disk
//! alone: shipping them cost the *apparent* size in the leader's memory too,
//! which meant a replica asking to join could OOM-kill the leader it was
//! joining.
//!
//! **A copy is either absent or whole, and never in between.** The image is
//! written beside its final name and renamed onto it, because `resume_from_disk`
//! reads a shard image's *existence* as the claim that this shard has a base —
//! and writing in place makes that claim false for the whole duration of a
//! copy. A standby interrupted mid-bootstrap then came back to a file it could
//! not parse and retried it for ever, never reaching the code that would have
//! replaced it. The rename is the single commit point, which is also why the
//! log is emptied *before* it: every interruption then lands on a consistent
//! pair, either the old image with its own log or the new image with an empty
//! one. Found by a live standby on 2026-09-05 whose volume filled mid-copy,
//! not by review — and note what the old shape cost: the *disk* being full was
//! reported for three minutes as a failure to fill a buffer.

use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use tonic::transport::Channel;
use yesno_core::mvcc::Version;
use yesno_core::repl::{Follower, WalCursor};

use super::pb::replication_client::ReplicationClient;
use super::{from_wire, pb};

/// Why a catch-up did not complete.
#[derive(Debug)]
pub enum FollowerError {
    /// The leader's transport failed.
    Transport(tonic::Status),
    /// Local disk.
    Io(std::io::Error),
    /// The shipped stream did not continue where this follower left off, or a
    /// batch failed its own checksum.
    Stream(yesno_core::CodecError),
    /// The leader described a topology this follower cannot serve.
    Topology(String),
    /// The service on the other end is not the database this follower replicates.
    WrongLeader { expected: [u8; 16], found: [u8; 16] },
    /// The endpoint is the right *database* at an **older leadership**.
    ///
    /// This is the zombie: a leader that died, was replaced, and came back.
    /// It carries the same `db_uuid` as the node that replaced it — it is a copy
    /// of the same database — so identity alone cannot tell them apart, and a
    /// follower that trusted identity alone would apply its records on top of
    /// the new timeline and diverge with nothing reporting it.
    StaleLeader { seen: u32, offered: u32 },
}

fn hex(id: &[u8; 16]) -> String {
    id.iter().map(|b| format!("{b:02x}")).collect()
}

impl std::fmt::Display for FollowerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FollowerError::Transport(s) => write!(f, "leader transport failed: {s}"),
            FollowerError::Io(e) => write!(f, "follower disk: {e}"),
            FollowerError::Stream(e) => write!(f, "shipped log rejected: {e}"),
            FollowerError::Topology(m) => write!(f, "{m}"),
            FollowerError::StaleLeader { seen, offered } => write!(
                f,
                "that endpoint is serving term {offered} and this follower has already seen \
                 term {seen}; it is an older leadership of the same database, and following \
                 it would apply records from a timeline that has been replaced"
            ),
            FollowerError::WrongLeader { expected, found } => write!(
                f,
                "this follower replicates {} but the leader is {}",
                hex(expected),
                hex(found)
            ),
        }
    }
}

impl std::error::Error for FollowerError {}

impl From<tonic::Status> for FollowerError {
    fn from(s: tonic::Status) -> Self {
        FollowerError::Transport(s)
    }
}
impl From<std::io::Error> for FollowerError {
    fn from(e: std::io::Error) -> Self {
        FollowerError::Io(e)
    }
}
impl From<yesno_core::CodecError> for FollowerError {
    fn from(e: yesno_core::CodecError) -> Self {
        FollowerError::Stream(e)
    }
}

type Result<T> = std::result::Result<T, FollowerError>;

/// Where replay must begin for a shard whose log is empty: the image's own
/// superblock is the only thing that knows, exactly as `FetchBaseSnapshot`
/// computes it on the leader.
fn image_replay_offset(img: &Path) -> Result<u64> {
    use std::io::Read;
    // Not `read_exact`. Both superblock slots are two pages, and a file
    // shorter than that is exactly the case `wal_replay_offset` has a named
    // error for — "shard image is too short to hold a superblock". Reading
    // with `read_exact` fails first, with `failed to fill whole buffer`, so the
    // precise diagnosis the code already contains was unreachable from the one
    // path that produces the condition. A live standby spent three minutes
    // repeating the generic message on 2026-09-05 while its disk was full.
    let mut head = Vec::with_capacity(2 * yesno_core::store::PAGE);
    std::fs::File::open(img)?
        .take(2 * yesno_core::store::PAGE as u64)
        .read_to_end(&mut head)?;
    Ok(yesno_core::wal_replay_offset(&head)?)
}

/// What one shard's catch-up moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaughtUp {
    pub shard: u32,
    /// Bytes appended to the local log.
    pub bytes: u64,
    /// Records the watermark tracker observed.
    pub records: u64,
    /// Where the next `Subscribe` should resume.
    pub next_lsn: u64,
}

/// A follower's local state: where its files are, and how far it has caught up.
pub struct FollowerClient {
    dir: PathBuf,
    tracker: Follower,
    /// The leader identity this client has bound itself to.
    ///
    /// In memory and deliberately *not* the whole story: the local MANIFEST
    /// is authoritative when there is one, and it is re-read per check rather
    /// than captured at construction, because the operational order is create
    /// the directory, seed the MANIFEST, *then* bootstrap. Capturing it in
    /// `new` would see nothing and bind to whatever leader answered first.
    ///
    /// This covers the remaining case: a follower with no MANIFEST yet adopts
    /// the first leader it reaches, and a second, different leader within the
    /// same client is then refused.
    adopted: Option<[u8; 16]>,
    /// The leadership term last observed. Reporting only — the fence itself
    /// reads the MANIFEST, so that it survives this object.
    term: u32,
}

impl FollowerClient {
    /// A follower over `dir`, resuming each shard from the given cursor.
    ///
    /// `visible_at` is the version the local image is already good for — 0 for
    /// a follower that will be shipped the whole log from the start, which is
    /// idempotent because replay ignores records at or below the checkpoint the
    /// image carries.
    pub fn new(
        dir: impl AsRef<Path>,
        visible_at: Version,
        cursors: impl IntoIterator<Item = WalCursor>,
    ) -> Self {
        FollowerClient {
            dir: dir.as_ref().to_path_buf(),
            tracker: Follower::new(visible_at, cursors),
            adopted: None,
            term: 0,
        }
    }

    /// Rebuild a follower's cursors from what is already on its disk.
    ///
    /// **Without this, a standby restart loses its place.**
    /// [`FollowerClient::new`] takes cursors in memory, so a process that
    /// restarts and passes none resumes from zero — which is below the leader's
    /// retained base after its first checkpoint, so the leader answers
    /// `FailedPrecondition` and the standby re-bootstraps the whole database.
    /// Correct, and a full image copy per restart.
    ///
    /// The next LSN is the end of the active file after every retained sealed
    /// generation. A shard with no log yet answers with the offset its base
    /// image recorded, and one with no image at all is simply absent — which is
    /// what tells the caller to bootstrap it.
    ///
    /// The returned tracker's `visible()` starts at **0** and climbs as
    /// batches arrive. That is a *reporting* lower bound, not a correctness
    /// problem: what a promoted standby actually holds is decided by `Db::open`
    /// replaying its logs, not by this counter. It is worth knowing when reading
    /// lag right after a restart.
    pub fn resume_from_disk(dir: impl AsRef<Path>, shards: u32) -> Result<Self> {
        let dir = dir.as_ref();
        let mut cursors = Vec::new();
        for shard in 0..shards {
            let wal = dir.join(format!("shard-{shard:04}.wal"));
            let img = dir.join(format!("shard-{shard:04}.yno"));
            if !img.exists() {
                // No base image: this shard has never been bootstrapped, and a
                // cursor for it would be a claim about state that is not there.
                continue;
            }
            // **An image this standby cannot read is not a fatal condition,
            // it is a shard to bootstrap.** Returning `Err` here aborts the
            // whole pass before it reaches the code that would replace the
            // file, so the standby retries the identical bytes for ever and
            // never serves again — which is what a follower interrupted
            // mid-bootstrap used to do. `bootstrap_shard` now renames a
            // complete image into place, so this should no longer be
            // reachable from an interruption; it stays because the file can
            // also be damaged by things this process did not do, and because a
            // standby already holding a bad image has to be able to heal.
            //
            // The safe direction, and it does cost something: a genuinely
            // corrupt superblock re-copies the whole shard rather than
            // reporting. That is why it is loud.
            let fallback = match image_replay_offset(&img) {
                Ok(offset) => offset,
                Err(error) => {
                    tracing::warn!(
                        shard,
                        image = %img.display(),
                        error = %error,
                        "unusable base image; this shard will be bootstrapped again"
                    );
                    continue;
                }
            };
            let next_lsn = yesno_core::wal::log_bounds(&wal, fallback)?.1;
            cursors.push(WalCursor { shard, next_lsn });
        }
        Ok(FollowerClient {
            dir: dir.to_path_buf(),
            tracker: Follower::new(0, cursors),
            adopted: None,
            term: 0,
        })
    }

    /// The leadership term last observed from the leader.
    pub fn term(&self) -> u32 {
        self.term
    }

    /// Versions at or below this are complete on the follower.
    ///
    /// Not "bytes received": a multi-shard commit counts only once every
    /// participant's records are in hand, which is the leader's own rule.
    pub fn visible(&self) -> Version {
        self.tracker.visible()
    }

    /// How far behind `leader_visible` this follower is, in versions.
    pub fn lag_versions(&self, leader_visible: Version) -> u64 {
        self.tracker.lag_versions(leader_visible)
    }

    /// Where a reconnect should resume this shard.
    pub fn cursor(&self, shard: u32) -> Option<WalCursor> {
        self.tracker.cursor(shard)
    }

    fn shard_path(&self, shard: u32, ext: &str) -> PathBuf {
        self.dir.join(format!("shard-{shard:04}.{ext}"))
    }

    /// The LSN this follower's own log file starts at.
    ///
    /// Asked of the file, exactly as `WalWriter::open` does, so the follower and
    /// the `Db` that will replay these bytes agree by construction rather than
    /// by two copies of a rule. A log that already has a first record answers
    /// for itself; an empty one starts wherever the next batch does, which is
    /// what a fresh bootstrap leaves behind.
    fn local_base(&self, shard: u32, file: &mut std::fs::File) -> Result<u64> {
        let len = file.metadata()?.len();
        if len == 0 {
            // Not zero. A bootstrap set the tracker's cursor to the offset the
            // base image carries, and the first frame that arrives will be at
            // that LSN — so the file's byte 0 is that LSN, not the origin.
            return Ok(self.tracker.cursor(shard).map_or(0, |c| c.next_lsn));
        }
        Ok(yesno_core::wal::read_first_frame(file, len)?
            .as_deref()
            .and_then(yesno_core::wal::Record::peek_lsn)
            .unwrap_or(0))
    }

    /// Refuse to exchange bytes with a service that is not this follower's
    /// leader.
    ///
    /// # Why this is called rather than offered
    ///
    /// `Status` has reported `db_uuid` since M7 "so the follower must be able to
    /// tell leaders apart", and until 2026-08-28 **nothing compared it**. The
    /// mistake surfaced only at `Db::open`, where `ShardStore::open` checks the
    /// superblock against the MANIFEST — a real safety net, and the last one, by
    /// which point a foreign image is already on disk.
    ///
    /// A public `verify_leader` an operator must remember to call is a rule
    /// that gets forgotten, which is the same shape as the identity that was
    /// written everywhere and read nowhere. So every method that writes bytes
    /// calls this first, and there is no way to skip it.
    ///
    /// It costs a `Status` round trip per call, and per call is the point: the
    /// client is a *parameter*, so nothing stops a caller passing leader A on one
    /// call and leader B on the next. Verifying once would check the wrong thing.
    /// Against an established HTTP/2 channel it is sub-millisecond, next to a
    /// call that ships a log.
    ///
    /// A follower with no MANIFEST yet — a genuinely empty directory, which is
    /// where a bootstrap legitimately starts — **adopts** the first leader it
    /// reaches rather than being blocked. That is the case that made folding the
    /// check into `bootstrap_shard` look impossible; adopting resolves it without
    /// leaving the check optional.
    async fn check_leader(&mut self, client: &mut ReplicationClient<Channel>) -> Result<()> {
        let st = client.status(pb::StatusRequest {}).await?.into_inner();
        let offered_term = st.term;
        let found: [u8; 16] = st.db_uuid.try_into().map_err(|_| {
            FollowerError::Topology("the leader reported no usable database identity".to_owned())
        })?;

        // The MANIFEST outranks anything adopted: it is what the replaying `Db`
        // will check the shard images against.
        if let Ok(expected) = yesno_core::database_uuid(&self.dir) {
            if expected != found {
                return Err(FollowerError::WrongLeader { expected, found });
            }
            self.adopted = Some(expected);
        } else {
            match self.adopted {
                Some(expected) if expected != found => {
                    return Err(FollowerError::WrongLeader { expected, found })
                }
                Some(_) => {}
                None => self.adopted = Some(found),
            }
        }

        // **The identity check above cannot catch a zombie.** A promoted
        // standby is a *copy* of the same database, so it carries the same
        // `db_uuid` as the leader it replaced — and a revived old leader
        // therefore passes every test up to this point. The term is what tells
        // two leaderships of one database apart.
        //
        // The highest term seen is kept on disk, in this follower's own
        // MANIFEST, because the whole point is that it survives a restart: a
        // fence held only in memory is no fence at all against the case that
        // matters, which is a process coming back.
        let seen = yesno_core::database_term(&self.dir).unwrap_or(0);
        if offered_term < seen {
            return Err(FollowerError::StaleLeader {
                seen,
                offered: offered_term,
            });
        }
        if offered_term > seen && yesno_core::database_term(&self.dir).is_ok() {
            // Adopt forward. Only ever upward — `promote_database` refuses a
            // term that does not raise, so this cannot walk a follower back.
            let _ = yesno_core::promote_database(&self.dir, offered_term);
        }
        self.term = offered_term;
        Ok(())
    }

    /// Take the leader's MANIFEST, so this follower routes keys the way the
    /// leader does.
    ///
    /// **The step an operator gets wrong, and until now the only
    /// implementation of it anywhere was a `std::fs::copy` in a test.** The
    /// MANIFEST carries the shard count and the `vshard -> shard` map as well as
    /// the identity; a follower given only the identity routes keys to shards
    /// that do not hold them, and a follower given nothing invents its own and
    /// becomes a different database. `db/manifest.rs` records what that cost
    /// when it happened locally: **31 of 64 keys readable, silently**.
    ///
    /// Idempotent. A follower that already has one keeps it and verifies it
    /// against the leader, because the local MANIFEST is authoritative — the
    /// same rule `check_leader` follows — and silently replacing it
    /// would be how a follower gets repointed at a different database by
    /// accident.
    pub async fn fetch_manifest(
        &mut self,
        client: &mut ReplicationClient<Channel>,
    ) -> Result<[u8; 16]> {
        if let Ok(mine) = yesno_core::database_uuid(&self.dir) {
            // Already seeded: this is a check, not a fetch.
            self.check_leader(client).await?;
            self.adopted = Some(mine);
            return Ok(mine);
        }

        let bytes = client
            .fetch_manifest(pb::ManifestRequest {})
            .await?
            .into_inner()
            .manifest;
        if bytes.is_empty() {
            return Err(FollowerError::Topology(
                "the leader returned an empty MANIFEST".to_owned(),
            ));
        }

        std::fs::create_dir_all(&self.dir)?;
        // Write-then-rename. A torn MANIFEST is not a file the next open can
        // repair: it carries the routing map, so a half-written one is a
        // database that answers for the wrong shards. The two-slot format
        // survives a torn *slot*; it does not survive a torn *create*.
        let tmp = self.dir.join("MANIFEST.tmp");
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, self.dir.join("MANIFEST"))?;

        // Verified through the same public call `check_leader` uses, so a
        // MANIFEST this crate accepts is one `Db::open` will accept too. On
        // failure the file goes away rather than being left for a later open to
        // trip over.
        let mine = match yesno_core::database_uuid(&self.dir) {
            Ok(u) => u,
            Err(e) => {
                let _ = std::fs::remove_file(self.dir.join("MANIFEST"));
                return Err(FollowerError::Topology(format!(
                    "the leader's MANIFEST does not parse: {e:?}"
                )));
            }
        };
        self.check_leader(client).await?;
        self.adopted = Some(mine);
        Ok(mine)
    }

    /// Copy one shard's base image, returning the offset replay must start at.
    ///
    /// Chunks are asserted contiguous rather than assumed: the stream is
    /// ordered by construction on the leader, and a silent gap here would
    /// produce a file that looks whole and decodes as corruption much later.
    pub async fn bootstrap_shard(
        &mut self,
        client: &mut ReplicationClient<Channel>,
        shard: u32,
    ) -> Result<u64> {
        // Before a byte is written, not after: this is the whole point.
        self.check_leader(client).await?;
        let mut chunks = client
            .fetch_base_snapshot(pb::SnapshotRequest { shard })
            .await?
            .into_inner();

        // **Written beside the image and renamed onto it, never into it.**
        // `File::create` truncates, so writing the image in place makes it
        // exist and be unreadable for the whole duration of the copy — and
        // `resume_from_disk` reads `exists()` as "this shard has a base". A
        // standby interrupted mid-bootstrap therefore came back to a short file
        // it could not parse, failed before it could reach the code that would
        // have replaced it, and retried the same file for ever.
        //
        // **Anything that cuts the write short does this, not just a crash.**
        // The live case on 2026-09-05 was a follower whose volume filled during
        // the copy: `write_all` returned `ENOSPC`, and every pass afterwards
        // reported `failed to fill whole buffer` about the remains — naming the
        // reader rather than the disk. With the rename in place the same
        // condition now reports `No space left on device`, repeatedly and
        // honestly, which is what led to the actual cause.
        //
        // A rename within one directory is atomic, so the image is absent or
        // whole and never in between.
        let path = self.shard_path(shard, "yno");
        let partial = self.shard_path(shard, "yno.partial");
        let mut file = std::fs::File::create(&partial)?;
        let mut written = 0u64;
        let mut replay_off = 0u64;
        // The image is sparse and the leader skips its holes, so `offset` is
        // no longer contiguous and the file's length can no longer be inferred
        // from what arrived. Three things replace what contiguity gave for
        // free, and all three are needed: the length, so the runs the leader
        // skipped become holes rather than a truncated file; the ordering and
        // bounds check, so no chunk lands outside the image; and the leader's
        // own count of the bytes it sent, because a *dropped* chunk is
        // otherwise indistinguishable from a hole and would become zeros in the
        // middle of the image.
        let mut length = None;
        let mut expected_data = None;
        let mut next_offset = 0u64;
        while let Some(c) = chunks.message().await? {
            if c.total_len == 0 {
                return Err(FollowerError::Topology(format!(
                    "the leader did not report the length of shard {shard}'s image; \
                     it is too old to bootstrap from"
                )));
            }
            match length {
                None => {
                    // Before a byte is written. Setting the length is what
                    // makes every run the leader skipped read as zero.
                    file.set_len(c.total_len)?;
                    length = Some(c.total_len);
                }
                Some(seen) if seen != c.total_len => {
                    return Err(FollowerError::Topology(format!(
                        "shard {shard}'s image changed length mid-stream, {seen} then {}",
                        c.total_len
                    )));
                }
                Some(_) => {}
            }
            if c.offset < next_offset {
                return Err(FollowerError::Topology(format!(
                    "snapshot chunk for shard {shard} arrived at offset {}, behind {next_offset}",
                    c.offset
                )));
            }
            let end = c
                .offset
                .checked_add(c.data.len() as u64)
                .filter(|end| *end <= c.total_len)
                .ok_or_else(|| {
                    FollowerError::Topology(format!(
                        "snapshot chunk for shard {shard} at offset {} runs past the image",
                        c.offset
                    ))
                })?;
            if !c.data.is_empty() {
                file.seek(SeekFrom::Start(c.offset))?;
                file.write_all(&c.data)?;
                written += c.data.len() as u64;
            }
            next_offset = end;
            if c.last {
                replay_off = c.wal_replay_off;
                expected_data = Some(c.data_len);
            }
        }
        file.sync_all()?;
        // Removed rather than left: a `.partial` is not wrong — nothing
        // reads it — but it would accumulate one per failed attempt on a
        // standby that is retrying.
        let discard = |error| {
            let _ = std::fs::remove_file(&partial);
            error
        };
        // A stream that ends without its marker is a truncated stream, and
        // used to be accepted: whatever had arrived was written, the image was
        // renamed into place, and the shard was quietly short.
        let Some(expected_data) = expected_data else {
            return Err(discard(FollowerError::Topology(format!(
                "the base snapshot stream for shard {shard} ended without a final chunk"
            ))));
        };
        if written != expected_data {
            return Err(discard(FollowerError::Topology(format!(
                "the leader sent {expected_data} bytes for shard {shard} and {written} arrived"
            ))));
        }
        if written == 0 {
            return Err(discard(FollowerError::Topology(format!(
                "the leader sent an empty base snapshot for shard {shard}"
            ))));
        }
        // The image replaces this shard wholesale, so whatever the tracker
        // believed about the old one is void. Leaving the old cursor in place
        // made re-bootstrapping — the remedy a stale-cursor error names — fail on
        // the next call with "WAL batch does not continue the cursor", so the
        // one recovery path a follower has did not work through this client.
        self.tracker.reset_shard(shard, replay_off);

        // And so is the old log. Its records sit at LSNs the new image
        // already covers, and its *first* record is what `local_base` and the
        // replaying `Db` both read to learn where this file starts — so keeping
        // it would place every incoming frame at an offset computed from the
        // wrong base and splice two generations into one file. Emptying it makes
        // the base the image's `replay_off`, which is what the next frame will
        // carry.
        //
        // **Before the rename, and that ordering is the crash argument.**
        // The rename is this operation's single commit point, so every
        // interruption has to land on a consistent pair. Emptying the log first
        // means a crash before the rename leaves the *old* image with an empty
        // log — the standby resumes from that image's own replay offset and
        // re-fetches, which is correct and merely slower. Emptying it after
        // would leave the *new* image beside the old generation's log, which is
        // precisely the splice this comment exists to prevent.
        let wal = self.shard_path(shard, "wal");
        yesno_core::wal::remove_log_generations(&wal)?;
        std::fs::File::create(&wal)?.sync_all()?;

        std::fs::rename(&partial, &path)?;
        // The rename itself has to reach the disk, or a crash can leave the
        // directory entry pointing at neither name.
        std::fs::File::open(&self.dir)?.sync_all()?;
        Ok(replay_off)
    }

    /// Stream one shard's frames into the local log until the leader is caught
    /// up, and return what moved.
    ///
    /// A heartbeat means "you have everything I have" and ends the pass.
    ///
    /// Every batch goes through [`Follower::apply`] **before** it is written,
    /// so a batch that does not continue the cursor, or fails its checksum, is
    /// refused rather than laid down. A gap written to the log would silently
    /// skip records and stall the prefix watermark for ever, waiting on
    /// versions that were never delivered.
    pub async fn catch_up_shard(
        &mut self,
        client: &mut ReplicationClient<Channel>,
        shard: u32,
        after_lsn: u64,
        max_batch_bytes: u32,
    ) -> Result<CaughtUp> {
        self.check_leader(client).await?;
        let mut stream = client
            .subscribe(pb::SubscribeRequest {
                shard,
                after_lsn,
                max_batch_bytes,
            })
            .await?
            .into_inner();

        let path = self.shard_path(shard, "wal");
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)?;

        // **The LSN is not the file offset here, and treating it as one was a
        // latent bug waiting for the leader to checkpoint.** A shipped record
        // carries the LSN the leader wrote it at, measured from that shard's
        // first record ever; this follower's file starts wherever its own
        // bootstrap left off. Seeking to the LSN would punch a hole the size of
        // everything the leader had ever reclaimed — a sparse multi-gigabyte log
        // whose first record does not sit at byte 0, which its own replay would
        // then refuse.
        let local_base = self.local_base(shard, &mut file)?;

        let (mut bytes, mut records) = (0u64, 0u64);
        while let Some(b) = stream.message().await? {
            if b.is_heartbeat {
                break;
            }
            let batch = from_wire(b);
            let at = batch.first_lsn.checked_sub(local_base).ok_or_else(|| {
                FollowerError::Topology(format!(
                    "shard {shard}: the leader shipped lsn {} below this follower's log base {local_base}",
                    batch.first_lsn
                ))
            })?;
            // Verifies the checksum and that it continues the cursor.
            records += self.tracker.apply(&batch)? as u64;
            file.seek(SeekFrom::Start(at))?;
            file.write_all(&batch.records)?;
            bytes += batch.records.len() as u64;
        }
        // One fsync per pass rather than per batch: a follower that loses its
        // tail simply resubscribes from its cursor, so per-batch durability
        // buys nothing and costs a sync per round trip.
        file.sync_all()?;

        Ok(CaughtUp {
            shard,
            bytes,
            records,
            next_lsn: self.cursor(shard).map_or(after_lsn, |c| c.next_lsn),
        })
    }

    /// Stream one shard's frames straight into an **open** replica.
    ///
    /// The live counterpart of [`Self::catch_up_shard`], and the difference is
    /// where the bytes land. `catch_up_shard` writes the shard's log file
    /// directly, which is only safe while nothing has the database open;
    /// this hands each batch to `Db::apply_wal_batch`, so the frames reach a
    /// database that is **serving reads at the same time**.
    ///
    /// Deliberately not both at once. Two writers to one `shard-NNNN.wal` —
    /// this client and the `Db`'s own `WalWriter`, which keeps `len`, `base` and
    /// `synced` in memory — desynchronise all three, and the failure is a log
    /// that decodes as far as the first foreign byte and then silently stops.
    ///
    /// `check_leader` runs first, so identity **and** the leadership term are
    /// verified before a byte is applied — a live replica gets the same fence a
    /// cold one does.
    pub async fn apply_shard_into(
        &mut self,
        client: &mut ReplicationClient<Channel>,
        db: &yesno_core::Db,
        shard: u32,
        max_batch_bytes: u32,
    ) -> Result<CaughtUp> {
        self.check_leader(client).await?;
        let after_lsn = db.apply_cursor(shard)?;

        let mut stream = client
            .subscribe(pb::SubscribeRequest {
                shard,
                after_lsn,
                max_batch_bytes,
            })
            .await?
            .into_inner();

        let (mut bytes, mut records) = (0u64, 0u64);
        while let Some(b) = stream.message().await? {
            if b.is_heartbeat {
                break;
            }
            let applied = db.apply_wal_batch(&from_wire(b))?;
            bytes += applied.bytes;
            records += applied.records;
            // The tracker mirrors the cursor so `ack` and `lag_versions` keep
            // answering, but the database is the authority on where the log is.
            self.tracker.reset_shard(shard, applied.next_lsn);
        }

        Ok(CaughtUp {
            shard,
            bytes,
            records,
            next_lsn: db.apply_cursor(shard)?,
        })
    }

    /// Tell the leader how far this follower has applied, and learn the lag.
    ///
    /// The leader uses it for its retention floor, so a follower that stops
    /// acking is what makes the leader hold log it would otherwise cut.
    pub async fn ack(&self, client: &mut ReplicationClient<Channel>, shard: u32) -> Result<u64> {
        let applied_lsn = self.cursor(shard).map_or(0, |c| c.next_lsn);
        let resp = client
            .ack(tokio_stream::iter(vec![pb::AckRequest {
                shard,
                applied_lsn,
            }]))
            .await?
            .into_inner();
        Ok(resp.lag_versions)
    }
}
