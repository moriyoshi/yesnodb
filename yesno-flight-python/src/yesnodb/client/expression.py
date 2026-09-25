"""The dependency-free yesnodb set-expression wire format.

The format mirrors the Rust ``yesno-wire`` crate. Expressions are immutable,
strictly checked before encoding, and decoded with explicit depth and node
limits because descriptors received from a network are untrusted bytes.
"""

from __future__ import annotations

import struct
from collections.abc import Sequence
from dataclasses import dataclass
from enum import IntEnum
from itertools import pairwise
from typing import ClassVar, overload

from .errors import ExpressionError

MAGIC = b"YSNX"
VERSION = 1
QUERY_MAGIC = b"YSNQ"
QUERY_VERSION = 1
MAX_DEPTH = 32
MAX_NODES = 4096
# A view descriptor is a fixed 13 bytes whatever it declares, while the server
# loops over every constituent -- so neither MAX_DEPTH nor MAX_NODES bounds it.
MAX_VIEW_SETS = 4096
ORDINAL_LIMIT = (1 << 64) - 1

_HEADER = MAGIC + bytes((VERSION, 0))
_BARE_KEY_LEN = 8
_QUERY_HEADER_LEN = 14
_QUERY_FLAG_PINNED = 1

_TAG_EMPTY = 0
_TAG_KEY = 1
_TAG_RANGE = 2
_TAG_AND = 3
_TAG_OR = 4
_TAG_AND_NOT = 5
_TAG_AT = 6
_TAG_FOLD = 7
_TAG_EXPAND = 8
_TAG_LITERAL = 9
_TAG_PACK = 10
# Vector-sorted nodes share one tag space with the set-sorted ones, so a node
# in the wrong position is a sort error rather than a reinterpretation.
_TAG_LIST = 11
_TAG_VIEW = 12
_TAG_HOLE = 13
_TAG_SELECT = 14
_TAG_MAP_SET = 15
_TAG_MAP_INT = 16
_TAG_MAP_BOOL = 17
_TAG_CARDINALITY = 18
_TAG_RANK = 19
_TAG_INT_LIT = 20
_TAG_INT_AT = 21
_TAG_INT_LIST = 22
_TAG_CONTAINS = 23
_TAG_BIG_LIT = 24
_TAG_BIG_WIDEN = 25
_TAG_BIG_READ = 26
_TAG_BIG_READ_SIGNED = 27
_TAG_BIG_NEG = 28
_TAG_BIG_ADD = 29
_TAG_BIG_SUB = 30
_TAG_BIG_MUL = 31
_TAG_BIG_DIV = 32
_TAG_BIG_REM = 33
_TAG_BIG_TRUNCATE = 34
_TAG_BIG_SATURATE = 35
_TAG_BIG_LIST = 36
_TAG_MAP_BIG = 37
_TAG_BIG_FOLD = 38
_TAG_BIG_POW_MOD = 39

# The widest value a big-integer node may denote. Mirrors `MAX_VALUE_BITS` in
# the Rust crate: multiplication adds the operands' widths, so without this a
# small payload describes a value no server should try to build.
MAX_VALUE_BITS = 1 << 20

# The widest whole result a vector of big integers may denote.
#
# The product of two bounded factors is not bounded by either: 4096
# constituents of 2**20 bits each is half a gigabyte of answer from a payload
# of a few dozen bytes. Mirrors MAX_RESULT_BITS in the Rust crate.
MAX_RESULT_BITS = 1 << 24

# Work a query may ask for, in limb operations.
#
# The first bound that is not about size. Modular exponentiation returns a value
# no wider than its modulus, so the width bound finds nothing wrong with a
# payload naming a computation that would not finish. Mirrors MAX_WORK in the
# Rust crate.
#
# An admission bound, not a cost model: every rule is a deliberate *upper*
# bound, which is right for refusing the absurd and wrong for ranking plans.
MAX_WORK = 1 << 28


def _limbs(bits: int) -> int:
    return -(-bits // 64)


# One table, consulted by every decoder's fallback. Enumerating other sorts'
# tags inside each arm is how a tag ends up reported as unknown in one position
# and as a sort mismatch in another.
SORT_SET = "set"
SORT_VEC_SET = "vector of sets"
SORT_INT = "integer"
SORT_VEC_INT = "vector of integers"
SORT_BOOL = "boolean"
SORT_BIG = "big integer"
SORT_VEC_BIG = "vector of big integers"

_TAG_SORTS = {
    _TAG_EMPTY: SORT_SET,
    _TAG_KEY: SORT_SET,
    _TAG_RANGE: SORT_SET,
    _TAG_AND: SORT_SET,
    _TAG_OR: SORT_SET,
    _TAG_AND_NOT: SORT_SET,
    _TAG_AT: SORT_SET,
    _TAG_FOLD: SORT_SET,
    _TAG_EXPAND: SORT_SET,
    _TAG_LITERAL: SORT_SET,
    _TAG_PACK: SORT_SET,
    _TAG_LIST: SORT_VEC_SET,
    _TAG_VIEW: SORT_VEC_SET,
    _TAG_HOLE: SORT_SET,
    _TAG_SELECT: SORT_SET,
    _TAG_MAP_SET: SORT_VEC_SET,
    _TAG_MAP_INT: SORT_VEC_INT,
    _TAG_MAP_BOOL: SORT_SET,
    _TAG_CARDINALITY: SORT_INT,
    _TAG_RANK: SORT_INT,
    _TAG_INT_LIT: SORT_INT,
    _TAG_INT_AT: SORT_INT,
    _TAG_INT_LIST: SORT_VEC_INT,
    _TAG_CONTAINS: SORT_BOOL,
    _TAG_BIG_LIT: SORT_BIG,
    _TAG_BIG_WIDEN: SORT_BIG,
    _TAG_BIG_READ: SORT_BIG,
    _TAG_BIG_READ_SIGNED: SORT_BIG,
    _TAG_BIG_NEG: SORT_BIG,
    _TAG_BIG_ADD: SORT_BIG,
    _TAG_BIG_SUB: SORT_BIG,
    _TAG_BIG_MUL: SORT_BIG,
    _TAG_BIG_DIV: SORT_BIG,
    _TAG_BIG_REM: SORT_BIG,
    _TAG_BIG_TRUNCATE: SORT_BIG,
    _TAG_BIG_SATURATE: SORT_BIG,
    _TAG_BIG_LIST: SORT_VEC_BIG,
    _TAG_MAP_BIG: SORT_VEC_BIG,
    _TAG_BIG_FOLD: SORT_BIG,
    _TAG_BIG_POW_MOD: SORT_BIG,
}


def _misplaced(expected: str, tag: int) -> ExpressionError:
    """A known tag of another sort is a sort mismatch; an undefined one is unknown."""

    if tag in _TAG_SORTS:
        return ExpressionError(f"tag {tag} where a {expected} was required")
    return ExpressionError(f"unknown expression tag {tag}")


def _whole_number(value: object, *, name: str, maximum: int) -> int:
    if isinstance(value, bool) or not isinstance(value, int):
        raise TypeError(f"{name} must be an integer, not {type(value).__name__}")
    if not 0 <= value <= maximum:
        raise ValueError(f"{name} must be in [0, {maximum}]")
    return value


def _u64(value: object, *, name: str) -> int:
    return _whole_number(value, name=name, maximum=(1 << 64) - 1)


def _u32(value: object, *, name: str) -> int:
    return _whole_number(value, name=name, maximum=(1 << 32) - 1)


class ViewLayout(IntEnum):
    """How packed view constituents share the physical ordinal space."""

    INTERLEAVED = 0
    BLOCKED = 1


class BigFoldOp(IntEnum):
    """How :class:`BigFold` combines a vector's elements.

    Four, where the set fold has three: the carrier is the integers, where
    ``+`` and ``*`` are the ring operations and ``min`` / ``max`` the lattice
    ones. All four are associative and commutative, so the answer does not
    depend on the order constituents are visited in. None needs an identity,
    because a vector is never empty -- which is what admits ``min`` and ``max``
    in an unbounded domain.
    """

    ADD = 0
    MUL = 1
    MIN = 2
    MAX = 3


class FoldOp(IntEnum):
    """How :class:`Fold` combines a vector's elements.

    Exactly three, and provably so: on ``{0, 1}`` an associative, unital
    operation is automatically commutative and is one of four -- ``or``, ``xor``
    with unit 0 and ``and``, ``iff`` with unit 1. ``iff`` keeps a fibre of
    zeros, so its result is dense however sparse the operand, and ``andnot`` is
    neither associative nor commutative.
    """

    OR = 0
    AND = 1
    PARITY = 2


@dataclass(frozen=True, slots=True)
class ViewSpec:
    """The descriptor required to interpret one stored key as a packed view."""

    sets: int
    layout: ViewLayout
    stride: int = 0

    def __post_init__(self) -> None:
        sets = _u32(self.sets, name="sets")
        if sets == 0:
            raise ValueError("a view must contain at least one constituent")
        if sets > MAX_VIEW_SETS:
            raise ValueError(f"a view must contain at most {MAX_VIEW_SETS} constituents")
        if not isinstance(self.layout, ViewLayout):
            raise TypeError("layout must be a ViewLayout")
        stride = _u64(self.stride, name="stride")
        if self.layout is ViewLayout.INTERLEAVED and stride != 0:
            raise ValueError("an interleaved view must have stride 0")
        if self.layout is ViewLayout.BLOCKED and stride == 0:
            raise ValueError("a blocked view must have a non-zero stride")

    @classmethod
    def interleaved(cls, sets: int) -> ViewSpec:
        return cls(sets=sets, layout=ViewLayout.INTERLEAVED)

    @classmethod
    def blocked(cls, sets: int, stride: int) -> ViewSpec:
        return cls(sets=sets, layout=ViewLayout.BLOCKED, stride=stride)

    def ordinal_of(self, set_index: int, logical_ordinal: int) -> int | None:
        """Return a constituent's physical ordinal, or ``None`` if unaddressable."""

        set_index = _u32(set_index, name="set_index")
        logical_ordinal = _u64(logical_ordinal, name="logical_ordinal")
        if set_index >= self.sets:
            return None
        if self.layout is ViewLayout.INTERLEAVED:
            ordinal = logical_ordinal * self.sets + set_index
        else:
            if logical_ordinal >= self.stride:
                return None
            ordinal = set_index * self.stride + logical_ordinal
        return ordinal if ordinal < ORDINAL_LIMIT else None

    def _encode_into(self, out: bytearray) -> None:
        out.extend(struct.pack("<IBQ", self.sets, int(self.layout), self.stride))


class SetExpr:
    """Base class for a yesnodb set expression."""

    _tag: ClassVar[int]

    def encode(self) -> bytes:
        """Encode this expression as a complete v1 descriptor command."""

        _check_shape(self)
        out = bytearray(_HEADER)
        self._encode_node(out)
        if len(out) == _BARE_KEY_LEN:
            raise AssertionError("an expression encoding must not be ambiguous with a bare key")
        return bytes(out)

    @classmethod
    def decode(cls, payload: bytes | bytearray | memoryview) -> SetExpr:
        """Decode a complete v1 descriptor command."""

        raw = bytes(payload)
        if len(raw) < len(_HEADER):
            raise ExpressionError("expression ended in its header")
        if raw[:4] != MAGIC:
            raise ExpressionError("not a yesnodb expression")
        if raw[4] != VERSION or raw[5] != 0:
            raise ExpressionError(f"unsupported expression version {raw[4]}")
        cursor = _Cursor(raw[len(_HEADER) :])
        expression = cursor.expression(0)
        if cursor.offset != len(cursor.payload):
            raise ExpressionError("trailing bytes after expression")
        return expression

    @staticmethod
    def looks_like_expression(payload: bytes | bytearray | memoryview) -> bool:
        raw = bytes(payload)
        return len(raw) != _BARE_KEY_LEN and len(raw) >= len(_HEADER) and raw[:4] == MAGIC

    def keys(self) -> tuple[int, ...]:
        """Return keys read by the expression, retaining traversal order."""

        found: list[int] = []
        self._append_keys(found)
        return tuple(found)

    def __and__(self, other: SetExpr) -> SetExpr:
        return And(self, _expression(other))

    def __or__(self, other: SetExpr) -> SetExpr:
        return Or(self, _expression(other))

    def __sub__(self, other: SetExpr) -> SetExpr:
        return AndNot(self, _expression(other))

    def __xor__(self, other: SetExpr) -> SetExpr:
        return xor(self, _expression(other))

    def __invert__(self) -> SetExpr:
        return complement(self)

    def _encode_node(self, out: bytearray) -> None:
        raise NotImplementedError

    def _append_keys(self, out: list[int]) -> None:
        raise NotImplementedError


def _expression(value: object) -> SetExpr:
    if not isinstance(value, SetExpr):
        raise TypeError(f"expected a SetExpr, not {type(value).__name__}")
    return value


@dataclass(frozen=True, slots=True)
class Empty(SetExpr):
    _tag: ClassVar[int] = _TAG_EMPTY

    def _encode_node(self, out: bytearray) -> None:
        out.append(self._tag)

    def _append_keys(self, out: list[int]) -> None:
        return


@dataclass(frozen=True, slots=True)
class Key(SetExpr):
    key: int
    _tag: ClassVar[int] = _TAG_KEY

    def __post_init__(self) -> None:
        _u64(self.key, name="key")

    def _encode_node(self, out: bytearray) -> None:
        out.append(self._tag)
        out.extend(struct.pack("<Q", self.key))

    def _append_keys(self, out: list[int]) -> None:
        out.append(self.key)


@dataclass(frozen=True, slots=True)
class Range(SetExpr):
    lo: int
    hi: int
    _tag: ClassVar[int] = _TAG_RANGE

    def __post_init__(self) -> None:
        _u64(self.lo, name="lo")
        _u64(self.hi, name="hi")

    def _encode_node(self, out: bytearray) -> None:
        out.append(self._tag)
        out.extend(struct.pack("<QQ", self.lo, self.hi))

    def _append_keys(self, out: list[int]) -> None:
        return


@dataclass(frozen=True, slots=True, init=False)
class Literal(SetExpr):
    """A canonical materialized set of ordinals."""

    ordinals: tuple[int, ...]
    _tag: ClassVar[int] = _TAG_LITERAL

    def __init__(self, *ordinals: int) -> None:
        checked = {
            _whole_number(value, name="ordinal", maximum=ORDINAL_LIMIT - 1) for value in ordinals
        }
        object.__setattr__(self, "ordinals", tuple(sorted(checked)))

    def _encode_node(self, out: bytearray) -> None:
        out.append(self._tag)
        out.extend(struct.pack("<I", len(self.ordinals)))
        for ordinal in self.ordinals:
            out.extend(struct.pack("<Q", ordinal))

    def _append_keys(self, out: list[int]) -> None:
        return


@dataclass(frozen=True, slots=True, init=False)
class And(SetExpr):
    operands: tuple[SetExpr, ...]
    _tag: ClassVar[int] = _TAG_AND

    def __init__(self, *operands: SetExpr) -> None:
        checked = tuple(_expression(value) for value in operands)
        if not checked:
            raise ValueError("AND must have at least one operand")
        object.__setattr__(self, "operands", checked)

    def _encode_node(self, out: bytearray) -> None:
        out.append(self._tag)
        out.extend(struct.pack("<H", len(self.operands)))
        for operand in self.operands:
            operand._encode_node(out)

    def _append_keys(self, out: list[int]) -> None:
        for operand in self.operands:
            operand._append_keys(out)


@dataclass(frozen=True, slots=True, init=False)
class Or(SetExpr):
    operands: tuple[SetExpr, ...]
    _tag: ClassVar[int] = _TAG_OR

    def __init__(self, *operands: SetExpr) -> None:
        checked = tuple(_expression(value) for value in operands)
        if not checked:
            raise ValueError("OR must have at least one operand")
        object.__setattr__(self, "operands", checked)

    def _encode_node(self, out: bytearray) -> None:
        out.append(self._tag)
        out.extend(struct.pack("<H", len(self.operands)))
        for operand in self.operands:
            operand._encode_node(out)

    def _append_keys(self, out: list[int]) -> None:
        for operand in self.operands:
            operand._append_keys(out)


@dataclass(frozen=True, slots=True)
class AndNot(SetExpr):
    left: SetExpr
    right: SetExpr
    _tag: ClassVar[int] = _TAG_AND_NOT

    def __post_init__(self) -> None:
        _expression(self.left)
        _expression(self.right)

    def _encode_node(self, out: bytearray) -> None:
        out.append(self._tag)
        self.left._encode_node(out)
        self.right._encode_node(out)

    def _append_keys(self, out: list[int]) -> None:
        self.left._append_keys(out)
        self.right._append_keys(out)


class VecSetExpr:
    """A fixed-arity vector of sets -- what a view produces.

    A separate base class from :class:`SetExpr` so that an ill-sorted tree is a
    ``TypeError`` at construction rather than a decode error at the server.
    Vectors do not nest: the element sort is a set, never another vector.
    """

    __slots__ = ()

    @property
    def arity(self) -> int:
        """How many elements. Always statically known, which makes indexing total."""
        raise NotImplementedError

    def _encode_node(self, out: bytearray) -> None:
        raise NotImplementedError

    def _append_keys(self, out: list[int]) -> None:
        raise NotImplementedError


def _vector(value: object) -> VecSetExpr:
    if not isinstance(value, VecSetExpr):
        raise TypeError(f"expected a vector of sets, not {type(value).__name__}")
    return value


@dataclass(frozen=True, slots=True)
class List(VecSetExpr):
    """A literal vector, ``[ a, b, c ]``."""

    elements: tuple[SetExpr, ...]
    _tag: ClassVar[int] = _TAG_LIST

    def __init__(self, *elements: SetExpr) -> None:
        if not elements:
            raise ValueError("a vector must have at least one element")
        for element in elements:
            _expression(element)
        object.__setattr__(self, "elements", tuple(elements))

    @property
    def arity(self) -> int:
        return len(self.elements)

    def _encode_node(self, out: bytearray) -> None:
        out.append(self._tag)
        out.extend(struct.pack("<H", len(self.elements)))
        for element in self.elements:
            element._encode_node(out)

    def _append_keys(self, out: list[int]) -> None:
        for element in self.elements:
            element._append_keys(out)


@dataclass(frozen=True, slots=True)
class View(VecSetExpr):
    """Read one set as ``view.sets`` constituents -- the curry direction."""

    input: SetExpr
    view: ViewSpec
    _tag: ClassVar[int] = _TAG_VIEW

    def __post_init__(self) -> None:
        _expression(self.input)
        if not isinstance(self.view, ViewSpec):
            raise TypeError("view must be a ViewSpec")

    @property
    def arity(self) -> int:
        return self.view.sets

    def _encode_node(self, out: bytearray) -> None:
        out.append(self._tag)
        self.view._encode_into(out)
        self.input._encode_node(out)

    def _append_keys(self, out: list[int]) -> None:
        self.input._append_keys(out)


@dataclass(frozen=True, slots=True)
class At(SetExpr):
    """One element of a vector, by zero-based index -- ``v[ i ]``.

    Total on a well-formed expression: arity is statically known, so an
    out-of-range index is refused here rather than discovered by the server.
    """

    vector: VecSetExpr
    index: int
    _tag: ClassVar[int] = _TAG_AT

    def __post_init__(self) -> None:
        _vector(self.vector)
        index = _u32(self.index, name="index")
        if index >= self.vector.arity:
            raise ValueError("index is at or above the vector's arity")

    def _encode_node(self, out: bytearray) -> None:
        out.append(self._tag)
        self.vector._encode_node(out)
        out.extend(struct.pack("<I", self.index))

    def _append_keys(self, out: list[int]) -> None:
        self.vector._append_keys(out)


@dataclass(frozen=True, slots=True)
class Fold(SetExpr):
    """Combine every element of a vector into one set."""

    vector: VecSetExpr
    op: FoldOp
    _tag: ClassVar[int] = _TAG_FOLD

    def __post_init__(self) -> None:
        _vector(self.vector)
        if not isinstance(self.op, FoldOp):
            raise TypeError("op must be a FoldOp")

    def _encode_node(self, out: bytearray) -> None:
        out.append(self._tag)
        self.vector._encode_node(out)
        out.append(int(self.op))

    def _append_keys(self, out: list[int]) -> None:
        self.vector._append_keys(out)


@dataclass(frozen=True, slots=True)
class Pack(SetExpr):
    """Pack a vector's elements into one set -- the uncurry direction."""

    vector: VecSetExpr
    view: ViewSpec
    _tag: ClassVar[int] = _TAG_PACK

    def __post_init__(self) -> None:
        _vector(self.vector)
        if not isinstance(self.view, ViewSpec):
            raise TypeError("view must be a ViewSpec")
        if self.vector.arity != self.view.sets:
            raise ValueError(
                f"packing {self.vector.arity} sets under a {self.view.sets}-set descriptor"
            )

    def _encode_node(self, out: bytearray) -> None:
        out.append(self._tag)
        self.view._encode_into(out)
        self.vector._encode_node(out)

    def _append_keys(self, out: list[int]) -> None:
        self.vector._append_keys(out)


@dataclass(frozen=True, slots=True)
class Expand(SetExpr):
    """Every constituent's slot, for each logical ordinal in the operand."""

    input: SetExpr
    view: ViewSpec
    _tag: ClassVar[int] = _TAG_EXPAND

    def __post_init__(self) -> None:
        _expression(self.input)
        if not isinstance(self.view, ViewSpec):
            raise TypeError("view must be a ViewSpec")

    def _encode_node(self, out: bytearray) -> None:
        out.append(self._tag)
        self.view._encode_into(out)
        self.input._encode_node(out)

    def _append_keys(self, out: list[int]) -> None:
        self.input._append_keys(out)


class IntExpr:
    """A single count or position."""

    __slots__ = ()

    def _encode_node(self, out: bytearray) -> None:
        raise NotImplementedError

    def _append_keys(self, out: list[int]) -> None:
        raise NotImplementedError


class BoolExpr:
    """A single truth value. Only ever a map body -- never a query result."""

    __slots__ = ()

    def _encode_node(self, out: bytearray) -> None:
        raise NotImplementedError

    def _append_keys(self, out: list[int]) -> None:
        raise NotImplementedError


class VecIntExpr:
    """One integer per constituent.

    Always exactly the vector's arity long. A reduction indexed by *ordinal*
    must stay sparse -- the logical universe is far too large to enumerate --
    so only one indexed by *constituent* may be dense, which is what makes this
    sort unambiguous about which axis was collapsed.
    """

    __slots__ = ()

    @property
    def arity(self) -> int:
        raise NotImplementedError

    def _encode_node(self, out: bytearray) -> None:
        raise NotImplementedError

    def _append_keys(self, out: list[int]) -> None:
        raise NotImplementedError

    def keys(self) -> tuple[int, ...]:
        """Every key this vector reads, in first-appearance order."""

        out: list[int] = []
        self._append_keys(out)
        return tuple(out)


class BigExpr:
    """A single arbitrary-precision signed integer.

    Distinct from :class:`IntExpr` rather than a widening of it. An ``IntExpr``
    is a count or a position -- a ``u64``, whose cost is bounded by node count
    alone. This sort's cost depends on how wide its values are, which is why it
    is the only sort with a width bound over it.
    """

    __slots__ = ()

    def _encode_node(self, out: bytearray) -> None:
        raise NotImplementedError

    def _append_keys(self, out: list[int]) -> None:
        raise NotImplementedError

    def width_bound(self) -> int:
        """An upper bound, in bits, on the value this expression can denote.

        Static: every source of width is known without evaluating anything, so
        an over-wide expression is refused before it is sent rather than after
        a server has tried to build it.
        """

        raise NotImplementedError

    def work_bound(self) -> int:
        """An upper bound on the limb operations evaluating this costs.

        Deliberately loose, and loose in one direction: schoolbook rather than
        Karatsuba, so it refuses work that would have been affordable and never
        admits work that would not.
        """

        raise NotImplementedError

    def keys(self) -> tuple[int, ...]:
        """Every key this expression reads, in first-appearance order."""

        out: list[int] = []
        self._append_keys(out)
        return tuple(out)


def _big(value: object) -> BigExpr:
    if not isinstance(value, BigExpr):
        raise TypeError(f"expected a big-integer expression, not {type(value).__name__}")
    if value.width_bound() > MAX_VALUE_BITS:
        raise ExpressionError(f"value is wider than {MAX_VALUE_BITS} bits")
    if value.work_bound() > MAX_WORK:
        raise ExpressionError(f"query asks for more than {MAX_WORK} limb operations")
    return value


def _bounded(node: BigExpr) -> None:
    """Refuse a node whose own result could exceed the width budget.

    Checking the operands is not enough, and that is the whole point of the
    bound: multiplication *adds* widths, so two operands that each fit can
    combine into one that does not. The Rust decoder makes the same check per
    node rather than at the root, for the same reason.
    """

    if node.width_bound() > MAX_VALUE_BITS:
        raise ExpressionError(f"value is wider than {MAX_VALUE_BITS} bits")
    if node.work_bound() > MAX_WORK:
        raise ExpressionError(f"query asks for more than {MAX_WORK} limb operations")


def _width(value: object, *, name: str) -> int:
    width = _u32(value, name=name)
    if width == 0:
        raise ExpressionError("a big-integer read has zero width")
    if width > MAX_VALUE_BITS:
        raise ExpressionError(f"value is wider than {MAX_VALUE_BITS} bits")
    return width


@dataclass(frozen=True, slots=True)
class BigLit(BigExpr):
    """A literal, as an ordinary Python ``int`` of any magnitude.

    Encoded sign-and-magnitude rather than two's complement, because a literal
    has no width to be negative in -- width is a reader's argument. The
    magnitude is little-endian and canonical: no trailing zero byte, and a
    negative zero cannot occur.
    """

    value: int

    def __post_init__(self) -> None:
        if not isinstance(self.value, int) or isinstance(self.value, bool):
            raise TypeError(f"expected an int, not {type(self.value).__name__}")
        if self.value.bit_length() > MAX_VALUE_BITS:
            raise ExpressionError(f"value is wider than {MAX_VALUE_BITS} bits")

    def _encode_node(self, out: bytearray) -> None:
        out.append(_TAG_BIG_LIT)
        magnitude = abs(self.value)
        body = magnitude.to_bytes((magnitude.bit_length() + 7) // 8, "little")
        out.append(1 if self.value < 0 else 0)
        out.extend(struct.pack("<I", len(body)))
        out.extend(body)

    def _append_keys(self, out: list[int]) -> None:
        return None

    def width_bound(self) -> int:
        return self.value.bit_length()

    def work_bound(self) -> int:
        return 0


@dataclass(frozen=True, slots=True)
class Widen(BigExpr):
    """A count as an arbitrary-precision integer: the inclusion of N into Z."""

    input: IntExpr

    def __post_init__(self) -> None:
        _integer(self.input)

    def _encode_node(self, out: bytearray) -> None:
        out.append(_TAG_BIG_WIDEN)
        self.input._encode_node(out)

    def _append_keys(self, out: list[int]) -> None:
        self.input._append_keys(out)

    def width_bound(self) -> int:
        return 64

    def work_bound(self) -> int:
        return 1


@dataclass(frozen=True, slots=True)
class ReadUint(BigExpr):
    """A set read as a magnitude, keeping its low ``width_bits`` bits.

    A set *is* an integer -- ordinal ``j`` carries the ``2**j`` term -- so this
    is a change of view, not a lookup. Reading narrower than the value occupies
    is exactly ``x % 2**width_bits``.
    """

    input: SetExpr
    width_bits: int

    def __post_init__(self) -> None:
        _expression(self.input)
        _width(self.width_bits, name="width_bits")

    def _encode_node(self, out: bytearray) -> None:
        out.append(_TAG_BIG_READ)
        out.extend(struct.pack("<I", self.width_bits))
        self.input._encode_node(out)

    def _append_keys(self, out: list[int]) -> None:
        self.input._append_keys(out)

    def width_bound(self) -> int:
        return self.width_bits

    def work_bound(self) -> int:
        return _limbs(self.width_bits)


@dataclass(frozen=True, slots=True)
class ReadInt(BigExpr):
    """A set read in two's complement over ``width_bits``.

    The top bit of the width is the sign, so the same stored set denotes a
    different number than :class:`ReadUint` gives for it. A separate node
    rather than a flag: nothing in the bits says which reading was meant, so an
    operation that sometimes did one and sometimes the other would have no
    error path, only a well-formed wrong answer.
    """

    input: SetExpr
    width_bits: int

    def __post_init__(self) -> None:
        _expression(self.input)
        _width(self.width_bits, name="width_bits")

    def _encode_node(self, out: bytearray) -> None:
        out.append(_TAG_BIG_READ_SIGNED)
        out.extend(struct.pack("<I", self.width_bits))
        self.input._encode_node(out)

    def _append_keys(self, out: list[int]) -> None:
        self.input._append_keys(out)

    def width_bound(self) -> int:
        return self.width_bits

    def work_bound(self) -> int:
        return _limbs(self.width_bits)


@dataclass(frozen=True, slots=True)
class Neg(BigExpr):
    """The additive inverse."""

    input: BigExpr

    def __post_init__(self) -> None:
        _big(self.input)

    def _encode_node(self, out: bytearray) -> None:
        out.append(_TAG_BIG_NEG)
        self.input._encode_node(out)

    def _append_keys(self, out: list[int]) -> None:
        self.input._append_keys(out)

    def width_bound(self) -> int:
        return self.input.width_bound()

    def work_bound(self) -> int:
        return self.input.work_bound() + _limbs(self.input.width_bound())


class _Binary(BigExpr):
    """Shared shape for the five binary arithmetic nodes."""

    __slots__ = ()
    _tag: ClassVar[int]

    def _encode_node(self, out: bytearray) -> None:
        out.append(self._tag)
        self.left._encode_node(out)  # type: ignore[attr-defined]
        self.right._encode_node(out)  # type: ignore[attr-defined]

    def _append_keys(self, out: list[int]) -> None:
        self.left._append_keys(out)  # type: ignore[attr-defined]
        self.right._append_keys(out)  # type: ignore[attr-defined]


@dataclass(frozen=True, slots=True)
class Add(_Binary):
    """Sum."""

    left: BigExpr
    right: BigExpr
    _tag: ClassVar[int] = _TAG_BIG_ADD

    def __post_init__(self) -> None:
        _big(self.left)
        _big(self.right)
        _bounded(self)

    def width_bound(self) -> int:
        return max(self.left.width_bound(), self.right.width_bound()) + 1

    def work_bound(self) -> int:
        return (
            self.left.work_bound()
            + self.right.work_bound()
            + max(_limbs(self.left.width_bound()), _limbs(self.right.width_bound()))
        )


@dataclass(frozen=True, slots=True)
class Sub(_Binary):
    """Difference. Total, which is the operation the signed sort exists for."""

    left: BigExpr
    right: BigExpr
    _tag: ClassVar[int] = _TAG_BIG_SUB

    def __post_init__(self) -> None:
        _big(self.left)
        _big(self.right)
        _bounded(self)

    def width_bound(self) -> int:
        return max(self.left.width_bound(), self.right.width_bound()) + 1

    def work_bound(self) -> int:
        return (
            self.left.work_bound()
            + self.right.work_bound()
            + max(_limbs(self.left.width_bound()), _limbs(self.right.width_bound()))
        )


@dataclass(frozen=True, slots=True)
class Mul(_Binary):
    """Product.

    The node that makes the width bound necessary: it adds the operands'
    widths, so a few nested products over wide reads describe a value no server
    should try to build.
    """

    left: BigExpr
    right: BigExpr
    _tag: ClassVar[int] = _TAG_BIG_MUL

    def __post_init__(self) -> None:
        _big(self.left)
        _big(self.right)
        _bounded(self)

    def width_bound(self) -> int:
        return self.left.width_bound() + self.right.width_bound()

    def work_bound(self) -> int:
        return (
            self.left.work_bound()
            + self.right.work_bound()
            + _limbs(self.left.width_bound()) * _limbs(self.right.width_bound())
        )


@dataclass(frozen=True, slots=True)
class Div(_Binary):
    """Quotient, truncating toward zero. A zero divisor is a server error."""

    left: BigExpr
    right: BigExpr
    _tag: ClassVar[int] = _TAG_BIG_DIV

    def __post_init__(self) -> None:
        _big(self.left)
        _big(self.right)

    def width_bound(self) -> int:
        return self.left.width_bound()

    def work_bound(self) -> int:
        return (
            self.left.work_bound()
            + self.right.work_bound()
            + _limbs(self.left.width_bound()) * _limbs(self.right.width_bound())
        )


@dataclass(frozen=True, slots=True)
class Rem(_Binary):
    """Remainder, carrying the sign of the dividend."""

    left: BigExpr
    right: BigExpr
    _tag: ClassVar[int] = _TAG_BIG_REM

    def __post_init__(self) -> None:
        _big(self.left)
        _big(self.right)

    def width_bound(self) -> int:
        return min(self.left.width_bound(), self.right.width_bound())

    def work_bound(self) -> int:
        return (
            self.left.work_bound()
            + self.right.work_bound()
            + _limbs(self.left.width_bound()) * _limbs(self.right.width_bound())
        )


@dataclass(frozen=True, slots=True)
class Truncate(BigExpr):
    """Wrap into a ``bits``-wide two's-complement field."""

    input: BigExpr
    bits: int

    def __post_init__(self) -> None:
        _big(self.input)
        bits = _u32(self.bits, name="bits")
        if bits > MAX_VALUE_BITS:
            raise ExpressionError(f"value is wider than {MAX_VALUE_BITS} bits")

    def _encode_node(self, out: bytearray) -> None:
        out.append(_TAG_BIG_TRUNCATE)
        out.extend(struct.pack("<I", self.bits))
        self.input._encode_node(out)

    def _append_keys(self, out: list[int]) -> None:
        self.input._append_keys(out)

    def width_bound(self) -> int:
        return min(self.input.width_bound(), self.bits)

    def work_bound(self) -> int:
        return self.input.work_bound() + _limbs(self.input.width_bound())


@dataclass(frozen=True, slots=True)
class Saturate(BigExpr):
    """Clamp into the same field :class:`Truncate` wraps into.

    One field, two overflow rules. They coincide only where nothing overflowed.
    """

    input: BigExpr
    bits: int

    def __post_init__(self) -> None:
        _big(self.input)
        bits = _u32(self.bits, name="bits")
        if bits > MAX_VALUE_BITS:
            raise ExpressionError(f"value is wider than {MAX_VALUE_BITS} bits")

    def _encode_node(self, out: bytearray) -> None:
        out.append(_TAG_BIG_SATURATE)
        out.extend(struct.pack("<I", self.bits))
        self.input._encode_node(out)

    def _append_keys(self, out: list[int]) -> None:
        self.input._append_keys(out)

    def width_bound(self) -> int:
        return min(self.input.width_bound(), self.bits)

    def work_bound(self) -> int:
        return self.input.work_bound() + _limbs(self.input.width_bound())


class VecBigExpr:
    """One arbitrary-precision integer per constituent.

    There is no unsigned counterpart, deliberately: sign is a property of the
    *reading* -- :class:`ReadUint` yields a magnitude, :class:`ReadInt` a two's
    complement of the same bits -- so a vector of unsigned values would be a
    sort whose only content is a promise the element's own node already makes.
    """

    __slots__ = ()

    @property
    def arity(self) -> int:
        raise NotImplementedError

    def element_bound(self) -> int:
        """An upper bound, in bits, on any one element."""

        raise NotImplementedError

    def result_bound(self) -> int:
        """An upper bound, in bits, on the *whole* result.

        ``arity * element width``, which is the quantity neither the
        constituent cap nor the per-value width bound covers: each caps one
        factor and the product is free.
        """

        return self.arity * self.element_bound()

    def work_bound(self) -> int:
        """An upper bound on the limb operations this vector costs."""

        raise NotImplementedError

    def _encode_node(self, out: bytearray) -> None:
        raise NotImplementedError

    def _append_keys(self, out: list[int]) -> None:
        raise NotImplementedError

    def keys(self) -> tuple[int, ...]:
        """Every key this vector reads, in first-appearance order."""

        out: list[int] = []
        self._append_keys(out)
        return tuple(out)


def _vec_big(value: object) -> VecBigExpr:
    if not isinstance(value, VecBigExpr):
        raise TypeError(f"expected a vector of big integers, not {type(value).__name__}")
    if value.result_bound() > MAX_RESULT_BITS:
        raise ExpressionError(f"result is larger than {MAX_RESULT_BITS} bits")
    if value.work_bound() > MAX_WORK:
        raise ExpressionError(f"query asks for more than {MAX_WORK} limb operations")
    return value


@dataclass(frozen=True, slots=True, init=False)
class BigList(VecBigExpr):
    """A literal vector of big integers."""

    elements: tuple[BigExpr, ...]

    def __init__(self, *elements: BigExpr) -> None:
        if not elements:
            raise ExpressionError("vector has no elements")
        for element in elements:
            _big(element)
        object.__setattr__(self, "elements", tuple(elements))
        _vec_big(self)

    @property
    def arity(self) -> int:
        return len(self.elements)

    def element_bound(self) -> int:
        return max((e.width_bound() for e in self.elements), default=0)

    def work_bound(self) -> int:
        return sum(e.work_bound() for e in self.elements)

    def _encode_node(self, out: bytearray) -> None:
        out.append(_TAG_BIG_LIST)
        out.extend(struct.pack("<H", len(self.elements)))
        for element in self.elements:
            element._encode_node(out)

    def _append_keys(self, out: list[int]) -> None:
        for element in self.elements:
            element._append_keys(out)


@dataclass(frozen=True, slots=True)
class MapBig(VecBigExpr):
    """A big-integer query applied to every element of a vector of sets.

    A map, not a fold, on the same terms as :class:`MapInt`.
    """

    vector: VecSetExpr
    body: BigExpr

    def __post_init__(self) -> None:
        _vector(self.vector)
        _big(self.body)
        _vec_big(self)

    @property
    def arity(self) -> int:
        return self.vector.arity

    def element_bound(self) -> int:
        return self.body.width_bound()

    def work_bound(self) -> int:
        # The body runs once per constituent.
        return self.arity * self.body.work_bound()

    def _encode_node(self, out: bytearray) -> None:
        out.append(_TAG_MAP_BIG)
        self.vector._encode_node(out)
        self.body._encode_node(out)

    def _append_keys(self, out: list[int]) -> None:
        self.vector._append_keys(out)
        self.body._append_keys(out)


@dataclass(frozen=True, slots=True)
class BigFold(BigExpr):
    """Reduce a vector of big integers to one.

    The transpose of :class:`MapBig`: a map keeps one value per constituent, a
    fold collapses them. The two are the marginals of the same matrix and do
    not determine each other.
    """

    vector: VecBigExpr
    op: BigFoldOp

    def __post_init__(self) -> None:
        _vec_big(self.vector)
        if not isinstance(self.op, BigFoldOp):
            raise TypeError(f"expected a BigFoldOp, not {type(self.op).__name__}")
        _bounded(self)

    def _encode_node(self, out: bytearray) -> None:
        out.append(_TAG_BIG_FOLD)
        out.append(int(self.op))
        self.vector._encode_node(out)

    def _append_keys(self, out: list[int]) -> None:
        self.vector._append_keys(out)

    def width_bound(self) -> int:
        """Only ``MUL`` grows with the arity.

        A sum of ``n`` values below ``2**w`` is below ``2**(w + bits(n))``, so
        addition costs a handful of bits. A product reaches ``n * w`` -- the
        same unbounded product the result bound exists for, except that here it
        lands in a single value.
        """

        arity = self.vector.arity
        widest = self.vector.element_bound()
        if self.op is BigFoldOp.ADD:
            return widest + arity.bit_length()
        if self.op is BigFoldOp.MUL:
            return widest * arity
        return widest

    def work_bound(self) -> int:
        arity = self.vector.arity
        element = _limbs(self.vector.element_bound())
        if self.op is BigFoldOp.MUL:
            # The accumulator grows as it goes, so the last multiply is against
            # the whole product.
            whole = arity * element
            reduce = whole * whole
        else:
            reduce = arity * element
        return self.vector.work_bound() + reduce


@dataclass(frozen=True, slots=True)
class PowMod(BigExpr):
    """``base ** exp mod modulus``, by Barrett reduction.

    The node the work bound exists for: the result is only as wide as the
    modulus, so nothing about its *size* is alarming, while its cost grows with
    the exponent's bit count times the square of the modulus's.

    A zero modulus has no residues and a negative exponent is a modular
    inverse, which the engine does not compute; both are server-side errors
    rather than answers. A negative base enters its residue class first, so the
    answer is always in ``[0, modulus)``.
    """

    base: BigExpr
    exp: BigExpr
    modulus: BigExpr

    def __post_init__(self) -> None:
        _big(self.base)
        _big(self.exp)
        _big(self.modulus)
        _bounded(self)

    def _encode_node(self, out: bytearray) -> None:
        out.append(_TAG_BIG_POW_MOD)
        self.base._encode_node(out)
        self.exp._encode_node(out)
        self.modulus._encode_node(out)

    def _append_keys(self, out: list[int]) -> None:
        self.base._append_keys(out)
        self.exp._append_keys(out)
        self.modulus._append_keys(out)

    def width_bound(self) -> int:
        # A residue is bounded by its modulus and by nothing else.
        return self.modulus.width_bound()

    def work_bound(self) -> int:
        m_limbs = _limbs(self.modulus.width_bound())
        # One squaring per exponent bit and one multiply per set bit, each
        # Barrett-reduced; the reduction is two multiplies.
        step = 4 * m_limbs * m_limbs
        return (
            self.base.work_bound()
            + self.exp.work_bound()
            + self.modulus.work_bound()
            + self.exp.width_bound() * step
        )


@overload
def big(value: int) -> BigLit: ...


@overload
def big(value: Sequence[int | BigExpr]) -> BigList: ...


def big(value: int | Sequence[int | BigExpr]) -> BigLit | BigList:
    """A big-integer literal, or a literal vector of them.

    Mirrors the query language exactly: ``big(1)`` is a single value and
    ``big([1, 2, 3])`` is a vector of three. The same spelling carries the sort
    in both, which is the point -- a bare list is a vector of *sets*
    everywhere, so a vector of big integers says so where it is written.
    """

    if isinstance(value, bool) or not isinstance(value, (int, Sequence)):
        raise TypeError(f"expected an int or a sequence, not {type(value).__name__}")
    if isinstance(value, int):
        return BigLit(value)
    return BigList(*(e if isinstance(e, BigExpr) else BigLit(e) for e in value))


def _integer(value: object) -> IntExpr:
    if not isinstance(value, IntExpr):
        raise TypeError(f"expected an integer expression, not {type(value).__name__}")
    return value


@dataclass(frozen=True, slots=True)
class IntLit(IntExpr):
    """A literal."""

    value: int
    _tag: ClassVar[int] = _TAG_INT_LIT

    def __post_init__(self) -> None:
        _u64(self.value, name="value")

    def _encode_node(self, out: bytearray) -> None:
        out.append(self._tag)
        out.extend(struct.pack("<Q", self.value))

    def _append_keys(self, out: list[int]) -> None:
        return None


@dataclass(frozen=True, slots=True)
class Cardinality(IntExpr):
    """How many ordinals a set holds."""

    input: SetExpr
    _tag: ClassVar[int] = _TAG_CARDINALITY

    def __post_init__(self) -> None:
        _expression(self.input)

    def _encode_node(self, out: bytearray) -> None:
        out.append(self._tag)
        self.input._encode_node(out)

    def _append_keys(self, out: list[int]) -> None:
        self.input._append_keys(out)


@dataclass(frozen=True, slots=True)
class Rank(IntExpr):
    """How many of a set's ordinals are strictly below a position."""

    input: SetExpr
    position: int
    _tag: ClassVar[int] = _TAG_RANK

    def __post_init__(self) -> None:
        _expression(self.input)
        _u64(self.position, name="position")

    def _encode_node(self, out: bytearray) -> None:
        out.append(self._tag)
        out.extend(struct.pack("<Q", self.position))
        self.input._encode_node(out)

    def _append_keys(self, out: list[int]) -> None:
        self.input._append_keys(out)


@dataclass(frozen=True, slots=True)
class IntAt(IntExpr):
    """One element of a vector of integers, by zero-based index."""

    vector: VecIntExpr
    index: int
    _tag: ClassVar[int] = _TAG_INT_AT

    def __post_init__(self) -> None:
        if not isinstance(self.vector, VecIntExpr):
            raise TypeError("vector must be a VecIntExpr")
        index = _u32(self.index, name="index")
        if index >= self.vector.arity:
            raise ValueError("index is at or above the vector's arity")

    def _encode_node(self, out: bytearray) -> None:
        out.append(self._tag)
        self.vector._encode_node(out)
        out.extend(struct.pack("<I", self.index))

    def _append_keys(self, out: list[int]) -> None:
        self.vector._append_keys(out)


@dataclass(frozen=True, slots=True)
class Contains(BoolExpr):
    """Whether a set holds one ordinal."""

    input: SetExpr
    ordinal: int
    _tag: ClassVar[int] = _TAG_CONTAINS

    def __post_init__(self) -> None:
        _expression(self.input)
        _u64(self.ordinal, name="ordinal")

    def _encode_node(self, out: bytearray) -> None:
        out.append(self._tag)
        out.extend(struct.pack("<Q", self.ordinal))
        self.input._encode_node(out)

    def _append_keys(self, out: list[int]) -> None:
        self.input._append_keys(out)


@dataclass(frozen=True, slots=True)
class IntList(VecIntExpr):
    """A literal vector of integers."""

    elements: tuple[IntExpr, ...]
    _tag: ClassVar[int] = _TAG_INT_LIST

    def __init__(self, *elements: IntExpr) -> None:
        if not elements:
            raise ValueError("a vector must have at least one element")
        for element in elements:
            _integer(element)
        object.__setattr__(self, "elements", tuple(elements))

    @property
    def arity(self) -> int:
        return len(self.elements)

    def _encode_node(self, out: bytearray) -> None:
        out.append(self._tag)
        out.extend(struct.pack("<H", len(self.elements)))
        for element in self.elements:
            element._encode_node(out)

    def _append_keys(self, out: list[int]) -> None:
        for element in self.elements:
            element._append_keys(out)


@dataclass(frozen=True, slots=True)
class MapInt(VecIntExpr):
    """A scalar query applied to every element -- the facet histogram.

    This is a **map, not a fold**: it does not combine the elements, it applies
    a query to each. ``MapInt(v, Cardinality(Hole()))`` is the per-constituent
    cardinality; ``Fold(v, FoldOp.OR)`` is the union. The two are the marginals
    of the same matrix and do not determine each other.
    """

    vector: VecSetExpr
    body: IntExpr
    _tag: ClassVar[int] = _TAG_MAP_INT

    def __post_init__(self) -> None:
        _vector(self.vector)
        _integer(self.body)

    @property
    def arity(self) -> int:
        return self.vector.arity

    def _encode_node(self, out: bytearray) -> None:
        out.append(self._tag)
        self.vector._encode_node(out)
        self.body._encode_node(out)

    def _append_keys(self, out: list[int]) -> None:
        self.vector._append_keys(out)
        self.body._append_keys(out)


@dataclass(frozen=True, slots=True)
class Hole(SetExpr):
    """``_`` -- the element of the enclosing :class:`Map` body.

    The language has no variables, so this is a hole rather than a name: no
    binder, no scope, no closure. **Every ``_`` in one body denotes the same
    element**, which is the opposite of the glyph's most famous precedent --
    in Scala ``_ + _`` is a binary function with two distinct parameters.
    """

    _tag: ClassVar[int] = _TAG_HOLE

    def _encode_node(self, out: bytearray) -> None:
        out.append(self._tag)

    def _append_keys(self, out: list[int]) -> None:
        return None


@dataclass(frozen=True, slots=True)
class Select(SetExpr):
    """The ``n``-th smallest ordinal, as a singleton -- or empty if there is none.

    Returns a **set** rather than an integer because it is genuinely partial:
    no type knows a set's cardinality, and an empty set says "no such ordinal"
    without a reserved value a caller could forget to check.
    """

    input: SetExpr
    index: int
    _tag: ClassVar[int] = _TAG_SELECT

    def __post_init__(self) -> None:
        _expression(self.input)
        _u64(self.index, name="index")

    def _encode_node(self, out: bytearray) -> None:
        out.append(self._tag)
        out.extend(struct.pack("<Q", self.index))
        self.input._encode_node(out)

    def _append_keys(self, out: list[int]) -> None:
        self.input._append_keys(out)


@dataclass(frozen=True, slots=True)
class Map(VecSetExpr):
    """A set-valued query applied to every element -- ``map(v, and(_, q))``."""

    vector: VecSetExpr
    body: SetExpr
    _tag: ClassVar[int] = _TAG_MAP_SET

    def __post_init__(self) -> None:
        _vector(self.vector)
        _expression(self.body)

    @property
    def arity(self) -> int:
        return self.vector.arity

    def _encode_node(self, out: bytearray) -> None:
        out.append(self._tag)
        self.vector._encode_node(out)
        self.body._encode_node(out)

    def _append_keys(self, out: list[int]) -> None:
        self.vector._append_keys(out)
        self.body._append_keys(out)


@dataclass(frozen=True, slots=True)
class MapBool(SetExpr):
    """Which constituents satisfy a boolean body.

    The result is one truth value per constituent, which **is** a subset of the
    constituent indices -- so this denotes a set, not a vector sort.
    """

    vector: VecSetExpr
    body: BoolExpr
    _tag: ClassVar[int] = _TAG_MAP_BOOL

    def __post_init__(self) -> None:
        _vector(self.vector)
        if not isinstance(self.body, BoolExpr):
            raise TypeError("body must be a BoolExpr")

    def _encode_node(self, out: bytearray) -> None:
        out.append(self._tag)
        self.vector._encode_node(out)
        self.body._encode_node(out)

    def _append_keys(self, out: list[int]) -> None:
        self.vector._append_keys(out)
        self.body._append_keys(out)


@dataclass(frozen=True, slots=True)
class AnyExpr:
    """A whole query, at whichever sort it denotes.

    A query is usually a set. ``MapInt`` denotes one integer per constituent --
    a facet histogram -- and those are the two shapes a server can return. A
    vector of *sets* is not among them: it has no single answer shape, and it
    always reaches a query result through a fold, a pack, or an index.
    """

    expression: SetExpr | VecIntExpr | BigExpr | VecBigExpr

    def __post_init__(self) -> None:
        if not isinstance(self.expression, (SetExpr, VecIntExpr, BigExpr, VecBigExpr)):
            raise TypeError("a query must be a set, a vector of integers, or a big integer")
        if isinstance(self.expression, BigExpr):
            _big(self.expression)
        if isinstance(self.expression, VecBigExpr):
            _vec_big(self.expression)

    @property
    def sort(self) -> str:
        if isinstance(self.expression, SetExpr):
            return SORT_SET
        if isinstance(self.expression, BigExpr):
            return SORT_BIG
        if isinstance(self.expression, VecBigExpr):
            return SORT_VEC_BIG
        return SORT_VEC_INT

    def encode(self) -> bytes:
        """Encode with the ``YSNX`` header, exactly as :meth:`SetExpr.encode` does."""

        if isinstance(self.expression, SetExpr):
            return self.expression.encode()
        if isinstance(self.expression, VecIntExpr):
            _check_shape(self.expression)
        out = bytearray(_HEADER)
        self.expression._encode_node(out)
        return bytes(out)

    @classmethod
    def decode(cls, payload: bytes | bytearray | memoryview) -> AnyExpr:
        """Decode a whole payload at whichever sort it carries."""

        raw = bytes(payload)
        if len(raw) < len(_HEADER):
            raise ExpressionError("expression ended in its header")
        if raw[:4] != MAGIC:
            raise ExpressionError("not a yesnodb expression")
        if raw[4] != VERSION or raw[5] != 0:
            raise ExpressionError(f"unsupported expression version {raw[4]}")
        cursor = _Cursor(raw[len(_HEADER) :])
        # The leading tag decides the sort, and the decoders reject each
        # other's tags, so this dispatch cannot silently pick the wrong one.
        # Through the one sort table, for the reason it exists: a per-sort list
        # here is a second enumeration of the same fact waiting to disagree.
        leading = cursor.payload[0] if cursor.payload else None
        leading_sort = _TAG_SORTS.get(leading) if leading is not None else None
        expression: SetExpr | VecIntExpr | BigExpr | VecBigExpr
        if leading_sort == SORT_VEC_INT:
            expression = cursor.int_vector(0)
        elif leading_sort == SORT_BIG:
            expression = cursor.big(0)
        elif leading_sort == SORT_VEC_BIG:
            expression = cursor.big_vector(0)
        else:
            expression = cursor.expression(0)
        if cursor.offset != len(cursor.payload):
            raise ExpressionError("trailing bytes after expression")
        return cls(expression)


@dataclass(frozen=True, slots=True)
class QueryRequest:
    """An expression and an optional exact database version to evaluate.

    The expression is at **whichever sort it denotes**. It was a set alone
    until the carrier was widened, which meant the multi-sorted language had no
    way to reach a server: a facet histogram could be built and never sent.
    """

    expression: SetExpr | VecIntExpr | BigExpr | VecBigExpr
    version: int | None = None

    def __post_init__(self) -> None:
        if not isinstance(self.expression, (SetExpr, VecIntExpr, BigExpr, VecBigExpr)):
            raise TypeError("a query must be a set, a vector of integers, or a big integer")
        if isinstance(self.expression, BigExpr):
            _big(self.expression)
        if isinstance(self.expression, VecBigExpr):
            _vec_big(self.expression)
        if self.version is not None:
            _u64(self.version, name="version")

    @classmethod
    def current(cls, expression: SetExpr | VecIntExpr | BigExpr | VecBigExpr) -> QueryRequest:
        return cls(expression)

    @classmethod
    def at(cls, expression: SetExpr | VecIntExpr | BigExpr, version: int) -> QueryRequest:
        return cls(expression, version)

    def encode(self) -> bytes:
        """Encode this request as a complete v1 descriptor command."""

        pinned = self.version is not None
        version = self.version if self.version is not None else 0
        return (
            QUERY_MAGIC
            + struct.pack("<BBQ", QUERY_VERSION, _QUERY_FLAG_PINNED if pinned else 0, version)
            # Through `AnyExpr`, which is the one encoder that knows every
            # sort: only `SetExpr` carries an `encode` of its own.
            + AnyExpr(self.expression).encode()
        )

    @classmethod
    def decode(cls, payload: bytes | bytearray | memoryview) -> QueryRequest:
        """Decode a complete v1 version-aware query request."""

        raw = bytes(payload)
        if len(raw) < _QUERY_HEADER_LEN:
            raise ExpressionError("query request ended in its header")
        if raw[:4] != QUERY_MAGIC:
            raise ExpressionError("not a yesnodb query request")
        wire_version, flags, version = struct.unpack("<BBQ", raw[4:_QUERY_HEADER_LEN])
        if wire_version != QUERY_VERSION or flags & ~_QUERY_FLAG_PINNED:
            raise ExpressionError(f"unsupported query request version {wire_version}")
        pinned = flags & _QUERY_FLAG_PINNED != 0
        if not pinned and version != 0:
            raise ExpressionError("an unpinned query request has a non-zero version")
        expression = AnyExpr.decode(raw[_QUERY_HEADER_LEN:]).expression
        return cls(expression, version if pinned else None)

    @staticmethod
    def looks_like_request(payload: bytes | bytearray | memoryview) -> bool:
        raw = bytes(payload)
        return len(raw) >= _QUERY_HEADER_LEN and raw[:4] == QUERY_MAGIC


def xor(left: SetExpr, right: SetExpr) -> SetExpr:
    """Build XOR from v1-compatible nodes."""

    left = _expression(left)
    right = _expression(right)
    return AndNot(Or(left, right), And(left, right))


def complement(expression: SetExpr) -> SetExpr:
    """Complement an expression over the valid ordinal universe."""

    return AndNot(Range(0, ORDINAL_LIMIT), _expression(expression))


def _check_shape(root: SetExpr | VecIntExpr) -> None:
    nodes = 0

    def visit(expression: SetExpr, depth: int) -> None:
        nonlocal nodes
        if depth > MAX_DEPTH:
            raise ExpressionError(f"expression nested deeper than {MAX_DEPTH}")
        nodes += 1
        if nodes > MAX_NODES:
            raise ExpressionError(f"expression has more than {MAX_NODES} nodes")
        if isinstance(expression, (And, Or)):
            if len(expression.operands) > (1 << 16) - 1:
                raise ExpressionError("junction has more than 65535 operands")
            for operand in expression.operands:
                visit(operand, depth + 1)
        elif isinstance(expression, AndNot):
            visit(expression.left, depth + 1)
            visit(expression.right, depth + 1)
        elif isinstance(expression, Expand):
            visit(expression.input, depth + 1)
        elif isinstance(expression, (At, Fold, Pack, MapBool)):
            visit_vector(expression.vector, depth + 1)
        elif isinstance(expression, Select):
            visit(expression.input, depth + 1)

    def visit_vector(vector: VecSetExpr, depth: int) -> None:
        nonlocal nodes
        if depth > MAX_DEPTH:
            raise ExpressionError(f"expression nested deeper than {MAX_DEPTH}")
        nodes += 1
        if nodes > MAX_NODES:
            raise ExpressionError(f"expression has more than {MAX_NODES} nodes")
        if isinstance(vector, List):
            if len(vector.elements) > (1 << 16) - 1:
                raise ExpressionError("vector has more than 65535 elements")
            for element in vector.elements:
                visit(element, depth + 1)
        elif isinstance(vector, View):
            visit(vector.input, depth + 1)
        elif isinstance(vector, Map):
            visit_vector(vector.vector, depth + 1)
            visit(vector.body, depth + 1)

    def visit_int_vector(vector: VecIntExpr, depth: int) -> None:
        nonlocal nodes
        if depth > MAX_DEPTH:
            raise ExpressionError(f"expression nested deeper than {MAX_DEPTH}")
        nodes += 1
        if nodes > MAX_NODES:
            raise ExpressionError(f"expression has more than {MAX_NODES} nodes")
        if isinstance(vector, IntList):
            if len(vector.elements) > (1 << 16) - 1:
                raise ExpressionError("vector has more than 65535 elements")
        elif isinstance(vector, MapInt):
            visit_vector(vector.vector, depth + 1)

    if isinstance(root, VecIntExpr):
        visit_int_vector(root, 0)
        return
    visit(root, 0)


class _Cursor:
    def __init__(self, payload: bytes) -> None:
        self.payload = payload
        self.offset = 0
        self.nodes = 0
        # Whether decoding is inside a map body. A bool rather than a counter
        # because nesting is *refused*, not tracked, and it is set only while a
        # body is decoded -- so a map in a vector position, which is sequential
        # rather than nested, is unaffected.
        self.in_map = False

    def take(self, size: int) -> bytes:
        end = self.offset + size
        if end > len(self.payload):
            raise ExpressionError("expression ended mid-node")
        value = self.payload[self.offset : end]
        self.offset = end
        return value

    def unpack(self, fmt: str) -> tuple[int, ...]:
        size = struct.calcsize(fmt)
        return struct.unpack(fmt, self.take(size))

    def view(self) -> ViewSpec:
        sets, layout, stride = self.unpack("<IBQ")
        try:
            layout_value = ViewLayout(layout)
        except ValueError as error:
            raise ExpressionError(f"unknown view layout {layout}") from error
        try:
            return ViewSpec(sets, layout_value, stride)
        except (TypeError, ValueError) as error:
            raise ExpressionError(str(error)) from error

    def expression(self, depth: int) -> SetExpr:
        if depth > MAX_DEPTH:
            raise ExpressionError(f"expression nested deeper than {MAX_DEPTH}")
        self.nodes += 1
        if self.nodes > MAX_NODES:
            raise ExpressionError(f"expression has more than {MAX_NODES} nodes")
        (tag,) = self.unpack("<B")

        if tag == _TAG_EMPTY:
            return Empty()
        if tag == _TAG_KEY:
            return Key(self.unpack("<Q")[0])
        if tag == _TAG_RANGE:
            lo, hi = self.unpack("<QQ")
            return Range(lo, hi)
        if tag == _TAG_LITERAL:
            (count,) = self.unpack("<I")
            raw = self.take(count * 8)
            ordinals = tuple(value[0] for value in struct.iter_unpack("<Q", raw))
            if any(ordinal == ORDINAL_LIMIT for ordinal in ordinals):
                raise ExpressionError(f"{ORDINAL_LIMIT} is outside the ordinal universe")
            if any(left >= right for left, right in pairwise(ordinals)):
                raise ExpressionError("ordinal-set literal is not strictly ascending and unique")
            return Literal(*ordinals)
        if tag in (_TAG_AND, _TAG_OR):
            (count,) = self.unpack("<H")
            if count == 0:
                raise ExpressionError("AND/OR with no operands")
            if self.nodes + count > MAX_NODES:
                raise ExpressionError(f"expression has more than {MAX_NODES} nodes")
            operands = tuple(self.expression(depth + 1) for _ in range(count))
            return And(*operands) if tag == _TAG_AND else Or(*operands)
        if tag == _TAG_AND_NOT:
            return AndNot(self.expression(depth + 1), self.expression(depth + 1))
        if tag == _TAG_AT:
            vector = self.vector(depth + 1)
            index = self.unpack("<I")[0]
            try:
                return At(vector, index)
            except (TypeError, ValueError) as error:
                raise ExpressionError(str(error)) from error
        if tag == _TAG_FOLD:
            vector = self.vector(depth + 1)
            (op,) = self.unpack("<B")
            try:
                op_value = FoldOp(op)
            except ValueError as error:
                raise ExpressionError(f"unknown fold operator {op}") from error
            return Fold(vector, op_value)
        if tag == _TAG_PACK:
            view = self.view()
            vector = self.vector(depth + 1)
            try:
                return Pack(vector, view)
            except (TypeError, ValueError) as error:
                raise ExpressionError(str(error)) from error
        if tag == _TAG_EXPAND:
            view = self.view()
            return Expand(self.expression(depth + 1), view)
        if tag == _TAG_HOLE:
            if not self.in_map:
                raise ExpressionError("`_` outside a map body")
            return Hole()
        if tag == _TAG_SELECT:
            index = self.unpack("<Q")[0]
            return Select(self.expression(depth + 1), index)
        if tag == _TAG_MAP_BOOL:
            vector = self.vector(depth + 1)
            return MapBool(vector, self.body(depth + 1, _Cursor.bool_expression))
        raise _misplaced(SORT_SET, tag)

    def body(self, depth: int, decode):  # type: ignore[no-untyped-def]
        """Decode a map body, the only place a hole is legal.

        Refuses a nested body outright and restores the flag afterwards, so a
        map in a *vector* position -- sequential rather than nested -- decodes.
        """

        if self.in_map:
            raise ExpressionError("a map body may not contain a map")
        self.in_map = True
        try:
            return decode(self, depth)
        finally:
            self.in_map = False

    def int_expression(self, depth: int) -> IntExpr:
        if depth > MAX_DEPTH:
            raise ExpressionError(f"expression nested deeper than {MAX_DEPTH}")
        self.nodes += 1
        if self.nodes > MAX_NODES:
            raise ExpressionError(f"expression has more than {MAX_NODES} nodes")
        (tag,) = self.unpack("<B")
        if tag == _TAG_INT_LIT:
            return IntLit(self.unpack("<Q")[0])
        if tag == _TAG_CARDINALITY:
            return Cardinality(self.expression(depth + 1))
        if tag == _TAG_RANK:
            position = self.unpack("<Q")[0]
            return Rank(self.expression(depth + 1), position)
        if tag == _TAG_INT_AT:
            vector = self.int_vector(depth + 1)
            index = self.unpack("<I")[0]
            try:
                return IntAt(vector, index)
            except (TypeError, ValueError) as error:
                raise ExpressionError(str(error)) from error
        raise _misplaced(SORT_INT, tag)

    def bool_expression(self, depth: int) -> BoolExpr:
        if depth > MAX_DEPTH:
            raise ExpressionError(f"expression nested deeper than {MAX_DEPTH}")
        self.nodes += 1
        if self.nodes > MAX_NODES:
            raise ExpressionError(f"expression has more than {MAX_NODES} nodes")
        (tag,) = self.unpack("<B")
        if tag == _TAG_CONTAINS:
            ordinal = self.unpack("<Q")[0]
            return Contains(self.expression(depth + 1), ordinal)
        raise _misplaced(SORT_BOOL, tag)

    def big(self, depth: int) -> BigExpr:
        if depth > MAX_DEPTH:
            raise ExpressionError(f"expression nested deeper than {MAX_DEPTH}")
        self.nodes += 1
        if self.nodes > MAX_NODES:
            raise ExpressionError(f"expression has more than {MAX_NODES} nodes")
        (tag,) = self.unpack("<B")

        if tag == _TAG_BIG_LIT:
            (sign,) = self.unpack("<B")
            if sign not in (0, 1):
                raise ExpressionError(f"unknown sign byte {sign}")
            (length,) = self.unpack("<I")
            # Checked before the bytes are taken, so an over-wide length is
            # refused as the amplification it is rather than as a truncation.
            if length * 8 > MAX_VALUE_BITS:
                raise ExpressionError(f"value is wider than {MAX_VALUE_BITS} bits")
            body = self.take(length)
            if body and body[-1] == 0:
                raise ExpressionError("big-integer literal is not canonical")
            magnitude = int.from_bytes(body, "little")
            if sign == 1 and magnitude == 0:
                raise ExpressionError("big-integer literal is not canonical")
            return BigLit(-magnitude if sign else magnitude)
        if tag == _TAG_BIG_WIDEN:
            return Widen(self.int_expression(depth + 1))
        if tag in (_TAG_BIG_READ, _TAG_BIG_READ_SIGNED):
            (width,) = self.unpack("<I")
            if width == 0:
                raise ExpressionError("a big-integer read has zero width")
            if width > MAX_VALUE_BITS:
                raise ExpressionError(f"value is wider than {MAX_VALUE_BITS} bits")
            operand = self.expression(depth + 1)
            return ReadUint(operand, width) if tag == _TAG_BIG_READ else ReadInt(operand, width)
        if tag == _TAG_BIG_NEG:
            return Neg(self.big(depth + 1))
        if tag == _TAG_BIG_POW_MOD:
            base = self.big(depth + 1)
            exp = self.big(depth + 1)
            return PowMod(base, exp, self.big(depth + 1))
        if tag == _TAG_BIG_FOLD:
            (raw,) = self.unpack("<B")
            try:
                op = BigFoldOp(raw)
            except ValueError as error:
                raise ExpressionError(f"unknown fold operator {raw}") from error
            return BigFold(self.big_vector(depth + 1), op)
        binary = {
            _TAG_BIG_ADD: Add,
            _TAG_BIG_SUB: Sub,
            _TAG_BIG_MUL: Mul,
            _TAG_BIG_DIV: Div,
            _TAG_BIG_REM: Rem,
        }.get(tag)
        if binary is not None:
            left = self.big(depth + 1)
            right = self.big(depth + 1)
            return binary(left, right)
        if tag in (_TAG_BIG_TRUNCATE, _TAG_BIG_SATURATE):
            # A zero width is meaningful here, unlike on a read: wrapping or
            # clamping into an empty field is zero, which is a value.
            (bits,) = self.unpack("<I")
            if bits > MAX_VALUE_BITS:
                raise ExpressionError(f"value is wider than {MAX_VALUE_BITS} bits")
            narrowed = self.big(depth + 1)
            return (
                Truncate(narrowed, bits) if tag == _TAG_BIG_TRUNCATE else Saturate(narrowed, bits)
            )
        raise _misplaced(SORT_BIG, tag)

    def big_vector(self, depth: int) -> VecBigExpr:
        if depth > MAX_DEPTH:
            raise ExpressionError(f"expression nested deeper than {MAX_DEPTH}")
        self.nodes += 1
        if self.nodes > MAX_NODES:
            raise ExpressionError(f"expression has more than {MAX_NODES} nodes")
        (tag,) = self.unpack("<B")
        if tag == _TAG_BIG_LIST:
            (count,) = self.unpack("<H")
            if count == 0:
                raise ExpressionError("vector has no elements")
            if self.nodes + count > MAX_NODES:
                raise ExpressionError(f"expression has more than {MAX_NODES} nodes")
            return BigList(*(self.big(depth + 1) for _ in range(count)))
        if tag == _TAG_MAP_BIG:
            vector = self.vector(depth + 1)
            return MapBig(vector, self.body(depth + 1, _Cursor.big))
        raise _misplaced(SORT_VEC_BIG, tag)

    def int_vector(self, depth: int) -> VecIntExpr:
        if depth > MAX_DEPTH:
            raise ExpressionError(f"expression nested deeper than {MAX_DEPTH}")
        self.nodes += 1
        if self.nodes > MAX_NODES:
            raise ExpressionError(f"expression has more than {MAX_NODES} nodes")
        (tag,) = self.unpack("<B")
        if tag == _TAG_INT_LIST:
            (count,) = self.unpack("<H")
            if count == 0:
                raise ExpressionError("vector has no elements")
            if self.nodes + count > MAX_NODES:
                raise ExpressionError(f"expression has more than {MAX_NODES} nodes")
            return IntList(*(self.int_expression(depth + 1) for _ in range(count)))
        if tag == _TAG_MAP_INT:
            vector = self.vector(depth + 1)
            return MapInt(vector, self.body(depth + 1, _Cursor.int_expression))
        raise _misplaced(SORT_VEC_INT, tag)

    def vector(self, depth: int) -> VecSetExpr:
        """Decode a vector-sorted node.

        The mirror of :meth:`expression`, and the reason both tag ranges share
        one space: a set-sorted tag arriving here is a sort error naming both
        sides, not a reinterpretation of whatever that byte means here.
        """

        if depth > MAX_DEPTH:
            raise ExpressionError(f"expression nested deeper than {MAX_DEPTH}")
        self.nodes += 1
        if self.nodes > MAX_NODES:
            raise ExpressionError(f"expression has more than {MAX_NODES} nodes")
        (tag,) = self.unpack("<B")
        if tag == _TAG_LIST:
            (count,) = self.unpack("<H")
            if count == 0:
                raise ExpressionError("vector has no elements")
            if self.nodes + count > MAX_NODES:
                raise ExpressionError(f"expression has more than {MAX_NODES} nodes")
            return List(*(self.expression(depth + 1) for _ in range(count)))
        if tag == _TAG_VIEW:
            view = self.view()
            return View(self.expression(depth + 1), view)
        if tag == _TAG_MAP_SET:
            vector = self.vector(depth + 1)
            return Map(vector, self.body(depth + 1, _Cursor.expression))
        raise _misplaced(SORT_VEC_SET, tag)
