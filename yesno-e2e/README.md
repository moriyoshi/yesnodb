# yesno-e2e

`yesno-e2e` is [yesnodb's](../README.md) Python-scripted end-to-end harness.
It runs checked-in scenarios in the sandboxed Monty interpreter while Rust host
verbs exercise the real database, Arrow, DataFusion, Flight, replication, and
server implementations.

From the repository root:

```console
cargo test -p yesno-e2e
cargo run -p yesno-e2e -- --list
cargo run -p yesno-e2e -- e2e/scenarios/lifecycle.py
```

With no scenario paths, the runner executes every file under `e2e/scenarios/`.
`--show-output` displays the output of passing measurement fixtures, `--timeout`
sets a per-scenario limit, and `--arg name=value` selects a larger corpus or
iteration count when a fixture supports it.

Each scenario must contain an assertion and invoke at least one host verb.
Python's `set` and `sorted` provide the semantic oracle. Scripts have no ambient
filesystem, environment, or network access, and verb names must not collide
with Python builtins.

External database fixtures live under `e2e/postgresql/` and `e2e/mysql/`.
Both Bazel tests invoke the same runner and link the same featureless fixture
host. Their scenarios compose the ordinary generic `fx_*` verbs for declared
resources, private paths, managed processes, readiness, interactive sessions,
exact output comparison, and cleanup. Backend identity and lifecycle remain
scenario data; there is no backend-specific Rust world, feature, or entrypoint.
The ABI-pinned suites are deliberately outside the ordinary Cargo scenario walk.

The Java search adapters and version-locked OpenSearch/Elasticsearch plugins
have opt-in scenarios on the same runner and shared verb surface:

```console
./scripts/gate-search.sh
```

The scenarios under `e2e/search/` use the ordinary `db_*`, `flight_*`, and
`search_*` verbs. They serve a real database over Flight, run the application helper,
installs each plugin into a disposable unpacked distribution, indexes a
positive-control corpus, and asserts the exact matching document IDs. Use
`--application-only`, `--opensearch-only`, or `--elasticsearch-only` while
iterating; the script only selects scenario paths and supplies a longer timeout.
The Rust host downloads SHA-512-pinned OpenSearch 3.8.0 and
Elasticsearch 9.5.2 archives into `.agents-workspace/tmp/search-e2e-cache`;
`YESNO_E2E_OPENSEARCH_ARCHIVE` and `YESNO_E2E_ELASTICSEARCH_ARCHIVE` may
override the cache with checksum-verified local archives when invoking the
runner directly. The gate itself needs only Docker: it shares the
all-in-one `yesno-e2e:local` image with every other containerized gate.

See the [contributor testing guide](../.agents/docs/TESTING.md) for the complete
verb and fixture conventions.
