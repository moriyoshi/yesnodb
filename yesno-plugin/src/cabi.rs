//! A C ABI over this crate's channel [`crate::client`].
//!
//! `include/yesno_channel.h` is the normative document for the contract; this
//! module implements it. One Rust implementation of the protocol serves every
//! language, which is the point: the channel is a socket **and** a `memfd` arena
//! handed over with `SCM_RIGHTS`, so a hand-written client in another language
//! has to reproduce the framing, the descriptor receive, the mapping and the
//! lane arithmetic. This repository already paid for that once, when the Flight
//! ticket header widened from 40 to 48 bytes and three independently written
//! clients were not widened with it.
//!
//! # Not the ABI that was removed
//!
//! `yesno-plugin` published a different C ABI until 2026-09-29 -- an in-process
//! `cdylib` host table under `include/yesno_plugin.h`, preserved in
//! `LTM/removed-cdylib-plugin-abi.md`. That one loaded foreign code **into**
//! yesnod, which is why it went: it shared the heap, a panic escaping a callback
//! aborted the daemon, and its leases were invisible to the shutdown proof.
//!
//! This runs the other way. A separate process talks to yesnod over a socket, so
//! none of those three properties exists here, and the channel is precisely what
//! replaced that ABI. The header is deliberately **not** called
//! `yesno_plugin.h`: that name belongs to a withdrawn contract a consumer may
//! still hold a copy of, and reusing it for different semantics would be worse
//! than choosing a new name. The symbol namespace is `yesno_channel_*` for the
//! same reason -- the old one was `yesno_plugin_*`.
//!
//! # Why not in `yesno-c`
//!
//! `yesno-c` embeds a database in the calling process and takes the directory's
//! exclusive lock, and its cursor **deliberately materializes** an owned
//! snapshot so that no borrowed lifetime is handed to a foreign caller. This ABI
//! is the opposite on both counts: it connects to a server that already holds the
//! lock, and a lane payload is *borrowed* -- a pointer into memory shared with
//! that server, valid until the next advance. Those two contracts cannot both be
//! kept by one library.
//!
//! # Safety
//!
//! Every non-null handle must be one returned by the matching open function;
//! close consumes it once and must not race another call on it. Output and error
//! pointers must reference the writable sizes the header declares. These rules
//! are stated here and in the header rather than on every symbol.
//!
//! One hazard the header warns about is structurally absent rather than merely
//! documented: a snapshot and a lane cursor hold their own references to the
//! connection, so closing the channel handle while either is open cannot dangle.
//! It is still wrong order and still reported, but it is not undefined.
#![deny(unsafe_op_in_unsafe_fn)]
#![allow(non_camel_case_types)]
#![allow(
    clippy::missing_safety_doc,
    reason = "the common C pointer contract is documented at crate level and in yesno_channel.h"
)]

use std::any::Any;
use std::ffi::{c_char, c_int, CStr};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;

use crate::client::{Client, Error, LaneCursor, Snapshot};
use crate::ipc::{Write, WriteOp};

const YESNO_CHANNEL_OK: c_int = 0;
const YESNO_CHANNEL_ERROR: c_int = 1;
const YESNO_CHANNEL_RETRY: c_int = 2;
const YESNO_CHANNEL_STALE: c_int = 3;
const YESNO_CHANNEL_EXPIRED: c_int = 4;
const YESNO_CHANNEL_WRONG_ROLE: c_int = 5;

pub struct yesno_channel {
    inner: Client,
}

pub struct yesno_channel_snapshot {
    inner: Snapshot,
}

pub struct yesno_channel_lanes {
    inner: LaneCursor,
}

#[repr(C)]
pub struct yesno_channel_limits {
    pub protocol: u32,
    pub generation: u64,
    pub role: u8,
    pub shards: u32,
    pub arena_bytes: u64,
    pub max_lanes: u32,
    pub max_handles: u32,
    pub max_blocks: u32,
    pub max_writes: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct yesno_channel_write {
    pub key: u64,
    pub lo: u64,
    pub hi: u64,
    pub op: u8,
}

/// Either a protocol error with a status, or a caller mistake that is simply an
/// error. Kept apart so that classification survives the boundary: a retryable
/// `Unavailable` and a null output pointer must not reach C as the same code.
enum CErr {
    Client(Error),
    Msg(String),
}

impl From<Error> for CErr {
    fn from(e: Error) -> Self {
        CErr::Client(e)
    }
}

impl CErr {
    fn status(&self) -> c_int {
        match self {
            // The whole reason the C enum is classified rather than boolean.
            // A host that cannot tell "back off and retry" from "reconnect"
            // from "take a fresh snapshot" has to treat every failure as fatal.
            CErr::Client(Error::Unavailable { .. }) => YESNO_CHANNEL_RETRY,
            CErr::Client(Error::Stale) => YESNO_CHANNEL_STALE,
            CErr::Client(Error::SnapshotExpired { .. }) => YESNO_CHANNEL_EXPIRED,
            CErr::Client(Error::WrongRole) => YESNO_CHANNEL_WRONG_ROLE,
            CErr::Client(_) | CErr::Msg(_) => YESNO_CHANNEL_ERROR,
        }
    }

    fn message(&self) -> String {
        match self {
            CErr::Client(e) => e.to_string(),
            CErr::Msg(m) => m.clone(),
        }
    }
}

fn msg<T>(m: impl Into<String>) -> Result<T, CErr> {
    Err(CErr::Msg(m.into()))
}

fn write_error(error: *mut c_char, capacity: usize, message: &str) {
    if error.is_null() || capacity == 0 {
        return;
    }
    let bytes = message.as_bytes();
    let len = bytes.len().min(capacity - 1);
    // SAFETY: the header requires `error` to reference `capacity` writable
    // bytes when non-null. This writes at most `capacity - 1` bytes plus a
    // terminator, all inside that allocation.
    unsafe {
        ptr::copy_nonoverlapping(bytes.as_ptr(), error.cast::<u8>(), len);
        *error.add(len) = 0;
    }
}

fn panic_message(payload: Box<dyn Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        format!("panic: {s}")
    } else if let Some(s) = payload.downcast_ref::<String>() {
        format!("panic: {s}")
    } else {
        "panic: unknown payload".to_owned()
    }
}

/// Run `operation`, catching panics, and translate the outcome into a status.
///
/// `catch_unwind` is not optional: unwinding across an FFI boundary is
/// undefined behaviour, and aborting would take the host process down for a
/// fault this library can report.
fn ffi_call(
    error: *mut c_char,
    error_capacity: usize,
    operation: impl FnOnce() -> Result<(), CErr>,
) -> c_int {
    write_error(error, error_capacity, "");
    match catch_unwind(AssertUnwindSafe(operation)) {
        Ok(Ok(())) => YESNO_CHANNEL_OK,
        Ok(Err(e)) => {
            write_error(error, error_capacity, &e.message());
            e.status()
        }
        Err(payload) => {
            write_error(error, error_capacity, &panic_message(payload));
            YESNO_CHANNEL_ERROR
        }
    }
}

/// Borrow a handle, reporting a null pointer rather than dereferencing it.
unsafe fn handle<'a, T>(p: *const T, name: &str) -> Result<&'a T, CErr> {
    if p.is_null() {
        return msg(format!("{name} handle is null"));
    }
    // SAFETY: non-null, and the header requires it to be a live handle from the
    // matching open function.
    Ok(unsafe { &*p })
}

unsafe fn handle_mut<'a, T>(p: *mut T, name: &str) -> Result<&'a mut T, CErr> {
    if p.is_null() {
        return msg(format!("{name} handle is null"));
    }
    // SAFETY: as `handle`, plus the header's rule that a lane cursor is
    // single-threaded, which is what makes the exclusive reference sound.
    Ok(unsafe { &mut *p })
}

fn out_ptr<T>(p: *mut T, name: &str) -> Result<(), CErr> {
    if p.is_null() {
        msg(format!("{name} output pointer is null"))
    } else {
        Ok(())
    }
}

unsafe fn str_arg<'a>(p: *const c_char, name: &str) -> Result<&'a str, CErr> {
    if p.is_null() {
        return msg(format!("{name} is null"));
    }
    // SAFETY: non-null, and the header requires a NUL-terminated string.
    unsafe { CStr::from_ptr(p) }
        .to_str()
        .map_err(|_| CErr::Msg(format!("{name} is not valid UTF-8")))
}

/// Copy a page of `u64` into a caller-owned buffer.
///
/// A buffer the caller sizes rather than an allocation handed across the ABI,
/// so there is no free function to get wrong and no ownership to document. A
/// page larger than the buffer is an error rather than a silent truncation:
/// truncating would look exactly like a short final page and the caller would
/// stop early with a wrong answer.
fn copy_page(
    values: &[u64],
    out: *mut u64,
    capacity: usize,
    written: *mut usize,
) -> Result<(), CErr> {
    if values.len() > capacity {
        return msg(format!(
            "{} values do not fit a buffer of {capacity}; ask for a smaller limit",
            values.len()
        ));
    }
    // SAFETY: `out` is non-null, checked by the caller, and holds `capacity`
    // writable `u64`; `values.len()` is not greater than `capacity` above.
    unsafe {
        ptr::copy_nonoverlapping(values.as_ptr(), out, values.len());
        *written = values.len();
    }
    Ok(())
}

#[no_mangle]
pub unsafe extern "C" fn yesno_channel_open(
    socket_path: *const c_char,
    name: *const c_char,
    out: *mut *mut yesno_channel,
    error: *mut c_char,
    error_capacity: usize,
) -> c_int {
    ffi_call(error, error_capacity, || {
        out_ptr(out, "channel")?;
        let path = unsafe { str_arg(socket_path, "socket_path") }?;
        let name = unsafe { str_arg(name, "name") }?;
        let client = Client::connect(path, name)?;
        let boxed = Box::new(yesno_channel { inner: client });
        // SAFETY: `out` is non-null, checked above.
        unsafe { *out = Box::into_raw(boxed) };
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn yesno_channel_close(channel: *mut yesno_channel) {
    if channel.is_null() {
        return;
    }
    // SAFETY: the header requires a live handle from `yesno_channel_open`,
    // closed exactly once. Dropping it does not invalidate a snapshot or lane
    // cursor still open: those hold their own reference to the connection.
    let _ = catch_unwind(AssertUnwindSafe(|| drop(unsafe { Box::from_raw(channel) })));
}

#[no_mangle]
pub unsafe extern "C" fn yesno_channel_get_limits(
    channel: *const yesno_channel,
    out: *mut yesno_channel_limits,
    error: *mut c_char,
    error_capacity: usize,
) -> c_int {
    ffi_call(error, error_capacity, || {
        out_ptr(out, "limits")?;
        let c = unsafe { handle(channel, "channel") }?;
        let l = c.inner.limits();
        // SAFETY: `out` is non-null, checked above.
        unsafe {
            *out = yesno_channel_limits {
                protocol: l.protocol,
                generation: l.generation,
                role: l.role as u8,
                shards: l.shards,
                arena_bytes: l.arena_bytes,
                max_lanes: l.max_lanes,
                max_handles: l.max_handles,
                max_blocks: l.max_blocks,
                max_writes: l.max_writes,
            }
        };
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn yesno_channel_is_arena(
    channel: *const yesno_channel,
    out: *mut u8,
    error: *mut c_char,
    error_capacity: usize,
) -> c_int {
    ffi_call(error, error_capacity, || {
        out_ptr(out, "is_arena")?;
        let c = unsafe { handle(channel, "channel") }?;
        // SAFETY: `out` is non-null, checked above.
        unsafe { *out = u8::from(c.inner.is_arena()) };
        Ok(())
    })
}

fn write_op(raw: u8) -> Result<WriteOp, CErr> {
    match raw {
        0 => Ok(WriteOp::Insert),
        1 => Ok(WriteOp::Remove),
        2 => Ok(WriteOp::InsertRange),
        3 => Ok(WriteOp::RemoveRange),
        4 => Ok(WriteOp::DeleteKey),
        other => msg(format!("write op {other} is not one this build defines")),
    }
}

#[no_mangle]
pub unsafe extern "C" fn yesno_channel_apply(
    channel: *const yesno_channel,
    writes: *const yesno_channel_write,
    count: usize,
    version: *mut u64,
    changed: *mut u64,
    error: *mut c_char,
    error_capacity: usize,
) -> c_int {
    ffi_call(error, error_capacity, || {
        out_ptr(version, "version")?;
        out_ptr(changed, "changed")?;
        let c = unsafe { handle(channel, "channel") }?;
        if writes.is_null() && count != 0 {
            return msg("writes pointer is null but count is not zero");
        }
        // SAFETY: the header requires `writes` to reference `count` entries.
        let slice = if count == 0 {
            &[][..]
        } else {
            unsafe { std::slice::from_raw_parts(writes, count) }
        };
        let mut owned = Vec::with_capacity(slice.len());
        for w in slice {
            owned.push(Write {
                key: w.key,
                lo: w.lo,
                hi: w.hi,
                op: write_op(w.op)?,
            });
        }
        let (v, ch) = c.inner.apply(owned)?;
        // SAFETY: both are non-null, checked above.
        unsafe {
            *version = v;
            *changed = ch;
        }
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn yesno_channel_snapshot_open(
    channel: *const yesno_channel,
    out: *mut *mut yesno_channel_snapshot,
    error: *mut c_char,
    error_capacity: usize,
) -> c_int {
    ffi_call(error, error_capacity, || {
        out_ptr(out, "snapshot")?;
        let c = unsafe { handle(channel, "channel") }?;
        let snap = c.inner.snapshot()?;
        let boxed = Box::new(yesno_channel_snapshot { inner: snap });
        // SAFETY: `out` is non-null, checked above.
        unsafe { *out = Box::into_raw(boxed) };
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn yesno_channel_snapshot_close(snapshot: *mut yesno_channel_snapshot) {
    if snapshot.is_null() {
        return;
    }
    // SAFETY: a live handle from `yesno_channel_snapshot_open`, closed once.
    // Dropping it releases the pinned version on the server.
    let _ = catch_unwind(AssertUnwindSafe(|| {
        drop(unsafe { Box::from_raw(snapshot) })
    }));
}

#[no_mangle]
pub unsafe extern "C" fn yesno_channel_snapshot_version(
    snapshot: *const yesno_channel_snapshot,
    out: *mut u64,
    error: *mut c_char,
    error_capacity: usize,
) -> c_int {
    ffi_call(error, error_capacity, || {
        out_ptr(out, "version")?;
        let s = unsafe { handle(snapshot, "snapshot") }?;
        // SAFETY: `out` is non-null, checked above.
        unsafe { *out = s.inner.version() };
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn yesno_channel_contains(
    snapshot: *const yesno_channel_snapshot,
    key: u64,
    ordinal: u64,
    present: *mut u8,
    error: *mut c_char,
    error_capacity: usize,
) -> c_int {
    ffi_call(error, error_capacity, || {
        out_ptr(present, "present")?;
        let s = unsafe { handle(snapshot, "snapshot") }?;
        let found = s.inner.contains(key, ordinal)?;
        // SAFETY: `present` is non-null, checked above.
        unsafe { *present = u8::from(found) };
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn yesno_channel_cardinality(
    snapshot: *const yesno_channel_snapshot,
    key: u64,
    out: *mut u64,
    error: *mut c_char,
    error_capacity: usize,
) -> c_int {
    ffi_call(error, error_capacity, || {
        out_ptr(out, "cardinality")?;
        let s = unsafe { handle(snapshot, "snapshot") }?;
        let n = s.inner.cardinality(key)?;
        // SAFETY: `out` is non-null, checked above.
        unsafe { *out = n };
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn yesno_channel_max(
    snapshot: *const yesno_channel_snapshot,
    key: u64,
    present: *mut u8,
    value: *mut u64,
    error: *mut c_char,
    error_capacity: usize,
) -> c_int {
    ffi_call(error, error_capacity, || {
        out_ptr(present, "present")?;
        out_ptr(value, "value")?;
        let s = unsafe { handle(snapshot, "snapshot") }?;
        let found = s.inner.max(key)?;
        // SAFETY: both are non-null, checked above. `value` is left untouched
        // when the key is empty, which is what the header promises.
        unsafe {
            *present = u8::from(found.is_some());
            if let Some(v) = found {
                *value = v;
            }
        }
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn yesno_channel_load(
    snapshot: *const yesno_channel_snapshot,
    key: u64,
    has_after: u8,
    after: u64,
    limit: u32,
    out: *mut u64,
    capacity: usize,
    written: *mut usize,
    more: *mut u8,
    error: *mut c_char,
    error_capacity: usize,
) -> c_int {
    ffi_call(error, error_capacity, || {
        out_ptr(out, "ordinals")?;
        out_ptr(written, "written")?;
        out_ptr(more, "more")?;
        let s = unsafe { handle(snapshot, "snapshot") }?;
        let resume = if has_after != 0 { Some(after) } else { None };
        let (values, has_more) = s.inner.load(key, resume, limit)?;
        copy_page(&values, out, capacity, written)?;
        // SAFETY: `more` is non-null, checked above.
        unsafe { *more = u8::from(has_more) };
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn yesno_channel_key_range(
    snapshot: *const yesno_channel_snapshot,
    lo: u64,
    hi: u64,
    limit: u32,
    out: *mut u64,
    capacity: usize,
    written: *mut usize,
    more: *mut u8,
    error: *mut c_char,
    error_capacity: usize,
) -> c_int {
    ffi_call(error, error_capacity, || {
        out_ptr(out, "keys")?;
        out_ptr(written, "written")?;
        out_ptr(more, "more")?;
        let s = unsafe { handle(snapshot, "snapshot") }?;
        let (values, has_more) = s.inner.key_range(lo, hi, limit)?;
        copy_page(&values, out, capacity, written)?;
        // SAFETY: `more` is non-null, checked above.
        unsafe { *more = u8::from(has_more) };
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn yesno_channel_lanes_open(
    snapshot: *const yesno_channel_snapshot,
    keys: *const u64,
    count: usize,
    out: *mut *mut yesno_channel_lanes,
    error: *mut c_char,
    error_capacity: usize,
) -> c_int {
    ffi_call(error, error_capacity, || {
        out_ptr(out, "lanes")?;
        let s = unsafe { handle(snapshot, "snapshot") }?;
        if keys.is_null() && count != 0 {
            return msg("keys pointer is null but count is not zero");
        }
        // SAFETY: the header requires `keys` to reference `count` values.
        let owned = if count == 0 {
            Vec::new()
        } else {
            unsafe { std::slice::from_raw_parts(keys, count) }.to_vec()
        };
        let cursor = s.inner.lanes(owned)?;
        let boxed = Box::new(yesno_channel_lanes { inner: cursor });
        // SAFETY: `out` is non-null, checked above.
        unsafe { *out = Box::into_raw(boxed) };
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn yesno_channel_lanes_close(lanes: *mut yesno_channel_lanes) {
    if lanes.is_null() {
        return;
    }
    // SAFETY: a live handle from `yesno_channel_lanes_open`, closed once.
    // Dropping it releases the handle on the server and invalidates every
    // payload pointer previously handed out, which the header states.
    let _ = catch_unwind(AssertUnwindSafe(|| drop(unsafe { Box::from_raw(lanes) })));
}

#[no_mangle]
pub unsafe extern "C" fn yesno_channel_lanes_advance(
    lanes: *mut yesno_channel_lanes,
    have: *mut u8,
    error: *mut c_char,
    error_capacity: usize,
) -> c_int {
    ffi_call(error, error_capacity, || {
        out_ptr(have, "have")?;
        let l = unsafe { handle_mut(lanes, "lanes") }?;
        let stepped = l.inner.advance()?;
        // SAFETY: `have` is non-null, checked above.
        unsafe { *have = u8::from(stepped) };
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn yesno_channel_lanes_prefix(
    lanes: *const yesno_channel_lanes,
    out: *mut u64,
    error: *mut c_char,
    error_capacity: usize,
) -> c_int {
    ffi_call(error, error_capacity, || {
        out_ptr(out, "prefix")?;
        let l = unsafe { handle(lanes, "lanes") }?;
        match l.inner.prefix() {
            Some(p) => {
                // SAFETY: `out` is non-null, checked above.
                unsafe { *out = p };
                Ok(())
            }
            None => msg("the lane cursor has no current block"),
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn yesno_channel_lanes_count(
    lanes: *const yesno_channel_lanes,
    out: *mut usize,
    error: *mut c_char,
    error_capacity: usize,
) -> c_int {
    ffi_call(error, error_capacity, || {
        out_ptr(out, "count")?;
        let l = unsafe { handle(lanes, "lanes") }?;
        if l.inner.prefix().is_none() {
            return msg("the lane cursor has no current block");
        }
        // SAFETY: `out` is non-null, checked above.
        unsafe { *out = l.inner.lane_count() };
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn yesno_channel_lane(
    lanes: *const yesno_channel_lanes,
    index: usize,
    kind: *mut u8,
    count: *mut u32,
    payload: *mut *const u8,
    payload_len: *mut usize,
    error: *mut c_char,
    error_capacity: usize,
) -> c_int {
    ffi_call(error, error_capacity, || {
        out_ptr(kind, "kind")?;
        out_ptr(count, "count")?;
        out_ptr(payload, "payload")?;
        out_ptr(payload_len, "payload_len")?;
        let l = unsafe { handle(lanes, "lanes") }?;
        let (lane, bytes) = l.inner.lane(index)?;
        // SAFETY: all four are non-null, checked above. The pointer handed out
        // borrows the arena mapping or this library's receive buffer; the
        // header states that it is valid only until the next advance or close,
        // which is the contract that makes a block read copy-free.
        unsafe {
            *kind = lane.kind as u8;
            *count = lane.count;
            *payload = bytes.as_ptr();
            *payload_len = bytes.len();
        }
        Ok(())
    })
}
