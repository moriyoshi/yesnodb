# yesno-flight

`yesno-flight` provides the Arrow Flight protocol surface for
[yesnodb](../README.md) queries and bulk ingest.

The default `server` feature exposes `YesnoFlightService`. The always-available
client surface includes `YesnoClient`, query metadata, streaming results, and
the shared expression types from `yesno-wire`. A client-only build can omit the
storage engine by disabling default features.

Flight metadata carries an exact result cardinality, and tickets identify the
snapshot version used to answer a query. The service supports ordinal streams,
`(key, ordinal)` ingest batches, atomic point insert, remove, and contains
actions, and whole-key clear. The `stats` result is a typed protobuf message;
fixed-width action bodies and other results use little-endian `u64` values.
Checkpointing is intentionally absent from Flight and belongs to the daemon's
control plane. This is not a Flight SQL implementation and does not carry
replication WAL.

From the repository root:

```console
cargo test -p yesno-flight
cargo test -p yesno-flight --no-default-features
cargo doc -p yesno-flight --no-deps
```

The [getting-started guide](../docs/getting-started.md) documents the shipped
server and client commands.
