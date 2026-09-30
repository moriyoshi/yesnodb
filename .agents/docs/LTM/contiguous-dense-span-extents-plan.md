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

## Why the index needs no change, which is the cheap part

`ChunkRef` is a packed `u64` whose `[ 0:40 )` field is **`cell`: a byte offset in the
shard address space**, not an extent identifier. A span member therefore addresses its
payload with `cell = span_base + i * 8192` and nothing new is needed. The 8 bytes per
chunk stay 8 bytes per chunk.

`[ 59:60 ) enc`, documented "0 = raw Roaring; MUST be 0 in v1", is the spare bit that
marks a member whose integrity comes from a span header rather than from a trailer.
`[ 60:64 )` remains reserved.

## Proposed layout

A **dense span** is a header followed by `N * 8192` contiguous bitmap payloads, with
the header carrying magic, version, `N`, the key, the base prefix and a CRC -- the
same arrangement packed pages already use, at a larger size. Members' `ChunkRef`s point
at their payload bytes with `enc = 1`.

**Admission is a rule, not a default.** Only full bitmap payloads, only consecutive
prefixes of one key, only at checkpoint, and only above a minimum count worth the
coupling. Modelled on `PACK_MAX`, and for the same reason: a layout that pays off for
one shape must not be imposed on the others.

## The three costs, and the second is the design work

1. **Reclamation granularity.** A span frees as a unit, so one live chunk pins the
   whole region. This is a fourth coupling beside the three existing conditions and
   has to be stated in `ARCHITECTURE.md` next to them.
2. **Degradation under partial update.** I2 makes a published extent immutable, so
   updating one chunk of a span means rewriting the span or spilling that chunk to a
   standalone extent. **Contiguity becomes a property that must be maintained and
   compacted, not one the store has.** A write-once read-many cache fits; a
   scattered-update set does not, and must degrade gracefully rather than thrash.
3. **Torn-write blast radius.** One CRC over a span rather than per chunk. Packed
   pages already accepted that trade at 4 KiB; this takes it to megabytes, so the
   crash matrix needs a span case rather than inheriting the per-chunk one.

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
