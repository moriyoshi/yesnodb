#!/usr/bin/env bash
#
# Compile and run the channel C ABI's C fixture.
#
# This is the only check that can fail in the ways that matter for a C header:
# that `yesno-plugin/include/yesno_channel.h` parses as C11 under
# `-Wall -Wextra -Werror`, that every symbol it declares is actually exported by
# the staticlib, and that the declared signatures agree with the Rust ones. A
# Rust test cannot fail at any of those, which is why `tests/cabi.rs` -- which
# drives the same entry points against a live server -- is not a substitute.
#
# The fixture needs no server: it covers the absent-socket path, null handles,
# null outputs, closing null, a truncating error buffer, and the enum values the
# header promises. Behaviour against a running channel is `tests/cabi.rs`.
set -euo pipefail

repo_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_dir"
. "$repo_dir/scripts/scratch.sh"

# `cargo test` builds test binaries, not the `staticlib` artifact, so this asks
# for it explicitly rather than assuming an earlier step left one behind.
cargo build -p yesno-plugin

mkdir -p "$YESNO_SCRATCH_DIR"
scratch=$(mktemp -d "$YESNO_SCRATCH_DIR/channel-cabi.XXXXXX")
trap 'rm -rf "$scratch"' EXIT

cc -std=c11 -Wall -Wextra -Werror \
  -Iyesno-plugin/include \
  yesno-plugin/tests/channel_smoke.c \
  target/debug/libyesno_plugin.a \
  -lpthread -ldl -lm \
  -o "$scratch/smoke"

"$scratch/smoke" "$scratch/absent.sock"
