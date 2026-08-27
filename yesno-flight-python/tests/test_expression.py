from __future__ import annotations

import contextlib
import struct

import pytest
from hypothesis import given
from hypothesis import strategies as st

from yesnodb.client import (
    MAX_NODES,
    And,
    AndNot,
    Empty,
    ExpressionError,
    Key,
    Literal,
    Or,
    QueryRequest,
    Range,
    SetExpr,
    ViewExpand,
    ViewFold,
    ViewReduce,
    ViewSelect,
    ViewSpec,
    complement,
    xor,
)


def sample_expression() -> SetExpr:
    return And(
        Key(42),
        Or(Range(0, 10), Range(100, (1 << 64) - 1)),
        AndNot(Key(7), Empty()),
        ViewSelect(50, ViewSpec.interleaved(3), 1),
        ViewFold(60, ViewSpec.blocked(3, 65_536), ViewReduce.ANY),
        ViewExpand(Key(70), ViewSpec.interleaved(2)),
    )


@pytest.mark.parametrize(
    ("expression", "wire_hex"),
    [
        (Empty(), "59534e58010000"),
        (Key(42), "59534e580100012a00000000000000"),
        (
            Literal(0, 2, 65_536, (1 << 64) - 2),
            "59534e5801000904000000000000000000000002000000000000000000010000000000feffffffffffffff",
        ),
        (
            Range(1, 3),
            "59534e5801000201000000000000000300000000000000",
        ),
        (
            And(Key(1), Or(Key(2), Key(3))),
            "59534e580100030200010100000000000000040200010200000000000000010300000000000000",
        ),
        (
            ViewSelect(9, ViewSpec.interleaved(3), 1),
            "59534e5801000609000000000000000300000000000000000000000001000000",
        ),
    ],
)
def test_fixed_rust_wire_vectors(expression: SetExpr, wire_hex: str) -> None:
    wire = bytes.fromhex(wire_hex)
    assert expression.encode() == wire
    assert SetExpr.decode(wire) == expression


def test_every_expression_variant_round_trips() -> None:
    expression = And(sample_expression(), Literal(9, 1, 9))
    assert SetExpr.decode(expression.encode()) == expression
    assert expression.keys() == (42, 7, 50, 60, 70)


def test_literal_normalizes_and_rejects_the_reserved_ordinal() -> None:
    assert Literal(9, 1, 9, 5).ordinals == (1, 5, 9)
    with pytest.raises(ValueError):
        Literal((1 << 64) - 1)


def test_boolean_helpers_use_only_v1_nodes() -> None:
    left = Key(1)
    right = Key(2)
    assert xor(left, right) == AndNot(Or(left, right), And(left, right))
    assert left ^ right == xor(left, right)
    assert complement(left) == AndNot(Range(0, (1 << 64) - 1), left)
    assert ~left == complement(left)


def test_a_key_that_starts_with_magic_is_not_an_expression() -> None:
    colliding_key = struct.unpack("<Q", b"YSNX\0\0\0\0")[0]
    assert not SetExpr.looks_like_expression(struct.pack("<Q", colliding_key))
    assert SetExpr.looks_like_expression(Key(colliding_key).encode())


def test_python_bool_is_not_silently_accepted_as_an_integer() -> None:
    with pytest.raises(TypeError):
        Key(True)
    with pytest.raises(TypeError):
        Range(0, False)
    with pytest.raises(TypeError):
        ViewSpec.interleaved(True)


def test_expression_limits_are_checked_before_encoding() -> None:
    too_deep: SetExpr = Empty()
    for _ in range(34):
        too_deep = And(too_deep)
    with pytest.raises(ExpressionError, match="deeper"):
        too_deep.encode()

    too_many = Or(*(Empty() for _ in range(MAX_NODES)))
    with pytest.raises(ExpressionError, match="more than"):
        too_many.encode()


@given(st.binary(max_size=512))
def test_decoder_never_raises_an_unexpected_exception(payload: bytes) -> None:
    with contextlib.suppress(ExpressionError):
        SetExpr.decode(payload)


def test_malformed_payloads_are_refused() -> None:
    with pytest.raises(ExpressionError):
        SetExpr.decode(b"")
    with pytest.raises(ExpressionError, match="trailing"):
        SetExpr.decode(Empty().encode() + b"extra")
    with pytest.raises(ExpressionError, match="no operands"):
        SetExpr.decode(b"YSNX\x01\x00\x03\x00\x00")
    with pytest.raises(ExpressionError, match="strictly ascending"):
        SetExpr.decode(bytes.fromhex("59534e580100090200000002000000000000000100000000000000"))


def test_query_request_matches_rust_wire_and_round_trips() -> None:
    request = QueryRequest.at(Key(42), 7)
    wire = bytes.fromhex("59534e510101070000000000000059534e580100012a00000000000000")
    assert request.encode() == wire
    assert QueryRequest.decode(wire) == request
    assert QueryRequest.looks_like_request(wire)

    current = QueryRequest.current(Range(1, 3))
    assert QueryRequest.decode(current.encode()) == current


@given(st.binary(max_size=512))
def test_query_request_decoder_never_raises_an_unexpected_exception(payload: bytes) -> None:
    with contextlib.suppress(ExpressionError):
        QueryRequest.decode(payload)


def test_malformed_query_requests_are_refused() -> None:
    with pytest.raises(ExpressionError):
        QueryRequest.decode(b"")
    unknown_flags = bytearray(QueryRequest.current(Empty()).encode())
    unknown_flags[5] = 2
    with pytest.raises(ExpressionError, match="unsupported"):
        QueryRequest.decode(unknown_flags)
    unpinned_version = bytearray(QueryRequest.current(Empty()).encode())
    unpinned_version[6] = 1
    with pytest.raises(ExpressionError, match="non-zero"):
        QueryRequest.decode(unpinned_version)
