//! The M3 gate: a systematic crash matrix.
//!
//! Individual crash tests check the cases someone thought of. This sweeps
//! *every* truncation point and *every* corruption offset, which is the only way
//! to be confident about a recovery path — the interesting failures are usually
//! at the boundary nobody enumerated.
//!
//! Two properties are asserted everywhere:
//!
//! 1. **Never panic.** Recovery reads bytes written by a process that died
//!    mid-write; a panic there is an outage, not an error.
//! 2. **Never lose an acknowledged commit.** A commit is acked only after every
//!    version at or below it is resolved and fsynced, so no truncation of the
//!    *tail* can put a hole below one.

use yesno_core::mvcc::Version;
use yesno_core::store::superblock::{self, SuperBlock};
use yesno_core::store::PAGE;
use yesno_core::wal::record::{encode_commit_intent, RecType, Record};
use yesno_core::wal::recover::{plan, ShardLog};

/// Builds one shard's log with byte offsets as LSNs.
struct Log {
    shard: u32,
    bytes: Vec<u8>,
    /// Offset just past each fully-resolved commit version.
    resolved_at: Vec<(Version, usize)>,
}

impl Log {
    fn new(shard: u32) -> Self {
        Log {
            shard,
            bytes: Vec::new(),
            resolved_at: Vec::new(),
        }
    }

    fn push(&mut self, rtype: RecType, cv: Version, body: Vec<u8>) {
        let lsn = self.bytes.len() as u64;
        self.bytes
            .extend_from_slice(&Record::new(rtype, lsn, cv, 1, body).encode());
    }

    fn commit(&mut self, cv: Version) -> &mut Self {
        self.push(RecType::ChunkDelta, cv, vec![cv as u8; 24]);
        self.push(RecType::ShardCommit, cv, vec![]);
        self.resolved_at.push((cv, self.bytes.len()));
        self
    }

    fn multi(&mut self, cv: Version, shards: &[u32], finish: bool) -> &mut Self {
        self.push(RecType::CommitIntent, cv, encode_commit_intent(shards));
        self.push(RecType::ChunkDelta, cv, vec![0u8; 24]);
        if finish {
            self.push(RecType::ShardCommit, cv, vec![]);
            self.resolved_at.push((cv, self.bytes.len()));
        }
        self
    }

    fn log(&self) -> ShardLog<'_> {
        ShardLog {
            shard: self.shard,
            bytes: &self.bytes,
            base_lsn: 0,
        }
    }
}

// ------------------------------------------------------- WAL truncation sweep

#[test]
fn recovery_survives_truncation_at_every_byte() {
    let mut l = Log::new(0);
    for cv in 1..=12 {
        l.commit(cv);
    }
    let full = l.bytes.clone();

    for cut in 0..=full.len() {
        let truncated = &full[..cut];
        let log = ShardLog {
            shard: 0,
            bytes: truncated,
            base_lsn: 0,
        };
        // Must not panic, whatever the cut lands in the middle of.
        let p = plan(&[log], 0).expect("recovery must not fail on a truncated log");

        // Every commit whose records are entirely present must be recovered.
        let fully_present = l
            .resolved_at
            .iter()
            .filter(|(_, end)| *end <= cut)
            .map(|(cv, _)| *cv)
            .max()
            .unwrap_or(0);
        assert_eq!(
            p.global_cv, fully_present,
            "cut at {cut}: expected to recover through {fully_present}, got {}",
            p.global_cv
        );

        // The truncation point can never exceed the bytes we still have.
        assert!(p.truncate_at[&0] <= cut as u64);
    }
}

#[test]
fn recovery_survives_corruption_at_every_byte() {
    let mut l = Log::new(0);
    for cv in 1..=8 {
        l.commit(cv);
    }
    let full = l.bytes.clone();

    for i in 0..full.len() {
        let mut bad = full.clone();
        bad[i] ^= 0xFF;
        let log = ShardLog {
            shard: 0,
            bytes: &bad,
            base_lsn: 0,
        };

        // A flip anywhere must yield an error or a shortened prefix — never a
        // panic, and never a *longer* recovery than the intact log.
        // An Err is legitimate here ( a flip can produce a valid-CRC unknown
        // record type ); what must never happen is recovering *further* than the
        // intact log, or panicking.
        if let Ok(p) = plan(&[log], 0) {
            assert!(
                p.global_cv <= 8,
                "corruption at {i} recovered further than the intact log"
            );
        }
    }
}

/// The safety property, swept rather than sampled.
#[test]
fn no_acknowledged_commit_is_lost_at_any_truncation() {
    let mut l = Log::new(0);
    for cv in 1..=10 {
        l.commit(cv);
    }
    let full = l.bytes.clone();

    for &(acked, end) in &l.resolved_at {
        // Any cut at or after this commit's last byte must still recover it.
        for cut in end..=full.len() {
            let log = ShardLog {
                shard: 0,
                bytes: &full[..cut],
                base_lsn: 0,
            };
            let p = plan(&[log], 0).unwrap();
            assert!(
                p.global_cv >= acked,
                "acked commit {acked} lost at cut {cut} (recovered {})",
                p.global_cv
            );
        }
    }
}

// ------------------------------------------- partial multi-shard batch sweep

#[test]
fn a_multi_shard_batch_is_all_or_nothing_at_every_step() {
    // Two shards participate in version 2. Sweep which of them finished.
    for a_done in [false, true] {
        for b_done in [false, true] {
            let mut a = Log::new(0);
            a.commit(1);
            a.multi(2, &[0, 1], a_done);
            a.commit(3);

            let mut b = Log::new(1);
            b.multi(2, &[0, 1], b_done);

            let p = plan(&[a.log(), b.log()], 0).unwrap();
            let expected = if a_done && b_done { 3 } else { 1 };
            assert_eq!(
                p.global_cv, expected,
                "a_done={a_done} b_done={b_done}: version 2 must be all-or-nothing"
            );

            // Nothing above the watermark may be replayed, ever.
            assert!(
                p.replay
                    .iter()
                    .all(|(_, r)| r.commit_version <= p.global_cv),
                "a_done={a_done} b_done={b_done}: replay leaked past the watermark"
            );
        }
    }
}

#[test]
fn truncating_one_shard_of_a_multi_shard_batch_blocks_the_whole_batch() {
    // Shard 1's log is cut at every point; version 2 must only be recovered
    // once its ShardCommit is fully present on both shards.
    let mut a = Log::new(0);
    a.commit(1);
    a.multi(2, &[0, 1], true);

    let mut b = Log::new(1);
    b.multi(2, &[0, 1], true);
    let b_full = b.bytes.clone();
    let b_complete_at = b_full.len();

    for cut in 0..=b_full.len() {
        let logs = [
            a.log(),
            ShardLog {
                shard: 1,
                bytes: &b_full[..cut],
                base_lsn: 0,
            },
        ];
        let p = plan(&logs, 0).unwrap();
        let expected = if cut >= b_complete_at { 2 } else { 1 };
        assert_eq!(p.global_cv, expected, "shard 1 cut at {cut}");
    }
}

// ------------------------------------------------------ superblock corruption

#[test]
fn a_torn_superblock_slot_always_falls_back_to_the_other() {
    let mut live = SuperBlock::initial([1u8; 16], 0);
    live.seq = 7;
    live.checkpoint_cv = 700;
    let live_bytes = live.encode().unwrap();

    let mut newer = SuperBlock::initial([1u8; 16], 0);
    newer.seq = 8;
    newer.checkpoint_cv = 800;
    let newer_bytes = newer.encode().unwrap();

    // Tear the slot being written at every byte. The older slot must always
    // remain selectable — that is what makes the flip safe without atomic
    // sector writes.
    for i in 0..PAGE {
        let mut torn = newer_bytes.clone();
        torn[i] ^= 0xFF;
        let picked = superblock::pick(&live_bytes, &torn)
            .expect("picking must not fail")
            .expect("one slot is intact, so one must be picked");
        assert!(
            picked.seq == 7 || picked.seq == 8,
            "byte {i}: picked a slot that was never written"
        );
        if picked.seq == 8 {
            // If the torn slot still validated, its contents must be coherent.
            assert_eq!(
                picked.checkpoint_cv, 800,
                "byte {i}: accepted a corrupt image"
            );
        }
    }
}

#[test]
fn superblock_decode_never_panics_on_arbitrary_bytes() {
    let mut seed = 0xF00D_BAAFu64;
    for _ in 0..2000 {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let n = (seed % (PAGE as u64 * 2)) as usize;
        let bytes: Vec<u8> = (0..n).map(|i| (seed >> (i % 56)) as u8).collect();
        let _ = SuperBlock::decode(&bytes);
        let _ = superblock::pick(&bytes, &bytes);
    }
}

// ------------------------------------------------------------ interleavings

#[test]
fn checkpoint_and_commit_records_interleave_safely() {
    // A checkpoint's markers land in the middle of ongoing commits. Recovery
    // must ignore them as replay material while still honouring the versions.
    let mut l = Log::new(0);
    l.commit(1);
    l.push(RecType::CheckpointBegin, 1, vec![]);
    l.commit(2);
    l.push(RecType::CheckpointEnd, 2, vec![]);
    l.commit(3);
    let full = l.bytes.clone();

    for cut in 0..=full.len() {
        let log = ShardLog {
            shard: 0,
            bytes: &full[..cut],
            base_lsn: 0,
        };
        let p = plan(&[log], 0).unwrap();
        assert!(
            p.replay.iter().all(|(_, r)| r.rtype == RecType::ChunkDelta),
            "cut {cut}: a checkpoint marker leaked into the replay set"
        );
    }
}

#[test]
fn an_abort_resolves_at_every_truncation_point() {
    let mut l = Log::new(0);
    l.commit(1);
    l.push(RecType::Abort, 2, vec![]);
    let abort_end = l.bytes.len();
    l.commit(3);
    let full = l.bytes.clone();

    for cut in 0..=full.len() {
        let log = ShardLog {
            shard: 0,
            bytes: &full[..cut],
            base_lsn: 0,
        };
        let p = plan(&[log], 0).unwrap();
        if cut < abort_end {
            assert!(p.global_cv <= 1, "cut {cut}: the abort is not yet durable");
        } else {
            assert!(
                p.global_cv >= 2,
                "cut {cut}: a durable abort must let the watermark pass"
            );
        }
    }
}
