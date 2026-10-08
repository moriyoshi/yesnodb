/* SPDX-License-Identifier: MIT OR Apache-2.0 */
/*
 * Links the library from C and exercises the paths that do not need a server.
 *
 * Deliberately not a behavioural test: that is `tests/abi.rs`, which drives the
 * same exported functions against a running channel. What only this file can
 * establish is that `yesno_channel.h` parses as C11 under `-Wall -Wextra
 * -Werror`, that every symbol it declares is exported with a matching
 * signature, and that the failure paths report instead of crashing -- none of
 * which a Rust test can fail at.
 */
#include <assert.h>
#include <stdio.h>
#include <string.h>

#include "yesno_channel.h"

int main(int argc, char **argv) {
  assert(argc >= 2);
  const char *absent = argv[1];

  char error[256];
  yesno_channel *channel = NULL;

  /* No server is listening, so this must fail with a message rather than
   * crash, and must not write a handle. */
  yesno_channel_status status =
      yesno_channel_open(absent, "smoke", &channel, error, sizeof error);
  if (status == YESNO_CHANNEL_OK) {
    fprintf(stderr, "connecting to %s should have failed\n", absent);
    return 1;
  }
  if (error[0] == '\0') {
    fprintf(stderr, "a failure must leave a message in the error buffer\n");
    return 1;
  }
  printf("absent socket reported: %s\n", error);

  /* A null error buffer is allowed; the call must not dereference it. */
  channel = NULL;
  status = yesno_channel_open(absent, "smoke", &channel, NULL, 0);
  if (status == YESNO_CHANNEL_OK) {
    fprintf(stderr, "a null error buffer must not change the outcome\n");
    return 1;
  }

  /* Null handles are reported, not dereferenced. */
  uint8_t flag = 7;
  status = yesno_channel_is_arena(NULL, &flag, error, sizeof error);
  if (status != YESNO_CHANNEL_ERROR) {
    fprintf(stderr, "a null channel must be an error, got %d\n", (int)status);
    return 1;
  }

  /* A null output pointer is reported too, rather than written through. */
  status = yesno_channel_is_arena(NULL, NULL, error, sizeof error);
  if (status != YESNO_CHANNEL_ERROR) {
    fprintf(stderr, "a null output must be an error, got %d\n", (int)status);
    return 1;
  }

  /* Closing a null handle is a no-op, which lets a caller clean up on an
   * error path without tracking which handles it got. */
  yesno_channel_close(NULL);
  yesno_channel_snapshot_close(NULL);
  yesno_channel_lanes_close(NULL);

  /* A truncating error buffer must still be NUL-terminated. */
  char tiny[4];
  memset(tiny, 'x', sizeof tiny);
  status = yesno_channel_open(absent, "smoke", &channel, tiny, sizeof tiny);
  if (status == YESNO_CHANNEL_OK) {
    fprintf(stderr, "expected failure with a tiny buffer\n");
    return 1;
  }
  if (strlen(tiny) >= sizeof tiny) {
    fprintf(stderr, "a truncated message must still terminate\n");
    return 1;
  }

  /* The enum values the header promises, asserted so a renumbering is caught
   * here rather than by a consumer reading a stale header. */
  assert(YESNO_CHANNEL_OK == 0);
  assert(YESNO_CHANNEL_ERROR == 1);
  assert(YESNO_CHANNEL_RETRY == 2);
  assert(YESNO_CHANNEL_STALE == 3);
  assert(YESNO_CHANNEL_EXPIRED == 4);
  assert(YESNO_CHANNEL_WRONG_ROLE == 5);
  assert(YESNO_CHANNEL_LANE_ARRAY == 0);
  assert(YESNO_CHANNEL_LANE_BITMAP == 1);
  assert(YESNO_CHANNEL_LANE_RUN == 2);
  assert(YESNO_CHANNEL_LANE_ABSENT == 3);
  assert(YESNO_CHANNEL_INSERT == 0);
  assert(YESNO_CHANNEL_DELETE_KEY == 4);

  printf("yesno-plugin channel C ABI smoke passed\n");
  return 0;
}
