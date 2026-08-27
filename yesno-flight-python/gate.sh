#!/usr/bin/env bash
set -euo pipefail

project_dir="$(dirname "$(readlink -f "$0")")"
repo_root="$(readlink -f "$project_dir/..")"
. "$repo_root/scripts/scratch.sh"
cd "$project_dir"

if ! command -v uv >/dev/null 2>&1; then
    printf 'gate-python: uv is required; install it from https://docs.astral.sh/uv/\n' >&2
    exit 2
fi

uv lock --check
uv sync --locked --all-extras
uv run ruff check src tests
uv run ruff format --check src tests
uv run mypy

# The integration fixture starts this exact artifact. Building once outside
# pytest avoids a cargo invocation per test and makes a missing daemon a gate
# failure rather than a skipped test.
cargo build --manifest-path "$repo_root/Cargo.toml" -p yesno-server --bin yesnod
YESNODB_TEST_SERVER="$repo_root/target/debug/yesnod" uv run pytest -q

# Build products belong in the ignored agent workspace, not in the source tree.
mkdir -p "$YESNO_SCRATCH_DIR/python-dist"
uv build --wheel --out-dir "$YESNO_SCRATCH_DIR/python-dist"
