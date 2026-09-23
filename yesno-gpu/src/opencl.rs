//! An OpenCL device backend.
//!
//! # Why OpenCL and not CUDA
//!
//! Measured, not assumed. The same kernel written both ways on this project's
//! GB10 ran at **0.99x to 1.03x** across every size and batch width tried, and
//! both agreed with a CPU reference on every count -- the device reports
//! itself as `OpenCL 3.0 CUDA`, so NVIDIA's OpenCL rides the same driver and
//! lowers to the same SASS. Details in
//! `LTM/gpu-offload-on-unified-memory.md`.
//!
//! With no performance to trade, the choice falls to reach: this backend also
//! runs on AMD and Intel, where a CUDA one would not.
//!
//! # No device is the normal case
//!
//! [`OpenClBackend::open`] returns `None` when there is no ICD, no platform,
//! no GPU, or a program that will not build, and [`crate::Offload`] then never
//! sees it -- the caller uses its CPU path, which is also the oracle. `opencl3`
//! loads the ICD through `dlopen2` rather than linking it, so this compiles
//! and its tests run on a machine with no OpenCL at all.
//!
//! # The kernel, and the one thing it is shaped around
//!
//! One work group per row; the row is staged in local memory once and every
//! filter is applied to it by a separate work item. That arrangement is the
//! whole measurement: the naive one -- a work item per `( row, filter )` pair
//! reading the row from global memory each time -- measured **1.06x to 1.23x**
//! of the CPU, while staging the row and reusing it measured **6.71x to
//! 11.08x**. The reuse of a row across filters is the entire win, which is
//! also why [`crate::MIN_BATCH_FILTERS`] exists.
//!
//! **The kernel is not yet tuned.** At 64 work items per group it is two warps
//! on an NVIDIA device and the harness measured 22-52 GB/s effective, well
//! under what the hardware does. Sweeping the group size is the first thing to
//! do before quoting a speedup from this.

use std::ffi::c_void;
use std::ptr;
use std::sync::Mutex;

use opencl3::command_queue::CommandQueue;
use opencl3::context::Context;
use opencl3::device::{get_all_devices, Device, CL_DEVICE_TYPE_GPU};
use opencl3::kernel::{ExecuteKernel, Kernel};
use opencl3::memory::{Buffer, CL_MEM_READ_ONLY, CL_MEM_READ_WRITE, CL_MEM_WRITE_ONLY};
use opencl3::program::Program;
use opencl3::types::{cl_uint, cl_ulong, CL_BLOCKING};

use crate::backend::{total_rows, Backend, Job};
use crate::residency::Slot;

const KERNEL: &str = r#"
// One work group per row of the whole batch. `row_base[ g ]` is the word
// offset of global row g inside the slot arena, which is what lets a single
// launch cover chunks scattered across slots.
__kernel void andpop_batch(__global const ulong *slots,
                           __global const ulong *row_base,
                           __global const ulong *filters,
                           __global uint *out,
                           __local ulong *row,
                           int row_words,
                           long total_rows,
                           int nfilters) {
  long g = get_group_id(0);
  int t = get_local_id(0);
  int nt = get_local_size(0);
  __global const ulong *data = slots + row_base[g];
  for (int w = t; w < row_words; w += nt) row[w] = data[w];
  barrier(CLK_LOCAL_MEM_FENCE);
  for (int f = t; f < nfilters; f += nt) {
    uint acc = 0;
    __global const ulong *fl = filters + (long)f * row_words;
    for (int w = 0; w < row_words; ++w) acc += popcount(row[w] & fl[w]);
    out[(long)f * total_rows + g] = acc;
  }
}
"#;

/// Work items per group. See the module header: this is a floor, not a tuned
/// value, and the kernel has measured headroom above it.
const GROUP: usize = 64;

struct Inner {
    queue: CommandQueue,
    kernel: Kernel,
    _program: Program,
    context: Context,
    slots: Buffer<cl_ulong>,
    /// Words valid in each slot, mirroring what the device holds.
    lens: Vec<usize>,
    filters: Buffer<cl_ulong>,
    filters_cap: usize,
    /// Epoch of the filter set currently on the device, and its width. `None`
    /// means nothing is loaded. Keeping this is the difference between
    /// copying the filters once per scan and once per chunk -- measured as
    /// the dominant cost of the whole offload path.
    filters_loaded: Option<(u64, usize)>,
    /// Per-global-row word offsets for the batch currently staged.
    row_base: Buffer<cl_ulong>,
    row_base_cap: usize,
    out: Buffer<cl_uint>,
    out_cap: usize,
}

/// A GPU reached through OpenCL.
pub struct OpenClBackend {
    capacity: usize,
    slot_words: usize,
    name: &'static str,
    device: String,
    inner: Mutex<Inner>,
}

impl OpenClBackend {
    /// Open the first GPU an OpenCL platform offers, or `None`.
    ///
    /// Every failure -- no ICD, no device, a program that will not build -- is
    /// `None` rather than an error. There is nothing a caller could do with
    /// the distinction: the CPU path is exact and always available.
    pub fn open(capacity: usize, slot_words: usize) -> Option<OpenClBackend> {
        assert!(capacity > 0 && slot_words > 0);
        let device_id = *get_all_devices(CL_DEVICE_TYPE_GPU).ok()?.first()?;
        let device = Device::new(device_id);
        let device_name = device.name().unwrap_or_else(|_| "unknown".to_string());
        let context = Context::from_device(&device).ok()?;
        let queue = CommandQueue::create_default_with_properties(&context, 0, 0).ok()?;
        let program = Program::create_and_build_from_source(&context, KERNEL, "").ok()?;
        let kernel = Kernel::create(&program, "andpop_batch").ok()?;

        // SAFETY: `Buffer::create` is unsafe because a non-null `host_ptr`
        // must outlive the buffer and match the flags. Null is passed, so the
        // device allocates and owns the storage and there is nothing to
        // outlive.
        let slots = unsafe {
            Buffer::<cl_ulong>::create(
                &context,
                CL_MEM_READ_ONLY,
                capacity * slot_words,
                ptr::null_mut(),
            )
        }
        .ok()?;
        let filters =
            unsafe { Buffer::<cl_ulong>::create(&context, CL_MEM_READ_ONLY, 1, ptr::null_mut()) }
                .ok()?;
        let out =
            unsafe { Buffer::<cl_uint>::create(&context, CL_MEM_WRITE_ONLY, 1, ptr::null_mut()) }
                .ok()?;
        let row_base =
            unsafe { Buffer::<cl_ulong>::create(&context, CL_MEM_READ_ONLY, 1, ptr::null_mut()) }
                .ok()?;

        Some(OpenClBackend {
            capacity,
            slot_words,
            name: "opencl",
            device: device_name,
            inner: Mutex::new(Inner {
                queue,
                kernel,
                _program: program,
                context,
                slots,
                lens: vec![0; capacity],
                filters,
                filters_cap: 1,
                filters_loaded: None,
                row_base,
                row_base_cap: 1,
                out,
                out_cap: 1,
            }),
        })
    }

    /// The device this opened, for diagnostics. Never parsed.
    pub fn device_name(&self) -> &str {
        &self.device
    }
}

impl Inner {
    /// Grow a scratch buffer if it is too small. Buffers only ever grow, so a
    /// steady-state workload stops reallocating.
    fn ensure(&mut self, filters_words: usize, out_len: usize, rows: usize) -> bool {
        if rows > self.row_base_cap {
            // SAFETY: as elsewhere -- null `host_ptr`, device-owned storage.
            let Ok(b) = (unsafe {
                Buffer::<cl_ulong>::create(&self.context, CL_MEM_READ_ONLY, rows, ptr::null_mut())
            }) else {
                return false;
            };
            self.row_base = b;
            self.row_base_cap = rows;
        }
        self.ensure_inner(filters_words, out_len)
    }

    fn ensure_inner(&mut self, filters_words: usize, out_len: usize) -> bool {
        if filters_words > self.filters_cap {
            // SAFETY: as in `open` -- null `host_ptr`, device-owned storage.
            let Ok(b) = (unsafe {
                Buffer::<cl_ulong>::create(
                    &self.context,
                    CL_MEM_READ_ONLY,
                    filters_words,
                    ptr::null_mut(),
                )
            }) else {
                return false;
            };
            self.filters = b;
            self.filters_cap = filters_words;
            // A new buffer holds nothing, whatever the epoch said.
            self.filters_loaded = None;
        }
        if out_len > self.out_cap {
            // SAFETY: as above.
            let Ok(b) = (unsafe {
                Buffer::<cl_uint>::create(
                    &self.context,
                    CL_MEM_READ_WRITE,
                    out_len,
                    ptr::null_mut(),
                )
            }) else {
                return false;
            };
            self.out = b;
            self.out_cap = out_len;
        }
        true
    }
}

impl Backend for OpenClBackend {
    fn capacity(&self) -> usize {
        self.capacity
    }

    fn slot_words(&self) -> usize {
        self.slot_words
    }

    fn slot_len(&self, slot: Slot) -> usize {
        if slot.0 >= self.capacity {
            return 0;
        }
        self.inner.lock().map(|i| i.lens[slot.0]).unwrap_or(0)
    }

    fn upload(&self, slot: Slot, words: &[u64]) -> bool {
        if slot.0 >= self.capacity || words.is_empty() || words.len() > self.slot_words {
            return false;
        }
        let Ok(mut inner) = self.inner.lock() else {
            return false;
        };
        // **Bytes, not elements.** `enqueue_write_buffer` passes `offset`
        // straight to `clEnqueueWriteBuffer`, which takes a byte offset, while
        // it derives the size from `size_of_val( data )`. Computing this in
        // elements wrote slot 2 a quarter of the way to where the kernel reads
        // it, and the device returned all zeros -- caught by the differential
        // against the host backend, and invisible to any test that compared
        // the device only against itself.
        //
        // `slot_base` below stays in *elements*, because the kernel does
        // pointer arithmetic on `ulong *`. The two units are genuinely
        // different and both are right.
        let offset = slot.0 * self.slot_words * std::mem::size_of::<u64>();
        // Split so the queue and the buffer are borrowed from disjoint fields.
        let Inner { queue, slots, .. } = &mut *inner;
        // SAFETY: `enqueue_write_buffer` is unsafe because it writes device
        // memory from a host slice that must stay valid until the transfer
        // completes. `CL_BLOCKING` makes the call synchronous, so `words`
        // outlives it by construction, and the region written is
        // `offset .. offset + words.len()`, checked above to lie inside the
        // slot and inside the allocation of `capacity * slot_words`.
        let ok =
            unsafe { queue.enqueue_write_buffer(slots, CL_BLOCKING, offset, words, &[]) }.is_ok();
        if ok {
            inner.lens[slot.0] = words.len();
        } else {
            // A slot whose device contents are unknown must not be reported as
            // holding anything: `Offload` would later serve a hit from it.
            inner.lens[slot.0] = 0;
        }
        ok
    }

    fn run_batch(
        &self,
        jobs: &[Job],
        row_words: usize,
        filters: &[&[u64]],
        filters_epoch: u64,
        out: &mut [u32],
    ) -> bool {
        if row_words == 0 || jobs.is_empty() || filters.iter().any(|f| f.len() != row_words) {
            return false;
        }
        let total = total_rows(jobs);
        if out.len() != filters.len() * total {
            return false;
        }
        let Ok(mut inner) = self.inner.lock() else {
            return false;
        };

        // Validate every job before touching the device, so a bad one cannot
        // leave the caller holding half an answer it believes is whole.
        for job in jobs {
            if job.slot.0 >= self.capacity || inner.lens[job.slot.0] < job.rows * row_words {
                return false;
            }
        }

        // One entry per row of the batch: the word offset of that row inside
        // the slot arena. This is what makes a single launch able to cover
        // chunks sitting in unrelated slots.
        let mut bases = Vec::with_capacity(total);
        for job in jobs {
            let base = job.slot.0 * self.slot_words;
            for r in 0..job.rows {
                bases.push((base + r * row_words) as cl_ulong);
            }
        }

        if !inner.ensure(filters.len() * row_words, out.len(), total) {
            return false;
        }

        // Filters are the constant half of a scan and are kept device-side
        // while the epoch is unchanged; re-sending them per batch was
        // measured as 2.45 ms of a 12.22 ms scan back when batches were one
        // chunk each.
        if inner.filters_loaded != Some((filters_epoch, filters.len() * row_words)) {
            let flat: Vec<u64> = filters.iter().flat_map(|f| f.iter().copied()).collect();
            {
                let Inner { queue, filters, .. } = &mut *inner;
                // SAFETY: blocking write of a host slice that outlives the
                // call, into a buffer `ensure` just sized to at least
                // `flat.len()`.
                if unsafe { queue.enqueue_write_buffer(filters, CL_BLOCKING, 0, &flat, &[]) }
                    .is_err()
                {
                    return false;
                }
            }
            inner.filters_loaded = Some((filters_epoch, flat.len()));
        }

        {
            let Inner {
                queue, row_base, ..
            } = &mut *inner;
            // SAFETY: blocking write of `bases`, which outlives the call, into
            // a buffer just sized to at least `total`.
            if unsafe { queue.enqueue_write_buffer(row_base, CL_BLOCKING, 0, &bases, &[]) }.is_err()
            {
                return false;
            }
        }

        let rw = row_words as i32;
        let trows = total as i64;
        let nf = filters.len() as i32;
        // SAFETY: the arguments match the kernel's declared signature in order
        // and type -- three buffers, a buffer, a local allocation of
        // `row_words` `ulong`s which is what the kernel indexes, and three
        // scalars. The global size is a whole multiple of the group size, as
        // `enqueue_nd_range` requires. Any mismatch is caught by the
        // differential against the host backend rather than by inspection.
        let run = unsafe {
            ExecuteKernel::new(&inner.kernel)
                .set_arg(&inner.slots)
                .set_arg(&inner.row_base)
                .set_arg(&inner.filters)
                .set_arg(&inner.out)
                .set_arg_local_buffer(row_words * std::mem::size_of::<cl_ulong>())
                .set_arg(&rw)
                .set_arg(&trows)
                .set_arg(&nf)
                .set_global_work_size(total * GROUP)
                .set_local_work_size(GROUP)
                .enqueue_nd_range(&inner.queue)
        };
        if run.is_err() || inner.queue.finish().is_err() {
            return false;
        }

        // SAFETY: blocking read into `out`, which outlives the call, of
        // exactly `out.len()` elements from a buffer `ensure` sized to at
        // least that.
        unsafe {
            inner
                .queue
                .enqueue_read_buffer(&inner.out, CL_BLOCKING, 0, out, &[])
        }
        .is_ok()
    }

    fn name(&self) -> &'static str {
        self.name
    }
}

/// Unused import guard: `c_void` is named by the `Buffer::create` signature.
const _: Option<*mut c_void> = None;
