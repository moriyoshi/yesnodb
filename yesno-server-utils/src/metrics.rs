//! The archive sidecar's Prometheus surface.
//!
//! # Why the sidecar needs one at all
//!
//! A reclamation pass decides what an archive keeps and what it destroys, and
//! until this module existed the only record of that decision was a log line.
//! Two fields of it are not a log line's business, and they are the two
//! directions the same survey can be wrong in:
//!
//! - **The unrecognized-key count.** An unrecognized key is an object the pass
//!   could not classify, so it was neither retained deliberately nor deleted —
//!   a non-zero count means the archive is accumulating objects nothing will
//!   ever reclaim, and the bucket grows without bound while every other signal
//!   looks healthy.
//! - **The missing-object count.** Its mirror image, and the more urgent of
//!   the two: an object a retained base names and the store does not hold. The
//!   bucket is not too big, it is incomplete, and the symptom surfaces at a
//!   restore — which is the one moment nobody wants to discover it.
//! - **The wrong-length count.** The same damage as the one above, reported
//!   by the half of the check a presence test cannot make: an object that is
//!   there and is not the length its manifest says. It fails a restore as
//!   completely as an absent one and later, after the recovery point has been
//!   chosen.
//! - **The unreadable-manifest count.** Not a third kind of damage but a
//!   change of *state*: a pass that meets one deletes nothing, and no later
//!   one will either until the object is explained. Without its own family
//!   that condition can only be inferred from a deletion count that has gone
//!   flat, which is also what a healthy archive with nothing to reclaim looks
//!   like.
//!
//! - **The oldest live object's timestamp.** The only one of these that is
//!   not damage. The three above say what has already gone wrong; this says
//!   how long an object-store lifecycle rule has left before it takes
//!   something the archive still needs, and it is alertable with arithmetic
//!   an operator can write against their own retention setting.
//!
//! All five are alerting conditions, and an alerting condition has to be a
//! metric.
//!
//! # It borrows the daemon's listener rather than inventing one
//!
//! The families are written with the daemon's own `gauge` / `counter` writers,
//! so they carry the same `yesnod_` namespace and the same help/type shape. The
//! namespace is the product's, not the binary's: an operator running the
//! sidecar next to the daemon scrapes two targets of one system, and a second
//! prefix would only make them build two dashboards. Each role keeps its own
//! family prefix — `yesnod_archive_*` here, as the standby uses
//! `yesnod_follower_*`.
//!
//! # Counter or gauge, and what a failed pass reads
//!
//! Both, deliberately, because they answer different questions:
//!
//! - **Counters** — passes, failures, skips, objects deleted — accumulate over
//!   this process's lifetime and reset on restart. They answer "is reclamation
//!   running, and is it succeeding".
//! - **Gauges** — unrecognized keys, retained bases — are the *last completed
//!   pass's* readings. They answer "what did the archive look like when it was
//!   last surveyed".
//!
//! A gauge that is only written on success is a trap: after the first
//! failure it keeps serving a stale value that looks healthy. Two things
//! defuse it here, and neither is "zero the gauge on failure" — a failed pass
//! learns nothing, and publishing zero unrecognized keys because the pass
//! *failed* would be a claim rather than an absence:
//!
//! 1. `yesnod_archive_reclamation_last_success_timestamp_seconds` publishes
//!    exactly how old the gauges are, so an alert can require freshness.
//! 2. The gauges are **not emitted at all** until a pass has completed. A
//!    sidecar that has not surveyed the archive yet reports no reading rather
//!    than a reassuring zero, which is the same reason the standby does not
//!    publish the leader's gauges filled with zeros.
//!
//! A pass that completes is not necessarily a pass that deleted: with no
//! retention policy the sidecar surveys on the same interval and deletes
//! nothing, so every gauge here is published by deployments with reclamation
//! off -- which are the ones for which the two integrity families matter
//! most. Only `archive_objects_deleted_total` tells the two modes apart.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use yesno_server::metrics::{counter, gauge, Serving};

use crate::gc::GcPlan;

/// Everything the sidecar publishes about reclamation.
///
/// Cheap enough to update under the retention loop and to read on a scrape:
/// plain relaxed atomics, no lock, and nothing sampled in the background.
#[derive(Debug, Default)]
pub struct ArchiveMetrics {
    passes: AtomicU64,
    failures: AtomicU64,
    skipped: AtomicU64,
    deleted: AtomicU64,
    /// Set once a pass has completed. Until then the last-pass gauges are
    /// withheld rather than reported as zero.
    surveyed: AtomicBool,
    unrecognized: AtomicU64,
    missing: AtomicU64,
    wrong_length: AtomicU64,
    damaged_bases: AtomicU64,
    unreadable_bases: AtomicU64,
    retained_bases: AtomicU64,
    /// Unix seconds of the oldest object the last pass kept, or zero when it
    /// kept none. Zero is the sentinel rather than a reading: 1970 would read
    /// as the most alarming value this gauge can take, so the family is
    /// withheld instead, for the reason the module comment gives about a
    /// reassuring zero.
    oldest_live_unix: AtomicU64,
    last_success_unix: AtomicU64,
}

impl ArchiveMetrics {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one pass that completed and produced a plan.
    ///
    /// `surveyed` is stored **last**, with release ordering, so a scrape that
    /// sees it also sees the values it gates. The individual gauges are relaxed
    /// because a scrape crossing an update may legitimately mix two passes'
    /// readings; it may not see uninitialized ones.
    pub fn pass_completed(&self, plan: &GcPlan) {
        self.passes.fetch_add(1, Ordering::Relaxed);
        self.deleted
            .fetch_add(plan.delete.len() as u64, Ordering::Relaxed);
        self.unrecognized
            .store(plan.unrecognized.len() as u64, Ordering::Relaxed);
        self.missing
            .store(plan.missing.len() as u64, Ordering::Relaxed);
        self.wrong_length
            .store(plan.wrong_length.len() as u64, Ordering::Relaxed);
        self.damaged_bases
            .store(plan.damaged_bases.len() as u64, Ordering::Relaxed);
        self.unreadable_bases
            .store(plan.unreadable_bases.len() as u64, Ordering::Relaxed);
        self.retained_bases
            .store(plan.retained_bases.len() as u64, Ordering::Relaxed);
        self.oldest_live_unix.store(
            plan.oldest_live_object
                .as_ref()
                .map(|(_, micros)| micros / 1_000_000)
                .unwrap_or(0),
            Ordering::Relaxed,
        );
        self.last_success_unix
            .store(unix_seconds(), Ordering::Relaxed);
        self.surveyed.store(true, Ordering::Release);
    }

    /// Record one pass that ran and failed. The gauges keep the last completed
    /// pass's readings; the freshness gauge is what says they are old.
    pub fn pass_failed(&self) {
        self.failures.fetch_add(1, Ordering::Relaxed);
    }

    /// Record one pass that never started, for either of the two reasons a
    /// pass declines to run: the writer lease was not renewed, so deleting
    /// would have been deleting from an archive another writer may already
    /// own; or a base capture was in progress, whose objects no published
    /// manifest names yet. Distinct from a failure because nothing was
    /// attempted, and in both cases the next tick simply tries again.
    pub fn pass_skipped(&self) {
        self.skipped.fetch_add(1, Ordering::Relaxed);
    }

    /// Render the Prometheus text exposition for one scrape.
    pub fn render(&self) -> String {
        let mut s = String::with_capacity(2048);

        gauge(
            &mut s,
            "archive_build_info",
            "Always 1; the labels carry the version and role.",
            format!(
                "{{version=\"{}\",role=\"archive\"}} 1",
                env!("CARGO_PKG_VERSION")
            ),
        );
        counter(
            &mut s,
            "archive_reclamation_passes_total",
            "Passes that completed and produced a plan. A deployment with \
             reclamation off surveys on the same interval and counts here too, \
             which is what keeps the gauges below fresh; it deletes nothing, so \
             archive_objects_deleted_total stays flat.",
            self.passes.load(Ordering::Relaxed),
        );
        counter(
            &mut s,
            "archive_reclamation_failures_total",
            "Reclamation passes that ran and failed.",
            self.failures.load(Ordering::Relaxed),
        );
        counter(
            &mut s,
            "archive_reclamation_skipped_total",
            "Passes skipped before anything was attempted: the writer lease was \
             not renewed, or a base capture was in progress.",
            self.skipped.load(Ordering::Relaxed),
        );
        counter(
            &mut s,
            "archive_objects_deleted_total",
            "Archive objects reclamation has deleted.",
            self.deleted.load(Ordering::Relaxed),
        );

        // Withheld until a pass has completed: see the module comment. An
        // absent family says "never surveyed"; a zero would say "surveyed and
        // clean", and only one of those is true of a sidecar that has not run
        // a pass yet.
        if self.surveyed.load(Ordering::Acquire) {
            gauge(
                &mut s,
                "archive_unrecognized_keys",
                "Objects the last completed pass could not classify, and so \
                 neither retained deliberately nor deleted. Non-zero means the \
                 archive holds objects this build does not understand and \
                 reclamation is incomplete until that is explained.",
                self.unrecognized.load(Ordering::Relaxed).to_string(),
            );
            gauge(
                &mut s,
                "archive_missing_objects",
                "Objects a base the last completed pass retained names, and \
                 the store does not hold. Non-zero means a retained recovery \
                 point cannot be restored; it is not self-healing and no later \
                 pass repairs it.",
                self.missing.load(Ordering::Relaxed).to_string(),
            );
            gauge(
                &mut s,
                "archive_wrong_length_objects",
                "Objects a base the last completed pass retained that the \
                 store holds at a length the manifest does not claim. The \
                 other half of archive_missing_objects, and the half a \
                 presence check cannot make: such an object fails a restore \
                 just as completely, and later. The usual cause is an \
                 interrupted copy of the archive rather than the store that \
                 wrote it.",
                self.wrong_length.load(Ordering::Relaxed).to_string(),
            );
            gauge(
                &mut s,
                "archive_damaged_bases",
                "Bases the last completed pass retained that name at least one \
                 object the store does not hold or holds at the wrong length, \
                 and so cannot be restored. The same damage as \
                 archive_missing_objects and archive_wrong_length_objects, \
                 counted in recovery \
                 points: one lost shared slab can cost every generation, and \
                 many lost objects can cost only one old base.",
                self.damaged_bases.load(Ordering::Relaxed).to_string(),
            );
            gauge(
                &mut s,
                "archive_unreadable_base_manifests",
                "Base manifests under this prefix whose bytes this build \
                 cannot decode. Non-zero means reclamation is halted: every \
                 deletion is justified by reachability, and a manifest that \
                 cannot be read may name any object in the store.",
                self.unreadable_bases.load(Ordering::Relaxed).to_string(),
            );
            gauge(
                &mut s,
                "archive_retained_bases",
                "Base images the last completed pass kept.",
                self.retained_bases.load(Ordering::Relaxed).to_string(),
            );
            // A timestamp rather than an age, so a scrape of a sidecar whose
            // passes have stopped does not report a floor that keeps looking
            // fresher than it is. The alert is `time() - <this>`, against the
            // operator's own lifecycle setting, which is a number this
            // process has no way to know.
            let oldest_live = self.oldest_live_unix.load(Ordering::Relaxed);
            if oldest_live != 0 {
                gauge(
                    &mut s,
                    "archive_oldest_live_object_timestamp_seconds",
                    "Last-modified time of the oldest object the last \
                     completed pass kept. An object-store lifecycle rule that \
                     expires objects older than time() minus this value \
                     deletes live archive data. Content addressing is what \
                     makes it necessary: a slab is named rather than \
                     re-uploaded by every later base that still contains it, \
                     so its age grows without bound while it stays live.",
                    oldest_live.to_string(),
                );
            }
            gauge(
                &mut s,
                "archive_reclamation_last_success_timestamp_seconds",
                "Unix time of the last completed pass. How stale the gauges above are.",
                self.last_success_unix.load(Ordering::Relaxed).to_string(),
            );
        }

        s
    }
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Serve `/metrics` and `/healthz` for the sidecar on `addr`.
pub async fn serve(
    addr: std::net::SocketAddr,
    metrics: Arc<ArchiveMetrics>,
) -> std::io::Result<Serving> {
    yesno_server::metrics::serve_text(addr, Arc::new(move || metrics.render())).await
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::Path;

    use crate::archive::ArchiveStore;
    use crate::fixture::{archive, write, HEX};
    use crate::gc::RetentionPolicy;

    fn value_of(rendered: &str, family: &str) -> Option<u64> {
        rendered
            .lines()
            .find(|line| line.starts_with(&format!("yesnod_{family} ")))
            .and_then(|line| line.rsplit(' ').next())
            .and_then(|v| v.parse().ok())
    }

    /// One real pass over a real store, so the number under test is the one the
    /// shipped reclaimer produced rather than a hand-built plan.
    async fn pass(root: &Path) -> GcPlan {
        let store = ArchiveStore::connect(&format!("file://{}", root.display())).unwrap();
        crate::gc::collect(
            &store,
            RetentionPolicy {
                window_micros: 86_400 * 1_000_000,
                min_bases: 1,
            },
        )
        .await
        .unwrap()
    }

    /// An object the active base names and the store does not hold is
    /// reported by the pass that surveyed, and reaches the gauge.
    ///
    /// Deleting the object by hand is what an object-store lifecycle rule
    /// does, and the reason that is now worth a metric is in
    /// [`crate::gc::GcPlan::missing`]: a content-addressed slab stops being
    /// re-uploaded once a generation can name it instead, so age is no longer
    /// any evidence at all about whether something is still in use.
    #[tokio::test]
    async fn an_object_the_store_lost_reaches_the_gauge() {
        let dir = tempfile::tempdir().unwrap();
        archive(dir.path(), &[]);
        let file_key = format!("db/{HEX}/term/0000000000/base/{:020}/MANIFEST", 1);
        std::fs::remove_file(dir.path().join(&file_key)).unwrap();

        let plan = pass(dir.path()).await;
        assert_eq!(plan.missing, vec![file_key], "{plan:?}");

        let metrics = ArchiveMetrics::new();
        metrics.pass_completed(&plan);
        let rendered = metrics.render();
        assert_eq!(
            value_of(&rendered, "archive_missing_objects"),
            Some(1),
            "{rendered}"
        );
        assert_eq!(
            value_of(&rendered, "archive_damaged_bases"),
            Some(1),
            "the lost object was the only base's, so the base goes with it: {rendered}"
        );
    }

    /// Set one object's last-modified time, which is what a store's listing
    /// reports and what a lifecycle rule keys on.
    fn backdate(path: &Path, when: SystemTime) {
        let file = std::fs::File::options().write(true).open(path).unwrap();
        file.set_times(std::fs::FileTimes::new().set_modified(when))
            .unwrap();
    }

    /// An object the store holds at the wrong length reaches its own gauge,
    /// and costs the base that names it.
    ///
    /// The fixture's manifest says the file is one byte. Writing two over it
    /// is what an interrupted copy of an archive leaves: the listing is
    /// complete, every key is where it should be, and a restore fails after
    /// it has chosen the recovery point.
    #[tokio::test]
    async fn an_object_of_the_wrong_length_reaches_the_gauge() {
        let dir = tempfile::tempdir().unwrap();
        archive(dir.path(), &[]);
        let file_key = format!("db/{HEX}/term/0000000000/base/{:020}/MANIFEST", 1);
        write(dir.path(), &file_key, b"xx");

        let plan = pass(dir.path()).await;
        assert!(plan.missing.is_empty(), "the object is present: {plan:?}");
        assert_eq!(
            plan.wrong_length,
            vec![format!("{file_key}: 2 bytes, manifest says 1")],
            "{plan:?}"
        );

        let metrics = ArchiveMetrics::new();
        metrics.pass_completed(&plan);
        let rendered = metrics.render();
        assert_eq!(
            value_of(&rendered, "archive_wrong_length_objects"),
            Some(1),
            "{rendered}"
        );
        assert_eq!(
            value_of(&rendered, "archive_damaged_bases"),
            Some(1),
            "the only base names it, so the base goes with it: {rendered}"
        );
    }

    /// The floor a lifecycle rule has to clear, taken off a real listing and
    /// published.
    ///
    /// Two objects here are a year old: one the pass keeps and one it is
    /// about to delete, which is the ordinary shape of an archive -- the
    /// oldest thing in the bucket is nearly always something superseded. A
    /// gauge fed by the listing alone would report the spent WAL object and
    /// tell an operator to lengthen a rule that was doing exactly its job.
    #[tokio::test]
    async fn the_oldest_object_the_archive_still_needs_reaches_the_gauge() {
        let dir = tempfile::tempdir().unwrap();
        archive(dir.path(), &[]);
        let spent = format!("db/{HEX}/term/0000000000/wal/0000/{:020}-{:020}.wal", 0, 0);
        write(dir.path(), &spent, b"spent");
        let live = format!("db/{HEX}/term/0000000000/base/{:020}/MANIFEST", 1);

        let year = SystemTime::now() - std::time::Duration::from_secs(365 * 86_400);
        backdate(&dir.path().join(&live), year);
        backdate(
            &dir.path().join(&spent),
            year - std::time::Duration::from_secs(86_400),
        );

        let plan = pass(dir.path()).await;
        assert!(
            plan.delete.contains(&spent),
            "the older of the two is reclaimable, which is the point: {plan:?}"
        );
        assert_eq!(
            plan.oldest_live_object.as_ref().map(|(key, _)| key.clone()),
            Some(live),
            "{plan:?}"
        );

        let metrics = ArchiveMetrics::new();
        metrics.pass_completed(&plan);
        let rendered = metrics.render();
        assert_eq!(
            value_of(&rendered, "archive_oldest_live_object_timestamp_seconds"),
            Some(year.duration_since(UNIX_EPOCH).unwrap().as_secs()),
            "{rendered}"
        );
    }

    /// The wiring test: the sidecar's own retention loop, over a real store
    /// with a real writer lease, must publish what its pass found. Everything
    /// else here would still pass if the loop never called `pass_completed`.
    ///
    /// Real time, not a paused clock: the loop sleeps before its first pass
    /// and the shortest interval it accepts is one second, so this test costs
    /// about that. It polls rather than sleeping a fixed span so a slow machine
    /// makes it slower, never flaky.
    #[tokio::test]
    async fn the_retention_loop_publishes_what_its_pass_found() {
        let dir = tempfile::tempdir().unwrap();
        archive(dir.path(), &[&format!("db/{HEX}/stray.txt")]);
        let store = ArchiveStore::connect(&format!("file://{}", dir.path().display())).unwrap();
        let lease = Arc::new(
            store
                .acquire_writer(crate::lease::new_writer_id().unwrap(), 30)
                .await
                .unwrap(),
        );
        let metrics = Arc::new(ArchiveMetrics::new());
        let task = tokio::spawn(crate::sidecar::retention_loop(
            store.clone(),
            lease,
            30,
            Some(RetentionPolicy {
                window_micros: 86_400 * 1_000_000,
                min_bases: 1,
            }),
            1,
            metrics.clone(),
            Arc::new(crate::sidecar::CaptureGate::default()),
        ));

        let mut rendered = metrics.render();
        for _ in 0..200 {
            if value_of(&rendered, "archive_reclamation_passes_total") == Some(1) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            rendered = metrics.render();
        }
        task.abort();

        assert_eq!(
            value_of(&rendered, "archive_reclamation_passes_total"),
            Some(1),
            "the loop never reported a pass: {rendered}"
        );
        assert_eq!(
            value_of(&rendered, "archive_unrecognized_keys"),
            Some(1),
            "{rendered}"
        );
    }

    /// With no retention policy the same loop still surveys, and still
    /// deletes nothing.
    ///
    /// Both halves matter and neither is vacuous. The archive here is missing
    /// an object a live base names, so a deployment that never configured
    /// retention -- the default, and the one most likely to be leaning on a
    /// bucket lifecycle rule -- learns about it from the gauge rather than
    /// from a restore. It also holds a WAL object wholly below the only
    /// base's cursor, which is what a reclaiming pass is for and what this one
    /// must leave alone; `archive_objects_deleted_total` has to agree with the
    /// disk, because a counter that moves while nothing is deleted is worse
    /// than no counter.
    #[tokio::test]
    async fn a_sidecar_with_reclamation_off_still_surveys() {
        let dir = tempfile::tempdir().unwrap();
        archive(dir.path(), &[]);
        let lost = format!("db/{HEX}/term/0000000000/base/{:020}/MANIFEST", 1);
        std::fs::remove_file(dir.path().join(&lost)).unwrap();
        let spent = format!("db/{HEX}/term/0000000000/wal/0000/{:020}-{:020}.wal", 0, 0);
        write(dir.path(), &spent, b"spent");

        let store = ArchiveStore::connect(&format!("file://{}", dir.path().display())).unwrap();
        let lease = Arc::new(
            store
                .acquire_writer(crate::lease::new_writer_id().unwrap(), 30)
                .await
                .unwrap(),
        );
        let metrics = Arc::new(ArchiveMetrics::new());
        let task = tokio::spawn(crate::sidecar::retention_loop(
            store.clone(),
            lease,
            30,
            None,
            1,
            metrics.clone(),
            Arc::new(crate::sidecar::CaptureGate::default()),
        ));

        let mut rendered = metrics.render();
        for _ in 0..200 {
            if value_of(&rendered, "archive_reclamation_passes_total") == Some(1) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            rendered = metrics.render();
        }
        task.abort();

        assert_eq!(
            value_of(&rendered, "archive_reclamation_passes_total"),
            Some(1),
            "the loop never surveyed without a policy: {rendered}"
        );
        assert_eq!(
            value_of(&rendered, "archive_missing_objects"),
            Some(1),
            "{rendered}"
        );
        assert_eq!(
            value_of(&rendered, "archive_objects_deleted_total"),
            Some(0),
            "a survey counted deletions: {rendered}"
        );
        assert!(
            dir.path().join(&spent).exists(),
            "a survey deleted the WAL a reclaiming pass would have"
        );
    }

    /// The gate, end to end, over the same object: a slab no manifest names
    /// is deletable, and must survive anyway while a base capture holds the
    /// gate -- because a capture in progress is exactly the case where "no
    /// manifest names it" does not mean "nothing wants it".
    ///
    /// Releasing the gate and watching the same object go is what makes this a
    /// test of exclusion rather than of a loop that never ticked.
    #[tokio::test]
    async fn a_capture_in_progress_keeps_a_pass_off_an_orphan_slab() {
        let dir = tempfile::tempdir().unwrap();
        archive(dir.path(), &[]);
        let slab_key = format!("db/{HEX}/slabs/{}", "ab".repeat(32));
        write(
            dir.path(),
            &slab_key,
            b"a slab this capture is about to name",
        );
        let slab = dir.path().join(&slab_key);

        let store = ArchiveStore::connect(&format!("file://{}", dir.path().display())).unwrap();
        let lease = Arc::new(
            store
                .acquire_writer(crate::lease::new_writer_id().unwrap(), 30)
                .await
                .unwrap(),
        );
        let metrics = Arc::new(ArchiveMetrics::new());
        let gate = Arc::new(crate::sidecar::CaptureGate::default());
        let capturing = gate.capturing().await;
        let task = tokio::spawn(crate::sidecar::retention_loop(
            store.clone(),
            lease,
            30,
            Some(RetentionPolicy {
                window_micros: 86_400 * 1_000_000,
                min_bases: 1,
            }),
            1,
            metrics.clone(),
            gate.clone(),
        ));

        let mut rendered = metrics.render();
        for _ in 0..200 {
            if value_of(&rendered, "archive_reclamation_skipped_total") == Some(1) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            rendered = metrics.render();
        }
        assert_eq!(
            value_of(&rendered, "archive_reclamation_skipped_total"),
            Some(1),
            "the gated loop never reported a skip: {rendered}"
        );
        assert_eq!(
            value_of(&rendered, "archive_reclamation_passes_total"),
            Some(0),
            "a pass ran while a capture held the gate: {rendered}"
        );
        assert!(slab.exists(), "the gated pass deleted the capture's slab");

        drop(capturing);
        for _ in 0..200 {
            if !slab.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        task.abort();
        assert!(
            !slab.exists(),
            "the slab outlived the gate: {}",
            metrics.render()
        );
    }

    /// The load-bearing one: an object nothing will ever reclaim has to reach
    /// the scrape, with its **count**, not merely a family name.
    #[tokio::test]
    async fn an_unrecognized_key_reaches_the_gauge() {
        let dir = tempfile::tempdir().unwrap();
        archive(
            dir.path(),
            &[
                &format!("db/{HEX}/term/0000000000/attic/something.bin"),
                &format!("db/{HEX}/stray.txt"),
            ],
        );
        let plan = pass(dir.path()).await;
        assert_eq!(plan.unrecognized.len(), 2, "{plan:?}");

        let metrics = ArchiveMetrics::new();
        metrics.pass_completed(&plan);
        let rendered = metrics.render();

        assert!(
            rendered.contains("# TYPE yesnod_archive_unrecognized_keys gauge"),
            "{rendered}"
        );
        assert_eq!(
            value_of(&rendered, "archive_unrecognized_keys"),
            Some(2),
            "{rendered}"
        );
        assert_eq!(
            value_of(&rendered, "archive_reclamation_passes_total"),
            Some(1),
            "{rendered}"
        );
    }

    /// Non-vacuity: a clean archive must publish zero. A gauge wired to a
    /// constant — of any value — fails this or the test above.
    #[tokio::test]
    async fn a_clean_archive_reads_zero() {
        let dir = tempfile::tempdir().unwrap();
        archive(dir.path(), &[]);
        let plan = pass(dir.path()).await;
        assert!(plan.unrecognized.is_empty(), "{plan:?}");
        assert!(plan.missing.is_empty(), "{plan:?}");
        assert!(plan.damaged_bases.is_empty(), "{plan:?}");

        let metrics = ArchiveMetrics::new();
        metrics.pass_completed(&plan);
        let rendered = metrics.render();
        assert_eq!(
            value_of(&rendered, "archive_unrecognized_keys"),
            Some(0),
            "{rendered}"
        );
        assert_eq!(
            value_of(&rendered, "archive_missing_objects"),
            Some(0),
            "{rendered}"
        );
        assert_eq!(
            value_of(&rendered, "archive_damaged_bases"),
            Some(0),
            "{rendered}"
        );
        assert_eq!(
            value_of(&rendered, "archive_retained_bases"),
            Some(1),
            "{rendered}"
        );
    }

    /// Before any pass the gauges are absent, not zero: a sidecar that has
    /// never surveyed the archive must not look like one that surveyed it and
    /// found nothing wrong. The counters are present at zero, because "no pass
    /// has run" is exactly what a zero pass count means.
    #[test]
    fn the_last_pass_gauges_are_absent_until_a_pass_completes() {
        let rendered = ArchiveMetrics::new().render();
        assert!(
            !rendered.contains("yesnod_archive_unrecognized_keys"),
            "{rendered}"
        );
        assert!(
            !rendered.contains("yesnod_archive_reclamation_last_success_timestamp_seconds"),
            "{rendered}"
        );
        assert_eq!(
            value_of(&rendered, "archive_reclamation_passes_total"),
            Some(0),
            "{rendered}"
        );
    }

    /// A failed pass leaves the last completed reading in place — it learned
    /// nothing that could replace it — and says so through the failure counter
    /// and the unmoved freshness stamp.
    #[test]
    fn a_failed_pass_keeps_the_last_reading_and_counts_itself() {
        let metrics = ArchiveMetrics::new();
        metrics.pass_completed(&GcPlan {
            unrecognized: vec!["db/x/attic/one".into(), "db/x/attic/two".into()],
            retained_bases: vec!["db/x/base/manifest.pb".into()],
            ..Default::default()
        });
        let stamp = value_of(
            &metrics.render(),
            "archive_reclamation_last_success_timestamp_seconds",
        );
        assert!(stamp.is_some_and(|t| t > 0));

        metrics.pass_failed();
        metrics.pass_skipped();
        let rendered = metrics.render();
        assert_eq!(
            value_of(&rendered, "archive_unrecognized_keys"),
            Some(2),
            "{rendered}"
        );
        assert_eq!(
            value_of(&rendered, "archive_reclamation_failures_total"),
            Some(1),
            "{rendered}"
        );
        assert_eq!(
            value_of(&rendered, "archive_reclamation_skipped_total"),
            Some(1),
            "{rendered}"
        );
        assert_eq!(
            value_of(
                &rendered,
                "archive_reclamation_last_success_timestamp_seconds"
            ),
            stamp,
            "a failed pass must not refresh the freshness stamp: {rendered}"
        );
        assert_eq!(
            value_of(&rendered, "archive_reclamation_passes_total"),
            Some(1),
            "{rendered}"
        );
    }

    /// The listener is the daemon's, so this asserts the wiring rather than the
    /// HTTP: what a scrape gets back is what `render` produced.
    #[tokio::test]
    async fn the_listener_serves_the_rendered_text() {
        let metrics = Arc::new(ArchiveMetrics::new());
        metrics.pass_completed(&GcPlan {
            unrecognized: vec!["db/x/attic/one".into()],
            ..Default::default()
        });
        let serving = serve("127.0.0.1:0".parse().unwrap(), metrics.clone())
            .await
            .unwrap();

        let body = scrape(serving.addr).await;
        assert!(
            body.contains("yesnod_archive_unrecognized_keys 1"),
            "{body}"
        );
        serving.stop().await;
    }

    /// A minimal HTTP/1.1 GET, so the test needs no client dependency.
    async fn scrape(addr: std::net::SocketAddr) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let mut socket = tokio::net::TcpStream::connect(addr).await.unwrap();
        socket
            .write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        socket.read_to_string(&mut response).await.unwrap();
        response
    }
}
