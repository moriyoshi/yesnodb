# yesno-pg

`yesno-pg` is the experimental PostgreSQL extension for
[yesnodb](../README.md). It provides three integration surfaces:

- a foreign data wrapper for remote yesno ordinal sets;
- an index access method over ordinary PostgreSQL heaps; and
- a table access method for a single-column `bigint` set.

This crate is intentionally outside the Cargo workspace. A PostgreSQL extension
must be built against the exact server ABI that will load it, so Bazel supplies
pinned PostgreSQL 17 and 18 builds, matching headers, and generated bindings.
Do not use `cargo build`, `cargo pgrx test`, or `cargo pgrx init` for this crate.

Docker is the only host dependency. From the repository root, build the ABI
artifacts and run the independent gate with:

```console
./scripts/gate-pg.sh
```

The completed artifact tree remains in the single all-in-one `yesno-e2e:local`
image. Every containerized gate — PostgreSQL, MySQL, OpenSearch, Elasticsearch,
the Kubernetes operator, and the native filesystems — uses that same image, so a
later gate with the same checkout reuses the pinned toolchains and artifact
caches. Set `YESNO_E2E_IMAGE` to choose a different tag.

Bazel supplies the ABI-pinned server and extension artifacts. The hermetic
`//e2e/postgresql:regress` target owns the fixture corpus and invokes the
ordinary `yesno-e2e` runner. Its Monty scenario composes the same generic
fixture verbs used by MySQL to own the throwaway cluster, in-process Flight
service, byte-exact SQL checks, persistent two-session isolation schedules, and
cleanup. Every external input is a declared Bazel runfile.
The extension does not share PostgreSQL's WAL or commit clock. Its isolation,
backup requirements, SQL examples, and current limitations are documented in
the [integrations guide](../docs/integrations.md) and
[operations guide](../docs/operations.md).
