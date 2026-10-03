# Out-of-Process Plugin and Foreign-Reader Boundary

**Current state:** the shared-directory exploration below was superseded by a
`yesnod`-served Unix-socket plugin channel; the in-process `cdylib` ABI was
removed. The PID-namespace liveness defect on the separate foreign-reader path
was fixed with per-slot locks. Read the original exploration as the reasoning
that led to the served channel, not as a description of what remains to build.

Explored 2026-09-28, against the in-process design in
[`hosted-plugin-abi-design.md`](./hosted-plugin-abi-design.md). The question asked
was whether a shared-memory IPC plugin infrastructure could let a third-party
server run in a separate process, even a separate container.

## Original short answer ( historical )

**For a separate process on one host, almost nothing needs building, and shared
memory is the wrong tool for the data plane.** The data is already file-backed:
`Db::open_reader` opens a directory read-only *without* taking the exclusive
`flock`, registers the caller in a shared `READERS` file so the writer's
reclamation accounts for it, and hands back a `Db` whose `snapshot()` works
normally. A peer process maps the same shard files itself. There is no payload to
ship, so there is nothing for an SHM channel to carry.

**For a separate container there is one real defect**, and it is the kind that
loses data rather than performance: reader liveness is decided by `kill( pid, 0 )`
and `/proc/<pid>/stat`, both of which are relative to a PID namespace. See *The
container problem* below.

## What `Db::open_reader` already provides

- **No exclusive lock.** `Db::open` takes a non-blocking exclusive `flock`, which
  is right for writers and excludes every reader. `open_reader` does not take it.
- **Accounted for by reclamation.** `evict_floor`'s sibling consults
  `foreign_reader_floors()`, with a comment that says not to drop it because
  "readers are rare": the cost is one page walk and the failure is a read that
  should have succeeded returning a decode error.
- **A real identity, not a pid.** The registry records `( pid, process start time )`
  -- field 22 of `/proc/<pid>/stat`, fixed for a process's life -- so a recycled
  pid cannot impersonate a crashed reader. Staleness is resolved by liveness rather
  than a heartbeat, deliberately: a heartbeat would make correctness depend on a
  timer and would declare a merely *slow* reader dead.
- **Ambiguity resolves toward "still live"**, because the unsafe direction is
  declaring a live reader dead and reclaiming extents underneath it.
- **The full read surface.** A foreign reader's `Snapshot` answers `cardinality`,
  `load`, `key_stream` -- and therefore `KeyLanes`, so the block-scoped lane
  primitive works out of process with **no changes at all**.

What it costs: **checkpoint-visible state only**. No log replay and no memtable, so
a foreign reader lags the writer by up to one checkpoint. Its own doc calls that a
statable semantic rather than a bug. **This is the one place a data-plane IPC would
be justified** -- if a consumer cannot tolerate checkpoint lag, the fix is shipping
memtable state, which is a much larger design than a shared mapping and should not
be confused with one.

Worth stating plainly: `open_reader` is reached only from
`tests/zero_copy_mvcc.rs` and `tests/durability.rs`. It is built and tested and has
**no production consumer**, so it is unexercised rather than proven.

## Out of process fixes the three worst properties of the in-process design

Each of these is a problem the in-process ABI document had to argue around, and the
process boundary dissolves all three.

1. **The panic asymmetry disappears.** In-process, the host cannot contain a
   plugin's panic: a Rust `extern "C"` function that unwinds aborts, so a plugin
   bug takes the database down. A peer process that aborts takes only itself down.
2. **The linking prohibition disappears, and with it the consumer's blocker.**
   `OPENED_DIRS` is process-global state that decides whether a freed slab may be
   punched, which is why a second copy of `yesno-core` in one address space is
   forbidden. In a separate process that state is *correct*, so a peer may link
   `yesno-core` freely. The consumer reported that `haiiie-core` links it
   unconditionally and that they must split the scorer from the embedded adapter
   before a safe cdylib exists -- **an out-of-process peer removes that work
   entirely.**
3. **The drain gets a backstop it structurally lacks in-process.** In-process, only
   `ReaderSlot::drop` frees a lease and no host call can force one back:
   `evict_oldest_reader` does not free the slot, so the directory lock stays pinned.
   Out of process, a peer that dies or hangs is detected and its floor released,
   because the registry answers liveness rather than trusting the peer.

Against that, in-process keeps two things: reads as fresh as the memtable, and no
IPC on the control path.

## The container problem as first assessed ( historical )

Liveness is `kill( pid, 0 )` followed by a start-time comparison read from
`/proc/<pid>/stat`. **Both are PID-namespace-relative**, so across containers:

- The peer registers the pid it sees inside its own namespace.
- The writer probes that number in *its* namespace, reaching an unrelated process
  or none.
- Either way the recorded start time positively disagrees with what the writer
  finds, the identity check refutes, and the reader is **positively declared
  dead** -- which is exactly the direction the registry's own header names as the
  unsafe one. Its extents are then reclaimed underneath a live reader.

So the existing mechanism is sound across processes in one namespace and **unsound
across containers**. Two ways out:

**Share the PID namespace** ( `docker --pid=container:...`, Kubernetes
`shareProcessNamespace: true` ). Zero code, and a deployment that forgets it fails
silently in the data-losing direction, which is a poor property for an operational
requirement.

**Replace pid-liveness with a per-slot advisory lock.** Each reader holds an
`flock` or `fcntl` lock on its own slot region or a per-reader file; the writer
asks whether it can take that lock, and success means the holder is gone. The
kernel releases such a lock when the holding process dies **regardless of
namespace**, because the lock belongs to the open file description rather than to a
pid. This is the better fix even on a single host: it removes the `/proc`
dependency, and the entire pid-reuse argument along with it. The caveat is the
filesystem -- advisory locks are reliable on a local bind-mounted volume and are
not to be trusted over NFS, so the lock file's location becomes a documented
requirement.

## The control plane needs no shared memory either

A peer needs to learn about role changes, generations and checkpoints. `yesno-server`
already publishes exactly those over its control surface: `EventHub` with
`publish`, `subscription` and `StateSnapshot`, over a proto carrying a `Role` enum
and a role transition with `from` and `to`, on TCP or a Unix socket. A peer
subscribes; nothing needs inventing, and a shared-memory ring would be a second
transport for messages that already have one.

## What would actually have to be built

1. **Namespace-independent reader liveness.** The blocker for containers, and worth
   doing for its own sake.
2. **A peer-side handle.** Thin: open the directory with `open_reader`, take a
   snapshot, build `KeyLanes`. The lane primitive needs nothing.
3. **A documented freshness contract.** "Up to one checkpoint behind" has to be
   stated to the consumer and checked against its requirement, because it is the
   only semantic the in-process design does not have.
4. **Operational shape.** Both containers need the data directory on a shared
   mount, read-write for the peer, since the `READERS` file is written by readers.

## Redirected 2026-09-28: yesnod serves the channel, it does not share the database

Everything above explores a **shared database**, where the peer calls
`Db::open_reader` and maps the extents itself. That was not the intent, and the
correction is worth keeping rather than overwriting, because the rejected shape is
the one an exploration naturally falls into: it needs no protocol, so it looks
cheaper.

What it actually costs, and what an IPC-served channel keeps:

- **Filesystem access.** A shared-database peer needs the data directory on a
  shared mount, writable, because the `READERS` file is written by readers. A served
  channel needs a socket. The trust surfaces are not comparable.
- **Freshness.** A foreign reader replays no log and has no memtable, so it is
  limited to checkpoint-visible state. yesnod serving the channel can answer from
  the memtable, so the served shape keeps the in-process design's only remaining
  advantage.
- **Sole ownership.** One process opening the directory is the invariant the locking
  design is built on, and it is what keeps later questions about backup, restore
  and volume layout answerable.

**And the liveness argument comes out better, not worse.** The snapshot belongs to
yesnod, keyed by the connection that asked for it, so a closed socket lets yesnod
drop the lease *itself*. That is the forcible reclamation the in-process ABI
structurally cannot have -- where only `ReaderSlot::drop` frees a lease -- obtained
without a pid, a namespace or any cooperation from the peer. It also means a served
peer registers nothing in `READERS`, so the PID-namespace defect filed below is not
on this path at all; it remains a real defect for the shared-database shape and for
`yesno-pg`.

The protocol is `yesno_plugin::ipc`. Its shape and the reasoning behind each
choice are in that module's header; the two decisions worth repeating here are that
**a container payload never exceeds 8192 bytes** -- the exact maximum of the three
encodings, not a margin -- so a handle's arena is `lanes * 8192` with no allocator,
and that **lane `i` always lives at `arena_off + i * LANE_BYTES`**, so no offset
travels on the wire and the two sides cannot disagree about where a lane is.

## Original recommendation ( superseded )

**Prefer the out-of-process shape, and do not build an SHM data plane.** The data
plane already exists through the filesystem, the control plane already exists over
gRPC, and the process boundary removes the three sharpest problems of the
in-process design -- including the one blocking the consumer. What is missing is
one liveness fix and a stated freshness contract, which is a much smaller and much
more honest surface than a shared-memory protocol.

Keep the in-process ABI: it is built, gated and the right answer where
checkpoint lag is unacceptable. But it should be the exception rather than the
default, and the in-process design document should say so.

The served channel replaced this recommendation: the peer does not open or map
the database directory, payloads travel through a sealed `memfd` arena or inline
frames, and the in-process ABI no longer exists. See
[Plugin Channel Protocol and Security](plugin-channel-protocol-and-security.md)
and [Removed cdylib Plugin ABI](removed-cdylib-plugin-abi.md).

## Foreign-reader liveness after the PID-namespace defect

The served plugin channel does not register a foreign reader: `yesnod` owns each snapshot, and closing the peer socket drops the session. The separate shared-directory reader path still needs a liveness test that works across PID namespaces. `kill( pid, 0 )` and `/proc/<pid>/stat` use namespace-relative pids and could falsely declare a live container reader dead, releasing its reclamation floor.

`db::readers` now holds a `flock` on `READERS.locks/<slot>` for each registration. A slot is live if the old pid test **or** the lock says it is live, preserving compatibility with readers from older builds. A failed open or lock probe answers "held"; only an absent lock file or a successfully acquired probe lock proves no holder. Lock files are never unlinked, because unlinking creates a new inode that a probe could lock while the original holder still owns the old one. Separate opens in one process conflict under `flock`, unlike classic per-process `fcntl` record locks. Advisory locking requires a reliable local filesystem; the code keeps pid-only protection when locking is unavailable rather than refusing an otherwise working reader.

Tests forge a pid identity that positively refutes while a lock remains held, then show that the slot stays live. Companion cases cover registration drop, a leftover unlocked file, and an older registration with no lock file. The refuted-pid test fails when the lock half is removed. This closes the recorded PID-namespace defect for the foreign-reader path; the plugin channel's own socket-close reclamation remains the simpler and stronger mechanism for served peers.
