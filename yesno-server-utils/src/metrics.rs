//! The archive sidecar's Prometheus surface.
//!
//! # Why the sidecar needs one at all
//!
//! A reclamation pass decides what an archive keeps and what it destroys, and
//! until this module existed the only record of that decision was a log line.
//! One field of it is not a log line's business: **the unrecognized-key
//! count**. An unrecognized key is an object the pass could not classify, so it
//! was neither retained deliberately nor deleted — a non-zero count means the
//! archive is accumulating objects nothing will ever reclaim, and the bucket
//! grows without bound while every other signal looks healthy. That is an
//! alerting condition, and an alerting condition has to be a metric.
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
//!    sidecar that has never reclaimed anything reports no reading rather than
//!    a reassuring zero, which is the same reason the standby does not publish
//!    the leader's gauges filled with zeros.

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
    retained_bases: AtomicU64,
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
        self.retained_bases
            .store(plan.retained_bases.len() as u64, Ordering::Relaxed);
        self.last_success_unix
            .store(unix_seconds(), Ordering::Relaxed);
        self.surveyed.store(true, Ordering::Release);
    }

    /// Record one pass that ran and failed. The gauges keep the last completed
    /// pass's readings; the freshness gauge is what says they are old.
    pub fn pass_failed(&self) {
        self.failures.fetch_add(1, Ordering::Relaxed);
    }

    /// Record one pass that never started — the writer lease was not renewed,
    /// so deleting would have been deleting from an archive another writer may
    /// already own. Distinct from a failure because nothing was attempted.
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
            "Reclamation passes that completed and produced a plan.",
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
            "Passes skipped because the writer lease was not renewed.",
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
                "archive_retained_bases",
                "Base images the last completed pass kept.",
                self.retained_bases.load(Ordering::Relaxed).to_string(),
            );
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

    use prost::Message;

    use crate::archive::{pb, ArchiveStore, SCHEMA_VERSION, STATE_OBJECT};
    use crate::gc::RetentionPolicy;

    const UUID: [u8; 16] = [0x11; 16];
    const HEX: &str = "11111111111111111111111111111111";

    fn value_of(rendered: &str, family: &str) -> Option<u64> {
        rendered
            .lines()
            .find(|line| line.starts_with(&format!("yesnod_{family} ")))
            .and_then(|line| line.rsplit(' ').next())
            .and_then(|v| v.parse().ok())
    }

    fn write(root: &Path, key: &str, bytes: &[u8]) {
        let path = root.join(key);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    /// An archive with one live base, plus whatever `junk` keys the caller
    /// wants the pass to fail to classify.
    fn archive(root: &Path, junk: &[&str]) {
        let dir = format!("db/{HEX}/term/0000000000/base/{:020}", 1);
        let manifest_key = format!("{dir}/manifest.pb");
        let file_key = format!("{dir}/MANIFEST");
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_micros() as u64;

        let mut manifest = pb::BaseManifest {
            schema_version: SCHEMA_VERSION,
            database_uuid: UUID.to_vec(),
            term: 0,
            archive_generation: 1,
            checkpoint_version: 10,
            checkpoint_time: now,
            files: vec![pb::ArchiveFile {
                name: "MANIFEST".into(),
                object_key: file_key.clone(),
                size: 1,
                crc32c: 0,
            }],
            wal_cursors: vec![pb::WalCursor {
                shard: 0,
                archived_lsn: 0,
                history_fingerprint: Vec::new(),
            }],
            ..Default::default()
        };
        manifest.history_anchor = crate::history::base_anchor(&manifest);
        for cursor in &mut manifest.wal_cursors {
            cursor.history_fingerprint = crate::history::base_cursor_fingerprint(
                &manifest.history_anchor,
                cursor.shard,
                cursor.archived_lsn,
            );
        }

        let state = pb::ArchiveState {
            schema_version: SCHEMA_VERSION,
            database_uuid: UUID.to_vec(),
            latest_base_manifest: manifest_key.clone(),
            wal_cursors: vec![pb::WalCursor {
                shard: 0,
                archived_lsn: 0,
                history_fingerprint: vec![0; 32],
            }],
            ..Default::default()
        };

        write(root, STATE_OBJECT, &state.encode_to_vec());
        write(root, &manifest_key, &manifest.encode_to_vec());
        write(root, &file_key, b"x");
        for key in junk {
            write(root, key, b"who put this here");
        }
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
            RetentionPolicy {
                window_micros: 86_400 * 1_000_000,
                min_bases: 1,
            },
            1,
            metrics.clone(),
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

        let metrics = ArchiveMetrics::new();
        metrics.pass_completed(&plan);
        let rendered = metrics.render();
        assert_eq!(
            value_of(&rendered, "archive_unrecognized_keys"),
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
