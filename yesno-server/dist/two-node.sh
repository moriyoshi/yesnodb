#!/usr/bin/env bash
#
# A leader and a standby, on one host, over TLS, ending in a failover.
#
# This is a **demonstration**, not a test. It is deliberately not wired into
# `scripts/gate.sh`: it needs two processes and real ports, and the gate asserts
# its own step count. What the gate covers instead is `yesno-server/tests/` and
# `e2e/scenarios/failover.py`, which drive the same code in one process.
#
# What it is for is the thing those cannot show: that the shipped binaries, the
# example configs and the certificate script actually fit together, and what an
# operator sees while they do.
#
#   cargo build --release -p yesno-server -p yesno-server-utils
#   ./yesno-server/dist/two-node.sh
#
# Environment: YESNOD / YESNO / YESNOCTL to point at binaries, WORKDIR to keep
# the output. Everything else is chosen so two runs cannot collide.

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"
YESNOD="${YESNOD:-$root/target/release/yesnod}"
YESNO="${YESNO:-$root/target/release/yesno}"
YESNOCTL="${YESNOCTL:-$root/target/release/yesnoctl}"
[ -x "$YESNOD" ] || YESNOD="$root/target/debug/yesnod"
[ -x "$YESNO" ] || YESNO="$root/target/debug/yesno"
[ -x "$YESNOCTL" ] || YESNOCTL="$root/target/debug/yesnoctl"
for b in "$YESNOD" "$YESNO" "$YESNOCTL"; do
  [ -x "$b" ] || { echo "two-node: $b not built; run cargo build -p yesno-server -p yesno-server-utils" >&2; exit 1; }
done

work="${WORKDIR:-$(mktemp -d)}"
keep="${WORKDIR:+yes}"
mkdir -p "$work"
say() { printf '\n\033[1m== %s\033[0m\n' "$*"; }

pids=()
cleanup() {
  for p in "${pids[@]:-}"; do kill "$p" 2>/dev/null || true; done
  for p in "${pids[@]:-}"; do wait "$p" 2>/dev/null || true; done
  if [ -z "$keep" ]; then rm -rf "$work"; else echo "kept $work"; fi
}
trap cleanup EXIT

# Ports chosen from the pid so two runs on one host do not collide. Not from
# $RANDOM: a rerun after a crash should reuse the same ports rather than leave a
# different set half-bound.
base=$(( 20000 + (($$ * 7) % 20000) ))
L_FLIGHT=$base L_REPL=$((base+1)) L_METRICS=$((base+2))
S_FLIGHT=$((base+3)) S_METRICS=$((base+4))

say "certificates"
"$here/gen-dev-certs.sh" "$work/tls" >/dev/null
tls="$work/tls"
replica_sha="$(cat "$tls/standby-b.sha256")"
client_sha="$(cat "$tls/client.sha256")"
ops_sha="$(cat "$tls/ops.sha256")"

say "configuration"
mkdir -p "$work/a" "$work/b"
rm -f "$work/leader.toml"
cat > "$work/leader.toml" <<EOF
[server]
role     = "leader"
data_dir = "$work/a"

[server.flight]
listen = "127.0.0.1:$L_FLIGHT"

[server.flight.tls]
cert      = "$tls/server.pem"
key       = "$tls/server.key"
client_ca = "$tls/ca.pem"
# Not required: a caller may present a certificate, or a bearer token, or — if
# no principals were configured at all — nothing. Here one is configured, so
# anonymous callers are refused.
require_client_auth = false

[server.control]
listen = "127.0.0.1:$L_REPL"
journal_dir = "$work/a-control"

[server.control.tls]
cert                = "$tls/server.pem"
key                 = "$tls/server.key"
client_ca           = "$tls/ca.pem"
require_client_auth = true

[server.metrics]
listen = "127.0.0.1:$L_METRICS"

[db]
shards = 2

[db.checkpoint]
interval_secs = 5

# The standby is recognised by its certificate's DER digest. Its role grants no
# Flight permission; the ordered rule below grants only replication.
#
[[auth.principal]]
name        = "standby-b"
role        = "replica"
cert_sha256 = "$replica_sha"

[[auth.principal]]
name        = "app"
role        = "writer"
cert_sha256 = "$client_sha"

[[auth.principal]]
name        = "ops"
role        = "admin"
cert_sha256 = "$ops_sha"

[[auth.rule]]
principal  = "ops"
address    = "127.0.0.0/8"
capability = "control-admin"
action     = "allow"

[[auth.rule]]
principal  = "standby-b"
address    = "127.0.0.0/8"
capability = "replication"
action     = "allow"

EOF

rm -f "$work/standby.toml"
cat > "$work/standby.toml" <<EOF
[server]
role     = "follower"
data_dir = "$work/b"

[server.flight]
listen = "127.0.0.1:$S_FLIGHT"

[server.flight.tls]
cert      = "$tls/server.pem"
key       = "$tls/server.key"
client_ca = "$tls/ca.pem"
require_client_auth = false

[server.metrics]
listen = "127.0.0.1:$S_METRICS"

# The standby carries the **same** principal table as the leader, because
# after a promotion it *is* the leader and a client that could reach one must be
# able to reach the other. A table that differs between the two turns a failover
# into an outage for whoever is missing from it.
[[auth.principal]]
name        = "app"
role        = "writer"
cert_sha256 = "$client_sha"

[[auth.principal]]
name        = "ops"
role        = "admin"
cert_sha256 = "$ops_sha"

[follower]
leader             = "https://localhost:$L_REPL"
poll_interval_secs = 1
serve_reads        = true

[follower.tls]
ca   = "$tls/ca.pem"
cert = "$tls/standby-b.pem"
key  = "$tls/standby-b.key"
EOF

# What ExecStartPre= runs. It opens no database, so it is safe at any time.
"$YESNOD" --config "$work/leader.toml"  --check-config >/dev/null
"$YESNOD" --config "$work/standby.toml" --check-config >/dev/null
echo "both configurations resolve and validate"

# Every client call carries the same identity, which is the point of the
# `app` principal above.
ctl() {                           # ctl <port> <args...>       — the `app` writer
  local port="$1"; shift
  "$YESNO" --endpoint "https://localhost:$port" \
    --ca "$tls/ca.pem" --cert "$tls/client.pem" --key "$tls/client.key" "$@"
}
ops() {                           # ops <port>                 — the `ops` admin
  local port="$1"; shift
  "$YESNOCTL" checkpoint --endpoint "https://localhost:$port" \
    --ca "$tls/ca.pem" --cert "$tls/ops.pem" --key "$tls/ops.key" "$@"
}
app_checkpoint() {                # app_checkpoint <port>      — refused writer
  local port="$1"; shift
  "$YESNOCTL" checkpoint --endpoint "https://localhost:$port" \
    --ca "$tls/ca.pem" --cert "$tls/client.pem" --key "$tls/client.key" "$@"
}

wait_for() {                      # wait_for <url> <substring> <what>
  for _ in $(seq 1 100); do
    if curl -fsS "$1" 2>/dev/null | grep -q "$2"; then return 0; fi
    sleep 0.2
  done
  echo "two-node: timed out waiting for $3" >&2; return 1
}

say "the leader"
"$YESNOD" --config "$work/leader.toml" >"$work/leader.log" 2>&1 & pids+=($!)
leader_pid=${pids[-1]}
wait_for "http://127.0.0.1:$L_METRICS/readyz" . "the leader to become ready"
echo "serving on https://localhost:$L_FLIGHT, shipping on 127.0.0.1:$L_REPL"

say "ingest"
seq 0 4999 | awk '{print "42," $1*3}' > "$work/pairs.csv"
ctl "$L_FLIGHT" put "$work/pairs.csv"
ops "$L_REPL"
echo "leader holds $(ctl "$L_FLIGHT" count 42) ordinals under key 42"

# Two identities, two roles, and the refusal is part of the demonstration:
# checkpoint is an admin RPC and `app` is a writer. 7 permission_denied and
# not 16 unauthenticated — the server knows exactly who this is, and a different
# credential is the only thing that would help.
app_checkpoint "$L_REPL" || true

say "the standby"
"$YESNOD" --config "$work/standby.toml" >"$work/standby.log" 2>&1 & pids+=($!)
standby_pid=${pids[-1]}
wait_for "http://127.0.0.1:$S_METRICS/metrics" "yesnod_follower_connected 1" "the standby to connect"
# Wait for a *completed* pass, not merely for records: a sweep opens the
# database partway through, so asking earlier races the bootstrap.
wait_for "http://127.0.0.1:$S_METRICS/metrics" "yesnod_follower_passes_total [1-9]" "a full catch-up pass"
echo "standby serves $(ctl "$S_FLIGHT" count 42) ordinals, while following"

say "a write reaches it live"
echo "42,999999" > "$work/one.csv"
ctl "$L_FLIGHT" put "$work/one.csv" >/dev/null
for _ in $(seq 1 60); do
  n=$(ctl "$S_FLIGHT" count 42 2>/dev/null || echo 0)
  [ "$n" = 5001 ] && break
  sleep 0.5
done
echo "standby now serves $n"

say "certificate rotation, without a restart"
# Both of the leader's listeners point at the same `server.pem`, so one
# SIGHUP rotates both. Nothing here restarts a process, and nothing here touches
# the database.
served_cert() {                   # served_cert <port>  — the DER digest on the wire
  openssl s_client -connect "127.0.0.1:$1" -servername localhost </dev/null 2>/dev/null \
    | openssl x509 -outform DER 2>/dev/null \
    | openssl dgst -sha256 -hex | awk '{print $NF}'
}
wait_log() {                      # wait_log <file> <pattern> <what>
  for _ in $(seq 1 100); do
    if grep -q "$2" "$1" 2>/dev/null; then return 0; fi
    sleep 0.2
  done
  echo "two-node: timed out waiting for $3" >&2; return 1
}
before="$(served_cert "$L_FLIGHT")"
echo "presenting ${before:0:16}…"

# The refusal first, because it is the case that turns a routine rotation into
# an outage. A truncated PEM is what a half-finished copy looks like.
cp "$tls/server.pem" "$work/server.pem.good"
rm -f "$tls/server.pem"
head -c 120 "$work/server.pem.good" > "$tls/server.pem"
kill -HUP "$leader_pid"
wait_log "$work/leader.log" "TLS reload refused" "the leader to refuse the broken certificate"
grep -o "TLS reload refused.*" "$work/leader.log" | head -1 | sed 's/^/  /' || true
[ "$(served_cert "$L_FLIGHT")" = "$before" ] || {
  echo "two-node: a refused rotation changed the certificate" >&2; exit 1; }
echo "refused, and the leader still holds $(ctl "$L_FLIGHT" count 42) ordinals"

# And now a real rotation: a new leaf under the **same** CA, so no client has to
# be told anything.
rm -f "$tls/server.pem" "$tls/server.key" "$tls/server.csr"
(
  cd "$tls"
  openssl req -newkey rsa:2048 -nodes -keyout server.key -subj "/CN=server" \
    -out server.csr 2>/dev/null
  openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -CAcreateserial \
    -days 30 -sha256 -out server.pem \
    -extfile <(printf 'basicConstraints=CA:FALSE\nsubjectAltName=DNS:localhost,IP:127.0.0.1\n') 2>/dev/null
  rm -f server.csr
  chmod 600 server.key
)
kill -HUP "$leader_pid"
wait_log "$work/leader.log" "TLS material reloaded" "the leader to reload"
after="$(served_cert "$L_FLIGHT")"
[ -n "$after" ] && [ "$after" != "$before" ] || {
  echo "two-node: SIGHUP did not change the certificate on the wire" >&2; exit 1; }
echo "now presenting ${after:0:16}… — same process, same pid $leader_pid"
echo "clients are unaffected: $(ctl "$L_FLIGHT" count 42) ordinals, and the standby is still following"

say "a replica refuses writes"
ctl "$S_FLIGHT" put "$work/one.csv" || true

say "failover"
# In production this step is STONITH, or revoking a VIP, or a human who has
# confirmed the machine is down. Nothing in yesno establishes that the old
# leader is gone, and promoting while it still serves is the split brain the
# leadership term detects but cannot prevent.
kill -TERM "$leader_pid"; wait "$leader_pid" 2>/dev/null || true
echo "old leader stopped"

kill -USR1 "$standby_pid"
for _ in $(seq 1 100); do
  curl -fsS "http://127.0.0.1:$S_METRICS/readyz" 2>/dev/null | grep -qi "serving" && break
  sleep 0.2
done
echo "promoted; it now holds $(ctl "$S_FLIGHT" count 42) ordinals"
ctl "$S_FLIGHT" status | sed 's/^/  /'

say "and it accepts writes"
echo "42,888888" > "$work/two.csv"
ctl "$S_FLIGHT" put "$work/two.csv"
echo "final count: $(ctl "$S_FLIGHT" count 42)"

say "done"
# No early `exit` in the awk. `yesno` restores the default SIGPIPE
# disposition, so a consumer that stops reading kills it with signal 13 — and
# under `set -o pipefail` that takes the whole script down. Read to the end.
term=$(ctl "$S_FLIGHT" status | awk '$1=="term"{t=$3} END{print t}')
echo "The leadership term is now $term, raised from 0 by the promotion."
echo "   The old leader still carries the lower one, so a standby pointed back at"
echo "   it will refuse to follow. It cannot rejoin without being wiped and"
echo "   re-bootstrapped: its uuid is identical, and only the term tells them apart."
