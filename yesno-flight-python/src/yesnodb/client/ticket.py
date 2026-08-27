"""Versioned yesnodb query tickets."""

from __future__ import annotations

import struct
from dataclasses import dataclass

from .errors import TicketError
from .expression import SetExpr, _u64

TICKET_HEADER_LEN = 40


@dataclass(frozen=True, slots=True)
class Ticket:
    """A snapshot version, routing key, and half-open chunk-prefix range."""

    version: int
    key: int
    prefix_lo: int
    prefix_hi: int
    expression_hash: int = 0
    expression: SetExpr | None = None

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

    def encode(self) -> bytes:
        payload = struct.pack(
            "<QQQQQ",
            self.version,
            self.key,
            self.prefix_lo,
            self.prefix_hi,
            self.expression_hash,
        )
        if self.expression is not None:
            payload += self.expression.encode()
        return payload

    @classmethod
    def decode(cls, payload: bytes | bytearray | memoryview) -> Ticket:
        raw = bytes(payload)
        if len(raw) < TICKET_HEADER_LEN:
            raise TicketError(f"ticket has {len(raw)} bytes; expected at least {TICKET_HEADER_LEN}")
        version, key, prefix_lo, prefix_hi, expression_hash = struct.unpack(
            "<QQQQQ", raw[:TICKET_HEADER_LEN]
        )
        expression_raw = raw[TICKET_HEADER_LEN:]
        try:
            expression = SetExpr.decode(expression_raw) if expression_raw else None
            return cls(version, key, prefix_lo, prefix_hi, expression_hash, expression)
        except (TypeError, ValueError) as error:
            raise TicketError(f"malformed ticket: {error}") from error
