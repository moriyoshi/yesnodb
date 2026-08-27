# yesno-wire

`yesno-wire` defines the versioned set-expression wire format shared by
[yesnodb](../README.md) clients and servers.

The crate contains the dependency-free request types and their encoding and
decoding logic, including Boolean expressions, ranges, materialized ordinal-set
literals, pinned snapshot requests, and packed-view transforms. A legacy
descriptor containing only one little-endian key remains unambiguous with the
versioned expression format.

Decoders treat their input as untrusted. Expression depth and node count are
bounded, and arbitrary input must return an error or a valid expression without
panicking. The crate intentionally has no dependencies so it remains cheap to
link into clients and the PostgreSQL extension.

From the repository root:

```console
cargo test -p yesno-wire
cargo doc -p yesno-wire --no-deps
```
