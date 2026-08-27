# yesnodb Python client

`yesnodb` is the typed Python client for the yesnodb Apache Arrow Flight
service. It provides streaming reads, exact cardinality queries, bounded pair
ingest, version-pinned query preparation, ordinal-set literals, packed-view expressions, bearer
authentication, and TLS or mutual TLS configuration.

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

Run `./gate.sh` from this directory for formatting, linting, strict typing,
live-server interoperability tests, and wheel construction.
