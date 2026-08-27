use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use yesno_core::events::{CoreEvent, CoreEventSink, ShutdownReason};
use yesno_core::{Db, DbOptions};

#[derive(Default)]
struct RecordingSink {
    events: Mutex<Vec<CoreEvent>>,
}

impl CoreEventSink for RecordingSink {
    fn publish(&self, event: CoreEvent) {
        self.events.lock().unwrap().push(event);
    }
}

struct PanickingSink;

impl CoreEventSink for PanickingSink {
    fn publish(&self, _event: CoreEvent) {
        panic!("observer failure");
    }
}

struct CleanDir(PathBuf);

impl Drop for CleanDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tmpdir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("yesno-events-{tag}-{}-{nanos}", std::process::id()))
}

fn opts() -> DbOptions {
    DbOptions {
        shards: 1,
        ..Default::default()
    }
}

#[test]
fn durable_lifecycle_phases_are_ordered_and_correlated() {
    let dir = tmpdir("lifecycle");
    let _clean = CleanDir(dir.clone());
    let sink = Arc::new(RecordingSink::default());

    let db = Db::open_with_events(
        &dir,
        opts(),
        yesno_core::events::Events::from_arc(sink.clone()),
    )
    .unwrap();
    db.insert(7, 11).unwrap();
    db.checkpoint().unwrap();
    let shutdown = db.begin_shutdown(ShutdownReason::Requested);
    drop(db);

    let events = sink.events.lock().unwrap();
    let open = match events.first().unwrap() {
        CoreEvent::DatabaseOpenStarted { operation_id, .. } => *operation_id,
        other => panic!("first event was {other:?}"),
    };
    assert!(events.iter().any(|event| matches!(
        event,
        CoreEvent::DatabaseOpenCompleted { operation_id, .. } if *operation_id == open
    )));

    let recovery = events
        .iter()
        .find_map(|event| match event {
            CoreEvent::RecoveryStarted { operation_id, .. } => Some(*operation_id),
            _ => None,
        })
        .unwrap();
    assert!(events.iter().any(|event| matches!(
        event,
        CoreEvent::RecoveryCompleted { operation_id, .. } if *operation_id == recovery
    )));

    let checkpoint = events
        .iter()
        .find_map(|event| match event {
            CoreEvent::CheckpointStarted { operation_id, .. } => Some(*operation_id),
            _ => None,
        })
        .unwrap();
    assert!(events.iter().any(|event| matches!(
        event,
        CoreEvent::WalGenerationRotated { operation_id, .. } if *operation_id == checkpoint
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        CoreEvent::CheckpointCompleted { operation_id, .. } if *operation_id == checkpoint
    )));

    let shutdown_started = events
        .iter()
        .position(|event| {
            matches!(
                event,
                CoreEvent::DatabaseShutdownStarted { operation_id, .. } if *operation_id == shutdown
            )
        })
        .unwrap();
    let shutdown_completed = events.iter().position(|event| matches!(
        event,
        CoreEvent::DatabaseShutdownCompleted { operation_id: Some(operation_id), graceful: true, .. }
            if *operation_id == shutdown
    )).unwrap();
    assert!(shutdown_started < shutdown_completed);
    assert_eq!(shutdown_completed, events.len() - 1);
}

#[test]
fn observer_panics_do_not_change_database_semantics() {
    let dir = tmpdir("panic");
    let _clean = CleanDir(dir.clone());
    let db = Db::open_with_events(
        &dir,
        opts(),
        yesno_core::events::Events::from_arc(Arc::new(PanickingSink)),
    )
    .unwrap();
    assert!(db.insert(1, 2).unwrap());
    db.checkpoint().unwrap();
    db.begin_shutdown(ShutdownReason::Requested);
    drop(db);
}

#[test]
fn unexpected_last_owner_drop_is_reported_as_ungraceful() {
    let dir = tmpdir("unexpected-drop");
    let _clean = CleanDir(dir.clone());
    let sink = Arc::new(RecordingSink::default());
    let db = Db::open_with_events(
        &dir,
        opts(),
        yesno_core::events::Events::from_arc(sink.clone()),
    )
    .unwrap();
    drop(db);

    let events = sink.events.lock().unwrap();
    assert!(!events
        .iter()
        .any(|event| matches!(event, CoreEvent::DatabaseShutdownStarted { .. })));
    assert!(matches!(
        events.last(),
        Some(CoreEvent::DatabaseShutdownCompleted {
            operation_id: None,
            graceful: false,
            ..
        })
    ));
}

/// The soft space-amplification threshold **reports and does not intervene.**
///
/// The design specified a soft threshold alongside the hard 2x bound; only the
/// hard bound was built, so an operator learned about retention growth by
/// having a reporting query aborted. This is the warning that was missing.
///
/// Both edges are exercised, and the threshold is chosen from measurement
/// rather than guessed. Measured on this corpus: `1000` at rest, `1033` under a
/// reader with 40 rounds of churn, `1001` one checkpoint after releasing it and
/// `1000` after two — reclamation needs the second checkpoint to drain. A
/// threshold of `1010` therefore has margin on both sides, where the `1001`
/// tried first sat exactly on the recovery value and never cleared.
///
/// Driving the *threshold* rather than the ratio is what keeps this a unit
/// test: reaching real 2x retention needs a corpus far larger than one should
/// build here.
#[test]
fn the_soft_space_amp_threshold_reports_both_edges_and_evicts_nothing() {
    let dir = tmpdir("space-amp-soft");
    let _clean = CleanDir(dir.clone());
    std::fs::create_dir_all(&dir).unwrap();

    let sink = Arc::new(RecordingSink::default());
    let opts = DbOptions {
        shards: 1,
        space_amp_soft_permille: 1_010,
        ..Default::default()
    };
    let db = Db::open_with_events(
        &dir,
        opts,
        yesno_core::events::Events::from_arc(sink.clone()),
    )
    .unwrap();

    let count = |kind: &str| {
        sink.events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| match kind {
                "breach" => matches!(e, CoreEvent::SpaceAmpSoftThreshold { .. }),
                _ => matches!(e, CoreEvent::SpaceAmpSoftRecovered { .. }),
            })
            .count()
    };

    db.insert_many(1, &[1, 2, 3]).unwrap();
    db.checkpoint().unwrap();
    db.observe_space_amp();
    assert_eq!(
        count("breach"),
        0,
        "an unretained database is not in breach"
    );
    assert!(!db.space_amp_soft_breached());

    // A reader pins what the churn below supersedes, which is the whole
    // mechanism the threshold exists to watch.
    let reader = db.snapshot().unwrap();
    for round in 0..40u64 {
        let vals: Vec<u64> = (0..256).map(|i| (round << 16) | i).collect();
        db.insert_many(1, &vals).unwrap();
        db.checkpoint().unwrap();
    }

    db.observe_space_amp();
    assert_eq!(count("breach"), 1, "growing retention must be reported");
    assert!(db.space_amp_soft_breached(), "and be visible to /metrics");
    assert!(
        !reader.is_evicted(),
        "observation must not evict — the hard bound is the only intervention"
    );

    // Edge-triggered: the evaluation point is every checkpoint, so a
    // level-triggered event would fire once per checkpoint for as long as one
    // reporting query runs.
    db.observe_space_amp();
    db.observe_space_amp();
    assert_eq!(count("breach"), 1, "a sustained breach must not re-report");

    // Releasing the reader lets reclamation return the space, and the recovery
    // edge must be reported too — a dashboard that latched on the breach and
    // never cleared is its own false alarm.
    drop(reader);
    db.checkpoint().unwrap();
    db.checkpoint().unwrap();
    db.observe_space_amp();
    assert_eq!(count("recovered"), 1, "recovery must be reported");
    assert!(!db.space_amp_soft_breached(), "and the latch must clear");
}

/// The snapshot soft age **names a long-running reader and does not end it.**
///
/// A different question from the space threshold: *which* reader is holding
/// retention down, before the space it pins is large enough to notice. There
/// is deliberately no hard-age counterpart — that would be a second way to kill
/// a query, and `AbortOldestReader` remains the only intervention.
///
/// The threshold is `0` seconds so any live reader is already over it. Ages
/// are wall-clock seconds, so a test that waited for a real one would either
/// sleep for the threshold or be flaky; driving the threshold is what the
/// neighbouring space-amp test does and for the same reason.
#[test]
fn the_snapshot_soft_age_names_a_reader_and_ends_nothing() {
    let dir = tmpdir("snapshot-age-soft");
    let _clean = CleanDir(dir.clone());
    std::fs::create_dir_all(&dir).unwrap();

    let sink = Arc::new(RecordingSink::default());
    let opts = DbOptions {
        shards: 1,
        snapshot_soft_age_secs: 1,
        ..Default::default()
    };
    let db = Db::open_with_events(
        &dir,
        opts,
        yesno_core::events::Events::from_arc(sink.clone()),
    )
    .unwrap();
    db.insert_many(1, &[1, 2, 3]).unwrap();

    let count = |kind: &str| {
        sink.events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| match kind {
                "breach" => matches!(e, CoreEvent::SnapshotAgeSoftThreshold { .. }),
                _ => matches!(e, CoreEvent::SnapshotAgeSoftRecovered { .. }),
            })
            .count()
    };

    // No reader at all: nothing to report, and `0` must not read as "ancient".
    db.observe_snapshot_ages();
    assert_eq!(count("breach"), 0, "no reader is not an aged reader");
    assert_eq!(db.oldest_reader_age_secs(), 0);

    let reader = db.snapshot().unwrap();
    // A reader younger than the threshold is not reported either.
    db.observe_snapshot_ages();
    assert_eq!(count("breach"), 0, "a fresh reader is not aged");
    assert!(!db.snapshot_age_soft_breached());

    std::thread::sleep(std::time::Duration::from_millis(1_100));
    db.observe_snapshot_ages();
    assert_eq!(count("breach"), 1, "an aged reader must be named");
    assert!(db.snapshot_age_soft_breached(), "and visible to /metrics");
    assert!(
        !reader.is_evicted(),
        "naming a reader must not end it — there is no hard age"
    );

    // Edge-triggered, for the same reason the space threshold is: the caller
    // evaluates this on a timer.
    db.observe_snapshot_ages();
    db.observe_snapshot_ages();
    assert_eq!(count("breach"), 1, "a sustained breach must not re-report");

    drop(reader);
    db.observe_snapshot_ages();
    assert_eq!(count("recovered"), 1, "releasing it must clear the report");
    assert!(!db.snapshot_age_soft_breached());

    // An **evicted** reader is not an aged reader. It no longer pins
    // retention, so naming it would point an operator at a query that is not
    // the problem — and it is still a live slot, so nothing else excludes it.
    //
    // This case was missing until a sabotage exposed it: deleting the
    // evicted-slot check left every assertion above green.
    let evicted_reader = db.snapshot().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1_100));
    assert!(
        db.oldest_reader_age_secs() >= 1,
        "a live aged reader must be counted before it is evicted"
    );
    assert!(db.evict_oldest_reader(), "there is one reader to evict");
    assert!(evicted_reader.is_evicted());
    assert_eq!(
        db.oldest_reader_age_secs(),
        0,
        "an evicted reader must not be reported as the oldest"
    );
    db.observe_snapshot_ages();
    assert_eq!(
        count("breach"),
        1,
        "and must not re-open the report it cannot be the cause of"
    );
}
