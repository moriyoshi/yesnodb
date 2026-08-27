#!/usr/bin/env bash
#
# The all-in-one E2E image has one entrypoint and two entry identities.
#
# The database and search gates run initdb, mysqld, OpenSearch and
# Elasticsearch, every one of which refuses to run as root, and they must leave
# the baked artifact tree owned by the identity that built it. They enter as
# `yesno-builder`, which is the image default and needs no `--user`.
#
# The operator and filesystem scenarios drive a bind-mounted Docker socket and
# /dev/kvm and start no server in this container, so they enter as root with an
# explicit `--user 0:0`. Checking by argument keeps that split visible here
# rather than as an unexplained initdb failure deep inside a gate.

set -euo pipefail

# A leading flag means the caller is addressing the harness, which is what the
# per-integration image this replaced accepted directly. Without this, `docker
# run <image> --timeout 1200 x.py` would reach bash's `exec` builtin and fail
# with its usage text instead of running the scenario.
if [[ "${1:-}" == -* ]]; then
    set -- /usr/local/bin/yesno-e2e "$@"
fi

case "${1:-}" in
    ./scripts/gate-pg.sh | ./scripts/gate-mysql.sh | ./scripts/gate-search.sh)
        if [[ "$(id -u)" != 10001 || "$(id -g)" != 10001 ]]; then
            printf '%s must run as uid/gid 10001, not %s:%s\n' \
                "$1" "$(id -u)" "$(id -g)" >&2
            exit 1
        fi
        ;;
esac

exec "$@"
