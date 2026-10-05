# Integrations

yesnodb separates its storage engine from Arrow, DataFusion, Flight, and its
database and search-engine adapters. Each surface has a different contract.

## Capability matrix

This table compares the main query surfaces. The language clients and external
database adapters are described below.

| Capability | Embedded core | `yesno` | Flight v1 | DataFusion | PostgreSQL |
|---|---:|---:|---:|---:|---:|
| Read one key | yes | yes | yes | yes | yes |
| Exact cardinality without returning ordinals | yes | yes | yes | planner statistics | `count(*)` pushdown |
| AND / OR / AND NOT | yes | yes | yes | predicate lowering | FDW qualifier lowering |
| XOR / complement | yes | yes | derived from v1 nodes | no SQL-specific surface | no SQL-specific surface |
| Bulk ingest | yes | CSV-like pairs | Arrow `DoPut` pairs, mixed operations, or container payloads | no | FDW and access-method writes |
| Consistent snapshot | yes | ticket version | ticket version | one `SnapshotSource` | transaction-level rules |
| Read replica | library primitives | transparent endpoint | transparent endpoint | transparent source | remote FDW endpoint |

`yesno query` accepts the full Boolean surface. XOR is encoded as
`(A OR B) AND NOT (A AND B)` and complement as the full ordinal range minus the
operand. This preserves compatibility with the existing v1 wire tags.

## Arrow

Use an ordinal batch reader when a downstream system genuinely needs integer
ordinals:

```rust
use std::sync::Arc;

use yesno_arrow::OrdinalBatchReader;
use yesno_core::OrdSet;

let set = Arc::new(OrdSet::from_iter_unsorted([1, 5, 9, 65_540]));
for batch in OrdinalBatchReader::new(set.stream()) {
    let batch = batch?;
    // The schema is one non-null UInt64 column named "ordinal".
}
```

Use a mask stream for filtering. A bitmap-backed chunk lends its bits directly
as an Arrow BooleanBuffer; sparse and run containers produce an equivalent mask:

```rust
use std::sync::Arc;

use yesno_arrow::MaskStream;
use yesno_core::stream::ChunkStreamExt;
use yesno_core::OrdSet;

let a = Arc::new(OrdSet::from_iter_unsorted([1, 2, 5, 8]));
let b = Arc::new(OrdSet::from_iter_unsorted([2, 5, 13]));

for mask in MaskStream::new(a.stream().and(b.stream())) {
    let mask = mask?;
    let boolean_array = mask.to_array();
    // Pass boolean_array to an Arrow filter kernel.
}
```

Choose masks for filtering and ordinal batches for joins, export, or other
consumers that need the actual integer values.

## Arrow Flight

The Rust convenience client handles yesno descriptors, versioned tickets,
record-batch decoding, ingest acknowledgements, and administrative actions:

```rust
use yesno_flight::{SetExpr, YesnoClient};

let mut client = YesnoClient::connect("http://127.0.0.1:50051").await?;
let expression = SetExpr::And(vec![
    SetExpr::Key(42),
    SetExpr::literal([9, 7, 9])?,
]);

// Count from index metadata without fetching an ordinal.
let count = client.query_cardinality(&expression).await?;

// Or stream Arrow RecordBatches. This explicit helper materializes all rows.
let result = client.query(&expression).await?;
assert_eq!(result.info().total_records(), count);
let ordinals = result.collect_ordinals().await?;

client.insert([(42, 10), (42, 11)]).await?;
client.remove([(42, 10)]).await?;

let stats = client.stats().await?;
assert!(stats.shards > 0);
```

Set queries offer three result representations. The default streams sorted
`UInt64` ordinals. For a consumer that understands Roaring payloads,
`fetch_containers` returns one encoded container per chunk with the container
schema described below. For an Arrow mask over a known bounded ordinal window,
`fetch_bitvector` returns one `Binary` value of packed bits per chunk, including
zero-filled gaps; its schema metadata names the base ordinal and bits per
value. The bitvector needs no Roaring decoder, but its size follows the window
rather than the match count. The server refuses a window above 16,384 chunks
instead of silently truncating it. Both alternatives are explicit client
requests, so a client expecting ordinals is never handed another schema.

`prepare_key` and `prepare_query` return reusable query metadata separately
from `fetch`. The exact count and ticket name one snapshot version, which is
useful when a transaction or coordinator needs repeatable reads. A custom tonic
`Channel` can be passed to `YesnoClient::new` for TLS configuration, and
`inner_mut` exposes Apache Flight's metadata API for authorization headers.

The primitive v1 expression nodes are `Empty`, `Key`, `Range`, `Literal`,
`And`, `Or`, and `AndNot`. Literal builders sort and deduplicate their input and
reject the reserved maximum ordinal. Clients construct the other two Boolean
operations compatibly:

```rust
let xor = SetExpr::xor(SetExpr::Key(1), SetExpr::Key(2));
let complement = SetExpr::complement(SetExpr::Key(3));
```

Expression depth is capped at 32 and node count at 4096. A ticket is leased for a
bounded interval, long enough to cross the round trip it was minted for. Once
that interval passes, a checkpoint may reclaim the version; `DoGet` then refuses
rather than answering from a different instant, and the client should request new
`FlightInfo` and retry at a fresh snapshot.

Bulk ingest uses Arrow `DoPut`. The descriptor command is required -- an absent,
empty or unrecognized command is an error rather than a silent default, so a
client asking for an operation the server does not implement is told so instead
of having its stream reinterpreted. `insert` and `remove` carry two non-null
`UInt64` columns named `key` and `ordinal`, and commit one version per record
batch.

`apply` carries a mixed-operation stream and commits the whole call as one
version. Its columns are `key`, `lo`, `hi` and `op`, all non-null: `op` selects
insert, remove, insert-range, remove-range, or delete-key, `lo` and `hi` bound
an inclusive range, and a single-ordinal operation sets them equal. Operations
apply in stream order, which is what lets a change-data feed replay a
delete-then-reinsert of the same key without the two reordering.

For dense sets, `apply` also accepts the container schema: non-null `key`,
`prefix48`, `kind`, `cardinality`, and `payload` columns. Each row is one
Roaring-compatible chunk payload whose members are **inserted** into the named
key; it does not replace the existing chunk and cannot express a removal.
The same schema is available on the read side through container-result tickets.
Use a container batch builder to make valid payloads, and check the server's
container-put capability before sending them. The Rust client's
`apply_containers` helper requires its optional `containers` feature. It
reports the number of ordinals decoded, rather than the number of Arrow rows.
Container batches can also be staged in a write transaction. Staging is bounded
by payload bytes as well as ordinal count, so a dense transaction cannot grow
without limit. On a measured half-dense 64-chunk page, the container wire was
64.8 times smaller than the pair wire.

A write spanning several calls uses a write transaction: an action opens one and
returns its identifier, `DoPut` calls with the command `txn:<id>` stage
mixed-operation batches into it, and a commit action applies the accumulated
operations as one version or an abort discards them. Staged rows are not visible
to readers until the commit. A server advertises `apply` and write-transaction
support in its capability flags; open transactions are bounded in number, in
staged rows, and by a deadline, so an abandoned one is reclaimed rather than
held.

The ingest acknowledgement is little-endian `app_metadata`: the accepted count
first, then the committed version. The count is the number of rows for pair or
mixed-operation batches, and the sum of decoded cardinalities for container
batches. Read it from the leading eight bytes and treat any other total width
than eight or sixteen as a
protocol error. A sixteen-byte acknowledgement whose version is zero committed
nothing, because no commit is ever assigned version zero; an eight-byte one comes
from a server predating the version field, so a client should lose the version
rather than the call.

Naming a version is what makes a read-your-writes read expressible. Pass the
version an ingest reported when planning a query at a caller-selected version:
that read is bound to a database state containing the write, and the server waits
briefly for a version that is merely not visible yet rather than refusing it. The
version is honoured exactly, so a coordinator fanning one query across several
endpoints still gets one instant. A read that does not name a version sees
whatever the visible watermark has reached, which is not necessarily the client's
own most recent write.

### Python client

The `yesnodb` Python package provides the same plan/fetch split, exact counts,
expression format, pair ingest, and administrative actions as the Rust client.
It is a synchronous wrapper over PyArrow Flight and yields native
`pyarrow.RecordBatch` values without converting them through Python objects:

```python
from yesnodb.client import And, Client, Key, Literal

with Client("http://127.0.0.1:50051") as client:
    expression = And(Key(42), Literal(9, 7, 9))
    count = client.query_cardinality(expression)
    planned = client.prepare_query(expression)
    historical = client.prepare_query_at(expression, planned.version)
    assert historical.version == planned.version

    result = client.query(expression)
    assert result.info.total_records == count
    for batch in result:
        consume(batch)

    client.insert([(42, 10), (42, 11)])
    client.remove_batch([(42, 10)])
```

`insert` and `remove` bound memory by sending batches of 8192 pairs. Each Arrow
batch is one database commit. Use `insert_batch` or `remove_batch` when all
pairs must share one commit, and `insert_batches` or `remove_batches` when the
caller already owns the transaction boundaries.

Bearer tokens and the minimum accepted leadership term are sent on every RPC:

```python
from yesnodb.client import Client, TLSConfig

tls = TLSConfig.from_files(
    root_certificates="/etc/yesno/tls/ca.pem",
    client_certificate="/etc/yesno/tls/client.pem",
    client_key="/etc/yesno/tls/client.key",
    server_name="yesno.internal",
)
client = Client(
    "https://yesno.internal:50051",
    tls=tls,
    token="application bearer token",
    minimum_term=4,
)
```

The client never retries a versioned ticket automatically. If PyArrow reports a
failed precondition while fetching, prepare the original key or expression
again and restart the unit of work. Once a stream has yielded rows, an automatic
restart could duplicate data already consumed.

### Go client

The Go module uses Apache Arrow Go directly. It exposes the same expression,
plan/fetch, exact-cardinality, bulk-mutation, point-action, statistics, bearer,
TLS, and leadership-fencing contracts as the other Flight clients:

```go
client, err := yesnodb.Dial(
    "yesno.internal:50051",
    yesnodb.WithTLSConfig(tlsConfig),
    yesnodb.WithBearerToken(token),
    yesnodb.WithMinimumTerm(4),
)
if err != nil {
    return err
}
defer client.Close()

stream, err := client.Query(ctx, yesnodb.And(
    yesnodb.Key(42),
    yesnodb.Range(0, 1_000_000),
))
if err != nil {
    return err
}
defer stream.Release()

for stream.Next() {
    consume(stream.Record())
}
if err := stream.Err(); err != nil {
    return err
}
```

The current record belongs to the stream and is valid until the next `Next`
call; retain it before storing it. Planning validates the Arrow schema and
ticket, while consumption validates nullability, strict global ordering, and
the promised count across batch boundaries. Transport choice is explicit:
plaintext requires an insecure option, while TLS accepts a cloned Go TLS
configuration. The client preserves gRPC status errors so callers can branch on
authentication, authorization, fencing, and reclaimed-version failures.

### C++ and Java clients

The synchronous C++ client uses Apache Arrow's native Flight API. It supports
versioned planning, exact counts, validated ordinal reads, bulk pair writes,
and atomic point insert, remove, contains, and clear actions. One client must
be serialized by its host or used by one thread at a time.

The synchronous Java 17 client supports streamed ordinal batches, versioned
planning, exact counts, bounded pair writes, and conversion to 32-bit
`RoaringBitmap` with a range check. Java `long` values carry the raw bits of
unsigned keys and ordinals; applications must use unsigned conversions when
displaying or parsing values above `Long.MAX_VALUE`. The Java client artifact
is built from this checkout and is not yet published.

## Tantivy

The `yesno-tantivy` crate turns a yesno result into a Tantivy `Query`.
yesno ordinals are stable application IDs, not Tantivy document IDs, so the
Tantivy schema needs a unique, single-valued `u64` fast field that carries the
same ID. A resolver translates that field into segment-local document IDs:

```rust
use std::sync::Arc;

use tantivy::collector::Count;
use yesno_core::Expr;
use yesno_tantivy::{
    FastFieldOrdinalResolver, MissingOrdinalPolicy, PreparedYesnoQuery,
};

let snapshot = db.snapshot()?;
let expression = Expr::set(Arc::new(snapshot.load(42)?));
let resolver = FastFieldOrdinalResolver::build(&searcher, "stable_id")?;
let query = PreparedYesnoQuery::from_expr(
    &searcher,
    &resolver,
    &expression,
    MissingOrdinalPolicy::Error,
    Some(snapshot.version()),
)?;

let matches = searcher.search(&query, &Count)?;
```

Preparation is fallible; Tantivy's scorer is not. The result is therefore fully
resolved before search and is bound to the exact Tantivy searcher generation.
A delete, commit, or segment merge requires rebuilding both the resolver and
query. Reusing an old query returns an error instead of an empty result. The
default score is zero so the query composes as a filter; `with_score` opts into
a finite constant score.

Enable the `flight` feature to prepare from a remote yesnodb server:

```rust
use yesno_flight::{SetExpr, YesnoClient};
use yesno_tantivy::flight::FlightQueryPreparer;

let mut client = YesnoClient::connect("http://127.0.0.1:50051").await?;
let query = FlightQueryPreparer::new(SetExpr::Key(42))
    .max_matches(2_000_000)
    .prepare(&mut client, &searcher, &resolver)
    .await?;
```

Remote preparation checks the exact `FlightInfo.total_records` before
allocating, validates that the stream has that cardinality and is strictly
ordered, and only then resolves IDs. `pinned(version)` asks for exactly that
retained yesno version and never falls forward. Current-version preparation may
retry a recoverable stale-ticket failure from the beginning; pinned preparation
does not replace its requested version. Missing stable IDs are errors unless
`MissingOrdinalPolicy::Ignore` is selected explicitly.

## DataFusion

The DataFusion crate's major version follows DataFusion's major version.
`yesno-datafusion = 55` is for DataFusion 55.

Register `yesno_lookup` against one database snapshot:

```rust
use std::sync::Arc;

use datafusion::common::ScalarValue;
use datafusion::prelude::SessionContext;
use yesno_core::Db;
use yesno_datafusion::{
    HashEncoder, SnapshotSource, TermEncoder, YesnoLookup,
};

let db = Db::open("./yesnodb-data")?;
let encoder = HashEncoder;
let rust_key = encoder
    .encode(&ScalarValue::Utf8(Some("rust".into())))
    .expect("a string term is encodable");
db.insert_many(rust_key, &[1, 3, 5, 7])?;

let source = Arc::new(SnapshotSource::new(db.snapshot()?));
let context = SessionContext::new();
context.register_udtf(
    "yesno_lookup",
    Arc::new(YesnoLookup::new(source)),
);

let batches = context
    .sql("SELECT ordinal FROM yesno_lookup('rust')")
    .await?
    .collect()
    .await?;
```

Use one `SnapshotSource` per query or bounded unit of work. It pins one version,
the database file lock, and the reclamation floor for its lifetime.

`PostingSource` distinguishes an absent key (`Ok(None)`) from a failed snapshot
read (`Err`). An evicted snapshot therefore makes planning fail with a DataFusion
error instead of silently returning an empty posting list. Keep source
lifetimes scoped to the query so they do not pin old versions unnecessarily.

## Search engines

An application can resolve a yesnodb expression to a portable bitmap filter
with the Java search adapter and submit the resulting query through its own
OpenSearch or Elasticsearch client. The target numeric field must use the same
stable ordinal assignment as yesnodb.

Version-locked plugins instead resolve a `yesno` query on the coordinating
search node before shard fan-out. The OpenSearch 3.8.0 plugin rewrites to its
native bitmap-valued `terms` query. The Elasticsearch 9.5.2 plugin ships a
portable Roaring filter against numeric document values. Both bound result
cardinality, bitmap size, and Flight time, and a pinned snapshot version is
never silently replaced with a newer one. These plugins are tied to their
named engine versions; validate a new engine version before upgrading.

## C embedding

The separate `yesno-c` workspace exposes opaque database and materialized
snapshot-cursor handles to C and C++ hosts. Each database handle owns an
independent durable database; a cursor retains the set it opened even while
later writes commit. The ABI returns explicit status codes and catches panics
at exported boundaries. The embedded MySQL backend uses this ABI.

## PostgreSQL

The PostgreSQL extension is experimental and is not distributed as an
installable package yet. Its separate repository gate builds it with Bazel
against the supported, source-pinned PostgreSQL server ABIs.

Do not build or test it with `cargo pgrx`; that uses machine-specific PostgreSQL
state and does not reproduce the pinned server ABI.

Once the extension library, control file, and versioned SQL file have been
installed into a matching PostgreSQL installation, load it with:

```sql
CREATE EXTENSION yesno_pg;
SELECT yesno_pg_version();
```

It exposes three distinct surfaces.

### Foreign data wrapper

A foreign table represents one remote yesnodb key as a single `bigint ordinal`
column:

```sql
CREATE SERVER yesno
  FOREIGN DATA WRAPPER yesno_fdw
  OPTIONS (endpoint 'https://yesno.internal:50051');

CREATE FOREIGN TABLE rust_docs (ordinal bigint NOT NULL)
  SERVER yesno
  OPTIONS (key '42');

SELECT count(*) FROM rust_docs;
```

A dictionary table can map names to keys for `IMPORT FOREIGN SCHEMA`:

```sql
CREATE TABLE yesno_terms (
  term text PRIMARY KEY,
  key bigint NOT NULL
);

CREATE SERVER yesno_named
  FOREIGN DATA WRAPPER yesno_fdw
  OPTIONS (
    endpoint 'https://yesno.internal:50051',
    dictionary 'public.yesno_terms'
  );

CREATE SCHEMA postings;
IMPORT FOREIGN SCHEMA yesno FROM SERVER yesno_named INTO postings;
```

The extension supports remote writes, but they are not atomically committed with
ordinary PostgreSQL heap writes.

### Index access method

The `yesno` index access method supplies equality posting lists and bitmap scans:

```sql
CREATE INDEX documents_tag_yesno
  ON documents USING yesno (tag);

SELECT * FROM documents WHERE tag = 'rust';
```

Use `EXPLAIN` to verify that a query actually selected the yesno access method.
A correct result alone does not prove pushdown happened.

### Table access method

A `yesno_table` table is a single-column `bigint` set:

```sql
CREATE TABLE selected_ordinals (ordinal bigint)
  USING yesno_table;

INSERT INTO selected_ordinals VALUES (1), (5), (9);
SELECT * FROM selected_ordinals ORDER BY ordinal;
```

It is not a general heap. `UPDATE`, `SELECT FOR UPDATE`, `ON CONFLICT`,
`CLUSTER`, `TABLESAMPLE`, and unsupported shapes fail explicitly.

Isolation within a yesno table follows PostgreSQL transaction levels:
`REPEATABLE READ` pins a version and `READ COMMITTED` takes a fresh snapshot per
statement. A transaction touching both a yesno table and an ordinary heap can
observe different commit moments because the two storage engines do not share a
commit protocol. Back up yesnodb data separately from PostgreSQL.

## MySQL

The experimental MySQL 8.4 storage engine exposes one yesnodb key as a table
with exactly one `BIGINT UNSIGNED NOT NULL PRIMARY KEY` column. The table's
`CONNECTION='key=<u64>'` selects that set. Tables with the same connection
value share data; dropping or truncating either clears the underlying set.

It supports inserts, deletes, exact and range reads, ordered scans, exact
counts, rollback, and savepoints. A transaction's buffered changes become one
yesnodb version at MySQL commit, using either the embedded C ABI or a remote
Flight backend selected at startup. There is no two-phase prepare: a crash
between this engine's commit and MySQL's binlog write can leave them out of
agreement. Back up embedded yesnodb data separately from MySQL's other engines.
