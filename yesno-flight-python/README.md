# yesnodb Python client

`yesnodb` is the typed Python client for the yesnodb Apache Arrow Flight
service. It provides streaming reads, exact cardinality queries, bounded pair
ingest, container-payload ingest, version-pinned query preparation, ordinal-set
literals, packed-view expressions, bearer authentication, and TLS or mutual TLS
configuration.

All public client types and expression builders are exported from
`yesnodb.client`; the top-level `yesnodb` name is a PEP 420 namespace package
and does not re-export them.
Install the project with a PEP 517-compatible installer, then connect to a
running `yesnod`:

```python
from yesnodb.client import And, Client, Key, Literal

with Client("http://127.0.0.1:50051") as client:
    client.insert([(42, 1), (42, 5)])
    expression = And(Key(42), Literal(5, 1, 5))
    assert client.query(expression).collect_ordinals() == [1, 5]
```

## Writing whole sets

The pair wire costs one row per ordinal. `insert_containers` costs one row per
65 536-ordinal chunk, carrying the Roaring-spec payload the page store itself
holds, which the server measured at 64.8x less wire on a half-dense page:

```python
from yesnodb.client import FEATURE_CONTAINER_PUT, Client

with Client("http://127.0.0.1:50051") as client:
    if client.supports(FEATURE_CONTAINER_PUT):
        client.insert_containers([(42, range(0, 1_000_000, 3))])
    else:
        client.insert((42, o) for o in range(0, 1_000_000, 3))
```

Container rows union into what is already under the key; there is no removal
form, because the write wire reuses the read side's schema unchanged and that
schema has no operation column. Remove with `Mutation.remove_range` instead.
A caller that already holds containers -- restoring a dump, forwarding a
foreign Roaring file -- builds batches with `ContainerBatchBuilder` and sends
them through `apply_containers`.

Run `./gate.sh` from this directory for formatting, linting, strict typing,
live-server interoperability tests, and wheel construction.
