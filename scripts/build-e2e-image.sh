#!/usr/bin/env bash
# Build the one all-in-one E2E image and print its tag.
#
# Every containerized gate goes through here, so the build command exists once
# and no gate can drift onto a private tag or a stale `--target`. Build output
# goes to stderr; stdout carries only the tag, so a caller can write
# `image="$(./scripts/build-e2e-image.sh)"`.

set -euo pipefail

cd "$(dirname "$(readlink -f "$0")")/.." || exit 1

# `YESNO_E2E_IMAGE` is the override. The three names below are what the
# per-integration images this one replaced were called; they remain accepted so
# an existing shell or CI job keeps working.
image="${YESNO_E2E_IMAGE:-${YESNO_BUILD_E2E_IMAGE:-${YESNO_DATABASE_BUILDER_IMAGE:-${YESNO_E2E_DRIVER_IMAGE:-yesno-e2e:local}}}}"

docker info >/dev/null
docker build \
    --progress=plain \
    --file e2e/Dockerfile \
    --tag "$image" \
    . >&2

printf '%s\n' "$image"
