// SPDX-License-Identifier: MIT OR Apache-2.0

#include "yesno/flight/client.h"

#include "protocol_internal.h"

#include <algorithm>
#include <cstddef>
#include <limits>
#include <memory>
#include <string>
#include <string_view>
#include <utility>
#include <vector>

#include <arrow/api.h>
#include <arrow/flight/api.h>
#include <arrow/status.h>

namespace yesno::flight {
namespace {

constexpr std::size_t kBatchRows = 8192;

}  // namespace

struct Client::Impl {
  explicit Impl(std::unique_ptr<arrow::flight::FlightClient> value)
      : flight(std::move(value)) {}

  std::unique_ptr<arrow::flight::FlightClient> flight;
};

arrow::Result<std::unique_ptr<Client>> Client::Connect(std::string_view uri) {
  ARROW_ASSIGN_OR_RAISE(
      auto location,
      arrow::flight::Location::Parse(std::string(uri)));
  ARROW_ASSIGN_OR_RAISE(auto flight,
                        arrow::flight::FlightClient::Connect(location));
  return std::unique_ptr<Client>(
      new Client(std::make_unique<Impl>(std::move(flight))));
}

Client::Client(std::unique_ptr<Impl> impl) : impl_(std::move(impl)) {}
Client::~Client() = default;
Client::Client(Client&&) noexcept = default;
Client& Client::operator=(Client&&) noexcept = default;

arrow::Result<QueryInfo> Client::PrepareKey(std::uint64_t key) {
  ARROW_ASSIGN_OR_RAISE(
      auto info,
      impl_->flight->GetFlightInfo(
          arrow::flight::FlightDescriptor::Command(internal::EncodeU64(key))));
  if (info->total_records() < 0) {
    return arrow::Status::Invalid(
        "yesno returned a negative total_records value");
  }
  if (info->endpoints().size() != 1) {
    return arrow::Status::Invalid("yesno returned ", info->endpoints().size(),
                                  " endpoints instead of 1");
  }
  return QueryInfo{
      static_cast<std::uint64_t>(info->total_records()),
      info->endpoints().front().ticket.ticket,
  };
}

arrow::Result<std::vector<std::uint64_t>> Client::Fetch(
    const QueryInfo& query) {
  ARROW_ASSIGN_OR_RAISE(
      auto stream,
      impl_->flight->DoGet(arrow::flight::Ticket(query.ticket)));

  if (query.total_records >
      static_cast<std::uint64_t>(std::numeric_limits<std::size_t>::max())) {
    return arrow::Status::CapacityError(
        "yesno result cannot fit in this process address space");
  }
  std::vector<std::uint64_t> ordinals;
  ordinals.reserve(static_cast<std::size_t>(query.total_records));

  while (true) {
    ARROW_ASSIGN_OR_RAISE(arrow::flight::FlightStreamChunk chunk,
                          stream->Next());
    if (chunk.data == nullptr) break;
    ARROW_RETURN_NOT_OK(
        internal::ValidateAndAppendOrdinals(*chunk.data, &ordinals));
  }

  ARROW_RETURN_NOT_OK(
      internal::ValidateOrdinalCount(ordinals.size(), query.total_records));
  return ordinals;
}

arrow::Result<std::vector<std::uint64_t>> Client::Get(std::uint64_t key) {
  ARROW_ASSIGN_OR_RAISE(auto query, PrepareKey(key));
  return Fetch(query);
}

arrow::Result<std::uint64_t> Client::Cardinality(std::uint64_t key) {
  ARROW_ASSIGN_OR_RAISE(auto query, PrepareKey(key));
  return query.total_records;
}

arrow::Result<std::uint64_t> Client::U64Action(std::string_view name,
                                               std::string body) {
  ARROW_ASSIGN_OR_RAISE(
      auto results,
      impl_->flight->DoAction(arrow::flight::Action(
          std::string(name), arrow::Buffer::FromString(std::move(body)))));
  std::string response;
  while (true) {
    ARROW_ASSIGN_OR_RAISE(auto result, results->Next());
    if (result == nullptr) break;
    if (result->body != nullptr) {
      response.append(reinterpret_cast<const char*>(result->body->data()),
                      static_cast<std::size_t>(result->body->size()));
    }
  }
  return internal::DecodeU64(response, name);
}

arrow::Result<bool> Client::BoolPairAction(std::string_view name,
                                           std::uint64_t key,
                                           std::uint64_t ordinal) {
  ARROW_ASSIGN_OR_RAISE(
      auto value,
      U64Action(name, internal::EncodePair(key, ordinal)));
  return internal::DecodeBoolean(value, name);
}

arrow::Result<bool> Client::Insert(std::uint64_t key,
                                   std::uint64_t ordinal) {
  return BoolPairAction("insert_one", key, ordinal);
}

arrow::Result<bool> Client::Remove(std::uint64_t key,
                                   std::uint64_t ordinal) {
  return BoolPairAction("remove_one", key, ordinal);
}

arrow::Result<bool> Client::Contains(std::uint64_t key,
                                     std::uint64_t ordinal) {
  return BoolPairAction("contains", key, ordinal);
}

arrow::Result<std::uint64_t> Client::Clear(std::uint64_t key) {
  return U64Action("clear", internal::EncodeU64(key));
}

arrow::Result<std::uint64_t> Client::InsertMany(
    const std::vector<std::pair<std::uint64_t, std::uint64_t>>& pairs) {
  return Put("insert", pairs);
}

arrow::Result<std::uint64_t> Client::RemoveMany(
    const std::vector<std::pair<std::uint64_t, std::uint64_t>>& pairs) {
  return Put("remove", pairs);
}

arrow::Result<Client::WriteTxn> Client::BeginWrite() {
  return U64Action("begin_write", std::string());
}

arrow::Result<std::uint64_t> Client::Stage(
    WriteTxn txn, const std::vector<Mutation>& mutations) {
  std::string command = "txn:";
  command.append(internal::EncodeU64(txn));
  return PutMutations(command, mutations);
}

arrow::Result<std::uint64_t> Client::CommitWrite(WriteTxn txn) {
  return U64Action("commit_write", internal::EncodeU64(txn));
}

arrow::Status Client::AbortWrite(WriteTxn txn) {
  return U64Action("abort_write", internal::EncodeU64(txn)).status();
}

arrow::Result<std::uint64_t> Client::Apply(
    const std::vector<Mutation>& mutations) {
  return PutMutations("apply", mutations);
}

arrow::Result<std::uint64_t> Client::PutMutations(
    std::string_view command, const std::vector<Mutation>& mutations) {
  auto schema = internal::MutationsSchema();
  ARROW_ASSIGN_OR_RAISE(
      auto put,
      impl_->flight->DoPut(
          arrow::flight::FlightDescriptor::Command(std::string(command)),
          schema));

  for (std::size_t offset = 0; offset < mutations.size();
       offset += kBatchRows) {
    const std::size_t count = std::min(kBatchRows, mutations.size() - offset);
    arrow::UInt64Builder keys;
    arrow::UInt64Builder los;
    arrow::UInt64Builder his;
    arrow::UInt8Builder ops;
    ARROW_RETURN_NOT_OK(keys.Reserve(static_cast<std::int64_t>(count)));
    ARROW_RETURN_NOT_OK(los.Reserve(static_cast<std::int64_t>(count)));
    ARROW_RETURN_NOT_OK(his.Reserve(static_cast<std::int64_t>(count)));
    ARROW_RETURN_NOT_OK(ops.Reserve(static_cast<std::int64_t>(count)));
    for (std::size_t i = 0; i < count; ++i) {
      const Mutation& m = mutations[offset + i];
      ARROW_RETURN_NOT_OK(keys.Append(m.key));
      ARROW_RETURN_NOT_OK(los.Append(m.lo));
      ARROW_RETURN_NOT_OK(his.Append(m.hi));
      ARROW_RETURN_NOT_OK(ops.Append(m.op));
    }
    std::shared_ptr<arrow::Array> key_array;
    std::shared_ptr<arrow::Array> lo_array;
    std::shared_ptr<arrow::Array> hi_array;
    std::shared_ptr<arrow::Array> op_array;
    ARROW_RETURN_NOT_OK(keys.Finish(&key_array));
    ARROW_RETURN_NOT_OK(los.Finish(&lo_array));
    ARROW_RETURN_NOT_OK(his.Finish(&hi_array));
    ARROW_RETURN_NOT_OK(ops.Finish(&op_array));
    auto batch = arrow::RecordBatch::Make(
        schema, static_cast<std::int64_t>(count),
        {std::move(key_array), std::move(lo_array), std::move(hi_array),
         std::move(op_array)});
    ARROW_RETURN_NOT_OK(put.writer->WriteRecordBatch(*batch));
  }
  ARROW_RETURN_NOT_OK(put.writer->DoneWriting());

  std::shared_ptr<arrow::Buffer> acknowledgement;
  ARROW_RETURN_NOT_OK(put.reader->ReadMetadata(&acknowledgement));
  if (acknowledgement == nullptr) {
    return arrow::Status::Invalid("yesno returned no ingest acknowledgement");
  }
  const std::string_view bytes(
      reinterpret_cast<const char*>(acknowledgement->data()),
      static_cast<std::size_t>(acknowledgement->size()));
  ARROW_ASSIGN_OR_RAISE(auto ack, internal::DecodeIngestAck(bytes));
  const std::uint64_t acknowledged = ack.rows;

  std::shared_ptr<arrow::Buffer> extra;
  ARROW_RETURN_NOT_OK(put.reader->ReadMetadata(&extra));
  if (extra != nullptr) {
    return arrow::Status::Invalid(
        "yesno returned more than one ingest acknowledgement");
  }
  ARROW_RETURN_NOT_OK(put.writer->Close());
  if (acknowledged != mutations.size()) {
    return arrow::Status::Invalid("yesno acknowledged ", acknowledged, " of ",
                                  mutations.size(), " staged mutations");
  }
  return acknowledged;
}

arrow::Result<std::uint64_t> Client::Put(
    std::string_view command,
    const std::vector<std::pair<std::uint64_t, std::uint64_t>>& pairs) {
  auto schema = internal::PairsSchema();
  ARROW_ASSIGN_OR_RAISE(
      auto put,
      impl_->flight->DoPut(
          arrow::flight::FlightDescriptor::Command(std::string(command)),
          schema));

  for (std::size_t offset = 0; offset < pairs.size();
       offset += kBatchRows) {
    const std::size_t count =
        std::min(kBatchRows, pairs.size() - offset);
    arrow::UInt64Builder keys;
    arrow::UInt64Builder ordinals;
    ARROW_RETURN_NOT_OK(keys.Reserve(static_cast<std::int64_t>(count)));
    ARROW_RETURN_NOT_OK(ordinals.Reserve(static_cast<std::int64_t>(count)));
    for (std::size_t i = 0; i < count; ++i) {
      ARROW_RETURN_NOT_OK(keys.Append(pairs[offset + i].first));
      ARROW_RETURN_NOT_OK(ordinals.Append(pairs[offset + i].second));
    }
    std::shared_ptr<arrow::Array> key_array;
    std::shared_ptr<arrow::Array> ordinal_array;
    ARROW_RETURN_NOT_OK(keys.Finish(&key_array));
    ARROW_RETURN_NOT_OK(ordinals.Finish(&ordinal_array));
    auto batch = arrow::RecordBatch::Make(
        schema, static_cast<std::int64_t>(count),
        {std::move(key_array), std::move(ordinal_array)});
    ARROW_RETURN_NOT_OK(put.writer->WriteRecordBatch(*batch));
  }
  ARROW_RETURN_NOT_OK(put.writer->DoneWriting());

  std::shared_ptr<arrow::Buffer> acknowledgement;
  ARROW_RETURN_NOT_OK(put.reader->ReadMetadata(&acknowledgement));
  if (acknowledgement == nullptr) {
    return arrow::Status::Invalid(
        "yesno returned no ingest acknowledgement");
  }
  const std::string_view bytes(
      reinterpret_cast<const char*>(acknowledgement->data()),
      static_cast<std::size_t>(acknowledgement->size()));
  ARROW_ASSIGN_OR_RAISE(auto ack, internal::DecodeIngestAck(bytes));
  const std::uint64_t acknowledged = ack.rows;

  std::shared_ptr<arrow::Buffer> extra;
  ARROW_RETURN_NOT_OK(put.reader->ReadMetadata(&extra));
  if (extra != nullptr) {
    return arrow::Status::Invalid(
        "yesno returned more than one ingest acknowledgement");
  }
  ARROW_RETURN_NOT_OK(put.writer->Close());
  if (acknowledged != pairs.size()) {
    return arrow::Status::Invalid("yesno acknowledged ", acknowledged,
                                  " of ", pairs.size(), " ingest pairs");
  }
  return acknowledged;
}

}  // namespace yesno::flight
