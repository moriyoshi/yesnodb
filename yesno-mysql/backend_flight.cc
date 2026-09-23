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

namespace yesno_mysql {
namespace {

class VectorCursor final : public Cursor {
 public:
  explicit VectorCursor(std::vector<std::uint64_t> ordinals)
      : ordinals_(std::move(ordinals)) {}

  bool First(std::uint64_t *ordinal, bool *found,
             std::string *error) override {
    position_ = ordinals_.empty() ? kAfter : 0;
    return Read(ordinal, found, error);
  }
  bool Next(std::uint64_t *ordinal, bool *found,
            std::string *error) override {
    if (position_ == kBefore) {
      position_ = 0;
    } else if (position_ != kAfter) {
      ++position_;
    }
    if (position_ >= ordinals_.size()) position_ = kAfter;
    return Read(ordinal, found, error);
  }
  bool Last(std::uint64_t *ordinal, bool *found,
            std::string *error) override {
    position_ = ordinals_.empty() ? kBefore : ordinals_.size() - 1;
    return Read(ordinal, found, error);
  }
  bool Prev(std::uint64_t *ordinal, bool *found,
            std::string *error) override {
    if (position_ == kAfter) {
      position_ = ordinals_.empty() ? kBefore : ordinals_.size() - 1;
    } else if (position_ != kBefore) {
      position_ = position_ == 0 ? kBefore : position_ - 1;
    }
    return Read(ordinal, found, error);
  }
  bool Seek(std::uint64_t target, SeekMode mode, std::uint64_t *ordinal,
            bool *found, std::string *error) override {
    const auto lower =
        std::lower_bound(ordinals_.begin(), ordinals_.end(), target);
    const auto upper =
        std::upper_bound(ordinals_.begin(), ordinals_.end(), target);
    switch (mode) {
      case SeekMode::kExact:
        position_ = lower != ordinals_.end() && *lower == target
                        ? Index(lower)
                        : kAfter;
        break;
      case SeekMode::kOrNext:
        position_ = lower == ordinals_.end() ? kAfter : Index(lower);
        break;
      case SeekMode::kAfter:
        position_ = upper == ordinals_.end() ? kAfter : Index(upper);
        break;
      case SeekMode::kOrPrev:
        position_ =
            upper == ordinals_.begin() ? kBefore : Index(std::prev(upper));
        break;
      case SeekMode::kBefore:
        position_ =
            lower == ordinals_.begin() ? kBefore : Index(std::prev(lower));
        break;
    }
    return Read(ordinal, found, error);
  }

 private:
  static constexpr std::size_t kBefore =
      static_cast<std::size_t>(-1);
  static constexpr std::size_t kAfter =
      static_cast<std::size_t>(-2);

  std::size_t Index(std::vector<std::uint64_t>::const_iterator iterator) const {
    return static_cast<std::size_t>(iterator - ordinals_.begin());
  }
  bool Read(std::uint64_t *ordinal, bool *found, std::string *error) const {
    error->clear();
    if (position_ == kBefore || position_ == kAfter) {
      *found = false;
    } else {
      *ordinal = ordinals_[position_];
      *found = true;
    }
    return true;
  }

  std::vector<std::uint64_t> ordinals_;
  std::size_t position_{kBefore};
};

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
