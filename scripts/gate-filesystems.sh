#!/usr/bin/env bash
#
# Opt-in native-filesystem snapshot test. Docker supplies isolation; KVM runs a
# bootable guest and a stateful Winterbaume S3 server in the all-in-one E2E
# image, so the host needs neither filesystem tools nor an external S3 account.

set -euo pipefail

cd "$(dirname "$(readlink -f "$0")")/.." || exit 1

if [[ ! -c /dev/kvm ]]; then
    echo "gate-filesystems: /dev/kvm must exist" >&2
    exit 2
fi

image="$(./scripts/build-e2e-image.sh)"

docker run --rm \
    --device /dev/kvm:/dev/kvm \
    --user 0:0 \
    "$image" \
    yesno-e2e \
    --timeout 1200 \
    e2e/filesystems/zfs.py \
    e2e/filesystems/btrfs.py \
    e2e/filesystems/lvm.py \
    e2e/filesystems/lvm_propagation.py
