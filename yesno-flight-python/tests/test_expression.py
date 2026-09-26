from __future__ import annotations

import contextlib
import struct

import pytest
from hypothesis import given
from hypothesis import strategies as st

from yesnodb.client import (
    MAX_NODES,
    MAX_RESULT_BITS,
    MAX_VALUE_BITS,
    MAX_WORK,
    SORT_BIG,
    SORT_VEC_BIG,
    Add,
    And,
    AndNot,
    AnyExpr,
    At,
    BigExpr,
    BigBinOp,
    BigFold,
    BigFoldOp,
    BigScale,
    BigZip,
    BigList,
    BigLit,
    Cardinality,
    Contains,
    Div,
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
    MapBig,
    MapBool,
    MapInt,
    Mul,
    Neg,
    Or,
    Pack,
    PowMod,
    QueryRequest,
    Range,
    ReadInt,
    ReadUint,
    Rem,
    Saturate,
    Select,
    SetExpr,
    Sub,
    Truncate,
    View,
    ViewLayout,
    ViewSpec,
    Widen,
    big,
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


# ---------------------------------------------------------------------------
# The big-integer sort
# ---------------------------------------------------------------------------


def test_big_wire_vector_matches_the_rust_crate() -> None:
    """Mirrors `the_big_cross_implementation_wire_vector_is_stable` in Rust.

    Five implementations of one format drift silently otherwise: each can
    round-trip against itself while disagreeing with the others.
    """

    e = Saturate(Mul(ReadUint(Key(4), 128), BigLit(-3)), 32)
    assert (
        AnyExpr(e).encode().hex()
        == "59534e58010023200000001f1a8000000001040000000000000018010100000003"
    )
    assert AnyExpr.decode(AnyExpr(e).encode()).expression == e
    assert AnyExpr(e).sort == SORT_BIG


@pytest.mark.parametrize(
    "expression",
    [
        BigLit(0),
        BigLit(1),
        BigLit(-1),
        BigLit(2**200),
        BigLit(-(2**200)),
        Widen(Cardinality(Key(7))),
        ReadUint(Key(4), 128),
        ReadInt(Key(4), 8),
        Neg(BigLit(5)),
        Add(BigLit(1), BigLit(2)),
        Sub(BigLit(1), BigLit(2)),
        Mul(BigLit(3), BigLit(4)),
        Div(BigLit(7), BigLit(2)),
        Rem(BigLit(7), BigLit(2)),
        Truncate(BigLit(300), 8),
        Saturate(BigLit(300), 8),
    ],
)
def test_big_nodes_round_trip(expression: BigExpr) -> None:
    assert AnyExpr.decode(AnyExpr(expression).encode()).expression == expression


def test_a_big_query_request_round_trips() -> None:
    e = Mul(ReadUint(Key(1), 64), BigLit(-3))
    for request in (QueryRequest.current(e), QueryRequest.at(e, 7)):
        assert QueryRequest.decode(request.encode()) == request


def test_a_negative_zero_is_unrepresentable() -> None:
    """One value, one encoding, or a shared byte vector states nothing."""

    assert not BigLit(0).value
    encoded = AnyExpr(BigLit(0)).encode()
    # sign byte 0, length 0: no trailing zero byte to disagree about.
    assert encoded[-5:] == b"\x00\x00\x00\x00\x00"
    # And a hand-built negative zero is refused on the way back in.
    bad = bytearray(encoded)
    bad[-5] = 1
    with pytest.raises(ExpressionError):
        AnyExpr.decode(bytes(bad))


def test_a_trailing_zero_byte_is_refused_rather_than_trimmed() -> None:
    encoded = bytearray(AnyExpr(BigLit(1)).encode())
    encoded[-5:-1] = (2).to_bytes(4, "little")
    encoded.append(0)
    with pytest.raises(ExpressionError):
        AnyExpr.decode(bytes(encoded))


def test_the_width_budget_refuses_an_amplification() -> None:
    """A read is six bytes that declare a width, so a product of two maximal
    reads is a tiny payload describing a value no server should build."""

    wide = ReadUint(Key(1), MAX_VALUE_BITS)
    with pytest.raises(ExpressionError):
        Mul(wide, wide)
    # One bit under the bound is accepted, so the refusal is the bound working
    # rather than the shape being rejected.
    half = ReadUint(Key(1), MAX_VALUE_BITS // 2)
    assert Mul(half, half).width_bound() == MAX_VALUE_BITS


def test_a_zero_width_read_is_refused() -> None:
    with pytest.raises(ExpressionError):
        ReadUint(Key(1), 0)


def test_a_big_tag_where_a_set_belongs_is_a_sort_mismatch() -> None:
    payload = bytearray(AnyExpr(BigLit(1)).encode())
    with pytest.raises(ExpressionError):
        SetExpr.decode(bytes(payload))


def _big_vector() -> MapBig:
    return MapBig(
        View(Key(9), ViewSpec(3, ViewLayout.INTERLEAVED, 0)),
        ReadUint(Hole(), 128),
    )


def test_a_vector_of_big_integers_is_built_by_map_and_round_trips() -> None:
    v = _big_vector()
    assert AnyExpr(v).sort == SORT_VEC_BIG
    assert AnyExpr.decode(AnyExpr(v).encode()).expression == v
    assert AnyExpr.decode(AnyExpr(BigList(BigLit(1), BigLit(-2))).encode()).expression == BigList(
        BigLit(1), BigLit(-2)
    )


@pytest.mark.parametrize(
    "op", [BigBinOp.ADD, BigBinOp.SUB, BigBinOp.MUL, BigBinOp.DIV, BigBinOp.REM]
)
def test_a_zip_and_a_scale_round_trip(op: BigBinOp) -> None:
    z = BigZip(BigList(BigLit(7), BigLit(8)), BigList(BigLit(1), BigLit(2)), op)
    assert AnyExpr(z).sort == SORT_VEC_BIG
    assert AnyExpr.decode(AnyExpr(z).encode()).expression == z

    sc = BigScale(_big_vector(), BigLit(3), op)
    assert AnyExpr(sc).sort == SORT_VEC_BIG
    assert AnyExpr.decode(AnyExpr(sc).encode()).expression == sc


def test_a_zip_of_unequal_arities_is_refused_when_built() -> None:
    """Both arities are statically known, so this never reaches evaluation."""

    with pytest.raises(ExpressionError):
        BigZip(BigList(BigLit(1), BigLit(2)), BigList(BigLit(3)), BigBinOp.ADD)


def test_a_zip_is_bounded_like_the_elements_written_out() -> None:
    """The point of sharing BigBinOp's rules with the scalar nodes."""

    a = BigScale(_big_vector(), BigLit(3), BigBinOp.MUL)
    # 128-bit elements times a 2-bit literal.
    assert a.element_bound() == 128 + BigLit(3).width_bound()
    assert BigScale(_big_vector(), BigLit(3), BigBinOp.ADD).element_bound() == 129


@pytest.mark.parametrize("op", [BigFoldOp.ADD, BigFoldOp.MUL, BigFoldOp.MIN, BigFoldOp.MAX])
def test_a_fold_collapses_a_big_vector_and_round_trips(op: BigFoldOp) -> None:
    f = BigFold(BigList(BigLit(1), BigLit(2)), op)
    assert AnyExpr(f).sort == SORT_BIG
    assert AnyExpr.decode(AnyExpr(f).encode()).expression == f


def test_only_a_product_fold_grows_with_the_arity() -> None:
    """A sum costs a handful of bits; a product costs arity times the width."""

    v = _big_vector()
    assert BigFold(v, BigFoldOp.ADD).width_bound() == 128 + 2
    assert BigFold(v, BigFoldOp.MUL).width_bound() == 128 * 3
    assert BigFold(v, BigFoldOp.MIN).width_bound() == 128


def test_the_result_bound_refuses_a_wide_vector_of_wide_values() -> None:
    """Two bounded factors have an unbounded product.

    The constituent cap and the per-value width bound each cap one factor and
    say nothing about 4096 elements *of* a million bits.
    """

    # The vector itself is refused at construction, before anything can be
    # asked of it -- which is earlier than `AnyExpr` and is the point.
    with pytest.raises(ExpressionError):
        MapBig(
            View(Key(1), ViewSpec(4096, ViewLayout.INTERLEAVED, 0)),
            ReadUint(Hole(), MAX_VALUE_BITS),
        )
    # The same shape inside the bound is accepted.
    ok = MapBig(
        View(Key(1), ViewSpec(64, ViewLayout.INTERLEAVED, 0)),
        ReadUint(Hole(), 1024),
    )
    assert AnyExpr(ok).sort == SORT_VEC_BIG
    assert ok.result_bound() == 64 * 1024 <= MAX_RESULT_BITS


def test_a_product_fold_over_a_wide_vector_is_refused() -> None:
    """The product lands in one value, so the per-value bound has to hold it."""

    v = MapBig(
        View(Key(1), ViewSpec(1024, ViewLayout.INTERLEAVED, 0)),
        ReadUint(Hole(), 4096),
    )
    with pytest.raises(ExpressionError):
        BigFold(v, BigFoldOp.MUL)
    # Summing the same vector is nowhere near the bound.
    assert BigFold(v, BigFoldOp.ADD).width_bound() == 4096 + 11


def test_pow_mod_wire_vector_matches_the_rust_crate() -> None:
    """Operand order is the one thing a reader cannot infer from the bytes."""

    e = PowMod(BigLit(2), BigLit(10), BigLit(1000))
    assert AnyExpr(e).encode().hex() == "59534e58010027180001000000021800010000000a180002000000e803"
    assert AnyExpr.decode(AnyExpr(e).encode()).expression == e


def test_a_costly_exponentiation_is_refused_where_its_width_is_unremarkable() -> None:
    """The amplification the width bound structurally cannot see.

    A residue is only as wide as its modulus, so nothing about the size of this
    query is alarming; the cost is what is wrong with it.
    """

    wide = ReadUint(Key(1), MAX_VALUE_BITS)
    with pytest.raises(ExpressionError):
        PowMod(BigLit(2), wide, wide)


def test_rsa_scale_exponentiation_is_admitted() -> None:
    """So the bound is calibrated rather than merely restrictive."""

    for bits in (2048, 4096):
        operand = ReadUint(Key(1), bits)
        e = PowMod(BigLit(2), operand, operand)
        assert e.work_bound() <= MAX_WORK
        assert AnyExpr.decode(AnyExpr(e).encode()).expression == e


def test_big_mirrors_the_query_language_spelling() -> None:
    """``big(1)`` is a value and ``big([1, 2, 3])`` is a vector.

    The same spelling carries the sort in both, exactly as the query language
    does -- a bare list is a vector of *sets* everywhere, so a vector of big
    integers has to say so where it is written.
    """

    assert big(7) == BigLit(7)
    assert big(-7) == BigLit(-7)
    assert big([1, 2, 3]) == BigList(BigLit(1), BigLit(2), BigLit(3))
    # Elements may already be expressions, so the factory composes.
    assert big([1, ReadUint(Key(4), 64)]) == BigList(BigLit(1), ReadUint(Key(4), 64))

    folded = BigFold(big([1, 2, 3]), BigFoldOp.ADD)
    assert AnyExpr.decode(AnyExpr(folded).encode()).expression == folded


def test_big_rejects_what_is_neither_a_value_nor_a_sequence() -> None:
    with pytest.raises(TypeError):
        big("3")  # type: ignore[call-overload]
    with pytest.raises(TypeError):
        big(True)  # type: ignore[arg-type]
