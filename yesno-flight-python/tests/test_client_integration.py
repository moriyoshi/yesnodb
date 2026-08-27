from __future__ import annotations

from collections.abc import Callable

import pyarrow as pa
import pyarrow.flight as flight
import pytest

import yesnodb
from yesnodb.client import And, Client, Key, Literal, Or, Range, TLSConfig, ViewSelect, ViewSpec


def test_yesnodb_is_a_namespace_and_does_not_reexport_client() -> None:
    assert yesnodb.__file__ is None
    assert not hasattr(yesnodb, "Client")
    assert Client.__module__ == "yesnodb.client.flight"


@pytest.mark.integration
def test_python_client_covers_the_shipped_flight_surface(
    server_factory: Callable[..., object],
) -> None:
    running = server_factory()
    endpoint = running.endpoint

    with Client(endpoint) as client:
        pairs = [(7, ordinal) for ordinal in range(9_000)]
        assert client.insert(pairs) == 9_000
        assert client.insert([]) == 0
        assert client.keys() == [7]
        assert client.cardinality(7) == 9_000

        planned = client.prepare_key(7)
        assert planned.total_records == 9_000
        assert planned.ticket.key == 7
        assert planned.version > 0

        # The plan remains pinned even after a newer write becomes visible.
        assert client.insert_batch([(7, 20_000)]) == 1
        explicitly_pinned = client.prepare_query_at(Key(7), planned.version)
        assert explicitly_pinned.total_records == 9_000
        assert explicitly_pinned.version == planned.version
        pinned = []
        for chunk in client.fetch(planned):
            pinned.extend(chunk.data.column(0).to_pylist())
        assert pinned == list(range(9_000))
        historical = []
        for chunk in client.fetch(explicitly_pinned):
            historical.extend(chunk.data.column(0).to_pylist())
        assert historical == pinned

        assert client.insert([(8, ordinal) for ordinal in range(0, 9_000, 2)]) == 4_500
        expression = And(Key(7), Or(Key(8), Range(20_000, 20_001)))
        assert client.query_cardinality(expression) == 4_501
        query = client.query(expression)
        assert query.info.total_records == 4_501
        assert query.collect_ordinals() == [*range(0, 9_000, 2), 20_000]

        literal = And(Key(7), Literal(9, 0, 9, 65_536, 5))
        assert client.query_cardinality(literal) == 3
        assert client.query(literal).collect_ordinals() == [0, 5, 9]

        packed = [(9, logical * 3 + 1) for logical in (2, 8, 89)]
        assert client.insert_batch(packed) == 3
        view = ViewSelect(9, ViewSpec.interleaved(3), 1)
        assert client.query(view).collect_ordinals() == [2, 8, 89]

        assert client.remove_batch([(7, 0), (7, 2)]) == 2
        rows = client.get(7).collect_ordinals()
        assert len(rows) == 8_999
        assert rows[0] == 1
        assert 2 not in rows
        assert rows[-1] == 20_000

        stats = client.stats()
        assert stats.shards > 0
        assert stats.wal_bytes > 0


@pytest.mark.integration
def test_bearer_roles_and_leadership_fence_reach_reads_and_writes(
    server_factory: Callable[..., object],
) -> None:
    running = server_factory(authenticated=True)

    with Client(running.endpoint, token="write-token") as writer:
        assert writer.insert_batch([(1, 10)]) == 1
        assert writer.cardinality(1) == 1
        with pytest.raises(flight.FlightUnauthorizedError):
            writer.clear(1)

    with Client(running.endpoint, token="read-token") as reader:
        assert reader.get(1).collect_ordinals() == [10]
        with pytest.raises(flight.FlightUnauthorizedError):
            reader.insert_batch([(1, 11)])

    with Client(running.endpoint, token="admin-token") as admin:
        assert admin.clear(1) > 0
        assert admin.cardinality(1) == 0

    with (
        Client(running.endpoint, token="wrong-token") as unknown,
        pytest.raises(flight.FlightUnauthenticatedError),
    ):
        unknown.cardinality(1)

    with (
        Client(running.endpoint, token="read-token", minimum_term=(1 << 32) - 1) as fenced,
        pytest.raises(pa.ArrowInvalid),
    ):
        fenced.cardinality(1)


@pytest.mark.integration
def test_mutual_tls_authenticates_a_python_writer(
    server_factory: Callable[..., object],
) -> None:
    running = server_factory(mutual_tls=True)
    assert running.tls_dir is not None
    tls = TLSConfig.from_files(
        root_certificates=running.tls_dir / "ca.pem",
        client_certificate=running.tls_dir / "client.pem",
        client_key=running.tls_dir / "client.key",
        server_name="localhost",
    )
    with Client(running.endpoint, tls=tls) as client:
        assert client.insert_batch([(5, 99)]) == 1
        assert client.get(5).collect_ordinals() == [99]

    no_identity = TLSConfig.from_files(
        root_certificates=running.tls_dir / "ca.pem",
        server_name="localhost",
    )
    with (
        Client(running.endpoint, tls=no_identity) as anonymous,
        pytest.raises((flight.FlightUnavailableError, flight.FlightUnauthenticatedError)),
    ):
        anonymous.cardinality(5)
