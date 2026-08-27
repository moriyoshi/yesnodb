// SPDX-License-Identifier: MIT OR Apache-2.0

#include "protocol_internal.h"

#include <cstddef>
#include <cstdint>
#include <limits>
#include <memory>
#include <string>
#include <string_view>
#include <vector>

#include <arrow/api.h>
#include <arrow/status.h>

namespace yesno::flight::internal {

std::string EncodeU64(std::uint64_t value) {
  std::string bytes(8, '\0');
  for (std::size_t i = 0; i < 8; ++i) {
    bytes[i] = static_cast<char>((value >> (i * 8)) & 0xff);
  }
  return bytes;
}

std::string EncodePair(std::uint64_t key, std::uint64_t ordinal) {
  std::string bytes = EncodeU64(key);
  bytes.append(EncodeU64(ordinal));
  return bytes;
}

arrow::Result<std::uint64_t> DecodeU64(std::string_view bytes,
                                       std::string_view operation) {
  if (bytes.size() != 8) {
    return arrow::Status::Invalid("yesno ", operation, " returned ",
                                  bytes.size(), " bytes instead of 8");
  }
  std::uint64_t value = 0;
  for (std::size_t i = 0; i < 8; ++i) {
    value |= static_cast<std::uint64_t>(
                 static_cast<unsigned char>(bytes[i]))
             << (i * 8);
  }
  return value;
}

arrow::Result<IngestAck> DecodeIngestAck(std::string_view bytes) {
  if (bytes.size() != 8 && bytes.size() != 16) {
    return arrow::Status::Invalid(
        "yesno ingest acknowledgement returned ", bytes.size(),
        " bytes, expected 8 or 16");
  }
  IngestAck ack;
  ARROW_ASSIGN_OR_RAISE(ack.rows,
                        DecodeU64(bytes.substr(0, 8), "ingest acknowledgement"));
  if (bytes.size() == 16) {
    ARROW_ASSIGN_OR_RAISE(
        ack.version, DecodeU64(bytes.substr(8, 8), "ingest acknowledgement"));
  }
  return ack;
}

arrow::Result<bool> DecodeBoolean(std::uint64_t value,
                                  std::string_view operation) {
  if (value > 1) {
    return arrow::Status::Invalid("yesno ", operation, " returned ", value,
                                  " instead of zero or one");
  }
  return value != 0;
}

std::shared_ptr<arrow::Schema> PairsSchema() {
  return arrow::schema({
      arrow::field("key", arrow::uint64(), false),
      arrow::field("ordinal", arrow::uint64(), false),
  });
}

arrow::Status ValidateAndAppendOrdinals(
    const arrow::RecordBatch& batch,
    std::vector<std::uint64_t>* output) {
  ARROW_RETURN_NOT_OK(batch.ValidateFull());
  if (batch.num_columns() != 1 ||
      batch.schema()->field(0)->name() != "ordinal" ||
      batch.schema()->field(0)->nullable() ||
      batch.column(0)->type_id() != arrow::Type::UINT64) {
    return arrow::Status::Invalid(
        "yesno returned a batch without exactly one non-null UInt64 ordinal column");
  }
  if (batch.column(0)->null_count() != 0) {
    return arrow::Status::Invalid(
        "yesno returned nulls in its non-null ordinal column");
  }

  const auto values =
      std::static_pointer_cast<arrow::UInt64Array>(batch.column(0));
  bool has_previous = !output->empty();
  std::uint64_t previous = has_previous ? output->back() : 0;
  for (std::int64_t i = 0; i < values->length(); ++i) {
    const std::uint64_t value = values->Value(i);
    if (has_previous && value <= previous) {
      return arrow::Status::Invalid(
          "yesno returned ordinals outside strict ascending set order");
    }
    previous = value;
    has_previous = true;
  }
  for (std::int64_t i = 0; i < values->length(); ++i) {
    output->push_back(values->Value(i));
  }
  return arrow::Status::OK();
}

arrow::Status ValidateOrdinalCount(std::size_t received,
                                   std::uint64_t promised) {
  if (promised >
          static_cast<std::uint64_t>(
              std::numeric_limits<std::size_t>::max()) ||
      received != static_cast<std::size_t>(promised)) {
    return arrow::Status::Invalid("yesno returned ", received,
                                  " ordinals after promising ", promised);
  }
  return arrow::Status::OK();
}

}  // namespace yesno::flight::internal
