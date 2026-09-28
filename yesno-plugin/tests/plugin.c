/* SPDX-License-Identifier: MIT OR Apache-2.0
 *
 * A test plugin, in C on purpose.
 *
 * Written in C rather than Rust for three reasons, in order of how much they
 * matter. It **cannot** link yesno-core, so the header's hard requirement about
 * two copies of the engine in one address space is structural here rather than
 * promised. It proves the header is usable from C, which is the actual contract
 * rather than a Rust-to-Rust convenience. And it sidesteps cargo having no
 * ordering between a cdylib target and a test that wants its path.
 *
 * It exercises the read path far enough to prove the table is wired: acquire a
 * snapshot, acquire lanes for three keys, walk every block, and record what each
 * lane held. The host test reads those records back through the exported
 * accessors below.
 */
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "yesno_plugin.h"

static const yesno_host_api *g_host;
static yesno_host_db *g_db;

/* What the host test inspects afterwards. */
static uint64_t g_generation;
static int g_unavailable_calls;
static int g_available_calls;
static int g_role_changes;
static int g_serving;
static char g_addr[256];

/* The scan's results, flattened: one row per ( block, lane ). */
#define MAX_ROWS 64
static uint64_t g_prefixes[MAX_ROWS];
static uint32_t g_kinds[MAX_ROWS];
static uint32_t g_counts[MAX_ROWS];
static size_t g_rows;
/* First value of the first array lane seen, to prove the payload is readable. */
static int g_first_array_value = -1;
/* The first run interval seen, after the scratch retry, as [ start, end ]. A run
 * is never lent -- its stored form is ( start, len_minus_1 ) -- so reading one
 * costs a conversion into caller memory, and this is where a C consumer's copy of
 * that handshake lives. */
static int g_first_run_start = -1;
static int g_first_run_end = -1;
/* Room for RUN_MAX_INTERVALS pairs; the host refuses a smaller buffer rather
 * than truncating. */
static uint16_t g_run_scratch[2032 * 2];

/* A lease held across calls, so the host's drain check has something to catch.
 *
 * A correct plugin never does this in v1 -- leases are request-scoped -- but the
 * host's report for a plugin that does is the one path with no backstop, so it
 * has to be reachable from a test. */
static yesno_snapshot *g_held;
static int g_drain_honestly = 1;

void yesno_test_set_drain(int honest) { g_drain_honestly = honest; }

yesno_status yesno_test_hold_lease(void) {
  if (g_held) {
    return YESNO_OK;
  }
  return g_host->snapshot_open(g_db, &g_held);
}

void yesno_test_release_lease(void) {
  if (g_held) {
    g_host->snapshot_close(g_held);
    g_held = NULL;
  }
}

static yesno_status on_unavailable(void) {
  g_unavailable_calls++;
  g_serving = 0;
  /* The drain. A correct plugin releases every handle it holds before
   * returning; `g_drain_honestly` exists so a test can watch what happens when
   * one does not. */
  if (g_drain_honestly) {
    yesno_test_release_lease();
  }
  return YESNO_OK;
}

static yesno_status on_available(uint64_t generation) {
  g_available_calls++;
  g_generation = generation;
  return YESNO_OK;
}

static yesno_status on_generation_change(uint64_t old_gen, uint64_t new_gen) {
  (void)old_gen;
  g_generation = new_gen;
  return YESNO_OK;
}

static yesno_status on_role_change(yesno_role from, yesno_role to) {
  (void)from;
  (void)to;
  g_role_changes++;
  return YESNO_OK;
}

static yesno_status serve_start(const char *addr) {
  /* Copied, because the header says the host's string is not ours to keep. */
  snprintf(g_addr, sizeof g_addr, "%s", addr ? addr : "");
  g_serving = 1;
  return YESNO_OK;
}

static yesno_status serve_stop(void) {
  g_serving = 0;
  return YESNO_OK;
}

static const yesno_plugin_api g_api = {
    {YESNO_PLUGIN_ABI_V1, (uint32_t)sizeof(yesno_plugin_api)},
    on_unavailable,
    on_available,
    on_generation_change,
    on_role_change,
    serve_start,
    serve_stop,
};

yesno_status yesno_plugin_init(const yesno_host_api *host, yesno_host_db *db,
                              const yesno_plugin_api **out) {
  if (!host || !db || !out) {
    return YESNO_INVALID_ARGUMENT;
  }
  if (host->header.version != YESNO_PLUGIN_ABI_V1) {
    return YESNO_ABI_MISMATCH;
  }
  /* The host may be newer and pass a larger table; refuse only a smaller one,
   * which would mean a member this plugin calls does not exist. */
  if (host->header.size < sizeof(yesno_host_api)) {
    return YESNO_ABI_MISMATCH;
  }
  g_host = host;
  g_db = db;
  *out = &g_api;
  return YESNO_OK;
}

/* Run one scan over `keys`, recording every block. Exported for the host test. */
yesno_status yesno_test_scan(const uint64_t *keys, size_t n) {
  yesno_snapshot *snap = NULL;
  yesno_lanes *lanes = NULL;
  yesno_status st;

  g_rows = 0;
  g_first_array_value = -1;
  g_first_run_start = -1;
  g_first_run_end = -1;

  st = g_host->snapshot_open(g_db, &snap);
  if (st != YESNO_OK) {
    return st;
  }
  st = g_host->lanes_acquire(snap, keys, n, &lanes);
  if (st != YESNO_OK) {
    g_host->snapshot_close(snap);
    return st;
  }

  /* Closing the parent while the lanes handle lives is legal and is the
   * fan-out pattern the header describes. Done here deliberately, so this test
   * plugin exercises it rather than only the easy ordering. */
  g_host->snapshot_close(snap);

  for (;;) {
    uint64_t prefix = 0;
    uint8_t done = 0;
    st = g_host->block_advance(lanes, &prefix, &done);
    if (st != YESNO_OK) {
      g_host->lanes_release(lanes);
      return st;
    }
    if (done) {
      break;
    }
    for (size_t lane = 0; lane < n; lane++) {
      yesno_chunk chunk;
      memset(&chunk, 0, sizeof chunk);
      st = g_host->block_lane(lanes, lane, &chunk);
      if (st != YESNO_OK) {
        g_host->block_release(lanes);
        g_host->lanes_release(lanes);
        return st;
      }
      /* A real kind with a NULL payload is the documented "ask again with
       * scratch". Every run answers that way. */
      if (chunk.data == NULL && chunk.kind != YESNO_CHUNK_ABSENT) {
        st = g_host->block_lane_into(lanes, lane, g_run_scratch,
                                     sizeof g_run_scratch, &chunk);
        if (st != YESNO_OK) {
          g_host->block_release(lanes);
          g_host->lanes_release(lanes);
          return st;
        }
      }
      if (chunk.kind == YESNO_CHUNK_RUN && chunk.count > 0 &&
          g_first_run_start < 0) {
        const uint16_t *pairs = (const uint16_t *)chunk.data;
        g_first_run_start = (int)pairs[0];
        g_first_run_end = (int)pairs[1];
      }
      if (g_rows < MAX_ROWS) {
        g_prefixes[g_rows] = prefix;
        g_kinds[g_rows] = chunk.kind;
        g_counts[g_rows] = chunk.count;
        g_rows++;
      }
      if (chunk.kind == YESNO_CHUNK_ARRAY && chunk.count > 0 &&
          g_first_array_value < 0) {
        const uint16_t *vals = (const uint16_t *)chunk.data;
        g_first_array_value = (int)vals[0];
      }
    }
    g_host->block_release(lanes);
  }

  g_host->lanes_release(lanes);
  return YESNO_OK;
}

size_t yesno_test_rows(void) { return g_rows; }
uint64_t yesno_test_prefix(size_t i) { return i < g_rows ? g_prefixes[i] : 0; }
uint32_t yesno_test_kind(size_t i) { return i < g_rows ? g_kinds[i] : 0xffffffffu; }
uint32_t yesno_test_count(size_t i) { return i < g_rows ? g_counts[i] : 0; }
int yesno_test_first_array_value(void) { return g_first_array_value; }
int yesno_test_first_run_start(void) { return g_first_run_start; }
int yesno_test_first_run_end(void) { return g_first_run_end; }
uint64_t yesno_test_generation(void) { return g_generation; }
int yesno_test_unavailable_calls(void) { return g_unavailable_calls; }
int yesno_test_available_calls(void) { return g_available_calls; }
int yesno_test_role_changes(void) { return g_role_changes; }
int yesno_test_serving(void) { return g_serving; }
const char *yesno_test_addr(void) { return g_addr; }

/* Deliberately advance twice without releasing, to prove the host refuses it. */
yesno_status yesno_test_double_advance(const uint64_t *keys, size_t n) {
  yesno_snapshot *snap = NULL;
  yesno_lanes *lanes = NULL;
  uint64_t prefix = 0;
  uint8_t done = 0;
  yesno_status st = g_host->snapshot_open(g_db, &snap);
  if (st != YESNO_OK) {
    return st;
  }
  st = g_host->lanes_acquire(snap, keys, n, &lanes);
  g_host->snapshot_close(snap);
  if (st != YESNO_OK) {
    return st;
  }
  st = g_host->block_advance(lanes, &prefix, &done);
  if (st == YESNO_OK) {
    st = g_host->block_advance(lanes, &prefix, &done);
  }
  g_host->lanes_release(lanes);
  return st;
}
