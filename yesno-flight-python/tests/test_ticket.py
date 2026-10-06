from __future__ import annotations

import contextlib

import pytest
from hypothesis import given
from hypothesis import strategies as st

from yesnodb.client import TICKET_HEADER_LEN, And, Key, Range, SetWire, Ticket, TicketError


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
    # Mirrors `the_cross_implementation_ticket_header_is_stable` in the Rust
    # crate byte for byte: version(8) key(8) prefix_lo(8) prefix_hi(8)
    # expr_hash(8) wire(8). The length alone is not enough -- this client was
    # self-consistently wrong about it for a week while every unit test passed.
    expected_header = (
        "09000000000000002a0000000000000001000000000000000000100000000000"
        "07000000000000000000000000000000"
    )
    encoded = ticket.encode()
    assert encoded[:TICKET_HEADER_LEN].hex() == expected_header
    assert encoded[TICKET_HEADER_LEN:] == expression.encode()
    assert Ticket.decode(encoded) == ticket


def test_header_only_ticket_remains_valid() -> None:
    ticket = Ticket(3, 7, 0, 1 << 48)
    assert len(ticket.encode()) == TICKET_HEADER_LEN
    assert Ticket.decode(ticket.encode()) == ticket


def test_requested_representation_is_the_sixth_header_word() -> None:
    """The representation's *position* is checked, not merely its presence.

    A vector for the default alone would still pass if the field moved, because
    its bytes are zero.
    """
    ticket = Ticket(9, 42, 1, 1 << 20, 7, None, SetWire.CONTAINERS)
    assert ticket.encode().hex() == (
        "09000000000000002a0000000000000001000000000000000000100000000000"
        "07000000000000000100000000000000"
    )
    assert Ticket.decode(ticket.encode()).wire is SetWire.CONTAINERS


def test_unknown_representation_is_rejected() -> None:
    """An unrecognised representation is an error, never a fallback.

    Reading a stream in a representation the server did not promise is
    corruption, and indistinguishable from a server that understood.
    """
    raw = bytearray(Ticket(1, 2, 0, 1 << 48).encode())
    raw[40:48] = (200).to_bytes(8, "little")
    with pytest.raises(TicketError, match="representation"):
        Ticket.decode(bytes(raw))

    # And the three it knows still round-trip, so the check is not vacuous.
    for wire in SetWire:
        ticket = Ticket(1, 2, 0, 1 << 48, 0, None, wire)
        assert Ticket.decode(ticket.encode()).wire is wire


def test_bad_ticket_is_not_guessed() -> None:
    with pytest.raises(TicketError, match="at least"):
        Ticket.decode(bytes(TICKET_HEADER_LEN - 1))
    with pytest.raises(TicketError, match="inverted"):
        Ticket(1, 2, 10, 5)

    valid = Ticket(1, 2, 0, 10).encode()
    with pytest.raises(TicketError, match="malformed"):
        Ticket.decode(valid + b"not an expression")


@given(st.binary(max_size=256))
def test_ticket_decoder_never_raises_an_unexpected_exception(payload: bytes) -> None:
    with contextlib.suppress(TicketError):
        Ticket.decode(payload)
