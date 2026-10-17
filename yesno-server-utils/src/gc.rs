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
//! - **Everything, if any base manifest's contents cannot be read.** The other
//!   entries on this list protect one object each; this one stops the pass
//!   deleting anything at all, because reachability is computed from the
//!   manifests and one that will not decode may name any object in the store.
//!   It is counted, reported and named rather than returned as an error, in
//!   [`GcPlan::unreadable_bases`].
//!
//! # Ordering
//!
//! Nothing is deleted until the retained set has been computed from a complete
//! listing, the writer lease has been renewed, and `state.pb` has been re-read
//! and found unchanged. That last check is what stops a pass that began
//! before a rebase from deleting the root the rebase just published — the lease
//! fences a *different writer*, not this writer's own stale plan.
//!
//! # The other thing a complete listing is good for
//!
//! A pass surveys the whole database prefix, so once it knows what the retained
//! bases name it can also check that those objects are *there*, for a set
//! difference and no extra round trip. That check is [`GcPlan::missing`], and
//! it is made here because content addressing changed what can make an object
//! vanish from under a base that still needs it.
//!
//! The same listing carries each object's last-modified time, and reachability
//! says which objects the archive still needs, so the pass can also report
//! **the oldest object it is keeping** -- [`GcPlan::oldest_live_object`]. That
//! is the one number here that warns *before* the damage: `missing` reports an
//! object a lifecycle rule has already taken, and this says how long is left
//! until it takes one.

use std::collections::BTreeSet;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::Args;
use prost::Message;

use crate::archive::{
    pb, validate_base_manifest, ArchiveError, ArchiveStore, Listed, STATE_OBJECT, WRITER_OBJECT,
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

impl RetentionPolicy {
    /// The policy that proposes nothing, for a pass that is asking the other
    /// question.
    ///
    /// Both axes are opened as far as they go, so neither can drop a base: the
    /// window reaches past the start of the archive and the floor on base
    /// count is every base there is. A plan made with it therefore retains the
    /// whole archive, which makes its [`GcPlan::missing`] the strongest form
    /// of the integrity check -- not "can the bases this policy would keep be
    /// restored" but "can every base here be restored".
    ///
    /// It still proposes deleting WAL below the oldest base's cursor, because
    /// that is unreachable however many bases are kept. A caller that is only
    /// surveying has to disregard that list rather than assume it is empty.
    pub const SURVEY: Self = Self {
        window_micros: u64::MAX,
        min_bases: usize::MAX,
    };
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
    /// Base manifests this build cannot read, each with the reason.
    ///
    /// Non-empty means **this pass deleted nothing and no later one will**
    /// until the object is explained: reachability is computed from the
    /// manifests, and one that cannot be decoded may name any slab in the
    /// store, so there is no conservative subset to fall back on -- the
    /// objects it protects are exactly the ones nothing else vouches for.
    ///
    /// Each entry is the object key and the reason, together: the message
    /// alone does not say which of an archive's thousands of objects is
    /// meant.
    pub unreadable_bases: Vec<String>,
    /// Objects a retained base names that the survey's listing did not hold.
    ///
    /// Empty is the only healthy reading. Non-zero means a base this pass is
    /// keeping cannot be restored: the manifest is there and the bytes are
    /// not, and a restore selecting that base will fail on a fetch after it
    /// has already committed to the recovery point.
    ///
    /// **Content addressing is why this is worth watching rather than
    /// assuming.** While a base owned its files under its own prefix, the only
    /// way to lose one was to delete that base's own objects. A slab is named
    /// by every later generation whose checkpoint did not rewrite it, and is
    /// deliberately *not re-uploaded* when that happens -- so its
    /// last-modified time stops advancing while it stays live, which is
    /// exactly the input an object-store lifecycle rule keys on. A rule that
    /// expires objects by age now deletes bytes of the **active** base.
    ///
    /// Only what a base names directly: its file objects and their slabs. WAL
    /// reachability is a chain walk rather than a set membership, and a
    /// restore already refuses a chain with a hole in it before fetching
    /// anything.
    pub missing: Vec<String>,
    /// Objects a retained base names that are there and are the wrong
    /// length, each with both numbers.
    ///
    /// The other half of [`GcPlan::missing`], and the half a presence check
    /// cannot see. An object that is present but short fails a restore just
    /// as completely as one that is absent, and it fails later: the restore
    /// has already selected the recovery point and fetched everything before
    /// it when the length disagrees.
    ///
    /// The rule is the one a restore applies on arrival, not a second
    /// spelling of it -- the manifest says how long each object must be, and
    /// for a file stored whole that is the file's length or its extents'
    /// total depending on whether the holes are in the object. Checking it
    /// here costs nothing extra, because an object-store listing reports
    /// sizes and the pass has already taken one.
    ///
    /// **The realistic cause is a copy rather than a store.** A PUT is
    /// atomic, so a store does not serve half an object it accepted; an
    /// interrupted `rsync` or `aws s3 sync` of the archive to somewhere else
    /// does exactly that, and leaves an archive that lists complete.
    ///
    /// Carries the same limit as `missing`: only what a base names directly.
    /// A length that agrees is not a checksum, and this says nothing about
    /// the bytes inside an object of the right size -- the CRC a restore
    /// verifies is the authority there, and it is not free.
    pub wrong_length: Vec<String>,
    /// Manifest keys of the retained bases that [`GcPlan::missing`] costs,
    /// sorted.
    ///
    /// Counts [`GcPlan::wrong_length`] too: an object of the wrong length costs
    /// the bases that name it exactly what an absent one costs them.
    ///
    /// The same damage counted in the unit an operator acts in. `missing` says
    /// how many objects are gone, which does not say how bad it is: one lost
    /// slab can be named by every generation in the archive, and a hundred
    /// lost objects can all belong to one old base nobody would restore.
    /// **Content addressing is what makes the count and the consequence come
    /// apart**, and it also makes the mapping impossible to do by hand -- a
    /// slab key is a hash, so nothing about it says which generations named
    /// it.
    ///
    /// Sorted by key, which for these keys is also oldest-first: the term and
    /// generation are zero-padded to fixed width, so lexicographic order is
    /// the retention order. The last entry is therefore the newest damaged
    /// base, and comparing it against the last of
    /// [`GcPlan::retained_bases`] distinguishes the two cases that matter --
    /// old history has holes, or the recovery point a restore would pick by
    /// default cannot be read.
    ///
    /// Carries the same limit as `missing`, and it is worth restating here
    /// because the phrasing invites more: a base that is **not** on this list
    /// is not thereby proven restorable. Only what the base names directly is
    /// checked.
    pub damaged_bases: Vec<String>,
    /// The object this pass is keeping whose last-modified time is oldest,
    /// with that time in epoch microseconds.
    ///
    /// **The number to compare an object-store lifecycle rule against.** An
    /// expiry rule set to delete objects older than thirty days destroys live
    /// data the moment this value passes thirty days ago, and it is the only
    /// reading here that says so in advance: [`GcPlan::missing`] counts
    /// objects such a rule has already taken.
    ///
    /// It is deliberately not the oldest object in the bucket. An archive
    /// always holds objects older than anything it needs -- superseded bases,
    /// spent WAL -- and those are exactly what an expiry rule is welcome to
    /// take. The candidates are what this pass is *keeping*, which is why the
    /// value is computed here rather than read off a listing.
    ///
    /// **Copying an archive resets it.** Every object an `rsync` or an
    /// `aws s3 sync` writes is new, so a fresh copy reports a recent time for
    /// a slab whose content has not been rewritten in a year. That is not an
    /// error in the reading: the copy's objects really are young, and a rule
    /// on the destination really will leave them alone for another window.
    ///
    /// `None` when nothing under the database prefix is being kept. The
    /// active base's manifest is always retained and always lives there, so
    /// for a live archive this means the prefix listed empty. `state.pb` is
    /// no help: it sits at the archive root rather than under the prefix the
    /// pass surveys.
    pub oldest_live_object: Option<(String, u64)>,
    /// The horizon this plan was computed against, in epoch microseconds.
    pub horizon_micros: u64,
}

impl GcPlan {
    /// True when nothing this pass surveyed is absent or unreadable.
    ///
    /// True when nothing it names is absent, short, or undecodable -- the
    /// three ways an archive stops being restorable without anyone asking it
    /// to.
    ///
    /// The verdict `yesnoctl archive-plan` exits on, and deliberately not a
    /// summary of everything the plan reports. `unrecognized` is left out: an
    /// archive written by a newer build has objects this one does not
    /// classify, which is worth an operator's attention and is not a reason
    /// for a scheduled check to fail. `delete` is left out because a pass
    /// having work to do is the healthy case.
    pub fn is_whole(&self) -> bool {
        self.missing.is_empty() && self.wrong_length.is_empty() && self.unreadable_bases.is_empty()
    }

    /// Stable human-facing summary.
    pub fn summary_line(&self) -> String {
        format!(
            "archive gc: delete={} retained_bases={} unrecognized={} missing={} \
             wrong_length={} damaged_bases={} unreadable_bases={} oldest_live={} horizon={}",
            self.delete.len(),
            self.retained_bases.len(),
            self.unrecognized.len(),
            self.missing.len(),
            self.wrong_length.len(),
            self.damaged_bases.len(),
            self.unreadable_bases.len(),
            match &self.oldest_live_object {
                Some((_, micros)) => crate::restore::format_micros(*micros),
                None => "none".to_string(),
            },
            crate::restore::format_micros(self.horizon_micros),
        )
    }
}

/// A key under `db/<uuid>/term/<term>/`, classified.
#[derive(Debug, PartialEq, Eq)]
enum Kind {
    BaseManifest,
    BaseFile,
    /// A content-addressed slab object, which is **not** term-scoped.
    ///
    /// Every other key here carries a term so a promoted timeline cannot
    /// overwrite an older one's identically numbered object. This one carries
    /// none, deliberately: two writers that produce this key produced the same
    /// bytes, and scoping it would defeat the sharing between generations that
    /// it exists for. So it is reclaimed purely by reachability, and the
    /// retained-terms test every other base object goes through does not apply.
    Slab,
    Wal {
        shard: u32,
        last_lsn: u64,
    },
    Unrecognized,
}

/// Classify one relative key inside the database prefix.
///
/// Structural only. It answers "what shape is this", never "is this needed";
/// a key it cannot parse is [`Kind::Unrecognized`] and is therefore kept.
fn classify(key: &str) -> (Option<u32>, Kind) {
    // db/<uuid>/term/<term>/{base|wal}/... , or db/<uuid>/slabs/<address>
    let parts: Vec<&str> = key.split('/').collect();
    if parts.len() == 4 && parts[0] == "db" && parts[2] == "slabs" {
        // 64 hex characters of SHA-256 and nothing else. A name that is not
        // one was not written by this scheme, so it stays unrecognized rather
        // than being swept by a prefix match.
        let address = parts[3];
        if address.len() == 64 && address.bytes().all(|b| b.is_ascii_hexdigit()) {
            return (None, Kind::Slab);
        }
        return (None, Kind::Unrecognized);
    }
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
    listing: &[Listed],
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
    // Each maps a key to the length the manifests say the object has, so one
    // walk answers both "is it there" and "is it whole". A key present in
    // neither map is unreachable; see `missing` and `wrong_length`.
    let mut keep_files: std::collections::BTreeMap<&str, u64> = Default::default();
    // Slab objects are named by content rather than spelled out in the
    // manifest, so this one is owned: the key is derived, not borrowed.
    let mut keep_slabs: std::collections::BTreeMap<String, u64> = Default::default();
    let mut wal_floor: std::collections::BTreeMap<(u32, u32), u64> = Default::default();
    let mut uncut: BTreeSet<(u32, u32)> = BTreeSet::new();
    let mut shards_by_term: std::collections::BTreeMap<u32, BTreeSet<u32>> = Default::default();
    for (_, manifest) in &retained {
        // Hex-encoded without the length check `uuid_hex` makes. This is a
        // planner, not a validator: a manifest that reached here came through
        // `validate_base_manifest`, and refusing one here would turn a bad
        // identity into a failed reclamation pass rather than a rejected
        // manifest. A prefix that matches no key keeps objects, which is the
        // safe direction.
        let uuid = crate::archive::hex(&manifest.database_uuid);
        for file in &manifest.files {
            // A slab-addressed file names no object of its own, and the empty
            // string is not a key. It matched nothing while this set was only
            // consulted, but `missing` searches the other way round and would
            // report it as an object the store had lost.
            if !file.object_key.is_empty() {
                keep_files.insert(file.object_key.as_str(), crate::restore::object_len(file));
            }
            for extent in &file.extents {
                if !extent.content_hash.is_empty() {
                    // First writer wins, and there is no second opinion worth
                    // having: two manifests naming one content hash with
                    // different lengths cannot both be right, since the hash
                    // is over exactly those bytes. Whichever is wrong fails
                    // its CRC at a restore, and reporting against either
                    // length here points at the same object.
                    keep_slabs
                        .entry(crate::archive::slab_key(&uuid, &extent.content_hash))
                        .or_insert(extent.length);
                }
            }
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

    for Listed { key, .. } in listing {
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
                if !keep_files.contains_key(key.as_str()) {
                    plan.delete.push(key.clone());
                }
            }
            // Reachability alone: a slab object two retained manifests name
            // survives the pruning of either, and one no retained manifest
            // names is unreachable whatever term wrote it.
            Kind::Slab => {
                if !keep_slabs.contains_key(key.as_str()) {
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
    // What the retained bases name, against what the store actually holds.
    // The listing is the same one every decision above was made from, so both
    // of these are a walk over data in hand and cost no round trip. See
    // [`GcPlan::missing`] and [`GcPlan::wrong_length`].
    let held: std::collections::BTreeMap<&str, u64> = listing
        .iter()
        .map(|listed| (listed.key.as_str(), listed.size))
        .collect();
    // Keys, kept beside the formatted messages, because attribution needs the
    // key and an operator needs the two numbers.
    let mut wrong_keys: Vec<String> = Vec::new();
    let mut check = |key: &str, want: u64, plan: &mut GcPlan| match held.get(key) {
        None => plan.missing.push(key.to_string()),
        Some(have) if *have != want => {
            wrong_keys.push(key.to_string());
            plan.wrong_length
                .push(format!("{key}: {have} bytes, manifest says {want}"));
        }
        Some(_) => {}
    };
    for (key, want) in &keep_files {
        check(key, *want, &mut plan);
    }
    for (key, want) in &keep_slabs {
        check(key, *want, &mut plan);
    }

    // Which recovery points those holes cost. A second walk over names the
    // loop above already visited, run only when there is damage to attribute,
    // because the healthy case is the one that runs every hour forever.
    if !plan.missing.is_empty() || !wrong_keys.is_empty() {
        let gone: BTreeSet<&str> = plan
            .missing
            .iter()
            .chain(wrong_keys.iter())
            .map(String::as_str)
            .collect();
        for (key, manifest) in &retained {
            let uuid = crate::archive::hex(&manifest.database_uuid);
            let damaged = manifest.files.iter().any(|file| {
                (!file.object_key.is_empty() && gone.contains(file.object_key.as_str()))
                    || file.extents.iter().any(|extent| {
                        !extent.content_hash.is_empty()
                            && gone.contains(
                                crate::archive::slab_key(&uuid, &extent.content_hash).as_str(),
                            )
                    })
            });
            if damaged {
                plan.damaged_bases.push(key.clone());
            }
        }
    }

    plan.delete.sort();
    plan.unrecognized.sort();
    plan.missing.sort();
    plan.wrong_length.sort();
    plan.damaged_bases.sort();
    plan.oldest_live_object = oldest_live(listing, &plan);
    Ok(plan)
}

/// The oldest object `plan` is keeping, by the listing's last-modified time.
///
/// Pure, and separate from [`plan`] because it asks a different question:
/// `plan` decides what may go, and this reads what the store said about what
/// stays. See [`GcPlan::oldest_live_object`] for what the answer is for.
///
/// Kept means neither proposed for deletion nor unrecognized. Unrecognized
/// objects are excluded although the pass does keep them, because it keeps
/// them out of caution rather than need -- they are somebody else's file, and
/// one of them lying in the prefix since last year would otherwise be
/// reported as the thing holding the archive's floor down.
///
/// A pass halted by an unreadable manifest has an empty `delete`, so every
/// recognized object counts as kept and the reading is as conservative as the
/// pass is. That is the right direction: a halted pass is not the moment to
/// report a later floor than the archive can justify.
///
/// Ties break on the key, so the answer does not depend on listing order --
/// an object store with second-granularity timestamps makes ties ordinary.
pub fn oldest_live(listing: &[Listed], plan: &GcPlan) -> Option<(String, u64)> {
    let going: BTreeSet<&str> = plan
        .delete
        .iter()
        .chain(plan.unrecognized.iter())
        .map(String::as_str)
        .collect();
    listing
        .iter()
        .filter(|listed| !going.contains(listed.key.as_str()))
        .min_by(|a, b| {
            a.modified_micros
                .cmp(&b.modified_micros)
                .then_with(|| a.key.cmp(&b.key))
        })
        .map(|listed| (listed.key.clone(), listed.modified_micros))
}

/// Command-line options for one reclamation preview.
#[derive(Args, Debug)]
pub struct PlanOptions {
    /// Object-store URL: file:///path, s3://bucket/prefix, or compatible S3.
    #[arg(long, env = "YESNO_ARCHIVE_STORE")]
    store: String,

    /// Retention window to plan against, as a number and a unit ( 7d, 72h ).
    ///
    /// Omit it to plan against a window nothing falls out of. The report then
    /// says nothing about what reclamation would take and everything about
    /// whether the archive is whole, which is the useful question on the
    /// default configuration -- where reclamation is off and no pass is
    /// publishing `yesnod_archive_missing_objects`.
    #[arg(long)]
    retention_window: Option<String>,

    /// Newest bases to keep regardless of the window.
    #[arg(long, default_value_t = 2)]
    retention_min_bases: usize,

    /// Print every key rather than a capped sample of each list.
    #[arg(long)]
    list: bool,
}

impl PlanOptions {
    /// The policy to plan against.
    ///
    /// No window is not "no retention": it is a window no base falls out of,
    /// so the horizon lands at the epoch and every base is retained. The
    /// report is then purely about what the archive holds.
    fn policy(&self) -> Result<RetentionPolicy, ArchiveError> {
        Ok(RetentionPolicy {
            window_micros: match self.retention_window.as_deref() {
                Some(text) => crate::sidecar::parse_duration_micros(text)?,
                None => u64::MAX,
            },
            min_bases: self.retention_min_bases.max(1),
        })
    }
}

/// How many keys of a list to print without `--list`.
const SAMPLE: usize = 20;

/// Plan one pass and report it. **Deletes nothing, ever.**
///
/// This is the command behind the advice to look before enabling reclamation:
/// `--retention-window` is irreversible in effect and, until this existed,
/// unpreviewable. It takes no writer lease, because taking one would fence the
/// sidecar that owns the prefix out of its own archive for the lease's term,
/// and nothing here writes.
///
/// A capture running concurrently is a hazard only in one narrow way, and the
/// command accepts it. A listing is served in key order and `slabs/` sorts
/// before `term/`, so a capture can upload its slabs after that part of the
/// listing was served and publish its manifest before the manifests were
/// read, which would report live objects as missing. The window is narrow,
/// the next run clears it, and the reader is watching the output -- whereas
/// the sidecar's own survey, which feeds an alert, takes the capture gate to
/// exclude it. Taking that gate here would fence the sidecar out of its own
/// archive for the duration. A concurrent capture also makes the *delete*
/// count optimistic, since objects uploaded since the listing are not in it,
/// and that number is advisory here in any case.
pub async fn run(options: PlanOptions) -> Result<GcPlan, ArchiveError> {
    let policy = options.policy()?;
    let store = ArchiveStore::connect(&options.store)?;
    let plan = plan_for(&store, policy).await?;

    // Printed in order of how much they should worry the reader. `missing` is
    // not a plan at all -- it is damage that has already happened -- so it
    // goes first even though it is the shortest list on a healthy archive.
    report(
        "unreadable base manifests",
        &plan.unreadable_bases,
        options.list,
    );
    // Before the objects, because it is the same damage in the unit the
    // reader acts in: a recovery point they can no longer take.
    report(
        "bases that cannot be restored",
        &plan.damaged_bases,
        options.list,
    );
    report("missing", &plan.missing, options.list);
    report("the wrong length", &plan.wrong_length, options.list);
    report("unrecognized", &plan.unrecognized, options.list);
    report("would delete", &plan.delete, options.list);
    // Always, including on a healthy archive, because this is the line the
    // command exists to be read for: it is what an expiry rule has to be set
    // longer than. The lists above are all empty on the archive an operator
    // is about to put a lifecycle rule on.
    if let Some((key, micros)) = &plan.oldest_live_object {
        println!(
            "oldest object still needed: {} ({key})",
            crate::restore::format_micros(*micros)
        );
    }
    Ok(plan)
}

/// One titled list, whole or sampled.
fn report(title: &str, keys: &[String], list: bool) {
    if keys.is_empty() {
        return;
    }
    let shown = if list {
        keys.len()
    } else {
        keys.len().min(SAMPLE)
    };
    println!("{title} ({}):", keys.len());
    for key in &keys[..shown] {
        println!("  {key}");
    }
    if shown < keys.len() {
        println!(
            "  ... and {} more; --list prints them all",
            keys.len() - shown
        );
    }
}

/// Wall clock, in epoch microseconds.
fn now_micros() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

/// List every base manifest under the database prefix, and every object key.
async fn survey(store: &ArchiveStore, database_uuid: &[u8]) -> Result<Survey, ArchiveError> {
    let hex: String = database_uuid.iter().map(|b| format!("{b:02x}")).collect();
    let listing = store.list_relative_meta(&format!("db/{hex}/")).await?;
    let mut bases = Vec::new();
    let mut unreadable = Vec::new();
    for Listed { key, .. } in &listing {
        if !key.ends_with("/manifest.pb") || !key.contains("/base/") {
            continue;
        }
        // The fetch failing is not the same event as the bytes being wrong.
        // A fetch is transient and the next pass should simply try again, so
        // it still ends the survey; what follows is a property of the object.
        let bytes = store.get_bytes(key).await?;
        match pb::BaseManifest::decode(bytes)
            .map_err(ArchiveError::from)
            .and_then(|manifest| validate_base_manifest(&manifest).map(|()| manifest))
        {
            Ok(manifest) if manifest.database_uuid == database_uuid => {
                bases.push((key.clone(), manifest))
            }
            Ok(_) => {}
            // Named, because the message alone does not say which object is
            // wrong and an archive holds thousands. This is the same lesson
            // `lib.rs::ctx` records for filesystem paths: the `?` that boxes
            // the error is where the only identifying information is lost.
            Err(error) => unreadable.push(format!("{key}: {error}")),
        }
    }
    Ok(Survey {
        bases,
        listing,
        unreadable,
    })
}

/// What one listing of the database prefix found.
struct Survey {
    bases: Vec<(String, pb::BaseManifest)>,
    /// Every object under the prefix, with the size and last-modified time
    /// the listing already carried. See [`Listed`].
    listing: Vec<Listed>,
    /// Base manifests under this prefix whose bytes this build cannot read,
    /// each with the reason, newest listing order.
    unreadable: Vec<String>,
}

/// Build the plan a survey supports, and withhold deletes it does not.
///
/// **A manifest that cannot be read is not merely an object to keep.** Every
/// deletion here is justified by reachability, reachability is computed from
/// the manifests, and one this build cannot decode may name any slab in the
/// store. There is no conservative subset to fall back on: the objects such a
/// manifest protects are exactly the ones nothing else can vouch for.
///
/// So the pass runs, reports, and deletes nothing. That is better than the
/// error it used to return in one way that matters -- the gauges are
/// published and the offending key is named, where before the sidecar logged
/// a bare decode message every interval and an operator had thousands of
/// objects to search. It is not better in the other: reclamation is halted
/// either way, and `yesnod_archive_unreadable_base_manifests` exists so that
/// is alertable rather than inferred from a flat deletion count.
///
/// The realistic cause is a writer from a release this one does not know,
/// which is why `backup.md` tells an operator to start a new schema version
/// in a new prefix.
fn planned(
    state: &pb::ArchiveState,
    survey: Survey,
    policy: RetentionPolicy,
) -> Result<GcPlan, ArchiveError> {
    let mut plan = plan(state, &survey.bases, &survey.listing, policy, now_micros())?;
    if !survey.unreadable.is_empty() {
        plan.delete.clear();
        plan.unreadable_bases = survey.unreadable;
        // Again, because clearing the deletions widened what the pass is
        // keeping. A halted pass should report the floor it is actually
        // holding rather than the one it would have held.
        plan.oldest_live_object = oldest_live(&survey.listing, &plan);
    }
    Ok(plan)
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
    let survey = survey(store, &state.database_uuid).await?;
    planned(&state, survey, policy)
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
    let survey = survey(store, &state.database_uuid).await?;
    let plan = planned(&state, survey, policy)?;
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
                    sparse: false,
                    extents: Vec::new(),
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

    /// The length `slab_base` gives every extent, and therefore the length
    /// its slab objects must be listed at.
    const SLAB: u64 = 1 << 21;

    /// One object as a listing would report it, at `NOW`.
    ///
    /// Sizes are not decoration here: the plan checks a kept object's length
    /// against what the manifests say, so a fixture that lists the wrong
    /// number is reporting damage it did not mean to build.
    fn at(key: &str, size: u64) -> Listed {
        Listed {
            key: key.to_string(),
            size,
            modified_micros: NOW,
        }
    }

    /// A slab object of the length `slab_base`'s extents claim.
    fn slab_object(key: &str) -> Listed {
        at(key, SLAB)
    }

    /// An object whose length no manifest speaks for, so nothing checks it.
    fn other(key: &str) -> Listed {
        at(key, 0)
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

    /// Every object an archive holding `bases` would list, each at the length
    /// its manifest says.
    fn keys(bases: &[(String, pb::BaseManifest)], wal: &[(u64, u64)]) -> Vec<Listed> {
        let mut out = vec![other(STATE_OBJECT), other(WRITER_OBJECT)];
        for (key, manifest) in bases {
            out.push(other(key));
            for f in &manifest.files {
                // A slab-addressed file names no object of its own, so it
                // contributes nothing to the listing -- its bytes are under
                // `slabs/`, which the caller adds.
                if !f.object_key.is_empty() {
                    out.push(at(&f.object_key, crate::restore::object_len(f)));
                }
            }
        }
        for (first, last) in wal {
            out.push(other(&wal_key(*first, *last)));
            out.push(other(&format!("{}.pb", wal_key(*first, *last))));
        }
        out.sort();
        out
    }

    /// A base whose files are content-addressed slabs.
    ///
    /// `shared` is in every generation; `own` is only in this one.
    fn slab_base(generation: u64, checkpoint_time: u64, own: u8) -> (String, pb::BaseManifest) {
        let (key, mut manifest) = base(generation, checkpoint_time, 100);
        let slab = |byte: u8, offset: u64| pb::FileExtent {
            offset,
            length: SLAB,
            content_hash: vec![byte; 32],
            crc32c: 0,
        };
        manifest.files = vec![pb::ArchiveFile {
            name: "shard-0000.yno".into(),
            object_key: String::new(),
            size: 1 << 22,
            crc32c: 0,
            sparse: true,
            extents: vec![slab(0xee, 0), slab(own, 1 << 21)],
        }];
        (key, manifest)
    }

    /// Slab objects are reclaimed by reachability, and shared ones survive the
    /// pruning of any single generation that names them.
    ///
    /// Without this, `classify` would leave every slab object `Unrecognized`
    /// -- kept and reported, so the archive would grow without bound while
    /// every other signal looked healthy.
    #[test]
    fn a_slab_object_survives_while_any_retained_base_names_it() {
        let old = slab_base(1, NOW - 100 * HOUR, 0x11);
        let kept = slab_base(2, NOW - HOUR, 0x22);
        let bases = vec![old.clone(), kept.clone()];
        let uuid = crate::archive::hex(&kept.1.database_uuid);
        let shared = crate::archive::slab_key(&uuid, &[0xee; 32]);
        let only_old = crate::archive::slab_key(&uuid, &[0x11; 32]);
        let only_kept = crate::archive::slab_key(&uuid, &[0x22; 32]);
        let orphan = crate::archive::slab_key(&uuid, &[0x99; 32]);

        let mut keys = keys(&bases, &[]);
        keys.extend([&shared, &only_old, &only_kept, &orphan].map(|key| slab_object(key)));
        keys.sort();

        // A window of zero roots the retention at the newest base, so `old`
        // is dropped and only what `kept` still names should survive.
        let plan = plan(&state_with(&kept.0), &bases, &keys, policy(0, 1), NOW).unwrap();

        assert!(
            plan.unrecognized.is_empty(),
            "slabs must classify: {plan:?}"
        );
        assert!(
            !plan.delete.contains(&shared),
            "named by the retained base: {plan:?}"
        );
        assert!(
            !plan.delete.contains(&only_kept),
            "named by the retained base: {plan:?}"
        );
        assert!(
            plan.delete.contains(&only_old),
            "the dropped generation's own slab is unreachable: {plan:?}"
        );
        assert!(
            plan.delete.contains(&orphan),
            "no manifest names it: {plan:?}"
        );
        // Also the guard on the empty string a slab-addressed file carries
        // where another file carries an object key. It is not a key, and
        // searching the listing for it would report every such file as lost.
        assert!(plan.missing.is_empty(), "nothing is absent here: {plan:?}");
    }

    /// One lost object, and the number of recovery points it costs is
    /// anywhere from one to all of them.
    ///
    /// This is the whole reason `damaged_bases` exists beside `missing`.
    /// Three generations here each name their own slab and one they share --
    /// which is what content addressing is for, and it means a single lost
    /// object can be the difference between losing one old base and losing
    /// the archive. A hash-named key says nothing about who named it, so this
    /// is not a mapping an operator can do by hand from the `missing` list.
    #[test]
    fn one_lost_slab_costs_every_generation_that_named_it() {
        let bases = vec![
            slab_base(1, NOW - 100 * HOUR, 0x11),
            slab_base(2, NOW - 50 * HOUR, 0x22),
            slab_base(3, NOW - HOUR, 0x33),
        ];
        let newest = bases[2].0.clone();
        let oldest = bases[0].0.clone();
        let uuid = crate::archive::hex(&bases[0].1.database_uuid);
        let slab = |byte: u8| crate::archive::slab_key(&uuid, &[byte; 32]);
        let all_slabs = [slab(0xee), slab(0x11), slab(0x22), slab(0x33)];

        let listing = |absent: &str| {
            let mut keys = keys(&bases, &[]);
            keys.extend(
                all_slabs
                    .iter()
                    .filter(|k| *k != absent)
                    .map(|k| slab_object(k)),
            );
            keys.sort();
            keys
        };
        // Everything retained, so what is reported is what the archive holds
        // rather than what this policy would have kept.
        let state = state_with(&newest);
        let shared = slab(0xee);
        let spread = plan(
            &state,
            &bases,
            &listing(&shared),
            RetentionPolicy::SURVEY,
            NOW,
        )
        .unwrap();
        assert_eq!(spread.missing, vec![shared], "{spread:?}");
        assert_eq!(
            spread.damaged_bases.len(),
            3,
            "every generation named it: {spread:?}"
        );
        assert_eq!(
            spread.damaged_bases.last(),
            spread.retained_bases.last(),
            "the newest base is among them, which is the loud case: {spread:?}"
        );

        // The same count of missing objects, one generation's worth of damage.
        let own = slab(0x11);
        let confined = plan(&state, &bases, &listing(&own), RetentionPolicy::SURVEY, NOW).unwrap();
        assert_eq!(confined.missing, vec![own], "{confined:?}");
        assert_eq!(confined.damaged_bases, vec![oldest], "{confined:?}");
        assert_ne!(
            confined.damaged_bases.last(),
            confined.retained_bases.last(),
            "only old history is gone, and the counts cannot say so: {confined:?}"
        );

        let whole = plan(&state, &bases, &listing(""), RetentionPolicy::SURVEY, NOW).unwrap();
        assert!(whole.missing.is_empty(), "{whole:?}");
        assert!(whole.damaged_bases.is_empty(), "{whole:?}");
    }

    /// The oldest object an archive still needs is not the oldest object it
    /// holds, and the difference is the whole reading.
    ///
    /// A healthy archive always holds objects older than anything it needs --
    /// superseded bases, spent WAL -- and those are exactly what an expiry
    /// rule is welcome to take. An operator who set a lifecycle rule from the
    /// oldest key in the bucket would lengthen it for the one object that
    /// does not matter, every time.
    #[test]
    fn the_oldest_object_kept_is_not_the_oldest_object_listed() {
        let dropped = slab_base(1, NOW - 100 * HOUR, 0x11);
        let kept = slab_base(2, NOW - HOUR, 0x22);
        let bases = vec![dropped.clone(), kept.clone()];
        let uuid = crate::archive::hex(&kept.1.database_uuid);
        let shared = crate::archive::slab_key(&uuid, &[0xee; 32]);
        let only_dropped = crate::archive::slab_key(&uuid, &[0x11; 32]);
        let only_kept = crate::archive::slab_key(&uuid, &[0x22; 32]);
        let stray = format!("db/{uuid}/notes-from-another-tool.txt");

        let mut keys = keys(&bases, &[]);
        keys.extend([&shared, &only_dropped, &only_kept].map(|key| slab_object(key)));
        keys.push(other(&stray));
        keys.sort();

        // A window of zero roots retention at the newest base, so generation
        // 1 goes and the slab only it named goes with it.
        let plan = plan(&state_with(&kept.0), &bases, &keys, policy(0, 1), NOW).unwrap();
        assert!(plan.delete.contains(&only_dropped), "{plan:?}");
        assert_eq!(plan.unrecognized, vec![stray.clone()], "{plan:?}");

        // Two objects are older than the shared slab: one this pass is about
        // to delete, and one it keeps only because it cannot classify it.
        let listing: Vec<Listed> = keys
            .iter()
            .map(|listed| {
                let age = if listed.key == stray {
                    1000 * HOUR
                } else if listed.key == only_dropped {
                    500 * HOUR
                } else if listed.key == shared {
                    300 * HOUR
                } else {
                    HOUR
                };
                Listed {
                    modified_micros: NOW - age,
                    ..listed.clone()
                }
            })
            .collect();

        assert_eq!(
            oldest_live(&listing, &plan),
            Some((shared.clone(), NOW - 300 * HOUR)),
            "the floor is the oldest object the archive still needs"
        );

        // And it does not depend on the order the listing arrived in: with
        // every kept object the same age, the answer is the first key rather
        // than whichever one was scanned first.
        let flat: Vec<Listed> = listing
            .iter()
            .rev()
            .map(|listed| Listed {
                modified_micros: NOW - HOUR,
                ..listed.clone()
            })
            .collect();
        let first_kept = keys
            .iter()
            .map(|listed| &listed.key)
            .find(|key| !plan.delete.contains(key) && **key != stray)
            .unwrap();
        assert_eq!(
            oldest_live(&flat, &plan),
            Some((first_kept.clone(), NOW - HOUR)),
            "ties break on the key"
        );
    }

    /// An object that is there and is the wrong length is damage the
    /// presence check cannot see, and it costs the same recovery points.
    ///
    /// A restore discovers it late: the recovery point has been selected and
    /// everything before the short object fetched by the time the length
    /// disagrees. The survey already holds the sizes, so it can say so first.
    #[test]
    fn an_object_that_is_there_and_short_costs_the_bases_that_name_it() {
        let bases = vec![
            slab_base(1, NOW - 100 * HOUR, 0x11),
            slab_base(2, NOW - HOUR, 0x22),
        ];
        let uuid = crate::archive::hex(&bases[0].1.database_uuid);
        let slab = |byte: u8| crate::archive::slab_key(&uuid, &[byte; 32]);
        let shared = slab(0xee);

        let mut keys = keys(&bases, &[]);
        keys.extend([slab(0x11), slab(0x22), shared.clone()].map(|key| slab_object(&key)));
        // The shared slab arrived half written, which is what an interrupted
        // copy of an archive leaves behind.
        keys.iter_mut()
            .find(|listed| listed.key == shared)
            .unwrap()
            .size = SLAB / 2;
        keys.sort();

        let state = state_with(&bases[1].0);
        let plan = plan(&state, &bases, &keys, RetentionPolicy::SURVEY, NOW).unwrap();

        assert!(
            plan.missing.is_empty(),
            "the object is present, which is exactly the trap: {plan:?}"
        );
        assert_eq!(
            plan.wrong_length,
            vec![format!(
                "{shared}: {} bytes, manifest says {SLAB}",
                SLAB / 2
            )],
            "both numbers, because neither alone says what to do: {plan:?}"
        );
        assert_eq!(
            plan.damaged_bases.len(),
            2,
            "every generation named it: {plan:?}"
        );
        assert!(!plan.is_whole(), "{plan:?}");
    }

    /// The same check on a whole-file object, where the length a manifest
    /// speaks for is the file's own rather than its extents' total.
    ///
    /// Two rules, one of each kind of archive, and the pass has to apply the
    /// one the manifest asks for -- reading a pre-slab object against an
    /// extent total would report every such archive as damaged.
    #[test]
    fn a_whole_file_object_of_the_wrong_length_is_reported_too() {
        let only = base(1, NOW - HOUR, 100);
        let bases = vec![only.clone()];
        let object = only.1.files[0].object_key.clone();

        let mut keys = keys(&bases, &[]);
        keys.iter_mut()
            .find(|listed| listed.key == object)
            .unwrap()
            .size = 9;

        let plan = plan(&state_with(&only.0), &bases, &keys, policy(24, 1), NOW).unwrap();

        assert_eq!(
            plan.wrong_length,
            vec![format!("{object}: 9 bytes, manifest says 1")],
            "{plan:?}"
        );
        assert_eq!(plan.damaged_bases, vec![only.0], "{plan:?}");
    }

    /// A sparse file stored whole in one object is measured by its runs and
    /// not by the file they belong to.
    ///
    /// The object holds the present runs concatenated, so its length is their
    /// total while the file's own length counts the holes as well. Reading
    /// one against the other reports every sparse archive as damaged, or
    /// misses damage in every pre-slab one. Both sizes here are written out
    /// rather than taken from the same helper the production path uses,
    /// because a shared derivation cannot catch itself being wrong.
    #[test]
    fn a_sparse_object_is_measured_by_its_runs_and_not_by_the_file() {
        let (key, mut manifest) = base(1, NOW - HOUR, 100);
        let object = manifest.files[0].object_key.clone();
        let run = |offset: u64, length: u64| pb::FileExtent {
            offset,
            length,
            content_hash: Vec::new(),
            crc32c: 0,
        };
        manifest.files[0].size = 4096;
        manifest.files[0].sparse = true;
        manifest.files[0].extents = vec![run(0, 16), run(2048, 32)];
        let bases = vec![(key.clone(), manifest)];
        let state = state_with(&key);

        let whole = vec![other(STATE_OBJECT), other(&key), at(&object, 48)];
        let plan = plan(&state, &bases, &whole, policy(24, 1), NOW).unwrap();
        assert!(
            plan.is_whole(),
            "forty-eight bytes is what the two runs total: {plan:?}"
        );

        let holes_too = vec![other(STATE_OBJECT), other(&key), at(&object, 4096)];
        let bad = super::plan(&state, &bases, &holes_too, policy(24, 1), NOW).unwrap();
        assert_eq!(
            bad.wrong_length,
            vec![format!("{object}: 4096 bytes, manifest says 48")],
            "an object carrying the holes as well is not this manifest's: {bad:?}"
        );
    }

    /// The verdict a scheduled check exits on, and what it deliberately
    /// ignores.
    ///
    /// A pass with work to do is healthy, and an archive holding objects a
    /// newer writer put there is a thing to look into rather than a nightly
    /// failure. Neither may turn the check red, or it stops being run.
    #[test]
    fn the_verdict_is_damage_and_not_housekeeping() {
        let whole = GcPlan::default();
        assert!(whole.is_whole(), "{whole:?}");

        let busy = GcPlan {
            delete: vec!["spent".into()],
            unrecognized: vec!["who put this here".into()],
            ..Default::default()
        };
        assert!(busy.is_whole(), "neither is damage: {busy:?}");

        let lost = GcPlan {
            missing: vec!["gone".into()],
            ..Default::default()
        };
        assert!(!lost.is_whole(), "{lost:?}");

        let blind = GcPlan {
            unreadable_bases: vec!["cannot decode".into()],
            ..Default::default()
        };
        assert!(
            !blind.is_whole(),
            "a pass that cannot read a manifest has not checked anything: {blind:?}"
        );
    }

    /// Every file under `root`, so a preview can be shown to have changed
    /// nothing.
    fn object_count(root: &std::path::Path) -> usize {
        fn walk(dir: &std::path::Path, n: &mut usize) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, n);
                } else {
                    *n += 1;
                }
            }
        }
        let mut n = 0;
        walk(root, &mut n);
        n
    }

    /// One manifest this build cannot read must not end reclamation for the
    /// whole archive, and must name itself when it stops it.
    ///
    /// Before this, `survey` propagated the decode failure, so the sidecar
    /// logged a bare "unsupported base-manifest schema" every interval, no
    /// pass ever completed, no gauge was ever published, and the operator had
    /// an archive full of objects and no idea which one was meant. The
    /// realistic way to get there is a writer from a release this one does
    /// not know.
    ///
    /// The healthy plan is computed first so the second assertion is about
    /// the manifest and not about there being nothing to delete.
    #[tokio::test]
    async fn a_manifest_this_build_cannot_read_halts_deletion_and_names_itself() {
        let dir = tempfile::tempdir().unwrap();
        let hex = crate::fixture::HEX;
        crate::fixture::archive(dir.path(), &[]);
        // Wholly at or below the only base's cursor, so no retained recovery
        // point reaches it: what a healthy pass is supposed to remove.
        let wal = format!("db/{hex}/term/0000000000/wal/0000/{:020}-{:020}.wal", 0, 0);
        crate::fixture::write(dir.path(), &wal, b"spent");

        let policy = RetentionPolicy {
            window_micros: 0,
            min_bases: 1,
        };
        let store = ArchiveStore::connect(&format!("file://{}", dir.path().display())).unwrap();
        let healthy = plan_for(&store, policy).await.unwrap();
        assert_eq!(healthy.delete, vec![wal], "{healthy:?}");
        assert!(healthy.unreadable_bases.is_empty(), "{healthy:?}");

        let bad = format!("db/{hex}/term/0000000000/base/{:020}/manifest.pb", 2);
        crate::fixture::write(
            dir.path(),
            &bad,
            &pb::BaseManifest {
                schema_version: 99,
                ..Default::default()
            }
            .encode_to_vec(),
        );

        let blind = plan_for(&store, policy).await.unwrap();
        assert!(blind.delete.is_empty(), "{blind:?}");
        assert_eq!(blind.unreadable_bases.len(), 1, "{blind:?}");
        assert!(blind.unreadable_bases[0].starts_with(&bad), "{blind:?}");
        assert!(
            blind.unreadable_bases[0].contains("schema"),
            "the reason has to travel with the key: {blind:?}"
        );
    }

    /// The preview over a real store: it reports, and it deletes nothing.
    ///
    /// The second half is the whole reason the command exists. An operator
    /// reaches for it precisely when they are not yet willing to let anything
    /// be deleted, so a test that only checked the numbers would miss the
    /// property they are relying on.
    #[tokio::test]
    async fn a_preview_reports_a_lost_object_and_removes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let hex = crate::fixture::HEX;
        crate::fixture::archive(dir.path(), &[&format!("db/{hex}/stray.txt")]);
        let file_key = format!("db/{hex}/term/0000000000/base/{:020}/MANIFEST", 1);
        std::fs::remove_file(dir.path().join(&file_key)).unwrap();
        let before = object_count(dir.path());

        let plan = run(PlanOptions {
            store: format!("file://{}", dir.path().display()),
            retention_window: None,
            retention_min_bases: 2,
            list: false,
        })
        .await
        .unwrap();

        assert_eq!(plan.missing, vec![file_key], "{plan:?}");
        assert_eq!(plan.unrecognized.len(), 1, "{plan:?}");
        assert!(plan.delete.is_empty(), "{plan:?}");
        assert_eq!(
            object_count(dir.path()),
            before,
            "a preview must not touch the store"
        );
    }

    /// The preview's default: a window nothing falls out of, so the report is
    /// about what the archive holds rather than about what a policy would
    /// take. Without this, an operator asking "is my archive whole" would be
    /// shown the answer for a seven-day window they never asked for.
    #[test]
    fn a_preview_with_no_window_retains_every_base() {
        let options = PlanOptions {
            store: "file:///archive".into(),
            retention_window: None,
            retention_min_bases: 2,
            list: false,
        };
        let policy = options.policy().unwrap();
        assert_eq!(policy.window_micros, u64::MAX);

        let bases = vec![
            base(1, NOW - 1_000 * HOUR, 100),
            base(2, NOW - 500 * HOUR, 200),
            base(3, NOW - HOUR, 300),
        ];
        let keys = keys(&bases, &[]);
        let plan = plan(&state_with(&bases[2].0), &bases, &keys, policy, NOW).unwrap();

        assert_eq!(plan.retained_bases.len(), 3, "{plan:?}");
        assert!(plan.delete.is_empty(), "{plan:?}");
    }

    /// Zero would keep nothing regardless of the window, which is the one
    /// setting a preview must not be able to produce.
    #[test]
    fn a_preview_floors_min_bases_at_one() {
        let options = PlanOptions {
            store: "file:///archive".into(),
            retention_window: Some("7d".into()),
            retention_min_bases: 0,
            list: false,
        };
        assert_eq!(options.policy().unwrap().min_bases, 1);
    }

    /// A slab a retained base names and the store does not hold is reported
    /// by the pass that noticed, not left for a restore to discover.
    ///
    /// This is the shape an object-store lifecycle rule produces: the slab was
    /// uploaded by the first generation, named without being re-sent by every
    /// generation since -- so its last-modified time never advanced -- and
    /// then expired on its age while the base that needs it is the active one.
    #[test]
    fn a_slab_the_store_lost_is_reported_rather_than_waiting_for_a_restore() {
        let old = slab_base(1, NOW - 100 * HOUR, 0x11);
        let kept = slab_base(2, NOW - HOUR, 0x22);
        let bases = vec![old.clone(), kept.clone()];
        let uuid = crate::archive::hex(&kept.1.database_uuid);
        let shared = crate::archive::slab_key(&uuid, &[0xee; 32]);
        let only_kept = crate::archive::slab_key(&uuid, &[0x22; 32]);

        // Everything the two bases name except `shared`.
        let mut keys = keys(&bases, &[]);
        keys.push(slab_object(&only_kept));
        keys.sort();

        let plan = plan(&state_with(&kept.0), &bases, &keys, policy(0, 1), NOW).unwrap();

        assert_eq!(plan.missing, vec![shared], "{plan:?}");
    }

    /// Only the bases the pass is keeping. An object a pruned base named is
    /// unreachable by the very decision being reported, and counting it would
    /// make every ordinary reclamation look like damage.
    #[test]
    fn an_object_only_a_pruned_base_named_is_not_reported_missing() {
        let old = slab_base(1, NOW - 100 * HOUR, 0x11);
        let kept = slab_base(2, NOW - HOUR, 0x22);
        let bases = vec![old.clone(), kept.clone()];
        let uuid = crate::archive::hex(&kept.1.database_uuid);

        // `only_old` -- the pruned generation's own slab -- is absent.
        let mut keys = keys(&bases, &[]);
        keys.extend(
            [
                crate::archive::slab_key(&uuid, &[0xee; 32]),
                crate::archive::slab_key(&uuid, &[0x22; 32]),
            ]
            .map(|key| slab_object(&key)),
        );
        keys.sort();

        let plan = plan(&state_with(&kept.0), &bases, &keys, policy(0, 1), NOW).unwrap();

        assert!(plan.missing.is_empty(), "{plan:?}");
    }

    /// The same check reaches a whole-file base object, which is what an
    /// archive written before slabs holds.
    #[test]
    fn a_whole_file_base_object_the_store_lost_is_reported_too() {
        let only = base(1, NOW - HOUR, 100);
        let bases = vec![only.clone()];
        let keys = vec![other(STATE_OBJECT), other(WRITER_OBJECT), other(&only.0)];

        let plan = plan(&state_with(&only.0), &bases, &keys, policy(24, 1), NOW).unwrap();

        assert_eq!(
            plan.missing,
            vec![only.1.files[0].object_key.clone()],
            "{plan:?}"
        );
    }

    /// A key under `slabs/` that is not an address is not swept by the prefix.
    #[test]
    fn a_slab_prefix_key_that_is_not_an_address_is_kept_and_reported() {
        for key in [
            "db/aa/slabs/not-a-hash",
            "db/aa/slabs/00112233",
            "db/aa/slabs/deadbeef/extra",
        ] {
            assert_eq!(classify(key).1, Kind::Unrecognized, "{key}");
        }
        assert_eq!(
            classify(&format!("db/aa/slabs/{}", "ab".repeat(32))).1,
            Kind::Slab
        );
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
        keys.push(other("db/aa/term/0000000000/attic/something.bin"));
        keys.push(other("db/aa/somethingelse"));
        keys.push(other("db/aa/term/0000000000/wal/0000/not-a-range.wal"));
        let plan = plan(&state_with(&only.0), &bases, &keys, policy(1, 1), NOW).unwrap();
        assert!(plan.delete.is_empty(), "{plan:?}");
        assert_eq!(plan.unrecognized.len(), 3, "{plan:?}");
    }

    /// `state.pb` and `writer.pb` are the archive, not its contents.
    #[test]
    fn the_state_and_lease_objects_are_never_candidates() {
        let bases: Vec<(String, pb::BaseManifest)> = Vec::new();
        let keys = vec![other(STATE_OBJECT), other(WRITER_OBJECT)];
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
            other(&newer_key),
            other("db/aa/term/0000000000/wal/0000/00000000000000000000-00000000000000000100.wal"),
            other("db/aa/term/0000000005/wal/0000/00000000000000000000-00000000000000000100.wal"),
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
