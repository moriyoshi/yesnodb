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
