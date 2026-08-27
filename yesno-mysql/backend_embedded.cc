/* SPDX-License-Identifier: GPL-2.0-only WITH Universal-FOSS-exception-1.0 */

#include "backend.h"

#include <array>
#include <cstddef>
#include <cstdint>
#include <memory>
#include <string>
#include <utility>

#include "yesno.h"

namespace yesno_mysql {
namespace {

constexpr std::size_t kErrorCapacity = 512;

class EmbeddedCursor final : public Cursor {
 public:
  explicit EmbeddedCursor(yesno_cursor *cursor) : cursor_(cursor) {}
  ~EmbeddedCursor() override { yesno_cursor_close(cursor_); }

  bool First(std::uint64_t *ordinal, bool *found,
             std::string *error) override {
    return Call(yesno_cursor_first, ordinal, found, error);
  }
  bool Next(std::uint64_t *ordinal, bool *found,
            std::string *error) override {
    return Call(yesno_cursor_next, ordinal, found, error);
  }
  bool Last(std::uint64_t *ordinal, bool *found,
            std::string *error) override {
    return Call(yesno_cursor_last, ordinal, found, error);
  }
  bool Prev(std::uint64_t *ordinal, bool *found,
            std::string *error) override {
    return Call(yesno_cursor_prev, ordinal, found, error);
  }
  bool Seek(std::uint64_t target, SeekMode mode, std::uint64_t *ordinal,
            bool *found, std::string *error) override {
    std::array<char, kErrorCapacity> message{};
    std::uint8_t value = 0;
    const int result = yesno_cursor_seek(
        cursor_, target, static_cast<yesno_seek_mode>(mode), ordinal, &value,
        message.data(), message.size());
    *found = value != 0;
    return Finish(result, message, error);
  }

 private:
  using CursorCall = yesno_status (*)(yesno_cursor *, std::uint64_t *,
                                      std::uint8_t *, char *, std::size_t);

  bool Call(CursorCall call, std::uint64_t *ordinal, bool *found,
            std::string *error) {
    std::array<char, kErrorCapacity> message{};
    std::uint8_t value = 0;
    const int result =
        call(cursor_, ordinal, &value, message.data(), message.size());
    *found = value != 0;
    return Finish(result, message, error);
  }

  static bool Finish(int result,
                     const std::array<char, kErrorCapacity>& message,
                     std::string *error) {
    if (result == YESNO_OK) {
      error->clear();
      return true;
    }
    *error = message.data();
    return false;
  }

  yesno_cursor *cursor_;
};

class EmbeddedBackend final : public Backend {
 public:
  explicit EmbeddedBackend(yesno_db *database) : database_(database) {}
  ~EmbeddedBackend() override { yesno_db_close(database_); }

  bool Insert(std::uint64_t key, std::uint64_t ordinal, bool *changed,
              std::string *error) override {
    return Change(yesno_db_insert, key, ordinal, changed, error);
  }
  bool Remove(std::uint64_t key, std::uint64_t ordinal, bool *changed,
              std::string *error) override {
    return Change(yesno_db_remove, key, ordinal, changed, error);
  }
  bool Contains(std::uint64_t key, std::uint64_t ordinal, bool *present,
                std::string *error) override {
    std::array<char, kErrorCapacity> message{};
    std::uint8_t value = 0;
    const int result = yesno_db_contains(database_, key, ordinal, &value,
                                         message.data(), message.size());
    *present = value != 0;
    return Finish(result, message, error);
  }
  bool Cardinality(std::uint64_t key, std::uint64_t *cardinality,
                   std::string *error) override {
    std::array<char, kErrorCapacity> message{};
    const int result = yesno_db_cardinality(
        database_, key, cardinality, message.data(), message.size());
    return Finish(result, message, error);
  }
  bool Clear(std::uint64_t key, std::string *error) override {
    std::array<char, kErrorCapacity> message{};
    const int result =
        yesno_db_clear(database_, key, message.data(), message.size());
    return Finish(result, message, error);
  }
  bool Checkpoint(std::string *error) override {
    std::array<char, kErrorCapacity> message{};
    const int result =
        yesno_db_checkpoint(database_, message.data(), message.size());
    return Finish(result, message, error);
  }
  bool OpenCursor(std::uint64_t key, std::unique_ptr<Cursor> *cursor,
                  std::string *error) override {
    std::array<char, kErrorCapacity> message{};
    yesno_cursor *raw = nullptr;
    const int result = yesno_cursor_open(database_, key, &raw, message.data(),
                                         message.size());
    if (!Finish(result, message, error)) return false;
    *cursor = std::make_unique<EmbeddedCursor>(raw);
    return true;
  }

 private:
  using ChangeCall = yesno_status (*)(const yesno_db *, std::uint64_t,
                                      std::uint64_t, std::uint8_t *, char *,
                                      std::size_t);

  bool Change(ChangeCall call, std::uint64_t key, std::uint64_t ordinal,
              bool *changed, std::string *error) {
    std::array<char, kErrorCapacity> message{};
    std::uint8_t value = 0;
    const int result = call(database_, key, ordinal, &value, message.data(),
                            message.size());
    *changed = value != 0;
    return Finish(result, message, error);
  }

  static bool Finish(int result,
                     const std::array<char, kErrorCapacity>& message,
                     std::string *error) {
    if (result == YESNO_OK) {
      error->clear();
      return true;
    }
    *error = message.data();
    return false;
  }

  yesno_db *database_;
};

}  // namespace

std::unique_ptr<Backend> OpenEmbeddedBackend(const std::string& path,
                                             std::string *error) {
  std::array<char, kErrorCapacity> message{};
  yesno_db *database = nullptr;
  if (yesno_db_open(path.c_str(), &database, message.data(), message.size()) !=
      YESNO_OK) {
    *error = message.data();
    return nullptr;
  }
  error->clear();
  return std::make_unique<EmbeddedBackend>(database);
}

#ifndef YESNO_WITH_FLIGHT
std::unique_ptr<Backend> OpenFlightBackend(const std::string&,
                                           std::string *error) {
  *error =
      "this yesno-mysql module was built without the Arrow Flight C++ backend";
  return nullptr;
}
#endif

}  // namespace yesno_mysql
