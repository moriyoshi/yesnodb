//! Durable Protobuf events and the shared gRPC control-plane endpoint.
//!
//! The data WAL remains the source of database durability. This journal is a
//! separate source for operational history: append and sync an `EventEnvelope`,
//! reduce it into current state, then broadcast it. A slow subscriber can lose
//! its live cursor without delaying a checkpoint or a role transition, and can
//! resume from the journal by sequence.
//!
//! Lifecycle control and physical replication share one service surface across
//! its TCP and Unix listeners. Ordered, pg_hba-style authorization rows select
//! the connection channel before keeping observation, administration, and
//! whole-database replication as separate capabilities.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use prost::Message;
use tokio::io::AsyncReadExt;
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

use yesno_core::events::{
    CoreEvent, CoreEventSink, DatabaseMode as CoreDatabaseMode, ErrorClass as CoreErrorClass,
};

use crate::auth::{Authenticator, Authorizer, Principal};
use crate::config::{AuthConfig, EndpointCapability, ServerConfig};

/// Generated control-plane wire types.
pub mod pb {
    tonic::include_proto!("yesno.control.v1");
}

const SCHEMA_VERSION: u32 = 1;
const FRAME_MAGIC: [u8; 4] = *b"YEV1";
const FRAME_HEADER: usize = 12;
const MAX_EVENT_BYTES: usize = 1 << 20;
const MAX_JOURNAL_BYTES: usize = 64 << 20;
const SUBSCRIBER_BUFFER: usize = 64;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// A lifecycle command accepted from the control-plane endpoint.
#[derive(Debug)]
pub enum Command {
    Promote {
        correlation_id: Vec<u8>,
    },
    Demote {
        correlation_id: Vec<u8>,
        leader: String,
    },
    Shutdown {
        correlation_id: Vec<u8>,
    },
}

struct Journal {
    file: File,
    dir: PathBuf,
    bytes: usize,
    max_bytes: usize,
}

impl Journal {
    fn open(
        dir: &Path,
        max_bytes: usize,
    ) -> Result<(Journal, Vec<pb::EventEnvelope>, Vec<u8>), BoxError> {
        std::fs::create_dir_all(dir)?;
        let node_id = load_or_create_node_id(&dir.join("NODE_ID"))?;
        let path = dir.join("events.yev");
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }

        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let mut events: Vec<pb::EventEnvelope> = Vec::new();
        let mut at = 0usize;
        let mut truncate_at = None;

        while at < bytes.len() {
            if bytes.len() - at < FRAME_HEADER {
                truncate_at = Some(at);
                break;
            }
            if bytes[at..at + 4] != FRAME_MAGIC {
                return Err(format!("control journal has bad magic at byte {at}").into());
            }
            let len = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
            let expected_crc = u32::from_le_bytes(bytes[at + 8..at + 12].try_into().unwrap());
            if len > MAX_EVENT_BYTES {
                return Err(format!("control journal frame at byte {at} is too large").into());
            }
            let end = at + FRAME_HEADER + len;
            if end > bytes.len() {
                truncate_at = Some(at);
                break;
            }
            let payload = &bytes[at + FRAME_HEADER..end];
            if crc32c::crc32c(payload) != expected_crc {
                if end == bytes.len() {
                    truncate_at = Some(at);
                    break;
                }
                return Err(format!("control journal CRC mismatch at byte {at}").into());
            }
            let event = pb::EventEnvelope::decode(payload)?;
            if event.schema_version != SCHEMA_VERSION {
                return Err(format!(
                    "control journal event {} has unsupported schema version {}",
                    event.sequence, event.schema_version
                )
                .into());
            }
            if let Some(previous) = events.last() {
                let expected = previous.sequence.saturating_add(1);
                if event.sequence != expected {
                    return Err(format!(
                        "control journal sequence {} follows {}, expected {}",
                        event.sequence, previous.sequence, expected
                    )
                    .into());
                }
            }
            events.push(event);
            at = end;
        }

        let valid_bytes = truncate_at.unwrap_or(bytes.len());
        if truncate_at.is_some() {
            file.set_len(valid_bytes as u64)?;
            file.sync_all()?;
        }
        file.seek(std::io::SeekFrom::End(0))?;
        Ok((
            Journal {
                file,
                dir: dir.to_path_buf(),
                bytes: valid_bytes,
                max_bytes,
            },
            events,
            node_id,
        ))
    }

    fn encoded_frame(event: &pb::EventEnvelope) -> Result<(Vec<u8>, [u8; FRAME_HEADER]), BoxError> {
        let payload = event.encode_to_vec();
        if payload.len() > MAX_EVENT_BYTES {
            return Err("control event exceeds the 1 MiB frame limit".into());
        }
        let mut header = [0u8; FRAME_HEADER];
        header[..4].copy_from_slice(&FRAME_MAGIC);
        header[4..8].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        header[8..12].copy_from_slice(&crc32c::crc32c(&payload).to_le_bytes());
        Ok((payload, header))
    }

    fn append(&mut self, event: &pb::EventEnvelope) -> Result<(), BoxError> {
        let (payload, header) = Self::encoded_frame(event)?;
        self.file.write_all(&header)?;
        self.file.write_all(&payload)?;
        self.file.sync_data()?;
        self.bytes += FRAME_HEADER + payload.len();
        Ok(())
    }

    fn should_compact(&self, event: &pb::EventEnvelope) -> bool {
        self.bytes > 0 && self.bytes + FRAME_HEADER + event.encoded_len() > self.max_bytes
    }

    fn replace_with(&mut self, event: &pb::EventEnvelope) -> Result<(), BoxError> {
        let suffix = fresh_id()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let next_path = self.dir.join(format!("events.compact-{suffix}"));
        let mut next = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&next_path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            next.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        let (payload, header) = Self::encoded_frame(event)?;
        next.write_all(&header)?;
        next.write_all(&payload)?;
        next.sync_all()?;
        std::fs::rename(&next_path, self.dir.join("events.yev"))?;
        File::open(&self.dir)?.sync_all()?;
        self.file = next;
        self.bytes = FRAME_HEADER + payload.len();
        Ok(())
    }
}

fn load_or_create_node_id(path: &Path) -> Result<Vec<u8>, BoxError> {
    match std::fs::read(path) {
        Ok(id) if id.len() == 16 => return Ok(id),
        Ok(_) => return Err("control NODE_ID is not 16 bytes".into()),
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error.into()),
        Err(_) => {}
    }
    let id = fresh_id();
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(&id)?;
    file.sync_all()?;
    Ok(id)
}

fn fresh_id() -> Vec<u8> {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mixed = nanos
        ^ ((std::process::id() as u128) << 64)
        ^ COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed) as u128;
    mixed.to_le_bytes().to_vec()
}

fn now() -> prost_types::Timestamp {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    prost_types::Timestamp {
        seconds: duration.as_secs() as i64,
        nanos: duration.subsec_nanos() as i32,
    }
}

struct HubInner {
    journal: Journal,
    events: Vec<pb::EventEnvelope>,
    snapshot: pb::StateSnapshot,
    database: Option<pb::DatabaseIdentity>,
    node_id: Vec<u8>,
    process_instance_id: Vec<u8>,
}

#[derive(Clone)]
struct EventDraft {
    source: pb::EventSource,
    severity: pb::Severity,
    correlation_id: Vec<u8>,
    causation_id: Vec<u8>,
    database: Option<pb::DatabaseIdentity>,
    shard: Option<u32>,
    payload: pb::event_envelope::Payload,
}

impl EventDraft {
    fn envelope(&self, inner: &HubInner, sequence: u64) -> pb::EventEnvelope {
        let mut event_id = inner.process_instance_id.clone();
        event_id.extend_from_slice(&sequence.to_le_bytes());
        pb::EventEnvelope {
            schema_version: SCHEMA_VERSION,
            sequence,
            event_id,
            occurred_at: Some(now()),
            recorded_at: Some(now()),
            source: self.source as i32,
            severity: self.severity as i32,
            node_id: inner.node_id.clone(),
            process_instance_id: inner.process_instance_id.clone(),
            correlation_id: self.correlation_id.clone(),
            causation_id: self.causation_id.clone(),
            database: self.database.clone().or_else(|| inner.database.clone()),
            shard: self.shard,
            payload: Some(self.payload.clone()),
        }
    }
}

/// Ordered durable event journal, state reducer, and live broadcaster.
pub struct EventHub {
    inner: Mutex<HubInner>,
    live: broadcast::Sender<pb::EventEnvelope>,
}

impl EventHub {
    /// Open and recover a control journal, then record this process start.
    pub fn open(dir: impl AsRef<Path>) -> Result<Arc<EventHub>, BoxError> {
        Self::open_bounded(dir, MAX_JOURNAL_BYTES)
    }

    fn open_bounded(
        dir: impl AsRef<Path>,
        max_journal_bytes: usize,
    ) -> Result<Arc<EventHub>, BoxError> {
        let (journal, events, node_id) = Journal::open(dir.as_ref(), max_journal_bytes)?;
        let process_instance_id = fresh_id();
        let mut snapshot = pb::StateSnapshot {
            storage_available: true,
            ..Default::default()
        };
        let mut database = None;
        for event in &events {
            reduce(&mut snapshot, &mut database, event);
        }
        let previous = snapshot
            .process_running
            .then(|| snapshot.process_instance_id.clone());
        let database_interrupted = previous.is_some() && snapshot.database_open;
        let (live, _) = broadcast::channel(256);
        let hub = Arc::new(EventHub {
            inner: Mutex::new(HubInner {
                journal,
                events,
                snapshot,
                database,
                node_id,
                process_instance_id,
            }),
            live,
        });

        if let Some(previous_process_instance_id) = previous {
            hub.publish(
                pb::EventSource::Reconciler,
                pb::Severity::Warning,
                Vec::new(),
                Vec::new(),
                None,
                None,
                pb::event_envelope::Payload::ProcessLifecycle(pb::ProcessLifecycleEvent {
                    operation: pb::process_lifecycle_event::Operation::Interruption as i32,
                    phase: pb::EventPhase::Observed as i32,
                    previous_process_instance_id: Some(previous_process_instance_id),
                    detail: "the previous process recorded no stop event".into(),
                }),
            )?;
        }
        if database_interrupted {
            hub.publish(
                pb::EventSource::Reconciler,
                pb::Severity::Warning,
                Vec::new(),
                Vec::new(),
                None,
                None,
                pb::event_envelope::Payload::DatabaseLifecycle(pb::DatabaseLifecycleEvent {
                    operation: pb::database_lifecycle_event::Operation::Interruption as i32,
                    phase: pb::EventPhase::Observed as i32,
                    graceful: false,
                    reason: "the owning process stopped without closing the database".into(),
                    ..Default::default()
                }),
            )?;
        }
        hub.publish(
            pb::EventSource::Server,
            pb::Severity::Info,
            Vec::new(),
            Vec::new(),
            None,
            None,
            pb::event_envelope::Payload::ProcessLifecycle(pb::ProcessLifecycleEvent {
                operation: pb::process_lifecycle_event::Operation::Start as i32,
                phase: pb::EventPhase::Completed as i32,
                previous_process_instance_id: None,
                detail: String::new(),
            }),
        )?;
        Ok(hub)
    }

    /// Adapter handed to `yesno-core`; Prost remains on this side of the seam.
    pub fn core_sink(self: &Arc<Self>) -> Arc<dyn CoreEventSink> {
        Arc::new(CoreSink { hub: self.clone() })
    }

    /// Current event-sourced projection.
    pub fn snapshot(&self) -> pb::StateSnapshot {
        self.inner.lock().unwrap().snapshot.clone()
    }

    /// Allocate an opaque command correlation identifier.
    pub fn correlation_id(&self, requested: &[u8]) -> Vec<u8> {
        if requested.is_empty() {
            fresh_id()
        } else {
            requested.to_vec()
        }
    }

    /// Append one server/reconciler fact.
    pub fn publish_server(
        &self,
        severity: pb::Severity,
        correlation_id: Vec<u8>,
        causation_id: Vec<u8>,
        payload: pb::event_envelope::Payload,
    ) -> Result<pb::EventEnvelope, BoxError> {
        self.publish(
            pb::EventSource::Server,
            severity,
            correlation_id,
            causation_id,
            None,
            None,
            payload,
        )
    }

    // The envelope is intentionally assembled at this one seam: keeping every
    // identity and ordering field here is safer than letting producers stamp
    // partially-populated envelopes independently.
    #[allow(clippy::too_many_arguments)]
    fn publish(
        &self,
        source: pb::EventSource,
        severity: pb::Severity,
        correlation_id: Vec<u8>,
        causation_id: Vec<u8>,
        database: Option<pb::DatabaseIdentity>,
        shard: Option<u32>,
        payload: pb::event_envelope::Payload,
    ) -> Result<pb::EventEnvelope, BoxError> {
        let draft = EventDraft {
            source,
            severity,
            correlation_id,
            causation_id,
            database,
            shard,
            payload,
        };
        let mut inner = self.inner.lock().unwrap();
        let sequence = inner.events.last().map_or(1, |event| event.sequence + 1);
        let mut event = draft.envelope(&inner, sequence);
        let mut broadcast_events = Vec::with_capacity(2);

        if inner.journal.should_compact(&event) {
            let checkpoint = EventDraft {
                source: pb::EventSource::Reconciler,
                severity: pb::Severity::Info,
                correlation_id: Vec::new(),
                causation_id: Vec::new(),
                database: inner.database.clone(),
                shard: None,
                payload: pb::event_envelope::Payload::ProjectionCheckpoint(
                    pb::ProjectionCheckpointEvent {
                        snapshot: Some(inner.snapshot.clone()),
                        database: inner.database.clone(),
                    },
                ),
            }
            .envelope(&inner, sequence);
            inner.journal.replace_with(&checkpoint)?;
            {
                let HubInner {
                    snapshot, database, ..
                } = &mut *inner;
                reduce(snapshot, database, &checkpoint);
            }
            inner.events.clear();
            inner.events.push(checkpoint.clone());
            broadcast_events.push(checkpoint);
            event = draft.envelope(&inner, sequence + 1);
        }

        inner.journal.append(&event)?;
        {
            let HubInner {
                snapshot, database, ..
            } = &mut *inner;
            reduce(snapshot, database, &event);
        }
        inner.events.push(event.clone());
        drop(inner);
        broadcast_events.push(event.clone());
        for published in broadcast_events {
            let _ = self.live.send(published);
        }
        Ok(event)
    }

    fn subscription(
        &self,
        after_sequence: u64,
    ) -> (
        Vec<pb::EventEnvelope>,
        u64,
        u64,
        broadcast::Receiver<pb::EventEnvelope>,
        pb::StateSnapshot,
    ) {
        let inner = self.inner.lock().unwrap();
        let oldest = inner.events.first().map_or(1, |e| e.sequence);
        let head = inner.events.last().map_or(0, |e| e.sequence);
        let replay = inner
            .events
            .iter()
            .filter(|event| event.sequence > after_sequence)
            .cloned()
            .collect();
        (
            replay,
            oldest,
            head,
            self.live.subscribe(),
            inner.snapshot.clone(),
        )
    }
}

fn reduce(
    snapshot: &mut pb::StateSnapshot,
    database: &mut Option<pb::DatabaseIdentity>,
    event: &pb::EventEnvelope,
) {
    if let Some(identity) = &event.database {
        *database = Some(identity.clone());
        snapshot.term = identity.term;
    }
    match event.payload.as_ref() {
        Some(pb::event_envelope::Payload::ProcessLifecycle(process)) => {
            use pb::process_lifecycle_event::Operation;
            match Operation::try_from(process.operation).unwrap_or(Operation::Unspecified) {
                Operation::Start => snapshot.process_running = true,
                Operation::Stop => snapshot.process_running = false,
                _ => {}
            }
        }
        Some(pb::event_envelope::Payload::DatabaseLifecycle(database_event)) => {
            use pb::database_lifecycle_event::Operation;
            let completed = database_event.phase == pb::EventPhase::Completed as i32;
            match Operation::try_from(database_event.operation).unwrap_or(Operation::Unspecified) {
                Operation::Open if completed => {
                    snapshot.database_open = true;
                    snapshot.visible_version = database_event.visible_version;
                }
                Operation::Shutdown if completed => snapshot.database_open = false,
                Operation::Interruption => snapshot.database_open = false,
                _ => {}
            }
        }
        Some(pb::event_envelope::Payload::RoleTransition(role)) => {
            if role.phase == pb::EventPhase::Completed as i32 {
                snapshot.role = role.to;
                snapshot.term = role.term;
            }
        }
        Some(pb::event_envelope::Payload::Storage(storage)) => {
            if storage.phase == pb::EventPhase::Failed as i32 {
                snapshot.storage_available = false;
            } else if storage.phase == pb::EventPhase::Completed as i32 {
                snapshot.storage_available = true;
            }
        }
        Some(pb::event_envelope::Payload::ProjectionCheckpoint(checkpoint)) => {
            if let Some(saved) = &checkpoint.snapshot {
                *snapshot = saved.clone();
            }
            *database = checkpoint.database.clone();
        }
        _ => {}
    }
    snapshot.through_sequence = event.sequence;
    snapshot.process_instance_id = event.process_instance_id.clone();
}

struct CoreSink {
    hub: Arc<EventHub>,
}

impl CoreEventSink for CoreSink {
    fn publish(&self, event: CoreEvent) {
        if let Err(error) = publish_core(&self.hub, event) {
            tracing::error!(error = %error, "cannot append a core control event");
        }
    }
}

fn correlation(operation_id: Option<u64>) -> Vec<u8> {
    operation_id
        .map(|id| id.to_le_bytes().to_vec())
        .unwrap_or_default()
}

fn core_mode(mode: CoreDatabaseMode) -> pb::DatabaseMode {
    match mode {
        CoreDatabaseMode::Writer => pb::DatabaseMode::Writer,
        CoreDatabaseMode::Replica => pb::DatabaseMode::Replica,
        CoreDatabaseMode::Reader => pb::DatabaseMode::Reader,
    }
}

fn core_error(error: yesno_core::events::EventError) -> pb::EventError {
    let error_class = match error.class {
        CoreErrorClass::Io => pb::ErrorClass::Io,
        CoreErrorClass::Corruption => pb::ErrorClass::Corruption,
        CoreErrorClass::Contention => pb::ErrorClass::Contention,
        CoreErrorClass::Invariant => pb::ErrorClass::Invariant,
        CoreErrorClass::Unsupported => pb::ErrorClass::Unsupported,
        CoreErrorClass::Other => pb::ErrorClass::Other,
    };
    pb::EventError {
        error_class: error_class as i32,
        os_error_code: None,
        detail: error.detail,
    }
}

fn publish_core(hub: &EventHub, event: CoreEvent) -> Result<pb::EventEnvelope, BoxError> {
    use pb::event_envelope::Payload;
    let operation_id = event.operation_id();
    let mut database = None;
    let mut shard = None;
    let (severity, payload) = match event {
        CoreEvent::DatabaseOpenStarted { mode, .. } => (
            pb::Severity::Info,
            Payload::DatabaseLifecycle(pb::DatabaseLifecycleEvent {
                operation: pb::database_lifecycle_event::Operation::Open as i32,
                phase: pb::EventPhase::Started as i32,
                mode: core_mode(mode) as i32,
                ..Default::default()
            }),
        ),
        CoreEvent::DatabaseOpenCompleted {
            mode,
            database_uuid,
            shards,
            epoch,
            term,
            visible,
            operation_id,
        } => {
            database = Some(pb::DatabaseIdentity {
                uuid: database_uuid.to_vec(),
                mode: core_mode(mode) as i32,
                open_instance: operation_id,
                shards,
                epoch,
                term,
            });
            (
                pb::Severity::Info,
                Payload::DatabaseLifecycle(pb::DatabaseLifecycleEvent {
                    operation: pb::database_lifecycle_event::Operation::Open as i32,
                    phase: pb::EventPhase::Completed as i32,
                    mode: core_mode(mode) as i32,
                    visible_version: visible,
                    ..Default::default()
                }),
            )
        }
        CoreEvent::DatabaseOpenFailed { mode, error, .. } => (
            pb::Severity::Error,
            Payload::DatabaseLifecycle(pb::DatabaseLifecycleEvent {
                operation: pb::database_lifecycle_event::Operation::Open as i32,
                phase: pb::EventPhase::Failed as i32,
                mode: core_mode(mode) as i32,
                error: Some(core_error(error)),
                ..Default::default()
            }),
        ),
        CoreEvent::RecoveryStarted {
            checkpoint_version, ..
        } => (
            pb::Severity::Info,
            Payload::Recovery(pb::RecoveryEvent {
                phase: pb::EventPhase::Started as i32,
                checkpoint_version,
                ..Default::default()
            }),
        ),
        CoreEvent::RecoveryCompleted {
            checkpoint_version,
            recovered_version,
            records_replayed,
            discarded_versions,
            ..
        } => (
            pb::Severity::Info,
            Payload::Recovery(pb::RecoveryEvent {
                phase: pb::EventPhase::Completed as i32,
                checkpoint_version,
                recovered_version,
                records_replayed,
                discarded_versions,
                error: None,
            }),
        ),
        CoreEvent::RecoveryFailed {
            checkpoint_version,
            error,
            ..
        } => (
            pb::Severity::Error,
            Payload::Recovery(pb::RecoveryEvent {
                phase: pb::EventPhase::Failed as i32,
                checkpoint_version,
                error: Some(core_error(error)),
                ..Default::default()
            }),
        ),
        CoreEvent::WalTailTruncated {
            shard: event_shard,
            old_end_lsn,
            new_end_lsn,
            ..
        } => {
            shard = Some(event_shard);
            (
                pb::Severity::Warning,
                Payload::Wal(pb::WalEvent {
                    operation: pb::wal_event::Operation::TailTruncation as i32,
                    phase: pb::EventPhase::Completed as i32,
                    old_end_lsn,
                    new_end_lsn,
                    ..Default::default()
                }),
            )
        }
        CoreEvent::CheckpointStarted {
            watermark,
            dirty_bytes,
            wal_bytes,
            ..
        } => (
            pb::Severity::Info,
            Payload::Checkpoint(pb::CheckpointEvent {
                phase: pb::EventPhase::Started as i32,
                watermark,
                dirty_bytes,
                wal_bytes,
                error: None,
            }),
        ),
        CoreEvent::CheckpointCompleted { watermark, .. } => (
            pb::Severity::Info,
            Payload::Checkpoint(pb::CheckpointEvent {
                phase: pb::EventPhase::Completed as i32,
                watermark,
                ..Default::default()
            }),
        ),
        CoreEvent::CheckpointFailed {
            watermark, error, ..
        } => (
            pb::Severity::Error,
            Payload::Checkpoint(pb::CheckpointEvent {
                phase: pb::EventPhase::Failed as i32,
                watermark,
                error: Some(core_error(error)),
                ..Default::default()
            }),
        ),
        CoreEvent::WalGenerationRotated {
            shard: event_shard,
            old_base_lsn,
            new_base_lsn,
            bytes_reclaimed,
            sealed_through_lsn,
            forced_past_retention,
            ..
        } => {
            shard = Some(event_shard);
            (
                if forced_past_retention {
                    pb::Severity::Warning
                } else {
                    pb::Severity::Info
                },
                Payload::Wal(pb::WalEvent {
                    operation: pb::wal_event::Operation::SegmentSeal as i32,
                    phase: if forced_past_retention {
                        pb::EventPhase::Forced as i32
                    } else {
                        pb::EventPhase::Completed as i32
                    },
                    old_base_lsn,
                    new_base_lsn,
                    bytes_reclaimed,
                    sealed_through_lsn,
                    forced_past_retention,
                    ..Default::default()
                }),
            )
        }
        CoreEvent::WalReclamationDeferred {
            shard: event_shard,
            end_lsn,
            retention_floor_lsn,
            ..
        } => {
            shard = Some(event_shard);
            (
                pb::Severity::Info,
                Payload::Wal(pb::WalEvent {
                    operation: pb::wal_event::Operation::SegmentReclaim as i32,
                    phase: pb::EventPhase::Deferred as i32,
                    old_end_lsn: end_lsn,
                    retention_floor_lsn: Some(retention_floor_lsn),
                    ..Default::default()
                }),
            )
        }
        CoreEvent::StorageOperationFailed {
            operation,
            shard: event_shard,
            error,
            ..
        } => {
            shard = event_shard;
            (
                pb::Severity::Error,
                Payload::Storage(pb::StorageEvent {
                    phase: pb::EventPhase::Failed as i32,
                    operation: operation.into(),
                    error: Some(core_error(error)),
                }),
            )
        }
        CoreEvent::DatabaseShutdownStarted { reason, .. } => (
            pb::Severity::Info,
            Payload::DatabaseLifecycle(pb::DatabaseLifecycleEvent {
                operation: pb::database_lifecycle_event::Operation::Shutdown as i32,
                phase: pb::EventPhase::Started as i32,
                reason: format!("{reason:?}"),
                ..Default::default()
            }),
        ),
        CoreEvent::DatabaseShutdownCompleted { graceful, .. } => (
            if graceful {
                pb::Severity::Info
            } else {
                pb::Severity::Warning
            },
            Payload::DatabaseLifecycle(pb::DatabaseLifecycleEvent {
                operation: pb::database_lifecycle_event::Operation::Shutdown as i32,
                phase: pb::EventPhase::Completed as i32,
                graceful,
                ..Default::default()
            }),
        ),
        _ => return Err("yesno-core emitted an event this yesnod does not understand".into()),
    };
    hub.publish(
        pb::EventSource::Core,
        severity,
        correlation(operation_id),
        Vec::new(),
        database,
        shard,
        payload,
    )
}

/// gRPC implementation and command ingress.
#[derive(Clone)]
struct ControlService {
    hub: Arc<EventHub>,
    commands: mpsc::Sender<Command>,
    authorizer: Arc<Authorizer>,
    replication: ReplicationSlot,
    snapshots: Option<Arc<crate::snapshot::SnapshotManager>>,
    snapshot_agent: crate::snapshot::agent::SnapshotAgentBroker,
}

#[tonic::async_trait]
impl pb::control_plane_server::ControlPlane for ControlService {
    type SubscribeStream = ReceiverStream<Result<pb::SubscribeItem, Status>>;
    type FetchSnapshotFileStream = ReceiverStream<Result<pb::SnapshotFileChunk, Status>>;

    async fn subscribe(
        &self,
        request: Request<pb::SubscribeRequest>,
    ) -> Result<Response<Self::SubscribeStream>, Status> {
        self.authorizer
            .authorize(&request, EndpointCapability::ControlRead)?;
        let after = request.into_inner().after_sequence;
        let (replay, oldest, head, mut live, snapshot) = self.hub.subscription(after);
        let (tx, rx) = mpsc::channel(SUBSCRIBER_BUFFER);
        tokio::spawn(async move {
            if after.saturating_add(1) < oldest {
                let item = pb::SubscribeItem {
                    item: Some(pb::subscribe_item::Item::ResyncRequired(
                        pb::ResyncRequired {
                            requested_sequence: after,
                            oldest_sequence: oldest,
                            snapshot: Some(snapshot),
                        },
                    )),
                };
                let _ = tx.send(Ok(item)).await;
                return;
            }
            if tx
                .send(Ok(pb::SubscribeItem {
                    item: Some(pb::subscribe_item::Item::Started(pb::SubscriptionStarted {
                        oldest_sequence: oldest,
                        head_sequence: head,
                    })),
                }))
                .await
                .is_err()
            {
                return;
            }
            for event in replay {
                if tx
                    .send(Ok(pb::SubscribeItem {
                        item: Some(pb::subscribe_item::Item::Event(Box::new(event))),
                    }))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            loop {
                match live.recv().await {
                    Ok(event) if event.sequence > head => {
                        if tx
                            .send(Ok(pb::SubscribeItem {
                                item: Some(pb::subscribe_item::Item::Event(Box::new(event))),
                            }))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Closed) => return,
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let _ = tx
                            .send(Err(Status::resource_exhausted(
                                "subscriber fell behind; resume from its last sequence",
                            )))
                            .await;
                        return;
                    }
                }
            }
        });
        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn get_snapshot(
        &self,
        request: Request<pb::GetSnapshotRequest>,
    ) -> Result<Response<pb::StateSnapshot>, Status> {
        self.authorizer
            .authorize(&request, EndpointCapability::ControlRead)?;
        Ok(Response::new(self.hub.snapshot()))
    }

    async fn checkpoint(
        &self,
        request: Request<pb::CheckpointRequest>,
    ) -> Result<Response<pb::CheckpointResponse>, Status> {
        self.authorizer
            .authorize(&request, EndpointCapability::ControlAdmin)?;
        let db = self.replication.database()?;
        let watermark = tokio::task::spawn_blocking(move || db.checkpoint())
            .await
            .map_err(|error| Status::internal(format!("checkpoint task failed: {error}")))?
            .map_err(|error| Status::internal(error.to_string()))?;
        Ok(Response::new(pb::CheckpointResponse { watermark }))
    }

    async fn promote(
        &self,
        request: Request<pb::PromoteRequest>,
    ) -> Result<Response<pb::CommandAccepted>, Status> {
        self.authorizer
            .authorize(&request, EndpointCapability::ControlAdmin)?;
        let correlation_id = self.hub.correlation_id(&request.into_inner().request_id);
        let permit = self
            .commands
            .reserve()
            .await
            .map_err(|_| Status::unavailable("lifecycle command loop is not running"))?;
        let event = role_requested(&self.hub, correlation_id.clone(), pb::Role::Leader)?;
        permit.send(Command::Promote {
            correlation_id: correlation_id.clone(),
        });
        Ok(Response::new(pb::CommandAccepted {
            correlation_id,
            accepted_sequence: event.sequence,
        }))
    }

    async fn demote(
        &self,
        request: Request<pb::DemoteRequest>,
    ) -> Result<Response<pb::CommandAccepted>, Status> {
        self.authorizer
            .authorize(&request, EndpointCapability::ControlAdmin)?;
        let request = request.into_inner();
        let correlation_id = self.hub.correlation_id(&request.request_id);
        let permit = self
            .commands
            .reserve()
            .await
            .map_err(|_| Status::unavailable("lifecycle command loop is not running"))?;
        let event = role_requested(&self.hub, correlation_id.clone(), pb::Role::Follower)?;
        permit.send(Command::Demote {
            correlation_id: correlation_id.clone(),
            leader: request.leader,
        });
        Ok(Response::new(pb::CommandAccepted {
            correlation_id,
            accepted_sequence: event.sequence,
        }))
    }

    async fn shutdown(
        &self,
        request: Request<pb::ShutdownRequest>,
    ) -> Result<Response<pb::CommandAccepted>, Status> {
        self.authorizer
            .authorize(&request, EndpointCapability::ControlAdmin)?;
        let correlation_id = self.hub.correlation_id(&request.into_inner().request_id);
        let permit = self
            .commands
            .reserve()
            .await
            .map_err(|_| Status::unavailable("lifecycle command loop is not running"))?;
        let event = self
            .hub
            .publish_server(
                pb::Severity::Info,
                correlation_id.clone(),
                Vec::new(),
                pb::event_envelope::Payload::ServerLifecycle(pb::ServerLifecycleEvent {
                    operation: pb::server_lifecycle_event::Operation::Shutdown as i32,
                    phase: pb::EventPhase::Requested as i32,
                    ..Default::default()
                }),
            )
            .map_err(internal)?;
        permit.send(Command::Shutdown {
            correlation_id: correlation_id.clone(),
        });
        Ok(Response::new(pb::CommandAccepted {
            correlation_id,
            accepted_sequence: event.sequence,
        }))
    }

    async fn begin_base_snapshot(
        &self,
        request: Request<pb::BeginBaseSnapshotRequest>,
    ) -> Result<Response<pb::BaseSnapshotLease>, Status> {
        self.authorizer
            .authorize(&request, EndpointCapability::Replication)?;
        let manager = self.snapshots.as_ref().ok_or_else(|| {
            Status::failed_precondition("this server has no base-snapshot provider")
        })?;
        let db = self.replication.database()?;
        // Provider reconciliation can involve several filesystem deletions.
        // Complete it before taking the checkpoint barrier; BeginBaseSnapshot
        // must block checkpointing only for creation of the new immutable view.
        manager.prepare().await.map_err(internal)?;
        let backup = tokio::task::spawn_blocking(move || db.begin_backup())
            .await
            .map_err(|error| Status::internal(format!("backup barrier task failed: {error}")))?;
        let captured = manager.capture().await.map_err(internal)?;
        drop(backup);
        let lease = manager.materialize(captured).await.map_err(internal)?;
        let source = match lease.source {
            crate::snapshot::SnapshotSource::Zfs => pb::BaseSnapshotSource::Zfs,
            crate::snapshot::SnapshotSource::Btrfs => pb::BaseSnapshotSource::Btrfs,
            crate::snapshot::SnapshotSource::Lvm => pb::BaseSnapshotSource::Lvm,
            crate::snapshot::SnapshotSource::Ebs => pb::BaseSnapshotSource::Ebs,
            crate::snapshot::SnapshotSource::Portable => pb::BaseSnapshotSource::Portable,
        };
        Ok(Response::new(pb::BaseSnapshotLease {
            lease_id: lease.id,
            source: source as i32,
            files: lease
                .files
                .into_iter()
                .map(|file| pb::SnapshotFile {
                    name: file.name,
                    size: file.size,
                })
                .collect(),
            lease_ttl_secs: lease.ttl_secs,
            direct_path_available: lease.direct_path_available,
            deferred_ebs: lease.deferred_ebs.map(|deferred| pb::DeferredEbsSnapshot {
                snapshot_id: deferred.snapshot_id,
                region: deferred.region,
                filesystem: deferred.filesystem,
                source_subpath: deferred.source_subpath,
                volume_size_gib: deferred.volume_size_gib,
            }),
        }))
    }

    async fn fetch_snapshot_file(
        &self,
        request: Request<pb::FetchSnapshotFileRequest>,
    ) -> Result<Response<Self::FetchSnapshotFileStream>, Status> {
        self.authorizer
            .authorize(&request, EndpointCapability::Replication)?;
        let request = request.into_inner();
        let manager = self.snapshots.as_ref().ok_or_else(|| {
            Status::failed_precondition("this server has no base-snapshot provider")
        })?;
        let access = manager
            .open_file(&request.lease_id, &request.name, request.allow_direct_path)
            .await
            .map_err(|error| Status::not_found(error.to_string()))?;
        let (tx, rx) = mpsc::channel(4);
        let manager = manager.clone();
        let lease_id = request.lease_id;
        tokio::spawn(async move {
            match access {
                crate::snapshot::SnapshotFileAccess::Direct { path, size } => {
                    let Some(path) = path.to_str() else {
                        let _ = tx
                            .send(Err(Status::internal(
                                "direct snapshot path is not valid UTF-8",
                            )))
                            .await;
                        return;
                    };
                    let _ = tx
                        .send(Ok(pb::SnapshotFileChunk {
                            offset: 0,
                            payload: Some(pb::snapshot_file_chunk::Payload::DirectPath(
                                path.to_owned(),
                            )),
                            last: true,
                            total_size: size,
                        }))
                        .await;
                }
                crate::snapshot::SnapshotFileAccess::Stream { mut file, size } => {
                    let mut offset = 0u64;
                    loop {
                        let mut bytes = vec![0u8; 1024 * 1024];
                        let count = match file.read(&mut bytes).await {
                            Ok(count) => count,
                            Err(error) => {
                                let _ = tx.send(Err(Status::unavailable(error.to_string()))).await;
                                return;
                            }
                        };
                        if count == 0 && offset < size {
                            let _ = tx
                                .send(Err(Status::data_loss(format!(
                                    "snapshot file ended at {offset}, expected {size} bytes"
                                ))))
                                .await;
                            return;
                        }
                        bytes.truncate(count);
                        let last = offset.saturating_add(count as u64) == size;
                        if manager.keep_alive(&lease_id).is_err() {
                            let _ = tx
                                .send(Err(Status::not_found("snapshot lease expired")))
                                .await;
                            return;
                        }
                        if tx
                            .send(Ok(pb::SnapshotFileChunk {
                                offset,
                                payload: Some(pb::snapshot_file_chunk::Payload::Data(bytes)),
                                last,
                                total_size: size,
                            }))
                            .await
                            .is_err()
                        {
                            return;
                        }
                        offset = offset.saturating_add(count as u64);
                        if last {
                            return;
                        }
                    }
                }
            }
        });
        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn keep_base_snapshot_alive(
        &self,
        request: Request<pb::KeepBaseSnapshotAliveRequest>,
    ) -> Result<Response<pb::BaseSnapshotLifetime>, Status> {
        self.authorizer
            .authorize(&request, EndpointCapability::Replication)?;
        let manager = self.snapshots.as_ref().ok_or_else(|| {
            Status::failed_precondition("this server has no base-snapshot provider")
        })?;
        let ttl = manager
            .keep_alive(&request.into_inner().lease_id)
            .map_err(|error| Status::not_found(error.to_string()))?;
        Ok(Response::new(pb::BaseSnapshotLifetime {
            lease_ttl_secs: ttl,
        }))
    }

    async fn release_base_snapshot(
        &self,
        request: Request<pb::ReleaseBaseSnapshotRequest>,
    ) -> Result<Response<pb::ReleaseBaseSnapshotResponse>, Status> {
        self.authorizer
            .authorize(&request, EndpointCapability::Replication)?;
        let manager = self.snapshots.as_ref().ok_or_else(|| {
            Status::failed_precondition("this server has no base-snapshot provider")
        })?;
        manager
            .release(&request.into_inner().lease_id)
            .await
            .map_err(|error| Status::not_found(error.to_string()))?;
        Ok(Response::new(pb::ReleaseBaseSnapshotResponse {}))
    }

    async fn claim_snapshot_agent_work(
        &self,
        request: Request<pb::ClaimSnapshotAgentWorkRequest>,
    ) -> Result<Response<pb::SnapshotAgentWork>, Status> {
        require_snapshot_agent(&request)?;
        self.snapshot_agent
            .claim()
            .await
            .map(Response::new)
            .map_err(internal)
    }

    async fn complete_snapshot_agent_work(
        &self,
        request: Request<pb::CompleteSnapshotAgentWorkRequest>,
    ) -> Result<Response<pb::CompleteSnapshotAgentWorkResponse>, Status> {
        require_snapshot_agent(&request)?;
        self.snapshot_agent
            .complete(request.into_inner())
            .map_err(|error| Status::failed_precondition(error.to_string()))?;
        Ok(Response::new(pb::CompleteSnapshotAgentWorkResponse {}))
    }
}

fn require_snapshot_agent<T>(request: &Request<T>) -> Result<(), Status> {
    request
        .extensions()
        .get::<crate::auth::SnapshotAgentCredential>()
        .map(|_| ())
        .ok_or_else(|| {
            Status::permission_denied(
                "snapshot-agent RPC requires explicit peer-matched root Unix credentials",
            )
        })
}

fn role_requested(
    hub: &EventHub,
    correlation_id: Vec<u8>,
    to: pb::Role,
) -> Result<pb::EventEnvelope, Status> {
    let snapshot = hub.snapshot();
    if snapshot.role == to as i32 {
        return Err(Status::failed_precondition(format!(
            "node is already {:?}",
            to
        )));
    }
    hub.publish_server(
        pb::Severity::Info,
        correlation_id,
        Vec::new(),
        pb::event_envelope::Payload::RoleTransition(pb::RoleTransitionEvent {
            phase: pb::EventPhase::Requested as i32,
            from: snapshot.role,
            to: to as i32,
            previous_term: snapshot.term,
            term: snapshot.term,
            detail: String::new(),
        }),
    )
    .map_err(internal)
}

fn internal(error: BoxError) -> Status {
    Status::internal(error.to_string())
}

/// The replication service currently available on the shared endpoint.
///
/// The listener outlives role transitions, while replication is available only
/// while this process owns an open leader database. Keeping the service
/// registered and swapping its implementation makes callers receive
/// `UNAVAILABLE` across promotion or demotion without losing the channel.
#[derive(Clone, Default)]
pub struct ReplicationSlot {
    inner: Arc<std::sync::RwLock<Option<ReplicationState>>>,
    snapshots: Option<Arc<crate::snapshot::SnapshotManager>>,
}

#[derive(Clone)]
struct ReplicationState {
    service: crate::replication::LeaderService,
    db: Option<Arc<yesno_core::Db>>,
}

impl ReplicationSlot {
    fn with_snapshots(snapshots: Option<Arc<crate::snapshot::SnapshotManager>>) -> Self {
        Self {
            inner: Arc::default(),
            snapshots,
        }
    }

    /// Publish a leader implementation for subsequent replication RPCs.
    pub fn install(&self, service: crate::replication::LeaderService) {
        *self.inner.write().unwrap() = Some(ReplicationState { service, db: None });
    }

    /// Publish replication together with the live database whose checkpoint
    /// barrier protects base-snapshot creation.
    pub fn install_with_db(
        &self,
        service: crate::replication::LeaderService,
        db: Arc<yesno_core::Db>,
    ) {
        *self.inner.write().unwrap() = Some(ReplicationState {
            service,
            db: Some(db),
        });
        if let Some(snapshots) = &self.snapshots {
            snapshots.reconcile_after_database_open();
        }
    }

    /// Make subsequent replication RPCs fail with `UNAVAILABLE`.
    pub fn clear(&self) {
        *self.inner.write().unwrap() = None;
    }

    fn current(&self) -> Result<crate::replication::LeaderService, Status> {
        self.inner
            .read()
            .map_err(|_| Status::internal("the replication slot is poisoned"))?
            .as_ref()
            .map(|state| state.service.clone())
            .ok_or_else(|| {
                Status::unavailable(
                    "replication is available only while this node is serving as leader",
                )
            })
    }

    fn database(&self) -> Result<Arc<yesno_core::Db>, Status> {
        self.inner
            .read()
            .map_err(|_| Status::internal("the replication slot is poisoned"))?
            .as_ref()
            .and_then(|state| state.db.clone())
            .ok_or_else(|| {
                Status::unavailable(
                    "database administration is available only from an active yesnod leader",
                )
            })
    }
}

#[derive(Clone)]
struct SharedReplication {
    slot: ReplicationSlot,
    authorizer: Arc<Authorizer>,
}

impl SharedReplication {
    fn authorize<T>(&self, request: &Request<T>) -> Result<(), Status> {
        self.authorizer
            .authorize(request, EndpointCapability::Replication)
    }
}

#[tonic::async_trait]
impl crate::replication::pb::replication_server::Replication for SharedReplication {
    type SubscribeStream = <crate::replication::LeaderService as
        crate::replication::pb::replication_server::Replication>::SubscribeStream;
    type FetchBaseSnapshotStream = <crate::replication::LeaderService as
        crate::replication::pb::replication_server::Replication>::FetchBaseSnapshotStream;

    async fn status(
        &self,
        request: Request<crate::replication::pb::StatusRequest>,
    ) -> Result<Response<crate::replication::pb::StatusResponse>, Status> {
        self.authorize(&request)?;
        self.slot.current()?.status(request).await
    }

    async fn subscribe(
        &self,
        request: Request<crate::replication::pb::SubscribeRequest>,
    ) -> Result<Response<Self::SubscribeStream>, Status> {
        self.authorize(&request)?;
        self.slot.current()?.subscribe(request).await
    }

    async fn ack(
        &self,
        mut request: Request<tonic::Streaming<crate::replication::pb::AckRequest>>,
    ) -> Result<Response<crate::replication::pb::AckResponse>, Status> {
        self.authorize(&request)?;
        let principal = request
            .extensions()
            .get::<Principal>()
            .ok_or_else(|| Status::unauthenticated("no principal on this request"))?;
        let identity = (!principal.unconfigured).then(|| principal.name.to_string());
        if let Some(identity) = identity {
            request
                .extensions_mut()
                .insert(crate::replication::FollowerIdentity(identity));
        }
        self.slot.current()?.ack(request).await
    }

    async fn fetch_base_snapshot(
        &self,
        request: Request<crate::replication::pb::SnapshotRequest>,
    ) -> Result<Response<Self::FetchBaseSnapshotStream>, Status> {
        self.authorize(&request)?;
        self.slot.current()?.fetch_base_snapshot(request).await
    }

    async fn fetch_manifest(
        &self,
        request: Request<crate::replication::pb::ManifestRequest>,
    ) -> Result<Response<crate::replication::pb::ManifestResponse>, Status> {
        self.authorize(&request)?;
        self.slot.current()?.fetch_manifest(request).await
    }
}

/// A running shared control-plane listener.
pub struct Serving {
    pub addr: Option<std::net::SocketAddr>,
    pub unix_socket: Option<PathBuf>,
    stop: broadcast::Sender<()>,
    served: Vec<tokio::task::JoinHandle<()>>,
    socket_file: Option<SocketFile>,
    hub: Arc<EventHub>,
    replication: ReplicationSlot,
    snapshots: Option<Arc<crate::snapshot::SnapshotManager>>,
    snapshot_agent: crate::snapshot::agent::SnapshotAgentBroker,
}

impl Serving {
    /// Dynamic replication service hosted by this same listener.
    pub fn replication_slot(&self) -> ReplicationSlot {
        self.replication.clone()
    }

    /// Record process stop and stop accepting new RPCs.
    pub async fn stop(self) {
        let _ = self.hub.publish_server(
            pb::Severity::Info,
            Vec::new(),
            Vec::new(),
            pb::event_envelope::Payload::ProcessLifecycle(pb::ProcessLifecycleEvent {
                operation: pb::process_lifecycle_event::Operation::Stop as i32,
                phase: pb::EventPhase::Completed as i32,
                ..Default::default()
            }),
        );
        let _ = self.stop.send(());
        if let Some(snapshots) = &self.snapshots {
            snapshots.shutdown().await;
        }
        self.snapshot_agent.close();
        for task in self.served {
            let _ = task.await;
        }
        drop(self.socket_file);
    }
}

/// A socket pathname owned by this listener. Comparing its inode before
/// unlinking avoids removing a replacement created after an external unlink.
struct SocketFile {
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl Drop for SocketFile {
    fn drop(&mut self) {
        let Ok(metadata) = std::fs::symlink_metadata(&self.path) else {
            return;
        };
        if metadata.dev() == self.device && metadata.ino() == self.inode {
            if let Err(error) = std::fs::remove_file(&self.path) {
                tracing::warn!(
                    path = %self.path.display(),
                    error = %error,
                    "cannot remove control-plane Unix socket"
                );
            }
        }
    }
}

async fn bind_unix(
    path: &Path,
    mode: Option<u32>,
) -> Result<(tokio::net::UnixListener, SocketFile), BoxError> {
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        if !metadata.file_type().is_socket() {
            return Err(format!(
                "control Unix socket path '{}' exists and is not a socket",
                path.display()
            )
            .into());
        }
        match tokio::net::UnixStream::connect(path).await {
            Ok(_) => {
                return Err(format!(
                    "control Unix socket '{}' is already accepting connections",
                    path.display()
                )
                .into());
            }
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {
                std::fs::remove_file(path)?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    let listener = tokio::net::UnixListener::bind(path)?;
    // There is a window between bind and chmod in which the socket carries
    // the umask-derived mode. The containing directory is the boundary that
    // closes it — an operator who narrows the socket must own the directory
    // too, which `operations.md` says — and doing this before bind would mean
    // mutating the process umask, which is global and would race every other
    // file the daemon creates.
    if let Some(mode) = mode {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    }
    let metadata = std::fs::symlink_metadata(path)?;
    let file = SocketFile {
        path: path.to_path_buf(),
        device: metadata.dev(),
        inode: metadata.ino(),
    };
    Ok((listener, file))
}

/// Bind the shared control and replication service on its configured channels.
pub async fn serve(
    config: &ServerConfig,
    auth_config: &AuthConfig,
    hub: Arc<EventHub>,
    commands: mpsc::Sender<Command>,
) -> Result<Serving, BoxError> {
    let tcp = if config.control.listen.trim().is_empty() {
        None
    } else {
        let listener = tokio::net::TcpListener::bind(&config.control.listen).await?;
        let addr = listener.local_addr()?;
        Some((listener, addr))
    };
    let unix = match &config.control.unix_socket {
        Some(path) => Some(bind_unix(path, config.control.socket_mode()).await?),
        None => None,
    };
    if tcp.is_none() && unix.is_none() {
        return Err("the control plane has no configured listener".into());
    }
    let (stop, _) = broadcast::channel(1);

    let authenticator = Arc::new(Authenticator::new(auth_config));
    let authorizer = Arc::new(Authorizer::new(&auth_config.rules)?);
    let snapshot_agent = crate::snapshot::agent::SnapshotAgentBroker::new();
    let snapshots = crate::snapshot::SnapshotManager::from_config(
        &config.snapshot,
        config.data_dir.as_deref().unwrap_or_else(|| Path::new(".")),
        snapshot_agent.clone(),
    );
    let replication = ReplicationSlot::with_snapshots(snapshots.clone());
    let control = ControlService {
        hub: hub.clone(),
        commands,
        authorizer: authorizer.clone(),
        replication: replication.clone(),
        snapshots: snapshots.clone(),
        snapshot_agent: snapshot_agent.clone(),
    };
    let shared_replication = SharedReplication {
        slot: replication.clone(),
        authorizer,
    };
    let mut served = Vec::new();
    let addr = tcp.as_ref().map(|(_, addr)| *addr);

    if let Some((listener, endpoint)) = tcp {
        let tls = config.control.tls.is_enabled();
        let control_service = pb::control_plane_server::ControlPlaneServer::with_interceptor(
            control.clone(),
            crate::auth::host_interceptor(authenticator.clone(), tls),
        );
        let replication_service =
            crate::replication::pb::replication_server::ReplicationServer::with_interceptor(
                shared_replication.clone(),
                crate::auth::host_interceptor(authenticator.clone(), tls),
            );
        let reloadable = crate::tls::ReloadableTls::new(&config.control.tls, "control")?;
        let router = tonic::transport::Server::builder()
            .add_service(control_service)
            .add_service(replication_service);
        hub.publish_server(
            pb::Severity::Info,
            Vec::new(),
            Vec::new(),
            pb::event_envelope::Payload::Listener(pb::ListenerEvent {
                kind: pb::listener_event::Kind::Control as i32,
                phase: pb::EventPhase::Completed as i32,
                endpoint: endpoint.to_string(),
                detail: "host control and replication gRPC services".into(),
            }),
        )?;
        let mut stopped = stop.subscribe();
        served.push(tokio::spawn(async move {
            let result = crate::tls::serve_router(router, listener, reloadable, async move {
                let _ = stopped.recv().await;
            })
            .await;
            if let Err(error) = result {
                tracing::error!(error = %error, "host control-plane listener stopped");
            }
        }));
    }

    let (unix_listener, socket_file, unix_socket) = match unix {
        Some((listener, file)) => {
            let path = file.path.clone();
            (Some(listener), Some(file), Some(path))
        }
        None => (None, None, None),
    };
    if let Some(listener) = unix_listener {
        let control_service = pb::control_plane_server::ControlPlaneServer::with_interceptor(
            control,
            crate::auth::local_interceptor(),
        );
        let replication_service =
            crate::replication::pb::replication_server::ReplicationServer::with_interceptor(
                shared_replication,
                crate::auth::local_interceptor(),
            );
        let endpoint = unix_socket.as_ref().expect("listener has path");
        hub.publish_server(
            pb::Severity::Info,
            Vec::new(),
            Vec::new(),
            pb::event_envelope::Payload::Listener(pb::ListenerEvent {
                kind: pb::listener_event::Kind::Control as i32,
                phase: pb::EventPhase::Completed as i32,
                endpoint: format!("unix://{}", endpoint.display()),
                detail: "local control and replication gRPC services".into(),
            }),
        )?;
        let mut stopped = stop.subscribe();
        served.push(tokio::spawn(async move {
            let result = tonic::transport::Server::builder()
                .add_service(control_service)
                .add_service(replication_service)
                .serve_with_incoming_shutdown(
                    crate::local_transport::incoming(listener),
                    async move {
                        let _ = stopped.recv().await;
                    },
                )
                .await;
            if let Err(error) = result {
                tracing::error!(error = %error, "local control-plane listener stopped");
            }
        }));
    }
    Ok(Serving {
        addr,
        unix_socket,
        stop,
        served,
        socket_file,
        hub,
        replication,
        snapshots,
        snapshot_agent,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pb::control_plane_server::ControlPlane as _;
    use yesno_core::{Db, DbOptions};

    fn test_rules() -> Vec<crate::config::AuthzRule> {
        [
            EndpointCapability::ControlRead,
            EndpointCapability::ControlAdmin,
            EndpointCapability::Replication,
        ]
        .into_iter()
        .map(|capability| crate::config::AuthzRule {
            channel: crate::config::AuthzChannel::Host,
            principal: "all".into(),
            address: "all".into(),
            capability,
            action: crate::config::RuleAction::Allow,
        })
        .collect()
    }

    fn test_auth() -> AuthConfig {
        let mut rules = test_rules();
        for rule in &mut rules {
            rule.address = "127.0.0.0/8".into();
        }
        AuthConfig {
            rules,
            ..Default::default()
        }
    }

    fn test_authorizer() -> Arc<Authorizer> {
        Arc::new(Authorizer::new(&test_rules()).unwrap())
    }

    fn authorized<T>(body: T) -> Request<T> {
        let mut request = Request::new(body);
        request.extensions_mut().insert(Principal {
            name: Arc::from("test"),
            role: crate::config::PrincipalRole::Admin,
            unconfigured: true,
        });
        request
            .extensions_mut()
            .insert(crate::config::AuthzChannel::Hostnossl);
        request
    }

    fn complete_agent_work(
        broker: &crate::snapshot::agent::SnapshotAgentBroker,
        work: pb::SnapshotAgentWork,
        root: String,
    ) {
        broker
            .complete(pb::CompleteSnapshotAgentWorkRequest {
                operation_id: work.operation_id,
                success: true,
                error: String::new(),
                root,
            })
            .unwrap();
    }

    fn role_event(
        phase: pb::EventPhase,
        from: pb::Role,
        to: pb::Role,
    ) -> pb::event_envelope::Payload {
        pb::event_envelope::Payload::RoleTransition(pb::RoleTransitionEvent {
            phase: phase as i32,
            from: from as i32,
            to: to as i32,
            previous_term: 3,
            term: 4,
            detail: String::new(),
        })
    }

    #[test]
    fn journal_truncates_only_a_torn_tail_and_preserves_projection() {
        let dir = tempfile::tempdir().unwrap();
        let hub = EventHub::open(dir.path()).unwrap();
        hub.publish_server(
            pb::Severity::Info,
            Vec::new(),
            Vec::new(),
            role_event(
                pb::EventPhase::Completed,
                pb::Role::Follower,
                pb::Role::Leader,
            ),
        )
        .unwrap();
        hub.publish_server(
            pb::Severity::Info,
            Vec::new(),
            Vec::new(),
            pb::event_envelope::Payload::ProcessLifecycle(pb::ProcessLifecycleEvent {
                operation: pb::process_lifecycle_event::Operation::Stop as i32,
                phase: pb::EventPhase::Completed as i32,
                ..Default::default()
            }),
        )
        .unwrap();
        let valid_head = hub.snapshot().through_sequence;
        drop(hub);

        let mut journal = OpenOptions::new()
            .append(true)
            .open(dir.path().join("events.yev"))
            .unwrap();
        journal.write_all(b"YEV1\x10").unwrap();
        journal.sync_all().unwrap();
        drop(journal);

        let recovered = EventHub::open(dir.path()).unwrap();
        let snapshot = recovered.snapshot();
        assert_eq!(snapshot.role, pb::Role::Leader as i32);
        assert_eq!(snapshot.term, 4);
        assert!(snapshot.process_running);
        assert_eq!(snapshot.through_sequence, valid_head + 1);
        let (events, oldest, head, _, _) = recovered.subscription(0);
        assert_eq!(oldest, 1);
        assert_eq!(head, valid_head + 1);
        assert!(events
            .windows(2)
            .all(|pair| pair[1].sequence == pair[0].sequence + 1));
    }

    #[test]
    fn unclosed_process_is_reconciled_as_an_interruption() {
        let dir = tempfile::tempdir().unwrap();
        let first = EventHub::open(dir.path()).unwrap();
        let previous_instance = first.snapshot().process_instance_id;
        first
            .publish_server(
                pb::Severity::Info,
                Vec::new(),
                Vec::new(),
                pb::event_envelope::Payload::DatabaseLifecycle(pb::DatabaseLifecycleEvent {
                    operation: pb::database_lifecycle_event::Operation::Open as i32,
                    phase: pb::EventPhase::Completed as i32,
                    mode: pb::DatabaseMode::Writer as i32,
                    visible_version: 7,
                    ..Default::default()
                }),
            )
            .unwrap();
        drop(first);

        let recovered = EventHub::open(dir.path()).unwrap();
        let (events, _, _, _, _) = recovered.subscription(0);
        assert!(events.iter().any(|event| matches!(
            event.payload.as_ref(),
            Some(pb::event_envelope::Payload::ProcessLifecycle(process))
                if process.operation == pb::process_lifecycle_event::Operation::Interruption as i32
                    && process.previous_process_instance_id.as_deref()
                        == Some(previous_instance.as_slice())
        )));
        assert!(events.iter().any(|event| matches!(
            event.payload.as_ref(),
            Some(pb::event_envelope::Payload::DatabaseLifecycle(database))
                if database.operation
                    == pb::database_lifecycle_event::Operation::Interruption as i32
                    && database.phase == pb::EventPhase::Observed as i32
                    && !database.graceful
        )));
        assert!(!recovered.snapshot().database_open);
    }

    #[test]
    fn core_facts_are_persisted_as_typed_protobuf_payloads() {
        let root = tempfile::tempdir().unwrap();
        let hub = EventHub::open(root.path().join("control")).unwrap();
        let db = Db::open_with_events(
            root.path().join("database"),
            DbOptions {
                shards: 1,
                ..Default::default()
            },
            hub.core_sink(),
        )
        .unwrap();
        db.insert(1, 2).unwrap();
        db.checkpoint().unwrap();
        db.begin_shutdown(yesno_core::events::ShutdownReason::Requested);
        drop(db);

        let (events, _, _, _, _) = hub.subscription(0);
        assert!(events.iter().any(|event| matches!(
            event.payload.as_ref(),
            Some(pb::event_envelope::Payload::DatabaseLifecycle(database))
                if database.operation == pb::database_lifecycle_event::Operation::Open as i32
                    && database.phase == pb::EventPhase::Completed as i32
        )));
        assert!(events.iter().any(|event| matches!(
            event.payload.as_ref(),
            Some(pb::event_envelope::Payload::Wal(wal))
                if wal.operation == pb::wal_event::Operation::SegmentSeal as i32
                    && wal.phase == pb::EventPhase::Completed as i32
                    && wal.sealed_through_lsn > 0
        )));
        assert!(events
            .iter()
            .all(|event| event.schema_version == SCHEMA_VERSION));
    }

    #[tokio::test]
    async fn promote_records_request_before_delivering_command() {
        let dir = tempfile::tempdir().unwrap();
        let hub = EventHub::open(dir.path()).unwrap();
        hub.publish_server(
            pb::Severity::Info,
            Vec::new(),
            Vec::new(),
            role_event(
                pb::EventPhase::Completed,
                pb::Role::Offline,
                pb::Role::Follower,
            ),
        )
        .unwrap();
        let (commands, mut received) = mpsc::channel(1);
        let service = ControlService {
            hub: hub.clone(),
            commands,
            authorizer: test_authorizer(),
            replication: ReplicationSlot::default(),
            snapshots: None,
            snapshot_agent: crate::snapshot::agent::SnapshotAgentBroker::new(),
        };

        let response = service
            .promote(authorized(pb::PromoteRequest {
                request_id: b"promote-1".to_vec(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(response.correlation_id, b"promote-1");
        match received.recv().await.unwrap() {
            Command::Promote { correlation_id } => {
                assert_eq!(correlation_id, response.correlation_id)
            }
            other => panic!("received {other:?}"),
        }
        let (events, _, _, _, _) = hub.subscription(response.accepted_sequence - 1);
        assert!(matches!(
            events.first().and_then(|event| event.payload.as_ref()),
            Some(pb::event_envelope::Payload::RoleTransition(role))
                if role.phase == pb::EventPhase::Requested as i32
                    && role.from == pb::Role::Follower as i32
                    && role.to == pb::Role::Leader as i32
        ));
    }

    #[tokio::test]
    async fn same_role_command_is_rejected_without_delivery() {
        let dir = tempfile::tempdir().unwrap();
        let hub = EventHub::open(dir.path()).unwrap();
        hub.publish_server(
            pb::Severity::Info,
            Vec::new(),
            Vec::new(),
            role_event(
                pb::EventPhase::Completed,
                pb::Role::Offline,
                pb::Role::Leader,
            ),
        )
        .unwrap();
        let head = hub.snapshot().through_sequence;
        let (commands, mut received) = mpsc::channel(1);
        let service = ControlService {
            hub: hub.clone(),
            commands,
            authorizer: test_authorizer(),
            replication: ReplicationSlot::default(),
            snapshots: None,
            snapshot_agent: crate::snapshot::agent::SnapshotAgentBroker::new(),
        };

        let error = service
            .promote(authorized(pb::PromoteRequest::default()))
            .await
            .unwrap_err();
        assert_eq!(error.code(), tonic::Code::FailedPrecondition);
        assert!(received.try_recv().is_err());
        assert_eq!(hub.snapshot().through_sequence, head);
    }

    #[tokio::test]
    async fn unavailable_command_loop_leaves_no_requested_fact() {
        let dir = tempfile::tempdir().unwrap();
        let hub = EventHub::open(dir.path()).unwrap();
        let head = hub.snapshot().through_sequence;
        let (commands, received) = mpsc::channel(1);
        drop(received);
        let service = ControlService {
            hub: hub.clone(),
            commands,
            authorizer: test_authorizer(),
            replication: ReplicationSlot::default(),
            snapshots: None,
            snapshot_agent: crate::snapshot::agent::SnapshotAgentBroker::new(),
        };

        let error = service
            .shutdown(authorized(pb::ShutdownRequest::default()))
            .await
            .unwrap_err();
        assert_eq!(error.code(), tonic::Code::Unavailable);
        assert_eq!(hub.snapshot().through_sequence, head);
    }

    #[tokio::test]
    async fn control_and_replication_share_one_authenticated_tcp_channel() {
        let dir = tempfile::tempdir().unwrap();
        let database = dir.path().join("database");
        let hub = EventHub::open(dir.path().join("journal")).unwrap();
        let (commands, _received) = mpsc::channel(1);
        let mut config = ServerConfig::default();
        config.control.listen = "127.0.0.1:0".into();
        let auth = test_auth();
        let serving = serve(&config, &auth, hub, commands).await.unwrap();

        let db = Db::open_with(
            &database,
            DbOptions {
                shards: 2,
                ..Default::default()
            },
        )
        .unwrap();
        let expected_uuid = yesno_core::database_uuid(&database).unwrap();
        let replication = serving.replication_slot();
        replication.install(crate::replication::LeaderService::with_retention(
            &database,
            db.shard_count(),
            db.retention_floor(),
        ));

        let channel = tonic::transport::Channel::from_shared(format!(
            "http://{}",
            serving.addr.expect("test config has TCP")
        ))
        .unwrap()
        .connect()
        .await
        .unwrap();
        let mut control = pb::control_plane_client::ControlPlaneClient::new(channel.clone());
        let snapshot = control
            .get_snapshot(pb::GetSnapshotRequest {})
            .await
            .unwrap()
            .into_inner();
        assert!(snapshot.process_running);

        let mut replica =
            crate::replication::pb::replication_client::ReplicationClient::new(channel);
        let status = replica
            .status(crate::replication::pb::StatusRequest {})
            .await
            .unwrap()
            .into_inner();
        assert_eq!(status.db_uuid, expected_uuid);
        assert_eq!(status.shard_count, 2);

        replication.clear();
        let error = replica
            .status(crate::replication::pb::StatusRequest {})
            .await
            .unwrap_err();
        assert_eq!(error.code(), tonic::Code::Unavailable);

        serving.stop().await;
        drop(db);
    }

    #[tokio::test]
    async fn unix_listener_uses_local_rules_and_removes_its_socket() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("control.sock");
        let hub = EventHub::open(dir.path().join("journal")).unwrap();
        let (commands, _received) = mpsc::channel(1);
        let mut config = ServerConfig::default();
        config.control.listen.clear();
        config.control.unix_socket = Some(socket.clone());
        let auth = AuthConfig {
            rules: vec![crate::config::AuthzRule {
                channel: crate::config::AuthzChannel::Local,
                principal: "all".into(),
                address: "192.0.2.1".into(),
                capability: EndpointCapability::ControlRead,
                action: crate::config::RuleAction::Allow,
            }],
            ..Default::default()
        };
        let serving = serve(&config, &auth, hub, commands).await.unwrap();
        assert_eq!(serving.addr, None);
        assert_eq!(serving.unix_socket.as_deref(), Some(socket.as_path()));

        let channel =
            tonic::transport::Endpoint::from_shared(format!("unix://{}", socket.display()))
                .unwrap()
                .connect()
                .await
                .unwrap();
        let mut control = pb::control_plane_client::ControlPlaneClient::new(channel.clone());
        let snapshot = control
            .get_snapshot(pb::GetSnapshotRequest {})
            .await
            .unwrap()
            .into_inner();
        assert!(snapshot.process_running);

        let mut replica =
            crate::replication::pb::replication_client::ReplicationClient::new(channel);
        let error = replica
            .status(crate::replication::pb::StatusRequest {})
            .await
            .unwrap_err();
        assert_eq!(error.code(), tonic::Code::PermissionDenied);

        serving.stop().await;
        assert!(!socket.exists());
    }

    #[tokio::test]
    async fn the_control_socket_takes_the_configured_mode_and_otherwise_the_umask() {
        use std::os::unix::fs::PermissionsExt as _;

        async fn bind_with(mode: Option<&str>) -> (tempfile::TempDir, Serving, std::path::PathBuf) {
            let dir = tempfile::tempdir().unwrap();
            let socket = dir.path().join("control.sock");
            let hub = EventHub::open(dir.path().join("journal")).unwrap();
            let (commands, _received) = mpsc::channel(1);
            let mut config = ServerConfig::default();
            config.control.listen.clear();
            config.control.unix_socket = Some(socket.clone());
            config.control.unix_socket_mode = mode.map(ToOwned::to_owned);
            let serving = serve(&config, &AuthConfig::default(), hub, commands)
                .await
                .unwrap();
            (dir, serving, socket)
        }

        async fn bound_mode(mode: Option<&str>) -> u32 {
            let (_dir, serving, socket) = bind_with(mode).await;
            let mode = std::fs::symlink_metadata(&socket)
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            serving.stop().await;
            mode
        }

        // The configured mode must land exactly, whatever the umask is. Both
        // directions matter: 0660 opens the socket to a group peer that the
        // usual umask of 022 would refuse, and 0600 closes one that a umask of
        // 002 would have admitted.
        assert_eq!(bound_mode(Some("0660")).await, 0o660);
        assert_eq!(bound_mode(Some("0600")).await, 0o600);
        assert_eq!(bound_mode(Some("0o660")).await, 0o660);

        // Unset keeps bind(2)'s umask-derived mode, and that is genuinely
        // environment dependent — this test runner's umask of 002 produces
        // 0775, which already grants a group peer the write bit needed to
        // connect, while the more usual 022 produces 0755 and refuses it. That
        // spread is the whole reason the option exists, so the only thing
        // asserted here is that the default remains usable by its owner.
        assert_eq!(bound_mode(None).await & 0o600, 0o600);
    }

    #[tokio::test]
    async fn snapshot_rpc_streams_by_default_and_direct_path_needs_client_opt_in() {
        use std::sync::atomic::Ordering;
        use tokio_stream::StreamExt as _;

        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("MANIFEST"), b"immutable manifest").unwrap();
        let (snapshots, cleanups) = crate::snapshot::SnapshotManager::for_test(
            root.path(),
            std::time::Duration::from_secs(5),
            true,
        );
        let database = tempfile::tempdir().unwrap();
        let db = Arc::new(Db::open(database.path()).unwrap());
        let replication = ReplicationSlot::default();
        replication.install_with_db(
            crate::replication::LeaderService::with_retention(
                database.path(),
                db.shard_count(),
                db.retention_floor(),
            ),
            db,
        );
        let journal = tempfile::tempdir().unwrap();
        let hub = EventHub::open(journal.path()).unwrap();
        let (commands, _received) = mpsc::channel(1);
        let service = ControlService {
            hub,
            commands,
            authorizer: test_authorizer(),
            replication,
            snapshots: Some(snapshots),
            snapshot_agent: crate::snapshot::agent::SnapshotAgentBroker::new(),
        };

        let lease = service
            .begin_base_snapshot(authorized(pb::BeginBaseSnapshotRequest {}))
            .await
            .unwrap()
            .into_inner();
        assert!(lease.direct_path_available);
        assert_eq!(lease.files.len(), 1);

        let request = pb::FetchSnapshotFileRequest {
            lease_id: lease.lease_id.clone(),
            name: "MANIFEST".into(),
            allow_direct_path: false,
        };
        let mut streamed = service
            .fetch_snapshot_file(authorized(request))
            .await
            .unwrap()
            .into_inner();
        let chunk = streamed.next().await.unwrap().unwrap();
        assert_eq!(
            chunk.payload,
            Some(pb::snapshot_file_chunk::Payload::Data(
                b"immutable manifest".to_vec()
            ))
        );
        assert!(chunk.last);
        assert!(streamed.next().await.is_none());

        let request = pb::FetchSnapshotFileRequest {
            lease_id: lease.lease_id.clone(),
            name: "MANIFEST".into(),
            allow_direct_path: true,
        };
        let mut direct = service
            .fetch_snapshot_file(authorized(request))
            .await
            .unwrap()
            .into_inner();
        let chunk = direct.next().await.unwrap().unwrap();
        let Some(pb::snapshot_file_chunk::Payload::DirectPath(path)) = chunk.payload else {
            panic!("direct-path request returned bytes")
        };
        assert_eq!(Path::new(&path), root.path().join("MANIFEST"));
        assert!(chunk.last);

        service
            .keep_base_snapshot_alive(authorized(pb::KeepBaseSnapshotAliveRequest {
                lease_id: lease.lease_id.clone(),
            }))
            .await
            .unwrap();
        service
            .release_base_snapshot(authorized(pb::ReleaseBaseSnapshotRequest {
                lease_id: lease.lease_id,
            }))
            .await
            .unwrap();
        assert_eq!(cleanups.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn snapshot_materialization_does_not_hold_the_checkpoint_barrier() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("MANIFEST"), b"immutable manifest").unwrap();
        let materialize_started = Arc::new(tokio::sync::Notify::new());
        let materialize_release = Arc::new(tokio::sync::Notify::new());
        let snapshots = crate::snapshot::SnapshotManager::for_deferred_test(
            root.path(),
            materialize_started.clone(),
            materialize_release.clone(),
        );
        let database = tempfile::tempdir().unwrap();
        let db = Arc::new(Db::open(database.path()).unwrap());
        let replication = ReplicationSlot::default();
        replication.install_with_db(
            crate::replication::LeaderService::with_retention(
                database.path(),
                db.shard_count(),
                db.retention_floor(),
            ),
            db.clone(),
        );
        let journal = tempfile::tempdir().unwrap();
        let hub = EventHub::open(journal.path()).unwrap();
        let (commands, _received) = mpsc::channel(1);
        let service = Arc::new(ControlService {
            hub,
            commands,
            authorizer: test_authorizer(),
            replication,
            snapshots: Some(snapshots),
            snapshot_agent: crate::snapshot::agent::SnapshotAgentBroker::new(),
        });

        let request = {
            let service = service.clone();
            tokio::spawn(async move {
                service
                    .begin_base_snapshot(authorized(pb::BeginBaseSnapshotRequest {}))
                    .await
            })
        };
        materialize_started.notified().await;
        let checkpoint = tokio::task::spawn_blocking(move || db.checkpoint());
        tokio::time::timeout(std::time::Duration::from_secs(1), checkpoint)
            .await
            .expect("checkpoint remained blocked during snapshot materialization")
            .unwrap()
            .unwrap();

        materialize_release.notify_one();
        let lease = request.await.unwrap().unwrap().into_inner();
        service
            .release_base_snapshot(authorized(pb::ReleaseBaseSnapshotRequest {
                lease_id: lease.lease_id,
            }))
            .await
            .unwrap();
    }

    #[test]
    fn snapshot_agent_rpcs_require_the_transport_credential() {
        let request = Request::new(pb::ClaimSnapshotAgentWorkRequest {});
        assert_eq!(
            require_snapshot_agent(&request).unwrap_err().code(),
            tonic::Code::PermissionDenied
        );

        let mut request = Request::new(pb::ClaimSnapshotAgentWorkRequest {});
        request
            .extensions_mut()
            .insert(crate::auth::SnapshotAgentCredential);
        require_snapshot_agent(&request).unwrap();
    }

    #[tokio::test]
    async fn lvm_agent_capture_completion_releases_the_checkpoint_barrier() {
        let materialized = tempfile::tempdir().unwrap();
        std::fs::write(materialized.path().join("MANIFEST"), b"immutable manifest").unwrap();
        let database = tempfile::tempdir().unwrap();
        let db = Arc::new(Db::open(database.path()).unwrap());
        let replication = ReplicationSlot::default();
        replication.install_with_db(
            crate::replication::LeaderService::with_retention(
                database.path(),
                db.shard_count(),
                db.retention_floor(),
            ),
            db.clone(),
        );
        let broker = crate::snapshot::agent::SnapshotAgentBroker::new();
        let snapshot_config = crate::config::SnapshotConfig {
            backend: crate::config::SnapshotBackend::Lvm,
            lvm: Some(crate::config::LvmSnapshotConfig {
                operation_timeout_secs: 5,
                ..Default::default()
            }),
            ..Default::default()
        };
        let snapshots = crate::snapshot::SnapshotManager::from_config(
            &snapshot_config,
            database.path(),
            broker.clone(),
        );
        let journal = tempfile::tempdir().unwrap();
        let hub = EventHub::open(journal.path()).unwrap();
        let (commands, _received) = mpsc::channel(1);
        let service = Arc::new(ControlService {
            hub,
            commands,
            authorizer: test_authorizer(),
            replication,
            snapshots,
            snapshot_agent: broker.clone(),
        });

        let begin = {
            let service = service.clone();
            tokio::spawn(async move {
                service
                    .begin_base_snapshot(authorized(pb::BeginBaseSnapshotRequest {}))
                    .await
            })
        };
        let reconcile = broker.claim().await.unwrap();
        assert_eq!(reconcile.operation(), pb::SnapshotAgentOperation::Reconcile);
        complete_agent_work(&broker, reconcile, String::new());

        let capture = broker.claim().await.unwrap();
        assert_eq!(capture.operation(), pb::SnapshotAgentOperation::Capture);
        let mut checkpoint = tokio::task::spawn_blocking(move || db.checkpoint());
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), &mut checkpoint)
                .await
                .is_err(),
            "checkpoint crossed an uncompleted agent capture"
        );

        complete_agent_work(&broker, capture, String::new());
        let materialize = broker.claim().await.unwrap();
        assert_eq!(
            materialize.operation(),
            pb::SnapshotAgentOperation::Materialize
        );
        tokio::time::timeout(std::time::Duration::from_secs(1), &mut checkpoint)
            .await
            .expect("checkpoint barrier remained after agent capture completion")
            .unwrap()
            .unwrap();
        complete_agent_work(
            &broker,
            materialize,
            materialized.path().to_string_lossy().into_owned(),
        );

        let lease = begin.await.unwrap().unwrap().into_inner();
        let release = {
            let service = service.clone();
            tokio::spawn(async move {
                service
                    .release_base_snapshot(authorized(pb::ReleaseBaseSnapshotRequest {
                        lease_id: lease.lease_id,
                    }))
                    .await
            })
        };
        let cleanup = broker.claim().await.unwrap();
        assert_eq!(cleanup.operation(), pb::SnapshotAgentOperation::Cleanup);
        complete_agent_work(&broker, cleanup, String::new());
        release.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn compaction_preserves_projection_and_requires_stale_consumers_to_resync() {
        use tokio_stream::StreamExt as _;

        let dir = tempfile::tempdir().unwrap();
        let hub = EventHub::open_bounded(dir.path(), 768).unwrap();
        hub.publish_server(
            pb::Severity::Info,
            Vec::new(),
            Vec::new(),
            role_event(
                pb::EventPhase::Completed,
                pb::Role::Follower,
                pb::Role::Leader,
            ),
        )
        .unwrap();
        for n in 0..12 {
            hub.publish_server(
                pb::Severity::Info,
                Vec::new(),
                Vec::new(),
                pb::event_envelope::Payload::ControlPlane(pb::ControlPlaneEvent {
                    operation: pb::control_plane_event::Operation::JournalRecovered as i32,
                    phase: pb::EventPhase::Observed as i32,
                    value: n,
                    detail: "x".repeat(400),
                }),
            )
            .unwrap();
        }
        assert_eq!(hub.snapshot().role, pb::Role::Leader as i32);
        let (_, oldest, _, _, _) = hub.subscription(0);
        assert!(oldest > 1, "old history should have been compacted");

        let (commands, _received) = mpsc::channel(1);
        let service = ControlService {
            hub: hub.clone(),
            commands,
            authorizer: test_authorizer(),
            replication: ReplicationSlot::default(),
            snapshots: None,
            snapshot_agent: crate::snapshot::agent::SnapshotAgentBroker::new(),
        };
        let mut stream = service
            .subscribe(authorized(pb::SubscribeRequest { after_sequence: 0 }))
            .await
            .unwrap()
            .into_inner();
        let first = stream.next().await.unwrap().unwrap();
        assert!(matches!(
            first.item,
            Some(pb::subscribe_item::Item::ResyncRequired(resync))
                if resync.oldest_sequence == oldest
                    && resync.snapshot.as_ref().is_some_and(|snapshot| snapshot.role == pb::Role::Leader as i32)
        ));
        drop(hub);

        let reopened = EventHub::open_bounded(dir.path(), 768).unwrap();
        assert_eq!(reopened.snapshot().role, pb::Role::Leader as i32);
        let (events, reopened_oldest, _, _, _) = reopened.subscription(0);
        assert!(reopened_oldest > 1);
        assert!(matches!(
            events.first().and_then(|event| event.payload.as_ref()),
            Some(pb::event_envelope::Payload::ProjectionCheckpoint(_))
        ));
    }
}
