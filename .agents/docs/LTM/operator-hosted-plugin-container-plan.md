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

**DONE ( 2026-09-29 ), both halves.** The flags and environment landed first, then
`yesno-plugin/src/bin/yesno-channel-peer.rs` with `tests/peer_process.rs`.

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

What was genuinely missing is narrower, and is now closed:
`yesno-plugin/src/bin/yesno-channel-peer.rs` is a peer in its own process, and
`tests/peer_process.rs` runs it against a socket this crate serves itself. It
covers an arena **mapped by another process** and a peer that is **killed** --
`SIGKILL` runs no destructor, so only the kernel closing the descriptor can
release the snapshot, which is the whole liveness argument and had never been made
against a process that could not cooperate.

One property is still unproven and is recorded rather than claimed: that a peer
needs nothing but the protocol. This one is a Rust binary in the workspace and
does link `yesno-core`. `ipc.rs` has **no imports at all**, so the dependency is
incidental, and showing it properly means splitting that module into its own
crate -- the file move `ARCHITECTURE.md` already anticipates. The deleted C
fixture proved the equivalent for the in-process ABI and nothing replaced it.

The binary is not yet in the unified image; `scripts/check-image-binaries.py`
covers `yesno-server`, `yesno-server-utils` and `yesno-operator` only, so adding
it is Phase 3's job along with the e2e arm that needs it.

A small binary that greets, opens a snapshot, acquires lanes for a key, walks
blocks and reports the cardinality over HTTP is enough. It doubles as the worked
example a third party copies, which is the artefact a consumer actually asks for.
It must be added to the unified image's binary set, which
`scripts/check-image-binaries.py` verifies in both directions, so that file
changes in the same commit.

## Phase 1: `spec.plugin` and the sidecar

**DONE 2026-09-30.** `PluginSpec` in `api.rs`, the sidecar and shared `emptyDir`
in `resources.rs`, the generated `[plugin]` section, `plugin_spec_error` wired
into the reconciler's `validate`, and seven tests. The checked-in CRD was
regenerated ( 373 -> 533 lines ); a test asserts it matches the Rust API, which
is what caught it.

Two things the implementation settled that the sketch left open. The Service is
**not** built -- `ports` are declared so a Service or probe can name them, and
creating one is separable. And the limits that have `YESNOD_*` flags are written
**only** to the environment, not also to the generated `[plugin]` section:
duplicating them would give two sources for one value with the environment
winning, which reads as a bug the first time somebody edits the ConfigMap and
nothing changes. Only `socket_mode`, `allow_uids`, `max_peers` and
`max_snapshots` -- which have no flags -- appear in the file.


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

**It runs as UID 10001 by default, and this paragraph is corrected.**

The original said a different UID was impossible because the channel applied no
socket mode, and that the operator should therefore reject such a spec. **Both
halves are now out of date**: `channel_socket_mode` is applied after bind, and
`channel_allow_uids` gates connections on `SO_PEERCRED`, both added 2026-09-29
after a security review. So a plugin running as another user is expressible --
allow its uid, and set a mode that admits it.

The default stays same-UID, because it is the configuration that needs no
thought: the socket lands at the umask owned by `yesnod`, and a peer sharing its
uid can open it with nothing further set. A spec naming another `runAsUser` is
therefore **accepted** and must set `channel.allowUids`; the reconciler refuses
only the combination that cannot work -- a foreign uid with no allow list -- and
says which field to set.

Generated config gains a `[plugin]` section; the `secretName` path relies on
Phase 0's environment variables. The reconciler should set the environment in
**both** cases, so there is one code path and the generated `[plugin]` section is
belt-and-braces rather than the mechanism.

## Phase 2: readiness, ordering, and the thing most likely to be got wrong

**DONE 2026-09-30.** `readinessProbe`, `livenessProbe` and `startupProbe` on
`PluginSpec`, passed through verbatim; the readiness and promotion semantics
written into the CRD's own documentation, which is where a user reads them; two
tests. The grace period already covered this -- it was `shutdownGraceSecs + 10`
before Phase 1 and is Pod-wide, so the sidecar shares it.

The CRD grew 533 -> 856 lines, almost all of it three embedded `Probe` schemas.
That is the price of passing probes through instead of narrowing them to a
subset the operator would then have to map, and it is worth paying: a peer's
health check is the peer author's business.


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
recording.** Teardown closes the channel, and closing a peer's socket is what
releases its snapshots. ( This paragraph said "drains plugin leases and then
stops the channel" until 2026-09-30; the drain belonged to the in-process
facility and went with it on 2026-09-29. Nothing drains now -- the host takes the
snapshots back by closing, which is the point. ) So if the plugin container dies
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

**DONE 2026-09-30.** Both bullets are closed, one by measurement that reversed its
own premise and one because the cap it asked for had already landed.


Arena size is `maxHandles * maxBlocks * maxLanes * LANE_BYTES`. At the defaults
that is `4 * 16 * 1024 * 8192` = **512 MiB per connection** of address space. It
is a sparse `memfd`, so the reservation is address space and the cost is the
pages actually touched -- that much is in the channel's own documentation and is
why the fixed-slot layout is affordable.

Two things follow that the plan must not hand-wave:

* **Measured 2026-09-30, and the assumption was wrong.** This bullet used to say
  the intuition was first-touch charging, so the arena would count against
  `yesnod` and not the peer, and that it should be measured before any limit was
  recommended. It was measured: **the same pages are charged in full to both
  cgroups at once.** A peer that has only *read* a 128 MiB half reports 128 MiB of
  `shmem` while the host's charge does not fall. So the guidance inverts -- size
  the arena into **both** containers' limits -- and a deployment that gave the
  peer a small limit on the old theory would OOMKill it on its first large scan,
  which is the failure this bullet predicted arriving from the direction it did
  not. Full construction, the numbers, and one anomaly left deliberately
  unexplained are in [Who Pays for a Touched Arena Page](./arena-cgroup-charging.md).
* **The connection cap exists now**, so this bullet is closed. It asked for a
  `channel_max_peers` on the grounds that a cap justified by a resource belongs to
  whoever owns that resource; the security review of 2026-09-29 arrived at the
  same conclusion independently and it landed then, with a per-session snapshot
  quota beside it. Both are on the CRD as `channel.maxPeers` and
  `channel.maxSnapshots`. Note the sharper reason the review supplied: the
  snapshot quota bounds a **shared** resource, since every snapshot claims one of
  the database's 4096 reader slots and pins the reclamation floor, so an
  unbounded peer denies service to readers that have nothing to do with it.

## Phase 4: tests, in the two layers that already exist

**DONE 2026-09-30.** Both layers landed. Running the gate cost four attempts and
turned up three defects, none of which the code review that preceded it had found.

* **The gate was already red, for a week.** `yesno put` gained an ` at version V`
  suffix on 2026-09-23 and the harness asserted the whole line by equality with a
  matcher from 2026-08-27, so no operator-gate run could have passed in between.
  Whoever runs an expensive gate first pays for everyone who did not.
* **A selector on a label nothing sets.** `plugin_report` matched Pods on
  `yesnodb.io/cluster`, where the ten other selectors in the same file use
  `app.kubernetes.io/instance`. An invented label matches nothing, and nothing is
  indistinguishable from "no leader yet" -- which is why it read as correct. Its
  `items[0]` jsonpath also made kubectl fail before the empty-result check could
  report anything, so the diagnosis arrived as a template dump.
* **A snapshot is a point in time, and the sidecar starts with its cluster.** The
  peer opened one snapshot at startup and held it, so its only scan predated every
  write the scenario made and `holding 0` was the right answer to the wrong
  question -- indistinguishable from an empty database. Fixed with a `--watch` mode
  that re-opens on an interval, deliberately **separate** from `--hold`: the kill
  test reads the first announcement and kills the process expecting a snapshot
  still open, and a timer that closed and reopened would make that premise
  intermittently false.

**The carry-away is where the third defect was catchable.** It needed no cluster.
`a_watching_peer_sees_a_write_that_came_after_it_started` in
`yesno-plugin/tests/peer_process.rs` asserts the count before the write and after
it, runs in under a second, and fails in half a second when the peer stops
re-scanning. A property that holds between a peer and a database does not become a
Kubernetes property by being deployed on Kubernetes, and proving it at the layer
that owns it is three orders of magnitude cheaper. The e2e scenario is still worth
having -- it is the only thing that proves a CRD field becomes a sidecar that
reaches a socket -- but it should not be where a protocol property is discovered.

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
