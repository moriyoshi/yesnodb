"""The dependency-free yesnodb set-expression wire format.

The format mirrors the Rust ``yesno-wire`` crate. Expressions are immutable,
strictly checked before encoding, and decoded with explicit depth and node
limits because descriptors received from a network are untrusted bytes.
"""

from __future__ import annotations

import struct
from dataclasses import dataclass
from enum import IntEnum
from itertools import pairwise
from typing import ClassVar

from .errors import ExpressionError

MAGIC = b"YSNX"
VERSION = 1
QUERY_MAGIC = b"YSNQ"
QUERY_VERSION = 1
MAX_DEPTH = 32
MAX_NODES = 4096
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
_TAG_VIEW_SELECT = 6
_TAG_VIEW_FOLD = 7
_TAG_VIEW_EXPAND = 8
_TAG_LITERAL = 9


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


class ViewReduce(IntEnum):
    """Reduction across all constituents of a packed view."""

    ANY = 0
    ALL = 1
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


@dataclass(frozen=True, slots=True)
class ViewSelect(SetExpr):
    key: int
    view: ViewSpec
    set_index: int
    _tag: ClassVar[int] = _TAG_VIEW_SELECT

    def __post_init__(self) -> None:
        _u64(self.key, name="key")
        if not isinstance(self.view, ViewSpec):
            raise TypeError("view must be a ViewSpec")
        set_index = _u32(self.set_index, name="set_index")
        if set_index >= self.view.sets:
            raise ValueError("view constituent is outside the descriptor")

    def _encode_node(self, out: bytearray) -> None:
        out.append(self._tag)
        out.extend(struct.pack("<Q", self.key))
        self.view._encode_into(out)
        out.extend(struct.pack("<I", self.set_index))

    def _append_keys(self, out: list[int]) -> None:
        out.append(self.key)


@dataclass(frozen=True, slots=True)
class ViewFold(SetExpr):
    key: int
    view: ViewSpec
    reduce: ViewReduce
    _tag: ClassVar[int] = _TAG_VIEW_FOLD

    def __post_init__(self) -> None:
        _u64(self.key, name="key")
        if not isinstance(self.view, ViewSpec):
            raise TypeError("view must be a ViewSpec")
        if not isinstance(self.reduce, ViewReduce):
            raise TypeError("reduce must be a ViewReduce")

    def _encode_node(self, out: bytearray) -> None:
        out.append(self._tag)
        out.extend(struct.pack("<Q", self.key))
        self.view._encode_into(out)
        out.append(int(self.reduce))

    def _append_keys(self, out: list[int]) -> None:
        out.append(self.key)


@dataclass(frozen=True, slots=True)
class ViewExpand(SetExpr):
    input: SetExpr
    view: ViewSpec
    _tag: ClassVar[int] = _TAG_VIEW_EXPAND

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


@dataclass(frozen=True, slots=True)
class QueryRequest:
    """An expression and an optional exact database version to evaluate."""

    expression: SetExpr
    version: int | None = None

    def __post_init__(self) -> None:
        _expression(self.expression)
        if self.version is not None:
            _u64(self.version, name="version")

    @classmethod
    def current(cls, expression: SetExpr) -> QueryRequest:
        return cls(expression)

    @classmethod
    def at(cls, expression: SetExpr, version: int) -> QueryRequest:
        return cls(expression, version)

    def encode(self) -> bytes:
        """Encode this request as a complete v1 descriptor command."""

        pinned = self.version is not None
        version = self.version if self.version is not None else 0
        return (
            QUERY_MAGIC
            + struct.pack("<BBQ", QUERY_VERSION, _QUERY_FLAG_PINNED if pinned else 0, version)
            + self.expression.encode()
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
        expression = SetExpr.decode(raw[_QUERY_HEADER_LEN:])
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


def _check_shape(root: SetExpr) -> None:
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
        elif isinstance(expression, ViewExpand):
            visit(expression.input, depth + 1)

    visit(root, 0)


class _Cursor:
    def __init__(self, payload: bytes) -> None:
        self.payload = payload
        self.offset = 0
        self.nodes = 0

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
        if tag == _TAG_VIEW_SELECT:
            key = self.unpack("<Q")[0]
            view = self.view()
            set_index = self.unpack("<I")[0]
            try:
                return ViewSelect(key, view, set_index)
            except (TypeError, ValueError) as error:
                raise ExpressionError(str(error)) from error
        if tag == _TAG_VIEW_FOLD:
            key = self.unpack("<Q")[0]
            view = self.view()
            (reduce,) = self.unpack("<B")
            try:
                reduce_value = ViewReduce(reduce)
            except ValueError as error:
                raise ExpressionError(f"unknown view reduction {reduce}") from error
            return ViewFold(key, view, reduce_value)
        if tag == _TAG_VIEW_EXPAND:
            view = self.view()
            return ViewExpand(self.expression(depth + 1), view)
        raise ExpressionError(f"unknown expression tag {tag}")
