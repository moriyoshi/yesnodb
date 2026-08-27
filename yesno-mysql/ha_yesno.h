/* SPDX-License-Identifier: GPL-2.0-only WITH Universal-FOSS-exception-1.0 */
#ifndef YESNO_MYSQL_HA_YESNO_H
#define YESNO_MYSQL_HA_YESNO_H

#include <sys/types.h>

#include <memory>
#include <string>

#include "my_base.h"
#include "my_compiler.h"
#include "my_inttypes.h"
#include "sql/handler.h"
#include "storage/yesno/backend.h"
#include "thr_lock.h"

class Yesno_share : public Handler_share {
 public:
  THR_LOCK lock;

  Yesno_share();
  ~Yesno_share() override { thr_lock_delete(&lock); }
};

class ha_yesno final : public handler {
  THR_LOCK_DATA lock_{};
  Yesno_share *share_{nullptr};
  std::unique_ptr<yesno_mysql::Cursor> cursor_;
  ulonglong key_{0};

  Yesno_share *get_share();
  int reset_cursor();
  int store_ordinal(uchar *buf, ulonglong ordinal);
  ulonglong read_ordinal(const uchar *buf) const;
  int cursor_read(uchar *buf, int operation, ulonglong target = 0);
  /// Open a store cursor, overlaid with this transaction's buffered writes.
  bool open_cursor(std::string *error);
  /// Whether this transaction's view holds `ordinal`.
  int view_contains(ulonglong ordinal, bool *present);

 public:
  ha_yesno(handlerton *hton, TABLE_SHARE *table_arg);
  ~ha_yesno() override;

  const char *table_type() const override { return "YESNO"; }
  enum ha_key_alg get_default_index_algorithm() const override {
    return HA_KEY_ALG_BTREE;
  }
  bool is_index_algorithm_supported(enum ha_key_alg algorithm) const override {
    return algorithm == HA_KEY_ALG_BTREE;
  }

  enum row_type get_real_row_type(const HA_CREATE_INFO *) const override {
    return ROW_TYPE_FIXED;
  }

  ulonglong table_flags() const override {
    return HA_FAST_KEY_READ | HA_KEYREAD_ONLY | HA_NO_BLOBS |
           HA_REQUIRE_PRIMARY_KEY | HA_STATS_RECORDS_IS_EXACT |
           HA_PRIMARY_KEY_IN_READ_INDEX | HA_PRIMARY_KEY_REQUIRED_FOR_POSITION |
           HA_PRIMARY_KEY_REQUIRED_FOR_DELETE | HA_NO_AUTO_INCREMENT |
           HA_COUNT_ROWS_INSTANT | HA_BINLOG_ROW_CAPABLE |
           HA_BINLOG_STMT_CAPABLE;
  }

  ulong index_flags(uint index, uint part, bool all_parts) const override;
  uint max_supported_record_length() const override {
    return HA_MAX_REC_LENGTH;
  }
  uint max_supported_keys() const override { return 1; }
  uint max_supported_key_parts() const override { return 1; }
  uint max_supported_key_length() const override { return 8; }
  double scan_time() override {
    return static_cast<double>(stats.records) / 20.0 + 1.0;
  }
  double read_time(uint, uint, ha_rows rows) override {
    return static_cast<double>(rows) / 20.0 + 1.0;
  }

  int open(const char *name, int mode, uint test_if_locked,
           const dd::Table *table_def) override;
  int close() override;
  int write_row(uchar *buf) override;
  int update_row(const uchar *old_data, uchar *new_data) override;
  int delete_row(const uchar *buf) override;

  int index_read_map(uchar *buf, const uchar *key, key_part_map keypart_map,
                     enum ha_rkey_function find_flag) override;
  int index_next(uchar *buf) override;
  int index_prev(uchar *buf) override;
  int index_first(uchar *buf) override;
  int index_last(uchar *buf) override;

  int rnd_init(bool scan) override;
  int rnd_end() override;
  int rnd_next(uchar *buf) override;
  int rnd_pos(uchar *buf, uchar *pos) override;
  void position(const uchar *record) override;

  int info(uint flag) override;
  int records(ha_rows *num_rows) override;
  int extra(enum ha_extra_function operation) override;
  int external_lock(THD *thd, int lock_type) override;
  int delete_all_rows() override;
  int truncate(dd::Table *table_def) override;
  ha_rows records_in_range(uint index, key_range *min_key,
                           key_range *max_key) override;
  int delete_table(const char *from, const dd::Table *table_def) override;
  int rename_table(const char *from, const char *to,
                   const dd::Table *from_table_def,
                   dd::Table *to_table_def) override;
  int create(const char *name, TABLE *form, HA_CREATE_INFO *create_info,
             dd::Table *table_def) override;

  THR_LOCK_DATA **store_lock(THD *thd, THR_LOCK_DATA **to,
                             enum thr_lock_type lock_type) override;
};

#endif
