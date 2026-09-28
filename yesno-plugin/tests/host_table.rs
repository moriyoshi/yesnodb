//! Drive the host table through the same function pointers a plugin would.
//!
//! Deliberately not through the Rust structs. The point of these tests is the C
//! surface -- null handling, the block protocol, the lease count, the absent
//! lane -- and calling the Rust methods directly would test a different thing and
//! pass while the table was mis-wired.

use std::ffi::c_void;

use yesno_core::{Db, DbOptions};
use yesno_plugin::abi::{Chunk, ChunkKind, Role, Status};
use yesno_plugin::table::{host_api, HostDb, LanesOpaque, SnapshotOpaque};
use yesno_plugin::Host;

struct Clean(std::path::PathBuf);
impl Drop for Clean {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tmpdir(tag: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("yesno-plug-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    p
}

/// A database with ragged lanes, and the host wrapper over it.
fn fixture(
    tag: &str,
) -> (
    Clean,
    std::sync::Arc<std::sync::RwLock<Option<std::sync::Arc<Db>>>>,
    Host,
) {
    let dir = tmpdir(tag);
    let clean = Clean(dir.clone());
    let db = Db::open_with(
        &dir,
        DbOptions {
            shards: 1,
            ..Default::default()
        },
    )
    .unwrap();
    let mut b = db.batch();
    // key 10: chunks 0, 1, 5 as small arrays. key 20: chunk 1 dense enough to be
    // a bitmap. key 30: chunk 3 as a contiguous run.
    for c in [0u64, 1, 5] {
        for i in 0..3u64 {
            b.insert(10, c * 65536 + i);
        }
    }
    // Scattered, not contiguous: 5000 values above ARRAY_MAX would still be a
    // *Run* if they were consecutive ( one interval ), which is what the first
    // version of this fixture produced. Stride 3 makes 5000 intervals, past
    // RUN_MAX_INTERVALS = 2032, so the chunk is a bitmap.
    for i in 0..5000u64 {
        b.insert(20, 65536 + i * 3);
    }
    b.insert_range(30, 3 * 65536, 3 * 65536 + 4096);
    b.commit().unwrap();
    db.checkpoint().unwrap();

    let slot = std::sync::Arc::new(std::sync::RwLock::new(Some(std::sync::Arc::new(db))));
    let host = Host::new(slot.clone(), 7, Role::Leader);
    (clean, slot, host)
}

/// Every kind reaches the caller with the count its own representation implies,
/// and a run reports **intervals** rather than the flat u16 length.
#[test]
fn each_container_kind_is_described_with_the_right_count() {
    let (_c, _slot, host) = fixture("kinds");
    let api = host_api();
    let mut h = host.clone();
    let db_ptr = &mut h as *mut Host as *mut HostDb;

    let mut snap: *mut SnapshotOpaque = std::ptr::null_mut();
    assert_eq!(
        unsafe { (api.snapshot_open)(db_ptr, &mut snap) },
        Status::Ok
    );

    let keys = [10u64, 20, 30];
    let mut lanes: *mut LanesOpaque = std::ptr::null_mut();
    assert_eq!(
        unsafe { (api.lanes_acquire)(snap, keys.as_ptr(), keys.len(), &mut lanes) },
        Status::Ok
    );
    assert_eq!(unsafe { (api.lanes_count)(lanes) }, 3);

    let mut seen: Vec<(u64, Vec<(u32, u32)>)> = Vec::new();
    loop {
        let mut prefix = 0u64;
        let mut done = 0u8;
        assert_eq!(
            unsafe { (api.block_advance)(lanes, &mut prefix, &mut done) },
            Status::Ok
        );
        if done == 1 {
            break;
        }
        let mut row = Vec::new();
        for lane in 0..3usize {
            let mut chunk = Chunk {
                prefix: 0,
                kind: 99,
                count: 0,
                data: std::ptr::null(),
            };
            assert_eq!(
                unsafe { (api.block_lane)(lanes, lane, &mut chunk) },
                Status::Ok
            );
            assert_eq!(chunk.prefix, prefix, "every lane echoes the block prefix");
            row.push((chunk.kind, chunk.count));
        }
        seen.push((prefix, row));
        unsafe { (api.block_release)(lanes) };
    }

    let absent = (ChunkKind::Absent as u32, 0u32);
    assert_eq!(
        seen,
        vec![
            (0, vec![(ChunkKind::Array as u32, 3), absent, absent]),
            // key 20's 5000 values exceed ARRAY_MAX, so that lane is a bitmap of
            // exactly 1024 words while key 10's lane stays a 3-value array.
            (
                1,
                vec![
                    (ChunkKind::Array as u32, 3),
                    (ChunkKind::Bitmap as u32, 1024),
                    absent
                ]
            ),
            // One contiguous range is one interval, not 2 u16s.
            (3, vec![absent, absent, (ChunkKind::Run as u32, 1)]),
            (5, vec![(ChunkKind::Array as u32, 3), absent, absent]),
        ]
    );

    unsafe { (api.lanes_release)(lanes) };
    unsafe { (api.snapshot_close)(snap) };
}

/// The borrowed array payload is the real data, read through the C pointer.
#[test]
fn a_borrowed_array_payload_holds_the_actual_values() {
    let (_c, _slot, host) = fixture("borrow");
    let api = host_api();
    let mut h = host.clone();
    let db_ptr = &mut h as *mut Host as *mut HostDb;

    let mut snap: *mut SnapshotOpaque = std::ptr::null_mut();
    unsafe { (api.snapshot_open)(db_ptr, &mut snap) };
    let keys = [10u64];
    let mut lanes: *mut LanesOpaque = std::ptr::null_mut();
    unsafe { (api.lanes_acquire)(snap, keys.as_ptr(), 1, &mut lanes) };

    let mut prefix = 0u64;
    let mut done = 0u8;
    unsafe { (api.block_advance)(lanes, &mut prefix, &mut done) };
    let mut chunk = Chunk {
        prefix: 0,
        kind: 99,
        count: 0,
        data: std::ptr::null(),
    };
    unsafe { (api.block_lane)(lanes, 0, &mut chunk) };
    assert_eq!(chunk.kind, ChunkKind::Array as u32);
    assert!(!chunk.data.is_null());
    // SAFETY: the table promises `count` u16 values at `data`, valid until
    // block_release, which has not been called.
    let vals =
        unsafe { std::slice::from_raw_parts(chunk.data as *const u16, chunk.count as usize) };
    assert_eq!(vals, &[0, 1, 2], "the low 16 bits of the three ordinals");

    unsafe { (api.block_release)(lanes) };
    unsafe { (api.lanes_release)(lanes) };
    unsafe { (api.snapshot_close)(snap) };
}

/// Advancing with a block still open is refused, and released is required.
#[test]
fn the_block_protocol_is_enforced_in_both_directions() {
    let (_c, _slot, host) = fixture("proto");
    let api = host_api();
    let mut h = host.clone();
    let db_ptr = &mut h as *mut Host as *mut HostDb;

    let mut snap: *mut SnapshotOpaque = std::ptr::null_mut();
    unsafe { (api.snapshot_open)(db_ptr, &mut snap) };
    let keys = [10u64];
    let mut lanes: *mut LanesOpaque = std::ptr::null_mut();
    unsafe { (api.lanes_acquire)(snap, keys.as_ptr(), 1, &mut lanes) };

    let mut chunk = Chunk {
        prefix: 0,
        kind: 99,
        count: 0,
        data: std::ptr::null(),
    };
    // Before any advance there is no block.
    assert_eq!(
        unsafe { (api.block_lane)(lanes, 0, &mut chunk) },
        Status::BlockState,
        "reading a lane with no block open is a protocol error, not empty data"
    );

    let mut prefix = 0u64;
    let mut done = 0u8;
    assert_eq!(
        unsafe { (api.block_advance)(lanes, &mut prefix, &mut done) },
        Status::Ok
    );
    assert_eq!(
        unsafe { (api.block_advance)(lanes, &mut prefix, &mut done) },
        Status::BlockState,
        "advancing while pointers are outstanding must be refused, not silently \
         invalidate them"
    );
    unsafe { (api.block_release)(lanes) };
    assert_eq!(
        unsafe { (api.block_advance)(lanes, &mut prefix, &mut done) },
        Status::Ok,
        "and released, the next advance proceeds"
    );

    unsafe { (api.lanes_release)(lanes) };
    unsafe { (api.snapshot_close)(snap) };
}

/// Lease counting is what the drain contract runs on, so it is asserted directly.
///
/// Note the shape being pinned: `live_readers()` would say **one** for all of
/// these together, because a Snapshot clone refcounts one registry slot. The
/// facility's count is the number of handles, which is the number the host has to
/// wait for.
#[test]
fn every_handle_is_one_lease_and_release_returns_it() {
    let (_c, slot, host) = fixture("lease");
    let api = host_api();
    let mut h = host.clone();
    let db_ptr = &mut h as *mut Host as *mut HostDb;
    assert_eq!(host.leases(), 0);

    let mut snap: *mut SnapshotOpaque = std::ptr::null_mut();
    unsafe { (api.snapshot_open)(db_ptr, &mut snap) };
    assert_eq!(host.leases(), 1, "a snapshot pins, so it is a lease");

    let keys = [10u64, 20];
    let mut a: *mut LanesOpaque = std::ptr::null_mut();
    let mut b: *mut LanesOpaque = std::ptr::null_mut();
    unsafe { (api.lanes_acquire)(snap, keys.as_ptr(), 2, &mut a) };
    unsafe { (api.lanes_acquire)(snap, keys.as_ptr(), 2, &mut b) };
    assert_eq!(host.leases(), 3, "two handles are two leases, not one slot");

    // And a zero-lane handle counts too, which is the case a slot-based count
    // could never see.
    let mut empty: *mut LanesOpaque = std::ptr::null_mut();
    unsafe { (api.lanes_acquire)(snap, std::ptr::null(), 0, &mut empty) };
    assert_eq!(host.leases(), 4);

    let db = slot.read().unwrap().clone().unwrap();
    assert_eq!(
        db.live_readers(),
        1,
        "four leases, one registry slot: this is why the facility counts its own"
    );

    unsafe { (api.snapshot_close)(snap) };
    assert_eq!(host.leases(), 3);
    unsafe { (api.lanes_release)(a) };
    unsafe { (api.lanes_release)(b) };
    unsafe { (api.lanes_release)(empty) };
    assert_eq!(host.leases(), 0, "a drained facility holds nothing");
    assert_eq!(db.live_readers(), 0);
}

/// With no database in the slot, a plugin is told `UNAVAILABLE` rather than being
/// handed a handle or a crash.
#[test]
fn an_empty_slot_reports_unavailable() {
    let (_c, slot, host) = fixture("gone");
    let api = host_api();
    let mut h = host.clone();
    let db_ptr = &mut h as *mut Host as *mut HostDb;

    slot.write().unwrap().take();

    let mut snap: *mut SnapshotOpaque = std::ptr::null_mut();
    assert_eq!(
        unsafe { (api.snapshot_open)(db_ptr, &mut snap) },
        Status::Unavailable
    );
    assert!(snap.is_null(), "no handle is produced on failure");

    let mut writes = 9u8;
    assert_eq!(
        unsafe { (api.db_accepts_writes)(db_ptr, &mut writes) },
        Status::Ok
    );
    assert_eq!(writes, 0, "an absent database accepts no writes");
}

/// Role and generation are readable, and the generation moves when told.
#[test]
fn role_and_generation_are_visible_to_the_plugin() {
    let (_c, _slot, host) = fixture("gen");
    let api = host_api();
    let mut h = host.clone();
    let db_ptr = &mut h as *mut Host as *mut HostDb;

    let mut gen = 0u64;
    assert_eq!(unsafe { (api.db_generation)(db_ptr, &mut gen) }, Status::Ok);
    assert_eq!(gen, 7, "the generation the host was constructed with");

    let mut role = Role::Follower;
    assert_eq!(unsafe { (api.db_role)(db_ptr, &mut role) }, Status::Ok);
    assert_eq!(role, Role::Leader);

    assert_eq!(host.bump_generation(), 8);
    unsafe { (api.db_generation)(db_ptr, &mut gen) };
    assert_eq!(
        gen, 8,
        "a replacement is visible without reopening anything"
    );

    host.set_role(Role::Follower);
    unsafe { (api.db_role)(db_ptr, &mut role) };
    assert_eq!(role, Role::Follower);
    let mut writes = 9u8;
    unsafe { (api.db_accepts_writes)(db_ptr, &mut writes) };
    assert_eq!(writes, 0, "a follower refuses writes");
}

/// Null anything is a code, never a crash.
#[test]
fn null_arguments_are_refused_rather_than_dereferenced() {
    let api = host_api();
    let mut out = 0u64;
    assert_eq!(
        unsafe { (api.db_generation)(std::ptr::null(), &mut out) },
        Status::InvalidArgument
    );
    let mut h = Host::new(
        std::sync::Arc::new(std::sync::RwLock::new(None)),
        0,
        Role::Leader,
    );
    let db_ptr = &mut h as *mut Host as *mut HostDb;
    assert_eq!(
        unsafe { (api.db_generation)(db_ptr, std::ptr::null_mut()) },
        Status::InvalidArgument
    );
    // Releasing null is a no-op, not a double free.
    unsafe { (api.snapshot_close)(std::ptr::null_mut()) };
    unsafe { (api.lanes_release)(std::ptr::null_mut()) };
    unsafe { (api.block_release)(std::ptr::null_mut()) };
    assert_eq!(unsafe { (api.lanes_count)(std::ptr::null()) }, 0);
}

/// `block_lane_into` borrows when it can, and refuses undersized or misaligned
/// scratch with the size a caller needs.
#[test]
fn scratch_is_only_used_when_a_borrow_is_impossible() {
    let (_c, _slot, host) = fixture("scratch");
    let api = host_api();
    let mut h = host.clone();
    let db_ptr = &mut h as *mut Host as *mut HostDb;

    let mut snap: *mut SnapshotOpaque = std::ptr::null_mut();
    unsafe { (api.snapshot_open)(db_ptr, &mut snap) };
    let keys = [20u64];
    let mut lanes: *mut LanesOpaque = std::ptr::null_mut();
    unsafe { (api.lanes_acquire)(snap, keys.as_ptr(), 1, &mut lanes) };
    let mut prefix = 0u64;
    let mut done = 0u8;
    unsafe { (api.block_advance)(lanes, &mut prefix, &mut done) };

    let mut scratch = vec![0u64; 1024];
    let mut chunk = Chunk {
        prefix: 0,
        kind: 99,
        count: 0,
        data: std::ptr::null(),
    };
    assert_eq!(
        unsafe {
            (api.block_lane_into)(
                lanes,
                0,
                scratch.as_mut_ptr() as *mut c_void,
                1024 * 8,
                &mut chunk,
            )
        },
        Status::Ok
    );
    assert_eq!(chunk.kind, ChunkKind::Bitmap as u32);
    assert_eq!(chunk.count, 1024);
    // Through the page store every slot is 64-byte aligned, so this bitmap is
    // borrowable and the scratch must be untouched.
    assert_ne!(
        chunk.data as *const u64,
        scratch.as_ptr(),
        "a borrowable payload must be lent, not copied into scratch"
    );
    assert!(scratch.iter().all(|&w| w == 0), "scratch was not written");

    unsafe { (api.block_release)(lanes) };
    unsafe { (api.lanes_release)(lanes) };
    unsafe { (api.snapshot_close)(snap) };
}

/// A call that fails before producing a handle must not leave a lease counted.
///
/// This is what the RAII guard buys. The obvious spelling -- increment, build,
/// hand out -- leaves the count permanently high if anything between the
/// increment and the return fails, and the drain that count feeds is the one
/// contract with no host-side backstop, so a stuck count is a server that can
/// never reopen.
#[test]
fn a_failed_acquire_counts_no_lease() {
    let (_c, slot, host) = fixture("failpath");
    let api = host_api();
    let mut h = host.clone();
    let db_ptr = &mut h as *mut Host as *mut HostDb;
    assert_eq!(host.leases(), 0);

    // Bad arguments, refused before any handle exists.
    let mut snap: *mut SnapshotOpaque = std::ptr::null_mut();
    assert_eq!(
        unsafe { (api.snapshot_open)(std::ptr::null_mut(), &mut snap) },
        Status::InvalidArgument
    );
    assert_eq!(host.leases(), 0, "a refused call counts nothing");

    // A real snapshot, then a lanes_acquire refused for a null key array.
    assert_eq!(
        unsafe { (api.snapshot_open)(db_ptr, &mut snap) },
        Status::Ok
    );
    assert_eq!(host.leases(), 1);
    let mut lanes: *mut LanesOpaque = std::ptr::null_mut();
    assert_eq!(
        unsafe { (api.lanes_acquire)(snap, std::ptr::null(), 3, &mut lanes) },
        Status::InvalidArgument
    );
    assert!(lanes.is_null());
    assert_eq!(host.leases(), 1, "still just the snapshot");

    // And an unavailable database, which fails after the argument checks.
    slot.write().unwrap().take();
    let mut snap2: *mut SnapshotOpaque = std::ptr::null_mut();
    assert_eq!(
        unsafe { (api.snapshot_open)(db_ptr, &mut snap2) },
        Status::Unavailable
    );
    assert_eq!(
        host.leases(),
        1,
        "a failure after the checks counts nothing either"
    );

    unsafe { (api.snapshot_close)(snap) };
    assert_eq!(host.leases(), 0);
}
