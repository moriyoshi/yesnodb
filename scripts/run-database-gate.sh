#!/usr/bin/env bash
# Build the all-in-one E2E image, then execute one database or search gate in
# it. Docker is the only host dependency; every gate reuses the same image.

set -euo pipefail

cd "$(dirname "$(readlink -f "$0")")/.." || exit 1

edge="${1:-}"
shift || true
case "$edge" in
    postgresql)
        gate=gate-pg.sh
        ;;
    mysql)
        gate=gate-mysql.sh
        ;;
    search)
        gate=gate-search.sh
        ;;
    *)
        printf 'usage: %s {postgresql|mysql|search} [gate arguments...]\n' "$0" >&2
        exit 2
        ;;
esac

image="$(./scripts/build-e2e-image.sh)"

run_args=(--rm --init)

# The image build and any uncached repository fetches in a later test run may
# need an explicit proxy. Proxy state never becomes a source-tree input.
for name in HTTP_PROXY HTTPS_PROXY NO_PROXY http_proxy https_proxy no_proxy; do
    if [[ -v "$name" ]]; then
        run_args+=(--env "$name")
    fi
done

# No `--user`: these gates need the image's default `yesno-builder` identity,
# because initdb, mysqld and the search engines all refuse root.
exec docker run "${run_args[@]}" "$image" \
    "./scripts/$gate" --container-internal "$@"
