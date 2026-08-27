"""A typed yesnodb client over :mod:`pyarrow.flight`."""

from __future__ import annotations

import os
import struct
from collections.abc import Callable, Iterable, Iterator
from dataclasses import dataclass
from pathlib import Path
from typing import Any, TypeAlias

import pyarrow as pa
import pyarrow.compute as pc
import pyarrow.flight as flight

from .errors import ProtocolError
from .expression import ORDINAL_LIMIT, QueryRequest, SetExpr, _u64
from .ticket import Ticket

BATCH_ROWS = 8192
ORDINAL_SCHEMA = pa.schema([pa.field("ordinal", pa.uint64(), nullable=False)])
PAIR_SCHEMA = pa.schema(
    [
        pa.field("key", pa.uint64(), nullable=False),
        pa.field("ordinal", pa.uint64(), nullable=False),
    ]
)

TokenProvider: TypeAlias = str | Callable[[], str]
PemSource: TypeAlias = bytes | bytearray | memoryview | str | os.PathLike[str]


def _read_pem(source: PemSource | None, *, name: str) -> bytes | None:
    if source is None:
        return None
    if isinstance(source, (bytes, bytearray, memoryview)):
        return bytes(source)
    try:
        return Path(source).read_bytes()
    except OSError as error:
        raise ValueError(f"cannot read {name} from {source!s}: {error}") from error


@dataclass(frozen=True, slots=True)
class TLSConfig:
    """TLS trust and optional mutual-authentication material."""

    root_certificates: bytes | None = None
    client_certificate: bytes | None = None
    client_key: bytes | None = None
    server_name: str | None = None
    disable_server_verification: bool = False

    def __post_init__(self) -> None:
        if (self.client_certificate is None) != (self.client_key is None):
            raise ValueError("client_certificate and client_key must be supplied together")
        if self.server_name is not None and not self.server_name:
            raise ValueError("server_name must not be empty")

    @classmethod
    def from_files(
        cls,
        *,
        root_certificates: PemSource | None = None,
        client_certificate: PemSource | None = None,
        client_key: PemSource | None = None,
        server_name: str | None = None,
        disable_server_verification: bool = False,
    ) -> TLSConfig:
        """Read PEM material immediately and return an immutable configuration."""

        return cls(
            root_certificates=_read_pem(root_certificates, name="root certificates"),
            client_certificate=_read_pem(client_certificate, name="client certificate"),
            client_key=_read_pem(client_key, name="client key"),
            server_name=server_name,
            disable_server_verification=disable_server_verification,
        )

    def connection_arguments(self) -> dict[str, Any]:
        arguments: dict[str, Any] = {}
        if self.root_certificates is not None:
            arguments["tls_root_certs"] = self.root_certificates
        if self.client_certificate is not None:
            arguments["cert_chain"] = self.client_certificate
            arguments["private_key"] = self.client_key
        if self.server_name is not None:
            arguments["override_hostname"] = self.server_name
        if self.disable_server_verification:
            arguments["disable_server_verification"] = True
        return arguments


def _protobuf_varint(body: bytes, offset: int) -> tuple[int, int]:
    value = 0
    for shift in range(0, 70, 7):
        if offset >= len(body):
            raise ProtocolError("yesnodb stats returned truncated protobuf")
        byte = body[offset]
        offset += 1
        if shift == 63 and byte > 1:
            raise ProtocolError("yesnodb stats protobuf contains an overflowing varint")
        value |= (byte & 0x7F) << shift
        if byte < 0x80:
            return value, offset
    raise ProtocolError("yesnodb stats protobuf contains an overflowing varint")


@dataclass(frozen=True, slots=True)
class ServerStats:
    """Space and reader counters returned by the ``stats`` action."""

    allocated_bytes: int
    deferred_bytes: int
    wal_bytes: int
    live_readers: int
    shards: int

    @classmethod
    def from_protobuf(cls, body: bytes) -> ServerStats:
        values = [0, 0, 0, 0, 0]
        offset = 0
        while offset < len(body):
            tag, offset = _protobuf_varint(body, offset)
            field_number = tag >> 3
            wire_type = tag & 7
            if field_number == 0:
                raise ProtocolError("yesnodb stats protobuf contains field number zero")

            if 1 <= field_number <= len(values):
                if wire_type != 0:
                    raise ProtocolError(
                        f"yesnodb stats protobuf field {field_number} is not a uint64"
                    )
                values[field_number - 1], offset = _protobuf_varint(body, offset)
                continue

            if wire_type == 0:
                _, offset = _protobuf_varint(body, offset)
            elif wire_type == 1:
                offset += 8
            elif wire_type == 2:
                size, offset = _protobuf_varint(body, offset)
                offset += size
            elif wire_type == 5:
                offset += 4
            else:
                raise ProtocolError(
                    f"yesnodb stats protobuf contains unsupported wire type {wire_type}"
                )
            if offset > len(body):
                raise ProtocolError("yesnodb stats returned truncated protobuf")

        return cls(*values)


@dataclass(frozen=True, slots=True)
class Ack:
    """What the server acknowledged for one ingest call.

    The version is optional, and the reason is wire compatibility: ``DoPut``'s
    ``app_metadata`` carried **8 bytes** -- the row count alone -- until
    2026-09-12, and a server that predates the change still sends 8.  Decoding
    accepts both widths and reports ``None`` for the short form, so a new client
    against an old server loses the version rather than the call.

    A version of ``0`` never occurs: version 0 is the empty database and no
    commit is assigned it, so a server that committed nothing also reports
    ``None``.
    """

    rows: int
    """Pairs the server accepted and committed."""

    version: int | None
    """The database version at which those rows are present, when reported."""

    @classmethod
    def _decode(cls, raw: bytes) -> Ack:
        if len(raw) not in (8, 16):
            raise ProtocolError(
                f"yesnodb ingest acknowledgement has {len(raw)} bytes, expected 8 or 16"
            )
        rows = struct.unpack_from("<Q", raw, 0)[0]
        version: int | None = None
        if len(raw) == 16:
            reported = struct.unpack_from("<Q", raw, 8)[0]
            version = reported or None
        return cls(rows=rows, version=version)


@dataclass(frozen=True, slots=True)
class QueryInfo:
    """Exact query metadata and the versioned ticket that produced it."""

    total_records: int
    ticket: Ticket
    ticket_bytes: bytes
    flight_info: flight.FlightInfo

    @property
    def version(self) -> int:
        return self.ticket.version


class QueryStream(Iterator[pa.RecordBatch]):
    """A query's metadata and incrementally decoded Arrow batches."""

    def __init__(self, info: QueryInfo, reader: flight.FlightStreamReader) -> None:
        self.info = info
        self.reader = reader
        self._received = 0
        self._finished = False

    def __iter__(self) -> QueryStream:
        return self

    def __next__(self) -> pa.RecordBatch:
        if self._finished:
            raise StopIteration
        try:
            chunk = self.reader.read_chunk()
        except StopIteration:
            self._finished = True
            if self._received != self.info.total_records:
                raise ProtocolError(
                    f"yesnodb returned {self._received} ordinals after promising "
                    f"{self.info.total_records}"
                ) from None
            raise
        batch = chunk.data
        if batch is None:
            raise ProtocolError("yesnodb returned a metadata-only chunk in an ordinal stream")
        _check_ordinal_batch(batch)
        self._received += batch.num_rows
        return batch

    def read_table(self) -> pa.Table:
        """Materialize all remaining record batches as one Arrow table."""

        return pa.Table.from_batches(list(self), schema=ORDINAL_SCHEMA)

    def collect_ordinals(self) -> list[int]:
        """Materialize every remaining ordinal as Python integers."""

        ordinals: list[int] = []
        for batch in self:
            ordinals.extend(batch.column(0).to_pylist())
        return ordinals

    def cancel(self) -> None:
        """Cancel the underlying Flight read."""

        self.reader.cancel()


class Client:
    """A yesnodb-shaped convenience layer over a PyArrow Flight client.

    The API is synchronous because PyArrow's Flight bindings are synchronous.
    Applications with an async event loop should execute calls in a worker
    thread rather than assuming the C++ client cooperates with that loop.
    """

    def __init__(
        self,
        endpoint: str | flight.Location,
        *,
        token: TokenProvider | None = None,
        minimum_term: int | None = None,
        timeout: float | None = None,
        tls: TLSConfig | None = None,
        headers: Iterable[tuple[str, str]] = (),
        write_size_limit_bytes: int | None = None,
    ) -> None:
        if minimum_term is not None:
            if isinstance(minimum_term, bool) or not isinstance(minimum_term, int):
                raise TypeError("minimum_term must be an integer")
            if not 0 <= minimum_term <= (1 << 32) - 1:
                raise ValueError("minimum_term must fit in u32")
        if timeout is not None and timeout <= 0:
            raise ValueError("timeout must be positive")
        self._token = token
        self._minimum_term = minimum_term
        self._timeout = timeout
        self._headers = tuple(headers)
        for name, value in self._headers:
            _check_header(name, value)

        location = _flight_location(endpoint)
        connection_arguments = tls.connection_arguments() if tls is not None else {}
        if write_size_limit_bytes is not None:
            if write_size_limit_bytes <= 0:
                raise ValueError("write_size_limit_bytes must be positive")
            connection_arguments["write_size_limit_bytes"] = write_size_limit_bytes
        self._flight = flight.connect(location, **connection_arguments)

    @classmethod
    def from_flight_client(
        cls,
        client: flight.FlightClient,
        *,
        token: TokenProvider | None = None,
        minimum_term: int | None = None,
        timeout: float | None = None,
        headers: Iterable[tuple[str, str]] = (),
    ) -> Client:
        """Wrap an already configured Flight client."""

        instance = cls.__new__(cls)
        instance._flight = client
        instance._token = token
        instance._minimum_term = minimum_term
        instance._timeout = timeout
        instance._headers = tuple(headers)
        return instance

    @property
    def flight_client(self) -> flight.FlightClient:
        """The underlying PyArrow client for non-yesnodb Flight operations."""

        return self._flight

    def __enter__(self) -> Client:
        return self

    def __exit__(self, *_error: object) -> None:
        self.close()

    def close(self) -> None:
        self._flight.close()

    def keys(self) -> list[int]:
        """Return every populated key in ascending order."""

        keys: list[int] = []
        for info in self._flight.list_flights(b"", self._call_options()):
            descriptor = info.descriptor
            command = bytes(descriptor.command)
            if len(command) != 8:
                raise ProtocolError(f"yesnodb key descriptor has {len(command)} bytes instead of 8")
            keys.append(struct.unpack("<Q", command)[0])
        return keys

    def prepare_key(self, key: int) -> QueryInfo:
        key = _u64(key, name="key")
        return self.prepare_command(struct.pack("<Q", key))

    def prepare_query(self, expression: SetExpr) -> QueryInfo:
        if not isinstance(expression, SetExpr):
            raise TypeError("expression must be a SetExpr")
        return self.prepare_command(expression.encode())

    def prepare_query_at(self, expression: SetExpr, version: int) -> QueryInfo:
        """Plan an expression at exactly ``version``, failing if it is unavailable."""

        return self.prepare_command(QueryRequest.at(expression, version).encode())

    def prepare_command(self, command: bytes | bytearray | memoryview) -> QueryInfo:
        descriptor = flight.FlightDescriptor.for_command(bytes(command))
        info = self._flight.get_flight_info(descriptor, self._call_options())
        return _query_info(info)

    def cardinality(self, key: int) -> int:
        return self.prepare_key(key).total_records

    def query_cardinality(self, expression: SetExpr) -> int:
        return self.prepare_query(expression).total_records

    def fetch(self, query: QueryInfo) -> flight.FlightStreamReader:
        if not isinstance(query, QueryInfo):
            raise TypeError("query must be a QueryInfo")
        return self.fetch_ticket(query.ticket_bytes)

    def fetch_ticket(self, ticket: bytes | bytearray | memoryview) -> flight.FlightStreamReader:
        return self._flight.do_get(flight.Ticket(bytes(ticket)), self._call_options())

    def get(self, key: int) -> QueryStream:
        info = self.prepare_key(key)
        return QueryStream(info, self.fetch(info))

    def query(self, expression: SetExpr) -> QueryStream:
        info = self.prepare_query(expression)
        return QueryStream(info, self.fetch(info))

    def insert(self, pairs: Iterable[tuple[int, int]]) -> int:
        return self._put_pairs(pairs, b"insert", one_batch=False)

    def remove(self, pairs: Iterable[tuple[int, int]]) -> int:
        return self._put_pairs(pairs, b"remove", one_batch=False)

    def insert_batch(self, pairs: Iterable[tuple[int, int]]) -> int:
        """Insert all pairs in one Arrow batch and one server-side commit."""

        return self._put_pairs(pairs, b"insert", one_batch=True)

    def remove_batch(self, pairs: Iterable[tuple[int, int]]) -> int:
        """Remove all pairs in one Arrow batch and one server-side commit."""

        return self._put_pairs(pairs, b"remove", one_batch=True)

    def insert_acked(self, pairs: Iterable[tuple[int, int]]) -> Ack:
        """:meth:`insert`, reporting the commit version alongside the count."""

        return self._put_pairs_acked(pairs, b"insert", one_batch=False)

    def remove_acked(self, pairs: Iterable[tuple[int, int]]) -> Ack:
        """:meth:`remove`, reporting the commit version alongside the count."""

        return self._put_pairs_acked(pairs, b"remove", one_batch=False)

    def insert_batch_acked(self, pairs: Iterable[tuple[int, int]]) -> Ack:
        """:meth:`insert_batch`, reporting the commit version alongside the count.

        This is the read-your-writes primitive: pass :attr:`Ack.version` to
        :meth:`prepare_query_at` and the read is bound to a database state that
        contains the write.  One batch is one commit, so the version names
        exactly this call's write -- unlike the streaming forms, which commit per
        batch and report the last version.
        """

        return self._put_pairs_acked(pairs, b"insert", one_batch=True)

    def remove_batch_acked(self, pairs: Iterable[tuple[int, int]]) -> Ack:
        """:meth:`remove_batch`, reporting the commit version alongside the count."""

        return self._put_pairs_acked(pairs, b"remove", one_batch=True)

    def insert_batches(self, batches: Iterable[pa.RecordBatch]) -> int:
        """Insert caller-bounded batches, one commit per batch."""

        return self._put_batches(batches, b"insert")

    def remove_batches(self, batches: Iterable[pa.RecordBatch]) -> int:
        """Remove caller-bounded batches, one commit per batch."""

        return self._put_batches(batches, b"remove")

    def stats(self) -> ServerStats:
        return ServerStats.from_protobuf(self._action("stats"))

    def clear(self, key: int) -> int:
        """Atomically remove every ordinal under one key."""

        key = _u64(key, name="key")
        body = self._action("clear", key.to_bytes(8, "little"))
        if len(body) != 8:
            raise ProtocolError(f"yesnodb clear returned {len(body)} bytes instead of 8")
        return int.from_bytes(body, "little")

    def _call_options(self) -> flight.FlightCallOptions:
        headers = list(self._headers)
        if self._token is not None:
            token = self._token() if callable(self._token) else self._token
            if not isinstance(token, str) or not token:
                raise ValueError("token provider must return a non-empty string")
            _check_header("authorization", f"Bearer {token}")
            headers.append(("authorization", f"Bearer {token}"))
        if self._minimum_term is not None:
            headers.append(("yesno-expect-term", str(self._minimum_term)))
        encoded = [(name.encode("ascii"), value.encode("ascii")) for name, value in headers]
        return flight.FlightCallOptions(timeout=self._timeout, headers=encoded)

    def _put_pairs(
        self,
        pairs: Iterable[tuple[int, int]],
        command: bytes,
        *,
        one_batch: bool,
    ) -> int:
        if one_batch:
            keys: list[int] = []
            ordinals: list[int] = []
            for pair in pairs:
                key, ordinal = _pair(pair)
                keys.append(key)
                ordinals.append(ordinal)
            batches: Iterable[pa.RecordBatch] = [_pair_batch(keys, ordinals)] if keys else []
        else:
            batches = _pair_batches(pairs)
        return self._put_batches(batches, command)

    def _put_pairs_acked(
        self,
        pairs: Iterable[tuple[int, int]],
        command: bytes,
        *,
        one_batch: bool,
    ) -> Ack:
        if one_batch:
            keys: list[int] = []
            ordinals: list[int] = []
            for pair in pairs:
                key, ordinal = _pair(pair)
                keys.append(key)
                ordinals.append(ordinal)
            batches: Iterable[pa.RecordBatch] = [_pair_batch(keys, ordinals)] if keys else []
        else:
            batches = _pair_batches(pairs)
        return self._put_batches_acked(batches, command)

    def _put_batches(self, batches: Iterable[pa.RecordBatch], command: bytes) -> int:
        return self._put_batches_acked(batches, command).rows

    def _put_batches_acked(self, batches: Iterable[pa.RecordBatch], command: bytes) -> Ack:
        descriptor = flight.FlightDescriptor.for_command(command)
        writer, acknowledgements = self._flight.do_put(
            descriptor, PAIR_SCHEMA, self._call_options()
        )
        sent = 0
        acknowledged: Ack | None = None
        try:
            for batch in batches:
                _check_pair_batch(batch)
                writer.write_batch(batch)
                sent += batch.num_rows
            writer.done_writing()

            while True:
                metadata = acknowledgements.read()
                if metadata is None:
                    break
                acknowledged = Ack._decode(bytes(metadata))
        finally:
            # PyArrow reports some server-side DoPut failures only here.
            writer.close()

        if acknowledged is None:
            raise ProtocolError("yesnodb returned no ingest acknowledgement")
        if acknowledged.rows != sent:
            raise ProtocolError(f"yesnodb acknowledged {acknowledged.rows} of {sent} ingest pairs")
        return acknowledged

    def _action(self, name: str, request: bytes = b"") -> bytes:
        body = bytearray()
        for result in self._flight.do_action(flight.Action(name, request), self._call_options()):
            body.extend(result.body)
        return bytes(body)


def _flight_location(endpoint: str | flight.Location) -> str | flight.Location:
    if not isinstance(endpoint, str):
        return endpoint
    if endpoint.startswith("http://"):
        return "grpc://" + endpoint.removeprefix("http://")
    if endpoint.startswith("https://"):
        return "grpc+tls://" + endpoint.removeprefix("https://")
    return endpoint


def _check_header(name: str, value: str) -> None:
    if not isinstance(name, str) or not isinstance(value, str):
        raise TypeError("Flight headers must contain strings")
    if not name or any(character in name for character in "\r\n"):
        raise ValueError("Flight header names must be non-empty single lines")
    if any(character in value for character in "\r\n"):
        raise ValueError(f"Flight header {name!r} must fit on one line")
    try:
        name.encode("ascii")
        value.encode("ascii")
    except UnicodeEncodeError as error:
        raise ValueError(f"Flight header {name!r} must contain ASCII only") from error


def _query_info(info: flight.FlightInfo) -> QueryInfo:
    total_records = info.total_records
    if isinstance(total_records, bool) or not isinstance(total_records, int) or total_records < 0:
        raise ProtocolError(f"yesnodb returned invalid total_records {total_records!r}")
    endpoints = info.endpoints
    if len(endpoints) != 1:
        raise ProtocolError(f"yesnodb returned {len(endpoints)} endpoints instead of 1")
    endpoint = endpoints[0]
    if endpoint.ticket is None:
        raise ProtocolError("yesnodb returned no query ticket")
    ticket_bytes = bytes(endpoint.ticket.ticket)
    try:
        ticket = Ticket.decode(ticket_bytes)
    except ValueError as error:
        raise ProtocolError(f"yesnodb returned a malformed query ticket: {error}") from error
    if not info.schema.equals(ORDINAL_SCHEMA, check_metadata=False):
        raise ProtocolError(f"yesnodb returned the wrong query schema: {info.schema}")
    return QueryInfo(total_records, ticket, ticket_bytes, info)


def _check_ordinal_batch(batch: pa.RecordBatch) -> None:
    if not batch.schema.equals(ORDINAL_SCHEMA, check_metadata=False):
        raise ProtocolError(f"yesnodb returned the wrong ordinal batch schema: {batch.schema}")
    if batch.column(0).null_count != 0:
        raise ProtocolError("yesnodb returned nulls in its non-null ordinal column")


def _pair(value: object) -> tuple[int, int]:
    if not isinstance(value, tuple) or len(value) != 2:
        raise TypeError("each pair must be a two-element tuple")
    key = _u64(value[0], name="key")
    ordinal = _u64(value[1], name="ordinal")
    if ordinal == ORDINAL_LIMIT:
        raise ValueError("u64::MAX is reserved and cannot be stored as an ordinal")
    return key, ordinal


def _pair_batch(keys: list[int], ordinals: list[int]) -> pa.RecordBatch:
    return pa.record_batch(
        [
            pa.array(keys, type=pa.uint64()),
            pa.array(ordinals, type=pa.uint64()),
        ],
        schema=PAIR_SCHEMA,
    )


def _pair_batches(pairs: Iterable[tuple[int, int]]) -> Iterator[pa.RecordBatch]:
    keys: list[int] = []
    ordinals: list[int] = []
    for pair in pairs:
        key, ordinal = _pair(pair)
        keys.append(key)
        ordinals.append(ordinal)
        if len(keys) == BATCH_ROWS:
            yield _pair_batch(keys, ordinals)
            keys = []
            ordinals = []
    if keys:
        yield _pair_batch(keys, ordinals)


def _check_pair_batch(batch: pa.RecordBatch) -> None:
    if not isinstance(batch, pa.RecordBatch):
        raise TypeError("batches must contain pyarrow.RecordBatch values")
    if not batch.schema.equals(PAIR_SCHEMA, check_metadata=False):
        raise ValueError(f"pair batch has the wrong schema: {batch.schema}")
    if batch.column(0).null_count or batch.column(1).null_count:
        raise ValueError("pair batches must not contain nulls")
    ordinals = batch.column(1)
    if (
        len(ordinals)
        and pc.any(pc.equal(ordinals, pa.scalar(ORDINAL_LIMIT, type=pa.uint64()))).as_py()
    ):
        raise ValueError("u64::MAX is reserved and cannot be stored as an ordinal")
