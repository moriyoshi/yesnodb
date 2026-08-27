/* SPDX-License-Identifier: GPL-2.0-only WITH Universal-FOSS-exception-1.0 */
#ifndef YESNO_MYSQL_BACKEND_H
#define YESNO_MYSQL_BACKEND_H

#include <cstdint>
#include <memory>
#include <string>
#include <vector>

namespace yesno_mysql {

enum class SeekMode {
  kExact = 0,
  kOrNext = 1,
  kAfter = 2,
  kOrPrev = 3,
  kBefore = 4,
};

class Cursor {
 public:
  virtual ~Cursor() = default;
  virtual bool First(std::uint64_t *ordinal, bool *found,
                     std::string *error) = 0;
  virtual bool Next(std::uint64_t *ordinal, bool *found,
                    std::string *error) = 0;
  virtual bool Last(std::uint64_t *ordinal, bool *found,
                    std::string *error) = 0;
  virtual bool Prev(std::uint64_t *ordinal, bool *found,
                    std::string *error) = 0;
  virtual bool Seek(std::uint64_t target, SeekMode mode,
                    std::uint64_t *ordinal, bool *found,
                    std::string *error) = 0;
};

/// One key's worth of a transaction's buffered writes, ready to apply.
///
/// `clear_first` carries a `TRUNCATE` that happened inside the transaction: the
/// key is emptied and then the surviving inserts are applied, which is what
/// makes `TRUNCATE; INSERT; ROLLBACK` leave the original rows alone.
struct KeyWrites {
  std::uint64_t key;
  bool clear_first;
  std::vector<std::uint64_t> inserts;
  std::vector<std::uint64_t> removes;
};

class Backend {
 public:
  virtual ~Backend() = default;

  /// Apply a whole transaction's writes.
  ///
  /// **The embedded backend applies these atomically across every key**, by
  /// putting them in one `yesno_batch`. The Flight backend sends one
  /// `RemoveMany` and one `InsertMany` covering every key, so each call is a
  /// batch but the pair is not one transaction -- a failure between them leaves
  /// the removals applied. Removals go first, so that partial state is a
  /// smaller set rather than a larger one.
  virtual bool Apply(const std::vector<KeyWrites> &writes,
                     std::string *error) = 0;
  virtual bool Insert(std::uint64_t key, std::uint64_t ordinal, bool *changed,
                      std::string *error) = 0;
  virtual bool Remove(std::uint64_t key, std::uint64_t ordinal, bool *changed,
                      std::string *error) = 0;
  virtual bool Contains(std::uint64_t key, std::uint64_t ordinal, bool *present,
                        std::string *error) = 0;
  virtual bool Cardinality(std::uint64_t key, std::uint64_t *cardinality,
                           std::string *error) = 0;
  virtual bool Clear(std::uint64_t key, std::string *error) = 0;
  virtual bool Checkpoint(std::string *error) = 0;
  virtual bool OpenCursor(std::uint64_t key, std::unique_ptr<Cursor> *cursor,
                          std::string *error) = 0;
};

std::unique_ptr<Backend> OpenEmbeddedBackend(const std::string& path,
                                             std::string *error);
std::unique_ptr<Backend> OpenFlightBackend(const std::string& endpoint,
                                           std::string *error);

}  // namespace yesno_mysql

#endif
