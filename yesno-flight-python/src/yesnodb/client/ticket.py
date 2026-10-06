"""Versioned yesnodb query tickets."""

from __future__ import annotations

import struct
from dataclasses import dataclass
from enum import IntEnum

from .errors import TicketError
from .expression import SetExpr, _u64

#: Size of a ticket's fixed header: six little-endian ``uint64``s, in the order
#: :class:`Ticket` declares them. A ticket may append an encoded expression
#: after this header.
#:
#: This was 40 bytes until the set representation was added to the header on
#: 2026-09-30, and this constant was not widened with it. The client kept
#: round-tripping against its own 40-byte header, so every unit test here stayed
#: green, while a real server ticket arrived with eight bytes that went to the
#: expression decoder and were rejected as a malformed ticket. The fixed vector
#: in ``test_fixed_ticket_vector_and_expression_suffix`` is what keeps this in
#: step with the Rust encoder; the length alone cannot, because the client is
#: free to be self-consistently wrong about it.
TICKET_HEADER_LEN = 48


class SetWire(IntEnum):
    """How a set result is encoded in the ``DoGet`` stream.

    Only sets have a choice: a vector or a scalar answers one small batch whose
    shape no representation question applies to. The numbers go on the wire and
    must never be reordered.
    """

    #: One ``uint64`` per ordinal. The default, because a client that has not
    #: asked for containers may have no decoder for them.
    ORDINALS = 0
    #: Container payloads, byte-identical to what the page store and a
    #: ``.roaring`` file hold.
    CONTAINERS = 1
    #: A wholly materialized bitvector over the ticket's prefix window, one bit
    #: per ordinal position, gaps included.
    BITVECTOR = 2


@dataclass(frozen=True, slots=True)
class Ticket:
    """A snapshot version, routing key, and half-open chunk-prefix range."""

    version: int
    key: int
    prefix_lo: int
    prefix_hi: int
    expression_hash: int = 0
    expression: SetExpr | None = None
    #: The representation the ``DoGet`` stream will use.
    wire: SetWire = SetWire.ORDINALS

    def __post_init__(self) -> None:
        _u64(self.version, name="version")
        _u64(self.key, name="key")
        _u64(self.prefix_lo, name="prefix_lo")
        _u64(self.prefix_hi, name="prefix_hi")
        _u64(self.expression_hash, name="expression_hash")
        if self.prefix_lo > self.prefix_hi:
            raise TicketError("ticket prefix range is inverted")
        if self.expression is not None and not isinstance(self.expression, SetExpr):
            raise TypeError("expression must be a SetExpr or None")
        if not isinstance(self.wire, SetWire):
            raise TicketError("ticket asks for an unknown set representation")

    def encode(self) -> bytes:
        payload = struct.pack(
            "<QQQQQQ",
            self.version,
            self.key,
            self.prefix_lo,
            self.prefix_hi,
            self.expression_hash,
            int(self.wire),
        )
        if self.expression is not None:
            payload += self.expression.encode()
        return payload

    @classmethod
    def decode(cls, payload: bytes | bytearray | memoryview) -> Ticket:
        raw = bytes(payload)
        if len(raw) < TICKET_HEADER_LEN:
            raise TicketError(f"ticket has {len(raw)} bytes; expected at least {TICKET_HEADER_LEN}")
        version, key, prefix_lo, prefix_hi, expression_hash, raw_wire = struct.unpack(
            "<QQQQQQ", raw[:TICKET_HEADER_LEN]
        )
        # An unrecognised representation is an error, never a fallback to the
        # default: reading a stream in a representation the server did not
        # promise is corruption, and indistinguishable from a server that
        # understood.
        try:
            wire = SetWire(raw_wire)
        except ValueError as error:
            raise TicketError(f"ticket asks for set representation {raw_wire}") from error
        expression_raw = raw[TICKET_HEADER_LEN:]
        try:
            expression = SetExpr.decode(expression_raw) if expression_raw else None
            return cls(version, key, prefix_lo, prefix_hi, expression_hash, expression, wire)
        except (TypeError, ValueError) as error:
            raise TicketError(f"malformed ticket: {error}") from error
