# Configuration and service lifecycle

This guide covers building `yesnod`, configuring its listeners, logging,
authentication and TLS, and starting and stopping it. The
[operations overview](index.md) states the operating envelope every deployment
must stay inside.

## Build and first start

Build optimized binaries from the repository root:

```console
cargo build --release -p yesno-server -p yesno-server-utils
```

The resulting programs are `yesnod`, the `yesno` data CLI, and the `yesnoctl`
administrative CLI. A minimal local start is:

```console
./target/release/yesnod --data-dir ./yesnodb-data
```

The default Flight and metrics listeners bind loopback. A non-loopback Flight
bind without TLS or principals is refused unless the operator passes
`--insecure` explicitly.

Validate a configuration without opening the database or taking its lock:

```console
./target/release/yesnod --config /etc/yesno/yesnod.toml --check-config
```

Configuration precedence is built-in defaults, configuration file,
`YESNOD_*` environment variables, then command-line flags.

For a follower, `follower.archive_store` (or
`YESNOD_FOLLOWER_ARCHIVE_STORE` / `--follower-archive-store`) names an optional
object archive for its first startup. The standby reads the archive before it
opens its database and then catches up from `follower.leader`. The data
directory must be empty for seeding; a directory with a MANIFEST resumes its
existing replica state.

### The out-of-process plugin channel

A plugin peer runs as its own process and reaches `yesnod` over a Unix socket. It
never opens the database: the snapshots it reads belong to `yesnod` and are keyed
by its connection, so closing the socket releases them. A peer needs no access
to the data directory and cannot keep space pinned after it exits or is killed.
The channel also accepts writes on a leader. Treat access to the socket as
authority to read and change the whole database.

One `Apply` request carries at most 16,384 point inserts, point removals,
inclusive-range inserts, inclusive-range removals, or whole-key deletions. The
server validates the request, commits it as one batch, and replies with its
version and whether it changed data. These operations are individually
idempotent, so a peer can retry a request after losing the reply when no
conflicting write has interleaved. A follower refuses writes; a database slot
that is temporarily empty refuses them rather than holding a serving thread.
For an atomic write spanning several requests, use a Flight write transaction.

The channel is off until a socket path is set. These are settable from the
configuration file, the environment, or the command line, which matters where the
configuration file is supplied by something else -- an orchestrator that mounts it
from a secret cannot also edit it, and the socket path is chosen by whoever places
the peer beside the daemon:

| flag | variable | meaning |
|---|---|---|
| `--plugin-channel-socket` | `YESNOD_PLUGIN_CHANNEL_SOCKET` | socket path; empty disables the channel |
| `--plugin-channel-max-handles` | `YESNOD_PLUGIN_CHANNEL_MAX_HANDLES` | concurrent lane handles per connection |
| `--plugin-channel-max-lanes` | `YESNOD_PLUGIN_CHANNEL_MAX_LANES` | lanes one handle may hold |
| `--plugin-channel-max-blocks` | `YESNOD_PLUGIN_CHANNEL_MAX_BLOCKS` | blocks one response may carry |
| `--plugin-channel-inline` | `YESNOD_PLUGIN_CHANNEL_INLINE` | serve payloads in the frames, not a shared region |

These have no flags and are set in the configuration file:

| key | default | meaning |
|---|---|---|
| `channel_max_peers` | 8 | connections served at once; further ones are refused, not queued |
| `channel_max_snapshots` | 64 | snapshots one connection may hold open |
| `channel_socket_mode` | umask | permission bits for the socket, such as `"0600"` |
| `channel_allow_uids` | empty | extra user ids allowed to connect |

Two things an operator should know before setting them.

**The socket is bound before the database opens, so its existence is not
readiness.** A peer can connect immediately and will be told the database is
unavailable until startup finishes, and again for the duration of a rebootstrap. A
peer that treats the first such answer as fatal will restart on every start. Wait
for the daemon to log that it is ready, not for the socket to appear.

**The three limits together size a shared region, per connection.** It is sparse,
so the reservation is address space and the cost is the pages actually used -- but
the product of the three is what bounds it, and raising all three at once raises
it multiplicatively. Leave them alone unless a peer reports a limit it cannot work
within. A failure to bind the socket is fatal and says so, rather than leaving the
channel quietly absent.

**The socket is an access boundary, and it has no password.** A peer names itself
in its greeting, but that name is a label it chooses, not a credential -- so
whoever can open the socket can read and change the whole database. Two gates control that,
and they are not interchangeable. The file mode is the first: set
`channel_socket_mode = "0600"` on any shared or group-writable mount, because the
default is whatever the umask gives. The second is the user id, taken from the
kernel rather than from anything the peer says: by default only the id the daemon
runs as, and `root`, may connect, and `channel_allow_uids` widens that
deliberately. A mode cannot express "this user and no other in the group", and a
user check cannot stop someone who never gets as far as connecting, which is why
both exist.

**Two daemons must not share a socket path.** Binding refuses a path that is
already accepting connections, and refuses one that exists and is not a socket,
rather than replacing it. A socket left behind by a process that died is
recognised and reclaimed. This matters because the data directory lock does not
help here: two daemons with different data directories and one socket path would
otherwise silently serve different databases to whoever connected when.

## Logging and tracing

`yesnod` writes structured tracing output to standard error. The default filter
is `info`; set it with `--log FILTER` or `YESNOD_LOG`. Filters use the usual
target-and-level syntax, for example:

```console
YESNOD_LOG='yesno_core=debug,yesno_flight=debug,yesno_server=info' \
  ./target/release/yesnod --config /etc/yesno/yesnod.toml
```

Storage spans cover database open and recovery, checkpoint, verification,
commit, and live-replica WAL apply. Flight spans cover catalogue and query
preparation, streamed reads, bulk-ingest commits, and administrative actions.
The daemon emits a record when each span closes; `time.busy` and `time.idle`
therefore expose elapsed work around mmap faults, WAL synchronization,
checkpoint serialization, and other blocking boundaries. Routine per-commit
and per-WAL-batch details are at `debug`; lifecycle transitions, long-running
operations, aggregate row or byte counts, and failures are visible at `info` or
higher.

Trace fields intentionally exclude keys, ordinals, query expressions, and
Flight payloads. They include operation type, shard, version or LSN, row or byte
counts, and outcomes. Keep this rule when adding instrumentation: logs are an
operational surface, not a copy of user data.

OpenTelemetry export is disabled by default. Enable OTLP/gRPC trace export and
choose the SDK sampler in the server configuration:

```toml
[server.telemetry]
enabled = true
endpoint = "http://collector:4317"
service_name = "yesnod"
sampler = "parent_based_trace_id_ratio"
sample_ratio = 0.1
```

The sampler accepts `always_on`, `always_off`, `trace_id_ratio`,
`parent_based_always_on`, `parent_based_always_off`, and
`parent_based_trace_id_ratio`. A ratio is inclusive from 0 to 1 and is used
only by the two ratio policies; setting a non-default ratio on another policy
is a configuration error. Parent-based policies retain an upstream sampling
decision and use the configured inner sampler for a new root trace.

If `endpoint` is omitted, the exporter honors
`OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`, then `OTEL_EXPORTER_OTLP_ENDPOINT`, and
finally the OpenTelemetry SDK's local-collector default. The endpoint and
standard OTLP headers may therefore be supplied by the deployment environment
without putting collector credentials in the rendered configuration. The
`--log` filter controls standard-error formatting only; exported traces are
controlled by the SDK sampler. On graceful process exit, the daemon shuts down
the tracer provider and flushes its batch processor before returning.

## Shared control and replication endpoint

Set `server.control.listen`, `server.control.unix_socket`, or both, together
with `server.control.journal_dir` to enable the shared endpoint. Leaving all
three empty disables it; configuring a listener without the journal (or the
journal without a listener) is an error. Both listeners carry the same gRPC
lifecycle, event, WAL-shipping, and shard-image services. TLS applies to TCP;
filesystem ownership and permissions admit peers to the Unix socket, with
`server.control.unix_socket_mode` setting that mode explicitly when the default
umask-derived one is not what the deployment needs.
Keep the journal on a different failure domain from the database directory when
the deployment permits it, so a data-volume failure does not also erase the
event stream that reports it.

The gRPC services and durable journal carry typed Protocol Buffers, not JSON.
Consumers can request the current state projection, subscribe after a durable
sequence number, and request promotion, demotion, or graceful shutdown. A
command is journaled before it is delivered to the lifecycle loop. Invalid
same-role commands are rejected.

The journal appends CRC32C-framed envelopes and synchronizes each fact before
broadcasting it. It repairs only a torn final frame; corruption inside the valid
prefix stops startup. History is bounded at roughly 64 MiB by an atomically
published Protobuf projection checkpoint. A subscriber whose cursor predates
that checkpoint receives `ResyncRequired` with the current projection instead
of an incomplete replay. An unclosed prior process and database are recorded as
interruption facts on the next start.

## Minimal leader configuration

```toml
[server]
role = "leader"
data_dir = "/var/lib/yesno"

[server.flight]
listen = "127.0.0.1:50051"

[server.metrics]
listen = "127.0.0.1:9750"

[server.control]
listen = "127.0.0.1:50052"
journal_dir = "/var/lib/yesno-control"

[db]
shards = 8
max_readers = 4096
on_space_amp = "abort_oldest_reader"

[db.checkpoint]
dirty_bytes = "256MiB"
max_dirty_bytes = "512MiB"
wal_bytes = "1GiB"
max_wal_bytes = "4GiB"
interval_secs = 60

[auth]
anonymous = "none"

[[auth.rule]]
channel = "host"
principal = "all"
address = "127.0.0.0/8"
capability = "control-read"
action = "allow"

[[auth.rule]]
channel = "host"
principal = "all"
address = "127.0.0.0/8"
capability = "control-admin"
action = "allow"
```

The shard count is creation-only. Once a directory has a manifest, its
persisted routing map wins over configuration.

With no configured principals, a loopback-only server is deliberately open to
local clients; `anonymous = "none"` starts to deny unauthenticated calls once
at least one principal enables authentication. A non-loopback open bind is
refused unless `--insecure` explicitly acknowledges it.

## TLS and authentication

A publicly reachable service should configure a server certificate, its private
key, and at least one principal:

```toml
[server.flight]
listen = "0.0.0.0:50051"

[server.flight.tls]
cert = "/etc/yesno/tls/server.pem"
key = "/etc/yesno/tls/server.key"
client_ca = "/etc/yesno/tls/clients-ca.pem"
require_client_auth = false

[auth]
anonymous = "none"

[[auth.principal]]
name = "query"
role = "reader"
token_sha256 = "lowercase-sha256-of-token"

[[auth.principal]]
name = "ingest"
role = "writer"
cert_sha256 = "lowercase-sha256-of-client-certificate-der"
```

Roles are `reader`, `writer`, `admin`, and `replica`; they govern Flight. The
shared control endpoint has an independent, ordered `[[auth.rule]]` table. Its
columns are `channel`, principal, source address, and capability. Channel
values follow `pg_hba.conf`: `local`, `host`, `hostssl`, and `hostnossl`;
`host` matches either TCP variant. A local peer's kernel identity is exposed as
`uid:<number>`, so a local row can name that principal or use `all`. The address
column is ignored for local rows. Capabilities are `control-read`,
`control-admin`, and `replication`; the first match decides and no match denies.

Store bearer-token digests in configuration, never plaintext tokens. Clients
read the plaintext token from a file:

```console
yesno --token-file /run/secrets/yesno-token status
```

### Rotating certificates

`SIGHUP` re-reads the certificate, private key and client CA named by
`[server.flight.tls]` and `[server.control.tls]` and installs them for
connections accepted afterwards. The process is not restarted and the database
is not reopened, so a rotation costs no replay and no downtime. Connections
already established keep the material they negotiated with; a rotation drops no
traffic. Rotate by replacing the contents of the configured files and signalling:

```console
install -m0644 new-server.pem /etc/yesno/tls/server.pem
install -m0600 new-server.key /etc/yesno/tls/server.key
systemctl reload yesnod          # the shipped unit maps this to SIGHUP
```

The *paths* are read once, at start. Changing which file a listener reads is
still a restart; changing what is inside that file is not.

A reload is all or nothing. The server reads every configured file, parses the
certificate chain, parses the key, proves that the key belongs to the
certificate, and builds the client-authentication verifier — all before it
installs anything. If any of that fails, and a truncated copy, a key left over
from the previous certificate, or a client CA file with no usable certificate in
it all do fail, the reload is refused, the listener keeps the material it already
had, and the reason is logged at `error` level naming the file. A rotation with a
mistake in it is a log line, not an outage.

A refused reload is not something the signal can report back, so
`systemctl reload` succeeding means the signal was delivered, not that the
certificate changed. Confirm the rotation rather than assuming it:

```console
journalctl -u yesnod --since -1min | grep 'TLS '
openssl s_client -connect leader.internal:50051 -servername leader.internal \
  </dev/null 2>/dev/null | openssl x509 -noout -dates -fingerprint -sha256
```

Every configured listener reloads on one signal and each decides independently:
a Flight listener that accepts its new material is not held back by a control
listener that refuses its own.

Client-side material rotates without a signal. A follower reads the certificate,
key and CA under `[follower.tls]` each time it dials its leader, so replacing
those files takes effect at its next reconnection; command-line clients read
theirs once per invocation.

`SIGHUP` reloads TLS material and nothing else. Listen addresses, principals,
authorization rules, shard count and checkpoint policy are still read once, at
start, and changing any of them needs a graceful restart.

## Health and metrics

The metrics listener serves:

- `/healthz`: the process is alive;
- `/readyz`: the node is ready to answer its configured workload;
- `/metrics`: Prometheus text.

A follower rebuilding its local image remains healthy but is not ready.

The archive sidecar has its own listener, off unless started with
`--metrics-addr`. It serves `/metrics` and `/healthz` and reports what each
reclamation pass decided; see
[Retention and reclamation](backup.md#retention-and-reclamation).
Scrape no faster than every 15 seconds because gathering store gauges takes a
lock once per shard.

Use `yesno status` for identity, leadership term, actions, and core space
counters. The Flight `stats` action returns typed Protocol Buffers; `yesno stats`
decodes it to stable key-value fields for scripts.

## Graceful shutdown

Send `SIGTERM` or `SIGINT`. The server stops accepting work, drains readers up
to the configured grace period, takes a final checkpoint, and releases the
database lock.

Do not use overlapping rolling replacement against one data directory. Stop the
old process before starting the new one. In an orchestrator, use a recreate
strategy for a single-node volume.
