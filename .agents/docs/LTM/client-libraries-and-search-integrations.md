# Client Libraries and Search Integrations

## Summary

The shared Flight protocol is consumed by Rust, Python, Java, Go, C++, PostgreSQL, MySQL, Tantivy, and search-engine integrations without changing storage semantics. Each client owns its language-specific unsigned-integer, streaming, TLS, and error boundaries while sharing strict expression, ticket, and version-pinned request formats.

## Key Facts

- `YesnoClient<Channel>` is the Rust convenience client for planning, fetching, cardinality, keys, ingest, removal, clear, and protobuf stats.
- `YSNQ` v1 wraps current or exact-version expression preparation; a pinned request never falls forward to a newer version.
- The Python distribution is `yesnodb`, implemented as the PEP 420 namespace package `yesnodb.client`.
- The Java 17 API lives under `dev.yesnodb.client` and treats Java `long` as raw unsigned 64-bit bits at the wire boundary.
- `yesno-tantivy` maps stable application ordinals through an explicit fast-field resolver; yesno ordinals are never Tantivy `DocId` values.
- Remote materialization is bounded by advertised cardinality and then verified for type, nullability, ordering, and row count.
- Ingest acknowledgements accept the legacy 8-byte row count or the 16-byte row-count-plus-version form; exact-version reads may wait briefly for that commit to enter the visible prefix.

## Details

### Shared Rust client and wire requests

The Rust client separates prepare from fetch so callers can retain versioned tickets. Iterator ingest is bounded into batches, while adapter-owned `insert_batch` and `remove_batch` preserve one record batch and one server commit where transaction boundaries require it.

`QueryRequest` preserves the legacy descriptor for current-version expressions and uses the `YSNQ` envelope only for explicit version pinning. The server calls `snapshot_at` for pinned requests. Recoverable current requests may restart from planning; pinned requests must fail rather than substitute another version.

`DoPut` acknowledgement metadata is either 8 bytes ( rows only, legacy ) or 16 bytes ( rows followed by commit version ). `Ack { rows, version: Option<u64> }` accepts both and rejects every other width; version zero is treated as absent because no commit is assigned zero. The original count-returning ingest methods remain compatible, while Rust, Python, Go, and Java expose acknowledged variants that return the version. Single-batch methods have one unambiguous commit version; streaming methods commit per batch and can report only the last one.

An exact-version `GetFlightInfo` waits for `visibility_wait` before calling `snapshot_at`, using `spawn_blocking` so the synchronous visibility poll does not occupy an async worker. The default is one second and zero restores immediate refusal. The request remains exact rather than meaning "at least": a coordinator fanning one query across endpoints depends on every endpoint honoring the same version, and a reclaimed exact version must not silently fall forward.

The C++ client decodes the widened acknowledgement but does not expose its version yet. Its read surface has no expression-at-version operation to consume the value, so adding a public result would be an unused API. Expose it when that client gains a version-bound query.

### Python client

The pure-Python PyArrow Flight client implements expressions and tickets without a Rust extension. It enforces unsigned bounds, rejects booleans where integers are required, limits expression depth and node count, validates packed views canonically, and exposes TLS, mTLS, bearer metadata, and leadership fencing.

Load-bearing PyArrow behavior includes:

- call metadata is sent as ASCII byte pairs;
- `DoPut` errors may surface only when `FlightStreamWriter.close()` runs;
- gRPC `FailedPrecondition` may arrive as `pyarrow.ArrowInvalid`, so the client does not parse messages to invent transparent retry.

The project lives under `yesno-flight-python/`, uses a `src/` layout, and marks typing ownership inside `yesnodb.client`. The top-level namespace deliberately exports no client symbols.

### Java client

The Java 17 module speaks Arrow Flight directly. It supports current and pinned planning, streaming batches, exact cardinality, key listing, bounded and single-commit writes, removal, clear, and protobuf stats.

Unsigned yesno values use raw `long` bits with decimal and `BigInteger` helpers. The 32-bit Roaring bridge preserves signed Java `int` bit patterns and refuses ordinals above `2^32 - 1`; incompatible Java 64-bit Roaring serialization is explicitly unsupported.

The Gradle wrapper is checksum-pinned. Java compilation uses release 17, `-Xlint:all -Werror`, JUnit Platform, and strict Javadoc generation.

### Tantivy query backend

`PreparedYesnoQuery` implements Tantivy's `Query -> Weight -> Scorer` path as a constant-score filter over segment-local sets. Preparation accepts a materialized set, a fallible embedded `Expr`, or a remote Flight expression.

The query and ordinal resolver bind to one exact `SearcherGeneration`. Segment IDs and deletion opstamps are rechecked at execution so commits and merges produce errors rather than plausible empty results. `FastFieldOrdinalResolver` requires a unique, single-valued `u64` fast field and refuses missing values, multi-valued documents, and duplicate stable IDs.

Missing yesno IDs are errors by default. `MissingOrdinalPolicy::Ignore` is the explicit eventual-consistency escape hatch. The cursor must add the lower chunk bound to `partition_point_in` because its result is relative to that bound; crossing a chunk after advancement is the regression case that exposed the mistake.

### OpenSearch and Elasticsearch

`yesno-search-java` materializes a yesno expression only after checking its
advertised cardinality. It validates the unsigned ordinal schema, nullability,
strict ordering and final row count, then emits portable 32-bit or 64-bit
Roaring bytes under explicit match and serialized-size ceilings.

The OpenSearch 3.8 plugin resolves on a bounded coordinator executor and
rewrites to OpenSearch's native bitmap-valued `terms` query. Elasticsearch
9.5.2 does not register the documented `bitmap_terms` query in the shipped
runtime, so that plugin transports the prepared bitmap to shards and applies a
constant-score query over numeric doc values. Both plugins are version-locked
because their query-builder and lifecycle APIs are engine internals.

The opt-in search E2E scenarios run through the ordinary Monty runner and
shared verb registry. They serve a real Rust Flight endpoint through
`flight_serve`, test both application adapters from Java, install the plugin
ZIPs into disposable engine distributions through `search_*`, and assert
exact matching IDs from real HTTP searches in Python. Normal Gradle and Cargo
gates do not download the large engine archives; the explicit scenarios use a
harness-owned downloader and cache exact SHA-512-pinned distributions.

### Native C++ and Go clients

`yesno-flight-c++` is a synchronous Apache Arrow C++ `FlightClient` wrapper used by the remote MySQL backend. Its protocol validators live in a server-free production helper, so fixed-width codecs, schemas, nullability, ordering across batches, and promised counts fail locally without a daemon. Bazel and CTest run those tests before the pinned MySQL fixture.

The standalone Go 1.25 module covers expression and ticket codecs, bounded DoPut, point and administrative actions, protobuf statistics, bearer/TLS/mTLS transport, and leadership fencing. Query streams retain Arrow ownership until advance and validate global ordering and advertised cardinality across batch boundaries. Its gate runs format, tidy, vet, unit and integration tests under the race detector.

### Canonical ordinal-set literals

The shared expression format includes tag 9 for a sorted, deduplicated list of ordinals below reserved `u64::MAX`. Rust, Python, Go, and Java pin the same fixed byte vector and round-trip every variant. Public constructors normalize duplicates and ordering before encoding; decoders reject non-canonical order, duplicates, lying counts, truncation, and the reserved maximum.

The textual query language spells literals `{a, b, ...}` and accepts `{}`. Java search maps expose a `literal` array of unsigned decimal values. C++ remains key-oriented and intentionally gains no general expression API merely to mirror the other clients.

### Administrative action surface

Flight no longer advertises or accepts `compact`; all Rust, Python, Go, Java, and C++ methods were removed, and E2E surface assertions reject its return. Administrative checkpointing belongs to the authenticated control service. `ServerStats` is a Protobuf message with canonical field bytes rather than language-specific JSON mirrors.

### MySQL storage engine: three backends behind one `Backend`

`yesno-mysql` reaches a database three ways -- embedded, Flight, and the plugin channel -- behind
one `Backend` trait, and each leg is held to the same cases rather than to a subset. Two process
lessons from building the channel leg:

- **A backend nothing compiles is not a backend.** One arm existed in source, type-checked under
  no configuration the gate built, and was therefore unverified in both directions.
- Widening `Backend` for the channel forced a **retraction** of an earlier claim about fidelity
  across backends; the differential fixture that compares the legs is what produced it.

### `records_in_range` was never consulted, and the flag was why

The engine's range estimate was recorded as the reason MySQL would not use the index. It was
genuinely wrong, and it was **not the cause**. The arc took four entries and the middle probe did
the work:

1. Demonstrate the plan -- which looked like confirmation and tested nothing.
2. **Force `records_in_range` to return 1.** The plan was byte-identical, so the estimate was
   never read. An *absurd* value made this decisive; a merely correct one changing nothing would
   have been ambiguous.
3. Read the handler, and find `HA_ONLY_WHOLE_INDEX` vacuous.

`HA_ONLY_WHOLE_INDEX` guards against MySQL asking for a **prefix** of a multi-part key. This key
has one part -- `max_supported_key_parts()` is 1 and `index_read_map` accepts `HA_WHOLE_KEY` and
`1` as the same request -- so there was no partial shape for it to refuse, and it was decisive to
the range optimizer, which built no range plan while it was set and therefore never asked for an
estimate.

Removing it is safe on evidence rather than hope: `index_read_map` already maps every
`ha_rkey_function` a range needs onto a seek mode, and `index_next` / `index_prev` continue from
there. **The capability was present; only the advertisement withheld it.**

The estimate is now bounded by the range's own inclusive width, `min( records(), hi - lo + 1 )`,
from the arguments in hand, ignoring flags deliberately -- the inclusive width is conservative
whichever way `HA_READ_AFTER_KEY` and its siblings read, and an over-estimate costs a plan while an
under-estimate costs a worse one. The plan, on all three backends, for 1000 contiguous ordinals
and `BETWEEN 100 AND 109`:

```text
before  1 SIMPLE est_* NULL ALL    PRIMARY NULL    NULL NULL 1000  11.11 Using where
after   1 SIMPLE est_* NULL range  PRIMARY PRIMARY 8    NULL    10 100.00 Using where
```

Rows examined 1000 -> 10, and `filtered` 11.11 -> 100.00 because MySQL is no longer guessing a
selectivity it could not compute. **One assertion guards both changes**: `rows` of 10 can only come
from the width bound, and `type` of `range` only from the flag's removal.

The width bound had been deleted days earlier on the correct grounds that no test could observe
it, and restored once the flag's removal made it visible. Neither decision invalidates the other.

An exact `len_in_range` would still beat the width bound on a **sparse** range, where the width
over-estimates. It was priced at three C and C++ ABI boundaries and is not built, because nothing
demonstrates a plan that needs it -- which is the bar that entry set for itself.

**The transferable form: ask what is refusing before asking what is imprecise.** The entry named
the estimate, the estimate was genuinely wrong, and four hours of pricing an exact count went into
a path the optimizer never walked.

## Files

- `yesno-flight/` - shared Rust client and Flight protocol integration.
- `yesno-wire/` - expression, ticket, view, and `YSNQ` codecs.
- `yesno-flight-python/` - Python package, tests, lockfile, and gate.
- `yesno-flight-java/` - Java 17 API and Gradle build.
- `yesno-flight-go/` - Go API, codecs, fuzzing, race-enabled tests, and live gate.
- `yesno-flight-c++/` - native synchronous client and server-free validators.
- `yesno-tantivy/` - embedded and remote Tantivy adapter.
- `yesno-search-java/` - bounded materialization and engine query adapters.
- `yesno-opensearch-plugin/` and `yesno-elasticsearch-plugin/` - version-locked coordinator plugins.
- `yesno-pg/` - synchronous PostgreSQL consumer of the shared Rust client.

## Test Coverage

- Fixed byte vectors pin Rust, Python, Go, Java, and server wire compatibility.
- Native C++ unit tests separate protocol validation from live-server interoperability.
- Acknowledgement tests cover 8-byte compatibility, 16-byte versions, zero-version handling, refusal of other widths, and a version increasing across commits.
- Paired Flight tests issue the same not-yet-visible exact request with waiting enabled and disabled, producing success and `VersionNotVisible` respectively.
- Python Hypothesis tests require arbitrary expression and ticket bytes to fail cleanly; daemon integration covers snapshot tickets, auth, fencing, TLS, and views.
- Java unit tests plus a live Rust-daemon program cover unsigned boundaries, Boolean expressions, writes, reads, and pinned planning.
- Tantivy tests cover seek/advance conformance, cross-segment matches, duplicate and missing IDs, generation invalidation, materialization ceilings, and current versus pinned reads across a write.
- The shared-harness search scenarios cover a real Flight resolution, adapter bytes, plugin installation, engine startup, indexing, coordinator rewrite and exact filtered IDs.
- Each language boundary has its own gate in addition to workspace and PostgreSQL gates.

## Pitfalls

- Do not auto-retry an exact-version request by preparing at a newer version.
- Do not flatten protocol or schema failures into network errors.
- Do not overload an exact-version field to mean a minimum version; reclamation makes the two semantics observably different.
- Enumerate every client implementation before changing a wire field. This acknowledgement had seven independent decoders across five languages, and grepping its Rust field name found only three.
- Do not confuse stable application ordinals with engine-local document IDs.
- Verify advertised cardinality before allocation and streamed cardinality afterward.
- Namespace ownership and `py.typed` belong in the concrete Python subpackage, not the shared PEP 420 namespace.
- A language's signed integer representation must not silently truncate yesno's unsigned wire domain.
