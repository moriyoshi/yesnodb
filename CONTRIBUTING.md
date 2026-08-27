# Contributing

yesnodb is pre-release. Changes should preserve the documented storage,
compatibility, and testing invariants rather than treating them as local
implementation details.

## Before changing code

Read the repository's `AGENTS.md` and the architecture and quality-gate
documents it links. Module-level Rust documentation records design constraints
such as size classes, hysteresis, buffer containment, and format compatibility.

Do not change the core representation constants as ordinary tuning knobs.
Research instruments and one-off measurements do not belong in production
source.

## Build and test

The routine workspace gate is:

```console
cargo fmt --all
./scripts/gate.sh
```

If `yesno-core/fuzz` was edited, format it separately because it is outside the
Cargo workspace:

```console
cd yesno-core/fuzz
cargo fmt --all -- --check
```

The PostgreSQL extension and MySQL storage engine are outside the Cargo
workspace and have separate ABI gates. Docker is their only host dependency:

```console
./scripts/gate-pg.sh
./scripts/gate-mysql.sh
```

Each command retains its complete database and integration artifacts in a
tagged local image for reuse by later sessions. Changes to a crate consumed by
either integration must pass the Cargo gate and its applicable database gate.

## Tests

Put regression coverage in the layer that can structurally catch the failure:

- semantic set behavior: the `BTreeSet` property oracle;
- Roaring encoding: byte-level differential tests;
- lazy expressions: eager/lazy/oracle equivalence;
- non-materializing behavior: allocation or call-path tests;
- durability: reopen before reading;
- operational sequences: end-to-end scenarios;
- PostgreSQL planning: pair result assertions with `EXPLAIN`.

Do not weaken an oracle, raise an allocation budget, or delete a persisted
proptest seed to make a change pass.

## Documentation

The root README owns the human-facing product description and deployment
status. Standing user guides belong under `docs/` and must be self-contained:
do not make them depend on implementation file paths. Agent-facing
implementation knowledge belongs under `.agents/docs/`.

Use half-width parentheses and colons in repository-authored documentation.

## Pull requests

A useful pull request explains:

- the user-visible problem;
- the behavioral or compatibility decision;
- the test layer that would catch a regression;
- commands run and their results;
- any remaining limitation.

Do not make discretionary commits in another contributor's working tree, and do
not rewrite unrelated changes.
