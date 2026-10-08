//! The exported C functions, driven against a real channel server.
//!
//! `tests/smoke.c` proves the header parses and the symbols link; it cannot
//! prove behaviour because it has no server. This does the opposite: it calls
//! the same `extern "C"` entry points from Rust against a live
//! `serve_blocking`, so the ABI's translation layer -- out-parameters, status
//! classification, the borrowed payload pointer -- is exercised rather than
//! only the client underneath it.
use std::os::unix::net::UnixListener;
use std::sync::{Arc, RwLock};

use yesno_core::{Db, DbOptions};
use yesno_plugin::abi::Role;
use yesno_plugin::channel::{serve_blocking, Limits, Session};
use yesno_plugin::Host;

use yesno_channel_c::*;

struct Clean(std::path::PathBuf);
impl Drop for Clean {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn limits() -> Limits {
    Limits {
        max_handles: 2,
        max_lanes: 4,
        max_blocks: 4,
        max_snapshots: 8,
        max_writes: yesno_plugin::ipc::MAX_WRITES,
    }
}

/// Key 1 holds 5, 6, 7 in chunk 0; key 3 spans chunks 0, 1 and 2.
fn serve(tag: &str) -> (Clean, std::path::PathBuf, std::thread::JoinHandle<()>) {
    let mut dir = std::env::temp_dir();
    dir.push(format!("yesno-chanc-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let clean = Clean(dir.clone());

    let db = Db::open_with(
        dir.join("db"),
        DbOptions {
            shards: 1,
            ..Default::default()
        },
    )
    .unwrap();
    let mut b = db.batch();
    for o in [5u64, 6, 7] {
        b.insert(1, o);
    }
    for o in [1u64, 2, 65546, 65547, 131092] {
        b.insert(3, o);
    }
    b.commit().unwrap();
    db.checkpoint().unwrap();

    let slot = Arc::new(RwLock::new(Some(Arc::new(db))));
    let host = Host::new(slot, 1, Role::Leader);
    let sock = dir.join("sock");
    let listener = UnixListener::bind(&sock).unwrap();
    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut session = Session::new_inline(host, limits());
        let _ = serve_blocking(&mut session, stream);
    });
    (clean, sock, handle)
}

fn cstr(p: &std::path::Path) -> std::ffi::CString {
    std::ffi::CString::new(p.to_string_lossy().as_bytes()).unwrap()
}

/// Every read path through the C surface, plus the borrowed payload pointer.
#[test]
fn the_c_surface_reads_through_a_live_channel() {
    let (_clean, sock, server) = serve("read");
    let path = cstr(&sock);
    let name = std::ffi::CString::new("abi-test").unwrap();
    let mut err = [0 as std::ffi::c_char; 256];

    unsafe {
        let mut channel: *mut yesno_channel = std::ptr::null_mut();
        assert_eq!(
            yesno_channel_open(
                path.as_ptr(),
                name.as_ptr(),
                &mut channel,
                err.as_mut_ptr(),
                err.len()
            ),
            0
        );
        assert!(!channel.is_null());

        // The greeting reaches C as a struct rather than being rediscovered.
        let mut lim = std::mem::zeroed::<yesno_channel_limits>();
        assert_eq!(
            yesno_channel_get_limits(channel, &mut lim, err.as_mut_ptr(), err.len()),
            0
        );
        assert_eq!(lim.max_blocks, 4);
        assert_eq!(lim.max_lanes, 4);
        assert_eq!(lim.role, 0, "leader");
        assert_eq!(lim.arena_bytes, 0, "serve_blocking offers no arena");

        let mut arena = 9u8;
        assert_eq!(
            yesno_channel_is_arena(channel, &mut arena, err.as_mut_ptr(), err.len()),
            0
        );
        assert_eq!(arena, 0);

        let mut snap: *mut yesno_channel_snapshot = std::ptr::null_mut();
        assert_eq!(
            yesno_channel_snapshot_open(channel, &mut snap, err.as_mut_ptr(), err.len()),
            0
        );

        let mut n = 0u64;
        assert_eq!(
            yesno_channel_cardinality(snap, 1, &mut n, err.as_mut_ptr(), err.len()),
            0
        );
        assert_eq!(n, 3);

        let mut present = 9u8;
        assert_eq!(
            yesno_channel_contains(snap, 1, 6, &mut present, err.as_mut_ptr(), err.len()),
            0
        );
        assert_eq!(present, 1);
        assert_eq!(
            yesno_channel_contains(snap, 1, 8, &mut present, err.as_mut_ptr(), err.len()),
            0
        );
        assert_eq!(present, 0);

        // An empty key leaves `value` untouched, which is what the header says.
        let mut value = 0xDEADu64;
        assert_eq!(
            yesno_channel_max(
                snap,
                999,
                &mut present,
                &mut value,
                err.as_mut_ptr(),
                err.len()
            ),
            0
        );
        assert_eq!(present, 0);
        assert_eq!(value, 0xDEAD, "value must not be written for an empty key");
        assert_eq!(
            yesno_channel_max(
                snap,
                1,
                &mut present,
                &mut value,
                err.as_mut_ptr(),
                err.len()
            ),
            0
        );
        assert_eq!((present, value), (1, 7));

        // Paged load, resuming strictly above a value the caller already holds.
        let mut page = [0u64; 8];
        let mut written = 0usize;
        let mut more = 9u8;
        assert_eq!(
            yesno_channel_load(
                snap,
                3,
                0,
                0,
                2,
                page.as_mut_ptr(),
                page.len(),
                &mut written,
                &mut more,
                err.as_mut_ptr(),
                err.len()
            ),
            0
        );
        assert_eq!(&page[..written], &[1, 2]);
        assert_eq!(more, 1);
        assert_eq!(
            yesno_channel_load(
                snap,
                3,
                1,
                2,
                8,
                page.as_mut_ptr(),
                page.len(),
                &mut written,
                &mut more,
                err.as_mut_ptr(),
                err.len()
            ),
            0
        );
        assert_eq!(&page[..written], &[65546, 65547, 131092]);
        assert_eq!(more, 0);

        // A page larger than the buffer is refused, not truncated: a short
        // write is indistinguishable from a short final page, so truncating
        // would make the caller stop early with a wrong answer.
        let mut one = [0u64; 1];
        let status = yesno_channel_load(
            snap,
            3,
            0,
            0,
            8,
            one.as_mut_ptr(),
            one.len(),
            &mut written,
            &mut more,
            err.as_mut_ptr(),
            err.len(),
        );
        assert_eq!(status, 1, "overflowing the buffer is an error");

        let mut keys = [0u64; 8];
        assert_eq!(
            yesno_channel_key_range(
                snap,
                0,
                10,
                8,
                keys.as_mut_ptr(),
                keys.len(),
                &mut written,
                &mut more,
                err.as_mut_ptr(),
                err.len()
            ),
            0
        );
        assert_eq!(&keys[..written], &[1, 3]);

        // The lane walk, and the borrowed payload pointer.
        let key_list = [3u64];
        let mut lanes: *mut yesno_channel_lanes = std::ptr::null_mut();
        assert_eq!(
            yesno_channel_lanes_open(
                snap,
                key_list.as_ptr(),
                1,
                &mut lanes,
                err.as_mut_ptr(),
                err.len()
            ),
            0
        );
        // Before the first advance there is no current block, so these report.
        let mut prefix = 0u64;
        assert_eq!(
            yesno_channel_lanes_prefix(lanes, &mut prefix, err.as_mut_ptr(), err.len()),
            1
        );

        let expected: [(u64, &[u8]); 3] = [(0, &[1, 0, 2, 0]), (1, &[10, 0, 11, 0]), (2, &[20, 0])];
        for (want_prefix, want_payload) in expected {
            let mut have = 9u8;
            assert_eq!(
                yesno_channel_lanes_advance(lanes, &mut have, err.as_mut_ptr(), err.len()),
                0
            );
            assert_eq!(have, 1);
            assert_eq!(
                yesno_channel_lanes_prefix(lanes, &mut prefix, err.as_mut_ptr(), err.len()),
                0
            );
            assert_eq!(prefix, want_prefix);

            let mut count = 0usize;
            assert_eq!(
                yesno_channel_lanes_count(lanes, &mut count, err.as_mut_ptr(), err.len()),
                0
            );
            assert_eq!(count, 1);

            let mut kind = 9u8;
            let mut lane_count = 0u32;
            let mut payload: *const u8 = std::ptr::null();
            let mut payload_len = 0usize;
            assert_eq!(
                yesno_channel_lane(
                    lanes,
                    0,
                    &mut kind,
                    &mut lane_count,
                    &mut payload,
                    &mut payload_len,
                    err.as_mut_ptr(),
                    err.len()
                ),
                0
            );
            assert_eq!(kind, 0, "array lane");
            assert!(!payload.is_null());
            let bytes = std::slice::from_raw_parts(payload, payload_len);
            assert_eq!(bytes, want_payload, "payload of block {want_prefix}");
        }

        let mut have = 9u8;
        assert_eq!(
            yesno_channel_lanes_advance(lanes, &mut have, err.as_mut_ptr(), err.len()),
            0
        );
        assert_eq!(have, 0, "a short batch ends the walk");

        yesno_channel_lanes_close(lanes);
        yesno_channel_snapshot_close(snap);
        yesno_channel_close(channel);
    }

    server.join().unwrap();
}

/// Closing the channel while a snapshot is open is the wrong order, and must
/// still not be undefined.
///
/// The header calls it wrong and the module documents why it is nonetheless
/// safe: a snapshot holds its own reference to the connection, so the handle
/// going away cannot dangle. That is a structural claim, so it gets a test --
/// an invariant asserted only in prose is an invariant nobody checks.
#[test]
fn a_snapshot_outlives_a_closed_channel_handle() {
    let (_clean, sock, server) = serve("order");
    let path = cstr(&sock);
    let name = std::ffi::CString::new("order-test").unwrap();
    let mut err = [0 as std::ffi::c_char; 256];

    unsafe {
        let mut channel: *mut yesno_channel = std::ptr::null_mut();
        assert_eq!(
            yesno_channel_open(
                path.as_ptr(),
                name.as_ptr(),
                &mut channel,
                err.as_mut_ptr(),
                err.len()
            ),
            0
        );
        let mut snap: *mut yesno_channel_snapshot = std::ptr::null_mut();
        assert_eq!(
            yesno_channel_snapshot_open(channel, &mut snap, err.as_mut_ptr(), err.len()),
            0
        );

        // Out of order on purpose.
        yesno_channel_close(channel);

        let mut n = 0u64;
        assert_eq!(
            yesno_channel_cardinality(snap, 1, &mut n, err.as_mut_ptr(), err.len()),
            0,
            "the snapshot still works: it holds the connection, not the handle"
        );
        assert_eq!(n, 3);

        yesno_channel_snapshot_close(snap);
    }

    server.join().unwrap();
}
