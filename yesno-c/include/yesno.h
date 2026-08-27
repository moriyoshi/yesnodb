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
