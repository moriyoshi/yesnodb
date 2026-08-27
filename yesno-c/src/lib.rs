//! A small, ownership-explicit C ABI over [`yesno_core::Db`].
//!
//! The API is host-independent: it knows nothing about MySQL, PostgreSQL, or a
//! process-global database. Callers own opaque database and snapshot-cursor
//! handles and may open more than one database at a time. Database handles may
//! be shared by caller-managed threads; cursor operations require exclusive
//! caller synchronization.
//!
//! # Safety
//!
//! This crate is consumed through `include/yesno.h`. Every non-null database or
//! cursor pointer must be a live handle returned by the matching open function;
//! close consumes it exactly once and must not race another operation. Output
//! and error pointers must reference the writable sizes declared by the C API.
//! These rules are stated once here and in the public header rather than
//! repeated on every exported symbol.

#![deny(unsafe_op_in_unsafe_fn)]
#![allow(
    clippy::missing_safety_doc,
    reason = "the common C pointer contract is documented at crate level and in yesno.h"
)]

use std::any::Any;
use std::ffi::{c_char, c_int, c_void, CStr};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;

use yesno_core::dispatch::{Dispatch, Dispatcher};
use yesno_core::{Db, DbOptions};

const YESNO_OK: c_int = 0;
const YESNO_ERROR: c_int = 1;

const SEEK_EXACT: c_int = 0;
const SEEK_OR_NEXT: c_int = 1;
const SEEK_AFTER: c_int = 2;
const SEEK_OR_PREV: c_int = 3;
const SEEK_BEFORE: c_int = 4;

#[repr(C)]
pub struct yesno_db {
    inner: Db,
}

/// Run `task( task_ctx, i )` for every `i` in `[0, n)`, possibly concurrently,
/// and **return only when every one of them has finished.**
///
/// The C form of [`yesno_core::dispatch::Dispatch`]. The host supplies it so
/// that `yesno-c` never creates threads of its own: a storage engine is
/// embedded in a server that has already sized its own scheduler.
///
/// Two obligations, both the caller's:
///
/// - **Call `task` once per index, and do not return early.** Returning while a
///   task is still running is a data race on the database's internal state.
///   Skipping one *is* caught -- the commit is refused rather than silently
///   dropping a shard's work -- but returning early is not, because nothing can
///   observe it in time.
/// - **`user_data` must outlive the database handle**, which holds this pointer
///   for as long as it is open.
///
/// `task` must not be called after the dispatch function returns.
#[allow(non_camel_case_types)]
pub type yesno_dispatch_fn = Option<
    unsafe extern "C" fn(
        user_data: *mut c_void,
        n: usize,
        task: unsafe extern "C" fn(task_ctx: *mut c_void, i: usize),
        task_ctx: *mut c_void,
    ),
>;

/// Adapts a C dispatch callback to the Rust trait.
struct CDispatch {
    call: yesno_dispatch_fn,
    user_data: *mut c_void,
}

// SAFETY: `user_data` is opaque here and is only ever handed back to the
// callback the host supplied. The contract requires it to stay valid for the
// life of the database handle and to be usable from whatever threads the host's
// dispatcher runs on -- which are the host's own choice of threads.
unsafe impl Send for CDispatch {}
unsafe impl Sync for CDispatch {}

impl Dispatch for CDispatch {
    fn run(&self, n: usize, f: &(dyn Fn(usize) + Sync)) {
        let Some(call) = self.call else {
            for i in 0..n {
                f(i);
            }
            return;
        };

        /// Called back from C, once per task index.
        ///
        /// `catch_unwind` because unwinding across an FFI boundary is undefined
        /// behaviour and the host's dispatcher is C. A swallowed panic shows up
        /// as a task that did nothing, which the commit's completion count then
        /// refuses -- loud, without being unsound.
        unsafe extern "C" fn trampoline(ctx: *mut c_void, i: usize) {
            let _ = catch_unwind(AssertUnwindSafe(|| {
                // SAFETY: `ctx` is the pointer handed to the dispatcher below,
                // which must not call this after returning.
                let f = unsafe { &*(ctx as *const &(dyn Fn(usize) + Sync)) };
                f(i);
            }));
        }

        let holder: Box<&(dyn Fn(usize) + Sync)> = Box::new(f);
        let ctx = Box::into_raw(holder) as *mut c_void;
        // SAFETY: the host returns only once every task is done, so `ctx`
        // outlives every call to `trampoline`.
        unsafe { call(self.user_data, n, trampoline, ctx) };
        // SAFETY: reclaimed exactly once, after the dispatcher has returned.
        drop(unsafe { Box::from_raw(ctx as *mut &(dyn Fn(usize) + Sync)) });
    }
}

/// Open-time options. Defaults match what `yesno_db_open` uses.
#[repr(C)]
pub struct yesno_options {
    inner: DbOptions,
}

#[repr(C)]
pub struct yesno_batch {
    inner: Option<yesno_core::WriteBatch>,
}

#[repr(C)]
pub struct yesno_cursor {
    ordinals: Vec<u64>,
    position: CursorPosition,
}

#[derive(Clone, Copy)]
enum CursorPosition {
    Before,
    At(usize),
    After,
}

fn panic_message(payload: Box<dyn Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        format!("yesno-c panicked: {message}")
    } else if let Some(message) = payload.downcast_ref::<String>() {
        format!("yesno-c panicked: {message}")
    } else {
        "yesno-c panicked".to_owned()
    }
}

fn write_error(error: *mut c_char, capacity: usize, message: &str) {
    if error.is_null() || capacity == 0 {
        return;
    }
    let bytes = message.as_bytes();
    let len = bytes.len().min(capacity - 1);
    // SAFETY: The C contract requires `error` to reference `capacity` writable
    // bytes whenever it is non-null. This copies at most `capacity - 1` bytes
    // and writes the terminator inside that allocation.
    unsafe {
        ptr::copy_nonoverlapping(bytes.as_ptr(), error.cast::<u8>(), len);
        *error.add(len) = 0;
    }
}

fn ffi_call(
    error: *mut c_char,
    error_capacity: usize,
    operation: impl FnOnce() -> Result<(), String>,
) -> c_int {
    write_error(error, error_capacity, "");
    match catch_unwind(AssertUnwindSafe(operation)) {
        Ok(Ok(())) => YESNO_OK,
        Ok(Err(message)) => {
            write_error(error, error_capacity, &message);
            YESNO_ERROR
        }
        Err(payload) => {
            write_error(error, error_capacity, &panic_message(payload));
            YESNO_ERROR
        }
    }
}

fn require_output<T>(output: *mut T, name: &str) -> Result<(), String> {
    if output.is_null() {
        Err(format!("{name} output pointer is null"))
    } else {
        Ok(())
    }
}

/// The live `WriteBatch` behind a batch handle.
///
/// `None` means the handle was already committed, which is a use-after-consume
/// the C caller was told not to do. Reported rather than dereferenced.
unsafe fn batch_mut<'a>(batch: *mut yesno_batch) -> Result<&'a mut yesno_core::WriteBatch, String> {
    if batch.is_null() {
        return Err("batch handle is null".to_owned());
    }
    // SAFETY: The API contract says `batch` was returned by `yesno_batch_begin`,
    // has not been committed or aborted, and is not used concurrently.
    unsafe { &mut *batch }
        .inner
        .as_mut()
        .ok_or_else(|| "batch has already been committed".to_owned())
}

unsafe fn options_mut<'a>(options: *mut yesno_options) -> Result<&'a mut yesno_options, String> {
    if options.is_null() {
        return Err("options handle is null".to_owned());
    }
    // SAFETY: the contract says `options` came from `yesno_options_new`, has
    // not been freed, and is not used concurrently.
    Ok(unsafe { &mut *options })
}

unsafe fn db_ref<'a>(db: *const yesno_db) -> Result<&'a yesno_db, String> {
    if db.is_null() {
        return Err("database handle is null".to_owned());
    }
    // SAFETY: The API contract says `db` was returned by `yesno_db_open`, has
    // not been closed, and is not being closed concurrently with this call.
    Ok(unsafe { &*db })
}

unsafe fn cursor_mut<'a>(cursor: *mut yesno_cursor) -> Result<&'a mut yesno_cursor, String> {
    if cursor.is_null() {
        return Err("cursor handle is null".to_owned());
    }
    // SAFETY: The API contract says `cursor` was returned by
    // `yesno_cursor_open`, remains live, and is used by one caller at a time.
    Ok(unsafe { &mut *cursor })
}

fn cursor_result(
    cursor: &mut yesno_cursor,
    position: CursorPosition,
    ordinal: *mut u64,
    found: *mut u8,
) {
    cursor.position = position;
    // SAFETY: Both outputs were checked non-null. Every `At` index comes from
    // this cursor's own vector bounds.
    unsafe {
        if let CursorPosition::At(index) = position {
            *ordinal = cursor.ordinals[index];
            *found = 1;
        } else {
            *ordinal = 0;
            *found = 0;
        }
    }
}

unsafe fn with_cursor(
    cursor: *mut yesno_cursor,
    ordinal: *mut u64,
    found: *mut u8,
    operation: impl FnOnce(&yesno_cursor) -> CursorPosition,
) -> Result<(), String> {
    require_output(ordinal, "ordinal")?;
    require_output(found, "found")?;
    // SAFETY: `cursor_mut` validates nullness; the remaining ownership
    // requirements are the caller's documented responsibility.
    let cursor = unsafe { cursor_mut(cursor)? };
    let position = operation(cursor);
    cursor_result(cursor, position, ordinal, found);
    Ok(())
}

#[no_mangle]
pub unsafe extern "C" fn yesno_db_open(
    path: *const c_char,
    db: *mut *mut yesno_db,
    error: *mut c_char,
    error_capacity: usize,
) -> c_int {
    ffi_call(error, error_capacity, || {
        require_output(db, "database")?;
        // SAFETY: `db` was checked non-null and points to one output slot.
        unsafe { *db = ptr::null_mut() };
        if path.is_null() {
            return Err("database path pointer is null".to_owned());
        }
        // SAFETY: The C contract requires a live NUL-terminated path string.
        let path = unsafe { CStr::from_ptr(path) }
            .to_str()
            .map_err(|_| "database path is not valid UTF-8".to_owned())?;
        let value = Box::new(yesno_db {
            inner: Db::open(path).map_err(|e| e.to_string())?,
        });
        // SAFETY: Ownership of the box transfers to the caller, which must
        // return it exactly once to `yesno_db_close`.
        unsafe { *db = Box::into_raw(value) };
        Ok(())
    })
}

/// Create an options object carrying the defaults `yesno_db_open` uses.
#[no_mangle]
pub unsafe extern "C" fn yesno_options_new(
    options: *mut *mut yesno_options,
    error: *mut c_char,
    error_capacity: usize,
) -> c_int {
    ffi_call(error, error_capacity, || {
        require_output(options, "options")?;
        // SAFETY: checked non-null, points at one output slot.
        unsafe { *options = ptr::null_mut() };
        let value = Box::new(yesno_options {
            inner: DbOptions::default(),
        });
        // SAFETY: ownership transfers to the caller, which must return it once
        // to `yesno_options_free`.
        unsafe { *options = Box::into_raw(value) };
        Ok(())
    })
}

/// Release an options object. Safe to call with NULL.
///
/// Options are read at open, so freeing them does not disturb a database
/// already opened with them. The `user_data` behind a dispatch callback is a
/// different matter: **that** must outlive the database handle.
#[no_mangle]
pub unsafe extern "C" fn yesno_options_free(options: *mut yesno_options) {
    if options.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: the caller returns a live handle exactly once.
        drop(unsafe { Box::from_raw(options) });
    }));
}

/// Set the shard count used when a database is **created**.
///
/// Ignored when opening one that exists: the count is persisted in the
/// MANIFEST because it is part of the routing function, and a caller guessing
/// differently would look in the wrong shard for most keys.
#[no_mangle]
pub unsafe extern "C" fn yesno_options_set_shards(
    options: *mut yesno_options,
    shards: u32,
    error: *mut c_char,
    error_capacity: usize,
) -> c_int {
    ffi_call(error, error_capacity, || {
        if shards == 0 {
            return Err("shard count must be at least 1".to_owned());
        }
        unsafe { options_mut(options)? }.inner.shards = shards as usize;
        Ok(())
    })
}

/// Lend the database an executor for a commit's per-shard work.
///
/// Passing NULL restores the default, which runs every task on the calling
/// thread and spawns nothing. See [`yesno_dispatch_fn`] for the contract.
///
/// This only has work to spread when a commit touches several shards, which
/// means a **batch**: `yesno_db_insert` is one key and one ordinal, so it is
/// one commit on one shard and a dispatcher would fan out to a single task.
#[no_mangle]
pub unsafe extern "C" fn yesno_options_set_dispatch(
    options: *mut yesno_options,
    dispatch: yesno_dispatch_fn,
    user_data: *mut c_void,
    error: *mut c_char,
    error_capacity: usize,
) -> c_int {
    ffi_call(error, error_capacity, || {
        let opts = unsafe { options_mut(options)? };
        opts.inner.dispatch = match dispatch {
            None => Dispatcher::sequential(),
            Some(_) => Dispatcher::new(CDispatch {
                call: dispatch,
                user_data,
            }),
        };
        Ok(())
    })
}

/// Open a database with explicit options.
///
/// `yesno_db_open` is this with the defaults. The options are read here and not
/// retained, so they may be freed immediately afterwards.
#[no_mangle]
pub unsafe extern "C" fn yesno_db_open_with(
    path: *const c_char,
    options: *const yesno_options,
    db: *mut *mut yesno_db,
    error: *mut c_char,
    error_capacity: usize,
) -> c_int {
    ffi_call(error, error_capacity, || {
        require_output(db, "database")?;
        // SAFETY: checked non-null, points at one output slot.
        unsafe { *db = ptr::null_mut() };
        if path.is_null() {
            return Err("database path pointer is null".to_owned());
        }
        if options.is_null() {
            return Err("options handle is null".to_owned());
        }
        // SAFETY: the C contract requires a live NUL-terminated path string.
        let path = unsafe { CStr::from_ptr(path) }
            .to_str()
            .map_err(|_| "database path is not valid UTF-8".to_owned())?;
        // SAFETY: the contract says `options` came from `yesno_options_new` and
        // has not been freed.
        let opts = unsafe { &*options }.inner.clone();
        let value = Box::new(yesno_db {
            inner: Db::open_with(path, opts).map_err(|e| e.to_string())?,
        });
        // SAFETY: ownership transfers to the caller.
        unsafe { *db = Box::into_raw(value) };
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn yesno_db_close(db: *mut yesno_db) {
    if db.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: The caller returns a live handle exactly once and guarantees
        // no operation is using it concurrently.
        drop(unsafe { Box::from_raw(db) });
    }));
}

#[no_mangle]
pub unsafe extern "C" fn yesno_db_checkpoint(
    db: *const yesno_db,
    error: *mut c_char,
    error_capacity: usize,
) -> c_int {
    ffi_call(error, error_capacity, || {
        // SAFETY: Pointer ownership is the C caller's contract.
        unsafe { db_ref(db)? }
            .inner
            .checkpoint()
            .map_err(|e| e.to_string())?;
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn yesno_db_insert(
    db: *const yesno_db,
    key: u64,
    ordinal: u64,
    changed: *mut u8,
    error: *mut c_char,
    error_capacity: usize,
) -> c_int {
    ffi_call(error, error_capacity, || {
        require_output(changed, "changed")?;
        // SAFETY: Pointer ownership is the C caller's contract.
        let value = unsafe { db_ref(db)? }
            .inner
            .insert(key, ordinal)
            .map_err(|e| e.to_string())?;
        // SAFETY: `changed` was checked non-null.
        unsafe { *changed = u8::from(value) };
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn yesno_db_remove(
    db: *const yesno_db,
    key: u64,
    ordinal: u64,
    changed: *mut u8,
    error: *mut c_char,
    error_capacity: usize,
) -> c_int {
    ffi_call(error, error_capacity, || {
        require_output(changed, "changed")?;
        // SAFETY: Pointer ownership is the C caller's contract.
        let value = unsafe { db_ref(db)? }
            .inner
            .remove(key, ordinal)
            .map_err(|e| e.to_string())?;
        // SAFETY: `changed` was checked non-null.
        unsafe { *changed = u8::from(value) };
        Ok(())
    })
}

/// Begin a batch: writes accumulate and are applied as **one commit**.
///
/// # Why this exists
///
/// `yesno_db_insert` is one key and one ordinal, which is one commit touching
/// one shard. A storage engine applying a multi-row statement that way pays a
/// version, a WAL append and a durability wait per row, and can never spread
/// work across shards -- so a host that lends an executor has nothing for it to
/// run. A batch is what makes a commit span shards.
///
/// The handle owns its own `Db` clone, so it is independent of the handle it
/// was opened from; that handle must still outlive it.
#[no_mangle]
pub unsafe extern "C" fn yesno_batch_begin(
    db: *const yesno_db,
    batch: *mut *mut yesno_batch,
    error: *mut c_char,
    error_capacity: usize,
) -> c_int {
    ffi_call(error, error_capacity, || {
        require_output(batch, "batch")?;
        // SAFETY: `batch` was checked non-null and points to one output slot.
        unsafe { *batch = ptr::null_mut() };
        let value = Box::new(yesno_batch {
            inner: Some(unsafe { db_ref(db)? }.inner.batch()),
        });
        // SAFETY: Ownership transfers to the caller, which must return it
        // exactly once to `yesno_batch_commit` or `yesno_batch_abort`.
        unsafe { *batch = Box::into_raw(value) };
        Ok(())
    })
}

/// Record an insert. Nothing is visible until the batch commits.
#[no_mangle]
pub unsafe extern "C" fn yesno_batch_insert(
    batch: *mut yesno_batch,
    key: u64,
    ordinal: u64,
    error: *mut c_char,
    error_capacity: usize,
) -> c_int {
    ffi_call(error, error_capacity, || {
        unsafe { batch_mut(batch)? }.insert(key, ordinal);
        Ok(())
    })
}

/// Record a removal. Nothing is visible until the batch commits.
#[no_mangle]
pub unsafe extern "C" fn yesno_batch_remove(
    batch: *mut yesno_batch,
    key: u64,
    ordinal: u64,
    error: *mut c_char,
    error_capacity: usize,
) -> c_int {
    ffi_call(error, error_capacity, || {
        unsafe { batch_mut(batch)? }.remove(key, ordinal);
        Ok(())
    })
}

/// Record a whole-key delete. Nothing is visible until the batch commits.
#[no_mangle]
pub unsafe extern "C" fn yesno_batch_delete_key(
    batch: *mut yesno_batch,
    key: u64,
    error: *mut c_char,
    error_capacity: usize,
) -> c_int {
    ffi_call(error, error_capacity, || {
        unsafe { batch_mut(batch)? }.delete_key(key);
        Ok(())
    })
}

/// Apply the batch as one commit and **consume the handle**.
///
/// The handle is invalid after this call **whether it succeeds or fails** --
/// `WriteBatch::commit` takes `self`, and a failed commit has still consumed
/// the recorded operations. Do not pass it to `yesno_batch_abort` afterwards.
/// `changed` receives the number of ordinals that actually changed state, which
/// is not the number recorded: re-inserting a present ordinal changes nothing.
#[no_mangle]
pub unsafe extern "C" fn yesno_batch_commit(
    batch: *mut yesno_batch,
    changed: *mut u64,
    error: *mut c_char,
    error_capacity: usize,
) -> c_int {
    ffi_call(error, error_capacity, || {
        require_output(changed, "changed")?;
        if batch.is_null() {
            return Err("batch pointer is null".to_owned());
        }
        // SAFETY: The caller returns a live handle exactly once, and this is
        // that once: the box is reclaimed here regardless of the outcome.
        let mut owned = unsafe { Box::from_raw(batch) };
        let inner = owned
            .inner
            .take()
            .ok_or_else(|| "batch has already been committed".to_owned())?;
        let committed = inner.commit().map_err(|e| e.to_string())?;
        // SAFETY: `changed` was checked non-null and points to one slot.
        unsafe { *changed = committed.changed };
        Ok(())
    })
}

/// Discard a batch without applying it.
#[no_mangle]
pub unsafe extern "C" fn yesno_batch_abort(batch: *mut yesno_batch) {
    if batch.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: The caller returns a live handle exactly once.
        drop(unsafe { Box::from_raw(batch) });
    }));
}

#[no_mangle]
pub unsafe extern "C" fn yesno_db_clear(
    db: *const yesno_db,
    key: u64,
    error: *mut c_char,
    error_capacity: usize,
) -> c_int {
    ffi_call(error, error_capacity, || {
        // SAFETY: Pointer ownership is the C caller's contract.
        let db = &unsafe { db_ref(db)? }.inner;
        let mut batch = db.batch();
        batch.delete_key(key);
        batch.commit().map_err(|e| e.to_string())?;
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn yesno_db_contains(
    db: *const yesno_db,
    key: u64,
    ordinal: u64,
    present: *mut u8,
    error: *mut c_char,
    error_capacity: usize,
) -> c_int {
    ffi_call(error, error_capacity, || {
        require_output(present, "present")?;
        // SAFETY: Pointer ownership is the C caller's contract.
        let snapshot = unsafe { db_ref(db)? }
            .inner
            .snapshot()
            .map_err(|e| e.to_string())?;
        let value = snapshot.contains(key, ordinal).map_err(|e| e.to_string())?;
        // SAFETY: `present` was checked non-null.
        unsafe { *present = u8::from(value) };
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn yesno_db_cardinality(
    db: *const yesno_db,
    key: u64,
    cardinality: *mut u64,
    error: *mut c_char,
    error_capacity: usize,
) -> c_int {
    ffi_call(error, error_capacity, || {
        require_output(cardinality, "cardinality")?;
        // SAFETY: Pointer ownership is the C caller's contract.
        let snapshot = unsafe { db_ref(db)? }
            .inner
            .snapshot()
            .map_err(|e| e.to_string())?;
        let value = snapshot.cardinality(key).map_err(|e| e.to_string())?;
        // SAFETY: `cardinality` was checked non-null.
        unsafe { *cardinality = value };
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn yesno_cursor_open(
    db: *const yesno_db,
    key: u64,
    cursor: *mut *mut yesno_cursor,
    error: *mut c_char,
    error_capacity: usize,
) -> c_int {
    ffi_call(error, error_capacity, || {
        require_output(cursor, "cursor")?;
        // SAFETY: `cursor` was checked non-null and points to one output slot.
        unsafe { *cursor = ptr::null_mut() };
        // SAFETY: Pointer ownership is the C caller's contract.
        let snapshot = unsafe { db_ref(db)? }
            .inner
            .snapshot()
            .map_err(|e| e.to_string())?;
        let set = snapshot.load(key).map_err(|e| e.to_string())?;
        let value = Box::new(yesno_cursor {
            ordinals: set.iter().collect(),
            position: CursorPosition::Before,
        });
        // SAFETY: Ownership transfers to the caller, which must close it once.
        unsafe { *cursor = Box::into_raw(value) };
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn yesno_cursor_close(cursor: *mut yesno_cursor) {
    if cursor.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: The caller returns a live handle exactly once.
        drop(unsafe { Box::from_raw(cursor) });
    }));
}

#[no_mangle]
pub unsafe extern "C" fn yesno_cursor_first(
    cursor: *mut yesno_cursor,
    ordinal: *mut u64,
    found: *mut u8,
    error: *mut c_char,
    error_capacity: usize,
) -> c_int {
    ffi_call(error, error_capacity, || {
        // SAFETY: Pointer ownership is the C caller's contract.
        unsafe {
            with_cursor(cursor, ordinal, found, |cursor| {
                if cursor.ordinals.is_empty() {
                    CursorPosition::After
                } else {
                    CursorPosition::At(0)
                }
            })
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn yesno_cursor_next(
    cursor: *mut yesno_cursor,
    ordinal: *mut u64,
    found: *mut u8,
    error: *mut c_char,
    error_capacity: usize,
) -> c_int {
    ffi_call(error, error_capacity, || {
        // SAFETY: Pointer ownership is the C caller's contract.
        unsafe {
            with_cursor(cursor, ordinal, found, |cursor| {
                let next = match cursor.position {
                    CursorPosition::Before => 0,
                    CursorPosition::At(index) => index + 1,
                    CursorPosition::After => return CursorPosition::After,
                };
                if next < cursor.ordinals.len() {
                    CursorPosition::At(next)
                } else {
                    CursorPosition::After
                }
            })
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn yesno_cursor_last(
    cursor: *mut yesno_cursor,
    ordinal: *mut u64,
    found: *mut u8,
    error: *mut c_char,
    error_capacity: usize,
) -> c_int {
    ffi_call(error, error_capacity, || {
        // SAFETY: Pointer ownership is the C caller's contract.
        unsafe {
            with_cursor(cursor, ordinal, found, |cursor| {
                cursor
                    .ordinals
                    .len()
                    .checked_sub(1)
                    .map_or(CursorPosition::Before, CursorPosition::At)
            })
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn yesno_cursor_prev(
    cursor: *mut yesno_cursor,
    ordinal: *mut u64,
    found: *mut u8,
    error: *mut c_char,
    error_capacity: usize,
) -> c_int {
    ffi_call(error, error_capacity, || {
        // SAFETY: Pointer ownership is the C caller's contract.
        unsafe {
            with_cursor(cursor, ordinal, found, |cursor| match cursor.position {
                CursorPosition::Before => CursorPosition::Before,
                CursorPosition::At(index) => index
                    .checked_sub(1)
                    .map_or(CursorPosition::Before, CursorPosition::At),
                CursorPosition::After => cursor
                    .ordinals
                    .len()
                    .checked_sub(1)
                    .map_or(CursorPosition::Before, CursorPosition::At),
            })
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn yesno_cursor_seek(
    cursor: *mut yesno_cursor,
    target: u64,
    mode: c_int,
    ordinal: *mut u64,
    found: *mut u8,
    error: *mut c_char,
    error_capacity: usize,
) -> c_int {
    ffi_call(error, error_capacity, || {
        if !(SEEK_EXACT..=SEEK_BEFORE).contains(&mode) {
            return Err(format!("unknown cursor seek mode {mode}"));
        }
        // SAFETY: Pointer ownership is the C caller's contract.
        unsafe {
            with_cursor(cursor, ordinal, found, |cursor| {
                let lower = cursor.ordinals.partition_point(|&value| value < target);
                let upper = cursor.ordinals.partition_point(|&value| value <= target);
                match mode {
                    SEEK_EXACT => {
                        if cursor
                            .ordinals
                            .get(lower)
                            .is_some_and(|&value| value == target)
                        {
                            CursorPosition::At(lower)
                        } else {
                            CursorPosition::After
                        }
                    }
                    SEEK_OR_NEXT => {
                        if lower < cursor.ordinals.len() {
                            CursorPosition::At(lower)
                        } else {
                            CursorPosition::After
                        }
                    }
                    SEEK_AFTER => {
                        if upper < cursor.ordinals.len() {
                            CursorPosition::At(upper)
                        } else {
                            CursorPosition::After
                        }
                    }
                    SEEK_OR_PREV => upper
                        .checked_sub(1)
                        .map_or(CursorPosition::Before, CursorPosition::At),
                    SEEK_BEFORE => lower
                        .checked_sub(1)
                        .map_or(CursorPosition::Before, CursorPosition::At),
                    _ => unreachable!(),
                }
            })
        }
    })
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    proptest! {
        #[test]
        fn error_writes_stay_inside_the_declared_buffer(
            message in any::<String>(),
            capacity in 1_usize..128,
        ) {
            const CANARY: u8 = 0xa5;
            let mut buffer = vec![CANARY; capacity + 8];
            write_error(buffer.as_mut_ptr().cast(), capacity, &message);

            let copied = message.len().min(capacity - 1);
            prop_assert_eq!(&buffer[..copied], &message.as_bytes()[..copied]);
            prop_assert_eq!(buffer[copied], 0);
            prop_assert!(buffer[capacity..].iter().all(|&byte| byte == CANARY));
        }

        #[test]
        fn cursor_seek_matches_partitioning_the_sorted_set(
            mut ordinals in prop::collection::vec(any::<u64>(), 0..128),
            target in any::<u64>(),
            mode in SEEK_EXACT..=SEEK_BEFORE,
        ) {
            ordinals.sort_unstable();
            ordinals.dedup();
            let mut cursor = yesno_cursor {
                ordinals: ordinals.clone(),
                position: CursorPosition::Before,
            };
            let mut ordinal = 0;
            let mut found = 0;

            // SAFETY: Every pointer names a live local for the duration of the
            // call, and the cursor has not been transferred or closed.
            let status = unsafe {
                yesno_cursor_seek(
                    &mut cursor,
                    target,
                    mode,
                    &mut ordinal,
                    &mut found,
                    ptr::null_mut(),
                    0,
                )
            };
            prop_assert_eq!(status, YESNO_OK);

            let lower = ordinals.partition_point(|&value| value < target);
            let upper = ordinals.partition_point(|&value| value <= target);
            let expected = match mode {
                SEEK_EXACT if ordinals.get(lower) == Some(&target) => Some(target),
                SEEK_EXACT => None,
                SEEK_OR_NEXT => ordinals.get(lower).copied(),
                SEEK_AFTER => ordinals.get(upper).copied(),
                SEEK_OR_PREV => upper.checked_sub(1).map(|index| ordinals[index]),
                SEEK_BEFORE => lower.checked_sub(1).map(|index| ordinals[index]),
                _ => unreachable!(),
            };
            prop_assert_eq!(found, u8::from(expected.is_some()));
            prop_assert_eq!(ordinal, expected.unwrap_or(0));
        }
    }
}
