# yesno-core

`yesno-core` is the storage and set-algebra engine for
[yesnodb](../README.md). It maps 64-bit keys to sparse sets of 64-bit ordinals
and provides eager and lazy set operations, persistent snapshots, write-ahead
logging, checkpointing, and replication primitives.

The crate deliberately has no async runtime, gRPC stack, or query-engine
dependency. Network services and integrations live in the satellite crates.

## Main surfaces

- `OrdSet`, `Container`, and `ops` implement Roaring-style sparse sets.
- `ChunkStream` and `Expr` evaluate set expressions without materializing
  intermediate results.
- `Db`, `WriteBatch`, and `Snapshot` provide durable, versioned storage.
  `Snapshot::key_stream_prefix_range` bounds metadata planning to a half-open
  chunk-prefix interval when a caller will read only part of a posting list.
- `matrix`, `bignum`, and `view` expose packed-bit representations.
- `roaring_format` imports and exports portable Roaring data.

`u64::MAX` is reserved and is not a valid ordinal. Container payloads are
byte-compatible with the portable Roaring format.

Enable the optional `tracing` feature to emit structured spans for database
open and recovery, commits, replica WAL apply, checkpoints, and verification.
The feature is off by default so the crate keeps its five-dependency baseline;
applications provide the subscriber. Spans expose operation metadata and
counts, never keys, ordinals, expressions, or payloads.

## Try it

From the repository root:

```console
cargo run -p yesno-core --example readme
cargo test -p yesno-core
cargo bench -p yesno-core --bench setops
```

See the [formal model](../docs/formal-model.md) for the data model and the
[workspace README](../README.md) for project status and higher-level usage.
