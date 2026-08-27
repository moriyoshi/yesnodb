#!/usr/bin/env bash
set -euo pipefail

project_dir="$(dirname "$(readlink -f "$0")")"
repo_root="$(readlink -f "$project_dir/..")"
. "$repo_root/scripts/scratch.sh"
agent_tmp="$YESNO_SCRATCH_DIR"
mkdir -p "$agent_tmp"
cd "$project_dir"

unformatted="$(gofmt -l .)"
if [[ -n "$unformatted" ]]; then
    printf 'gate-go: gofmt required:\n%s\n' "$unformatted" >&2
    exit 1
fi

go mod tidy -diff
go vet ./...
go vet -tags=integration ./...
TMPDIR="$agent_tmp" go test -race ./...

# The integration suite talks to this exact artifact. A missing or stale daemon
# is therefore a gate failure rather than a skipped test.
cargo build --manifest-path "$repo_root/Cargo.toml" -p yesno-server --bin yesnod
TMPDIR="$agent_tmp" \
YESNODB_TEST_SERVER="$repo_root/target/debug/yesnod" \
    go test -race -tags=integration ./...
