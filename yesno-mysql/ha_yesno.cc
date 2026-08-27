/* SPDX-License-Identifier: GPL-2.0-only WITH Universal-FOSS-exception-1.0 */

#include "storage/yesno/ha_yesno.h"

#include <charconv>
#include <cstring>
#include <map>
#include <set>
#include <string>
#include <string_view>
#include <system_error>
#include <utility>
#include <vector>

#include "my_byteorder.h"
#include "my_dbug.h"
#include "mysql/plugin.h"
#include "mysqld_error.h"
#include "nulls.h"
#include "sql/dd/properties.h"
#include "sql/dd/types/table.h"
#include "sql/field.h"
#include "sql/mysqld.h"
#include "sql/sql_class.h"
#include "sql/sql_plugin.h"
#include "typelib.h"

namespace {

constexpr std::string_view kConnectionPrefix = "key=";

handlerton *yesno_hton = nullptr;
char *yesno_data_dir = nullptr;
char *yesno_backend_mode = nullptr;
char *yesno_flight_endpoint = nullptr;
std::string resolved_data_dir;
std::unique_ptr<yesno_mysql::Backend> yesno_backend;

int bridge_error(const std::string &error) {
  my_printf_error(ER_UNKNOWN_ERROR, "YESNO: %s", MYF(0), error.c_str());
  return HA_ERR_INTERNAL_ERROR;
}

int command_error(const char *message) {
  my_printf_error(ER_UNKNOWN_ERROR, "YESNO: %s", MYF(0), message);
  return HA_ERR_WRONG_COMMAND;
}

bool parse_key(std::string_view connection, ulonglong *key) {
  if (!connection.starts_with(kConnectionPrefix)) return false;
  connection.remove_prefix(kConnectionPrefix.size());
  if (connection.empty()) return false;

  ulonglong parsed = 0;
  const char *first = connection.data();
  const char *last = first + connection.size();
  const auto result = std::from_chars(first, last, parsed, 10);
  if (result.ec != std::errc() || result.ptr != last) return false;
  *key = parsed;
  return true;
}

bool key_from_share(const TABLE_SHARE *share, ulonglong *key) {
  if (share == nullptr || share->connect_string.str == nullptr) return false;
  return parse_key(
      std::string_view(share->connect_string.str, share->connect_string.length),
      key);
}

bool key_from_create_info(const HA_CREATE_INFO *create_info, ulonglong *key) {
  if (create_info == nullptr || create_info->connect_string.str == nullptr)
    return false;
  return parse_key(std::string_view(create_info->connect_string.str,
                                    create_info->connect_string.length),
                   key);
}

bool key_from_table_def(const dd::Table *table_def, ulonglong *key) {
  if (table_def == nullptr || !table_def->options().exists("connection_string"))
    return false;
  dd::String_type connection;
  if (table_def->options().get("connection_string", &connection)) return false;
  return parse_key(connection, key);
}

int invalid_connection() {
  return command_error(
      "CONNECTION must be exactly 'key=<unsigned 64-bit decimal>'");
}

// ── Transaction buffering ────────────────────────────────────────────────────
//
// A MySQL transaction is not a yesno transaction: yesno commits per
// `WriteBatch`, MySQL commits when it says so, and neither can roll the other
// back. The answer is the one `yesno-pg`'s FDW already uses -- buffer the
// writes and apply them as one yesno batch when MySQL commits -- and it is what
// lets this engine stop advertising `HA_NO_TRANSACTIONS`.
//
// **Savepoints are handled in the same change, deliberately.** The FDW shipped
// its buffer without them, and for a day that was a silent wrong answer in both
// directions: writes after a rolled-back `SAVEPOINT` were still committed, and
// a `DELETE` rolled back the same way was still applied, losing a row the user
// had restored. The structure that avoids it is a **stack of levels** rather
// than one map, because the per-ordinal last-write-wins rule destroys exactly
// the parent verdict a rollback has to restore.

/// One nesting level's writes for one key.
struct LevelWrites {
  /// A `TRUNCATE` issued inside this level. The store is ignored from here up.
  bool cleared = false;
  /// ordinal -> was the last write at this level a removal?
  std::map<std::uint64_t, bool> ops;
};

/// One nesting level, across every key the transaction has touched.
struct TxnLevel {
  std::map<ulonglong, LevelWrites> by_key;
};

/// What one key's buffered writes settle to once every level is applied.
struct Settled {
  bool base_suppressed = false;
  std::set<std::uint64_t> inserts;
  std::set<std::uint64_t> removes;

  bool empty() const {
    return !base_suppressed && inserts.empty() && removes.empty();
  }
};

/// A transaction's buffered writes, one entry per open level.
///
/// Level 0 is the transaction. A statement pushes one so it can be rolled back
/// on its own, and so does `SAVEPOINT`; both unwind through the same two
/// operations.
class TxnBuffer {
 public:
  TxnBuffer() : levels_(1) {}

  void record(ulonglong key, std::uint64_t ordinal, bool remove) {
    levels_.back().by_key[key].ops[ordinal] = remove;
  }

  void record_clear(ulonglong key) {
    LevelWrites &w = levels_.back().by_key[key];
    w.cleared = true;
    // Emptying the key subsumes what *this* level had said about it. Shallower
    // levels keep theirs: those have to survive this level being rolled back.
    w.ops.clear();
  }

  /// Fold every level for one key, shallowest first so the deepest wins.
  Settled settled_for(ulonglong key) const {
    Settled out;
    for (const TxnLevel &level : levels_) {
      const auto it = level.by_key.find(key);
      if (it == level.by_key.end()) continue;
      if (it->second.cleared) {
        out.base_suppressed = true;
        out.inserts.clear();
        out.removes.clear();
      }
      for (const auto &entry : it->second.ops) {
        if (entry.second) {
          out.inserts.erase(entry.first);
          out.removes.insert(entry.first);
        } else {
          out.removes.erase(entry.first);
          out.inserts.insert(entry.first);
        }
      }
    }
    return out;
  }

  bool empty() const {
    for (const TxnLevel &level : levels_) {
      if (!level.by_key.empty()) return false;
    }
    return true;
  }

  /// Every touched key, settled and ready for `Backend::Apply`.
  std::vector<yesno_mysql::KeyWrites> flush_plan() const {
    std::set<ulonglong> keys;
    for (const TxnLevel &level : levels_) {
      for (const auto &entry : level.by_key) keys.insert(entry.first);
    }
    std::vector<yesno_mysql::KeyWrites> plan;
    for (const ulonglong key : keys) {
      const Settled s = settled_for(key);
      if (s.empty()) continue;
      yesno_mysql::KeyWrites w;
      w.key = key;
      w.clear_first = s.base_suppressed;
      w.inserts.assign(s.inserts.begin(), s.inserts.end());
      // A suppressed base has nothing left to remove from; the clear did it.
      if (!s.base_suppressed)
        w.removes.assign(s.removes.begin(), s.removes.end());
      plan.push_back(std::move(w));
    }
    return plan;
  }

  std::size_t depth() const { return levels_.size(); }

  void push_level() { levels_.emplace_back(); }

  /// Throw away every level deeper than `depth`, leaving the parent untouched.
  /// That is what restores a row a rolled-back level deleted.
  void discard_above(std::size_t depth) {
    if (depth < 1) depth = 1;
    while (levels_.size() > depth) levels_.pop_back();
  }

  /// Hand every level deeper than `depth` to its parent, deepest winning.
  void release_above(std::size_t depth) {
    if (depth < 1) depth = 1;
    while (levels_.size() > depth) {
      TxnLevel top = std::move(levels_.back());
      levels_.pop_back();
      TxnLevel &parent = levels_.back();
      for (auto &entry : top.by_key) {
        LevelWrites &into = parent.by_key[entry.first];
        if (entry.second.cleared) {
          into.cleared = true;
          into.ops.clear();
        }
        for (const auto &op : entry.second.ops) into.ops[op.first] = op.second;
      }
    }
  }

  void reset() {
    levels_.clear();
    levels_.emplace_back();
  }

 private:
  std::vector<TxnLevel> levels_;
};

/// Per-connection transaction state, owned through `thd_set_ha_data`.
struct TxnState {
  TxnBuffer buffer;
  /// Depth the current statement's level sits above, or 0 when no statement
  /// level is open.
  std::size_t stmt_depth = 0;
};

/// A cursor over the store **as this transaction sees it**.
///
/// Without this a transaction cannot read its own writes, which is worse than
/// not buffering at all: `INSERT` then `SELECT` inside one transaction would
/// return nothing. Every position is computed by merging two ordered sources --
/// the stored set with this transaction's removals skipped, and its insertions
/// -- so the merge has to be redone per step rather than cached, because the
/// engine may seek anywhere between calls.
class OverlayCursor final : public yesno_mysql::Cursor {
 public:
  OverlayCursor(std::unique_ptr<yesno_mysql::Cursor> base, Settled settled)
      : base_(std::move(base)), settled_(std::move(settled)) {}

  bool First(std::uint64_t *ordinal, bool *found, std::string *error) override {
    return forward(0, true, ordinal, found, error);
  }

  bool Next(std::uint64_t *ordinal, bool *found, std::string *error) override {
    if (!positioned_) return forward(0, true, ordinal, found, error);
    if (position_ == UINT64_MAX) {
      *found = false;
      return true;
    }
    return forward(position_, false, ordinal, found, error);
  }

  bool Last(std::uint64_t *ordinal, bool *found, std::string *error) override {
    return backward(UINT64_MAX, true, ordinal, found, error);
  }

  bool Prev(std::uint64_t *ordinal, bool *found, std::string *error) override {
    if (!positioned_) return backward(UINT64_MAX, true, ordinal, found, error);
    if (position_ == 0) {
      *found = false;
      return true;
    }
    return backward(position_, false, ordinal, found, error);
  }

  bool Seek(std::uint64_t target, yesno_mysql::SeekMode mode,
            std::uint64_t *ordinal, bool *found,
            std::string *error) override {
    switch (mode) {
      case yesno_mysql::SeekMode::kExact: {
        bool present = false;
        if (!contains(target, &present, error)) return false;
        *found = present;
        if (present) {
          *ordinal = target;
          position_ = target;
          positioned_ = true;
        }
        return true;
      }
      case yesno_mysql::SeekMode::kOrNext:
        return forward(target, true, ordinal, found, error);
      case yesno_mysql::SeekMode::kAfter:
        return forward(target, false, ordinal, found, error);
      case yesno_mysql::SeekMode::kOrPrev:
        return backward(target, true, ordinal, found, error);
      case yesno_mysql::SeekMode::kBefore:
        return backward(target, false, ordinal, found, error);
    }
    *found = false;
    return true;
  }

  /// Whether the transaction's view holds `ordinal`.
  bool contains(std::uint64_t ordinal, bool *present, std::string *error) {
    if (settled_.inserts.count(ordinal) != 0) {
      *present = true;
      return true;
    }
    if (settled_.removes.count(ordinal) != 0 || settled_.base_suppressed) {
      *present = false;
      return true;
    }
    std::uint64_t got = 0;
    return base_->Seek(ordinal, yesno_mysql::SeekMode::kExact, &got, present,
                       error);
  }

 private:
  /// The stored set's next value at or after `from`, with this transaction's
  /// removals skipped. Stepping past a removed value one at a time is why the
  /// loop is here rather than a single seek.
  bool base_forward(std::uint64_t from, bool inclusive, std::uint64_t *out,
                    bool *found, std::string *error) {
    if (settled_.base_suppressed) {
      *found = false;
      return true;
    }
    std::uint64_t cursor = from;
    bool inclusive_now = inclusive;
    for (;;) {
      std::uint64_t got = 0;
      bool got_one = false;
      const auto mode = inclusive_now ? yesno_mysql::SeekMode::kOrNext
                                      : yesno_mysql::SeekMode::kAfter;
      if (!base_->Seek(cursor, mode, &got, &got_one, error)) return false;
      if (!got_one) {
        *found = false;
        return true;
      }
      if (settled_.removes.count(got) == 0) {
        *out = got;
        *found = true;
        return true;
      }
      cursor = got;
      inclusive_now = false;
    }
  }

  bool base_backward(std::uint64_t from, bool inclusive, std::uint64_t *out,
                     bool *found, std::string *error) {
    if (settled_.base_suppressed) {
      *found = false;
      return true;
    }
    std::uint64_t cursor = from;
    bool inclusive_now = inclusive;
    for (;;) {
      std::uint64_t got = 0;
      bool got_one = false;
      const auto mode = inclusive_now ? yesno_mysql::SeekMode::kOrPrev
                                      : yesno_mysql::SeekMode::kBefore;
      if (!base_->Seek(cursor, mode, &got, &got_one, error)) return false;
      if (!got_one) {
        *found = false;
        return true;
      }
      if (settled_.removes.count(got) == 0) {
        *out = got;
        *found = true;
        return true;
      }
      if (got == 0) {
        *found = false;
        return true;
      }
      cursor = got;
      inclusive_now = false;
    }
  }

  bool forward(std::uint64_t from, bool inclusive, std::uint64_t *ordinal,
               bool *found, std::string *error) {
    std::uint64_t best = 0;
    bool have = false;
    if (!base_forward(from, inclusive, &best, &have, error)) return false;
    const auto it = inclusive ? settled_.inserts.lower_bound(from)
                              : settled_.inserts.upper_bound(from);
    if (it != settled_.inserts.end() && (!have || *it < best)) {
      best = *it;
      have = true;
    }
    *found = have;
    if (have) {
      *ordinal = best;
      position_ = best;
      positioned_ = true;
    }
    return true;
  }

  bool backward(std::uint64_t from, bool inclusive, std::uint64_t *ordinal,
                bool *found, std::string *error) {
    std::uint64_t best = 0;
    bool have = false;
    if (!base_backward(from, inclusive, &best, &have, error)) return false;
    const auto upper = inclusive ? settled_.inserts.upper_bound(from)
                                 : settled_.inserts.lower_bound(from);
    if (upper != settled_.inserts.begin()) {
      const std::uint64_t candidate = *std::prev(upper);
      if (!have || candidate > best) {
        best = candidate;
        have = true;
      }
    }
    *found = have;
    if (have) {
      *ordinal = best;
      position_ = best;
      positioned_ = true;
    }
    return true;
  }

  std::unique_ptr<yesno_mysql::Cursor> base_;
  Settled settled_;
  std::uint64_t position_ = 0;
  bool positioned_ = false;
};


int validate_schema(TABLE *form) {
  if (form == nullptr || form->s == nullptr || form->s->fields != 1)
    return command_error(
        "a YESNO table must have exactly one BIGINT UNSIGNED column");

  Field *field = form->field[0];
  if (field == nullptr || field->type() != MYSQL_TYPE_LONGLONG ||
      !field->is_flag_set(UNSIGNED_FLAG) || field->is_nullable())
    return command_error("the YESNO column must be BIGINT UNSIGNED NOT NULL");

  if (form->s->keys != 1 || form->s->primary_key != 0 ||
      form->key_info[0].user_defined_key_parts != 1 ||
      form->key_info[0].key_part[0].field != field ||
      !(form->key_info[0].flags & HA_NOSAME))
    return command_error(
        "the YESNO column must be the table's single PRIMARY KEY");

  return 0;
}

handler *yesno_create_handler(handlerton *hton, TABLE_SHARE *table,
                              bool /* partitioned */, MEM_ROOT *mem_root) {
  return new (mem_root) ha_yesno(hton, table);
}

/// The transaction state for this connection, created on first use.
TxnState *txn_state(THD *thd) {
  auto *state = static_cast<TxnState *>(thd_get_ha_data(thd, yesno_hton));
  if (state == nullptr) {
    state = new TxnState();
    thd_set_ha_data(thd, yesno_hton, state);
  }
  return state;
}

TxnState *txn_state_if_any(THD *thd) {
  return static_cast<TxnState *>(thd_get_ha_data(thd, yesno_hton));
}

/// A transaction's view of one key, or an empty one when it has written
/// nothing -- in which case every read goes straight to the store.
Settled settled_view(THD *thd, ulonglong key) {
  TxnState *state = txn_state_if_any(thd);
  if (state == nullptr) return Settled{};
  return state->buffer.settled_for(key);
}

/// Drop the connection's transaction state.
///
/// **Not just tidiness: `thd_set_ha_data` holds a reference to the plugin until
/// it is reset to null**, so state left behind after a transaction makes
/// `UNINSTALL PLUGIN` report the engine as busy and defer to shutdown. The
/// state is recreated on the next statement that needs it.
void forget_txn_state(THD *thd) {
  TxnState *state = txn_state_if_any(thd);
  if (state == nullptr) return;
  delete state;
  thd_set_ha_data(thd, yesno_hton, nullptr);
}

/// Whether this `commit`/`rollback` ends the whole transaction.
///
/// `all` is false at statement end inside an explicit transaction, and MySQL
/// also drives an autocommit statement through the same pair -- so the engine
/// has to ask whether a transaction is open rather than trust `all` alone.
bool ends_transaction(THD *thd, bool all) {
  return all || !thd_test_options(thd, OPTION_NOT_AUTOCOMMIT | OPTION_BEGIN);
}

/// Send everything the transaction buffered, as one batch.
int yesno_commit(handlerton *, THD *thd, bool all) {
  TxnState *state = txn_state_if_any(thd);
  if (state == nullptr) return 0;
  if (!ends_transaction(thd, all)) {
    // Statement end: its level belongs to the transaction now.
    state->buffer.release_above(state->stmt_depth);
    state->stmt_depth = 0;
    return 0;
  }
  state->buffer.release_above(state->stmt_depth);
  state->stmt_depth = 0;
  const std::vector<yesno_mysql::KeyWrites> plan = state->buffer.flush_plan();
  forget_txn_state(thd);
  if (plan.empty()) return 0;
  std::string error;
  if (!yesno_backend->Apply(plan, &error)) return bridge_error(error);
  return 0;
}

/// Discard, which is the whole point of buffering: nothing was ever sent.
int yesno_rollback(handlerton *, THD *thd, bool all) {
  TxnState *state = txn_state_if_any(thd);
  if (state == nullptr) return 0;
  if (!ends_transaction(thd, all)) {
    state->buffer.discard_above(state->stmt_depth);
    state->stmt_depth = 0;
    return 0;
  }
  forget_txn_state(thd);
  return 0;
}

/// MySQL hands the engine `savepoint_offset` bytes inside each savepoint, so
/// the level a savepoint names is recorded there rather than inferred from the
/// order events arrive in.
int yesno_savepoint_set(handlerton *, THD *thd, void *sv) {
  TxnState *state = txn_state(thd);
  auto *depth = static_cast<std::size_t *>(sv);
  *depth = state->buffer.depth();
  state->buffer.push_level();
  return 0;
}

/// Discard every level above the savepoint and reopen one: MySQL keeps the
/// savepoint usable afterwards, so writes must have somewhere to go.
int yesno_savepoint_rollback(handlerton *, THD *thd, void *sv) {
  TxnState *state = txn_state_if_any(thd);
  if (state == nullptr) return 0;
  const auto *depth = static_cast<const std::size_t *>(sv);
  state->buffer.discard_above(*depth);
  state->buffer.push_level();
  // The statement level was inside what was just discarded.
  if (state->stmt_depth > *depth) state->stmt_depth = 0;
  return 0;
}

/// `RELEASE SAVEPOINT`: its writes belong to the parent, deepest winning.
int yesno_savepoint_release(handlerton *, THD *thd, void *sv) {
  TxnState *state = txn_state_if_any(thd);
  if (state == nullptr) return 0;
  const auto *depth = static_cast<const std::size_t *>(sv);
  state->buffer.release_above(*depth);
  if (state->stmt_depth > *depth) state->stmt_depth = 0;
  return 0;
}

int yesno_close_connection(handlerton *, THD *thd) {
  forget_txn_state(thd);
  return 0;
}

bool yesno_is_supported_system_table(const char *, const char *, bool) {
  return false;
}

int yesno_init_func(void *plugin) {
  DBUG_TRACE;
  yesno_hton = static_cast<handlerton *>(plugin);
  yesno_hton->state = SHOW_OPTION_YES;
  yesno_hton->create = yesno_create_handler;
  yesno_hton->flags = HTON_NO_FLAGS;
  yesno_hton->is_supported_system_table = yesno_is_supported_system_table;
  // Writes are buffered per connection and applied as one yesno batch at
  // commit, so `ROLLBACK` really rolls back. There is deliberately **no
  // `prepare`**: a crash between this engine's commit and MySQL's binlog write
  // leaves the two disagreeing, which is the same gap `fdw-two-phase-commit`
  // records on the PostgreSQL side and needs the same durable prepare/resolve
  // protocol yesno does not expose.
  yesno_hton->commit = yesno_commit;
  yesno_hton->rollback = yesno_rollback;
  yesno_hton->savepoint_set = yesno_savepoint_set;
  yesno_hton->savepoint_rollback = yesno_savepoint_rollback;
  yesno_hton->savepoint_release = yesno_savepoint_release;
  yesno_hton->savepoint_offset = sizeof(std::size_t);
  yesno_hton->close_connection = yesno_close_connection;

  const std::string mode =
      yesno_backend_mode == nullptr ? "embedded" : yesno_backend_mode;
  std::string error;
  if (mode == "embedded") {
    resolved_data_dir = yesno_data_dir != nullptr && yesno_data_dir[0] != '\0'
                            ? yesno_data_dir
                            : std::string(mysql_real_data_home) + "yesno";
    yesno_backend =
        yesno_mysql::OpenEmbeddedBackend(resolved_data_dir, &error);
  } else if (mode == "flight") {
    if (yesno_flight_endpoint == nullptr || yesno_flight_endpoint[0] == '\0') {
      my_printf_error(
          ER_UNKNOWN_ERROR,
          "YESNO: yesno_flight_endpoint is required for backend=flight",
          MYF(0));
      return 1;
    }
    yesno_backend =
        yesno_mysql::OpenFlightBackend(yesno_flight_endpoint, &error);
  } else {
    my_printf_error(ER_UNKNOWN_ERROR,
                    "YESNO: backend must be 'embedded' or 'flight'", MYF(0));
    return 1;
  }
  if (yesno_backend == nullptr) {
    my_printf_error(ER_UNKNOWN_ERROR, "YESNO: cannot initialize %s backend: %s",
                    MYF(0), mode.c_str(), error.c_str());
    return 1;
  }
  return 0;
}

int yesno_deinit_func(void *) {
  DBUG_TRACE;
  std::string error;
  if (yesno_backend != nullptr && !yesno_backend->Checkpoint(&error)) {
    my_printf_error(ER_UNKNOWN_ERROR,
                    "YESNO: checkpoint during plugin shutdown failed: %s",
                    MYF(0), error.c_str());
  }
  yesno_backend.reset();
  resolved_data_dir.clear();
  return 0;
}

static MYSQL_SYSVAR_STR(
    data_dir, yesno_data_dir,
    PLUGIN_VAR_RQCMDARG | PLUGIN_VAR_READONLY | PLUGIN_VAR_MEMALLOC,
    "Directory holding the embedded yesno database. Defaults to "
    "<mysql-datadir>/yesno.",
    nullptr, nullptr, nullptr);

static MYSQL_SYSVAR_STR(
    backend, yesno_backend_mode,
    PLUGIN_VAR_RQCMDARG | PLUGIN_VAR_READONLY | PLUGIN_VAR_MEMALLOC,
    "Backend implementation: 'embedded' or 'flight'.", nullptr, nullptr,
    "embedded");

static MYSQL_SYSVAR_STR(
    flight_endpoint, yesno_flight_endpoint,
    PLUGIN_VAR_RQCMDARG | PLUGIN_VAR_READONLY | PLUGIN_VAR_MEMALLOC,
    "Arrow Flight URI used when backend=flight, for example "
    "'grpc://127.0.0.1:50051'.",
    nullptr, nullptr, nullptr);

static SYS_VAR *yesno_system_variables[] = {
    MYSQL_SYSVAR(data_dir), MYSQL_SYSVAR(backend),
    MYSQL_SYSVAR(flight_endpoint), nullptr};

struct st_mysql_storage_engine yesno_storage_engine = {
    MYSQL_HANDLERTON_INTERFACE_VERSION};

}  // namespace

Yesno_share::Yesno_share() { thr_lock_init(&lock); }

ha_yesno::ha_yesno(handlerton *hton, TABLE_SHARE *table_arg)
    : handler(hton, table_arg) {
  ref_length = sizeof(ulonglong);
}

ha_yesno::~ha_yesno() { reset_cursor(); }

Yesno_share *ha_yesno::get_share() {
  lock_shared_ha_data();
  auto *value = static_cast<Yesno_share *>(get_ha_share_ptr());
  if (value == nullptr) {
    value = new Yesno_share;
    if (value != nullptr) set_ha_share_ptr(static_cast<Handler_share *>(value));
  }
  unlock_shared_ha_data();
  return value;
}

int ha_yesno::reset_cursor() {
  cursor_.reset();
  return 0;
}

int ha_yesno::open(const char *, int, uint, const dd::Table *) {
  DBUG_TRACE;
  if (!key_from_share(table_share, &key_)) return invalid_connection();
  share_ = get_share();
  if (share_ == nullptr) return HA_ERR_OUT_OF_MEM;
  thr_lock_data_init(&share_->lock, &lock_, nullptr);
  return 0;
}

int ha_yesno::close() {
  DBUG_TRACE;
  return reset_cursor();
}

ulong ha_yesno::index_flags(uint index, uint part, bool /* all_parts */) const {
  if (index != 0 || part != 0) return 0;
  return HA_READ_NEXT | HA_READ_PREV | HA_READ_ORDER | HA_READ_RANGE |
         HA_ONLY_WHOLE_INDEX;
}

ulonglong ha_yesno::read_ordinal(const uchar *buf) const {
  Field *field = table->field[0];
  const ptrdiff_t offset = buf - table->record[0];
  field->move_field_offset(offset);
  const ulonglong ordinal = static_cast<ulonglong>(field->val_int());
  field->move_field_offset(-offset);
  return ordinal;
}

int ha_yesno::store_ordinal(uchar *buf, ulonglong ordinal) {
  std::memset(buf, 0, table->s->reclength);
  my_bitmap_map *old_map = dbug_tmp_use_all_columns(table, table->write_set);
  Field *field = table->field[0];
  const ptrdiff_t offset = buf - table->record[0];
  field->move_field_offset(offset);
  field->set_notnull();
  const type_conversion_status status =
      field->store(static_cast<longlong>(ordinal), true);
  field->move_field_offset(-offset);
  dbug_tmp_restore_column_map(table->write_set, old_map);
  return status == TYPE_OK ? 0 : HA_ERR_INTERNAL_ERROR;
}

/// Buffer an insert, answering the duplicate-key question from the
/// transaction's own view.
///
/// **The verdict is the reason this needs a probe.** MySQL asks whether row N
/// collided before it offers row N+1, and it reads that from this return value.
/// Before buffering, the answer came free from `Insert`'s `changed` flag; a
/// deferred write cannot report it, so the row's presence is looked up instead
/// -- in the store *and* in this transaction's buffered writes, or a row the
/// same transaction just inserted would not collide with itself.
int ha_yesno::write_row(uchar *buf) {
  DBUG_TRACE;
  if (table->field[0]->is_null())
    return command_error("the ordinal column must not be NULL");
  const ulonglong ordinal = read_ordinal(buf);
  bool present = false;
  if (const int result = view_contains(ordinal, &present); result != 0)
    return result;
  if (present) {
    errkey = 0;
    return HA_ERR_FOUND_DUPP_KEY;
  }
  txn_state(ha_thd())->buffer.record(key_, ordinal, false);
  return 0;
}

int ha_yesno::update_row(const uchar *, uchar *) {
  DBUG_TRACE;
  return command_error(
      "UPDATE is not supported; the ordinal is the row identity, so use "
      "DELETE followed by INSERT");
}

int ha_yesno::delete_row(const uchar *buf) {
  DBUG_TRACE;
  const ulonglong ordinal = read_ordinal(buf);
  bool present = false;
  if (const int result = view_contains(ordinal, &present); result != 0)
    return result;
  if (!present) return HA_ERR_KEY_NOT_FOUND;
  txn_state(ha_thd())->buffer.record(key_, ordinal, true);
  return 0;
}

/// Whether this transaction's view holds `ordinal`, buffered writes included.
int ha_yesno::view_contains(ulonglong ordinal, bool *present) {
  const Settled view = settled_view(ha_thd(), key_);
  if (view.inserts.count(ordinal) != 0) {
    *present = true;
    return 0;
  }
  if (view.removes.count(ordinal) != 0 || view.base_suppressed) {
    *present = false;
    return 0;
  }
  std::string error;
  if (!yesno_backend->Contains(key_, ordinal, present, &error))
    return bridge_error(error);
  return 0;
}

int ha_yesno::cursor_read(uchar *buf, int operation, ulonglong target) {
  std::string error;
  if (cursor_ == nullptr && !open_cursor(&error)) return bridge_error(error);

  uint64_t ordinal = 0;
  bool found = false;
  bool result = false;
  switch (operation) {
    case -2:
      result = cursor_->First(&ordinal, &found, &error);
      break;
    case -1:
      result = cursor_->Prev(&ordinal, &found, &error);
      break;
    case 0:
      result = cursor_->Next(&ordinal, &found, &error);
      break;
    case 1:
      result = cursor_->Last(&ordinal, &found, &error);
      break;
    default:
      result = cursor_->Seek(
          target, static_cast<yesno_mysql::SeekMode>(operation - 2), &ordinal,
          &found, &error);
      break;
  }
  if (!result) return bridge_error(error);
  if (!found) return HA_ERR_END_OF_FILE;
  return store_ordinal(buf, ordinal);
}

int ha_yesno::index_read_map(uchar *buf, const uchar *key,
                             key_part_map keypart_map,
                             enum ha_rkey_function find_flag) {
  DBUG_TRACE;
  if (keypart_map != HA_WHOLE_KEY && keypart_map != 1)
    return HA_ERR_WRONG_COMMAND;
  reset_cursor();
  const ulonglong target = uint8korr(key);
  int mode = 0;
  switch (find_flag) {
    case HA_READ_KEY_EXACT:
    case HA_READ_PREFIX:
      mode = static_cast<int>(yesno_mysql::SeekMode::kExact);
      break;
    case HA_READ_KEY_OR_NEXT:
      mode = static_cast<int>(yesno_mysql::SeekMode::kOrNext);
      break;
    case HA_READ_AFTER_KEY:
      mode = static_cast<int>(yesno_mysql::SeekMode::kAfter);
      break;
    case HA_READ_KEY_OR_PREV:
    case HA_READ_PREFIX_LAST_OR_PREV:
      mode = static_cast<int>(yesno_mysql::SeekMode::kOrPrev);
      break;
    case HA_READ_BEFORE_KEY:
      mode = static_cast<int>(yesno_mysql::SeekMode::kBefore);
      break;
    case HA_READ_PREFIX_LAST:
      mode = static_cast<int>(yesno_mysql::SeekMode::kExact);
      break;
    default:
      return HA_ERR_WRONG_COMMAND;
  }
  const int result = cursor_read(buf, mode + 2, target);
  return result == HA_ERR_END_OF_FILE ? HA_ERR_KEY_NOT_FOUND : result;
}

int ha_yesno::index_next(uchar *buf) { return cursor_read(buf, 0); }

int ha_yesno::index_prev(uchar *buf) { return cursor_read(buf, -1); }

int ha_yesno::index_first(uchar *buf) {
  reset_cursor();
  return cursor_read(buf, -2);
}

int ha_yesno::index_last(uchar *buf) {
  reset_cursor();
  return cursor_read(buf, 1);
}

int ha_yesno::rnd_init(bool scan) {
  DBUG_TRACE;
  reset_cursor();
  if (!scan) return 0;
  std::string error;
  if (!open_cursor(&error)) return bridge_error(error);
  return 0;
}

/// Open a cursor over the store, wrapped so it shows this transaction's own
/// writes. The view is snapshotted at open: a scan that changed shape halfway
/// through would break the engine's cursor contract, and MySQL runs one
/// statement at a time per connection anyway.
bool ha_yesno::open_cursor(std::string *error) {
  std::unique_ptr<yesno_mysql::Cursor> base;
  if (!yesno_backend->OpenCursor(key_, &base, error)) return false;
  Settled view = settled_view(ha_thd(), key_);
  if (view.empty()) {
    cursor_ = std::move(base);
    return true;
  }
  cursor_ = std::make_unique<OverlayCursor>(std::move(base), std::move(view));
  return true;
}

int ha_yesno::rnd_end() {
  DBUG_TRACE;
  return reset_cursor();
}

int ha_yesno::rnd_next(uchar *buf) { return cursor_read(buf, 0); }

int ha_yesno::rnd_pos(uchar *buf, uchar *pos) {
  const ulonglong ordinal = uint8korr(pos);
  bool present = false;
  if (const int result = view_contains(ordinal, &present); result != 0)
    return result;
  return present ? store_ordinal(buf, ordinal) : HA_ERR_KEY_NOT_FOUND;
}

void ha_yesno::position(const uchar *record) {
  int8store(ref, read_ordinal(record));
}

int ha_yesno::info(uint flag) {
  if ((flag & HA_STATUS_ERRKEY) != 0) errkey = 0;
  ha_rows count = 0;
  const int result = records(&count);
  if (result != 0) return result;
  stats.records = count;
  stats.deleted = 0;
  stats.mean_rec_length = sizeof(ulonglong);
  stats.data_file_length = count * sizeof(ulonglong);
  stats.index_file_length = 0;
  return 0;
}

/// The row count this transaction sees.
///
/// The stored cardinality is exact and free; the buffered writes are not, since
/// an insert of a value the store already holds changes nothing and a delete of
/// one it does not hold changes nothing either. Each buffered ordinal therefore
/// costs one `Contains` probe -- bounded by what the transaction has written,
/// not by the corpus, and zero for the overwhelmingly common read-only case.
int ha_yesno::records(ha_rows *num_rows) {
  std::string error;
  uint64_t count = 0;
  const Settled view = settled_view(ha_thd(), key_);
  if (view.base_suppressed) {
    count = 0;
  } else if (!yesno_backend->Cardinality(key_, &count, &error)) {
    return bridge_error(error);
  }
  for (const std::uint64_t ordinal : view.inserts) {
    bool present = false;
    if (!view.base_suppressed &&
        !yesno_backend->Contains(key_, ordinal, &present, &error))
      return bridge_error(error);
    if (!present) ++count;
  }
  if (!view.base_suppressed) {
    for (const std::uint64_t ordinal : view.removes) {
      bool present = false;
      if (!yesno_backend->Contains(key_, ordinal, &present, &error))
        return bridge_error(error);
      if (present) --count;
    }
  }
  *num_rows = count;
  return 0;
}

int ha_yesno::extra(enum ha_extra_function) { return 0; }

/// Join the transaction at statement start, and open a level for the statement.
///
/// **Registration happens here rather than at the first write**, because MySQL
/// only calls `savepoint_set` on engines already in the transaction: an engine
/// that joins later gets a full rollback instead of a savepoint rollback, which
/// is safe but throws away work the user did not roll back.
int ha_yesno::external_lock(THD *thd, int lock_type) {
  DBUG_TRACE;
  if (lock_type == F_UNLCK) return 0;
  TxnState *state = txn_state(thd);
  if (state->stmt_depth == 0) {
    state->stmt_depth = state->buffer.depth();
    state->buffer.push_level();
  }
  trans_register_ha(thd, false, yesno_hton, nullptr);
  if (thd_test_options(thd, OPTION_NOT_AUTOCOMMIT | OPTION_BEGIN))
    trans_register_ha(thd, true, yesno_hton, nullptr);
  return 0;
}



/// `TRUNCATE` is buffered like any other write, so it rolls back.
///
/// MySQL commits implicitly around `TRUNCATE TABLE`, so this ordinarily flushes
/// immediately anyway; buffering it matters for the `DELETE FROM t` spelling
/// that reaches the same handler inside an open transaction.
int ha_yesno::delete_all_rows() {
  txn_state(ha_thd())->buffer.record_clear(key_);
  return 0;
}

int ha_yesno::truncate(dd::Table *) { return delete_all_rows(); }

ha_rows ha_yesno::records_in_range(uint index, key_range *min_key,
                                   key_range *max_key) {
  if (index != 0) return HA_POS_ERROR;
  if (min_key != nullptr && max_key == nullptr &&
      min_key->flag == HA_READ_KEY_EXACT) {
    std::string error;
    bool present = false;
    const ulonglong ordinal = uint8korr(min_key->key);
    if (!yesno_backend->Contains(key_, ordinal, &present, &error))
      return HA_POS_ERROR;
    return present;
  }
  ha_rows count = 0;
  return records(&count) == 0 ? count : HA_POS_ERROR;
}

int ha_yesno::delete_table(const char *, const dd::Table *table_def) {
  ulonglong key = 0;
  if (!key_from_table_def(table_def, &key)) return invalid_connection();
  std::string error;
  return yesno_backend->Clear(key, &error) ? 0 : bridge_error(error);
}

int ha_yesno::rename_table(const char *, const char *, const dd::Table *,
                           dd::Table *) {
  // Storage identity comes from CONNECTION='key=...', not the SQL name.
  return 0;
}

int ha_yesno::create(const char *, TABLE *form, HA_CREATE_INFO *create_info,
                     dd::Table *) {
  if (const int result = validate_schema(form); result != 0) return result;
  ulonglong key = 0;
  return key_from_create_info(create_info, &key) ? 0 : invalid_connection();
}

THR_LOCK_DATA **ha_yesno::store_lock(THD *, THR_LOCK_DATA **to,
                                     enum thr_lock_type lock_type) {
  if (lock_type != TL_IGNORE && lock_.type == TL_UNLOCK) lock_.type = lock_type;
  *to++ = &lock_;
  return to;
}

mysql_declare_plugin(yesno){
    MYSQL_STORAGE_ENGINE_PLUGIN,
    &yesno_storage_engine,
    "YESNO",
    "yesnodb contributors",
    "yesno-backed single-column unsigned ordinal-set storage engine",
    PLUGIN_LICENSE_GPL,
    yesno_init_func,
    nullptr,
    yesno_deinit_func,
    0x0001,
    nullptr,
    yesno_system_variables,
    nullptr,
    0,
} mysql_declare_plugin_end;
