# Making a Plugin Container Available Through the Operator

A plan, written 2026-09-29, for exposing the out-of-process plugin channel as a
first-class thing a `YesnoCluster` can declare -- so a peer like `haiiie` ships as
a container beside `yesnod` rather than as a hand-written Pod. Nothing here is
implemented yet. Every claim about the current tree below was read out of it
rather than recalled; the two places that rest on an assumption say so.

## The shape is a sidecar, and the transport decides that

The channel is a Unix socket, and the arena is a `memfd` passed over it with
`SCM_RIGHTS`. Two consequences fix the topology:

* **The peer must share a filesystem namespace with `yesnod`** for the socket path
  to name the same socket. In Kubernetes that means the same Pod and a shared
  volume. A plugin in its own Deployment cannot reach a Unix socket, so
  cross-Pod is not a configuration choice but a different transport ( see the
  last section ).
* **It must not share a PID namespace.** Liveness is socket closure, so the peer
  needs no pid and no `/proc`. This is the property that makes a sidecar clean,
  and it is worth naming as a thing to preserve: the moment something starts
  resolving the peer's pid, `shareProcessNamespace` becomes load-bearing and the
  namespace defect that `db::readers` had until 2026-09-29 comes back by another
  route.

And one thing the peer conspicuously does **not** need: access to the data
directory. `yesnod` is the only process that opens the database; the peer's
snapshots belong to `yesnod` and are keyed by connection. So the plugin container
mounts the socket volume and nothing else, and that should be **enforced by the
reconciler and asserted by a test**, not merely left out. An accidental `data`
mount would quietly discard a property nothing else is defending.

## The in-process plugin is not part of this

**Settled 2026-09-29: the `cdylib` path is gone**, removed from the tree the same
day. `spec.plugin` describes a container, and there is nothing else it could
describe.

The property that makes this the right and only extension point is worth stating
positively rather than as a comparison: **a peer in its own process cannot corrupt
`yesnod`'s address space and cannot abort it.** An escaped panic kills the peer,
its socket closes, its snapshots are released, and the daemon carries on. That is
not a mitigation of the in-process design's hazards, it is the absence of them,
and it is why the operator has a plugin story at all.

`PluginConfig::library` **was removed from the daemon's configuration on
2026-09-29**, along with the ABI behind it -- see
[the removal record](./removed-cdylib-plugin-abi.md). So there is no longer a
field for a CRD to reach even if someone wanted one, and an old `yesnod.toml`
naming a library fails to parse rather than being quietly ignored.

## Phase 0: the socket path must be settable outside the config file

**The flag and environment half of this is DONE ( 2026-09-29 ).** The peer binary
half is not; see the end of this section.

**This is the only change needed outside the operator, and it is the one that
decides whether the feature works in production.**

`config_source` generates `yesnod.toml` only when `spec.config.secretName` is
absent. When it is present -- which is the mTLS production path -- the operator
writes no config and so cannot add a `[plugin]` section. Without a second way to
set the socket, the sidecar would be available exactly where it is least wanted.

`yesno-server/src/config.rs` already documents the precedence chain
`defaults < config file < YESNOD_* environment < command line`, implemented with
`clap`'s `env` feature, so this is additive and idiomatic rather than new
mechanism:

| flag | variable | landed |
|---|---|---|
| `--plugin-channel-socket` | `YESNOD_PLUGIN_CHANNEL_SOCKET` | yes |
| `--plugin-channel-max-handles` | `YESNOD_PLUGIN_CHANNEL_MAX_HANDLES` | yes |
| `--plugin-channel-max-lanes` | `YESNOD_PLUGIN_CHANNEL_MAX_LANES` | yes |
| `--plugin-channel-max-blocks` | `YESNOD_PLUGIN_CHANNEL_MAX_BLOCKS` | yes |
| `--plugin-channel-inline` | `YESNOD_PLUGIN_CHANNEL_INLINE` | yes |

`channel_inline` is `Option<bool>` on the `Cli`, not `bool`, and that is the one
subtlety worth carrying: a plain flag is `false` when absent, so every start
without it would silently overwrite a file that had asked for `true` -- on a host
that cannot make a `memfd`, exactly the setting an operator chose deliberately. An
override must be able to say nothing. `an_absent_inline_flag_leaves_the_file_alone`
pins it and fails when the field is treated as a plain `bool`.

`Config::resolve` had **no tests at all** before this, so the precedence chain the
module documents was unverified; it now has four.

The rejected alternative was to have the operator refuse a plugin sidecar
whenever `secretName` is set. It is simpler and it makes the feature unavailable
in the only configuration a real deployment uses, which is worse than the
plumbing.

`--check-config` "opens nothing" by contract, so it stays usable as a validation
step with these added.

Phase 0 also needs **a minimal channel peer binary**, and the reason is narrower
than an earlier draft of this document claimed.

**Corrected 2026-09-29.** That draft said "nothing in the tree has ever spoken the
protocol over a real socket", which is **false**. `yesno-server`'s
`tests/plugin_channel.rs` connects a real `UnixStream`, receives the arena
descriptor through a real `SCM_RIGHTS` `recv_fd`, maps it, and exchanges encoded
frames -- so the socket, the descriptor handoff, the arena mapping, the
notifications and liveness-by-socket-close are all exercised today. Two earlier
`JOURNAL` entries say so explicitly ( "Only a real socket round trip showed it" ),
so this was a claim I had already contradicted before making it.

What is genuinely missing is narrower and still worth closing: **a peer in a
separate process**. Everything above happens inside one test binary, so nothing
covers a peer that is killed rather than dropped, an arena mapped across a real
process boundary, or a peer built without linking `yesno-core` -- which is the
property the deleted C fixture used to prove for the in-process ABI. And there is
no standalone example a third party can copy.

A small binary that greets, opens a snapshot, acquires lanes for a key, walks
blocks and reports the cardinality over HTTP is enough. It doubles as the worked
example a third party copies, which is the artefact a consumer actually asks for.
It must be added to the unified image's binary set, which
`scripts/check-image-binaries.py` verifies in both directions, so that file
changes in the same commit.

## Phase 1: `spec.plugin` and the sidecar

```yaml
spec:
  plugin:
    image: ghcr.io/example/haiiie:1.2.3
    imagePullPolicy: IfNotPresent
    args: ["--socket", "/run/yesno/plugin.sock"]
    env: [...]
    resources: { requests: {...}, limits: {...} }
    ports:
      - name: search
        containerPort: 8080
    channel:
      maxHandles: 4
      maxLanes: 1024
      maxBlocks: 16
      inline: false
    service:
      enabled: false
```

**The socket path is operator-owned and not user-settable.** It is an internal
contract between two containers the operator itself writes, so a field for it is
a way to get it wrong with nothing to gain: `/run/yesno/plugin.sock`, on an
`emptyDir` mounted at `/run/yesno` in both containers. The `emptyDir` is what
makes it writable despite `readOnlyRootFilesystem: true` on the `yesnod`
container, which is already set and must stay.

The plugin container gets the same hardening the `yesnod` container has, copied
rather than relaxed: `allowPrivilegeEscalation: false`, `capabilities.drop: [ALL]`,
`readOnlyRootFilesystem: true`, `runAsNonRoot: true`, `runAsUser`/`runAsGroup`
10001, and the Pod's `seccompProfile: RuntimeDefault`.

**It must run as UID 10001, and that is a requirement rather than a default.**
The plugin channel does not set a socket mode -- `unix_socket_mode` exists in the
config but the channel's `start` never applies one -- so the socket lands at the
process umask and owned by `yesnod`. Same-UID is what makes it reachable. If a
plugin image insists on another UID, that needs a socket-mode option on the
channel first; the plan is to reject the spec with a clear message rather than
produce a Pod whose containers cannot talk.

Generated config gains a `[plugin]` section; the `secretName` path relies on
Phase 0's environment variables. The reconciler should set the environment in
**both** cases, so there is one code path and the generated `[plugin]` section is
belt-and-braces rather than the mechanism.

## Phase 2: readiness, ordering, and the thing most likely to be got wrong

**Socket existence is not readiness.** `plugin::wire` binds the socket *before*
the database opens, so a peer can connect immediately and get `UNAVAILABLE` from
every request until `yesnod` finishes opening -- and again for the duration of a
rebootstrap. This was written down on 2026-09-29 after a daemon smoke test raced
it. So:

* The plugin container must treat a successful `connect` as meaning nothing, and
  `UNAVAILABLE` as "retry", never as fatal. A peer that exits on the first
  `UNAVAILABLE` will crash-loop through every startup.
* The plugin container's readiness probe must describe **the plugin's** own
  service, not `yesnod`'s. Wiring the plugin's readiness to the daemon's makes the
  whole Pod unready during a rebootstrap, which may be what you want for a
  search-serving plugin and is definitely not what you want for a scorer; the
  choice belongs to whoever writes the plugin, so the CRD takes a probe rather
  than synthesising one.
* `yesnod` already logs `yesnod is ready` when it enters its supervision loop,
  distinct from `yesnod is serving` when listeners bind. That is the line an
  operator-side wait should key on.

**Shutdown needs no ordering, and this is a property of the design worth
recording.** Teardown drains plugin leases and then stops the channel; closing a
peer's socket is what releases its snapshots. So if the plugin container dies
first the drain is trivially satisfied, and if `yesnod` goes first the peer sees
EOF. Either order is safe *because* liveness is socket closure. The Pod's
`terminationGracePeriodSeconds` should still be at least
`spec.shutdownGraceSecs`, which governs the daemon's drain and final checkpoint.

A native sidecar ( an `initContainer` with `restartPolicy: Always` ) is **not**
wanted here. Those exist to start a dependency *before* the main container; the
plugin depends on `yesnod`, not the reverse, and it must retry anyway. A plain
second container plus retry is simpler and correct.

**Promotion restarts the plugin, and that is acceptable.** A promotion changes
the role label and the generated config, the config-identity annotation changes
with it, and the Deployment strategy is `Recreate` -- so the Pod is replaced and
the plugin restarts against the new role. Simpler than asking every plugin to
handle a role transition live, and it should be documented rather than discovered.

## Phase 3: resource accounting, which needs a measurement not an assertion

Arena size is `maxHandles * maxBlocks * maxLanes * LANE_BYTES`. At the defaults
that is `4 * 16 * 1024 * 8192` = **512 MiB per connection** of address space. It
is a sparse `memfd`, so the reservation is address space and the cost is the
pages actually touched -- that much is in the channel's own documentation and is
why the fixed-slot layout is affordable.

Two things follow that the plan must not hand-wave:

* **Which cgroup is charged for a touched arena page is an open question.** The
  pages are shared `memfd` pages; the intuition is that first touch charges the
  faulting process, which is `yesnod` writing lanes, so the arena counts against
  the `yesnod` container's limit and not the plugin's. **That is an assumption,
  and it should be measured before any memory limit is recommended** -- a probe
  that reads `memory.current` for both containers' cgroups across a large scan
  settles it in an afternoon, and getting it backwards means an OOMKill in the
  container an operator did not size.
* **There is no cap on concurrent connections.** `Channel::start` accepts in a
  loop and spawns a thread per peer, and each `Session` builds **its own** arena,
  so N peers cost N arenas and N threads with nothing bounding N. For an
  operator-deployed sidecar the peer is trusted and this is tolerable, but it
  should be stated in the CRD documentation, and a `channel_max_peers` is the
  right follow-up -- it is a cap whose justification names a resource the channel
  owns, so it belongs to the channel.

## Phase 4: tests, in the two layers that already exist

* **`resources.rs` unit tests**, which is where the existing Pod assertions live
  ( they already assert mount counts and args ): the sidecar appears only when
  `spec.plugin` is set; it mounts the socket volume and **not** `data`; the
  socket path and environment reach both containers; the hardening fields are
  present; a plugin with a non-10001 UID is rejected.
* **An operator e2e scenario** in the kind arm, using the `op_*` verbs, which
  already build both images and own the cluster end to end. Deploy a cluster with
  the Phase 0 peer binary as the plugin image, wait for `yesnod is ready`, and
  assert the peer reports a correct cardinality for a key the scenario wrote
  through Flight. That is the assertion that proves the whole path: CRD to
  sidecar to socket to arena to a real snapshot.
* Add it to `scripts/gate-operator.sh`, not to `scripts/gate.sh`, which does not
  run Kubernetes.

## What this deliberately does not do

* **No cross-Pod plugins.** A Unix socket cannot leave the Pod. Supporting a
  remote peer means a TCP transport, which cannot pass a `memfd`, so it would be
  inline-only -- and the measured cost of inline against the arena is already
  recorded in `plugin-shape-performance.md`. Worth doing only against a consumer
  who wants it, and worth quoting that cost when they ask.
* **No sandbox.** The peer cannot corrupt `yesnod`'s address space or abort it,
  but it can still hold handles and open connections. `maxHandles` bounds one
  connection; nothing bounds connections, as above. "Cannot take the daemon down"
  is not "cannot waste its resources", and only the first is true here.
* **No `library` field, and no in-process plugin**, per the second section.
