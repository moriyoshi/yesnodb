/* SPDX-License-Identifier: GPL-2.0-only WITH Universal-FOSS-exception-1.0 */
#ifndef YESNO_MYSQL_BACKEND_H
#define YESNO_MYSQL_BACKEND_H

#include <cstdint>
#include <memory>
#include <string>
#include <vector>

namespace yesno_mysql {

/// How a backend call ended.
///
/// Wider than a bool because the plugin channel distinguishes failures that a
/// host must handle differently, and a bool plus a message forces every one of
/// them to be treated as fatal. The channel's own C header carries the same
/// five, and this is where they stop being a string an operator reads and start
/// being something `ha_yesno.cc` can branch on.
///
/// The embedded and Flight backends only ever report `kOk` and `kError`. That
/// is not a gap: an embedded database has no peer to be stale against, and the
/// Flight surface exposes no equivalent classification. A backend returning
/// only the two is conforming.
enum class BackendStatus {
  kOk = 0,
  /// An unclassified failure. The message is all there is.
  kError = 1,
  /// Transient: the server has no database open while a follower rebootstraps.
  /// **The only status worth retrying.** The handle is still good.
  kRetry = 2,
  /// The database was replaced. Every handle this backend holds is dead and it
  /// must reconnect; retrying the call cannot help.
  kStale = 3,
  /// The pinned read version is gone. A fresh read may succeed, so this is
  /// distinct from `kStale`: the connection is fine and only the snapshot died.
  kExpired = 4,
  /// The request needed a leader and reached a read-only replica.
  kWrongRole = 5,
};

enum class SeekMode {
  kExact = 0,
  kOrNext = 1,
  kAfter = 2,
  kOrPrev = 3,
  kBefore = 4,
};

/// A positioned scan over one key's ordinals.
///
/// **Deliberately still `bool`.** Every remote backend materializes the key
/// before handing a cursor back -- it has to, because neither Flight nor the
/// channel offers a backward continuation and MySQL needs `Prev` -- so by the
/// time a cursor exists there is no transport left to fail, and the embedded
/// cursor's failures are plain errors with no peer to be stale against. Giving
/// it a [`BackendStatus`] would widen an interface whose implementations could
/// never return anything but the two ends of it.
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
  virtual BackendStatus Apply(const std::vector<KeyWrites> &writes,
                     std::string *error) = 0;
  virtual BackendStatus Insert(std::uint64_t key, std::uint64_t ordinal, bool *changed,
                      std::string *error) = 0;
  virtual BackendStatus Remove(std::uint64_t key, std::uint64_t ordinal, bool *changed,
                      std::string *error) = 0;
  virtual BackendStatus Contains(std::uint64_t key, std::uint64_t ordinal, bool *present,
                        std::string *error) = 0;
  virtual BackendStatus Cardinality(std::uint64_t key, std::uint64_t *cardinality,
                           std::string *error) = 0;
  virtual BackendStatus Clear(std::uint64_t key, std::string *error) = 0;
  virtual BackendStatus Checkpoint(std::string *error) = 0;
  virtual BackendStatus OpenCursor(std::uint64_t key, std::unique_ptr<Cursor> *cursor,
                          std::string *error) = 0;
};

std::unique_ptr<Backend> OpenEmbeddedBackend(const std::string& path,
                                             std::string *error);
std::unique_ptr<Backend> OpenFlightBackend(const std::string& endpoint,
                                           std::string *error);

/// The plugin-channel backend: a Unix socket to a yesnod that already holds the
/// database's exclusive lock.
///
/// Unlike `OpenFlightBackend`, its `Apply` is **one commit** across every key,
/// because a single channel `Apply` frame carries them all -- so its atomicity
/// matches the embedded backend's rather than Flight's.
std::unique_ptr<Backend> OpenChannelBackend(const std::string &socket_path,
                                            std::string *error);

}  // namespace yesno_mysql

#endif
