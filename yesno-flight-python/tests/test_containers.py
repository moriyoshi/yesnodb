from __future__ import annotations

import itertools
import struct
from random import Random

import pyarrow as pa
import pytest
from hypothesis import given
from hypothesis import strategies as st

from yesnodb.client import (
    ARRAY_MAX,
    BITMAP_BYTES,
    CHUNK_CARD,
    CONTAINERS_SCHEMA,
    KIND_ARRAY,
    KIND_BITMAP,
    KIND_RUN,
    ContainerBatchBuilder,
    container_batches,
    encode_chunk,
)


def decode_chunk(kind: int, cardinality: int, payload: bytes) -> list[int]:
    """The documented layouts, read back.

    An independent implementation on purpose: this is the oracle for the
    encoder's unit tests, and an oracle that shared code with the thing it
    checks would agree with it about a shared mistake. The integration test
    uses the real server instead, which is the stronger check -- this one runs
    without a daemon and localizes a failure to the byte layout.
    """

    if kind == KIND_ARRAY:
        assert len(payload) == 2 * cardinality
        return list(struct.unpack(f"<{cardinality}H", payload))
    if kind == KIND_BITMAP:
        assert len(payload) == BITMAP_BYTES
        return [i for i in range(CHUNK_CARD) if payload[i >> 3] >> (i & 7) & 1]
    if kind == KIND_RUN:
        (count,) = struct.unpack_from("<H", payload, 0)
        assert len(payload) == 2 + 4 * count
        out: list[int] = []
        for i in range(count):
            start, length_minus_one = struct.unpack_from("<HH", payload, 2 + 4 * i)
            out.extend(range(start, start + length_minus_one + 1))
        return out
    raise AssertionError(f"unknown kind {kind}")


def test_a_short_run_of_values_is_an_array() -> None:
    kind, cardinality, payload = encode_chunk([1, 3, 5, 7])
    assert kind == KIND_ARRAY
    assert cardinality == 4
    assert payload == struct.pack("<4H", 1, 3, 5, 7)


def test_a_contiguous_span_is_one_run() -> None:
    kind, cardinality, payload = encode_chunk(list(range(100, 5000)))
    assert kind == KIND_RUN
    assert cardinality == 4900
    assert payload == struct.pack("<HHH", 1, 100, 4899)


def test_a_dense_scatter_is_a_bitmap() -> None:
    # Every other value: 32 768 values is 65 536 bytes as an array and
    # 131 074 as runs, so only the fixed 8 KiB bitmap is viable.
    values = list(range(0, CHUNK_CARD, 2))
    kind, cardinality, payload = encode_chunk(values)
    assert kind == KIND_BITMAP
    assert cardinality == len(values)
    assert len(payload) == BITMAP_BYTES
    assert decode_chunk(kind, cardinality, payload) == values


def test_the_encoding_is_whichever_is_smallest() -> None:
    # 4096 values, no two adjacent: an array at the cardinality ceiling is
    # 8 192 bytes, exactly the bitmap, and runs would be 16 386. The tie goes
    # to the array because `KIND_ARRAY` is the lower discriminant, which is
    # arbitrary but must be stable.
    values = list(range(0, 2 * ARRAY_MAX, 2))
    kind, _, payload = encode_chunk(values)
    assert len(payload) == BITMAP_BYTES
    assert kind == KIND_ARRAY

    # One more value and the array is gone on cardinality alone, while runs
    # are still far worse than the bitmap.
    kind, cardinality, _ = encode_chunk(sorted([*values, 1]))
    assert cardinality == ARRAY_MAX + 1
    assert kind == KIND_BITMAP


def test_a_full_chunk_is_one_run() -> None:
    kind, cardinality, payload = encode_chunk(list(range(CHUNK_CARD)))
    assert kind == KIND_RUN
    assert cardinality == CHUNK_CARD
    assert payload == struct.pack("<HHH", 1, 0, CHUNK_CARD - 1)


def test_a_clustered_chunk_is_a_multi_interval_run() -> None:
    """100 blocks of 50, which is the only hand-written multi-interval case.

    5 000 values shut the array out on cardinality alone, and 100 intervals
    cost 402 bytes against the bitmap's 8 192. ``_runs`` emits its last
    interval from the statement after its loop rather than from inside it, so
    an error in the loop's length arithmetic is invisible to every
    single-interval case -- and it was invisible to this whole suite until this
    test and the sweep below were added. A sabotage confirming that is in the
    journal.
    """

    values = [block * 100 + offset for block in range(100) for offset in range(50)]
    kind, cardinality, payload = encode_chunk(values)
    assert kind == KIND_RUN
    assert cardinality == 5000
    assert int.from_bytes(payload[:2], "little") == 100
    assert len(payload) == 2 + 4 * 100
    # The second interval, which the loop wrote.
    assert struct.unpack_from("<HH", payload, 6) == (100, 49)
    assert decode_chunk(kind, cardinality, payload) == values


def test_adjacent_runs_are_coalesced() -> None:
    # The server's decoder repairs adjacency rather than refusing it, but only
    # because a foreign file may carry it. Emitting it would make the server
    # undo this encoder's work, so the intervals must already be maximal.
    _, _, payload = encode_chunk([*range(0, 4), *range(4, 8)])
    assert payload == struct.pack("<HHH", 1, 0, 7)


@pytest.mark.parametrize(
    ("values", "message"),
    [
        ([], "at least one value"),
        ([5, 5], "ascend and be unique"),
        ([5, 4], "ascend and be unique"),
        ([CHUNK_CARD], "outside"),
        ([-1], "outside"),
    ],
)
def test_a_malformed_chunk_is_refused_before_the_wire(values: list[int], message: str) -> None:
    with pytest.raises(ValueError, match=message):
        encode_chunk(values)


def test_booleans_are_not_ordinals() -> None:
    # `True == 1` in Python, so a bool reaches every integer check here
    # unnoticed and would silently encode as the value 1.
    with pytest.raises(TypeError, match="integers"):
        encode_chunk([True])


@given(
    st.lists(st.integers(min_value=0, max_value=CHUNK_CARD - 1), min_size=1, max_size=4000).map(
        lambda values: sorted(set(values))
    )
)
def test_any_chunk_round_trips(values: list[int]) -> None:
    kind, cardinality, payload = encode_chunk(values)
    assert cardinality == len(values)
    assert decode_chunk(kind, cardinality, payload) == values


@given(
    st.lists(st.integers(min_value=0, max_value=CHUNK_CARD - 1), min_size=1, max_size=600).map(
        lambda values: sorted(set(values))
    )
)
def test_no_encoding_beats_the_one_chosen(values: list[int]) -> None:
    _, _, payload = encode_chunk(values)
    runs = 1 + sum(1 for a, b in itertools.pairwise(values) if b != a + 1)
    viable = [BITMAP_BYTES, 2 + 4 * runs]
    if len(values) <= ARRAY_MAX:
        viable.append(2 * len(values))
    assert len(payload) == min(viable)


#: Chunk shapes spanning the three encodings, and the shapes within them.
#:
#: The hypothesis properties above cover one corner of this well and cannot
#: reach the others: a strategy over sorted sets of uniform draws produces
#: arrays, almost never a run container, and never a bitmap, because a bitmap
#: wins only above ``ARRAY_MAX`` values. This sweep is deterministic for a
#: second reason -- the assertion that every encoding actually appeared has to
#: hold inside one test, and hypothesis runs each example in its own call.
CHUNK_SHAPES = [
    ("contiguous", CHUNK_CARD, 1, 3000),
    ("clustered runs", 50, 8, 6000),
    ("dense scatter", 1, 2, 20000),
    ("sparse", 1, 64, 400),
    ("very sparse", 1, 1024, 40),
    ("singleton", 1, 1, 1),
]


def _random_chunk(random: Random, run_len: int, gap_max: int, size: int) -> list[int]:
    """Ascending unique values in runs of at most ``run_len``, gaps of at least one."""

    values: list[int] = []
    value = random.randrange(gap_max)
    while len(values) < size and value < CHUNK_CARD:
        for _ in range(1 + random.randrange(run_len)):
            if len(values) >= size or value >= CHUNK_CARD:
                break
            values.append(value)
            value += 1
        value += 1 + random.randrange(gap_max)
    return values


def test_every_encoding_round_trips_and_none_is_beaten() -> None:
    seen: dict[int, int] = {}
    intervals = 0
    for name, run_len, gap_max, size in CHUNK_SHAPES:
        for seed in range(30):
            values = _random_chunk(Random(seed), run_len, gap_max, size)
            kind, cardinality, payload = encode_chunk(values)
            assert cardinality == len(values), name
            assert decode_chunk(kind, cardinality, payload) == values, name
            runs = 1 + sum(1 for a, b in itertools.pairwise(values) if b != a + 1)
            viable = [BITMAP_BYTES, 2 + 4 * runs]
            if len(values) <= ARRAY_MAX:
                viable.append(2 * len(values))
            assert len(payload) == min(viable), name
            seen[kind] = seen.get(kind, 0) + 1
            if kind == KIND_RUN:
                intervals = max(intervals, int.from_bytes(payload[:2], "little"))
    # Without these the sweep could pass having built one encoding throughout,
    # or only single-interval runs, which is what let a sabotage through.
    assert set(seen) == {KIND_ARRAY, KIND_BITMAP, KIND_RUN}, seen
    assert intervals >= 2, intervals


def test_a_set_is_split_into_chunks_by_its_high_bits() -> None:
    builder = ContainerBatchBuilder()
    builder.push_set(7, [1, 2, 1 << 16, (3 << 16) + 9])
    assert len(builder) == 3
    assert builder.cardinality == 4
    batch = builder.finish()
    assert batch is not None
    assert batch.schema.equals(CONTAINERS_SCHEMA)
    assert batch.column(0).to_pylist() == [7, 7, 7]
    assert batch.column(1).to_pylist() == [0, 1, 3]
    assert batch.column(3).to_pylist() == [2, 1, 1]
    assert decode_chunk(
        batch.column(2)[2].as_py(), batch.column(3)[2].as_py(), batch.column(4)[2].as_py()
    ) == [9]


def test_finishing_clears_the_builder() -> None:
    builder = ContainerBatchBuilder()
    builder.push_set(1, [1])
    assert builder.finish() is not None
    assert len(builder) == 0
    assert builder.cardinality == 0
    assert builder.finish() is None


def test_unsorted_ordinals_and_duplicates_are_accepted_for_a_whole_set() -> None:
    # `push_set` sorts and deduplicates, unlike `push_chunk`: a caller holding
    # a Python set has no order to offer, and a caller holding one chunk's
    # values is close enough to the wire that a silent sort would hide a bug.
    builder = ContainerBatchBuilder()
    builder.push_set(1, [9, 1, 9, 5])
    batch = builder.finish()
    assert batch is not None
    assert batch.column(3).to_pylist() == [3]
    assert decode_chunk(
        batch.column(2)[0].as_py(), batch.column(3)[0].as_py(), batch.column(4)[0].as_py()
    ) == [1, 5, 9]


def test_a_reserved_ordinal_is_refused() -> None:
    builder = ContainerBatchBuilder()
    with pytest.raises(ValueError, match="reserved"):
        builder.push_set(1, [(1 << 64) - 1])


def test_a_prefix_beyond_48_bits_is_refused() -> None:
    builder = ContainerBatchBuilder()
    with pytest.raises(ValueError, match="48 bits"):
        builder.push_chunk(1, 1 << 48, [0])


def test_batches_are_bounded_by_chunk_count() -> None:
    sets = [(key, [key << 16, (key + 1) << 16]) for key in range(10)]
    batches = list(container_batches(sets, max_chunks=4))
    assert [b.num_rows for b in batches] == [4, 4, 4, 4, 4]
    assert sum(b.num_rows for b in batches) == 20

    # A key's chunks stay in one batch, so the bound is a floor once a key
    # straddles it rather than a ceiling.
    wide = [(1, [i << 16 for i in range(10)])]
    only = list(container_batches(wide, max_chunks=4))
    assert [b.num_rows for b in only] == [10]


def test_an_empty_input_sends_nothing() -> None:
    assert list(container_batches([])) == []
    assert list(container_batches([(1, [])])) == []


def test_the_schema_matches_the_servers_field_for_field() -> None:
    # The server dispatches on schema equality: a container batch that misses
    # by a name or an integer width is read as the mutation wire instead.
    assert CONTAINERS_SCHEMA.names == ["key", "prefix48", "kind", "cardinality", "payload"]
    assert CONTAINERS_SCHEMA.types == [
        pa.uint64(),
        pa.uint64(),
        pa.uint8(),
        pa.uint32(),
        pa.binary(),
    ]
    assert all(not f.nullable for f in CONTAINERS_SCHEMA)
