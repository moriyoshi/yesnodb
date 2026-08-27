/* SPDX-License-Identifier: GPL-2.0-only WITH Universal-FOSS-exception-1.0 */

#include "storage/yesno/ha_yesno.h"

#include <charconv>
#include <cstring>
#include <string>
#include <string_view>
#include <system_error>

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

int ha_yesno::write_row(uchar *buf) {
  DBUG_TRACE;
  if (table->field[0]->is_null())
    return command_error("the ordinal column must not be NULL");
  const ulonglong ordinal = read_ordinal(buf);
  std::string error;
  bool changed = false;
  if (!yesno_backend->Insert(key_, ordinal, &changed, &error))
    return bridge_error(error);
  if (!changed) {
    errkey = 0;
    return HA_ERR_FOUND_DUPP_KEY;
  }
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
  std::string error;
  bool changed = false;
  if (!yesno_backend->Remove(key_, ordinal, &changed, &error))
    return bridge_error(error);
  return changed ? 0 : HA_ERR_KEY_NOT_FOUND;
}

int ha_yesno::cursor_read(uchar *buf, int operation, ulonglong target) {
  std::string error;
  if (cursor_ == nullptr &&
      !yesno_backend->OpenCursor(key_, &cursor_, &error))
    return bridge_error(error);

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
  if (!yesno_backend->OpenCursor(key_, &cursor_, &error))
    return bridge_error(error);
  return 0;
}

int ha_yesno::rnd_end() {
  DBUG_TRACE;
  return reset_cursor();
}

int ha_yesno::rnd_next(uchar *buf) { return cursor_read(buf, 0); }

int ha_yesno::rnd_pos(uchar *buf, uchar *pos) {
  const ulonglong ordinal = uint8korr(pos);
  std::string error;
  bool present = false;
  if (!yesno_backend->Contains(key_, ordinal, &present, &error))
    return bridge_error(error);
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

int ha_yesno::records(ha_rows *num_rows) {
  std::string error;
  uint64_t count = 0;
  if (!yesno_backend->Cardinality(key_, &count, &error))
    return bridge_error(error);
  *num_rows = count;
  return 0;
}

int ha_yesno::extra(enum ha_extra_function) { return 0; }

int ha_yesno::external_lock(THD *, int) { return 0; }

int ha_yesno::delete_all_rows() {
  std::string error;
  return yesno_backend->Clear(key_, &error) ? 0 : bridge_error(error);
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
