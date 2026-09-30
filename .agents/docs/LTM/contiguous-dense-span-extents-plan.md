# Contiguous dense span extents

**Status: planned 2026-09-30.** Product-level requirement: a consumer storing a huge
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
