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

## Where the budget is worth spending

Measured on 2026-10-24, 180 seconds per target against the corpora on that
machine. **Two of the three found no new coverage at all**: `decode_container`
executed 4 024 836 inputs and `roaring_import` 538 546, and both ended on
exactly the edge count they started with ( 274 and 801 ). They are saturated at
their corpora, so more time buys nothing until the corpora change or the
decoders do.

`decode_expr` is the one that is not saturated: 19 241 943 inputs, 1 727 edges
to 1 749, corpus 1 317 entries to 1 413, and still climbing when the budget ran
out. That is also the only one of the three whose input reaches the server from
a client rather than out of a file the server wrote. Spend time here.

**Pass `-rss_limit_mb` deliberately** rather than taking libFuzzer's 2 048 MB
default: `decode_expr` peaked at 1 040 MB in that run with nothing wrong, so the
default is within reach of a legitimate input, and an inherited one reports a
memory-hungry case as a crash.

## The dictionary, and why it is worth the flag

`decode_expr.dict` holds 55 structural tokens taken from `yesno-wire`'s tag
table -- the six-byte header, whole minimal expressions, the list nodes with
their `u16` child counts, the leaves with their payload widths, and the bignum
operators. cargo-fuzz does not pick a dictionary up by name, so it has to be
passed:

```console
cargo +nightly fuzz run decode_expr -- -dict=decode_expr.dict
```

**Measured on 2026-10-24, and it is not a marginal effect.** Same starting
corpus, 120 seconds, two seeds per arm: without the dictionary the run gained
**6 and 8** edges, with it **76 and 90**, at an unchanged execution rate of
113 000 to 128 000 per second. So it is not buying coverage by running slower --
the tokens are the openings into the recursive grammar, and a child count or a
six-byte header is something a byte-at-a-time mutator almost never assembles by
chance.

The dictionary libFuzzer itself recommends at the end of a run is not the one to
use. It is almost entirely generic integer patterns and does not include the
`YSNX` magic, because the corpus already carries it and so the recommender never
sees it as valuable.

The corpora are gitignored, so none of the above is shared between machines.
Whether that should change is filed as
`fuzz-corpora-are-discarded-and-the-targets-run-nowhere` in
`.agents/docs/TODO.md`, with the size and replay cost already measured.
