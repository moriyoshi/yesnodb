# yesno-core-fuzz

This standalone crate contains the `cargo-fuzz` targets for
[`yesno-core`](../README.md). It is outside the parent Cargo workspace because
libFuzzer requires a nightly toolchain and sanitizer instrumentation.

The targets are:

- `decode_container`: arbitrary container payloads must decode to an error or
  to a container that passes validation, never panic; and
- `roaring_import`: arbitrary whole-file portable Roaring input must return an
  error or a valid set, never panic.

Install `cargo-fuzz`, then run a target from this directory:

```console
cargo +nightly fuzz run decode_container
cargo +nightly fuzz run roaring_import
```

Fuzzing complements the deterministic and property tests in `yesno-core`; it
does not replace the workspace test gate.
