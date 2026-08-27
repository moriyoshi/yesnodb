#!/usr/bin/env bash

set -euo pipefail

repo_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_dir"
. "$repo_dir/scripts/scratch.sh"
export CARGO_TARGET_DIR="$YESNO_SCRATCH_DIR/yesno-c-target"

cargo test --manifest-path yesno-c/Cargo.toml --locked
cargo clippy --manifest-path yesno-c/Cargo.toml --all-targets --locked -- \
  -D warnings
cargo fmt --manifest-path yesno-c/Cargo.toml --all -- --check
cargo build --manifest-path yesno-c/Cargo.toml --locked

mkdir -p "$YESNO_SCRATCH_DIR"
scratch=$(mktemp -d "$YESNO_SCRATCH_DIR/yesno-c-gate.XXXXXX")
trap 'rm -rf "$scratch"' EXIT

cc -std=c11 -Wall -Wextra -Werror \
  -Iyesno-c/include \
  yesno-c/tests/smoke.c \
  "$CARGO_TARGET_DIR/debug/libyesno_c.a" \
  -lpthread -ldl -lm \
  -o "$scratch/smoke"

"$scratch/smoke" "$scratch/db1" "$scratch/db2"
