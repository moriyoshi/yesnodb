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

## Proposed layout, corrected: a size class rather than a span type

The blocker is visible in one line of the ladder. `CLASS_SIZES[ 10 ] = 8256`, annotated
"exact for 8192", because a slot must hold the payload *plus its 8-byte trailer* rounded
to a multiple of 64. **That 64 bytes of slack per chunk is what breaks adjacency**, and
nothing else does.

A new class whose slot is **exactly 8192 with no trailer**, admitting only full bitmap
payloads and taking its integrity from the slab header as `PACKED_CLASS` does, makes
consecutive slots adjacent by construction -- `slot_offset` arithmetic then *is* the
contiguity. With `SLAB_BODY = SLAB_SIZE - SLAB_META = 2 097 152 - 8 192`, a slab holds
**255 such slots**: one contiguous 2 MiB bitvector covering 16 711 680 ordinals.

**And 2 MiB is enough, which is the reframing that makes this small.** Zero-copy does
not require the whole set to be one buffer; it requires **each Arrow batch** to be one
contiguous slice, because Arrow's constraint is one contiguous values buffer per column
per *batch*. A 2 MiB slab yields a 16 711 680-row boolean batch, which is an ordinary
batch size. So cross-slab contiguity is unnecessary and no allocator change beyond the
new class is needed -- the existing policy already claims "a contiguous run of slabs"
per checkpoint and bump-allocates within it, so a bulk-loaded key's chunks already land
in one file window.

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

* **Phase 1 -- read path, no writer.** Span header type, its CRC, and
  `read_container_for` learning `enc = 1`. Verified by hand-built spans; nothing
  produces one yet, so a deliberate test is the only coverage ( the lesson from
  `RecType::ChunkImage`, whose arm lost its producer and needed
  `an_old_chunk_image_record_still_replays` ).
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
