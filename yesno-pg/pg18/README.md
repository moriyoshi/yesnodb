# yesno-pg PostgreSQL 18 manifest

This directory is the PostgreSQL 18 dependency-resolution mirror for
[`yesno-pg`](../README.md). It is not a second extension implementation.

The manifest points at the shared source one directory above and differs from
the PostgreSQL 17 manifest only by selecting the `pg18` feature and by using a
relative library path. Bazel uses it to create the PostgreSQL 18
`crate_universe` dependency hub.

Do not build this directory directly and do not add source files here. From the
repository root, validate both supported PostgreSQL majors with:

```console
./scripts/gate-pg.sh
```
