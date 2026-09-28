//! Loading a plugin, and agreeing an ABI with it.
//!
//! # Why this is here and not in `yesno-server`
//!
//! The design first put loading in the server "where the slot and the role already
//! are". That conflated *loading* with *lifecycle*: only the lifecycle needs a
//! server. Negotiation -- version, table size, a missing symbol, a plugin that
//! refuses -- is the part most likely to be got wrong and the part a server makes
//! expensive to test, so it lives here and `yesno-server` decides only *when* to
//! call it.
//!
//! # The load is not sandboxed and cannot be
//!
//! `dlopen` runs the library's initializers before returning, so a plugin can act
//! before any function here is called. Once loaded it shares the address space:
//! it can corrupt the host's heap, and if it unwinds out of a callback the process
//! aborts. **A plugin is as trusted as the server binary**, and the configuration
//! that names one is an operator decision of the same weight as the data
//! directory.
//!
//! What the header can still do, and does, is state the obligations precisely
//! enough that a correct plugin is writable: catch everything at the callback
//! edge, never link `yesno-core`, drain on `on_unavailable`.

use std::ffi::{CString, OsStr};

use crate::abi::{AbiHeader, Role, Status, ABI_V1};
use crate::table::{host_api, HostApi, HostDb, PluginApi};
use crate::Host;

/// Why a load did not produce a usable plugin.
#[derive(Debug)]
pub enum LoadError {
    /// `dlopen` failed: missing file, missing dependency, wrong architecture.
    Open(String),
    /// The library has no `yesno_plugin_init`.
    MissingSymbol(String),
    /// `yesno_plugin_init` answered a failure.
    InitRefused(Status),
    /// It answered `OK` and left the table pointer null.
    NoTable,
    /// The table's version is not one this host speaks.
    Version { got: u32, want: u32 },
    /// The table is smaller than v1 requires, so a member we must call is absent.
    TooSmall { got: u32, need: u32 },
    /// The path is not representable as a C string.
    BadPath,
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::Open(e) => write!(f, "cannot load the plugin library: {e}"),
            LoadError::MissingSymbol(e) => {
                write!(f, "the plugin exports no yesno_plugin_init: {e}")
            }
            LoadError::InitRefused(s) => {
                write!(f, "the plugin refused to initialize: {}", s.name())
            }
            LoadError::NoTable => write!(
                f,
                "the plugin reported success but left its table pointer null"
            ),
            LoadError::Version { got, want } => write!(
                f,
                "the plugin speaks ABI version {got}; this host speaks {want}"
            ),
            LoadError::TooSmall { got, need } => write!(
                f,
                "the plugin's table is {got} bytes; version {ABI_V1} needs at least {need}"
            ),
            LoadError::BadPath => write!(f, "the plugin path is not a valid C string"),
        }
    }
}

impl std::error::Error for LoadError {}

/// A loaded plugin, its table, and the host state it was given.
pub struct LoadedPlugin {
    api: *const PluginApi,
    /// Boxed so its address is stable: the plugin holds this pointer for the life
    /// of the process, so it must not be a local that moves.
    _host: Box<Host>,
    /// **Never closed, deliberately.**
    ///
    /// `ManuallyDrop` rather than a plain `Library` because unloading is the one
    /// thing v1 must not do. The plugin owns its listener and may have created
    /// threads; `dlclose` while one of them is running executes freed code, and
    /// the failure is a crash in the host with a stack that points nowhere. There
    /// is no hot unload in v1, so the honest implementation of "loaded for the
    /// process lifetime" is to leak exactly one handle and say so.
    _lib: std::mem::ManuallyDrop<libloading::Library>,
}

// SAFETY: The table is a set of `extern "C"` function pointers into a library that
// is never unloaded, and `Host` is `Send + Sync` ( every field is an `Arc` ). The
// plugin's own thread-safety is its contract, stated in the header; this marker
// says the handle may be moved and shared, not that the plugin is reentrant.
unsafe impl Send for LoadedPlugin {}
unsafe impl Sync for LoadedPlugin {}

impl LoadedPlugin {
    /// Load `path`, hand it `host`, and agree an ABI.
    ///
    /// # Safety
    ///
    /// Loads and executes arbitrary code: the library's initializers run inside
    /// `dlopen`, before this returns. The caller is asserting that `path` names a
    /// library it trusts as much as the host binary itself.
    pub unsafe fn load(path: &OsStr, host: Host) -> Result<LoadedPlugin, LoadError> {
        // SAFETY: The caller's contract, restated in this function's own.
        let lib = unsafe { libloading::Library::new(path) }
            .map_err(|e| LoadError::Open(e.to_string()))?;

        type InitFn =
            unsafe extern "C" fn(*const HostApi, *mut HostDb, *mut *const PluginApi) -> Status;
        // SAFETY: The symbol is called only through the signature the header
        // declares for it, and a mismatch there is the plugin's bug to fix.
        let init: libloading::Symbol<InitFn> = unsafe { lib.get(b"yesno_plugin_init\0") }
            .map_err(|e| LoadError::MissingSymbol(e.to_string()))?;

        // Boxed before the call, because the pointer handed over must stay valid
        // for the life of the process and a local would move on return.
        let host = Box::new(host);
        let host_ptr = (&*host) as *const Host as *mut HostDb;

        // The host table is a value the plugin may keep, so it must not be a
        // temporary. Leaked once per load, which is the same lifetime as the
        // library it was handed to.
        let table: &'static HostApi = Box::leak(Box::new(host_api()));

        let mut api: *const PluginApi = std::ptr::null();
        // SAFETY: `table` is 'static, `host_ptr` points at a boxed `Host` this
        // struct keeps alive, and `api` is one writable slot. Anything the plugin
        // does beyond that is outside what this call can guarantee, which is the
        // point of the `unsafe` on `load`.
        let status = unsafe { init(table as *const HostApi, host_ptr, &mut api) };
        if status != Status::Ok {
            return Err(LoadError::InitRefused(status));
        }
        if api.is_null() {
            return Err(LoadError::NoTable);
        }

        // SAFETY: Non-null, and the plugin promises a table it keeps alive for the
        // process. Only the header prefix is read before the size is checked.
        let header: AbiHeader = unsafe { std::ptr::read(api as *const AbiHeader) };
        if header.version != ABI_V1 {
            return Err(LoadError::Version {
                got: header.version,
                want: ABI_V1,
            });
        }
        let need = std::mem::size_of::<PluginApi>() as u32;
        if header.size < need {
            // Smaller means a member this host will call does not exist. Larger is
            // fine and is the forward-compatible case: the plugin was built
            // against a later header and we read only the prefix we know.
            return Err(LoadError::TooSmall {
                got: header.size,
                need,
            });
        }

        Ok(LoadedPlugin {
            api,
            _host: host,
            _lib: std::mem::ManuallyDrop::new(lib),
        })
    }

    fn api(&self) -> &PluginApi {
        // SAFETY: Checked non-null at load, and the plugin keeps the table alive
        // for the process. The library is never unloaded.
        unsafe { &*self.api }
    }

    /// The plugin's ABI version and table size, as it declared them.
    pub fn header(&self) -> AbiHeader {
        self.api().header
    }

    /// Tell the plugin the database is going away. It must drain before returning.
    ///
    /// Every call below crosses into the plugin, which means it may abort the
    /// process if the plugin lets a panic or an exception escape. There is nothing
    /// to wrap: by the time this frame could observe an unwind it has already
    /// crossed a boundary where unwinding is undefined.
    pub fn on_unavailable(&self) -> Status {
        // SAFETY: A function pointer from a validated table into a live library.
        unsafe { (self.api().on_unavailable)() }
    }

    pub fn on_available(&self, generation: u64) -> Status {
        // SAFETY: As above.
        unsafe { (self.api().on_available)(generation) }
    }

    pub fn on_generation_change(&self, old: u64, new: u64) -> Status {
        // SAFETY: As above.
        unsafe { (self.api().on_generation_change)(old, new) }
    }

    pub fn on_role_change(&self, from: Role, to: Role) -> Status {
        // SAFETY: As above.
        unsafe { (self.api().on_role_change)(from, to) }
    }

    /// Ask the plugin to bind and serve. `addr` is the host's configuration.
    pub fn serve_start(&self, addr: &str) -> Result<Status, LoadError> {
        let c = CString::new(addr).map_err(|_| LoadError::BadPath)?;
        // SAFETY: `c` outlives the call, and the header says the plugin must copy
        // anything it keeps.
        Ok(unsafe { (self.api().serve_start)(c.as_ptr()) })
    }

    pub fn serve_stop(&self) -> Status {
        // SAFETY: As above.
        unsafe { (self.api().serve_stop)() }
    }
}
