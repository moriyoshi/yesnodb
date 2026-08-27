# Storage Format, Index, and Zero-Copy Reads

## Summary

The persistent store combines sparse mmap segments, size-classed extents, packed pages, and a prefix-compressed COW B+tree. Zero-copy reads depend on explicit lifetime guards, alignment guarantees, and online validation of every reference before its payload is interpreted.

## Key Facts

- A segment is 1 GiB of sparse address space; file length is not physical space used.
- Slabs are 2 MiB and reserve `SLAB_META` at their start. Slab 0 is reserved because its first bytes hold the two superblocks.
- Extent classes are multiples of 64 bytes and include trailer space.
- Packed pages serve tiny payloads and track live bytes separately from slot occupancy.
- `ChunkRef` is a 64-bit descriptor carrying cell, cardinality-minus-one, kind, encoding, and reserved bits.
- Leaf keys are prefix-compressed; internal separators remain full-width.
- `node_size` and the size-class ladder are persisted so retuning does not silently reinterpret old files.
- `ExtentGuard` keeps the mmap alive for every zero-copy `Buffer` handed out.
- The A/B `MANIFEST` persists database UUID, shard count, and the 256-entry virtual-to-physical shard map.
- Full-range cardinality and range summaries use index cardinalities and read at most two partial boundary payloads.
- Posting-list **endpoints** are index reads too: `min` and `max` locate the endpoint chunk from `card_m1` and decode exactly one payload. `min` is flat in chunk count; `max` still walks the key's index range because the scan is ascending and there is no reverse traversal.
- B+tree nodes, packed pages, and standalone payloads are checked on their first online read through an offset-keyed CRC32C cache, then rechecked only after a write invalidates that region.
- Manifest updates target the non-live slot and preserve the previous durable slot until the higher-sequence replacement is complete.
- The opener refuses compiled-versus-persisted geometry and shard-identity mismatches. Supporting a non-default decoded geometry remains a separate capability.
- Leaf suffix widths 10 and 12 require 80-bit and 96-bit comparisons; `LeafRef::search` uses `u128` for every legal width above eight bytes.

## Details

### Unsafe boundary and alignment

The mmap-backed `Buffer` construction is the storage layer's unsafe boundary. The lifetime proof is embodied by ownership: every returned buffer retains an `ExtentGuard`, and the guard retains the segment mapping.

MIRI cannot execute file-backed mappings, and ASan and Valgrind cannot detect an overread that remains inside the mapped file region. Both tools catch a heap control in the same probe and remain silent for a 1,000-byte overread past a 16-byte logical slice inside a 4,096-byte mapping. The blindness follows the bytes: a SIMD kernel over a disk-backed zero-copy container inherits it even though the same kernel is instrumented over heap data.

Out-of-mapping access is guarded twice before the pointer is formed: `segment_for` checks containment, then a safe slice index checks `off..off + len`. The harder case is wrong-but-in-range, where a plausible `ChunkRef` aliases a neighbour's bytes. Online checksum verification now detects that case unless both payload and stored checksum are corrupted consistently.

Allocator geometry provides 64-byte aligned payload starts, which subsume `u16` and `u64` alignment. Foreign or misaligned shared data follows copying codec fallbacks. The removed `check_alignment` helper was unwired and documented an error policy production did not use.

### Reference identity

Standalone extents carry an `ExtTrailer` containing a 32-bit chunk-key tag. Packed pages carry a header CRC and first/last key range. `read_container_for(key, cref)` validates `ChunkRef`, extent geometry, trailer or packed-page range, payload placement, identity, and the stored checksum before decoding. A packed offset below the payload floor can otherwise decode page header bytes as a plausible array.

The checksum cache is keyed by file offset, not node or extent identity, because allocator identifiers can be recycled while the physical bytes at an offset change. `write_at` invalidates the overlapping cached regions after the write completes. Invalidating before the write would allow a concurrent reader to cache a verdict for old bytes that the write then falsifies.

### Online checksum verification

`SegmentedMmap::verify_once` runs a caller-supplied verification the first time a region is read and caches only success. The storage caller owns the format-specific details: index nodes and packed pages know their fixed regions, while a standalone extent's checksum lives in a trailer outside the payload. The extent path was already reading that trailer for `ckey_tag`, so it supplies both identity and checksum without another lookup.

`Snapshot::load` now returns an error for a corrupt node or payload. Before the error channel was threaded through `merged_chunks`, adding the check made behavior worse by converting corruption into a silently empty or short set. A check whose error cannot reach the caller is not an enforced check.

The first touch of an 8 KiB bitmap region costs about 1.25 us for verification; warm reads pay no checksum. Across 2,000 bitmap keys the cold pass was 2.66x the warm pass. Hashing the mapping directly instead of copying through `read_at` reduced the one-off cost from 1.99 us to 1.25 us.

`Db::verify` deliberately walks index nodes through `RawNodes` rather than the production verifying reader. An offline diagnostic must continue past a corrupt node so it can attribute all damage; a production reader must refuse the first corrupt value rather than return partial data.

### B+tree and prefix compression

Leaf entries store a common key prefix plus the shortest supported suffix width that fits the sorted range. `LeafBuilder::push` separately enforces ascending keys and suffix fit: unsorted input is an error, while a sorted key outside the current width seals the leaf.

Range cursors retain a stack and cache the current leaf. Re-descending or re-reading the leaf per entry produces silent truncation or large read amplification. The hot range path is one descent plus a sequential cursor walk.

A legal leaf suffix width is one of 2, 4, 6, 8, 10, 12, or 14 bytes. Widths 10 and 12 cannot use a 64-bit suffix helper: truncation loses the high 16 or 32 bits and an 8-byte slice calculation underflows. `LeafRef::search` therefore compares a `u128` suffix. Coverage sweeps every legal width with differences above bit 64; a low-bit-only fixture catches the panic but not the silent truncation behind it.

Internal nodes retain full 14-byte separators. Their fanout is 55 entries per 1 KiB node, so internal nodes occupy 1.828% to 1.837% of index nodes across every legal leaf suffix width. Compressing internal separators could save about 1% of index bytes but only about 0.05% of the database, so the format change is closed as uneconomic. Leaf fanout cancels out of that ratio; no corpus can produce the lower-fanout falsifier once proposed for it.

### Persistent geometry

`class_sizes` and `node_size` are superblock fields. A zero `node_size` means the historical default, preserving older files. Geometry changes without persisted values can parse every existing node with the wrong width while returning plausible data.

The opener honors node size and structurally validates the class ladder. **The correctness half of this closed on 2026-09-12 and the paragraph above described the superseded state until 2026-09-14.** `SuperBlock::decode` now refuses a file whose `class_sizes`, `page_size` or `slab_size` differ from the build's, and `superblock::pick` -- the only production read path -- decodes both slots and propagates that error, so no address calculation can run against a geometry the compiled constants disagree with. The image-swap hole is closed with it: `shard_id` is compared and deliberately given no zero-tolerance, because shard 0's number is zero and tolerating it would reopen the hole for every pair involving shard 0.

What remains is a capability rather than a gap: allocator addressing still uses the compiled ladder, so a non-default geometry is **refused** rather than honoured. Threading one decoded geometry object through capacity, slot lookup and allocation is tracked as `decoded-geometry-does-not-drive-address-calculation` -- the entry this paragraph's old slug, `stored-superblock-descriptors-are-not-authoritative`, was renamed to.

### Manifest identity and routing

The database-level `MANIFEST` owns the UUID, shard count, and virtual-shard routing map. Its file reserves two copies. **This paragraph described the pre-repair state until 2026-09-14, and the repair had already landed**: `write_manifest` targets the slot `manifest::pick` is *not* returning ( `manifest::next_slot_offset` ), leaving the live slot byte-for-byte intact, so at every offset at which a crash can occur `pick` still returns a manifest -- the old one while the new slot is incomplete, the new one once it is whole. A partially written slot fails its CRC, so its higher `seq` is never observable.

Two consequences worth carrying. The `sync_all` before returning is **ordering, not belt-and-braces**: the next update overwrites the slot this one left alone, so these bytes must be durable before that is allowed to start, and it is `sync_all` rather than `sync_data` because first-time extension of the file makes a non-durable length a missing slot. And a slot becomes authoritative purely by carrying the higher `seq`, so an update that failed to raise it would land on disk and be silently ignored -- that is checked rather than commented. Writing *both* slots survives only for the case that cannot lose anything: a file with no readable slot at all, which is creation.

The old slug for this, `manifest-writes-do-not-alternate-slots`, is recorded nowhere and tracks nothing; see `dangling-backlog-citations`.

`DbOptions::shards` is a creation parameter; an existing database uses persisted geometry rather than rejecting a harmless caller mismatch.

Persisting only the count fixes reopen misrouting, while persisting and consulting the map creates the indirection needed for future resharding. A regression with a deliberately permuted map is required because the default map equals `vshard % shards` and would let modulo-based code pass.

Both torn manifest slots are an error, never a cue to recreate identity and routing over existing shards. Every shard superblock is checked against the manifest UUID on open.

### Index-only range summaries

`ChunkRef.card_m1` makes whole-chunk cardinality an index read. `Snapshot::len_in_range` and `range_summary` decode only the two partially covered boundary chunks, independent of the number of complete chunks between them. Allocation coverage protects this property because correctness tests cannot distinguish it from decoding every payload.

The same rule reaches the endpoints, and it is the third application of *an answer must not cost more than the answer it is weaker than*. `Snapshot::min` and `max` were `load( key ).min()` / `.max()`, so reading one value decoded and cloned every chunk of the posting list. Because `card_m1` says whether a chunk is empty without decoding it, the endpoint chunk can be located from the index alone and exactly one payload read. Measured at one ordinal per chunk, so only chunk count varies:

```text
chunks      min(key)      max(key)     load(key)   min/load
       1         444 ns         532 ns         622 ns      0.714
     100         654 ns        3958 ns        9367 ns      0.070
   10000         543 ns      241865 ns      629208 ns      0.001
```

`min` is flat — 444 to 543 ns from 1 to 10 000 chunks, against 870 to 612 516 before. `max` is only 2.5x and still scales, because finding the *last* chunk walks the key's index range; it is index-bound now rather than decode-bound, and a reverse cursor is the remaining work. That cursor was assessed and deliberately not built: `Snapshot::max` has exactly one caller in the workspace, and adding a second traversal direction to the core index for one test verb is machinery carried for a question that has not arrived.

Making `max` flat as well needs a reverse index cursor, and that was assessed and deliberately declined. The tree has one cursor, forward, and its correctness rests on a root-path stack precisely because re-descending for the next key after the last one silently truncates a scan; a reverse cursor is the mirror of that problem and owes the same gap-crossing tests. It would serve exactly one caller — the harness verb — with nothing else in the workspace wanting backward index traversal. The win is recorded so it need not be re-derived: `max` would go flat at roughly `min`'s 543 ns instead of 241 865 ns at ten thousand chunks. Revisit when a second caller exists, not before.

Like `cardinality`, these are a **parallel implementation** returning identical values, so no correctness test can separate them from the materializing version. Allocation counts and an equivalence test across every overlay shape are what hold them apart.

The upper scan bound must use `ChunkKey::range_end`; computing maximum prefix plus one wraps through the 48-bit mask and silently turns a whole-universe query into an empty range.

## Files

- `yesno-core/src/store/segment.rs` - mmap segments, containment, and the offset-keyed verification cache.
- `yesno-core/src/store/extent.rs` - extent and `ChunkRef` layouts.
- `yesno-core/src/store/packed.rs` - packed-page format and validation.
- `yesno-core/src/store/slabmeta.rs` - durable slab occupancy metadata.
- `yesno-core/src/store/superblock.rs` - A/B slots and persisted geometry.
- `yesno-core/src/db/manifest.rs` - database identity and persisted shard routing.
- `yesno-core/src/db/store.rs` - format-aware online verification and raw diagnostic node access.
- `yesno-core/src/index/{node,tree}.rs` - prefix-compressed B+tree.
- `scripts/check-r1.py` - public Arrow-type containment baseline.

## Test Coverage

- Layout round trips and corruption tests cover every on-disk structure.
- `LeafRef::search` tests cross every legal suffix width, and `widely_separated_keys_survive_a_reopen` reaches the wide-suffix path through the public API.
- Checksum-cache tests pin first-read verification, overlap invalidation, unrelated-write retention, and failure propagation.
- Lifetime tests keep buffers alive across snapshot and mapping transitions.
- `fsck`, durability, and integrity scenarios exercise online and offline validation separately.
- `scripts/check-r1.py` checks effective public visibility through every ancestor module.

## Pitfalls

- Never use sparse file length as allocated or live-byte accounting.
- Never treat slab 0 as an ordinary allocation slab.
- A public item inside a crate-private module is not public API; visibility checks must walk ancestors.
- A validation helper called only from `fsck` does not enforce a format-version gate on normal reads.
- A stored CRC field is not an integrity guarantee unless the read path recomputes it and can propagate its failure.
- Do not replace the raw fsck reader with the production verifying reader; diagnostics must attribute corruption beyond the first bad node.
- Do not describe the manifest as alternating merely because it contains two copies.
- Do not treat persisted geometry as authoritative until open rejects mismatches.
- Never recreate a manifest merely because both slots are unreadable; that can silently route existing keys to the wrong files.
- A default routing map is deliberately indistinguishable from a modulo. Test indirection with a non-default permutation.

