# Plugin Hosting and Isolation Synthesis

## Summary

`yesnod` hosts read-only plugin peers through a Unix socket while keeping the database directory and snapshots under daemon ownership. The socket is the lifetime and revocation boundary; a sealed `memfd` arena or inline frames carry container payloads. The operator places a peer in the same Pod without giving it the data directory. The former in-process `cdylib` contract is archived only as history.

## Included Documents

| Document | Focus |
|---|---|
| [Plugin Channel Protocol and Security](./plugin-channel-protocol-and-security.md) | Frames, snapshot handles, arena and inline paths, admission, access, and rebootstrap. |
| [Out-of-Process Plugin and Foreign-Reader Boundary](./out-of-process-plugin-via-foreign-reader.md) | Why the served channel replaced shared-directory access; the separate foreign-reader liveness fix. |
| [Making a Plugin Container Available Through the Operator](./operator-hosted-plugin-container-plan.md) | Sidecar topology, config precedence, readiness, and operator tests. |
| [Who Pays for a Touched Arena Page](./arena-cgroup-charging.md) | Measured shared-memory charging and its unresolved phase-C explanation. |
| [Plugin Shape Performance](./plugin-shape-performance.md) | Copy and round-trip costs, batching, and direct arena encoding. |
| [Removed: the in-process cdylib Plugin ABI](./removed-cdylib-plugin-abi.md) | Archived published header and reasons for removal. |

## Stable Knowledge

- The current plugin path is the `yesnod`-served channel. Peers hold connection-scoped snapshot handles; closing a socket lets the daemon drop them without a peer callback. A rebootstrap empties the database slot before disconnecting peers, preventing newly admitted connections from taking an old snapshot.
- The channel is read-only and does not grant the peer filesystem access to the database. Write batches use Flight `PUT_APPLY`. A write-capable channel would need transaction identity and an answer for disconnect after commit.
- A container lane is at most 8,192 bytes. The arena locates lane `i` at `arena_off + i * LANE_BYTES`; descriptors carry kind and count. Arena creation can fail, so the server selects an inline fallback whose advertised width must fit its frame limit. Stored run pairs `( start, len_minus_1 )` must become inclusive `[start, end]` on the wire.
- An atomic peer slot is reserved before thread or arena creation. Each session also caps snapshots; a held snapshot consumes the database-wide reader registry and pins reclamation. Socket mode and `SO_PEERCRED` are independent checks, and a failed credential query is a refusal.
- The operator's `spec.plugin` is a same-Pod sidecar with a shared socket `emptyDir` at `/run/yesno`, not a mount of the database directory. It uses a normal second container that retries `UNAVAILABLE`: the socket can bind before the database is ready and during rebootstrap. A successful connect is not readiness.
- The default peer UID is 10001. A different UID is valid when `channel.allowUids` includes it and the socket mode permits access. The operator source note's later test bullet still says any non-10001 UID is rejected; its earlier correction and the current `plugin_spec_error` implementation establish the narrower rule.
- A sparse default arena reserves 512 MiB of address space per connection (`4 * 16 * 1024 * 8192`), while touched pages consume memory. A 256 MiB two-container experiment showed the same 128 MiB half charged in full to both cgroups after the peer read it. Size both containers for touched arena pages; the later phase-C accounting anomaly remains unexplained.
- The in-process `cdylib` ABI was removed. Its shared address space, process-aborting escaped panics, and leases that a host call could not revoke outweighed its no-copy advantage. The older shared-directory `Db::open_reader` exploration is also separate from the served plugin path: it sees checkpoint state, needs a writable `READERS` mount, and now uses per-slot `flock` alongside the legacy pid check for PID-namespace-safe liveness.

## Operational Guidance

Keep the daemon as sole database owner. Configure the channel through `YESNOD_PLUGIN_CHANNEL_*` overrides when an external config Secret prevents the operator from generating `[plugin]`; an absent inline flag must not overwrite a file's `true`. Keep peer readiness tied to the peer's own service, and let it reconnect after `UNAVAILABLE` or EOF. The operator's sidecar phases are marked done later in their source note, despite that note opening as an unimplemented plan; use the later status and current code when changing deployment behavior.

Treat protocol structure, authentication, and lifecycle as one boundary. Reserve admission before allocation, validate file mode and peer uid, refuse a live socket pathname, and disconnect rather than retry a partially written frame. Do not hold a peer-list or response-writer lock across an unbounded socket write. Keep run conversion in the shared encoder used by arena and inline destinations.

Performance evidence has distinct scopes. One-byte-per-lane access probes found unbatched socket scans 4.9-7.6x the removed table path; batching reduced the residual chiefly to copying, but did not price a real scorer. Direct arena encoding cut daemon CPU from about 4.1 to 3.2 seconds and peak memory from about 41 to 31 MiB in a separate-process search run; its wall-clock cells failed the quiet-host gate. A matched latency claim remains open.

## Files

- `yesno-plugin/src/{ipc,channel}.rs` - frame contract, sessions, arenas, descriptors, and socket service.
- `yesno-server/src/plugin.rs` - channel configuration, access, and lifecycle wiring.
- `yesno-operator/src/{api,resources,controller}.rs` - `PluginSpec`, same-Pod resources, and validation.
- `yesno-plugin/tests/peer_process.rs` and `yesno-server/tests/plugin_channel.rs` - process and socket boundary regressions.
- `yesno-core/src/db/readers.rs` - separate shared-directory foreign-reader registry and slot locks.

## Tests

- `cargo test -p yesno-plugin` covers codec truncation, lane and snapshot handling, and a real peer process.
- `cargo test -p yesno-server` covers the real socket, credentials, concurrent admission, and peer revocation.
- `./scripts/gate-operator.sh` checks that the CRD produces a sidecar that reaches a real snapshot through the socket. The unit test for watching a post-start write belongs in `yesno-plugin/tests/peer_process.rs`.
- `cargo test -p yesno-core` includes the foreign-reader liveness regression, which must fail if the lock half is removed.

## Pitfalls

- A unit-tested `Session::new_inline` says nothing about whether `yesnod` selects it on arena failure. Test the running server.
- A sent notification does not release a silent peer's snapshot; socket closure does.
- A socket path or mode does not authenticate a uid. Check both mode and `SO_PEERCRED`.
- A shared-directory reader and a served-channel peer have different freshness, filesystem, and reclamation contracts. Do not transfer the old PID-namespace hazard onto the served channel.
- Do not quote access-only ratios or noisy wall-clock data as whole-query latency.

