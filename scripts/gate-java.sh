#!/usr/bin/env bash
# The Java client against a live `yesnod`, inside the shared E2E image.
#
# `yesno-flight-java/gate.sh` compiles the client, runs its unit tests and runs
# javadoc. What it cannot do is talk to a server: its only server is a fake in
# its own test sources, and a fake that mints tickets from the client's own
# constants agrees with the client by construction. That is how the module
# stayed green through the ticket header widening from 40 to 48 bytes while the
# Go and Python integration suites failed every test.
#
# This gate is the missing half. It needs a JDK, the staged client and a daemon
# in one place, which is exactly what the shared image has -- so it is a
# container gate rather than a line in `scripts/gate.sh`.

set -euo pipefail

cd "$(dirname "$(readlink -f "$0")")/.." || exit 1

if [[ "${1:-}" != "--container-internal" ]]; then
  exec ./scripts/run-database-gate.sh java "$@"
fi
shift

if [[ $# -gt 0 ]]; then
  printf 'usage: %s\n' "$0" >&2
  exit 2
fi

# The runner the image already carries, rather than `cargo run`. The image
# keeps no `target/` -- it is a build-time cache mount -- so a `cargo run` here
# would recompile the whole workspace before the first assertion. The installed
# binary was built from this same source copy, and its `CARGO_MANIFEST_DIR`
# resolves to `/workspace`, which is where the staged client lives.
exec /usr/local/bin/yesno-e2e --timeout 600 "$PWD/e2e/java/client.py"
