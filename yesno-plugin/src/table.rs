//! The `extern "C"` host table.
//!
//! # Every entry point catches panics
//!
//! Unwinding across an FFI boundary is undefined, so each function is wrapped in
//! `catch_unwind` and answers [`Status::Internal`] on a panic. This is why the
//! read paths below avoid panicking in the first place -- an out-of-range lane
//! answers "absent" rather than indexing -- since a panic that has to be caught
//! to be reported has already lost the backtrace that would explain it.
//!
//! # Null is a caller error, not a crash
//!
//! Every pointer is checked. A null handle answers `InvalidArgument`, because a
//! plugin with a bug should get a code it can log rather than a segfault inside
//! the host that will be attributed to the database.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::Ordering;

use yesno_core::{Container, KeyLanes};

use crate::abi::{AbiHeader, Chunk, ChunkKind, Role, Status, ABI_V1};
use crate::{Host, LanesHandle, SnapshotHandle};

/// Opaque to C; a `*mut Host` in practice.
pub enum HostDb {}
/// Opaque to C; a `*mut SnapshotHandle` in practice.
pub enum SnapshotOpaque {}
/// Opaque to C; a `*mut LanesHandle` in practice.
pub enum LanesOpaque {}

/// The host table, laid out exactly as `yesno_host_api`.
#[repr(C)]
pub struct HostApi {
    pub header: AbiHeader,
    pub status_name: extern "C" fn(Status) -> *const std::ffi::c_char,
    pub db_generation: unsafe extern "C" fn(*const HostDb, *mut u64) -> Status,
    pub db_role: unsafe extern "C" fn(*const HostDb, *mut Role) -> Status,
    pub db_accepts_writes: unsafe extern "C" fn(*const HostDb, *mut u8) -> Status,
    pub snapshot_open: unsafe extern "C" fn(*mut HostDb, *mut *mut SnapshotOpaque) -> Status,
    pub snapshot_close: unsafe extern "C" fn(*mut SnapshotOpaque),
    pub snapshot_version: unsafe extern "C" fn(*const SnapshotOpaque, *mut u64) -> Status,
    pub lanes_acquire: unsafe extern "C" fn(
        *mut SnapshotOpaque,
        *const u64,
        usize,
        *mut *mut LanesOpaque,
    ) -> Status,
    pub lanes_release: unsafe extern "C" fn(*mut LanesOpaque),
    pub lanes_count: unsafe extern "C" fn(*const LanesOpaque) -> usize,
    pub block_advance: unsafe extern "C" fn(*mut LanesOpaque, *mut u64, *mut u8) -> Status,
    pub block_lane: unsafe extern "C" fn(*const LanesOpaque, usize, *mut Chunk) -> Status,
    pub block_lane_into: unsafe extern "C" fn(
        *const LanesOpaque,
        usize,
        *mut std::ffi::c_void,
        usize,
        *mut Chunk,
    ) -> Status,
    pub block_release: unsafe extern "C" fn(*mut LanesOpaque),
}

/// The table this build publishes.
pub const fn host_api() -> HostApi {
    HostApi {
        header: AbiHeader {
            version: ABI_V1,
            size: std::mem::size_of::<HostApi>() as u32,
        },
        status_name,
        db_generation,
        db_role,
        db_accepts_writes,
        snapshot_open,
        snapshot_close,
        snapshot_version,
        lanes_acquire,
        lanes_release,
        lanes_count,
        block_advance,
        block_lane,
        block_lane_into,
        block_release,
    }
}

/// Run `f`, turning a panic into `Internal`.
fn guard(f: impl FnOnce() -> Status) -> Status {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(s) => s,
        Err(_) => Status::Internal,
    }
}

extern "C" fn status_name(status: Status) -> *const std::ffi::c_char {
    // Every arm is a literal with a NUL, so this is a pointer into static memory
    // with no allocation and no lifetime for the caller to respect.
    let s: &'static str = match status {
        Status::Ok => "OK\0",
        Status::InvalidArgument => "INVALID_ARGUMENT\0",
        Status::Internal => "INTERNAL\0",
        Status::Unavailable => "UNAVAILABLE\0",
        Status::SnapshotTooOld => "SNAPSHOT_TOO_OLD\0",
        Status::GenerationChanged => "GENERATION_CHANGED\0",
        Status::WrongRole => "WRONG_ROLE\0",
        Status::AbiMismatch => "ABI_MISMATCH\0",
        Status::BlockState => "BLOCK_STATE\0",
    };
    s.as_ptr() as *const std::ffi::c_char
}

/// # Safety
/// `db` must be null or a pointer the host handed out, still live.
unsafe extern "C" fn db_generation(db: *const HostDb, out: *mut u64) -> Status {
    guard(|| {
        if db.is_null() || out.is_null() {
            return Status::InvalidArgument;
        }
        // SAFETY: Checked non-null; provenance is the host's contract.
        let host = unsafe { &*(db as *const Host) };
        // SAFETY: Checked non-null, one output slot.
        unsafe { *out = host.generation() };
        Status::Ok
    })
}

/// # Safety
/// As [`db_generation`].
unsafe extern "C" fn db_role(db: *const HostDb, out: *mut Role) -> Status {
    guard(|| {
        if db.is_null() || out.is_null() {
            return Status::InvalidArgument;
        }
        // SAFETY: Checked non-null; provenance is the host's contract.
        let host = unsafe { &*(db as *const Host) };
        // SAFETY: Checked non-null, one output slot.
        unsafe { *out = host.role() };
        Status::Ok
    })
}

/// # Safety
/// As [`db_generation`].
unsafe extern "C" fn db_accepts_writes(db: *const HostDb, out: *mut u8) -> Status {
    guard(|| {
        if db.is_null() || out.is_null() {
            return Status::InvalidArgument;
        }
        // SAFETY: Checked non-null; provenance is the host's contract.
        let host = unsafe { &*(db as *const Host) };
        // A follower refuses writes, and so does an absent database -- the
        // distinction a plugin needs is "may I write", and both answers are no.
        let yes = host.role() == Role::Leader && host.db().is_some();
        // SAFETY: Checked non-null, one output slot.
        unsafe { *out = u8::from(yes) };
        Status::Ok
    })
}

/// # Safety
/// As [`db_generation`]; `out` must point at one writable slot.
unsafe extern "C" fn snapshot_open(db: *mut HostDb, out: *mut *mut SnapshotOpaque) -> Status {
    guard(|| {
        if db.is_null() || out.is_null() {
            return Status::InvalidArgument;
        }
        // SAFETY: Checked non-null, one output slot.
        unsafe { *out = std::ptr::null_mut() };
        // SAFETY: Checked non-null; provenance is the host's contract.
        let host = unsafe { &*(db as *const Host) }.clone();
        let Some(database) = host.db() else {
            return Status::Unavailable;
        };
        let snap = match database.snapshot() {
            Ok(s) => s,
            Err(e) => return Status::from_core(&e),
        };
        // Counted before the handle escapes, so a lease is never outstanding
        // without having been counted.
        host.leases.fetch_add(1, Ordering::AcqRel);
        let handle = Box::new(SnapshotHandle { snap, host });
        // SAFETY: Ownership transfers to the caller, released by snapshot_close.
        unsafe { *out = Box::into_raw(handle) as *mut SnapshotOpaque };
        Status::Ok
    })
}

/// # Safety
/// `snap` must be null or a live handle from [`snapshot_open`], returned once.
unsafe extern "C" fn snapshot_close(snap: *mut SnapshotOpaque) {
    if snap.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: The caller returns a live handle exactly once. Drop decrements
        // the lease count.
        drop(unsafe { Box::from_raw(snap as *mut SnapshotHandle) });
    }));
}

/// # Safety
/// `snap` must be null or a live handle from [`snapshot_open`].
unsafe extern "C" fn snapshot_version(snap: *const SnapshotOpaque, out: *mut u64) -> Status {
    guard(|| {
        if snap.is_null() || out.is_null() {
            return Status::InvalidArgument;
        }
        // SAFETY: Checked non-null; provenance is the caller's contract.
        let handle = unsafe { &*(snap as *const SnapshotHandle) };
        // SAFETY: Checked non-null, one output slot.
        unsafe { *out = handle.snap.version() };
        Status::Ok
    })
}

/// # Safety
/// `snap` must be a live handle; `keys` must point at `n` readable `u64`s, or be
/// null when `n` is 0.
unsafe extern "C" fn lanes_acquire(
    snap: *mut SnapshotOpaque,
    keys: *const u64,
    n: usize,
    out: *mut *mut LanesOpaque,
) -> Status {
    guard(|| {
        if snap.is_null() || out.is_null() || (keys.is_null() && n != 0) {
            return Status::InvalidArgument;
        }
        // SAFETY: Checked non-null, one output slot.
        unsafe { *out = std::ptr::null_mut() };
        // SAFETY: Checked non-null; provenance is the caller's contract.
        let handle = unsafe { &*(snap as *const SnapshotHandle) };
        // Copied, so the caller may free its array on return. An empty slice for
        // n == 0 rather than a null-derived one, which would be UB.
        let requested: &[u64] = if n == 0 {
            &[]
        } else {
            // SAFETY: Checked non-null with n != 0; the caller promises n
            // readable elements.
            unsafe { std::slice::from_raw_parts(keys, n) }
        };
        let lanes = match KeyLanes::new(&handle.snap, requested) {
            Ok(l) => l,
            Err(e) => return Status::from_core(&e),
        };
        let host = handle.host.clone();
        host.leases.fetch_add(1, Ordering::AcqRel);
        let boxed = Box::new(LanesHandle {
            lanes,
            host,
            block_open: false,
        });
        // SAFETY: Ownership transfers to the caller, released by lanes_release.
        unsafe { *out = Box::into_raw(boxed) as *mut LanesOpaque };
        Status::Ok
    })
}

/// # Safety
/// `lanes` must be null or a live handle from [`lanes_acquire`], returned once.
unsafe extern "C" fn lanes_release(lanes: *mut LanesOpaque) {
    if lanes.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: The caller returns a live handle exactly once.
        drop(unsafe { Box::from_raw(lanes as *mut LanesHandle) });
    }));
}

/// # Safety
/// `lanes` must be null or a live handle.
unsafe extern "C" fn lanes_count(lanes: *const LanesOpaque) -> usize {
    if lanes.is_null() {
        return 0;
    }
    catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: Checked non-null; provenance is the caller's contract.
        unsafe { &*(lanes as *const LanesHandle) }.lanes.lanes()
    }))
    .unwrap_or(0)
}

/// # Safety
/// `lanes` must be a live handle; both outputs one writable slot each.
unsafe extern "C" fn block_advance(
    lanes: *mut LanesOpaque,
    prefix: *mut u64,
    done: *mut u8,
) -> Status {
    guard(|| {
        if lanes.is_null() || prefix.is_null() || done.is_null() {
            return Status::InvalidArgument;
        }
        // SAFETY: Checked non-null; provenance is the caller's contract.
        let handle = unsafe { &mut *(lanes as *mut LanesHandle) };
        if handle.block_open {
            // The one protocol mistake worth its own code: advancing with
            // borrowed pointers still outstanding would hand the caller freed
            // descriptors, and there is no way to detect that afterwards.
            return Status::BlockState;
        }
        match handle.lanes.advance() {
            Ok(Some(p)) => {
                handle.block_open = true;
                // SAFETY: Both checked non-null, one slot each.
                unsafe {
                    *prefix = p;
                    *done = 0;
                }
                Status::Ok
            }
            Ok(None) => {
                // SAFETY: Both checked non-null, one slot each.
                unsafe {
                    *prefix = 0;
                    *done = 1;
                }
                Status::Ok
            }
            Err(e) => Status::from_core(&e),
        }
    })
}

/// Describe `c` without copying, or report that it cannot be lent.
///
/// `None` means "kind is known, payload is not borrowable": only a bitmap from an
/// unaligned imported mapping, where `BitmapContainer::try_words` answers `None`.
fn describe(prefix: u64, c: &Container) -> Option<Chunk> {
    let (kind, count, data) = match c {
        Container::Array(a) => {
            let s = a.as_slice();
            (
                ChunkKind::Array,
                s.len(),
                s.as_ptr() as *const std::ffi::c_void,
            )
        }
        Container::Run(r) => {
            let s = r.as_flat();
            // Intervals, not u16s: the header says `count` is pairs for a run,
            // and handing the caller the flat length would have it read twice as
            // many intervals as exist.
            (
                ChunkKind::Run,
                s.len() / 2,
                s.as_ptr() as *const std::ffi::c_void,
            )
        }
        Container::Bitmap(b) => {
            let w = b.try_words()?;
            (
                ChunkKind::Bitmap,
                w.len(),
                w.as_ptr() as *const std::ffi::c_void,
            )
        }
    };
    Some(Chunk {
        prefix,
        kind: kind as u32,
        count: count as u32,
        data,
    })
}

/// # Safety
/// `lanes` must be a live handle; `out` one writable slot.
unsafe extern "C" fn block_lane(lanes: *const LanesOpaque, lane: usize, out: *mut Chunk) -> Status {
    guard(|| {
        if lanes.is_null() || out.is_null() {
            return Status::InvalidArgument;
        }
        // SAFETY: Checked non-null; provenance is the caller's contract.
        let handle = unsafe { &*(lanes as *const LanesHandle) };
        if !handle.block_open {
            return Status::BlockState;
        }
        let prefix = handle.lanes.prefix().unwrap_or(0);
        let described = match handle.lane(lane) {
            None => Chunk::absent(prefix),
            Some(c) => match describe(prefix, c) {
                Some(d) => d,
                // Kind is real, payload is not lendable. Null data with a real
                // kind is the documented signal to retry with scratch.
                None => Chunk {
                    prefix,
                    kind: ChunkKind::Bitmap as u32,
                    count: 0,
                    data: std::ptr::null(),
                },
            },
        };
        // SAFETY: Checked non-null, one output slot.
        unsafe { *out = described };
        Status::Ok
    })
}

/// # Safety
/// As [`block_lane`]; `scratch` must be `cap` writable bytes when non-null.
unsafe extern "C" fn block_lane_into(
    lanes: *const LanesOpaque,
    lane: usize,
    scratch: *mut std::ffi::c_void,
    cap: usize,
    out: *mut Chunk,
) -> Status {
    guard(|| {
        if lanes.is_null() || out.is_null() {
            return Status::InvalidArgument;
        }
        // SAFETY: Checked non-null; provenance is the caller's contract.
        let handle = unsafe { &*(lanes as *const LanesHandle) };
        if !handle.block_open {
            return Status::BlockState;
        }
        let prefix = handle.lanes.prefix().unwrap_or(0);
        let Some(c) = handle.lane(lane) else {
            // SAFETY: Checked non-null, one output slot.
            unsafe { *out = Chunk::absent(prefix) };
            return Status::Ok;
        };
        // A borrow is preferred even here, so a caller may use only this entry
        // point and still pay nothing when the payload is lendable.
        if let Some(d) = describe(prefix, c) {
            // SAFETY: Checked non-null, one output slot.
            unsafe { *out = d };
            return Status::Ok;
        }
        let Container::Bitmap(b) = c else {
            // `describe` only declines for a bitmap; anything else here is a bug
            // in this file rather than in the caller.
            return Status::Internal;
        };
        let need = yesno_core::BITMAP_WORDS * std::mem::size_of::<u64>();
        // The alignment check is not pedantry. A bitmap is handed to the caller as
        // `u64` words, so scratch that is not 8-byte aligned would have the caller
        // reading a misaligned pointer -- undefined for it, and invisible here.
        let misaligned = !(scratch as usize).is_multiple_of(std::mem::align_of::<u64>());
        if scratch.is_null() || cap < need || misaligned {
            // The needed byte count goes back in `count`, which is what the
            // header promises, so a caller can size its buffer once and retry.
            // SAFETY: Checked non-null, one output slot.
            unsafe {
                *out = Chunk {
                    prefix,
                    kind: ChunkKind::Bitmap as u32,
                    count: need as u32,
                    data: std::ptr::null(),
                }
            };
            return Status::InvalidArgument;
        }
        // SAFETY: `scratch` is non-null, 8-byte aligned and at least `need` bytes
        // by the checks above, so it is a valid `[u64; BITMAP_WORDS]`. It is the
        // caller's buffer and cannot alias the container's payload.
        let dst = unsafe {
            std::slice::from_raw_parts_mut(scratch as *mut u64, yesno_core::BITMAP_WORDS)
        };
        if !b.copy_words_into(dst) {
            return Status::Internal;
        }
        // SAFETY: Checked non-null, one output slot.
        unsafe {
            *out = Chunk {
                prefix,
                kind: ChunkKind::Bitmap as u32,
                count: yesno_core::BITMAP_WORDS as u32,
                data: scratch as *const std::ffi::c_void,
            }
        };
        Status::Ok
    })
}

/// # Safety
/// `lanes` must be null or a live handle.
unsafe extern "C" fn block_release(lanes: *mut LanesOpaque) {
    if lanes.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: Checked non-null; provenance is the caller's contract.
        let handle = unsafe { &mut *(lanes as *mut LanesHandle) };
        handle.block_open = false;
        // Dropped here, not at the next advance. The header promises that every
        // pointer taken from this block is invalid now, and a promise about
        // freed memory that leaves the memory valid is the kind that holds until
        // the day it matters.
        handle.lanes.release_block();
    }));
}
