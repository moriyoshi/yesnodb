/* SPDX-License-Identifier: MIT OR Apache-2.0 */

#include "yesno.h"

#include <inttypes.h>
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

  yesno_db_close(first);
  yesno_db_close(second);
  yesno_db_close(NULL);
  yesno_cursor_close(NULL);
  return 0;
}
