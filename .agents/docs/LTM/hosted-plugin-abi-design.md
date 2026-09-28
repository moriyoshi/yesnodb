# A yesnod-hosted Plugin Facility: Design

Companion to [the assessment](./hosted-plugin-abi-assessment.md), which established
what `yesno-c` and `yesnod` supply today and listed eleven missing primitives. This
document is the design, and it **changes one of that document's recommendations**.
Nothing here is built.

## The one decision that shapes everything: a lease owns a `Snapshot`

The assessment said the host needs new lease accounting because a `Container`
retained after its `KeyStream` drops holds no reader slot, so `Db::live_readers()`
cannot see it. That is true, and the conclusion drawn from it was wrong. **The fix is
not to build accounting for bare containers; it is to never hand one across the
boundary.** Every lease the ABI issues owns a `Snapshot` clone, and then the existing,
tested machinery covers it:

- `Snapshot` clone is a refcount on a registry slot, so the version stays pinned.
- Reclamation floors already consult reader slots, so the extents behind a leased
  container cannot be recycled.
- `Db::live_readers()` counts it, so the shutdown proof obligation in `lifecycle.rs`
  covers it with no new instrument.

**Why this is mandatory and not merely tidy.** A bare container is not safe to hold
across a database replacement, and the reason is narrow enough to be missed. At open,
`Allocator::adopt_live_at_open` clears slots the committed root does not reach and
marks emptied inherited slabs `Free`; `punch_floor` then excludes them from *punching*,
which is what preserves
`zero_copy_mvcc::reopening_does_not_disturb_a_container_from_the_previous_instance`.
But `Allocator::new_slab_for` selects **the first `Free` slab with no `punch_floor`
filter**, so an inherited slab is still *reused* -- re-initialized by `Slab::new` and
allocated into. The bytes a cross-reopen container aliases are then overwritten in
place, through the same inode, and nothing reports it. The slab comment says as much
( "Reuse would break that guarantee too, so it was already conditional -- punching
only makes it observable" ). So the guarantee a plugin needs does not exist for bare
containers, and buying it would mean extending `punch_floor`'s logic to allocation.
Owning a `Snapshot` obtains it for free from the live instance instead.

The cost is larger than "a lease pins a version and blocks reclamation", which is how
this paragraph first read. **A `Snapshot` also pins the directory flock**, so a lease
outstanding when the host wants to reopen makes `Db::open_replica` fail
`AlreadyOpen`. The assessment's "revocation with a deadline" was retracted here on the
grounds that it was a safety requirement and safety comes free from reader slots. That
retraction was half right and the half it got wrong matters more: **accounting and
drain are required, for reopen liveness rather than for memory safety.** Corrected
after a source review from the `haiiie` side; the drain requirement below is the
substance of the correction.

## The drain requirement

A `Snapshot`-backed lease pins the flock, so the host cannot reopen while one is
outstanding. The existing code tolerates exactly this, but only barely and only
because of an assumption a plugin breaks. `close_for_rebuild` says it plainly:

> Dropping the last `Arc<Db>` is what releases the directory lock, and an in-flight
> `do_get` holds a `Snapshot` -- which holds an `Arc<DbInner>` -- so the lock may
> outlive this by the length of one read.

**"The length of one read" is the assumption.** A `do_get` is short and self-limiting,
so the window closes on its own and `open_if_needed` succeeds on a later pass. A
scoring lease is not bounded that way: it lives as long as the plugin chooses, and a
plugin that caches leases between requests -- which is the obvious optimization -- can
hold the lock open indefinitely. The host would sit in a retry loop reporting
`AlreadyOpen` with no way to attribute it.

So three rules, and they are contract rather than advice:

1. **`on_unavailable` is a synchronous drain point.** The plugin must release every
   lease it holds before returning. Returning with one outstanding is a contract
   violation, not a slow path.
2. **A lease may not outlive the request that acquired it.** No caching leases across
   requests in v1. A plugin wanting a warm handle caches derived state, not a
   `Snapshot`.
3. **The host verifies against its own count, because `live_readers()` cannot answer
   this question in either direction.** It **overcounts** what is attributable to the
   plugin, being a bare `usize` over all reader slots with no identity while an
   in-flight `do_get` holds one too -- so reporting a non-zero count as the plugin's
   fault is wrong whenever any other reader is live. And it **undercounts leases**,
   which is the sharper half: `Snapshot` clone is "refcounting the registry slot, not
   taking a second one", so N `yesno_lanes` handles derived from one `yesno_snapshot`
   share **one** slot and read as **one**. A plugin holding ten leases and a plugin
   holding one are indistinguishable there, and a host that waited for
   `live_readers()` to fall would be waiting on the wrong number.

   So the facility mints every lease and counts every lease, and `on_unavailable` must
   wait for **all** of them rather than for the slot. `live_readers()` survives only as
   a cross-check to be read alongside the server's own readers.

### There is no forcible remedy, and the design says so rather than implying one

`Db::evict_oldest_reader()` looks like the escalation and **is the wrong tool**. Its
own doc says "Does not free the slot", and reading what it does:

- It sets `reader_evicted[slot]` and nothing else. The slot stays non-`FREE`, so
  `live_readers()` still counts it and the count does not fall.
- The `Snapshot` object is untouched, so it still holds `Arc<DbInner>` and **the flock
  stays pinned**. Reopen is no closer to succeeding.
- But `evict_floor` *skips* evicted slots -- "An evicted reader is entitled to nothing:
  that is what evicting it means, and skipping it here is what actually returns the
  space" -- so the reclamation floor **is** released, and the extents behind any
  container the plugin still holds become reclaimable and reusable.

So for this purpose eviction is strictly the worst of both: it destroys the plugin's
data safety without buying the host's liveness. It is correct for what it exists for,
which is `enforce_space_amp` reclaiming space from a reader expected to fail its query;
it must not be documented as a reopen remedy. Only `ReaderSlot::drop` frees a slot, so
**only the plugin releasing its lease restores reopen liveness.**

Which means the drain contract is load-bearing with no backstop. The design accepts
that and fails loudly instead of pretending otherwise: the host reports which plugin
holds how many leases and for how long, and an operator kills the process. A real
remedy -- an interposed lease handle the host can invalidate without touching the
reader slot, so the flock is released while the plugin's next call gets
`GENERATION_CHANGED` -- is the obvious v2 feature and is deliberately out of v1 scope.
It is named here so the absence is a decision rather than an oversight.

## What the engine already guarantees, verified

- **A bare container cannot pin the flock. A `Snapshot`-backed lease can, and does.**
  `MmapSegment` is `{ map: memmap2::Mmap, base: u64 }` -- a mapping and an offset, no
  descriptor -- so mapped bytes alone hold nothing. But the flock is a `std::fs::File`
  inside `DbInner` ( "Held for the lifetime of the `Db`; dropping it releases the file
  lock" ), and `Snapshot` holds `Arc<DbInner>`. `lifecycle.rs` states it outright: a
  live `Snapshot` is invisible to the `Arc<Db>` refcount "while still pinning the lock".
  **`CodecError::AlreadyOpen` is therefore a first-class failure mode of this design**,
  and the decision above is what introduces it. See *The drain requirement* below.
- **Leased bytes stay addressable.** `ExtentGuard` holds an `Arc<MmapSegment>` as the
  Arrow allocation owner, so the mapping outlives the `Db` "however far that `Buffer`
  travels". Dropping the database does not invalidate a lease's address space; only
  reuse changes its contents, which the `Snapshot` rule prevents.
- **Zero copy is real through the page store.** Every slot is 64-byte aligned, so
  `BitmapContainer::words()` borrows; the copying arm is reachable only through an
  imported `.roaring` mapping.

## The rebootstrap question, settled

The assessment flagged that `guard.rs` justifies `DbSlot` by saying "`bootstrap_shard`
truncates the shard image, and truncating under a live mapping raises `SIGBUS`", while
`bootstrap_shard` renames. Resolved by reading the whole path: it writes
`<shard>.yno.partial`, `set_len`s **that** file, `sync_all`s it, empties the WAL with
`File::create`, and only then `rename`s the partial onto the image, calling the rename
its single commit point and `fsync`ing the directory. The one in-place truncation is
the **WAL**, and nothing maps the WAL -- there is no `MmapOptions` anywhere under
`yesno-core/src/wal/`.

**So no `SIGBUS` is reachable from a held lease, and `guard.rs`'s stated reason is
wrong.** The slot is still needed, for a better reason: after the rename the live
inode is orphaned, so a mapping of it would serve stale data for ever and the database
must be reopened to see the new one. A lease held across rebootstrap is therefore
**stale, not fatal** -- and because rename orphans the inode rather than reusing it,
such a lease is not even at risk from the reuse hazard above. It reads correct bytes
from a superseded database, which is the failure a generation check catches.

This is what lets revocation be advisory. The mandatory half is a generation check the
plugin performs, and the design puts it where it cannot be skipped.

## Shape: two tables, both versioned

The host exports a table of services; the plugin exports a table of callbacks. Neither
side links the other's symbols beyond one entry point.

```c
/* Every table starts with these two fields, and they are never reordered.
 * `size` is how the reader knows which trailing members exist: a host built
 * later than the plugin passes a larger struct, and the plugin reads only the
 * prefix it knows. Adding a member at the end is compatible; changing one is a
 * version bump. */
typedef struct yesno_abi_header {
  uint32_t version;
  uint32_t size;
} yesno_abi_header;

#define YESNO_PLUGIN_ABI_V1 1u

/* The single symbol a plugin must export. Called once, on the host's thread,
 * before any request is served. The plugin fills `*out` with a pointer to a
 * table it owns and keeps alive for the process. */
typedef yesno_status (*yesno_plugin_init_fn)(
    const yesno_host_api *host,   /* host services; valid for the process */
    yesno_host_db *db,            /* opaque; never a Rust Db or Container */
    const yesno_plugin_api **out);
```

`yesno_host_db` is an opaque handle the host owns. There is no path from the plugin to
the directory: the assessment's first missing primitive is satisfied by there being no
`open` in the host table at all.

### Status codes

The current `OK`/`ERROR` pair cannot express the conditions this design turns on, so
the enum is the part to get right first:

```c
typedef enum yesno_status {
  YESNO_OK = 0,
  YESNO_INVALID_ARGUMENT = 1,
  YESNO_INTERNAL = 2,
  YESNO_UNAVAILABLE = 3,        /* no database in the slot: rebootstrapping */
  YESNO_SNAPSHOT_TOO_OLD = 4,   /* the pinned version was evicted */
  YESNO_GENERATION_CHANGED = 5, /* the database was replaced; re-acquire */
  YESNO_WRONG_ROLE = 6,         /* a write on a follower */
  YESNO_ABI_MISMATCH = 7,
} yesno_status;
```

`SNAPSHOT_TOO_OLD` already exists in the core as `CodecError::SnapshotTooOld` and is
currently flattened into a message string.

### Snapshots and multi-lane acquisition

```c
/* A version pinned for the life of the handle. Holds a Snapshot clone, so it
 * holds a reader slot: reclamation cannot recycle anything it can reach, and
 * Db::live_readers() counts it -- and it pins the directory flock, so closing it
 * is not optional and not merely tidy: the host cannot reopen until it is gone. */
yesno_status (*snapshot_open)(yesno_host_db *, yesno_snapshot **);
void         (*snapshot_close)(yesno_snapshot *);
uint64_t     (*snapshot_version)(const yesno_snapshot *);
uint64_t     (*db_generation)(const yesno_host_db *);

/* Every lane of a block under ONE version, in one call.
 *
 * This is the correctness primitive, not a convenience. Acquiring lanes one at
 * a time takes one snapshot per lane, so a checkpoint between two acquisitions
 * has a block scored against two database states -- which is what
 * yesno_cursor_open does today. */
yesno_status (*lanes_acquire)(yesno_snapshot *, const uint64_t *keys,
                              size_t n, yesno_lanes **out);
void         (*lanes_release)(yesno_lanes *);
```

### Iteration is block-scoped, not handle-scoped

The first draft of this document said a borrowed pointer stayed valid "while the owning
`yesno_lanes` lives". **That is wrong and expensively so**, and the consumer's own code
is what shows it: it keeps one `Container` per lane for a single block, tiles all lanes
together, and clears the block before advancing. Handle-scoped validity would oblige the
host to retain every `Container` it visited for the whole scan, because
`KeyStream::next_chunk` yields them one at a time and nothing else would keep them
alive. Memory would grow with the length of the scan to buy a guarantee no scorer asked
for.

So a block is the unit. A **block** is the next chunk prefix at which *any* requested
lane has data; lanes with no chunk at that prefix are reported present-but-absent rather
than omitted, so the caller's lane indices never shift.

```c
/* Resolve every requested lane at the next prefix. Resolves all lanes or none:
 * a failure on lane 7 must not leave a tiled accumulator holding lanes 0..6,
 * which is why this is one call and not a loop the caller writes. */
yesno_status (*block_advance)(yesno_lanes *, uint64_t *prefix, uint8_t *done);

/* Valid from block_advance until block_release. A call on one lane never
 * invalidates another lane's pointer -- that is what makes tiling all lanes
 * simultaneously legal. */
yesno_status (*block_lane)(const yesno_lanes *, size_t lane, yesno_chunk *out);

/* Ends the block and invalidates every pointer from it. Required before the
 * next block_advance, which answers YESNO_INVALID_ARGUMENT otherwise. */
void (*block_release)(yesno_lanes *);
```

Requiring `block_release` rather than letting `block_advance` imply it costs one call
per block -- nothing beside scoring 65 536 documents -- and buys a single place to
poison freed descriptors in a debug build. Neither shape prevents a caller from holding
a pointer too long; only one of them can diagnose it.

**The host retains at most one `Container` per lane**, which is the whole point: a
bounded working set regardless of scan length, and the same shape the consumer already
has.

### Borrowed payloads, with the kind exposed

A lane's chunks are described, not copied. The kind is part of the contract because a
scorer wants different code per representation, and because the bitmap arm is the only
one that can decline to borrow.

```c
typedef enum yesno_chunk_kind {
  YESNO_CHUNK_ARRAY = 0,  /* u16 values, ascending */
  YESNO_CHUNK_BITMAP = 1, /* u64 words, exactly 1024 of them */
  YESNO_CHUNK_RUN = 2,    /* u16 pairs, [start, end] ascending */
} yesno_chunk_kind;

typedef enum { YESNO_CHUNK_ABSENT = 3 } yesno_chunk_absent; /* lane has nothing here */

typedef struct yesno_chunk {
  uint64_t prefix;  /* the block's prefix, echoed for the caller's own checks */
  uint32_t kind;    /* ARRAY, BITMAP, RUN, or ABSENT */
  uint32_t count;   /* values, words, or intervals, per kind; 0 when ABSENT */
  const void *data; /* borrowed; valid until block_release. NULL when ABSENT */
} yesno_chunk;

/* Where the payload cannot be lent -- an unaligned imported `.roaring` mapping,
 * the one case where BitmapContainer::words() answers Owned -- block_lane sets
 * data to NULL and kind to the real kind, and the caller passes scratch it owns
 * and reuses. The host never allocates for this; being told is the point, since
 * the alternative is paying 8 KiB per call per lane without knowing. */
yesno_status (*block_lane_into)(const yesno_lanes *, size_t lane,
                                void *scratch, size_t cap, yesno_chunk *out);
```

Expanding an array or run lane into a uniform bitmap is the **caller's** business, in
the caller's scratch: a scorer that wants one representation knows its own tiling width
and can size a buffer once, whereas a host that expanded eagerly would allocate per
chunk for callers that did not want it. So all three kinds are borrowed as they are
stored, and `block_lane_into` exists only for the case that cannot be borrowed at all.

`ArrayContainer::as_slice` and `RunContainer::as_flat` are already public;
`BitmapContainer::words` is `pub(crate)` and returns a `Cow`, so publishing a
borrowing accessor that can answer "not borrowable" is the one core change this
section needs.

### Concurrency

Two threads may hold two separate `yesno_lanes` from one `yesno_snapshot`, and this is
the intended shape for a multi-worker scorer. The engine supports it directly:
`Snapshot` is documented `Clone + Send + Sync + 'static`, and `KeyStream::over` takes
`slot: snap._slot.clone()` -- the same `Arc<ReaderSlot>` -- while giving each stream its
own `idx` and `plan`. `next_chunk` mutates only its own stream and takes the shard store
lock for disk reads. So `lanes_acquire` must give each handle its own `KeyStream`s and
its own `Snapshot` clone, and then the handles are independent.

**One `yesno_lanes` is never shared between threads.** It is a cursor with interior
position; two threads advancing one handle is a data race, and the ABI states it rather
than leaving it to be inferred from "cursors are single-threaded" the way `yesno.h` does
today.

Three consequences of the shared slot, none of them obvious from the call signatures:

- The facility's lease counter must count **handles**, not slots -- see rule 3 above.
- `on_unavailable` must wait for every handle, not for the version to be unpinned.
- Evicting that slot invalidates **all** handles sharing it, simultaneously. There is no
  per-handle eviction, which is another reason eviction is not a remedy for one
  misbehaving lease.

A derived handle outliving its parent is therefore **legal and must stay legal**:
`snapshot_close` releases one `Snapshot` clone and must not assert that no handle
remains, because each handle holds its own clone and the slot lives until the last of
them goes. An implementation that made `snapshot_close` require zero outstanding handles
would be sound but would forbid the natural pattern of opening a snapshot, fanning out,
and letting the workers own their views.

The test this wants is the reviewer's, twice refined, and it is better than what I would
have written: two threads, two handles, one shared snapshot, concurrent `block_advance`es
with a writer and a checkpoint running, release one handle before the other, then assert
both saw the same version.

**And close the parent `yesno_snapshot` after creating both handles, before asserting
anything about the slot.** Otherwise the assertion is vacuous: the parent's own
`Snapshot` clone keeps the slot live by itself, so "the slot stayed pinned until the
final release" would hold even if the handles pinned nothing at all. With the parent
closed, the slot must still be live after the first handle releases and must become
`FREE` only after the second -- which is the property actually under test, that each
handle carries its own clone rather than borrowing the parent's.

That is the third time in this session a test has been caught passing for a reason other
than the one it claimed -- after the single-slab evacuation fixture that offered no
candidates, and the sabotage check whose fixtures never spanned two output chunks. The
common shape: the assertion is true, and something other than the mechanism under test
is what makes it true.

### Role, generation, and writes

```c
typedef enum yesno_role { YESNO_ROLE_LEADER = 0, YESNO_ROLE_FOLLOWER = 1 } yesno_role;

yesno_status (*db_role)(const yesno_host_db *, yesno_role *);
yesno_status (*db_accepts_writes)(const yesno_host_db *, uint8_t *);
```

And on the plugin's table, so the host can tell it rather than be polled:

```c
/* Called on the host's thread, with no lease outstanding from the host's point
 * of view. The plugin must drop every handle derived from the old generation
 * before returning; after it returns the host may replace the database. */
yesno_status (*on_generation_change)(uint64_t old_gen, uint64_t new_gen);
yesno_status (*on_role_change)(yesno_role from, yesno_role to);
yesno_status (*on_unavailable)(void); /* entering the rebootstrap interval */
yesno_status (*on_available)(uint64_t generation);
```

The generation is the mandatory check. It is a `u64` read from the slot wrapper, not
from `Db`, so it costs an atomic load per call and survives the database being absent.
`Arc::ptr_eq` -- what `guard.rs` uses for its service cache -- is the right identity
in-process but cannot cross the ABI.

## Lifecycle, and where it attaches

`yesnod` has no plugin loading at all, so this is new code, but the attachment points
exist and are unusually well shaped for it.

- **Load**: at startup, after config and before the listeners bind. `libloading`, one
  `dlopen`, `yesno_plugin_init` once. No hot unload in v1, per the handoff.
- **The slot**: `DbSlot = Arc<RwLock<Option<Arc<Db>>>>` already models "no database
  right now", introduced for exactly the rebootstrap interval. The plugin facility
  wraps the same slot and adds the generation counter beside it, incremented on every
  successful replacement.
- **Rebootstrap**: the follower calls `on_unavailable`, drops the database, checks
  its own outstanding-lease counter, runs `bootstrap_shard`, reopens, increments the
  generation, then
  calls `on_available`. **The two hazards here have opposite strengths and were
  previously conflated.** Truncation under a mapping is not reachable today, because
  `bootstrap_shard` renames rather than writing in place, so no barrier is needed to
  prevent `SIGBUS` -- but `bootstrap_shard` itself opens no `Db`, so it was never the
  step that contended for the lock. The step that does is the **reopen**, and there the
  drain is mandatory: `open_replica` acquires the flock with a non-blocking `try_lock`
  and answers `AlreadyOpen` otherwise. So the barrier is advisory against corruption
  and required for availability.
- **Shutdown**: unchanged in mechanism. `Arc::into_inner( db )` plus
  `Db::live_readers()` already cover every holder, and the `Snapshot` rule is what
  keeps that true with a plugin in the process.

## Service registration

Both listeners are `tonic::Server::builder().add_service( ... )`, a compile-time
generic tower composition that a `cdylib` cannot join. There is nothing to extend, so
the design does not try.

**The host drives a listener the plugin owns.** The plugin table carries
`serve_start( const char *addr )` and `serve_stop( void )`; the host supplies the
address from its own config, calls `serve_start` after `on_available`, and
`serve_stop` before teardown and on becoming unavailable. The plugin may create its
own threads for this -- unlike `yesno_dispatch_fn`, which exists so the *engine*
creates none -- but must have joined them before `serve_stop` returns.

The alternative, forwarding request bytes through the host, was considered and
rejected for v1: it puts the host in the business of framing a protocol it does not
own, and buys only a shared port.

## Dynamic loading: what exists, and the four things that constrain it

Explored 2026-09-28, before writing the loader.

**Nothing exists.** `libloading` is absent from `Cargo.lock`, and `dlopen`,
`libloading` and `plugin` appear nowhere in `yesno-server`. The `cdylib` build
side is known territory -- `yesno-pg` and `yesno-c` both produce one -- but those
are libraries *other* programs load, not loaders.

### A Rust plugin must not link `yesno-core`, and this is not hygiene

The engine keeps process-global state whose correctness argument is literally the
word "process". `OPENED_DIRS` records every directory this process has opened, and
it decides whether a freed slab may be hole-punched: its own comment says a
`Container` "aliases this process's mapping and cannot cross a process boundary,
so such a container exists only if *this process* opened this directory before".

Two copies of the engine in one address space are two such sets, and **dlopen's
default `RTLD_LOCAL` makes two copies the expected outcome** rather than a
deduplicated one. The second copy answers "first open in this process" for a
directory the host already opened, concludes every inherited slab is punchable,
and zeroes bytes a host-side reader is holding. Nothing reports it.

So the rule "the plugin never opens the directory" is load-bearing for an
invariant two modules away from where it is written, and the requirement is now in
the header. It cannot be enforced from the host: a Rust plugin linking `yesno-core`
statically exports no C symbols to probe for. What removes the risk structurally is
that the plugin needs no engine types at all, because everything it reads comes
through the C header.

### Panic and exception handling is asymmetric, and the host cannot contain the plugin

The host half is containable and now is: every `yesno_host_api` function catches a
panic originating inside it and answers `Internal`. Two defects were found while
checking this and are fixed:

- **A caught panic in `block_advance` did not poison the handle.** `guard` turned
  it into a status while the lane loop was partway through -- streams advanced,
  heads half refreshed -- and a retry would then produce a block with duplicated
  or skipped chunks. That is precisely what the poison exists for, and only the
  `Err` path set it. It is also what made the `AssertUnwindSafe` around `&mut`
  state a promise nobody kept.
- **Lease counting had a panic window.** Increment, build the handle, hand it out
  leaves the count permanently high if anything in between fails; moving the
  increment after construction underflows a `usize` when a partly built handle
  drops. Replaced with an RAII `LeaseGuard` created before the handle and moved
  into it, so the count tracks the guard's existence on every path. This matters
  more than it looks: the count feeds the drain, and the drain is the one contract
  with no backstop, so a stuck count is a server that can never reopen.

The plugin half **cannot** be contained, and the header says so. The host calls
the plugin's callbacks directly; a Rust `extern "C"` function that unwinds aborts
the process, and a C++ exception escaping one is undefined. So a panic in a
callback does not fail the callback, it takes the database down. There is no
interposition possible, because by the time the host could observe the unwind it
has already crossed. A plugin built with `panic = "abort"` cannot honour the
contract at all, and one carrying its own Rust runtime has its own panic hook that
the host can neither configure nor observe.

### Three attachment facts about `yesno-server`

- **The leader path discards its slot binding.** `lifecycle.rs` builds the flight
  service with `GuardedFlight::new( slot_of( db.clone() ) )` inline, so there is no
  named slot for a facility to share. A small refactor binds it first.
- **On a follower a plugin has a database only if `follower.serve_reads` is
  enabled.** The slot is `cfg.follower.serve_reads.then( ... )`, and the comment
  is explicit: "a cold one never opens a database". Otherwise the slot is `None`
  for the process's life and every plugin call answers `UNAVAILABLE` for ever, so
  the configuration must refuse that combination rather than appear to work.
- **The drain hooks attach where the hazard is already documented.**
  `close_for_rebuild` is the function whose comment observes that an in-flight
  `do_get`'s `Snapshot` means "the lock may outlive this by the length of one
  read"; `on_unavailable` goes immediately before its `drop( taken )`, and the
  lease check immediately after. `open_if_needed` is where the generation is
  bumped and `on_available` called.

### Where the loader should live, revising this document

Put `dlopen` and version negotiation in **`yesno-plugin`**, not `yesno-server`.
This document originally said loading belongs in the server "where the slot and
the role already are"; that conflates *loading* with *lifecycle*. Only the
lifecycle needs the server. Keeping the loader in `yesno-plugin` means the whole
ABI including negotiation is testable without starting a server, which is the
reason the crate exists, and leaves `yesno-server`'s dependency list untouched.
The server decides *when* to load and owns the callbacks.

### The test plugin should be written in C

Following `yesno-c/gate.sh`, which compiles `tests/smoke.c` with `cc -std=c11
-Wall -Wextra -Werror` against the built library and runs it. A C plugin built
`-shared` is better than a Rust one for three reasons: it **cannot** accidentally
link `yesno-core`, so the constraint above becomes structural rather than a
promise; it proves the header is usable from C, which is the actual contract; and
it avoids cargo's lack of ordering between a `cdylib` target and a test that wants
its path.

### Bazel, and why the deferred `gate-pg.sh` is owed

`yesno-server` is not in the Bazel build at all -- no `BUILD.bazel`, no
references. But `crate.from_cargo` reads the **root** `Cargo.toml` and
`Cargo.lock`, so any dependency change reaches the `crates` hub. Adding
`libloading` will require `CARGO_BAZEL_REPIN=1 bazel mod deps`.

Note that this is **already outstanding**: adding `yesno-plugin` to
`[workspace] members` changed what `crate_universe` resolves, and `gate-pg.sh`
has not run since. That is exactly the class of breakage the rule about running
both gates exists for -- a change that satisfies cargo failing Bazel through a
stale lockfile resolution.

## What `yesno-core` must add

Small, and none of it is new capability:

1. A borrowing `BitmapContainer` words accessor that can report "not borrowable"
   instead of silently copying.
2. A snapshot-scoped multi-key acquire that builds N `KeyStream` plans against one
   `Snapshot` -- the plans already hold their own `Arc<ReaderSlot>` -- **plus a lockstep
   advance** over those N streams that yields the next prefix any of them holds and each
   lane's chunk or absence at it. The plans are the easy half; nothing in the tree does
   the lockstep part. `stream/` composes n-ary operators that *combine* streams into one
   answer, and `view/fold.rs` walks sets aligned but folds them as it goes; neither
   exposes N aligned chunks to a caller, which is what a scorer needs. It is a small
   merge over `Prefix48`, but it is new code and should be written and tested in the
   core against the existing `ChunkStream` laws rather than improvised in the ABI layer.
3. A generation counter on whatever owns the slot. This belongs in `yesno-server`
   rather than `yesno-core`, since the core has no concept of being replaced.

Everything else is `yesno-c`-shaped work: a new crate, or a new module beside it, that
does not call `Db::open`.

## Deliberately not in v1

Hot unload. Cross-process plugins. Any write path beyond the existing batch, since the
handoff's plugin scores rather than ingests. And no attempt to let the plugin
participate in the host's tonic router.

## How it gets tested, and the one benchmark that decides it

The lifecycle cases are the ones that will actually break: leader start, follower
start, rebootstrap with a lease outstanding, promotion, and shutdown with a lease
outstanding. Each has an observable the host already exposes -- the generation, the
role, the facility's lease counter -- so they are assertions rather than inspections.
The rebootstrap-with-a-lease-outstanding case should assert the *specific* failure: a
plugin that does not drain makes `open_replica` answer `AlreadyOpen`, and no host-side
call fixes it.

The benchmark that decides whether any of this was worth building is whole-query
latency and allocation against the embedded `YesnoStore`, on a multi-lane workload,
**with a block's lanes acquired under one snapshot**. That is the only configuration
whose allocation profile differs from `yesno_cursor_open`, which collects every ordinal
into a `Vec<u64>`.

Report the lane count, the population per lane, and the container kinds that resulted.
A bitmap lane and an array lane have different zero-copy stories, and a corpus that
yields only one of them cannot speak about the other -- the same lesson as the
evacuation measurement, where the instrument's resolution, not the policy, produced the
verdict.
