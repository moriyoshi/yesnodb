# yesno-mysql

`yesno-mysql` is an experimental MySQL 8.4 storage engine with embedded and
remote yesnodb backends. The embedded backend uses the host-independent
[`yesno-c`](../yesno-c/) ABI; the remote backend uses the native Arrow C++
client in [`yesno-flight-c++`](../yesno-flight-c++/). It models one yesnodb
ordinal set as one SQL table:

```sql
CREATE TABLE postings (
  ordinal BIGINT UNSIGNED NOT NULL PRIMARY KEY
) ENGINE=YESNO CONNECTION='key=42';
```

The schema is deliberately narrow. A YESNO table must have exactly that one
unsigned, non-null `BIGINT` column and one single-column primary key. The
`CONNECTION` value selects the underlying yesnodb key and is the table's storage
identity; renaming the SQL table does not move data.

## Semantics

- `INSERT`, `DELETE`, exact primary-key reads, ordered scans, and primary-key
  ranges are supported. `COUNT(*)` uses the exact yesnodb cardinality.
- `UPDATE` is rejected; use `DELETE` followed by `INSERT`.
- Cursors read a stable snapshot. A scan does not see rows inserted after that
  scan starts.
- Writes are **transactional**. They are buffered per connection and applied
  when MySQL commits, so `ROLLBACK` discards them and `SAVEPOINT` unwinds to a
  point. One SQL transaction becomes one yesnodb version, whichever backend is
  in use.
- There is deliberately **no two-phase prepare**, so cross-engine commits are
  still not atomic: a crash between this engine's commit and MySQL's binlog
  write leaves the two disagreeing. Closing that needs a durable
  prepare/resolve protocol yesnodb does not expose.
- `TRUNCATE` and `DROP TABLE` clear the selected yesnodb key. Two SQL tables
  using the same `CONNECTION` therefore alias the same set, and dropping either
  clears what both see.
- `UINT64_MAX` is reserved by yesnodb and cannot be stored.

The read-only startup option `--yesno-backend=embedded|flight` selects the
backend and defaults to `embedded`. The embedded database lives under
`<mysql-datadir>/yesno` by default; `--yesno-data-dir=/absolute/path` places it
elsewhere. Back it up separately from MySQL's transactional engines.

For a remote server, start MySQL with `--yesno-backend=flight` and
`--yesno-flight-endpoint=grpc://host:port`. Plugin initialization probes the
endpoint and fails if it is unavailable. Each scan materializes one remote
snapshot into a C++ cursor, while point writes use atomic Flight actions so
duplicate and missing-row results are not implemented as racy read-then-write
sequences. A committed transaction is staged through one Flight write
transaction and published as a single version, so the remote backend gives the
same atomicity as the embedded one rather than a weaker approximation of it. The remote server owns checkpoint policy and durability; clean
plugin unload only releases the client connection.

## Build and test against pinned MySQL 8.4

Bazel is the authoritative build because a MySQL storage-engine module is tied
to the exact server ABI. The target uses sha256-pinned MySQL 8.4.0, Apache
Arrow 25.0.1, OpenSSL 3.0.13, ncurses 6.5, patchelf 0.18.0, and Arrow Flight's
native dependencies. It builds `yesno-c` and `yesno-flight-c++`, links both
backends into `ha_yesno.so`, and bundles Arrow's runtime libraries. Docker is
the only host dependency. Build every artifact and run the complete edge gate
with:

```console
./scripts/gate-mysql.sh
```

The completed artifact tree remains in the single all-in-one `yesno-e2e:local`
image. Every containerized gate — PostgreSQL, MySQL, OpenSearch, Elasticsearch,
the Kubernetes operator, and the native filesystems — uses that same image, so a
later gate with the same checkout reuses the pinned toolchains and artifact
caches. Set `YESNO_E2E_IMAGE` to choose a different tag.

The regression target invokes the ordinary `yesno-e2e` runner on
[`e2e/mysql/mysql.py`](../e2e/mysql/mysql.py). That scenario uses the same
generic fixture verbs as PostgreSQL to run two private copies of the exact
server: embedded mode exercises the C ABI and byte-exact `mysqltest` corpus;
Flight mode loads the native C++ client against the harness's in-process Flight
service. Both modes assert schema rejection, unsigned boundaries, ordered and
range scans, exact lookup and cardinality, duplicate and NULL refusal, update
refusal, shared-key aliasing, delete/drop behavior, transactional rollback
and savepoint semantics, and plugin lifecycle. It also rejects a plugin that leaks the
standalone `yesno_*` C ABI. No host MySQL installation is used.

For interactive inspection, build `//third_party/mysql:gen_dir`; its output is
the complete install prefix containing `bin/mysqld`, `bin/mysql`, and
`lib/plugin/ha_yesno.so`. Register the module with:

```sql
INSTALL PLUGIN yesno SONAME 'ha_yesno.so';
```

The source-tree CMake path remains a developer fallback when modifying the C++
handler. Symlink this directory to `storage/yesno` in an exact MySQL 8.4 source
tree and build target `yesno`. By default CMake builds the adjacent `yesno-c`
workspace with position-independent code; an outer build may instead supply
`YESNO_C_PREBUILT_LIBRARY` and `YESNO_C_HEADER`, which is how Bazel keeps the
Rust build independent from MySQL's CMake graph. Configure with
`-DWITH_YESNO_STORAGE_ENGINE=1` to compile the engine into `mysqld` rather than
emitting the loadable module.
