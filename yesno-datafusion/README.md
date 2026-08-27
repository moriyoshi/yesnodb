# yesno-datafusion

`yesno-datafusion` integrates [yesnodb](../README.md) with Apache DataFusion.

It provides:

- predicate lowering from DataFusion expressions into yesno set expressions,
  while returning any residual predicate DataFusion must still evaluate; and
- the `yesno_lookup` table function through `YesnoLookup` and the
  `PostingSource` abstraction.

The crate is a satellite rather than a `yesno-core` feature so DataFusion's
dependency tree and release cadence do not affect the storage engine. Its major
version tracks DataFusion's major version; this source tree targets DataFusion
55.

From the repository root:

```console
cargo test -p yesno-datafusion
cargo doc -p yesno-datafusion --no-deps
```

See the [integrations guide](../docs/integrations.md) for the user-facing SQL
surface.
