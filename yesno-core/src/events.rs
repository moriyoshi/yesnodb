//! Typed operational facts emitted by the storage engine.
//!
//! # Observation is not durability
//!
//! Database state is made durable by the page store and WAL. These events are
//! deliberately not another WAL record type: local process lifecycle does not
//! belong in the replicated data stream, and a failed WAL cannot also be the
//! only place allowed to report its own failure.
//!
//! The core owns only typed facts and a synchronous observer. It owns no
//! serialization, clock, queue, or transport; `yesnod` adds those at the
//! control-plane boundary. Observer failure must never change database
//! semantics, so callbacks are best-effort and a panicking observer is isolated.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Process-local identifier shared by the phases of one operation.
pub type OperationId = u64;

static NEXT_OPERATION_ID: AtomicU64 = AtomicU64::new(1);

/// Allocate a process-local operation identifier.
pub(crate) fn next_operation_id() -> OperationId {
    NEXT_OPERATION_ID.fetch_add(1, Ordering::Relaxed)
}

/// How a durable database was opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DatabaseMode {
    Writer,
    Replica,
    Reader,
}

/// Why an orderly shutdown was started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShutdownReason {
    Requested,
    Promotion,
    Demotion,
    Rebuild,
}

/// Stable, coarse classification for an operation failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorClass {
    Io,
    Corruption,
    Contention,
    Invariant,
    Unsupported,
    Other,
}

/// A failure suitable for an operational event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventError {
    pub class: ErrorClass,
    /// Diagnostic only. Consumers must not branch on this text.
    pub detail: String,
}

impl EventError {
    pub(crate) fn from_codec(error: &crate::CodecError) -> Self {
        use crate::CodecError;
        let class = match error {
            CodecError::AlreadyOpen(_) => ErrorClass::Contention,
            CodecError::UnsupportedEndianness | CodecError::UnsupportedEncoding => {
                ErrorClass::Unsupported
            }
            CodecError::BadLength { .. }
            | CodecError::BadCardinality(_)
            | CodecError::BadRunCount(_)
            | CodecError::OutOfBounds { .. }
            | CodecError::UnknownKind(_)
            | CodecError::DatabaseIdentityMismatch
            | CodecError::ManifestUnreadable
            | CodecError::BadCookie(_)
            | CodecError::Truncated { .. } => ErrorClass::Corruption,
            CodecError::Invariant(message) if message.contains("I/O") => ErrorClass::Io,
            CodecError::Invariant(_) => ErrorClass::Invariant,
            _ => ErrorClass::Other,
        };
        EventError {
            class,
            detail: error.to_string(),
        }
    }
}

/// A typed storage-engine fact.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CoreEvent {
    DatabaseOpenStarted {
        operation_id: OperationId,
        mode: DatabaseMode,
    },
    DatabaseOpenCompleted {
        operation_id: OperationId,
        mode: DatabaseMode,
        database_uuid: [u8; 16],
        shards: u32,
        epoch: u64,
        term: u32,
        visible: u64,
    },
    DatabaseOpenFailed {
        operation_id: OperationId,
        mode: DatabaseMode,
        error: EventError,
    },
    RecoveryStarted {
        operation_id: OperationId,
        checkpoint_version: u64,
    },
    RecoveryCompleted {
        operation_id: OperationId,
        checkpoint_version: u64,
        recovered_version: u64,
        records_replayed: u64,
        discarded_versions: Vec<u64>,
    },
    RecoveryFailed {
        operation_id: OperationId,
        checkpoint_version: u64,
        error: EventError,
    },
    WalTailTruncated {
        operation_id: OperationId,
        shard: u32,
        old_end_lsn: u64,
        new_end_lsn: u64,
    },
    CheckpointStarted {
        operation_id: OperationId,
        watermark: u64,
        dirty_bytes: u64,
        wal_bytes: u64,
    },
    CheckpointCompleted {
        operation_id: OperationId,
        watermark: u64,
    },
    CheckpointFailed {
        operation_id: OperationId,
        watermark: u64,
        error: EventError,
    },
    /// Space amplification crossed the configured **soft** threshold.
    ///
    /// Observation only: nothing is evicted and no writer stalls. The hard
    /// bound remains the sole intervention, and it is enforced separately by
    /// [`crate::Db::enforce_space_amp`]. This exists so an operator learns that
    /// retention is growing *before* a reader is aborted for it.
    ///
    /// **Edge-triggered.** It fires on the transition into breach and again
    /// only after recovering below the threshold, because the natural place to
    /// evaluate it is every checkpoint and a level-triggered event would emit
    /// once per checkpoint for as long as a reporting query runs.
    SpaceAmpSoftThreshold {
        operation_id: OperationId,
        /// `allocated / ( allocated - deferred )` in **parts per thousand**,
        /// the same ratio the hard bound is expressed in: `2000` is the 2x
        /// bound, `1250` the default soft threshold.
        ///
        /// Not an `f64`, deliberately. `CoreEvent` derives `Eq` so that a
        /// subscriber can compare and deduplicate events, and a float makes
        /// that either impossible or subtly wrong.
        amplification_permille: u32,
        /// The threshold this crossed, in the same units.
        threshold_permille: u32,
        allocated_bytes: u64,
        deferred_bytes: u64,
    },
    /// A snapshot has been open longer than the configured soft age.
    ///
    /// Observation only, like [`CoreEvent::SpaceAmpSoftThreshold`], and for a
    /// different question: *which* reader is holding retention down, before the
    /// space it pins is large enough to notice. Edge-triggered on the oldest
    /// reader crossing the age, not per reader, because the caller evaluates it
    /// on a timer and a per-reader event would repeat.
    ///
    /// There is deliberately no hard-age counterpart. A hard age that ended a
    /// query would be a **second** way to kill one, and the policy decision this
    /// was built under keeps `SpaceAmpPolicy::AbortOldestReader` as the only
    /// intervention.
    SnapshotAgeSoftThreshold {
        operation_id: OperationId,
        oldest_age_secs: u64,
        threshold_secs: u64,
        live_readers: u64,
    },
    /// No snapshot is older than the configured soft age any more.
    SnapshotAgeSoftRecovered {
        operation_id: OperationId,
        threshold_secs: u64,
    },
    /// Space amplification fell back below the soft threshold.
    SpaceAmpSoftRecovered {
        operation_id: OperationId,
        amplification_permille: u32,
        threshold_permille: u32,
    },
    WalGenerationRotated {
        operation_id: OperationId,
        shard: u32,
        old_base_lsn: u64,
        new_base_lsn: u64,
        sealed_through_lsn: u64,
        bytes_reclaimed: u64,
        forced_past_retention: bool,
    },
    WalReclamationDeferred {
        operation_id: OperationId,
        shard: u32,
        end_lsn: u64,
        retention_floor_lsn: u64,
    },
    StorageOperationFailed {
        operation_id: OperationId,
        operation: &'static str,
        shard: Option<u32>,
        error: EventError,
    },
    DatabaseShutdownStarted {
        operation_id: OperationId,
        reason: ShutdownReason,
    },
    DatabaseShutdownCompleted {
        operation_id: Option<OperationId>,
        graceful: bool,
        epoch: u64,
        term: u32,
    },
}

impl CoreEvent {
    /// Identifier shared by related phases, when the event belongs to one.
    pub fn operation_id(&self) -> Option<OperationId> {
        match self {
            CoreEvent::DatabaseShutdownCompleted { operation_id, .. } => *operation_id,
            CoreEvent::DatabaseOpenStarted { operation_id, .. }
            | CoreEvent::DatabaseOpenCompleted { operation_id, .. }
            | CoreEvent::DatabaseOpenFailed { operation_id, .. }
            | CoreEvent::RecoveryStarted { operation_id, .. }
            | CoreEvent::RecoveryCompleted { operation_id, .. }
            | CoreEvent::RecoveryFailed { operation_id, .. }
            | CoreEvent::WalTailTruncated { operation_id, .. }
            | CoreEvent::CheckpointStarted { operation_id, .. }
            | CoreEvent::CheckpointCompleted { operation_id, .. }
            | CoreEvent::CheckpointFailed { operation_id, .. }
            | CoreEvent::SpaceAmpSoftThreshold { operation_id, .. }
            | CoreEvent::SpaceAmpSoftRecovered { operation_id, .. }
            | CoreEvent::SnapshotAgeSoftThreshold { operation_id, .. }
            | CoreEvent::SnapshotAgeSoftRecovered { operation_id, .. }
            | CoreEvent::WalGenerationRotated { operation_id, .. }
            | CoreEvent::WalReclamationDeferred { operation_id, .. }
            | CoreEvent::StorageOperationFailed { operation_id, .. }
            | CoreEvent::DatabaseShutdownStarted { operation_id, .. } => Some(*operation_id),
        }
    }
}

/// Receives storage-engine events.
///
/// Implementations should return quickly. Panics are caught by the engine and
/// cannot abort the storage operation that produced the event.
pub trait CoreEventSink: Send + Sync + 'static {
    fn publish(&self, event: CoreEvent);
}

#[derive(Debug)]
pub(crate) struct NoopEventSink;

impl CoreEventSink for NoopEventSink {
    fn publish(&self, _event: CoreEvent) {}
}

pub(crate) fn noop_sink() -> Arc<dyn CoreEventSink> {
    Arc::new(NoopEventSink)
}

/// Publish without allowing an observer panic to affect database correctness.
pub(crate) fn emit(sink: &Arc<dyn CoreEventSink>, event: CoreEvent) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| sink.publish(event)));
}
