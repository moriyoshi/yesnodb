/* SPDX-License-Identifier: MIT OR Apache-2.0 */
#ifndef YESNO_CHANNEL_C_YESNO_CHANNEL_H
#define YESNO_CHANNEL_C_YESNO_CHANNEL_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/*
 * A C client for the yesno plugin channel: a Unix socket to a running yesnod,
 * with block payloads delivered through a shared memory arena.
 *
 * This is NOT yesno.h. That header embeds a database in this process and takes
 * the directory's exclusive lock; this one connects to a server that already
 * holds it. A host that forks one process per connection -- PostgreSQL, say --
 * can use this and cannot use that.
 */

typedef struct yesno_channel yesno_channel;
typedef struct yesno_channel_snapshot yesno_channel_snapshot;
typedef struct yesno_channel_lanes yesno_channel_lanes;

/*
 * Status is classified, not merely success or failure, because the four
 * failure modes below need different handling and a caller cannot tell them
 * apart from a message. Treating them alike is the mistake this enum exists to
 * prevent:
 *
 *   RETRY       the server has no database open -- a follower is
 *               rebootstrapping. Back off and try again; the handle is fine.
 *   STALE       the database was replaced. Every handle is dead. Reconnect.
 *   EXPIRED     this snapshot's version is gone. Take a fresh snapshot. Do not
 *               retry the call.
 *   WRONG_ROLE  this connection is to a read-only replica and the request
 *               needed a leader.
 *   ERROR       everything else: I/O, a malformed frame, a protocol violation,
 *               or a server fault with no special handling.
 */
typedef enum yesno_channel_status {
  YESNO_CHANNEL_OK = 0,
  YESNO_CHANNEL_ERROR = 1,
  YESNO_CHANNEL_RETRY = 2,
  YESNO_CHANNEL_STALE = 3,
  YESNO_CHANNEL_EXPIRED = 4,
  YESNO_CHANNEL_WRONG_ROLE = 5,
} yesno_channel_status;

/* Lane payload encodings. The payload's length is implied by the kind and the
 * count, and `yesno_channel_lane` reports both so a caller can check rather
 * than assume. */
typedef enum yesno_channel_lane_kind {
  YESNO_CHANNEL_LANE_ARRAY = 0,  /* `count` little-endian uint16 lows */
  YESNO_CHANNEL_LANE_BITMAP = 1, /* `count` little-endian uint64 words, always 1024 */
  YESNO_CHANNEL_LANE_RUN = 2,    /* `count` [start, end] uint16 pairs, ascending */
  YESNO_CHANNEL_LANE_ABSENT = 3, /* no payload */
} yesno_channel_lane_kind;

/* What the server advertised when it greeted this connection. Read it rather
 * than assuming constants: `max_blocks` and `max_writes` are the server's
 * configured limits, and a client that guessed high on the second would have
 * its frame refused before anything reached the socket. */
typedef struct yesno_channel_limits {
  uint32_t protocol;
  uint64_t generation;
  uint8_t role; /* 0 leader, 1 follower */
  uint32_t shards;
  uint64_t arena_bytes; /* 0 when payloads travel inside frames instead */
  uint32_t max_lanes;
  uint32_t max_handles;
  uint32_t max_blocks;
  uint32_t max_writes;
} yesno_channel_limits;

typedef enum yesno_channel_write_op {
  YESNO_CHANNEL_INSERT = 0,
  YESNO_CHANNEL_REMOVE = 1,
  YESNO_CHANNEL_INSERT_RANGE = 2,
  YESNO_CHANNEL_REMOVE_RANGE = 3,
  YESNO_CHANNEL_DELETE_KEY = 4,
} yesno_channel_write_op;

/* `lo` and `hi` are inclusive for the range operations and `hi` is ignored for
 * the others. */
typedef struct yesno_channel_write {
  uint64_t key;
  uint64_t lo;
  uint64_t hi;
  uint8_t op;
} yesno_channel_write;

/*
 * Error buffers are optional, exactly as in yesno.h: when `error` is non-NULL
 * and `error_capacity` non-zero, every call writes a NUL-terminated message,
 * empty on success.
 *
 * Threading. A channel handle may be used from caller-managed threads; the
 * protocol is strict request/response over one socket, so calls serialize
 * internally rather than running concurrently. A snapshot and a lane cursor are
 * single-threaded -- do not call their functions concurrently. Nothing may be
 * closed while another call is using it.
 */

/* Connect, complete the handshake, and map the arena if the server sends one.
 * `name` is what the server logs this peer as. */
yesno_channel_status yesno_channel_open(const char *socket_path, const char *name,
                                        yesno_channel **out, char *error,
                                        size_t error_capacity);
void yesno_channel_close(yesno_channel *channel);

yesno_channel_status yesno_channel_get_limits(const yesno_channel *channel,
                                              yesno_channel_limits *out, char *error,
                                              size_t error_capacity);

/* 1 when block payloads arrive through the shared arena, 0 when they travel
 * inside frames. Both are correct; the arena avoids a copy. */
yesno_channel_status yesno_channel_is_arena(const yesno_channel *channel, uint8_t *out,
                                            char *error, size_t error_capacity);

/* Apply writes in one commit. Connection-scoped, NOT snapshot-scoped: a write
 * is not visible to an already-pinned snapshot. At most `limits.max_writes`. */
yesno_channel_status yesno_channel_apply(const yesno_channel *channel,
                                         const yesno_channel_write *writes, size_t count,
                                         uint64_t *version, uint64_t *changed, char *error,
                                         size_t error_capacity);

/* Pin a version for the life of the handle. */
yesno_channel_status yesno_channel_snapshot_open(const yesno_channel *channel,
                                                 yesno_channel_snapshot **out, char *error,
                                                 size_t error_capacity);
void yesno_channel_snapshot_close(yesno_channel_snapshot *snapshot);

yesno_channel_status yesno_channel_snapshot_version(const yesno_channel_snapshot *snapshot,
                                                    uint64_t *out, char *error,
                                                    size_t error_capacity);

yesno_channel_status yesno_channel_contains(const yesno_channel_snapshot *snapshot, uint64_t key,
                                            uint64_t ordinal, uint8_t *present, char *error,
                                            size_t error_capacity);

yesno_channel_status yesno_channel_cardinality(const yesno_channel_snapshot *snapshot,
                                               uint64_t key, uint64_t *out, char *error,
                                               size_t error_capacity);

/* `*present` is 0 for an empty key, in which case `*value` is untouched. */
yesno_channel_status yesno_channel_max(const yesno_channel_snapshot *snapshot, uint64_t key,
                                       uint8_t *present, uint64_t *value, char *error,
                                       size_t error_capacity);

/*
 * One page of a key's ordinals, ascending, into a caller-owned buffer.
 *
 * Continuation is by value and there is no cursor to close: pass the last
 * ordinal received back as `after` with `has_after` 1 to resume **strictly
 * above** it. `has_after` 0 starts from the beginning, which is not the same as
 * `after` 0. `*more` is 1 when another page follows.
 *
 * This is what an ordered SQL cursor is built from: first is `has_after` 0,
 * next resumes above the current ordinal, and a `>=` seek resumes above
 * `target - 1`. There is no backward continuation -- see the note on
 * `yesno_channel_max` for `last`, and expect to buffer if you need `prev`.
 */
yesno_channel_status yesno_channel_load(const yesno_channel_snapshot *snapshot, uint64_t key,
                                        uint8_t has_after, uint64_t after, uint32_t limit,
                                        uint64_t *out, size_t capacity, size_t *written,
                                        uint8_t *more, char *error, size_t error_capacity);

/* One page of the populated keys in [lo, hi], ascending, into a caller-owned
 * buffer. yesno has no key catalogue, so this enumerates what exists rather
 * than the span. */
yesno_channel_status yesno_channel_key_range(const yesno_channel_snapshot *snapshot, uint64_t lo,
                                             uint64_t hi, uint32_t limit, uint64_t *out,
                                             size_t capacity, size_t *written, uint8_t *more,
                                             char *error, size_t error_capacity);

/* Acquire a lane handle over `keys` and walk their blocks in ascending order.
 * At most `limits.max_handles` may be open at once. */
yesno_channel_status yesno_channel_lanes_open(const yesno_channel_snapshot *snapshot,
                                              const uint64_t *keys, size_t count,
                                              yesno_channel_lanes **out, char *error,
                                              size_t error_capacity);
void yesno_channel_lanes_close(yesno_channel_lanes *lanes);

/* Step to the next block, fetching a batch when the current one runs out.
 * `*have` is 1 while there is a current block and 0 when the walk is over. */
yesno_channel_status yesno_channel_lanes_advance(yesno_channel_lanes *lanes, uint8_t *have,
                                                 char *error, size_t error_capacity);

/* The current block's key prefix, and how many lanes it carries. Both fail if
 * there is no current block, which is the case before the first advance and
 * after the last. */
yesno_channel_status yesno_channel_lanes_prefix(const yesno_channel_lanes *lanes, uint64_t *out,
                                                char *error, size_t error_capacity);
yesno_channel_status yesno_channel_lanes_count(const yesno_channel_lanes *lanes, size_t *out,
                                               char *error, size_t error_capacity);

/*
 * One lane of the current block, with its payload.
 *
 * **`*payload` is borrowed, not owned.** In arena mode it points into memory
 * shared with the server; in frame mode into this library's receive buffer.
 * Either way it is valid only until the next `yesno_channel_lanes_advance` or
 * `yesno_channel_lanes_close` on this handle. Do not free it, and copy it if
 * you need it to outlive the current block.
 *
 * That borrow is the reason this header exists separately from yesno.h, whose
 * cursor deliberately materializes an owned snapshot to avoid handing a
 * borrowed lifetime to a C caller. Here the borrow is the point: it is what
 * makes a block read copy-free.
 *
 * `*payload_len` always equals the length implied by `*kind` and `*count`, and
 * is reported so a caller can assert rather than recompute.
 */
yesno_channel_status yesno_channel_lane(const yesno_channel_lanes *lanes, size_t index,
                                        uint8_t *kind, uint32_t *count, const uint8_t **payload,
                                        size_t *payload_len, char *error, size_t error_capacity);

#ifdef __cplusplus
}
#endif

#endif /* YESNO_CHANNEL_C_YESNO_CHANNEL_H */
