//! Reachability-based reclamation of archive objects.
//!
//! Bases, WAL frames and their descriptors are immutable and are never
//! rewritten; `state.pb` moves the active root but deletes nothing. Something
//! has to, or an archive grows without bound — but the something has to be able
//! to say, precisely, which recovery points it is destroying.
//!
//! # Retention is a duration, and that is the point
//!
//! The policy is one number: **every wall-clock instant within the window must
//! remain restorable.** That is expressible only because commit times are
//! persisted, and it is the unit an operator actually reasons in. An
//! object-store lifecycle rule keyed on upload age cannot express it: upload age
//! is not commit age, and a rule that deletes "objects older than 30 days" will
//! happily delete the base that every retained WAL object is anchored to.
//!
//! # The rule
//!
//! For a horizon `H = now - window`, the reachable set is rooted at:
//!
//! - the **active** base named by `state.pb`, always;
//! - the **newest base at or below `H`** — the root a restore to the start of
//!   the window would select — and every base after it;
//! - the newest `min_bases` bases, so a database that has been quiet for longer
//!   than the window still has a root;
//! - every base this pass cannot place in time or cannot prove complete.
//!
//! From each retained base, every WAL object in its term at or above its cursor
//! is reachable. Everything else under the database prefix is unreachable and
//! may go.
//!
//! # What is never deleted, and why each one is listed
//!
//! - `state.pb` and `writer.pb`. They are the archive, not its contents.
//! - The active base and anything reachable from it. Deleting it would destroy
//!   the archive while the sidecar was still appending to it.
//! - A base with no `checkpoint_time`. It predates commit-time stamping, so this
//!   pass **cannot place it in the window** — and "cannot place" is not
//!   "outside". Do not read a missing time as the epoch here; that reading
//!   deletes exactly the oldest history, which is the history nobody can
//!   reconstruct.
//! - A base with an empty file list and a nonzero generation. That is the
//!   durable marker of a rebase interrupted before capture, and the sidecar
//!   resumes from it on restart.
//! - Any object whose key this module does not recognize. An unrecognized key is
//!   a newer writer's, or a hand-placed file, and either way guessing is how a
//!   reclaimer becomes a data-loss incident. They are counted and reported.
//!
//! # Ordering
//!
//! Nothing is deleted until the retained set has been computed from a complete
//! listing, the writer lease has been renewed, and `state.pb` has been re-read
//! and found unchanged. That last check is what stops a pass that began
//! before a rebase from deleting the root the rebase just published — the lease
//! fences a *different writer*, not this writer's own stale plan.

use std::collections::BTreeSet;
use std::time::{SystemTime, UNIX_EPOCH};

use prost::Message;

use crate::archive::{
    pb, validate_base_manifest, ArchiveError, ArchiveStore, STATE_OBJECT, WRITER_OBJECT,
};

/// How much history to keep restorable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetentionPolicy {
    /// Every instant within this many microseconds of now stays restorable.
    pub window_micros: u64,
    /// Newest bases to keep regardless of the window.
    ///
    /// A database quiet for longer than the window would otherwise have every
    /// base fall out of it. One would be enough for correctness; the default is
    /// higher so an operator who shortens the window by mistake still has a
    /// previous root to fall back to.
    pub min_bases: usize,
}

/// What one reclamation pass decided.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GcPlan {
    /// Keys this pass would delete, sorted.
    pub delete: Vec<String>,
    /// Manifest keys of the bases being kept, sorted.
    pub retained_bases: Vec<String>,
    /// Objects kept only because their key was not recognized.
    ///
    /// Non-zero is worth an operator's attention: it means this archive holds
    /// objects this build does not understand, and reclamation is conservative
    /// rather than complete until that is explained.
    pub unrecognized: Vec<String>,
    /// The horizon this plan was computed against, in epoch microseconds.
    pub horizon_micros: u64,
}

impl GcPlan {
    /// Stable human-facing summary.
    pub fn summary_line(&self) -> String {
        format!(
            "archive gc: delete={} retained_bases={} unrecognized={} horizon={}",
            self.delete.len(),
            self.retained_bases.len(),
            self.unrecognized.len(),
            crate::restore::format_micros(self.horizon_micros),
        )
    }
}

/// A key under `db/<uuid>/term/<term>/`, classified.
enum Kind {
    BaseManifest,
    BaseFile,
    Wal { shard: u32, last_lsn: u64 },
    Unrecognized,
}

/// Classify one relative key inside the database prefix.
///
/// Structural only. It answers "what shape is this", never "is this needed";
/// a key it cannot parse is [`Kind::Unrecognized`] and is therefore kept.
fn classify(key: &str) -> (Option<u32>, Kind) {
    // db/<uuid>/term/<term>/{base|wal}/...
    let parts: Vec<&str> = key.split('/').collect();
    if parts.len() < 6 || parts[0] != "db" || parts[2] != "term" {
        return (None, Kind::Unrecognized);
    }
    let Ok(term) = parts[3].parse::<u32>() else {
        return (None, Kind::Unrecognized);
    };
    match parts[4] {
        "base" if key.ends_with("/manifest.pb") => (Some(term), Kind::BaseManifest),
        "base" => (Some(term), Kind::BaseFile),
        "wal" => {
            let Ok(shard) = parts[5].parse::<u32>() else {
                return (Some(term), Kind::Unrecognized);
            };
            // <first:020>-<last:020>.wal[.pb]
            let Some(name) = parts.get(6) else {
                return (Some(term), Kind::Unrecognized);
            };
            let stem = name
                .strip_suffix(".wal.pb")
                .or_else(|| name.strip_suffix(".wal"));
            let Some((_, last)) = stem.and_then(|s| s.split_once('-')) else {
                return (Some(term), Kind::Unrecognized);
            };
            match last.parse::<u64>() {
                Ok(last_lsn) => (Some(term), Kind::Wal { shard, last_lsn }),
                Err(_) => (Some(term), Kind::Unrecognized),
            }
        }
        _ => (Some(term), Kind::Unrecognized),
    }
}

/// Order bases newest-last. Term dominates: a later timeline supersedes an
/// earlier one regardless of the generation counters inside it.
fn base_order(m: &pb::BaseManifest) -> (u32, u64) {
    (m.term, m.archive_generation)
}

/// Decide what one pass may delete.
///
/// Pure given its inputs, so every retention decision is testable without an
/// object store. `now_micros` is a parameter for the same reason.
pub fn plan(
    state: &pb::ArchiveState,
    bases: &[(String, pb::BaseManifest)],
    keys: &[String],
    policy: RetentionPolicy,
    now_micros: u64,
) -> Result<GcPlan, ArchiveError> {
    let horizon = now_micros.saturating_sub(policy.window_micros);

    let mut ordered: Vec<&(String, pb::BaseManifest)> = bases.iter().collect();
    ordered.sort_by_key(|(_, m)| base_order(m));

    // The index of the oldest base that must be kept. Every base from here on is
    // retained, so retention is always a *suffix* of the ordering -- which is
    // what makes "a restore inside the window still finds its root" true by
    // construction rather than by case analysis.
    let mut floor = ordered.len();

    for (i, (key, manifest)) in ordered.iter().enumerate() {
        let unplaceable = manifest.checkpoint_time == 0;
        let incomplete = manifest.files.is_empty() && manifest.archive_generation != 0;
        let active = *key == state.latest_base_manifest;
        if unplaceable || incomplete || active {
            floor = floor.min(i);
        }
    }
    // The newest base at or below the horizon roots a restore to the start of
    // the window. Everything after it is retained with it.
    if let Some(i) = ordered
        .iter()
        .rposition(|(_, m)| m.checkpoint_time != 0 && m.checkpoint_time <= horizon)
    {
        floor = floor.min(i);
    } else if !ordered.is_empty() {
        // Nothing is old enough to be the window's root, so the window reaches
        // past the start of the archive and every base is inside it.
        floor = 0;
    }
    // The floor for a database quieter than its window.
    floor = floor.min(ordered.len().saturating_sub(policy.min_bases.max(1)));

    let retained: Vec<&(String, pb::BaseManifest)> = ordered[floor..].to_vec();
    let retained_keys: BTreeSet<&str> = retained.iter().map(|(k, _)| k.as_str()).collect();

    // Object keys of every retained base's files, and the per-(term, shard) LSN
    // floor below which WAL is unreachable.
    //
    // The floor is the minimum over the retained bases *of that term*, and a
    // shard is only cut when **every** retained base in the term names it. A
    // base that does not mention a shard has no cursor for it, so there is
    // nothing to compare against — and taking another base's cursor as a stand-in
    // would cut WAL the silent base still needs. Shard topology does not change
    // within a database, so this should never fire; it is here because the
    // failure if it did would be deleting a needed WAL interval, not an error.
    let mut keep_files: BTreeSet<&str> = BTreeSet::new();
    let mut wal_floor: std::collections::BTreeMap<(u32, u32), u64> = Default::default();
    let mut uncut: BTreeSet<(u32, u32)> = BTreeSet::new();
    let mut shards_by_term: std::collections::BTreeMap<u32, BTreeSet<u32>> = Default::default();
    for (_, manifest) in &retained {
        for file in &manifest.files {
            keep_files.insert(file.object_key.as_str());
        }
        shards_by_term
            .entry(manifest.term)
            .or_default()
            .extend(manifest.wal_cursors.iter().map(|c| c.shard));
    }
    for (_, manifest) in &retained {
        let named: BTreeSet<u32> = manifest.wal_cursors.iter().map(|c| c.shard).collect();
        for shard in shards_by_term
            .get(&manifest.term)
            .into_iter()
            .flatten()
            .filter(|s| !named.contains(*s))
        {
            uncut.insert((manifest.term, *shard));
        }
        for cursor in &manifest.wal_cursors {
            let slot = wal_floor
                .entry((manifest.term, cursor.shard))
                .or_insert(u64::MAX);
            *slot = (*slot).min(cursor.archived_lsn);
        }
    }
    let retained_terms: BTreeSet<u32> = retained.iter().map(|(_, m)| m.term).collect();

    let mut plan = GcPlan {
        horizon_micros: horizon,
        retained_bases: retained_keys.iter().map(|k| (*k).to_string()).collect(),
        ..Default::default()
    };

    for key in keys {
        if key == STATE_OBJECT || key == WRITER_OBJECT {
            continue;
        }
        let (term, kind) = classify(key);
        match kind {
            Kind::Unrecognized => plan.unrecognized.push(key.clone()),
            Kind::BaseManifest => {
                if !retained_keys.contains(key.as_str()) {
                    plan.delete.push(key.clone());
                }
            }
            Kind::BaseFile => {
                if !keep_files.contains(key.as_str()) {
                    plan.delete.push(key.clone());
                }
            }
            Kind::Wal { shard, last_lsn } => {
                let Some(term) = term else {
                    plan.unrecognized.push(key.clone());
                    continue;
                };
                if !retained_terms.contains(&term) {
                    plan.delete.push(key.clone());
                    continue;
                }
                if uncut.contains(&(term, shard)) {
                    continue;
                }
                match wal_floor.get(&(term, shard)) {
                    // Wholly at or below the oldest retained root's cursor: no
                    // retained recovery point can reach it.
                    Some(floor) if last_lsn <= *floor => plan.delete.push(key.clone()),
                    _ => {}
                }
            }
        }
    }
    plan.delete.sort();
    plan.unrecognized.sort();
    Ok(plan)
}

/// Wall clock, in epoch microseconds.
fn now_micros() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

/// List every base manifest under the database prefix, and every object key.
async fn survey(
    store: &ArchiveStore,
    database_uuid: &[u8],
) -> Result<(Vec<(String, pb::BaseManifest)>, Vec<String>), ArchiveError> {
    let hex: String = database_uuid.iter().map(|b| format!("{b:02x}")).collect();
    let keys = store.list_relative(&format!("db/{hex}/")).await?;
    let mut bases = Vec::new();
    for key in &keys {
        if !key.ends_with("/manifest.pb") || !key.contains("/base/") {
            continue;
        }
        let manifest = pb::BaseManifest::decode(store.get_bytes(key).await?)?;
        validate_base_manifest(&manifest)?;
        if manifest.database_uuid == database_uuid {
            bases.push((key.clone(), manifest));
        }
    }
    Ok((bases, keys))
}

/// Compute one reclamation plan without deleting anything.
///
/// Safe to run against a live archive, and what an operator should look at
/// before enabling reclamation.
pub async fn plan_for(
    store: &ArchiveStore,
    policy: RetentionPolicy,
) -> Result<GcPlan, ArchiveError> {
    let state = store.load_state().await?.ok_or("archive has no state.pb")?;
    let (bases, keys) = survey(store, &state.database_uuid).await?;
    plan(&state, &bases, &keys, policy, now_micros())
}

/// Run one reclamation pass.
///
/// The caller must hold the archive writer lease and must have renewed it. The
/// active `state.pb` is re-read immediately before the first delete and the pass
/// is abandoned if it moved, because the lease fences another *writer* and this
/// check fences this writer's own stale plan.
pub async fn collect(
    store: &ArchiveStore,
    policy: RetentionPolicy,
) -> Result<GcPlan, ArchiveError> {
    let state = store.load_state().await?.ok_or("archive has no state.pb")?;
    let (bases, keys) = survey(store, &state.database_uuid).await?;
    let plan = plan(&state, &bases, &keys, policy, now_micros())?;
    if plan.delete.is_empty() {
        return Ok(plan);
    }

    let current = store.load_state().await?.ok_or("archive has no state.pb")?;
    if current != state {
        return Err("archive state changed while reclamation was planning; retry".into());
    }

    // Every reference is removed before the thing it refers to: WAL
    // descriptors, then base manifests, then the bytes and files they named.
    //
    // A crash mid-pass then leaves *orphans* -- bytes nothing points at -- which
    // the next pass classifies as unreachable and finishes off. The other order
    // leaves *dangling references*: a manifest naming files that are gone, which
    // a restore selects and then fails to download, and which no later pass
    // cleans up because the manifest keeps the base looking retainable.
    let mut ordered = plan.delete.clone();
    ordered.sort_by_key(|k| {
        (
            !k.ends_with(".wal.pb"),
            !k.ends_with("/manifest.pb"),
            k.clone(),
        )
    });
    for key in &ordered {
        store.delete_object(key).await?;
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: u64 = 3_600_000_000;
    const NOW: u64 = 1_000 * HOUR;

    fn policy(window_hours: u64, min_bases: usize) -> RetentionPolicy {
        RetentionPolicy {
            window_micros: window_hours * HOUR,
            min_bases,
        }
    }

    /// One base: manifest key, generation, checkpoint time, and its cursor.
    fn base(generation: u64, checkpoint_time: u64, cursor_lsn: u64) -> (String, pb::BaseManifest) {
        let key = format!("db/aa/term/0000000000/base/{generation:020}/manifest.pb");
        (
            key.clone(),
            pb::BaseManifest {
                database_uuid: vec![0xaa],
                term: 0,
                archive_generation: generation,
                checkpoint_version: generation * 10,
                checkpoint_time,
                files: vec![pb::ArchiveFile {
                    name: "MANIFEST".into(),
                    object_key: format!("db/aa/term/0000000000/base/{generation:020}/MANIFEST"),
                    size: 1,
                    crc32c: 0,
                }],
                wal_cursors: vec![pb::WalCursor {
                    shard: 0,
                    archived_lsn: cursor_lsn,
                    history_fingerprint: vec![0; 32],
                }],
                ..Default::default()
            },
        )
    }

    fn wal_key(first: u64, last: u64) -> String {
        format!("db/aa/term/0000000000/wal/0000/{first:020}-{last:020}.wal")
    }

    fn state_with(active: &str) -> pb::ArchiveState {
        pb::ArchiveState {
            database_uuid: vec![0xaa],
            latest_base_manifest: active.to_string(),
            ..Default::default()
        }
    }

    /// Every key an archive holding `bases` would list.
    fn keys(bases: &[(String, pb::BaseManifest)], wal: &[(u64, u64)]) -> Vec<String> {
        let mut out = vec![STATE_OBJECT.to_string(), WRITER_OBJECT.to_string()];
        for (key, manifest) in bases {
            out.push(key.clone());
            for f in &manifest.files {
                out.push(f.object_key.clone());
            }
        }
        for (first, last) in wal {
            out.push(wal_key(*first, *last));
            out.push(format!("{}.pb", wal_key(*first, *last)));
        }
        out.sort();
        out
    }

    /// The load-bearing case: the newest base at or below the horizon is kept,
    /// because a restore to the *start* of the window is rooted there.
    #[test]
    fn the_base_rooting_the_start_of_the_window_survives() {
        let old = base(1, NOW - 100 * HOUR, 100);
        let spanning = base(2, NOW - 50 * HOUR, 200);
        let recent = base(3, NOW - 10 * HOUR, 300);
        let bases = vec![old.clone(), spanning.clone(), recent.clone()];
        let keys = keys(&bases, &[]);

        // A 24-hour window starts 24 hours ago, which is inside generation 2's
        // coverage: generation 2 is the root, generation 1 is not needed.
        let plan = plan(&state_with(&recent.0), &bases, &keys, policy(24, 1), NOW).unwrap();
        assert!(plan.delete.contains(&old.0), "{plan:?}");
        assert!(plan.delete.contains(&old.1.files[0].object_key), "{plan:?}");
        assert!(!plan.delete.contains(&spanning.0), "{plan:?}");
        assert!(!plan.delete.contains(&recent.0), "{plan:?}");
    }

    /// The one that would be a data-loss incident: a window wider than the
    /// archive must delete nothing, not fall through to "keep only the newest".
    #[test]
    fn a_window_older_than_the_archive_deletes_no_base() {
        let bases = vec![base(1, NOW - 20 * HOUR, 100), base(2, NOW - 10 * HOUR, 200)];
        let keys = keys(&bases, &[]);
        let plan = plan(&state_with(&bases[1].0), &bases, &keys, policy(500, 1), NOW).unwrap();
        assert!(plan.delete.is_empty(), "{plan:?}");
    }

    /// A base this build cannot place in time is not a base it may delete.
    #[test]
    fn an_unstamped_base_is_kept_and_pins_everything_after_it() {
        let unstamped = base(1, 0, 100);
        let newer = base(2, NOW - 200 * HOUR, 200);
        let newest = base(3, NOW - HOUR, 300);
        let bases = vec![unstamped.clone(), newer.clone(), newest.clone()];
        let keys = keys(&bases, &[]);
        let plan = plan(&state_with(&newest.0), &bases, &keys, policy(2, 1), NOW).unwrap();
        assert!(
            plan.delete.is_empty(),
            "an unplaceable base pins its suffix: {plan:?}"
        );
    }

    /// An interrupted rebase leaves an empty manifest at a nonzero generation.
    /// The sidecar resumes from it, so reclamation must not erase it.
    #[test]
    fn an_interrupted_rebase_marker_is_kept() {
        let old = base(1, NOW - 100 * HOUR, 100);
        let mut interrupted = base(2, NOW - 99 * HOUR, 200);
        interrupted.1.files.clear();
        let newest = base(3, NOW - HOUR, 300);
        let bases = vec![old.clone(), interrupted.clone(), newest.clone()];
        let keys = keys(&bases, &[]);
        let plan = plan(&state_with(&newest.0), &bases, &keys, policy(2, 1), NOW).unwrap();
        assert!(!plan.delete.contains(&interrupted.0), "{plan:?}");
        // It pins itself and everything after it, but it does **not** extend
        // retention backwards. It cannot root a restore -- it has no files -- so
        // the base before it is still governed by the window alone, and here the
        // window has moved well past it.
        assert!(plan.delete.contains(&old.0), "{plan:?}");
    }

    /// A database quiet for longer than its window still keeps roots.
    #[test]
    fn min_bases_is_a_floor_under_the_window() {
        let bases = vec![
            base(1, NOW - 400 * HOUR, 100),
            base(2, NOW - 300 * HOUR, 200),
            base(3, NOW - 200 * HOUR, 300),
        ];
        let keys = keys(&bases, &[]);
        let plan = plan(&state_with(&bases[2].0), &bases, &keys, policy(1, 2), NOW).unwrap();
        assert!(!plan.delete.contains(&bases[1].0), "{plan:?}");
        assert!(!plan.delete.contains(&bases[2].0), "{plan:?}");
        assert!(plan.delete.contains(&bases[0].0), "{plan:?}");
    }

    /// The active root is never deletable, even when the window has moved past
    /// it entirely -- a quiet database's only base is still the archive.
    #[test]
    fn the_active_base_is_never_deleted() {
        let bases = vec![base(1, 1, 100)]; // as old as an epoch-adjacent stamp can be
        let keys = keys(&bases, &[]);
        let plan = plan(&state_with(&bases[0].0), &bases, &keys, policy(1, 1), NOW).unwrap();
        assert!(plan.delete.is_empty(), "{plan:?}");
    }

    /// WAL below the oldest retained root's cursor is unreachable; WAL above it
    /// is what makes every instant in the window restorable.
    #[test]
    fn wal_is_cut_at_the_oldest_retained_cursor_and_not_above_it() {
        let old = base(1, NOW - 100 * HOUR, 100);
        let kept = base(2, NOW - 50 * HOUR, 500);
        let bases = vec![old, kept.clone()];
        let wal = [(100u64, 200u64), (400, 500), (500, 600), (600, 700)];
        let keys = keys(&bases, &wal);
        let plan = plan(&state_with(&kept.0), &bases, &keys, policy(24, 1), NOW).unwrap();

        assert!(plan.delete.contains(&wal_key(100, 200)), "{plan:?}");
        assert!(
            plan.delete.contains(&wal_key(400, 500)),
            "at the cursor: {plan:?}"
        );
        assert!(
            !plan.delete.contains(&wal_key(500, 600)),
            "above the cursor: {plan:?}"
        );
        assert!(!plan.delete.contains(&wal_key(600, 700)), "{plan:?}");
        // Descriptors follow their bytes exactly.
        assert!(
            plan.delete.contains(&format!("{}.pb", wal_key(400, 500))),
            "{plan:?}"
        );
        assert!(
            !plan.delete.contains(&format!("{}.pb", wal_key(500, 600))),
            "{plan:?}"
        );
    }

    /// A key this build does not understand is kept and reported, never guessed
    /// at. It is a newer writer's object or a hand-placed file, and either way a
    /// reclaimer that guesses becomes a data-loss incident.
    #[test]
    fn an_unrecognized_key_is_kept_and_reported() {
        let only = base(1, NOW - HOUR, 100);
        let bases = vec![only.clone()];
        let mut keys = keys(&bases, &[]);
        keys.push("db/aa/term/0000000000/attic/something.bin".into());
        keys.push("db/aa/somethingelse".into());
        keys.push("db/aa/term/0000000000/wal/0000/not-a-range.wal".into());
        let plan = plan(&state_with(&only.0), &bases, &keys, policy(1, 1), NOW).unwrap();
        assert!(plan.delete.is_empty(), "{plan:?}");
        assert_eq!(plan.unrecognized.len(), 3, "{plan:?}");
    }

    /// `state.pb` and `writer.pb` are the archive, not its contents.
    #[test]
    fn the_state_and_lease_objects_are_never_candidates() {
        let bases: Vec<(String, pb::BaseManifest)> = Vec::new();
        let keys = vec![STATE_OBJECT.to_string(), WRITER_OBJECT.to_string()];
        let plan = plan(&state_with(""), &bases, &keys, policy(1, 1), NOW).unwrap();
        assert!(plan.delete.is_empty(), "{plan:?}");
        assert!(plan.unrecognized.is_empty(), "{plan:?}");
    }

    /// WAL belonging to a term with no retained base is unreachable outright.
    #[test]
    fn wal_from_a_wholly_superseded_term_is_reclaimed() {
        let mut newer = base(1, NOW - HOUR, 0);
        newer.1.term = 5;
        let newer_key = "db/aa/term/0000000005/base/0/manifest.pb".to_string();
        let bases = vec![(newer_key.clone(), newer.1)];
        let keys = vec![
            newer_key.clone(),
            "db/aa/term/0000000000/wal/0000/00000000000000000000-00000000000000000100.wal".into(),
            "db/aa/term/0000000005/wal/0000/00000000000000000000-00000000000000000100.wal".into(),
        ];
        let plan = plan(&state_with(&newer_key), &bases, &keys, policy(1, 1), NOW).unwrap();
        assert!(
            plan.delete
                .iter()
                .any(|k| k.contains("term/0000000000/wal")),
            "{plan:?}"
        );
        assert!(
            !plan
                .delete
                .iter()
                .any(|k| k.contains("term/0000000005/wal")),
            "the retained term's WAL above its cursor must survive: {plan:?}"
        );
    }

    /// A bare number is ambiguous, and every wrong guess about the unit
    /// deletes recoverable history.
    #[test]
    fn duration_parsing_requires_a_unit() {
        use crate::sidecar::parse_duration_micros;
        assert_eq!(parse_duration_micros("7d").unwrap(), 7 * 24 * HOUR);
        assert_eq!(parse_duration_micros("72h").unwrap(), 72 * HOUR);
        assert_eq!(parse_duration_micros("90m").unwrap(), 90 * 60_000_000);
        assert_eq!(parse_duration_micros("30s").unwrap(), 30_000_000);
        assert!(
            parse_duration_micros("7").is_err(),
            "a bare number is ambiguous"
        );
        assert!(parse_duration_micros("0d").is_err(), "zero keeps nothing");
        assert!(parse_duration_micros("").is_err());
        assert!(parse_duration_micros("7w").is_err());
    }
}
