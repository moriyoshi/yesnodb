# Peer sockets for the MySQL and PostgreSQL plugins

Exploration, 2026-10-08. **No code written and nothing measured** -- a peer session held
cores 5-9 and 15-17 throughout, so every claim below is from reading the tree, and the one
thing that would decide the design is a measurement that has not been taken. Read the
recommendation as an ordering, not a verdict.

## The question has two answers, because "peer socket" names two different things here

This is the first thing to settle, because the two differ in operation set, in authorization,
and in how much work they are.

### A. The plugin channel -- `yesno-plugin`, a bespoke framed protocol

Configured by `plugin.channel_socket` with `plugin.channel_socket_mode`; empty disables it.
Its `Frame` enum is the whole operation set, and for a database plugin it is a surprisingly
good fit:

    SnapshotOpen / SnapshotClose      SnapshotContains      SnapshotCardinality
    SnapshotMax / SnapshotLoad        SnapshotKeyRange      Apply / ChunkPut
    LanesAcquire / LanesRelease       BlockAdvance / BlockAdvanceMany / BlockRelease

`ServerHello` advertises `max_blocks` and `max_writes` rather than making a peer guess, with
a comment recording that a consumer already hit `TooLarge` from its own encoder against a
published constant the frame could not honour.

**Authorization is the socket's filesystem permissions and nothing else.** There is no
per-connection principal. The config's own comment is the honest statement of the boundary:
*"Empty is not 'anyone': it is 'this uid, and root'."*

### B. The Flight data service -- which is **TCP only** today

`yesno-server/src/lifecycle.rs` binds it with `TcpListener::bind( cfg.flight_addr()? )` in
both of its two call sites. There is no Unix listener for the data path.

**But the machinery for one exists and is already proven**, in
`yesno-server/src/local_transport.rs`: a Unix listener carrying HTTP/2, where ordinary
clients are authenticated from `SO_PEERCRED` and a privileged snapshot agent additionally
sends explicit `SCM_CREDENTIALS` that must match. `yesno-server/src/auth.rs`'s
`local_interceptor` turns that into a `Principal { name: "uid:N" }` and an
`AuthzChannel::Local`. It is wired to the **control** service, not to Flight.

So: gRPC over a Unix socket with kernel-credential identity is not a new mechanism in this
tree. It is an existing, tested one pointed at a different service.

## What each plugin would need

### MySQL already has the seam, and for option B needs **no client change at all**

`yesno-mysql/backend.h` declares an abstract `Backend` with two implementations already
present -- `backend_embedded.cc` and `backend_flight.cc` -- so a third is the natural shape.
Its interface is `Apply`, `Insert`, `Remove`, `Contains`, `Cardinality`, `Clear`,
`Checkpoint`, `OpenCursor`, plus a `Cursor` of `First` / `Next` / `Last` / `Prev` / `Seek`.

The useful accident: `backend_flight.cc` passes its endpoint string straight into
`yesno::flight::Client::Connect`, which is `arrow::flight::Location::Parse`. Arrow Flight
parses `grpc+unix://` locations. **So the MySQL client can already reach a Unix socket the
moment the server listens on one** -- a configuration string, not a patch.

### PostgreSQL needs a small client change, and this is the answer to a recorded blocker

`yesno-pg/src/options.rs` has `Transport::Flight { endpoint }` and
`Transport::Local { data_dir }`, the second declared and unimplemented. The to-do
`multiprocess-read-only-reader` records why: `Db::open` takes a **non-blocking exclusive
`flock`** and yesno has no shared or read-only open, so PostgreSQL forking one backend per
connection means N-1 failures, and a running `yesnod` makes it N.

**A peer socket does not work around that blocker; it is the shape that makes it moot.** One
owner process, N client connections, which is what a socket is for. That reframes the
backlog: the peer-socket work and `Transport::Local` are **alternatives, not complements**,
and the peer socket is the one that needs no reader-sharing redesign in the storage engine.

On the client side, tonic needs `connect_with_connector` for a Unix target, so PostgreSQL
pays a small patch where MySQL pays none.

## The three gaps if the plugin channel ( option A ) is chosen

`Backend`'s interface maps onto the channel better than expected -- `Contains` to
`SnapshotContains`, `Cardinality` to `SnapshotCardinality`, and `Insert` / `Remove` /
`Clear` all to `Apply`, whose `WriteOp` already has `Insert`, `Remove`, `InsertRange`,
`RemoveRange` and `DeleteKey`. Three things do not map:

1. **No `Checkpoint` frame.** Grepping `Checkpoint` in `yesno-plugin/src/ipc.rs` returns
   nothing. `Backend::Checkpoint` has no counterpart, and MySQL calls it on plugin shutdown.
2. **The ordered cursor.** The channel offers forward block streaming and `SnapshotMax`.
   `First`, `Next` and `Last` follow; **`Prev` and `Seek` are the awkward ones**, because
   reverse iteration over a forward-only `BlockAdvance` is not a thin adapter. This is the
   single largest piece of work in option A and it is worth sizing before committing.
3. **No per-connection principal.** Socket permissions are the entire boundary. Both
   plugins run as their own OS user and multiplex many SQL users over one connection, so
   yesno-side authorization cannot distinguish them either way -- but Flight at least
   produces a `Principal` that a rule can narrow, and the channel produces nothing.

## Recommended ordering

1. **Bind the Flight data service on a Unix socket, reusing `local_transport`'s accept path
   and `auth::local_interceptor`.** Highest value per unit of work by a wide margin: MySQL
   needs a config string, PostgreSQL a small connector change, the entire Flight operation
   surface and authorization model are retained, loopback TCP and network exposure go away,
   and connections gain kernel-credential identity. It also unblocks the PostgreSQL
   fork-per-backend problem without touching the storage engine.
2. **Then measure, before going further.** The only reason to prefer the plugin channel is
   throughput on scan-heavy paths -- lanes, blocks, `BlockAdvanceMany`, inline blocks -- and
   **nothing has measured a DB plugin's scan cost against Flight on the same host.** The
   MySQL cursor path is the natural subject. Note that `records_in_range` currently answers
   every non-point range with the whole-key cardinality ( to-do
   `mysql-records-in-range-is-whole-key` ), so the optimizer drives full scans through that
   cursor more often than it should, which makes the measurement more relevant, not less.
3. **Adopt the plugin channel only if step 2 shows Flight's framing is the bottleneck**, and
   then treat the reverse cursor as the real cost and the missing `Checkpoint` frame as a
   protocol addition.

## What would change this

A measurement showing Flight-over-Unix is still far from the channel's block streaming on a
cursor scan would move option A up. The opposite result retires option A entirely. Until
that exists, option A's attraction is an inference from its frame set, and this document
should not be read as evidence for it.

---

# Addendum, same day: where the channel seam belongs, and why not `yesno-c`

Two facts found after the above, which together decide the shape.

## The channel is a socket **and** a shared-memory arena

`yesno-plugin/src/channel.rs` carries an `Arena` that is an anonymous `memfd`, mapped
`MmapMut`, whose descriptor is handed to the peer over the socket with `SCM_RIGHTS`
( `send_fd` / `recv_fd`, with a one-byte payload because `sendmsg` carrying only ancillary
data is permitted but not reliably *received*, and that byte doubles as the protocol
version ). The module records why `memfd` over POSIX or System V shared memory: it crosses a
container boundary as a descriptor, it cannot leak because the region dies with the last
descriptor and mapping, and it has no name to guess.

So a C++ client would have to reimplement the frame codec **and** `recvmsg` with
`SCM_RIGHTS`, **and** the mmap, **and** the lane and block lifetime rules. That is the
argument for a single Rust implementation behind an ABI rather than a hand-written C++
client, and the precedent is already in this repository: the Flight ticket header widened
from 40 to 48 bytes and three independently written clients were not widened with it, which
is why `check-gate-parity.py` now compares `*/gate.sh` too.

## There is no client in `yesno-plugin` at all

`channel.rs` is the **server**: `Session`, `serve_blocking`, `serve_locked`, `read_frame`.
`ipc.rs` is the frame codec. **Nothing in the tree implements the client side** -- the
existing consumer wrote its own. So the missing seam is not the C ABI; it is the Rust
client, and both prospective consumers need it.

## Why `yesno-c` is the wrong home

Not primarily dependency weight, though that is real: `yesno-c` depends on exactly **one**
crate, `yesno-core` by path, and the channel would add `yesno-plugin` and `memmap2` to it.
Two stronger reasons:

1. **It is an *embedding* ABI, and the channel exists because embedding does not work for
   these callers.** Its surface is `yesno_db_open`, `yesno_db_insert`, `yesno_db_contains`,
   `yesno_cursor_open` -- it opens a database, which takes the non-blocking exclusive
   `flock`. That is exactly what PostgreSQL's fork-per-backend cannot do, and the whole
   reason a socket is attractive. Putting both in one library means every embedder links a
   socket client it will not use and every client links a storage engine it must not open.
2. **Their defining design decisions are incompatible.** `yesno-c`'s cursor *deliberately*
   materializes an immutable snapshot and spends `O( cardinality )` memory **to avoid
   borrowed Rust lifetimes in foreign callers**. The channel's entire value is zero-copy
   blocks in a shared arena. A materializing C cursor over the channel throws away the
   reason to use the channel; a borrowing one contradicts the contract `yesno-c` is built
   on. One ABI cannot hold both promises, and the right move is two contracts rather than a
   weakened one.

## Proposed layering, in build order

    yesno-plugin::client          NEW, Rust -- the one implementation of the protocol:
                                  connect, handshake, recv the arena fd, map it, typed
                                  requests, lane and block lifetime
        |
        +-- yesno-pg  Transport::Channel { socket }    Rust, uses it directly, no C involved
        |
        +-- yesno-channel-c       NEW, a C ABI over the client, with an explicitly
                |                 BORROWING block contract: pointer and length valid until
                |                 the next advance or release, stated in the header
                +-- yesno-mysql/backend_channel.cc     a third Backend implementation

The order is load-bearing. The Rust client comes first because both consumers sit on it, and
because designing the C borrow contract before the client's shape is known would be guessing
at the thing hardest to change later. PostgreSQL needs no C at all, which is worth saying
plainly: **the C seam serves MySQL**, and `Transport::Channel` serves PostgreSQL.

## Before writing the client, read the existing one

A working client exists in the consumer's tree. It is the existence proof of what the client
side actually needs -- handshake ordering, where the fd arrives relative to `ServerHello`,
how lanes are released on error -- and reading it is cheaper and more reliable than deriving
all of that from the server plus the frame enum. Read only; it is not ours to edit.

## Still unmeasured, and still the thing that decides whether any of this is worth it

Everything above is shape, not justification. The case for the channel over Flight-on-a-Unix-
socket rests on scan throughput that **has not been measured on one host**, and the
Flight-over-Unix route costs MySQL a configuration string. If that measurement comes back
showing Flight's framing is not the bottleneck, this addendum describes work that should not
be done.

---

# Design: the `yesno-plugin` client, derived from the existing one

## Source and its standing

Read 2026-10-08 from `/home/moriyoshi/src/haiiie/haiiie-peer/src/lib.rs` ( 962 lines ),
with `write.rs` and `control.rs` alongside it and tests in `haiiie-peer/tests/channel.rs`.
**That is not a stable reference**: it is another project's code, authored by a Codex session,
living in an **uncommitted** haiiie working tree, and unreleased. The peer session confirmed
access and asked for exactly this citation, and stated it has **not** reviewed that code for
the ordering questions below -- so those are taken from the code itself, not from anyone's
summary of it. Nothing here was copied; what was taken is the list of things a from-scratch
client gets wrong.

Its shape is also not the shape we want. It implements haiiie's own `SetStore`,
`SetSnapshot` and `Lanes` traits directly, so the transport and the consumer's abstractions
are fused. Ours must be **transport-level and trait-free**, because `yesno-pg` and a C ABI
have nothing in common above the wire.

## Ten things a from-scratch client would get wrong

These are the deliverable of the read, and most are invisible from the server plus `ipc.rs`.

1. **In arena mode a one-byte version arrives with `SCM_RIGHTS` *before* `ServerHello`.**
   Inline mode starts directly with the frame. So the first read **must** be `recvmsg`; an
   ordinary `read` silently discards the descriptor and the client then fails much later
   with a confusing error.
2. **That same first `recvmsg` can also pull in part of `ServerHello`**, so the client must
   buffer the remainder after skipping the version byte. A client that reads one byte and
   then starts frame-decoding loses whatever arrived with it.
3. **The fd and `arena_bytes` must agree in both directions**, and each direction is a
   distinct fault: a descriptor with `arena_bytes == 0` is "sent an arena fd but advertised
   inline mode"; `arena_bytes > 0` with no descriptor is "advertised an arena without an fd".
4. **Length-check the descriptor before mapping it**, against the advertised `arena_bytes`,
   and map **read-only**. More than one `ScmRights` descriptor is a fault, not a surplus.
5. **Arena lane addressing is a fixed stride, not packed**:
   `arena_off + block * max_lanes * LANE_BYTES + lane * LANE_BYTES`. Inline mode *is* packed,
   by summing `kind.payload_bytes( count )`, and its total must equal the frame payload
   length exactly. Two different addressing schemes behind one `advance`.
6. **The response frame must match the mode.** `Blocks` only with an arena, `BlocksInline`
   only without. Either crossed is a protocol fault rather than something to adapt to.
7. **`BlockAdvanceMany` is lock-step, not pipelined**: one round trip per batch of at most
   `max_blocks`, and an empty batch is the terminator. `max_blocks` is advertised precisely
   because guessing low is slow for a reason the peer cannot see.
8. **Teardown is RAII and its errors are deliberately dropped.** `LanesRelease` on the lane
   handle's `Drop`, `SnapshotClose` on the connection's. There is no explicit error path to
   write, which is the only way a release cannot be skipped on an early return.
9. **Six `io::ErrorKind`s during a pinned snapshot are *expiry*, not failure** --
   `NotConnected`, `ConnectionAborted`, `ConnectionReset`, `BrokenPipe`, `UnexpectedEof`,
   `WriteZero` -- because yesnod disconnects every old session before a follower cutover and
   need not get a notification out first. Mapping these to a generic I/O error would turn a
   routine cutover into an incident; the right answer is a typed `SnapshotExpired` carrying
   the version, and a latched `stale` flag so the handle cannot be reused.
10. **The client validates the server's output.** Lanes per block within `max_lanes`, blocks
    **strictly ascending** by prefix, lane payload within `LANE_BYTES`, arena slices inside
    the mapping. A client that trusts its server cannot distinguish a yesnod bug from its own.

## Proposed surface

Synchronous, because every consumer is: `yesno-pg` runs inside a PostgreSQL backend and
MySQL's handler is blocking C++, and a `tokio` runtime dragged into a `cdylib` that
PostgreSQL `dlopen`s is a cost with no buyer. The server stays async; only the client is not.
It reuses `ipc.rs` for framing, which is the entire point -- one codec, one implementation.

    yesno-plugin/src/client.rs

    pub struct Client                       socket, optional arena, advertised limits
      connect( path, name ) -> Result<Client>
      limits() -> &Limits                   generation, role, shards, max_* , arena_bytes
      snapshot() -> Result<Snapshot>

    pub struct Snapshot                     version-pinned for the connection's life
      contains( key, ordinal ) -> Result<bool>
      cardinality( key ) -> Result<u64>
      max( key ) -> Result<Option<u64>>
      key_range( .. ) -> Result<Vec<u64>>
      apply( writes ) -> Result<Committed>
      lanes( spec ) -> Result<LaneCursor>

    pub struct LaneCursor
      advance() -> Result<Option<Block<'_>>>    borrows: arena mode must not copy

`Block<'_>` borrowing is the load-bearing choice and the reason this does not belong in
`yesno-c`: the lifetime is what makes arena mode zero-copy, and the C ABI's job is to
restate it as "pointer and length valid until the next advance or release" in a header,
which is a contract a C caller can keep and `yesno-c`'s materializing cursor deliberately
refuses to offer.

Requests serialize behind one lock, because the protocol is strict request/response on one
socket; that is a property to document rather than a limitation to engineer around, and
`max_blocks` batching is what makes it adequate.

## Status

**Not implemented.** The peer session holds cores 5-9 and 15-17 for a multi-hour measurement
and asked me to stay fully off -- including `cargo check`, which writes `target/` and could
trip the disk half of their gate, a consideration I had missed when I asked whether a pinned
build was acceptable. Their topology answer also retired my proposal outright: L3 #1 is cpus
0-9 at 8 MiB and L3 #2 is cpus 10-19 at 16 MiB, and their pinned set **spans both**, so no
core on this host is outside their cache footprint. Writing a new module without compiling it
once is not a draft, it is a guess, so the code waits for their signal.
