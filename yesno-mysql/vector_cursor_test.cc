/* SPDX-License-Identifier: GPL-2.0-only WITH Universal-FOSS-exception-1.0 */
//
// The shared ordinal cursor, and the channel backend's failure path.
//
// `VectorCursor` was file-local to `backend_flight.cc` until the channel
// backend needed it, and it had no direct test -- only whatever the MySQL
// fixtures exercised through Flight. Extracting logic that a shipping backend
// depends on, with no test of its own, is how an off-by-one gets a second home,
// so it gets one here. The `kBefore` / `kAfter` sentinels are the whole
// subtlety: a cursor must be able to sit *outside* the set at either end and
// come back, which is what `ORDER BY ... DESC` needs after a failed seek.

#include <cassert>
#include <cstdint>
#include <cstdio>
#include <memory>
#include <string>
#include <vector>

#include "backend.h"
#include "vector_cursor.h"

using yesno_mysql::Cursor;
using yesno_mysql::SeekMode;
using yesno_mysql::VectorCursor;

namespace {

struct Probe {
  std::uint64_t ordinal = 0;
  bool found = false;
  std::string error;
};

void expect(bool ok, const char *what) {
  if (!ok) {
    std::fprintf(stderr, "FAILED: %s\n", what);
    std::exit(1);
  }
}

/// Walk 10, 20, 30 forwards, backwards, and off both ends.
void forward_and_back() {
  VectorCursor cursor({10, 20, 30});
  Probe p;

  expect(cursor.First(&p.ordinal, &p.found, &p.error) && p.found &&
             p.ordinal == 10,
         "First is the smallest");
  expect(cursor.Next(&p.ordinal, &p.found, &p.error) && p.found &&
             p.ordinal == 20,
         "Next ascends");
  expect(cursor.Next(&p.ordinal, &p.found, &p.error) && p.found &&
             p.ordinal == 30,
         "Next ascends again");
  // Off the top: not found, and not an error.
  expect(cursor.Next(&p.ordinal, &p.found, &p.error) && !p.found,
         "Next past the end is not found rather than an error");
  // And back: the cursor sitting past the end must return to the last element,
  // which is the case a plain index would get wrong.
  expect(cursor.Prev(&p.ordinal, &p.found, &p.error) && p.found &&
             p.ordinal == 30,
         "Prev from past-the-end returns the last element");
  expect(cursor.Prev(&p.ordinal, &p.found, &p.error) && p.found &&
             p.ordinal == 20,
         "Prev descends");
  expect(cursor.Prev(&p.ordinal, &p.found, &p.error) && p.found &&
             p.ordinal == 10,
         "Prev descends to the first");
  expect(cursor.Prev(&p.ordinal, &p.found, &p.error) && !p.found,
         "Prev before the start is not found");
  // Symmetric: from before-the-start, Next returns the first element.
  expect(cursor.Next(&p.ordinal, &p.found, &p.error) && p.found &&
             p.ordinal == 10,
         "Next from before-the-start returns the first element");
  expect(cursor.Last(&p.ordinal, &p.found, &p.error) && p.found &&
             p.ordinal == 30,
         "Last is the largest");
}

/// Every seek mode, against a present and an absent target.
void seeks() {
  VectorCursor cursor({10, 20, 30});
  Probe p;

  expect(cursor.Seek(20, SeekMode::kExact, &p.ordinal, &p.found, &p.error) &&
             p.found && p.ordinal == 20,
         "exact seek finds a present ordinal");
  expect(cursor.Seek(25, SeekMode::kExact, &p.ordinal, &p.found, &p.error) &&
             !p.found,
         "exact seek misses an absent ordinal");

  expect(cursor.Seek(20, SeekMode::kOrNext, &p.ordinal, &p.found, &p.error) &&
             p.found && p.ordinal == 20,
         "or-next includes the target itself");
  expect(cursor.Seek(25, SeekMode::kOrNext, &p.ordinal, &p.found, &p.error) &&
             p.found && p.ordinal == 30,
         "or-next climbs to the successor");
  expect(cursor.Seek(31, SeekMode::kOrNext, &p.ordinal, &p.found, &p.error) &&
             !p.found,
         "or-next past the top is not found");

  expect(cursor.Seek(20, SeekMode::kAfter, &p.ordinal, &p.found, &p.error) &&
             p.found && p.ordinal == 30,
         "after excludes the target itself");

  expect(cursor.Seek(20, SeekMode::kOrPrev, &p.ordinal, &p.found, &p.error) &&
             p.found && p.ordinal == 20,
         "or-prev includes the target itself");
  expect(cursor.Seek(25, SeekMode::kOrPrev, &p.ordinal, &p.found, &p.error) &&
             p.found && p.ordinal == 20,
         "or-prev falls to the predecessor");
  expect(cursor.Seek(9, SeekMode::kOrPrev, &p.ordinal, &p.found, &p.error) &&
             !p.found,
         "or-prev below the bottom is not found");

  expect(cursor.Seek(20, SeekMode::kBefore, &p.ordinal, &p.found, &p.error) &&
             p.found && p.ordinal == 10,
         "before excludes the target itself");
  expect(cursor.Seek(10, SeekMode::kBefore, &p.ordinal, &p.found, &p.error) &&
             !p.found,
         "before the first is not found");

  // A failed seek must leave the cursor usable, not wedged.
  expect(cursor.Seek(99, SeekMode::kOrNext, &p.ordinal, &p.found, &p.error) &&
             !p.found,
         "seek off the top");
  expect(cursor.Prev(&p.ordinal, &p.found, &p.error) && p.found &&
             p.ordinal == 30,
         "a cursor left past the end still answers Prev");
}

/// An empty key answers every operation with not-found, never an error.
void empty_set() {
  VectorCursor cursor({});
  Probe p;
  expect(cursor.First(&p.ordinal, &p.found, &p.error) && !p.found,
         "First of nothing");
  expect(cursor.Last(&p.ordinal, &p.found, &p.error) && !p.found,
         "Last of nothing");
  expect(cursor.Next(&p.ordinal, &p.found, &p.error) && !p.found,
         "Next of nothing");
  expect(cursor.Prev(&p.ordinal, &p.found, &p.error) && !p.found,
         "Prev of nothing");
  expect(cursor.Seek(1, SeekMode::kOrNext, &p.ordinal, &p.found, &p.error) &&
             !p.found,
         "Seek in nothing");
}

/// A single element, where before-the-start and past-the-end are adjacent.
void one_element() {
  VectorCursor cursor({42});
  Probe p;
  expect(cursor.First(&p.ordinal, &p.found, &p.error) && p.found &&
             p.ordinal == 42,
         "First of one");
  expect(cursor.Next(&p.ordinal, &p.found, &p.error) && !p.found,
         "Next off the only element");
  expect(cursor.Prev(&p.ordinal, &p.found, &p.error) && p.found &&
             p.ordinal == 42,
         "Prev back onto the only element");
  expect(cursor.Prev(&p.ordinal, &p.found, &p.error) && !p.found,
         "Prev off the bottom");
}

/// The channel backend refuses an absent socket rather than crashing, and says
/// something an operator can act on.
void absent_socket() {
  std::string error;
  auto backend = yesno_mysql::OpenChannelBackend("/nonexistent/yesno.sock",
                                                 &error);
  expect(backend == nullptr, "an absent socket yields no backend");
  expect(!error.empty(), "and leaves a message");
  expect(error.find("yesno channel") != std::string::npos,
         "the message names the transport");
  std::printf("absent socket reported: %s\n", error.c_str());
}

}  // namespace

int main() {
  forward_and_back();
  seeks();
  empty_set();
  one_element();
  absent_socket();
  std::printf("yesno-mysql shared cursor and channel backend tests passed\n");
  return 0;
}
