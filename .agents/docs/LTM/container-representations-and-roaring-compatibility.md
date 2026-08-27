# Container Representations and Roaring Compatibility

## Summary

Each 16-bit fiber is represented as an array, bitmap, or run container. Representation choice is intentionally history-sensitive in memory, while portable Roaring serialization infers non-run kind from cardinality; the export boundary must reconcile those two models explicitly.

## Key Facts

- `ARRAY_MAX = 4096` is the exact byte crossover between a `u16` array and an 8192-byte bitmap.
- `BITMAP_DEMOTE = 3584` creates a hysteresis gap. It is not the portable-format threshold.
- `OPT_GAIN_NUM / OPT_GAIN_DEN = 7 / 8` prevents representation churn when an alternative is only marginally smaller.
- `RUN_MAX_INTERVALS = 2032` is a writer capacity choice with slack, not a format limit or tight derivation.
- `RUN_DECODE_MAX` is larger because readers must accept valid foreign run containers beyond the writer's preferred capacity.
- Portable Roaring carries no non-run kind field. A reader infers array versus bitmap from cardinality.
- The internal store is safe from that ambiguity because `ChunkRef` carries an explicit 2-bit kind.
- `ChunkRef` kind 3 is rejected online. It was evaluated for a compressed binary trie and deliberately remains unused.
- `u64::MAX` is not an ordinal. The top 16-bit fiber has 65,535 legal values and can never be full.

## Details

### Representation independence

Container kinds encode subsets of the same 16-bit universe. Correct set algebra is defined on decoded subsets, so any specialized arm is semantically correct when it agrees with the generic kernel. Output kind is a performance choice inside the algebra, but it becomes observable at a serialization boundary.

The generic kernel is retained as the oracle for specialized implementations. Tests compare contents, cached cardinality, non-emptiness, and structural validity in both operand orders.

### Hysteresis versus portable Roaring

In-memory kind is a function of cardinality and history. A bitmap may remain a bitmap below `ARRAY_MAX` because immediate demotion would destroy the amortized conversion bound. Portable Roaring instead decodes kind as a function of cardinality alone.

Before the fix, a retained bitmap with `card <= 4096` could be serialized as an 8192-byte bitmap beneath a header promising `2 * card` array bytes. At cardinality 1 this could decode successfully to the wrong value, so a cardinality-only regression assertion was insufficient.

`Roaring32::serialize` now uses an export-only normalization that converts a bitmap with `card <= ARRAY_MAX` to an array. It must not call the general `optimize()` path, because the 7/8 rule may preserve the bitmap and because optimization may create a run after the run-cookie decision has already been made.

### Codec and foreign input policy

- Run payloads include the leading `u16 nruns`; encoded size is `2 + 4 * nruns`.
- Deserialization bounds allocation from the remaining input before reserving container tables.
- `container::codec::decode` must return `Err` or a valid container for arbitrary bytes and must never panic.
- Misaligned shared arrays copy through the fallback path rather than erroring. Bitmap word access also has a copying fallback.
- Reserved `ChunkRef` discriminant 3, non-zero encoding bits, and reserved high bits are rejected by `ChunkRef::validate` on the online read path, not only by `fsck`.

### I8 and the top fiber

`ORDINAL_MAX = u64::MAX - 1`. This makes every set cardinality fit in `u64` and makes the universe-wide lazy complement countable. The excluded point is `( TOP_PREFIX, 0xFFFF )`; low value `0xFFFF` remains legal at every other prefix.

The structural I8 check is split because no layer has both inputs by itself: `fsck::rebuild` knows the chunk key, while codec validation knows the payload. `Rebuilt::i8_violations` joins them and reads at most one top-prefix container per key.

### Shape economics and copying fallbacks

Container kind reflects three independent gates, not a single threshold. A run must fit `RUN_MAX_INTERVALS`, occupy fewer bytes than the alternative, and beat it by `OPT_GAIN`. The standing `container_shape.py` scenario keeps clustered high-run and scattered non-run controls so a policy change cannot erase the workload that run-kernel work targets.

A compressed binary trie was measured as both a replacement and a fourth kind. It lost 20.1% as a wholesale replacement and saved at most 6.6% when selected only for winning chunks, before its omitted overhead. The mid-density bitmap band dominates stored bytes and is structurally hostile to the trie. The kind was declined rather than deferred; revisit only with a materially different corpus.

Misaligned bitmap data must decode through a copying `Cow` path across the whole accessor surface. Fixing `iter` alone left `min`, `max`, `rank`, `select`, `run_count`, and `runs` returning plausible empty or zero answers. Page-store geometry makes the case unusual, but imported shared buffers can reach it.

## Files

- `yesno-core/src/container/` - array, bitmap, run, and codec implementations.
- `yesno-core/src/roaring_format.rs` - portable 32-bit and 64-bit Roaring I/O.
- `yesno-core/src/store/extent.rs` - explicit on-disk container kind and cardinality.
- `yesno-core/src/lib.rs` - thresholds and ordinal ceiling.
- `yesno-core/fuzz/fuzz_targets/decode_container.rs` - arbitrary-byte decoder coverage.

## Test Coverage

- `yesno-core/tests/differential.rs` checks semantic agreement and byte identity.
- `yesno-core/tests/proptest_oracle.rs` covers representation and ordinal boundaries.
- `deserialize_never_panics` protects the fuzz-target contract.
- Removal-from-bitmap regressions construct the history-sensitive states insertion-only generators cannot reach.
- Aligned and deliberately misaligned bitmap twins cover every accessor, including run analysis used by optimization.
- I8 tests cover the illegal top ordinal, the largest legal ordinal, and selectivity at lower prefixes.

## Pitfalls

- Never change a representation threshold to repair an interchange-format boundary.
- Never reject foreign run counts merely because the local writer would choose another representation.
- Never infer non-run kind from cardinality inside the internal store; use `ChunkRef`'s explicit kind.
- A generator that builds only from final sorted values cannot exercise history-sensitive representation states.
- Do not close a shared-buffer bug after fixing one accessor; audit every use of the fallible alignment view.

