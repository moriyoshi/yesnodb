#!/bin/sh
#
# Multi-call dispatcher for the unified yesnodb image.
#
# ---------------------------------------------------------------------------
# Why a dispatcher exists at all
# ---------------------------------------------------------------------------
#
# The three deployment targets disagree about which half of the argv they are
# allowed to set, and one image has to satisfy all three:
#
#   * **Kubernetes.** A `YesnoCluster` supplies `args` and no `command`, exactly
#     as a user's own manifest does, so the image must start `yesnod` by itself.
#     That argues for `ENTRYPOINT ["yesnod"]`.
#
#   * **ECS.** A container override sets `command`, which replaces CMD and
#     leaves ENTRYPOINT in place -- and the override the archiver builds for the
#     deferred-materialization worker begins with a *binary name*,
#     `yesno-snapshot-stage`. Under `ENTRYPOINT ["yesnod"]` that would run
#     `yesnod yesno-snapshot-stage --source ...`, which is the same failure the
#     per-role images avoided by shipping a second ENTRYPOINT-less stage.
#
#   * **Local Docker.** `docker run <image> --data-dir ...` appends to
#     ENTRYPOINT and expects `yesnod`.
#
# Making ENTRYPOINT this script satisfies all three at once: a first argument
# naming one of the shipped binaries selects it, and anything else is `yesnod`'s
# own argv. The two readings can never collide, because every `yesnod` option is
# a `--flag` and it takes no bare positional argument -- so a bare word in the
# first position is unambiguously a binary name. A unit test in the workspace
# pins that property.
#
# ---------------------------------------------------------------------------
# `exec`, everywhere, without exception
# ---------------------------------------------------------------------------
#
# `yesnod` must receive SIGTERM to drain readers and take its final checkpoint.
# Every branch below therefore `exec`s, so this shell *becomes* the target and
# the container's PID 1 is the binary itself -- there is no supervisor left in
# the middle to swallow the signal.
#
# Do not add a trailing command, a cleanup trap, or a `wait` to this file.
# Any of them turns the `exec` into a fork and reintroduces exactly the problem
# the exec form of ENTRYPOINT exists to avoid.

set -eu

BIN_DIR=/usr/local/bin

case "${1:-}" in
    # A shipped binary by name: what an ECS `command` override supplies.
    yesnod | yesno | yesnoctl | yesno-archive | yesno-snapshot-stage | \
        yesno-snapshot-agent | yesno-operator)
        command_name="$1"
        shift
        exec "${BIN_DIR}/${command_name}" "$@"
        ;;

    # An absolute path: run it verbatim. This is what `docker run <image>
    # /bin/sh` means, and it is also the long form of the case above. Anyone who
    # can set `command` could already have set `entryPoint`, so this grants no
    # authority that the runtime did not already hand them.
    /*)
        exec "$@"
        ;;

    # Anything else -- a `--flag`, or nothing at all -- is yesnod's own argv.
    # This is the Kubernetes contract and the plain `docker run` one.
    *)
        exec "${BIN_DIR}/yesnod" "$@"
        ;;
esac
