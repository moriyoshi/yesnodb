# yesno-core-fuzz

This standalone crate contains the `cargo-fuzz` targets for
[`yesno-core`](../README.md). It is outside the parent Cargo workspace because
libFuzzer requires a nightly toolchain and sanitizer instrumentation.

The targets are:

- `decode_container`: arbitrary container payloads must decode to an error or
  to a container that passes validation, never panic;
- `roaring_import`: arbitrary whole-file portable Roaring input must return an
  error or a valid set, never panic; and
- `decode_expr`: arbitrary bytes offered to the wire expression decoders in
  [`yesno-wire`](../../yesno-wire) -- `SetExpr::decode`, `AnyExpr::decode` and
  `QueryRequest::decode` -- must return an error or an expression satisfying the
  decoder's admission bounds, never panic. This is the one target whose input
  reaches a server from a *client* rather than out of a file the server wrote,
  which is why it is here despite its subject living in a sibling crate.

Install `cargo-fuzz`, then run a target from this directory:

```console
cargo +nightly fuzz run decode_container
cargo +nightly fuzz run roaring_import
cargo +nightly fuzz run decode_expr
```

Fuzzing complements the deterministic and property tests in `yesno-core`; it
does not replace the workspace test gate.
