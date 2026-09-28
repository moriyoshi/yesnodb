//! Load the C fixture plugin and drive the whole handshake through it.
//!
//! This is the only test in the tree that exercises the ABI the way a real plugin
//! will: `dlopen`, `yesno_plugin_init`, version and size negotiation, then the
//! plugin calling *back* into the host table to read data. The host-table tests
//! beside it call the pointers directly, which cannot catch a mistake in the
//! handshake or in the header a C compiler has to accept.

use std::ffi::{c_char, CStr, OsStr};
use std::sync::{Arc, RwLock};

use yesno_core::{Db, DbOptions};
use yesno_plugin::abi::{ChunkKind, Role, Status};
use yesno_plugin::loader::LoadedPlugin;
use yesno_plugin::Host;

/// Where the build script put the compiled plugin, or why it did not.
fn plugin_path() -> std::path::PathBuf {
    match option_env!("YESNO_TEST_PLUGIN") {
        Some(p) => std::path::PathBuf::from(p),
        None => panic!(
            "the C fixture plugin was not built: {}",
            option_env!("YESNO_TEST_PLUGIN_ERROR").unwrap_or("no reason was recorded")
        ),
    }
}

/// Serializes every case that loads the fixture.
///
/// **Not hygiene, and the gate caught its absence.** `dlopen` returns one image
/// per process, so `yesno_plugin_init` overwrites the fixture's `g_host` and
/// `g_db` globals on every load -- and cargo runs these as threads in one process.
/// Concurrently, one case's scan then runs against another case's database and
/// counts its lease against another case's host, which is exactly what failed:
/// `Ok` where `Unavailable` was expected, and a lease count of 1 where 0 was.
///
/// The sibling suite in `yesno-server/tests/plugin_facility.rs` carries the same
/// guard. This one was missed after the class had already been diagnosed there,
/// which is the more useful half of the lesson: fixing one instance of a shared-
/// state hazard does not fix the class, and nothing but a run finds the rest.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

struct Clean(std::path::PathBuf);
impl Drop for Clean {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn fixture(tag: &str) -> (Clean, Arc<RwLock<Option<Arc<Db>>>>) {
    let mut dir = std::env::temp_dir();
    dir.push(format!("yesno-loadc-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
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
    // key 10: chunks 0, 1, 5 as arrays starting at 7. key 20: chunk 1 as a
    // bitmap ( scattered, past RUN_MAX_INTERVALS ). key 30: chunk 3 as one run,
    // deliberately **not** starting at the chunk origin: stored it is
    // ( 1000, 5000 ), so a consumer handed the stored pair reads an end of 5000
    // and silently loses four thousand ordinals.
    for c in [0u64, 1, 5] {
        for i in 0..3u64 {
            b.insert(10, c * 65536 + 7 + i);
        }
    }
    for i in 0..5000u64 {
        b.insert(20, 65536 + i * 3);
    }
    b.insert_range(30, 3 * 65536 + 1000, 3 * 65536 + 6000);
    b.commit().unwrap();
    db.checkpoint().unwrap();
    (clean, Arc::new(RwLock::new(Some(Arc::new(db)))))
}

/// Symbols the fixture exports so the host can read back what it observed.
struct Probe<'a> {
    lib: &'a libloading::Library,
}

impl Probe<'_> {
    fn call0<T>(&self, name: &[u8]) -> T {
        // SAFETY: Every symbol below is declared in tests/plugin.c with the
        // signature used here, and the library outlives this borrow.
        unsafe {
            let f: libloading::Symbol<unsafe extern "C" fn() -> T> = self.lib.get(name).unwrap();
            f()
        }
    }
    fn call1<T>(&self, name: &[u8], i: usize) -> T {
        // SAFETY: As above.
        unsafe {
            let f: libloading::Symbol<unsafe extern "C" fn(usize) -> T> =
                self.lib.get(name).unwrap();
            f(i)
        }
    }
    fn scan(&self, name: &[u8], keys: &[u64]) -> Status {
        // SAFETY: As above; `keys` outlives the call.
        unsafe {
            let f: libloading::Symbol<unsafe extern "C" fn(*const u64, usize) -> Status> =
                self.lib.get(name).unwrap();
            f(keys.as_ptr(), keys.len())
        }
    }
}

/// Open the library a second time to reach the probe symbols.
///
/// `dlopen` on the same path returns the same handle with a bumped refcount, so
/// this is the *same* loaded image the plugin is running in -- its statics are the
/// ones the host just drove. A second `Library` value is only a second reference.
fn probe_lib(path: &std::path::Path) -> libloading::Library {
    // SAFETY: The same trusted path the test just loaded.
    unsafe { libloading::Library::new(path) }.unwrap()
}

/// The handshake, a full scan driven from C, and the lifecycle callbacks.
#[test]
fn the_c_plugin_completes_the_handshake_and_reads_through_the_host_table() {
    let _serial = serial();
    let path = plugin_path();
    let (_c, slot) = fixture("handshake");
    let host = Host::new(slot.clone(), 3, Role::Leader);

    // SAFETY: the fixture plugin is built from this crate's own sources.
    let plugin = unsafe { LoadedPlugin::load(OsStr::new(&path), host.clone()) }
        .expect("the fixture plugin must load and negotiate");
    let header = plugin.header();
    assert_eq!(header.version, 1, "the fixture speaks v1");
    assert!(header.size >= 8, "and declares a table size");

    let lib = probe_lib(&path);
    let probe = Probe { lib: &lib };

    // The plugin scans three keys through the host table.
    let keys = [10u64, 20, 30];
    assert_eq!(probe.scan(b"yesno_test_scan\0", &keys), Status::Ok);

    let rows: usize = probe.call0(b"yesno_test_rows\0");
    assert_eq!(rows, 12, "four blocks times three lanes, absences included");

    let mut seen = Vec::new();
    for i in 0..rows {
        seen.push((
            probe.call1::<u64>(b"yesno_test_prefix\0", i),
            probe.call1::<u32>(b"yesno_test_kind\0", i),
            probe.call1::<u32>(b"yesno_test_count\0", i),
        ));
    }
    let absent = ChunkKind::Absent as u32;
    assert_eq!(
        seen,
        vec![
            (0, ChunkKind::Array as u32, 3),
            (0, absent, 0),
            (0, absent, 0),
            (1, ChunkKind::Array as u32, 3),
            (1, ChunkKind::Bitmap as u32, 1024),
            (1, absent, 0),
            (3, absent, 0),
            (3, absent, 0),
            (3, ChunkKind::Run as u32, 1),
            (5, ChunkKind::Array as u32, 3),
            (5, absent, 0),
            (5, absent, 0),
        ],
        "the C side must see every block, every lane, and absence in place"
    );

    // And the payload was readable through the borrowed pointer.
    assert_eq!(
        probe.call0::<i32>(b"yesno_test_first_array_value\0"),
        7,
        "the first array lane's first value, read from C through chunk.data"
    );

    // The run reached C as `[ start, end ]`, across a real dlopen boundary and
    // through the NULL-payload retry. Reading the stored pair instead would give
    // ( 1000, 5000 ): ascending, so nothing would fault, and wrong.
    assert_eq!(
        (
            probe.call0::<i32>(b"yesno_test_first_run_start\0"),
            probe.call0::<i32>(b"yesno_test_first_run_end\0"),
        ),
        (1000, 6000),
        "a run converts to an inclusive end, not to its stored length"
    );

    // Every lease the plugin took is returned; it holds nothing between calls.
    assert_eq!(host.leases(), 0, "the fixture drains within each call");

    // Lifecycle callbacks reach it.
    assert_eq!(
        plugin.on_role_change(Role::Leader, Role::Follower),
        Status::Ok
    );
    assert_eq!(probe.call0::<i32>(b"yesno_test_role_changes\0"), 1);

    assert_eq!(plugin.serve_start("127.0.0.1:0").unwrap(), Status::Ok);
    assert_eq!(probe.call0::<i32>(b"yesno_test_serving\0"), 1);
    // SAFETY: the symbol returns a pointer to the plugin's own static buffer.
    let addr = unsafe {
        let f: libloading::Symbol<unsafe extern "C" fn() -> *const c_char> =
            lib.get(b"yesno_test_addr\0").unwrap();
        CStr::from_ptr(f()).to_string_lossy().into_owned()
    };
    assert_eq!(addr, "127.0.0.1:0", "the host's address reached the plugin");
    assert_eq!(plugin.serve_stop(), Status::Ok);
    assert_eq!(probe.call0::<i32>(b"yesno_test_serving\0"), 0);

    // The rebootstrap sequence: drain, replace, announce.
    assert_eq!(plugin.on_unavailable(), Status::Ok);
    assert_eq!(probe.call0::<i32>(b"yesno_test_unavailable_calls\0"), 1);
    assert_eq!(
        host.leases(),
        0,
        "the drain must leave nothing outstanding, which is what lets the host reopen"
    );
    let gen = host.bump_generation();
    assert_eq!(gen, 4);
    assert_eq!(plugin.on_available(gen), Status::Ok);
    assert_eq!(probe.call0::<u64>(b"yesno_test_generation\0"), 4);
}

/// The block protocol is enforced against a real C caller, not only a Rust one.
#[test]
fn the_host_refuses_a_second_advance_from_c() {
    let _serial = serial();
    let path = plugin_path();
    let (_c, slot) = fixture("double");
    let host = Host::new(slot, 1, Role::Leader);
    // SAFETY: as above.
    let _plugin = unsafe { LoadedPlugin::load(OsStr::new(&path), host.clone()) }.unwrap();
    let lib = probe_lib(&path);
    let probe = Probe { lib: &lib };

    let keys = [10u64];
    assert_eq!(
        probe.scan(b"yesno_test_double_advance\0", &keys),
        Status::BlockState,
        "advancing with a block open must be refused across the C boundary too"
    );
    assert_eq!(host.leases(), 0, "and the refused path still releases");
}

/// An unavailable database reaches the C side as UNAVAILABLE, not as a crash.
#[test]
fn a_rebootstrapping_database_reports_unavailable_to_c() {
    let _serial = serial();
    let path = plugin_path();
    let (_c, slot) = fixture("unavail");
    let host = Host::new(slot.clone(), 1, Role::Follower);
    // SAFETY: as above.
    let _plugin = unsafe { LoadedPlugin::load(OsStr::new(&path), host.clone()) }.unwrap();
    let lib = probe_lib(&path);
    let probe = Probe { lib: &lib };

    slot.write().unwrap().take();
    let keys = [10u64];
    assert_eq!(
        probe.scan(b"yesno_test_scan\0", &keys),
        Status::Unavailable,
        "the interval with no database is a code the plugin can retry on"
    );
    assert_eq!(host.leases(), 0);
}

/// Loading something that is not a plugin fails with a reason, not a panic.
#[test]
fn a_library_without_the_init_symbol_is_refused() {
    let _serial = serial();
    let (_c, slot) = fixture("nosym");
    let host = Host::new(slot, 1, Role::Leader);
    // libc has no yesno_plugin_init. Loading it is harmless and already resident.
    let candidate = if cfg!(target_os = "macos") {
        "libSystem.dylib"
    } else {
        "libc.so.6"
    };
    // SAFETY: a system library, loaded and immediately dropped.
    let err = unsafe { LoadedPlugin::load(OsStr::new(candidate), host) };
    match err {
        Err(yesno_plugin::loader::LoadError::MissingSymbol(_)) => {}
        Err(yesno_plugin::loader::LoadError::Open(_)) => {
            // Acceptable on a host where that soname is not present; the point is
            // that it is an error rather than a crash.
        }
        Err(other) => panic!("expected a refusal about the symbol, got {other:?}"),
        Ok(_) => panic!("a library with no yesno_plugin_init must not load"),
    }
}

/// A path that does not exist is an error, not a panic.
#[test]
fn a_missing_library_is_refused() {
    let (_c, slot) = fixture("missing");
    let host = Host::new(slot, 1, Role::Leader);
    // SAFETY: nothing is loaded; the call fails in dlopen.
    let err = unsafe { LoadedPlugin::load(OsStr::new("/nonexistent/libnope.so"), host) };
    assert!(matches!(err, Err(yesno_plugin::loader::LoadError::Open(_))));
}
