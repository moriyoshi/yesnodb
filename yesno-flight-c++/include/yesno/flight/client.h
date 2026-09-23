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
  /// One staged operation. `op` is one of the `kOp*` constants below.
  struct Mutation {
    std::uint64_t key = 0;
    std::uint64_t lo = 0;
    std::uint64_t hi = 0;
    std::uint8_t op = 0;

    static Mutation Insert(std::uint64_t key, std::uint64_t ordinal) {
      return Mutation{key, ordinal, ordinal, kOpInsert};
    }
    static Mutation Remove(std::uint64_t key, std::uint64_t ordinal) {
      return Mutation{key, ordinal, ordinal, kOpRemove};
    }
    static Mutation InsertRange(std::uint64_t key, std::uint64_t lo,
                                std::uint64_t hi) {
      return Mutation{key, lo, hi, kOpInsertRange};
    }
    static Mutation RemoveRange(std::uint64_t key, std::uint64_t lo,
                                std::uint64_t hi) {
      return Mutation{key, lo, hi, kOpRemoveRange};
    }
    static Mutation DeleteKey(std::uint64_t key) {
      return Mutation{key, 0, 0, kOpDeleteKey};
    }
  };

  static constexpr std::uint8_t kOpInsert = 0;
  static constexpr std::uint8_t kOpRemove = 1;
  static constexpr std::uint8_t kOpInsertRange = 2;
  static constexpr std::uint8_t kOpRemoveRange = 3;
  static constexpr std::uint8_t kOpDeleteKey = 4;

  /// An open write transaction: nothing staged under it is visible until
  /// `CommitWrite`, which publishes all of it at one version.
  ///
  /// Deliberately not a query ticket. A ticket names an immutable version and
  /// is safe to cache and replay; this owns mutable staged state with an
  /// expiry and exactly one resolution.
  using WriteTxn = std::uint64_t;

  arrow::Result<WriteTxn> BeginWrite();

  /// Append operations, which take effect in the order they are staged.
  ///
  /// One stream at a time per transaction: the server refuses a concurrent
  /// one rather than interleave two into an order the caller cannot predict.
  arrow::Result<std::uint64_t> Stage(WriteTxn txn,
                                     const std::vector<Mutation>& mutations);

  /// Publish, returning the one version the staged work became visible at.
  ///
  /// Idempotent per handle: a retry after a lost response returns the original
  /// version rather than applying the work twice.
  arrow::Result<std::uint64_t> CommitWrite(WriteTxn txn);

  /// Discard staged work. Aborting one already committed is an error.
  arrow::Status AbortWrite(WriteTxn txn);

  /// Mixed operations as one commit, without a handle, for work that fits in
  /// one request.
  arrow::Result<std::uint64_t> Apply(const std::vector<Mutation>& mutations);

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
  arrow::Result<std::uint64_t> PutMutations(
      std::string_view command, const std::vector<Mutation>& mutations);
  arrow::Result<std::uint64_t> Put(
      std::string_view command,
      const std::vector<std::pair<std::uint64_t, std::uint64_t>>& pairs);

  std::unique_ptr<Impl> impl_;
};

}  // namespace yesno::flight

#endif
