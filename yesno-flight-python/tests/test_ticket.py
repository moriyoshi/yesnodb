from __future__ import annotations

import contextlib

import pytest
from hypothesis import given
from hypothesis import strategies as st

from yesnodb.client import And, Key, Range, Ticket, TicketError


def test_fixed_ticket_vector_and_expression_suffix() -> None:
    expression = And(Key(1), Range(2, 9))
    ticket = Ticket(
        version=9,
        key=42,
        prefix_lo=1,
        prefix_hi=1 << 20,
        expression_hash=7,
        expression=expression,
    )
    expected_header = (
        "09000000000000002a00000000000000010000000000000000001000000000000700000000000000"
    )
    assert ticket.encode().hex().startswith(expected_header)
    assert Ticket.decode(ticket.encode()) == ticket


def test_old_header_only_ticket_remains_valid() -> None:
    ticket = Ticket(3, 7, 0, 1 << 48)
    assert len(ticket.encode()) == 40
    assert Ticket.decode(ticket.encode()) == ticket


def test_bad_ticket_is_not_guessed() -> None:
    with pytest.raises(TicketError, match="at least"):
        Ticket.decode(bytes(39))
    with pytest.raises(TicketError, match="inverted"):
        Ticket(1, 2, 10, 5)

    valid = Ticket(1, 2, 0, 10).encode()
    with pytest.raises(TicketError, match="malformed"):
        Ticket.decode(valid + b"not an expression")


@given(st.binary(max_size=256))
def test_ticket_decoder_never_raises_an_unexpected_exception(payload: bytes) -> None:
    with contextlib.suppress(TicketError):
        Ticket.decode(payload)
