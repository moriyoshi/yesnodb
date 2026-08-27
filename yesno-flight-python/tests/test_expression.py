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
    AnyExpr,
    At,
    Cardinality,
    Contains,
    Empty,
    Expand,
    ExpressionError,
    Fold,
    FoldOp,
    Hole,
    Key,
    List,
    Literal,
    Map,
    MapBool,
    MapInt,
    Or,
    Pack,
    QueryRequest,
    Range,
    Select,
    SetExpr,
    View,
    ViewSpec,
    complement,
    xor,
)


def sample_expression() -> SetExpr:
    return And(
        Key(42),
        Or(Range(0, 10), Range(100, (1 << 64) - 1)),
        AndNot(Key(7), Empty()),
        At(View(Key(50), ViewSpec.interleaved(3)), 1),
        # The composition the old leaf nodes could not express: the folded
        # operand is computed rather than a bare key.
        Fold(View(AndNot(Key(60), Key(61)), ViewSpec.blocked(3, 65_536)), FoldOp.OR),
        Expand(Key(70), ViewSpec.interleaved(2)),
        Pack(List(Key(80), Key(81)), ViewSpec.interleaved(2)),
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
        # Mirrors `the_cross_implementation_wire_vector_is_stable` in the Rust
        # crate byte for byte. Five implementations of one format drift
        # silently otherwise -- each round-trips against itself while
        # disagreeing with the others, and only a shared constant catches it.
        (
            At(View(Key(9), ViewSpec.interleaved(3)), 1),
            "59534e580100060c0300000000000000000000000001090000000000000001000000",
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
    # The walk reaches through the view nodes, which no longer name a key in a
    # field: 61 and the packed keys are only found by recursing.
    assert expression.keys() == (42, 7, 50, 60, 61, 70, 80, 81)


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


def test_the_facet_query_round_trips() -> None:
    """The query the sorted language exists for: per-cohort counts under a filter.

    Mirrors ``the_facet_query_round_trips`` in the Rust crate. It is a *map*,
    not a fold -- it applies a query to each element rather than combining them
    -- and its result is the row marginal, which a fold cannot produce.
    """

    query = AnyExpr(
        MapInt(
            View(Key(9), ViewSpec.interleaved(4)),
            Cardinality(And(Hole(), Key(7))),
        )
    )
    assert query.sort == "vector of integers"
    assert AnyExpr.decode(query.encode()) == query
    assert query.expression.arity == 4
    # The hole names no key; the view's operand and the filter both do.
    assert query.expression.keys() == (9, 7)


def test_the_hole_is_scoped_to_a_map_body() -> None:
    """``_`` is legal only inside a map body, and a body may not contain a map."""

    with pytest.raises(ExpressionError, match="outside a map body"):
        SetExpr.decode(Hole().encode())

    two = List(Key(1), Key(2))

    # Inside a body it is fine, and every ``_`` in one body is the same element.
    ok = Fold(Map(two, And(Hole(), Key(7))), FoldOp.OR)
    assert SetExpr.decode(ok.encode()) == ok

    # A map in a body is refused ..
    nested = Map(two, Fold(Map(two, Hole()), FoldOp.OR))
    with pytest.raises(ExpressionError, match="may not contain a map"):
        SetExpr.decode(Fold(nested, FoldOp.OR).encode())

    # .. but a map in a *vector* position is sequential, not nested.
    sequential = Fold(Map(Map(two, Hole()), Hole()), FoldOp.OR)
    assert SetExpr.decode(sequential.encode()) == sequential


def test_the_sorts_reject_each_others_tags() -> None:
    """An integer vector is not a set, and the error names both sides."""

    counts = AnyExpr(MapInt(View(Key(9), ViewSpec.interleaved(2)), Cardinality(Hole())))
    with pytest.raises(ExpressionError, match="where a set was required"):
        SetExpr.decode(counts.encode())


def test_bool_bodies_and_select_denote_sets() -> None:
    """A truth value per constituent is a subset of the constituent indices."""

    which = MapBool(View(Key(9), ViewSpec.interleaved(3)), Contains(Hole(), 4))
    assert SetExpr.decode(which.encode()) == which

    # ``select`` is partial, so it yields a set rather than a sentinel.
    nth = Select(Key(7), 0)
    assert SetExpr.decode(nth.encode()) == nth
