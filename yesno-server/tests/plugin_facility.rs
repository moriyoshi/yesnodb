//! The plugin facility's lifecycle, against the C fixture plugin.
//!
//! Loads the real library through `dlopen` rather than stubbing the table, because
//! the failures worth catching here -- a handshake that does not agree, a drain
//! that does not drain -- only exist across that boundary.

use std::sync::{Arc, RwLock};

use yesno_core::{Db, DbOptions};
use yesno_plugin::abi::{Role, Status};

use yesno_server::config::Config;
use yesno_server::plugin::{Drained, Facility};

fn fixture_path() -> String {
    match yesno_plugin::fixture_plugin_path() {
        Some(p) => p.to_string(),
        None => panic!(
            "the C fixture plugin was not built: {}",
            yesno_plugin::fixture_plugin_error()
        ),
    }
}

/// Serializes every case that touches the fixture.
///
/// **Not hygiene.** `dlopen` returns one image per process and the fixture's
/// observations live in its statics, while cargo runs these tests as threads in a
/// single process. Without this, `set_drain( false )` in one case reaches another's
/// drain, and one case's listener makes another's "must not be serving" assertion
/// fail -- each passing alone and failing in the suite, which is the shape a
/// mutating test seam took earlier in this tree's history.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    // A poisoned lock means another case panicked; its state is already reported
    // and blocking every later case behind that adds nothing.
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

struct Clean(std::path::PathBuf);
impl Drop for Clean {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A config naming the fixture, plus a live database in a slot.
fn setup(tag: &str, listen: &str) -> (Clean, Config, Arc<RwLock<Option<Arc<Db>>>>) {
    let mut dir = std::env::temp_dir();
    dir.push(format!("yesno-facility-{tag}-{}", std::process::id()));
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
    for i in 0..8u64 {
        b.insert(1, i);
    }
    b.commit().unwrap();
    db.checkpoint().unwrap();

    let mut cfg = Config::default();
    cfg.plugin.library = fixture_path();
    cfg.plugin.listen = listen.to_string();
    (clean, cfg, Arc::new(RwLock::new(Some(Arc::new(db)))))
}

/// A probe into the fixture's own statics, to read back what it observed.
struct Probe(libloading::Library);
impl Probe {
    fn open() -> Probe {
        // SAFETY: dlopen on the same path returns the same image with a bumped
        // refcount, so these are the statics the facility just drove.
        Probe(unsafe { libloading::Library::new(fixture_path()) }.unwrap())
    }
    fn get0<T>(&self, name: &[u8]) -> T {
        // SAFETY: each symbol is declared in tests/plugin.c with this signature.
        unsafe {
            let f: libloading::Symbol<unsafe extern "C" fn() -> T> = self.0.get(name).unwrap();
            f()
        }
    }
    fn set_drain(&self, honest: bool) {
        // SAFETY: as above.
        unsafe {
            let f: libloading::Symbol<unsafe extern "C" fn(i32)> =
                self.0.get(b"yesno_test_set_drain\0").unwrap();
            f(i32::from(honest));
        }
    }
    fn hold_lease(&self) -> Status {
        // SAFETY: as above.
        unsafe {
            let f: libloading::Symbol<unsafe extern "C" fn() -> Status> =
                self.0.get(b"yesno_test_hold_lease\0").unwrap();
            f()
        }
    }
    fn release_lease(&self) {
        // SAFETY: as above.
        unsafe {
            let f: libloading::Symbol<unsafe extern "C" fn()> =
                self.0.get(b"yesno_test_release_lease\0").unwrap();
            f();
        }
    }
}

/// Nothing configured means no facility, and no attempt to load anything.
#[test]
fn an_unconfigured_plugin_is_absent_rather_than_an_error() {
    let cfg = Config::default();
    let slot: Arc<RwLock<Option<Arc<Db>>>> = Arc::new(RwLock::new(None));
    // SAFETY: nothing is loaded -- the library path is empty.
    let f = unsafe { Facility::load(&cfg, slot, Role::Leader) }.unwrap();
    assert!(f.is_none(), "the default configuration loads no plugin");
}

/// The handshake agrees, and the facility reports what it agreed.
#[test]
fn the_handshake_agrees_on_version_and_table_size() {
    let _serial = serial();
    let (_c, cfg, slot) = setup("handshake", "");
    // SAFETY: the fixture is built from this workspace's own sources.
    let f = unsafe { Facility::load(&cfg, slot, Role::Leader) }
        .unwrap()
        .expect("a configured library must load");
    assert_eq!(f.header_version(), 1);
    assert!(f.header_size() >= 8);
    assert_eq!(f.generation(), 1, "a fresh facility starts at generation 1");
    assert_eq!(f.leases(), 0);
    assert_eq!(f.library(), cfg.plugin.library);
}

/// A bad path is an error the operator can read, not a panic.
#[test]
fn a_missing_library_is_a_reported_error() {
    let _serial = serial();
    let (_c, mut cfg, slot) = setup("badpath", "");
    cfg.plugin.library = "/nonexistent/libnope.so".to_string();
    // SAFETY: nothing loads; dlopen fails.
    let msg = match unsafe { Facility::load(&cfg, slot, Role::Leader) } {
        Err(e) => e.to_string(),
        Ok(_) => panic!("a configured but unloadable library must fail loudly"),
    };
    assert!(
        msg.contains("cannot load the plugin library"),
        "the message must name the problem: {msg}"
    );
}

/// A role change reaches the plugin once, and only when it actually changes.
#[test]
fn a_role_transition_is_announced_once() {
    let _serial = serial();
    let (_c, cfg, slot) = setup("role", "");
    // SAFETY: as above.
    let f = unsafe { Facility::load(&cfg, slot, Role::Leader) }
        .unwrap()
        .unwrap();
    let probe = Probe::open();
    let before: i32 = probe.get0(b"yesno_test_role_changes\0");

    f.set_role(Role::Follower);
    assert_eq!(
        probe.get0::<i32>(b"yesno_test_role_changes\0"),
        before + 1,
        "a real change is announced"
    );
    f.set_role(Role::Follower);
    assert_eq!(
        probe.get0::<i32>(b"yesno_test_role_changes\0"),
        before + 1,
        "and setting the same role again is not"
    );
}

/// Serving starts and stops only when an address is configured.
#[test]
fn the_listener_runs_only_when_an_address_is_given() {
    let _serial = serial();
    let (_c, cfg, slot) = setup("listen", "127.0.0.1:0");
    // SAFETY: as above.
    let f = unsafe { Facility::load(&cfg, slot, Role::Leader) }
        .unwrap()
        .unwrap();
    let probe = Probe::open();

    f.start_serving();
    assert_eq!(probe.get0::<i32>(b"yesno_test_serving\0"), 1);
    f.stop_serving();
    assert_eq!(probe.get0::<i32>(b"yesno_test_serving\0"), 0);

    // With no address the plugin is loaded and notified but never asked to serve.
    let (_c2, cfg2, slot2) = setup("nolisten", "");
    // SAFETY: as above.
    let g = unsafe { Facility::load(&cfg2, slot2, Role::Leader) }
        .unwrap()
        .unwrap();
    g.start_serving();
    assert_eq!(
        probe.get0::<i32>(b"yesno_test_serving\0"),
        0,
        "an empty listen address must not start a listener"
    );
}

/// The rebootstrap sequence: drain clean, bump, announce.
#[test]
fn a_rebootstrap_drains_then_bumps_the_generation() {
    let _serial = serial();
    let (_c, cfg, slot) = setup("reboot", "127.0.0.1:0");
    // SAFETY: as above.
    let f = unsafe { Facility::load(&cfg, slot, Role::Follower) }
        .unwrap()
        .unwrap();
    let probe = Probe::open();
    probe.set_drain(true);

    f.start_serving();
    assert_eq!(probe.get0::<i32>(b"yesno_test_serving\0"), 1);

    assert_eq!(
        f.before_close(),
        Drained::Clean,
        "a plugin that releases its handles drains clean"
    );
    assert_eq!(
        probe.get0::<i32>(b"yesno_test_serving\0"),
        0,
        "and the drain stops the listener first"
    );
    assert_eq!(f.leases(), 0);

    let generation = f.after_replace();
    assert_eq!(generation, 2, "the replacement bumps the generation");
    f.after_open(generation);
    assert_eq!(
        probe.get0::<u64>(b"yesno_test_generation\0"),
        2,
        "and the plugin is told which generation it is now reading"
    );
    assert_eq!(
        probe.get0::<i32>(b"yesno_test_serving\0"),
        1,
        "serving resumes after the database is back"
    );
    f.stop_serving();
}

/// A plugin that returns from the drain still holding a lease is caught, named,
/// and counted.
///
/// This is the path with no host-side remedy: `evict_oldest_reader` does not free
/// the slot and does not release the lock, so the only thing the host can do is
/// report. If this assertion ever weakens, the failure it replaces is an
/// unattributable `AlreadyOpen` from a reopen seconds later.
#[test]
fn a_plugin_that_does_not_drain_is_reported_with_its_count() {
    let _serial = serial();
    let (_c, cfg, slot) = setup("nodrain", "");
    // SAFETY: as above.
    let f = unsafe { Facility::load(&cfg, slot, Role::Follower) }
        .unwrap()
        .unwrap();
    let probe = Probe::open();

    probe.set_drain(false);
    assert_eq!(probe.hold_lease(), Status::Ok);
    assert_eq!(f.leases(), 1, "the plugin is holding a snapshot");

    assert_eq!(
        f.before_close(),
        Drained::Outstanding(1),
        "returning from on_unavailable with a lease outstanding must be detected"
    );

    // Recovery is the plugin letting go; nothing on the host side can force it.
    probe.release_lease();
    assert_eq!(f.leases(), 0);
    assert_eq!(
        f.before_close(),
        Drained::Clean,
        "and once it releases, the drain is clean"
    );
    probe.set_drain(true);
}
