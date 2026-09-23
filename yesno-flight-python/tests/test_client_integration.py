from __future__ import annotations

from collections.abc import Callable

import pyarrow as pa
import pyarrow.flight as flight
import pytest

import yesnodb
from yesnodb.client import (
    FEATURE_MIXED_PUT,
    FEATURE_WRITE_TRANSACTIONS,
    OP_DELETE_KEY,
    OP_INSERT,
    OP_INSERT_RANGE,
    OP_REMOVE_RANGE,
    And,
    At,
    Client,
    Key,
    Literal,
    Mutation,
    Or,
    Range,
    TLSConfig,
    View,
    ViewSpec,
)


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
        view = At(View(Key(9), ViewSpec.interleaved(3)), 1)
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


@pytest.mark.integration
def test_apply_commits_mixed_operations_at_one_version(
    server_factory: Callable[..., object],
) -> None:
    running = server_factory()

    with Client(running.endpoint) as client:
        assert client.supports(FEATURE_MIXED_PUT | FEATURE_WRITE_TRANSACTIONS)

        seed = client.insert_batch_acked([(1, 10), (1, 11), (2, 20)])
        assert seed.rows == 3

        ack = client.apply(
            [
                Mutation.remove(1, 10),
                Mutation.insert(1, 12),
                Mutation.insert_range(3, 100, 104),
                Mutation.delete_key(2),
            ]
        )
        assert ack.rows == 4
        assert ack.version is not None and ack.version > seed.version

        assert sorted(client.get(1).collect_ordinals()) == [11, 12]
        assert client.cardinality(2) == 0
        assert sorted(client.get(3).collect_ordinals()) == [100, 101, 102, 103, 104]


@pytest.mark.integration
def test_apply_preserves_order_within_one_key(
    server_factory: Callable[..., object],
) -> None:
    """A removal *after* an insertion must not be reordered before it.

    Replaying these grouped by kind would leave 5 present, because the delete
    would run first. This is the property a change-data feed depends on when
    it replays a delete-then-reinsert of one key.
    """

    running = server_factory()

    with Client(running.endpoint) as client:
        client.apply([Mutation.insert(4, 5), Mutation.delete_key(4), Mutation.insert(4, 6)])
        assert client.get(4).collect_ordinals() == [6]

        client.apply([Mutation.insert(5, 7), Mutation.remove(5, 7)])
        assert client.cardinality(5) == 0


@pytest.mark.integration
def test_a_write_transaction_is_invisible_until_commit(
    server_factory: Callable[..., object],
) -> None:
    running = server_factory()

    with Client(running.endpoint) as client:
        txn = client.begin_write()
        assert client.stage(txn, [Mutation.insert(8, 80), Mutation.insert(8, 81)]) == 2
        assert client.stage(txn, [Mutation.insert_range(8, 90, 92)]) == 1

        # Nothing staged is visible to a reader yet.
        assert client.cardinality(8) == 0

        version = client.commit_write(txn)
        assert version > 0
        assert sorted(client.get(8).collect_ordinals()) == [80, 81, 90, 91, 92]

        # Commit is idempotent: a retry after a lost response returns the
        # original version rather than committing a second time.
        assert client.commit_write(txn) == version


@pytest.mark.integration
def test_an_aborted_write_transaction_applies_nothing(
    server_factory: Callable[..., object],
) -> None:
    running = server_factory()

    with Client(running.endpoint) as client:
        txn = client.begin_write()
        assert client.stage(txn, [Mutation.insert(9, 90)]) == 1
        client.abort_write(txn)

        assert client.cardinality(9) == 0
        # Aborting one the server no longer knows about still succeeds: the
        # caller's intent already holds.
        client.abort_write(txn)


@pytest.mark.integration
def test_a_locally_refused_stage_leaves_the_transaction_usable(
    server_factory: Callable[..., object],
) -> None:
    """Validation happens before a stream is opened, so nothing is sent.

    That is the difference between a client-side refusal and a server-side
    one. A failure the client cannot see coming -- the row bound, a dropped
    connection -- leaves earlier batches of the same call staged, and the
    server then poisons the transaction so they cannot be published. This
    client rejects the malformed mutation first, so the transaction is
    untouched and still usable.
    """

    running = server_factory()

    with Client(running.endpoint) as client:
        txn = client.begin_write()
        with pytest.raises(ValueError):
            client.stage(
                txn,
                [Mutation.insert(4, 40), Mutation(key=5, lo=9, hi=4, op=OP_REMOVE_RANGE)],
            )
        # Untouched: a later stage and the commit both succeed, and the valid
        # row from the refused call was never sent.
        assert client.stage(txn, [Mutation.insert(4, 41)]) == 1
        client.commit_write(txn)
        assert client.get(4).collect_ordinals() == [41]


def test_a_mutation_is_validated_on_every_construction_path() -> None:
    """The factories are not the only way in; this is a dataclass."""

    with pytest.raises(ValueError):
        Mutation(key=1, lo=2, hi=3, op=OP_INSERT)
    with pytest.raises(ValueError):
        Mutation(key=1, lo=5, hi=0, op=OP_DELETE_KEY)
    with pytest.raises(ValueError):
        Mutation(key=1, lo=9, hi=4, op=OP_INSERT_RANGE)
    with pytest.raises(ValueError):
        Mutation(key=1, lo=0, hi=0, op=99)
