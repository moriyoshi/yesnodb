# yesno-tantivy

`yesno-tantivy` adapts [yesnodb](../README.md) ordinal sets into Tantivy
queries.

A yesno ordinal is a stable application-level document ID, while Tantivy uses
segment-local document IDs that change as the index merges. The crate resolves
those IDs before query execution and binds each `PreparedYesnoQuery` to exactly
one Tantivy searcher generation. A generation mismatch is an error instead of
silently producing incomplete results.

The default build supports embedded `OrdSet` values. The optional `flight`
feature fetches a remote yesno result before preparing the synchronous Tantivy
query.

From the repository root:

```console
cargo test -p yesno-tantivy
cargo test -p yesno-tantivy --features flight
cargo run -p yesno-tantivy --example embedded_filter
cargo run -p yesno-tantivy --features flight --example remote_filter -- \
  http://127.0.0.1:50051 100 200
```

The remote example expects a running yesno Flight server whose keys `100` and
`200` contain the example's stable product ordinals.

Missing ordinals are errors by default. Applications that deliberately accept
eventual consistency must opt into `MissingOrdinalPolicy::Ignore`.
