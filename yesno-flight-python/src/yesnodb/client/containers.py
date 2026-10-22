"""Container payloads for the write wire.

The pair and mutation wires cost one row per **set bit**. A container row
carries one chunk of up to 65 536 ordinals as the Roaring-spec bytes the page
store already holds, which the server measured at 64.8x less wire on a
half-dense page. This module builds those rows.

It is an encoder and nothing more. There is no Roaring dependency and none is
wanted: a container payload is three fixed layouts over sorted 16-bit values,
all three documented in ``docs/storage-format.md``, and this file implements
exactly what that table says. The server validates every payload through a
decoder that is a fuzz target by contract, so a payload this module got wrong
comes back as ``invalid_argument`` rather than as corruption.

Insert only. A container row unions into whatever is already under the key --
the write wire has no ``op`` column, because giving it one would fork it from
the read side's schema, which is the single property that makes the whole thing
cheap. A removal is :data:`~yesnodb.client.OP_REMOVE_RANGE` on the mutation
wire.
"""

from __future__ import annotations

import struct
from collections.abc import Iterable, Iterator, Sequence

import pyarrow as pa

from .expression import ORDINAL_LIMIT, _u32, _u64

#: Ordinals per chunk. The low 16 bits of an ordinal index within a container;
#: the remaining 48 are the chunk's ``prefix48``.
CHUNK_BITS = 16
CHUNK_CARD = 1 << CHUNK_BITS
#: Above this cardinality there is no array encoding, whatever the byte count
#: says. The server's decoder refuses a longer one.
ARRAY_MAX = 4096
#: A bitmap payload is this many bytes whatever its cardinality.
BITMAP_BYTES = 8192
#: The most intervals a run payload may declare. A chunk cannot need more than
#: 32 768 -- that is the alternating worst case -- so this never binds here; it
#: is checked because the decoder checks it.
RUN_MAX_INTERVALS = 32768
PREFIX48_LIMIT = 1 << 48

#: Payload layouts, as stored. These numbers go on the wire.
KIND_ARRAY = 0
KIND_BITMAP = 1
KIND_RUN = 2

#: ``{ key, prefix48, kind, cardinality, payload }`` -- containers, verbatim.
#:
#: Byte-identical to what the page store holds and to what the read side ships,
#: which is why the write half needed no new schema. ``kind`` and
#: ``cardinality`` are columns rather than comments because neither is
#: derivable from ``payload`` alone: an array of *n* values and a run of *n / 2*
#: intervals can be the same length, and a bitmap's cardinality is a popcount
#: the reader should not have to redo.
CONTAINERS_SCHEMA = pa.schema(
    [
        pa.field("key", pa.uint64(), nullable=False),
        pa.field("prefix48", pa.uint64(), nullable=False),
        pa.field("kind", pa.uint8(), nullable=False),
        pa.field("cardinality", pa.uint32(), nullable=False),
        pa.field("payload", pa.binary(), nullable=False),
    ]
)

#: Chunks per record batch, so one batch stays a bounded allocation. A bitmap
#: chunk is 8 KiB of payload, so this is a ~16 MiB ceiling on the values buffer.
BATCH_CHUNKS = 2048


def _runs(values: Sequence[int]) -> list[tuple[int, int]]:
    """Maximal ascending intervals, as ``(start, length - 1)`` pairs.

    Maximal, not merely non-overlapping. The Roaring spec permits adjacent runs
    and the server's decoder coalesces them, but it only does so because a
    foreign file may carry them -- it is never legal state inside the engine,
    whose run kernels assume maximal intervals. Emitting them would make the
    server do work to undo this function's output.
    """

    runs: list[tuple[int, int]] = []
    start = previous = values[0]
    for value in values[1:]:
        if value == previous + 1:
            previous = value
            continue
        runs.append((start, previous - start))
        start = previous = value
    runs.append((start, previous - start))
    return runs


def _array_payload(values: Sequence[int]) -> bytes:
    return struct.pack(f"<{len(values)}H", *values)


def _bitmap_payload(values: Iterable[int]) -> bytes:
    # 1024 little-endian `u64` words, bit `i` representing value `i`. Within a
    # little-endian word, byte `b` holds bits `8b..8b+8`, so the whole payload
    # is one flat LSB-first bit array and the word size never appears here.
    bits = bytearray(BITMAP_BYTES)
    for value in values:
        bits[value >> 3] |= 1 << (value & 7)
    return bytes(bits)


def _run_payload(runs: Sequence[tuple[int, int]]) -> bytes:
    out = bytearray(struct.pack("<H", len(runs)))
    for start, length_minus_one in runs:
        out.extend(struct.pack("<HH", start, length_minus_one))
    return bytes(out)


def encode_chunk(values: Sequence[int]) -> tuple[int, int, bytes]:
    """The smallest valid payload for one chunk, as ``(kind, cardinality, payload)``.

    ``values`` are the low 16 bits of one chunk's ordinals, ascending and
    unique. Both properties are checked: the server's decoder would catch a
    descending array payload, but it cannot catch a caller who passed
    unsorted values and got a run encoding, because the runs it derived would be
    structurally valid and describe a different set.

    **Smallest, rather than the engine's own choice.** ``yesno-core`` picks a
    representation with hysteresis -- a bitmap demotes only below
    ``BITMAP_DEMOTE``, well under the cardinality at which an array would first
    be smaller -- because it is deciding whether to *re-encode* a container that
    is being mutated, and flapping between two representations costs more than
    either. A fresh encode has nothing to flap against, so the only question is
    bytes on the wire, and the engine normalizes what it receives anyway.
    """

    if not values:
        raise ValueError("a container needs at least one value")
    if len(values) > CHUNK_CARD:
        raise ValueError(f"a chunk holds at most {CHUNK_CARD} values, got {len(values)}")
    previous = -1
    for value in values:
        if not isinstance(value, int) or isinstance(value, bool):
            raise TypeError(f"chunk values must be integers, got {type(value).__name__}")
        if not 0 <= value < CHUNK_CARD:
            raise ValueError(f"chunk value {value} is outside 0..{CHUNK_CARD}")
        if value <= previous:
            raise ValueError("chunk values must ascend and be unique")
        previous = value

    cardinality = len(values)
    runs = _runs(values)
    run_bytes = 2 + 4 * len(runs)

    # Every candidate, then the smallest. `array` drops out above ARRAY_MAX
    # whatever its byte count, and `run` above RUN_MAX_INTERVALS -- which a
    # chunk cannot reach, but the bound is the decoder's and belongs here
    # rather than in a comment claiming it is unreachable.
    candidates: list[tuple[int, int, bytes]] = []
    if cardinality <= ARRAY_MAX:
        candidates.append((2 * cardinality, KIND_ARRAY, b""))
    if len(runs) <= RUN_MAX_INTERVALS:
        candidates.append((run_bytes, KIND_RUN, b""))
    candidates.append((BITMAP_BYTES, KIND_BITMAP, b""))
    size, kind, _ = min(candidates, key=lambda c: (c[0], c[1]))

    if kind == KIND_ARRAY:
        payload = _array_payload(values)
    elif kind == KIND_RUN:
        payload = _run_payload(runs)
    else:
        payload = _bitmap_payload(values)
    assert len(payload) == size, "payload length must match the size it was chosen by"
    return kind, cardinality, payload


class ContainerBatchBuilder:
    """Accumulate chunks and hand them over as record batches.

    ``finish`` returns ``None`` for an empty builder rather than an empty batch,
    because a ``do_put`` stream carrying a zero-row batch is a different thing
    from one carrying nothing.
    """

    def __init__(self) -> None:
        self._key: list[int] = []
        self._prefix: list[int] = []
        self._kind: list[int] = []
        self._card: list[int] = []
        self._payload: list[bytes] = []

    def __len__(self) -> int:
        return len(self._key)

    @property
    def cardinality(self) -> int:
        """Ordinals staged so far, which is what the server acknowledges."""

        return sum(self._card)

    def push_chunk(self, key: int, prefix48: int, values: Sequence[int]) -> None:
        """One chunk, encoded by :func:`encode_chunk`."""

        key = _u64(key, name="key")
        prefix48 = _u64(prefix48, name="prefix48")
        if prefix48 >= PREFIX48_LIMIT:
            raise ValueError(f"prefix48 {prefix48} does not fit 48 bits")
        kind, cardinality, payload = encode_chunk(values)
        self._key.append(key)
        self._prefix.append(prefix48)
        self._kind.append(kind)
        self._card.append(_u32(cardinality, name="cardinality"))
        self._payload.append(payload)

    def push_set(self, key: int, ordinals: Iterable[int]) -> None:
        """Every chunk of one key's ordinals, which need not arrive sorted."""

        key = _u64(key, name="key")
        chunks: dict[int, list[int]] = {}
        for ordinal in ordinals:
            ordinal = _u64(ordinal, name="ordinal")
            if ordinal >= ORDINAL_LIMIT:
                raise ValueError(f"ordinal {ordinal} is reserved")
            chunks.setdefault(ordinal >> CHUNK_BITS, []).append(ordinal & (CHUNK_CARD - 1))
        for prefix48 in sorted(chunks):
            values = sorted(set(chunks[prefix48]))
            self.push_chunk(key, prefix48, values)

    def finish(self) -> pa.RecordBatch | None:
        """The accumulated chunks, clearing the builder."""

        if not self._key:
            return None
        batch = pa.RecordBatch.from_arrays(
            [
                pa.array(self._key, type=pa.uint64()),
                pa.array(self._prefix, type=pa.uint64()),
                pa.array(self._kind, type=pa.uint8()),
                pa.array(self._card, type=pa.uint32()),
                pa.array(self._payload, type=pa.binary()),
            ],
            schema=CONTAINERS_SCHEMA,
        )
        self.__init__()  # type: ignore[misc]
        return batch


def container_batches(
    sets: Iterable[tuple[int, Iterable[int]]],
    *,
    max_chunks: int = BATCH_CHUNKS,
) -> Iterator[pa.RecordBatch]:
    """Batches of at most ``max_chunks`` chunks, from ``(key, ordinals)`` pairs.

    A key's chunks are not split across batches, so a batch may exceed
    ``max_chunks`` by the chunk count of its last key. That keeps one key's
    write in one commit on the streaming forms, which is the property a caller
    is more likely to depend on than an exact batch size.
    """

    if max_chunks < 1:
        raise ValueError(f"max_chunks must be at least 1, got {max_chunks}")
    builder = ContainerBatchBuilder()
    for key, ordinals in sets:
        builder.push_set(key, ordinals)
        if len(builder) >= max_chunks:
            batch = builder.finish()
            if batch is not None:
                yield batch
    batch = builder.finish()
    if batch is not None:
        yield batch
