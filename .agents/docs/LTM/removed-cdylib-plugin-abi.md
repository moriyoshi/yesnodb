# Removed: the in-process `cdylib` Plugin ABI

**Removed from the tree on 2026-09-29**, by the maintainer's decision, after the
out-of-process channel became the plugin story. This document preserves the part
that was a **published contract** -- the C header -- because a consumer may hold a
copy of it and because a header is the one artefact that cannot be reconstructed
from behaviour.

This follows the `yesno-core/src/stats.rs` precedent recorded in `AGENTS.md`: the
finding outlives the instrument, and removal is recorded rather than merely done.

## What was removed

| path | lines | what it was |
|---|---|---|
| `yesno-plugin/include/yesno_plugin.h` | 287 | the C contract, reproduced below |
| `yesno-plugin/src/table.rs` | 586 | the `extern "C"` host table over a live `Db` |
| `yesno-plugin/src/loader.rs` | 222 | `dlopen`, `yesno_plugin_init`, version and table-size negotiation |
| `yesno-plugin/src/abi.rs` | partial | `ChunkKind`, `AbiHeader`, `Chunk`, `ABI_V1`; `Status` and `Role` stayed, the channel uses them |
| `yesno-plugin/build.rs` | 57 | compiled the C fixture so a test could `dlopen` it |
| `yesno-plugin/tests/plugin.c` | 266 | the fixture, in C on purpose so it could not link `yesno-core` |
| `yesno-plugin/tests/host_table.rs` | 615 | the table's tests |
| `yesno-plugin/tests/load_c_plugin.rs` | 313 | the real `dlopen` round trip |
| `yesno-server/src/plugin.rs` | partial | `Facility`, and the lifecycle callbacks that drove it |
| `PluginConfig::library`, `PluginConfig::listen` | | the configuration that named a library |

The Rust source is not reproduced here. It is recoverable in full with
`git show <the removal commit>^:<path>`, and unlike the header it describes a
mechanism this project has decided against rather than an interface anyone else
implements.

## Why it went

The out-of-process channel does the same job without the three properties that
made this one a permanent liability, and they are worth stating because they are
the general argument against in-process extension, not a complaint about this
implementation:

* **It shared the address space.** A plugin could corrupt the heap. The
  configuration field's own documentation said naming a library "is an operator
  decision of the same weight as naming the data directory", which is an
  admission that it was never a sandboxed extension point.
* **A panic escaping a callback aborted the daemon.** Every table entry had to
  catch its own unwinding, and the correctness of the whole surface rested on
  nobody ever adding an entry that forgot.
* **Its leases were invisible to the shutdown proof.** The drain contract existed
  because a plugin holding a lease across a close had no other backstop; the
  channel replaces that with socket closure, which needs no cooperation from the
  peer and survives its `SIGKILL`.

Against that, the channel costs a copy and some round trips -- measured, and
recorded in `plugin-shape-performance.md`: the copy is 1.5-2.1x and batching
blocks recovers most of the rest.

One thing was genuinely lost. **A run could be lent zero-copy through the C table
and cannot be over the wire** -- though as it turned out, the table was lending
the *stored* `( start, len_minus_1 )` pairs under a header promising
`[ start, end ]`, which was a silent wrong answer fixed hours before this removal.
The zero-copy path for runs was therefore never correct in the form anyone used.

## The header, as published

```c
/* SPDX-License-Identifier: MIT OR Apache-2.0
 *
 * The yesnod-hosted plugin ABI.
 *
 * yesnod owns the database directory, replication, backup, PITR and promotion. A
 * plugin is loaded into the same process and reads the live database through the
 * function table below. It never opens the directory: there is deliberately no
 * `open` in the host table.
 *
 * Design and rationale: .agents/docs/LTM/hosted-plugin-abi-design.md
 */
#ifndef YESNO_PLUGIN_H
#define YESNO_PLUGIN_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* PANIC AND EXCEPTION CONTRACT. Read this before writing either side.
 *
 * Unwinding across this boundary is undefined, and the two directions have
 * different failure modes and different owners.
 *
 * What the host guarantees. Every function in `yesno_host_api` catches a panic
 * originating inside it and answers YESNO_INTERNAL. None of them unwinds into
 * the plugin. Where a caught panic could have left host state half-modified,
 * that state is poisoned rather than reported-and-reused -- a lane handle whose
 * advance panicked refuses every later call instead of letting a retry produce a
 * block with duplicated or skipped chunks.
 *
 * What the plugin must guarantee, and why it is not symmetric. The host calls
 * `yesno_plugin_api`'s members directly. A Rust `extern "C"` function that
 * unwinds **aborts the process**, and a C++ exception escaping one is undefined
 * -- so a panic or a throw inside a callback does not fail that callback, it
 * takes the whole database down with it. The plugin must catch everything at
 * each callback's edge and return a status. Do not rely on the host to contain
 * it: there is no interposition, and there cannot be one, because by the time
 * the host could observe the unwind it has already crossed the boundary.
 *
 * Two consequences worth stating. A plugin built with `panic = "abort"` cannot
 * honour this at all, since it has no unwinding to catch. And a plugin that
 * links its own copy of a Rust runtime has its own panic hook and its own
 * abort behaviour, neither of which the host can configure or observe.
 *
 * A HARD REQUIREMENT ABOUT LINKING. A Rust plugin must NOT link yesno-core.
 * The engine keeps process-global state whose correctness argument is literally
 * "this process": the set of directories this process has opened decides whether
 * a freed slab may be hole-punched, because a Container aliasing an earlier
 * instance's mapping cannot cross a process boundary. Two copies of the engine
 * in one address space are two such sets, and dlopen's default RTLD_LOCAL makes
 * two copies the expected outcome rather than a deduplicated one -- so the
 * second copy would answer "first open in this process" for a directory the host
 * had already opened and consider the host's live extents punchable. Nothing
 * reports this; the bytes simply go to zero underneath a reader. The plugin needs
 * no engine types, because everything it reads comes through this header.
 */

/* Every table begins with this, and these two fields never move.
 *
 * `size` is how a reader knows which trailing members exist. A host built later
 * than the plugin passes a larger struct and the plugin reads only the prefix it
 * knows; a plugin built later than the host must check `size` before touching a
 * member it learned about afterwards. Appending a member is compatible. Changing
 * or reordering one is a version bump. */
typedef struct yesno_abi_header {
  uint32_t version;
  uint32_t size;
} yesno_abi_header;

#define YESNO_PLUGIN_ABI_V1 1u

/* Distinct codes, because the conditions this ABI turns on are not
 * distinguishable from a single ERROR.
 *
 * There is no error-message channel in v1, and that is a choice rather than an
 * omission: a message would need either an out-parameter on every call, which is
 * what makes yesno.h's signatures what they are, or thread-local state on the
 * host side. The codes below are what a plugin actually branches on. Use
 * `status_name` for logging. */
typedef enum yesno_status {
  YESNO_OK = 0,
  YESNO_INVALID_ARGUMENT = 1,
  YESNO_INTERNAL = 2,
  /* No database in the slot. A follower is rebootstrapping; retry later. */
  YESNO_UNAVAILABLE = 3,
  /* The pinned version was evicted. The lease is dead; acquire a new one. */
  YESNO_SNAPSHOT_TOO_OLD = 4,
  /* The database was replaced. Drop every handle and re-acquire. */
  YESNO_GENERATION_CHANGED = 5,
  /* A write was attempted on a follower. */
  YESNO_WRONG_ROLE = 6,
  YESNO_ABI_MISMATCH = 7,
  /* A block is open and must be released before advancing, or none is open and
   * a block accessor was called. Separate from INVALID_ARGUMENT because it is
   * the one protocol mistake that is easy to make and cheap to diagnose. */
  YESNO_BLOCK_STATE = 8,
} yesno_status;

typedef enum yesno_role {
  YESNO_ROLE_LEADER = 0,
  YESNO_ROLE_FOLLOWER = 1,
} yesno_role;

/* How a chunk's payload is stored. A scorer wants different code per
 * representation, so the kind is part of the contract rather than hidden behind
 * a uniform accessor that would have to expand. */
typedef enum yesno_chunk_kind {
  YESNO_CHUNK_ARRAY = 0,  /* `count` uint16_t values, ascending */
  YESNO_CHUNK_BITMAP = 1, /* `count` uint64_t words; count is always 1024 */
  /* `count` [start, end] uint16_t pairs, ascending, both ends INCLUSIVE. Note
   * that this is not how a run is stored -- in memory it is ( start,
   * len_minus_1 ), the Roaring spec's on-disk form -- so a run is never lent and
   * block_lane always answers it with a NULL payload. Read one through
   * block_lane_into, which converts into your scratch. */
  YESNO_CHUNK_RUN = 2,
  /* This lane holds nothing at this block. Reported rather than omitted, so the
   * caller's lane indices never shift under it. */
  YESNO_CHUNK_ABSENT = 3,
} yesno_chunk_kind;

typedef struct yesno_chunk {
  /* The block's prefix ( ordinal >> 16 ), echoed so a caller can assert. */
  uint64_t prefix;
  uint32_t kind; /* yesno_chunk_kind */
  /* Values, words, or intervals, per kind. Zero when ABSENT. */
  uint32_t count;
  /* Borrowed, valid until block_release. NULL when ABSENT, and NULL from
   * `block_lane` when the payload cannot be lent -- see `block_lane_into`. */
  const void *data;
} yesno_chunk;

/* Host-owned, opaque. Never a Rust Db, Snapshot or Container. */
typedef struct yesno_host_db yesno_host_db;
typedef struct yesno_snapshot yesno_snapshot;
typedef struct yesno_lanes yesno_lanes;

/* Services the host provides. Valid for the life of the process. */
typedef struct yesno_host_api {
  yesno_abi_header header;

  const char *(*status_name)(yesno_status status);

  /* --- database state ---------------------------------------------------- */

  /* Bumped every time the database is replaced. Read it before acting on a
   * handle and after any callback; a change means every handle is stale.
   * Answers YESNO_UNAVAILABLE while there is no database. */
  yesno_status (*db_generation)(const yesno_host_db *db, uint64_t *out);
  yesno_status (*db_role)(const yesno_host_db *db, yesno_role *out);
  yesno_status (*db_accepts_writes)(const yesno_host_db *db, uint8_t *out);

  /* --- snapshots --------------------------------------------------------- */

  /* A pinned read version.
   *
   * Holds a reader slot, so nothing it can reach is reclaimed while it lives --
   * AND it pins the database's directory lock. Closing it is therefore not
   * merely tidy: the host cannot reopen the database until every snapshot and
   * every lane handle derived from one is gone. See the drain rules below. */
  yesno_status (*snapshot_open)(yesno_host_db *db, yesno_snapshot **out);
  /* Releases this handle's claim. Lane handles derived from it stay valid and
   * keep the version pinned; closing the parent while they live is legal and
   * is the expected fan-out pattern. */
  void (*snapshot_close)(yesno_snapshot *snap);
  yesno_status (*snapshot_version)(const yesno_snapshot *snap, uint64_t *out);

  /* --- lanes ------------------------------------------------------------- */

  /* Every requested key as a lane, all at ONE version.
   *
   * This is a correctness primitive, not a convenience. Acquiring lanes one at
   * a time takes one snapshot each, so a checkpoint between two acquisitions
   * scores one block against two database states.
   *
   * `keys` is copied; the caller may free it on return. Lane i corresponds to
   * keys[i] for the life of the handle. n may be 0.
   *
   * Two handles from one snapshot may be used by two threads concurrently. ONE
   * handle must never be shared between threads: it is a cursor with interior
   * position. */
  yesno_status (*lanes_acquire)(yesno_snapshot *snap, const uint64_t *keys,
                               size_t n, yesno_lanes **out);
  void (*lanes_release)(yesno_lanes *lanes);
  size_t (*lanes_count)(const yesno_lanes *lanes);

  /* --- blocks ------------------------------------------------------------ */

  /* Resolve every lane at the next prefix. `*done` is 1 when the scan is over.
   *
   * Resolves all lanes or none. A failure must not leave a tiled accumulator
   * holding lanes 0..k of a block whose lane k+1 failed, which is why this is
   * one call and not a loop the caller writes. On failure the handle is
   * unusable and every later call on it fails; release it.
   *
   * YESNO_BLOCK_STATE if a block is still open: call block_release first. */
  yesno_status (*block_advance)(yesno_lanes *lanes, uint64_t *prefix,
                               uint8_t *done);

  /* Describe lane `lane` of the open block. Borrowed until block_release.
   *
   * A call on one lane never invalidates another lane's pointer, which is what
   * makes tiling every lane at once legal.
   *
   * `out->data` is NULL with a real `kind` when the payload cannot be lent
   * without a copy, and `out->count` is then 0. Two cases answer that way: a
   * bitmap imported from an unaligned `.roaring` mapping, and EVERY run, whose
   * stored ( start, len_minus_1 ) pairs are not the [ start, end ] above. Being
   * told is the point: the alternative is paying an 8 KiB copy per call without
   * knowing, or -- for a run -- reading a length as an end. Use block_lane_into
   * for those. */
  yesno_status (*block_lane)(const yesno_lanes *lanes, size_t lane,
                             yesno_chunk *out);

  /* As block_lane, but writes into caller-owned scratch when a borrow is
   * impossible, and points at it. `out->data` is then `scratch`. When a borrow
   * IS possible this behaves exactly like block_lane and does not touch
   * scratch, so a caller may use only this entry point if it prefers.
   *
   * YESNO_INVALID_ARGUMENT if cap is too small; the needed size is written to
   * out->count as a byte count in that case. */
  yesno_status (*block_lane_into)(const yesno_lanes *lanes, size_t lane,
                                  void *scratch, size_t cap, yesno_chunk *out);

  /* End the open block and invalidate every pointer taken from it.
   *
   * Required before the next block_advance. One call per block against scoring
   * 65 536 documents is nothing, and it buys a single place to poison freed
   * descriptors in a debug build -- neither shape stops a caller holding a
   * pointer too long, but only one can diagnose it. */
  void (*block_release)(yesno_lanes *lanes);
} yesno_host_api;

/* Callbacks the plugin provides. The host calls these on its own thread. */
typedef struct yesno_plugin_api {
  yesno_abi_header header;

  /* The database is about to be replaced or closed.
   *
   * MANDATORY DRAIN. The plugin must release every snapshot and every lane
   * handle before returning. This is a contract, not a slow path: a
   * snapshot-backed lease pins the directory lock, so a handle still held here
   * makes the host's reopen fail with AlreadyOpen for as long as the plugin
   * holds it. There is no host-side call that can take a lease back -- evicting
   * the reader does not release the lock -- so the host's only remedy is to
   * report the plugin and let an operator kill the process.
   *
   * Reads are unavailable from this point until on_available. */
  yesno_status (*on_unavailable)(void);

  /* A database is in the slot again, at generation `generation`. Any state the
   * plugin derived from the previous one is stale. */
  yesno_status (*on_available)(uint64_t generation);

  /* The database was replaced while remaining available. Same obligation as
   * on_unavailable: drop everything derived from `old_gen` before returning. */
  yesno_status (*on_generation_change)(uint64_t old_gen, uint64_t new_gen);

  yesno_status (*on_role_change)(yesno_role from, yesno_role to);

  /* Bind and serve on `addr`, and stop serving.
   *
   * The plugin owns its listener; the host owns when it runs and supplies the
   * address from its own configuration. Unlike the engine, which creates no
   * threads at all, a plugin may create its own here -- but serve_stop must not
   * return until they have been joined. */
  yesno_status (*serve_start)(const char *addr);
  yesno_status (*serve_stop)(void);
} yesno_plugin_api;

/* The one symbol a plugin must export, called once before anything is served.
 *
 * `host` and `db` are valid for the life of the process. The plugin fills `*out`
 * with a table it owns and keeps alive for the life of the process. There is no
 * hot unload in v1. */
#define YESNO_PLUGIN_INIT_SYMBOL "yesno_plugin_init"
typedef yesno_status (*yesno_plugin_init_fn)(const yesno_host_api *host,
                                            yesno_host_db *db,
                                            const yesno_plugin_api **out);

#ifdef __cplusplus
}
#endif

#endif /* YESNO_PLUGIN_H */
```
