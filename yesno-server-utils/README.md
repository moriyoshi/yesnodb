# yesno-server-utils

`yesno-server-utils` contains the operational clients shipped alongside
`yesnod`:

- `yesnoctl`, the administrative CLI, with typed control-plane checkpoint, hot
  base-backup, and offline restore subcommands; and
- `yesno-archive`, a lifecycle-driven sidecar that continuously transfers WAL
  and checkpoint bases to local or S3-compatible object storage.

Base snapshots are created, leased, and cleaned up by `yesnod`. The default
provider stages a portable immutable copy; ZFS, Btrfs, local LVM, and EBS are
optional providers behind the same Protobuf protocol. Local LVM returns an
ordinary file-bearing lease. Only the archive sidecar has a dual-opt-in
same-mount direct-path optimization; `yesnoctl basebackup` always streams
bytes.

`yesnoctl basebackup` uses the same lease protocol. When no filesystem-native
backend is configured, `yesnod` supplies a portable staged snapshot; the utility
still never snapshots or copies the live database directory on its own.

Build or inspect them from the repository root:

```console
cargo build -p yesno-server-utils
cargo run -p yesno-server-utils --bin yesnoctl -- --help
cargo run -p yesno-server-utils --bin yesno-archive -- --help
```

The workspace [operations guide](../docs/operations.md) owns configuration,
backup, snapshot, and recovery procedures.
