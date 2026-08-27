// SPDX-License-Identifier: MIT OR Apache-2.0

#include "protocol_internal.h"

#include <cstddef>
#include <cstdint>
#include <iostream>
#include <memory>
#include <string>
#include <string_view>
#include <utility>
#include <vector>

#include <arrow/api.h>
#include <arrow/result.h>
#include <arrow/status.h>

namespace {

int failures = 0;

void Check(bool condition, std::string_view message) {
  if (!condition) {
    std::cerr << "FAIL: " << message << '\n';
    ++failures;
  }
}

void CheckInvalid(const arrow::Status& status, std::string_view fragment,
                  std::string_view context) {
  Check(status.IsInvalid(), std::string(context) + " was not invalid");
  Check(status.message().find(fragment) != std::string::npos,
        std::string(context) + " did not identify the failure");
}

std::shared_ptr<arrow::Array> U64Array(
    const std::vector<std::uint64_t>& values) {
  arrow::UInt64Builder builder;
  for (const std::uint64_t value : values) {
    Check(builder.Append(value).ok(), "could not append unit-test UInt64");
  }
  std::shared_ptr<arrow::Array> array;
  Check(builder.Finish(&array).ok(), "could not finish unit-test UInt64 array");
  return array;
}

std::shared_ptr<arrow::Array> NullableU64Array() {
  arrow::UInt64Builder builder;
  Check(builder.Append(1).ok(), "could not append unit-test value");
  Check(builder.AppendNull().ok(), "could not append unit-test null");
  std::shared_ptr<arrow::Array> array;
  Check(builder.Finish(&array).ok(), "could not finish nullable test array");
  return array;
}

std::shared_ptr<arrow::Array> I64Array() {
  arrow::Int64Builder builder;
  Check(builder.Append(1).ok(), "could not append unit-test Int64");
  std::shared_ptr<arrow::Array> array;
  Check(builder.Finish(&array).ok(), "could not finish unit-test Int64 array");
  return array;
}

std::shared_ptr<arrow::RecordBatch> Batch(
    std::string name, bool nullable, std::shared_ptr<arrow::Array> values) {
  auto schema = arrow::schema(
      {arrow::field(std::move(name), values->type(), nullable)});
  return arrow::RecordBatch::Make(schema, values->length(),
                                  {std::move(values)});
}

void EncodingIsFixedWidthLittleEndian() {
  const std::string encoded =
      yesno::flight::internal::EncodeU64(0x8877665544332211ULL);
  Check(encoded.size() == 8, "u64 encoding is not fixed width");
  for (std::size_t i = 0; i < encoded.size(); ++i) {
    Check(static_cast<unsigned char>(encoded[i]) == 0x11 + i * 0x11,
          "u64 encoding is not little endian");
  }

  const std::string pair = yesno::flight::internal::EncodePair(
      0x8877665544332211ULL, 0x1020304050607080ULL);
  Check(pair.size() == 16, "pair encoding is not fixed width");
  Check(pair.substr(0, 8) == encoded, "pair key framing changed");
  const auto decoded =
      yesno::flight::internal::DecodeU64(pair.substr(8), "pair");
  Check(decoded.ok() && *decoded == 0x1020304050607080ULL,
        "pair ordinal did not round trip");
}

void IngestAcknowledgementsAcceptBothWidths() {
  // The 8-byte case is a server predating the version field: the version is
  // lost, the call is not.
  const auto old_server = yesno::flight::internal::DecodeIngestAck(
      yesno::flight::internal::EncodeU64(5000));
  Check(old_server.ok() && old_server->rows == 5000 &&
            old_server->version == 0,
        "an 8-byte acknowledgement must decode with no version");

  const auto current = yesno::flight::internal::DecodeIngestAck(
      yesno::flight::internal::EncodeU64(5000) +
      yesno::flight::internal::EncodeU64(42));
  Check(current.ok() && current->rows == 5000 && current->version == 42,
        "a 16-byte acknowledgement must report rows and version");

  // No commit is ever assigned version 0, so a zero means the server committed
  // nothing and must not be handed back as a readable version.
  const auto empty = yesno::flight::internal::DecodeIngestAck(
      yesno::flight::internal::EncodeU64(0) +
      yesno::flight::internal::EncodeU64(0));
  Check(empty.ok() && empty->rows == 0 && empty->version == 0,
        "a zero version must not be reported as a version");

  // Any other width is refused rather than truncated -- including 24, so a
  // future widening is detected by an older client.
  for (const std::size_t width : {std::size_t{0}, std::size_t{7},
                                  std::size_t{9}, std::size_t{15},
                                  std::size_t{24}}) {
    const auto bad =
        yesno::flight::internal::DecodeIngestAck(std::string(width, '\0'));
    CheckInvalid(bad.status(), "expected 8 or 16",
                 "unexpected acknowledgement width");
  }
}

void ScalarResponsesAreStrict() {
  const auto zero = yesno::flight::internal::DecodeU64(
      std::string(8, '\0'), "clear");
  Check(zero.ok() && *zero == 0, "zero response did not decode");

  const auto short_response =
      yesno::flight::internal::DecodeU64(std::string(7, '\0'), "clear");
  CheckInvalid(short_response.status(), "7 bytes instead of 8",
               "short scalar response");
  const auto long_response =
      yesno::flight::internal::DecodeU64(std::string(9, '\0'), "clear");
  CheckInvalid(long_response.status(), "9 bytes instead of 8",
               "long scalar response");

  const auto false_value =
      yesno::flight::internal::DecodeBoolean(0, "contains");
  const auto true_value =
      yesno::flight::internal::DecodeBoolean(1, "contains");
  const auto invalid_value =
      yesno::flight::internal::DecodeBoolean(2, "contains");
  Check(false_value.ok() && !*false_value, "zero did not decode as false");
  Check(true_value.ok() && *true_value, "one did not decode as true");
  CheckInvalid(invalid_value.status(), "instead of zero or one",
               "non-boolean action response");
}

void PairSchemaIsExact() {
  const auto schema = yesno::flight::internal::PairsSchema();
  Check(schema->num_fields() == 2, "pair schema field count changed");
  Check(schema->field(0)->name() == "key" &&
            schema->field(0)->type()->id() == arrow::Type::UINT64 &&
            !schema->field(0)->nullable(),
        "pair key field changed");
  Check(schema->field(1)->name() == "ordinal" &&
            schema->field(1)->type()->id() == arrow::Type::UINT64 &&
            !schema->field(1)->nullable(),
        "pair ordinal field changed");
}

void OrdinalBatchesAreValidatedAtomically() {
  std::vector<std::uint64_t> output;
  auto first = Batch("ordinal", false, U64Array({1, 3, 5}));
  Check(yesno::flight::internal::ValidateAndAppendOrdinals(*first, &output)
            .ok(),
        "valid ordinal batch was rejected");
  auto second = Batch("ordinal", false, U64Array({8, 13}));
  Check(yesno::flight::internal::ValidateAndAppendOrdinals(*second, &output)
            .ok(),
        "ascending second batch was rejected");
  Check(output == std::vector<std::uint64_t>({1, 3, 5, 8, 13}),
        "valid ordinal batches appended incorrectly");

  auto duplicate = Batch("ordinal", false, U64Array({13, 21}));
  const auto before = output;
  CheckInvalid(
      yesno::flight::internal::ValidateAndAppendOrdinals(*duplicate, &output),
      "strict ascending", "cross-batch duplicate");
  Check(output == before, "rejected batch partially modified output");

  auto descending = Batch("ordinal", false, U64Array({34, 33}));
  CheckInvalid(
      yesno::flight::internal::ValidateAndAppendOrdinals(*descending, &output),
      "strict ascending", "descending ordinal batch");
  Check(output == before, "descending batch partially modified output");
}

void MalformedOrdinalSchemasAreRejected() {
  std::vector<std::uint64_t> output;
  auto wrong_name = Batch("value", false, U64Array({1}));
  CheckInvalid(yesno::flight::internal::ValidateAndAppendOrdinals(
                   *wrong_name, &output),
               "exactly one non-null UInt64 ordinal", "wrong field name");

  auto nullable = Batch("ordinal", true, U64Array({1}));
  CheckInvalid(yesno::flight::internal::ValidateAndAppendOrdinals(
                   *nullable, &output),
               "exactly one non-null UInt64 ordinal", "nullable field");

  auto wrong_type = Batch("ordinal", false, I64Array());
  CheckInvalid(yesno::flight::internal::ValidateAndAppendOrdinals(
                   *wrong_type, &output),
               "exactly one non-null UInt64 ordinal", "wrong field type");

  auto null_value = Batch("ordinal", false, NullableU64Array());
  CheckInvalid(yesno::flight::internal::ValidateAndAppendOrdinals(
                   *null_value, &output),
               "nulls in its non-null ordinal", "null ordinal");

  auto two_columns = arrow::RecordBatch::Make(
      arrow::schema({arrow::field("ordinal", arrow::uint64(), false),
                     arrow::field("extra", arrow::uint64(), false)}),
      1, {U64Array({1}), U64Array({2})});
  CheckInvalid(yesno::flight::internal::ValidateAndAppendOrdinals(
                   *two_columns, &output),
               "exactly one non-null UInt64 ordinal", "extra field");
  Check(output.empty(), "malformed schema modified output");
}

void PromisedCardinalityIsExact() {
  Check(yesno::flight::internal::ValidateOrdinalCount(3, 3).ok(),
        "matching cardinality was rejected");
  CheckInvalid(yesno::flight::internal::ValidateOrdinalCount(2, 3),
               "2 ordinals after promising 3", "short result");
  CheckInvalid(yesno::flight::internal::ValidateOrdinalCount(4, 3),
               "4 ordinals after promising 3", "long result");
}

}  // namespace

int main() {
  EncodingIsFixedWidthLittleEndian();
  ScalarResponsesAreStrict();
  IngestAcknowledgementsAcceptBothWidths();
  PairSchemaIsExact();
  OrdinalBatchesAreValidatedAtomically();
  MalformedOrdinalSchemasAreRejected();
  PromisedCardinalityIsExact();
  if (failures != 0) {
    std::cerr << failures << " unit assertion(s) failed\n";
    return 1;
  }
  return 0;
}
