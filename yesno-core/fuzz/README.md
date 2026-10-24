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

`decode_expr.dict` holds 82 tokens, and **none of them was written by hand**.
Each comes from a minimal well-formed expression -- one per node kind, 41 in
all -- encoded by `yesno-wire`'s own encoder and decoded back to the same value
before it was kept. Each expression contributes its node bytes, which can be
spliced into a subtree, and its whole encoding with the header. cargo-fuzz does
not pick a dictionary up by name, so it has to be passed, and the path is
relative to where you stand:

```console
cd yesno-core && cargo +nightly fuzz run decode_expr -- -dict=fuzz/decode_expr.dict
```

A mistyped path is not an error: libFuzzer prints `ParseDictionaryFile: file
does not exist or is empty` and runs with no dictionary. Read the log for
`Dictionary: 82 entries` before believing any number from the run.

**Measured on 2026-10-24, from the same minimized corpus, 120 seconds a run**,
edges gained per seed:

| dictionary | seeds | gain |
|---|---|---|
| none | 6 | 6 to 16 |
| the earlier hand-written file, 55 tokens | 8 | 14 to 90 |
| this file, 82 tokens | 6 | **106 to 111** |

The hand-written file is the cautionary half. It was adopted on two seeds that
happened to score 76 and 90, and a coverage report later showed its fold tokens
could never parse: a fold is the tag, then a whole vector expression, then the
operator byte, and the tokens put the operator straight after the tag. The view,
pack and expand tokens had the same mistake. libFuzzer has no way to say that a
token cannot parse, so the only signal was a noisy gain. Generating the tokens from
the encoder ends that class of error, and the spread across seeds shrank from
76 edges to 5.

## Reading a coverage report of the corpus

`cargo fuzz coverage` replays the **corpus**, and a corpus keeps only inputs
that added a libFuzzer feature. So a line at zero means "no retained input
reaches it", not "never executed". Two kinds of zero in `yesno-wire` look the
same in the report and are not:

- **Executed but never retained.** The `FOLD_XOR` decode arm is at zero after
  hundreds of millions of executions with a correct XOR token in the
  dictionary. A single XOR-fold input does reach it, but merged into the corpus
  it adds 0 features: in the optimized fuzzing build its edge is already
  covered by other inputs, so libFuzzer has no reason to keep one.
- **Unreachable at the default length.** The per-node `TooManyNodes`
  rejections need more than `MAX_NODES` (4096) nodes of at least one byte each,
  after a six-byte header, so at least 4103 bytes. libFuzzer's default
  `-max_len` is 4096. To fuzz the node cap past the junction pre-check, pass
  `-max_len=16384`.

The dictionary libFuzzer itself recommends at the end of a run is not the one to
use. It is almost entirely generic integer patterns and does not include the
`YSNX` magic, because the corpus already carries it and so the recommender never
sees it as valuable.

The corpora are gitignored, so none of the above is shared between machines.
Whether that should change is filed as
`fuzz-corpora-are-discarded-and-the-targets-run-nowhere` in
`.agents/docs/TODO.md`, with the size and replay cost already measured.
