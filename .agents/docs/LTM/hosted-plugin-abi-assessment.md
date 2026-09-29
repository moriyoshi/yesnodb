# A yesnod-hosted Plugin ABI: What Exists, and the Primitives That Do Not

> **SUPERSEDED 2026-09-29.** The in-process `cdylib` ABI this document designs was
> **removed from the tree**; the out-of-process channel is the plugin story. Read
> this as the record of a design that was built, measured against the alternative,
> and then retired -- not as a plan. What survives of it is in
> [the removal record](./removed-cdylib-plugin-abi.md), which preserves the
> published C header, and the channel's own design is in `yesno-plugin/src/ipc.rs`
> and [the operator plan](./operator-hosted-plugin-container-plan.md). The
> reasoning below about lease lifetimes, rebootstrap semantics and role changes is
> still the reasoning the **channel** implements, so it is worth reading for that;
> the function table and the drain contract are gone.

Assessment of the `haiiie` handoff of 2026-09-28 ( a versioned function-pointer
table over the live `Db`, snapshot-safe multi-lane reads, follower rebootstrap and
role lifecycle, service registration ) against the tree as of `cfd0e88`.

**Provenance, because it is easy to get wrong and this session did.** The handoff was
typed into this session's tmux pane by a `haiiie` session that is **not** `haiiie-a6`,
the peer this session had been corresponding with about shard-mutex measurements. Its
own text says so -- it refers to "the yesno session" in the third person and records
only the pane it was entered into. The assessment below was nonetheless sent to
`haiiie-a6`, who correctly declined ownership and relayed it onward, and who has
**not** reviewed the ABI questions. So nothing here has been agreed by the contract's
owner; treat the whole document as one side's reading until that owner answers. A
later session wanting to discuss it should find the authoring session rather than
assume `haiiie-a6`.

## The verdict in one paragraph

**The engine already has the hard part; the boundary has none of it.** `yesno-core`
supplies refcounted snapshot pinning, zero-copy containers that outlive the handle
that produced them, and a reader registry with forcible eviction -- which is most of
what the handoff asks for as a *lease* model. `yesno-c` exposes none of it and is
shaped against it: it opens the directory itself and materializes every posting list
into `Vec<u64>`. `yesno-server` has no plugin loading of any kind. So the missing
primitives are mostly **boundary surface over capability that exists**, which is the
cheap kind of missing. The two exceptions are real design problems, and the handoff
does not anticipate either.

## What `yesno-c` exposes today ( `yesno-c/include/yesno.h`, 147 lines )

27 functions: `db_open` / `db_open_with` / `db_close`; options for shard count and
dispatch; `db_insert` / `remove` / `clear` / `contains` / `cardinality` /
`checkpoint`; a batch ( `begin` / `insert` / `remove` / `delete_key` / `commit` /
`abort` ); and a cursor ( `open` / `close` / `first` / `next` / `last` / `prev` /
`seek` with five seek modes ).

Lifetime guarantees it does make, all in the header's prose: a `yesno_db` may be
shared across caller threads; a cursor is single-threaded; handles must not be closed
concurrently with operations using them; `batch_commit` consumes the handle on both
outcomes; options are read at open and not retained, but `user_data` behind a
dispatch callback must outlive the database.

Three properties matter for this design:

- **It owns the directory.** `yesno_db_open( path, ... )` is the only constructor.
  There is no way to receive a database the host already opened, which is the
  handoff's first requirement.
- **`yesno_status` is `OK = 0` and `ERROR = 1`**, plus an optional message buffer.
  There is no code a caller can branch on, so "the database is unavailable while this
  follower rebootstraps" is indistinguishable from a bad argument.
- **It is a flat set of exported symbols, not a table.** No version, no struct size,
  no function-pointer block -- so there is nothing to negotiate against.

One precedent worth keeping: `yesno_dispatch_fn` already establishes that **the host
owns the threads** and yesno creates none, with the obligations written down. A
host-API table is the same move in the other direction, and the header's tone for it
already exists.

### The cursor is the disqualifying part

`yesno_cursor_open` does this ( `yesno-c/src/lib.rs:698` ):

```rust
let snapshot = unsafe { db_ref( db )? }.inner.snapshot()?;
let set = snapshot.load( key )?;
let value = Box::new( yesno_cursor { ordinals: set.iter().collect(), ... } );
```

The header's own summary is accurate: "a cursor owns a materialized, immutable
snapshot of **one key's** set". Both halves are fatal here.

- **It materializes to `Vec<u64>`** -- eight bytes per document per lane, from a
  Roaring container that may be a few hundred KB for the same population. This is
  precisely the outcome the handoff names as insufficient, and it is worse than
  copying a posting list, because it also discards the representation.
- **Each cursor takes its own snapshot.** Opening the lanes of one block in a loop
  gives each lane a *different* version. A checkpoint between two opens means exact
  top-k scores a block against two database states. **That is a correctness gap
  against haiiie's exactness oracle, not a performance one**, and it is the finding
  to act on first.

## What the core already supplies, and would only need exposing

`Snapshot` ( `yesno-core/src/db/mod.rs:4425` ) is the object the ABI wants:

- `Clone` is a refcount on a registry slot, not a second snapshot, so "the version
  stays pinned until every clone is gone".
- Reads: `contains`, `cardinality`, `is_empty`, `len_in_range`, `range_summary`,
  `load`, `key_stream`, `key_stream_prefix_range`, `key_expr`, `min`, `max`, `keys`,
  `key_range`.
- `version()` and `is_evicted()`, and every read returns `Err( SnapshotTooOld )` once
  invalidated. **Eviction gates future reads only** -- anything already materialized
  stays sound.

`KeyStream` ( `db/keystream.rs:120` ) is the non-materializing lane read, yielding
`( Prefix48, Container )` per chunk, and it holds its own `Arc<ReaderSlot>` so the
extents behind its plan cannot be reclaimed mid-walk.

`Container` clone is "O(1) for shared payloads ( a refcount bump )", and the reason
is `ExtentGuard` ( `store/segment.rs:133` ), which holds an `Arc<MmapSegment>` as the
allocation owner handed to `Buffer::from_custom_allocation`. So **a leased container
keeps its own mapping alive however far it travels**, with no Rust lifetime involved
and no dependence on the `Db`. `Container: 'static + Send + Sync` follows. This is
the lease model the handoff asks for, already built.

Raw payload access for a zero-copy C lane is *nearly* there:
`ArrayContainer::as_slice() -> &[u16]` and `RunContainer::as_flat() -> &[u16]` are
public; the bitmap equivalent, `BitmapContainer::words()`, is `pub(crate)` and
returns `Cow<[u64]>` because an unaligned shared buffer must be copied. Through the
page store that never happens -- every slot is 64-byte aligned -- and the copying arm
is reachable only through an imported `.roaring` mapping. So a borrowed-words
accessor is publishable, but **it has to be able to say "not borrowable"** rather
than silently costing an 8 KiB copy per call per lane.

## What `yesnod` supplies

No plugin loading whatsoever: `libloading`, `dlopen` and `plugin` appear nowhere in
`yesno-server`. Question two is answered "nothing exists".

The invalidation machinery, however, is all present, and shaped almost exactly as the
handoff wants -- just not reachable from a plugin:

- **`DbSlot = Arc<RwLock<Option<Arc<Db>>>>`** ( `guard.rs:54` ). `None` *is* "no
  database right now", introduced for precisely the rebootstrap interval, so that the
  port need not close with the database and callers can be told instead.
- **A service cache keyed by `Arc::ptr_eq`** ( `guard.rs:60`, and the field comment ).
  The established pattern for "rebuild your handle when the database is replaced",
  including the reasoning for why identity and not path is the key. A plugin's index
  handle should be invalidated the same way.
- **Term**, carried per request as `yesno-expect-term` / `yesno-term`, read from
  whatever is in the slot *now* rather than captured once, because the database can
  be replaced under a running server.
- **`EventHub`** ( `control.rs:300` ) with `publish`, `subscription` and
  `StateSnapshot`, over a `control.proto` that already has a `Role` enum and a role
  transition carrying `from` and `to`. Role and generation notification exists; it is
  published over gRPC, not offered as an in-process callback.

Service registration does **not** fit. Both listeners are
`tonic::Server::builder().add_service( ... )`, a compile-time generic tower
composition. A `cdylib` cannot hand a Rust-generic tonic service across the boundary,
so there is no way to extend this from a plugin. The plugin needs either a
host-driven listener lifecycle or a byte-level forwarding hook; nothing existing can
be reused.

## Design problem one: the shutdown proof obligation cannot see a lease

`lifecycle.rs` is explicit that **there is no `Db::close()`**. Teardown is `Drop` on
the last `Arc<DbInner>`, the exclusive `flock` lives inside it, and "did we shut down
cleanly" is a proof obligation with two halves that do not imply each other:
`Arc::into_inner( db )` returning `Some` proves no `Arc<Db>` clone survives, and
proves **nothing** about `Snapshot`s, which hold `Arc<DbInner>` directly and are
invisible to that refcount while still pinning the lock. `Db::live_readers()` exists
for that gap.

A plugin makes this worse in a way neither instrument covers. A `Container` extracted
from a `KeyStream` and retained after the stream drops holds **no reader slot at
all** -- `live_readers()` cannot see it -- while its `ExtentGuard` keeps the mapping
alive. So the host can satisfy both halves of its proof, conclude it shut down
cleanly, and still have a plugin holding mapped bytes.

That is benign for reads, because the mapping is genuinely kept alive and the data is
immutable. It is not benign across a reopen: the mapping is of a *file*, shared, so a
reopened database allocating into those extents would have its writes appear under a
lease the old snapshot believes is frozen.

**The missing primitive is the inverse of the one the handoff names.** It asks that
the host keep borrowed bytes alive for the lease duration; the engine does that
already and for free. What is missing is a way for the host to know what is
outstanding.

**Superseded in part by [the design](./hosted-plugin-abi-design.md), 2026-09-28.**
This section concluded that the host needs new accounting for retained containers,
with revocation and a deadline. That was the wrong conclusion from a correct
observation. The fix is not to account for bare containers but to **never hand one
across the boundary**: every lease the ABI issues owns a `Snapshot` clone, and then
reader slots and reclamation floors cover it with no new machinery. ( `live_readers()`
counts such a lease but cannot attribute it: it is a bare `usize` over all slots, and
an in-flight `do_get` holds one too. )

The observation that forced it also turned out sharper than written here. A bare
container held across a database replacement is not merely unaccounted, it is
**unsafe**: `Allocator::new_slab_for` takes the first `Free` slab with no
`punch_floor` filter, so an inherited slab is reused and re-initialized even though
punching spares it, and the aliased bytes are overwritten in place.

**And then corrected again, 2026-09-28, after a source review from the `haiiie` side.**
Retracting the accounting requirement was half right. Memory safety does come free from
reader slots -- but a `Snapshot` holds `Arc<DbInner>`, and `DbInner` holds the flock
`File`, so a `Snapshot`-backed lease **pins the directory lock**. `lifecycle.rs` says so
in the passage this document already quotes: a live `Snapshot` is invisible to the
`Arc<Db>` refcount "while still pinning the lock". So accounting and a drain are
required after all, for **reopen liveness** rather than for safety --
`Db::open_replica` uses a non-blocking `try_lock` and answers `AlreadyOpen`. The design
now carries a mandatory drain contract; see *The drain requirement* there.

## Design problem two: two documents disagree about rebootstrap, and the ABI depends on which is right

`guard.rs:54` justifies the slot this way: "`bootstrap_shard` truncates the shard
image, and truncating under a live mapping raises `SIGBUS`, which I6 states is not
catchable as a `Result`".

**`bootstrap_shard` does not truncate the image.** It writes
`<shard>.yno.partial`, calls `set_len` on *that* file, `sync_all`s it, and renames it
onto the image -- with its own comment calling the rename "this operation's single
commit point" and stating the image "is absent or whole and never in between"
( `yesno-server/src/replication/follower.rs:478-560` ). A rename replaces a directory
entry; the old inode survives for anyone still holding it mapped.

The difference decides the ABI:

- **If rebootstrap renames** ( what the code does ), a lease held across it is
  **stale, not fatal**. The plugin keeps reading the old inode and silently scores
  against a superseded database -- a wrong answer, which for an exactness oracle is
  the worse failure but a recoverable one, and a generation check catches it.
- **If rebootstrap truncated** ( what `guard.rs` says ), the same lease is a
  **`SIGBUS` that takes the whole process down**, plugin and host together, and the
  barrier must be mandatory and enforced before a byte is written.

Both readings demand that the plugin drop and re-acquire on a generation bump. They
disagree on whether the barrier may be advisory.

**Settled 2026-09-28, in favour of the code.** The only in-place truncation in
`bootstrap_shard` is the **WAL**, and nothing maps the WAL -- there is no
`MmapOptions` anywhere under `yesno-core/src/wal/`. The image is written beside and
renamed, with the directory `fsync`ed after. So no `SIGBUS` is reachable from a held
lease, the barrier may be advisory, and `guard.rs`'s rationale is wrong. The slot is
still needed for a better reason: after the rename the live inode is orphaned, so a
mapping of it would serve stale data for ever. A lease held across rebootstrap reads
correct bytes from a superseded database, which a generation check catches -- and
because rename orphans the inode rather than recycling it, such a lease is not even
exposed to the slab-reuse hazard described above.

## The concrete missing primitives

Ordered by what blocks the design rather than by effort.

1. **A host-handoff constructor.** Some `yesno_host_*` entry that receives an opaque
   host-owned context instead of a path. Everything else is downstream of this, and
   `yesno_db_open` cannot be adapted -- it is the wrong ownership direction.
2. **A snapshot object at the boundary.** `Snapshot` is already refcounted and
   pinning; the ABI has no handle for it, so lanes cannot share a version. This is
   the correctness fix, and it is close to free.
3. **A multi-lane acquire under one snapshot.** One call taking a set of keys and
   returning per-lane handles pinned to a single version, so a block's lanes are
   acquired once rather than N times with N snapshots.
4. **Borrowed lane payloads with a kind tag.** `as_slice` and `as_flat` are public;
   bitmap words are not. Needs a published accessor that reports the container kind
   and can answer "not borrowable" instead of copying 8 KiB silently.
5. **Status codes.** At minimum `UNAVAILABLE`, `SNAPSHOT_TOO_OLD`, `WRONG_ROLE` and
   `GENERATION_CHANGED` as distinct values. `SnapshotTooOld` already exists in the
   core and is currently flattened to a string.
6. **A lease type that owns a `Snapshot`, plus a drain contract.** Rather than account
   for bare containers, do not lend one -- a constraint on the ABI that reuses the
   reader-slot machinery wholesale for safety. But a `Snapshot` pins the flock, so the
   host also needs a lease count it can read and a mandatory drain point before reopen,
   or `open_replica` fails `AlreadyOpen` for as long as a plugin holds one.
7. **A generation counter on the database handle.** The term is per-request metadata
   and `Arc::ptr_eq` is in-process only; a plugin needs a scalar it can compare
   cheaply on every call and after every reopen.
8. **An in-process role and generation callback.** `EventHub` has the events and
   publishes them over gRPC; a co-hosted plugin should not dial its own host to learn
   it became a follower.
9. **A write refusal predicate on followers.** The plugin is asked to reject writes
   on a follower, and there is no primitive to ask.
10. **A host-sanctioned listener lifecycle.** `add_service` is unreachable across a
    `cdylib`. Either the host drives the plugin's listener ( bind, serve, drain,
    stop, tied to role ) or it forwards requests as bytes.
11. **The table itself**: a version and a size, so that a plugin built against an
    older host can be refused or degraded rather than mismatched.

## What to measure before believing any of it

The handoff is right that a complete ABI is not enough. The benchmark that decides
this is whole-query latency and allocation against the current embedded
`YesnoStore`, on a multi-lane workload -- and the arm that matters is the one where
a block's lanes come from one snapshot, because that is the only configuration whose
allocation profile differs from what `yesno_cursor_open` already does.

One caution from this session's own record: a ratio between two arms of a fixture is
a claim about that fixture. Report the lane count, the population per lane and the
container kinds that resulted, because a bitmap lane and an array lane have
different zero-copy stories and a corpus that yields only one of them cannot speak
about the other.
