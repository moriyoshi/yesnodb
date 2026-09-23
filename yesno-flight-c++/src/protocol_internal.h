// SPDX-License-Identifier: MIT OR Apache-2.0
#ifndef YESNO_FLIGHT_PROTOCOL_INTERNAL_H
#define YESNO_FLIGHT_PROTOCOL_INTERNAL_H

#include <cstddef>
#include <cstdint>
#include <memory>
#include <string>
#include <string_view>
#include <vector>

#include <arrow/record_batch.h>
#include <arrow/result.h>
#include <arrow/status.h>
#include <arrow/type_fwd.h>

namespace yesno::flight::internal {

std::string EncodeU64(std::uint64_t value);
std::string EncodePair(std::uint64_t key, std::uint64_t ordinal);
arrow::Result<std::uint64_t> DecodeU64(std::string_view bytes,
                                       std::string_view operation);
arrow::Result<bool> DecodeBoolean(std::uint64_t value,
                                  std::string_view operation);

// The ingest acknowledgement, which is *not* a bare u64 and must not be decoded
// with DecodeU64. It carried 8 bytes -- the row count alone -- until 2026-09-12,
// when the commit version was appended, so both widths are accepted and a
// version of 0 means the server reported none. DecodeU64 stays strict because
// the action results it decodes really are 8 bytes, and relaxing it would
// weaken those.
//
// NOTE: the version is decoded but deliberately NOT exposed on the public
// Client, unlike the Rust, Python, Go and Java clients. Those expose it because
// each has a version-bound query to pass it to; this client's read surface is
// PrepareKey / Get / Cardinality only, with no expression form and no
// at-a-version form, so a commit version here would have no caller. Expose it
// when that query arrives, not before.
struct IngestAck {
  std::uint64_t rows = 0;
  std::uint64_t version = 0;  // 0 means not reported; no commit is ever 0.
};
arrow::Result<IngestAck> DecodeIngestAck(std::string_view bytes);

std::shared_ptr<arrow::Schema> PairsSchema();

/// `( key, lo, hi, op )`, one row per staged `WriteBatch` operation.
///
/// Ranges travel as ranges: the engine writes one record and one container
/// call per chunk for one, and expanding client-side throws both away.
std::shared_ptr<arrow::Schema> MutationsSchema();

// Validate one server batch and append it without partially modifying output
// on a validation failure. Ordering is checked across batch boundaries too.
arrow::Status ValidateAndAppendOrdinals(
    const arrow::RecordBatch& batch,
    std::vector<std::uint64_t>* output);

arrow::Status ValidateOrdinalCount(std::size_t received,
                                   std::uint64_t promised);

}  // namespace yesno::flight::internal

#endif
