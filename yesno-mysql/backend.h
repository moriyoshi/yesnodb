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
/// The embedded backend only ever reports `kOk` and `kError`, and the Flight
/// backend those and `kInterrupted`. That is not a gap: an embedded database
/// has no peer to be stale against, and the Flight surface exposes no
/// equivalent classification. A backend returning only the two is conforming.
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
  /// The query was killed while the backend waited for its server. Only the
  /// remote backends report it; the statement is over, so nothing retries.
  kInterrupted = 6,
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
  /// **Every backend applies these atomically across every key**: the
  /// embedded one in one `yesno_batch`, the Flight one in one write
  /// transaction, and the channel one in one `Apply` frame.
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

/// The Flight backend: a yesnod's Flight endpoint.
///
/// Its first call has 10 s to be answered, so an endpoint that accepts and
/// never answers fails the open rather than holding mysqld's startup. Later
/// calls have no time limit and answer `should_stop` instead, exactly as
/// `OpenChannelBackend` describes. A Flight call cannot be cancelled from
/// another thread, so each runs on a thread of the backend's and a stopped
/// session stops *waiting* for it; the call goes on until its server answers,
/// and the backend connects afresh for the next one.
std::unique_ptr<Backend> OpenFlightBackend(const std::string &endpoint,
                                           int (*should_stop)(void *),
                                           std::string *error);

/// The plugin-channel backend: a Unix socket to a yesnod that already holds the
/// database's exclusive lock.
///
/// `should_stop` is called with a null argument on whichever thread is waiting,
/// about every 50 ms, and nonzero ends the wait with `kInterrupted`. The engine
/// passes a check of the waiting session's kill flag, which is what lets `KILL
/// QUERY` and `max_execution_time` end a query whose yesnod stopped answering.
std::unique_ptr<Backend> OpenChannelBackend(const std::string &socket_path,
                                            int (*should_stop)(void *),
                                            std::string *error);

}  // namespace yesno_mysql

#endif
