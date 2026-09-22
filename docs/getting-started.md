# Getting started

yesnodb maps each unsigned 64-bit key to a set of unsigned 64-bit ordinals. A
key is commonly a term, tag, feature, or other posting-list identifier. The
largest unsigned 64-bit value is reserved; valid ordinals end at
`u64::MAX - 1`.

yesnodb is pre-release software. Build it from a source checkout and use a local
filesystem. There are no published crates or binary releases yet.

## Choose a surface

| Goal | Start with |
|---|---|
| Embed the storage engine in Rust | `yesno-core` |
| Run a service and use it from a shell | `yesnod` and `yesno` |
| Send or receive Arrow data | `yesno-arrow` or Arrow Flight |
| Query posting lists from DataFusion | `yesno-datafusion` |
| Experiment with PostgreSQL integration | `yesno-pg` |

The Cargo workspace, including the core crate, needs Rust 1.95 or newer. The
separate PostgreSQL extension needs Rust 1.96.
The supported platforms are 64-bit Linux and macOS on local filesystems.
Windows and network filesystems such as NFS are not supported.

## Run the checked example

From the repository root:

```console
cargo run -p yesno-core --example readme
```

A successful run prints:

```text
readme example ok: cardinality = 3, durable = true, matrix trace = 4
```

The example writes three posting lists, checkpoints them, opens a consistent
snapshot, and evaluates `rust AND (database OR durable)` over the stored data.
Its cardinality walk does not materialize an intermediate result.

## Embed the database

The core storage workflow is synchronous:

```rust
use std::sync::Arc;

use yesno_core::stream::ChunkStreamExt;
use yesno_core::{Db, Result};

fn main() -> Result<()> {
    let db = Db::open("./yesnodb-data")?;

    let rust = 42;
    let database = 7;
    let durable = 9;

    db.insert_many(rust, &[1, 5, 9, 65_540])?;
    db.insert_many(database, &[5, 9, 13])?;
    db.insert_many(durable, &[1, 5])?;
    db.checkpoint()?;

    let snapshot = db.snapshot()?;
    let a = Arc::new(snapshot.load(rust)?);
    let b = Arc::new(snapshot.load(database)?);
    let c = Arc::new(snapshot.load(durable)?);

    let query = a.stream().and(b.stream().or(c.stream()));
    assert_eq!(query.cardinality()?, 3);
    Ok(())
}
```

A `Snapshot` sees one version across every shard and remains stable while later
writes commit. Holding a snapshot also holds the reclamation floor down. Keep
snapshots scoped to the work that needs them and handle `SnapshotTooOld` when
the configured space policy evicts a long-lived reader.

`Db::checkpoint` persists a consistent image and permits old WAL and extents to
be reclaimed. The embedded library has no background thread: an application
that needs periodic checkpointing must schedule it. The server does this for
you.

## Run the server

Build the daemon, data CLI, and administrative CLI:

```console
cargo build --release -p yesno-server -p yesno-server-utils
```

Start a loopback-only server:

```console
./target/release/yesnod --data-dir ./yesnodb-data
```

In another shell, verify its identity and state:

```console
./target/release/yesno status
```

Ingest `key,ordinal` pairs from standard input:

```console
printf '42,1\n42,5\n42,9\n7,5\n7,9\n9,1\n9,5\n' |
  ./target/release/yesno put -
```

Fetch one posting list:

```console
./target/release/yesno get 42
```

Evaluate a Boolean query over stored keys:

```console
./target/release/yesno query 'and(42,or(7,9))'
```

Ask only for its exact cardinality, without fetching any ordinals:

```console
./target/release/yesno query --count-only 'and(42,or(7,9))'
```

The expression grammar is:

```text
NUMBER                         bare NUMBER means key(NUMBER)
key(NUMBER)
range(LO, HI)                  half-open ordinal range [LO, HI)
and(EXPR, EXPR, ...)
or(EXPR, EXPR, ...)
xor(EXPR, EXPR)
and-not(EXPR, EXPR)
not(EXPR)
empty
```

Whitespace and underscores in numbers are accepted. `xor` and `not` are encoded
using the v1 `OR`, `AND`, `AND NOT`, and full-range primitives, so they work
without a protocol-version change.

`not(EXPR)` can describe almost the entire ordinal universe. Use
`--count-only` when you need only its cardinality, or `-n LIMIT` when you
want to inspect a bounded prefix rather than stream that full result.

Force and verify a durable checkpoint with:

```console
./target/release/yesnoctl checkpoint
```

Use the [operations guide](operations.md) before binding a non-loopback address
or keeping important data. It covers TLS, authentication, metrics, backup,
replication, promotion, and restore. The [integrations guide](integrations.md)
covers Arrow, Flight, DataFusion, and PostgreSQL.
