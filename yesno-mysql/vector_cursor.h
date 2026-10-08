/* SPDX-License-Identifier: GPL-2.0-only WITH Universal-FOSS-exception-1.0 */
#ifndef YESNO_MYSQL_VECTOR_CURSOR_H
#define YESNO_MYSQL_VECTOR_CURSOR_H

#include <algorithm>
#include <cstdint>
#include <string>
#include <utility>
#include <vector>

#include "backend.h"

namespace yesno_mysql {

/// A [`Cursor`] over an ascending, materialized set of ordinals.
///
/// Extracted from `backend_flight.cc` on 2026-10-08 when the channel backend
/// needed the same thing. It is pure index arithmetic over a sorted vector with
/// no transport in it, and the `kBefore` / `kAfter` sentinel handling is subtle
/// enough that a second copy would be a second set of off-by-ones -- which is
/// the whole argument, since two independent implementations of one protocol is
/// what this repository has been unpicking all week.
///
/// # Why a remote backend materializes at all
///
/// MySQL's index scans need `Prev` and a backward `Seek`, and neither the Flight
/// surface nor the plugin channel offers a backward continuation: the channel's
/// `SnapshotLoad` resumes **strictly above** a value the caller already holds,
/// and there is no downward equivalent. So a remote cursor either buffers or
/// cannot answer `ORDER BY ... DESC`. Buffering costs `O( cardinality )` memory,
/// which is the same trade `yesno-c`'s cursor makes deliberately and for a
/// related reason -- there, to avoid handing a borrowed lifetime to a foreign
/// caller; here, to supply an ordering the wire does not.
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

}  // namespace yesno_mysql

#endif
