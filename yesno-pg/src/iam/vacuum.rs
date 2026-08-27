//! `ambulkdelete` and `amvacuumcleanup`.
//!
//! # Why this needs key enumeration
//!
//! When VACUUM removes heap tuples, the index must drop those TIDs — and
//! doing that means visiting **every key** the index wrote. There is no reverse
//! map from an ordinal to the keys containing it, so the only way is to
//! enumerate. That requirement is what put `Snapshot::keys` into `yesno-core`;
//! before it existed, this callback could not be written.
//!
//! A key skipped here leaves dangling TIDs: the index keeps returning
//! pointers to tuples that no longer exist.
//!
//! **But that is bloat, not a wrong answer, and the difference was measured
//! rather than assumed.** The planning note for this AM predicted "wrong rows or
//! a heap-fetch error"; it is wrong, because [`super::scan::amgetbitmap`] sets
//! `recheck` unconditionally. The Bitmap Heap Scan therefore re-evaluates the
//! qual against the real tuple, and a stale TID is discarded either as a dead
//! tuple or as a live one whose value no longer matches — including when VACUUM
//! has freed the line pointer and a later insert has taken the slot back.
//!
//! Two independent decisions interact here: `recheck` exists because the key
//! is a **hash** and a posting list is a superset, and it happens to make
//! dangling TIDs harmless to correctness as well. The cost of skipping a key is
//! an index that grows without bound and heap fetches that are thrown away.
//!
//! **The consequence for testing is the sharp part.** No row count anywhere
//! can detect a skipped key — verified by sabotaging this function to remove
//! nothing, after which every count in `test/sql/iam.sql`, including a
//! heap-oracle comparison in both directions, was unchanged. What does detect it
//! is the Bitmap **Index** Scan's own row count under `EXPLAIN ( ANALYZE )`,
//! which is the index's output *before* recheck: 102 stale TIDs against 51 live
//! rows with the sabotage in place, 51 against 51 without it.
//!
//! # The shape of the work
//!
//! One set operation per key, not one probe per dead TID: build the dead TIDs as
//! an `OrdSet` once, then `store_set( k, load( k ).and_not( &dead ) )` for each
//! key. That is why this is affordable at all — a per-TID membership test
//! against every key would be `keys × dead_tids` probes.
//!
//! # What is not implemented, and why it is recorded rather than hidden
//!
//! This runs against a **Flight endpoint**, which exposes `do_put` for
//! inserts and removals but has no "replace this key's whole set" operation. So
//! the removal is expressed as a `do_put` of the dead ordinals in remove mode —
//! correct, and one round trip per key rather than one set operation. The
//! `and_not` formulation above is what a local transport would use.

use pgrx::prelude::*;

use super::{index_target, open_transport_for_index};
use crate::iam::tid::tid_to_ordinal;
use crate::transport::Transport;

/// Remove every dead TID from every key this index wrote.
///
/// # Safety
///
/// Called by VACUUM with valid pointers.
#[pg_guard]
pub unsafe extern "C-unwind" fn ambulkdelete(
    info: *mut pg_sys::IndexVacuumInfo,
    stats: *mut pg_sys::IndexBulkDeleteResult,
    callback: pg_sys::IndexBulkDeleteCallback,
    callback_state: *mut core::ffi::c_void,
) -> *mut pg_sys::IndexBulkDeleteResult {
    let stats = unsafe { ensure_stats(stats) };
    let Some(cb) = callback else {
        return stats;
    };
    let index = unsafe { (*info).index };
    let Some((_endpoint, _key_hint)) = (unsafe { index_target(index) }) else {
        return stats;
    };

    let mut transport = match unsafe { open_transport_for_index(index) } {
        Ok(t) => t,
        Err(e) => {
            // A warning, not an error. VACUUM failing outright would block
            // the whole table's maintenance — including the freezing that
            // prevents transaction-id wraparound — because one index could not
            // reach its server. But the index is now stale, and saying so is
            // the least this can do.
            pgrx::warning!(
                "yesno_iam: cannot reach the yesno server to vacuum this index ({e}); \
                 it now holds TIDs for deleted tuples and needs REINDEX"
            );
            return stats;
        }
    };

    let keys = match transport.keys() {
        Ok(k) => k,
        Err(e) => {
            pgrx::warning!(
                "yesno_iam: cannot enumerate keys to vacuum this index ({e}); \
                 it now holds TIDs for deleted tuples and needs REINDEX"
            );
            return stats;
        }
    };

    let mut removed = 0u64;
    let mut remaining = 0u64;
    for key in keys {
        let cmd = crate::transport::flight::FlightTransport::key_cmd(key);
        if transport.open_scan(&cmd).is_err() {
            continue;
        }
        let mut dead: Vec<u64> = Vec::new();
        let mut live = 0u64;
        loop {
            match transport.next_batch() {
                Err(_) => break,
                Ok(None) => break,
                Ok(Some(b)) => {
                    for v in b {
                        let o = crate::ordinal::i64_to_ordinal(v);
                        if !super::tid::is_tid_ordinal(o) {
                            // Not this index's posting; leave it alone.
                            live += 1;
                            continue;
                        }
                        let (block, offset) = super::tid::ordinal_to_tid(o);
                        let mut tid = super::tid::make_tid(block, offset);
                        if unsafe { cb(&mut tid, callback_state) } {
                            dead.push(o);
                        } else {
                            live += 1;
                        }
                    }
                }
            }
        }
        transport.close_scan();
        if !dead.is_empty() {
            if let Err(e) = transport.put(key, &dead, true) {
                pgrx::warning!("yesno_iam: removing dead TIDs from key {key} failed: {e}");
                continue;
            }
            removed += dead.len() as u64;
        }
        remaining += live;
    }

    unsafe {
        (*stats).tuples_removed += removed as f64;
        (*stats).num_index_tuples = remaining as f64;
    }
    stats
}

/// # Safety
///
/// Called by VACUUM with valid pointers.
#[pg_guard]
pub unsafe extern "C-unwind" fn amvacuumcleanup(
    _info: *mut pg_sys::IndexVacuumInfo,
    stats: *mut pg_sys::IndexBulkDeleteResult,
) -> *mut pg_sys::IndexBulkDeleteResult {
    // Nothing to do. `ambulkdelete` already reported the surviving count, and
    // there are no index pages to reclaim — the storage is yesno's. Returning
    // `stats` unchanged is what tells VACUUM the index needs no second pass.
    stats
}

/// VACUUM may pass a null `stats` on the first call.
///
/// # Safety
///
/// Called in a VACUUM memory context.
unsafe fn ensure_stats(
    stats: *mut pg_sys::IndexBulkDeleteResult,
) -> *mut pg_sys::IndexBulkDeleteResult {
    if !stats.is_null() {
        return stats;
    }
    unsafe {
        pg_sys::palloc0(core::mem::size_of::<pg_sys::IndexBulkDeleteResult>())
            .cast::<pg_sys::IndexBulkDeleteResult>()
    }
}

/// Unused, but kept so the TID helper is exercised from this module too.
#[allow(dead_code)]
fn _tid_helper_is_shared(block: u32, offset: u16) -> u64 {
    tid_to_ordinal(block, offset)
}
