/* SPDX-License-Identifier: MIT OR Apache-2.0 */

#include "yesno.h"

#include <inttypes.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define CHECK_OK(call)                                                        \
  do {                                                                        \
    if ((call) != YESNO_OK) {                                                 \
      fprintf(stderr, "%s failed: %s\n", #call, error);                     \
      return 1;                                                               \
    }                                                                         \
  } while (0)

#define CHECK(condition)                                                      \
  do {                                                                        \
    if (!(condition)) {                                                       \
      fprintf(stderr, "check failed at line %d: %s\n", __LINE__, #condition); \
      return 1;                                                               \
    }                                                                         \
  } while (0)

/* A host-supplied dispatcher, as a storage engine would provide one.
 *
 * Deliberately spawns a thread per task rather than using a pool: the point
 * here is to prove the callback is invoked and that a *genuinely concurrent*
 * executor produces the same database, not to be fast. A real host lends its
 * existing pool, and yesno creates no threads either way.
 */
struct dispatch_arg {
  void (*task)(void *, size_t);
  void *ctx;
  size_t i;
};

static void *dispatch_thread(void *p) {
  struct dispatch_arg *a = (struct dispatch_arg *)p;
  a->task(a->ctx, a->i);
  return NULL;
}

#define MAX_TASKS 64

static void threaded_dispatch(void *user_data, size_t n,
                              void (*task)(void *, size_t), void *task_ctx) {
  unsigned *calls = (unsigned *)user_data;
  ++*calls;
  if (n == 0) {
    return;
  }
  if (n > MAX_TASKS) {
    for (size_t i = 0; i < n; ++i) {
      task(task_ctx, i);
    }
    return;
  }
  pthread_t threads[MAX_TASKS];
  struct dispatch_arg args[MAX_TASKS];
  int started[MAX_TASKS];
  for (size_t i = 0; i < n; ++i) {
    args[i].task = task;
    args[i].ctx = task_ctx;
    args[i].i = i;
    started[i] = pthread_create(&threads[i], NULL, dispatch_thread, &args[i]) == 0;
    if (!started[i]) {
      /* Falling back inline keeps the contract: every task runs exactly once. */
      task(task_ctx, i);
    }
  }
  for (size_t i = 0; i < n; ++i) {
    if (started[i]) {
      pthread_join(threads[i], NULL);
    }
  }
}

int main(int argc, char **argv) {
  if (argc != 3) {
    fprintf(stderr, "usage: %s DB1 DB2\n", argv[0]);
    return 2;
  }

  char error[256] = {0};
  yesno_db *first = NULL;
  yesno_db *second = NULL;
  CHECK_OK(yesno_db_open(argv[1], &first, error, sizeof(error)));
  CHECK_OK(yesno_db_open(argv[2], &second, error, sizeof(error)));
  CHECK(first != NULL && second != NULL && first != second);

  const uint64_t values[] = {1, 7, 9, UINT64_MAX - 1};
  for (size_t i = 0; i < sizeof(values) / sizeof(values[0]); ++i) {
    uint8_t changed = 0;
    CHECK_OK(yesno_db_insert(first, 42, values[i], &changed, error,
                             sizeof(error)));
    CHECK(changed == 1);
  }

  uint8_t changed = 1;
  CHECK_OK(yesno_db_insert(first, 42, 7, &changed, error, sizeof(error)));
  CHECK(changed == 0);
  CHECK(yesno_db_insert(first, 42, UINT64_MAX, &changed, error,
                        sizeof(error)) == YESNO_ERROR);
  CHECK(error[0] != '\0');

  uint64_t cardinality = 0;
  CHECK_OK(yesno_db_cardinality(first, 42, &cardinality, error, sizeof(error)));
  CHECK(cardinality == 4);
  CHECK_OK(
      yesno_db_cardinality(second, 42, &cardinality, error, sizeof(error)));
  CHECK(cardinality == 0);

  yesno_cursor *cursor = NULL;
  CHECK_OK(yesno_cursor_open(first, 42, &cursor, error, sizeof(error)));
  uint64_t ordinal = 0;
  uint8_t found = 0;
  CHECK_OK(yesno_cursor_first(cursor, &ordinal, &found, error, sizeof(error)));
  CHECK(found == 1 && ordinal == 1);
  CHECK_OK(yesno_cursor_seek(cursor, 8, YESNO_SEEK_OR_NEXT, &ordinal, &found,
                             error, sizeof(error)));
  CHECK(found == 1 && ordinal == 9);
  CHECK_OK(yesno_cursor_seek(cursor, 8, YESNO_SEEK_BEFORE, &ordinal, &found,
                             error, sizeof(error)));
  CHECK(found == 1 && ordinal == 7);

  CHECK_OK(yesno_cursor_seek(cursor, 8, YESNO_SEEK_EXACT, &ordinal, &found,
                             error, sizeof(error)));
  CHECK(found == 0);
  CHECK_OK(yesno_cursor_next(cursor, &ordinal, &found, error, sizeof(error)));
  CHECK(found == 0);
  CHECK_OK(yesno_cursor_next(cursor, &ordinal, &found, error, sizeof(error)));
  CHECK(found == 0);

  CHECK_OK(yesno_db_insert(first, 42, 11, &changed, error, sizeof(error)));
  CHECK(changed == 1);
  CHECK_OK(yesno_cursor_last(cursor, &ordinal, &found, error, sizeof(error)));
  CHECK(found == 1 && ordinal == UINT64_MAX - 1);
  CHECK_OK(yesno_cursor_prev(cursor, &ordinal, &found, error, sizeof(error)));
  CHECK(found == 1 && ordinal == 9);
  yesno_cursor_close(cursor);

  /* --- batches -------------------------------------------------------- */

  /* One commit spanning many keys, which is what `yesno_db_insert` cannot do:
     it is one key and one ordinal per commit, so it touches one shard. */
  yesno_batch *batch = NULL;
  CHECK_OK(yesno_batch_begin(first, &batch, error, sizeof(error)));
  for (uint64_t k = 100; k < 140; ++k) {
    CHECK_OK(yesno_batch_insert(batch, k, k * 3, error, sizeof(error)));
    CHECK_OK(yesno_batch_insert(batch, k, k * 3 + 1, error, sizeof(error)));
  }
  uint64_t batch_changed = 0;
  CHECK_OK(yesno_batch_commit(batch, &batch_changed, error, sizeof(error)));
  CHECK(batch_changed == 80);
  batch = NULL;

  for (uint64_t k = 100; k < 140; ++k) {
    uint64_t n = 0;
    CHECK_OK(yesno_db_cardinality(first, k, &n, error, sizeof(error)));
    CHECK(n == 2);
  }

  /* `changed` counts what actually moved, not what was recorded: re-inserting
     a present ordinal changes nothing. */
  CHECK_OK(yesno_batch_begin(first, &batch, error, sizeof(error)));
  CHECK_OK(yesno_batch_insert(batch, 100, 300, error, sizeof(error)));
  CHECK_OK(yesno_batch_insert(batch, 101, 999, error, sizeof(error)));
  CHECK_OK(yesno_batch_commit(batch, &batch_changed, error, sizeof(error)));
  CHECK(batch_changed == 1);
  batch = NULL;

  /* Removal and whole-key delete travel in the same commit. */
  CHECK_OK(yesno_batch_begin(first, &batch, error, sizeof(error)));
  CHECK_OK(yesno_batch_remove(batch, 102, 306, error, sizeof(error)));
  CHECK_OK(yesno_batch_delete_key(batch, 103, error, sizeof(error)));
  CHECK_OK(yesno_batch_commit(batch, &batch_changed, error, sizeof(error)));
  batch = NULL;
  uint64_t n102 = 0, n103 = 0;
  CHECK_OK(yesno_db_cardinality(first, 102, &n102, error, sizeof(error)));
  CHECK_OK(yesno_db_cardinality(first, 103, &n103, error, sizeof(error)));
  CHECK(n102 == 1 && n103 == 0);

  /* An aborted batch applies nothing. */
  CHECK_OK(yesno_batch_begin(first, &batch, error, sizeof(error)));
  CHECK_OK(yesno_batch_insert(batch, 200, 1, error, sizeof(error)));
  yesno_batch_abort(batch);
  batch = NULL;
  uint64_t n200 = 0;
  CHECK_OK(yesno_db_cardinality(first, 200, &n200, error, sizeof(error)));
  CHECK(n200 == 0);

  /* Null handles are reported, not dereferenced. */
  CHECK(yesno_batch_insert(NULL, 1, 1, error, sizeof(error)) != YESNO_OK);
  CHECK(yesno_batch_commit(NULL, &batch_changed, error, sizeof(error)) !=
        YESNO_OK);
  yesno_batch_abort(NULL);

  CHECK_OK(yesno_db_checkpoint(first, error, sizeof(error)));
  yesno_db_close(first);
  first = NULL;
  CHECK_OK(yesno_db_open(argv[1], &first, error, sizeof(error)));
  uint8_t present = 0;
  CHECK_OK(yesno_db_contains(first, 42, 11, &present, error, sizeof(error)));
  CHECK(present == 1);

  char tiny[1] = {'x'};
  CHECK(yesno_db_cardinality(first, 42, NULL, tiny, sizeof(tiny)) ==
        YESNO_ERROR);
  CHECK(tiny[0] == '\0');

  CHECK_OK(yesno_db_clear(first, 42, error, sizeof(error)));
  CHECK_OK(yesno_db_cardinality(first, 42, &cardinality, error, sizeof(error)));
  CHECK(cardinality == 0);

  /* --- options and a host-supplied dispatcher ------------------------- */

  char third[4096];
  snprintf(third, sizeof(third), "%s-dispatch", argv[2]);

  yesno_options *options = NULL;
  CHECK_OK(yesno_options_new(&options, error, sizeof(error)));
  CHECK_OK(yesno_options_set_shards(options, 4, error, sizeof(error)));
  CHECK(yesno_options_set_shards(options, 0, error, sizeof(error)) != YESNO_OK);

  unsigned dispatch_calls = 0;
  CHECK_OK(yesno_options_set_dispatch(options, threaded_dispatch,
                                      &dispatch_calls, error, sizeof(error)));

  yesno_db *third_db = NULL;
  CHECK_OK(yesno_db_open_with(third, options, &third_db, error, sizeof(error)));
  /* Options are read at open and not retained. */
  yesno_options_free(options);
  options = NULL;

  /* A batch spanning many keys is what gives the dispatcher more than one
     task: a commit touching one shard fans out to one. */
  yesno_batch *wide = NULL;
  CHECK_OK(yesno_batch_begin(third_db, &wide, error, sizeof(error)));
  for (uint64_t k = 0; k < 64; ++k) {
    CHECK_OK(yesno_batch_insert(wide, k, k * 5, error, sizeof(error)));
    CHECK_OK(yesno_batch_insert(wide, k, k * 5 + 2, error, sizeof(error)));
  }
  uint64_t wide_changed = 0;
  CHECK_OK(yesno_batch_commit(wide, &wide_changed, error, sizeof(error)));
  CHECK(wide_changed == 128);
  wide = NULL;

  /* The host's executor was actually used. Without this the rest of the
     section passes identically against a dispatcher that is never called. */
  CHECK(dispatch_calls > 0);

  /* And it produced the right database. */
  for (uint64_t k = 0; k < 64; ++k) {
    uint64_t n = 0;
    uint8_t present = 0;
    CHECK_OK(yesno_db_cardinality(third_db, k, &n, error, sizeof(error)));
    CHECK(n == 2);
    CHECK_OK(yesno_db_contains(third_db, k, k * 5 + 2, &present, error,
                               sizeof(error)));
    CHECK(present == 1);
  }

  CHECK_OK(yesno_db_checkpoint(third_db, error, sizeof(error)));
  yesno_db_close(third_db);

  /* Null and default handling. */
  CHECK(yesno_options_set_shards(NULL, 4, error, sizeof(error)) != YESNO_OK);
  CHECK(yesno_db_open_with(third, NULL, &third_db, error, sizeof(error)) !=
        YESNO_OK);
  yesno_options_free(NULL);

  /* A NULL dispatch restores the default, which spawns nothing. */
  CHECK_OK(yesno_options_new(&options, error, sizeof(error)));
  CHECK_OK(yesno_options_set_dispatch(options, NULL, NULL, error,
                                      sizeof(error)));
  yesno_options_free(options);

  yesno_db_close(first);
  yesno_db_close(second);
  yesno_db_close(NULL);
  yesno_cursor_close(NULL);
  return 0;
}
