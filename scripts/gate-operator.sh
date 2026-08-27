#!/usr/bin/env bash
#
# Opt-in live Kubernetes operator test. The host contract is only a working
# Docker daemon. The all-in-one image also carries the filesystem-test KVM guest
# and the database toolchains, but this scenario needs neither /dev/kvm nor
# filesystem privileges.

set -euo pipefail

cd "$(dirname "$(readlink -f "$0")")/.." || exit 1

driver_container="yesno-operator-e2e-$$"
image="$(./scripts/build-e2e-image.sh)"

run_args=(
    --rm
    --name "$driver_container"
    # The harness drives the bind-mounted daemon socket and starts no server in
    # this container, so it enters as root rather than as the image's default
    # `yesno-builder`. See `e2e/entrypoint.sh`.
    --user 0:0
    --volume /var/run/docker.sock:/var/run/docker.sock
    --env "YESNO_E2E_DRIVER_CONTAINER=$driver_container"
)

for name in YESNO_E2E_KEEP_KIND YESNO_E2E_KIND_NODE_IMAGE HTTP_PROXY HTTPS_PROXY NO_PROXY; do
    if [[ -v "$name" ]]; then
        run_args+=(--env "$name")
    fi
done

if [[ -v YESNO_E2E_CERT_MANAGER_MANIFEST ]]; then
    if [[ -f "$YESNO_E2E_CERT_MANAGER_MANIFEST" ]]; then
        cert_manager_manifest="$(readlink -f "$YESNO_E2E_CERT_MANAGER_MANIFEST")"
        run_args+=(
            --volume "$cert_manager_manifest:/e2e/cert-manager.yaml:ro"
            --env YESNO_E2E_CERT_MANAGER_MANIFEST=/e2e/cert-manager.yaml
        )
    else
        run_args+=(--env YESNO_E2E_CERT_MANAGER_MANIFEST)
    fi
fi

docker run "${run_args[@]}" "$image"
