# Representation, Format, and Space Synthesis

## Summary

Yesno represents each 16-bit fiber as an array, bitmap, or run container, persists it through explicit manifest, index, and extent metadata, and exports through portable Roaring rules that differ from in-memory hysteresis. Correct work here must preserve decoded-set semantics, byte-level interchange fidelity, zero-copy lifetime safety, and total storage economics together.

## Included Documents

| Document | Focus |
|---|---|
| [Container Representations and Roaring Compatibility](./container-representations-and-roaring-compatibility.md) | Container invariants, thresholds, canonical codecs, shape economics, and interchange normalization. |
| [Storage Format, Index, and Zero-Copy Reads](./storage-format-index-and-zero-copy.md) | Manifest routing, extents, packed pages, `ChunkRef`, B+tree layout, mmap ownership, and validation. |
| [Compression Models and Space Economics](./compression-models-and-space-economics.md) | Entropy bounds, run-aware models, measured waste, trie economics, and index overhead. |

## Stable Knowledge

- `ARRAY_MAX = 4096` is the exact array-versus-bitmap byte crossover. `BITMAP_DEMOTE = 3584` supplies hysteresis, and `OPT_GAIN_NUM / OPT_GAIN_DEN = 7 / 8` prevents marginal representation churn.
- `RUN_MAX_INTERVALS = 2032` is a local writer-capacity choice. Readers accept larger valid foreign runs through `RUN_DECODE_MAX`.
- In-memory kind is history-sensitive, while portable Roaring infers non-run kind from cardinality. Export normalizes retained bitmaps with `card <= ARRAY_MAX` to arrays without calling the general `optimize()` path.
- `ChunkRef` carries explicit kind and cardinality. Reserved kind 3, non-zero encoding bits, and reserved high bits are rejected online rather than only by `fsck`.
- A compressed binary trie was declined: it lost 20.1% as a replacement and saved at most 6.6% selectively on the measured corpus before omitted overhead. A wider chunk increased its margin only by degrading the array baseline; total bytes per ordinal stayed flat.
- Misaligned bitmap data copies through one `Cow`-based accessor policy. `iter`, `min`, `max`, `rank`, `select`, `run_count`, and `runs` must not disagree about alignment fallback.
- `ORDINAL_MAX = u64::MAX - 1`; the top fiber has 65,535 legal values and can never be full.
- The A/B `MANIFEST` persists database UUID, shard count, and the 256-entry virtual-to-physical map. `DbOptions::shards` is a creation parameter once a manifest exists.
- Whole-chunk cardinality is an index read. `Snapshot::len_in_range` and `range_summary` decode only the two partial boundary chunks, regardless of interior width.
- `ExtentGuard` is the mmap lifetime proof for zero-copy buffers. Persistent `class_sizes` and `node_size` prevent geometry changes from silently reinterpreting old data.
- Payload compression and index compression are one economic question. A new kind must justify lookup, iteration, update, conversion, metadata, and every additional dispatch pair, not merely improve an entropy bound.

## Operational Guidance

Start representation changes from decoded-set semantics, then audit every boundary where representation becomes observable: optimization, portable serialization, `ChunkRef`, extent or packed-page identity, index metadata, imported shared buffers, and `fsck`.

Treat the manifest and superblocks as separate A/B identities. A torn manifest must not be recreated over existing shards, and persisted-map behavior needs a deliberately permuted-map regression because the default map equals the modulo.

For compression proposals, measure total bytes and operation costs on a named corpus. Keep one standing fixture that proves the targeted shape exists and retain the generic kernel and portable codec as semantic and byte-fidelity oracles.

## Files

- `yesno-core/src/lib.rs` - representation thresholds and ordinal ceiling.
- `yesno-core/src/container/` and `yesno-core/src/roaring_format.rs` - representations, codecs, and portable normalization.
- `yesno-core/src/db/manifest.rs` - database identity and persisted routing.
- `yesno-core/src/store/{extent,packed,segment,slabmeta,superblock}.rs` - persistent layout and mmap ownership.
- `yesno-core/src/index/{node,tree}.rs` - prefix-compressed COW index.
- `.agents/docs/LTM/removed-stats-instrument-source.md` - preserved removed `( m, r )` and trie instrument source.

## Tests

- `cargo test -p yesno-core --test differential` checks semantic and byte-level Roaring compatibility.
- `cargo test -p yesno-core --test proptest_oracle` covers representation and ordinal boundaries.
- `cargo test -p yesno-core --test allocation` protects index-only cardinality and bounded range-summary work.
- `cargo run -p yesno-e2e -- e2e/scenarios/container_shape.py` holds the clustered-run and scattered controls.
- Decoder fuzzing must return `Err` or a valid container for arbitrary bytes and never panic.

## Pitfalls

- Do not change representation thresholds to repair an interchange mismatch.
- Do not infer internal kind from cardinality or trust a reference before online validation.
- Do not close a shared-buffer defect after fixing only one accessor.
- Do not recreate identity or routing because both manifest slots are unreadable.
- Do not quote a compression percentage without its denominator, absolute bytes, corpus construction, and operation cost.
