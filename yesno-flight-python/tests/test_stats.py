from __future__ import annotations

import pytest

from yesnodb.client import ProtocolError, ServerStats


def test_server_stats_decode_protobuf_and_ignore_unknown_fields() -> None:
    body = bytes(
        [
            0x08,
            0x80,
            0x08,
            0x10,
            0x40,
            0x18,
            0x80,
            0x01,
            0x20,
            0x02,
            0x28,
            0x04,
            0x38,
            0x63,
        ]
    )
    # Field 7 is the unknown-field probe. It used to be field 6, which became
    # `features`; an unknown-field test must name a field that is still unknown.
    assert ServerStats.from_protobuf(body) == ServerStats(1024, 64, 128, 2, 4)


def test_server_stats_reads_the_capability_field() -> None:
    """`features` is field 6 and a server predating it reports zero."""

    body = bytes([0x08, 0x80, 0x08, 0x30, 0x03])
    stats = ServerStats.from_protobuf(body)
    assert stats.allocated_bytes == 1024
    assert stats.features == 3

    assert ServerStats.from_protobuf(b"\x08\x80\x08").features == 0


@pytest.mark.parametrize(
    "body",
    [
        b"\x08\x80",
        b"\x0a\x00",
        b"\x00",
        b"\x0f",
    ],
)
def test_server_stats_reject_malformed_protobuf(body: bytes) -> None:
    with pytest.raises(ProtocolError):
        ServerStats.from_protobuf(body)
