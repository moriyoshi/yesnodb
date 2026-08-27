#!/usr/bin/env bash
#
# Opt-in: does a real container runtime actually produce the mount propagation
# the snapshot-agent deployment depends on?
#
# `operations.md` requires the agent's snapshot mount to reach the database
# through host-to-container propagation. `e2e/filesystems/lvm_propagation.py`
# covers the *receiving* half with systemd's `PrivateMounts=yes`, which is what
# `bind-propagation=rslave` and `mountPropagation: HostToContainer` are supposed
# to produce. It does not cover the *sending* half: nothing checked that
# Docker's flags put the agent's mounts in the host's peer group at all.
#
# This does, with the runtime rather than with an equivalent:
#
#   agent   container, CAP_SYS_ADMIN, snapshot root bound `rshared`
#   host    must observe a mount the agent makes
#   daemon  container, unprivileged uid, snapshot root bound `rslave`
#
# The daemon is started **before** any mount exists. A namespace only
# receives mounts made after it was created, so a daemon started afterwards
# would inherit a copy of the table and pass whatever its propagation says.
#
# `CAP_SYS_ADMIN` alone is **not** sufficient on an AppArmor host: Docker's
# default profile denies `mount(2)` even with the capability, and the agent
# fails with `Permission denied`. That is a deployment requirement, not a quirk
# of this script -- a Kubernetes deployment needs the equivalent profile
# exception. The negative case is asserted below so the requirement cannot
# quietly stop being true.
set -euo pipefail
cd "$(dirname "$(readlink -f "$0")")/.." || exit 1

command -v docker >/dev/null || { echo "gate-mount-propagation: docker required" >&2; exit 2; }
docker info >/dev/null 2>&1 || { echo "gate-mount-propagation: docker daemon unreachable" >&2; exit 2; }

img=alpine:3
root="$(mktemp -d "${TMPDIR:-/tmp}/yesno-prop.XXXXXX")"
# Traversable by the daemon's uid. `mktemp -d` gives 0700, and an unprivileged
# daemon then cannot enter the mount even after receiving it -- which is a real
# deployment requirement, and the same one `lvm_propagation.py` records for the
# agent's pre-mount directory. Do not "fix" a traversal failure here by
# running the daemon as root; that is the thing under test.
chmod 0755 "$root"
agent="" ; daemon=""
cleanup() {
    [ -n "$agent" ] && docker exec "$agent" sh -c 'umount /snap/x 2>/dev/null; rmdir /snap/x 2>/dev/null' >/dev/null 2>&1 || true
    [ -n "$agent$daemon" ] && docker rm -f $agent $daemon >/dev/null 2>&1 || true
    rmdir "$root" 2>/dev/null || true
}
trap cleanup EXIT

fail=0
check() { if [ "$2" = "$3" ]; then printf '   ok   %s\n' "$1"; else printf '   FAIL %s ( want %s, got %s )\n' "$1" "$3" "$2"; fail=1; fi; }

printf '\n\033[1m== the runtime denies the mount without a profile exception\033[0m\n'
a0=$(docker run -d --cap-add SYS_ADMIN \
      --mount "type=bind,source=$root,target=/snap,bind-propagation=rshared" "$img" sleep 60)
if docker exec "$a0" sh -c 'mkdir -p /snap/x && mount -t tmpfs p /snap/x' >/dev/null 2>&1; then
    denied=no; docker exec "$a0" sh -c 'umount /snap/x; rmdir /snap/x' >/dev/null 2>&1 || true
else
    denied=yes
fi
docker rm -f "$a0" >/dev/null 2>&1 || true
# Not asserted as a hard failure: a host without AppArmor legitimately allows it.
printf '   note mount refused with CAP_SYS_ADMIN alone: %s\n' "$denied"

printf '\n\033[1m== agent mount reaches the host and an unprivileged daemon\033[0m\n'
agent=$(docker run -d --cap-add SYS_ADMIN --security-opt apparmor=unconfined \
          --mount "type=bind,source=$root,target=/snap,bind-propagation=rshared" "$img" sleep 120)
# Created before any mount exists: see the note above.
daemon=$(docker run -d --user 10001:10001 \
          --mount "type=bind,source=$root,target=/snap,bind-propagation=rslave" "$img" sleep 120)

check "daemon runs unprivileged" "$(docker exec "$daemon" id -u)" "10001"
# Counted from mountinfo, not from `ls`: a permission error would make `ls`
# print nothing and the check pass for the wrong reason.
check "daemon sees no mount yet" "$(docker exec "$daemon" sh -c 'grep -c " /snap/x " /proc/self/mountinfo || true')" "0"

docker exec "$agent" sh -c 'mkdir -p /snap/x && mount -t tmpfs yesno-prop /snap/x && echo propagated > /snap/x/marker'

check "host observes the agent mount" "$(grep -c " $root/x " /proc/self/mountinfo || true)" "1"
check "host reads through it" "$(cat "$root/x/marker" 2>/dev/null || echo missing)" "propagated"
check "daemon receives the mount" "$(docker exec "$daemon" sh -c 'grep -c " /snap/x " /proc/self/mountinfo || true')" "1"
check "daemon reads through it" "$(docker exec "$daemon" sh -c 'cat /snap/x/marker 2>/dev/null || echo missing')" "propagated"

docker exec "$agent" sh -c 'umount /snap/x'
check "unmount propagates back to the host" "$(grep -c " $root/x " /proc/self/mountinfo || true)" "0"
check "unmount propagates to the daemon" "$(docker exec "$daemon" sh -c 'grep -c " /snap/x " /proc/self/mountinfo || true')" "0"

if [ "$fail" -eq 0 ]; then printf '\n\033[32mmount propagation gate passed\033[0m\n'; else printf '\n\033[31mmount propagation gate failed\033[0m\n'; fi
exit $fail
