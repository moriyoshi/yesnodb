#!/usr/bin/env python3
"""Prove the two pgrx resolver manifests are semantically identical.

crate-universe resolves Cargo features before Bazel configuration analysis, so
each supported PostgreSQL major needs a manifest with a different default pgN
feature. The dependency versions still have one authority: both hubs consume
yesno-pg/Cargo.lock. This check prevents the unavoidable manifest mirror from
becoming a second, silently drifting dependency declaration. The nested pg18
manifest also needs an explicit path back to the shared library source.
"""

import pathlib
import sys
import tomllib

ROOT = pathlib.Path(__file__).resolve().parent.parent
PG17 = ROOT / "yesno-pg/Cargo.toml"
PG18 = ROOT / "yesno-pg/pg18/Cargo.toml"


def read(path: pathlib.Path) -> dict:
    with path.open("rb") as stream:
        return tomllib.load(stream)


def main() -> int:
    pg17 = read(PG17)
    pg18 = read(PG18)

    expected = [(PG17, pg17, ["pg17"]), (PG18, pg18, ["pg18"])]
    for path, manifest, feature in expected:
        actual = manifest.get("features", {}).get("default")
        if actual != feature:
            print(
                f"  {path.relative_to(ROOT)}: default feature is "
                f"{actual!r}, expected {feature!r}"
            )
            return 1

    pg17["features"]["default"] = ["pgN"]
    pg18["features"]["default"] = ["pgN"]
    pg17["lib"]["path"] = "src/lib.rs"
    pg18["lib"]["path"] = "src/lib.rs"
    if pg17 != pg18:
        print("  PostgreSQL resolver manifests have different package metadata")
        print(
            "  keep dependency metadata identical; versions come from "
            "yesno-pg/Cargo.lock"
        )
        return 1

    print("  pg17 and pg18 resolver manifests have identical package metadata")
    return 0


if __name__ == "__main__":
    sys.exit(main())
