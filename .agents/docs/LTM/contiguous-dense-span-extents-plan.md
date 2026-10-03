# Contiguous dense span extents

**Status: DONE 2026-09-30 for the dense case.** `CLASS_SIZES[ 10 ]` is 8192 and the extent
trailer lives in a table at the tail of the slab body, so consecutive bitmap payloads are
adjacent and OS-page aligned. The adjacency harness flipped from **0 of 63** adjacent pairs
to **63 of 63**. 254 slots per slab where an inline trailer allowed 253.

**Both deferred decisions turned out to be moot, which is the main thing to carry away.**
The phases below call for a `DbOptions` switch and a minimum-run-length admission rule;
neither exists, because **placement A needs no admission rule**. Every standalone bitmap
gets a slot its own size, and consecutive ones land adjacent because a checkpoint
bump-allocates within a contiguous slab run -- the locality policy `alloc.rs` already had.
A threshold and a mode flag were artifacts of the packed-page design, not of the goal.
`PACKED_LARGE_CLASS` is deleted and `MAX_VERIFIED_SPAN` is back to 8192.

## 2026-10-02: measured cost of lending on the Flight bitvector path

**The direct batch-building step improves, but a loopback Flight request has no stable
end-to-end speedup at these sizes.** The earlier 9x result ( 57.7 versus 529 GiB/s )
isolated copying bytes into an Arrow batch. This measurement includes the indexed
`dense_span` walk and trailer checks in the local comparison, then includes the server,
Arrow IPC, HTTP/2 loopback transport and client Arrow decoding in the wire comparison.

Construction: scratch crate and raw samples at
`.agents-workspace/tmp/flight-bitvector-bench/`, run in release mode against commit
`7276ddf` ( containing the fix in `644ca48` ) on this 20-core aarch64 host. The
scratch crate uses the shared workspace `target`. **Version correction:** the initial
timings in the two tables below resolved `arrow-array`, Flight and IPC at 59.2.0 but
`arrow-buffer`, data and schema at 59.3.0, although the repository lockfile has
59.2.0 for all of them. The scratch manifest now pins every Arrow crate at 59.2.0;
the fully pinned socket A/B below reproduces the 40 ms finding. One checkpointed
key holds 1024 or 2048 consecutive 8 KiB
bitmap chunks ( 8 or 16 MiB ), each with alternating bits (`0x55`, cardinality
32 768 per chunk). Every run uses one Flight server and client on `127.0.0.1`, one
connection, warm page cache, three warmup requests per arm, and rotated arm order.
Ticket planning and database setup are outside each timed request. The client times
`do_get` through the last decoded `BinaryArray`, checks every value's length and its
edge bytes, and consumes a middle byte through `black_box`.

Arms: `bare_borrow` is a bare-key bitvector ticket; `identity_staged` uses the
expression `Key(7)`, which returns the **same bytes** but disables the borrow; and
`filtered_staged` intersects the key with the middle half of its range. The last arm
tests the required fallback but is not a same-work speed comparison: it evaluates a
different expression. `identity_staged` also pays expression evaluation, so its wire
time is not a pure measure of the gather alone. The direct local comparison uses one
preloaded set on both sides and constructs `BinaryArray` batches with 64 chunks per
batch; the borrowing side performs the real `dense_span` scan and checksum verification.

| Payload | Local `load_set` | Local borrowed batches | Local staged batches |
| --- | ---: | ---: | ---: |
| 8 MiB | 0.201 ms | **0.183 ms** | 0.235 ms |
| 16 MiB | 0.411 ms | **0.363 ms** | 0.608 ms |

These are medians of 25 interleaved, warmed local samples. Borrowing saves 0.052 ms
at 8 MiB ( 1.28x batch-building speedup ) and 0.245 ms at 16 MiB ( 1.67x ). The
`load_set` cost precedes either batch builder in the current Flight implementation.

Flight wall time is bimodal: a fast cluster below 20 ms and a second cluster near
45 to 53 ms, present in **every** arm. A single median can land in either cluster
depending on how often the long delay occurs, so the table reports the median of
the fast cluster and the number of slow requests separately. The threshold separates
two observed clusters; it is not a trimmed benchmark score.

| Payload and run | Bare borrow, fast median ( fast / total ) | Identity staged, fast median ( fast / total ) | Filtered staged, fast median ( fast / total ) |
| --- | ---: | ---: | ---: |
| 8 MiB, 24 requests per arm | 4.682 ms ( 16 / 24 ) | 4.821 ms ( 12 / 24 ) | 5.729 ms ( 19 / 24 ) |
| 16 MiB, 24 requests per arm | 9.421 ms ( 22 / 24 ) | 9.041 ms ( 20 / 24 ) | 9.635 ms ( 19 / 24 ) |
| 16 MiB repeat, 15 per arm | 10.264 ms ( 13 / 15 ) | 11.008 ms ( 13 / 15 ) | 11.138 ms ( 9 / 15 ) |

At 16 MiB, the bare-key result changes from 0.380 ms slower to 0.744 ms faster
than the staged identity control across the two runs. **No reproducible Flight
speedup is established**, although the local batch-building saving is real. The
roughly 40 ms extra delay is visible in all three arms and must not be credited
to either implementation. Its cause was subsequently identified below. These
measurements are loopback, warm-cache, without TLS, and do not include shifou's
encode/decode or GPU upload. They therefore say nothing about a remote network
or a cold cache.

### 2026-10-02: the 40 ms tail is a TCP socket setting on custom incoming streams

Phase timing found the missing interval **at the tail**, not in the indexed read:
of 55 requests with a 35-75 ms total in three balanced runs, 43 waited about 41 ms
before the final decoded batch and 12 waited after the last batch for end-of-stream.
Headers and earlier batches arrived normally. It occurred in bare, identity and
filtered arms.

The scratch server uses `Server::serve_with_incoming` with a
`TcpListenerStream`. Its accepted socket reported `TCP_NODELAY=false`. Tonic 0.14.6
defaults its own server `tcp_nodelay` option to true, but documents in its local
`transport/server/mod.rs` that **the option is ignored for `serve_with_incoming`**;
the caller owns the incoming socket settings. That is also the shape used by
`yesno-server`'s plaintext and TLS listeners; neither sets `TCP_NODELAY` on an
accepted socket. A small last write held by Nagle until the peer's delayed ACK is
the mechanism consistent with both the location and the approximately 41 ms gap.

To test causality, the scratch incoming stream set `TCP_NODELAY` on each accepted
socket while leaving the database, Flight service, client, payload and arm order
unchanged. With **all Arrow crates pinned at 59.2.0**, one paired 8 MiB experiment
issued 100 requests per arm, 300 per socket setting:

| Accepted server socket | 35-75 ms requests | At least 150 ms requests | Median total |
| --- | ---: | ---: | ---: |
| `TCP_NODELAY=false` | **97 / 300** | 0 / 300 | 5.713 ms |
| `TCP_NODELAY=true` | **0 / 300** | 1 / 300 | 5.252 ms |

Of the 97 slow default-socket requests, 91 waited before the final batch and six
waited for end-of-stream after it. Three earlier paired runs with the mixed Arrow
resolution agreed on the mechanism: 55 of 780 default-socket requests had the
40 ms mode, against zero of 780 with NODELAY. Thus the original 40 ms mode
disappears under the socket intervention. **Simply
enabling NODELAY is not yet a latency fix**: the true arm exposed a different
roughly 200 ms mode, sometimes longer. In one paired 300-request-per-setting run,
host-wide TCP counters rose by 368 retransmitted segments and 20 timeouts with
NODELAY, against 13 and 0 with the default; this host reports a 200 000 us TCP
minimum retransmission timeout. Those counters are system-wide and cannot assign
every event to this connection, but their change and the phase gaps indicate a
transport retransmission problem in the faster-sending arm. No production socket
setting was changed. Raw phase samples and the controlled socket A/B are under
`.agents-workspace/tmp/flight-bitvector-bench/`.

**B is closed as of 2026-10-01, and the first attempt to close it used the wrong argument.**
That attempt measured Arrow *batch size* -- finding 64 KiB to 2 MiB a plateau and one 128 MiB
batch 30 to 50 times worse -- and concluded that B's larger regions were therefore useless.
**That conflated two independent things**, as the maintainer pointed out: batch size is how
much goes in one `RecordBatch`, while B is about how long a contiguous *region* exists on
disk. Sixty-four-chunk batches can be cut from a 128 MiB region as easily as from a 2 MiB
one, each borrowing a slice, so the batch-size curve says nothing about B.

**What B actually buys is that more windows qualify for a borrow**, and the correct reason it
is now uninteresting is a different one. `unstable_arrow::dense_span` reports **how far it
could lend** rather than demanding the whole window, so a slab boundary truncates one batch
slightly and the caller continues from there. A longer region would only make batch lengths
more uniform. Before that contract existed the boundary was far from harmless -- a body holds
254 chunks, a fixed 64-chunk window straddles roughly every fourth time, and the Flight arm
stopped borrowing at the first straddle and gathered **every later slab**, each of which is
contiguous within itself. Pinned by
`a_key_spanning_a_slab_boundary_is_lent_in_full`, which asserts both that every chunk is lent
and that at least one call came back short, so it cannot pass without crossing a boundary.

So: B is not worth its cost -- a payload-only slab is not self-describing, the hazard
`slabmeta` exists to prevent -- but the reason is partial coverage, not batch size.

Read the phases below as the record of how this was argued, not as a description of the
code. Phase 1's large packed page is gone; Phases 2 to 4 as written no longer apply.

**Original status: planned 2026-09-30.** Product-level requirement: a consumer storing a huge
dense set ( a KV cache ) cannot receive it without a gather, which makes zero-copy
transmission impossible and the product materially less attractive for that shape.

## The measurement that motivates it

A 64-chunk half-dense key, checkpointed so every payload is mmap-backed: all 64
chunks are individually lendable through `unstable_arrow::bitmap_words`, and **0 of 63
consecutive pairs are adjacent**. Longest contiguous run: one chunk. Harness at
`.agents-workspace/tmp/adjacency`, which prints the verdict directly and will flip
when this lands.

Two mechanisms, both deliberate and both documented where they live:

* A **standalone extent** carries `EXT_TRAILER_BYTES` ( 8 ) after its payload, and the
  size classes are sized so "a power-of-two payload plus its 8-byte trailer" fits. So
  even back-to-back slots leave payloads at least 8 bytes apart.
* **Packed pages** carry no per-payload trailer and *are* contiguous, but they are
  4 KiB with a 40-byte header -- "4096 - 40 = 4056, the number the PACK\_MAX analysis
  is built on" -- so an 8 192-byte bitmap never fits one.

Dense bitmap payloads are therefore **structurally** guaranteed non-contiguous, not
merely unlucky.

## Why this is a store-layout change and not a roaring format extension

This distinction is the whole reason the work is affordable, and the first answer to
the question got it wrong by citing `CHUNK_CARD` as an objection.

* Each container's bytes stay **byte-identical** to roaring's. The M0 differential
  gate asserts byte-level identity of serialized *set* output; how extents group
  payloads is invisible to it.
* `.roaring` import stays `O( container count )`, and can write the contiguous region
  directly.
* `CHUNK_CARD`'s rationale -- "fixed by the `u16` container value width, and by the
  resulting 8 KiB bitmap being L1-resident" -- constrains **container width**, which
  does not change. A span is a grouping of 8 KiB containers, not a wider container.

## Why the index needs no change, and a correction to this plan's first version

`ChunkRef` is a packed `u64` whose `[ 0:40 )` field is **`cell`: a byte offset in the
shard address space**, not an extent identifier. A member of a contiguous region
therefore addresses its payload arithmetically and the 8 bytes per chunk stay 8 bytes.

**The first version of this plan then proposed marking members with the `[ 59:60 ) enc`
bit, and that was wrong on two counts.** `ChunkRef::validate` *refuses* `enc = 1` on a
non-inline reference with `UnsupportedEncoding`, so setting it would make every reader
reject the file rather than understand it; and the bit's documented meaning is "an
alternative array encoding", so using it for this would overload a field that already
means something else. The reserved bits `[ 60:64 )` are likewise refused when non-zero,
deliberately -- "a reader that ignores them cannot tell a future format from a valid
file".

**The mechanism that already exists is the slab class.** `slab_of( cell )` resolves an
address to a slab by arithmetic, a slab records its `class: u8`, and `PACKED_CLASS = 0`
is the precedent: a reader learns "this is a packed page, its integrity comes from a
page header rather than a trailer" from the **class**, never from a bit in the
reference. Slab classes are persisted -- `store::slabmeta` landed 2026-09-13, and
`Allocator::restore` falls back to `Opaque` only when its decode fails -- so a new
class survives reopen.

So: **no `ChunkRef` change, no bit overloading, and no file a current reader must
refuse.**

## Proposed layout, corrected twice: a larger packed page, not a new mechanism

**Payload size must be arbitrary**, and that requirement does not cost adjacency. It
costs only the uniform-stride reading, and those are different properties that the
first two versions of this plan ran together:

* **Adjacency** -- no gaps between payloads -- is all that zero-copy needs for the
  `Containers` wire. An Arrow `BinaryArray` is offsets plus **one** contiguous values
  buffer, so if payloads are adjacent the offsets can point into the lent mapping and
  nothing is copied. The offsets are already in hand: `ChunkRef::cell` is a byte offset
  and each payload's length follows from its kind ( array `2 * card`, bitmap 8192, run
  self-describing from its leading interval count ). **Arbitrary sizes are fine here.**
* **Uniform stride** -- every member exactly 8192 bytes for a full chunk -- is needed
  only to read a region *as one bitvector*, because bit `i` must be ordinal `base + i`.
  That becomes an **opportunistic fast path**, detected when a run happens to be all
  full bitmaps, rather than the mechanism.

**And the mechanism already exists, at the wrong size.** `PackedPageBuilder::new(
page_size )` takes the size as a **parameter**, `capacity( page_size ) = page_size -
HEADER`, payloads are appended back-to-back in ascending `ChunkKey` order -- "the order
the checkpointer already writes in, which is also what makes the header's `[ first,
last ]` range a valid index-scan bound" -- and a packed page carries **no per-payload
trailer**. Adjacency for arbitrary sizes is what packed pages have always done.

Only two conditions keep dense data out, both at the checkpointer's admission test:

```text
if payload.len() <= PACK_MAX && c.kind() != ContainerKind::Bitmap
```

`PACK_MAX` is 2028, which is `capacity( 4096 ) / 2`: packing is admitted only when at
least two payloads fit one page, so that it always saves an extent. The `kind` test is
**redundant today** -- a bitmap payload is always 8192 and already fails the size test
-- so it documents intent rather than excluding anything the size test admits.

So the work is a **larger page class** plus a relaxed admission rule for a run of
consecutive chunks of one key, and *no structural change to the packed-page format*.
At a 2 MiB page, `capacity` is 2 097 112 bytes: 255 full bitmaps, or any mix of
arbitrary payloads, contiguous. No per-chunk trailer, no `ChunkRef` change, no new
integrity scheme -- the page header's CRC already covers the body, which is exactly why
packed pages need no trailers.

## Superseded direction, 2026-09-30: move the trailer out of the slot

**The better answer is not a bigger page, it is a smaller slot.** Raised by the
maintainer after Phase 1 landed, and the arithmetic supports it over everything above.

The trailer sits *after* the payload and the ladder rounds the slot to
`round_up_64( payload + 8 )`, which is the single cause of **both** problems: slot 8256
for an 8192 payload is why consecutive dense payloads are 64 bytes apart, and
`8256 % 4096 == 64` is why each one spans three OS pages instead of two. Move the
trailer into a per-slab table and the slot becomes exactly 8192.

| | today, slot 8256 | trailer out of line, slot 8192 |
| --- | --- | --- |
| slots per slab | 253 | **255** |
| stride `% 4096` | 64 -- neither adjacent nor aligned | **0 -- both** |
| writes per 255-chunk slab | 510 | **256** |
| CRC granularity | per chunk | **per chunk, unchanged** |

There is room: the `SLAB_META` region is 8 192 bytes and uses about 64 of them -- 32
fixed fields plus a 255-bit occupancy bitmap -- so a 255-entry table of 8-byte trailers
needs 2 040 of the 8 128 free.

**Three properties this has that the large packed page does not.**

* **It halves the writes.** The checkpointer issues a *separate* 8-byte `write_extent`
  per standalone chunk for its trailer. Out of line they ride the meta-page write that
  already happens for occupancy: 510 writes become 256 for a full slab.
* **It keeps per-chunk checksums.** `PACKED_LARGE_CLASS` bought adjacency by putting
  eight chunks under one CRC, which widened the torn-write radius and forced
  `MAX_VERIFIED_SPAN` up eightfold -- a write-path cost on every write in the shard.
  This needs none of that.
* **It gains space rather than spending it.** Padding a page for alignment would cost
  4 056 bytes per page, 5.8%; this adds two slots per slab.

**So `PACKED_LARGE_CLASS` is likely unnecessary** for uniform payloads, which is the
shape that motivated the work. Phase 1 is not wasted -- `is_packed_class`, the
`MAX_CLASSES` derivation and the class-aware page-base arithmetic were latent defects in
the **existing** 4 KiB packed path -- but the class itself should be expected to come
back out. A large page may still earn its place for runs of *mixed* arbitrary payload
sizes, where a uniform stride is impossible by construction; that is a separate question
from the dense one.

**The cost, and it is the thing to design.** A torn meta page affects up to 255 trailers'
verifiability rather than one. The `ckey_tag` half is re-derivable from the index, which
says which key owns which cell; the `crc32c` half is not, and recomputing it from the
payload would be circular. The region is 8 192 bytes, exactly two OS pages, so A/B'ing
it is natural and is what the superblock already does.

## Two placements for the trailer, and which one to do first

Raised by the maintainer: the eventual shape is a combination of **A** trailers in the
slab's own metadata with contiguous payloads in that slab, and **B** trailers out of
place entirely with payload-only slabs. They are not alternatives so much as two points
on one axis, and the axis is **where contiguity stops**.

`SLAB_META` sits *before* the body -- a slot is at
`slab_id * SLAB_SIZE + SLAB_META + slot * class_size` -- so trailers placed in it do not
interrupt the body, but the region itself still separates one body from the next.

| | max contiguous run | slabs self-describing | metadata writes |
| --- | --- | --- | --- |
| **A** trailers in `SLAB_META` | 2 088 960 B = **255 payloads, 1.99 MiB** | yes | rides the occupancy write already made |
| **B** payload-only slabs | `k * 2 MiB`; 64 slabs is **128 MiB, 16 384 payloads** | **no** | a side slab per run |

**A is the cheap nine-tenths and should come first.** It delivers the whole goal --
adjacency, hence a region Arrow can reference without a gather -- because **2 MiB is
already 16 777 216 boolean rows**, an ordinary batch. Arrow's constraint is one
contiguous values buffer **per batch**, not per result, so B's larger regions buy fewer
and bigger batches rather than the capability itself. That is a throughput question and
needs a measurement before it is worth the cost below.

**B's cost is that a payload-only slab is not self-describing, and this tree has already
been bitten by exactly that.** `slabmeta` exists because a reopened shard "could not tell
which slots in an existing slab were live -- or even what size class the slab held", and
the consequence was that "the first allocation after a reopen claimed slab 0 and wrote
over the extents already in it". B reintroduces that shape for payload slabs: their class
and occupancy live somewhere else. It is recoverable -- `fsck`'s rebuild re-derives
occupancy from the index, which is the real source of truth -- but the side slab becomes a
structure whose loss costs many slabs' worth of interpretation, and it wants A/B'ing at
minimum.

So: **A now, B behind a measurement**, and the admission rule that chooses between them
per run is the decision the maintainer deferred. What that measurement has to answer is
narrow -- whether one 128 MiB batch beats 64 of 2 MiB for a consumer -- and it can be
answered with a throwaway writer before any of this is committed to.

## What the alignment measurement actually said, after three attempts at it

**Conclusion: payload misalignment costs nothing measurable on this kernel, and the
reason is that the mapping unit is not 4 KiB.** The page cache uses large folios, so one
minor fault maps up to 2 MiB and whether a payload spans two 4 KiB pages or three is
invisible.

Evidence. 4 096 payloads of 8 KiB read in a fixed random order out of a freshly mapped
file, nine interleaved passes per arm:

| arm | OS pages spanned | min | median | max | faults |
| --- | --- | --- | --- | --- | --- |
| class-10 slot, stride 8256 | 2.98 | **1.93 ms** | 2.17 ms | 7.52 ms | 21 |
| inside a packed page, +40 | 3.00 | 2.07 ms | 2.12 ms | 2.24 ms | 17 |
| padded and page-aligned | 2.00 | 2.04 ms | 2.15 ms | 2.30 ms | 17 |

Medians of 2.17, 2.12 and 2.15 are indistinguishable, and the **misaligned** arm has the
lowest minimum. 17 faults against 16 predicted for a 32 MiB region at 2 MiB granularity
is the arithmetic that explains it; `hpage_pmd_size` is 2 097 152 on this host.

**Three mistakes on the way, all mine, and each would have produced a confident wrong
answer.**

* The first run measured each arm **once** and the write-up called a 0.3 ms gap "within
  noise" -- an assertion, not a finding, and 0.3 ms on 2.2 ms is 13.6%. Nine interleaved
  passes show it *was* noise, which is the same claim now supported rather than assumed.
* The gap was then attributed to fault-around, and `MADV_RANDOM` was added to defeat it.
  Wrong mechanism: the granularity is folio size, not the fault-around window, and the
  counts barely moved.
* Removing the `println!` that consumed the checksum let LLVM delete the read loop
  entirely, and every arm reported 0.00 ms and **zero faults**. An impossible result is
  the good case; `black_box` fixed it.

**The alignment that gates zero-copy is 8-byte, and the format already satisfies it** --
`8256 % 8 == 0`, and a 40-byte header is 8-aligned -- so `bitmap_words` and `bytemuck`'s
`u64` casts work today.

**Not measured, and any of these would overturn it**: a cold cache, where actual disk I/O
and readahead granularity replace folio mapping; a working set beyond RAM; a kernel or
filesystem without large-folio page cache, where the 4 KiB result would be the one that
matters; and the AVX2 and NEON bitmap arms, where 64-byte alignment may matter and a
40-byte page-header offset breaks it.

**This does not weaken the case for moving the trailer out of the slot.** Alignment was
only ever a bonus there. Adjacency, 510 writes becoming 256, two extra slots per slab and
per-chunk checksums preserved are the reasons, and none of them depends on this result.
Harness at `.agents-workspace/tmp/alignment`.

## The three costs, and the second is the design work

1. **Reclamation granularity, bounded to a slab.** A slab of this class frees as the
   allocator already frees slabs, so this is not a new mechanism -- but a live chunk
   keeps its 2 MiB slab alive, which is a coupling to state in `ARCHITECTURE.md` beside
   the three existing conditions. Smaller than the first version of this plan feared,
   because the unit is a slab rather than a whole key's region.
2. **Degradation under partial update.** I2 makes a published extent immutable, so
   rewriting one chunk spills it to a fresh slot and punches a hole in the run.
   **Contiguity becomes a property that must be maintained and compacted, not one the
   store has** -- and the compactor already exists for exactly this, since
   `alloc.rs` calls compaction "a *locality* mechanism, not merely space reclamation"
   and relocates in `ChunkKey` order. So the lending path must handle a run broken into
   several, and fall back to gathering for the fragments.
3. **Torn-write blast radius.** Integrity for 255 chunks from a slab header rather
   than 255 trailers. Packed pages already accepted that trade at 4 KiB; this takes it
   to 2 MiB, so `crash_matrix` needs a case of its own rather than inheriting the
   per-chunk one.

## Phases

* **Phase 1 -- read path, no writer. DONE 2026-09-30.** Landed as a
  `PACKED_LARGE_CLASS` of 65 600 bytes ( eight bitmaps plus the page header, rounded to
  the ladder's 64-byte rule -- a flat 65 536 holds only seven, because `capacity` is
  `page_size - HEADER` ), a reader that resolves a page's base and size from the slab's
  class instead of a global `PAGE` modulus, and `fsck` carrying each page's size so it
  can verify one without an allocator to ask. **Nothing is contiguous yet**: there is no
  writer, so the adjacency harness still reports "not contiguous". Phase 1 buys the
  ability to *read* a large page.
  Two deliberate fixtures, since nothing produces one incidentally --
  `a_large_packed_page_lays_bitmaps_out_adjacently` asserts the payload **stride** is
  exactly `BITMAP_BYTES`, and `a_large_packed_page_resolves_its_own_base_and_size`
  asserts the old modulus *disagrees* with the correct base so it cannot pass vacuously.
  The written description above is superseded in one respect: no span header type and no
  `enc` bit were needed, for the reasons in the layout section.
  Four latent assumptions surfaced and are recorded in the JOURNAL entry of the same
  date -- `MAX_VERIFIED_SPAN` as a compile-time bound that makes page size a *write*-path
  cost, `MAX_CLASSES` derived against the wrong field and overwriting the B+tree root,
  and four sites spelling "packed is class 0" as an index rather than a predicate.
* **Phase 2 -- writer at checkpoint**, behind the admission rule, with the allocator
  placing a span contiguously. Gate: `crash_matrix` gains a torn-span case;
  `differential` and the roaring byte-identity tests must be untouched, which is the
  evidence that the format did not change.
* **Phase 3 -- reclamation and compaction.** Free a span as a unit; decide and
  implement what a partial overwrite does. Until this lands Phase 2 stays behind a
  `DbOptions` switch that defaults off.
* **Phase 4 -- lend it.** A span-aware accessor, and the Flight `Bitvector` and
  `Containers` arms taking it, so the gather disappears for the shape that motivated
  the work. The adjacency harness flipping to "already one contiguous bitvector" is
  the acceptance test.

## What is already done and must not be redone

The two *encoding* costs are gone: the WAL no longer expands dense data
( `compact-wal-chunk-images`, 32.01x to 1.01x ) and Flight can ship container payloads
or a materialized bitvector ( `flight-dense-set-results` ). This plan is only about the
remaining **gather**.
