#!/usr/bin/env bash

set -euo pipefail

repo_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_dir"
. "$repo_dir/scripts/scratch.sh"
export CARGO_TARGET_DIR="$YESNO_SCRATCH_DIR/yesno-channel-c-target"

cargo test --manifest-path yesno-channel-c/Cargo.toml --locked
cargo clippy --manifest-path yesno-channel-c/Cargo.toml --all-targets --locked -- \
  -D warnings
cargo fmt --manifest-path yesno-channel-c/Cargo.toml --all -- --check
cargo build --manifest-path yesno-channel-c/Cargo.toml --locked

mkdir -p "$YESNO_SCRATCH_DIR"
scratch=$(mktemp -d "$YESNO_SCRATCH_DIR/yesno-channel-c-gate.XXXXXX")
trap 'rm -rf "$scratch"' EXIT

# The C compile is the point of this step and not a formality: it is the only
# thing that proves the header parses as C, that every declared symbol is
# actually exported, and that the signatures agree. A Rust test cannot fail in
# any of those ways, which is why `tests/abi.rs` is not a substitute.
cc -std=c11 -Wall -Wextra -Werror \
  -Iyesno-channel-c/include \
  yesno-channel-c/tests/smoke.c \
  "$CARGO_TARGET_DIR/debug/libyesno_channel_c.a" \
  -lpthread -ldl -lm \
  -o "$scratch/smoke"

"$scratch/smoke" "$scratch/absent.sock"
