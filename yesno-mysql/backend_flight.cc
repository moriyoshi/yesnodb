/* SPDX-License-Identifier: GPL-2.0-only WITH Universal-FOSS-exception-1.0 */

#include "backend.h"

#include <dlfcn.h>

#include <atomic>
#include <chrono>
#include <condition_variable>
#include <cstddef>
#include <cstdint>
#include <functional>
#include <memory>
#include <mutex>
#include <optional>
#include <string>
#include <thread>
#include <utility>
#include <vector>

#include <arrow/flight/types.h>

#include "yesno/flight/client.h"
#include "vector_cursor.h"

namespace yesno_mysql {
namespace {

/// How often a waiting session checks its stop flag, as the channel does.
constexpr std::chrono::milliseconds kStopPoll{50};

/// How long opening waits for the endpoint's first answer: the channel's
/// handshake limit, for the same reason.
constexpr std::chrono::milliseconds kOpenTimeout{10000};

/// Keeps this plugin's code mapped for the rest of the process.
///
/// An abandoned call's thread runs this library's code until its server
/// answers, which can be after `UNINSTALL PLUGIN` has unloaded the library.
/// The extra reference taken here, never released, turns that unload into a
/// no-op rather than unmapping code a thread is still in. Taken on the first
/// abandonment only, so a server that never kills a Flight wait unloads as
/// before.
void PinThisLibrary() {
  static std::once_flag once;
  std::call_once(once, [] {
    Dl_info info{};
    if (dladdr(reinterpret_cast<void *>(&PinThisLibrary), &info) != 0 &&
        info.dli_fname != nullptr) {
      static_cast<void>(
          dlopen(info.dli_fname, RTLD_NOW | RTLD_NOLOAD | RTLD_NODELETE));
    }
  });
}

/// One thread that makes a backend's Flight calls, so that the session waiting
/// for one can stop waiting.
///
/// **A Flight call cannot be ended from outside.** Arrow reads a call's stop
/// token only between stream messages, and `DoAction`, `DoPut` and a unary
/// `GetFlightInfo` give no handle to cancel through, so a thread blocked in one
/// waits until the server answers. What can be ended is the *waiting*: the call
/// runs here, the session waits for it in `kStopPoll` slices checking its stop
/// flag, and a stopped session walks away. The backend then replaces this
/// worker and its client. The abandoned call keeps its client alive, finishes
/// whenever the server answers, and its thread exits.
class Worker {
 public:
  /// A call. `abandoned` turns true once the session has stopped waiting for
  /// it, which a call made of several requests can read between them.
  using Job = std::function<void(const std::atomic<bool> &abandoned)>;

  Worker() : state_(std::make_shared<State>()) {
    thread_ = std::thread([state = state_] { Loop(*state); });
  }

  /// Joins an idle worker. An abandoned one may still be inside its call, so
  /// it is let go instead.
  ~Worker() {
    {
      std::lock_guard<std::mutex> lock(state_->mutex);
      state_->quit = true;
    }
    state_->wake.notify_all();
    if (state_->abandoned.load()) {
      thread_.detach();
    } else {
      thread_.join();
    }
  }

  Worker(const Worker &) = delete;
  Worker &operator=(const Worker &) = delete;

  /// Runs `job` here and waits for it. False if `should_stop` ended the wait
  /// first, after which this worker is abandoned and must not be used again.
  bool Run(Job job, int (*should_stop)(void *)) {
    std::unique_lock<std::mutex> lock(state_->mutex);
    state_->job = std::move(job);
    state_->wake.notify_all();
    while (!state_->wake.wait_for(lock, kStopPoll, [this] {
      return state_->job == nullptr && !state_->running;
    })) {
      if (should_stop != nullptr && should_stop(nullptr) != 0) {
        // A job not yet started is dropped rather than run for nobody.
        state_->quit = true;
        state_->abandoned.store(true);
        PinThisLibrary();
        return false;
      }
    }
    return true;
  }

 private:
  struct State {
    std::mutex mutex;
    std::condition_variable wake;
    Job job;
    bool running = false;
    bool quit = false;
    std::atomic<bool> abandoned{false};
  };

  static void Loop(State &state) {
    std::unique_lock<std::mutex> lock(state.mutex);
    for (;;) {
      state.wake.wait(lock, [&state] { return state.quit || state.job != nullptr; });
      if (state.quit) return;
      Job job = std::move(state.job);
      state.job = nullptr;
      state.running = true;
      lock.unlock();
      job(state.abandoned);
      lock.lock();
      state.running = false;
      state.wake.notify_all();
    }
  }

  std::shared_ptr<State> state_;
  std::thread thread_;
};

/// The Flight backend.
///
/// `should_stop` is called on whichever thread is waiting, as the channel
/// backend's is, and nonzero ends the wait with `kInterrupted` -- both a wait
/// for the server, through [`Worker`], and a wait for another session to finish
/// with the connection. A killed wait leaves its call running, so the next call
/// connects afresh rather than queue behind it.
class FlightBackend final : public Backend {
 public:
  FlightBackend(std::string endpoint, int (*should_stop)(void *),
                std::unique_ptr<yesno::flight::Client> client)
      : endpoint_(std::move(endpoint)),
        should_stop_(should_stop),
        client_(std::move(client)),
        worker_(std::make_unique<Worker>()) {}

  BackendStatus Insert(std::uint64_t key, std::uint64_t ordinal, bool *changed,
              std::string *error) override {
    return Call<bool>(
        [key, ordinal](yesno::flight::Client &client, const std::atomic<bool> &) {
          return client.Insert(key, ordinal);
        },
        changed, error);
  }
  BackendStatus Remove(std::uint64_t key, std::uint64_t ordinal, bool *changed,
              std::string *error) override {
    return Call<bool>(
        [key, ordinal](yesno::flight::Client &client, const std::atomic<bool> &) {
          return client.Remove(key, ordinal);
        },
        changed, error);
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
  ///
  /// **A session that stopped waiting is not committed for.** It has already
  /// been told its statement was interrupted, so the call aborts rather than
  /// commits if the stop came before the commit was sent. A stop during the
  /// commit itself leaves the outcome unknown, as a lost connection would.
  BackendStatus Apply(const std::vector<KeyWrites> &writes,
             std::string *error) override {
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
      return BackendStatus::kOk;
    }

    std::uint64_t version = 0;
    return Call<std::uint64_t>(
        [mutations = std::move(mutations)](
            yesno::flight::Client &client,
            const std::atomic<bool> &abandoned) -> arrow::Result<std::uint64_t> {
          ARROW_ASSIGN_OR_RAISE(auto txn, client.BeginWrite());
          auto staged = client.Stage(txn, mutations);
          if (!staged.ok()) {
            // Best effort: release the server's staged memory rather than
            // waiting for the deadline. The staging failure is what the
            // caller must see.
            static_cast<void>(client.AbortWrite(txn));
            return staged.status();
          }
          if (abandoned.load()) {
            static_cast<void>(client.AbortWrite(txn));
            return arrow::Status::Cancelled(
                "the session stopped waiting before the commit");
          }
          auto committed = client.CommitWrite(txn);
          if (!committed.ok()) {
            static_cast<void>(client.AbortWrite(txn));
          }
          return committed;
        },
        &version, error);
  }

  BackendStatus Contains(std::uint64_t key, std::uint64_t ordinal, bool *present,
                std::string *error) override {
    return Call<bool>(
        [key, ordinal](yesno::flight::Client &client, const std::atomic<bool> &) {
          return client.Contains(key, ordinal);
        },
        present, error);
  }
  BackendStatus Cardinality(std::uint64_t key, std::uint64_t *cardinality,
                   std::string *error) override {
    return Call<std::uint64_t>(
        [key](yesno::flight::Client &client, const std::atomic<bool> &) {
          return client.Cardinality(key);
        },
        cardinality, error);
  }
  BackendStatus Clear(std::uint64_t key, std::string *error) override {
    std::uint64_t cleared = 0;
    return Call<std::uint64_t>(
        [key](yesno::flight::Client &client, const std::atomic<bool> &) {
          return client.Clear(key);
        },
        &cleared, error);
  }
  BackendStatus Checkpoint(std::string *error) override {
    // The remote server owns its checkpoint policy and deliberately exposes no
    // administrative checkpoint action on the public Flight surface. MySQL
    // shutdown only needs to release this client's connection.
    error->clear();
    return BackendStatus::kOk;
  }
  BackendStatus OpenCursor(std::uint64_t key, std::unique_ptr<Cursor> *cursor,
                  std::string *error) override {
    std::vector<std::uint64_t> ordinals;
    const BackendStatus status = Call<std::vector<std::uint64_t>>(
        [key](yesno::flight::Client &client, const std::atomic<bool> &) {
          return client.Get(key);
        },
        &ordinals, error);
    if (status == BackendStatus::kOk) {
      *cursor = std::make_unique<VectorCursor>(std::move(ordinals));
    }
    return status;
  }

 private:
  /// Runs `call` on the worker, holding the connection, and stores its value.
  ///
  /// `call` must own everything it touches: if the session stops waiting, the
  /// call outlives this frame, so it captures by value and its result lands in
  /// a slot the two share rather than in `output`.
  template <typename T, typename F>
  BackendStatus Call(F call, T *output, std::string *error) {
    std::unique_lock<std::timed_mutex> guard(mutex_, std::defer_lock);
    while (!guard.try_lock_for(kStopPoll)) {
      if (Stopped()) {
        *error = "yesno flight: interrupted while another query held the connection";
        return BackendStatus::kInterrupted;
      }
    }
    if (worker_ == nullptr) {
      // The last call here was abandoned. Its client may still be waiting on
      // its answer, and one Client must not be called concurrently.
      auto connected = yesno::flight::Client::Connect(endpoint_);
      if (!connected.ok()) {
        *error = connected.status().ToString();
        return BackendStatus::kError;
      }
      client_ = std::shared_ptr<yesno::flight::Client>(
          std::move(connected).ValueOrDie());
      worker_ = std::make_unique<Worker>();
    }
    auto result = std::make_shared<std::optional<arrow::Result<T>>>();
    const bool finished = worker_->Run(
        [client = client_, result, call = std::move(call)](
            const std::atomic<bool> &abandoned) {
          result->emplace(call(*client, abandoned));
        },
        should_stop_);
    if (!finished) {
      worker_.reset();
      client_.reset();
      *error = "yesno flight: interrupted while waiting for the server";
      return BackendStatus::kInterrupted;
    }
    arrow::Result<T> answer = std::move(**result);
    if (!answer.ok()) {
      // The Flight surface carries no equivalent of the channel's retryable or
      // stale statuses, so this stays unclassified rather than guessing one.
      *error = answer.status().ToString();
      return BackendStatus::kError;
    }
    *output = std::move(answer).ValueOrDie();
    error->clear();
    return BackendStatus::kOk;
  }

  bool Stopped() const {
    return should_stop_ != nullptr && should_stop_(nullptr) != 0;
  }

  const std::string endpoint_;
  int (*const should_stop_)(void *);
  /// Guards `client_` and `worker_`, which a killed wait replaces together.
  std::timed_mutex mutex_;
  std::shared_ptr<yesno::flight::Client> client_;
  /// Null after an abandoned call, until the next call connects afresh.
  std::unique_ptr<Worker> worker_;
};

}  // namespace

std::unique_ptr<Backend> OpenFlightBackend(const std::string &endpoint,
                                           int (*should_stop)(void *),
                                           std::string *error) {
  auto connected = yesno::flight::Client::Connect(endpoint);
  if (!connected.ok()) {
    *error = connected.status().ToString();
    return nullptr;
  }
  auto client = std::move(connected).ValueOrDie();
  // The plugin's init function runs before mysqld accepts a connection, so no
  // session exists to kill and an endpoint that accepts and never answers
  // would hold mysqld's startup for ever. The probe alone has a time limit;
  // later calls have none, and answer the waiting session's kill instead.
  client->SetCallTimeout(kOpenTimeout);
  auto probe = client->Cardinality(0);
  client->SetCallTimeout(std::chrono::milliseconds::zero());
  if (!probe.ok()) {
    const auto detail =
        arrow::flight::FlightStatusDetail::UnwrapStatus(probe.status());
    if (detail != nullptr &&
        detail->code() == arrow::flight::FlightStatusCode::TimedOut) {
      *error = "yesno Flight endpoint " + endpoint + " did not answer in time: " +
               probe.status().ToString();
    } else {
      *error = probe.status().ToString();
    }
    return nullptr;
  }
  error->clear();
  return std::make_unique<FlightBackend>(endpoint, should_stop, std::move(client));
}

}  // namespace yesno_mysql
