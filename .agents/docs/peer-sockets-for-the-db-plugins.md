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
