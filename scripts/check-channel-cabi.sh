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

# And the C++ consumer, which is a different check from the C one above.
#
# `yesno-mysql/vector_cursor.h` holds the ordinal cursor that the Flight and
# channel backends share. It was file-local to `backend_flight.cc` and had no
# test of its own -- only whatever the MySQL fixtures reached through Flight --
# so extracting it put logic a shipping backend depends on behind no direct
# coverage. Its `kBefore` / `kAfter` sentinels are the subtlety: a cursor has to
# sit outside the set at either end and come back, which is what a descending
# scan needs after a failed seek.
#
# Compiling it here also links `backend_channel.cc` against the staticlib, which
# proves the C++ side of the boundary -- the header usable from C++ as well as
# C, and every symbol the backend calls actually exported. It needs no MySQL
# headers, which is why it can run in this gate at all rather than only behind
# the full MySQL build.
c++ -std=c++17 -Wall -Wextra -Werror \
  -Iyesno-mysql -Iyesno-plugin/include \
  yesno-mysql/vector_cursor_test.cc \
  yesno-mysql/backend_channel.cc \
  target/debug/libyesno_plugin.a \
  -lpthread -ldl -lm \
  -o "$scratch/cursor"

"$scratch/cursor"
