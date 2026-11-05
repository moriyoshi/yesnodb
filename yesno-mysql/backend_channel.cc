/* SPDX-License-Identifier: GPL-2.0-only WITH Universal-FOSS-exception-1.0 */
//
// The plugin-channel backend: a Unix socket to a running yesnod.
//
// The third Backend, beside embedded and flight, and the only one that can be
// used by a host yesnod already has the directory lock on. It speaks
// yesno-plugin's channel through `yesno_channel.h`, so the framing, the
// descriptor handoff and the arena mapping are one Rust implementation rather
// than a second hand-written client.

#include <algorithm>
#include <chrono>
#include <cstdint>
#include <memory>
#include <mutex>
#include <string>
#include <utility>
#include <vector>

#include "backend.h"
#include "vector_cursor.h"
#include "yesno_channel.h"

namespace yesno_mysql {
namespace {

/// One page of ordinals per round trip when materializing a cursor.
///
/// Not `MAX_PAGE`: the server caps the page anyway, and asking for its maximum
/// would size the stack buffer below by the protocol's limit rather than by
/// anything this backend needs.
constexpr std::size_t kPage = 1024;

/// How long opening the channel may wait for yesnod's greeting.
///
/// The first open runs inside mysqld's startup, where no kill can reach it, so
/// without a limit a yesnod that accepts and never greets holds the whole
/// server's startup for ever. Measured 2026-11-06: mysqld never accepted a
/// connection. On expiry the open fails, the plugin does not initialize, and
/// mysqld starts without the engine.
constexpr std::uint32_t kHandshakeTimeoutMs = 10000;

/// How often a thread waiting for the connection checks whether it was killed.
constexpr std::chrono::milliseconds kLockPoll{50};

/// Fold a channel status and its message into a `Backend` error string.
///
/// **The classification is lost here and that is a real cost, recorded rather
/// than hidden.** `yesno_channel.h` distinguishes RETRY -- the server has no
/// database open while a follower rebootstraps, and the call is worth trying
/// again -- from STALE, EXPIRED and WRONG_ROLE, each needing different handling.
/// `Backend` returns a bool and a string, so none of that reaches MySQL as
/// anything it can branch on. The code is put in the message so an operator can
/// see it, and widening `Backend` is what it would take to act on it.
BackendStatus Fail(yesno_channel_status status, const char *buffer,
                   std::string *error) {
  const char *name = "error";
  BackendStatus mapped = BackendStatus::kError;
  switch (status) {
    case YESNO_CHANNEL_RETRY:
      name = "unavailable (retryable)";
      mapped = BackendStatus::kRetry;
      break;
    case YESNO_CHANNEL_STALE:
      name = "stale (reconnect)";
      mapped = BackendStatus::kStale;
      break;
    case YESNO_CHANNEL_EXPIRED:
      name = "snapshot expired";
      mapped = BackendStatus::kExpired;
      break;
    case YESNO_CHANNEL_WRONG_ROLE:
      name = "read-only replica";
      mapped = BackendStatus::kWrongRole;
      break;
    case YESNO_CHANNEL_INTERRUPTED:
      name = "interrupted";
      mapped = BackendStatus::kInterrupted;
      break;
    case YESNO_CHANNEL_OK:
    case YESNO_CHANNEL_ERROR:
      break;
  }
  *error = std::string("yesno channel ") + name + ": " + buffer;
  return mapped;
}

/// A snapshot handle that closes itself.
///
/// One snapshot per read operation, opened and released around it. The
/// alternative -- holding one open -- would pin a version for the connection's
/// life and serve every later read from a database that had stopped changing,
/// so the two round trips are the price of seeing committed writes.
class ScopedSnapshot {
 public:
  ScopedSnapshot() = default;
  ~ScopedSnapshot() { yesno_channel_snapshot_close(handle_); }
  ScopedSnapshot(const ScopedSnapshot &) = delete;
  ScopedSnapshot &operator=(const ScopedSnapshot &) = delete;

  /// Returns a [`BackendStatus`] rather than a bool so that a snapshot the
  /// server refused for a *classified* reason -- a follower mid-rebootstrap,
  /// say -- reaches the caller as that reason and not as a bare failure. Every
  /// read below forwards it unchanged.
  BackendStatus Open(yesno_channel *channel, char *buffer,
                     std::size_t capacity, std::string *error) {
    const yesno_channel_status status =
        yesno_channel_snapshot_open(channel, &handle_, buffer, capacity);
    if (status != YESNO_CHANNEL_OK) return Fail(status, buffer, error);
    return BackendStatus::kOk;
  }
  yesno_channel_snapshot *get() const { return handle_; }

 private:
  yesno_channel_snapshot *handle_{nullptr};
};

/// One connection, shared by every session and taken in turn.
///
/// **Every wait here answers a kill.** A request waits in the channel, which
/// runs `should_stop_` while yesnod is silent, and a session queued behind it
/// waits in [`ChannelBackend::Enter`], which checks the same flag. A killed
/// request leaves the channel broken, because its answer may still arrive and
/// would be read as the next request's, so `Enter` opens a fresh one for the
/// next session.
class ChannelBackend final : public Backend {
 public:
  ChannelBackend(std::string socket_path, int (*should_stop)(void *),
                 yesno_channel *channel)
      : socket_path_(std::move(socket_path)),
        should_stop_(should_stop),
        channel_(channel) {}
  ~ChannelBackend() override { yesno_channel_close(channel_); }

  BackendStatus Insert(std::uint64_t key, std::uint64_t ordinal, bool *changed,
              std::string *error) override {
    return One(key, ordinal, YESNO_CHANNEL_INSERT, changed, error);
  }
  BackendStatus Remove(std::uint64_t key, std::uint64_t ordinal, bool *changed,
              std::string *error) override {
    return One(key, ordinal, YESNO_CHANNEL_REMOVE, changed, error);
  }

  /// Apply a whole transaction in **one** commit.
  ///
  /// Unlike the Flight backend, which sends a removal batch and an insertion
  /// batch and is therefore not one transaction, a single `Apply` frame carries
  /// every key's writes and the server commits them together. So this backend
  /// matches the embedded one's atomicity rather than Flight's.
  ///
  /// The limit is the server's advertised `max_writes`, and exceeding it is an
  /// **error rather than a split**. Splitting would silently give up the
  /// atomicity this paragraph just promised, and a transaction that half
  /// applied would be worse than one refused.
  BackendStatus Apply(const std::vector<KeyWrites> &writes,
             std::string *error) override {
    std::unique_lock<std::timed_mutex> guard;
    if (const BackendStatus entered = Enter(&guard, error);
        entered != BackendStatus::kOk) {
      return entered;
    }
    std::vector<yesno_channel_write> flat;
    for (const KeyWrites &per_key : writes) {
      // A `TRUNCATE` inside the transaction empties the key before the
      // surviving inserts land, which is what makes TRUNCATE; INSERT; ROLLBACK
      // leave the original rows alone.
      if (per_key.clear_first) {
        flat.push_back({per_key.key, 0, 0, YESNO_CHANNEL_DELETE_KEY});
      }
      // Removals before insertions, so that a re-inserted ordinal survives.
      for (const std::uint64_t ordinal : per_key.removes) {
        flat.push_back({per_key.key, ordinal, ordinal, YESNO_CHANNEL_REMOVE});
      }
      for (const std::uint64_t ordinal : per_key.inserts) {
        flat.push_back({per_key.key, ordinal, ordinal, YESNO_CHANNEL_INSERT});
      }
    }
    if (flat.empty()) {
      error->clear();
      return BackendStatus::kOk;
    }
    char buffer[512] = {0};
    yesno_channel_limits limits{};
    const yesno_channel_status got =
        yesno_channel_get_limits(channel_, &limits, buffer, sizeof buffer);
    if (got != YESNO_CHANNEL_OK) return Fail(got, buffer, error);
    if (flat.size() > limits.max_writes) {
      *error = "yesno channel: this transaction needs " +
               std::to_string(flat.size()) + " writes and the server accepts " +
               std::to_string(limits.max_writes) +
               " in one commit; splitting it would give up atomicity";
      return BackendStatus::kError;
    }
    std::uint64_t version = 0;
    std::uint64_t changed = 0;
    const yesno_channel_status status =
        yesno_channel_apply(channel_, flat.data(), flat.size(), &version,
                            &changed, buffer, sizeof buffer);
    if (status != YESNO_CHANNEL_OK) return Fail(status, buffer, error);
    error->clear();
    return BackendStatus::kOk;
  }

  BackendStatus Contains(std::uint64_t key, std::uint64_t ordinal, bool *present,
                std::string *error) override {
    std::unique_lock<std::timed_mutex> guard;
    if (const BackendStatus entered = Enter(&guard, error);
        entered != BackendStatus::kOk) {
      return entered;
    }
    char buffer[512] = {0};
    ScopedSnapshot snapshot;
    const BackendStatus opened =
        snapshot.Open(channel_, buffer, sizeof buffer, error);
    // Forwarded, not flattened: a snapshot refused because a follower is
    // rebootstrapping is retryable, and collapsing it to kError here would
    // discard exactly what the widening exists to carry.
    if (opened != BackendStatus::kOk) return opened;
    std::uint8_t found = 0;
    const yesno_channel_status status = yesno_channel_contains(
        snapshot.get(), key, ordinal, &found, buffer, sizeof buffer);
    if (status != YESNO_CHANNEL_OK) return Fail(status, buffer, error);
    *present = found != 0;
    error->clear();
    return BackendStatus::kOk;
  }

  BackendStatus Cardinality(std::uint64_t key, std::uint64_t *cardinality,
                   std::string *error) override {
    std::unique_lock<std::timed_mutex> guard;
    if (const BackendStatus entered = Enter(&guard, error);
        entered != BackendStatus::kOk) {
      return entered;
    }
    char buffer[512] = {0};
    ScopedSnapshot snapshot;
    const BackendStatus opened =
        snapshot.Open(channel_, buffer, sizeof buffer, error);
    // Forwarded, not flattened: a snapshot refused because a follower is
    // rebootstrapping is retryable, and collapsing it to kError here would
    // discard exactly what the widening exists to carry.
    if (opened != BackendStatus::kOk) return opened;
    const yesno_channel_status status = yesno_channel_cardinality(
        snapshot.get(), key, cardinality, buffer, sizeof buffer);
    if (status != YESNO_CHANNEL_OK) return Fail(status, buffer, error);
    error->clear();
    return BackendStatus::kOk;
  }

  BackendStatus Clear(std::uint64_t key, std::string *error) override {
    std::unique_lock<std::timed_mutex> guard;
    if (const BackendStatus entered = Enter(&guard, error);
        entered != BackendStatus::kOk) {
      return entered;
    }
    const yesno_channel_write write{key, 0, 0, YESNO_CHANNEL_DELETE_KEY};
    char buffer[512] = {0};
    std::uint64_t version = 0;
    std::uint64_t changed = 0;
    const yesno_channel_status status = yesno_channel_apply(
        channel_, &write, 1, &version, &changed, buffer, sizeof buffer);
    if (status != YESNO_CHANNEL_OK) return Fail(status, buffer, error);
    error->clear();
    return BackendStatus::kOk;
  }

  /// A no-op that succeeds, for the reason the Flight backend's does.
  ///
  /// yesnod owns the database and its checkpoint policy, and the channel
  /// deliberately exposes no administrative checkpoint: there is no frame for
  /// it. Nothing is buffered on this side either -- `Apply` has already
  /// committed server-side by the time it returns -- so there is nothing a
  /// checkpoint here could flush. MySQL shutdown only needs the socket closed,
  /// which the destructor does.
  BackendStatus Checkpoint(std::string *error) override {
    error->clear();
    return BackendStatus::kOk;
  }

  /// Materialize the key's ordinals and hand back a [`VectorCursor`].
  ///
  /// Paged with `SnapshotLoad`, whose continuation is **by value**: each page
  /// resumes strictly above the last ordinal received, so there is no
  /// server-side cursor to leak or expire, and the pinned snapshot is what makes
  /// resuming from a value yield a consistent sequence rather than a smear of
  /// two states. The snapshot is released as soon as the last page is in hand.
  ///
  /// It materializes because MySQL needs `Prev` and a backward `Seek` and the
  /// protocol has no backward continuation at all -- see `vector_cursor.h`.
  BackendStatus OpenCursor(std::uint64_t key, std::unique_ptr<Cursor> *cursor,
                  std::string *error) override {
    std::unique_lock<std::timed_mutex> guard;
    if (const BackendStatus entered = Enter(&guard, error);
        entered != BackendStatus::kOk) {
      return entered;
    }
    char buffer[512] = {0};
    ScopedSnapshot snapshot;
    const BackendStatus opened =
        snapshot.Open(channel_, buffer, sizeof buffer, error);
    // Forwarded, not flattened: a snapshot refused because a follower is
    // rebootstrapping is retryable, and collapsing it to kError here would
    // discard exactly what the widening exists to carry.
    if (opened != BackendStatus::kOk) return opened;

    std::vector<std::uint64_t> ordinals;
    std::uint64_t page[kPage];
    std::uint8_t has_after = 0;
    std::uint64_t after = 0;
    for (;;) {
      std::size_t written = 0;
      std::uint8_t more = 0;
      const yesno_channel_status status = yesno_channel_load(
          snapshot.get(), key, has_after, after, static_cast<std::uint32_t>(kPage),
          page, kPage, &written, &more, buffer, sizeof buffer);
      if (status != YESNO_CHANNEL_OK) return Fail(status, buffer, error);
      ordinals.insert(ordinals.end(), page, page + written);
      if (more == 0) break;
      if (written == 0) {
        // The server said more follow and sent none. Believing it would spin
        // forever, so this reports instead of hanging.
        *error = "yesno channel: a page was empty but more were promised";
        return BackendStatus::kError;
      }
      after = ordinals.back();
      has_after = 1;
    }
    *cursor = std::make_unique<VectorCursor>(std::move(ordinals));
    error->clear();
    return BackendStatus::kOk;
  }

 private:
  /// Take the connection, giving up if this session is killed while another
  /// holds it, and reopen the channel if the last request on it was abandoned.
  BackendStatus Enter(std::unique_lock<std::timed_mutex> *guard,
                      std::string *error) {
    *guard = std::unique_lock<std::timed_mutex>(mutex_, std::defer_lock);
    while (!guard->try_lock_for(kLockPoll)) {
      if (should_stop_ != nullptr && should_stop_(nullptr) != 0) {
        *error = "yesno channel: interrupted while another query held the connection";
        return BackendStatus::kInterrupted;
      }
    }
    char buffer[512] = {0};
    if (channel_ != nullptr) {
      std::uint8_t broken = 1;
      if (yesno_channel_is_broken(channel_, &broken, buffer, sizeof buffer) ==
              YESNO_CHANNEL_OK &&
          broken == 0) {
        return BackendStatus::kOk;
      }
      yesno_channel_close(channel_);
      channel_ = nullptr;
    }
    const yesno_channel_status status = yesno_channel_open_waiting(
        socket_path_.c_str(), "mysql", kHandshakeTimeoutMs, should_stop_, nullptr,
        &channel_, buffer, sizeof buffer);
    if (status != YESNO_CHANNEL_OK) {
      channel_ = nullptr;
      return Fail(status, buffer, error);
    }
    return BackendStatus::kOk;
  }

  BackendStatus One(std::uint64_t key, std::uint64_t ordinal,
           yesno_channel_write_op op, bool *changed, std::string *error) {
    std::unique_lock<std::timed_mutex> guard;
    if (const BackendStatus entered = Enter(&guard, error);
        entered != BackendStatus::kOk) {
      return entered;
    }
    const yesno_channel_write write{key, ordinal, ordinal,
                                    static_cast<std::uint8_t>(op)};
    char buffer[512] = {0};
    std::uint64_t version = 0;
    std::uint64_t count = 0;
    const yesno_channel_status status = yesno_channel_apply(
        channel_, &write, 1, &version, &count, buffer, sizeof buffer);
    if (status != YESNO_CHANNEL_OK) return Fail(status, buffer, error);
    *changed = count != 0;
    error->clear();
    return BackendStatus::kOk;
  }

  const std::string socket_path_;
  int (*const should_stop_)(void *);
  /// Null only between a broken channel being closed and its replacement
  /// opening, which `Enter` completes or reports before anything reads it.
  yesno_channel *channel_;
  std::timed_mutex mutex_;
};

}  // namespace

std::unique_ptr<Backend> OpenChannelBackend(const std::string &socket_path,
                                            int (*should_stop)(void *),
                                            std::string *error) {
  char buffer[512] = {0};
  yesno_channel *channel = nullptr;
  const yesno_channel_status status = yesno_channel_open_waiting(
      socket_path.c_str(), "mysql", kHandshakeTimeoutMs, should_stop, nullptr,
      &channel, buffer, sizeof buffer);
  if (status != YESNO_CHANNEL_OK) {
    Fail(status, buffer, error);
    return nullptr;
  }
  error->clear();
  return std::make_unique<ChannelBackend>(socket_path, should_stop, channel);
}

}  // namespace yesno_mysql
