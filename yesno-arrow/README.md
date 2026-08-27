# yesno-arrow

`yesno-arrow` provides Apache Arrow interchange for
[yesnodb](../README.md).

It offers two output paths:

- `MaskStream` exposes posting-list chunks as Arrow boolean selection masks.
  Bitmap containers use the same bit layout, so this path can be zero-copy.
- `OrdinalBatchReader` materializes ordinals as non-null `UInt64Array` batches
  when a consumer needs values rather than a filter.

The crate also defines schemas for ordinals, `(key, ordinal)` pairs, masks, and
serialized containers, plus `ContainerBatchBuilder` and `read_containers` for
container interchange. It re-exports its Arrow crates so downstream code can
use the exact compatible Arrow version.

From the repository root:

```console
cargo test -p yesno-arrow
cargo doc -p yesno-arrow --no-deps
```

Prefer masks when the downstream consumer accepts a row filter; converting a
set to an ordinal array necessarily materializes it.
