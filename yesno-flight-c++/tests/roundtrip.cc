// SPDX-License-Identifier: MIT OR Apache-2.0

#include "yesno/flight/client.h"

#include <cstdint>
#include <iostream>
#include <limits>
#include <string>
#include <utility>
#include <vector>

#include <arrow/result.h>
#include <arrow/status.h>

namespace {

arrow::Status Check(bool condition, const char* message) {
  return condition ? arrow::Status::OK() : arrow::Status::Invalid(message);
}

arrow::Status RoundTrip(const std::string& endpoint) {
  ARROW_ASSIGN_OR_RAISE(auto client,
                        yesno::flight::Client::Connect(endpoint));
  constexpr std::uint64_t key = 0xf17ec001ULL;
  ARROW_ASSIGN_OR_RAISE(auto ignored, client->Clear(key));
  static_cast<void>(ignored);

  ARROW_ASSIGN_OR_RAISE(auto inserted, client->Insert(key, 7));
  ARROW_RETURN_NOT_OK(Check(inserted, "first insert did not change the set"));
  ARROW_ASSIGN_OR_RAISE(inserted, client->Insert(key, 7));
  ARROW_RETURN_NOT_OK(Check(!inserted, "duplicate insert changed the set"));
  ARROW_ASSIGN_OR_RAISE(auto present, client->Contains(key, 7));
  ARROW_RETURN_NOT_OK(Check(present, "inserted ordinal is absent"));

  std::vector<std::pair<std::uint64_t, std::uint64_t>> pairs;
  for (std::uint64_t ordinal = 10; ordinal < 9000; ++ordinal) {
    pairs.emplace_back(key, ordinal);
  }
  ARROW_ASSIGN_OR_RAISE(auto acknowledged, client->InsertMany(pairs));
  ARROW_RETURN_NOT_OK(
      Check(acknowledged == pairs.size(), "bulk acknowledgement mismatch"));

  ARROW_ASSIGN_OR_RAISE(auto query, client->PrepareKey(key));
  ARROW_RETURN_NOT_OK(
      Check(query.total_records == 8991, "wrong planned cardinality"));
  ARROW_ASSIGN_OR_RAISE(auto rows, client->Fetch(query));
  ARROW_RETURN_NOT_OK(Check(rows.size() == 8991, "wrong fetched row count"));
  ARROW_RETURN_NOT_OK(Check(rows.front() == 7 && rows.back() == 8999,
                            "fetched row bounds are wrong"));

  ARROW_ASSIGN_OR_RAISE(auto removed, client->Remove(key, 7));
  ARROW_RETURN_NOT_OK(Check(removed, "existing ordinal was not removed"));
  ARROW_ASSIGN_OR_RAISE(removed, client->Remove(key, 7));
  ARROW_RETURN_NOT_OK(Check(!removed, "missing ordinal was removed twice"));

  const auto reserved =
      client->Insert(key, std::numeric_limits<std::uint64_t>::max());
  ARROW_RETURN_NOT_OK(
      Check(!reserved.ok(), "reserved UINT64_MAX ordinal was accepted"));

  ARROW_ASSIGN_OR_RAISE(auto cleared, client->Clear(key));
  ARROW_RETURN_NOT_OK(Check(cleared > 0, "clear returned no commit version"));
  ARROW_ASSIGN_OR_RAISE(auto cardinality, client->Cardinality(key));
  ARROW_RETURN_NOT_OK(Check(cardinality == 0, "clear left rows behind"));
  return arrow::Status::OK();
}

}  // namespace

int main(int argc, char** argv) {
  if (argc != 2) {
    std::cerr << "usage: yesno-flight-cpp-roundtrip grpc://host:port\n";
    return 2;
  }
  const arrow::Status status = RoundTrip(argv[1]);
  if (!status.ok()) {
    std::cerr << status << '\n';
    return 1;
  }
  return 0;
}
