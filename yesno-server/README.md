# yesno-server

`yesno-server` contains the [yesnodb](../README.md) daemon and data client:

- `yesnod`, the Arrow Flight database daemon and replication endpoint;
- `yesno`, the command-line client for ingest, queries, statistics, and status.

The `yesnoctl` administrative CLI and independently deployable `yesno-archive`
sidecar live in [`yesno-server-utils`](../yesno-server-utils/README.md), keeping
object storage dependencies out of the daemon crate. Filesystem snapshot
creation is owned here because it must be coordinated with the live database's
checkpoint lifecycle; the utility receives an opaque lease over Protobuf.

The shared library owns configuration, authentication and authorization, TLS,
metrics, checkpoint maintenance, follower lifecycle, and orderly shutdown.
Lifecycle events and commands share one Protobuf gRPC endpoint with WAL and
image replication. Ordered `[[auth.rule]]` rows match principal, source CIDR,
and capability (`control-read`, `control-admin`, or `replication`); the
first match decides and no match denies.
Configuration validation rejects an unauthenticated non-loopback endpoint
unless the operator explicitly chooses insecure operation.

Build the binaries or inspect their command surfaces from the repository root:

```console
cargo build -p yesno-server
cargo run -p yesno-server --bin yesnod -- --help
cargo run -p yesno-server --bin yesno -- --help
```

The root [getting-started guide](../docs/getting-started.md) owns setup and
first-use instructions. Use the [operations guide](../docs/operations.md) for
configuration, TLS, backup, replication, and recovery procedures. Current
deployment limitations are listed in the [workspace README](../README.md).
