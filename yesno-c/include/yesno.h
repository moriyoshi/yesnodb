/* SPDX-License-Identifier: MIT OR Apache-2.0 */
#ifndef YESNO_C_YESNO_H
#define YESNO_C_YESNO_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct yesno_db yesno_db;
typedef struct yesno_cursor yesno_cursor;
typedef struct yesno_batch yesno_batch;
typedef struct yesno_options yesno_options;

typedef enum yesno_status {
  YESNO_OK = 0,
  YESNO_ERROR = 1,
} yesno_status;

typedef enum yesno_seek_mode {
  YESNO_SEEK_EXACT = 0,
  YESNO_SEEK_OR_NEXT = 1,
  YESNO_SEEK_AFTER = 2,
  YESNO_SEEK_OR_PREV = 3,
  YESNO_SEEK_BEFORE = 4,
} yesno_seek_mode;

/*
 * Error buffers are optional. When `error` is non-NULL and `error_capacity`
 * is non-zero, every call writes a NUL-terminated message (empty on success).
 * A database handle may be shared by caller-managed threads. A cursor is
 * single-threaded; do not invoke cursor functions concurrently. Handles must
 * not be closed concurrently with operations using them.
 */
yesno_status yesno_db_open(const char *path, yesno_db **db, char *error,
                           size_t error_capacity);
void yesno_db_close(yesno_db *db);

/* Run `task(task_ctx, i)` for every i in [0, n), possibly concurrently, and
 * return only when every one of them has finished.
 *
 * The host supplies this so that yesno never creates threads of its own: a
 * storage engine is embedded in a server that has already sized its scheduler.
 *
 * Two obligations, both the caller's:
 *   - Call `task` once per index and do not return early. Returning while a
 *     task is still running is a data race. Skipping one IS caught -- the
 *     commit is refused -- but returning early is not.
 *   - `user_data` must outlive the database handle.
 * `task` must not be called after this function returns.
 */
typedef void (*yesno_dispatch_fn)(void *user_data, size_t n,
                                  void (*task)(void *task_ctx, size_t i),
                                  void *task_ctx);

/* Open-time options. `yesno_db_open` is `yesno_db_open_with` using the
 * defaults. Options are read at open and not retained, so they may be freed
 * immediately afterwards; `user_data` behind a dispatch callback may not.
 *
 * A dispatcher only has work to spread when a commit touches several shards,
 * which means a batch: `yesno_db_insert` is one key and one ordinal, so it is
 * one commit on one shard and would fan out to a single task.
 */
yesno_status yesno_options_new(yesno_options **options, char *error,
                               size_t error_capacity);
void yesno_options_free(yesno_options *options);
yesno_status yesno_options_set_shards(yesno_options *options, uint32_t shards,
                                      char *error, size_t error_capacity);
yesno_status yesno_options_set_dispatch(yesno_options *options,
                                        yesno_dispatch_fn dispatch,
                                        void *user_data, char *error,
                                        size_t error_capacity);
yesno_status yesno_db_open_with(const char *path, const yesno_options *options,
                                yesno_db **db, char *error,
                                size_t error_capacity);
yesno_status yesno_db_checkpoint(const yesno_db *db, char *error,
                                 size_t error_capacity);

yesno_status yesno_db_insert(const yesno_db *db, uint64_t key,
                             uint64_t ordinal, uint8_t *changed, char *error,
                             size_t error_capacity);
yesno_status yesno_db_remove(const yesno_db *db, uint64_t key,
                             uint64_t ordinal, uint8_t *changed, char *error,
                             size_t error_capacity);
yesno_status yesno_db_clear(const yesno_db *db, uint64_t key, char *error,
                            size_t error_capacity);
yesno_status yesno_db_contains(const yesno_db *db, uint64_t key,
                               uint64_t ordinal, uint8_t *present, char *error,
                               size_t error_capacity);
yesno_status yesno_db_cardinality(const yesno_db *db, uint64_t key,
                                  uint64_t *cardinality, char *error,
                                  size_t error_capacity);

/* A batch accumulates writes and applies them as one commit.
 *
 * `yesno_db_insert` is one key and one ordinal, so it is one commit touching
 * one shard: a multi-row statement applied that way pays a version, a WAL
 * append and a durability wait per row. A batch is also what lets one commit
 * span shards, which is what a host-supplied executor has to work with.
 *
 * `yesno_batch_commit` *consumes* the handle whether it succeeds or fails.
 * Abort is for the path that never commits. A batch owns its own reference to
 * the database, but the database handle must still outlive it.
 */
yesno_status yesno_batch_begin(const yesno_db *db, yesno_batch **batch,
                               char *error, size_t error_capacity);
yesno_status yesno_batch_insert(yesno_batch *batch, uint64_t key,
                                uint64_t ordinal, char *error,
                                size_t error_capacity);
yesno_status yesno_batch_remove(yesno_batch *batch, uint64_t key,
                                uint64_t ordinal, char *error,
                                size_t error_capacity);
yesno_status yesno_batch_delete_key(yesno_batch *batch, uint64_t key,
                                    char *error, size_t error_capacity);
yesno_status yesno_batch_commit(yesno_batch *batch, uint64_t *changed,
                                char *error, size_t error_capacity);
void yesno_batch_abort(yesno_batch *batch);

/* A cursor owns a materialized, immutable snapshot of one key's set. */
yesno_status yesno_cursor_open(const yesno_db *db, uint64_t key,
                               yesno_cursor **cursor, char *error,
                               size_t error_capacity);
void yesno_cursor_close(yesno_cursor *cursor);
yesno_status yesno_cursor_first(yesno_cursor *cursor, uint64_t *ordinal,
                                uint8_t *found, char *error,
                                size_t error_capacity);
yesno_status yesno_cursor_next(yesno_cursor *cursor, uint64_t *ordinal,
                               uint8_t *found, char *error,
                               size_t error_capacity);
yesno_status yesno_cursor_last(yesno_cursor *cursor, uint64_t *ordinal,
                               uint8_t *found, char *error,
                               size_t error_capacity);
yesno_status yesno_cursor_prev(yesno_cursor *cursor, uint64_t *ordinal,
                               uint8_t *found, char *error,
                               size_t error_capacity);
yesno_status yesno_cursor_seek(yesno_cursor *cursor, uint64_t target,
                               yesno_seek_mode mode, uint64_t *ordinal,
                               uint8_t *found, char *error,
                               size_t error_capacity);

#ifdef __cplusplus
}
#endif

#endif
