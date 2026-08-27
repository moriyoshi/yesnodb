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
use std::ffi::{c_char, c_int, CStr};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;

use yesno_core::Db;

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
