/* SPDX-License-Identifier: GPL-2.0-only WITH Universal-FOSS-exception-1.0 */

#include "backend.h"

#include <algorithm>
#include <cstddef>
#include <cstdint>
#include <iterator>
#include <memory>
#include <mutex>
#include <string>
#include <utility>
#include <vector>

#include "yesno/flight/client.h"
#include "vector_cursor.h"

namespace yesno_mysql {
namespace {


class FlightBackend final : public Backend {
 public:
  explicit FlightBackend(std::unique_ptr<yesno::flight::Client> client)
      : client_(std::move(client)) {}

  bool Insert(std::uint64_t key, std::uint64_t ordinal, bool *changed,
              std::string *error) override {
    std::lock_guard<std::mutex> guard(mutex_);
    return Assign(client_->Insert(key, ordinal), changed, error);
  }
  bool Remove(std::uint64_t key, std::uint64_t ordinal, bool *changed,
              std::string *error) override {
    std::lock_guard<std::mutex> guard(mutex_);
    return Assign(client_->Remove(key, ordinal), changed, error);
  }
  /// One MySQL transaction becomes **one** yesno commit.
  ///
  /// This used to be a `Clear` per cleared key, then one `RemoveMany`, then
  /// one `InsertMany` -- so a transaction that truncated *c* keys published
  /// `c + 2` separate versions, and a reader could observe the removals
  /// without the insertions, or a truncation without its replacement. The
  /// embedded backend already applied the whole plan atomically through one
  /// `yesno_batch`; this brings the Flight backend to the same guarantee
  /// rather than leaving the two backends semantically different.
  ///
  /// **Order is the point, not just atomicity.** A key's `DeleteKey` is staged
  /// ahead of its own removals and insertions, and the server applies
  /// operations on one key in the order they were staged. Grouping them by
  /// kind would still commit atomically and would turn a whole-key
  /// replacement into an empty key.
  bool Apply(const std::vector<KeyWrites> &writes,
             std::string *error) override {
    std::lock_guard<std::mutex> guard(mutex_);
    std::vector<yesno::flight::Client::Mutation> mutations;
    for (const KeyWrites &w : writes) {
      if (w.clear_first) {
        mutations.push_back(
            yesno::flight::Client::Mutation::DeleteKey(w.key));
      }
      for (std::uint64_t o : w.removes) {
        mutations.push_back(
            yesno::flight::Client::Mutation::Remove(w.key, o));
      }
      for (std::uint64_t o : w.inserts) {
        mutations.push_back(
            yesno::flight::Client::Mutation::Insert(w.key, o));
      }
    }
    if (mutations.empty()) {
      error->clear();
      return true;
    }

    auto txn = client_->BeginWrite();
    if (!txn.ok()) {
      *error = txn.status().ToString();
      return false;
    }
    auto staged = client_->Stage(*txn, mutations);
    if (!staged.ok()) {
      // Best effort: release the server's staged memory rather than waiting
      // for the deadline. The staging failure is what the caller must see.
      static_cast<void>(client_->AbortWrite(*txn));
      *error = staged.status().ToString();
      return false;
    }
    auto version = client_->CommitWrite(*txn);
    if (!version.ok()) {
      static_cast<void>(client_->AbortWrite(*txn));
      *error = version.status().ToString();
      return false;
    }
    error->clear();
    return true;
  }

  bool Contains(std::uint64_t key, std::uint64_t ordinal, bool *present,
                std::string *error) override {
    std::lock_guard<std::mutex> guard(mutex_);
    return Assign(client_->Contains(key, ordinal), present, error);
  }
  bool Cardinality(std::uint64_t key, std::uint64_t *cardinality,
                   std::string *error) override {
    std::lock_guard<std::mutex> guard(mutex_);
    return Assign(client_->Cardinality(key), cardinality, error);
  }
  bool Clear(std::uint64_t key, std::string *error) override {
    std::lock_guard<std::mutex> guard(mutex_);
    return Discard(client_->Clear(key), error);
  }
  bool Checkpoint(std::string *error) override {
    // The remote server owns its checkpoint policy and deliberately exposes no
    // administrative checkpoint action on the public Flight surface. MySQL
    // shutdown only needs to release this client's connection.
    error->clear();
    return true;
  }
  bool OpenCursor(std::uint64_t key, std::unique_ptr<Cursor> *cursor,
                  std::string *error) override {
    std::lock_guard<std::mutex> guard(mutex_);
    auto result = client_->Get(key);
    if (!result.ok()) {
      *error = result.status().ToString();
      return false;
    }
    *cursor =
        std::make_unique<VectorCursor>(std::move(result).ValueOrDie());
    error->clear();
    return true;
  }

 private:
  template <typename T>
  static bool Assign(arrow::Result<T> result, T *output,
                     std::string *error) {
    if (!result.ok()) {
      *error = result.status().ToString();
      return false;
    }
    *output = std::move(result).ValueOrDie();
    error->clear();
    return true;
  }

  template <typename T>
  static bool Discard(arrow::Result<T> result, std::string *error) {
    if (!result.ok()) {
      *error = result.status().ToString();
      return false;
    }
    error->clear();
    return true;
  }

  std::mutex mutex_;
  std::unique_ptr<yesno::flight::Client> client_;
};

}  // namespace

std::unique_ptr<Backend> OpenFlightBackend(const std::string& endpoint,
                                           std::string *error) {
  auto connected = yesno::flight::Client::Connect(endpoint);
  if (!connected.ok()) {
    *error = connected.status().ToString();
    return nullptr;
  }
  auto client = std::move(connected).ValueOrDie();
  auto probe = client->Cardinality(0);
  if (!probe.ok()) {
    *error = probe.status().ToString();
    return nullptr;
  }
  error->clear();
  return std::make_unique<FlightBackend>(std::move(client));
}

}  // namespace yesno_mysql
