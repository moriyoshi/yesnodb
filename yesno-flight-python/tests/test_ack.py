"""The ingest acknowledgement's wire width, which changed under a shipped client.

``DoPut``'s ``app_metadata`` carried 8 bytes -- the row count alone -- until
2026-09-12, when the commit version was appended.  This client rejected anything
but 8 bytes with a ``ProtocolError``, so every ingest failed outright against a
current server, and no Rust gate noticed: the package is outside the cargo
workspace, and the integration test that would have caught it runs only from this
project's own gate.
"""

from __future__ import annotations

import struct

import pytest

from yesnodb.client import Ack
from yesnodb.client.errors import ProtocolError


def test_an_old_server_loses_the_version_rather_than_the_call() -> None:
    """The case that is easy to forget: a new client against an old server."""

    assert Ack._decode(struct.pack("<Q", 5_000)) == Ack(rows=5_000, version=None)


def test_a_current_server_reports_rows_and_version() -> None:
    assert Ack._decode(struct.pack("<QQ", 5_000, 42)) == Ack(rows=5_000, version=42)


def test_a_zero_version_is_absent_rather_than_version_zero() -> None:
    """Version 0 is the empty database and no commit is assigned it, so a zero
    means "committed nothing" and must not be handed back as a readable version.
    """

    assert Ack._decode(struct.pack("<QQ", 0, 0)) == Ack(rows=0, version=None)


@pytest.mark.parametrize("length", [0, 4, 9, 15, 24])
def test_any_other_width_is_refused(length: int) -> None:
    """Including 24: a future server that widens this again must be detected by an
    older client rather than have its extra field silently ignored.
    """

    with pytest.raises(ProtocolError, match="expected 8 or 16"):
        Ack._decode(b"\x00" * length)
