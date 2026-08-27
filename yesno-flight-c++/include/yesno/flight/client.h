// SPDX-License-Identifier: MIT OR Apache-2.0
#ifndef YESNO_FLIGHT_CLIENT_H
#define YESNO_FLIGHT_CLIENT_H

#include <cstdint>
#include <memory>
#include <string>
#include <string_view>
#include <utility>
#include <vector>

#include <arrow/result.h>

namespace yesno::flight {

// Metadata and opaque versioned ticket returned by GetFlightInfo.
struct QueryInfo {
  std::uint64_t total_records;
  std::string ticket;
};

// A yesno-shaped synchronous client over Arrow's native C++ Flight client.
//
// One Client must not be called concurrently. Callers that share it between
// host threads must serialize calls or give each thread its own connection.
class Client {
 public:
  static arrow::Result<std::unique_ptr<Client>> Connect(std::string_view uri);

  ~Client();
  Client(const Client&) = delete;
  Client& operator=(const Client&) = delete;
  Client(Client&&) noexcept;
  Client& operator=(Client&&) noexcept;

  arrow::Result<QueryInfo> PrepareKey(std::uint64_t key);
  arrow::Result<std::vector<std::uint64_t>> Fetch(const QueryInfo& query);
  arrow::Result<std::vector<std::uint64_t>> Get(std::uint64_t key);
  arrow::Result<std::uint64_t> Cardinality(std::uint64_t key);

  // Point mutations report whether the set changed. They use yesno Flight
  // actions rather than DoPut so duplicate insert and missing delete remain
  // atomic observations for database adapters.
  arrow::Result<bool> Insert(std::uint64_t key, std::uint64_t ordinal);
  arrow::Result<bool> Remove(std::uint64_t key, std::uint64_t ordinal);
  arrow::Result<bool> Contains(std::uint64_t key, std::uint64_t ordinal);
  arrow::Result<std::uint64_t> Clear(std::uint64_t key);

  // Bulk mutation uses Arrow RecordBatch streams and returns the server's
  // acknowledged pair count.
  arrow::Result<std::uint64_t> InsertMany(
      const std::vector<std::pair<std::uint64_t, std::uint64_t>>& pairs);
  arrow::Result<std::uint64_t> RemoveMany(
      const std::vector<std::pair<std::uint64_t, std::uint64_t>>& pairs);

 private:
  struct Impl;
  explicit Client(std::unique_ptr<Impl> impl);

  arrow::Result<std::uint64_t> U64Action(std::string_view name,
                                         std::string body);
  arrow::Result<bool> BoolPairAction(std::string_view name, std::uint64_t key,
                                     std::uint64_t ordinal);
  arrow::Result<std::uint64_t> Put(
      std::string_view command,
      const std::vector<std::pair<std::uint64_t, std::uint64_t>>& pairs);

  std::unique_ptr<Impl> impl_;
};

}  // namespace yesno::flight

#endif
