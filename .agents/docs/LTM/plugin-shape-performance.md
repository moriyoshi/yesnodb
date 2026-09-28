# In-Process Plugin ABI against the Out-of-Process IPC Channel

Measured 2026-09-28 on the same scan through both shapes. Two harness defects were
found and fixed before any of these numbers; both are recorded below, because each
produced a confident wrong conclusion first.

## The arms

- **A** -- the in-process C table: `block_advance`, then `block_lane` per lane,
  borrowed pointers, no copy. What a `dlopen`'d plugin pays.
- **B** -- the IPC protocol logic with **no socket**. Isolates **the copy** into the
  shared arena.
- **C** -- `B` through a real `UnixStream` pair, one block per round trip.
- **D** -- `C` with `BlockAdvanceMany`, `k` blocks per round trip, previous batch
  released implicitly. Swept over `k`.

Every arm asserts it consumed identical work -- blocks, present lanes, summed
element counts -- so an arm cannot be fast by skipping something. Contexts are built
once and reused across reps, with one untimed warm scan, and only the scan loop is
timed.

## Results

Three runs, 9 / 25 / 25 reps, medians. `vs A` is the ratio to the in-process table.

```text
                        8 keys, arrays    8 keys, mixed    64 keys, mixed
  B  copy only            0.95 - 1.00x      1.62 - 2.53x      2.07 - 2.78x
  C  socket, k = 1        4.85 - 5.40x      6.73 - 7.55x      4.01 - 7.07x
  D  socket, best k       1.04 - 1.37x      1.95 - 2.60x      3.28 - 3.86x
```

"mixed" rotates arrays, bitmaps and runs on `( key + chunk ) % 3`, so the 8 KiB
bitmap lane -- the worst case the arena is sized for -- is exercised rather than
assumed.

## What is stable across all three runs

**Not batching is the whole problem.** `k = 1` costs 4.9x to 7.6x. Any `k >= 2`
lands far below it, and the best `k` is within reach of `B` in every shape.

**After batching, the residual is the copy, not the transport.** The best `D` sits
close to `B` everywhere: 1.04x against 0.95x on arrays, 2.60x against 1.62x on
8-lane mixed, 3.28x against 2.07x at 64 lanes. Whatever remains above `B` is what a
faster notification could win, and it is the smaller half.

**Which `k` is best is not resolved.** `k` of 4, 8, 16 and 32 each won some run, and
the differences between them are inside the run-to-run spread. `k >= 4` is enough to
say from this data; anything more specific would be reading one run's ordering as a
property.

## What the shared memory is actually buying

Every IPC arm above puts payloads in the arena and control frames on the socket, so
none of them priced the alternative -- payloads inlined in the frames. Measured
separately, shipping the same byte volume through a `UnixStream` pair in the same
message sizes, with no framing and no encode, which makes it a **lower bound** on
inlining rather than an estimate of it:

```text
  payload per scan                     msg size   us / scan   GiB/s
  8 keys arrays,   40 KiB                 1 KiB        33.2    1.23
  8 keys mixed,   730 KiB                64 KiB       151.4    4.94
  64 keys mixed,  2.9 MiB               512 KiB       475.2    6.41
```

Against the best batched scans, inlining would add at least 33 us to ~180, 151 us to
~275, and 475 us to ~1900. Real, and less than the headline difference between the
shapes -- a `UnixStream` moves 1 to 6 GiB/s, so **the arena is not primarily a
bandwidth win**.

**The structural argument is much stronger than the microseconds.** A single block's
worst-case payload is `lanes * LANE_BYTES`, so at `MAX_PAYLOAD` = 64 KiB an inlined
frame holds **eight lanes and one block** -- before any batching at all:

```text
   8 lanes    64 KiB   fits
  16 lanes   128 KiB   does not
  64 lanes   512 KiB   does not
 256 lanes  2048 KiB   does not
```

Inlining therefore forces either a frame cap raised by orders of magnitude -- the
allocation-attack surface the cap exists to bound -- or blocks small enough to fit,
which is exactly the configuration that costs 4.9x to 7.6x in round trips. **The
arena is what decouples batch size from frame size**, and batch size is what removed
the transport cost. Without it the two are coupled and the thing that fixed the
performance is unavailable.

## The answer to "which synchronization primitive"

**The socket, batched. Do not add a second primitive.**

A futex on a word in the arena works across containers -- `FUTEX_WAIT` / `FUTEX_WAKE`
without `FUTEX_PRIVATE_FLAG` keys on the page's inode and offset -- and an `eventfd`
passed with `SCM_RIGHTS` is cheaper per wake than a socket round trip and, unlike a
futex, is pollable alongside the control socket. Either would have been worth
building against arm `C`.

Against arm `D` neither is, because **batching already removed the term they
optimize**. A round trip costs what it costs; doing one per `k` blocks instead of
two per block is a factor of `2k` fewer of them, and past `k = 4` the remaining
transport is smaller than the copy that no notification primitive touches.

Two things do not change with the primitive and are worth stating because a
shared-memory design tends to lose them:

- **The socket must stay open regardless.** Its closure is what lets yesnod drop the
  snapshot, which is this shape's entire reclamation story. A futex gives no death
  signal.
- **Memory ordering currently comes from the socket.** The server writes the arena,
  then writes the socket; the peer reads the socket, then the arena. That syscall
  pair is the happens-before. Anything replacing it needs explicit `Release` and
  `Acquire`, and a design that swapped it for a plain flag read would be fast and
  wrong in a way arm `B` cannot catch, because `B` is single-threaded and has
  ordering for free.

## Two harness defects, and what each one claimed

**Arena construction inside the timed region.** The first batched result said
batching made things *worse*, up to 13.5x at 64 lanes. Arm `D`'s arena is `k` times
arm `C`'s, and a `memfd` faults its pages on first touch: 1792 extra pages at 2.3 us
each accounts for essentially the whole difference. The harness was measuring
`memfd` page faults and reporting them as the cost of batching.

**Setup hoisted, but the arena still cold.** Moving setup out of the timer was not
enough, because the *first* scan still faulted every page it wrote. A real peer
opens one connection and scans many times, so the fix was to reuse contexts across
reps with one untimed warm scan. Only then did batching show a win.

Both defects shared a shape: **the bigger configuration paid a one-time cost that
the harness charged per iteration.** Neither was visible in the output; both were
found by asking whether a surprising result had a mundane explanation, and in both
cases the arithmetic accounted for the whole surprise.

A third conclusion was retracted rather than fixed. The 9-rep run showed an interior
optimum in `k` and I explained it with cache footprint, which is a plausible story
and did not reproduce in either 25-rep run. It is recorded here as not supported.

## Making the channel cross-platform

The protocol is already portable: `yesno_plugin::ipc` is bytes, makes no system
calls, and has no platform types. **Only the transport is platform-specific**, and it
has exactly three jobs -- a bidirectional byte stream, a way to share a memory
region, and a liveness signal. The first and third exist everywhere a socket does.
The second is the only hard part, and it is now optional.

**The portable path is no shared region at all.** `Session::new_inline` serves the
same scan with payloads inside the response frames, advertised by `arena_bytes = 0`
in the greeting so a peer needs no second negotiation field.

**A protocol property is not a server property, and this document said so too
loosely at first** ( corrected 2026-09-28, on a consumer's reading ). For a day
`Session::new_inline` existed and was tested while `serve_one` still called
`Arena::new` unconditionally and refused the connection when it failed -- so a
non-Linux host would not have fallen back, it would simply not have served. The
running server now falls back, with `plugin.channel_inline` to choose it
deliberately, and a test drives the inline path over a real socket. The failure is
not only a non-Linux one either: `memfd_create` needs a descriptor and the region
needs backing memory, so fd exhaustion and `ENOSPC` reach the same branch on
Linux. It costs a second copy
and, more importantly, couples the batch to the frame size: at 64 lanes an inline
batch is **two** blocks against the arena's sixty-four, because one block is 512 KiB
against `MAX_INLINE_PAYLOAD` of 1 MiB. Since batch size is what removed the transport
cost, that coupling is the real price of having no shared memory -- not the extra
copy, which the socket-throughput figures above put at 33 to 475 us.

**Per-platform regions, if the inline path is not enough:**

| Platform | Region | Notes |
|---|---|---|
| Linux | `memfd_create` + seals | Built. Sealed against resize; travels by `SCM_RIGHTS`. |
| FreeBSD | `shm_open( SHM_ANON )` | Anonymous and fd-based, so the closest equivalent. |
| macOS, other POSIX | `shm_open` then immediate `shm_unlink` | Named for a moment; no sealing; short name limit. |
| Windows | `CreateFileMapping` + `DuplicateHandle` | No `SCM_RIGHTS`, so the handshake differs: the host needs the peer's process handle. |

None of the last three is written, deliberately. This tree can compile and test only
the first, and a backend that cannot be run is a claim rather than a capability --
the same reason `stats.rs` was deleted. The seam is what matters: `Session` takes an
`Option<Arena>`, so adding a platform means adding a region type and nothing else.

## The caveat that outweighs every number above

**These arms touch one byte per lane.** They measure the cost of *reaching* the
data, deliberately, because that is where the shapes differ. A real scorer reads
whole payloads and does arithmetic, and that work is identical in both.

The 8-lane mixed shape moves 109 781 elements per scan. At one nanosecond per
element a scorer spends 110 us against arm A's 95 to 150 us of total access --
already comparable, and dominant at any realistic rate. **So the whole-query ratio
will be much closer to 1 than even the best figures here**, and quoting these to
someone choosing between the shapes would hand them an access-overhead number
dressed as a query number. The whole-query mixed-container benchmark is still the
one that decides, and it needs the consumer's scorer.

## Not measured

A container boundary -- the arms use a socket pair in one process, which shares a
kernel with a cross-container socket and should cost the same, but that is an
argument rather than a measurement. Also absent: concurrent peers, a cold page
cache, and the `memfd` handshake, which happens once per connection.

The machine was noisy throughout: arm A varied 95 to 150 us for identical work
across runs. Between-arm comparisons within one run are sound; comparisons of
absolute numbers across runs are not.
